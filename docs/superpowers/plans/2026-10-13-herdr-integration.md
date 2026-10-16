# Herdr Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hulder becomes a native GUI client of herdr's Unix socket: a Herd panel with live agent status, open-worktree-as-project, attach-agent-in-terminal, blocked-agent toasts, and send-selection-to-agent.

**Architecture:** One new crate `herdr` holds the protocol types, an async newline-JSON client over `smol::net::unix::UnixStream`, and a `HerdState` gpui entity updated from `events.subscribe`. A second new crate `herd_panel` renders the dock panel and owns the user actions. The `zed` crate wires both in `main.rs` and `zed.rs` exactly like `outline_panel`.

**Tech Stack:** Rust 1.98, gpui (entities, `cx.spawn`, `cx.background_spawn`), smol 2 for the socket, serde/serde_json for the wire, `workspace::Panel` and `workspace::Toast`, `terminal_view::TerminalPanel::add_terminal_task`.

**Spec:** `docs/superpowers/specs/2026-10-13-hulder-design.md`, section 5.

## Global Constraints

- herdr protocol 16, newline-delimited JSON. Request: `{"id": string, "method": string, "params": object}`. Success: `{"id", "result": {...}}`. Error: `{"id", "error": {"code", "message"}}`. Event push: `{"event": string, "data": {"type": string, ...}}`.
- Socket path: `~/.config/herdr/herdr.sock`. Local only; no TCP ever.
- `params` is required on every request, even when empty (`{}`), or herdr answers `invalid_request`.
- `events.subscribe` params: `{"subscriptions": [{"type": "workspace.created"}, ...]}`; subscription type names use dots (`pane.agent_status_changed`) while pushed event names use underscores (`pane_agent_status_changed`).
- Agent status enum: `idle | working | blocked | done | unknown`.
- Follow repo CLAUDE.md: no `unwrap()` outside tests, no `let _ =` on fallible calls, no `mod.rs`, 2-space JSON, `./script/clippy` not `cargo clippy`.
- Keep README's `> [!IMPORTANT]` marker in place.
- Build with `cargo build -p zed`; `.cargo/config.toml` already points at Xcode.app.

## Review Focus

1. herdr not running: the panel must show a message and a Start button, never a spinner forever or a panic. Test in Task 3 (connect to a missing path returns `Err`, state becomes `Disconnected`).
2. herdr restarts mid-session: the client must reconnect and refetch the full snapshot, not merge stale events. Test in Task 3 (fake server closes; client reconnects and issues `session.snapshot` again).
3. Events for unknown workspace ids (race between snapshot and subscribe): must be ignored, not inserted as orphans. Test in Task 4.
4. Attach when the terminal panel is closed: `AttachAgent` must open the dock panel first. Test in Task 7 (manual, listed).
5. Agent names can be null: the panel must fall back to `display_agent`, then `terminal_title_stripped`, then `pane_id`. Test in Task 4 (`AgentRow::label`).

---

### Task 1: Crate scaffold and protocol types

**Files:**
- Create: `crates/herdr/Cargo.toml`
- Create: `crates/herdr/src/herdr.rs`
- Create: `crates/herdr/src/protocol.rs`
- Modify: `Cargo.toml` (workspace members glob already includes `crates/*`; add `herdr = { path = "crates/herdr" }` under `[workspace.dependencies]` next to `git_ui`)

**Interfaces:**
- Produces: `herdr::protocol::{Request, Response, Event, EventData, AgentStatus, WorkspaceInfo, WorktreeInfo, AgentInfo, Subscription}` with serde derives; `Request::new(id: u64, method: &str, params: serde_json::Value) -> Request`.

- [ ] **Step 1: Write the failing test**

`crates/herdr/src/protocol.rs` (tests at the bottom):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_with_params_always_present() {
        let request = Request::new(7, "ping", serde_json::json!({}));
        let text = serde_json::to_string(&request).expect("serialize");
        assert_eq!(text, r#"{"id":"7","method":"ping","params":{}}"#);
    }

    #[test]
    fn parses_success_error_and_event_lines() {
        let ok: Incoming = serde_json::from_str(
            r#"{"id":"1","result":{"type":"pong","version":"0.7.4","protocol":16}}"#,
        )
        .expect("ok line");
        assert!(matches!(ok, Incoming::Response(Response { id, result: Some(_), error: None }) if id == "1"));

        let err: Incoming = serde_json::from_str(
            r#"{"id":"","error":{"code":"invalid_request","message":"missing field `params`"}}"#,
        )
        .expect("err line");
        assert!(matches!(err, Incoming::Response(Response { error: Some(ref e), .. }) if e.code == "invalid_request"));

        let event: Incoming = serde_json::from_str(
            r#"{"event":"pane_agent_status_changed","data":{"type":"pane_agent_status_changed","pane_id":"w1:p2","workspace_id":"w1","agent":"claude","agent_status":"blocked","display_agent":null,"title":null,"state_labels":{}}}"#,
        )
        .expect("event line");
        match event {
            Incoming::Event(Event { data: EventData::PaneAgentStatusChanged { pane_id, agent_status, .. }, .. }) => {
                assert_eq!(pane_id, "w1:p2");
                assert_eq!(agent_status, AgentStatus::Blocked);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p herdr`
Expected: fails to compile, crate does not exist.

- [ ] **Step 3: Write the crate**

`crates/herdr/Cargo.toml`:

```toml
[package]
name = "herdr"
version = "0.1.0"
edition.workspace = true
publish.workspace = true
license = "GPL-3.0-or-later"

[lints]
workspace = true

[lib]
path = "src/herdr.rs"

[dependencies]
anyhow.workspace = true
collections.workspace = true
futures.workspace = true
gpui.workspace = true
log.workspace = true
paths.workspace = true
serde.workspace = true
serde_json.workspace = true
smol.workspace = true
util.workspace = true

[dev-dependencies]
gpui = { workspace = true, features = ["test-support"] }
```

`crates/herdr/src/herdr.rs`:

```rust
pub mod client;
pub mod protocol;
pub mod state;

pub use client::HerdrClient;
pub use protocol::*;
pub use state::{AgentRow, HerdState, WorkspaceRow};
```

(`client` and `state` are created in Tasks 2 and 4; for this task create them as empty files so the crate compiles.)

`crates/herdr/src/protocol.rs`:

```rust
use collections::HashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub params: serde_json::Value,
}

impl Request {
    pub fn new(id: u64, method: &str, params: serde_json::Value) -> Self {
        Self {
            id: id.to_string(),
            method: method.to_string(),
            params,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    pub id: String,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub number: u64,
    pub label: String,
    pub focused: bool,
    pub pane_count: u64,
    pub tab_count: u64,
    pub active_tab_id: String,
    pub agent_status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    #[serde(default)]
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_linked_worktree: bool,
    #[serde(default)]
    pub open_workspace_id: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    pub terminal_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub display_agent: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    pub agent_status: AgentStatus,
    pub focused: bool,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Subscription {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// Every subscription type Hulder needs. Names use dots on the wire.
pub const SUBSCRIPTIONS: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.closed",
    "workspace.renamed",
    "workspace.focused",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
    "pane.created",
    "pane.closed",
    "pane.updated",
    "pane.focused",
    "pane.agent_detected",
    "pane.agent_status_changed",
];

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub event: String,
    pub data: EventData,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    WorkspaceCreated { workspace: WorkspaceInfo },
    WorkspaceUpdated { workspace: WorkspaceInfo },
    WorkspaceClosed { workspace_id: String },
    WorkspaceRenamed { workspace_id: String, label: String },
    WorkspaceFocused { workspace_id: String },
    WorktreeCreated { #[serde(default)] worktree: Option<WorktreeInfo> },
    WorktreeOpened { #[serde(default)] worktree: Option<WorktreeInfo> },
    WorktreeRemoved { #[serde(default)] path: Option<String> },
    PaneCreated { pane: AgentInfo },
    PaneClosed { pane_id: String, workspace_id: String },
    PaneUpdated { pane: AgentInfo },
    PaneFocused { pane_id: String, workspace_id: String },
    PaneAgentDetected {
        pane_id: String,
        workspace_id: String,
        #[serde(default)]
        agent: Option<String>,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        workspace_id: String,
        #[serde(default)]
        agent: Option<String>,
        agent_status: AgentStatus,
        #[serde(default)]
        display_agent: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        state_labels: HashMap<String, String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Event(Event),
    Response(Response),
}
```

Note on field shapes: `WorktreeCreated`/`WorktreeOpened`/`PaneCreated` payload field names are taken from the schema file captured at `herdr api schema --json`. If `cargo test` in Task 3's fixture shows a mismatch, fix the struct here, not the fixture.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p herdr`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/herdr
git commit -m "herdr: Add protocol types for the herdr socket API"
```

---

### Task 2: Socket client with request correlation and event stream

**Files:**
- Create: `crates/herdr/src/client.rs`
- Test: same file, `#[cfg(test)]`

**Interfaces:**
- Consumes: `protocol::{Request, Incoming, Response, Event, SUBSCRIPTIONS}`.
- Produces:
  - `pub fn socket_path() -> PathBuf` (returns `paths::home_dir().join(".config/herdr/herdr.sock")`)
  - `pub struct HerdrClient` with `pub async fn connect(path: PathBuf) -> anyhow::Result<(HerdrClient, futures::channel::mpsc::UnboundedReceiver<Event>)>`
  - `pub async fn request(&self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value>`
  - `pub async fn subscribe_all(&self) -> anyhow::Result<()>`

- [ ] **Step 1: Write the failing test**

The test runs a fake herdr on a temp Unix socket using smol.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use smol::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use smol::net::unix::UnixListener;

    async fn fake_server(path: std::path::PathBuf) {
        let listener = UnixListener::bind(&path).expect("bind");
        let (stream, _) = listener.accept().await.expect("accept");
        let (reader, mut writer) = smol::io::split(stream);
        let mut lines = BufReader::new(reader).lines();
        while let Some(Ok(line)) = lines.next().await {
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
            writer.write_all(format!("{reply}\n").as_bytes()).await.expect("write");
        }
    }

    #[test]
    fn ping_round_trip_and_event_delivery() {
        smol::block_on(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("h.sock");
            smol::spawn(fake_server(path.clone())).detach();
            smol::Timer::after(std::time::Duration::from_millis(50)).await;

            let (client, mut events) = HerdrClient::connect(path).await.expect("connect");
            let pong = client.request("ping", serde_json::json!({})).await.expect("ping");
            assert_eq!(pong["type"], "pong");

            client.subscribe_all().await.expect("subscribe");
            let event = events.next().await.expect("one event");
            assert_eq!(event.event, "pane_agent_status_changed");

            let err = client.request("nope", serde_json::json!({})).await;
            assert!(err.is_err());
        });
    }

    #[test]
    fn connect_to_missing_socket_fails_cleanly() {
        smol::block_on(async {
            let result = HerdrClient::connect(std::path::PathBuf::from("/nonexistent/h.sock")).await;
            assert!(result.is_err());
        });
    }
}
```

Add to `[dev-dependencies]` in `crates/herdr/Cargo.toml`: `tempfile.workspace = true`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p herdr client`
Expected: compile error, `HerdrClient` undefined.

- [ ] **Step 3: Write the client**

```rust
use crate::protocol::{Event, Incoming, Request, Subscription, SUBSCRIPTIONS};
use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::channel::{mpsc, oneshot};
use futures::{StreamExt, lock::Mutex};
use smol::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use smol::net::unix::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn socket_path() -> PathBuf {
    paths::home_dir().join(".config").join("herdr").join("herdr.sock")
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<crate::protocol::Response>>>>;

#[derive(Clone)]
pub struct HerdrClient {
    writer: Arc<Mutex<WriteHalf<UnixStream>>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

impl HerdrClient {
    pub async fn connect(path: PathBuf) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let stream = UnixStream::connect(&path)
            .await
            .with_context(|| format!("connecting to herdr at {}", path.display()))?;
        let (reader, writer) = smol::io::split(stream);
        let pending: Pending = Arc::new(Mutex::new(HashMap::default()));
        let (event_tx, event_rx) = mpsc::unbounded();

        let reader_pending = pending.clone();
        smol::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Some(line) = lines.next().await {
                let line = match line {
                    Ok(line) => line,
                    Err(error) => {
                        log::warn!("herdr socket read failed: {error}");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Incoming>(&line) {
                    Ok(Incoming::Response(response)) => {
                        if let Some(sender) = reader_pending.lock().await.remove(&response.id) {
                            sender.send(response).ok();
                        }
                    }
                    Ok(Incoming::Event(event)) => {
                        if event_tx.unbounded_send(event).is_err() {
                            break;
                        }
                    }
                    Err(error) => log::warn!("herdr: unparseable line {line:?}: {error}"),
                }
            }
            // Dropping event_tx closes the receiver so HerdState knows to reconnect.
        })
        .detach();

        Ok((
            Self {
                writer: Arc::new(Mutex::new(writer)),
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
            },
            event_rx,
        ))
    }

    pub async fn request(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, method, params);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(request.id.clone(), sender);

        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        self.writer.lock().await.write_all(line.as_bytes()).await?;

        let response = receiver.await.context("herdr connection closed before reply")?;
        match (response.result, response.error) {
            (Some(result), _) => Ok(result),
            (None, Some(error)) => Err(anyhow!("herdr {}: {} ({})", method, error.message, error.code)),
            (None, None) => Err(anyhow!("herdr {}: empty response", method)),
        }
    }

    pub async fn subscribe_all(&self) -> Result<()> {
        let subscriptions: Vec<Subscription> =
            SUBSCRIPTIONS.iter().map(|kind| Subscription { kind }).collect();
        self.request(
            "events.subscribe",
            serde_json::json!({ "subscriptions": subscriptions }),
        )
        .await?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p herdr`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/herdr
git commit -m "herdr: Add async socket client with request correlation and event stream"
```

---

### Task 3: Live fixture test against the real schema shapes

**Files:**
- Create: `crates/herdr/fixtures/snapshot.json` (paste the `result` of a real `session.snapshot` call; capture with the python one-liner below)
- Create: `crates/herdr/fixtures/events.jsonl` (ten real pushed event lines, captured by subscribing and renaming a workspace in herdr)
- Test: `crates/herdr/src/protocol.rs` tests module

Capture command (run once, commit the output; strip nothing, these are local paths and labels only):

```bash
python3 - <<'EOF'
import socket, json, os, time
p=os.path.expanduser('~/.config/herdr/herdr.sock')
s=socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(3); s.connect(p)
s.sendall(b'{"id":"1","method":"session.snapshot","params":{}}\n')
buf=b""
while not buf.endswith(b"\n"):
    buf+=s.recv(1<<20)
open('crates/herdr/fixtures/snapshot.json','w').write(json.dumps(json.loads(buf)['result'], indent=2))
EOF
```

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn real_snapshot_deserializes() {
    let text = include_str!("../fixtures/snapshot.json");
    let snapshot: serde_json::Value = serde_json::from_str(text).expect("json");
    let workspaces: Vec<WorkspaceInfo> =
        serde_json::from_value(snapshot["snapshot"]["workspaces"].clone()).expect("workspaces");
    let agents: Vec<AgentInfo> =
        serde_json::from_value(snapshot["snapshot"]["agents"].clone()).expect("agents");
    assert!(!workspaces.is_empty());
    assert!(!agents.is_empty());
}

#[test]
fn real_events_deserialize() {
    for line in include_str!("../fixtures/events.jsonl").lines().filter(|l| !l.trim().is_empty()) {
        let parsed: Incoming = serde_json::from_str(line).expect(line);
        assert!(matches!(parsed, Incoming::Event(_)), "{line}");
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p herdr real_`
Expected: fails on a field name mismatch or missing fixture.

- [ ] **Step 3: Fix struct fields until both pass**

Adjust `protocol.rs` field names and `Option` wrappers to match the fixture. Do not loosen to `serde_json::Value` wholesale; only the fields the panel does not use may be dropped.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p herdr`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/herdr
git commit -m "herdr: Pin protocol structs to real snapshot and event fixtures"
```

---

### Task 4: HerdState entity (snapshot + event folding + reconnect)

**Files:**
- Create: `crates/herdr/src/state.rs`
- Test: same file

**Interfaces:**
- Consumes: `HerdrClient`, `protocol::*`.
- Produces:
  ```rust
  pub enum Connection { Disconnected { reason: String }, Connecting, Connected }
  pub struct WorkspaceRow { pub id: String, pub label: String, pub focused: bool, pub worktree_path: Option<String>, pub branch: Option<String>, pub agents: Vec<AgentRow> }
  pub struct AgentRow { pub pane_id: String, pub terminal_id: String, pub workspace_id: String, pub agent: Option<String>, pub display_agent: Option<String>, pub name: Option<String>, pub title: Option<String>, pub status: AgentStatus, pub cwd: Option<String> }
  impl AgentRow { pub fn label(&self) -> String }
  pub struct HerdState { pub connection: Connection, pub workspaces: Vec<WorkspaceRow> }
  impl HerdState {
      pub fn new(cx: &mut Context<Self>) -> Self           // starts connect loop
      pub fn global(cx: &App) -> Entity<HerdState>         // via a Global wrapper
      pub fn client(&self) -> Option<HerdrClient>
      pub fn apply_snapshot(&mut self, workspaces: Vec<WorkspaceInfo>, worktrees: Vec<WorktreeInfo>, agents: Vec<AgentInfo>)
      pub fn apply_event(&mut self, event: &EventData) -> Option<HerdEvent>
      pub fn request_reconnect(&mut self, cx: &mut Context<Self>)
  }
  pub enum HerdEvent { AgentStatusChanged { agent: AgentRow, previous: AgentStatus } }
  impl EventEmitter<HerdEvent> for HerdState {}
  pub fn init(cx: &mut App)   // creates the global entity
  ```

- [ ] **Step 1: Write the failing tests** (pure functions, no socket)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn agent(pane: &str, ws: &str, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            pane_id: pane.into(), tab_id: format!("{ws}:t1"), workspace_id: ws.into(),
            terminal_id: format!("term_{pane}"), agent: Some("claude".into()), display_agent: None,
            name: None, title: None, terminal_title_stripped: Some("plan".into()),
            agent_status: status, focused: false, cwd: Some("/tmp".into()),
        }
    }
    fn workspace(id: &str, label: &str) -> WorkspaceInfo {
        WorkspaceInfo { workspace_id: id.into(), number: 1, label: label.into(), focused: false,
            pane_count: 1, tab_count: 1, active_tab_id: format!("{id}:t1"), agent_status: AgentStatus::Idle }
    }

    #[test]
    fn snapshot_groups_agents_under_workspaces_and_attaches_worktrees() {
        let mut state = HerdState::empty();
        let worktree = WorktreeInfo { path: "/repo".into(), branch: Some("main".into()), is_bare: false,
            is_detached: false, is_linked_worktree: false, open_workspace_id: Some("w1".into()), label: "repo".into() };
        state.apply_snapshot(vec![workspace("w1", "repo")], vec![worktree], vec![agent("w1:p1", "w1", AgentStatus::Working)]);
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].worktree_path.as_deref(), Some("/repo"));
        assert_eq!(state.workspaces[0].agents[0].status, AgentStatus::Working);
    }

    #[test]
    fn status_event_updates_agent_and_reports_previous() {
        let mut state = HerdState::empty();
        state.apply_snapshot(vec![workspace("w1", "repo")], vec![], vec![agent("w1:p1", "w1", AgentStatus::Working)]);
        let event = EventData::PaneAgentStatusChanged { pane_id: "w1:p1".into(), workspace_id: "w1".into(),
            agent: Some("claude".into()), agent_status: AgentStatus::Blocked, display_agent: None, title: None,
            state_labels: Default::default() };
        let Some(HerdEvent::AgentStatusChanged { agent, previous }) = state.apply_event(&event) else { panic!() };
        assert_eq!(previous, AgentStatus::Working);
        assert_eq!(agent.status, AgentStatus::Blocked);
    }

    #[test]
    fn events_for_unknown_workspace_are_ignored() {
        let mut state = HerdState::empty();
        let event = EventData::PaneCreated { pane: agent("w9:p1", "w9", AgentStatus::Idle) };
        assert!(state.apply_event(&event).is_none());
        assert!(state.workspaces.is_empty());
    }

    #[test]
    fn label_falls_back_in_order() {
        let mut row = AgentRow::from(agent("w1:p1", "w1", AgentStatus::Idle));
        row.name = None; row.display_agent = None; row.title = None;
        assert_eq!(row.label(), "plan");
        row.display_agent = Some("Claude".into());
        assert_eq!(row.label(), "Claude");
        row.name = Some("builder".into());
        assert_eq!(row.label(), "builder");
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p herdr state`
Expected: compile error.

- [ ] **Step 3: Implement**

```rust
use crate::client::{HerdrClient, socket_path};
use crate::protocol::{AgentInfo, AgentStatus, EventData, WorkspaceInfo, WorktreeInfo};
use anyhow::Result;
use futures::StreamExt;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    Disconnected { reason: String },
    Connecting,
    Connected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub status: AgentStatus,
    pub cwd: Option<String>,
}

impl From<AgentInfo> for AgentRow {
    fn from(info: AgentInfo) -> Self {
        Self {
            pane_id: info.pane_id, terminal_id: info.terminal_id, workspace_id: info.workspace_id,
            agent: info.agent, display_agent: info.display_agent, name: info.name, title: info.title,
            terminal_title_stripped: info.terminal_title_stripped, status: info.agent_status, cwd: info.cwd,
        }
    }
}

impl AgentRow {
    pub fn label(&self) -> String {
        self.name.clone()
            .or_else(|| self.display_agent.clone())
            .or_else(|| self.terminal_title_stripped.clone())
            .or_else(|| self.agent.clone())
            .unwrap_or_else(|| self.pane_id.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub id: String,
    pub label: String,
    pub focused: bool,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub agents: Vec<AgentRow>,
}

pub enum HerdEvent {
    AgentStatusChanged { agent: AgentRow, previous: AgentStatus },
}

pub struct HerdState {
    pub connection: Connection,
    pub workspaces: Vec<WorkspaceRow>,
    client: Option<HerdrClient>,
    _connect_task: Option<Task<()>>,
}

impl EventEmitter<HerdEvent> for HerdState {}

struct GlobalHerdState(Entity<HerdState>);
impl Global for GlobalHerdState {}

pub fn init(cx: &mut App) {
    let state = cx.new(HerdState::new);
    cx.set_global(GlobalHerdState(state));
}

impl HerdState {
    pub fn global(cx: &App) -> Entity<HerdState> {
        cx.global::<GlobalHerdState>().0.clone()
    }

    pub fn empty() -> Self {
        Self { connection: Connection::Disconnected { reason: "not started".into() }, workspaces: Vec::new(), client: None, _connect_task: None }
    }

    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self::empty();
        this.request_reconnect(cx);
        this
    }

    pub fn client(&self) -> Option<HerdrClient> {
        self.client.clone()
    }

    pub fn request_reconnect(&mut self, cx: &mut Context<Self>) {
        self.connection = Connection::Connecting;
        cx.notify();
        self._connect_task = Some(cx.spawn(async move |this, cx| {
            let mut delay = Duration::from_millis(500);
            loop {
                match HerdrClient::connect(socket_path()).await {
                    Ok((client, mut events)) => {
                        let loaded = Self::load_snapshot(&client).await;
                        let subscribed = client.subscribe_all().await;
                        let outcome = this.update(cx, |this, cx| match (loaded, subscribed) {
                            (Ok((workspaces, worktrees, agents)), Ok(())) => {
                                this.client = Some(client.clone());
                                this.apply_snapshot(workspaces, worktrees, agents);
                                this.connection = Connection::Connected;
                                cx.notify();
                                Ok(())
                            }
                            (Err(error), _) | (_, Err(error)) => Err(error),
                        });
                        match outcome {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                log::warn!("herdr: snapshot failed: {error}");
                            }
                            Err(_) => return,
                        }
                        while let Some(event) = events.next().await {
                            if this.update(cx, |this, cx| {
                                if let Some(herd_event) = this.apply_event(&event.data) {
                                    cx.emit(herd_event);
                                }
                                cx.notify();
                            }).is_err() {
                                return;
                            }
                        }
                        // stream closed: herdr went away
                        if this.update(cx, |this, cx| {
                            this.client = None;
                            this.workspaces.clear();
                            this.connection = Connection::Disconnected { reason: "herdr closed the connection".into() };
                            cx.notify();
                        }).is_err() {
                            return;
                        }
                        delay = Duration::from_millis(500);
                    }
                    Err(error) => {
                        if this.update(cx, |this, cx| {
                            this.connection = Connection::Disconnected { reason: error.to_string() };
                            cx.notify();
                        }).is_err() {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(delay).await;
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        }));
    }

    async fn load_snapshot(client: &HerdrClient) -> Result<(Vec<WorkspaceInfo>, Vec<WorktreeInfo>, Vec<AgentInfo>)> {
        let snapshot = client.request("session.snapshot", serde_json::json!({})).await?;
        let workspaces = serde_json::from_value(snapshot["snapshot"]["workspaces"].clone())?;
        let agents = serde_json::from_value(snapshot["snapshot"]["agents"].clone())?;
        let worktrees = match client.request("worktree.list", serde_json::json!({})).await {
            Ok(value) => serde_json::from_value(value["worktrees"].clone()).unwrap_or_default(),
            Err(error) => {
                log::info!("herdr: worktree.list unavailable: {error}");
                Vec::new()
            }
        };
        Ok((workspaces, worktrees, agents))
    }

    pub fn apply_snapshot(&mut self, workspaces: Vec<WorkspaceInfo>, worktrees: Vec<WorktreeInfo>, agents: Vec<AgentInfo>) {
        self.workspaces = workspaces.into_iter().map(|workspace| {
            let worktree = worktrees.iter().find(|w| w.open_workspace_id.as_deref() == Some(&workspace.workspace_id));
            WorkspaceRow {
                id: workspace.workspace_id.clone(),
                label: workspace.label,
                focused: workspace.focused,
                worktree_path: worktree.map(|w| w.path.clone()),
                branch: worktree.and_then(|w| w.branch.clone()),
                agents: Vec::new(),
            }
        }).collect();
        for agent in agents {
            if let Some(row) = self.workspaces.iter_mut().find(|w| w.id == agent.workspace_id) {
                row.agents.push(AgentRow::from(agent));
            }
        }
    }

    pub fn apply_event(&mut self, event: &EventData) -> Option<HerdEvent> {
        match event {
            EventData::WorkspaceCreated { workspace } | EventData::WorkspaceUpdated { workspace } => {
                match self.workspaces.iter_mut().find(|w| w.id == workspace.workspace_id) {
                    Some(row) => { row.label = workspace.label.clone(); row.focused = workspace.focused; }
                    None => self.workspaces.push(WorkspaceRow {
                        id: workspace.workspace_id.clone(), label: workspace.label.clone(), focused: workspace.focused,
                        worktree_path: None, branch: None, agents: Vec::new(),
                    }),
                }
                None
            }
            EventData::WorkspaceClosed { workspace_id } => { self.workspaces.retain(|w| &w.id != workspace_id); None }
            EventData::WorkspaceRenamed { workspace_id, label } => {
                if let Some(row) = self.workspaces.iter_mut().find(|w| &w.id == workspace_id) { row.label = label.clone(); }
                None
            }
            EventData::WorkspaceFocused { workspace_id } => {
                for row in &mut self.workspaces { row.focused = &row.id == workspace_id; }
                None
            }
            EventData::PaneCreated { pane } | EventData::PaneUpdated { pane } => {
                let row = self.workspaces.iter_mut().find(|w| w.id == pane.workspace_id)?;
                let new_row = AgentRow::from(pane.clone());
                match row.agents.iter_mut().find(|a| a.pane_id == pane.pane_id) {
                    Some(existing) => *existing = new_row,
                    None => row.agents.push(new_row),
                }
                None
            }
            EventData::PaneClosed { pane_id, workspace_id } => {
                if let Some(row) = self.workspaces.iter_mut().find(|w| &w.id == workspace_id) { row.agents.retain(|a| &a.pane_id != pane_id); }
                None
            }
            EventData::PaneAgentStatusChanged { pane_id, workspace_id, agent, agent_status, display_agent, title, .. } => {
                let row = self.workspaces.iter_mut().find(|w| &w.id == workspace_id)?;
                let existing = row.agents.iter_mut().find(|a| &a.pane_id == pane_id)?;
                let previous = existing.status;
                existing.status = *agent_status;
                if agent.is_some() { existing.agent = agent.clone(); }
                if display_agent.is_some() { existing.display_agent = display_agent.clone(); }
                if title.is_some() { existing.title = title.clone(); }
                Some(HerdEvent::AgentStatusChanged { agent: existing.clone(), previous })
            }
            EventData::PaneAgentDetected { pane_id, workspace_id, agent } => {
                let row = self.workspaces.iter_mut().find(|w| &w.id == workspace_id)?;
                if let Some(existing) = row.agents.iter_mut().find(|a| &a.pane_id == pane_id) { existing.agent = agent.clone(); }
                None
            }
            EventData::WorktreeOpened { worktree: Some(worktree) } | EventData::WorktreeCreated { worktree: Some(worktree) } => {
                if let Some(id) = &worktree.open_workspace_id
                    && let Some(row) = self.workspaces.iter_mut().find(|w| &w.id == id)
                {
                    row.worktree_path = Some(worktree.path.clone());
                    row.branch = worktree.branch.clone();
                }
                None
            }
            _ => None,
        }
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p herdr`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/herdr
git commit -m "herdr: Add HerdState entity with snapshot, event folding and reconnect"
```

---

### Task 5: Herd panel crate (dock panel rendering the state)

**Files:**
- Create: `crates/herd_panel/Cargo.toml`
- Create: `crates/herd_panel/src/herd_panel.rs`
- Modify: `Cargo.toml` (`herd_panel = { path = "crates/herd_panel" }` under workspace deps)
- Modify: `crates/zed/Cargo.toml` (add `herd_panel.workspace = true` and `herdr.workspace = true` next to `outline_panel.workspace = true`)
- Modify: `crates/zed/src/main.rs` (after `outline_panel::init(cx);` add `herdr::state::init(cx); herd_panel::init(cx);`)
- Modify: `crates/zed/src/zed.rs` in `initialize_panels` (add `let herd_panel = HerdPanel::load(workspace_handle.clone(), cx.clone());` and `add_panel_when_ready(herd_panel, ...)` in the `join!`), and the `use herd_panel::HerdPanel;` import.
- Modify: `assets/keymaps/default-macos.json` top-level bindings block that holds `"cmd-shift-e": "project_panel::ToggleFocus"`: add `"cmd-shift-h": "herd_panel::ToggleFocus"`.

**Interfaces:**
- Consumes: `herdr::{HerdState, Connection, WorkspaceRow, AgentRow, AgentStatus, HerdEvent}`.
- Produces: `herd_panel::HerdPanel` implementing `workspace::Panel`; actions in namespace `herd_panel`: `ToggleFocus`, `OpenWorktree`, `FocusAgent`, `AttachAgent`, `SendSelection`, `SendFilePath`, `StartHerdr`, `Refresh`; `pub fn init(cx: &mut App)`; `HerdPanel::load(workspace: WeakEntity<Workspace>, cx: AsyncWindowContext) -> Task<Result<Entity<HerdPanel>>>`.

- [ ] **Step 1: Write the failing test** (gpui test that the panel renders rows from state)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use herdr::{AgentRow, AgentStatus, WorkspaceRow};

    #[gpui::test]
    fn rows_flatten_workspaces_then_agents(cx: &mut TestAppContext) {
        let rows = cx.update(|_cx| {
            let workspaces = vec![WorkspaceRow {
                id: "w1".into(), label: "repo".into(), focused: true,
                worktree_path: Some("/repo".into()), branch: Some("main".into()),
                agents: vec![AgentRow {
                    pane_id: "w1:p1".into(), terminal_id: "t1".into(), workspace_id: "w1".into(),
                    agent: Some("claude".into()), display_agent: None, name: Some("builder".into()),
                    title: None, terminal_title_stripped: None, status: AgentStatus::Blocked, cwd: None,
                }],
            }];
            flatten_rows(&workspaces)
        });
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0], Row::Workspace { .. }));
        assert!(matches!(&rows[1], Row::Agent { label, status: AgentStatus::Blocked, .. } if label == "builder"));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p herd_panel`
Expected: compile error.

- [ ] **Step 3: Implement the panel**

`crates/herd_panel/Cargo.toml`:

```toml
[package]
name = "herd_panel"
version = "0.1.0"
edition.workspace = true
publish.workspace = true
license = "GPL-3.0-or-later"

[lints]
workspace = true

[lib]
path = "src/herd_panel.rs"

[dependencies]
anyhow.workspace = true
db.workspace = true
gpui.workspace = true
herdr.workspace = true
log.workspace = true
project.workspace = true
serde.workspace = true
settings.workspace = true
ui.workspace = true
util.workspace = true
workspace.workspace = true

[dev-dependencies]
gpui = { workspace = true, features = ["test-support"] }
workspace = { workspace = true, features = ["test-support"] }
```

`crates/herd_panel/src/herd_panel.rs` (rendering only in this task; actions beyond `ToggleFocus` and `Refresh` are registered in Task 6):

```rust
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, Styled, Subscription, Task, WeakEntity, Window, actions,
};
use herdr::{AgentStatus, Connection, HerdState, WorkspaceRow};
use ui::{
    Color, Icon, IconName, Label, LabelCommon, ListItem, ListItemSpacing, h_flex, prelude::*, v_flex,
    Button, ButtonCommon,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    herd_panel,
    [
        /// Toggles focus on the Herd panel.
        ToggleFocus,
        /// Reconnects to herdr and reloads the snapshot.
        Refresh,
        /// Opens the selected workspace's worktree as a project.
        OpenWorktree,
        /// Focuses the selected agent pane in herdr.
        FocusAgent,
        /// Attaches the selected agent in a Hulder terminal.
        AttachAgent,
        /// Sends the current selection to an agent.
        SendSelection,
        /// Sends the current file path and line to an agent.
        SendFilePath,
        /// Starts a herdr server in the background.
        StartHerdr,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<HerdPanel>(window, cx);
        });
    })
    .detach();
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Workspace { id: String, label: String, branch: Option<String>, has_worktree: bool, focused: bool },
    Agent { pane_id: String, terminal_id: String, workspace_id: String, label: String, status: AgentStatus },
}

pub fn flatten_rows(workspaces: &[WorkspaceRow]) -> Vec<Row> {
    let mut rows = Vec::new();
    for workspace in workspaces {
        rows.push(Row::Workspace {
            id: workspace.id.clone(), label: workspace.label.clone(), branch: workspace.branch.clone(),
            has_worktree: workspace.worktree_path.is_some(), focused: workspace.focused,
        });
        for agent in &workspace.agents {
            rows.push(Row::Agent {
                pane_id: agent.pane_id.clone(), terminal_id: agent.terminal_id.clone(),
                workspace_id: agent.workspace_id.clone(), label: agent.label(), status: agent.status,
            });
        }
    }
    rows
}

pub struct HerdPanel {
    workspace: WeakEntity<Workspace>,
    state: Entity<HerdState>,
    focus_handle: FocusHandle,
    selected: Option<usize>,
    width: Option<Pixels>,
    _subscriptions: Vec<Subscription>,
}

impl HerdPanel {
    pub fn load(workspace: WeakEntity<Workspace>, mut cx: AsyncWindowContext) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                let state = HerdState::global(cx);
                cx.new(|cx| Self::new(workspace, state, window, cx))
            })
        })
    }

    fn new(workspace: &Workspace, state: Entity<HerdState>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            workspace: workspace.weak_handle(),
            state,
            focus_handle: cx.focus_handle(),
            selected: None,
            width: None,
            _subscriptions: vec![subscription],
        }
    }

    pub fn selected_row(&self, cx: &App) -> Option<Row> {
        let rows = flatten_rows(&self.state.read(cx).workspaces);
        self.selected.and_then(|ix| rows.get(ix).cloned())
    }

    fn status_icon(status: AgentStatus) -> (IconName, Color) {
        match status {
            AgentStatus::Working => (IconName::Play, Color::Accent),
            AgentStatus::Blocked => (IconName::Warning, Color::Warning),
            AgentStatus::Done => (IconName::Check, Color::Success),
            AgentStatus::Idle => (IconName::Circle, Color::Muted),
            AgentStatus::Unknown => (IconName::QuestionMark, Color::Disabled),
        }
    }

    fn render_disconnected(&self, reason: &str, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .p_2()
            .gap_2()
            .child(Label::new("herdr is not running").color(Color::Muted))
            .child(Label::new(reason.to_string()).size(ui::LabelSize::Small).color(Color::Disabled))
            .child(
                Button::new("start-herdr", "Start herdr")
                    .on_click(cx.listener(|_, _, window, cx| window.dispatch_action(StartHerdr.boxed_clone(), cx))),
            )
    }
}

impl Render for HerdPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let body = match &state.connection {
            Connection::Disconnected { reason } => self.render_disconnected(reason, cx).into_any_element(),
            Connection::Connecting => Label::new("Connecting to herdr…").color(Color::Muted).into_any_element(),
            Connection::Connected => {
                let rows = flatten_rows(&state.workspaces);
                v_flex()
                    .children(rows.into_iter().enumerate().map(|(ix, row)| {
                        let selected = self.selected == Some(ix);
                        match row {
                            Row::Workspace { label, branch, focused, .. } => ListItem::new(("ws", ix))
                                .spacing(ListItemSpacing::Sparse)
                                .toggle_state(selected)
                                .on_click(cx.listener(move |this, _, _, cx| { this.selected = Some(ix); cx.notify(); }))
                                .child(
                                    h_flex().gap_1()
                                        .child(Icon::new(IconName::Folder).color(if focused { Color::Accent } else { Color::Muted }))
                                        .child(Label::new(label))
                                        .children(branch.map(|b| Label::new(b).size(ui::LabelSize::Small).color(Color::Muted))),
                                )
                                .into_any_element(),
                            Row::Agent { label, status, .. } => {
                                let (icon, color) = Self::status_icon(status);
                                ListItem::new(("agent", ix))
                                    .spacing(ListItemSpacing::Sparse)
                                    .indent_level(1)
                                    .toggle_state(selected)
                                    .on_click(cx.listener(move |this, _, _, cx| { this.selected = Some(ix); cx.notify(); }))
                                    .on_secondary_mouse_down(cx.listener(move |this, _, window, cx| {
                                        this.selected = Some(ix);
                                        window.dispatch_action(FocusAgent.boxed_clone(), cx);
                                    }))
                                    .child(h_flex().gap_1().child(Icon::new(icon).color(color)).child(Label::new(label)))
                                    .into_any_element()
                            }
                        }
                    }))
                    .into_any_element()
            }
        };
        v_flex()
            .key_context("HerdPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(body)
    }
}

impl Focusable for HerdPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle { self.focus_handle.clone() }
}

impl EventEmitter<PanelEvent> for HerdPanel {}

impl Panel for HerdPanel {
    fn persistent_name() -> &'static str { "HerdPanel" }
    fn panel_key() -> &'static str { "HerdPanel" }
    fn position(&self, _window: &Window, _cx: &App) -> DockPosition { DockPosition::Left }
    fn position_is_valid(&self, position: DockPosition) -> bool { matches!(position, DockPosition::Left | DockPosition::Right) }
    fn set_position(&mut self, _position: DockPosition, _window: &mut Window, _cx: &mut Context<Self>) {}
    fn size(&self, _window: &Window, _cx: &App) -> Pixels { self.width.unwrap_or(px(260.)) }
    fn set_size(&mut self, size: Option<Pixels>, _window: &mut Window, cx: &mut Context<Self>) { self.width = size; cx.notify(); }
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> { Some(IconName::Sparkle) }
    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> { Some("Herd") }
    fn toggle_action(&self) -> Box<dyn Action> { Box::new(ToggleFocus) }
    fn activation_priority(&self) -> u32 { 3 }
}
```

Match the exact `Panel` trait method set against `crates/workspace/src/dock.rs:36` when implementing; copy any additional required methods from `crates/outline_panel/src/outline_panel.rs:5593` onward. Pick an icon that exists in `crates/ui/src/components/icon.rs` (`IconName::Sparkle` is a placeholder; if it is absent use `IconName::Terminal`).

- [ ] **Step 4: Run tests and build**

Run: `cargo test -p herd_panel && cargo build -p zed`
Expected: test passes, app builds. Launch `target/debug/Hulder` with herdr running: the left dock shows a Herd icon; the panel lists your workspaces and agents with status icons.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/herd_panel crates/zed assets/keymaps/default-macos.json
git commit -m "herd_panel: Add Herd dock panel listing herdr workspaces and agents"
```

---

### Task 6: Panel actions: open worktree, focus agent, start herdr, refresh

**Files:**
- Modify: `crates/herd_panel/src/herd_panel.rs` (`init`, plus `impl HerdPanel` action handlers)
- Modify: `assets/keymaps/default-macos.json`: add a block

```json
  {
    "context": "HerdPanel",
    "use_key_equivalents": true,
    "bindings": {
      "enter": "herd_panel::OpenWorktree",
      "f": "herd_panel::FocusAgent",
      "a": "herd_panel::AttachAgent",
      "r": "herd_panel::Refresh",
      "up": "menu::SelectPrevious",
      "down": "menu::SelectNext"
    }
  },
```

**Interfaces:**
- Consumes: `HerdState::client()`, `HerdrClient::request`, `workspace::open_paths` (see `crates/workspace/src/workspace.rs:11153`), `util::command::new_smol_command`.
- Produces: handlers for `OpenWorktree`, `FocusAgent`, `StartHerdr`, `Refresh`, `menu::SelectNext`, `menu::SelectPrevious` on the panel element (`.on_action(cx.listener(...))` in `render`).

- [ ] **Step 1: Write the failing test** (selection movement is pure)

```rust
#[gpui::test]
fn select_next_and_previous_clamp(cx: &mut TestAppContext) {
    cx.update(|_| {
        let mut selected: Option<usize> = None;
        move_selection(&mut selected, 3, 1);
        assert_eq!(selected, Some(0));
        move_selection(&mut selected, 3, 1);
        move_selection(&mut selected, 3, 1);
        move_selection(&mut selected, 3, 1);
        assert_eq!(selected, Some(2));
        move_selection(&mut selected, 3, -1);
        assert_eq!(selected, Some(1));
    });
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p herd_panel select_next`
Expected: `move_selection` undefined.

- [ ] **Step 3: Implement**

```rust
pub fn move_selection(selected: &mut Option<usize>, len: usize, delta: isize) {
    if len == 0 { *selected = None; return; }
    let current = selected.map(|ix| ix as isize).unwrap_or(-1);
    let next = (current + delta).clamp(0, len as isize - 1);
    *selected = Some(next as usize);
}

impl HerdPanel {
    fn open_worktree(&mut self, _: &OpenWorktree, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Row::Workspace { id, .. }) = self.selected_row(cx) else { return };
        let path = self.state.read(cx).workspaces.iter().find(|w| w.id == id).and_then(|w| w.worktree_path.clone());
        let Some(path) = path else {
            log::info!("herd: workspace {id} has no worktree path");
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else { return };
        let app_state = workspace.read(cx).app_state().clone();
        let task = workspace::open_paths(
            &[std::path::PathBuf::from(path)],
            app_state,
            workspace::OpenOptions::default(),
            cx,
        );
        cx.spawn_in(window, async move |_, _| { task.await.log_err(); }).detach();
    }

    fn focus_agent(&mut self, _: &FocusAgent, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(Row::Agent { terminal_id, .. }) = self.selected_row(cx) else { return };
        let Some(client) = self.state.read(cx).client() else { return };
        cx.background_spawn(async move {
            client.request("agent.focus", serde_json::json!({ "target": terminal_id })).await.log_err();
        })
        .detach();
    }

    fn start_herdr(&mut self, _: &StartHerdr, _window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.clone();
        cx.spawn(async move |_, cx| {
            let spawned = util::command::new_smol_command("herdr")
                .arg("server")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match spawned {
                Ok(_child) => {
                    cx.background_executor().timer(std::time::Duration::from_millis(800)).await;
                    state.update(cx, |state, cx| state.request_reconnect(cx)).ok();
                }
                Err(error) => log::error!("herd: could not start herdr: {error}"),
            }
        })
        .detach();
    }

    fn refresh(&mut self, _: &Refresh, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| state.request_reconnect(cx));
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let len = flatten_rows(&self.state.read(cx).workspaces).len();
        move_selection(&mut self.selected, len, 1);
        cx.notify();
    }

    fn select_previous(&mut self, _: &menu::SelectPrevious, _window: &mut Window, cx: &mut Context<Self>) {
        let len = flatten_rows(&self.state.read(cx).workspaces).len();
        move_selection(&mut self.selected, len, -1);
        cx.notify();
    }
}
```

Wire them in `render` on the root `v_flex()`:

```rust
            .on_action(cx.listener(Self::open_worktree))
            .on_action(cx.listener(Self::focus_agent))
            .on_action(cx.listener(Self::start_herdr))
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
```

Add `menu.workspace = true` to the crate's dependencies. Workspace rows get `.on_click` double-click: use `.on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| { this.selected = Some(ix); if event.click_count() == 2 { window.dispatch_action(OpenWorktree.boxed_clone(), cx); } cx.notify(); }))`. Agent rows the same with `AttachAgent` on double click.

- [ ] **Step 4: Run tests and verify manually**

Run: `cargo test -p herd_panel && cargo build -p zed`
Manual: select a workspace, press Enter, Hulder opens that worktree as a project. Select an agent, press `f`, the herdr TUI focuses that pane.

- [ ] **Step 5: Commit**

```bash
git add crates/herd_panel assets/keymaps/default-macos.json
git commit -m "herd_panel: Open worktrees, focus agents, start herdr, keyboard selection"
```

---

### Task 7: Attach agent in the terminal panel

**Files:**
- Modify: `crates/herd_panel/src/herd_panel.rs`
- Modify: `crates/herd_panel/Cargo.toml` (add `task.workspace = true`, `terminal_view.workspace = true`)

**Interfaces:**
- Consumes: `terminal_view::terminal_panel::TerminalPanel`, `TerminalPanel::add_terminal_task(SpawnInTerminal, RevealStrategy, window, cx)` at `crates/terminal_view/src/terminal_panel.rs:881`, `task::{SpawnInTerminal, RevealStrategy, TaskId}`.

- [ ] **Step 1: Write the failing test** (the task builder is pure)

```rust
#[test]
fn attach_task_runs_herdr_agent_attach() {
    let task = attach_task("term_abc", "builder");
    assert_eq!(task.command.as_deref(), Some("herdr"));
    assert_eq!(task.args, vec!["agent", "attach", "term_abc"]);
    assert_eq!(task.label, "herd: builder");
    assert!(task.use_new_terminal);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p herd_panel attach_task`
Expected: undefined.

- [ ] **Step 3: Implement**

```rust
use task::{RevealStrategy, SpawnInTerminal, TaskId};
use terminal_view::terminal_panel::TerminalPanel;

pub fn attach_task(terminal_id: &str, label: &str) -> SpawnInTerminal {
    SpawnInTerminal {
        id: TaskId(format!("herd-attach-{terminal_id}")),
        full_label: format!("herd: {label}"),
        label: format!("herd: {label}"),
        command: Some("herdr".to_string()),
        args: vec!["agent".into(), "attach".into(), terminal_id.to_string()],
        command_label: format!("herdr agent attach {terminal_id}"),
        use_new_terminal: true,
        allow_concurrent_runs: false,
        reveal: RevealStrategy::Always,
        ..SpawnInTerminal::default()
    }
}

impl HerdPanel {
    fn attach_agent(&mut self, _: &AttachAgent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Row::Agent { terminal_id, label, .. }) = self.selected_row(cx) else { return };
        let Some(workspace) = self.workspace.upgrade() else { return };
        let Some(panel) = workspace.read(cx).panel::<TerminalPanel>(cx) else {
            log::warn!("herd: terminal panel not available");
            return;
        };
        workspace.update(cx, |workspace, cx| workspace.open_panel::<TerminalPanel>(window, cx));
        panel.update(cx, |panel, cx| {
            panel.add_terminal_task(attach_task(&terminal_id, &label), RevealStrategy::Always, window, cx)
                .detach_and_log_err(cx);
        });
    }
}
```

Register with `.on_action(cx.listener(Self::attach_agent))`. If `SpawnInTerminal` does not implement `Default` in this checkout (check `crates/task/src/task.rs:41`), fill every field explicitly: `cwd: None, env: HashMap::default(), reveal_target: RevealTarget::Dock, hide: HideStrategy::Never, shell: Shell::System, show_summary: false, show_command: false, show_rerun: false`.

- [ ] **Step 4: Run tests and verify manually**

Run: `cargo test -p herd_panel && cargo build -p zed`
Manual: close the terminal dock, select an agent, press `a`. The terminal dock opens with a tab "herd: <name>" showing the live Claude pane. Review Focus item 4.

- [ ] **Step 5: Commit**

```bash
git add crates/herd_panel
git commit -m "herd_panel: Attach herdr agents inside the terminal panel"
```

---

### Task 8: Blocked-agent toasts

**Files:**
- Modify: `crates/herd_panel/src/herd_panel.rs` (`init` subscribes the workspace to `HerdState` events)

**Interfaces:**
- Consumes: `HerdEvent::AgentStatusChanged`, `workspace::Toast::new(NotificationId, message).on_click(label, fn)`, `NotificationId::composite::<T>(id)`, `Workspace::show_toast`.

- [ ] **Step 1: Write the failing test** (pure decision function)

```rust
#[test]
fn toast_only_when_entering_blocked_or_finishing_work() {
    assert_eq!(toast_for(AgentStatus::Working, AgentStatus::Blocked, "b"), Some(("b needs you".to_string(), false)));
    assert_eq!(toast_for(AgentStatus::Working, AgentStatus::Idle, "b"), Some(("b is idle".to_string(), true)));
    assert_eq!(toast_for(AgentStatus::Idle, AgentStatus::Working, "b"), None);
    assert_eq!(toast_for(AgentStatus::Blocked, AgentStatus::Blocked, "b"), None);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p herd_panel toast_only`

- [ ] **Step 3: Implement**

```rust
/// Returns (message, autohide) when a status change deserves a toast.
pub fn toast_for(previous: AgentStatus, current: AgentStatus, label: &str) -> Option<(String, bool)> {
    match (previous, current) {
        (p, AgentStatus::Blocked) if p != AgentStatus::Blocked => Some((format!("{label} needs you"), false)),
        (AgentStatus::Working, AgentStatus::Idle | AgentStatus::Done) => Some((format!("{label} is idle"), true)),
        _ => None,
    }
}
```

In `init`, inside the `observe_new` closure, after registering the action:

```rust
        let state = HerdState::global(cx);
        let weak = cx.entity().downgrade();
        cx.subscribe(&state, move |workspace, _state, event: &HerdEvent, cx| {
            let HerdEvent::AgentStatusChanged { agent, previous } = event;
            let Some((message, autohide)) = toast_for(*previous, agent.status, &agent.label()) else { return };
            struct HerdToast;
            let id = workspace::notifications::NotificationId::composite::<HerdToast>(agent.pane_id.clone());
            let terminal_id = agent.terminal_id.clone();
            let mut toast = workspace::Toast::new(id, message).on_click("Focus", move |_window, cx| {
                if let Some(client) = HerdState::global(cx).read(cx).client() {
                    let terminal_id = terminal_id.clone();
                    cx.background_spawn(async move {
                        client.request("agent.focus", serde_json::json!({ "target": terminal_id })).await.log_err();
                    }).detach();
                }
            });
            if autohide { toast = toast.autohide(); }
            workspace.show_toast(toast, cx);
        })
        .detach();
```

(`observe_new` gives `cx: &mut Context<Workspace>`, so `cx.subscribe` and `workspace.show_toast` are available directly; drop the unused `weak` if the compiler flags it.)

- [ ] **Step 4: Run tests and verify manually**

Run: `cargo test -p herd_panel && cargo build -p zed`
Manual: in a herdr Claude pane, trigger a permission prompt so the agent goes `blocked`; Hulder shows "<name> needs you" with a Focus button.

- [ ] **Step 5: Commit**

```bash
git add crates/herd_panel
git commit -m "herd_panel: Toast when an agent becomes blocked or finishes working"
```

---

### Task 9: Send selection and file path to an agent

**Files:**
- Modify: `crates/herd_panel/src/herd_panel.rs`
- Modify: `crates/herd_panel/Cargo.toml` (add `editor.workspace = true`, `picker.workspace = true`, `fuzzy.workspace = true`)
- Modify: `assets/keymaps/default-macos.json` top-level `Editor` context block: `"cmd-alt-h": "herd_panel::SendSelection"`, `"cmd-alt-shift-h": "herd_panel::SendFilePath"`.

**Interfaces:**
- Consumes: `editor::Editor` (`selections.newest::<usize>(cx)`, `buffer().read(cx).snapshot(cx).text_for_range`), `workspace.active_item_as::<Editor>(cx)`, `HerdrClient::request("agent.send", {"target", "text"})`, `picker::Picker` with a `PickerDelegate` listing agents.

- [ ] **Step 1: Write the failing test** (payload formatting is pure)

```rust
#[test]
fn file_reference_formats_relative_path_and_lines() {
    assert_eq!(file_reference("src/main.rs", 10, 10), "@src/main.rs:10");
    assert_eq!(file_reference("src/main.rs", 10, 14), "@src/main.rs:10-14");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p herd_panel file_reference`

- [ ] **Step 3: Implement**

```rust
pub fn file_reference(relative_path: &str, start_line: u32, end_line: u32) -> String {
    if start_line == end_line { format!("@{relative_path}:{start_line}") } else { format!("@{relative_path}:{start_line}-{end_line}") }
}

struct AgentPickerDelegate {
    agents: Vec<(String, String)>, // (terminal_id, label)
    matches: Vec<usize>,
    selected: usize,
    on_pick: std::sync::Arc<dyn Fn(String, &mut Window, &mut App)>,
}
```

Implement `picker::PickerDelegate` for it following `crates/language_selector/src/language_selector.rs` exactly (match count, `update_matches` with `fuzzy::match_strings`, `confirm` calling `on_pick(terminal_id)`, `dismissed` emitting `DismissEvent`, `render_match` returning a `ListItem` with the label). Open it with `workspace.toggle_modal(window, cx, |window, cx| Picker::uniform_list(delegate, window, cx))`.

Send handlers on the Workspace (register in `init` next to `ToggleFocus`):

```rust
        workspace.register_action(|workspace, _: &SendSelection, window, cx| {
            let Some(editor) = workspace.active_item_as::<editor::Editor>(cx) else { return };
            let text = editor.update(cx, |editor, cx| {
                let range = editor.selections.newest::<usize>(cx).range();
                editor.buffer().read(cx).snapshot(cx).text_for_range(range).collect::<String>()
            });
            if text.is_empty() { return; }
            pick_agent_and_send(workspace, text, window, cx);
        });
        workspace.register_action(|workspace, _: &SendFilePath, window, cx| {
            let Some(editor) = workspace.active_item_as::<editor::Editor>(cx) else { return };
            let Some((path, start, end)) = editor.update(cx, |editor, cx| {
                let selection = editor.selections.newest::<text::Point>(cx);
                let path = editor.buffer().read(cx).as_singleton()?.read(cx).file()?.path().to_string_lossy().into_owned();
                Some((path, selection.start.row + 1, selection.end.row + 1))
            }) else { return };
            pick_agent_and_send(workspace, file_reference(&path, start, end), window, cx);
        });

fn pick_agent_and_send(workspace: &mut Workspace, text: String, window: &mut Window, cx: &mut Context<Workspace>) {
    let state = HerdState::global(cx);
    let Some(client) = state.read(cx).client() else {
        workspace.show_toast(workspace::Toast::new(
            workspace::notifications::NotificationId::unique::<HerdPanel>(), "herdr is not connected").autohide(), cx);
        return;
    };
    let agents: Vec<(String, String)> = state.read(cx).workspaces.iter()
        .flat_map(|w| w.agents.iter().map(|a| (a.terminal_id.clone(), a.label())))
        .collect();
    let on_pick = std::sync::Arc::new(move |terminal_id: String, _window: &mut Window, cx: &mut App| {
        let client = client.clone();
        let text = text.clone();
        cx.background_spawn(async move {
            client.request("agent.send", serde_json::json!({ "target": terminal_id, "text": text })).await.log_err();
        }).detach();
    });
    workspace.toggle_modal(window, cx, |window, cx| {
        picker::Picker::uniform_list(AgentPickerDelegate { agents, matches: Vec::new(), selected: 0, on_pick }, window, cx)
    });
}
```

Add `text.workspace = true` to dependencies for `text::Point`.

- [ ] **Step 4: Run tests and verify manually**

Run: `cargo test -p herd_panel && cargo build -p zed`
Manual: select a line, press `cmd-alt-h`, pick an agent; the text appears typed into that Claude pane in herdr (herdr's `agent send` writes literal text without Enter, so the agent sees it as draft input).

- [ ] **Step 5: Commit**

```bash
git add crates/herd_panel assets/keymaps/default-macos.json
git commit -m "herd_panel: Send selection or file reference to a herdr agent"
```

---

### Task 10: View menu entry, spec cross-check, clippy

**Files:**
- Modify: `crates/zed/src/zed/app_menus.rs` (after `MenuItem::action("Outline Panel", outline_panel::ToggleFocus),` add `MenuItem::action("Herd Panel", herd_panel::ToggleFocus),`)
- Modify: `crates/zed/src/zed.rs` test `expected_namespaces` list: add `"herd_panel"` in sorted position.

- [ ] **Step 1: Add the menu item and namespace**
- [ ] **Step 2: Run** `./script/clippy -p herdr -p herd_panel -p zed` and fix every warning.
- [ ] **Step 3: Run** `cargo test -p herdr -p herd_panel` and `cargo test -p zed test_actions_namespaces` (name may differ; run `cargo test -p zed namespaces`).
- [ ] **Step 4: Manual acceptance:** with herdr running, launch `target/debug/Hulder`, open the Herd panel from View, confirm the list matches `herdr workspace list` and `herdr agent list`, attach one agent, send one selection, and run `lsof -nP -i -a -p $(pgrep -x Hulder)` to confirm there are still no network sockets.
- [ ] **Step 5: Commit**

```bash
git add crates/zed
git commit -m "zed: Expose the Herd panel in the View menu"
```
