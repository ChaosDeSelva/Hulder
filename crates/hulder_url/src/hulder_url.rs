//! One URL validator for every place Hulder hands a user-typed URL to a
//! child process as an argument. Chrome accepts a `--` terminator, lightpanda
//! does not, so the value itself must never look like a switch.

use anyhow::{Context as _, Result};

/// What `lightpanda fetch` may be asked to load.
pub const WEB_SCHEMES: &[&str] = &["http", "https"];

/// What the Chrome we launch may be asked to load.
pub const BROWSER_SCHEMES: &[&str] = &["http", "https", "file", "about"];

/// Inputs that every caller must refuse regardless of its scheme set. Both
/// the browser and lightpanda crates run their tests over this table.
pub const HOSTILE_INPUTS: &[&str] = &[
    "",
    "   ",
    "-remote-debugging-port=9222",
    "--remote-debugging-port=9222",
    " --headless",
    "--http_proxy=http://x",
    "-",
    "--",
    "javascript:alert(1)",
    "ftp://example.com",
    "data:text/html,<script>alert(1)</script>",
    "not a url",
    "http://",
    "https://exa mple.com",
];

/// Trims, defaults a bare host to `https://`, parses, and checks the scheme.
/// The returned string is the parsed form, so it always starts with a scheme
/// and can never be read as a flag by the receiving process.
pub fn normalize_url(input: &str, allowed_schemes: &[&str]) -> Result<String> {
    let trimmed = input.trim();
    anyhow::ensure!(!trimmed.is_empty(), "enter a URL to open");
    anyhow::ensure!(
        !trimmed.starts_with('-'),
        "URL must not start with '-': {trimmed}"
    );
    let candidate = if trimmed.contains("://") || trimmed.starts_with("about:") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let parsed = url::Url::parse(&candidate).with_context(|| format!("invalid URL {trimmed}"))?;
    anyhow::ensure!(
        allowed_schemes.contains(&parsed.scheme()),
        "unsupported URL scheme {}; use {}",
        parsed.scheme(),
        allowed_schemes.join(", ")
    );
    let normalized = parsed.as_str().to_string();
    anyhow::ensure!(
        !normalized.starts_with('-'),
        "URL must not start with '-': {normalized}"
    );
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_hosts_default_to_https() {
        assert_eq!(
            normalize_url(" example.com ", WEB_SCHEMES).expect("valid"),
            "https://example.com/"
        );
        assert_eq!(
            normalize_url("http://localhost:3000/x", WEB_SCHEMES).expect("valid"),
            "http://localhost:3000/x"
        );
    }

    #[test]
    fn scheme_set_is_per_caller() {
        assert!(normalize_url("about:blank", WEB_SCHEMES).is_err());
        assert_eq!(
            normalize_url("about:blank", BROWSER_SCHEMES).expect("valid"),
            "about:blank"
        );
        assert!(normalize_url("file:///etc/hosts", WEB_SCHEMES).is_err());
        assert_eq!(
            normalize_url("file:///etc/hosts", BROWSER_SCHEMES).expect("valid"),
            "file:///etc/hosts"
        );
    }

    #[test]
    fn hostile_inputs_are_refused_by_every_scheme_set() {
        for input in HOSTILE_INPUTS {
            for schemes in [WEB_SCHEMES, BROWSER_SCHEMES] {
                assert!(
                    normalize_url(input, schemes).is_err(),
                    "{input:?} accepted with {schemes:?}"
                );
            }
        }
    }

    #[test]
    fn output_never_starts_with_a_dash() {
        for input in ["example.com/--flag", "https://a.test/?x=--y", "a.test#--z"] {
            let url = normalize_url(input, WEB_SCHEMES).expect("valid");
            assert!(!url.starts_with('-'), "{url}");
            assert!(url.starts_with("https://"), "{url}");
        }
    }
}
