use crate::protocol::{Event, Incoming, Request, TargetInfo};
use anyhow::{Context as _, Result, anyhow};
use async_tungstenite::tungstenite::Message;
use async_tungstenite::tungstenite::protocol::WebSocketConfig;
use collections::HashMap;
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use parking_lot::Mutex;
use smol::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// No single CDP reply should take longer than this; a page that does is
/// reported instead of hanging the caller forever.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// `DOM.getDocument` on a large page easily tops tungstenite's 16 MiB
/// default, which would close the connection instead of delivering the reply.
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// The clock the connection times calls with. The caller supplies it (a
/// `gpui::BackgroundExecutor::timer` in the editor) so this crate stays
/// free of the executor and tests can use a wall clock against real sockets.
pub type Sleep = Arc<dyn Fn(Duration) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

type Pending = HashMap<u64, oneshot::Sender<Result<serde_json::Value>>>;
type Subscribers = Vec<mpsc::UnboundedSender<Event>>;

/// A Chrome DevTools Protocol connection over a loopback WebSocket. Cloning
/// shares the connection; requests are correlated by `id`, events are fanned
/// out to every receiver handed out by [`CdpClient::events`].
#[derive(Clone)]
pub struct CdpClient {
    outgoing: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<Pending>>,
    subscribers: Arc<Mutex<Subscribers>>,
    next_id: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    sleep: Sleep,
}

impl CdpClient {
    pub async fn connect(url: &str, sleep: Sleep) -> Result<Self> {
        let address = loopback_address(url)?;
        let stream = TcpStream::connect(&address)
            .await
            .with_context(|| format!("connecting to {address}"))?;
        let config = WebSocketConfig::default()
            .max_frame_size(Some(MAX_FRAME_BYTES))
            .max_message_size(Some(MAX_FRAME_BYTES));
        let (socket, _) = async_tungstenite::client_async_with_config(url, stream, Some(config))
            .await
            .with_context(|| format!("websocket handshake with {url}"))?;
        let (mut sink, mut source) = socket.split();
        let (outgoing, mut outgoing_receiver) = mpsc::unbounded::<String>();
        let pending: Arc<Mutex<Pending>> = Arc::default();
        let subscribers: Arc<Mutex<Subscribers>> = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));

        smol::spawn(async move {
            while let Some(text) = outgoing_receiver.next().await {
                if let Err(error) = sink.send(Message::Text(text.into())).await {
                    log::warn!("cdp: write failed: {error}");
                    break;
                }
            }
        })
        .detach();

        smol::spawn({
            let pending = pending.clone();
            let subscribers = subscribers.clone();
            let closed = closed.clone();
            async move {
                while let Some(message) = source.next().await {
                    let text = match message {
                        Ok(Message::Text(text)) => text,
                        Ok(Message::Close(_)) => break,
                        Ok(_) => continue,
                        Err(error) => {
                            log::warn!("cdp: read failed: {error}");
                            break;
                        }
                    };
                    match serde_json::from_str::<Incoming>(&text) {
                        Ok(Incoming::Response(response)) => {
                            let Some(sender) = pending.lock().remove(&response.id) else {
                                log::warn!("cdp: response for unknown id {}", response.id);
                                continue;
                            };
                            let outcome = match (response.result, response.error) {
                                (_, Some(error)) => {
                                    Err(anyhow!("{} ({})", error.message, error.code))
                                }
                                (Some(result), None) => Ok(result),
                                (None, None) => Ok(serde_json::Value::Null),
                            };
                            // The caller may have given up waiting; nothing to do then.
                            sender.send(outcome).ok();
                        }
                        Ok(Incoming::Event(event)) => {
                            subscribers.lock().retain(|subscriber| {
                                subscriber.unbounded_send(event.clone()).is_ok()
                            });
                        }
                        Err(error) => log::warn!("cdp: unparseable frame {text:?}: {error}"),
                    }
                }
                closed.store(true, Ordering::SeqCst);
                // Fail every in-flight request so callers never hang on a dead socket.
                for (_, sender) in pending.lock().drain() {
                    sender.send(Err(anyhow!("cdp: connection closed"))).ok();
                }
                subscribers.lock().clear();
            }
        })
        .detach();

        Ok(Self {
            outgoing,
            pending,
            subscribers,
            next_id: Arc::new(AtomicU64::new(1)),
            closed,
            sleep,
        })
    }

    /// True once the socket has closed or failed; every later call errors.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// A fresh receiver for every event on the connection, including events
    /// from attached sessions. The receiver closes when the connection does.
    pub fn events(&self) -> mpsc::UnboundedReceiver<Event> {
        let (sender, receiver) = mpsc::unbounded();
        self.subscribers.lock().push(sender);
        receiver
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.send(method, params, None, CALL_TIMEOUT).await
    }

    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        self.send(method, params, None, timeout).await
    }

    pub async fn call_in_session(
        &self,
        session_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.send(method, params, Some(session_id.to_string()), CALL_TIMEOUT)
            .await
    }

    async fn send(
        &self,
        method: &str,
        params: serde_json::Value,
        session_id: Option<String>,
        timeout: Duration,
    ) -> Result<serde_json::Value> {
        if self.is_closed() {
            return Err(anyhow!("cdp {method}: connection closed"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, method, params, session_id);
        let text = serde_json::to_string(&request)?;
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().insert(id, sender);
        if self.outgoing.unbounded_send(text).is_err() {
            self.pending.lock().remove(&id);
            return Err(anyhow!("cdp {method}: connection closed"));
        }
        let reply = async {
            receiver
                .await
                .unwrap_or_else(|_| Err(anyhow!("cdp {method}: connection closed")))
        };
        let expire = async {
            (self.sleep)(timeout).await;
            // Drop the sender so a late reply is logged as unknown rather than leaking.
            self.pending.lock().remove(&id);
            Err(anyhow!(
                "cdp {method}: no reply within {} s",
                timeout.as_secs()
            ))
        };
        futures::pin_mut!(reply);
        futures::pin_mut!(expire);
        let outcome = match futures::future::select(reply, expire).await {
            futures::future::Either::Left((outcome, _))
            | futures::future::Either::Right((outcome, _)) => outcome,
        };
        outcome.with_context(|| format!("cdp {method}"))
    }

    pub async fn get_targets(&self) -> Result<Vec<TargetInfo>> {
        let result = self
            .call("Target.getTargets", serde_json::json!({}))
            .await?;
        let targets = serde_json::from_value(result["targetInfos"].clone())
            .context("cdp Target.getTargets: unexpected shape")?;
        Ok(targets)
    }

    pub async fn attach_to_target(&self, target_id: &str) -> Result<CdpSession> {
        let result = self
            .call(
                "Target.attachToTarget",
                serde_json::json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session_id = result["sessionId"]
            .as_str()
            .context("cdp Target.attachToTarget: no sessionId")?
            .to_string();
        Ok(CdpSession {
            client: self.clone(),
            session_id,
        })
    }
}

fn loopback_address(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("ws://")
        .with_context(|| format!("cdp: only ws:// loopback URLs are supported, got {url}"))?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    anyhow::ensure!(
        host_port.starts_with("127.0.0.1:") || host_port.starts_with("localhost:"),
        "cdp: refusing non-loopback host in {url}"
    );
    Ok(host_port.to_string())
}

/// A flat-mode session attached to one target; every call carries `sessionId`.
#[derive(Clone)]
pub struct CdpSession {
    client: CdpClient,
    session_id: String,
}

impl CdpSession {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The browser-level connection, for events that arrive without a
    /// `sessionId` but concern this session (`Target.detachedFromTarget`).
    pub fn client(&self) -> &CdpClient {
        &self.client
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.client
            .call_in_session(&self.session_id, method, params)
            .await
    }

    /// Events for this session only.
    pub fn events(&self) -> mpsc::UnboundedReceiver<Event> {
        let (sender, receiver) = mpsc::unbounded();
        let session_id = self.session_id.clone();
        let mut all = self.client.events();
        smol::spawn(async move {
            while let Some(event) = all.next().await {
                if event.session_id.as_deref() == Some(session_id.as_str())
                    && sender.unbounded_send(event).is_err()
                {
                    break;
                }
            }
        })
        .detach();
        receiver
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "these tests drive a real socket, so they need a wall clock, not the test scheduler's"
)]
mod tests {
    use super::*;
    use smol::net::TcpListener;

    fn wall_clock() -> Sleep {
        Arc::new(|duration| {
            Box::pin(async move {
                smol::Timer::after(duration).await;
            })
        })
    }

    /// Mirrors Chrome's browser endpoint closely enough to exercise the
    /// client: `Slow` is answered only after the next `Fast`, `Emit` pushes
    /// an event before its reply, `Garbage` sends a non-JSON frame first.
    async fn fake_server(listener: TcpListener) {
        loop {
            let (stream, _) = listener.accept().await.expect("accept");
            smol::spawn(async move {
                let mut socket = async_tungstenite::accept_async(stream)
                    .await
                    .expect("handshake");
                let mut parked: Option<u64> = None;
                while let Some(Ok(message)) = socket.next().await {
                    let Message::Text(text) = message else {
                        continue;
                    };
                    let request: serde_json::Value = serde_json::from_str(&text).expect("json");
                    let id = request["id"].as_u64().expect("id");
                    let method = request["method"].as_str().expect("method");
                    let session = request
                        .get("sessionId")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let mut frames: Vec<String> = Vec::new();
                    match method {
                        "Target.getTargets" => frames.push(format!(
                            r#"{{"id":{id},"result":{{"targetInfos":[{{"targetId":"T1","type":"page","title":"t","url":"https://example.com","attached":false}}]}}}}"#
                        )),
                        "Target.attachToTarget" => {
                            assert_eq!(request["params"]["flatten"], true);
                            frames.push(format!(r#"{{"id":{id},"result":{{"sessionId":"S1"}}}}"#));
                        }
                        "Slow" => {
                            parked = Some(id);
                        }
                        "Fast" => {
                            frames.push(format!(r#"{{"id":{id},"result":{{"which":"fast"}}}}"#));
                            if let Some(slow) = parked.take() {
                                frames.push(format!(
                                    r#"{{"id":{slow},"result":{{"which":"slow"}}}}"#
                                ));
                            }
                        }
                        "Emit" => {
                            frames.push(format!(
                                r#"{{"method":"Overlay.inspectNodeRequested","params":{{"backendNodeId":7}},"sessionId":{session}}}"#
                            ));
                            frames.push(format!(r#"{{"id":{id},"result":{{}}}}"#));
                        }
                        "Garbage" => {
                            frames.push("this is not json".to_string());
                            frames.push(format!(
                                r#"{{"id":{id},"result":{{"after":"garbage"}}}}"#
                            ));
                        }
                        "Echo" => frames.push(format!(
                            r#"{{"id":{id},"result":{{"session":{session}}}}}"#
                        )),
                        "Big" => {
                            let blob = "a".repeat(BIG_REPLY_BYTES);
                            frames.push(format!(r#"{{"id":{id},"result":{{"blob":"{blob}"}}}}"#));
                        }
                        "Hangup" => return,
                        _ => frames.push(format!(
                            r#"{{"id":{id},"error":{{"code":-32601,"message":"'{method}' wasn't found"}}}}"#
                        )),
                    }
                    for frame in frames {
                        socket
                            .send(Message::Text(frame.into()))
                            .await
                            .expect("send");
                    }
                }
            })
            .detach();
        }
    }

    /// Over tungstenite's 16 MiB default frame limit.
    const BIG_REPLY_BYTES: usize = 20 * 1024 * 1024;

    async fn start() -> CdpClient {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        smol::spawn(fake_server(listener)).detach();
        CdpClient::connect(
            &format!("ws://127.0.0.1:{port}/devtools/browser/x"),
            wall_clock(),
        )
        .await
        .expect("connect")
    }

    #[test]
    fn correlates_out_of_order_responses() {
        smol::block_on(async {
            let client = start().await;
            let slow = client.call("Slow", serde_json::json!({}));
            let fast = client.call("Fast", serde_json::json!({}));
            let (slow, fast) = futures::join!(slow, fast);
            assert_eq!(slow.expect("slow")["which"], "slow");
            assert_eq!(fast.expect("fast")["which"], "fast");
        });
    }

    #[test]
    fn surfaces_protocol_errors() {
        smol::block_on(async {
            let client = start().await;
            let error = client
                .call("Nope", serde_json::json!({}))
                .await
                .expect_err("error");
            assert!(format!("{error:#}").contains("wasn't found"), "{error:#}");
        });
    }

    #[test]
    fn delivers_events_to_every_subscriber() {
        smol::block_on(async {
            let client = start().await;
            let mut first = client.events();
            let dropped = client.events();
            drop(dropped);
            let mut second = client.events();
            client
                .call("Emit", serde_json::json!({}))
                .await
                .expect("emit");
            let event = first.next().await.expect("event");
            assert_eq!(event.method, "Overlay.inspectNodeRequested");
            assert_eq!(event.params["backendNodeId"], 7);
            assert_eq!(
                second.next().await.expect("event").method,
                "Overlay.inspectNodeRequested"
            );
        });
    }

    #[test]
    fn survives_a_malformed_frame() {
        smol::block_on(async {
            let client = start().await;
            let result = client
                .call("Garbage", serde_json::json!({}))
                .await
                .expect("still alive");
            assert_eq!(result["after"], "garbage");
            let targets = client.get_targets().await.expect("targets");
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0].kind, "page");
        });
    }

    #[test]
    fn attaches_a_flat_session_and_tags_requests() {
        smol::block_on(async {
            let client = start().await;
            let targets = client.get_targets().await.expect("targets");
            let session = client
                .attach_to_target(&targets[0].target_id)
                .await
                .expect("attach");
            assert_eq!(session.session_id(), "S1");
            let echo = session
                .call("Echo", serde_json::json!({}))
                .await
                .expect("echo");
            assert_eq!(echo["session"], "S1");
            let mut events = session.events();
            session
                .call("Emit", serde_json::json!({}))
                .await
                .expect("emit");
            assert_eq!(
                events.next().await.expect("event").session_id.as_deref(),
                Some("S1")
            );
        });
    }

    #[test]
    fn receives_replies_larger_than_the_default_frame_limit() {
        smol::block_on(async {
            let client = start().await;
            let big = client
                .call("Big", serde_json::json!({}))
                .await
                .expect("big reply");
            assert_eq!(big["blob"].as_str().map(str::len), Some(BIG_REPLY_BYTES));
            assert!(!client.is_closed());
        });
    }

    #[test]
    fn calls_time_out_instead_of_hanging() {
        smol::block_on(async {
            let client = start().await;
            let error = client
                .call_with_timeout("Slow", serde_json::json!({}), Duration::from_millis(100))
                .await
                .expect_err("timeout");
            assert!(format!("{error:#}").contains("no reply"), "{error:#}");
            // The connection itself is still fine.
            let fast = client
                .call("Fast", serde_json::json!({}))
                .await
                .expect("fast");
            assert_eq!(fast["which"], "fast");
        });
    }

    #[test]
    fn reports_closed_after_the_peer_hangs_up() {
        smol::block_on(async {
            let client = start().await;
            assert!(!client.is_closed());
            let error = client
                .call("Hangup", serde_json::json!({}))
                .await
                .expect_err("hung up");
            assert!(format!("{error:#}").contains("closed"), "{error:#}");
            for _ in 0..50 {
                if client.is_closed() {
                    break;
                }
                smol::Timer::after(Duration::from_millis(20)).await;
            }
            assert!(client.is_closed());
            assert!(client.call("Fast", serde_json::json!({})).await.is_err());
        });
    }

    #[test]
    fn connect_to_closed_port_fails_cleanly() {
        smol::block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let port = listener.local_addr().expect("addr").port();
            drop(listener);
            assert!(
                CdpClient::connect(&format!("ws://127.0.0.1:{port}/x"), wall_clock())
                    .await
                    .is_err()
            );
        });
    }
}
