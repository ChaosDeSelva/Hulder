//! Sending one request. The `ReqwestClient` is built inside [`send`] and
//! dropped when it returns; the global (blocked) client is never touched.
//! Variables are expanded on copies: `display` carries only non-secret
//! values and is what logs and clippings see, `request` carries the Hoard
//! values too and lives only as long as the send.

use crate::collection::{Request, expand_request};
use anyhow::{Context as _, Result, anyhow};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient as _, RedirectPolicy, RequestTimeout, http};
use hoard::{Applied, Secret};
use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};

pub const TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_REDIRECTS: u32 = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub truncated: bool,
    pub duration: Duration,
    pub request_bytes: u64,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    pub fn set_cookie_values(&self) -> Vec<String> {
        self.headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| value.clone())
            .collect()
    }
}

/// The editor language for a response body, from its `Content-Type`.
pub fn language_for_content_type(content_type: Option<&str>) -> Option<&'static str> {
    let content_type = content_type?.to_ascii_lowercase();
    let media = content_type.split(';').next().unwrap_or_default().trim();
    if media.ends_with("/json") || media.ends_with("+json") {
        Some("JSON")
    } else if media.ends_with("/xml") || media.ends_with("+xml") {
        Some("XML")
    } else if media == "text/html" || media == "application/xhtml+xml" {
        Some("HTML")
    } else if media == "text/markdown" {
        Some("Markdown")
    } else if media.ends_with("javascript") {
        Some("JavaScript")
    } else if media == "text/css" {
        Some("CSS")
    } else {
        None
    }
}

/// Text for the response pane: JSON is pretty-printed, text is shown as is,
/// and bytes that are not UTF-8 are described rather than dumped.
pub fn pretty_body(body: &[u8], content_type: Option<&str>) -> String {
    if body.is_empty() {
        return String::new();
    }
    if language_for_content_type(content_type) == Some("JSON")
        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(body)
        && let Ok(pretty) = serde_json::to_string_pretty(&value)
    {
        return pretty;
    }
    match std::str::from_utf8(body) {
        Ok(text) => text.to_string(),
        Err(_) => format!("<{} bytes of binary data>", body.len()),
    }
}

/// The value a `$hoard:<profile>` variable takes: the bare secret, in the
/// shape a user would paste by hand.
pub fn hoard_variable_value(secret: &Secret, scheme: &str, host: &str, path: &str) -> String {
    match secret {
        Secret::Bearer { token } => token.clone(),
        Secret::Basic { user, password } => format!("{user}:{password}"),
        Secret::Header { value, .. } => value.clone(),
        Secret::Cookies { jar } => {
            hoard::model::cookie_header(jar, scheme, host, path, hoard::model::now_unix())
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub scheme: String,
    pub host: String,
    pub path: String,
}

/// Validates the URL through `hulder_url` (http and https only) and splits
/// out what the Hoard needs to match on.
pub fn target_for(url: &str) -> Result<(String, Target)> {
    if url.contains("{{") {
        return Err(anyhow!("the URL still has unresolved {{{{variables}}}}"));
    }
    let normalized = hulder_url::normalize_url(url, hulder_url::WEB_SCHEMES)?;
    let parsed = http_client::Url::parse(&normalized)?;
    let host = parsed
        .host_str()
        .context("the URL has no host")?
        .trim_matches(|c| c == '[' || c == ']')
        .to_string();
    Ok((
        normalized,
        Target {
            scheme: parsed.scheme().to_string(),
            host,
            path: parsed.path().to_string(),
        },
    ))
}

/// A request with every variable filled in and the matching profile applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    /// What goes on the wire.
    pub request: Request,
    /// The same request with only non-secret variables expanded and no
    /// profile headers: what logs and clippings may see.
    pub display: Request,
    pub target: Target,
    pub applied: Option<Applied>,
}

/// Expands `plain` and `secret` variables and validates the URL. `secret`
/// holds the resolved `$hoard:` values; `profile_headers` what the Hoard adds.
pub fn prepare(
    request: &Request,
    plain: &BTreeMap<String, String>,
    secret: &BTreeMap<String, String>,
) -> Result<(Request, Request, Target)> {
    let display = expand_request(request, plain);
    let mut all = plain.clone();
    all.extend(secret.iter().map(|(k, v)| (k.clone(), v.clone())));
    let mut wire = expand_request(request, &all);
    let (url, target) = target_for(&wire.url)?;
    wire.url = url;
    wire.method = crate::collection::validate_method(&wire.method)?;
    Ok((wire, display, target))
}

/// Checks every `$hoard:` substitution against the plain-expanded target:
/// the profile must apply to `scheme://host`, and a secret never goes into
/// the URL (only headers and body), so it cannot land in a log or a query
/// string. Returns the target.
pub fn check_hoard_substitutions(
    display: &Request,
    pending: &[(String, String)],
    profiles: &[hoard::Profile],
) -> Result<Target> {
    if pending.is_empty() {
        return Ok(target_for(&display.url)?.1);
    }
    for (variable, _) in pending {
        if display.url.contains(&format!("{{{{{variable}}}}}")) {
            return Err(anyhow!(
                "{{{{{variable}}}}} is a Hoard secret and may not be used in the URL; put it in a header or the body"
            ));
        }
    }
    let (_, target) = target_for(&display.url)
        .context("a request using a Hoard variable needs a fully resolved URL")?;
    for (_, profile_name) in pending {
        let profile = profiles
            .iter()
            .find(|profile| &profile.name == profile_name)
            .with_context(|| format!("no Hoard profile named {profile_name}"))?;
        if !profile.applies_to(&target.scheme, &target.host) {
            return Err(anyhow!(
                "profile {profile_name} does not apply to host {} over {}",
                target.host,
                target.scheme
            ));
        }
    }
    Ok(target)
}

fn origin_of(url: &str) -> Option<(String, String, u16)> {
    let parsed = http_client::Url::parse(url).ok()?;
    Some((
        parsed.scheme().to_string(),
        parsed.host_str()?.to_ascii_lowercase(),
        parsed.port_or_known_default()?,
    ))
}

/// Where a redirect goes next: `location` resolved against `current`, with
/// `Authorization` and `Cookie` dropped when the origin changes and the
/// method switched to GET for 301/302/303 (as browsers do).
pub fn next_hop(current: &Request, status: u16, location: &str) -> Result<Request> {
    let base = http_client::Url::parse(&current.url)?;
    let next = base.join(location).context("resolving the Location header")?;
    let next_url = hulder_url::normalize_url(next.as_str(), hulder_url::WEB_SCHEMES)?;
    let same_origin = origin_of(&current.url) == origin_of(&next_url);
    let headers = current
        .headers
        .iter()
        .filter(|(name, _)| {
            same_origin
                || !(name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("cookie"))
        })
        .cloned()
        .collect();
    let switch_to_get = matches!(status, 301..=303) && current.method != "GET" && current.method != "HEAD";
    Ok(Request {
        name: current.name.clone(),
        method: if switch_to_get { "GET".into() } else { current.method.clone() },
        url: next_url,
        headers,
        body: if switch_to_get { String::new() } else { current.body.clone() },
    })
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

async fn exchange_once(
    client: &reqwest_client::ReqwestClient,
    request: &Request,
    max_bytes: usize,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>, bool)> {
    let mut builder = http::Request::builder()
        .method(request.method.as_str())
        .uri(request.url.as_str())
        .extension(RequestTimeout(TIMEOUT))
        // Redirects are handled here, never by the client, so a secret can
        // never be replayed to a host the user did not name.
        .extension(RedirectPolicy::NoFollow);
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let http_request = builder
        .body(AsyncBody::from_bytes(request.body.as_bytes().to_vec().into()))
        .context("building the request")?;
    let response = client.send(http_request).await?;
    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut body = Vec::new();
    response
        .into_body()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut body)
        .await
        .context("reading the response body")?;
    let truncated = body.len() > max_bytes;
    body.truncate(max_bytes);
    Ok((status, headers, body, truncated))
}

/// Sends `request` through a client that lives only for this call. The body
/// is capped at `max_bytes` (one byte more is read to tell "exactly full"
/// from "truncated"); `deadline` aborts the whole exchange. With
/// `follow_redirects` false (any Hoard secret in play) a 3xx is returned as
/// is; otherwise up to `MAX_REDIRECTS` hops are followed with credentials
/// stripped on a cross-origin hop.
pub async fn send(
    request: &Request,
    deadline: impl Future<Output = ()>,
    max_bytes: usize,
    follow_redirects: bool,
) -> Result<Response> {
    let request_bytes = request.body.len() as u64;
    let started = Instant::now();
    let exchange = async {
        let client = reqwest_client::ReqwestClient::new();
        let mut current = request.clone();
        let mut hops = 0;
        let (status, headers, body, truncated) = loop {
            let (status, headers, body, truncated) =
                exchange_once(&client, &current, max_bytes).await?;
            let location = headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                .map(|(_, value)| value.clone());
            match location {
                Some(location) if follow_redirects && is_redirect(status) && hops < MAX_REDIRECTS => {
                    hops += 1;
                    current = next_hop(&current, status, &location)?;
                }
                _ => break (status, headers, body, truncated),
            }
        };
        // The client is dropped here, with its connection pool.
        drop(client);
        anyhow::Ok(Response {
            status,
            headers,
            body,
            truncated,
            duration: started.elapsed(),
            request_bytes,
        })
    };
    let expire = async {
        deadline.await;
        Err(anyhow!("no response within {} seconds", TIMEOUT.as_secs()))
    };
    futures::pin_mut!(exchange);
    futures::pin_mut!(expire);
    match futures::future::select(exchange, expire).await {
        futures::future::Either::Left((outcome, _))
        | futures::future::Either::Right((outcome, _)) => outcome,
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "these tests talk to a real socket, so deadlines need a wall clock, not the test scheduler's"
)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    fn wall(duration: Duration) -> impl Future<Output = ()> {
        async move {
            smol::Timer::after(duration).await;
        }
    }

    /// Answers one request with `response_bytes` after reading its head and
    /// body, and hands the raw request back through the returned receiver.
    fn one_shot_server(response_bytes: Vec<u8>) -> (u16, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("timeout");
            let mut raw = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(_) => break,
                };
                raw.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&raw);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let head = &text[..head_end];
                    let length: usize = head
                        .lines()
                        .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                        .unwrap_or(0);
                    if raw.len() >= head_end + 4 + length {
                        break;
                    }
                }
            }
            sender.send(String::from_utf8_lossy(&raw).into_owned()).ok();
            stream.write_all(&response_bytes).expect("write");
            stream.flush().ok();
        });
        (port, receiver)
    }

    fn request(method: &str, url: &str, body: &str) -> Request {
        Request {
            name: "t".into(),
            method: method.into(),
            url: url.into(),
            headers: vec![("X-Test".into(), "yes".into())],
            body: body.into(),
        }
    }

    #[test]
    fn sends_headers_and_body_and_reads_the_reply() {
        let body = "{\"ok\":true}";
        let reply = format!(
            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nSet-Cookie: s=1; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (port, received) = one_shot_server(reply.into_bytes());
        let response = smol::block_on(send(
            &request("POST", &format!("http://127.0.0.1:{port}/users"), "{\"a\":1}"),
            wall(Duration::from_secs(10)),
            MAX_BODY_BYTES,
            true,
        ))
        .expect("response");
        assert_eq!(response.status, 201);
        assert_eq!(response.content_type(), Some("application/json"));
        assert_eq!(response.set_cookie_values(), vec!["s=1; Path=/"]);
        assert_eq!(response.body, body.as_bytes());
        assert!(!response.truncated);
        assert_eq!(response.request_bytes, 7);
        let raw = received.recv().expect("request seen");
        assert!(raw.starts_with("POST /users HTTP/1.1"), "{raw}");
        assert!(raw.to_ascii_lowercase().contains("x-test: yes"), "{raw}");
        assert!(raw.ends_with("{\"a\":1}"), "{raw}");
        assert_eq!(
            pretty_body(&response.body, response.content_type()),
            "{\n  \"ok\": true\n}"
        );
        assert_eq!(language_for_content_type(response.content_type()), Some("JSON"));
    }

    #[test]
    fn bodies_over_the_cap_are_truncated_with_a_flag() {
        let body = "x".repeat(5000);
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (port, _received) = one_shot_server(reply.into_bytes());
        let response = smol::block_on(send(
            &request("GET", &format!("http://127.0.0.1:{port}/big"), ""),
            wall(Duration::from_secs(10)),
            1000,
            true,
        ))
        .expect("response");
        assert!(response.truncated);
        assert_eq!(response.body.len(), 1000);
        assert_eq!(language_for_content_type(response.content_type()), None);
    }

    #[test]
    fn a_silent_server_hits_the_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let error = smol::block_on(send(
            &request("GET", &format!("http://127.0.0.1:{port}/slow"), ""),
            wall(Duration::from_millis(300)),
            MAX_BODY_BYTES,
            true,
        ))
        .expect_err("timeout");
        assert!(format!("{error:#}").contains("no response"), "{error:#}");
        drop(listener);
    }

    #[test]
    fn prepare_keeps_secrets_out_of_the_display_copy() {
        let request = Request {
            name: "me".into(),
            method: "get".into(),
            url: "{{base}}/me?key={{token}}".into(),
            headers: vec![("X-Key".into(), "{{token}}".into())],
            body: String::new(),
        };
        let mut plain = BTreeMap::new();
        plain.insert("base".to_string(), "https://api.test".to_string());
        let mut secret = BTreeMap::new();
        secret.insert("token".to_string(), "s3cret".to_string());
        let (wire, display, target) = prepare(&request, &plain, &secret).expect("prepared");
        assert_eq!(wire.url, "https://api.test/me?key=s3cret");
        assert_eq!(wire.method, "GET");
        assert_eq!(wire.headers[0].1, "s3cret");
        assert_eq!(display.url, "https://api.test/me?key={{token}}");
        assert_eq!(display.headers[0].1, "{{token}}");
        assert_eq!(target.host, "api.test");
        assert_eq!(target.scheme, "https");
        assert_eq!(target.path, "/me");
        assert!(prepare(&request, &BTreeMap::new(), &secret).is_err(), "unresolved base");
        let bad = Request {
            url: "ftp://a.test/x".into(),
            ..request
        };
        assert!(prepare(&bad, &plain, &secret).is_err());
        for input in hulder_url::HOSTILE_INPUTS {
            assert!(target_for(input).is_err(), "{input:?}");
        }
    }

    fn secret_request_to(url: &str) -> Request {
        let mut request = request("GET", url, "");
        request.headers.push(("Authorization".into(), "Bearer s3cret".into()));
        request
    }

    #[test]
    fn cross_origin_redirects_drop_credentials_and_secrets_never_follow() {
        let final_reply = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string();
        let (final_port, final_received) = one_shot_server(final_reply.into_bytes());
        // A different port is a different origin.
        let redirect_reply = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{final_port}/landed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let (first_port, first_received) = one_shot_server(redirect_reply.into_bytes());
        let mut secret_request = request("POST", &format!("http://127.0.0.1:{first_port}/start"), "data");
        secret_request.headers.push(("Authorization".into(), "Bearer s3cret".into()));
        secret_request.headers.push(("Cookie".into(), "s=1".into()));
        let response = smol::block_on(send(
            &secret_request,
            wall(Duration::from_secs(10)),
            MAX_BODY_BYTES,
            true,
        ))
        .expect("response");
        assert_eq!(response.status, 200);
        let first = first_received.recv().expect("first hop");
        assert!(first.contains("Bearer s3cret"), "{first}");
        let landed = final_received.recv().expect("second hop");
        assert!(landed.starts_with("GET /landed"), "302 on POST becomes GET: {landed}");
        assert!(!landed.to_ascii_lowercase().contains("authorization"), "{landed}");
        assert!(!landed.to_ascii_lowercase().contains("cookie"), "{landed}");
        assert!(landed.to_ascii_lowercase().contains("x-test: yes"), "other headers stay: {landed}");

        // With a secret in play the second hop must never happen: the target
        // server is a fresh listener whose receiver stays empty.
        let (never_port, never_received) =
            one_shot_server(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec());
        let redirect_reply = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{never_port}/never\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let (no_follow_port, _) = one_shot_server(redirect_reply.into_bytes());
        let response = smol::block_on(send(
            &secret_request_to(&format!("http://127.0.0.1:{no_follow_port}/start")),
            wall(Duration::from_secs(10)),
            MAX_BODY_BYTES,
            false,
        ))
        .expect("response");
        assert_eq!(response.status, 302, "a secret-bearing request shows the 3xx instead");
        assert!(response.header("location").is_some_and(|location| location.ends_with("/never")));
        assert!(
            never_received.recv_timeout(Duration::from_millis(500)).is_err(),
            "the redirect target must never be contacted"
        );

        let same_origin = next_hop(
            &secret_request,
            307,
            &format!("http://127.0.0.1:{first_port}/again"),
        )
        .expect("hop");
        assert_eq!(same_origin.method, "POST");
        assert_eq!(same_origin.body, "data");
        assert!(same_origin.header("authorization").is_some(), "same origin keeps credentials");
        assert!(next_hop(&secret_request, 302, "ftp://x.test/").is_err());
    }

    fn profile(name: &str, hosts: &[&str]) -> hoard::Profile {
        hoard::Profile {
            name: name.into(),
            kind: hoard::ProfileKind::Bearer,
            hosts: hosts.iter().map(|host| host.to_string()).collect(),
            created: "2026-10-13".into(),
            header_name: None,
            keep_session_cookies: false,
        }
    }

    #[test]
    fn hoard_values_only_reach_matching_hosts_and_never_the_url() {
        let profiles = vec![profile("work", &["api.example"])];
        let pending = vec![("token".to_string(), "work".to_string())];
        let mut display = Request {
            name: "me".into(),
            method: "GET".into(),
            url: "https://api.example/me".into(),
            headers: vec![("Authorization".into(), "Bearer {{token}}".into())],
            body: String::new(),
        };
        let target = check_hoard_substitutions(&display, &pending, &profiles).expect("matching host");
        assert_eq!(target.host, "api.example");

        display.url = "https://evil.example/me".into();
        let error = check_hoard_substitutions(&display, &pending, &profiles).expect_err("foreign host");
        assert!(format!("{error:#}").contains("does not apply to host evil.example"), "{error:#}");

        display.url = "http://api.example/me".into();
        assert!(check_hoard_substitutions(&display, &pending, &profiles).is_err(), "plain http");

        display.url = "https://api.example/me?key={{token}}".into();
        let error = check_hoard_substitutions(&display, &pending, &profiles).expect_err("secret in url");
        assert!(format!("{error:#}").contains("may not be used in the URL"), "{error:#}");

        display.url = "https://api.example/me".into();
        let unknown = vec![("token".to_string(), "nope".to_string())];
        assert!(check_hoard_substitutions(&display, &unknown, &profiles).is_err());
        display.url = "{{base}}/me".into();
        assert!(check_hoard_substitutions(&display, &pending, &profiles).is_err(), "unresolved base");
        assert!(check_hoard_substitutions(&display, &[], &profiles).is_err());
    }

    #[test]
    fn hoard_variables_take_the_bare_secret() {
        assert_eq!(
            hoard_variable_value(&Secret::Bearer { token: "t".into() }, "https", "a.test", "/"),
            "t"
        );
        assert_eq!(
            hoard_variable_value(
                &Secret::Basic {
                    user: "u".into(),
                    password: "p".into()
                },
                "https",
                "a.test",
                "/"
            ),
            "u:p"
        );
        assert_eq!(
            pretty_body(b"\xff\xfe", Some("application/octet-stream")),
            "<2 bytes of binary data>"
        );
        assert_eq!(language_for_content_type(Some("text/html; charset=utf-8")), Some("HTML"));
        assert_eq!(language_for_content_type(Some("application/problem+json")), Some("JSON"));
    }
}
