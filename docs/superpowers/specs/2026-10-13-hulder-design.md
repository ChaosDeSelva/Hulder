# Hulder: design spec

Hulder is this fork of Zed: a free, native, offline editor with Vim keys
and a JetBrains keymap, no AI, no telemetry. The name is Old Norse *hylja*,
"to hide". Every feature below is a consequence of that name: the editor
stays hidden, and when something does leave the machine you can see it.

Already done before this spec: AI, collab, telemetry, update, and feedback
surfaces removed; `BlockedHttpClient` installed as the global HTTP client;
JetBrains keymap on by default, Vim mode off by default (toggle with
"vim: toggle vim mode" or `"vim_mode": true`); app renamed to Hulder
(`paths::APP_NAME`, binary `hulder`, bundle `io.hulder.Hulder`, menus,
release channel names). Config now lives in `~/.config/hulder`.

## 1. Hollow Back: the blocked-request eye

The huldra's back is hollow; you can see straight through. A status bar
item shows how many outbound HTTP requests were refused this session.

- New `crates/hulder_net`: `RecordingBlockedHttpClient` wraps
  `http_client::BlockedHttpClient`. On every `send` it records
  `(Instant, method, url)` into a bounded ring (1000 entries), bumps a
  counter, and emits on a gpui `Entity<NetLog>` global. It then returns
  the same `PermissionDenied` error.
- `main.rs` installs it instead of `BlockedHttpClient`.
- Status bar item `HollowBack` (right side, next to the Vim mode
  indicator): eye icon plus count. Tooltip: "N requests refused". Click
  opens a modal listing time, method, host, path, newest first.
- Lightpanda page loads (section 6) are recorded in the same log under a
  separate kind, `Browsed`, so the eye shows both "refused" and
  "browsed via panda" counts.
- Setting: none. It is always on; that is the point.

## 2. Tail toggle: show invisibles

One key reveals what the buffer hides.

- Action `hulder::ToggleTail` on `cmd-alt-i` (JetBrains and Vim layers).
- Toggles, for the active editor only, a `tail_visible: bool` on
  `Editor`. When set, the editor renders as if
  `show_whitespaces: "all"` and additionally highlights: zero-width
  characters (U+200B..U+200F, U+FEFF), bidi controls (U+202A..U+202E,
  U+2066..U+2069), non-breaking spaces, and Unicode confusables of ASCII
  letters. Highlight uses the existing `invisible` highlight style that
  `editor` already applies to control characters.
- Line endings and BOM already show in the status bar via
  `line_ending_selector` and `encoding_selector`; the toggle also forces
  the end-of-line glyph on each line (`¬`) while active.
- Implementation point: `crates/editor/src/element.rs` whitespace layout
  takes the setting from `EditorSettings`; add an `Editor::tail_visible`
  override consulted at that one site. Confusable detection ports the
  table from the removed `agent_ui/src/unicode_confusables.rs` into
  `crates/editor` as `invisibles.rs`.

## 3. Skog theme pair

Spruce and birch. Dark `Skog Natt` is the default, light `Skog Dag`.

- Two theme files under `assets/themes/skog/`, in Zed's theme JSON schema.
  Palette: background #161A17 / #F3F1EA, foreground #D8DCD2 / #2B2F2B,
  accent moss #7FA07A, bark #8C6A4A, berry #C45C5C for errors, lake blue
  #5E8FA8 for links and keywords, lichen #B9C48A for strings.
- `assets/settings/default.json`: `"theme": { "mode": "system",
  "light": "Skog Dag", "dark": "Skog Natt" }`.
- No other theme changes. Existing themes stay selectable.

## 4. Hidden folk startup

- `main.rs`: the `FIRST_OPEN` check that calls `show_onboarding_view`
  is removed; instead the first-open key is written and the app restores
  the last session or opens an empty buffer.
- `onboarding::ShowWelcome` stays reachable from Help for anyone who wants
  it. `show_onboarding_banner` is already off.
- Remove "What's New" and release-notes actions from menus if any remain.

## 5. Herdr: the herd

herdr is a terminal workspace manager for agent panes with a JSON socket
API at `~/.config/herdr/herdr.sock` (protocol 16, methods such as
`workspace.list`, `worktree.list`, `agent.list`, `agent.send`,
`agent.focus`, `pane.read`, `notification.show`, `events.subscribe`,
`plugin.*`). Hulder becomes a first-class GUI client of that socket.

### 5.1 `crates/herdr` client

- Connects lazily when the socket exists; otherwise the panel shows
  "herdr not running" with a Start button that spawns `herdr server` in
  a background process.
- Framing: discover from `herdr api schema --json` at build time and
  vendor the request and event types as Rust structs in
  `crates/herdr/src/protocol.rs` (generated once, committed). Wire
  format is newline-delimited JSON with `id` correlation; the client
  log shows handshake `encoding=SemanticFrame`, so the first step of
  implementation is to capture one real exchange with `socat` and match
  it exactly.
- `HerdrClient` entity: request/response with ids, `events.subscribe` on
  connect, reconnect with backoff on EOF, no polling.
- `HerdState` entity: workspaces, worktrees, tabs, panes, agents with
  status (`idle`, `working`, `blocked`, `unknown`), updated from events.
- Local Unix socket only. Nothing here touches the network.

### 5.2 Herd panel

Replaces the removed agents sidebar. A dock panel (left, like the project
panel) listing:

```
workspace label
  worktree (branch)          ← click: open as project
    agent name  ● working    ← click: focus in herdr; double click: attach
    agent name  ○ idle
    agent name  ◆ blocked
```

- Actions: `herd::ToggleFocus`, `herd::OpenWorktree`, `herd::FocusAgent`,
  `herd::AttachAgent`, `herd::SendSelection`, `herd::SendFilePath`.
- Keymap: `cmd-shift-h` toggles the panel.

### 5.3 Attach in the terminal

`herd::AttachAgent` opens a new terminal in `TerminalPanel` running
`herdr agent attach <terminal_id>`. The terminal tab is titled with the
agent name and keeps a `HerdrAttachment` tag so status changes update the
tab icon.

### 5.4 Notifications

On `pane_agent_status_changed` to `blocked`: workspace toast "<agent>
needs you" with actions Focus and Attach. Optional sound from herdr's own
`[ui.sound]` setting is not duplicated; Hulder stays silent unless
`herd.sound: true`. Status changes to `idle` after `working` get a quieter
toast that auto-dismisses.

### 5.5 Send to agent

`herd::SendSelection` sends the selected text via `agent.send` to an
agent chosen in a picker (last used remembered per project).
`herd::SendFilePath` sends `@<relative path>` plus line range. These are
the only ways Hulder talks to an agent: the user initiates every one.

### 5.6 Reverse direction

- `hulder` CLI: the existing `cli` crate renamed on the binary side so
  `hulder <path>` and `hulder <path>:<line>` open in the running app
  (same IPC as Zed's `zed` CLI). Install via "Install CLI" as today.
- herdr config: document setting `open_worktree` and notification
  targets to `hulder`.
- Plugin registration: on connect, Hulder calls `plugin.link` with a
  manifest naming itself and the actions `open_path` and `focus`, so
  herdr can route "open in editor" and focus requests to Hulder. Exact
  manifest shape is read from `server.agent_manifests` and the plugin
  schema during implementation; if the plugin API turns out to require a
  pane-owning plugin, this item degrades to CLI-only and the spec notes
  it.

## 6. Lightpanda: sending the panda out

Lightpanda is a headless browser (Zig, CDP, no pixels, AGPL), installed
at `~/.local/bin/lightpanda` with `fetch`, `serve`, and `mcp` modes. It
is the only way Hulder touches the web, always on explicit action, always
logged in the Hollow Back.

### 6.1 Panda runner

- `crates/lightpanda`: `PandaRunner` entity that spawns
  `lightpanda serve --host 127.0.0.1 --port <p> --log-format json` and
  tails its stdout; each page load line becomes a `Browsed` entry in the
  Hollow Back log.
- Settings: `"lightpanda": { "enabled": false, "binary": null,
  "port": 9222, "obey_robots": true }`. When disabled, every action in
  this section is hidden from menus and the command palette.
- Status bar: panda icon next to the eye, grey when stopped, green when
  serving. Click toggles.

### 6.2 Reader buffers

- Action `panda::OpenUrl` prompts for a URL, runs
  `lightpanda fetch --dump markdown --strip-mode js,css <url>`, and opens
  the output in a read-only Markdown buffer titled by the page title.
  `panda::OpenUrlSemantic` does the same with `semantic_tree_text`.
- Links in the buffer open through the same action, so you can follow a
  docs site without a browser.
- Each fetch is one `Browsed` log entry.

### 6.3 Run target

- A built-in task template `Run tests against Lightpanda` in the tasks
  list: ensures the runner is up, then runs the project's test command
  with `LIGHTPANDA_WS=ws://127.0.0.1:<port>` and
  `PLAYWRIGHT_CDP_ENDPOINT` set. The command is taken from
  `package.json` scripts `test:e2e` or `test`, else prompted.

### 6.4 MCP handoff to herdr agents

- Action `panda::ShareWithAgents` writes an MCP server entry
  `{"lightpanda": {"command": "<binary>", "args": ["mcp"]}}` into the
  project's `.mcp.json` (Claude Code's project MCP config) and, for other
  agents listed by `herdr agent list`, into the config path herdr's
  integration status reports. It never edits a file it did not create
  without showing a diff first.
- Agents then browse through their own panda subprocess; the Hollow Back
  only sees Hulder's own runner, so the modal notes "agent browsing is
  not logged here".

## Non-goals

- No embedded chat, completion, or inline AI of any kind.
- No sync, accounts, or collab.
- No rendering of web pages as pixels; reader buffers are text.
- No automatic network access: the only sockets are the herdr Unix
  socket and Lightpanda's loopback CDP port, and the latter only when the
  runner is on.

## Testing

- `hulder_net`: unit test that a `send` increments the counter, records
  the url, and still returns `PermissionDenied`.
- `editor` tail toggle: unit test over a buffer with NBSP, ZWSP, and a
  Cyrillic "а" asserting the three highlight ranges.
- `herdr`: protocol round-trip tests against a recorded fixture of real
  socket traffic; a fake server for reconnect and event fan-out.
- `lightpanda`: runner spawns a stub script that prints two JSON lines
  and asserts two `Browsed` entries.
- Manual: socket capture of the running app shows only the herdr socket
  and, when enabled, loopback 9222.

## Order

1. Hidden folk startup, Skog theme pair, Hollow Back (one PR each, small).
2. Tail toggle.
3. Herdr client and panel, then attach, notifications, send.
4. `hulder` CLI and plugin registration.
5. Lightpanda runner and reader, then run target, then MCP handoff.

## 7. The window in the woods: browser and element picker

Like Cursor's browser pane: look at a page, click an element, and what
you clicked lands in an agent's input. Two phases.

### 7.1 Phase one: drive real Chrome over CDP

- `crates/cdp`: a Chrome DevTools Protocol client over a loopback
  WebSocket (`async-tungstenite`, already a workspace dependency).
  Request/response by `id`, event stream, flat session attach
  (`Target.attachToTarget { flatten: true }`). Shared later by Lightpanda.
- `crates/browser`: `ChromeRunner` launches the user's Chrome
  (`/Applications/Google Chrome.app`, Edge as fallback, path overridable)
  with `--remote-debugging-port=0 --user-data-dir=<hulder data>/chrome
  --no-first-run --new-window <url>`. Port discovery reads
  `<user-data-dir>/DevToolsActivePort`, never HTTP, so the blocked HTTP
  client stays untouched.
- Actions: `browser::Open` (URL prompt), `browser::PickElement`,
  `browser::SendPickToAgent`, `browser::OpenPickInBuffer`.
- Pick flow: `Overlay.setInspectMode { mode: "searchForNode" }`, wait for
  `Overlay.inspectNodeRequested { backendNodeId }`, then
  `DOM.pushNodesByBackendIdsToFrontend`, `DOM.getOuterHTML`,
  `DOM.describeNode`, `CSS.getComputedStyleForNode`, `DOM.getBoxModel`,
  `Page.captureScreenshot { clip }`. Result is a `Pick { url, selector,
  outer_html, computed: Vec<(name, value)>, screenshot_png: Vec<u8> }`.
- Delivery: a Markdown buffer titled by the selector, or `agent.send` to a
  herdr agent with the Markdown form (URL, selector, fenced HTML, the ten
  most relevant computed styles). Screenshot is saved under
  `<hulder data>/picks/<timestamp>.png` and referenced by path.
- Status: the Hollow Back modal lists "Chrome session open" with the
  profile path; page loads are not logged (that is the user's browser).
- Settings: `"browser": { "binary": null, "profile": null }`.
- Risk: Chrome's remote debugging port is loopback-only but unauthenticated,
  so while the Hulder-launched Chrome is open any local process can connect
  and drive it. Mitigations: a dedicated 0700 profile directory under
  `<hulder data>/chrome` (never the user's real profile; symlinks and
  foreign-uid directories are refused), `--disable-extensions
  --disable-sync --no-service-autorun --password-store=basic` so the
  profile holds nothing valuable, URLs validated and passed after a `--`
  switch terminator, and the Chrome process killed on Hulder quit. Residual
  risk: do not log into anything sensitive in that window.

### 7.2 Phase two: embedded WebKit pane

- A `WebView` gpui element backed by a native `WKWebView` added as a
  subview of the window's content view in `gpui_macos`, positioned at the
  element's bounds each frame and hidden when the element is not painted.
- Picker is an injected script (hover outline, click to report
  `outerHTML`, a generated selector, and `getComputedStyle`), bridged via
  `WKScriptMessageHandler` to the same `Pick` type.
- Known limits accepted up front: the native view paints above gpui, so
  modals and popovers must avoid its rectangle; no DevTools beyond the
  injected picker; WebKit rather than Chrome.
- Lives behind `"browser": { "embedded": false }` until it is stable.

## 8. The Grove: a project tree with context

Every editor lists files. Hulder's tree should tell you what is going on
in them. Built on Zed's `project_panel` (keep it; extend it), not a
rewrite.

### 8.1 Filter bar
- A filter field pinned at the top of the tree (`/` focuses it, Esc
  clears). Typing flattens the tree into matching paths, fuzzy, with the
  match highlighted; Enter opens, arrow keys move.
- Filter syntax, all optional prefixes that compose:
  `ext:ts` by extension, `git:modified|added|untracked|conflict`,
  `@<agent>` files under a herdr agent's cwd, `since:2h` modified
  recently, `size:>1m`, `diag:error|warning`.

### 8.2 Rows that carry context
- Beside the name, quiet badges: git status (already there), diagnostic
  counts, and a ram when a herdr agent's cwd is this folder or below.
- Hover or `space` shows a peek popover: first 40 lines for text, the
  image for images, size and modified age for everything.
- Folders show file count and total size on expand (computed in the
  background, cached per worktree snapshot).

### 8.3 Groups above the tree
- "Open in terminals": every center terminal's cwd and every attached
  herdr agent, as jump targets.
- "Recent": the last ten edited files in this project.
- "Pinned": folders and files you pin with `p`; persisted per project.

### 8.4 Actions that reach the terminal
- `t` on a folder: open a center terminal there. `T`: open the herdr TUI
  with a new workspace created at that folder (`workspace.create --cwd`).
- Right-click: Open in Terminal, Copy relative path, Copy absolute path,
  Reveal in Finder (exists), Send path to agent (herdr).

### 8.5 Non-goals
- No file previews that render HTML or run code.
- No cloud or indexing service; everything comes from the worktree
  snapshot, git, LSP diagnostics, and herdr's socket.

## 9. Run and debug without downloads

Zed's debugger fetches its adapters from GitHub (CodeLLDB as a VSIX,
vscode-js-debug, the delve shim) or PyPI (debugpy). Hulder blocks every
outbound request, so those paths can never succeed. Instead Hulder runs
and debugs with the tools already on the machine.

### 9.1 What ships

- **`LLDB` debug adapter** (`crates/dap_adapters/src/lldb_dap.rs`): a
  stdio DAP adapter over the `lldb-dap` binary. Binary resolution order,
  first hit wins, nothing downloaded:
  1. `dap.LLDB.binary` in settings;
  2. `lldb-dap` on the shell `PATH`;
  3. `xcrun --find lldb-dap`;
  4. `/Applications/Xcode.app/Contents/Developer/usr/bin/lldb-dap`, then
     `/Library/Developer/CommandLineTools/usr/bin/lldb-dap`.
  Launch requests carry `program`, `args`, `cwd` (defaults to the
  worktree root), `env`, and `stopOnEntry`; attach requests carry `pid`.
  `env` objects are rewritten to `["KEY=VALUE"]` because the Xcode 16
  `lldb-dap` (lldb-1600) only accepts that form. `LLDB` is listed first
  for C, C++, and Rust, so "New debug session" preselects it.
- **Download refusal**: CodeLLDB, JavaScript, Delve, and Debugpy use a
  user-set binary or a copy already in the adapter cache dir. Otherwise
  they print, in the debug console, exactly
  `Hulder does not download debug adapters; install <X> and set
  dap.<Name>.binary` and fail before any request is made. The helper is
  `dap::adapters::refuse_download`. `go install` for `dlv` and `pip
  download` for debugpy are refused the same way.
- **Templates**: `assets/settings/initial_tasks.json` ships `cargo run`,
  `cargo test`, `cargo build`, "C++: compile and run active file"
  (`clang++ -g -O0 $ZED_FILE -o $ZED_WORKTREE_ROOT/.hulder/bin/$ZED_STEM`
  then run it), and "Run active file (shebang)". Zed's task layer joins
  `command` and `args` into one raw line for the user's shell
  (`prepare_task_for_spawn` uses `build_no_quote`), so these templates
  put no path on that line: paths go in the task's `env` as
  `HULDER_BIN`, `HULDER_FILE`, `HULDER_STEM` and `HULDER_CC` (Zed
  substitutes `$ZED_*` in `env` values and hands them to the process
  unparsed), and the `sh -c` script, passed single-quoted as one arg,
  expands them only inside double quotes. Hulder-authored task and
  debug templates follow the same rule.
  `assets/settings/initial_debug_tasks.json` ships "Debug Rust binary"
  (adapter `LLDB`, `build` = `cargo build`, program
  `$ZED_WORKTREE_ROOT/target/debug/<crate>`) and "Debug active C++ file"
  (adapter `LLDB`, `build` = the clang++ line above).
- **Gutter runnables**: Rust runnables keep coming from rust-analyzer,
  which Hulder finds via `rustup which rust-analyzer` and then `PATH`
  before any download path. C and C++ files with `int main(` get a play
  button (the existing `cpp-main` / `c-main` tags in
  `crates/grammars/src/{cpp,c}/runnables.scm`) wired to the compile-and-
  run task by `languages::cpp::CFamilyContextProvider`.
- **Run menu**: Run Task…, Rerun Last Task, Start Debugging…, Debug
  Active File. `debugger::DebugActiveFile` compiles a C or C++ buffer
  with clang into `.hulder/bin/<stem>` and starts an `LLDB` session; for
  any other language it opens the new-session modal.

### 9.2 What the user installs

- **C and C++ on a Mac with Xcode or the Command Line Tools**: nothing.
  `clang`, `clang++`, and `lldb-dap` come with `xcode-select --install`.
- **Rust**: a `rustup` toolchain. `cargo` for the tasks and the
  toolchain's `rust-analyzer` component for gutter runnables. Debugging
  uses the same `lldb-dap`; Rust pretty-printers are not loaded
  (`rust-lldb`'s formatters can be added through `initCommands`).
- **Linux**: an `lldb-dap` from the distro's LLVM package, found on
  `PATH` or set with `dap.LLDB.binary`.
- **Python, JavaScript, Go**: install the adapter by hand and set
  `dap.<Name>.binary`; the console message names what to fetch.

### 9.3 Non-goals

- No bundled debug adapters, no vendored CodeLLDB, no fetch-on-demand of
  any kind.
- No build-system detection beyond `cargo` and single-file clang; CMake
  and friends are a `build` step the user writes in `debug.json`.

## 10. Documents for agent work

Cheap, offline, native. Nothing here renders HTML.

- **PDF reader tab** (`crates/pdf_viewer`, modeled on `image_viewer`):
  pages rasterized with CoreGraphics (`CGPDFDocument`) at the current zoom,
  text extracted with PDFKit (`PDFDocument.string` per page) for Cmd+C,
  "Copy Page Text", "Copy Document Text", and "Open as Markdown" which
  opens a read-only buffer with one heading per page. macOS only; other
  platforms open PDFs as binary like today.
- **Markdown links**: relative links in the preview open the target file in
  Hulder (not the browser); `[[wiki links]]` resolve to `<name>.md` in the
  same folder or worktree root; external links still go to the browser on
  explicit click.
- **Send selection to agent**: `herd::SendSelection` sends the editor
  selection (or the Markdown preview selection) as typed input to the
  agent currently shown in the Herd view's terminal, via `agent.send`.
  Nothing is sent unless the user triggers it.
- **Mermaid export**: right-click a rendered diagram in Markdown preview:
  "Save Diagram as SVG…" and "Copy Diagram SVG".

## 11. The Saga: notes, tickets and todos per project

Naming: the panel is the **Saga**; tickets are **Tales** (þættir), todos
are **Verses**, notes are **Lore**, and a tale's `## Log` is its
**Chronicle**. Action namespace `saga`.

Plain Markdown in `.hulder/` so agents read it as context with no
integration and git can carry it. No database.

### 11.1 Layout (created on first use)
```
.hulder/
  NOTES.md             free-form project notes
  TODO.md              checkbox list for quick capture
  tickets/NNNN-slug.md one file per ticket
  AGENTS.md            tells agents what lives here and how to log progress
```
Ticket front matter: `id`, `title`, `status` (open|doing|blocked|done),
`priority` (low|normal|high), `tags`, `created`, `updated`; body is free
Markdown with a `## Chronicle` section the user or an agent appends to.
`.hulder/bin/` (build output) is always gitignored by Hulder.

### 11.2 First use
When the first note or ticket is created, a one-time prompt per project
asks "Keep `.hulder/` in git, or local only?" Local only appends
`.hulder/` (except nothing else) to `.git/info/exclude`; the choice is
stored in `.hulder/saga.json` (`"tracked": true|false`).

### 11.3 Saga panel (dock, right side)
- Tickets grouped by status with counts; keys: `n` new ticket (title
  prompt, slug and id generated), `s` cycle status, `p` cycle priority,
  Enter opens the file, `d` marks done.
- Todos: `TODO.md` checkboxes listed and toggled in place; `t` adds one.
- Notes: files under `.hulder/` and `.hulder/notes/`; `N` creates
  `notes/YYYY-MM-DD.md`.
- Everything is a file edit; the panel watches the folder and re-renders.

### 11.4 Agent handoff (terminal-first)
- `saga::SendToAgent`: sends the ticket path plus body (or the note) as
  typed input to the agent shown in the Herd terminal via `agent.send`.
- `saga::StartAgentOnTale`: creates a herdr workspace at the project
  (`workspace.create --cwd`), sends the ticket as the opening prompt, sets
  status to `doing`, and appends a Log line with the agent name.
- Generated `AGENTS.md` names the files and asks agents to append to a
  ticket's `## Log` instead of editing front matter.

### 11.5 Non-goals
No sync, no cloud, no AI inside the panel, no custom file format.

## 12. Bifröst: the bridge between the terminal agent and the editor

Agents stay in the terminal. Bifröst is how they reach the editor's
knowledge without the editor hosting any AI.

### 12.1 Shape
- Hulder listens on a local Unix socket `~/.config/hulder/hulder.sock`
  (newline JSON, `{"id","method","params"}`, same style as herdr), served
  by `crates/bifrost` inside the app. Loopback only, same user only.
- `hulder mcp` (a subcommand of the `cli` crate) is a stdio MCP server
  that agents spawn; it relays each tool call to the socket and returns
  the reply. No network. If Hulder is not running it answers every tool
  with a clear error.
- Installed per project with the same `.mcp.json` writer Lightpanda uses:
  `{"hulder": {"command": "/usr/local/bin/hulder", "args": ["mcp"]}}`.
- As built: the socket is `paths::config_dir()/hulder.sock`, which is
  `~/.config/hulder/hulder.sock` on macOS (or `<user-data-dir>/config/hulder.sock`
  with `--user-data-dir`); it is bound with mode 0600, a stale file is replaced
  and a symlink at that path refuses to bind. The CLI invocation is
  `hulder mcp` (the `cli` binary with the `mcp` subcommand); the Go menu's
  "Share Editor with Agents (Bifröst)" writes it into `.mcp.json`, pointing at
  `/usr/local/bin/hulder` when installed, else the `cli` next to the app, else
  the running executable.

### 12.2 Tools (v1)
Saga: `saga_list_tales(status?)`, `saga_get_tale(id)`, `saga_create_tale
(title, body?, priority?, tags?)`, `saga_set_status(id, status)`,
`saga_append_chronicle(id, text)`, `saga_list_verses()`,
`saga_toggle_verse(line)`, `saga_add_verse(text)`, `saga_list_lore()`,
`saga_read_lore(name)`, `saga_search(query)`.
Editor: `editor_current_file()` (path, language, cursor line),
`editor_selection()` (text, path, range), `editor_open(path, line?)`,
`editor_diagnostics(path?)`, `editor_open_files()`, `project_root()`.
Every write tool is a file edit under `.hulder/` and shows in the Saga
panel at once.

### 12.3 Guardrails
- Tools never read files outside the project root or `.hulder/`.
- No tool runs commands or touches the terminal; herdr owns that.
- The Hollow Back modal lists Bifröst connections (agent pid, tool
  count) so you can see what asked.

## 13. Clippings: web and document captures as files

Every capture becomes a file under `.hulder/clippings/`, a lore type in
the Saga, so agents can read it and the user can annotate it.
- Element picks (section 7), Lightpanda fetches (6.2) and "Open as
  Markdown" from the PDF reader (10) write `clippings/YYYY-MM-DD-<slug>.md`
  with front matter `source` (url or path), `captured`, `kind`
  (pick|fetch|pdf), `selector` when relevant, and the Markdown body;
  screenshots go to `clippings/assets/`.
- The buffer that opens is the saved file, not a scratch buffer.
- The Saga panel lists Clippings; `saga::SendToAgent` works on them; a
  Bifröst tool `saga_list_clippings()` and `saga_read_clipping(name)`.
- A setting `"saga": { "save_clippings": true }` turns it off for people
  who want scratch buffers only.

## 14. Glosses: notes on files and lines

Marginalia for code. A gloss attaches a note to a file or to a line range.
- Stored in `.hulder/glosses/<relative path>.md`: one section per gloss
  with front-matter-like header lines `line: 42-48`, `anchor: <trimmed
  text of the first anchored line>`, `created`, followed by Markdown.
  File-level glosses have no `line:`.
- Anchoring: when the file changes, a gloss re-anchors by searching ±200
  lines for its anchor text; if not found it is shown as "detached" in the
  gutter and the Saga, never deleted.
- UI: gutter mark on glossed lines (click to open the gloss in a popover
  with an editor; `Cmd+Ctrl+G` adds a gloss at the cursor or selection);
  the Grove hover popover shows file-level glosses and a count of line
  glosses; the Saga lists glosses per file.
- Agents: `saga_glosses(path?)` returns glosses with line ranges and
  anchor text; `saga_add_gloss(path, line_range?, text)` lets an agent
  leave a note for you (shown with the agent's name).

## 15. The Kit: what the agents will use, under the tree

A tabbed strip docked at the bottom of the Grove, collapsible, default
one third of the panel height.
- **Skills**: `.claude/skills/*/SKILL.md` and `skills/*/SKILL.md`
  (name, description, trigger line from front matter); click opens;
  right-click "Send name to agent".
- **Agents**: `.claude/agents/*.md` subagent definitions (name, model,
  tools) plus the live herdr agents whose cwd is inside this project,
  with status; click opens the file or attaches in the Herd.
- **MCP**: servers from the project `.mcp.json` and user-level config,
  with "running" state when the command is a live child, plus Bifröst and
  the panda with one-click add via the existing writer.
- **Rules**: `CLAUDE.md`, `AGENTS.md`, `.rules`, `.claude/settings*.json`
  hooks and `.claude/commands/*` found in the project, as a checklist of
  what an agent will read, with missing-file hints ("no AGENTS.md; create
  from the Saga template").
- All four are file listings from the worktree snapshot, refreshed on
  worktree events; no parsing beyond front matter.

## 16. The Veil: away mode

A privacy screen, not encryption. The huldra hides in plain sight.

- Status bar button at the far left (`IconName::Veil`, a hooded figure
  or an eye with a line). First use: a passkey prompt (any length,
  minimum 1 char); stored as an Argon2id or scrypt hash with a random
  salt in `~/.config/hulder/veil.json`; the key itself is never stored.
- `veil::Toggle` (`cmd-ctrl-shift-l`) veils the whole app. While veiled:
  - every text run drawn by gpui is transliterated to Elder Futhark
    (`a→ᚨ b→ᛒ c→ᚲ d→ᛞ e→ᛖ f→ᚠ g→ᚷ h→ᚺ i→ᛁ j→ᛃ k→ᚲ l→ᛚ m→ᛗ n→ᚾ o→ᛟ
    p→ᛈ q→ᚲ r→ᚱ s→ᛊ t→ᛏ u→ᚢ v→ᚹ w→ᚹ x→ᚲᛊ y→ᛇ z→ᛉ`, digits to `ᛜ ᛝ ᛞ ᚦ
    ᛃ ᛈ ᛖ ᛒ ᛟ ᚨ` by index, punctuation kept, case folded), applied at the
    single choke point where the window text system shapes lines, so
    editor, terminal, panels, tabs, title and status bar are all covered;
  - the editor remains visible exactly as it was (open file, tabs, tree,
    terminal, status bar, everything in place) with every text run in
    runes; a fully transparent full-window overlay draws nothing of its
    own and swallows every mouse event, keystroke and action. Every
    window of the app is covered: workspace windows get the overlay,
    any other window (settings, About) is shielded by gpui itself, and
    every native window title reads `Hulder` until unveiled;
  - the only live control is the hood icon at its usual bottom-left
    status-bar spot: a click there, or `veil::Toggle`, opens a small
    unlock popover anchored above the icon with a masked passkey field
    (its placeholder in runes like everything else). Enter unlocks,
    Escape closes the popover and stays locked, a wrong key shakes the
    field, and after three wrong keys each attempt waits 10 s, doubling
    to five minutes; the lockout message shows in the popover. Clicking
    anywhere else does nothing;
  - a prompt that opens while veiled is cancelled; app-menu actions are
    dropped even when no window is active.
- Not veiled: images, SVG previews, PDF pages, the Chrome window.
  Documented in the button's tooltip.
- Unveil restores everything; no state is touched. The lock itself is
  persisted, so a relaunch comes back veiled. A `veil.json` that exists
  but cannot be read counts as locked with a passkey present and the
  popover says so. The veil also engages automatically after
  `veil.auto_after_minutes` idle when set.
- Passkeys are typed twice when set, and a passkey is refused if any of
  its characters could not be typed into the unlock field while veiled.
- Implementation: a `gpui::TextVeilState` global (set once by the veil,
  no window-level setter) consulted at the start of every window draw
  and in `WindowTextSystem::shape_line` / `layout_line`, plus
  keystroke and action interceptors, a `crates/veil` with the
  transliteration (pure, tested), the hash store, the status item, the
  overlay and popover views, and the actions.

## 17. Worktrees in the Grove

Agents make git worktrees and people lose them. The repo already knows
about every one (`.git/worktrees/<name>/`), so no session context is
needed.

- A "Worktrees" group in the Grove, under Pinned and Recent, listing every
  worktree of the project's root repository except the main checkout.
  Source: `git worktree list --porcelain` run for the root worktree,
  parsed into `{path, head, branch, locked: Option<String>, prunable}`.
  Re-run when `.git/worktrees` changes (one watcher via the project's
  `Fs::watch`, debounced 500 ms) and on panel focus; never polled.
- Row: branch (or detached short SHA), short SHA, age of HEAD commit,
  `dirty`/`clean` (from `git status --porcelain` run lazily when the row
  is first visible, cached 30 s), a lock badge with the lock reason as
  tooltip. Paths under `.claude/worktrees/` get an "agent" tag with the id
  trimmed to 7 chars; if herdr's `worktree.list` knows the path, the
  agent's herdr name and status replace the raw id.
- Actions: Enter/click adds the worktree as a folder to the current
  workspace (`workspace::AddFolderToProject` semantics, no new window);
  `t` opens a terminal there (same binding as folders); `Reveal in
  Finder`; `Prune stale` runs `git worktree prune` and then deletes
  worktrees that are unlocked, clean, and whose branch is merged into the
  main checkout's HEAD, after a confirm dialog that lists them.
- Everything runs git as argv with `cwd` set, never through a shell, and
  only inside the repository root; paths from porcelain output are
  canonicalized and must stay under the repository's common dir parent or
  under the user's home before any delete.
- Tests: porcelain parsing (locked, prunable, detached), agent-path
  detection, the prune filter (unlocked ∧ clean ∧ merged), and a FakeFs
  test that the group re-renders when `.git/worktrees` changes.

## 18. Raven and the Hoard: a small API client with borrowed logins

Odin's ravens fetch news and bring it back. The dragon's hoard is where
the keys are kept. Together they give Hulder a lightweight Postman and a
single place for auth that the Panda (6) and the element picker (7) can
use too.

### 18.1 Contract with the privacy model

The global HTTP client stays blocked. Raven owns a separate
`reqwest_client` instance created when the user presses Send and dropped
afterwards. Every request Raven sends is a `NetEntryKind::Sent` in Hollow
Back with method, host, status and bytes. Nothing is sent on open, on
save, on typing, or on a timer. The README privacy model gains one line.

### 18.2 Raven: requests as files

- Requests live in `.hulder/ravens/<collection>.http`, the plain-text
  REST Client format (`### name`, `METHOD url`, headers, blank line,
  body; `{{var}}` placeholders). Agents can read and write them; the user
  edits them as text. A collection is one file; the Grove lists the
  directory like any other.
- `raven::Toggle` (`cmd-ctrl-shift-r`) opens the Raven as a center Item:
  left, the request list grouped by collection with a filter; right, the
  request (method dropdown, URL field, headers and body as a small
  editor) and below it the response: status, time, size, headers, body
  pretty-printed into a read-only buffer with JSON/XML/HTML language
  set from Content-Type. Cmd-Enter sends. A response over 8 MiB is
  truncated with a notice; a request waits at most 60 s.
- Environments: `.hulder/ravens/env.json` holds non-secret variables per
  environment (`{"local": {"base": "http://localhost:3000"}}`); the
  active environment is a dropdown. A variable whose value is `$hoard:
  <profile>` resolves from the Hoard (18.3) at send time and is never
  written to disk resolved.
- Log: each send appends one JSON line to `.hulder/ravens/log.jsonl`
  (request name, method, url, status, duration, sizes, timestamp),
  capped at 2 MiB by rotating to `log.1.jsonl`. Bodies are not logged.
- Snippets for agents: "Save as Clipping" writes
  `.hulder/clippings/YYYY-MM-DD-<slug>.md` with kind `raven`, the request
  as an `http` fenced block, the response headers and body (body capped
  at 256 KiB), with `Authorization`, `Cookie`, `Set-Cookie`,
  `X-Api-Key` and any header named in `raven.redact_headers` replaced by
  `<redacted>`. Section 13's Saga listing and Bifröst clipping tools
  apply unchanged.
- Bifröst gains `raven_list()` (names and methods) and
  `raven_last_response(name)` (the redacted last response from the log
  and a per-request cache in memory). Agents never trigger a send.
- Import: "Import cURL" from the clipboard parses a `curl` command line
  (method, URL, `-H`, `-d/--data`, `-u`) into a new request.

### 18.3 The Hoard: one auth store for Raven, the Panda and the picker

- A profile is `{name, kind, hosts: [String], created}` in
  `~/.config/hulder/hoard/profiles.json`; the secret material lives in
  the macOS Keychain under service `Hulder Hoard` and account
  `<profile>`, through the existing `credentials_provider` crate (local
  Keychain only; the Zed cloud provider is not used). Kinds: `bearer`
  (token), `basic` (user:password), `header` (name and value), `cookies`
  (a cookie jar as JSON `[{name, value, domain, path, secure, httpOnly,
  expires}]`).
- Hosts match by exact host or `*.example.com` suffix; a profile is only
  applied to a matching host, and only over `https` unless the host is
  `localhost`, `127.0.0.1` or `::1`.
- `hoard::Toggle` opens a modal that lists profiles, adds one by hand,
  deletes one, and offers **Borrow from Chrome**: Hulder launches the
  same hardened Chrome profile the picker uses (7.2), the user logs in
  normally, and on "Borrow" Hulder calls `Storage.getCookies` over CDP,
  keeps the cookies for the hosts the user ticks, stores them as a
  `cookies` profile, and clears them from that Chrome profile
  (`Network.clearBrowserCookies`) so the throwaway profile stays empty.
  Expired cookies are dropped on load.
- Raven applies a matching profile at send time: bearer/basic/header as
  the header, cookies as a `Cookie` header. The response's `Set-Cookie`
  updates the jar of a `cookies` profile when the user has ticked "keep
  session cookies" on that profile.
- The Panda applies a matching `cookies` profile to `lightpanda fetch`:
  if the installed Lightpanda accepts a cookie file or `--header`
  (probe `lightpanda fetch --help` once per binary, cache by mtime), pass
  them; otherwise start `lightpanda serve` on loopback, connect with
  `crates/cdp`, `Network.setCookies`, `Page.navigate`, wait for load,
  dump the document and close. Either way the URL and host go to Hollow
  Back as `Browsed` with the note "with Hoard profile <name>".
- The picker (7) gets the same cookies injected into its Chrome session
  before navigating when a profile matches, so a picked element can come
  from a logged-in page.
- Secrets never reach: logs, clippings, Bifröst, the Saga, toasts, or
  Hollow Back entries. A test asserts the redaction set covers every
  header a profile can produce. The Veil (16) hides the Hoard modal like
  any other text.
- Profiles are per user, not per project; `env.json` references them by
  name so a project file can be committed without secrets.
