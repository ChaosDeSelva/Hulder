//! The Saga panel: a right-dock list of Tales, Verses, Lore, Clippings and
//! Glosses for the project's first worktree, with keys to create and edit
//! them and two handoffs to herdr agents.

use crate::model::{Status, Tale, herd_label};
use crate::store::{SagaStore, Snapshot, TaleEntry};
use anyhow::{Context as _, Result, anyhow};
use clippings::ClippingStore;
use editor::Editor;
use fs::Fs;
use glosses::{Gloss, GlossStore, model::gloss_file_path};
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, ClickEvent, Context, DismissEvent, Entity,
    EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Pixels, Render, SharedString,
    Styled, Subscription, Task, WeakEntity, Window, actions, div, px, rems,
};
use herd_panel::HerdView;
use herdr::HerdState;
use path::rel_path::RelPath;
use project::Project;
use std::path::PathBuf;
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::{
    DismissDecision, ModalView, OpenOptions, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

actions!(
    saga,
    [
        /// Opens or focuses the Saga panel.
        ToggleFocus,
        /// Creates a tale after asking for its title.
        NewTale,
        /// Cycles the selected tale's status: open, doing, blocked, done.
        CycleStatus,
        /// Cycles the selected tale's priority: low, normal, high.
        CyclePriority,
        /// Marks the selected tale done.
        MarkDone,
        /// Adds a verse to TODO.md after asking for its text.
        NewVerse,
        /// Creates (or opens) today's lore file under notes/.
        NewLore,
        /// Opens the selected tale, verse list or lore file in the editor.
        OpenSelected,
        /// Toggles the selected verse's checkbox.
        ToggleVerse,
        /// Sends the selected tale or lore to the agent shown in the Herd.
        SendToAgent,
        /// Starts a new herdr agent on the selected tale.
        StartAgentOnTale,
    ]
);

pub fn init(cx: &mut App) {
    crate::init_write_lock(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SagaPanel>(window, cx);
        });
    })
    .detach();
}

/// What the list shows, top to bottom. Headers are not selectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Header(String),
    Tale(usize),
    Verse(usize),
    Lore(usize),
    Clipping(usize),
    Gloss(usize),
}

/// One glossed file: the source path relative to the worktree and its
/// glosses, as `GlossStore::all_for_worktree` lists them.
pub type GlossListing = (Arc<RelPath>, Vec<Gloss>);

impl Row {
    pub fn is_selectable(&self) -> bool {
        !matches!(self, Row::Header(_))
    }
}

pub fn flatten_rows(snapshot: &Snapshot, gloss_files: usize) -> Vec<Row> {
    let mut rows = vec![Row::Header(format!("Tales ({})", snapshot.tales.len()))];
    for status in Status::ALL {
        let indices: Vec<usize> = snapshot
            .tales
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.tale.status == status)
            .map(|(index, _)| index)
            .collect();
        if indices.is_empty() {
            continue;
        }
        rows.push(Row::Header(format!(
            "{} ({})",
            status.as_str(),
            indices.len()
        )));
        rows.extend(indices.into_iter().map(Row::Tale));
    }
    rows.push(Row::Header(format!("Verses ({})", snapshot.verses.len())));
    rows.extend((0..snapshot.verses.len()).map(Row::Verse));
    rows.push(Row::Header(format!("Lore ({})", snapshot.lore.len())));
    rows.extend((0..snapshot.lore.len()).map(Row::Lore));
    rows.push(Row::Header(format!(
        "Clippings ({})",
        snapshot.clippings.len()
    )));
    rows.extend((0..snapshot.clippings.len()).map(Row::Clipping));
    rows.push(Row::Header(format!("Glosses ({gloss_files})")));
    rows.extend((0..gloss_files).map(Row::Gloss));
    rows
}

/// Moves the selection by `delta` selectable rows, clamping at both ends. A
/// delta of zero re-validates the current selection after the rows changed.
pub fn move_selection(rows: &[Row], current: Option<usize>, delta: isize) -> Option<usize> {
    let selectable: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.is_selectable())
        .map(|(index, _)| index)
        .collect();
    if selectable.is_empty() {
        return None;
    }
    let position = match current {
        Some(current) => selectable
            .iter()
            .position(|&index| index >= current)
            .unwrap_or(selectable.len() - 1),
        None => {
            return match delta {
                delta if delta < 0 => selectable.last().copied(),
                _ => selectable.first().copied(),
            };
        }
    };
    let target = (position as isize + delta).clamp(0, selectable.len() as isize - 1) as usize;
    selectable.get(target).copied()
}

pub fn workspace_id_from_create_reply(reply: &serde_json::Value) -> Result<String> {
    reply["workspace"]["workspace_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow!("herdr workspace.create reply has no workspace.workspace_id: {reply}")
        })
}

pub fn terminal_id_from_start_reply(reply: &serde_json::Value) -> Result<String> {
    reply["agent"]["terminal_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("herdr agent.start reply has no agent.terminal_id: {reply}"))
}

pub const AGENT_ARGV: [&str; 1] = ["claude"];
pub const AGENT_LABEL: &str = "claude";

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// A write that waits for the first-use tracking choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingWrite {
    Tale(String),
    Verse(String),
    Lore,
}

/// A write parked behind the first-use choice, together with what the
/// project looked like when it was parked. It is flushed only if the root
/// and the tale set are still the same, and every path check runs again at
/// flush time through the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedWrite {
    pub write: PendingWrite,
    pub root: PathBuf,
    pub tale_paths: Vec<PathBuf>,
}

/// Initializes the layout with the chosen tracking and then applies the
/// parked write. Refuses when the project moved under it.
pub async fn flush_parked(
    store: SagaStore,
    fs: Arc<dyn Fs>,
    tracked: bool,
    parked: ParkedWrite,
    current_tale_paths: Vec<PathBuf>,
) -> Result<Option<PathBuf>> {
    if parked.root != store.root() {
        anyhow::bail!(
            "the project root changed from {} to {}; the parked write was discarded",
            parked.root.display(),
            store.root().display()
        );
    }
    if parked.tale_paths != current_tale_paths {
        anyhow::bail!(
            "the tales changed while waiting for the choice; the parked write was discarded"
        );
    }
    store.initialize(fs.as_ref(), tracked).await?;
    let after_init = store.load(fs.as_ref()).await?;
    let after_init: Vec<PathBuf> = after_init
        .tales
        .into_iter()
        .map(|entry| entry.path)
        .collect();
    if after_init != parked.tale_paths {
        anyhow::bail!("the tales changed during initialization; the parked write was discarded");
    }
    apply_write(store, fs, parked.write).await
}

async fn apply_write(
    store: SagaStore,
    fs: Arc<dyn Fs>,
    write: PendingWrite,
) -> Result<Option<PathBuf>> {
    let today = today();
    match write {
        PendingWrite::Tale(title) => {
            let entry = store.create_tale(fs.as_ref(), &title, &today).await?;
            Ok(Some(entry.path))
        }
        PendingWrite::Verse(text) => {
            store.add_verse(fs.as_ref(), &text).await?;
            Ok(None)
        }
        PendingWrite::Lore => Ok(Some(store.create_lore(fs.as_ref(), &today).await?)),
    }
}

pub struct SagaPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    snapshot: Snapshot,
    glosses: Vec<GlossListing>,
    rows: Vec<Row>,
    selected: Option<usize>,
    pending: Option<ParkedWrite>,
    focus_handle: FocusHandle,
    _reload: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl SagaPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                cx.new(|cx| Self::new(workspace, window, cx))
            })
        })
    }

    fn new(workspace: &Workspace, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let subscription = cx.subscribe(&project, |this, _, event: &project::Event, cx| {
            let touched = match event {
                project::Event::WorktreeUpdatedEntries(_, changes) => changes
                    .iter()
                    .any(|(path, _, _)| SagaStore::is_saga_relative_path(path.as_unix_str())),
                project::Event::WorktreeAdded(_)
                | project::Event::WorktreeRemoved(_)
                | project::Event::WorktreeOrderChanged => {
                    // The root may differ now; a write parked against the
                    // old one must not replay into the new one.
                    this.pending = None;
                    true
                }
                _ => false,
            };
            if touched {
                this.reload(cx);
            }
        });
        let mut this = Self {
            workspace: workspace.weak_handle(),
            fs: project.read(cx).fs().clone(),
            project,
            snapshot: Snapshot::default(),
            glosses: Vec::new(),
            rows: Vec::new(),
            selected: None,
            pending: None,
            focus_handle: cx.focus_handle(),
            _reload: Task::ready(()),
            _subscriptions: vec![subscription],
        };
        this.reload(cx);
        this
    }

    /// The Saga lives in the first visible worktree; no worktree means no
    /// Saga.
    pub fn store(&self, cx: &App) -> Option<SagaStore> {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| crate::store_for(worktree.read(cx).abs_path().to_path_buf(), cx))
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store(cx) else {
            self.set_snapshot(Snapshot::default(), Vec::new(), cx);
            return;
        };
        let fs = self.fs.clone();
        let worktree_id = self
            .project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id());
        let glosses =
            GlossStore::try_global(cx)
                .zip(worktree_id)
                .map(|(gloss_store, worktree_id)| {
                    gloss_store.read(cx).all_for_worktree(worktree_id, cx)
                });
        self._reload = cx.spawn(async move |this, cx| {
            let loaded = store.load(fs.as_ref()).await;
            let glosses = match glosses {
                Some(task) => task.await.unwrap_or_else(|error| {
                    log::warn!("saga: glosses not listed: {error:#}");
                    Vec::new()
                }),
                None => Vec::new(),
            };
            this.update(cx, |this, cx| match loaded {
                Ok(snapshot) => this.set_snapshot(snapshot, glosses, cx),
                Err(error) => this.toast(format!("Saga: {error:#}"), cx),
            })
            .log_err();
        });
    }

    fn set_snapshot(
        &mut self,
        snapshot: Snapshot,
        glosses: Vec<GlossListing>,
        cx: &mut Context<Self>,
    ) {
        self.snapshot = snapshot;
        self.glosses = glosses;
        self.rows = flatten_rows(&self.snapshot, self.glosses.len());
        self.selected = move_selection(&self.rows, self.selected, 0);
        cx.notify();
    }

    fn toast(&self, message: String, cx: &mut Context<Self>) {
        struct SagaToast;
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<SagaToast>(), message).autohide(),
                    cx,
                )
            })
            .log_err();
    }

    fn selected_row(&self) -> Option<&Row> {
        self.selected.and_then(|index| self.rows.get(index))
    }

    fn selected_tale(&self) -> Option<&TaleEntry> {
        match self.selected_row()? {
            Row::Tale(index) => self.snapshot.tales.get(*index),
            _ => None,
        }
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.rows.get(index).is_some_and(Row::is_selectable) {
            self.selected = Some(index);
            cx.notify();
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = move_selection(&self.rows, self.selected, 1);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = move_selection(&self.rows, self.selected, -1);
        cx.notify();
    }

    /// Runs a write, opens the file it returns, reloads, and surfaces any
    /// error as a toast.
    fn run_write(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        write: impl Future<Output = Result<Option<PathBuf>>> + 'static,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let outcome = write.await;
            this.update_in(cx, |this, window, cx| {
                match outcome {
                    Ok(Some(path)) => this.open_path(path, window, cx),
                    Ok(None) => {}
                    Err(error) => this.toast(format!("Saga: {error:#}"), cx),
                }
                this.reload(cx);
            })
            .log_err();
        })
        .detach();
    }

    fn open_path(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(path, OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    /// Writes right away when `.hulder/` exists; otherwise parks the write
    /// and asks the one-time "Keep in git / Local only" question.
    pub fn write_with_init(
        &mut self,
        write: PendingWrite,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.store(cx) else {
            self.toast("Saga: open a project folder first".into(), cx);
            return;
        };
        let fs = self.fs.clone();
        cx.spawn_in(window, async move |this, cx| {
            let exists = store.exists(fs.as_ref()).await;
            this.update_in(cx, |this, window, cx| {
                if exists {
                    this.run_write(window, cx, apply_write(store, fs, write));
                } else {
                    this.pending = Some(ParkedWrite {
                        write,
                        root: store.root().to_path_buf(),
                        tale_paths: this.tale_paths(),
                    });
                    this.prompt_tracking(cx);
                }
            })
            .log_err();
        })
        .detach();
    }

    fn prompt_tracking(&mut self, cx: &mut Context<Self>) {
        struct SagaTrackingPrompt;
        let panel = cx.entity().downgrade();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_notification(
                    NotificationId::unique::<SagaTrackingPrompt>(),
                    cx,
                    |cx| {
                        cx.new(|cx| {
                            MessageNotification::new(
                                "Keep the Saga (.hulder/) in git, or local only?",
                                cx,
                            )
                            .primary_message("Keep in git")
                            .primary_on_click({
                                let panel = panel.clone();
                                move |window, cx| {
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.choose_tracking(true, window, cx)
                                        })
                                        .log_err();
                                    cx.emit(DismissEvent);
                                }
                            })
                            .secondary_message("Local only")
                            .secondary_on_click(move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        panel.choose_tracking(false, window, cx)
                                    })
                                    .log_err();
                                cx.emit(DismissEvent);
                            })
                        })
                    },
                )
            })
            .log_err();
    }

    fn tale_paths(&self) -> Vec<PathBuf> {
        self.snapshot
            .tales
            .iter()
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// Creates the layout with the chosen tracking and flushes the parked
    /// write. The parked write is taken before anything runs, so a failure
    /// here can never leave it behind for a later action to replay.
    pub fn choose_tracking(&mut self, tracked: bool, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self.pending.take();
        let Some(store) = self.store(cx) else {
            return;
        };
        let fs = self.fs.clone();
        let current = self.tale_paths();
        self.run_write(window, cx, async move {
            match pending {
                Some(parked) => flush_parked(store, fs, tracked, parked, current).await,
                None => {
                    store.initialize(fs.as_ref(), tracked).await?;
                    Ok(None)
                }
            }
        });
    }

    fn prompt(
        &mut self,
        placeholder: &'static str,
        help: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_confirm: impl FnOnce(String, &mut Window, &mut App) + 'static,
    ) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, move |window, cx| {
                    SagaPrompt::new(placeholder, help, Box::new(on_confirm), window, cx)
                })
            })
            .log_err();
    }

    fn new_tale(&mut self, _: &NewTale, window: &mut Window, cx: &mut Context<Self>) {
        let panel = cx.entity().downgrade();
        self.prompt(
            "Tale title",
            "Enter creates tickets/NNNN-slug.md",
            window,
            cx,
            move |title, window, cx| {
                panel
                    .update(cx, |panel, cx| {
                        panel.write_with_init(PendingWrite::Tale(title), window, cx)
                    })
                    .log_err();
            },
        );
    }

    fn new_verse(&mut self, _: &NewVerse, window: &mut Window, cx: &mut Context<Self>) {
        let panel = cx.entity().downgrade();
        self.prompt(
            "Verse",
            "Enter appends a checkbox to TODO.md",
            window,
            cx,
            move |text, window, cx| {
                panel
                    .update(cx, |panel, cx| {
                        panel.write_with_init(PendingWrite::Verse(text), window, cx)
                    })
                    .log_err();
            },
        );
    }

    fn new_lore(&mut self, _: &NewLore, window: &mut Window, cx: &mut Context<Self>) {
        self.write_with_init(PendingWrite::Lore, window, cx);
    }

    fn update_selected_tale(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Tale),
    ) {
        let (Some(store), Some(entry)) = (self.store(cx), self.selected_tale().cloned()) else {
            return;
        };
        let mut entry = entry;
        change(&mut entry.tale);
        entry.tale.touch(&today());
        let fs = self.fs.clone();
        self.run_write(window, cx, async move {
            store.write_tale(fs.as_ref(), &entry).await?;
            Ok(None)
        });
    }

    fn cycle_status(&mut self, _: &CycleStatus, window: &mut Window, cx: &mut Context<Self>) {
        self.update_selected_tale(window, cx, |tale| tale.status = tale.status.next());
    }

    fn cycle_priority(&mut self, _: &CyclePriority, window: &mut Window, cx: &mut Context<Self>) {
        self.update_selected_tale(window, cx, |tale| tale.priority = tale.priority.next());
    }

    fn mark_done(&mut self, _: &MarkDone, window: &mut Window, cx: &mut Context<Self>) {
        self.update_selected_tale(window, cx, |tale| tale.status = Status::Done);
    }

    fn toggle_verse_at(&mut self, line: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = self.store(cx) else {
            return;
        };
        let fs = self.fs.clone();
        self.run_write(window, cx, async move {
            store.toggle_verse(fs.as_ref(), line).await?;
            Ok(None)
        });
    }

    fn toggle_selected_verse(
        &mut self,
        _: &ToggleVerse,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Row::Verse(index)) = self.selected_row().cloned()
            && let Some(verse) = self.snapshot.verses.get(index)
        {
            self.toggle_verse_at(verse.line, window, cx);
        }
    }

    fn selected_path(&self, cx: &App) -> Option<PathBuf> {
        match self.selected_row()? {
            Row::Tale(index) => self
                .snapshot
                .tales
                .get(*index)
                .map(|entry| entry.path.clone()),
            Row::Verse(_) => self.store(cx).map(|store| store.todo_path()),
            Row::Lore(index) => self.snapshot.lore.get(*index).cloned(),
            Row::Clipping(index) => self.snapshot.clippings.get(*index).cloned(),
            Row::Gloss(index) => {
                let (source, _) = self.glosses.get(*index)?;
                let store = self.store(cx)?;
                gloss_file_path(store.root(), source).log_err()
            }
            Row::Header(_) => None,
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.selected_path(cx) {
            self.open_path(path, window, cx);
        }
    }

    /// The text handed to an agent for the selection: the file path first,
    /// then its contents, read through the store that owns the file so the
    /// same guards and caps apply as everywhere else.
    fn handoff_for_selection(&self, cx: &App) -> Option<Task<Result<String>>> {
        let path = self.selected_path(cx)?;
        let store = self.store(cx)?;
        let is_clipping = matches!(self.selected_row(), Some(Row::Clipping(_)));
        let fs = self.fs.clone();
        let executor = cx.background_executor().clone();
        Some(cx.background_spawn(async move {
            let text = if is_clipping {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .context("clipping path has no file name")?;
                ClippingStore::new(store.root())
                    .read(fs.as_ref(), &executor, &name)
                    .await?
            } else {
                store.read_at(fs.as_ref(), &path).await?
            };
            Ok(Tale::handoff_text(&path.to_string_lossy(), &text))
        }))
    }

    fn herdr_client(&self, cx: &mut Context<Self>) -> Option<herdr::HerdrClient> {
        let client = HerdState::global(cx).read(cx).client();
        if client.is_none() {
            self.toast("Open herdr first".into(), cx);
        }
        client
    }

    fn send_to_agent(&mut self, _: &SendToAgent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.herdr_client(cx) else {
            return;
        };
        let shown = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).item_of_type::<HerdView>(cx))
            .and_then(|view| view.read(cx).shown_agent_terminal_id());
        let Some(terminal_id) = shown else {
            self.toast("Select an agent in the Herd first".into(), cx);
            return;
        };
        let Some(handoff) = self.handoff_for_selection(cx) else {
            self.toast(
                "Select a tale, verse, lore, clipping or gloss first".into(),
                cx,
            );
            return;
        };
        self.run_write(window, cx, async move {
            let text = handoff.await?;
            client
                .request(
                    "agent.send",
                    serde_json::json!({ "target": terminal_id, "text": text }),
                )
                .await?;
            Ok(None)
        });
    }

    fn start_agent_on_tale(
        &mut self,
        _: &StartAgentOnTale,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.herdr_client(cx) else {
            return;
        };
        let (Some(store), Some(entry)) = (self.store(cx), self.selected_tale().cloned()) else {
            self.toast("Select a tale first".into(), cx);
            return;
        };
        let fs = self.fs.clone();
        self.run_write(window, cx, async move {
            let root = store.root().to_string_lossy().into_owned();
            // The label and agent name come from the ticket file name, which
            // the store already validated, and are sanitized again here so
            // nothing but [A-Za-z0-9._-] ever reaches herdr. argv is fixed.
            let slug = herd_label(
                &entry
                    .path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("tale-{}", entry.tale.id)),
            );
            let created = client
                .request(
                    "workspace.create",
                    serde_json::json!({ "cwd": root, "label": slug, "focus": true }),
                )
                .await?;
            let workspace_id = workspace_id_from_create_reply(&created)?;
            let started = client
                .request(
                    "agent.start",
                    serde_json::json!({
                        "name": slug,
                        "cwd": root,
                        "workspace_id": workspace_id,
                        "argv": AGENT_ARGV,
                        "focus": true,
                    }),
                )
                .await?;
            let terminal_id = terminal_id_from_start_reply(&started)?;
            let text = Tale::handoff_text(&entry.path.to_string_lossy(), &entry.tale.serialize());
            client
                .request(
                    "agent.send",
                    serde_json::json!({ "target": terminal_id, "text": text }),
                )
                .await
                .context("agent started but the tale could not be sent")?;

            let mut entry = entry;
            let today = today();
            entry.tale.status = Status::Doing;
            entry.tale.touch(&today);
            entry
                .tale
                .append_chronicle(&format!("{today} started by {AGENT_LABEL}"));
            store.write_tale(fs.as_ref(), &entry).await?;
            Ok(None)
        });
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.selected == Some(index);
        let Some(row) = self.rows.get(index) else {
            return div().into_any_element();
        };
        match row {
            Row::Header(text) => div()
                .px_2()
                .pt_2()
                .pb_1()
                .child(
                    Label::new(text.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            Row::Tale(tale_index) => {
                let Some(entry) = self.snapshot.tales.get(*tale_index) else {
                    return div().into_any_element();
                };
                let (icon, color) = match entry.tale.status {
                    Status::Open => (IconName::TodoPending, Color::Muted),
                    Status::Doing => (IconName::TodoProgress, Color::Accent),
                    Status::Blocked => (IconName::XCircle, Color::Warning),
                    Status::Done => (IconName::TodoComplete, Color::Success),
                };
                let priority = entry.tale.priority.as_str();
                ListItem::new(("saga-tale", index))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(1)
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select(index, cx);
                        this.open_selected(&OpenSelected, window, cx);
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(icon).color(color))
                            .child(
                                Label::new(format!("{:04}", entry.tale.id))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(entry.tale.title.clone()).truncate())
                            .child(Label::new(priority).size(LabelSize::Small).color(
                                match entry.tale.priority {
                                    crate::model::Priority::High => Color::Error,
                                    crate::model::Priority::Normal => Color::Muted,
                                    crate::model::Priority::Low => Color::Disabled,
                                },
                            )),
                    )
                    .into_any_element()
            }
            Row::Verse(verse_index) => {
                let Some(verse) = self.snapshot.verses.get(*verse_index) else {
                    return div().into_any_element();
                };
                let line = verse.line;
                let icon = if verse.done {
                    IconName::TodoComplete
                } else {
                    IconName::TodoPending
                };
                ListItem::new(("saga-verse", index))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(1 + verse.depth / 2)
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select(index, cx);
                        this.open_selected(&OpenSelected, window, cx);
                    }))
                    .start_slot(
                        IconButton::new(("saga-verse-toggle", index), icon)
                            .icon_size(IconSize::Small)
                            .aria_label("Toggle task completion")
                            .tooltip(Tooltip::text("Toggle task completion"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.toggle_verse_at(line, window, cx);
                            })),
                    )
                    .child(Label::new(verse.text.clone()).when(verse.done, |label| {
                        label.color(Color::Muted).strikethrough()
                    }))
                    .into_any_element()
            }
            Row::Lore(lore_index) => {
                let Some(path) = self.snapshot.lore.get(*lore_index) else {
                    return div().into_any_element();
                };
                let name = self
                    .store(cx)
                    .and_then(|store| {
                        path.strip_prefix(store.dir())
                            .ok()
                            .map(|relative| relative.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                ListItem::new(("saga-lore", index))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(1)
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select(index, cx);
                        this.open_selected(&OpenSelected, window, cx);
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::Book).color(Color::Muted))
                            .child(Label::new(name).truncate()),
                    )
                    .into_any_element()
            }
            Row::Clipping(clipping_index) => {
                let Some(path) = self.snapshot.clippings.get(*clipping_index) else {
                    return div().into_any_element();
                };
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                ListItem::new(("saga-clipping", index))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(1)
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select(index, cx);
                        this.open_selected(&OpenSelected, window, cx);
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::Scissors).color(Color::Muted))
                            .child(Label::new(name).truncate()),
                    )
                    .into_any_element()
            }
            Row::Gloss(gloss_index) => {
                let Some((source, glosses)) = self.glosses.get(*gloss_index) else {
                    return div().into_any_element();
                };
                let detached = glosses.iter().filter(|gloss| gloss.lines.is_none()).count();
                let count = if detached > 0 {
                    format!("{} ({detached} file-level)", glosses.len())
                } else {
                    glosses.len().to_string()
                };
                ListItem::new(("saga-gloss", index))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(1)
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select(index, cx);
                        this.open_selected(&OpenSelected, window, cx);
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::Quote).color(Color::Muted))
                            .child(Label::new(source.as_unix_str().to_string()))
                            .child(Label::new(count).size(LabelSize::Small).color(Color::Muted)),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_empty(&self, cx: &App) -> gpui::AnyElement {
        let message = match self.store(cx) {
            Some(_) => "No Saga yet. Press n for a tale, t for a verse, N for today's lore.",
            None => "Open a project folder to start its Saga.",
        };
        v_flex()
            .p_2()
            .gap_2()
            .child(Label::new(message).color(Color::Muted))
            .into_any_element()
    }
}

impl Render for SagaPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = self.snapshot.tales.is_empty()
            && self.snapshot.verses.is_empty()
            && self.snapshot.lore.is_empty()
            && self.snapshot.clippings.is_empty()
            && self.glosses.is_empty();
        v_flex()
            .id("saga-panel")
            .key_context("SagaPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_tale))
            .on_action(cx.listener(Self::new_verse))
            .on_action(cx.listener(Self::new_lore))
            .on_action(cx.listener(Self::cycle_status))
            .on_action(cx.listener(Self::cycle_priority))
            .on_action(cx.listener(Self::mark_done))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::toggle_selected_verse))
            .on_action(cx.listener(Self::send_to_agent))
            .on_action(cx.listener(Self::start_agent_on_tale))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .size_full()
            .p_2()
            .bg(cx.theme().colors().panel_background)
            .overflow_y_scroll()
            .when(empty, |this| this.child(self.render_empty(cx)))
            .when(!empty, |this| {
                this.children((0..self.rows.len()).map(|index| self.render_row(index, cx)))
            })
    }
}

impl Focusable for SagaPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SagaPanel {}

impl Panel for SagaPanel {
    fn persistent_name() -> &'static str {
        "SagaPanel"
    }

    fn panel_key() -> &'static str {
        "SagaPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Saga)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Saga")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        5
    }
}

type PromptCallback = Box<dyn FnOnce(String, &mut Window, &mut App)>;

/// A one-line modal prompt, after the go-to-line modal.
pub struct SagaPrompt {
    editor: Entity<Editor>,
    help: &'static str,
    on_confirm: Option<PromptCallback>,
}

impl SagaPrompt {
    fn new(
        placeholder: &'static str,
        help: &'static str,
        on_confirm: PromptCallback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(placeholder, window, cx);
            editor
        });
        Self {
            editor,
            help,
            on_confirm: Some(on_confirm),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx).trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(on_confirm) = self.on_confirm.take() {
            on_confirm(text, window, cx);
        }
        cx.emit(DismissEvent);
    }
}

impl ModalView for SagaPrompt {
    fn on_before_dismiss(&mut self, _: &mut Window, _: &mut Context<Self>) -> DismissDecision {
        DismissDecision::Dismiss(true)
    }
}

impl Focusable for SagaPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for SagaPrompt {}

impl Render for SagaPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(24.))
            .elevation_2(cx)
            .key_context("SagaPrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                div()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .px_2()
                    .py_1()
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .child(Label::new(SharedString::from(self.help)).color(Color::Muted)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Priority, Verse};

    fn snapshot() -> Snapshot {
        let tale = |id: u32, status: Status| TaleEntry {
            path: PathBuf::from(format!("/r/.hulder/tickets/{id:04}-t.md")),
            tale: Tale {
                id,
                title: format!("T{id}"),
                status,
                priority: Priority::Normal,
                tags: vec![],
                created: String::new(),
                updated: String::new(),
                extra: vec![],
                body: String::new(),
            },
        };
        Snapshot {
            tales: vec![
                tale(1, Status::Open),
                tale(2, Status::Done),
                tale(3, Status::Open),
            ],
            todo: String::new(),
            verses: vec![Verse {
                line: 2,
                depth: 0,
                done: false,
                text: "v".into(),
            }],
            lore: vec![PathBuf::from("/r/.hulder/NOTES.md")],
            clippings: vec![
                PathBuf::from("/r/.hulder/clippings/2026-10-13-b.md"),
                PathBuf::from("/r/.hulder/clippings/2026-10-12-a.md"),
            ],
            config: None,
        }
    }

    #[test]
    fn rows_group_tales_by_status_with_counts() {
        let rows = flatten_rows(&snapshot(), 1);
        assert_eq!(
            rows,
            vec![
                Row::Header("Tales (3)".into()),
                Row::Header("open (2)".into()),
                Row::Tale(0),
                Row::Tale(2),
                Row::Header("done (1)".into()),
                Row::Tale(1),
                Row::Header("Verses (1)".into()),
                Row::Verse(0),
                Row::Header("Lore (1)".into()),
                Row::Lore(0),
                Row::Header("Clippings (2)".into()),
                Row::Clipping(0),
                Row::Clipping(1),
                Row::Header("Glosses (1)".into()),
                Row::Gloss(0),
            ]
        );
        let empty = flatten_rows(&Snapshot::default(), 0);
        assert!(empty.iter().all(|row| !row.is_selectable()));
    }

    #[test]
    fn selection_skips_headers_and_clamps() {
        let rows = flatten_rows(&snapshot(), 0);
        assert_eq!(move_selection(&rows, None, 1), Some(2));
        assert_eq!(move_selection(&rows, None, -1), Some(12));
        assert_eq!(move_selection(&rows, Some(2), 1), Some(3));
        assert_eq!(move_selection(&rows, Some(3), 1), Some(5));
        assert_eq!(move_selection(&rows, Some(5), 1), Some(7));
        // Down from the lore row skips the Clippings header.
        assert_eq!(move_selection(&rows, Some(9), 1), Some(11));
        assert_eq!(move_selection(&rows, Some(12), 1), Some(12));
        assert_eq!(move_selection(&rows, Some(2), -1), Some(2));
        assert_eq!(move_selection(&rows, Some(7), -1), Some(5));
        // A stale index past the end lands on the last selectable row.
        assert_eq!(move_selection(&rows, Some(50), 0), Some(12));
        // A stale index on a header moves to the next selectable row.
        assert_eq!(move_selection(&rows, Some(4), 0), Some(5));
        assert_eq!(move_selection(&[], Some(0), 0), None);
    }

    #[gpui::test]
    async fn parked_write_is_refused_when_a_symlink_appears(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            serde_json::json!({ "victim.md": "untouched", "src": {} }),
        )
        .await;
        let store = SagaStore::new("/repo");
        let parked = ParkedWrite {
            write: PendingWrite::Tale("Fix it".into()),
            root: PathBuf::from("/repo"),
            tale_paths: vec![],
        };
        // Between parking and the choice, something plants `.hulder/` with a
        // symlink where the new tale would be written.
        fs.insert_tree("/repo/.hulder", serde_json::json!({ "tickets": {} }))
            .await;
        fs.insert_symlink(
            "/repo/.hulder/tickets/0001-fix-it.md",
            PathBuf::from("/repo/victim.md"),
        )
        .await;
        let fs_dyn: Arc<dyn Fs> = fs.clone();
        let error = flush_parked(store.clone(), fs_dyn, true, parked, vec![])
            .await
            .expect_err("symlinked target must be refused at flush time");
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(
            fs.load(std::path::Path::new("/repo/victim.md"))
                .await
                .expect("read"),
            "untouched"
        );
    }

    #[gpui::test]
    async fn parked_write_is_discarded_when_the_project_drifts(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/repo", serde_json::json!({ "src": {} }))
            .await;
        let fs_dyn: Arc<dyn Fs> = fs.clone();
        let parked = |root: &str| ParkedWrite {
            write: PendingWrite::Verse("v".into()),
            root: PathBuf::from(root),
            tale_paths: vec![],
        };
        let moved = flush_parked(
            SagaStore::new("/repo"),
            fs_dyn.clone(),
            false,
            parked("/other"),
            vec![],
        )
        .await
        .expect_err("root drift");
        assert!(moved.to_string().contains("root changed"), "{moved:#}");
        assert!(
            !fs.is_dir(std::path::Path::new("/repo/.hulder")).await,
            "nothing initialized"
        );

        let tales = flush_parked(
            SagaStore::new("/repo"),
            fs_dyn.clone(),
            false,
            parked("/repo"),
            vec![PathBuf::from("/repo/.hulder/tickets/0001-new.md")],
        )
        .await
        .expect_err("tale drift");
        assert!(tales.to_string().contains("tales changed"), "{tales:#}");

        let ok = flush_parked(
            SagaStore::new("/repo"),
            fs_dyn,
            false,
            parked("/repo"),
            vec![],
        )
        .await
        .expect("same project flushes");
        assert_eq!(ok, None);
        assert_eq!(
            fs.load(std::path::Path::new("/repo/.hulder/TODO.md"))
                .await
                .expect("todo"),
            "# Todo\n\n- [ ] v\n"
        );
    }

    #[test]
    fn herdr_reply_parsing() {
        let created = serde_json::json!({
            "type": "workspace_created",
            "workspace": { "workspace_id": "w9", "label": "x" },
            "tab": {}, "root_pane": {}
        });
        assert_eq!(workspace_id_from_create_reply(&created).expect("id"), "w9");
        assert!(workspace_id_from_create_reply(&serde_json::json!({ "type": "ok" })).is_err());

        let started = serde_json::json!({
            "type": "agent_started",
            "agent": { "terminal_id": "t4", "pane_id": "w9:p1" },
            "argv": ["claude"]
        });
        assert_eq!(terminal_id_from_start_reply(&started).expect("id"), "t4");
        assert!(terminal_id_from_start_reply(&serde_json::json!({ "agent": {} })).is_err());
    }
}
