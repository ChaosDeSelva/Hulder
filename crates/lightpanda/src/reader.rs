use crate::fetch::Dump;
use crate::hoard_fetch::fetch_with_cookies;
use crate::settings::LightpandaSettings;
use crate::{OpenUrl, OpenUrlSemantic};
use editor::Editor;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Global, IntoElement, ParentElement, Render, Styled, Subscription, Window, rems,
};
use hulder_net::NetLog;
use settings::Settings as _;
use std::time::SystemTime;
use theme::ActiveTheme as _;
use ui::{Color, Label, LabelCommon, LabelSize, div, h_flex, prelude::*, v_flex};
use workspace::{ModalView, Toast, Workspace, notifications::NotificationId};

/// Text placed in the read-only buffer for a fetched page.
pub fn reader_text(url: &str, body: &str) -> String {
    let header = format!("<!-- fetched from {url} by Lightpanda -->\n\n");
    if body.trim().is_empty() {
        format!("{header}_Lightpanda returned no text for this page._\n")
    } else {
        format!("{header}{body}")
    }
}

/// The first Markdown heading, else the url itself.
pub fn page_title(url: &str, body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find_map(|line| {
            line.strip_prefix('#')
                .map(|rest| rest.trim_start_matches('#').trim())
                .filter(|title| !title.is_empty())
        })
        .map(str::to_string)
        .unwrap_or_else(|| url.to_string())
}

#[derive(Debug, Clone)]
pub struct FetchRecord {
    pub at: SystemTime,
    pub title: String,
    pub url: String,
    pub dump: Dump,
    pub text: String,
}

/// Every fetch made this session, newest first, so the Panda panel can reopen one.
#[derive(Default)]
pub struct FetchHistory {
    records: Vec<FetchRecord>,
}

impl FetchHistory {
    pub const MAX_RECORDS: usize = 200;

    pub fn global(cx: &App) -> Entity<FetchHistory> {
        cx.global::<GlobalFetchHistory>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<FetchHistory>> {
        cx.try_global::<GlobalFetchHistory>()
            .map(|global| global.0.clone())
    }

    pub fn push(&mut self, record: FetchRecord) {
        self.records.insert(0, record);
        self.records.truncate(Self::MAX_RECORDS);
    }

    pub fn records(&self) -> &[FetchRecord] {
        &self.records
    }
}

struct GlobalFetchHistory(Entity<FetchHistory>);

impl Global for GlobalFetchHistory {}

pub fn init(cx: &mut App) {
    let history = cx.new(|_| FetchHistory::default());
    cx.set_global(GlobalFetchHistory(history));
}

struct LightpandaToast;

pub fn toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<LightpandaToast>(), message).autohide(),
        cx,
    );
}

pub fn disabled_message() -> &'static str {
    "Lightpanda is off. Set \"lightpanda\": { \"enabled\": true } in settings to let Hulder run it."
}

/// Fetches `url` with the configured binary and opens the result as a read-only
/// Markdown buffer. Every successful fetch lands in the Hollow Back log and the
/// session history.
pub fn open_reader(
    workspace: &mut Workspace,
    url: String,
    dump: Dump,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let settings = LightpandaSettings::get_global(cx).clone();
    if !settings.enabled {
        toast(workspace, disabled_message().to_string(), cx);
        return;
    }
    let url = match normalize_url(&url) {
        Ok(url) => url,
        Err(error) => {
            toast(workspace, format!("Lightpanda: {error:#}"), cx);
            return;
        }
    };
    toast(workspace, format!("Lightpanda is fetching {url}…"), cx);
    // A matching Hoard `cookies` profile rides along; the jar is read on the
    // foreground and handed to the fetch, never written anywhere.
    let cookies = match hoard_target(&url) {
        Some((scheme, host)) => hoard::cookies_for_host(&host, &scheme, cx),
        None => gpui::Task::ready(Ok(None)),
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let (profile, cookies) = match cookies.await {
            Ok(Some((profile, cookies))) if !cookies.is_empty() => (Some(profile), cookies),
            Ok(_) => (None, Vec::new()),
            Err(error) => {
                workspace.update(cx, |workspace, cx| {
                    toast(workspace, format!("Lightpanda: Hoard: {error:#}"), cx);
                })?;
                return anyhow::Ok(());
            }
        };
        let body = fetch_with_cookies(
            &settings.binary,
            dump,
            &url,
            &cookies,
            cx.background_executor(),
        )
        .await;
        let body = match body {
            Ok(body) => body,
            Err(error) => {
                workspace.update(cx, |workspace, cx| {
                    toast(workspace, format!("Lightpanda: {error:#}"), cx);
                })?;
                return anyhow::Ok(());
            }
        };
        let text = reader_text(&url, &body);
        let title = page_title(&url, &body);
        cx.update(|_, cx| {
            NetLog::record_browsed_with_profile(cx, url.clone(), profile.as_deref());
            if let Some(history) = FetchHistory::try_global(cx) {
                history.update(cx, |history, cx| {
                    history.push(FetchRecord {
                        at: SystemTime::now(),
                        title: title.clone(),
                        url: url.clone(),
                        dump,
                        text: text.clone(),
                    });
                    cx.notify();
                });
            }
        })?;
        if save_clipping(&workspace, &url, &title, &text, cx).await? {
            return anyhow::Ok(());
        }
        open_text_buffer(workspace, title, text, cx).await
    })
    .detach_and_log_err(cx);
}

/// Saves the fetch as a clipping (spec section 13) and opens that file.
/// Returns `false` when the page should open in a scratch buffer instead:
/// clippings are off, there is no local worktree, or the save failed (which
/// is shown as a toast so the text is never lost).
async fn save_clipping(
    workspace: &gpui::WeakEntity<Workspace>,
    url: &str,
    title: &str,
    text: &str,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<bool> {
    let draft = clippings::ClippingDraft {
        title: (title != url).then(|| title.to_string()),
        ..clippings::ClippingDraft::text(clippings::Kind::Fetch, url, text.to_string())
    };
    match clippings::save_and_open(workspace.clone(), draft, cx).await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(error) => {
            workspace.update(cx, |workspace, cx| {
                toast(workspace, format!("Clipping not saved: {error:#}"), cx);
            })?;
            Ok(false)
        }
    }
}

/// Opens `text` in a new read-only Markdown tab titled `title`.
pub async fn open_text_buffer(
    workspace: gpui::WeakEntity<Workspace>,
    title: String,
    text: String,
    cx: &mut gpui::AsyncWindowContext,
) -> anyhow::Result<()> {
    let (languages, project) = workspace.read_with(cx, |workspace, _| {
        (
            workspace.app_state().languages.clone(),
            workspace.project().clone(),
        )
    })?;
    let markdown = languages.language_for_name("Markdown").await.ok();
    let buffer = project
        .update(cx, |project, cx| project.create_buffer(markdown, false, cx))
        .await?;
    workspace.update_in(cx, |workspace, window, cx| {
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(0..0, text)], None, cx);
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer, Some(project.clone()), window, cx);
            editor.set_read_only(true);
            editor
                .buffer()
                .update(cx, |buffer, cx| buffer.set_title(title, cx));
            editor
        });
        workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
    })?;
    Ok(())
}

/// `(scheme, host)` for the Hoard's host matching.
fn hoard_target(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    Some((parsed.scheme().to_string(), parsed.host_str()?.to_string()))
}

/// The URL is lightpanda's last argument and lightpanda has no `--`
/// terminator, so only a parsed http(s) URL ever reaches its command line.
pub fn normalize_url(input: &str) -> anyhow::Result<String> {
    hulder_url::normalize_url(input, hulder_url::WEB_SCHEMES)
}

/// Single-line modal asking for a URL, in the shape of the go-to-line prompt.
pub struct UrlPrompt {
    editor: Entity<Editor>,
    dump: Dump,
    workspace: gpui::WeakEntity<Workspace>,
    _subscriptions: Vec<Subscription>,
}

impl UrlPrompt {
    pub fn register(workspace: &mut Workspace) {
        workspace.register_action(|workspace, _: &OpenUrl, window, cx| {
            Self::toggle(workspace, Dump::Markdown, window, cx);
        });
        workspace.register_action(|workspace, _: &OpenUrlSemantic, window, cx| {
            Self::toggle(workspace, Dump::SemanticText, window, cx);
        });
    }

    pub fn toggle(
        workspace: &mut Workspace,
        dump: Dump,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if !LightpandaSettings::get_global(cx).enabled {
            toast(workspace, disabled_message().to_string(), cx);
            return;
        }
        let weak = cx.entity().downgrade();
        workspace.toggle_modal(window, cx, |window, cx| Self::new(weak, dump, window, cx));
    }

    fn new(
        workspace: gpui::WeakEntity<Workspace>,
        dump: Dump,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("https://example.com/docs", window, cx);
            editor
        });
        let subscription = cx.subscribe_in(&editor, window, Self::on_editor_event);
        Self {
            editor,
            dump,
            workspace,
            _subscriptions: vec![subscription],
        }
    }

    fn on_editor_event(
        &mut self,
        _: &Entity<Editor>,
        event: &editor::EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, editor::EditorEvent::Blurred) && window.is_window_active() {
            cx.emit(DismissEvent);
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.editor.read(cx).text(cx);
        if url.trim().is_empty() {
            return;
        }
        let dump = self.dump;
        self.workspace
            .update(cx, |workspace, cx| {
                open_reader(workspace, url, dump, window, cx)
            })
            .ok();
        cx.emit(DismissEvent);
    }
}

impl Render for UrlPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = format!("Open as {} through Lightpanda", self.dump.label());
        v_flex()
            .w(rems(34.))
            .elevation_2(cx)
            .key_context("LightpandaUrlPrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                div()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .px_2()
                    .py_1()
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .child(Label::new(hint).size(LabelSize::Small).color(Color::Muted)),
            )
    }
}

impl Focusable for UrlPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for UrlPrompt {}
impl ModalView for UrlPrompt {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_text_handles_empty_body() {
        let text = reader_text("https://a.test", "   \n");
        assert!(text.starts_with("<!-- fetched from https://a.test by Lightpanda -->"));
        assert!(text.contains("returned no text"));
        let text = reader_text("https://a.test", "# Hi");
        assert!(text.ends_with("# Hi"));
    }

    #[test]
    fn page_title_prefers_first_heading() {
        assert_eq!(page_title("https://a.test", "\n\n## Docs  \ntext"), "Docs");
        assert_eq!(
            page_title("https://a.test", "no headings"),
            "https://a.test"
        );
        assert_eq!(page_title("https://a.test", "#\n# Real"), "Real");
    }

    #[test]
    fn normalize_url_adds_scheme() {
        assert_eq!(
            normalize_url("  example.com ").expect("valid"),
            "https://example.com/"
        );
        assert_eq!(
            normalize_url("http://a.test/x").expect("valid"),
            "http://a.test/x"
        );
    }

    #[test]
    fn normalize_url_refuses_switches_and_non_web_schemes() {
        for input in hulder_url::HOSTILE_INPUTS {
            assert!(normalize_url(input).is_err(), "{input:?} accepted");
        }
        assert!(normalize_url("file:///etc/passwd").is_err());
        assert!(normalize_url("about:blank").is_err());
    }

    #[test]
    fn history_is_bounded_and_newest_first() {
        let mut history = FetchHistory::default();
        for index in 0..(FetchHistory::MAX_RECORDS + 5) {
            history.push(FetchRecord {
                at: SystemTime::UNIX_EPOCH,
                title: format!("t{index}"),
                url: format!("u{index}"),
                dump: Dump::Markdown,
                text: String::new(),
            });
        }
        assert_eq!(history.records().len(), FetchHistory::MAX_RECORDS);
        assert_eq!(history.records()[0].url, "u204");
    }
}
