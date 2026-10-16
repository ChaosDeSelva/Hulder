use collections::HashMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub params: serde_json::Value,
}

impl Request {
    pub fn new(id: u64, method: &str, params: serde_json::Value) -> Self {
        Self {
            id: id.to_string(),
            method: method.to_string(),
            params,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    pub id: String,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    #[serde(other)]
    Unknown,
}


#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub pane_count: u64,
    #[serde(default)]
    pub tab_count: u64,
    #[serde(default)]
    pub active_tab_id: String,
    #[serde(default)]
    pub agent_status: AgentStatus,
}

impl WorkspaceInfo {
    /// The label, or the workspace id when herdr sends null.
    pub fn label_or_id(&self) -> String {
        self.label
            .clone()
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| self.workspace_id.clone())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorktreeInfo {
    pub path: String,
    #[serde(default)]
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_linked_worktree: bool,
    #[serde(default)]
    pub open_workspace_id: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub pane_id: String,
    #[serde(default)]
    pub tab_id: String,
    pub workspace_id: String,
    pub terminal_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub display_agent: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Subscription {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// Every subscription type Hulder needs. Names use dots on the wire.
pub const SUBSCRIPTIONS: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.closed",
    "workspace.renamed",
    "workspace.focused",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
    "pane.created",
    "pane.closed",
    "pane.updated",
    "pane.focused",
    "pane.agent_detected",
    "tab.closed",
    // `pane.agent_status_changed` needs a pane_id filter, so status changes are
    // derived from `pane.updated`, which is global and carries agent_status.
];

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub event: String,
    pub data: EventData,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    WorkspaceCreated {
        workspace: WorkspaceInfo,
    },
    WorkspaceUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceClosed {
        workspace_id: String,
    },
    WorkspaceRenamed {
        workspace_id: String,
        label: String,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    WorktreeCreated {
        #[serde(default)]
        worktree: Option<WorktreeInfo>,
    },
    WorktreeOpened {
        #[serde(default)]
        worktree: Option<WorktreeInfo>,
    },
    WorktreeRemoved {
        #[serde(default)]
        path: Option<String>,
    },
    PaneCreated {
        pane: AgentInfo,
    },
    PaneClosed {
        pane_id: String,
        workspace_id: String,
    },
    PaneUpdated {
        pane: AgentInfo,
    },
    PaneFocused {
        pane_id: String,
        workspace_id: String,
    },
    TabClosed {
        tab_id: String,
        workspace_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        workspace_id: String,
        #[serde(default)]
        agent: Option<String>,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        workspace_id: String,
        #[serde(default)]
        agent: Option<String>,
        agent_status: AgentStatus,
        #[serde(default)]
        display_agent: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        state_labels: HashMap<String, String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Event(Event),
    Response(Response),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_with_params_always_present() {
        let request = Request::new(7, "ping", serde_json::json!({}));
        let text = serde_json::to_string(&request).expect("serialize");
        assert_eq!(text, r#"{"id":"7","method":"ping","params":{}}"#);
    }

    #[test]
    fn parses_success_error_and_event_lines() {
        let ok: Incoming = serde_json::from_str(
            r#"{"id":"1","result":{"type":"pong","version":"0.7.4","protocol":16}}"#,
        )
        .expect("ok line");
        assert!(
            matches!(ok, Incoming::Response(Response { ref id, result: Some(_), error: None }) if id == "1")
        );

        let err: Incoming = serde_json::from_str(
            r#"{"id":"","error":{"code":"invalid_request","message":"missing field `params`"}}"#,
        )
        .expect("err line");
        assert!(
            matches!(err, Incoming::Response(Response { error: Some(ref e), .. }) if e.code == "invalid_request")
        );

        let event: Incoming = serde_json::from_str(
            r#"{"event":"pane_agent_status_changed","data":{"type":"pane_agent_status_changed","pane_id":"w1:p2","workspace_id":"w1","agent":"claude","agent_status":"blocked","display_agent":null,"title":null,"state_labels":{}}}"#,
        )
        .expect("event line");
        match event {
            Incoming::Event(Event {
                data: EventData::PaneAgentStatusChanged { pane_id, agent_status, .. },
                ..
            }) => {
                assert_eq!(pane_id, "w1:p2");
                assert_eq!(agent_status, AgentStatus::Blocked);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unknown_agent_status_value_parses_as_unknown() {
        let status: AgentStatus = serde_json::from_str(r#""error""#).expect("parse");
        assert_eq!(status, AgentStatus::Unknown);
        let info: AgentInfo = serde_json::from_str(
            r#"{"pane_id":"w1:p1","workspace_id":"w1","terminal_id":"t1","agent_status":"working"}"#,
        )
        .expect("minimal agent info parses");
        assert_eq!(info.agent_status, AgentStatus::Working);
        let workspace: WorkspaceInfo =
            serde_json::from_str(r#"{"workspace_id":"w1","label":null}"#).expect("minimal workspace");
        assert_eq!(workspace.label_or_id(), "w1");
    }

    #[test]
    fn real_snapshot_deserializes() {
        let text = include_str!("../fixtures/snapshot.json");
        let snapshot: serde_json::Value = serde_json::from_str(text).expect("json");
        let workspaces: Vec<WorkspaceInfo> =
            serde_json::from_value(snapshot["snapshot"]["workspaces"].clone())
                .expect("workspaces");
        let agents: Vec<AgentInfo> =
            serde_json::from_value(snapshot["snapshot"]["agents"].clone()).expect("agents");
        assert!(!workspaces.is_empty());
        assert!(!agents.is_empty());
    }

    #[test]
    fn real_events_deserialize() {
        for line in include_str!("../fixtures/events.jsonl")
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            let parsed: Incoming = serde_json::from_str(line).expect(line);
            assert!(matches!(parsed, Incoming::Event(_)), "{line}");
        }
    }
}
