//! The Hoard's view of the Chrome we launch: the `ChromeBridge` it borrows
//! cookies through (only ever the `ChromeRunner`'s own connection), and the
//! cookie injection `ChromeRunner::open` performs before navigating when a
//! `cookies` profile matches the page's host.

use crate::chrome_runner::ChromeRunner;
use anyhow::{Context as _, Result};
use cdp::{CdpClient, CdpSession};
use gpui::{App, Task};
use hoard::{ChromeBridge, Cookie};
use std::sync::Arc;

pub struct RunnerBridge;

impl ChromeBridge for RunnerBridge {
    fn open(&self, url: String, cx: &mut App) -> Task<Result<()>> {
        ChromeRunner::global(cx).update(cx, |runner, cx| runner.open(url, cx))
    }

    fn client(&self, cx: &mut App) -> Option<CdpClient> {
        ChromeRunner::global(cx).update(cx, |runner, _| runner.borrow_client())
    }
}

pub fn register(cx: &mut App) {
    hoard::set_chrome_bridge(cx, Arc::new(RunnerBridge));
}

/// `(scheme, host)` of a normalized URL, for the Hoard's host matching.
pub fn target_of(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    Some((parsed.scheme().to_string(), parsed.host_str()?.to_string()))
}

pub const BLANK_PAGE: &str = "about:blank";

/// With cookies to inject, Chrome starts on a blank page so the jar is in
/// place before the first request to the site.
pub fn launch_url_for(url: &str, has_cookies: bool) -> String {
    if has_cookies {
        BLANK_PAGE.to_string()
    } else {
        url.to_string()
    }
}

/// Only the cookies that belong to `scheme://host` go into the session; a
/// `Secure` cookie never travels to an http page.
pub fn cookies_for_page(jar: &[Cookie], url: &str) -> Vec<Cookie> {
    let Some((scheme, host)) = target_of(url) else {
        return Vec::new();
    };
    let now = hoard::model::now_unix();
    jar.iter()
        .filter(|cookie| cookie.applies(&scheme, &host, &cookie.path, now))
        .cloned()
        .collect()
}

/// Sets the page's cookies through the session's `Network.setCookies`
/// (never argv, the URL or a file in the profile) and then navigates.
pub async fn inject_and_navigate(
    _client: &CdpClient,
    session: &CdpSession,
    cookies: &[Cookie],
    url: &str,
) -> Result<()> {
    let cookies = cookies_for_page(cookies, url);
    anyhow::ensure!(!cookies.is_empty(), "no cookie in the profile applies to this page");
    session
        .call("Network.enable", serde_json::json!({}))
        .await
        .context("Network.enable")?;
    // Whatever a previous session left in the profile goes first.
    clear_cookies(session).await?;
    session
        .call("Network.setCookies", hoard::borrow::set_cookies_params(&cookies))
        .await
        .context("Network.setCookies")?;
    session
        .call("Page.navigate", serde_json::json!({ "url": url }))
        .await
        .context("Page.navigate")?;
    Ok(())
}

pub async fn clear_cookies(session: &CdpSession) -> Result<()> {
    session
        .call("Network.clearBrowserCookies", serde_json::json!({}))
        .await
        .context("Network.clearBrowserCookies")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, domain: &str, secure: bool) -> Cookie {
        Cookie {
            name: name.into(),
            value: "v".into(),
            domain: domain.into(),
            path: "/".into(),
            secure,
            http_only: false,
            expires: None,
        }
    }

    #[test]
    fn only_the_pages_cookies_enter_the_session() {
        let jar = vec![
            cookie("site", ".a.test", false),
            cookie("exact", "app.a.test", true),
            cookie("other", "b.test", false),
        ];
        let names = |cookies: Vec<Cookie>| cookies.into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(names(cookies_for_page(&jar, "https://app.a.test/x")), vec!["site", "exact"]);
        assert_eq!(names(cookies_for_page(&jar, "http://app.a.test/x")), vec!["site"]);
        assert!(cookies_for_page(&jar, "https://c.test/").is_empty());
        assert!(cookies_for_page(&jar, "about:blank").is_empty());
    }

    #[test]
    fn launch_url_is_blank_only_when_cookies_travel() {
        assert_eq!(launch_url_for("https://a.test/", true), BLANK_PAGE);
        assert_eq!(launch_url_for("https://a.test/", false), "https://a.test/");
        assert_eq!(
            target_of("https://App.test:8443/x"),
            Some(("https".to_string(), "app.test".to_string()))
        );
        assert_eq!(target_of("about:blank"), None);
    }
}
