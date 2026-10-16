//! Bifröst's view of the Raven: list requests and read the redacted last
//! response. Both are reads of `.hulder/ravens/` and the in-memory cache;
//! neither can send anything.

use crate::dispatch::{string_param, unknown_method};
use crate::protocol::RpcError;
use serde_json::{Value, json};
use std::path::Path;

pub const RAVEN_METHODS: &[&str] = &["raven_list", "raven_last_response"];

pub fn dispatch_raven(root: &Path, method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        "raven_list" => {
            let collections = raven::store::list_collections(root)?;
            let requests: Vec<Value> = collections
                .iter()
                .flat_map(|collection| {
                    collection.requests.iter().map(move |request| {
                        json!({
                            "collection": collection.name,
                            "file": format!(".hulder/ravens/{}.http", collection.name),
                            "name": request.name,
                            "method": request.method,
                            "url": raven::log::log_url(&request.url),
                        })
                    })
                })
                .collect();
            Ok(json!({ "requests": requests }))
        }
        "raven_last_response" => {
            let name = string_param(params, "name")?;
            if let Some(cached) = raven::last_response(root, name) {
                return Ok(json!({ "response": cached.for_agents(), "source": "cache" }));
            }
            match raven::log::last_log_line(root, name)? {
                Some(line) => {
                    let value = serde_json::to_value(line).map_err(RpcError::internal)?;
                    Ok(json!({ "response": value, "source": "log" }))
                }
                None => Err(RpcError::not_found(format!(
                    "no response recorded for request {name:?}"
                ))),
            }
        }
        other => Err(unknown_method(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{Routed, route};

    #[test]
    fn raven_methods_route_as_raven() {
        for method in RAVEN_METHODS {
            assert_eq!(route(method), Routed::Raven, "{method}");
        }
    }

    #[test]
    fn lists_requests_and_serves_only_redacted_responses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let listed = dispatch_raven(root, "raven_list", &json!({})).expect("empty list");
        assert_eq!(listed["requests"].as_array().map(Vec::len), Some(0));

        std::fs::create_dir_all(root.join(".hulder/ravens")).expect("mkdir");
        std::fs::write(
            root.join(".hulder/ravens/users.http"),
            "### list users\nGET {{base}}/users?api_key=k-123\nAuthorization: Bearer {{token}}\n",
        )
        .expect("write");
        let listed = dispatch_raven(root, "raven_list", &json!({})).expect("list");
        assert_eq!(listed["requests"][0]["name"], "list users");
        assert_eq!(listed["requests"][0]["method"], "GET");
        assert_eq!(listed["requests"][0]["collection"], "users");
        assert!(
            listed.to_string().contains("{{base}}"),
            "placeholders stay unresolved"
        );
        assert!(
            !listed.to_string().contains("token"),
            "headers are not listed"
        );
        assert!(
            !listed.to_string().contains("k-123"),
            "query string stripped"
        );

        let missing = dispatch_raven(
            root,
            "raven_last_response",
            &json!({ "name": "list users" }),
        )
        .expect_err("nothing yet");
        assert_eq!(missing.code, "not_found");
        assert_eq!(
            dispatch_raven(root, "raven_last_response", &json!({}))
                .expect_err("no name")
                .code,
            "invalid_params"
        );

        let display = raven::Request {
            name: "list users".into(),
            method: "GET".into(),
            url: "https://bob:pw@api.test/users?api_key=k-123".into(),
            headers: vec![
                ("Authorization".into(), "Bearer visible?".into()),
                ("Cookie".into(), "session=c00kie".into()),
            ],
            body: String::new(),
        };
        let response = raven::send::Response {
            status: 200,
            headers: vec![
                ("set-cookie".into(), "s=1".into()),
                ("x-request-id".into(), "r1".into()),
            ],
            body: b"{}".to_vec(),
            truncated: false,
            duration: std::time::Duration::from_millis(3),
            request_bytes: 0,
        };
        let set = hoard::redaction_set(&[], &[]);
        raven::log::remember_last_response(
            root,
            raven::RedactedResponse::new(&display, &response, &set, "t".into()),
        );
        let found = dispatch_raven(
            root,
            "raven_last_response",
            &json!({ "name": "list users" }),
        )
        .expect("cached");
        assert_eq!(found["source"], "cache");
        let text = found.to_string();
        for secret in [
            "visible?",
            "s=1",
            "set-cookie",
            "c00kie",
            "k-123",
            "bob",
            "pw@",
            "Authorization",
            "request_headers",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert_eq!(found["response"]["url"], "https://api.test/users");
        assert_eq!(found["response"]["headers"][0][0], "x-request-id");
        assert_eq!(
            found["response"]["headers"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(
            dispatch_raven(root, "raven_send", &json!({}))
                .expect_err("no such tool")
                .code,
            "unknown_method"
        );
    }
}
