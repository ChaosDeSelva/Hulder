//! Raven: a small API client whose requests are `.http` files under
//! `.hulder/ravens/`. Nothing is sent on open, save, typing or a timer; a
//! send happens only from `raven::Send`, through a `ReqwestClient` built for
//! that one call, and lands in the Hollow Back as `Sent`.

pub mod collection;
pub mod curl;
pub mod log;
pub mod send;
pub mod store;
pub mod view;

pub use collection::{Collection, Environments, Request};
pub use log::{RedactedResponse, last_response};

use anyhow::{Context as _, Result};
use fs::Fs;
use gpui::{App, AppContext as _, Task, actions};
use hulder_net::NetLog;
use send::{MAX_BODY_BYTES, Prepared, Response, TIMEOUT};
use settings::{RegisterSetting, Settings, SettingsContent};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use workspace::Workspace;

actions!(
    raven,
    [
        /// Opens or hides the Raven API client.
        Toggle,
        /// Sends the request shown in the Raven (the only thing that ever sends).
        Send,
        /// Writes the edited request back to its .http collection.
        SaveRequest,
        /// Adds a new request to the selected collection.
        NewRequest,
        /// Saves the last response as a redacted clipping under .hulder/clippings/.
        SaveAsClipping,
        /// Parses a curl command line from the clipboard into a new request.
        ImportCurl,
        /// Selects the next request in the list.
        SelectNext,
        /// Selects the previous request in the list.
        SelectPrevious,
    ]
);

#[derive(Clone, Debug, PartialEq, Eq, RegisterSetting)]
pub struct RavenSettings {
    /// Extra header names (any case) to replace with `<redacted>` in clippings and Bifröst replies.
    pub redact_headers: Vec<String>,
}

impl Settings for RavenSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let content = content.raven.clone().unwrap_or_default();
        Self {
            redact_headers: content.redact_headers.unwrap_or_default(),
        }
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            view::RavenView::toggle(workspace, window, cx);
        });
    })
    .detach();
}

/// The lower-cased header names to redact: the Hoard's set plus the setting.
pub fn redaction_set(cx: &App) -> Vec<String> {
    let extra = RavenSettings::get_global(cx).redact_headers.clone();
    match hoard::Hoard::try_global(cx) {
        Some(hoard) => hoard.read(cx).redaction_set(&extra),
        None => hoard::redaction_set(&[], &extra),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SendOutcome {
    pub display: Request,
    pub response: Response,
    pub redacted: RedactedResponse,
    pub hoard_profile: Option<String>,
}

/// Resolves `$hoard:` variables, applies the matching profile, sends, and
/// writes the metadata line and the redacted cache. Secrets exist only in
/// the `Prepared::request` that this task owns.
pub fn send_request(
    fs: Arc<dyn Fs>,
    root: PathBuf,
    request: Request,
    variables: BTreeMap<String, String>,
    cx: &mut App,
) -> Task<Result<SendOutcome>> {
    let (plain, pending) = collection::split_hoard_variables(&variables);
    let redaction = redaction_set(cx);
    let hoard = hoard::Hoard::try_global(cx);
    let timer = cx.background_executor().timer(TIMEOUT);
    cx.spawn(async move |cx| {
        let display = collection::expand_request(&request, &plain);
        let started = std::time::Instant::now();
        let outcome: Result<(Prepared, Response)> = async {
            // Every `$hoard:` value is checked against the plain-expanded
            // target before any secret is read: the profile must apply to
            // that host and the value may only go into headers or the body.
            let profiles = match &hoard {
                Some(hoard) => cx.update(|cx| hoard.read(cx).profiles().to_vec()),
                None => Vec::new(),
            };
            let display_target = send::check_hoard_substitutions(&display, &pending, &profiles)?;
            let mut secret = BTreeMap::new();
            for (variable, profile) in pending {
                let hoard = hoard
                    .clone()
                    .with_context(|| format!("{variable} needs the Hoard profile {profile}"))?;
                let read = cx.update(|cx| {
                    let secrets = hoard.read(cx).secret_store();
                    secrets.read(&profile, cx)
                });
                let found = read
                    .await?
                    .with_context(|| format!("Hoard profile {profile} has no secret"))?;
                secret.insert(
                    variable,
                    send::hoard_variable_value(
                        &found,
                        &display_target.scheme,
                        &display_target.host,
                        &display_target.path,
                    ),
                );
            }
            let secrets_in_play = !secret.is_empty();
            let (mut wire, display, target) = send::prepare(&request, &plain, &secret)?;
            let (headers, applied) = cx
                .update(|cx| {
                    hoard::apply_to_request(
                        &target.host,
                        &target.scheme,
                        &target.path,
                        std::mem::take(&mut wire.headers),
                        cx,
                    )
                })
                .await?;
            wire.headers = headers;
            // A 3xx is shown rather than followed whenever a secret travelled
            // with the request, so it can never be replayed to another host.
            let follow_redirects = !secrets_in_play && applied.is_none();
            let prepared = Prepared {
                request: wire,
                display,
                target,
                applied,
            };
            let response = cx
                .background_spawn({
                    let wire = prepared.request.clone();
                    async move { send::send(&wire, timer, MAX_BODY_BYTES, follow_redirects).await }
                })
                .await?;
            Ok((prepared, response))
        }
        .await;

        let at = log::now_rfc3339();
        match outcome {
            Ok((prepared, response)) => {
                let host = prepared.target.host.clone();
                cx.update(|cx| {
                    NetLog::record_sent(
                        cx,
                        &prepared.display.method,
                        &host,
                        Some(response.status),
                        response.request_bytes,
                        response.body.len() as u64,
                    );
                });
                if let Some(applied) = &prepared.applied
                    && applied.keep_session_cookies
                {
                    let set_cookies = response.set_cookie_values();
                    let profile = applied.profile.clone();
                    let remember = cx.update(|cx| {
                        hoard::remember_set_cookies(&profile, &host, set_cookies, cx)
                    });
                    if let Err(error) = remember.await {
                        ::log::warn!("raven: keeping session cookies for {profile}: {error:#}");
                    }
                }
                let line = log::LogLine::for_response(&prepared.display, &response, at.clone());
                append_line(fs.as_ref(), &root, &line).await;
                let redacted =
                    RedactedResponse::new(&prepared.display, &response, &redaction, at);
                log::remember_last_response(&root, redacted.clone());
                Ok(SendOutcome {
                    display: prepared.display,
                    response,
                    redacted,
                    hoard_profile: prepared.applied.map(|applied| applied.profile),
                })
            }
            Err(error) => {
                let host = send::target_for(&display.url)
                    .map(|(_, target)| target.host)
                    .unwrap_or_else(|_| "?".into());
                cx.update(|cx| {
                    NetLog::record_sent(cx, &display.method, &host, None, 0, 0);
                });
                let line = log::LogLine::for_error(
                    &display,
                    started.elapsed().as_millis() as u64,
                    error_category(&error),
                    at,
                );
                append_line(fs.as_ref(), &root, &line).await;
                Err(error)
            }
        }
    })
}

/// A word for the log instead of the error text, which can quote a URL or
/// a header.
fn error_category(error: &anyhow::Error) -> &'static str {
    let text = format!("{error:#}");
    if text.contains("no response within") {
        "timeout"
    } else if text.contains("Hoard") || text.contains("profile") {
        "hoard"
    } else if text.contains("URL") || text.contains("url") {
        "invalid_url"
    } else {
        "failed"
    }
}

async fn append_line(fs: &dyn Fs, root: &Path, line: &log::LogLine) {
    match line.to_json() {
        Ok(json) => {
            if let Err(error) = store::append_log_line(fs, root, &json).await {
                ::log::warn!("raven: writing log.jsonl: {error:#}");
            }
        }
        Err(error) => ::log::warn!("raven: serializing log line: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn settings_default_to_no_extra_redactions(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            assert!(RavenSettings::get_global(cx).redact_headers.is_empty());
            let set = redaction_set(cx);
            assert!(set.contains(&"authorization".to_string()));
            assert!(set.contains(&"cookie".to_string()));
        });
    }
}
