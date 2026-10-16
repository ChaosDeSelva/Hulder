# Browser Picker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hulder drives the user's real Chrome over the DevTools Protocol: `browser::Open` launches Chrome on a URL, `browser::PickElement` turns on Chrome's inspect overlay, and the clicked element lands in a read-only Markdown buffer (URL, selector, outer HTML, computed styles, screenshot path).

**Architecture:** One new crate `cdp` holds a runtime-agnostic Chrome DevTools Protocol client over a loopback WebSocket (`async-tungstenite` with the default `handshake` feature on a `smol::net::TcpStream`): requests correlated by `id`, a fan-out event stream, flat session attach. A second new crate `browser` holds `ChromeRunner` (a global gpui entity that launches Chrome, discovers the port from `DevToolsActivePort`, attaches to a page target, and runs the pick flow), the pure `pick` module (selector builder, Markdown rendering), the URL prompt modal, and the workspace actions. `zed` wires it in `main.rs`, `app_menus.rs`, the default keymap and `default.json` exactly like `herd_panel`.

**Tech Stack:** Rust 1.98, gpui (entities, `cx.spawn`, `cx.background_spawn`), smol 2 for TCP, `async-tungstenite` 0.33 for WebSocket framing, serde/serde_json for the wire, `base64` for screenshots, `workspace::Toast`, `workspace::ModalView`, `editor::Editor::for_buffer` + `set_read_only(true)`.

**Spec:** `docs/superpowers/specs/2026-10-13-hulder-design.md`, section 7.1 (phase one only; 7.2 is out of scope).

## Global Constraints

- CDP wire format: request `{"id": u64, "method": string, "params": object, "sessionId"?: string}`. Success `{"id", "result": {...}, "sessionId"?}`. Error `{"id", "error": {"code": i64, "message": string}}`. Event `{"method": string, "params": {...}, "sessionId"?}`. Everything is a WebSocket text frame.
- Port discovery reads `<user-data-dir>/DevToolsActivePort` (line 1: port, line 2: browser WebSocket path such as `/devtools/browser/<uuid>`). The app's HTTP client is deliberately blocked and must never be used; `ws://127.0.0.1:<port><path>` is dialed directly over TCP.
- Chrome binary: `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`, fallback `/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge`, overridden by `browser.binary`. Args: `--remote-debugging-port=0 --user-data-dir=<paths::data_dir()>/chrome --no-first-run --no-default-browser-check --disable-extensions --disable-sync --no-service-autorun --password-store=basic --new-window -- <url>`.
- The debug port is unauthenticated, so: the URL is validated with `url::Url` (http, https, file, about only; never starting with `-`) and placed after the `--` switch terminator; the profile directory is created 0700 and refused when it is a symlink or owned by another uid; the Chrome child is killed on drop and from `cx.on_app_quit`.
- Pick flow (all in the page session): `Overlay.enable`, `DOM.enable`, `CSS.enable`, `DOM.getDocument {depth:-1}`, `Overlay.setInspectMode {mode:"searchForNode", highlightConfig:{showInfo:true, contentColor:{r:111,g:168,b:220,a:0.66}}}`, wait for `Overlay.inspectNodeRequested {backendNodeId}`, `Overlay.setInspectMode {mode:"none"}`, `DOM.pushNodesByBackendIdsToFrontend {backendNodeIds:[id]}`, `DOM.getOuterHTML {nodeId}`, `DOM.describeNode {nodeId}`, `CSS.getComputedStyleForNode {nodeId}`, `DOM.getBoxModel {nodeId}`, `Page.captureScreenshot {format:"png", clip:{x,y,width,height,scale:1}}`.
- Screenshots are saved under `paths::data_dir()/picks/<unix-ms>.png`.
- `SendPickToAgent` stays registered but only shows the toast "Send pick to agent is not yet wired".
- Follow repo CLAUDE.md: no `unwrap()` outside tests, no `let _ =` on fallible calls, no `mod.rs`, 2-space JSON, `./script/clippy` not `cargo clippy`.
- Keep README's `> [!IMPORTANT]` marker in place.
- Do not edit `crates/herdr`, `crates/herd_panel`, `crates/project_panel`, `crates/editor`, or `crates/lightpanda`. Integration edits to `Cargo.toml`, `crates/zed`, `assets/` and `crates/settings_content` stay minimal and additive.

## Review Focus

1. Request correlation under interleaving: two in-flight requests answered out of order must each resolve to their own result. Test in Task 2.
2. A malformed frame must not kill the connection; it is logged and the next valid frame is still delivered. Test in Task 2.
3. Events are fanned out to every live subscriber and never block the reader when a subscriber is dropped. Test in Task 2.
4. Chrome never starts (bad binary, no `DevToolsActivePort` within the timeout): `Open` must surface a toast with the reason, never hang. Covered by the error path in Task 5 (manual) and the pure parsing test in Task 4.
5. Selector building is a pure function over the DOM tree returned by `DOM.getDocument`: ids short-circuit the ancestry, classes are appended, `:nth-of-type` only appears when needed. Test in Task 3.

---

### Task 1: `cdp` crate scaffold and wire types

**Files:**
- Create: `crates/cdp/Cargo.toml`
- Create: `crates/cdp/src/cdp.rs`
- Create: `crates/cdp/src/protocol.rs`
- Modify: `Cargo.toml` (add `"crates/cdp"` to members next to `"crates/herdr"`; add `cdp = { path = "crates/cdp" }` under `[workspace.dependencies]` next to `herdr`)

**Interfaces:**
- Produces: `cdp::protocol::{Request, Incoming, Response, ErrorBody, Event, TargetInfo}`; `Request::new(id, method, params, session_id: Option<String>)`.

- [ ] **Step 1: Write the failing test**

`crates/cdp/src/protocol.rs` (tests at the bottom):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_omits_session_id_when_absent() {
        let request = Request::new(3, "Target.getTargets", serde_json::json!({}), None);
        let text = serde_json::to_string(&request).expect("serialize");
        assert_eq!(text, r#"{"id":3,"method":"Target.getTargets","params":{}}"#);

        let request = Request::new(4, "DOM.enable", serde_json::json!({}), Some("S1".into()));
        let text = serde_json::to_string(&request).expect("serialize");
        assert_eq!(text, r#"{"id":4,"method":"DOM.enable","params":{},"sessionId":"S1"}"#);
    }

    #[test]
    fn parses_result_error_and_event_frames() {
        let ok: Incoming = serde_json::from_str(r#"{"id":1,"result":{"targetInfos":[]}}"#).expect("ok");
        assert!(matches!(ok, Incoming::Response(Response { id: 1, result: Some(_), error: None, .. })));

        let err: Incoming =
            serde_json::from_str(r#"{"id":2,"error":{"code":-32601,"message":"'X' wasn't found"}}"#)
                .expect("err");
        assert!(matches!(err, Incoming::Response(Response { id: 2, error: Some(ref e), .. }) if e.code == -32601));

        let event: Incoming = serde_json::from_str(
            r#"{"method":"Overlay.inspectNodeRequested","params":{"backendNodeId":42},"sessionId":"S1"}"#,
        )
        .expect("event");
        match event {
            Incoming::Event(Event { method, params, session_id }) => {
                assert_eq!(method, "Overlay.inspectNodeRequested");
                assert_eq!(params["backendNodeId"], 42);
                assert_eq!(session_id.as_deref(), Some("S1"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p cdp`
Expected: fails to compile, crate does not exist.

- [ ] **Step 3: Write the crate**

`crates/cdp/Cargo.toml`:

```toml
[package]
name = "cdp"
version = "0.1.0"
edition.workspace = true
publish.workspace = true
license = "GPL-3.0-or-later"

[lints]
workspace = true

[lib]
path = "src/cdp.rs"
doctest = false

[dependencies]
anyhow.workspace = true
async-tungstenite.workspace = true
collections.workspace = true
futures.workspace = true
log.workspace = true
parking_lot.workspace = true
serde.workspace = true
serde_json.workspace = true
smol.workspace = true
```

`crates/cdp/src/cdp.rs`:

```rust
pub mod client;
pub mod protocol;

pub use client::{CdpClient, CdpSession};
pub use protocol::*;
```

`crates/cdp/src/protocol.rs`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl Request {
    pub fn new(id: u64, method: &str, params: serde_json::Value, session_id: Option<String>) -> Self {
        Self { id, method: method.to_string(), params, session_id }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorBody {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<ErrorBody>,
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Response(Response),
    Event(Event),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub target_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub url: String,
    pub attached: bool,
}
```

(`client` is created in Task 2; create it as an empty file so the crate compiles.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p cdp`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/cdp
git commit -m "cdp: Add crate scaffold and protocol wire types

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 2: WebSocket client with request correlation, event fan-out and flat sessions

**Files:**
- Create: `crates/cdp/src/client.rs`

**Interfaces:**
- Produces: `CdpClient::connect(url: &str) -> Result<CdpClient>`, `CdpClient::call(method, params) -> Result<Value>`, `CdpClient::call_in_session(session_id, method, params)`, `CdpClient::events() -> mpsc::UnboundedReceiver<Event>`, `CdpClient::get_targets() -> Result<Vec<TargetInfo>>`, `CdpClient::attach_to_target(target_id) -> Result<CdpSession>`, `CdpSession::{call, events, session_id}`.
- Connect dials `smol::net::TcpStream` at the URL's host:port, then `async_tungstenite::client_async(url, stream)`. The reader task is spawned with `smol::spawn` (same as `herdr`), so the client works outside gpui in unit tests.

- [ ] **Step 1: Write the failing tests**

A fake server built from `smol::net::TcpListener` + `async_tungstenite::accept_async`. It answers `Target.getTargets` with one page target, `Target.attachToTarget` with a session id, `Slow` by answering only after the next `Fast` request (out of order), `Emit` by sending an event frame before the response, `Garbage` by sending a non-JSON text frame followed by the response, and anything else with an error.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use async_tungstenite::tungstenite::Message;
    use futures::{SinkExt, StreamExt};
    use smol::net::TcpListener;

    async fn fake_server(listener: TcpListener) {
        loop {
            let (stream, _) = listener.accept().await.expect("accept");
            smol::spawn(async move {
                let mut socket = async_tungstenite::accept_async(stream).await.expect("handshake");
                let mut parked: Option<u64> = None;
                while let Some(Ok(message)) = socket.next().await {
                    let Message::Text(text) = message else { continue };
                    let request: serde_json::Value = serde_json::from_str(&text).expect("json");
                    let id = request["id"].as_u64().expect("id");
                    let method = request["method"].as_str().expect("method");
                    let session = request.get("sessionId").cloned().unwrap_or(serde_json::Value::Null);
                    let mut frames: Vec<String> = Vec::new();
                    match method {
                        "Target.getTargets" => frames.push(format!(
                            r#"{{"id":{id},"result":{{"targetInfos":[{{"targetId":"T1","type":"page","title":"t","url":"https://example.com","attached":false}}]}}}}"#
                        )),
                        "Target.attachToTarget" => {
                            assert_eq!(request["params"]["flatten"], true);
                            frames.push(format!(r#"{{"id":{id},"result":{{"sessionId":"S1"}}}}"#));
                        }
                        "Slow" => { parked = Some(id); }
                        "Fast" => {
                            frames.push(format!(r#"{{"id":{id},"result":{{"which":"fast"}}}}"#));
                            if let Some(slow) = parked.take() {
                                frames.push(format!(r#"{{"id":{slow},"result":{{"which":"slow"}}}}"#));
                            }
                        }
                        "Emit" => {
                            frames.push(format!(r#"{{"method":"Overlay.inspectNodeRequested","params":{{"backendNodeId":7}},"sessionId":{session}}}"#));
                            frames.push(format!(r#"{{"id":{id},"result":{{}}}}"#));
                        }
                        "Garbage" => {
                            frames.push("this is not json".to_string());
                            frames.push(format!(r#"{{"id":{id},"result":{{"after":"garbage"}}}}"#));
                        }
                        "Echo" => frames.push(format!(r#"{{"id":{id},"result":{{"session":{session}}}}}"#)),
                        _ => frames.push(format!(r#"{{"id":{id},"error":{{"code":-32601,"message":"'{method}' wasn't found"}}}}"#)),
                    }
                    for frame in frames {
                        socket.send(Message::Text(frame.into())).await.expect("send");
                    }
                }
            })
            .detach();
        }
    }

    async fn start() -> CdpClient {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        smol::spawn(fake_server(listener)).detach();
        CdpClient::connect(&format!("ws://127.0.0.1:{port}/devtools/browser/x")).await.expect("connect")
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
            let error = client.call("Nope", serde_json::json!({})).await.expect_err("error");
            assert!(error.to_string().contains("wasn't found"), "{error}");
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
            client.call("Emit", serde_json::json!({})).await.expect("emit");
            let event = first.next().await.expect("event");
            assert_eq!(event.method, "Overlay.inspectNodeRequested");
            assert_eq!(event.params["backendNodeId"], 7);
            assert_eq!(second.next().await.expect("event").method, "Overlay.inspectNodeRequested");
        });
    }

    #[test]
    fn survives_a_malformed_frame() {
        smol::block_on(async {
            let client = start().await;
            let result = client.call("Garbage", serde_json::json!({})).await.expect("still alive");
            assert_eq!(result["after"], "garbage");
            let targets = client.get_targets().await.expect("targets");
            assert_eq!(targets.len(), 1);
        });
    }

    #[test]
    fn attaches_a_flat_session_and_tags_requests() {
        smol::block_on(async {
            let client = start().await;
            let targets = client.get_targets().await.expect("targets");
            let session = client.attach_to_target(&targets[0].target_id).await.expect("attach");
            assert_eq!(session.session_id(), "S1");
            let echo = session.call("Echo", serde_json::json!({})).await.expect("echo");
            assert_eq!(echo["session"], "S1");
            let mut events = session.events();
            session.call("Emit", serde_json::json!({})).await.expect("emit");
            assert_eq!(events.next().await.expect("event").session_id.as_deref(), Some("S1"));
        });
    }

    #[test]
    fn connect_to_closed_port_fails_cleanly() {
        smol::block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let port = listener.local_addr().expect("addr").port();
            drop(listener);
            assert!(CdpClient::connect(&format!("ws://127.0.0.1:{port}/x")).await.is_err());
        });
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p cdp`
Expected: compile error, `CdpClient` missing.

- [ ] **Step 3: Write the client**

`crates/cdp/src/client.rs`:

```rust
use crate::protocol::{Event, Incoming, Request, TargetInfo};
use anyhow::{Context as _, Result, anyhow};
use async_tungstenite::tungstenite::Message;
use collections::HashMap;
use futures::channel::{mpsc, oneshot};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use smol::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

type Pending = HashMap<u64, oneshot::Sender<Result<serde_json::Value>>>;

#[derive(Clone)]
pub struct CdpClient {
    outgoing: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<Pending>>,
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<Event>>>>,
    next_id: Arc<AtomicU64>,
}

impl CdpClient {
    pub async fn connect(url: &str) -> Result<Self> {
        let address = loopback_address(url)?;
        let stream = TcpStream::connect(&address).await.with_context(|| format!("connecting to {address}"))?;
        let (socket, _) = async_tungstenite::client_async(url, stream).await.with_context(|| format!("websocket handshake with {url}"))?;
        let (mut sink, mut source) = socket.split();
        let (outgoing, mut outgoing_rx) = mpsc::unbounded::<String>();
        let pending: Arc<Mutex<Pending>> = Arc::default();
        let subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<Event>>>> = Arc::default();

        smol::spawn(async move {
            while let Some(text) = outgoing_rx.next().await {
                if let Err(error) = sink.send(Message::Text(text.into())).await {
                    log::warn!("cdp: write failed: {error}");
                    break;
                }
            }
        }).detach();

        smol::spawn({
            let pending = pending.clone();
            let subscribers = subscribers.clone();
            async move {
                while let Some(message) = source.next().await {
                    let text = match message {
                        Ok(Message::Text(text)) => text,
                        Ok(Message::Close(_)) | Err(_) => break,
                        Ok(_) => continue,
                    };
                    match serde_json::from_str::<Incoming>(&text) {
                        Ok(Incoming::Response(response)) => {
                            let Some(sender) = pending.lock().remove(&response.id) else {
                                log::warn!("cdp: response for unknown id {}", response.id);
                                continue;
                            };
                            let outcome = match (response.result, response.error) {
                                (_, Some(error)) => Err(anyhow!("{} ({})", error.message, error.code)),
                                (Some(result), None) => Ok(result),
                                (None, None) => Ok(serde_json::Value::Null),
                            };
                            sender.send(outcome).ok();
                        }
                        Ok(Incoming::Event(event)) => {
                            subscribers.lock().retain(|subscriber| subscriber.unbounded_send(event.clone()).is_ok());
                        }
                        Err(error) => log::warn!("cdp: unparseable frame {text:?}: {error}"),
                    }
                }
                // Fail every in-flight request so callers never hang on a dead socket.
                for (_, sender) in pending.lock().drain() {
                    sender.send(Err(anyhow!("cdp: connection closed"))).ok();
                }
                subscribers.lock().clear();
            }
        }).detach();

        Ok(Self { outgoing, pending, subscribers, next_id: Arc::new(AtomicU64::new(1)) })
    }

    pub fn events(&self) -> mpsc::UnboundedReceiver<Event> {
        let (sender, receiver) = mpsc::unbounded();
        self.subscribers.lock().push(sender);
        receiver
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.send(method, params, None).await
    }

    pub async fn call_in_session(&self, session_id: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.send(method, params, Some(session_id.to_string())).await
    }

    async fn send(&self, method: &str, params: serde_json::Value, session_id: Option<String>) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, method, params, session_id);
        let text = serde_json::to_string(&request)?;
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().insert(id, sender);
        if self.outgoing.unbounded_send(text).is_err() {
            self.pending.lock().remove(&id);
            return Err(anyhow!("cdp {method}: connection closed"));
        }
        receiver.await.unwrap_or_else(|_| Err(anyhow!("cdp {method}: connection closed")))
            .with_context(|| format!("cdp {method}"))
    }

    pub async fn get_targets(&self) -> Result<Vec<TargetInfo>> {
        let result = self.call("Target.getTargets", serde_json::json!({})).await?;
        let targets = serde_json::from_value(result["targetInfos"].clone()).context("cdp Target.getTargets: unexpected shape")?;
        Ok(targets)
    }

    pub async fn attach_to_target(&self, target_id: &str) -> Result<CdpSession> {
        let result = self.call("Target.attachToTarget", serde_json::json!({ "targetId": target_id, "flatten": true })).await?;
        let session_id = result["sessionId"].as_str().context("cdp Target.attachToTarget: no sessionId")?.to_string();
        Ok(CdpSession { client: self.clone(), session_id })
    }
}

fn loopback_address(url: &str) -> Result<String> {
    let rest = url.strip_prefix("ws://").with_context(|| format!("cdp: only ws:// loopback URLs are supported, got {url}"))?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    anyhow::ensure!(host_port.starts_with("127.0.0.1:") || host_port.starts_with("localhost:"), "cdp: refusing non-loopback host in {url}");
    Ok(host_port.to_string())
}

#[derive(Clone)]
pub struct CdpSession { client: CdpClient, session_id: String }

impl CdpSession {
    pub fn session_id(&self) -> &str { &self.session_id }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.client.call_in_session(&self.session_id, method, params).await
    }

    /// Events for this session only.
    pub fn events(&self) -> mpsc::UnboundedReceiver<Event> {
        let (sender, receiver) = mpsc::unbounded();
        let session_id = self.session_id.clone();
        let mut all = self.client.events();
        smol::spawn(async move {
            while let Some(event) = all.next().await {
                if event.session_id.as_deref() == Some(session_id.as_str()) && sender.unbounded_send(event).is_err() {
                    break;
                }
            }
        }).detach();
        receiver
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p cdp`
Expected: all green, including the malformed-frame and out-of-order cases.

- [ ] **Step 5: Commit**

```bash
git add crates/cdp
git commit -m "cdp: Add loopback WebSocket client with request correlation and sessions

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 3: `browser` crate scaffold, settings, and the pure `pick` module

**Files:**
- Create: `crates/browser/Cargo.toml`
- Create: `crates/browser/src/browser.rs` (module list and `init` stub)
- Create: `crates/browser/src/pick.rs`
- Create: `crates/browser/src/chrome_runner.rs` (empty for now)
- Create: `crates/settings_content/src/browser.rs`
- Modify: `crates/settings_content/src/settings_content.rs` (`mod browser; pub use browser::*;`, field `pub browser: Option<BrowserSettingsContent>` before `pub node`, add `browser` to the `options:` list)
- Modify: `assets/settings/default.json` (`"browser": { "binary": null },` right before `"node": {`)
- Modify: `Cargo.toml` (members `"crates/browser"`, dependency `browser = { path = "crates/browser" }` next to `herdr`)

**Interfaces:**
- Produces: `browser::pick::{DomNode, Pick, selector_for, computed_table, pick_markdown}`; `browser::BrowserSettings { binary: Option<PathBuf> }` registered via `RegisterSetting`.

- [ ] **Step 1: Write the failing tests**

`crates/browser/src/pick.rs` (tests at the bottom):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn element(name: &str, attributes: &[(&str, &str)], backend_node_id: u64, children: Vec<DomNode>) -> DomNode {
        DomNode {
            node_type: 1,
            local_name: name.to_string(),
            attributes: attributes.iter().flat_map(|(key, value)| [key.to_string(), value.to_string()]).collect(),
            backend_node_id,
            children,
        }
    }

    fn document() -> DomNode {
        DomNode {
            node_type: 9,
            local_name: String::new(),
            attributes: vec![],
            backend_node_id: 1,
            children: vec![element("html", &[], 2, vec![element("body", &[], 3, vec![
                element("div", &[("class", "nav main")], 4, vec![]),
                element("div", &[("id", "content")], 5, vec![
                    element("p", &[], 6, vec![]),
                    element("p", &[("class", "lead")], 7, vec![element("a", &[("href", "/x")], 8, vec![])]),
                ]),
                element("span", &[], 9, vec![]),
            ])])],
        }
    }

    #[test]
    fn selector_uses_id_as_anchor_and_nth_of_type_only_when_ambiguous() {
        let root = document();
        assert_eq!(selector_for(&root, 8).as_deref(), Some("#content > p.lead:nth-of-type(2) > a"));
        assert_eq!(selector_for(&root, 4).as_deref(), Some("html > body > div.nav.main:nth-of-type(1)"));
        assert_eq!(selector_for(&root, 9).as_deref(), Some("html > body > span"));
        assert_eq!(selector_for(&root, 5).as_deref(), Some("#content"));
        assert_eq!(selector_for(&root, 999), None);
    }

    #[test]
    fn computed_table_keeps_the_ten_useful_properties_in_order() {
        let computed = vec![
            ("z-index".to_string(), "1".to_string()),
            ("font-size".to_string(), "16px".to_string()),
            ("display".to_string(), "block".to_string()),
            ("color".to_string(), "rgb(0, 0, 0)".to_string()),
        ];
        let rows = computed_table(&computed);
        assert_eq!(rows, vec![
            ("display", "block".to_string()),
            ("color", "rgb(0, 0, 0)".to_string()),
            ("font-size", "16px".to_string()),
        ]);
    }

    #[test]
    fn markdown_has_url_selector_html_table_and_screenshot() {
        let pick = Pick {
            url: "https://example.com/".into(),
            selector: "html > body > span".into(),
            outer_html: "<span>hi</span>".into(),
            computed: vec![("display".into(), "inline".into())],
            screenshot_png: vec![],
        };
        let markdown = pick_markdown(&pick, Path::new("/tmp/picks/1.png"));
        assert!(markdown.starts_with("# html > body > span\n"));
        assert!(markdown.contains("**URL:** https://example.com/\n"));
        assert!(markdown.contains("```html\n<span>hi</span>\n```"));
        assert!(markdown.contains("| display | inline |"));
        assert!(markdown.contains("/tmp/picks/1.png"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p browser`
Expected: fails to compile.

- [ ] **Step 3: Write the crate, settings and pick module**

`crates/browser/Cargo.toml`:

```toml
[package]
name = "browser"
version = "0.1.0"
edition.workspace = true
publish.workspace = true
license = "GPL-3.0-or-later"

[lints]
workspace = true

[lib]
path = "src/browser.rs"
doctest = false

[dependencies]
anyhow.workspace = true
base64.workspace = true
cdp.workspace = true
editor.workspace = true
futures.workspace = true
gpui.workspace = true
log.workspace = true
menu.workspace = true
paths.workspace = true
serde.workspace = true
serde_json.workspace = true
settings.workspace = true
smol.workspace = true
theme.workspace = true
ui.workspace = true
util.workspace = true
workspace.workspace = true
```

`crates/settings_content/src/browser.rs`:

```rust
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

#[with_fallible_options]
#[derive(Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug)]
pub struct BrowserSettingsContent {
    /// Path to the browser binary used by `browser::Open`. When unset, Hulder
    /// looks for Google Chrome and then Microsoft Edge in /Applications.
    ///
    /// Default: null
    pub binary: Option<String>,
}
```

`crates/browser/src/browser.rs` (this task: settings + modules; actions arrive in Task 5):

```rust
pub mod chrome_runner;
pub mod pick;

use gpui::App;
use settings::{RegisterSetting, Settings};
use std::path::PathBuf;

#[derive(Clone, Debug, RegisterSetting)]
pub struct BrowserSettings {
    pub binary: Option<PathBuf>,
}

impl Settings for BrowserSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let browser = content.browser.clone().unwrap();
        Self { binary: browser.binary.map(PathBuf::from) }
    }
}

pub fn init(_cx: &mut App) {}
```

`crates/browser/src/pick.rs`:

```rust
use serde::Deserialize;
use std::path::Path;

/// The subset of `DOM.Node` that selector building needs.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DomNode {
    pub node_type: u8,
    #[serde(default)]
    pub local_name: String,
    #[serde(default)]
    pub attributes: Vec<String>,
    pub backend_node_id: u64,
    #[serde(default)]
    pub children: Vec<DomNode>,
}

#[derive(Debug, Clone)]
pub struct Pick {
    pub url: String,
    pub selector: String,
    pub outer_html: String,
    pub computed: Vec<(String, String)>,
    pub screenshot_png: Vec<u8>,
}

pub const USEFUL_COMPUTED: [&str; 10] = [
    "display", "position", "width", "height", "color", "background-color",
    "font-family", "font-size", "margin", "padding",
];

impl DomNode {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.chunks(2).find(|pair| pair[0] == name).and_then(|pair| pair.get(1)).map(String::as_str)
    }
}

/// Builds `tag#id.class:nth-of-type(n)` steps from the nearest id-bearing ancestor
/// (or the root element) down to the node with `backend_node_id`.
pub fn selector_for(root: &DomNode, backend_node_id: u64) -> Option<String> {
    let mut path = Vec::new();
    if !find_path(root, backend_node_id, &mut path) { return None; }
    let mut steps: Vec<String> = Vec::new();
    for (parent, node) in path.iter().rev() {
        if let Some(id) = node.attribute("id").filter(|id| !id.is_empty()) {
            steps.push(format!("#{id}"));
            break;
        }
        let mut step = node.local_name.clone();
        if let Some(classes) = node.attribute("class") {
            for class in classes.split_whitespace() { step.push('.'); step.push_str(class); }
        }
        if let Some(parent) = parent {
            let same_type: Vec<&DomNode> = parent.children.iter().filter(|child| child.node_type == 1 && child.local_name == node.local_name).collect();
            if same_type.len() > 1 {
                let index = same_type.iter().position(|child| child.backend_node_id == node.backend_node_id)? + 1;
                step.push_str(&format!(":nth-of-type({index})"));
            }
        }
        steps.push(step);
    }
    steps.reverse();
    Some(steps.join(" > "))
}

/// Fills `path` with `(parent, node)` pairs from the target up to the root element.
fn find_path<'a>(node: &'a DomNode, backend_node_id: u64, path: &mut Vec<(Option<&'a DomNode>, &'a DomNode)>) -> bool {
    for child in &node.children {
        if child.node_type != 1 { continue; }
        if child.backend_node_id == backend_node_id || find_path(child, backend_node_id, path) {
            path.push((Some(node).filter(|parent| parent.node_type == 1), child));
            return true;
        }
    }
    false
}

pub fn computed_table(computed: &[(String, String)]) -> Vec<(&'static str, String)> {
    USEFUL_COMPUTED.iter().filter_map(|name| {
        computed.iter().find(|(key, _)| key == name).map(|(_, value)| (*name, value.clone()))
    }).collect()
}

pub fn pick_markdown(pick: &Pick, screenshot_path: &Path) -> String {
    let mut markdown = format!("# {}\n\n**URL:** {}\n\n**Selector:** `{}`\n\n```html\n{}\n```\n\n| Property | Value |\n| --- | --- |\n", pick.selector, pick.url, pick.selector, pick.outer_html.trim_end());
    for (name, value) in computed_table(&pick.computed) {
        markdown.push_str(&format!("| {name} | {} |\n", value.replace('|', "\\|")));
    }
    markdown.push_str(&format!("\n**Screenshot:** {}\n", screenshot_path.display()));
    markdown
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p browser -p settings` (the settings crate parses `default.json`; a missing `browser` block panics in `from_settings`).
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/browser crates/settings_content assets/settings/default.json
git commit -m "browser: Add crate scaffold, settings and pure pick module

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 4: `ChromeRunner`: launch, `DevToolsActivePort`, attach, pick flow

**Files:**
- Modify: `crates/browser/src/chrome_runner.rs`

**Interfaces:**
- Produces: `parse_devtools_active_port(text, &str) -> Result<String>` (pure, tested), `default_binary() -> Result<PathBuf>`, `ChromeRunner` entity with `global(cx)`, `open(url, cx) -> Task<Result<()>>`, `pick_element(cx) -> Task<Result<Pick>>`, `last_pick()`, `is_open()`, and `run_pick(session: &CdpSession, url) -> Result<Pick>` (async, no gpui).

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devtools_active_port_into_a_loopback_url() {
        let url = parse_devtools_active_port("54321\n/devtools/browser/abc-123\n").expect("parsed");
        assert_eq!(url, "ws://127.0.0.1:54321/devtools/browser/abc-123");
        assert!(parse_devtools_active_port("").is_err());
        assert!(parse_devtools_active_port("notaport\n/devtools/browser/x").is_err());
        assert!(parse_devtools_active_port("1234\n").is_err());
    }

    #[test]
    fn box_model_quad_becomes_a_clip() {
        let quad = [10.0, 20.0, 110.0, 20.0, 110.0, 70.0, 10.0, 70.0];
        let clip = clip_from_quad(&quad).expect("clip");
        assert_eq!(clip, (10.0, 20.0, 100.0, 50.0));
        assert!(clip_from_quad(&[1.0, 2.0]).is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p browser`
Expected: compile error.

- [ ] **Step 3: Write the runner**

Key pieces (full code lands in the file):

```rust
pub fn parse_devtools_active_port(text: &str) -> Result<String> {
    let mut lines = text.lines();
    let port: u16 = lines.next().context("DevToolsActivePort is empty")?.trim().parse().context("DevToolsActivePort: first line is not a port")?;
    let path = lines.next().map(str::trim).filter(|path| path.starts_with('/')).context("DevToolsActivePort: missing browser websocket path")?;
    Ok(format!("ws://127.0.0.1:{port}{path}"))
}

pub fn default_binary() -> Result<PathBuf> {
    [CHROME_BINARY, EDGE_BINARY].iter().map(PathBuf::from).find(|path| path.exists())
        .context("no Chrome or Edge found in /Applications; set browser.binary")
}

async fn launch(binary: PathBuf, user_data_dir: PathBuf, url: String) -> Result<(Child, String)> {
    smol::fs::create_dir_all(&user_data_dir).await?;
    let port_file = user_data_dir.join("DevToolsActivePort");
    if let Err(error) = smol::fs::remove_file(&port_file).await && error.kind() != io::ErrorKind::NotFound { return Err(error.into()); }
    let mut child = util::command::new_command(&binary)
        .args(["--remote-debugging-port=0", "--no-first-run", "--no-default-browser-check"])
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--new-window").arg(&url)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn().with_context(|| format!("launching {}", binary.display()))?;
    for _ in 0..150 {                                   // 15 s
        if let Some(status) = child.try_status()? { anyhow::bail!("{} exited before publishing a DevTools port ({status})", binary.display()); }
        if let Ok(text) = smol::fs::read_to_string(&port_file).await && let Ok(ws_url) = parse_devtools_active_port(&text) {
            return Ok((child, ws_url));
        }
        smol::Timer::after(Duration::from_millis(100)).await;
    }
    anyhow::bail!("timed out waiting for {}", port_file.display())
}
```

`ChromeRunner` keeps `child: Option<Child>`, `client: Option<CdpClient>`, `session: Option<CdpSession>`, `page_url: Option<String>`, `last_pick: Option<(Pick, PathBuf)>`. `open(url)`:

1. If a client exists, `Target.createTarget {url}` then attach; otherwise launch, `CdpClient::connect`, `get_targets`, pick the first `page` target whose `url` equals the requested one or else any page, attach.
2. Stores session + url.

`pick_element()` spawns `run_pick(session, url)` in the background and stores the result; `run_pick` follows the Global Constraints sequence, subscribing to `session.events()` *before* `Overlay.setInspectMode`, and converts `DOM.getBoxModel` → `model.border` quad to a clip via `clip_from_quad`. The PNG is decoded with `base64::engine::general_purpose::STANDARD` and written to `paths::data_dir()/picks/<unix-ms>.png`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p browser`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add crates/browser
git commit -m "browser: Add ChromeRunner with DevToolsActivePort discovery and CDP pick flow

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 5: Actions, URL prompt modal, and the read-only Markdown buffer

**Files:**
- Modify: `crates/browser/src/browser.rs`
- Create: `crates/browser/src/open_url_modal.rs`

**Interfaces:**
- Produces: `actions!(browser, [Open, PickElement, OpenPickInBuffer, SendPickToAgent])`; `browser::init(cx)` creates the `ChromeRunner` global and registers the four actions on every `Workspace`.

- [ ] **Step 1: Write the modal**

`OpenUrlModal` mirrors `GoToLine`: a single-line `Editor` with placeholder `https://`, `menu::Cancel` dismisses, `menu::Confirm` reads the text, prefixes `https://` when no scheme is present, and calls `ChromeRunner::global(cx).update(cx, |runner, cx| runner.open(url, cx))`, surfacing errors through a toast on the workspace.

- [ ] **Step 2: Wire actions in `init`**

```rust
pub fn init(cx: &mut App) {
    chrome_runner::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &Open, window, cx| {
            let workspace_handle = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, |window, cx| OpenUrlModal::new(workspace_handle, window, cx));
        });
        workspace.register_action(|workspace, _: &PickElement, window, cx| pick_element(workspace, window, cx));
        workspace.register_action(|workspace, _: &OpenPickInBuffer, window, cx| open_last_pick(workspace, window, cx));
        workspace.register_action(|workspace, _: &SendPickToAgent, _window, cx| {
            toast(workspace, "Send pick to agent is not yet wired", cx);
        });
    }).detach();
}
```

`pick_element` toasts "No browser open; run browser: open first" when the runner has no session, otherwise awaits `runner.pick_element(cx)` in `cx.spawn_in(window, …)`, then opens the buffer. `open_last_pick` toasts "No element picked yet" when `last_pick()` is `None`.

`open_pick_buffer(workspace, markdown, selector, window, cx)` follows `main.rs`' `BuiltinJsonSchema` branch: `languages.language_for_name("Markdown")`, `project.create_buffer(language, false, cx)`, `buffer.edit([(0..0, markdown)])`, `Editor::for_buffer` + `set_read_only(true)`, `editor.buffer().update(cx, |buffer, cx| buffer.set_title(selector, cx))`, `workspace.add_item_to_active_pane(...)`.

- [ ] **Step 3: Build and clippy**

Run: `cargo test -p browser && ./script/clippy -p cdp -p browser`
Expected: green and clean.

- [ ] **Step 4: Commit**

```bash
git add crates/browser
git commit -m "browser: Add Open/PickElement/OpenPickInBuffer actions and URL prompt

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```

---

### Task 6: Wire into `zed`: init, Go menu, keymap

**Files:**
- Modify: `crates/zed/Cargo.toml` (`browser.workspace = true` next to `herd_panel`)
- Modify: `crates/zed/src/main.rs` (`browser::init(cx);` right after `herd_panel::init(cx);`)
- Modify: `crates/zed/src/zed/app_menus.rs` (a "Browser" submenu at the end of the Go menu: Open URL…, Pick Element, Open Last Pick)
- Modify: `assets/keymaps/default-macos.json` (`"cmd-alt-b": "browser::PickElement"` next to `"cmd-shift-h": "herd_panel::ToggleFocus"`)

- [ ] **Step 1: Make the edits**

```rust
MenuItem::separator(),
MenuItem::submenu(Menu {
    name: "Browser".into(),
    disabled: false,
    items: vec![
        MenuItem::action("Open URL...", browser::Open),
        MenuItem::action("Pick Element", browser::PickElement),
        MenuItem::action("Open Last Pick", browser::OpenPickInBuffer),
    ],
}),
```

- [ ] **Step 2: Verify**

Run: `cargo test -p cdp -p browser && ./script/clippy -p cdp -p browser -p zed && cargo build -p zed`
Expected: tests green, clippy clean, binary at `target/debug/Hulder`.

- [ ] **Step 3: Manual check (optional, do not leave Chrome running)**

Launch `target/debug/Hulder`, `Go > Browser > Open URL...`, enter `example.com`, Chrome opens in the Hulder profile; `cmd-alt-b`, click a heading, a read-only `html > body > div > h1` Markdown tab opens and `~/Library/Application Support/Hulder/picks/*.png` exists. Quit Hulder; the Chrome child is killed on drop.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/zed assets/keymaps/default-macos.json
git commit -m "zed: Wire browser actions into init, the Go menu and the macOS keymap

Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>"
```
