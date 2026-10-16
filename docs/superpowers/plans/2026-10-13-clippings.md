# Clippings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Spec section 13. Every element pick (browser), Lightpanda fetch and PDF "Open as Markdown" becomes a file `.hulder/clippings/YYYY-MM-DD-<slug>.md` with front matter `source`, `captured`, `kind` (pick|fetch|pdf) and `selector`; pick screenshots land in `.hulder/clippings/assets/`. The buffer that opens is the saved file. The Saga panel lists clippings under a "Clippings" group and `saga::SendToAgent` works on them. Bifröst gains `saga_list_clippings()` and `saga_read_clipping(name)`. `"saga": { "save_clippings": false }` restores the scratch-buffer behaviour.

**Architecture:** One new crate `clippings`, lib root `src/clippings.rs`, with three modules. `model.rs` is pure: front matter round trip, slug, file and asset naming, name validation, size caps. `store.rs` is `ClippingStore` over `&dyn fs::Fs`: the same path containment, symlink refusal and atomic writes as `saga::store`, copied rather than imported so `saga` can depend on `clippings` without a cycle. `capture.rs` is the only gpui code: `save_and_open` reads the setting, finds the first visible worktree, writes through the store on the background executor and opens the saved file with `Workspace::open_abs_path`; it returns `Ok(None)` when the setting is off or there is no local worktree so each producer can fall back to its existing scratch buffer. The producers (`browser`, `lightpanda`, `pdf_viewer`) each gain one early-return call. `saga` lists clippings in its `Snapshot` through `ClippingStore::list` and excludes `clippings/` from lore. `bifrost` adds `clipping_files.rs` mirroring `saga_files.rs` (sync `std::fs` on the background thread).

**Tech Stack:** Rust, gpui, `fs::Fs` (`FakeFs` in tests), `settings_content` + `settings::RegisterSetting`, `workspace::Workspace::open_abs_path`, `chrono`.

**Spec:** `docs/superpowers/specs/2026-10-13-hulder-design.md` section 13 (with 6.2, 7.1, 10, 11, 12).

## Global Constraints

- `model.rs` and `store.rs` contain no gpui types.
- Clipping name: `YYYY-MM-DD-<slug>.md`, slug `[a-z0-9-]+`, at most 48 chars; a second capture with the same name gets `-2`, `-3`, ... before `.md`.
- Caps: markdown body 1 MiB, asset 8 MiB, listing stops at 2000 files. Larger inputs are an error, never a truncation.
- Store paths: relative, no `..`, no absolute, no separators inside a name; neither `.hulder` nor anything down to the target may be a symlink; markdown is written with `Fs::atomic_write`.
- Screenshot reference in the body is relative to the markdown file (`assets/<stem>.png`) so Markdown preview resolves it.
- Keep edits to the shared crates small and additive; new behaviour lives in new files or new functions.
- Follow repo CLAUDE.md: no `unwrap()` outside tests, no `let _ =`, no `mod.rs`, `./script/clippy`, README marker stays.

## Review Focus

1. Front matter round trip, including `selector` absent and a `source` with `: `. Task 1.
2. Name validation refuses `/`, `\`, `..`, uppercase, non-`.md`, and the store refuses a symlinked `clippings/`. Task 2.
3. `save_and_open` returns `None` (producer falls back) when `save_clippings` is false. Task 3.
4. Bifröst `saga_read_clipping` refuses a name that is valid in shape but resolves through a symlink. Task 6.

---

### Task 1: Crate scaffold and pure model

**Files:** `crates/clippings/Cargo.toml`, `crates/clippings/src/clippings.rs`, `crates/clippings/src/model.rs`, root `Cargo.toml`.

- [x] Failing tests: round trip, slug and naming, name validation, kind parse.
- [x] Implement `Kind`, `Clipping { source, captured, kind, selector, body }` with `parse`/`serialize`, `slugify`, `file_name`, `stem`, `is_valid_clipping_name`, `title_for_source`.

### Task 2: `ClippingStore`

**Files:** `crates/clippings/src/store.rs`.

- [x] Failing tests on `FakeFs`: save writes `clippings/<date>-<slug>.md` and the asset, second save gets `-2`, oversize body and asset refused, listing skips non-clippings, symlink refused, `..` refused, 2000 cap.
- [x] Implement `resolve`, `guarded`, `unique_stem`, `save(fs, draft, today)`, `list(fs)`, `read(fs, name)`.

### Task 3: Setting and `capture::save_and_open`

**Files:** `crates/settings_content/src/saga.rs`, `settings_content.rs`, `crates/settings/src/vscode_import.rs`, `assets/settings/default.json`, `crates/clippings/src/settings.rs`, `crates/clippings/src/capture.rs`.

- [x] Failing test: default resolves `save_clippings == true`; `save_and_open` with the setting off returns `None` and writes nothing.
- [x] Implement `ClippingsSettings`, `store_for(workspace)`, `save_and_open`.

### Task 4: Wire the three producers

**Files:** `crates/browser/src/browser.rs`, `crates/lightpanda/src/reader.rs`, `crates/pdf_viewer/src/pdf_viewer.rs` and their `Cargo.toml`.

- [x] Each producer builds a `ClippingDraft` and calls `save_and_open`; on `None` it runs the existing scratch-buffer code unchanged.

### Task 5: Saga panel group

**Files:** `crates/saga/src/model.rs`, `store.rs`, `panel.rs`, `Cargo.toml`.

- [x] Failing tests: `lore_files` excludes `clippings/`; `flatten_rows` emits `Clippings (n)`; snapshot load lists clippings.
- [x] `Snapshot.clippings`, `Row::Clipping`, render with `IconName::Scissors`, `selected_path` covers it so `OpenSelected` and `SendToAgent` work.

### Task 6: Bifröst tools

**Files:** `crates/bifrost/src/clipping_files.rs`, `dispatch.rs`, `bifrost.rs`, `Cargo.toml`, `crates/cli/src/mcp.rs`.

- [x] Failing tests: list and read through `dispatch_saga`; escape, symlink and oversize refused; `hulder mcp` lists the two tools.
- [x] Implement and route `saga_list_clippings`, `saga_read_clipping`.

### Deviations recorded while implementing

- A symlink already sitting at the generated name (`<date>-<slug>.md` or its `.png`) refuses the whole save rather than numbering around it; the producer then opens its scratch buffer with a toast, so the capture is kept and the link is never written through.
- `ClippingStore::read` takes a `gpui::BackgroundExecutor` so the capped read runs off the foreground thread, as `glosses` does.
- Bifröst's layer is sync `std::fs` (like `saga_files`), so it reuses only the pure `clippings::model` and not `ClippingStore`.

### Task 7: Verify

- [x] `cargo test -p clippings -p saga -p bifrost`
- [x] `./script/clippy -p clippings -p saga -p bifrost -p browser -p lightpanda -p pdf_viewer`
- [x] `cargo build -p zed`
