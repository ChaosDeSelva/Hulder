# Grove Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend Zed's `project_panel` into the first slice of the Grove: a filter bar with prefix syntax that flattens the tree into fuzzy matches, "Pinned" and "Recent" groups above the tree persisted per workspace, a `t` key that opens a center terminal at the selected folder, and file-count/size stats on expanded folders.

**Architecture:** No new crates. A new module `crates/project_panel/src/grove.rs` holds the pure, unit-tested logic: `parse_filter`, `Filter::matches`, `format_size`, `FolderStats`, the recent/pinned list helpers, and the `db::kvp` key scheme. `ProjectPanel` in `project_panel.rs` grows a second single-line `Editor` (the filter), a `filter_matches` list computed in a background task with the `fuzzy` crate, `pinned`/`recent` lists loaded from `db::kvp` on construction, and a `folder_stats` cache filled by a background task after every visible-entries update. Rendering branches once: when the filter editor is non-empty the panel renders a flat `uniform_list` of matches instead of the tree; otherwise it renders the existing tree with the group rows and filter bar stacked above it.

**Tech Stack:** Rust 1.98, gpui (`cx.spawn_in`, `cx.background_spawn`, `uniform_list`, `KeyContext`), `editor::Editor::single_line`, `fuzzy::match_strings` + `ui::HighlightedLabel` (as `crates/file_finder` does), `project::GitTraversal` for per-entry git summaries, `worktree::Snapshot::traverse_from_path` for folder stats, `db::kvp::KeyValueStore` + `db::write_and_log` for persistence, `terminal_view::terminal_panel::TerminalPanel::add_terminal_task` with `task::SpawnInTerminal`.

**Spec:** `docs/superpowers/specs/2026-10-13-hulder-design.md`, section 8 (8.1 filter bar, 8.2 last bullet folder stats, 8.3 groups, 8.4 terminal actions).

## Global Constraints

- Extend `crates/project_panel`; do not rewrite it. No edits to `crates/herdr`, `crates/herd_panel`, `crates/lightpanda`, `crates/browser`, `crates/cdp`, or `crates/editor`. `crates/zed` and `assets/settings/default.json` only if strictly required (phase 1 needs neither).
- Follow repo CLAUDE.md: no `unwrap()` outside tests, no `let _ =` on fallible calls, no `mod.rs`, full-word identifiers, `./script/clippy -p project_panel` not `cargo clippy`.
- Keep README's `> [!IMPORTANT]` marker in place (already present).
- Build with `export PATH="$HOME/.cargo/bin:$PATH"; export CARGO_TARGET_DIR=/Users/jlashley/Code/cotzed/target; cargo build -p zed`.
- Keybindings go in `assets/keymaps/default-macos.json`: new single-key bindings in the existing `"ProjectPanel && not_editing"` block, and a new `"GroveFilter > Editor"` block placed after the `Editor` blocks (gpui picks the deepest matching context, then the later keymap entry, so this overrides the editor's `up`/`down`/`escape` the same way `"Picker > Editor"` does).
- The filter editor must count as `editing` in `ProjectPanel::dispatch_context`, otherwise `space` (Open), vim letter bindings, and the new `/`, `p`, `t` would fire while typing a query.
- Persistence keys are scoped by `Workspace::database_id()`. When it is `None` (tests, transient windows) the lists live in memory only.
- Persisted paths are `{ "worktree": "<absolute root>", "path": "<unix relative path>" }` so multi-root workspaces are unambiguous; the spec's "relative paths" are kept relative to their worktree root.
- TDD: write the failing test, run it, then implement. Pure logic lives in `grove.rs` with `#[cfg(test)] mod tests` at the bottom; panel behavior tests append to `project_panel_tests.rs` using the existing `init_test`, `init_test_with_editor`, `select_path`, `visible_entries_as_strings`, and `ensure_single_file_is_opened` helpers.

## Review Focus

1. Typing in the filter must never trigger panel single-key actions. Test: `dispatch_context` contains `editing` and `filtering` while the filter editor is focused (Task 3).
2. Filter results must be computed off the main thread and must be cancelled when the query changes (`AtomicBool` cancel flag, task stored in a field so dropping it cancels). Test in Task 3 (stale task's results are not applied: set text twice, run until parked, assert matches reflect the last query).
3. Enter on a directory match must not open it as a file: it clears the filter and reveals the directory in the tree (Task 3).
4. Pinned/recent entries whose files were deleted must be skipped when rendering, not panic and not be removed from the persisted list (they may come back on a branch switch) (Task 4).
5. Folder stats must be keyed by snapshot `scan_id` so a stale cache is recomputed after a filesystem change, and the cache must be pruned to the currently expanded set so it cannot grow unbounded (Task 6).
6. `t` on a file uses the file's parent; on a worktree root uses the root; on a remote project the error is logged, never a panic (Task 5).

---

### Task 1: `grove.rs` filter parsing

**Files:**
- Create: `crates/project_panel/src/grove.rs`
- Modify: `crates/project_panel/src/project_panel.rs` (add `pub mod grove;` next to `mod undo;`)

**Interfaces:**
- `pub struct Filter { pub text: String, pub ext: Option<String>, pub git: Option<GitFilter>, pub since: Option<Duration>, pub size_gt: Option<u64> }`
- `pub enum GitFilter { Modified, Added, Untracked, Conflict }`
- `pub fn parse_filter(input: &str) -> Filter`
- `fn parse_duration(text: &str) -> Option<Duration>` (`m`, `h`, `d`), `fn parse_size(text: &str) -> Option<u64>` (`k`, `m`, `g`, optional leading `>`), both private.

- [x] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_has_no_prefixes() {
        let filter = parse_filter("main rs");
        assert_eq!(filter.text, "main rs");
        assert_eq!(filter.ext, None);
        assert_eq!(filter.git, None);
        assert_eq!(filter.since, None);
        assert_eq!(filter.size_gt, None);
    }

    #[test]
    fn prefixes_compose_and_leave_free_text() {
        let filter = parse_filter("ext:ts git:modified since:2h size:>1m panel");
        assert_eq!(filter.text, "panel");
        assert_eq!(filter.ext.as_deref(), Some("ts"));
        assert_eq!(filter.git, Some(GitFilter::Modified));
        assert_eq!(filter.since, Some(Duration::from_secs(2 * 3600)));
        assert_eq!(filter.size_gt, Some(1024 * 1024));
    }

    #[test]
    fn ext_prefix_strips_leading_dot_and_lowercases() {
        assert_eq!(parse_filter("ext:.TS").ext.as_deref(), Some("ts"));
    }

    #[test]
    fn duration_units() {
        assert_eq!(parse_filter("since:15m").since, Some(Duration::from_secs(900)));
        assert_eq!(parse_filter("since:3d").since, Some(Duration::from_secs(3 * 86400)));
        assert_eq!(parse_filter("since:3w").since, None);
        assert_eq!(parse_filter("since:").since, None);
    }

    #[test]
    fn size_units_and_optional_gt() {
        assert_eq!(parse_filter("size:>2k").size_gt, Some(2048));
        assert_eq!(parse_filter("size:1g").size_gt, Some(1024 * 1024 * 1024));
        assert_eq!(parse_filter("size:512").size_gt, Some(512));
        assert_eq!(parse_filter("size:>big").size_gt, None);
    }

    #[test]
    fn unknown_git_value_is_kept_as_text() {
        let filter = parse_filter("git:weird");
        assert_eq!(filter.git, None);
        assert_eq!(filter.text, "git:weird");
    }
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p project_panel grove::`
Expected: compile error, module does not exist.

- [x] **Step 3: Write the module**

`grove.rs` top half: `Filter`, `GitFilter` (derive `Debug, Clone, Copy, PartialEq, Eq`), `parse_filter` splitting on whitespace, matching `key:value` for `ext`, `git`, `since`, `size`; tokens that do not parse are kept in `text`; `text` is the remaining tokens joined with single spaces.

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test -p project_panel grove::`

- [x] **Step 5: Commit**

`git commit -m "project_panel: Add grove filter parsing"` (with the Co-Authored-By trailer).

---

### Task 2: `grove.rs` entry matching, size formatting, list helpers

**Files:**
- Modify: `crates/project_panel/src/grove.rs`

**Interfaces:**
- `pub struct EntryFacts<'a> { pub path: &'a str, pub is_dir: bool, pub mtime: Option<SystemTime>, pub size: u64, pub git: GitSummary }`
- `impl Filter { pub fn has_prefixes(&self) -> bool; pub fn matches(&self, facts: &EntryFacts, now: SystemTime) -> bool }`
- `impl GitFilter { pub fn matches(self, summary: &GitSummary) -> bool }`
- `pub fn format_size(bytes: u64) -> String` (`512 B`, `1.5 KB`, `3.2 MB`, `1.1 GB`)
- `pub struct FolderStats { pub files: usize, pub bytes: u64 }` with `pub fn label(&self) -> String` (`"12 files · 3.2 MB"`, singular `"1 file"`)
- `#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)] pub struct GrovePath { pub worktree: PathBuf, pub path: String }`
- `pub fn push_recent(list: &mut Vec<GrovePath>, item: GrovePath, cap: usize)` (move to front, dedupe, truncate)
- `pub fn toggle(list: &mut Vec<GrovePath>, item: GrovePath) -> bool` (returns whether now pinned)
- `pub fn encode(list: &[GrovePath]) -> Result<String>`, `pub fn decode(json: &str) -> Vec<GrovePath>` (invalid JSON yields empty list and logs)
- `pub const RECENT_CAP: usize = 10;`
- `pub fn kvp_key(kind: &str, workspace_id: i64) -> String` → `"grove:{kind}:{id}"`

- [x] **Step 1: Write the failing tests**

Tests cover: ext match is case-insensitive and ignores directories; `since` rejects entries older than the window and entries without mtime; `size_gt` is strict; `git:modified` matches `worktree.modified` or `index.modified`, `git:added` matches `index.added`, `git:untracked`, `git:conflict`; `has_prefixes`; `format_size` boundaries; `FolderStats::label` singular/plural; `push_recent` moves an existing item to the front and caps at 10; `toggle` adds then removes; `decode("not json")` is empty; `encode`/`decode` round trip.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p project_panel grove::`

- [x] **Step 3: Implement**

Pure functions only. `matches` ignores `self.text` (fuzzy matching is the caller's job). Directories satisfy `ext` only when `ext` is `None`; `size_gt` and `since` apply to files only (directories have no meaningful size; their mtime is their own).

- [x] **Step 4: Run test to verify it passes**

- [x] **Step 5: Commit**

---

### Task 3: Filter bar in the panel

**Files:**
- Modify: `crates/project_panel/src/project_panel.rs`
- Modify: `crates/project_panel/Cargo.toml` (add `fuzzy.workspace = true`)
- Modify: `assets/keymaps/default-macos.json`
- Modify: `crates/project_panel/src/project_panel_tests.rs`

**Interfaces:**
- Actions: `project_panel::FocusFilter`, `project_panel::ClearFilter`.
- Fields on `ProjectPanel`: `filter_editor: Entity<Editor>`, `filter_matches: Vec<FilterMatch>`, `filter_task: Task<()>`, `filter_cancel: Arc<AtomicBool>`.
- `struct FilterMatch { worktree_id: WorktreeId, entry_id: ProjectEntryId, is_dir: bool, label: String, positions: Vec<usize> }`
- `fn is_filtering(&self, cx: &App) -> bool` (filter editor text non-empty)
- `fn refresh_filter(&mut self, window, cx)`: parse, snapshot worktrees + repo snapshots + path style + `hide_hidden`, spawn background: walk `GitTraversal::new(&repo_snapshots, snapshot.entries(false, 0))`, skip the root entry, apply `Filter::matches`, build `StringMatchCandidate`s from `entry.path.display(path_style)`, run `fuzzy::match_strings(.., max 200, ..)` (or take the first 200 candidates when `text` is empty), sort by score desc then label; on completion set `filter_matches`, select the first match, notify.
- `fn clear_filter(&mut self, window, cx)`: clear editor text, drop matches, focus the panel.
- `fn render_filter_bar(&self, cx) -> impl IntoElement`: `div().key_context("GroveFilter")` wrapping the editor with a search icon, border-bottom, placeholder `Filter  (ext: git: since: size:)`.
- `fn render_filter_matches(&mut self, window, cx) -> AnyElement`: `uniform_list("grove-filter", n, ..)` tracking `self.scroll_handle`; rows are `ListItem` with the file/folder icon, `HighlightedLabel::new(label, positions)`, `.toggle_state(is_selected)`, on click: select + open (file) or reveal (dir).
- `dispatch_context`: `editing` when either editor is focused; add `filtering` when the filter editor is focused, else `not_filtering`.
- Navigation: `select_next`, `select_previous`, `select_first`, `select_last`, `autoscroll` branch on `is_filtering` and move inside `filter_matches`. `open_internal` branch: directory match → `clear_filter` then `reveal_entry`. `cancel` (Escape with the panel focused) clears the filter when non-empty.

- [x] **Step 1: Write the failing tests** (append to `project_panel_tests.rs`)

```rust
#[gpui::test]
async fn test_grove_filter_flattens_tree_into_matches(cx: &mut gpui::TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({
        "src": { "main.rs": "", "lib.rs": "", "util.ts": "" },
        "docs": { "guide.md": "" },
    })).await;
    let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
    let workspace = cx.add_window(|window, cx| Workspace::test_new(project.clone(), window, cx));
    let cx = &mut VisualTestContext::from_window(*workspace, cx);
    let panel = workspace.update(cx, ProjectPanel::new).unwrap();

    set_filter(&panel, "mainrs", cx);
    assert_eq!(filter_labels(&panel, cx), vec!["src/main.rs"]);

    set_filter(&panel, "ext:ts", cx);
    assert_eq!(filter_labels(&panel, cx), vec!["src/util.ts"]);

    set_filter(&panel, "", cx);
    assert!(filter_labels(&panel, cx).is_empty());
    assert!(!panel.read_with(cx, |panel, cx| panel.is_filtering(cx)));
}

#[gpui::test]
async fn test_grove_filter_navigation_and_open(cx: &mut gpui::TestAppContext) {
    // init_test_with_editor; two matches for "rs"; SelectNext moves selection to the second;
    // Open opens that file (ensure_single_file_is_opened); Open on a directory match clears the
    // filter and selects the directory in the tree.
}

#[gpui::test]
async fn test_grove_filter_context_marks_editing(cx: &mut gpui::TestAppContext) {
    // focus filter editor via FocusFilter; dispatch_context contains "editing" and "filtering";
    // ClearFilter empties the editor and returns focus to the panel.
}
```

Helpers `set_filter(panel, text, cx)` (sets the editor text through `Editor::set_text` in `update_in`, then `cx.run_until_parked()`) and `filter_labels(panel, cx) -> Vec<String>`.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p project_panel test_grove_filter`

- [x] **Step 3: Implement** (see Interfaces). Register `on_action` for `FocusFilter` and `ClearFilter` in `render`. Subscribe to the filter editor's `EditorEvent::BufferEdited` in `new`.

- [x] **Step 4: Keymap**

In `"ProjectPanel && not_editing"`: `"/": "project_panel::FocusFilter"`. New block after it:

```json
{
  "context": "GroveFilter > Editor",
  "use_key_equivalents": true,
  "bindings": {
    "escape": "project_panel::ClearFilter",
    "up": "menu::SelectPrevious",
    "down": "menu::SelectNext",
    "enter": "project_panel::Open"
  }
}
```

- [x] **Step 5: Run tests, clippy**

Run: `cargo test -p project_panel && ./script/clippy -p project_panel`

- [x] **Step 6: Commit**

---

### Task 4: Pinned and Recent groups

**Files:**
- Modify: `crates/project_panel/src/project_panel.rs`
- Modify: `crates/project_panel/Cargo.toml` (add `db.workspace = true`)
- Modify: `assets/keymaps/default-macos.json` (`"p": "project_panel::TogglePin"` in `"ProjectPanel && not_editing"`)
- Modify: `crates/project_panel/src/project_panel_tests.rs`

**Interfaces:**
- Action: `project_panel::TogglePin`.
- Fields: `pinned: Vec<GrovePath>`, `recent: Vec<GrovePath>`, `pinned_expanded: bool`, `recent_expanded: bool`, `grove_workspace_id: Option<i64>`.
- `fn grove_path_for_entry(&self, worktree_id, entry_id, cx) -> Option<GrovePath>` and `fn resolve_grove_path(&self, path: &GrovePath, cx) -> Option<(WorktreeId, ProjectEntryId, bool /*is_dir*/)>` (match worktree by `abs_path`, then `entry_for_path`).
- `fn toggle_pin(&mut self, _: &TogglePin, window, cx)` toggles the selected entry and persists.
- `fn note_active_item(&mut self, workspace: &Entity<Workspace>, cx)` on `workspace::Event::ActiveItemChanged`: `active_item(cx)?.project_path(cx)` → `push_recent` → persist.
- `fn persist_grove_list(&self, kind: &'static str, list: &[GrovePath], cx)` via `db::write_and_log`; `fn load_grove_list(kind, workspace_id, cx) -> Vec<GrovePath>` via `KeyValueStore::global(cx).read_kvp`.
- `fn render_grove_group(&self, title, expanded, entries, on_toggle, cx) -> Option<impl IntoElement>`: header `ListItem` (chevron + muted uppercase label + count), rows with icon + file name + muted parent path; click opens (file) or reveals (dir); skipped when the list resolves to nothing.
- Groups render above the tree only when not filtering; pinned first, then recent.

- [x] **Step 1: Write the failing tests**

`test_grove_toggle_pin_persists_in_memory` (select `src/main.rs`, `TogglePin` → `pinned` has one entry with `path == "src/main.rs"`; again → empty), `test_grove_recent_tracks_opened_files` (`init_test_with_editor`; open two files through `workspace.open_path`; `recent` lists them most-recent-first; opening the first again moves it to the front without duplicates).

- [x] **Step 2: Run test to verify it fails**
- [x] **Step 3: Implement**
- [x] **Step 4: Run tests, clippy**
- [x] **Step 5: Commit**

---

### Task 5: Center terminal action and context menu

**Files:**
- Modify: `crates/project_panel/src/project_panel.rs`
- Modify: `crates/project_panel/Cargo.toml` (add `task.workspace = true`, `terminal_view.workspace = true`)
- Modify: `assets/keymaps/default-macos.json` (`"t": "project_panel::OpenInTerminal"` in `"ProjectPanel && not_editing"`)

**Interfaces:**
- Action: `project_panel::OpenInTerminal` (the existing import `workspace::OpenInTerminal` is renamed `WorkspaceOpenInTerminal` and keeps driving the context-menu item).
- `fn terminal_directory_for_selection(&self, cx) -> Option<PathBuf>` (dir → itself, file → parent) shared with the existing `open_in_terminal`.
- `fn open_in_center_terminal(&mut self, _: &OpenInTerminal, window, cx)`: `workspace.panel::<TerminalPanel>(cx)` → `add_terminal_task(SpawnInTerminal { cwd: Some(path), label/full_label: folder name, reveal_target: RevealTarget::Center, use_new_terminal: true, ..Default::default() }, RevealStrategy::Always, window, cx).detach_and_log_err(cx)`.
- Context menu: "Open in Terminal", "Copy Path", "Copy Relative Path" already exist; verify and leave in place.

- [x] **Step 1: Write the failing test**

`test_grove_terminal_directory_for_selection`: selecting `src/main.rs` yields `/root/src`; selecting `src` yields `/root/src`; selecting the root yields `/root`.

- [x] **Step 2: Run test to verify it fails**
- [x] **Step 3: Implement** and register `on_action` under the same `is_local || wsl` / `is_via_remote_server` guards as `open_in_terminal`.
- [x] **Step 4: Run tests, clippy, `cargo build -p zed`**
- [x] **Step 5: Commit**

---

### Task 6: Folder stats

**Files:**
- Modify: `crates/project_panel/src/project_panel.rs`
- Modify: `crates/project_panel/src/project_panel_tests.rs`

**Interfaces:**
- Field: `folder_stats: HashMap<(WorktreeId, Arc<RelPath>), (usize, FolderStats)>` (scan id + stats), `folder_stats_task: Task<()>`.
- `fn refresh_folder_stats(&mut self, cx)`: collect `(snapshot, Vec<Arc<RelPath>>)` for expanded directories missing or stale in the cache; prune cache entries no longer expanded; background-compute with `snapshot.traverse_from_path(true, false, false, &path)` stopping when `!entry.path.starts_with(&path)`; apply + notify. Called at the end of the `update_visible_entries` completion closure.
- `EntryDetails.folder_stats: Option<String>` filled in `details_for_entry` for expanded directories; rendered as `ListItem::end_slot(Label::new(..).size(LabelSize::Small).color(Color::Muted))`.

- [x] **Step 1: Write the failing test**

`test_grove_folder_stats_for_expanded_dir`: tree `src/{a.rs: "xxxx", b.rs: "yy"}`; after `ExpandAllEntries` + `run_until_parked`, `folder_stats` for `src` is `FolderStats { files: 2, bytes: 6 }`, and the root (`""`) counts 2 files too; `details.folder_stats` for `src` reads `"2 files · 6 B"`.

- [x] **Step 2: Run test to verify it fails**
- [x] **Step 3: Implement**
- [x] **Step 4: Run tests, clippy**
- [x] **Step 5: Commit**

---

### Task 7: Build, launch, verify

- [x] `cargo test -p project_panel` green.
- [x] `./script/clippy -p project_panel` clean.
- [x] `cargo build -p zed` succeeds.
- [x] Launch `target/debug/Hulder` on this repo, confirm the filter bar renders above the tree, `/` focuses it, typing flattens the tree, `p` pins, the Pinned group appears, opening a file populates Recent, `t` opens a center terminal, expanded folders show stats.
- [x] Final commit with the plan checkboxes updated.

## Deferred to phase 2

- `@<agent>` and `diag:error|warning` filter prefixes (need herdr / diagnostics plumbing beyond the snapshot).
- "Open in terminals" group (needs center terminal cwd tracking and herdr agents).
- Peek popover on hover/`space`, diagnostic badges, herdr ram badge (8.2).
- `T` (herdr TUI workspace) and "Send path to agent" (8.4).
- Linux/Windows keymaps.

## Review follow-ups

- Vim mode: `assets/keymaps/vim.json` binds `/`, `p` and `t` in `"ProjectPanel && not_editing"` (netrw compatibility) and loads after the default keymap, so the Grove keys vanished whenever vim mode was on. The vim keymap now carries its own Grove bindings on non-colliding keys: `?` focuses the filter, `shift-p` toggles pin, `shift-t` opens the terminal.
- Folder stats: one walk per worktree credits every file to each expanded ancestor on the path stack, debounced 300 ms after the last visible-entries update with an `AtomicBool` cancel checked in the loop; hidden and gitignored entries are skipped the same way rows are.
- Filter: the refresh after a worktree change is debounced 300 ms, the selection survives a refresh when its entry still matches (it only resets when the query text changes), the candidate walk is reused while the worktree scan ids and prefix filters are unchanged, and a whitespace-only query does not count as filtering.
- Recent: the design spec says "the last ten edited files"; the implementation records files on activation (`workspace::Event::ActiveItemChanged`), which was deliberately relaxed to "opened" so that read-only browsing still populates the group. Files from invisible worktrees (rust std, the cargo registry) are skipped because they never render in the Grove and would only evict real entries.
