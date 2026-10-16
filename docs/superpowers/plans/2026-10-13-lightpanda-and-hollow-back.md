# Lightpanda and Hollow Back Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A status bar eye that shows every refused outbound request, and Lightpanda as the one explicit way Hulder reaches the web: a managed local CDP server, read-only Markdown buffers from `lightpanda fetch`, a test run target, and MCP handoff to herdr agents.

**Architecture:** `crates/hulder_net` wraps `BlockedHttpClient` with a recording layer and a `NetLog` global entity; the `HollowBack` status item renders it. `crates/lightpanda` owns a `PandaRunner` entity (child process, stdout tail into `NetLog`), the `fetch`-to-buffer actions, the task template, and the MCP config writer. Both plug into `main.rs` and the status bar in `zed.rs`.

**Tech Stack:** Rust 1.98, gpui entities, `util::command::new_smol_command`, `smol` process + `BufReader::lines`, `workspace::StatusItemView`, `project.create_buffer` + `Editor::for_buffer` + `set_read_only(true)` (pattern at `crates/zed/src/main.rs` BuiltinJsonSchema handler), `task::TaskTemplate`, serde_json for `.mcp.json`.

**Spec:** `docs/superpowers/specs/2026-10-13-hulder-design.md`, sections 1 and 6.

## Global Constraints

- The global HTTP client must still fail every request with `PermissionDenied`; recording never changes the outcome.
- Lightpanda binary default: `~/.local/bin/lightpanda`, overridable by `lightpanda.binary`. Serve flags: `serve --host 127.0.0.1 --port <port> --log-level info` plus `--obey-robots` when `lightpanda.obey_robots` is true.
- Fetch flags: `fetch --dump markdown --strip-mode js,css <url>` and `--dump semantic_tree_text` for the semantic variant.
- Settings keys (all under `"lightpanda"`): `enabled: false`, `binary: null`, `port: 9222`, `obey_robots: true`. When `enabled` is false every lightpanda action is hidden and a no-op.
- Follow repo CLAUDE.md (no `unwrap()` outside tests, no `let _ =`, no `mod.rs`, `./script/clippy`). Keep README's marker.

## Review Focus

1. Lightpanda binary missing: toggling the runner must show a toast with the path it tried, not panic or spin. Test in Task 5 (spawn failure sets `RunnerState::Failed(reason)`).
2. Port already in use: the process exits immediately; the runner must surface the first stderr line. Test in Task 5 (stub script that exits 1 with a message).
3. Fetch of a URL that returns nothing (empty Markdown): open the buffer anyway with a one-line note, so the user knows the fetch ran. Test in Task 6.
4. `.mcp.json` already has other servers: the writer must merge, not clobber, and must refuse if the file is not valid JSON. Test in Task 8.
5. The eye's ring is bounded: 10,000 refused requests must not grow memory without limit. Test in Task 1 (ring keeps the newest 1000).

---

### Task 1: `hulder_net` recording client and `NetLog`

**Files:**
- Create: `crates/hulder_net/Cargo.toml`
- Create: `crates/hulder_net/src/hulder_net.rs`
- Modify: `Cargo.toml` workspace deps: `hulder_net = { path = "crates/hulder_net" }`

**Interfaces:**
- Produces:
  ```rust
  pub enum NetEntryKind { Refused, Browsed }
  pub struct NetEntry { pub at: std::time::SystemTime, pub kind: NetEntryKind, pub method: String, pub url: String }
  pub struct NetLog { entries: VecDeque<NetEntry>, pub refused: u64, pub browsed: u64 }
  impl NetLog { pub fn push(&mut self, entry: NetEntry); pub fn entries(&self) -> impl Iterator<Item=&NetEntry>; pub fn global(cx: &App) -> Entity<NetLog>; }
  pub fn init(cx: &mut App) -> Arc<RecordingBlockedHttpClient>   // registers the global and returns the client to install
  pub struct RecordingBlockedHttpClient { .. } impl http_client::HttpClient
  pub const MAX_ENTRIES: usize = 1000;
  ```
  `RecordingBlockedHttpClient` cannot touch gpui from `send` (it runs on any thread), so it pushes into an `Arc<Mutex<VecDeque<NetEntry>>>` and bumps an `AtomicU64`; `NetLog::drain_pending(cx)` is called from a foreground `cx.spawn` loop every 250 ms that the `init` function starts.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{AsyncBody, HttpClient, Request};

    #[test]
    fn send_is_refused_and_recorded() {
        let shared = SharedPending::default();
        let client = RecordingBlockedHttpClient::new(shared.clone());
        let request = Request::builder().method("GET").uri("https://example.com/x").body(AsyncBody::empty()).expect("req");
        let result = smol::block_on(client.send(request));
        let error = result.err().expect("must be refused");
        assert!(error.to_string().contains("disallowed"));
        let pending = shared.take();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].url, "https://example.com/x");
        assert_eq!(pending[0].method, "GET");
    }

    #[test]
    fn log_is_bounded() {
        let mut log = NetLog::default();
        for i in 0..(MAX_ENTRIES + 50) {
            log.push(NetEntry { at: std::time::SystemTime::UNIX_EPOCH, kind: NetEntryKind::Refused, method: "GET".into(), url: format!("u{i}") });
        }
        assert_eq!(log.entries().count(), MAX_ENTRIES);
        assert_eq!(log.refused, (MAX_ENTRIES + 50) as u64);
        assert_eq!(log.entries().next().map(|e| e.url.as_str()), Some("u1049"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p hulder_net`
Expected: crate missing.

- [ ] **Step 3: Implement**

`Cargo.toml` deps: `anyhow, collections, futures, gpui, http_client, log, parking_lot, smol`; dev: `gpui` test-support.

```rust
use gpui::{App, AppContext as _, Context, Entity, Global};
use http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub const MAX_ENTRIES: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetEntryKind { Refused, Browsed }

#[derive(Debug, Clone)]
pub struct NetEntry { pub at: SystemTime, pub kind: NetEntryKind, pub method: String, pub url: String }

#[derive(Default, Clone)]
pub struct SharedPending(Arc<Mutex<Vec<NetEntry>>>);
impl SharedPending {
    pub fn push(&self, entry: NetEntry) { self.0.lock().push(entry); }
    pub fn take(&self) -> Vec<NetEntry> { std::mem::take(&mut *self.0.lock()) }
}

#[derive(Default)]
pub struct NetLog { entries: VecDeque<NetEntry>, pub refused: u64, pub browsed: u64 }

impl NetLog {
    pub fn push(&mut self, entry: NetEntry) {
        match entry.kind { NetEntryKind::Refused => self.refused += 1, NetEntryKind::Browsed => self.browsed += 1 }
        self.entries.push_front(entry);
        self.entries.truncate(MAX_ENTRIES);
    }
    pub fn entries(&self) -> impl Iterator<Item = &NetEntry> { self.entries.iter() }
    pub fn global(cx: &App) -> Entity<NetLog> { cx.global::<GlobalNetLog>().0.clone() }
    pub fn record_browsed(cx: &mut App, url: String) {
        Self::global(cx).update(cx, |log, cx| {
            log.push(NetEntry { at: SystemTime::now(), kind: NetEntryKind::Browsed, method: "GET".into(), url });
            cx.notify();
        });
    }
}

struct GlobalNetLog(Entity<NetLog>);
impl Global for GlobalNetLog {}

pub struct RecordingBlockedHttpClient { pending: SharedPending }
impl RecordingBlockedHttpClient { pub fn new(pending: SharedPending) -> Self { Self { pending } } }

impl HttpClient for RecordingBlockedHttpClient {
    fn send(&self, request: Request<AsyncBody>) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        self.pending.push(NetEntry {
            at: SystemTime::now(), kind: NetEntryKind::Refused,
            method: request.method().to_string(), url: request.uri().to_string(),
        });
        Box::pin(async {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Hulder disallowed request").into())
        })
    }
    fn user_agent(&self) -> Option<&HeaderValue> { None }
    fn proxy(&self) -> Option<&Url> { None }
    #[cfg(feature = "test-support")]
    fn as_fake(&self) -> &http_client::FakeHttpClient { panic!("not a fake client") }
}

pub fn init(cx: &mut App) -> Arc<RecordingBlockedHttpClient> {
    let pending = SharedPending::default();
    let log = cx.new(|_| NetLog::default());
    cx.set_global(GlobalNetLog(log.clone()));
    let drain_pending = pending.clone();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_millis(250)).await;
            let batch = drain_pending.take();
            if batch.is_empty() { continue; }
            if log.update(cx, |log, cx| { for entry in batch { log.push(entry); } cx.notify(); }).is_err() { break; }
        }
    })
    .detach();
    Arc::new(RecordingBlockedHttpClient::new(pending))
}
```

Mirror `BlockedHttpClient`'s exact trait method list from `crates/http_client/src/http_client.rs:400` (it may have `as_fake` only under a feature; copy what is there).

Then in `crates/zed/src/main.rs` replace `cx.set_http_client(Arc::new(BlockedHttpClient::new()));` with `cx.set_http_client(hulder_net::init(cx));`, drop the `BlockedHttpClient` import, add `hulder_net.workspace = true` to `crates/zed/Cargo.toml`.

- [ ] **Step 4: Run tests and build**

Run: `cargo test -p hulder_net && cargo build -p zed`

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/hulder_net crates/zed
git commit -m "hulder_net: Record refused requests behind the blocked HTTP client"
```

---

### Task 2: Hollow Back status bar item and log modal

**Files:**
- Create: `crates/hulder_net/src/hollow_back.rs` (add `pub mod hollow_back;` to `hulder_net.rs`)
- Modify: `crates/zed/src/zed.rs` status bar block near `status_bar.add_right_item(...)`: add `let hollow_back = cx.new(|cx| hulder_net::hollow_back::HollowBack::new(cx)); status_bar.add_right_item(hollow_back, window, cx);` before the Vim mode indicator item.
- Deps for `hulder_net`: add `ui, workspace, picker, time_format` (`time_format` for timestamps; check it exists in workspace deps, else format with `chrono` which is a workspace dep).

**Interfaces:**
- Produces: `HollowBack` implementing `Render` and `workspace::StatusItemView`; action `hollow_back::Toggle` opening a modal `NetLogView` (a `Picker` with a read-only delegate listing entries; `Picker` is used so filtering by host works for free).

- [ ] **Step 1: Write the failing test** (label formatting is pure)

```rust
#[test]
fn status_label_shows_both_counts_only_when_nonzero() {
    assert_eq!(status_label(0, 0), "0");
    assert_eq!(status_label(12, 0), "12");
    assert_eq!(status_label(12, 3), "12 · 3");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p hulder_net status_label`

- [ ] **Step 3: Implement**

```rust
pub fn status_label(refused: u64, browsed: u64) -> String {
    if browsed == 0 { refused.to_string() } else { format!("{refused} · {browsed}") }
}

pub struct HollowBack { log: Entity<NetLog>, _subscription: Subscription }

impl HollowBack {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let log = NetLog::global(cx);
        let subscription = cx.observe(&log, |_, _, cx| cx.notify());
        Self { log, _subscription: subscription }
    }
}

impl Render for HollowBack {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let log = self.log.read(cx);
        let label = status_label(log.refused, log.browsed);
        Button::new("hollow-back", label)
            .icon(IconName::Eye)
            .icon_position(IconPosition::Start)
            .tooltip(Tooltip::text("Hollow Back: refused outbound requests · pages browsed via Lightpanda"))
            .on_click(cx.listener(|_, _, window, cx| window.dispatch_action(Toggle.boxed_clone(), cx)))
    }
}

impl StatusItemView for HollowBack {
    fn set_active_pane_item(&mut self, _: Option<&dyn ItemHandle>, _: &mut Window, _: &mut Context<Self>) {}
}
```

Copy the full `StatusItemView` method list from `crates/workspace/src/status_bar.rs:42` (there is a second method about hiding via settings; return `None` from it). Register `Toggle` on the workspace in a `pub fn init(cx)` inside `hollow_back.rs` using `cx.observe_new(|workspace: &mut Workspace, ...| workspace.register_action(...))` which opens `workspace.toggle_modal(window, cx, |window, cx| Picker::uniform_list(NetLogDelegate::new(cx), window, cx))`. `NetLogDelegate` lists `"{HH:MM:SS}  {REFUSED|BROWSED}  {METHOD}  {url}"` strings and matches on the url with `fuzzy::match_strings`. Pick a real `IconName` (`IconName::Eye` exists in `crates/ui/src/components/icon.rs`; verify).

Call `hulder_net::hollow_back::init(cx)` in `main.rs` after `hulder_net::init`.

- [ ] **Step 4: Run and verify**

Run: `cargo test -p hulder_net && cargo build -p zed`
Manual: launch, open a `.ts` file so ESLint tries to download; the eye count rises; click it, the modal lists the refused GitHub URL.

- [ ] **Step 5: Commit**

```bash
git add crates/hulder_net crates/zed
git commit -m "hulder_net: Add Hollow Back status item and log modal"
```

---

### Task 3: Lightpanda settings

**Files:**
- Create: `crates/lightpanda/Cargo.toml`, `crates/lightpanda/src/lightpanda.rs`, `crates/lightpanda/src/settings.rs`
- Modify: `crates/settings_content/src/settings_content.rs`: add `pub lightpanda: Option<LightpandaSettingsContent>,` to `SettingsContent` and a new file `crates/settings_content/src/lightpanda.rs` with the content struct.
- Modify: `assets/settings/default.json`: add before `"node": {`:

```json
  // Lightpanda headless browser. Off by default: when enabled, Hulder may
  // run `lightpanda` on your behalf and every page it loads is logged in
  // the Hollow Back.
  "lightpanda": {
    "enabled": false,
    "binary": null,
    "port": 9222,
    "obey_robots": true,
  },
```

- Modify: `Cargo.toml` workspace deps: `lightpanda = { path = "crates/lightpanda" }`.

**Interfaces:**
- Produces: `settings_content::LightpandaSettingsContent { enabled: Option<bool>, binary: Option<String>, port: Option<u16>, obey_robots: Option<bool> }` and `lightpanda::LightpandaSettings { enabled: bool, binary: PathBuf, port: u16, obey_robots: bool }` implementing `settings::Settings` like `VimSettings` at `crates/vim/src/vim.rs:2426`. `LightpandaSettings::register(cx)` is called from `lightpanda::init`.

- [ ] **Step 1: Write the failing test**

```rust
#[gpui::test]
fn defaults_resolve_from_default_json(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = settings::SettingsStore::test(cx);
        cx.set_global(store);
        LightpandaSettings::register(cx);
        let settings = LightpandaSettings::get_global(cx);
        assert!(!settings.enabled);
        assert_eq!(settings.port, 9222);
        assert!(settings.binary.ends_with(".local/bin/lightpanda"));
    });
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p lightpanda`

- [ ] **Step 3: Implement**

```rust
// crates/settings_content/src/lightpanda.rs
use serde::{Deserialize, Serialize};
use settings_macros::MergeFrom;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, MergeFrom, schemars::JsonSchema)]
pub struct LightpandaSettingsContent {
    /// Whether Hulder may run Lightpanda at all.
    pub enabled: Option<bool>,
    /// Path to the lightpanda binary. Defaults to ~/.local/bin/lightpanda.
    pub binary: Option<String>,
    /// CDP port for `lightpanda serve`.
    pub port: Option<u16>,
    /// Pass --obey-robots to serve.
    pub obey_robots: Option<bool>,
}
```

Copy the derive list from a neighbouring small content struct (for example `TitleBarSettingsContent`) so the macros match this checkout.

```rust
// crates/lightpanda/src/settings.rs
use settings::{Settings, SettingsContent};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct LightpandaSettings { pub enabled: bool, pub binary: PathBuf, pub port: u16, pub obey_robots: bool }

impl Settings for LightpandaSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let content = content.lightpanda.clone().unwrap_or_default();
        Self {
            enabled: content.enabled.unwrap_or(false),
            binary: content.binary.map(PathBuf::from).unwrap_or_else(|| paths::home_dir().join(".local/bin/lightpanda")),
            port: content.port.unwrap_or(9222),
            obey_robots: content.obey_robots.unwrap_or(true),
        }
    }
}
```

If `Settings::from_settings` in this checkout requires `.unwrap()` on content fields because defaults are guaranteed by `default.json` (as `VimSettings` does), keep the `unwrap_or` form anyway: it is safer and `clippy` accepts it.

- [ ] **Step 4: Run tests**

Run: `cargo test -p lightpanda && cargo build -p zed`

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/lightpanda crates/settings_content assets/settings/default.json
git commit -m "lightpanda: Add settings and crate scaffold"
```

---

### Task 4: Lightpanda `fetch` wrapper (pure process helper)

**Files:**
- Create: `crates/lightpanda/src/fetch.rs`
- Test: same file

**Interfaces:**
- Produces: `pub enum Dump { Markdown, SemanticText }`, `pub fn fetch_args(dump: Dump, url: &str) -> Vec<String>`, `pub async fn fetch(binary: &Path, dump: Dump, url: &str) -> anyhow::Result<String>` (runs the binary via `util::command::new_smol_command`, returns stdout, errors include the first 200 bytes of stderr).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn fetch_args_markdown() {
    assert_eq!(fetch_args(Dump::Markdown, "https://x.test/a"), vec!["fetch", "--dump", "markdown", "--strip-mode", "js,css", "https://x.test/a"]);
}
#[test]
fn fetch_args_semantic() {
    assert_eq!(fetch_args(Dump::SemanticText, "https://x.test/a"), vec!["fetch", "--dump", "semantic_tree_text", "--strip-mode", "js,css", "https://x.test/a"]);
}
#[test]
fn fetch_runs_binary_and_returns_stdout() {
    let dir = tempfile::tempdir().expect("tmp");
    let script = dir.path().join("fake-panda");
    std::fs::write(&script, "#!/bin/sh\necho \"# hello from $6\"\n").expect("write");
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");
    let out = smol::block_on(fetch(&script, Dump::Markdown, "https://x.test/a")).expect("ok");
    assert_eq!(out.trim(), "# hello from https://x.test/a");
}
#[test]
fn fetch_reports_stderr_on_failure() {
    let dir = tempfile::tempdir().expect("tmp");
    let script = dir.path().join("fake-panda");
    std::fs::write(&script, "#!/bin/sh\necho boom >&2\nexit 3\n").expect("write");
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");
    let err = smol::block_on(fetch(&script, Dump::Markdown, "https://x.test/a")).err().expect("err");
    assert!(err.to_string().contains("boom"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p lightpanda fetch`

- [ ] **Step 3: Implement**

```rust
use anyhow::{Context as _, Result, anyhow};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dump { Markdown, SemanticText }

pub fn fetch_args(dump: Dump, url: &str) -> Vec<String> {
    let dump = match dump { Dump::Markdown => "markdown", Dump::SemanticText => "semantic_tree_text" };
    ["fetch", "--dump", dump, "--strip-mode", "js,css", url].iter().map(|s| s.to_string()).collect()
}

pub async fn fetch(binary: &Path, dump: Dump, url: &str) -> Result<String> {
    let output = util::command::new_smol_command(binary)
        .args(fetch_args(dump, url))
        .output()
        .await
        .with_context(|| format!("running {}", binary.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let head: String = stderr.chars().take(200).collect();
        return Err(anyhow!("lightpanda fetch failed ({}): {}", output.status, head.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p lightpanda`

- [ ] **Step 5: Commit**

```bash
git add crates/lightpanda
git commit -m "lightpanda: Add fetch wrapper around the lightpanda CLI"
```

---

### Task 5: `PandaRunner` entity (serve process, state, stdout into NetLog)

**Files:**
- Create: `crates/lightpanda/src/runner.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum RunnerState { Stopped, Starting, Running { port: u16 }, Failed { reason: String } }
  pub struct PandaRunner { pub state: RunnerState, child: Option<smol::process::Child>, _tail: Option<Task<()>> }
  impl PandaRunner {
      pub fn global(cx: &App) -> Entity<PandaRunner>;
      pub fn serve_args(port: u16, obey_robots: bool) -> Vec<String>;
      pub fn start(&mut self, cx: &mut Context<Self>);
      pub fn stop(&mut self, cx: &mut Context<Self>);
      pub fn toggle(&mut self, cx: &mut Context<Self>);
      pub fn ws_url(&self) -> Option<String>;   // "ws://127.0.0.1:<port>" when Running
  }
  pub fn parse_page_url(log_line: &str) -> Option<String>; // extracts the URL from a serve log line mentioning a navigation
  ```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn serve_args_include_robots_when_asked() {
    assert_eq!(PandaRunner::serve_args(9333, true), vec!["serve", "--host", "127.0.0.1", "--port", "9333", "--log-level", "info", "--obey-robots"]);
    assert_eq!(PandaRunner::serve_args(9333, false), vec!["serve", "--host", "127.0.0.1", "--port", "9333", "--log-level", "info"]);
}
#[test]
fn parse_page_url_finds_navigation_lines() {
    assert_eq!(parse_page_url(r#"$time=1 $scope=page $level=info $msg=navigate url=https://a.test/p"#), Some("https://a.test/p".into()));
    assert_eq!(parse_page_url(r#"$time=1 $scope=cdp $level=info $msg=connected"#), None);
}
#[gpui::test]
async fn runner_reports_failure_when_binary_exits(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().expect("tmp");
    let script = dir.path().join("fake-panda");
    std::fs::write(&script, "#!/bin/sh\necho 'address in use' >&2\nexit 1\n").expect("write");
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");
    let runner = cx.new(|_| PandaRunner::with_binary(script, 9555, false));
    runner.update(cx, |runner, cx| runner.start(cx));
    cx.run_until_parked();
    cx.background_executor.timer(std::time::Duration::from_millis(300)).await;
    cx.run_until_parked();
    runner.read_with(cx, |runner, _| {
        assert!(matches!(&runner.state, RunnerState::Failed { reason } if reason.contains("address in use")), "{:?}", runner.state);
    });
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p lightpanda runner`

- [ ] **Step 3: Implement**

```rust
use crate::settings::LightpandaSettings;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use hulder_net::NetLog;
use settings::Settings;
use smol::io::{AsyncBufReadExt, BufReader};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerState { Stopped, Starting, Running { port: u16 }, Failed { reason: String } }

pub struct PandaRunner {
    pub state: RunnerState,
    binary: PathBuf,
    port: u16,
    obey_robots: bool,
    child: Option<smol::process::Child>,
    _tail: Option<Task<()>>,
}

struct GlobalPandaRunner(Entity<PandaRunner>);
impl Global for GlobalPandaRunner {}

pub fn parse_page_url(line: &str) -> Option<String> {
    if !line.contains("$msg=navigate") { return None; }
    line.split_whitespace().find_map(|token| token.strip_prefix("url=").map(str::to_string))
}

impl PandaRunner {
    pub fn with_binary(binary: PathBuf, port: u16, obey_robots: bool) -> Self {
        Self { state: RunnerState::Stopped, binary, port, obey_robots, child: None, _tail: None }
    }
    pub fn from_settings(cx: &App) -> Self {
        let settings = LightpandaSettings::get_global(cx);
        Self::with_binary(settings.binary.clone(), settings.port, settings.obey_robots)
    }
    pub fn global(cx: &App) -> Entity<PandaRunner> { cx.global::<GlobalPandaRunner>().0.clone() }
    pub fn ws_url(&self) -> Option<String> {
        match self.state { RunnerState::Running { port } => Some(format!("ws://127.0.0.1:{port}")), _ => None }
    }
    pub fn serve_args(port: u16, obey_robots: bool) -> Vec<String> {
        let mut args: Vec<String> = ["serve", "--host", "127.0.0.1", "--port", &port.to_string(), "--log-level", "info"].iter().map(|s| s.to_string()).collect();
        if obey_robots { args.push("--obey-robots".into()); }
        args
    }
    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        match self.state { RunnerState::Running { .. } | RunnerState::Starting => self.stop(cx), _ => self.start(cx) }
    }
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(mut child) = self.child.take() { child.kill().ok(); }
        self._tail = None;
        self.state = RunnerState::Stopped;
        cx.notify();
    }
    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        self.state = RunnerState::Starting;
        cx.notify();
        let spawned = util::command::new_smol_command(&self.binary)
            .args(Self::serve_args(self.port, self.obey_robots))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                self.state = RunnerState::Failed { reason: format!("{}: {error}", self.binary.display()) };
                cx.notify();
                return;
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let port = self.port;
        self.child = Some(child);
        self._tail = Some(cx.spawn(async move |this, cx| {
            let mut first_stderr: Option<String> = None;
            let stdout_task = {
                let this = this.clone();
                let mut cx = cx.clone();
                cx.spawn(async move |cx| {
                    let Some(stdout) = stdout else { return };
                    let mut lines = BufReader::new(stdout).lines();
                    while let Some(Ok(line)) = lines.next().await {
                        if let Some(url) = parse_page_url(&line) {
                            if cx.update(|cx| NetLog::record_browsed(cx, url)).is_err() { return; }
                        }
                    }
                    let _ = this;
                })
            };
            if let Some(stderr) = stderr {
                let mut lines = BufReader::new(stderr).lines();
                while let Some(Ok(line)) = lines.next().await {
                    if first_stderr.is_none() && !line.trim().is_empty() { first_stderr = Some(line.clone()); }
                    if line.contains("listening") || line.contains("$msg=serve") {
                        this.update(cx, |this, cx| { this.state = RunnerState::Running { port }; cx.notify(); }).ok();
                    }
                }
            }
            // stderr closed: the process is gone
            let reason = first_stderr.unwrap_or_else(|| "lightpanda exited".into());
            this.update(cx, |this, cx| {
                if !matches!(this.state, RunnerState::Stopped) {
                    this.state = RunnerState::Failed { reason };
                    this.child = None;
                    cx.notify();
                }
            }).ok();
            stdout_task.await;
        }));
        // Treat the server as running once it has stayed alive for a second; lightpanda prints
        // its listen line to stderr at info level, but a stable process is a fine fallback.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
            this.update(cx, |this, cx| {
                if matches!(this.state, RunnerState::Starting) && this.child.is_some() {
                    this.state = RunnerState::Running { port };
                    cx.notify();
                }
            }).ok();
        }).detach();
    }
}

pub fn init(cx: &mut App) {
    let runner = cx.new(|cx| PandaRunner::from_settings(cx));
    cx.set_global(GlobalPandaRunner(runner));
}
```

Verify the exact log line lightpanda prints on listen by running `~/.local/bin/lightpanda serve --port 9777 --log-level info` once and reading the first lines; adjust the two `contains` checks and `parse_page_url` to the real tokens, and put a real line into the `parse_page_url` test.

Call `lightpanda::init(cx)` (which calls `LightpandaSettings::register`, `runner::init`) in `main.rs` after `hulder_net::hollow_back::init(cx)`; on app quit (`cx.on_app_quit`) call `PandaRunner::global(cx).update(cx, |r, cx| r.stop(cx))`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p lightpanda && cargo build -p zed`

- [ ] **Step 5: Commit**

```bash
git add crates/lightpanda crates/zed
git commit -m "lightpanda: Add PandaRunner managing a local lightpanda serve process"
```

---

### Task 6: Reader buffers and panda status item

**Files:**
- Create: `crates/lightpanda/src/reader.rs`, `crates/lightpanda/src/status.rs`
- Modify: `crates/zed/src/zed.rs` status bar: `let panda = cx.new(|cx| lightpanda::status::PandaStatus::new(cx)); status_bar.add_right_item(panda, window, cx);` right after the Hollow Back item.
- Modify: `crates/zed/src/zed/app_menus.rs` Go menu: `MenuItem::action("Open URL as Markdown…", lightpanda::OpenUrl), MenuItem::action("Open URL as Semantic Text…", lightpanda::OpenUrlSemantic)`.

**Interfaces:**
- Produces: actions `lightpanda::{OpenUrl, OpenUrlSemantic, ToggleServer}`; `pub fn open_reader(workspace: &mut Workspace, url: String, dump: Dump, window, cx)`; `pub fn reader_text(url: &str, body: &str) -> String` (prepends `<!-- fetched from {url} by Lightpanda -->` and, when body is blank, the line `_Lightpanda returned no text for this page._`).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn reader_text_handles_empty_body() {
    let text = reader_text("https://a.test", "   \n");
    assert!(text.starts_with("<!-- fetched from https://a.test by Lightpanda -->"));
    assert!(text.contains("returned no text"));
    let text = reader_text("https://a.test", "# Hi");
    assert!(text.ends_with("# Hi"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p lightpanda reader_text`

- [ ] **Step 3: Implement**

```rust
pub fn reader_text(url: &str, body: &str) -> String {
    let header = format!("<!-- fetched from {url} by Lightpanda -->\n\n");
    if body.trim().is_empty() { format!("{header}_Lightpanda returned no text for this page._\n") } else { format!("{header}{body}") }
}

pub fn open_reader(workspace: &mut Workspace, url: String, dump: Dump, window: &mut Window, cx: &mut Context<Workspace>) {
    let settings = LightpandaSettings::get_global(cx).clone();
    if !settings.enabled {
        workspace.show_toast(Toast::new(NotificationId::unique::<OpenUrl>(), "Lightpanda is disabled in settings (lightpanda.enabled)").autohide(), cx);
        return;
    }
    let project = workspace.project().clone();
    let languages = workspace.app_state().languages.clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let body = fetch(&settings.binary, dump, &url).await;
        let text = match body {
            Ok(body) => reader_text(&url, &body),
            Err(error) => {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_toast(Toast::new(NotificationId::unique::<OpenUrl>(), format!("Lightpanda: {error}")), cx);
                }).ok();
                return anyhow::Ok(());
            }
        };
        cx.update(|_, cx| NetLog::record_browsed(cx, url.clone()))?;
        let markdown = languages.language_for_name("Markdown").await.ok();
        let buffer = project.update_in(cx, |project, _, cx| project.create_buffer(markdown, false, cx))?.await?;
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, text)], None, cx))?;
        workspace.update_in(cx, |workspace, window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = editor::Editor::for_buffer(buffer, Some(project.clone()), window, cx);
                editor.set_read_only(true);
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}
```

Register in `lightpanda::init` with `cx.observe_new(|workspace: &mut Workspace, _, _| { workspace.register_action(|workspace, _: &OpenUrl, window, cx| prompt_for_url(workspace, Dump::Markdown, window, cx)); ... })`. `prompt_for_url` opens a single-line modal: reuse `workspace::prompt_for_input`-style helper if present; otherwise a tiny `Picker` with a `confirm` on the typed query (see `crates/go_to_line` for the single-editor modal pattern) that calls `open_reader`.

`status.rs`: `PandaStatus` mirrors `HollowBack` (Task 2) with icon `IconName::Globe` (verify exists), color green when `Running`, muted when stopped, red when `Failed`, tooltip showing the reason, and click dispatching `ToggleServer`. Hidden entirely (`return div()` empty) when `LightpandaSettings.enabled` is false.

- [ ] **Step 4: Run, build, verify**

Run: `cargo test -p lightpanda && cargo build -p zed`
Manual: set `"lightpanda": {"enabled": true}` in `~/.config/hulder/settings.json`; Go › Open URL as Markdown › `https://example.com`; a read-only Markdown tab opens, the eye's browsed count becomes 1.

- [ ] **Step 5: Commit**

```bash
git add crates/lightpanda crates/zed
git commit -m "lightpanda: Reader buffers from fetch and a status bar runner toggle"
```

---

### Task 7: Run target task template

**Files:**
- Create: `crates/lightpanda/src/run_target.rs`
- Modify: `crates/lightpanda/src/lightpanda.rs` (`init` registers the template provider)

**Interfaces:**
- Consumes: `task::{TaskTemplate, TaskTemplates}`, `project::task_store::TaskStore` (grep `fn register_static_task_source` or how `tasks_ui` tests build `ContextProviderWithTasks` at `crates/tasks_ui/src/modal.rs:1148`; use the same mechanism to add a global static source).
- Produces: `pub fn lightpanda_task(ws_url: &str, test_command: &str) -> TaskTemplate` with `label: "Run tests against Lightpanda"`, `command: test_command`, `env: {"LIGHTPANDA_WS": ws_url, "PLAYWRIGHT_CDP_ENDPOINT": ws_url}`; `pub fn detect_test_command(package_json: &str) -> Option<String>` returning `npm run test:e2e` if that script exists, else `npm test` if `test` exists.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn detect_prefers_e2e_script() {
    assert_eq!(detect_test_command(r#"{"scripts":{"test":"vitest","test:e2e":"playwright test"}}"#), Some("npm run test:e2e".into()));
    assert_eq!(detect_test_command(r#"{"scripts":{"test":"vitest"}}"#), Some("npm test".into()));
    assert_eq!(detect_test_command(r#"{"name":"x"}"#), None);
    assert_eq!(detect_test_command("not json"), None);
}
#[test]
fn task_sets_both_env_vars() {
    let task = lightpanda_task("ws://127.0.0.1:9222", "npm test");
    assert_eq!(task.env.get("LIGHTPANDA_WS").map(String::as_str), Some("ws://127.0.0.1:9222"));
    assert_eq!(task.env.get("PLAYWRIGHT_CDP_ENDPOINT").map(String::as_str), Some("ws://127.0.0.1:9222"));
    assert_eq!(task.command, "npm test");
}
```

- [ ] **Step 2: Run to verify failure**
- [ ] **Step 3: Implement** both functions with `serde_json::from_str::<serde_json::Value>` and `TaskTemplate { label: ..., command: ..., env: HashMap::from([...]), ..Default::default() }`. The provider: on `OpenRunTarget` action (bind in command palette as "lightpanda: run tests against lightpanda"), ensure `PandaRunner` is running (start it and wait for `ws_url()` up to 5 s), read `<worktree root>/package.json` via `project.fs()`, detect the command (prompt with the url modal from Task 6 if none), then spawn via `TerminalPanel::add_terminal_task(SpawnInTerminal { command: Some(cmd), env, label, ..Default::default() }, RevealStrategy::Always, window, cx)`.
- [ ] **Step 4: Run tests and build**
- [ ] **Step 5: Commit** `git commit -m "lightpanda: Run tests against the local lightpanda CDP endpoint"`

---

### Task 8: MCP handoff writer

**Files:**
- Create: `crates/lightpanda/src/mcp.rs`

**Interfaces:**
- Produces: `pub fn merge_mcp_config(existing: Option<&str>, binary: &Path) -> anyhow::Result<String>`; action `lightpanda::ShareWithAgents` which calls it on `<worktree root>/.mcp.json`, shows the result as a diff in a buffer when the file existed (open both texts; a plain two-tab display is enough), and writes on confirm via a toast with an "Apply" button.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn creates_file_when_absent() {
    let out = merge_mcp_config(None, Path::new("/usr/local/bin/lightpanda")).expect("ok");
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["mcpServers"]["lightpanda"]["command"], "/usr/local/bin/lightpanda");
    assert_eq!(v["mcpServers"]["lightpanda"]["args"][0], "mcp");
}
#[test]
fn merges_without_clobbering_other_servers() {
    let existing = r#"{"mcpServers":{"other":{"command":"x"}}}"#;
    let out = merge_mcp_config(Some(existing), Path::new("/p")).expect("ok");
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["mcpServers"]["other"]["command"], "x");
    assert_eq!(v["mcpServers"]["lightpanda"]["command"], "/p");
}
#[test]
fn refuses_invalid_json() {
    assert!(merge_mcp_config(Some("{nope"), Path::new("/p")).is_err());
}
```

- [ ] **Step 2: Run to verify failure**
- [ ] **Step 3: Implement**

```rust
pub fn merge_mcp_config(existing: Option<&str>, binary: &Path) -> anyhow::Result<String> {
    let mut root: serde_json::Value = match existing {
        Some(text) => serde_json::from_str(text).context(".mcp.json is not valid JSON; fix it by hand first")?,
        None => serde_json::json!({}),
    };
    let servers = root.as_object_mut().context(".mcp.json must be an object")?
        .entry("mcpServers").or_insert_with(|| serde_json::json!({}));
    let servers = servers.as_object_mut().context("mcpServers must be an object")?;
    servers.insert("lightpanda".into(), serde_json::json!({ "command": binary.to_string_lossy(), "args": ["mcp"] }));
    Ok(serde_json::to_string_pretty(&root)? + "\n")
}
```

- [ ] **Step 4: Run tests and build**
- [ ] **Step 5: Commit** `git commit -m "lightpanda: Share the panda with agents via .mcp.json"`

---

### Task 9: Namespaces, menus, clippy, acceptance

- [ ] Add `"hollow_back"` and `"lightpanda"` to the `expected_namespaces` test list in `crates/zed/src/zed.rs`.
- [ ] `./script/clippy -p hulder_net -p lightpanda -p zed`; fix all.
- [ ] `cargo test -p hulder_net -p lightpanda`.
- [ ] Manual acceptance: eye counts rise on ESLint download attempts; with lightpanda enabled, toggling the panda icon starts and stops the server (`lsof -nP -i -a -p $(pgrep -x Hulder)` shows only loopback 9222 while it runs, nothing when stopped); Open URL as Markdown works; the run target launches with both env vars visible in the terminal via `env | grep -i panda`; Share with Agents writes `.mcp.json` and `claude` in a herdr pane lists the lightpanda tool.
- [ ] Commit.
