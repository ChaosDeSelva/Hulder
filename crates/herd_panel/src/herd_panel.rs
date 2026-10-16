//! The Herd: a full-size view listing herdr workspaces and agents on the left
//! with one embedded terminal on the right showing whatever is selected.

pub mod factory;
mod factory_client;

use collections::HashMap;
use editor::{Editor, MultiBufferOffset};
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    TaskExt as _, WeakEntity, Window, actions, px,
};
use herdr::{AgentStatus, Connection, HerdEvent, HerdState, WorkspaceRow};
use project::Project;
use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId};
use terminal_view::TerminalView;
use ui::{
    ActiveTheme as _, Button, ButtonCommon as _, Clickable, Color, Icon, IconName, Label,
    LabelCommon, LabelSize, ListItem, ListItemSpacing, StatefulInteractiveElement as _, Toggleable,
    h_flex, v_flex,
};
use util::ResultExt as _;
use workspace::{
    Toast, Workspace,
    item::{Item, ItemEvent},
    notifications::NotificationId,
};

actions!(
    herd_panel,
    [
        /// Opens or focuses the Herd view.
        Toggle,
        /// Alias kept for the dock-era keymap: opens the Herd view.
        ToggleFocus,
        /// Attaches the selected agent (or shows the herdr TUI for a workspace).
        AttachAgent,
        /// Opens the selected workspace's worktree as a project.
        OpenWorktree,
        /// Reconnects to herdr and reloads the snapshot.
        Refresh,
        /// Shows the herdr TUI, which launches the session if needed.
        StartHerdr,
        /// Sends the active editor's selection to the agent shown in the Herd view.
        SendSelection,
    ]
);

pub fn init(cx: &mut App) {
    factory::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, cx| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            HerdView::toggle(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            HerdView::toggle(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &SendSelection, _window, cx| {
            send_selection(workspace, cx);
        });

        let state = HerdState::global(cx);
        cx.subscribe(&state, |workspace, _state, event: &HerdEvent, cx| {
            let HerdEvent::AgentStatusChanged { agent, previous } = event;
            let Some((message, autohide)) = toast_for(*previous, agent.status, &agent.label())
            else {
                return;
            };
            struct HerdToast;
            let id =
                NotificationId::composite::<HerdToast>(SharedString::from(agent.pane_id.clone()));
            let selection = Selection::Agent {
                terminal_id: agent.terminal_id.clone(),
                label: agent.label(),
            };
            let mut toast = Toast::new(id, message).on_click("Show", move |window, cx| {
                let selection = selection.clone();
                workspace::with_active_or_new_workspace(cx, move |workspace, window, cx| {
                    HerdView::open(workspace, Some(selection), window, cx);
                });
                let _ = window;
            });
            if autohide {
                toast = toast.autohide();
            }
            workspace.show_toast(toast, cx);
        })
        .detach();
    })
    .detach();
}

/// Returns (message, autohide) when a status change deserves a toast.
pub fn toast_for(
    previous: AgentStatus,
    current: AgentStatus,
    label: &str,
) -> Option<(String, bool)> {
    match (previous, current) {
        (p, AgentStatus::Blocked) if p != AgentStatus::Blocked => {
            Some((format!("{label} needs you"), false))
        }
        (AgentStatus::Working, AgentStatus::Idle | AgentStatus::Done) => {
            Some((format!("{label} is idle"), true))
        }
        _ => None,
    }
}

const SEND_SELECTION_HINT: &str = "Open the Herd and select an agent first";

/// Terminal id of the agent shown under a `HerdView::active_key`, if that key
/// names an agent rather than the herdr TUI.
pub fn agent_terminal_id_from_key(key: &str) -> Option<&str> {
    key.strip_prefix("agent:").filter(|id| !id.is_empty())
}

/// Params for herdr `agent.send`. The text is sanitized here so no caller can
/// hand the agent's terminal an escape sequence or a control character.
pub fn send_params(target: &str, text: &str) -> serde_json::Value {
    let text = herdr::sanitize::sanitize_for_terminal(text);
    serde_json::json!({ "target": target, "text": text })
}

/// Sends the active editor's newest selection to the agent whose terminal the
/// Herd view is showing. Nothing is sent without an explicit selection.
fn send_selection(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    struct SendSelectionToast;
    let toast_id = NotificationId::unique::<SendSelectionToast>();
    let text = workspace.active_item_as::<Editor>(cx).and_then(|editor| {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selection = editor.selections.newest::<MultiBufferOffset>(&snapshot);
            let text = snapshot
                .buffer_snapshot()
                .text_for_range(selection.range())
                .collect::<String>();
            (!text.is_empty()).then_some(text)
        })
    });
    let Some(text) = text else {
        workspace.show_toast(Toast::new(toast_id, "Select some text to send first"), cx);
        return;
    };
    let terminal_id = workspace
        .item_of_type::<HerdView>(cx)
        .and_then(|view| view.read(cx).shown_agent_terminal_id());
    let client = HerdState::global(cx).read(cx).client();
    let (Some(terminal_id), Some(client)) = (terminal_id, client) else {
        workspace.show_toast(Toast::new(toast_id, SEND_SELECTION_HINT), cx);
        return;
    };
    cx.spawn(async move |workspace, cx| {
        let result = cx
            .background_spawn(async move {
                client
                    .request("agent.send", send_params(&terminal_id, &text))
                    .await
            })
            .await;
        if let Err(error) = result {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(toast_id, format!("Could not send selection: {error:#}")),
                    cx,
                );
            })?;
        }
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

#[derive(Debug, Clone, Eq)]
pub enum Selection {
    Workspace { id: String },
    Agent { terminal_id: String, label: String },
}

/// Agents are identified by terminal id only: their label is a live terminal
/// title that changes while they work.
impl PartialEq for Selection {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Selection::Workspace { id: a }, Selection::Workspace { id: b }) => a == b,
            (Selection::Agent { terminal_id: a, .. }, Selection::Agent { terminal_id: b, .. }) => {
                a == b
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Workspace {
        id: String,
        label: String,
        branch: Option<String>,
        has_worktree: bool,
        focused: bool,
    },
    Agent {
        pane_id: String,
        terminal_id: String,
        workspace_id: String,
        label: String,
        status: AgentStatus,
    },
}

impl Row {
    fn selection(&self) -> Selection {
        match self {
            Row::Workspace { id, .. } => Selection::Workspace { id: id.clone() },
            Row::Agent {
                terminal_id, label, ..
            } => Selection::Agent {
                terminal_id: terminal_id.clone(),
                label: label.clone(),
            },
        }
    }
}

pub fn flatten_rows(workspaces: &[WorkspaceRow]) -> Vec<Row> {
    let mut rows = Vec::new();
    for workspace in workspaces {
        rows.push(Row::Workspace {
            id: workspace.id.clone(),
            label: workspace.label.clone(),
            branch: workspace.branch.clone(),
            has_worktree: workspace.worktree_path.is_some(),
            focused: workspace.focused,
        });
        for agent in &workspace.agents {
            rows.push(Row::Agent {
                pane_id: agent.pane_id.clone(),
                terminal_id: agent.terminal_id.clone(),
                workspace_id: agent.workspace_id.clone(),
                label: agent.label(),
                status: agent.status,
            });
        }
    }
    rows
}

/// Moves a selection through `rows` by `delta`, clamping at the ends.
pub fn move_selection(
    rows: &[Row],
    current: Option<&Selection>,
    delta: isize,
) -> Option<Selection> {
    if rows.is_empty() {
        return None;
    }
    let current_ix = current
        .and_then(|selection| rows.iter().position(|row| &row.selection() == selection))
        .map(|ix| ix as isize)
        .unwrap_or(-1);
    let next = (current_ix + delta).clamp(0, rows.len() as isize - 1) as usize;
    Some(rows[next].selection())
}

fn herdr_terminal(id: &str, label: &str, args: Vec<String>) -> SpawnInTerminal {
    SpawnInTerminal {
        id: TaskId(id.to_string()),
        full_label: label.to_string(),
        label: label.to_string(),
        command: Some("herdr".to_string()),
        command_label: format!("herdr {}", args.join(" ")),
        args,
        use_new_terminal: true,
        allow_concurrent_runs: true,
        reveal: RevealStrategy::Never,
        reveal_target: RevealTarget::Center,
        ..SpawnInTerminal::default()
    }
}

/// `herdr agent attach` for one agent pane.
pub fn attach_task(terminal_id: &str, label: &str) -> SpawnInTerminal {
    herdr_terminal(
        &format!("herd-attach-{terminal_id}"),
        &format!("herd: {label}"),
        vec!["agent".into(), "attach".into(), terminal_id.to_string()],
    )
}

/// The herdr TUI itself (launches or attaches the session).
pub fn tui_task() -> SpawnInTerminal {
    herdr_terminal("herd-tui", "herdr", Vec::new())
}

const TUI_KEY: &str = "tui";

pub struct HerdView {
    workspace: WeakEntity<Workspace>,
    project: WeakEntity<Project>,
    state: Entity<HerdState>,
    focus_handle: FocusHandle,
    selected: Option<Selection>,
    terminals: HashMap<String, Entity<TerminalView>>,
    active_key: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl HerdView {
    /// Opens the Herd view in the active pane (or activates the existing one)
    /// and optionally selects something in it.
    pub fn open(
        workspace: &mut Workspace,
        selection: Option<Selection>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let view = match workspace.item_of_type::<HerdView>(cx) {
            Some(existing) => {
                workspace.activate_item(&existing, true, true, window, cx);
                existing
            }
            None => {
                let view = cx.new(|cx| HerdView::new(workspace, window, cx));
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
                view
            }
        };
        if let Some(selection) = selection {
            view.update(cx, |view, cx| view.select(selection, window, cx));
        }
    }

    /// Opens the Herd view, or if it is already the active item, goes back to
    /// the previously active item so the key works as a toggle.
    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let active_is_herd = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<HerdView>())
            .is_some();
        if active_is_herd {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.activate_previous_item(
                    &workspace::pane::ActivatePreviousItem::default(),
                    window,
                    cx,
                );
            });
        } else {
            Self::open(workspace, None, window, cx);
        }
    }

    fn new(workspace: &Workspace, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = HerdState::global(cx);
        let subscription = cx.observe(&state, |this, state, cx| {
            this.prune_terminals(&state, cx);
            cx.notify();
        });
        Self {
            workspace: workspace.weak_handle(),
            project: workspace.project().downgrade(),
            state,
            focus_handle: cx.focus_handle(),
            selected: None,
            terminals: HashMap::default(),
            active_key: None,
            _subscriptions: vec![subscription],
        }
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        flatten_rows(&self.state.read(cx).workspaces)
    }

    /// The agent whose attach terminal is currently shown, if any.
    pub fn shown_agent_terminal_id(&self) -> Option<String> {
        self.active_key
            .as_deref()
            .and_then(agent_terminal_id_from_key)
            .map(str::to_string)
    }

    /// Selects a row and shows its terminal on the right.
    pub fn select(&mut self, selection: Selection, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(selection.clone());
        cx.notify();
        match selection {
            Selection::Agent { terminal_id, label } => {
                let key = format!("agent:{terminal_id}");
                self.show_terminal(key, attach_task(&terminal_id, &label), window, cx);
            }
            Selection::Workspace { id } => {
                if let Some(client) = self.state.read(cx).client() {
                    cx.background_spawn(async move {
                        client
                            .request("workspace.focus", serde_json::json!({ "workspace_id": id }))
                            .await
                            .log_err();
                    })
                    .detach();
                }
                self.show_terminal(TUI_KEY.to_string(), tui_task(), window, cx);
            }
        }
    }

    /// Drops cached terminals for agents herdr no longer lists, so their
    /// `herdr agent attach` processes are killed instead of lingering.
    fn prune_terminals(&mut self, state: &Entity<HerdState>, cx: &mut Context<Self>) {
        let live: std::collections::HashSet<String> = state
            .read(cx)
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.agents.iter())
            .map(|agent| format!("agent:{}", agent.terminal_id))
            .collect();
        let before = self.terminals.len();
        self.terminals
            .retain(|key, _| key == TUI_KEY || live.contains(key));
        if self.terminals.len() != before
            && let Some(active) = &self.active_key
            && !self.terminals.contains_key(active)
        {
            self.active_key = None;
        }
    }

    fn terminal_has_exited(view: &Entity<TerminalView>, cx: &App) -> bool {
        let terminal = view.read(cx).terminal().read(cx);
        matches!(
            terminal.task().map(|task| &task.status),
            Some(terminal::TaskStatus::Completed { .. } | terminal::TaskStatus::Unknown)
        )
    }

    fn show_terminal(
        &mut self,
        key: String,
        task: SpawnInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(existing) = self.terminals.get(&key) {
            if Self::terminal_has_exited(existing, cx) {
                // The attach process is gone (herdr restarted or the pane closed);
                // fall through and attach again.
                self.terminals.remove(&key);
            } else {
                self.active_key = Some(key);
                cx.notify();
                return;
            }
        }
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let creating = project.update(cx, |project, cx| project.create_terminal_task(task, cx));
        let workspace = self.workspace.clone();
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            let terminal = creating.await?;
            this.update_in(cx, |this, window, cx| {
                let workspace_id = workspace
                    .upgrade()
                    .and_then(|workspace| workspace.read(cx).database_id());
                let view = cx.new(|cx| {
                    TerminalView::new(
                        terminal,
                        workspace.clone(),
                        workspace_id,
                        project,
                        window,
                        cx,
                    )
                });
                this.terminals.insert(key.clone(), view);
                this.active_key = Some(key);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn close_active_terminal(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.active_key.take() {
            self.terminals.remove(&key);
            cx.notify();
        }
    }

    fn attach_selected(&mut self, _: &AttachAgent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selection) = self.selected.clone() {
            self.select(selection, window, cx);
        }
    }

    fn open_worktree(&mut self, _: &OpenWorktree, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Selection::Workspace { id }) = self.selected.clone() else {
            return;
        };
        let path = self
            .state
            .read(cx)
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .and_then(|workspace| workspace.worktree_path.clone());
        let Some(path) = path else {
            log::info!("herd: workspace {id} has no worktree path");
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let app_state = workspace.read(cx).app_state().clone();
        let task = workspace::open_paths(
            &[std::path::PathBuf::from(path)],
            app_state,
            workspace::OpenOptions::default(),
            cx,
        );
        cx.spawn_in(window, async move |_, _| {
            task.await.log_err();
        })
        .detach();
    }

    fn start_herdr(&mut self, _: &StartHerdr, window: &mut Window, cx: &mut Context<Self>) {
        self.show_terminal(TUI_KEY.to_string(), tui_task(), window, cx);
        let state = self.state.clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            state.update(cx, |state, cx| state.request_reconnect(cx));
        })
        .detach();
    }

    fn refresh(&mut self, _: &Refresh, _window: &mut Window, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, cx| state.request_reconnect(cx));
    }

    fn select_next(&mut self, _: &menu::SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        if let Some(next) = move_selection(&rows, self.selected.as_ref(), 1) {
            self.select(next, window, cx);
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.rows(cx);
        if let Some(previous) = move_selection(&rows, self.selected.as_ref(), -1) {
            self.select(previous, window, cx);
        }
    }

    fn status_icon(status: AgentStatus) -> (IconName, Color) {
        match status {
            AgentStatus::Working => (IconName::PlayFilled, Color::Accent),
            AgentStatus::Blocked => (IconName::Warning, Color::Warning),
            AgentStatus::Done => (IconName::Check, Color::Success),
            AgentStatus::Idle => (IconName::Circle, Color::Muted),
            AgentStatus::Unknown => (IconName::CircleHelp, Color::Disabled),
        }
    }

    fn render_disconnected(&self, reason: &str, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .p_2()
            .gap_2()
            .child(Label::new("herdr is not running").color(Color::Muted))
            .child(Label::new(reason.to_string()).color(Color::Muted))
            .child(Button::new("start-herdr", "Open herdr").on_click(
                cx.listener(|this, _, window, cx| this.start_herdr(&StartHerdr, window, cx)),
            ))
    }

    fn render_row(&self, ix: usize, row: Row, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selection = row.selection();
        let selected = self.selected.as_ref() == Some(&selection);
        match row {
            Row::Workspace {
                label,
                branch,
                focused,
                ..
            } => ListItem::new(("herd-workspace", ix))
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    if event.click_count() == 2 {
                        this.selected = Some(selection.clone());
                        this.open_worktree(&OpenWorktree, window, cx);
                    } else {
                        this.select(selection.clone(), window, cx);
                    }
                }))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Icon::new(IconName::Folder).color(if focused {
                            Color::Accent
                        } else {
                            Color::Muted
                        }))
                        .child(Label::new(label).truncate())
                        .children(branch.map(|branch| {
                            Label::new(branch)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        })),
                )
                .into_any_element(),
            Row::Agent { label, status, .. } => {
                let (icon, color) = Self::status_icon(status);
                ListItem::new(("herd-agent", ix))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(1)
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select(selection.clone(), window, cx);
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(icon).color(color))
                            .child(Label::new(label).truncate()),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let connection = self.state.read(cx).connection.clone();
        match connection {
            Connection::Disconnected { reason } => {
                self.render_disconnected(&reason, cx).into_any_element()
            }
            Connection::Connecting => Label::new("Connecting to herdr…")
                .color(Color::Muted)
                .into_any_element(),
            Connection::Connected => {
                let rows = self.rows(cx);
                v_flex()
                    .child(
                        ListItem::new("herd-tui")
                            .spacing(ListItemSpacing::Sparse)
                            .toggle_state(self.active_key.as_deref() == Some(TUI_KEY))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_terminal(TUI_KEY.to_string(), tui_task(), window, cx);
                            }))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(Icon::new(IconName::HerdrInitial).color(Color::Accent))
                                    .child(Label::new("herdr")),
                            ),
                    )
                    .children(
                        rows.into_iter()
                            .enumerate()
                            .map(|(ix, row)| self.render_row(ix, row, cx)),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_terminal(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Allow closing the shown terminal so the next selection recreates it.
        let close = self.active_key.is_some().then(|| {
            Button::new("herd-close-terminal", "Close terminal")
                .on_click(cx.listener(|this, _, _window, cx| this.close_active_terminal(cx)))
        });
        let active = self
            .active_key
            .as_ref()
            .and_then(|key| self.terminals.get(key))
            .cloned();
        v_flex()
            .flex_1()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .children(close.map(|close| h_flex().justify_end().px_2().py_1().child(close)))
            .child(match active {
                Some(view) => div_fill(view.into_any_element()).p_3(),
                None => div_fill(
                    v_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(Icon::new(IconName::HerdrInitial).color(Color::Muted))
                        .child(
                            Label::new("Select an agent to attach, or a workspace to open herdr")
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                ),
            })
            .into_any_element()
    }
}

fn div_fill(child: gpui::AnyElement) -> gpui::Div {
    gpui::div().flex_1().size_full().child(child)
}

impl Render for HerdView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Key handling lives on the list column only, so single-letter bindings
        // never swallow keystrokes typed into the embedded terminal.
        h_flex()
            .size_full()
            .child(
                v_flex()
                    .id("herd-list")
                    .key_context("HerdList")
                    .track_focus(&self.focus_handle)
                    .on_action(cx.listener(Self::attach_selected))
                    .on_action(cx.listener(Self::open_worktree))
                    .on_action(cx.listener(Self::start_herdr))
                    .on_action(cx.listener(Self::refresh))
                    .on_action(cx.listener(Self::select_next))
                    .on_action(cx.listener(Self::select_previous))
                    .w(px(320.))
                    .min_w(px(160.))
                    .h_full()
                    .p_3()
                    .gap_2()
                    .border_r_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().panel_background)
                    .overflow_y_scroll()
                    .child(self.render_list(cx)),
            )
            .child(self.render_terminal(cx))
    }
}

impl Focusable for HerdView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for HerdView {}

impl Item for HerdView {
    type Event = ();

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Herd".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::HerdrInitial))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr::{AgentRow, AgentStatus, WorkspaceRow};

    fn sample() -> Vec<WorkspaceRow> {
        vec![WorkspaceRow {
            id: "w1".into(),
            label: "repo".into(),
            focused: true,
            worktree_path: Some("/repo".into()),
            branch: Some("main".into()),
            agents: vec![AgentRow {
                pane_id: "w1:p1".into(),
                tab_id: "w1:t1".into(),
                terminal_id: "t1".into(),
                workspace_id: "w1".into(),
                agent: Some("claude".into()),
                display_agent: None,
                name: Some("builder".into()),
                title: None,
                terminal_title_stripped: None,
                status: AgentStatus::Blocked,
                cwd: None,
            }],
        }]
    }

    #[test]
    fn rows_flatten_workspaces_then_agents() {
        let rows = flatten_rows(&sample());
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0], Row::Workspace { .. }));
        assert!(
            matches!(&rows[1], Row::Agent { label, status: AgentStatus::Blocked, .. } if label == "builder")
        );
    }

    #[test]
    fn selection_moves_by_identity_not_index() {
        let rows = flatten_rows(&sample());
        let first = move_selection(&rows, None, 1).expect("first");
        assert_eq!(first, Selection::Workspace { id: "w1".into() });
        let second = move_selection(&rows, Some(&first), 1).expect("second");
        assert!(matches!(second, Selection::Agent { ref terminal_id, .. } if terminal_id == "t1"));
        let clamped = move_selection(&rows, Some(&second), 1).expect("clamped");
        assert_eq!(clamped, second);
        assert_eq!(move_selection(&[], None, 1), None);
    }

    #[test]
    fn toast_only_when_entering_blocked_or_finishing_work() {
        assert_eq!(
            toast_for(AgentStatus::Working, AgentStatus::Blocked, "b"),
            Some(("b needs you".to_string(), false))
        );
        assert_eq!(
            toast_for(AgentStatus::Working, AgentStatus::Idle, "b"),
            Some(("b is idle".to_string(), true))
        );
        assert_eq!(
            toast_for(AgentStatus::Idle, AgentStatus::Working, "b"),
            None
        );
        assert_eq!(
            toast_for(AgentStatus::Blocked, AgentStatus::Blocked, "b"),
            None
        );
    }

    #[test]
    fn shown_agent_comes_from_agent_keys_only() {
        assert_eq!(agent_terminal_id_from_key("agent:t1"), Some("t1"));
        assert_eq!(agent_terminal_id_from_key(TUI_KEY), None);
        assert_eq!(agent_terminal_id_from_key("agent:"), None);
    }

    #[test]
    fn send_params_strip_terminal_controls() {
        let params = send_params("t1", "a\u{1b}[31mb\r\nc\u{200b}d\u{7}");
        assert_eq!(params["text"], "ab\ncd");
    }

    #[test]
    fn send_params_target_the_terminal() {
        assert_eq!(
            send_params("t1", "fn main() {}"),
            serde_json::json!({ "target": "t1", "text": "fn main() {}" })
        );
    }

    #[test]
    fn attach_task_runs_herdr_agent_attach() {
        let task = attach_task("term_abc", "builder");
        assert_eq!(task.command.as_deref(), Some("herdr"));
        assert_eq!(task.args, vec!["agent", "attach", "term_abc"]);
        assert_eq!(task.label, "herd: builder");
        assert!(task.use_new_terminal);
    }
}

/// Status bar button with the ram icon that opens the Herd view.
pub struct HerdButton {
    workspace: WeakEntity<Workspace>,
    state: Entity<HerdState>,
    _subscription: Subscription,
}

impl HerdButton {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let state = HerdState::global(cx);
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            workspace: workspace.weak_handle(),
            state,
            _subscription: subscription,
        }
    }
}

impl Render for HerdButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let blocked = state
            .workspaces
            .iter()
            .flat_map(|workspace| workspace.agents.iter())
            .filter(|agent| agent.status == AgentStatus::Blocked)
            .count();
        let connected = matches!(state.connection, Connection::Connected);
        let label = if blocked > 0 {
            format!("Herd · {blocked} waiting")
        } else {
            "Herd".to_string()
        };
        let workspace = self.workspace.clone();
        ui::IconButton::new("herd-status", IconName::HerdrInitial)
            .icon_color(if blocked > 0 {
                Color::Warning
            } else if connected {
                Color::Accent
            } else {
                Color::Muted
            })
            .aria_label("Herd")
            .tooltip(ui::Tooltip::text(label))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        HerdView::open(workspace, None, window, cx);
                    })
                    .log_err();
            })
    }
}

impl workspace::StatusItemView for HerdButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}
