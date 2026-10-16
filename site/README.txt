> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

<p align="center">
  <img src="assets/hulder-banner.png" width="100%" alt="Hulder — a moonlit Norse forest and carved code editor" />
</p>

<h1 align="center">Hulder</h1>

<p align="center">
  A lightweight, offline code editor for people who run their AI agents in a terminal.<br/>
  Built on the <a href="https://github.com/zed-industries/zed">Zed</a> engine, with the parts that phone home removed.
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="#what-it-is">What it is</a> ·
  <a href="#features">Features</a> ·
  <a href="#keys">Keys</a> ·
  <a href="#building">Building</a> ·
  <a href="#license">License</a>
</p>

---

## What it is

Hulder is a fork of Zed with one rule: **nothing leaves the machine unless you ask.**
No account, no telemetry, no auto update, no AI inside the editor, no collaboration server.
The global HTTP client refuses every request, and a status bar eye shows you each one it refused.

What it keeps is the fast native engine: GPU rendering, Tree sitter, language servers,
a real terminal, Vim mode, and a JetBrains keymap on by default.

What it adds is a terminal first way to work with coding agents:

* **The Herd**: a full size view that lists the workspaces and agents in [herdr](https://github.com/jlashley/herdr),
  your terminal workspace manager, and embeds the selected agent's session as a terminal.
* **The Saga**: notes, tickets and todos per project as plain Markdown in `.hulder/`, so any agent reads them as context.
* **Bifröst**: a stdio MCP bridge (`hulder mcp`) that lets an agent in the terminal read your current file,
  selection and diagnostics, and update the Saga, without the editor hosting any model.

The name is Old Norse *hylja*, "to hide". The huldra are the hidden folk: lovely from the front, hollow at the back.

> **Status:** early. It builds, it runs, it is used daily by its author. macOS only for now.

## Install

Hulder is not yet distributed as a signed app. Build it from a local source checkout with Rust and Xcode installed:

```sh
# From the root of your Hulder source checkout:
cargo build --release -p zed
./target/release/Hulder
```

Then **Install hulder CLI** from the app menu to get the `hulder` command.

### Optional companions

| Tool | What it gives Hulder | Install |
|---|---|---|
| [herdr](https://github.com/jlashley/herdr) | The Herd view: agents and workspaces, attach in one click | `~/.local/bin/herdr` |
| [Lightpanda](https://lightpanda.io) | Headless browser for the Panda panel: fetch pages as Markdown, run Playwright offline ish | `curl -fsSL https://pkg.lightpanda.io/install.sh \| bash` |
| Google Chrome | Element picker: click anything on a page, get its HTML, styles and a screenshot as Markdown | already installed on most Macs |
| Xcode or Command Line Tools | C and C++ via clangd, and `lldb-dap` for the debugger | `xcode-select --install` |

Language servers are never downloaded. Hulder finds the ones on your `PATH` (`vtsls`, `clangd`, `rust-analyzer`, `lua-language-server`, `kotlin-language-server`, `jdtls`).

## Features

### Editor
* Zed's editor, multibuffers, project search, Tree sitter highlighting, LSP, Git gutter and panel.
* **Grove** project tree: filter bar (`/`) with `ext:ts`, `git:modified`, `since:2h`, `size:>1m`; pinned and recent groups; `t` opens a terminal at a folder; folder file counts and sizes.
* **Tail toggle** (`Cmd+Alt+I`): reveals whitespace, line endings, zero width and bidi characters, and mixed script lookalike letters.
* **Skog** theme pair, dark and light, as defaults.
* Encoding and line ending selectors, Format menu entries.
* Markdown preview with Mermaid diagrams rendered natively, SVG and image viewers, CSV tables, and a PDF reader.

### Terminal and agents
* **Herd** (`Cmd+Ctrl+H`): herdr workspaces and agents with live status; select an agent and its session attaches in an embedded terminal; a ram in the status bar counts agents waiting on you.
* Toasts when an agent blocks or finishes.
* **Saga** (`Cmd+Ctrl+Shift+T`): tales (tickets), verses (todos) and lore (notes) in `.hulder/`; send a tale to the shown agent or start an agent on it.
* **Bifröst**: `hulder mcp` exposes the Saga and editor context to any MCP capable agent.

### Web, without a web view
* **Panda** panel (`Cmd+Ctrl+Shift+P`): Lightpanda as a managed local process; fetch a URL as Markdown into a read only buffer; share it with agents via `.mcp.json`.
* **Element picker** (`Cmd+Ctrl+Shift+E`): drives your Chrome over DevTools Protocol with a hardened throwaway profile.
* **Hollow Back**: the status bar eye. Every refused outbound request, every page the panda browsed, every bridge call, every Raven send, listed.
* **Raven** (`Cmd+Ctrl+Shift+R`): a small API client. Requests are `.http` files in `.hulder/ravens/`, environments in `env.json`; Cmd Enter sends through a client built for that one call, responses show pretty printed with the language from `Content-Type`, and "Save as Clipping" writes a redacted `kind: raven` clipping. Import a cURL line from the clipboard; Bifröst can list requests and read the redacted last response but never send.
* **Hoard** (`hoard: toggle`): one auth store for Raven, the Panda and the picker. Profiles (bearer, basic, header, cookies) keep their metadata in `~/.config/hulder/hoard/profiles.json` and their secret in the macOS Keychain, match hosts exactly or by `*.suffix`, and travel over https only (except to localhost). "Borrow from Chrome" lifts a cookie jar out of the throwaway Chrome profile and clears it there.

### Run and debug, offline
* `LLDB` debug adapter using the `lldb-dap` that ships with Xcode. Nothing downloaded.
* Shipped tasks for `cargo run`, `cargo test`, and compile and run for C and C++.
* Gutter play buttons for Rust tests and C/C++ `main`.

## The Viking software factory

Use the task picker to open Heimdall, check the local services, or start the factory. Inspect plans, agents, blockers and verification in Heimdall; edit the task worktrees in Hulder. The existing Herd panel attaches to Herdr backed agents. See [factory integration](FACTORY_INTEGRATION.md) for setup and the boundaries between the IDE, dashboard and agent runtimes.

## Keys

The JetBrains keymap is the default. Vim mode is a toggle ("vim: toggle vim mode").

| Action | Key |
|---|---|
| Fuzzy file search | `Cmd+Shift+O` |
| Herd view | `Cmd+Ctrl+H` |
| Saga panel | `Cmd+Ctrl+Shift+T` |
| Panda panel | `Cmd+Ctrl+Shift+P` |
| Pick element in Chrome | `Cmd+Ctrl+Shift+E` |
| Raven API client | `Cmd+Ctrl+Shift+R` |
| Tail toggle | `Cmd+Alt+I` |
| Tree filter / pin / terminal here | `/` `p` `t` (Vim mode: `?` `P` `T`) |
| Toggle breakpoint | `Ctrl+F8` (JetBrains) or `F9` |
| Markdown preview | `Cmd+Shift+V` |

## Privacy model

* Outbound HTTP is refused at the one place it is created. Nothing in the editor can fetch.
* The one exception is the Raven: a request goes out only when you press Send, through a client that exists for that call alone, and it is listed in the Hollow Back with its host, status and byte counts. Hoard secrets live in the Keychain and never reach logs, clippings, Bifröst or the Hollow Back.
* The only sockets are local: herdr's Unix socket, Bifröst's Unix socket, and Lightpanda's loopback port while you run it.
* Chrome, when you open it from Hulder, uses a dedicated profile with extensions and sync off. While it is open, other local processes can also reach its debug port; do not log into anything sensitive in that window.
* While the Panda fetches with a Hoard profile, its loopback DevTools port is open for the duration of that fetch; other local processes could reach it. Lightpanda builds that take a cookie file use that instead, and the jar is cleared and the process killed when the fetch ends.
* Settings live in `~/.config/hulder`, data in `~/Library/Application Support/Hulder`, logs in `~/Library/Logs/Hulder`.

## Building

```sh
cargo build -p zed            # debug
cargo build --release -p zed  # release
cargo test -p herdr -p herd_panel -p hulder_net -p lightpanda -p cdp -p browser -p dap_adapters -p project_panel
./script/clippy -p herdr      # lint with the repo's settings
```

If `xcode-select` points at Command Line Tools that lack C++ headers, `.cargo/config.toml` already pins `DEVELOPER_DIR` to Xcode.app.

Design documents: `docs/superpowers/specs/` and `docs/superpowers/plans/`.

## Contributing

Issues and pull requests are welcome. Keep the rule: no network, no AI in the editor, agents in the terminal.
Every PR keeps the review marker at the top of this file until the author has read their own diff.

## License

Hulder is licensed under the GNU General Public License v3.0 or later, like the Zed editor it is built from.
GPUI, Zed's UI framework, is Apache 2.0. See `LICENSE-GPL` and `LICENSE-APACHE`.
"Zed" and the Zed logo are trademarks of Zed Industries; Hulder is not affiliated with or endorsed by Zed Industries.
