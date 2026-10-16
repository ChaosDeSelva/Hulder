# Run and Debug Without Downloads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Running and debugging C++ and Rust in Hulder works on a Mac with Xcode Command Line Tools and nothing else, with no network. The debugger drives the system `lldb-dap`; adapters that need a download say so in the console instead of trying.

**Architecture:** One new stdio debug adapter `LLDB` in `crates/dap_adapters/src/lldb_dap.rs`, modeled on `gdb.rs`. It resolves the binary from `dap.LLDB.binary`, then `PATH`, then `xcrun --find lldb-dap`, then the two Xcode install paths. The download paths in `codelldb.rs`, `javascript.rs`, `go.rs`, and `python.rs` are gated behind one shared `adapters::refuse_download` helper that prints the Hulder message. Shipped `tasks.json` and `debug.json` templates, a C++ `ContextProvider` for the existing `cpp-main` runnable tag, and a `debugger::DebugActiveFile` action tie it together.

**Tech Stack:** Rust 1.98, gpui, `dap::adapters::DebugAdapter`, `task::TaskTemplate` / `DebugScenario` / `BuildTaskDefinition`, tree-sitter `runnables.scm`.

**Spec:** `docs/superpowers/specs/2026-10-13-hulder-design.md`, section 9 (added by Task 7).

## Global Constraints

- Never call `delegate.http_client()` from an adapter in a way that reaches the network; the global client is `BlockedHttpClient` and every request fails. Fail before the request with a message that names what to install.
- Task variables used in JSON templates must exist in `task::VariableName`: `ZED_FILE`, `ZED_STEM`, `ZED_WORKTREE_ROOT`, `ZED_DIRNAME`, `ZED_FILENAME`.
- `lldb-dap` (lldb-1600, Xcode 16) takes `env` as an array of `"KEY=VALUE"` strings; newer builds also accept an object. Emit the array form and normalize objects in `get_binary`.
- Follow repo CLAUDE.md: no `unwrap()` outside tests, no `let _ =`, no `mod.rs`, `./script/clippy` not `cargo clippy`.
- Keep README's `> [!IMPORTANT]` marker in place.
- Do not edit `crates/herdr`, `crates/herd_panel`, `crates/project_panel`, `crates/editor`, `crates/lightpanda`, `crates/browser`, `crates/cdp`, or `crates/title_bar`. Keep `crates/zed/src/main.rs`, `zed.rs`, `app_menus.rs`, `Cargo.toml`, and `assets/settings/default.json` additive.

## Review Focus

1. Binary resolution order is exactly: user setting, `which`, `xcrun --find`, known paths. Test in Task 1 with a fake delegate whose `which` answer is configurable.
2. `env` normalization: an object `{"A":"1"}` becomes `["A=1"]`; an array passes through; missing stays missing. Test in Task 1.
3. Every download site refuses before any HTTP call and the console line reads `Hulder does not download debug adapters; install <X> and set dap.<Name>.binary`. Test in Task 2 for the shared helper; the four call sites are checked by reading.
4. `cwd` defaults to the worktree root when the scenario omits it, same as GDB. Test in Task 1.
5. The C++ runnable only appears for files containing `int main(`; the task it spawns is the same compile-and-run command as the shipped template. Checked manually in Task 8.

---

### Task 1: `LLDB` adapter

**Files:**
- Create: `crates/dap_adapters/src/lldb_dap.rs`
- Modify: `crates/dap_adapters/src/dap_adapters.rs` (register, `mod lldb_dap`)
- Modify: `crates/grammars/src/{c,cpp,rust}/config.toml` (`debuggers = ["LLDB", "CodeLLDB", "GDB"]`)

- [ ] Write failing tests: `resolves_user_setting_first`, `falls_back_to_which`, `falls_back_to_known_paths`, `env_object_becomes_array`, `launch_config_maps_fields`, `attach_config_maps_pid`, `cwd_defaults_to_worktree_root`.
- [ ] Implement `LldbDapDebugAdapter` with `name`, `config_from_zed_format`, `dap_schema`, `get_binary`; `resolve_binary` takes the candidate list so tests do not spawn `xcrun`.
- [ ] Register in `init` after `GdbDebugAdapter`; put `LLDB` first in the three `config.toml` `debuggers` lists so "New debug session" preselects it.
- [ ] `cargo test -p dap_adapters` green.
- [ ] Commit.

### Task 2: Refuse downloads

**Files:**
- Modify: `crates/dap/src/adapters.rs` (add `pub fn refuse_download(adapter: &DebugAdapterName, install_hint: &str, delegate: &dyn DapDelegate) -> anyhow::Error`)
- Modify: `crates/dap_adapters/src/{codelldb,javascript,go,python}.rs`

- [ ] Test the helper's message text in `crates/dap/src/adapters.rs`.
- [ ] CodeLLDB: when no user path and nothing cached, refuse (hint: CodeLLDB VSIX extracted somewhere, `dap.CodeLLDB.binary` to `extension/adapter/codelldb`).
- [ ] JavaScript: skip the "Checking latest version" block; refuse when `get_installed_binary` cannot find a cached adapter and no user path.
- [ ] Go: `install_shim` refuses unless cached; user-installed `dlv` path still works.
- [ ] Python: `fetch_debugpy_whl` refuses unless the cached adapter dir exists (pip install of debugpy in the active venv is still honoured).
- [ ] `cargo test -p dap_adapters -p dap`, clippy.
- [ ] Commit.

### Task 3: Shipped templates

**Files:**
- Modify: `assets/settings/initial_debug_tasks.json`
- Modify: `assets/settings/initial_tasks.json`

- [ ] Replace examples with "Debug Rust binary" (LLDB, `build` = `cargo build`, program `$ZED_WORKTREE_ROOT/target/debug/<crate>`) and "Debug active C++ file" (LLDB, `build` = clang++ into `.hulder/bin/$ZED_STEM`).
- [ ] Add run tasks: cargo run / test / build, "C++: compile and run active file", "Run active file (shebang)".
- [ ] Commit.

### Task 4: C++ runnable and context provider

**Files:**
- Modify: `crates/languages/src/cpp.rs` (add `CppContextProvider`)
- Modify: `crates/languages/src/lib.rs` (attach to `c` and `cpp`)
- Verify: `crates/grammars/src/cpp/runnables.scm` already tags `cpp-main`; `c/runnables.scm` tags `c-main`.

- [ ] `associated_tasks` returns one template tagged `cpp-main` and `c-main` whose command is the same compile-and-run shell line as the shipped task.
- [ ] Unit test that the template carries both tags and uses `$ZED_FILE`.
- [ ] Commit.

### Task 5: Run menu and `DebugActiveFile`

**Files:**
- Modify: `crates/debugger_ui/src/debugger_ui.rs` (new action, handler)
- Modify: `crates/zed/src/zed/app_menus.rs` (Run menu entries)

- [ ] Add `DebugActiveFile` to the `debugger` actions. Handler: if the active buffer is C/C++, start an LLDB session with the clang++ build step; otherwise open the Start modal.
- [ ] Run menu: "Run Task…" (`zed_actions::Spawn::modal()`), "Rerun Last Task" (`zed_actions::Rerun`), "Start Debugging…" (`debugger_ui::Start`), "Debug Active File" (`debugger_ui::DebugActiveFile`).
- [ ] Commit.

### Task 6: Verify rust-analyzer runnables offline

- [ ] Confirm `crates/languages/src/rust.rs::check_if_user_installed` finds `rustup which rust-analyzer` before any download path; note the fallback order in the spec.
- [ ] Manual: open a Rust file, click the gutter play button, see `cargo` output in the terminal panel.

### Task 7: Spec section 9

- [ ] Append "9. Run and debug without downloads" to the design spec: what ships, what the user installs (nothing for C++/Rust with Xcode CLT), the resolution order, and the refusal message.
- [ ] Commit.

### Task 8: Manual check

- [ ] `/tmp/hulder-cpp/main.cpp` with a loop; open in `target/debug/Hulder`; run "C++: compile and run active file" from the task picker; confirm output in the terminal panel.
- [ ] Set a breakpoint, start an LLDB session via "Debug active C++ file"; confirm it stops, or if the UI cannot be driven, confirm `lldb-dap` launches from the debug console / log.
- [ ] Quit Hulder.
