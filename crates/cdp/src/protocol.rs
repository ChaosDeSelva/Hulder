use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl Request {
    pub fn new(
        id: u64,
        method: &str,
        params: serde_json::Value,
        session_id: Option<String>,
    ) -> Self {
        Self {
            id,
            method: method.to_string(),
            params,
            session_id,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorBody {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<ErrorBody>,
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Response(Response),
    Event(Event),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub target_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub attached: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_omits_session_id_when_absent() {
        let request = Request::new(3, "Target.getTargets", serde_json::json!({}), None);
        let text = serde_json::to_string(&request).expect("serialize");
        assert_eq!(text, r#"{"id":3,"method":"Target.getTargets","params":{}}"#);

        let request = Request::new(4, "DOM.enable", serde_json::json!({}), Some("S1".into()));
        let text = serde_json::to_string(&request).expect("serialize");
        assert_eq!(
            text,
            r#"{"id":4,"method":"DOM.enable","params":{},"sessionId":"S1"}"#
        );
    }

    #[test]
    fn parses_result_error_and_event_frames() {
        let ok: Incoming =
            serde_json::from_str(r#"{"id":1,"result":{"targetInfos":[]}}"#).expect("ok");
        assert!(matches!(
            ok,
            Incoming::Response(Response {
                id: 1,
                result: Some(_),
                error: None,
                ..
            })
        ));

        let err: Incoming = serde_json::from_str(
            r#"{"id":2,"error":{"code":-32601,"message":"'X' wasn't found"}}"#,
        )
        .expect("err");
        assert!(matches!(
            err,
            Incoming::Response(Response { id: 2, error: Some(ref e), .. }) if e.code == -32601
        ));

        let event: Incoming = serde_json::from_str(
            r#"{"method":"Overlay.inspectNodeRequested","params":{"backendNodeId":42},"sessionId":"S1"}"#,
        )
        .expect("event");
        match event {
            Incoming::Event(Event {
                method,
                params,
                session_id,
            }) => {
                assert_eq!(method, "Overlay.inspectNodeRequested");
                assert_eq!(params["backendNodeId"], 42);
                assert_eq!(session_id.as_deref(), Some("S1"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
