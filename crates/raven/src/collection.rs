//! The REST Client `.http` format (`### name`, `METHOD url`, headers, blank
//! line, body) and `env.json` environments. Pure text: no I/O, no gpui.
//! `{{var}}` placeholders stay in the file; expansion happens on a copy at
//! send time, and a `$hoard:<profile>` value is only ever resolved there.

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS",
];

pub const HOARD_PREFIX: &str = "$hoard:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub name: String,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            method: "GET".into(),
            url: String::new(),
            headers: Vec::new(),
            body: String::new(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Collection {
    /// The file stem, e.g. `users` for `users.http`.
    pub name: String,
    pub requests: Vec<Request>,
}

fn is_separator(line: &str) -> bool {
    line.trim_start().starts_with("###")
}

fn separator_name(line: &str) -> Option<String> {
    let name = line.trim_start().trim_start_matches('#').trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn is_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('#') || trimmed.starts_with("//")
}

/// `# @name foo` is the other REST Client spelling for a request name.
fn at_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("# @name")
        .or_else(|| trimmed.strip_prefix("// @name"))?;
    let name = rest.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn parse_request_line(line: &str) -> (String, String) {
    let mut parts = line.trim().splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or_default();
    let rest = parts.next().map(str::trim).unwrap_or_default();
    let upper = first.to_ascii_uppercase();
    if METHODS.contains(&upper.as_str()) || (first.chars().all(|c| c.is_ascii_uppercase()) && !rest.is_empty()) {
        // "POST https://x HTTP/1.1": the protocol tail is noise.
        let url = rest
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        (upper, url)
    } else {
        ("GET".into(), first.to_string())
    }
}

/// Parses one collection. A request without `### name` or `# @name` is
/// named `request-N` by position.
pub fn parse(name: &str, text: &str) -> Collection {
    let mut requests = Vec::new();
    let mut blocks: Vec<(Option<String>, Vec<&str>)> = vec![(None, Vec::new())];
    for line in text.lines() {
        if is_separator(line) {
            blocks.push((separator_name(line), Vec::new()));
        } else if let Some((_, lines)) = blocks.last_mut() {
            lines.push(line);
        }
    }
    for (separator, lines) in blocks {
        let mut name = separator;
        let mut request_line = None;
        let mut headers = Vec::new();
        let mut body_lines: Vec<&str> = Vec::new();
        let mut in_body = false;
        for line in lines {
            if request_line.is_none() {
                if let Some(at) = at_name(line) {
                    name = Some(at);
                    continue;
                }
                if line.trim().is_empty() || is_comment(line) {
                    continue;
                }
                request_line = Some(line);
                continue;
            }
            if in_body {
                body_lines.push(line);
            } else if line.trim().is_empty() {
                in_body = true;
            } else if let Some(at) = at_name(line) {
                name = Some(at);
            } else if is_comment(line) {
                continue;
            } else if let Some((header, value)) = line.split_once(':') {
                headers.push((header.trim().to_string(), value.trim().to_string()));
            }
        }
        let Some(request_line) = request_line else {
            continue;
        };
        let (method, url) = parse_request_line(request_line);
        let body = body_lines.join("\n").trim_end().to_string();
        let index = requests.len() + 1;
        requests.push(Request {
            name: name.unwrap_or_else(|| format!("request-{index}")),
            method,
            url,
            headers,
            body,
        });
    }
    Collection {
        name: name.to_string(),
        requests,
    }
}

pub fn serialize(collection: &Collection) -> String {
    let mut out = String::new();
    for request in &collection.requests {
        out.push_str(&serialize_request(request));
        out.push('\n');
    }
    out
}

pub fn serialize_request(request: &Request) -> String {
    let mut out = format!("### {}\n{} {}\n", request.name, request.method, request.url);
    for (name, value) in &request.headers {
        out.push_str(&format!("{name}: {value}\n"));
    }
    if !request.body.trim().is_empty() {
        out.push('\n');
        out.push_str(request.body.trim_end());
        out.push('\n');
    }
    out
}

/// Replaces `{{name}}` with the variable's value; unknown names stay as
/// written so the user sees what is missing.
pub fn expand(text: &str, variables: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                match variables.get(name) {
                    Some(value) => out.push_str(value),
                    None => out.push_str(&rest[start..start + 2 + end + 2]),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Every `{{name}}` in a request, in order of first appearance.
pub fn placeholders(request: &Request) -> Vec<String> {
    let mut names = Vec::new();
    let mut texts = vec![request.url.as_str(), request.body.as_str()];
    for (name, value) in &request.headers {
        texts.push(name);
        texts.push(value);
    }
    for text in texts {
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else { break };
            let name = after[..end].trim().to_string();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
            rest = &after[end + 2..];
        }
    }
    names
}

/// Applies `variables` to a copy of the request.
pub fn expand_request(request: &Request, variables: &BTreeMap<String, String>) -> Request {
    Request {
        name: request.name.clone(),
        method: request.method.clone(),
        url: expand(&request.url, variables),
        headers: request
            .headers
            .iter()
            .map(|(name, value)| (expand(name, variables), expand(value, variables)))
            .collect(),
        body: expand(&request.body, variables),
    }
}

/// `{"local": {"base": "http://localhost:3000"}, "prod": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Environments(pub BTreeMap<String, BTreeMap<String, String>>);

impl Environments {
    pub fn parse(text: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(text).context("env.json must map environment names to {var: value}")
    }

    pub fn names(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }

    pub fn variables(&self, environment: &str) -> BTreeMap<String, String> {
        self.0.get(environment).cloned().unwrap_or_default()
    }
}

/// Splits an environment into plain variables and `(variable, profile)`
/// pairs that the Hoard fills in at send time.
pub fn split_hoard_variables(
    variables: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<(String, String)>) {
    let mut plain = BTreeMap::new();
    let mut pending = Vec::new();
    for (name, value) in variables {
        match value.strip_prefix(HOARD_PREFIX) {
            Some(profile) => pending.push((name.clone(), profile.trim().to_string())),
            None => {
                plain.insert(name.clone(), value.clone());
            }
        }
    }
    (plain, pending)
}

pub fn validate_method(method: &str) -> Result<String> {
    let method = method.trim().to_ascii_uppercase();
    if method.is_empty() || !method.chars().all(|c| c.is_ascii_uppercase() || c == '-') {
        bail!("{method:?} is not an HTTP method");
    }
    Ok(method)
}

/// A file-safe request or collection name: letters, digits, dash,
/// underscore, dot and spaces inside; nothing that is a path.
pub fn slug(text: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for character in text.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character.to_ascii_lowercase());
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    if slug.is_empty() {
        "request".into()
    } else {
        slug.chars().take(60).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# comment at top\n### list users\nGET {{base}}/users?limit={{limit}}\nAccept: application/json\nAuthorization: Bearer {{token}}\n\n### create user\nPOST {{base}}/users HTTP/1.1\nContent-Type: application/json\n\n{\n  \"name\": \"Ada\"\n}\n\n###\nDELETE https://api.test/users/1\n";

    #[test]
    fn parses_named_blocks_headers_and_bodies() {
        let collection = parse("users", SAMPLE);
        assert_eq!(collection.name, "users");
        assert_eq!(collection.requests.len(), 3);
        let list = &collection.requests[0];
        assert_eq!(list.name, "list users");
        assert_eq!(list.method, "GET");
        assert_eq!(list.url, "{{base}}/users?limit={{limit}}");
        assert_eq!(list.headers.len(), 2);
        assert_eq!(list.header("accept"), Some("application/json"));
        assert_eq!(list.body, "");
        let create = &collection.requests[1];
        assert_eq!(create.method, "POST");
        assert_eq!(create.url, "{{base}}/users", "protocol tail dropped");
        assert_eq!(create.body, "{\n  \"name\": \"Ada\"\n}");
        let delete = &collection.requests[2];
        assert_eq!(delete.name, "request-3");
        assert_eq!(delete.method, "DELETE");
        assert_eq!(delete.url, "https://api.test/users/1");
    }

    #[test]
    fn bare_url_and_at_name_are_accepted() {
        let collection = parse("x", "# @name ping\nhttps://a.test/ping\n");
        assert_eq!(collection.requests[0].name, "ping");
        assert_eq!(collection.requests[0].method, "GET");
        assert_eq!(collection.requests[0].url, "https://a.test/ping");
        assert!(parse("x", "\n\n### empty\n\n").requests.is_empty());
    }

    #[test]
    fn serialize_round_trips() {
        let collection = parse("users", SAMPLE);
        let text = serialize(&collection);
        assert!(text.starts_with("### list users\nGET {{base}}/users?limit={{limit}}\n"));
        let again = parse("users", &text);
        assert_eq!(again, collection);
        assert_eq!(serialize(&again), text);
    }

    #[test]
    fn placeholders_expand_and_unknown_ones_stay() {
        let mut variables = BTreeMap::new();
        variables.insert("base".to_string(), "http://localhost:3000".to_string());
        assert_eq!(
            expand("{{base}}/x/{{ missing }}/{{base}}", &variables),
            "http://localhost:3000/x/{{ missing }}/http://localhost:3000"
        );
        assert_eq!(expand("open {{brace", &variables), "open {{brace");
        let request = &parse("users", SAMPLE).requests[0];
        assert_eq!(placeholders(request), vec!["base", "limit", "token"]);
        let expanded = expand_request(request, &variables);
        assert_eq!(expanded.url, "http://localhost:3000/users?limit={{limit}}");
        assert_eq!(request.url, "{{base}}/users?limit={{limit}}", "original untouched");
    }

    #[test]
    fn environments_parse_and_hoard_values_are_set_aside() {
        let environments = Environments::parse(
            r#"{"local": {"base": "http://localhost:3000", "token": "$hoard:work"}, "prod": {"base": "https://api.test"}}"#,
        )
        .expect("env");
        assert_eq!(environments.names(), vec!["local", "prod"]);
        let (plain, pending) = split_hoard_variables(&environments.variables("local"));
        assert_eq!(plain.get("base").map(String::as_str), Some("http://localhost:3000"));
        assert!(!plain.contains_key("token"), "hoard value never in the plain map");
        assert_eq!(pending, vec![("token".to_string(), "work".to_string())]);
        assert!(Environments::parse("").expect("empty").0.is_empty());
        assert!(Environments::parse("[1]").is_err());
        assert!(Environments::parse(r#"{"local": {"n": 1}}"#).is_err(), "values are strings");
    }

    #[test]
    fn methods_and_slugs_are_validated() {
        assert_eq!(validate_method(" post ").expect("ok"), "POST");
        assert!(validate_method("").is_err());
        assert!(validate_method("GET /x").is_err());
        assert_eq!(slug("List Users: v2!"), "list-users-v2");
        assert_eq!(slug("   "), "request");
    }
}
