//! Pure data for the Hoard: what a profile is, which hosts it applies to,
//! what it adds to a request, and which header names must be redacted
//! because a profile could have produced them. No I/O and no gpui here.

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// The Keychain item is keyed by "server" on macOS, so each profile gets its
/// own server string under this prefix; the account is the profile name.
pub const KEYCHAIN_SERVICE: &str = "Hulder Hoard";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    Bearer,
    Basic,
    Header,
    Cookies,
}

impl ProfileKind {
    pub const ALL: [ProfileKind; 4] = [
        ProfileKind::Bearer,
        ProfileKind::Basic,
        ProfileKind::Header,
        ProfileKind::Cookies,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ProfileKind::Bearer => "bearer",
            ProfileKind::Basic => "basic",
            ProfileKind::Header => "header",
            ProfileKind::Cookies => "cookies",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.label().eq_ignore_ascii_case(text.trim()))
    }
}

/// The non-secret half of a profile, kept in `profiles.json`. `header_name`
/// is stored here (not in the Keychain) so the redaction set can be computed
/// without touching secret material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub kind: ProfileKind,
    pub hosts: Vec<String>,
    pub created: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_name: Option<String>,
    /// Only meaningful for `cookies`: let a response's `Set-Cookie` update the jar.
    #[serde(default)]
    pub keep_session_cookies: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default = "root_path")]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    /// Seconds since the Unix epoch; `None` or a negative number means a session cookie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
}

fn root_path() -> String {
    "/".to_string()
}

impl Cookie {
    pub fn is_expired(&self, now_unix: f64) -> bool {
        matches!(self.expires, Some(expires) if expires >= 0.0 && expires <= now_unix)
    }

    /// Cookie domains starting with a dot match subdomains; a bare domain is
    /// host-only, the way browsers treat `Set-Cookie` without `Domain=`.
    pub fn matches_host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        let domain = self.domain.to_ascii_lowercase();
        match domain.strip_prefix('.') {
            Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
            None => host == domain,
        }
    }

    /// RFC 6265 section 5.1.4 path matching.
    pub fn matches_path(&self, request_path: &str) -> bool {
        path_matches(&self.path, request_path)
    }

    /// Whether this cookie may travel on a request to `scheme://host/path`:
    /// a `Secure` cookie never goes over plain http, not even to localhost.
    pub fn applies(&self, scheme: &str, host: &str, request_path: &str, now_unix: f64) -> bool {
        !self.is_expired(now_unix)
            && (!self.secure || scheme.eq_ignore_ascii_case("https"))
            && self.matches_host(host)
            && self.matches_path(request_path)
    }
}

pub fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    let request_path = if request_path.is_empty() {
        "/"
    } else {
        request_path
    };
    let cookie_path = if cookie_path.is_empty() {
        "/"
    } else {
        cookie_path
    };
    if request_path == cookie_path {
        return true;
    }
    let Some(rest) = request_path.strip_prefix(cookie_path) else {
        return false;
    };
    cookie_path.ends_with('/') || rest.starts_with('/')
}

/// Cookie names and values must be a single header token: no control
/// characters, no `;` (the pair separator) and no line breaks.
pub fn is_valid_cookie_text(text: &str) -> bool {
    !text.is_empty()
        && !text
            .chars()
            .any(|character| character.is_control() || character == ';' || character == '\n')
}

/// Registrable suffixes a cookie may never be set for. Bare top-level
/// domains are refused by the "needs a dot" rule; these are the common
/// two-label public suffixes where that rule is not enough.
pub const PUBLIC_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "net.uk", "com.au", "net.au", "org.au",
    "edu.au", "gov.au", "co.nz", "net.nz", "org.nz", "co.jp", "ne.jp", "or.jp", "ac.jp",
    "co.kr", "or.kr", "com.br", "net.br", "org.br", "co.in", "net.in", "org.in", "com.cn",
    "net.cn", "org.cn", "com.mx", "com.ar", "com.tr", "co.za", "com.sg", "com.hk", "co.il",
    "github.io", "herokuapp.com", "vercel.app", "netlify.app", "pages.dev", "web.app",
    "firebaseapp.com", "cloudfront.net", "amazonaws.com", "azurewebsites.net",
];

pub fn is_public_suffix(domain: &str) -> bool {
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    !domain.contains('.') || PUBLIC_SUFFIXES.contains(&domain.as_str())
}

/// RFC 6265 section 5.1.3: `host` domain-matches `domain` when they are
/// equal or `host` ends with `.domain`.
pub fn domain_matches(domain: &str, host: &str) -> bool {
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    let host = host.to_ascii_lowercase();
    !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
}

/// The secret half of a profile, serialized as JSON into the Keychain item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Secret {
    Bearer { token: String },
    Basic { user: String, password: String },
    Header { name: String, value: String },
    Cookies { jar: Vec<Cookie> },
}

impl Secret {
    pub fn kind(&self) -> ProfileKind {
        match self {
            Secret::Bearer { .. } => ProfileKind::Bearer,
            Secret::Basic { .. } => ProfileKind::Basic,
            Secret::Header { .. } => ProfileKind::Header,
            Secret::Cookies { .. } => ProfileKind::Cookies,
        }
    }

    pub fn header_name(&self) -> Option<&str> {
        match self {
            Secret::Header { name, .. } => Some(name),
            _ => None,
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).context("serializing hoard secret")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).context("parsing hoard secret")
    }

    /// The headers this secret adds for a request to `scheme://host/path`,
    /// with expired, foreign-host, foreign-path and (over http) secure
    /// cookies left out.
    pub fn headers_for(
        &self,
        scheme: &str,
        host: &str,
        request_path: &str,
        now_unix: f64,
    ) -> Vec<(String, String)> {
        match self {
            Secret::Bearer { token } => vec![("Authorization".into(), format!("Bearer {token}"))],
            Secret::Basic { user, password } => {
                let encoded =
                    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
                vec![("Authorization".into(), format!("Basic {encoded}"))]
            }
            Secret::Header { name, value } => vec![(name.clone(), value.clone())],
            Secret::Cookies { jar } => {
                let header = cookie_header(jar, scheme, host, request_path, now_unix);
                if header.is_empty() {
                    Vec::new()
                } else {
                    vec![("Cookie".into(), header)]
                }
            }
        }
    }
}

/// Drops cookies that have expired by `now_unix`, and any whose name or
/// value could not be sent as a header token.
pub fn drop_expired(jar: Vec<Cookie>, now_unix: f64) -> Vec<Cookie> {
    jar.into_iter()
        .filter(|cookie| {
            !cookie.is_expired(now_unix)
                && is_valid_cookie_text(&cookie.name)
                && (cookie.value.is_empty() || is_valid_cookie_text(&cookie.value))
        })
        .collect()
}

/// The `Cookie` header value for `scheme://host/path`: `name=value` pairs
/// joined by `; `.
pub fn cookie_header(
    jar: &[Cookie],
    scheme: &str,
    host: &str,
    request_path: &str,
    now_unix: f64,
) -> String {
    jar.iter()
        .filter(|cookie| cookie.applies(scheme, host, request_path, now_unix))
        .filter(|cookie| {
            is_valid_cookie_text(&cookie.name)
                && (cookie.value.is_empty() || is_valid_cookie_text(&cookie.value))
        })
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Merges `Set-Cookie` values from a response into `jar`, replacing a cookie
/// with the same name, domain and path. Cookies without `Domain=` are
/// host-only for `host`; a `Domain=` that `host` does not belong to is dropped.
pub fn merge_set_cookies(jar: &mut Vec<Cookie>, host: &str, set_cookie_values: &[String]) {
    for value in set_cookie_values {
        let Some(cookie) = parse_set_cookie(value, host) else {
            continue;
        };
        jar.retain(|existing| {
            !(existing.name == cookie.name
                && existing.domain.eq_ignore_ascii_case(&cookie.domain)
                && existing.path == cookie.path)
        });
        jar.push(cookie);
    }
}

/// Parses one `Set-Cookie` value received from `host`, following RFC 6265
/// section 5.3: the cookie is refused when its name or value is not a
/// header token, or when `Domain=` is empty, has no dot, is a public suffix,
/// or does not cover the responding host.
pub fn parse_set_cookie(value: &str, host: &str) -> Option<Cookie> {
    let mut parts = value.split(';').map(str::trim);
    let (name, cookie_value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    let cookie_value = cookie_value.trim();
    if !is_valid_cookie_text(name) || (!cookie_value.is_empty() && !is_valid_cookie_text(cookie_value)) {
        return None;
    }
    let mut cookie = Cookie {
        name: name.to_string(),
        value: cookie_value.to_string(),
        domain: host.to_ascii_lowercase(),
        path: "/".into(),
        secure: false,
        http_only: false,
        expires: None,
    };
    for attribute in parts {
        let (key, attribute_value) = attribute
            .split_once('=')
            .map(|(key, value)| (key.trim(), Some(value.trim())))
            .unwrap_or((attribute, None));
        match key.to_ascii_lowercase().as_str() {
            "domain" => {
                let domain = attribute_value?.trim_start_matches('.').to_ascii_lowercase();
                if domain.is_empty() || is_public_suffix(&domain) || !domain_matches(&domain, host) {
                    return None;
                }
                cookie.domain = format!(".{domain}");
            }
            "path" => {
                if let Some(path) = attribute_value
                    && path.starts_with('/')
                    && is_valid_cookie_text(path)
                {
                    cookie.path = path.to_string();
                }
            }
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            "max-age" => {
                if let Some(seconds) = attribute_value.and_then(|value| value.parse::<f64>().ok())
                {
                    cookie.expires = Some(now_unix() + seconds);
                }
            }
            "expires" => {
                if cookie.expires.is_none()
                    && let Some(text) = attribute_value
                    && let Ok(when) = chrono::DateTime::parse_from_rfc2822(text)
                {
                    cookie.expires = Some(when.timestamp() as f64);
                }
            }
            _ => {}
        }
    }
    Some(cookie)
}

pub fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// Exact host, or `*.example.com` for any host under `example.com` (the apex
/// itself is not a subdomain and needs its own entry). Case-insensitive.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    let host = host.trim().to_ascii_lowercase();
    if pattern.is_empty() || host.is_empty() {
        return false;
    }
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.ends_with(&format!(".{suffix}")),
        None => pattern == host,
    }
}

pub fn is_loopback_host(host: &str) -> bool {
    matches!(
        host.trim().to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]"
    )
}

/// A profile only ever travels over https, except to the local machine.
pub fn scheme_allowed(scheme: &str, host: &str) -> bool {
    match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => is_loopback_host(host),
        _ => false,
    }
}

impl Profile {
    pub fn applies_to(&self, scheme: &str, host: &str) -> bool {
        scheme_allowed(scheme, host)
            && self
                .hosts
                .iter()
                .any(|pattern| host_matches(pattern, host))
    }
}

/// The first profile that applies to `scheme://host`, in file order.
pub fn matching_profile<'a>(profiles: &'a [Profile], scheme: &str, host: &str) -> Option<&'a Profile> {
    profiles
        .iter()
        .find(|profile| profile.applies_to(scheme, host))
}

/// Header names that are always redacted from anything Raven writes down.
pub const ALWAYS_REDACTED: &[&str] = &["authorization", "cookie", "set-cookie", "x-api-key"];

/// Lower-case header names to replace with `<redacted>`: the fixed set, every
/// `header` profile's own name, and whatever `raven.redact_headers` adds.
pub fn redaction_set(profiles: &[Profile], extra: &[String]) -> Vec<String> {
    let mut names: Vec<String> = ALWAYS_REDACTED.iter().map(|name| name.to_string()).collect();
    for name in profiles
        .iter()
        .filter_map(|profile| profile.header_name.as_deref())
        .chain(extra.iter().map(String::as_str))
    {
        let name = name.trim().to_ascii_lowercase();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// A profile name becomes a Keychain account and a file-safe key, so it is
/// limited to ASCII letters, digits, dash, underscore and dot.
pub fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("profile name is empty");
    }
    if name.len() > 64 {
        bail!("profile name is longer than 64 characters");
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        bail!("profile name {name:?} may only use letters, digits, '-', '_' and '.'");
    }
    Ok(name.to_string())
}

/// Hosts are plain host names or `*.suffix` patterns, one per line or comma.
pub fn parse_hosts(text: &str) -> Result<Vec<String>> {
    let mut hosts = Vec::new();
    for raw in text.split([',', '\n', ' ']) {
        let host = raw.trim().to_ascii_lowercase();
        if host.is_empty() {
            continue;
        }
        let bare = host.strip_prefix("*.").unwrap_or(&host);
        if bare.is_empty()
            || bare.contains("://")
            || bare.contains('/')
            || bare.contains('*')
            || bare.contains(char::is_whitespace)
        {
            bail!("{raw:?} is not a host name or *.suffix pattern");
        }
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    if hosts.is_empty() {
        bail!("enter at least one host");
    }
    Ok(hosts)
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesFile {
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, kind: ProfileKind, hosts: &[&str]) -> Profile {
        Profile {
            name: name.into(),
            kind,
            hosts: hosts.iter().map(|host| host.to_string()).collect(),
            created: "2026-10-13".into(),
            header_name: (kind == ProfileKind::Header).then(|| "X-Team-Token".to_string()),
            keep_session_cookies: false,
        }
    }

    #[test]
    fn hosts_match_exactly_or_by_wildcard_suffix() {
        assert!(host_matches("api.example.com", "API.Example.com"));
        assert!(host_matches("*.example.com", "api.example.com"));
        assert!(host_matches("*.example.com", "a.b.example.com"));
        assert!(!host_matches("*.example.com", "example.com"));
        assert!(!host_matches("*.example.com", "notexample.com"));
        assert!(!host_matches("example.com", "api.example.com"));
        assert!(!host_matches("", "example.com"));
    }

    #[test]
    fn profiles_only_travel_over_https_except_to_loopback() {
        assert!(scheme_allowed("https", "api.example.com"));
        assert!(!scheme_allowed("http", "api.example.com"));
        assert!(scheme_allowed("http", "localhost"));
        assert!(scheme_allowed("http", "127.0.0.1"));
        assert!(scheme_allowed("http", "::1"));
        assert!(!scheme_allowed("ftp", "localhost"));
        let work = profile("work", ProfileKind::Bearer, &["*.example.com", "localhost"]);
        assert!(work.applies_to("https", "api.example.com"));
        assert!(!work.applies_to("http", "api.example.com"));
        assert!(work.applies_to("http", "localhost"));
        let profiles = vec![work, profile("other", ProfileKind::Basic, &["b.test"])];
        assert_eq!(
            matching_profile(&profiles, "https", "b.test").map(|profile| profile.name.as_str()),
            Some("other")
        );
        assert!(matching_profile(&profiles, "https", "c.test").is_none());
    }

    #[test]
    fn expired_cookies_are_dropped_and_the_header_is_assembled() {
        let jar = vec![
            Cookie {
                name: "session".into(),
                value: "abc".into(),
                domain: ".example.com".into(),
                path: "/".into(),
                secure: true,
                http_only: true,
                expires: Some(2_000.0),
            },
            Cookie {
                name: "old".into(),
                value: "x".into(),
                domain: ".example.com".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
                expires: Some(500.0),
            },
            Cookie {
                name: "forever".into(),
                value: "y".into(),
                domain: "api.example.com".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
                expires: Some(-1.0),
            },
            Cookie {
                name: "elsewhere".into(),
                value: "z".into(),
                domain: "other.test".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
                expires: None,
            },
        ];
        let kept = drop_expired(jar.clone(), 1_000.0);
        assert_eq!(kept.len(), 3);
        assert!(kept.iter().all(|cookie| cookie.name != "old"));
        assert_eq!(
            cookie_header(&jar, "https", "api.example.com", "/", 1_000.0),
            "session=abc; forever=y"
        );
        assert_eq!(
            cookie_header(&jar, "https", "example.com", "/", 1_000.0),
            "session=abc"
        );
        assert_eq!(
            cookie_header(&jar, "https", "other.test", "/", 1_000.0),
            "elsewhere=z"
        );
    }

    #[test]
    fn secure_cookies_never_go_over_http_even_to_localhost() {
        let jar = vec![
            Cookie {
                name: "secure".into(),
                value: "1".into(),
                domain: "localhost".into(),
                path: "/".into(),
                secure: true,
                http_only: false,
                expires: None,
            },
            Cookie {
                name: "plain".into(),
                value: "2".into(),
                domain: "localhost".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
                expires: None,
            },
        ];
        assert_eq!(cookie_header(&jar, "http", "localhost", "/", 0.0), "plain=2");
        assert_eq!(
            cookie_header(&jar, "https", "localhost", "/", 0.0),
            "secure=1; plain=2"
        );
    }

    #[test]
    fn cookies_only_travel_on_matching_paths() {
        let jar = vec![
            Cookie {
                name: "admin".into(),
                value: "1".into(),
                domain: "a.test".into(),
                path: "/admin".into(),
                secure: false,
                http_only: false,
                expires: None,
            },
            Cookie {
                name: "root".into(),
                value: "2".into(),
                domain: "a.test".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
                expires: None,
            },
        ];
        assert_eq!(cookie_header(&jar, "https", "a.test", "/", 0.0), "root=2");
        assert_eq!(cookie_header(&jar, "https", "a.test", "/api", 0.0), "root=2");
        assert_eq!(
            cookie_header(&jar, "https", "a.test", "/admin", 0.0),
            "admin=1; root=2"
        );
        assert_eq!(
            cookie_header(&jar, "https", "a.test", "/admin/users", 0.0),
            "admin=1; root=2"
        );
        assert_eq!(
            cookie_header(&jar, "https", "a.test", "/administrator", 0.0),
            "root=2"
        );
        assert!(path_matches("/a/", "/a/b"));
        assert!(!path_matches("/a/", "/a"));
        assert!(path_matches("", "/anything"));
    }

    #[test]
    fn set_cookie_updates_the_jar_in_place() {
        let mut jar = vec![Cookie {
            name: "session".into(),
            value: "old".into(),
            domain: "api.example.com".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            expires: None,
        }];
        merge_set_cookies(
            &mut jar,
            "api.example.com",
            &[
                "session=new; Path=/; Secure; HttpOnly".to_string(),
                "theme=dark; Domain=example.com; Max-Age=60".to_string(),
                "=broken".to_string(),
            ],
        );
        assert_eq!(jar.len(), 2);
        let session = jar.iter().find(|cookie| cookie.name == "session").expect("session");
        assert_eq!(session.value, "new");
        assert!(session.secure && session.http_only);
        assert_eq!(session.domain, "api.example.com", "no Domain= means host-only");
        let theme = jar.iter().find(|cookie| cookie.name == "theme").expect("theme");
        assert_eq!(theme.domain, ".example.com", "a parent domain is accepted");
        assert!(theme.expires.is_some_and(|expires| expires > now_unix()));
    }

    #[test]
    fn set_cookie_domain_rules_follow_rfc_6265() {
        let host = "api.example.com";
        assert!(
            parse_set_cookie("a=1; Domain=evil.test", host).is_none(),
            "foreign domain dropped"
        );
        assert!(
            parse_set_cookie("a=1; Domain=sub.api.example.com", host).is_none(),
            "a deeper domain than the host does not cover it"
        );
        assert!(
            parse_set_cookie("a=1; Domain=notexample.com", host).is_none(),
            "suffix without a dot boundary dropped"
        );
        assert!(parse_set_cookie("a=1; Domain=com", host).is_none(), "bare TLD dropped");
        assert!(parse_set_cookie("a=1; Domain=", host).is_none(), "empty domain dropped");
        assert!(
            parse_set_cookie("a=1; Domain=co.uk", "shop.co.uk").is_none(),
            "public suffix dropped"
        );
        assert!(
            parse_set_cookie("a=1; Domain=github.io", "me.github.io").is_none(),
            "hosting suffix dropped"
        );
        let parent = parse_set_cookie("a=1; Domain=.Example.com", host).expect("parent domain");
        assert_eq!(parent.domain, ".example.com");
        let same = parse_set_cookie("a=1; Domain=api.example.com", host).expect("same host");
        assert_eq!(same.domain, ".api.example.com");
        assert!(is_public_suffix("com.au") && !is_public_suffix("example.com.au"));
        assert!(domain_matches("example.com", "a.example.com"));
        assert!(!domain_matches("ample.com", "example.com"));
    }

    #[test]
    fn set_cookie_refuses_control_characters_and_bad_paths() {
        let host = "a.test";
        assert!(parse_set_cookie("a\u{1}=1", host).is_none());
        assert!(parse_set_cookie("a=1\u{7f}", host).is_none());
        assert!(parse_set_cookie("na;me=1", host).is_none());
        assert!(parse_set_cookie("a=line\nbreak", host).is_none());
        let relative = parse_set_cookie("a=1; Path=admin", host).expect("cookie");
        assert_eq!(relative.path, "/", "a relative Path falls back to /");
        let absolute = parse_set_cookie("a=1; Path=/admin", host).expect("cookie");
        assert_eq!(absolute.path, "/admin");
        let empty_value = parse_set_cookie("a=; Path=/", host).expect("empty value is allowed");
        assert_eq!(empty_value.value, "");
        assert!(!is_valid_cookie_text(""));
        let tainted = vec![Cookie {
            name: "a\u{1}".into(),
            value: "1".into(),
            domain: host.into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            expires: None,
        }];
        assert!(drop_expired(tainted.clone(), 0.0).is_empty());
        assert_eq!(cookie_header(&tainted, "https", host, "/", 0.0), "");
    }

    #[test]
    fn each_kind_produces_its_header() {
        let now = 1_000.0;
        assert_eq!(
            Secret::Bearer { token: "t0k".into() }.headers_for("https", "a.test", "/", now),
            vec![("Authorization".to_string(), "Bearer t0k".to_string())]
        );
        assert_eq!(
            Secret::Basic {
                user: "u".into(),
                password: "p".into()
            }
            .headers_for("https", "a.test", "/", now),
            vec![("Authorization".to_string(), "Basic dTpw".to_string())]
        );
        assert_eq!(
            Secret::Header {
                name: "X-Api-Key".into(),
                value: "k".into()
            }
            .headers_for("https", "a.test", "/", now),
            vec![("X-Api-Key".to_string(), "k".to_string())]
        );
        let cookies = Secret::Cookies {
            jar: vec![Cookie {
                name: "s".into(),
                value: "1".into(),
                domain: "a.test".into(),
                path: "/".into(),
                secure: false,
                http_only: false,
                expires: None,
            }],
        };
        assert_eq!(
            cookies.headers_for("https", "a.test", "/", now),
            vec![("Cookie".to_string(), "s=1".to_string())]
        );
        assert!(cookies.headers_for("https", "b.test", "/", now).is_empty());
    }

    #[test]
    fn every_profile_header_is_redacted() {
        let profiles = vec![
            profile("bearer", ProfileKind::Bearer, &["a.test"]),
            profile("basic", ProfileKind::Basic, &["a.test"]),
            profile("header", ProfileKind::Header, &["a.test"]),
            profile("cookies", ProfileKind::Cookies, &["a.test"]),
        ];
        let secrets = vec![
            Secret::Bearer { token: "t".into() },
            Secret::Basic {
                user: "u".into(),
                password: "p".into(),
            },
            Secret::Header {
                name: "X-Team-Token".into(),
                value: "v".into(),
            },
            Secret::Cookies {
                jar: vec![Cookie {
                    name: "s".into(),
                    value: "1".into(),
                    domain: "a.test".into(),
                    path: "/".into(),
                    secure: false,
                    http_only: false,
                    expires: None,
                }],
            },
        ];
        let redacted = redaction_set(&profiles, &["x-custom".to_string()]);
        for secret in secrets {
            for (name, _) in secret.headers_for("https", "a.test", "/", 0.0) {
                assert!(
                    redacted.contains(&name.to_ascii_lowercase()),
                    "{name} produced by {:?} is not in {redacted:?}",
                    secret.kind()
                );
            }
        }
        assert!(redacted.contains(&"set-cookie".to_string()));
        assert!(redacted.contains(&"x-api-key".to_string()));
        assert!(redacted.contains(&"x-custom".to_string()));
    }

    #[test]
    fn profiles_file_round_trips_and_secret_bytes_parse() {
        let file = ProfilesFile {
            profiles: vec![profile("work", ProfileKind::Header, &["*.example.com"])],
        };
        let text = serde_json::to_string_pretty(&file).expect("json");
        assert!(text.contains("\"kind\": \"header\""));
        assert!(text.contains("X-Team-Token"));
        assert!(!text.contains("value"));
        let parsed: ProfilesFile = serde_json::from_str(&text).expect("parse");
        assert_eq!(parsed, file);

        let secret = Secret::Cookies { jar: Vec::new() };
        let bytes = secret.to_bytes().expect("bytes");
        assert_eq!(Secret::from_bytes(&bytes).expect("parse"), secret);
        assert_eq!(ProfileKind::parse(" Bearer "), Some(ProfileKind::Bearer));
    }

    #[test]
    fn names_and_hosts_are_validated() {
        assert_eq!(validate_name(" work-1.a ").expect("ok"), "work-1.a");
        assert!(validate_name("").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("slash/").is_err());
        assert_eq!(
            parse_hosts("API.example.com, *.other.test\nlocalhost").expect("hosts"),
            vec!["api.example.com", "*.other.test", "localhost"]
        );
        assert!(parse_hosts("https://a.test").is_err());
        assert!(parse_hosts("a.test/path").is_err());
        assert!(parse_hosts("").is_err());
    }
}
