//! What Raven writes down after a send: one metadata line in `log.jsonl`
//! (no headers, no body), a redacted copy of the last response per request
//! in memory for Bifröst, and "Save as Clipping" files. Everything here
//! takes the `display` request from `send::Prepared`, never the wire copy.

use crate::collection::{Request, serialize_request};
use crate::send::{Response, pretty_body};
use crate::store;
use anyhow::Result;
use chrono::{DateTime, Local};
use clippings::{ClippingDraft, ClippingStore, Kind};
use fs::Fs;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const REDACTED: &str = "<redacted>";
/// A clipping's response body is capped here.
pub const CLIPPING_BODY_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    pub name: String,
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub duration_ms: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The URL as the log may hold it: no query string, no fragment and no
/// userinfo, so neither an `?api_key=` nor a `user:pass@` can land on disk.
/// Works on unparsable URLs too (placeholders may remain in the display URL).
pub fn log_url(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or_default();
    match without_query.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = match rest.split_once('/') {
                Some((authority, path)) => (authority, Some(path)),
                None => (rest, None),
            };
            let host = authority.rsplit('@').next().unwrap_or(authority);
            match path {
                Some(path) => format!("{scheme}://{host}/{path}"),
                None => format!("{scheme}://{host}"),
            }
        }
        None => without_query.to_string(),
    }
}

/// The URL with a `user:pass@` dropped from its authority and nothing else
/// changed.
pub fn without_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    format!("{scheme}://{host}{tail}")
}

impl LogLine {
    pub fn for_response(display: &Request, response: &Response, at: String) -> Self {
        Self {
            name: display.name.clone(),
            method: display.method.clone(),
            url: log_url(&display.url),
            status: Some(response.status),
            duration_ms: response.duration.as_millis() as u64,
            request_bytes: response.request_bytes,
            response_bytes: response.body.len() as u64,
            at,
            error: None,
        }
    }

    /// `error` is a short category, never the error's text: an error can
    /// quote the URL or a header it was built from.
    pub fn for_error(display: &Request, duration_ms: u64, error: &str, at: String) -> Self {
        Self {
            name: display.name.clone(),
            method: display.method.clone(),
            url: log_url(&display.url),
            status: None,
            duration_ms,
            request_bytes: display.body.len() as u64,
            response_bytes: 0,
            at,
            error: Some(error.chars().take(200).collect()),
        }
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

pub fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Replaces the value of every header whose lower-cased name is in `set`.
pub fn redact(headers: &[(String, String)], set: &[String]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            if set.iter().any(|redacted| redacted.eq_ignore_ascii_case(name)) {
                (name.clone(), REDACTED.to_string())
            } else {
                (name.clone(), value.clone())
            }
        })
        .collect()
}

/// The redacted record of a request's last response, safe for Bifröst.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedactedResponse {
    pub name: String,
    pub method: String,
    pub url: String,
    pub request_headers: Vec<(String, String)>,
    pub request_body: String,
    pub status: u16,
    pub duration_ms: u64,
    pub response_bytes: u64,
    pub truncated: bool,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub at: String,
}

impl RedactedResponse {
    pub fn new(display: &Request, response: &Response, set: &[String], at: String) -> Self {
        let mut body = pretty_body(&response.body, response.content_type());
        let mut truncated = response.truncated;
        if body.len() > CLIPPING_BODY_BYTES {
            let mut cut = CLIPPING_BODY_BYTES;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
            truncated = true;
        }
        Self {
            name: display.name.clone(),
            method: display.method.clone(),
            url: display.url.clone(),
            request_headers: redact(&display.headers, set),
            request_body: display.body.clone(),
            status: response.status,
            duration_ms: response.duration.as_millis() as u64,
            response_bytes: response.body.len() as u64,
            truncated,
            headers: redact(&response.headers, set),
            body,
            at,
        }
    }
}

impl RedactedResponse {
    /// The shape handed to terminal agents over Bifröst: response headers
    /// already redacted with `Set-Cookie` removed outright, the URL without
    /// query or userinfo, no request headers, no variables.
    pub fn for_agents(&self) -> serde_json::Value {
        let headers: Vec<(String, String)> = self
            .headers
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("set-cookie"))
            .cloned()
            .collect();
        serde_json::json!({
            "name": self.name,
            "method": self.method,
            "url": log_url(&self.url),
            "status": self.status,
            "duration_ms": self.duration_ms,
            "response_bytes": self.response_bytes,
            "truncated": self.truncated,
            "headers": headers,
            "body": self.body,
            "at": self.at,
        })
    }
}

type Cache = HashMap<(PathBuf, String), RedactedResponse>;

static LAST_RESPONSES: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

/// Caches an already redacted response for `name` under `root`.
pub fn remember_last_response(root: &Path, redacted: RedactedResponse) {
    LAST_RESPONSES
        .lock()
        .insert((root.to_path_buf(), redacted.name.clone()), redacted);
}

pub fn last_response(root: &Path, name: &str) -> Option<RedactedResponse> {
    LAST_RESPONSES
        .lock()
        .get(&(root.to_path_buf(), name.to_string()))
        .cloned()
}

/// The newest log line for `name`, from `log.1.jsonl` and `log.jsonl`.
pub fn last_log_line(root: &Path, name: &str) -> Result<Option<LogLine>> {
    let lines = store::read_log_lines(root)?;
    Ok(lines
        .iter()
        .rev()
        .filter_map(|line| serde_json::from_str::<LogLine>(line).ok())
        .find(|line| line.name == name))
}

/// The body of a section 13 clipping with `kind: raven`: the request as an
/// `http` block, then the response status, headers and body. Every fence is
/// longer than any backtick run inside it, so a body cannot break out.
pub fn clipping_body(redacted: &RedactedResponse) -> String {
    let request = Request {
        name: redacted.name.clone(),
        method: redacted.method.clone(),
        // The query stays: it is the request as written, placeholders and
        // all. Userinfo is a credential and never belongs in a file.
        url: without_userinfo(&redacted.url),
        headers: redacted.request_headers.clone(),
        body: redacted.request_body.clone(),
    };
    let request_text = serialize_request(&request);
    let request_fence = clippings::fence_for(&request_text);
    let mut text = format!(
        "\n# {} {}\n\n## Request\n\n{request_fence}http\n{request_text}{request_fence}\n\n## Response\n\nStatus {} in {} ms, {} bytes{}\n\n",
        redacted.method,
        redacted.name,
        redacted.status,
        redacted.duration_ms,
        redacted.response_bytes,
        if redacted.truncated { " (body truncated)" } else { "" }
    );
    let headers_text: String = redacted
        .headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\n"))
        .collect();
    let headers_fence = clippings::fence_for(&headers_text);
    text.push_str(&format!("{headers_fence}\n{headers_text}{headers_fence}\n\n"));
    if !redacted.body.is_empty() {
        let fence = clippings::fence_for(&redacted.body);
        text.push_str(&format!("{fence}\n{}\n{fence}\n", redacted.body.trim_end()));
    }
    text
}

/// The draft the Clippings store writes: `source` is the request URL (the
/// store strips its query and userinfo), the slug comes from the request
/// name, and `request` and `status` ride along in the front matter.
pub fn clipping_draft(redacted: &RedactedResponse) -> ClippingDraft {
    ClippingDraft::text(Kind::Raven, redacted.url.clone(), clipping_body(redacted))
        .with_title(redacted.name.clone())
        .with_extra("request", redacted.name.clone())
        .with_extra("status", redacted.status.to_string())
}

/// Writes the clipping through the Clippings store under `root`.
pub async fn save_clipping(
    fs: &dyn Fs,
    root: &Path,
    redacted: &RedactedResponse,
    captured: DateTime<Local>,
) -> Result<PathBuf> {
    let saved = ClippingStore::new(root)
        .save(fs, clipping_draft(redacted), captured)
        .await?;
    Ok(saved.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn display() -> Request {
        Request {
            name: "list users".into(),
            method: "GET".into(),
            url: "https://api.test/users?key={{token}}".into(),
            headers: vec![
                ("Accept".into(), "*/*".into()),
                ("Authorization".into(), "Bearer {{token}}".into()),
                ("X-Team-Token".into(), "abc".into()),
            ],
            body: String::new(),
        }
    }

    fn response() -> Response {
        Response {
            status: 200,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("set-cookie".into(), "session=1".into()),
                ("x-request-id".into(), "r1".into()),
            ],
            body: b"{\"users\":[]}".to_vec(),
            truncated: false,
            duration: Duration::from_millis(42),
            request_bytes: 0,
        }
    }

    #[test]
    fn log_lines_carry_metadata_only() {
        let line = LogLine::for_response(&display(), &response(), "2026-10-13T10:00:00Z".into());
        let json = line.to_json().expect("json");
        assert!(json.contains("\"status\":200"));
        assert!(json.contains("\"duration_ms\":42"));
        assert!(json.contains("\"response_bytes\":12"));
        assert!(!json.contains("body"), "{json}");
        assert!(!json.contains("header"), "{json}");
        assert!(!json.contains("session"), "{json}");
        assert!(json.contains("\"url\":\"https://api.test/users\""), "query stripped: {json}");
        let failed = LogLine::for_error(&display(), 7, "boom", "t".into());
        assert_eq!(failed.status, None);
        assert_eq!(failed.error.as_deref(), Some("boom"));
        let parsed: LogLine = serde_json::from_str(&failed.to_json().expect("json")).expect("parse");
        assert_eq!(parsed, failed);
    }

    #[test]
    fn log_line_holds_no_secret_bytes() {
        let display = Request {
            name: "me".into(),
            method: "GET".into(),
            url: "https://alice:hunter2@api.test/me?api_key=sk-live-123&t={{token}}#frag".into(),
            headers: vec![
                ("Authorization".into(), "Bearer sk-live-123".into()),
                ("Cookie".into(), "session=abc".into()),
                ("X-Token".into(), "{{token}}".into()),
            ],
            body: "{\"password\":\"hunter2\"}".into(),
        };
        let mut response = response();
        response.headers.push(("set-cookie".into(), "session=def".into()));
        for json in [
            LogLine::for_response(&display, &response, "t".into()).to_json().expect("json"),
            LogLine::for_error(&display, 1, "timeout", "t".into()).to_json().expect("json"),
        ] {
            for secret in ["sk-live-123", "hunter2", "session", "{{token}}", "api_key", "alice", "Bearer", "frag"] {
                assert!(!json.contains(secret), "{secret} leaked into {json}");
            }
            assert!(json.contains("\"url\":\"https://api.test/me\""), "{json}");
        }
        assert_eq!(log_url("{{base}}/x?k=v"), "{{base}}/x");
        assert_eq!(log_url("https://u:p@h.test"), "https://h.test");
        assert_eq!(log_url("https://h.test/"), "https://h.test/");
    }

    #[test]
    fn redaction_is_case_insensitive_and_covers_profile_headers() {
        let set = hoard::redaction_set(&[], &["x-team-token".to_string()]);
        let redacted = RedactedResponse::new(&display(), &response(), &set, "t".into());
        assert_eq!(redacted.request_headers[0].1, "*/*");
        assert_eq!(redacted.request_headers[1].1, REDACTED);
        assert_eq!(redacted.request_headers[2].1, REDACTED);
        assert_eq!(redacted.headers[1], ("set-cookie".to_string(), REDACTED.to_string()));
        assert_eq!(redacted.headers[2].1, "r1");
        assert_eq!(redacted.body, "{\n  \"users\": []\n}");
        let text = serde_json::to_string(&redacted).expect("json");
        assert!(!text.contains("abc"));
        assert!(!text.contains("session=1"));
    }

    #[test]
    fn cache_holds_the_redacted_form_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        assert!(last_response(root, "list users").is_none());
        let set = hoard::redaction_set(&[], &[]);
        remember_last_response(root, RedactedResponse::new(&display(), &response(), &set, "t".into()));
        let cached = last_response(root, "list users").expect("cached");
        assert_eq!(cached.request_headers[1].1, REDACTED);
        assert!(last_response(&root.join("other"), "list users").is_none(), "per root");
    }

    #[test]
    fn clipping_has_front_matter_fenced_request_and_capped_body() {
        let set = hoard::redaction_set(&[], &[]);
        let mut big = response();
        big.body = "y".repeat(CLIPPING_BODY_BYTES + 10).into_bytes();
        big.headers[0].1 = "text/plain".into();
        let redacted = RedactedResponse::new(&display(), &big, &set, "2026-10-13T10:00:00Z".into());
        assert!(redacted.truncated);
        assert_eq!(redacted.body.len(), CLIPPING_BODY_BYTES);
        let draft = clipping_draft(&redacted);
        assert_eq!(draft.kind, Kind::Raven);
        assert_eq!(draft.title.as_deref(), Some("list users"));
        assert_eq!(
            draft.extra,
            vec![
                ("request".to_string(), "list users".to_string()),
                ("status".to_string(), "200".to_string()),
            ]
        );
        let text = clipping_body(&redacted);
        assert!(text.contains("```http\n### list users\nGET https://api.test/users?key={{token}}\nAccept: */*\nAuthorization: <redacted>\n"), "{text}");
        assert!(text.contains("set-cookie: <redacted>"));
        assert!(text.contains("(body truncated)"));
        assert!(!text.contains("session=1"));
    }

    #[test]
    fn clipping_fences_outgrow_backticks_in_the_response() {
        let set = hoard::redaction_set(&[], &[]);
        let mut tricky = response();
        tricky.body = b"```\n# injected\n```".to_vec();
        tricky.headers[0].1 = "text/plain".into();
        let redacted = RedactedResponse::new(&display(), &tricky, &set, "t".into());
        let text = clipping_body(&redacted);
        assert!(text.contains("````\n```\n# injected\n```\n````\n"), "{text}");
    }

    #[gpui::test]
    async fn saved_clipping_holds_no_secret_bytes(cx: &mut gpui::TestAppContext) {
        use chrono::TimeZone as _;
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/repo", serde_json::json!({ ".hulder": {} })).await;
        let display = Request {
            name: "me".into(),
            method: "GET".into(),
            url: "https://alice:hunter2@api.test/me?api_key={{key}}#frag".into(),
            headers: vec![
                ("Authorization".into(), "Bearer sk-live-999".into()),
                ("Cookie".into(), "session=abc".into()),
                ("X-Api-Key".into(), "key-777".into()),
            ],
            body: String::new(),
        };
        let mut response = response();
        response.headers.push(("Set-Cookie".into(), "session=def; HttpOnly".into()));
        let set = hoard::redaction_set(&[], &[]);
        let redacted = RedactedResponse::new(&display, &response, &set, "2026-10-13T10:00:00Z".into());
        let captured = Local
            .with_ymd_and_hms(2026, 10, 13, 10, 0, 0)
            .single()
            .expect("valid time");
        let path = save_clipping(fs.as_ref(), Path::new("/repo"), &redacted, captured)
            .await
            .expect("save");
        assert_eq!(path, PathBuf::from("/repo/.hulder/clippings/2026-10-13-me.md"));
        let text = fs.load(&path).await.expect("load");
        for secret in [
            "sk-live-999",
            "session=abc",
            "session=def",
            "session=1",
            "key-777",
            "Bearer",
            "hunter2",
            "alice",
        ] {
            assert!(!text.contains(secret), "{secret} leaked into {text}");
        }
        assert!(text.contains("Authorization: <redacted>"), "{text}");
        assert!(text.contains("Cookie: <redacted>"), "{text}");
        assert!(text.contains("Set-Cookie: <redacted>"), "{text}");
        assert!(text.contains("GET https://api.test/me?api_key={{key}}#frag\n"), "{text}");
        assert_eq!(without_userinfo("https://u:p@h.test"), "https://h.test");
        assert_eq!(without_userinfo("{{base}}/x?k=v"), "{{base}}/x?k=v");
        assert!(text.starts_with("---\nsource: https://api.test/me\ncaptured: 2026-10-13T10:00:00"), "{text}");
        assert!(text.contains("\nkind: raven\nquery_present: true\nrequest: me\nstatus: 200\n---\n"), "{text}");
    }
}
