<p align="center">
  <img src="assets/hulder-banner.png" width="100%" alt="Hulder — a moonlit Norse forest and carved code editor" />
</p>

<h1 align="center">Hulder</h1>

<p align="center">
  A lightweight, offline code editor for people who run their AI agents in a terminal.<br/>
  An independently maintained development workspace with local tools and optional factory integration.
</p>

<p align="center">
  <a href="https://chaosdeselva.github.io/Hulder/guide.html">Read in dark mode</a> ·
  <a href="#install">Install</a> ·
  <a href="#what-it-is">What it is</a> ·
  <a href="#features">Features</a> ·
  <a href="#keys">Keys</a> ·
  <a href="#building">Building</a> ·
  <a href="#license">License</a>
</p>

---

## What it is

Hulder is an independently maintained IDE derived from Zed, designed for local development.
It does not require a Zed account or cloud subscription. The shared editor HTTP client refuses outbound requests, and the Hollow Back records refused requests. The separate Hulder updater checks GitHub releases when automatic checks are enabled. Explicit Raven requests and external tools have their own network behavior; see the privacy model below.

What it keeps is the fast native engine: GPU rendering, Tree-sitter, language servers,
a real terminal, Vim mode, and a JetBrains keymap on by default.

What it adds is a terminal-first way to work with coding agents:

- **The Herd**: a full-size view that lists the workspaces and agents in [herdr](https://github.com/jlashley/herdr),
  your terminal workspace manager, and embeds the selected agent's session as a terminal.
- **The Saga**: notes, tickets and todos per project as plain Markdown in `.hulder/`, so any agent reads them as context.
- **Factory**: a native read-only mission/task inspector for local Mimir, with task briefs, acceptance criteria, execution evidence and worktree opening. See [factory integration](FACTORY_INTEGRATION.md).
- **Bifröst**: a stdio MCP bridge (`hulder mcp`) that lets an agent in the terminal read your current file,
  selection and diagnostics, and update the Saga, without the editor hosting any model.

The name is Old Norse *hylja*, "to hide". The huldra are the hidden folk: lovely from the front, hollow at the back.

> **Status:** early. It builds, it runs, it is used daily by its author. macOS only for now.

## Install

Download the [signed and notarized Apple silicon app](https://github.com/ChaosDeSelva/Hulder/releases/download/v0.1.4/Hulder-0.1.4-macos-arm64.dmg). Open the disk image, drag Hulder to Applications, then open it. This is an experimental release; manual accessibility and a separate clean Mac acceptance remain pending. Language extensions and language servers are installed separately.

To build from source with Rust and Xcode installed:

```sh
# From the root of your Hulder source checkout:
cargo build --locked -p zed --bin Hulder --features util/debug-embed
./target/debug/Hulder
```

Use **Hulder > Check for Updates** for a manual check, or **Automatic Update Checks** to control launch and daily checks. Updates download a verified installer; replacement and restart happen when you choose.

Then **Install hulder CLI** from the app menu to get the `hulder` command.

### Optional companions

| Tool | What it gives Hulder | Install |
|---|---|---|
| [herdr](https://github.com/jlashley/herdr) | The Herd view: agents and workspaces, attach in one click | `~/.local/bin/herdr` |
| [Lightpanda](https://lightpanda.io) | Headless browser for the Panda panel: fetch pages as Markdown, run Playwright offline-ish | `curl -fsSL https://pkg.lightpanda.io/install.sh \| bash` |
| Google Chrome | Element picker: click anything on a page, get its HTML, styles and a screenshot as Markdown | already installed on most Macs |
| Xcode or Command Line Tools | C and C++ via clangd, and `lldb-dap` for the debugger | `xcode-select --install` |

Language servers are never downloaded. Hulder finds the ones on your `PATH` (`vtsls`, `clangd`, `rust-analyzer`, `lua-language-server`, `kotlin-language-server`, `jdtls`).

## Features

### Editor
- Zed's editor, multibuffers, project search, Tree-sitter highlighting, LSP, Git gutter and panel.
- **Grove** project tree: filter bar (`/`) with `ext:ts`, `git:modified`, `since:2h`, `size:>1m`; pinned and recent groups; `t` opens a terminal at a folder; folder file counts and sizes.
- **Tail toggle** (`Cmd+Alt+I`): reveals whitespace, line endings, zero-width and bidi characters, and mixed-script lookalike letters.
- **Skog** theme pair, dark and light, as defaults.
- Encoding and line-ending selectors, Format menu entries.
- Markdown preview with Mermaid diagrams rendered natively, SVG and image viewers, CSV tables, and a PDF reader.

### Terminal and agents

The [Hooks and agent activity inspector](docs/agent_activity.md) shows project and user hook configuration alongside recorded Claude and Codex tool activity. Open it with the wrench in the status bar.

- **Herd** (`Cmd+Ctrl+H`): herdr workspaces and agents with live status; select an agent and its session attaches in an embedded terminal; a ram in the status bar counts agents waiting on you.
- Toasts when an agent blocks or finishes.
- **Saga** (`Cmd+Ctrl+Shift+T`): tales (tickets), verses (todos) and lore (notes) in `.hulder/`; send a tale to the shown agent or start an agent on it.
- **Bifröst**: `hulder mcp` exposes the Saga and editor context to any MCP-capable agent.

### Web, without a web view
- **Panda** panel (`Cmd+Ctrl+Shift+P`): Lightpanda as a managed local process; fetch a URL as Markdown into a read-only buffer; share it with agents via `.mcp.json`.
- **Element picker** (`Cmd+Ctrl+Shift+E`): drives your Chrome over DevTools Protocol with a hardened throwaway profile.
- **Hollow Back**: the status-bar eye. Every refused outbound request, every page the panda browsed, every bridge call, every Raven send, listed.
- **Raven** (`Cmd+Ctrl+Shift+R`): a small API client. Requests are `.http` files in `.hulder/ravens/`, environments in `env.json`; Cmd-Enter sends through a client built for that one call, responses show pretty-printed with the language from `Content-Type`, and "Save as Clipping" writes a redacted `kind: raven` clipping. Import a cURL line from the clipboard; Bifröst can list requests and read the redacted last response but never send.
- **Hoard** (`hoard: toggle`): one auth store for Raven, the Panda and the picker. Profiles (bearer, basic, header, cookies) keep their metadata in `~/.config/hulder/hoard/profiles.json` and their secret in the macOS Keychain, match hosts exactly or by `*.suffix`, and travel over https only (except to localhost). "Borrow from Chrome" lifts a cookie jar out of the throwaway Chrome profile and clears it there.

### Run and debug, offline
- `LLDB` debug adapter using the `lldb-dap` that ships with Xcode. Nothing downloaded.
- Shipped tasks for `cargo run`, `cargo test`, and compile-and-run for C and C++.
- Gutter play buttons for Rust tests and C/C++ `main`.

## The Viking software factory

Use the task picker to open Heimdall, check the local services, or start the factory. Inspect plans, agents, blockers and verification in Heimdall; edit the task worktrees in Hulder. The existing Herd panel attaches to Herdr-backed agents. See [factory integration](FACTORY_INTEGRATION.md) for setup and the boundaries between the IDE, dashboard and agent runtimes.

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

- The shared editor HTTP client refuses outbound requests, including inherited cloud calls.
- Raven uses a separate, explicit request path: a request goes out only when you press Send, through a client that exists for that call alone, and it is listed in the Hollow Back with its host, status and byte counts. Hoard secrets live in the Keychain and never reach logs, clippings, Bifröst or the Hollow Back.
- Editor integrations include local Herdr and Bifröst sockets, browser debugging ports, and authenticated loopback access to Mimir. External tools, language servers, Git and terminal commands have their own network behavior.
- Chrome, when you open it from Hulder, uses a dedicated profile with extensions and sync off. While it is open, other local processes can also reach its debug port; do not log into anything sensitive in that window.
- While the Panda fetches with a Hoard profile, its loopback DevTools port is open for the duration of that fetch; other local processes could reach it. Lightpanda builds that take a cookie file use that instead, and the jar is cleared and the process killed when the fetch ends.
- Settings live in `~/.config/hulder`, data in `~/Library/Application Support/Hulder`, logs in `~/Library/Logs/Hulder`.

## Building

```sh
cargo build --locked -p zed --bin Hulder --features util/debug-embed
# See RELEASE.md before preparing a public binary.
cargo test -p herdr -p herd_panel -p hulder_net -p lightpanda -p cdp -p browser -p dap_adapters -p project_panel
./script/clippy -p herdr      # lint with the repo's settings
```

If `xcode-select` points at Command Line Tools that lack C++ headers, `.cargo/config.toml` already pins `DEVELOPER_DIR` to Xcode.app.

Design documents: `docs/superpowers/specs/` and `docs/superpowers/plans/`.

## Contributing

Issues and pull requests are welcome. Keep agents in the terminal and make network access explicit. Review your diff before submitting.

See [the Hulder guide](HULDER_GUIDE.md), [release instructions](RELEASE.md) and [security policy](SECURITY.md).

## License

Hulder is licensed under the GNU General Public License v3.0 or later, like the Zed editor it is built from.
GPUI, Zed's UI framework, is Apache 2.0. See `LICENSE-GPL` and `LICENSE-APACHE`.
"Zed" and the Zed logo are trademarks of Zed Industries; Hulder is not affiliated with or endorsed by Zed Industries.

## Veil privacy screen

Veil is a playful visual privacy screen, not an operating-system lock. Each lock asks for a new temporary password; unlocking discards it, and quitting/reopening Hulder resets it. The unlock card stays readable, with a show/hide-password button. Existing legacy `veil.json` files are left untouched and are no longer loaded by normal startup. Use the macOS lock screen when leaving your laptop unattended.

## Extensions and add ons

Customize Hulder with language and appearance extensions, or install native feature add ons with commands, settings and project templates. See [the extension guide](EXTENSIONS.md) and [LÖVE game tools](examples/addons/love/README.md). Feature add ons use explicit command approval and start disabled after installation or updates.

## Bifrost source ownership

Bifrost is maintained as a separate project. This repository embeds its core and native adapter so Hulder builds remain self contained. The `hulder mcp` command is unchanged. See [Bifrost integration](BIFROST_INTEGRATION.md) for updating and verifying the embedded source.

## Experimental release status

The experimental public source prerelease is `v0.1.0`. See [release scope and setup](RELEASE-v0.1.0.md) for tested behavior and acceptance limits. Manual accessibility testing is pending. Vendor integrations without authenticated live acceptance remain experimental.
