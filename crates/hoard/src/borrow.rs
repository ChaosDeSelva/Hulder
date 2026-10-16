//! Borrow from Chrome: read the cookies of the hardened profile Hulder
//! launched, keep the ones for the hosts the user ticked, and clear the
//! profile afterwards so the throwaway browser holds nothing. The `CdpClient`
//! always comes from [`crate::ChromeBridge`], never from a port the user typed.

use crate::model::{Cookie, drop_expired};
use anyhow::{Context as _, Result};
use cdp::CdpClient;
use serde_json::{Value, json};

/// `Storage.getCookies` returns `{ cookies: [{ name, value, domain, path,
/// expires, secure, httpOnly, ... }] }`; `expires` is -1 for session cookies.
pub fn cookies_from_storage(reply: &Value, now_unix: f64) -> Vec<Cookie> {
    let Some(entries) = reply["cookies"].as_array() else {
        return Vec::new();
    };
    let jar = entries
        .iter()
        .filter_map(|entry| {
            Some(Cookie {
                name: entry["name"].as_str()?.to_string(),
                value: entry["value"].as_str()?.to_string(),
                domain: entry["domain"].as_str()?.to_string(),
                path: entry["path"].as_str().unwrap_or("/").to_string(),
                secure: entry["secure"].as_bool().unwrap_or(false),
                http_only: entry["httpOnly"].as_bool().unwrap_or(false),
                expires: entry["expires"].as_f64().filter(|expires| *expires >= 0.0),
            })
        })
        .collect();
    drop_expired(jar, now_unix)
}

/// The distinct hosts a jar covers, with the leading dot of domain cookies
/// removed, sorted, for the user to tick.
pub fn hosts_in(jar: &[Cookie]) -> Vec<String> {
    let mut hosts: Vec<String> = jar
        .iter()
        .map(|cookie| cookie.domain.trim_start_matches('.').to_ascii_lowercase())
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Keeps only the cookies whose (dot-stripped) domain is in `ticked`.
pub fn select_hosts(jar: Vec<Cookie>, ticked: &[String]) -> Vec<Cookie> {
    jar.into_iter()
        .filter(|cookie| {
            let host = cookie.domain.trim_start_matches('.').to_ascii_lowercase();
            ticked.iter().any(|ticked| ticked.eq_ignore_ascii_case(&host))
        })
        .collect()
}

/// Host patterns for the new profile: `example.com` cookies become
/// `example.com` plus `*.example.com` since a domain cookie covers both.
pub fn profile_hosts(jar: &[Cookie]) -> Vec<String> {
    let mut hosts = Vec::new();
    for cookie in jar {
        let bare = cookie.domain.trim_start_matches('.').to_ascii_lowercase();
        let patterns = if cookie.domain.starts_with('.') {
            vec![bare.clone(), format!("*.{bare}")]
        } else {
            vec![bare]
        };
        for pattern in patterns {
            if !hosts.contains(&pattern) {
                hosts.push(pattern);
            }
        }
    }
    hosts.sort();
    hosts
}

/// Params for `Network.setCookies` / `Storage.setCookies`.
pub fn set_cookies_params(jar: &[Cookie]) -> Value {
    let cookies: Vec<Value> = jar
        .iter()
        .map(|cookie| {
            let mut entry = json!({
                "name": cookie.name,
                "value": cookie.value,
                "domain": cookie.domain,
                "path": cookie.path,
                "secure": cookie.secure,
                "httpOnly": cookie.http_only,
            });
            if let Some(expires) = cookie.expires {
                entry["expires"] = json!(expires);
            }
            entry
        })
        .collect();
    json!({ "cookies": cookies })
}

pub async fn fetch_all_cookies(client: &CdpClient, now_unix: f64) -> Result<Vec<Cookie>> {
    let reply = client
        .call("Storage.getCookies", json!({}))
        .await
        .context("Storage.getCookies")?;
    Ok(cookies_from_storage(&reply, now_unix))
}

/// `Network.clearBrowserCookies` is a page-domain command, so it runs in a
/// page session; the browser-level `Storage.clearCookies` is the fallback.
pub async fn clear_browser_cookies(client: &CdpClient) -> Result<()> {
    let targets = client.get_targets().await?;
    if let Some(page) = targets.iter().find(|target| target.kind == "page") {
        let session = client.attach_to_target(&page.target_id).await?;
        if session
            .call("Network.clearBrowserCookies", json!({}))
            .await
            .is_ok()
        {
            return Ok(());
        }
    }
    client
        .call("Storage.clearCookies", json!({}))
        .await
        .context("Storage.clearCookies")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_reply_becomes_a_jar_without_expired_cookies() {
        let reply = json!({
            "cookies": [
                { "name": "s", "value": "1", "domain": ".example.com", "path": "/", "expires": -1, "secure": true, "httpOnly": true },
                { "name": "old", "value": "2", "domain": "example.com", "path": "/", "expires": 10.0 },
                { "name": "x", "value": "3", "domain": "other.test" },
                { "value": "no name" }
            ]
        });
        let jar = cookies_from_storage(&reply, 100.0);
        assert_eq!(jar.len(), 2);
        assert_eq!(jar[0].name, "s");
        assert_eq!(jar[0].expires, None);
        assert!(jar[0].secure && jar[0].http_only);
        assert_eq!(hosts_in(&jar), vec!["example.com", "other.test"]);
        let ticked = select_hosts(jar.clone(), &["example.com".to_string()]);
        assert_eq!(ticked.len(), 1);
        assert_eq!(
            profile_hosts(&ticked),
            vec!["*.example.com".to_string(), "example.com".to_string()]
        );
        assert!(select_hosts(jar, &[]).is_empty());
    }

    #[test]
    fn set_cookies_params_keep_the_cdp_shape() {
        let jar = vec![Cookie {
            name: "s".into(),
            value: "1".into(),
            domain: ".example.com".into(),
            path: "/".into(),
            secure: true,
            http_only: false,
            expires: Some(50.0),
        }];
        let params = set_cookies_params(&jar);
        assert_eq!(params["cookies"][0]["domain"], ".example.com");
        assert_eq!(params["cookies"][0]["httpOnly"], false);
        assert_eq!(params["cookies"][0]["expires"], 50.0);
        assert!(cookies_from_storage(&json!({}), 0.0).is_empty());
    }
}
