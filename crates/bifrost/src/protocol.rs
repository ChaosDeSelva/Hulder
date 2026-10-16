use serde::{Deserialize, Serialize};

/// One request line on the socket, the same shape herdr uses, plus the
/// relay's working directory so the call lands in the window whose project
/// contains it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Request {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

impl RpcError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new("invalid_params", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self::new("internal", format!("{error:#}"))
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(error)
    }
}

impl From<std::io::Error> for RpcError {
    fn from(error: std::io::Error) -> Self {
        Self::internal(error)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Response {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: impl Into<String>, result: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: impl Into<String>, error: RpcError) -> Self {
        Self {
            id: id.into(),
            result: None,
            error: Some(error),
        }
    }

    pub fn from_result(id: impl Into<String>, result: Result<serde_json::Value, RpcError>) -> Self {
        match result {
            Ok(value) => Self::ok(id, value),
            Err(error) => Self::err(id, error),
        }
    }

    pub fn to_line(&self) -> String {
        match serde_json::to_string(self) {
            Ok(mut line) => {
                line.push('\n');
                line
            }
            // A Response only holds strings and JSON values, so serializing cannot fail;
            // answering something is still better than closing the connection.
            Err(error) => format!(
                "{{\"id\":\"{}\",\"error\":{{\"code\":\"internal\",\"message\":\"{error}\"}}}}\n",
                self.id
            ),
        }
    }
}

/// Parses one request line. A line that is not a request yields an error response
/// addressed to whatever id could be recovered, so the client never waits forever.
pub fn parse_request(line: &str) -> Result<Request, Box<Response>> {
    serde_json::from_str::<Request>(line).map_err(|error| {
        let id = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|value| value.get("id").cloned())
            .map(|id| match id {
                serde_json::Value::String(id) => id,
                other => other.to_string(),
            })
            .unwrap_or_default();
        Box::new(Response::err(
            id,
            RpcError::new("invalid_request", format!("unparseable request: {error}")),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_and_defaults_params() {
        let request = parse_request(r#"{"id":"1","method":"project_root"}"#).expect("request");
        assert_eq!(request.id, "1");
        assert_eq!(request.method, "project_root");
        assert!(request.params.is_null());
        assert_eq!(request.cwd, None);
        let request = parse_request(r#"{"id":"1","method":"project_root","cwd":"/home/me/work"}"#)
            .expect("request with cwd");
        assert_eq!(request.cwd.as_deref(), Some("/home/me/work"));
    }

    #[test]
    fn unparseable_line_answers_with_recovered_id() {
        let response = parse_request(r#"{"id":"9","params":{}}"#).expect_err("missing method");
        assert_eq!(response.id, "9");
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid_request")
        );
        let response = parse_request("not json").expect_err("garbage");
        assert_eq!(response.id, "");
    }

    #[test]
    fn response_lines_match_the_herdr_shape() {
        let ok = Response::ok("1", serde_json::json!({"root": "/p"}));
        assert_eq!(
            ok.to_line(),
            "{\"id\":\"1\",\"result\":{\"root\":\"/p\"}}\n"
        );
        let err = Response::err("2", RpcError::new("unknown_method", "nope"));
        assert_eq!(
            err.to_line(),
            "{\"id\":\"2\",\"error\":{\"code\":\"unknown_method\",\"message\":\"nope\"}}\n"
        );
    }
}
