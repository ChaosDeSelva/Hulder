//! The "Worktrees" group of the Grove: every linked worktree of the project's
//! root repository, read from `git worktree list --porcelain`.

use crate::grove::format_age;
use crate::{
    Copy as CopyEntry, Cut, Delete, Duplicate, NewDirectory, NewFile, OpenInTerminal, Paste,
    Rename, RevealInFileManager, Trash,
};
use anyhow::{Context as _, Result};
use collections::HashMap;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    Action as _, AnyElement, App, Context, DismissEvent, Entity, FocusHandle, Focusable,
    FontWeight, Global, MouseButton, Point, PromptLevel, Subscription, Task, WeakEntity, Window,
    actions, anchored, deferred,
};
use herdr::{AgentStatus, HerdState};
use menu::{Confirm, SelectNext, SelectPrevious};
use project::Project;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use task::{RevealStrategy, RevealTarget, SpawnInTerminal};
use terminal_view::terminal_panel::TerminalPanel;
use ui::{ContextMenu, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::{ResultExt as _, paths::home_dir};
use workspace::Workspace;

actions!(
    project_panel,
    [
        /// Runs `git worktree prune`, then removes the worktrees that are unlocked, clean and
        /// merged into the main checkout's HEAD after confirming the list.
        PruneStaleWorktrees
    ]
);

/// How long the dirty/clean answer of a worktree is trusted.
pub const STATUS_CACHE_TTL: Duration = Duration::from_secs(30);
/// Changes under `.git/worktrees` are coalesced for this long before the list is re-read.
pub const REFRESH_DEBOUNCE: Duration = Duration::from_millis(500);
/// A worktree still at the main checkout's HEAD is only pruned once nothing has touched it
/// for this long; one an agent just created is clean and "merged" but not stale.
pub const MIN_PRUNE_AGE: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    /// `Some` when locked; the string is the reason, empty when none was given.
    pub locked: Option<String>,
    pub prunable: bool,
}

/// Every stanza of `git worktree list --porcelain`, main checkout first, bare entries skipped.
pub fn parse_porcelain(text: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    for stanza in text.split("\n\n") {
        let mut path = None;
        let mut head = String::new();
        let mut branch = None;
        let mut locked = None;
        let mut prunable = false;
        let mut bare = false;
        for line in stanza.lines() {
            let (key, value) = line
                .split_once(' ')
                .map_or((line, ""), |(key, value)| (key, value));
            match key {
                "worktree" => path = Some(PathBuf::from(value)),
                "HEAD" => head = value.to_string(),
                "branch" => {
                    branch = Some(
                        value
                            .strip_prefix("refs/heads/")
                            .unwrap_or(value)
                            .to_string(),
                    )
                }
                "detached" => branch = None,
                "locked" => locked = Some(value.to_string()),
                "prunable" => prunable = true,
                "bare" => bare = true,
                _ => {}
            }
        }
        if let Some(path) = path
            && !bare
        {
            entries.push(WorktreeEntry {
                path,
                head,
                branch,
                locked,
                prunable,
            });
        }
    }
    entries
}

/// Drops the main checkout, which git always lists first.
pub fn linked_only(mut entries: Vec<WorktreeEntry>) -> Vec<WorktreeEntry> {
    if !entries.is_empty() {
        entries.remove(0);
    }
    entries
}

pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The short agent id for a path under `.claude/worktrees/`; `agent-<id>` loses its prefix
/// and ids are trimmed to seven characters.
pub fn agent_tag(path: &Path) -> Option<String> {
    let mut components = path.components().map(|component| component.as_os_str());
    while let Some(component) = components.next() {
        if component == ".claude"
            && components.next() == Some(std::ffi::OsStr::new("worktrees"))
            && let Some(name) = components.next()
        {
            let name = name.to_string_lossy();
            let id = name.strip_prefix("agent-").unwrap_or(&name);
            return Some(id.chars().take(7).collect());
        }
    }
    None
}

/// What is known about one worktree beyond the porcelain line, gathered at prune time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorktreeFacts {
    pub dirty: Option<bool>,
    pub merged: Option<bool>,
    /// HEAD read right now; the listed one is from the last refresh and may be stale.
    pub head: Option<String>,
    /// Time since the worktree was last touched: its HEAD commit or its directory,
    /// whichever is more recent.
    pub age: Option<Duration>,
}

/// Worktrees that are unlocked, known clean, known merged, and either moved past the main
/// checkout's HEAD or left alone for [`MIN_PRUNE_AGE`]. The returned entries carry the
/// freshly read HEAD. Prunable ones (whose directory is gone) are left to `git worktree
/// prune`.
pub fn prune_candidates(
    entries: &[WorktreeEntry],
    facts: &HashMap<PathBuf, WorktreeFacts>,
    main_head: &str,
) -> Vec<WorktreeEntry> {
    entries
        .iter()
        .filter(|entry| entry.locked.is_none() && !entry.prunable)
        .filter_map(|entry| {
            let facts = facts.get(&entry.path)?;
            if facts.dirty != Some(false) || facts.merged != Some(true) {
                return None;
            }
            let head = facts.head.clone()?;
            let untouched = head == main_head && !facts.age.is_some_and(|age| age > MIN_PRUNE_AGE);
            if untouched {
                return None;
            }
            Some(WorktreeEntry {
                head,
                ..entry.clone()
            })
        })
        .collect()
}

/// Herdr reports worktree paths as it saw them (`/tmp/x`) while ours are canonical
/// (`/private/tmp/x`); `canonical_herdr_paths` maps the canonical form back to herdr's.
pub fn herdr_path_for<'a>(
    path: &Path,
    canonical_herdr_paths: &'a HashMap<PathBuf, String>,
) -> Option<&'a str> {
    canonical_herdr_paths.get(path).map(String::as_str)
}

/// A canonical worktree path may only be deleted when it lives under the repository's parent
/// directory or under the user's home, and is neither of those directories itself.
pub fn delete_allowed(path: &Path, repository_parent: &Path, home: &Path) -> bool {
    if path == repository_parent || path == home {
        return false;
    }
    path.starts_with(repository_parent) || path.starts_with(home)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    fn stdout_or_error(self, what: &str) -> Result<String> {
        anyhow::ensure!(self.success, "{what} failed: {}", self.stderr.trim());
        Ok(self.stdout)
    }
}

/// Runs git as argv with a working directory; never through a shell.
pub trait GitRunner: Send + Sync {
    fn run(&self, cwd: &Path, args: &[&str], cx: &App) -> Task<Result<GitOutput>>;
}

pub struct SystemGit;

impl GitRunner for SystemGit {
    fn run(&self, cwd: &Path, args: &[&str], cx: &App) -> Task<Result<GitOutput>> {
        let cwd = cwd.to_path_buf();
        let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        cx.background_spawn(async move {
            let output = util::command::new_command("git")
                .args(&args)
                .current_dir(&cwd)
                .output()
                .await
                .with_context(|| format!("running git {} in {}", args.join(" "), cwd.display()))?;
            Ok(GitOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        })
    }
}

/// Tests install a fake runner here before the panel is built.
pub struct GitRunnerOverride(pub Arc<dyn GitRunner>);
impl Global for GitRunnerOverride {}

fn git_runner(cx: &App) -> Arc<dyn GitRunner> {
    match cx.try_global::<GitRunnerOverride>() {
        Some(runner) => runner.0.clone(),
        None => Arc::new(SystemGit),
    }
}

struct StatusCache {
    /// `None` when `git status` failed; the failure is cached for the TTL too so a broken
    /// worktree does not spawn git on every frame.
    dirty: Option<bool>,
    fetched_at: Instant,
}

struct DeployedMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    _subscription: Subscription,
}

pub struct Worktrees {
    root: PathBuf,
    fs: Arc<dyn Fs>,
    git: Arc<dyn GitRunner>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    entries: Vec<WorktreeEntry>,
    head_times: HashMap<PathBuf, SystemTime>,
    status: HashMap<PathBuf, StatusCache>,
    status_tasks: HashMap<PathBuf, Task<()>>,
    expanded: bool,
    selected: Option<usize>,
    error: Option<String>,
    /// Kept apart from `error`, which every refresh clears, until the next user action.
    prune_error: Option<String>,
    /// Canonical worktree path to the path herdr reported for it.
    herdr_paths: HashMap<PathBuf, String>,
    herdr_paths_task: Task<()>,
    context_menu: Option<DeployedMenu>,
    refresh_task: Task<()>,
    _watch_task: Task<()>,
    prune_task: Task<()>,
    /// Number of completed list reads; lets tests check the watcher fired.
    refreshes: Arc<AtomicUsize>,
    _subscriptions: Vec<Subscription>,
}

impl Worktrees {
    pub fn new(
        root: PathBuf,
        fs: Arc<dyn Fs>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();
        let herd = HerdState::try_global(cx);
        if let Some(herd) = &herd {
            subscriptions.push(cx.observe(herd, |this, herd, cx| {
                this.refresh_herdr_paths(Self::herd_worktree_paths(&herd, cx), cx);
                cx.notify();
            }));
        }
        let mut this = Self {
            root,
            fs,
            git: git_runner(cx),
            project,
            workspace,
            focus_handle: cx.focus_handle(),
            entries: Vec::new(),
            head_times: HashMap::default(),
            status: HashMap::default(),
            status_tasks: HashMap::default(),
            expanded: true,
            selected: None,
            error: None,
            prune_error: None,
            herdr_paths: HashMap::default(),
            herdr_paths_task: Task::ready(()),
            context_menu: None,
            refresh_task: Task::ready(()),
            _watch_task: Task::ready(()),
            prune_task: Task::ready(()),
            refreshes: Arc::new(AtomicUsize::new(0)),
            _subscriptions: subscriptions,
        };
        this._watch_task = this.start_watching(cx);
        this.refresh_task = this.refresh(cx);
        if let Some(herd) = herd {
            this.refresh_herdr_paths(Self::herd_worktree_paths(&herd, cx), cx);
        }
        this
    }

    fn herd_worktree_paths(herd: &Entity<HerdState>, cx: &App) -> Vec<String> {
        herd.read(cx)
            .workspaces
            .iter()
            .filter_map(|workspace| workspace.worktree_path.clone())
            .collect()
    }

    pub fn entries(&self) -> &[WorktreeEntry] {
        &self.entries
    }

    pub fn refresh_count(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }

    pub fn has_selection(&self) -> bool {
        self.selected.is_some()
    }

    /// Canonicalizes the paths herdr reported so they compare equal to the canonical
    /// paths in `entries` (`/tmp` versus `/private/tmp`).
    pub fn refresh_herdr_paths(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        self.herdr_paths_task = cx.spawn(async move |this, cx| {
            let mut canonical = HashMap::default();
            for path in paths {
                let resolved = fs
                    .canonicalize(Path::new(&path))
                    .await
                    .unwrap_or_else(|_| PathBuf::from(&path));
                canonical.insert(resolved, path);
            }
            this.update(cx, |this, cx| {
                if this.herdr_paths != canonical {
                    this.herdr_paths = canonical;
                    cx.notify();
                }
            })
            .ok();
        });
    }

    fn clear_prune_error(&mut self, cx: &mut Context<Self>) {
        if self.prune_error.take().is_some() {
            cx.notify();
        }
    }

    /// Re-reads the list after `REFRESH_DEBOUNCE`; repeated calls restart the wait.
    pub fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            let Ok(task) = this.update(cx, |this, cx| this.refresh(cx)) else {
                return;
            };
            task.await;
        });
    }

    fn common_dir(&self, cx: &App) -> Task<Result<PathBuf>> {
        let root = self.root.clone();
        let git = self.git.clone();
        let fs = self.fs.clone();
        let output = git.run(&root, &["rev-parse", "--git-common-dir"], cx);
        cx.background_spawn(async move {
            let stdout = output.await?.stdout_or_error("git rev-parse")?;
            let common = PathBuf::from(stdout.trim());
            let common = if common.is_absolute() {
                common
            } else {
                root.join(common)
            };
            Ok(fs.canonicalize(&common).await.unwrap_or(common))
        })
    }

    /// Whether `root` is a git checkout at all; nothing runs git otherwise, so projects
    /// without a `.git` (and every test that builds a panel on a FakeFs) never spawn it.
    async fn is_repository(fs: &Arc<dyn Fs>, root: &Path) -> bool {
        fs.metadata(&root.join(".git"))
            .await
            .ok()
            .flatten()
            .is_some()
    }

    fn start_watching(&self, cx: &mut Context<Self>) -> Task<()> {
        let root = self.root.clone();
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            if !Self::is_repository(&fs, &root).await {
                return;
            }
            let Ok(common_dir) = this.update(cx, |this, cx| this.common_dir(cx)) else {
                return;
            };
            let Some(common_dir) = common_dir.await.log_err() else {
                return;
            };
            let worktrees_dir = common_dir.join("worktrees");
            // The whole common dir is watched because `worktrees/` does not exist until the
            // first linked worktree is added; events are filtered to that subtree.
            let (mut events, watcher) = fs.watch(&common_dir, REFRESH_DEBOUNCE).await;
            let _watcher = watcher;
            while let Some(batch) = events.next().await {
                if !batch
                    .iter()
                    .any(|event| event.path.starts_with(&worktrees_dir))
                {
                    continue;
                }
                if this
                    .update(cx, |this, cx| this.schedule_refresh(cx))
                    .is_err()
                {
                    return;
                }
            }
        })
    }

    fn refresh(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let root = self.root.clone();
        let git = self.git.clone();
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            if !Self::is_repository(&fs, &root).await {
                return;
            }
            let list = cx.update(|cx| git.run(&root, &["worktree", "list", "--porcelain"], cx));
            let listed = async {
                let stdout = list.await?.stdout_or_error("git worktree list")?;
                let mut entries = linked_only(parse_porcelain(&stdout));
                for entry in &mut entries {
                    if let Ok(canonical) = fs.canonicalize(&entry.path).await {
                        entry.path = canonical;
                    }
                }
                anyhow::Ok(entries)
            }
            .await;
            let entries = match listed {
                Ok(entries) => entries,
                Err(error) => {
                    // A root that git cannot list (not a repository, git missing) has no
                    // worktrees to show; the group stays hidden rather than showing an error.
                    log::warn!("worktrees: {error:#}");
                    this.update(cx, |this, cx| {
                        if !this.entries.is_empty() {
                            this.entries.clear();
                            cx.notify();
                        }
                    })
                    .ok();
                    return;
                }
            };
            let mut head_times = HashMap::default();
            for entry in &entries {
                if entry.prunable {
                    continue;
                }
                let task = cx
                    .update(|cx| git.run(&entry.path, &["log", "-1", "--format=%ct", "HEAD"], cx));
                if let Some(seconds) = task
                    .await
                    .ok()
                    .filter(|output| output.success)
                    .and_then(|output| output.stdout.trim().parse::<u64>().ok())
                {
                    head_times.insert(
                        entry.path.clone(),
                        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
                    );
                }
            }
            this.update(cx, |this, cx| {
                this.error = None;
                this.status
                    .retain(|path, _| entries.iter().any(|entry| &entry.path == path));
                if this
                    .selected
                    .is_some_and(|selected| selected >= entries.len())
                {
                    this.selected = entries.last().map(|_| entries.len() - 1);
                }
                this.entries = entries;
                this.head_times = head_times;
                this.refreshes.fetch_add(1, Ordering::SeqCst);
                cx.notify();
            })
            .ok();
        })
    }

    /// The cached dirty flag, starting a `git status` when it is missing or older than
    /// `STATUS_CACHE_TTL`.
    fn dirty_for(&mut self, path: &Path, cx: &mut Context<Self>) -> Option<bool> {
        let now = cx.background_executor().now();
        if let Some(cached) = self.status.get(path)
            && now.duration_since(cached.fetched_at) < STATUS_CACHE_TTL
        {
            return cached.dirty;
        }
        if !self.status_tasks.contains_key(path) {
            let output = self.git.run(path, &["status", "--porcelain"], cx);
            let task_path = path.to_path_buf();
            let task = cx.spawn(async move |this, cx| {
                let path = task_path;
                let dirty = match output.await {
                    Ok(output) if output.success => Some(!output.stdout.trim().is_empty()),
                    Ok(output) => {
                        log::warn!("git status in {path:?} failed: {}", output.stderr.trim());
                        None
                    }
                    Err(error) => {
                        log::warn!("git status in {path:?} failed: {error:#}");
                        None
                    }
                };
                this.update(cx, |this, cx| {
                    this.status_tasks.remove(&path);
                    let fetched_at = cx.background_executor().now();
                    this.status.insert(path, StatusCache { dirty, fetched_at });
                    cx.notify();
                })
                .ok();
            });
            self.status_tasks.insert(path.to_path_buf(), task);
        }
        self.status.get(path).and_then(|cached| cached.dirty)
    }

    fn selected_entry(&self) -> Option<&WorktreeEntry> {
        self.entries.get(self.selected?)
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_prune_error(cx);
        if index < self.entries.len() {
            self.selected = Some(index);
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.selected.map_or(0, |selected| selected + 1);
        self.select(next, window, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        let previous = self.selected.and_then(|selected| selected.checked_sub(1));
        if let Some(previous) = previous {
            self.select(previous, window, cx);
        }
    }

    fn toggle_expanded(&mut self, cx: &mut Context<Self>) {
        self.clear_prune_error(cx);
        self.expanded = !self.expanded;
        cx.notify();
    }

    /// Adds the selected worktree as a folder of the current workspace.
    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_prune_error(cx);
        let Some(path) = self.selected_entry().map(|entry| entry.path.clone()) else {
            return;
        };
        self.add_to_workspace(path, cx);
    }

    fn add_to_workspace(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let task = self.project.update(cx, |project, cx| {
            project.find_or_create_worktree(path, true, cx)
        });
        cx.spawn(async move |_, _| task.await.map(|_| ()))
            .detach_and_log_err(cx);
    }

    fn open_in_terminal(
        &mut self,
        _: &OpenInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_prune_error(cx);
        let Some(path) = self.selected_entry().map(|entry| entry.path.clone()) else {
            return;
        };
        self.open_terminal_at(path, window, cx);
    }

    fn open_terminal_at(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(terminal_panel) = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).panel::<TerminalPanel>(cx))
        else {
            log::warn!("cannot open a center terminal: the terminal panel is not available");
            return;
        };
        let label = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let spawn = SpawnInTerminal {
            full_label: label.clone(),
            label,
            cwd: Some(path),
            use_new_terminal: true,
            reveal_target: RevealTarget::Center,
            ..Default::default()
        };
        terminal_panel
            .update(cx, |terminal_panel, cx| {
                terminal_panel.add_terminal_task(spawn, RevealStrategy::Always, window, cx)
            })
            .detach_and_log_err(cx);
    }

    fn reveal_in_finder(
        &mut self,
        _: &RevealInFileManager,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_prune_error(cx);
        if let Some(path) = self.selected_entry().map(|entry| entry.path.clone()) {
            self.project
                .update(cx, |project, cx| project.reveal_path(&path, cx));
        }
    }

    fn prune_stale(
        &mut self,
        _: &PruneStaleWorktrees,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_prune_error(cx);
        let root = self.root.clone();
        let git = self.git.clone();
        let fs = self.fs.clone();
        let entries = self.entries.clone();
        self.prune_task = cx.spawn_in(window, async move |this, cx| {
            let gathered = async {
                let main_head = cx
                    .update(|_, cx| git.run(&root, &["rev-parse", "HEAD"], cx))?
                    .await?
                    .stdout_or_error("git rev-parse")?
                    .trim()
                    .to_string();
                let mut facts: HashMap<PathBuf, WorktreeFacts> = HashMap::default();
                for entry in entries.iter().filter(|entry| !entry.prunable) {
                    let status =
                        cx.update(|_, cx| git.run(&entry.path, &["status", "--porcelain"], cx))?;
                    // The listed HEAD is from the last refresh; a commit made since would
                    // be lost if it were trusted here.
                    let head = cx
                        .update(|_, cx| git.run(&entry.path, &["rev-parse", "HEAD"], cx))?
                        .await?
                        .stdout_or_error("git rev-parse")?
                        .trim()
                        .to_string();
                    let merged = cx.update(|_, cx| {
                        git.run(
                            &root,
                            &["merge-base", "--is-ancestor", &head, "HEAD"],
                            cx,
                        )
                    })?;
                    let commit_time = cx.update(|_, cx| {
                        git.run(&entry.path, &["log", "-1", "--format=%ct", "HEAD"], cx)
                    })?;
                    let dirty = !status
                        .await?
                        .stdout_or_error("git status")?
                        .trim()
                        .is_empty();
                    let merged = merged.await?.success;
                    let now = SystemTime::now();
                    let commit_age = commit_time
                        .await
                        .ok()
                        .filter(|output| output.success)
                        .and_then(|output| output.stdout.trim().parse::<u64>().ok())
                        .and_then(|seconds| {
                            now.duration_since(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
                                .ok()
                        });
                    let directory_age = fs
                        .metadata(&entry.path)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|metadata| {
                            now.duration_since(metadata.mtime.timestamp_for_user()).ok()
                        });
                    let age = match (commit_age, directory_age) {
                        (Some(commit_age), Some(directory_age)) => {
                            Some(commit_age.min(directory_age))
                        }
                        (commit_age, directory_age) => commit_age.or(directory_age),
                    };
                    facts.insert(
                        entry.path.clone(),
                        WorktreeFacts {
                            dirty: Some(dirty),
                            merged: Some(merged),
                            head: Some(head),
                            age,
                        },
                    );
                }
                anyhow::Ok(prune_candidates(&entries, &facts, &main_head))
            }
            .await;
            let candidates = match gathered {
                Ok(candidates) => candidates,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.prune_error = Some(error.to_string());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let detail = if candidates.is_empty() {
                "No worktree is unlocked, clean, merged and older than ten minutes. Only `git worktree prune` will run."
                    .to_string()
            } else {
                candidates
                    .iter()
                    .map(|entry| {
                        format!(
                            "{} ({}, {})",
                            entry.path.display(),
                            entry.branch.as_deref().unwrap_or("detached"),
                            short_sha(&entry.head)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let message = format!("Prune {} stale worktrees?", candidates.len());
            let Ok(answer) = cx.update(|window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    &message,
                    Some(&detail),
                    &["Prune", "Cancel"],
                    cx,
                )
            }) else {
                return;
            };
            if answer.await != Ok(0) {
                return;
            }
            let pruned = async {
                cx.update(|_, cx| git.run(&root, &["worktree", "prune"], cx))?
                    .await?
                    .stdout_or_error("git worktree prune")?;
                let repository_parent = fs
                    .canonicalize(&root)
                    .await
                    .unwrap_or_else(|_| root.clone())
                    .parent()
                    .map(Path::to_path_buf)
                    .context("repository has no parent directory")?;
                for entry in &candidates {
                    let path = fs.canonicalize(&entry.path).await?;
                    anyhow::ensure!(
                        delete_allowed(&path, &repository_parent, home_dir()),
                        "refusing to delete {} outside the repository's parent or home",
                        path.display()
                    );
                    let path_argument = path.to_string_lossy().into_owned();
                    cx.update(|_, cx| {
                        git.run(&root, &["worktree", "remove", "--", &path_argument], cx)
                    })?
                    .await?
                    .stdout_or_error("git worktree remove")?;
                }
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                if let Err(error) = pruned {
                    this.prune_error = Some(error.to_string());
                    cx.notify();
                }
                this.schedule_refresh(cx);
            })
            .ok();
        });
    }

    fn deploy_context_menu(
        &mut self,
        index: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select(index, window, cx);
        let Some(path) = self.selected_entry().map(|entry| entry.path.clone()) else {
            return;
        };
        let this = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, |menu, _, _| {
            menu.context(self.focus_handle.clone())
                .entry("Add to Workspace", Some(Confirm.boxed_clone()), {
                    let this = this.clone();
                    let path = path.clone();
                    move |_, cx| {
                        this.update(cx, |this, cx| this.add_to_workspace(path.clone(), cx))
                            .ok();
                    }
                })
                .entry("Open in Terminal", Some(OpenInTerminal.boxed_clone()), {
                    let this = this.clone();
                    let path = path.clone();
                    move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.open_terminal_at(path.clone(), window, cx)
                        })
                        .ok();
                    }
                })
                .action("Reveal in Finder", RevealInFileManager.boxed_clone())
                .separator()
                .action("Prune Stale Worktrees", PruneStaleWorktrees.boxed_clone())
        });
        window.focus(&menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some(DeployedMenu {
            menu,
            position,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn agent_label(&self, path: &Path, cx: &App) -> Option<(String, Option<AgentStatus>)> {
        let tag = agent_tag(path)?;
        let herdr_path = herdr_path_for(path, &self.herdr_paths).map(Path::new);
        let known = HerdState::try_global(cx).and_then(|herd| {
            let herd = herd.read(cx);
            herdr_path
                .and_then(|herdr_path| herd.agent_for_path(herdr_path))
                .or_else(|| herd.agent_for_path(path))
        });
        Some(match known {
            Some((name, status)) => (name, Some(status)),
            None => (format!("agent {tag}"), None),
        })
    }

    fn render_row(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entry = self.entries[index].clone();
        let dirty = if entry.prunable {
            None
        } else {
            self.dirty_for(&entry.path, cx)
        };
        let now = SystemTime::now();
        let age = self
            .head_times
            .get(&entry.path)
            .and_then(|head_time| now.duration_since(*head_time).ok())
            .map(format_age);
        let name = entry
            .branch
            .clone()
            .unwrap_or_else(|| short_sha(&entry.head).to_string());
        let mut details = vec![short_sha(&entry.head).to_string()];
        details.extend(age);
        if entry.prunable {
            details.push("prunable".to_string());
        } else if let Some(dirty) = dirty {
            details.push(if dirty { "dirty" } else { "clean" }.to_string());
        }
        let agent = self.agent_label(&entry.path, cx);
        let is_selected = self.selected == Some(index) && self.focus_handle.is_focused(window);
        let tooltip = entry.path.to_string_lossy().into_owned();
        let path_for_click = entry.path.clone();
        ListItem::new(("worktree", index))
            .spacing(ListItemSpacing::Dense)
            .indent_level(1)
            .toggle_state(is_selected)
            .tooltip(Tooltip::text(tooltip))
            .start_slot(
                Icon::new(IconName::GitBranch)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_1()
                    .min_w_0()
                    .child(Label::new(name).single_line())
                    .when_some(entry.locked.clone(), |this, reason| {
                        let reason = if reason.is_empty() {
                            "Locked".to_string()
                        } else {
                            format!("Locked: {reason}")
                        };
                        this.child(
                            div()
                                .id("worktree-lock")
                                .tooltip(Tooltip::text(reason))
                                .child(
                                    Icon::new(IconName::Lock)
                                        .size(IconSize::XSmall)
                                        .color(Color::Warning),
                                ),
                        )
                    })
                    .when_some(agent, |this, (label, status)| {
                        let color = match status {
                            Some(AgentStatus::Working) => Color::Accent,
                            Some(AgentStatus::Blocked) => Color::Warning,
                            Some(AgentStatus::Done) => Color::Success,
                            _ => Color::Muted,
                        };
                        let label = match status {
                            Some(status) => format!("{label} · {status:?}").to_lowercase(),
                            None => label,
                        };
                        this.child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .bg(cx.theme().colors().element_background)
                                .child(Label::new(label).size(LabelSize::XSmall).color(color)),
                        )
                    }),
            )
            .end_slot(
                Label::new(details.join(" · "))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .single_line(),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select(index, window, cx);
                this.add_to_workspace(path_for_click.clone(), cx);
            }))
            .on_secondary_mouse_down(cx.listener(
                move |this, event: &gpui::MouseDownEvent, window, cx| {
                    this.deploy_context_menu(index, event.position, window, cx);
                },
            ))
            .into_any_element()
    }
}

impl Focusable for Worktrees {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Worktrees {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.entries.is_empty() && self.error.is_none() && self.prune_error.is_none() {
            return div().into_any_element();
        }
        let header = ListItem::new("grove-group-Worktrees")
            .spacing(ListItemSpacing::Dense)
            .toggle(self.expanded)
            .on_toggle(cx.listener(|this, _, _, cx| this.toggle_expanded(cx)))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_expanded(cx)))
            .child(
                Label::new("Worktrees")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .weight(FontWeight::SEMIBOLD),
            )
            .end_slot(
                Label::new(self.entries.len().to_string())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        let mut container = v_flex()
            .id("worktrees-group")
            .w_full()
            .key_context("Worktrees")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::open_in_terminal))
            .on_action(cx.listener(Self::reveal_in_finder))
            .on_action(cx.listener(Self::prune_stale))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            // Bindings of the enclosing ProjectPanel context still resolve while a worktree
            // row is focused; swallowing the file actions here keeps them from reaching
            // the panel, where they would act on its hidden file selection.
            .on_action(|_: &Rename, _, _| {})
            .on_action(|_: &Trash, _, _| {})
            .on_action(|_: &Delete, _, _| {})
            .on_action(|_: &Cut, _, _| {})
            .on_action(|_: &CopyEntry, _, _| {})
            .on_action(|_: &Paste, _, _| {})
            .on_action(|_: &Duplicate, _, _| {})
            .on_action(|_: &NewFile, _, _| {})
            .on_action(|_: &NewDirectory, _, _| {})
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus_handle, cx)),
            )
            .child(header);
        if self.expanded {
            for index in 0..self.entries.len() {
                container = container.child(self.render_row(index, window, cx));
            }
            for error in self.error.iter().chain(self.prune_error.iter()) {
                container = container.child(
                    Label::new(error.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Error)
                        .ml_4(),
                );
            }
        }
        container
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .children(self.context_menu.as_ref().map(|menu| {
                deferred(anchored().position(menu.position).child(menu.menu.clone()))
                    .with_priority(3)
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORCELAIN: &str = "\
worktree /repo
HEAD 1111111aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
branch refs/heads/main

worktree /repo/.claude/worktrees/agent-aad806ed1051ed32d
HEAD 2222222bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
branch refs/heads/feat/worktrees

worktree /tmp/detached
HEAD 3333333cccccccccccccccccccccccccccccccc
detached
locked

worktree /tmp/locked-with-reason
HEAD 4444444dddddddddddddddddddddddddddddddd
branch refs/heads/hold
locked being reviewed

worktree /tmp/gone
HEAD 5555555eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
branch refs/heads/old
prunable gitdir file points to non-existent location
";

    #[test]
    fn porcelain_parses_branch_detached_locked_and_prunable() {
        let entries = parse_porcelain(PORCELAIN);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].path, PathBuf::from("/repo"));
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[0].locked, None);
        assert!(!entries[0].prunable);

        assert_eq!(entries[1].branch.as_deref(), Some("feat/worktrees"));
        assert_eq!(short_sha(&entries[1].head), "2222222");

        assert_eq!(entries[2].branch, None);
        assert_eq!(entries[2].locked.as_deref(), Some(""));

        assert_eq!(entries[3].locked.as_deref(), Some("being reviewed"));
        assert!(!entries[3].prunable);

        assert!(entries[4].prunable);
        assert_eq!(entries[4].branch.as_deref(), Some("old"));
    }

    #[test]
    fn porcelain_skips_bare_and_linked_only_drops_the_main_checkout() {
        let text = "worktree /bare.git\nHEAD 0000000\nbare\n\nworktree /a\nHEAD 1111111\nbranch refs/heads/a\n\nworktree /b\nHEAD 2222222\ndetached\n";
        let entries = parse_porcelain(text);
        assert_eq!(entries.len(), 2);
        let linked = linked_only(entries);
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].path, PathBuf::from("/b"));
        assert!(linked_only(Vec::new()).is_empty());
        assert!(parse_porcelain("").is_empty());
    }

    #[test]
    fn agent_tag_detects_claude_worktree_paths() {
        assert_eq!(
            agent_tag(Path::new("/repo/.claude/worktrees/agent-aad806ed1051ed32d")).as_deref(),
            Some("aad806e")
        );
        assert_eq!(
            agent_tag(Path::new("/repo/.claude/worktrees/short")).as_deref(),
            Some("short")
        );
        assert_eq!(agent_tag(Path::new("/repo/.claude/skills/x")), None);
        assert_eq!(agent_tag(Path::new("/repo/worktrees/agent-1")), None);
        assert_eq!(agent_tag(Path::new("/repo/.claude/worktrees")), None);
    }

    fn entry(path: &str, locked: Option<&str>, prunable: bool) -> WorktreeEntry {
        WorktreeEntry {
            path: PathBuf::from(path),
            head: "abcdef0123".into(),
            branch: Some("b".into()),
            locked: locked.map(str::to_string),
            prunable,
        }
    }

    const MAIN_HEAD: &str = "1111111aaaaaaa";

    fn facts(dirty: Option<bool>, merged: Option<bool>) -> WorktreeFacts {
        WorktreeFacts {
            dirty,
            merged,
            head: Some("2222222bbbbbbb".into()),
            age: Some(Duration::from_secs(3600)),
        }
    }

    #[test]
    fn prune_candidates_require_unlocked_clean_and_merged() {
        let entries = vec![
            entry("/clean-merged", None, false),
            entry("/dirty-merged", None, false),
            entry("/clean-unmerged", None, false),
            entry("/locked", Some("why"), false),
            entry("/unknown", None, false),
            entry("/gone", None, true),
        ];
        let mut facts_by_path = HashMap::default();
        facts_by_path.insert(
            PathBuf::from("/clean-merged"),
            facts(Some(false), Some(true)),
        );
        facts_by_path.insert(
            PathBuf::from("/dirty-merged"),
            facts(Some(true), Some(true)),
        );
        facts_by_path.insert(
            PathBuf::from("/clean-unmerged"),
            facts(Some(false), Some(false)),
        );
        facts_by_path.insert(PathBuf::from("/locked"), facts(Some(false), Some(true)));
        facts_by_path.insert(PathBuf::from("/unknown"), facts(None, Some(true)));
        facts_by_path.insert(PathBuf::from("/gone"), facts(Some(false), Some(true)));
        let candidates = prune_candidates(&entries, &facts_by_path, MAIN_HEAD);
        let paths = candidates
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![PathBuf::from("/clean-merged")]);
        assert_eq!(
            candidates[0].head, "2222222bbbbbbb",
            "the candidate carries the HEAD read at prune time"
        );
    }

    #[test]
    fn prune_candidates_skip_fresh_worktrees_still_at_the_main_head() {
        let entries = vec![
            entry("/fresh", None, false),
            entry("/abandoned", None, false),
            entry("/unknown-age", None, false),
            entry("/no-head", None, false),
        ];
        let mut facts_by_path = HashMap::default();
        facts_by_path.insert(
            PathBuf::from("/fresh"),
            WorktreeFacts {
                head: Some(MAIN_HEAD.into()),
                age: Some(Duration::from_secs(30)),
                ..facts(Some(false), Some(true))
            },
        );
        facts_by_path.insert(
            PathBuf::from("/abandoned"),
            WorktreeFacts {
                head: Some(MAIN_HEAD.into()),
                age: Some(MIN_PRUNE_AGE + Duration::from_secs(1)),
                ..facts(Some(false), Some(true))
            },
        );
        facts_by_path.insert(
            PathBuf::from("/unknown-age"),
            WorktreeFacts {
                head: Some(MAIN_HEAD.into()),
                age: None,
                ..facts(Some(false), Some(true))
            },
        );
        facts_by_path.insert(
            PathBuf::from("/no-head"),
            WorktreeFacts {
                head: None,
                ..facts(Some(false), Some(true))
            },
        );
        let candidates = prune_candidates(&entries, &facts_by_path, MAIN_HEAD);
        let paths = candidates
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![PathBuf::from("/abandoned")]);
    }

    #[test]
    fn herdr_paths_match_after_canonicalizing() {
        let mut canonical = HashMap::default();
        canonical.insert(PathBuf::from("/private/tmp/wt"), "/tmp/wt".to_string());
        assert_eq!(
            herdr_path_for(Path::new("/private/tmp/wt"), &canonical),
            Some("/tmp/wt")
        );
        assert_eq!(herdr_path_for(Path::new("/tmp/wt"), &canonical), None);
    }

    struct FakeGit {
        porcelain: std::sync::Mutex<String>,
        list_calls: AtomicUsize,
        status_calls: AtomicUsize,
        status_fails: std::sync::atomic::AtomicBool,
        prune_fails: std::sync::atomic::AtomicBool,
        /// `rev-parse HEAD` answer per working directory; `MAIN_HEAD` otherwise.
        heads: std::sync::Mutex<HashMap<PathBuf, String>>,
        /// HEAD commit time per working directory; an old fixed time otherwise.
        commit_times: std::sync::Mutex<HashMap<PathBuf, u64>>,
        calls: std::sync::Mutex<Vec<(PathBuf, Vec<String>)>>,
    }

    impl FakeGit {
        fn new(porcelain: &str) -> Arc<Self> {
            Arc::new(Self {
                porcelain: std::sync::Mutex::new(porcelain.to_string()),
                list_calls: AtomicUsize::new(0),
                status_calls: AtomicUsize::new(0),
                status_fails: std::sync::atomic::AtomicBool::new(false),
                prune_fails: std::sync::atomic::AtomicBool::new(false),
                heads: std::sync::Mutex::new(HashMap::default()),
                commit_times: std::sync::Mutex::new(HashMap::default()),
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<(PathBuf, Vec<String>)> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    impl GitRunner for FakeGit {
        fn run(&self, cwd: &Path, args: &[&str], _cx: &App) -> Task<Result<GitOutput>> {
            self.calls.lock().expect("calls lock").push((
                cwd.to_path_buf(),
                args.iter().map(|arg| arg.to_string()).collect(),
            ));
            let mut success = true;
            let stdout = match args {
                ["rev-parse", "--git-common-dir"] => ".git\n".to_string(),
                ["rev-parse", "HEAD"] => {
                    let heads = self.heads.lock().expect("heads lock");
                    format!("{}\n", heads.get(cwd).map_or(MAIN_HEAD, String::as_str))
                }
                ["worktree", "list", "--porcelain"] => {
                    self.list_calls.fetch_add(1, Ordering::SeqCst);
                    self.porcelain.lock().expect("porcelain lock").clone()
                }
                ["status", "--porcelain"] => {
                    self.status_calls.fetch_add(1, Ordering::SeqCst);
                    success = !self.status_fails.load(Ordering::SeqCst);
                    String::new()
                }
                ["worktree", "prune"] => {
                    success = !self.prune_fails.load(Ordering::SeqCst);
                    String::new()
                }
                ["log", ..] => {
                    let commit_times = self.commit_times.lock().expect("commit times lock");
                    format!("{}\n", commit_times.get(cwd).copied().unwrap_or(1_700_000_000))
                }
                _ => String::new(),
            };
            Task::ready(Ok(GitOutput {
                success,
                stdout,
                stderr: if success {
                    String::new()
                } else {
                    "fake git failure".to_string()
                },
            }))
        }
    }

    /// A panel over `/root` whose Worktrees group lists the worktrees in `porcelain`.
    async fn panel_with_worktrees(
        porcelain: &str,
        cx: &mut gpui::TestAppContext,
    ) -> (
        Arc<FakeGit>,
        Arc<fs::FakeFs>,
        Entity<crate::ProjectPanel>,
        Entity<Worktrees>,
        gpui::VisualTestContext,
    ) {
        use fs::FakeFs;
        use gpui::VisualTestContext;
        use serde_json::json;
        use util::path;
        use workspace::MultiWorkspace;

        crate::project_panel_tests::init_test(cx);
        let git = FakeGit::new(porcelain);
        cx.update(|cx| cx.set_global(GitRunnerOverride(git.clone())));

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                ".git": { "HEAD": "ref: refs/heads/main\n" },
                ".claude": { "worktrees": { "fresh": { "a.rs": "" }, "old": { "a.rs": "" } } },
                "src": { "main.rs": "" }
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("workspace");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            let panel = crate::ProjectPanel::new(workspace, window, cx);
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        cx.run_until_parked();
        let worktrees = panel
            .read_with(&cx, |panel, _| panel.worktrees.clone())
            .expect("the panel has a root folder");
        (git, fs, panel, worktrees, cx)
    }

    fn two_worktrees_porcelain() -> String {
        use util::path;
        format!(
            "worktree {root}\nHEAD {MAIN_HEAD}\nbranch refs/heads/main\n\nworktree {fresh}\nHEAD {MAIN_HEAD}\nbranch refs/heads/feat/fresh\n\nworktree {old}\nHEAD {MAIN_HEAD}\nbranch refs/heads/feat/old\n",
            root = path!("/root"),
            fresh = path!("/root/.claude/worktrees/fresh"),
            old = path!("/root/.claude/worktrees/old"),
        )
    }

    #[gpui::test]
    async fn prune_rereads_each_head_and_keeps_fresh_worktrees(cx: &mut gpui::TestAppContext) {
        use util::path;
        let (git, _fs, _panel, worktrees, mut cx) =
            panel_with_worktrees(&two_worktrees_porcelain(), cx).await;
        assert_eq!(
            worktrees.read_with(&cx, |worktrees, _| worktrees.entries().len()),
            2
        );
        // A commit made in `old` after the last refresh: the list still shows MAIN_HEAD.
        git.heads.lock().expect("heads lock").insert(
            PathBuf::from(path!("/root/.claude/worktrees/old")),
            "2222222bbbbbbb".to_string(),
        );
        // `fresh` was just created from main: clean, "merged", and at the main HEAD.
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs();
        git.commit_times
            .lock()
            .expect("commit times lock")
            .insert(PathBuf::from(path!("/root/.claude/worktrees/fresh")), now);

        worktrees.update_in(&mut cx, |worktrees, window, cx| {
            worktrees.prune_stale(&PruneStaleWorktrees, window, cx);
        });
        cx.run_until_parked();
        let (message, detail) = cx.pending_prompt().expect("prune asks first");
        assert_eq!(message, "Prune 1 stale worktrees?");
        assert!(detail.contains("old"), "{detail}");
        assert!(detail.contains("2222222"), "the prompt shows the re-read HEAD: {detail}");
        assert!(!detail.contains("fresh"), "a just-created worktree is not stale: {detail}");

        cx.simulate_prompt_answer("Prune");
        cx.run_until_parked();
        let removed: Vec<_> = git
            .calls()
            .into_iter()
            .filter(|(_, args)| args.starts_with(&["worktree".to_string(), "remove".to_string()]))
            .map(|(_, args)| args[3].clone())
            .collect();
        assert_eq!(removed, vec![path!("/root/.claude/worktrees/old").to_string()]);
        assert_eq!(
            worktrees.read_with(&cx, |worktrees, _| worktrees.prune_error.clone()),
            None
        );
    }

    #[gpui::test]
    async fn prune_error_outlives_the_refresh_until_the_next_action(
        cx: &mut gpui::TestAppContext,
    ) {
        let (git, _fs, _panel, worktrees, mut cx) =
            panel_with_worktrees(&two_worktrees_porcelain(), cx).await;
        git.prune_fails.store(true, Ordering::SeqCst);

        worktrees.update_in(&mut cx, |worktrees, window, cx| {
            worktrees.prune_stale(&PruneStaleWorktrees, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_prompt_answer("Prune");
        cx.run_until_parked();
        cx.executor().advance_clock(REFRESH_DEBOUNCE);
        cx.run_until_parked();
        let error = worktrees
            .read_with(&cx, |worktrees, _| worktrees.prune_error.clone())
            .expect("the failed prune is still shown after the refresh");
        assert!(error.contains("git worktree prune failed"), "{error}");

        worktrees.update_in(&mut cx, |worktrees, window, cx| {
            worktrees.select(0, window, cx);
        });
        assert_eq!(
            worktrees.read_with(&cx, |worktrees, _| worktrees.prune_error.clone()),
            None,
            "the next user action clears it"
        );
    }

    #[gpui::test]
    async fn failed_status_is_cached_for_the_ttl(cx: &mut gpui::TestAppContext) {
        use util::path;
        let (fake, _fs, _panel, worktrees, mut cx) =
            panel_with_worktrees(&two_worktrees_porcelain(), cx).await;
        let path = PathBuf::from(path!("/root/.claude/worktrees/old"));
        let status_calls_for_old = || {
            fake.calls()
                .iter()
                .filter(|(cwd, args)| *cwd == path && args[0] == "status")
                .count()
        };
        // Rendering the rows already cached a successful status; let it expire first.
        cx.executor().advance_clock(STATUS_CACHE_TTL);
        fake.status_fails.store(true, Ordering::SeqCst);
        let before = status_calls_for_old();
        // The expired answer is handed out once more while the refresh runs.
        worktrees.update(&mut cx, |worktrees, cx| {
            worktrees.dirty_for(&path, cx);
        });
        cx.run_until_parked();
        for _ in 0..3 {
            worktrees.update(&mut cx, |worktrees, cx| {
                assert_eq!(worktrees.dirty_for(&path, cx), None);
            });
            cx.run_until_parked();
        }
        assert_eq!(
            status_calls_for_old() - before,
            1,
            "a failed status is not retried before the TTL"
        );
        cx.executor().advance_clock(STATUS_CACHE_TTL);
        worktrees.update(&mut cx, |worktrees, cx| {
            worktrees.dirty_for(&path, cx);
        });
        cx.run_until_parked();
        assert_eq!(status_calls_for_old() - before, 2);
    }

    #[gpui::test]
    async fn herdr_paths_are_canonicalized(cx: &mut gpui::TestAppContext) {
        use util::path;
        let (_git, fs, _panel, worktrees, mut cx) =
            panel_with_worktrees(&two_worktrees_porcelain(), cx).await;
        fs.insert_symlink(path!("/tmp"), PathBuf::from(path!("/root/.claude/worktrees")))
            .await;
        worktrees.update(&mut cx, |worktrees, cx| {
            worktrees.refresh_herdr_paths(vec![path!("/tmp/old").to_string()], cx);
        });
        cx.run_until_parked();
        worktrees.read_with(&cx, |worktrees, _| {
            assert_eq!(
                herdr_path_for(
                    Path::new(path!("/root/.claude/worktrees/old")),
                    &worktrees.herdr_paths
                ),
                Some(path!("/tmp/old"))
            );
        });
    }

    #[gpui::test]
    async fn file_actions_do_not_reach_the_panel_from_a_worktree_row(
        cx: &mut gpui::TestAppContext,
    ) {
        use util::path;
        let (_git, fs, panel, worktrees, mut cx) =
            panel_with_worktrees(&two_worktrees_porcelain(), cx).await;
        crate::project_panel_tests::select_path(&panel, "root/src/main.rs", &mut cx);
        worktrees.update_in(&mut cx, |worktrees, window, cx| {
            worktrees.select(0, window, cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(Delete { skip_prompt: true });
        cx.run_until_parked();
        assert!(
            fs.is_file(path!("/root/src/main.rs").as_ref()).await,
            "delete from a worktree row must not touch the panel's file selection"
        );

        panel.update_in(&mut cx, |panel, window, cx| {
            window.focus(&panel.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        cx.dispatch_action(Delete { skip_prompt: true });
        cx.run_until_parked();
        assert!(
            !fs.is_file(path!("/root/src/main.rs").as_ref()).await,
            "the same action with the panel focused still deletes"
        );
    }

    #[gpui::test]
    async fn group_refreshes_when_git_worktrees_changes(cx: &mut gpui::TestAppContext) {
        use util::path;

        let (git, fs, _panel, worktrees, cx) = panel_with_worktrees(
            &format!("worktree {}\nHEAD {MAIN_HEAD}\nbranch refs/heads/main\n", path!("/root")),
            cx,
        )
        .await;
        let (entries, refreshes) = worktrees.read_with(&cx, |worktrees, _| {
            (worktrees.entries().to_vec(), worktrees.refresh_count())
        });
        assert!(entries.is_empty());
        assert_eq!(refreshes, 1);

        let agent_path = path!("/root/.claude/worktrees/agent-aad806ed1051ed32d");
        *git.porcelain.lock().expect("porcelain lock") = format!(
            "worktree {root}\nHEAD {MAIN_HEAD}\nbranch refs/heads/main\n\nworktree {agent_path}\nHEAD 2222222bbbbbbb\nbranch refs/heads/feat/x\nlocked reviewing\n",
            root = path!("/root")
        );
        fs.create_dir(path!("/root/.git/worktrees/agent-aad806ed1051ed32d").as_ref())
            .await
            .expect("create worktree dir");
        fs.insert_file(
            path!("/root/.git/worktrees/agent-aad806ed1051ed32d/gitdir"),
            b"x".to_vec(),
        )
        .await;
        cx.run_until_parked();
        cx.executor().advance_clock(REFRESH_DEBOUNCE);
        cx.run_until_parked();

        let entries = worktrees.read_with(&cx, |worktrees, _| worktrees.entries().to_vec());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch.as_deref(), Some("feat/x"));
        assert_eq!(entries[0].locked.as_deref(), Some("reviewing"));
        assert_eq!(agent_tag(&entries[0].path).as_deref(), Some("aad806e"));
        assert!(git.list_calls.load(Ordering::SeqCst) >= 2);

        // A change outside `.git/worktrees` does not re-read the list.
        let before = worktrees.read_with(&cx, |worktrees, _| worktrees.refresh_count());
        fs.insert_file(path!("/root/.git/index"), b"i".to_vec())
            .await;
        cx.run_until_parked();
        cx.executor().advance_clock(REFRESH_DEBOUNCE);
        cx.run_until_parked();
        let after = worktrees.read_with(&cx, |worktrees, _| worktrees.refresh_count());
        assert_eq!(before, after);
    }

    #[test]
    fn delete_allowed_only_under_repository_parent_or_home() {
        let parent = Path::new("/code");
        let home = Path::new("/Users/me");
        assert!(delete_allowed(
            Path::new("/code/repo/.claude/worktrees/x"),
            parent,
            home
        ));
        assert!(delete_allowed(Path::new("/code/other"), parent, home));
        assert!(delete_allowed(Path::new("/Users/me/tmp/wt"), parent, home));
        assert!(!delete_allowed(Path::new("/tmp/wt"), parent, home));
        assert!(!delete_allowed(Path::new("/code"), parent, home));
        assert!(!delete_allowed(Path::new("/Users/me"), parent, home));
        assert!(!delete_allowed(Path::new("/codex/wt"), parent, home));
    }
}
