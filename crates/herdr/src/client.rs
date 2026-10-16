use crate::protocol::{Event, Incoming, Request, Response, SUBSCRIPTIONS, Subscription};
use anyhow::{Context as _, Result, anyhow};
use futures::StreamExt;
use futures::channel::mpsc;
use smol::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use smol::net::unix::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn socket_path() -> PathBuf {
    paths::home_dir()
        .join(".config")
        .join("herdr")
        .join("herdr.sock")
}

/// herdr answers exactly one request per connection and then closes it; only
/// `events.subscribe` keeps a connection open. So every request dials the
/// socket afresh, and the event stream owns its own connection.
#[derive(Clone)]
pub struct HerdrClient {
    path: PathBuf,
    next_id: Arc<AtomicU64>,
}

impl HerdrClient {
    pub async fn connect(
        path: PathBuf,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>, smol::Task<()>)> {
        let client = Self {
            path,
            next_id: Arc::new(AtomicU64::new(1)),
        };
        client.request("ping", serde_json::json!({})).await?;
        let (events, reader) = client.subscribe_all().await?;
        Ok((client, events, reader))
    }

    async fn dial(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.path)
            .await
            .with_context(|| format!("connecting to herdr at {}", self.path.display()))
    }

    fn next_request(&self, method: &str, params: serde_json::Value) -> Result<(Request, String)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, method, params);
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        Ok((request, line))
    }

    /// Bounds every socket round trip so a wedged herdr cannot hang the caller.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    async fn with_timeout<T>(
        what: &str,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        // Production I/O timeout; the test-determinism concern behind the lint
        // does not apply to a real socket.
        #[allow(clippy::disallowed_methods)]
        let timer = async {
            smol::Timer::after(Self::TIMEOUT).await;
            Err(anyhow!("herdr {what}: timed out after {:?}", Self::TIMEOUT))
        };
        smol::future::or(future, timer).await
    }

    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let method_name = method.to_string();
        Self::with_timeout(&method_name, self.request_inner(method, params)).await
    }

    async fn request_inner(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let (request, line) = self.next_request(method, params)?;
        let mut stream = self.dial().await?;
        stream.write_all(line.as_bytes()).await?;
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        reader.read_line(&mut reply).await?;
        if reply.trim().is_empty() {
            return Err(anyhow!("herdr {}: connection closed before reply", method));
        }
        let response: Response = serde_json::from_str(&reply)
            .with_context(|| format!("herdr {}: unparseable reply {reply:?}", method))?;
        if let Some(error) = &response.error {
            return Err(anyhow!("herdr {}: {} ({})", method, error.message, error.code));
        }
        if response.id != request.id {
            return Err(anyhow!(
                "herdr {}: reply id {} does not match request id {}",
                method,
                response.id,
                request.id
            ));
        }
        match (response.result, response.error) {
            (Some(result), _) => Ok(result),
            (None, Some(error)) => Err(anyhow!(
                "herdr {}: {} ({})",
                method,
                error.message,
                error.code
            )),
            (None, None) => Err(anyhow!("herdr {}: empty response", method)),
        }
    }

    /// Opens the long-lived subscription connection. The returned receiver
    /// closes when herdr closes the connection; dropping the returned task
    /// closes the connection from our side.
    pub async fn subscribe_all(
        &self,
    ) -> Result<(mpsc::UnboundedReceiver<Event>, smol::Task<()>)> {
        let subscriptions: Vec<Subscription> = SUBSCRIPTIONS
            .iter()
            .map(|kind| Subscription { kind })
            .collect();
        let (request, line) = self.next_request(
            "events.subscribe",
            serde_json::json!({ "subscriptions": subscriptions }),
        )?;
        let mut stream = Self::with_timeout("events.subscribe dial", self.dial()).await?;
        stream.write_all(line.as_bytes()).await?;
        let mut lines = BufReader::new(stream).lines();
        let ack = Self::with_timeout("events.subscribe ack", async {
            lines
                .next()
                .await
                .context("herdr closed the subscription before acknowledging it")?
                .map_err(anyhow::Error::from)
        })
        .await?;
        let ack: Response = serde_json::from_str(&ack)
            .with_context(|| format!("herdr events.subscribe: unparseable reply {ack:?}"))?;
        if ack.id != request.id || ack.error.is_some() {
            return Err(anyhow!("herdr events.subscribe refused: {ack:?}"));
        }

        let (event_tx, event_rx) = mpsc::unbounded();
        let reader = smol::spawn(async move {
            while let Some(line) = lines.next().await {
                let line = match line {
                    Ok(line) => line,
                    Err(error) => {
                        log::warn!("herdr event stream read failed: {error}");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Incoming>(&line) {
                    Ok(Incoming::Event(event)) => {
                        if event_tx.unbounded_send(event).is_err() {
                            break;
                        }
                    }
                    Ok(Incoming::Response(response)) => {
                        log::warn!("herdr: unexpected response on event stream: {response:?}");
                    }
                    Err(error) => log::warn!("herdr: unparseable event line {line:?}: {error}"),
                }
            }
            // event_tx drops here, closing the receiver so the owner reconnects.
        });
        Ok((event_rx, reader))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use smol::net::unix::UnixListener;

    /// Mirrors herdr: one request per connection, except events.subscribe
    /// which acknowledges and then streams.
    async fn fake_server(listener: UnixListener) {
        loop {
            let (stream, _) = listener.accept().await.expect("accept");
            smol::spawn(async move {
                let (reader, mut writer) = smol::io::split(stream);
                let mut lines = BufReader::new(reader).lines();
                let Some(Ok(line)) = lines.next().await else { return };
                let request: serde_json::Value = serde_json::from_str(&line).expect("json");
                let id = request["id"].as_str().expect("id").to_string();
                let method = request["method"].as_str().expect("method");
                let reply = match method {
                    "ping" => format!(r#"{{"id":"{id}","result":{{"type":"pong","protocol":16}}}}"#),
                    "events.subscribe" => {
                        let ack = format!(r#"{{"id":"{id}","result":{{"type":"subscription_started"}}}}"#);
                        let event = r#"{"event":"pane_agent_status_changed","data":{"type":"pane_agent_status_changed","pane_id":"w1:p1","workspace_id":"w1","agent":"claude","agent_status":"blocked","display_agent":null,"title":null,"state_labels":{}}}"#;
                        format!("{ack}\n{event}")
                    }
                    _ => format!(r#"{{"id":"{id}","error":{{"code":"unknown","message":"no"}}}}"#),
                };
                writer
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .expect("write");
                if method == "events.subscribe" {
                    // keep the stream open like herdr does; the task is dropped with the test
                    futures::future::pending::<()>().await;
                }
            })
            .detach();
        }
    }

    #[test]
    fn ping_round_trip_and_event_delivery() {
        smol::block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("h.sock");
            let listener = UnixListener::bind(&path).expect("bind");
            smol::spawn(fake_server(listener)).detach();

            let (client, mut events, _reader) =
                HerdrClient::connect(path).await.expect("connect");
            let pong = client
                .request("ping", serde_json::json!({}))
                .await
                .expect("ping");
            assert_eq!(pong["type"], "pong");

            let event = events.next().await.expect("one event");
            assert_eq!(event.event, "pane_agent_status_changed");

            let pong_again = client
                .request("ping", serde_json::json!({}))
                .await
                .expect("second ping on a fresh connection");
            assert_eq!(pong_again["type"], "pong");

            let err = client.request("nope", serde_json::json!({})).await;
            assert!(err.is_err());
        });
    }

    #[test]
    fn connect_to_missing_socket_fails_cleanly() {
        smol::block_on(async {
            let result =
                HerdrClient::connect(std::path::PathBuf::from("/nonexistent/h.sock")).await;
            assert!(result.is_err());
        });
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Runs only against a real herdr: `cargo test -p herdr -- --ignored live_`
    #[test]
    #[ignore]
    fn live_snapshot_round_trip() {
        smol::block_on(async {
            let (client, events, _reader) =
                HerdrClient::connect(socket_path()).await.expect("connect");
            let snapshot = client
                .request("session.snapshot", serde_json::json!({}))
                .await
                .expect("snapshot");
            assert!(snapshot["snapshot"]["workspaces"].is_array());
            let list = client
                .request("workspace.list", serde_json::json!({}))
                .await
                .expect("second request");
            assert!(list["workspaces"].is_array());
            drop(events);
        });
    }
}
