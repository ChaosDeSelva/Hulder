# Worktrees in the Grove — plan

Spec: `docs/superpowers/specs/2026-10-13-hulder-design.md` §17.

## Shape

- `crates/project_panel/src/worktrees.rs` (new): everything for the group.
  - Pure: `WorktreeEntry`, `parse_porcelain`, `agent_tag`, `prune_candidates`,
    `delete_allowed`, `short_sha`.
  - `GitRunner` trait: `run(cwd, args) -> Task<Result<GitOutput>>`. The real
    impl spawns `git` as argv with `current_dir` through `util::command`;
    tests install a `FakeGitRunner` global that answers from canned output.
  - `Worktrees` entity (`Render`): holds entries, dirty cache (30 s), HEAD
    ages, selection, expanded flag, its own `FocusHandle` and a `Worktrees`
    key context so Enter / `t` / arrows act on the selected row. Watches
    `<common dir>/worktrees` through `Fs::watch` (500 ms) and refreshes on
    panel focus. No polling.
  - Actions: `menu::Confirm` adds the folder (`find_or_create_worktree`),
    `project_panel::OpenInTerminal` opens a center terminal there,
    `RevealInFileManager`, `PruneStaleWorktrees` (confirm dialog listing the
    candidates, then `git worktree prune` and `git worktree remove` per
    candidate, each path checked by `delete_allowed`).
- `crates/herdr/src/state.rs`: `HerdState::try_global`, `agent_for_path`.
- `project_panel.rs`: additive only — `mod worktrees;`, a `worktrees` field,
  construction in `new`, one child in `render`, a refresh in `focus_in`,
  the `PruneStaleWorktrees` action in the actions list.
- Keymaps: a `Worktrees` context block (enter, t, up, down).

## TDD order

1. herdr `agent_for_path` test → impl.
2. `parse_porcelain` tests (locked with/without reason, prunable, detached,
   bare, main checkout excluded) → impl.
3. `agent_tag` tests → impl.
4. `prune_candidates` (unlocked ∧ clean ∧ merged) and `delete_allowed` tests
   → impl.
5. `Worktrees` entity with FakeGitRunner: FakeFs test that touching
   `.git/worktrees/<name>/gitdir` re-runs the list and the rows change.
6. Wire into the panel, keymaps, build, clippy.
