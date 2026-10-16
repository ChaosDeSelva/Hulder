use crate::client::{HerdrClient, socket_path};
use crate::protocol::{AgentInfo, AgentStatus, EventData, WorkspaceInfo, WorktreeInfo};
use anyhow::Result;
use futures::StreamExt;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use std::{path::Path, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    Disconnected { reason: String },
    Connecting,
    Connected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub pane_id: String,
    pub tab_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub status: AgentStatus,
    pub cwd: Option<String>,
}

impl From<AgentInfo> for AgentRow {
    fn from(info: AgentInfo) -> Self {
        Self {
            pane_id: info.pane_id,
            tab_id: info.tab_id,
            terminal_id: info.terminal_id,
            workspace_id: info.workspace_id,
            agent: info.agent,
            display_agent: info.display_agent,
            name: info.name,
            title: info.title,
            terminal_title_stripped: info.terminal_title_stripped,
            status: info.agent_status,
            cwd: info.cwd,
        }
    }
}

impl AgentRow {
    pub fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.display_agent.clone())
            .or_else(|| self.terminal_title_stripped.clone())
            .or_else(|| self.agent.clone())
            .unwrap_or_else(|| self.pane_id.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub id: String,
    pub label: String,
    pub focused: bool,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub agents: Vec<AgentRow>,
}

pub enum HerdEvent {
    AgentStatusChanged {
        agent: AgentRow,
        previous: AgentStatus,
    },
}

pub struct HerdState {
    pub connection: Connection,
    pub workspaces: Vec<WorkspaceRow>,
    client: Option<HerdrClient>,
    _connect_task: Option<Task<()>>,
}

impl EventEmitter<HerdEvent> for HerdState {}

struct GlobalHerdState(Entity<HerdState>);
impl Global for GlobalHerdState {}

pub fn init(cx: &mut App) {
    let state = cx.new(HerdState::new);
    cx.set_global(GlobalHerdState(state));
}

/// Installs the global without ever touching the herdr socket, for test apps: the reconnect
/// loop does real socket IO, which the deterministic test scheduler rejects.
pub fn init_disconnected(cx: &mut App) {
    let state = cx.new(|_| HerdState::empty());
    cx.set_global(GlobalHerdState(state));
}

type Snapshot = (Vec<WorkspaceInfo>, Vec<WorktreeInfo>, Vec<AgentInfo>);

/// What applying an event did, so callers can skip redraws for no-ops.
pub struct Applied {
    pub event: Option<HerdEvent>,
    pub changed: bool,
}

impl HerdState {
    pub fn global(cx: &App) -> Entity<HerdState> {
        cx.global::<GlobalHerdState>().0.clone()
    }

    /// `None` when `init` never ran, as in tests.
    pub fn try_global(cx: &App) -> Option<Entity<HerdState>> {
        cx.try_global::<GlobalHerdState>()
            .map(|global| global.0.clone())
    }

    /// The herdr agent working in the worktree at `path`, as `(label, status)`.
    /// A workspace whose worktree is `path` but that has no agent yet answers
    /// with its own label and `AgentStatus::Unknown`.
    pub fn agent_for_path(&self, path: &Path) -> Option<(String, AgentStatus)> {
        let workspace = self.workspaces.iter().find(|workspace| {
            workspace
                .worktree_path
                .as_deref()
                .is_some_and(|worktree_path| Path::new(worktree_path) == path)
        })?;
        let agent = workspace
            .agents
            .iter()
            .find(|agent| agent.name.is_some())
            .or_else(|| workspace.agents.first());
        Some(match agent {
            Some(agent) => (agent.label(), agent.status),
            None => (workspace.label.clone(), AgentStatus::Unknown),
        })
    }

    pub fn empty() -> Self {
        Self {
            connection: Connection::Disconnected {
                reason: "not started".into(),
            },
            workspaces: Vec::new(),
            client: None,
            _connect_task: None,
        }
    }

    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self::empty();
        this.request_reconnect(cx);
        this
    }

    pub fn client(&self) -> Option<HerdrClient> {
        self.client.clone()
    }

    pub fn request_reconnect(&mut self, cx: &mut Context<Self>) {
        self.connection = Connection::Connecting;
        cx.notify();
        self._connect_task = Some(cx.spawn(async move |this, cx| {
            let mut delay = Duration::from_millis(500);
            loop {
                let connect_started = std::time::Instant::now();
                match HerdrClient::connect(socket_path()).await {
                    Ok((client, mut events, reader)) => {
                        // Held so that dropping this loop closes the subscription socket.
                        let _reader = reader;
                        let loaded = Self::load_snapshot(&client).await;
                        if let Ok((workspaces, _, agents)) = &loaded {
                            log::info!(
                                "herdr: connected in {:?} ({} workspaces, {} agents)",
                                connect_started.elapsed(),
                                workspaces.len(),
                                agents.len()
                            );
                        }
                        // worktree.list runs git and takes 100-200 ms; fetch it after the
                        // list is already on screen.
                        let worktree_client = client.clone();
                        let worktree_state = this.clone();
                        cx.spawn(async move |cx| {
                            let Ok(value) = worktree_client
                                .request("worktree.list", serde_json::json!({}))
                                .await
                            else {
                                return;
                            };
                            let worktrees: Vec<WorktreeInfo> =
                                serde_json::from_value(value["worktrees"].clone())
                                    .unwrap_or_default();
                            worktree_state
                                .update(cx, |this, cx| {
                                    if this.apply_worktrees(&worktrees) {
                                        cx.notify();
                                    }
                                })
                                .ok();
                        })
                        .detach();
                        let applied = this.update(cx, |this, cx| match loaded {
                            Ok((workspaces, worktrees, agents)) => {
                                this.client = Some(client.clone());
                                this.apply_snapshot(workspaces, worktrees, agents);
                                this.connection = Connection::Connected;
                                cx.notify();
                                Ok(())
                            }
                            Err(error) => Err(error),
                        });
                        match applied {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                log::warn!("herdr: snapshot failed: {error}");
                                drop(events);
                                if this
                                    .update(cx, |this, cx| {
                                        this.connection = Connection::Disconnected {
                                            reason: format!("snapshot failed: {error}"),
                                        };
                                        cx.notify();
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                                cx.background_executor().timer(delay).await;
                                delay = (delay * 2).min(Duration::from_secs(10));
                                continue;
                            }
                            Err(_) => return,
                        }
                        while let Some(event) = events.next().await {
                            let alive = this.update(cx, |this, cx| {
                                let applied = this.apply_event(&event.data);
                                if let Some(herd_event) = applied.event {
                                    cx.emit(herd_event);
                                }
                                if applied.changed {
                                    cx.notify();
                                }
                            });
                            if alive.is_err() {
                                return;
                            }
                        }
                        let alive = this.update(cx, |this, cx| {
                            this.client = None;
                            this.workspaces.clear();
                            this.connection = Connection::Disconnected {
                                reason: "herdr closed the connection".into(),
                            };
                            cx.notify();
                        });
                        if alive.is_err() {
                            return;
                        }
                        delay = Duration::from_millis(500);
                    }
                    Err(error) => {
                        let alive = this.update(cx, |this, cx| {
                            this.connection = Connection::Disconnected {
                                reason: error.to_string(),
                            };
                            cx.notify();
                        });
                        if alive.is_err() {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(delay).await;
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        }));
    }

    async fn load_snapshot(client: &HerdrClient) -> Result<Snapshot> {
        let snapshot = client
            .request("session.snapshot", serde_json::json!({}))
            .await?;
        let workspaces = serde_json::from_value(snapshot["snapshot"]["workspaces"].clone())?;
        let agents = serde_json::from_value(snapshot["snapshot"]["agents"].clone())?;
        Ok((workspaces, Vec::new(), agents))
    }

    pub fn apply_snapshot(
        &mut self,
        workspaces: Vec<WorkspaceInfo>,
        worktrees: Vec<WorktreeInfo>,
        agents: Vec<AgentInfo>,
    ) {
        self.workspaces = workspaces
            .into_iter()
            .map(|workspace| {
                let worktree = worktrees.iter().find(|worktree| {
                    worktree.open_workspace_id.as_deref() == Some(&workspace.workspace_id)
                });
                WorkspaceRow {
                    id: workspace.workspace_id.clone(),
                    label: workspace.label_or_id(),
                    focused: workspace.focused,
                    worktree_path: worktree.map(|worktree| worktree.path.clone()),
                    branch: worktree.and_then(|worktree| worktree.branch.clone()),
                    agents: Vec::new(),
                }
            })
            .collect();
        for agent in agents {
            if let Some(row) = self
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == agent.workspace_id)
            {
                row.agents.push(AgentRow::from(agent));
            }
        }
    }

    /// Attaches worktree paths and branches to workspaces; true when anything changed.
    pub fn apply_worktrees(&mut self, worktrees: &[WorktreeInfo]) -> bool {
        let mut changed = false;
        for worktree in worktrees {
            let Some(id) = &worktree.open_workspace_id else {
                continue;
            };
            if let Some(row) = self.workspace_mut(id) {
                let path = Some(worktree.path.clone());
                if row.worktree_path != path || row.branch != worktree.branch {
                    row.worktree_path = path;
                    row.branch = worktree.branch.clone();
                    changed = true;
                }
            }
        }
        changed
    }

    fn workspace_mut(&mut self, workspace_id: &str) -> Option<&mut WorkspaceRow> {
        self.workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
    }

    pub fn apply_event(&mut self, event: &EventData) -> Applied {
        let before = self.workspaces.clone();
        let herd_event = self.apply_event_inner(event);
        Applied {
            event: herd_event,
            changed: before != self.workspaces,
        }
    }

    fn apply_event_inner(&mut self, event: &EventData) -> Option<HerdEvent> {
        match event {
            EventData::WorkspaceCreated { workspace }
            | EventData::WorkspaceUpdated { workspace } => {
                match self.workspace_mut(&workspace.workspace_id) {
                    Some(row) => {
                        row.label = workspace.label_or_id();
                        row.focused = workspace.focused;
                    }
                    None => self.workspaces.push(WorkspaceRow {
                        id: workspace.workspace_id.clone(),
                        label: workspace.label_or_id(),
                        focused: workspace.focused,
                        worktree_path: None,
                        branch: None,
                        agents: Vec::new(),
                    }),
                }
                None
            }
            EventData::WorkspaceClosed { workspace_id } => {
                self.workspaces
                    .retain(|workspace| &workspace.id != workspace_id);
                None
            }
            EventData::WorkspaceRenamed {
                workspace_id,
                label,
            } => {
                if let Some(row) = self.workspace_mut(workspace_id) {
                    row.label = label.clone();
                }
                None
            }
            EventData::WorkspaceFocused { workspace_id } => {
                for row in &mut self.workspaces {
                    row.focused = &row.id == workspace_id;
                }
                None
            }
            EventData::PaneCreated { pane } | EventData::PaneUpdated { pane } => {
                let row = self.workspace_mut(&pane.workspace_id)?;
                // Only panes running an agent are listed; plain shells are not.
                if pane.agent.is_none() {
                    row.agents.retain(|agent| agent.pane_id != pane.pane_id);
                    return None;
                }
                let new_row = AgentRow::from(pane.clone());
                match row
                    .agents
                    .iter_mut()
                    .find(|agent| agent.pane_id == pane.pane_id)
                {
                    Some(existing) => {
                        let previous = existing.status;
                        *existing = new_row;
                        if previous != existing.status {
                            return Some(HerdEvent::AgentStatusChanged {
                                agent: existing.clone(),
                                previous,
                            });
                        }
                    }
                    None => row.agents.push(new_row),
                }
                None
            }
            EventData::TabClosed {
                tab_id,
                workspace_id,
            } => {
                if let Some(row) = self.workspace_mut(workspace_id) {
                    row.agents.retain(|agent| &agent.tab_id != tab_id);
                }
                None
            }
            EventData::PaneClosed {
                pane_id,
                workspace_id,
            } => {
                if let Some(row) = self.workspace_mut(workspace_id) {
                    row.agents.retain(|agent| &agent.pane_id != pane_id);
                }
                None
            }
            EventData::PaneAgentStatusChanged {
                pane_id,
                workspace_id,
                agent,
                agent_status,
                display_agent,
                title,
                ..
            } => {
                let row = self.workspace_mut(workspace_id)?;
                let existing = row
                    .agents
                    .iter_mut()
                    .find(|existing| &existing.pane_id == pane_id)?;
                let previous = existing.status;
                existing.status = *agent_status;
                if agent.is_some() {
                    existing.agent = agent.clone();
                }
                if display_agent.is_some() {
                    existing.display_agent = display_agent.clone();
                }
                if title.is_some() {
                    existing.title = title.clone();
                }
                Some(HerdEvent::AgentStatusChanged {
                    agent: existing.clone(),
                    previous,
                })
            }
            EventData::PaneAgentDetected {
                pane_id,
                workspace_id,
                agent,
            } => {
                let row = self.workspace_mut(workspace_id)?;
                if let Some(existing) = row
                    .agents
                    .iter_mut()
                    .find(|existing| &existing.pane_id == pane_id)
                {
                    existing.agent = agent.clone();
                }
                None
            }
            EventData::WorktreeOpened {
                worktree: Some(worktree),
            }
            | EventData::WorktreeCreated {
                worktree: Some(worktree),
            } => {
                if let Some(id) = &worktree.open_workspace_id
                    && let Some(row) = self.workspace_mut(id)
                {
                    row.worktree_path = Some(worktree.path.clone());
                    row.branch = worktree.branch.clone();
                }
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AgentInfo, AgentStatus, EventData, WorkspaceInfo, WorktreeInfo};
    use std::path::Path;

    fn agent(pane: &str, ws: &str, status: AgentStatus) -> AgentInfo {
        AgentInfo {
            pane_id: pane.into(),
            tab_id: format!("{ws}:t1"),
            workspace_id: ws.into(),
            terminal_id: format!("term_{pane}"),
            agent: Some("claude".into()),
            display_agent: None,
            name: None,
            title: None,
            terminal_title_stripped: Some("plan".into()),
            agent_status: status,
            focused: false,
            cwd: Some("/tmp".into()),
        }
    }

    fn workspace(id: &str, label: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            workspace_id: id.into(),
            number: 1,
            label: Some(label.into()),
            focused: false,
            pane_count: 1,
            tab_count: 1,
            active_tab_id: format!("{id}:t1"),
            agent_status: AgentStatus::Idle,
        }
    }

    #[test]
    fn snapshot_groups_agents_under_workspaces_and_attaches_worktrees() {
        let mut state = HerdState::empty();
        let worktree = WorktreeInfo {
            path: "/repo".into(),
            branch: Some("main".into()),
            is_bare: false,
            is_detached: false,
            is_linked_worktree: false,
            open_workspace_id: Some("w1".into()),
            label: "repo".into(),
        };
        state.apply_snapshot(
            vec![workspace("w1", "repo")],
            vec![worktree],
            vec![agent("w1:p1", "w1", AgentStatus::Working)],
        );
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].worktree_path.as_deref(), Some("/repo"));
        assert_eq!(state.workspaces[0].agents[0].status, AgentStatus::Working);
    }

    #[test]
    fn status_event_updates_agent_and_reports_previous() {
        let mut state = HerdState::empty();
        state.apply_snapshot(
            vec![workspace("w1", "repo")],
            vec![],
            vec![agent("w1:p1", "w1", AgentStatus::Working)],
        );
        let event = EventData::PaneAgentStatusChanged {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            agent: Some("claude".into()),
            agent_status: AgentStatus::Blocked,
            display_agent: None,
            title: None,
            state_labels: Default::default(),
        };
        let Some(HerdEvent::AgentStatusChanged { agent, previous }) =
            state.apply_event(&event).event
        else {
            panic!("expected a status change event");
        };
        assert_eq!(previous, AgentStatus::Working);
        assert_eq!(agent.status, AgentStatus::Blocked);
    }

    #[test]
    fn pane_updated_with_new_status_reports_change_once() {
        let mut state = HerdState::empty();
        state.apply_snapshot(
            vec![workspace("w1", "repo")],
            vec![],
            vec![agent("w1:p1", "w1", AgentStatus::Working)],
        );
        let blocked = EventData::PaneUpdated {
            pane: agent("w1:p1", "w1", AgentStatus::Blocked),
        };
        let Some(HerdEvent::AgentStatusChanged { previous, agent }) =
            state.apply_event(&blocked).event
        else {
            panic!("expected a status change");
        };
        assert_eq!(previous, AgentStatus::Working);
        assert_eq!(agent.status, AgentStatus::Blocked);
        let again = state.apply_event(&blocked);
        assert!(
            again.event.is_none() && !again.changed,
            "same status again is not a change"
        );
    }

    #[test]
    fn panes_without_an_agent_are_not_listed() {
        let mut state = HerdState::empty();
        state.apply_snapshot(vec![workspace("w1", "repo")], vec![], vec![]);
        let mut shell = agent("w1:p9", "w1", AgentStatus::Unknown);
        shell.agent = None;
        assert!(
            state
                .apply_event(&EventData::PaneCreated { pane: shell })
                .event
                .is_none()
        );
        assert!(state.workspaces[0].agents.is_empty());
    }

    #[test]
    fn tab_closed_removes_its_agents() {
        let mut state = HerdState::empty();
        state.apply_snapshot(
            vec![workspace("w1", "repo")],
            vec![],
            vec![agent("w1:p1", "w1", AgentStatus::Working)],
        );
        assert_eq!(state.workspaces[0].agents.len(), 1);
        state.apply_event(&EventData::TabClosed {
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
        });
        assert!(state.workspaces[0].agents.is_empty());
    }

    #[test]
    fn events_for_unknown_workspace_are_ignored() {
        let mut state = HerdState::empty();
        let event = EventData::PaneCreated {
            pane: agent("w9:p1", "w9", AgentStatus::Idle),
        };
        let applied = state.apply_event(&event);
        assert!(applied.event.is_none() && !applied.changed);
        assert!(state.workspaces.is_empty());
    }

    #[test]
    fn label_falls_back_in_order() {
        let mut row = AgentRow::from(agent("w1:p1", "w1", AgentStatus::Idle));
        row.name = None;
        row.display_agent = None;
        row.title = None;
        assert_eq!(row.label(), "plan");
        row.display_agent = Some("Claude".into());
        assert_eq!(row.label(), "Claude");
        row.name = Some("builder".into());
        assert_eq!(row.label(), "builder");
    }

    #[test]
    fn agent_for_path_matches_a_workspace_worktree_and_prefers_a_named_agent() {
        let mut state = HerdState::empty();
        let worktree = WorktreeInfo {
            path: "/repo/.claude/worktrees/agent-abc".into(),
            branch: Some("feat".into()),
            is_bare: false,
            is_detached: false,
            is_linked_worktree: true,
            open_workspace_id: Some("w1".into()),
            label: "agent-abc".into(),
        };
        let mut named = agent("w1:p2", "w1", AgentStatus::Blocked);
        named.name = Some("builder".into());
        state.apply_snapshot(
            vec![workspace("w1", "feature work"), workspace("w2", "idle")],
            vec![worktree],
            vec![agent("w1:p1", "w1", AgentStatus::Working), named],
        );

        let found = state.agent_for_path(Path::new("/repo/.claude/worktrees/agent-abc"));
        assert_eq!(found, Some(("builder".to_string(), AgentStatus::Blocked)));
        assert_eq!(state.agent_for_path(Path::new("/elsewhere")), None);
    }

    #[test]
    fn agent_for_path_falls_back_to_the_workspace_label_without_agents() {
        let mut state = HerdState::empty();
        let worktree = WorktreeInfo {
            path: "/repo/wt".into(),
            branch: None,
            is_bare: false,
            is_detached: true,
            is_linked_worktree: true,
            open_workspace_id: Some("w1".into()),
            label: "wt".into(),
        };
        state.apply_snapshot(vec![workspace("w1", "quiet")], vec![worktree], Vec::new());
        assert_eq!(
            state.agent_for_path(Path::new("/repo/wt")),
            Some(("quiet".to_string(), AgentStatus::Unknown))
        );
    }
}
