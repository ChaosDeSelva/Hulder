use crate::{NetEntryKind, NetLog};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    Action, App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement, Render, Subscription, Task, WeakEntity, Window,
    actions,
};
use picker::{Picker, PickerDelegate};
use std::sync::Arc;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, ModalView, StatusItemView, Workspace};

actions!(
    hollow_back,
    [
        /// Opens the Hollow Back log of refused and browsed requests.
        Toggle
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            workspace.toggle_modal(window, cx, |window, cx| NetLogView::new(window, cx));
        });
    })
    .detach();
}

pub fn status_label(refused: u64, browsed: u64) -> String {
    if browsed == 0 {
        refused.to_string()
    } else {
        format!("{refused} · {browsed}")
    }
}

pub fn status_tooltip(refused: u64, browsed: u64) -> String {
    format!("Hollow Back: {refused} requests refused · {browsed} pages browsed via Lightpanda")
}

pub struct HollowBack {
    log: Entity<NetLog>,
    _subscription: Subscription,
}

impl HollowBack {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let log = NetLog::global(cx);
        let subscription = cx.observe(&log, |_, _, cx| cx.notify());
        Self {
            log,
            _subscription: subscription,
        }
    }
}

impl Render for HollowBack {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let log = self.log.read(cx);
        let label = status_label(log.refused, log.browsed);
        let tooltip = status_tooltip(log.refused, log.browsed);
        Button::new("hollow-back", label)
            .start_icon(Icon::new(IconName::Eye).size(IconSize::Small).color(Color::Muted))
            .tooltip(move |_window, cx| Tooltip::simple(tooltip.clone(), cx))
            .on_click(cx.listener(|_, _, window, cx| {
                window.dispatch_action(Toggle.boxed_clone(), cx)
            }))
    }
}

impl StatusItemView for HollowBack {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    // Always on: seeing what leaves the machine is the point of the fork.
    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

pub struct NetLogView {
    picker: Entity<Picker<NetLogDelegate>>,
}

impl NetLogView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = NetLogDelegate::new(cx.entity().downgrade(), cx);
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for NetLogView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("NetLogView")
            .w(rems(48.))
            .child(self.picker.clone())
            .child(
                div().mx_2().my_1().child(
                    Label::new("Agent browsing through their own Lightpanda is not logged here.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
    }
}

impl Focusable for NetLogView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for NetLogView {}
impl ModalView for NetLogView {}

pub fn entry_line(at: std::time::SystemTime, kind: NetEntryKind, method: &str, url: &str) -> String {
    let time = chrono::DateTime::<chrono::Local>::from(at).format("%H:%M:%S");
    let kind = match kind {
        NetEntryKind::Refused => "REFUSED",
        NetEntryKind::Browsed => "BROWSED",
        NetEntryKind::Bridge => "BRIDGE",
        NetEntryKind::Sent => "SENT",
    };
    format!("{time}  {kind:<7}  {method:<6}  {url}")
}

pub struct NetLogDelegate {
    view: WeakEntity<NetLogView>,
    candidates: Arc<Vec<StringMatchCandidate>>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl NetLogDelegate {
    fn new(view: WeakEntity<NetLogView>, cx: &App) -> Self {
        let candidates = NetLog::global(cx)
            .read(cx)
            .entries()
            .enumerate()
            .map(|(id, entry)| {
                StringMatchCandidate::new(
                    id,
                    &entry_line(entry.at, entry.kind, &entry.method, &entry.url),
                )
            })
            .collect();
        Self {
            view,
            candidates: Arc::new(candidates),
            matches: Vec::new(),
            selected_index: 0,
        }
    }
}

impl PickerDelegate for NetLogDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "hollow back log"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Filter refused, browsed, bridged and sent requests…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("Nothing has tried to leave the machine yet.".into())
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string.clone(),
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    crate::MAX_ENTRIES,
                    &Default::default(),
                    background,
                )
                .await
            };
            this.update_in(cx, |this, window, cx| {
                this.delegate.matches = matches;
                this.set_selected_index(0, None, false, window, cx);
                cx.notify();
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.view.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let found = self.matches.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    found.string.clone(),
                    found.positions.clone(),
                )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_shows_both_counts_only_when_nonzero() {
        assert_eq!(status_label(0, 0), "0");
        assert_eq!(status_label(12, 0), "12");
        assert_eq!(status_label(12, 3), "12 · 3");
    }

    #[test]
    fn sent_entries_have_their_own_column_word() {
        let line = entry_line(
            std::time::SystemTime::UNIX_EPOCH,
            NetEntryKind::Sent,
            "POST",
            "api.test  201  12 B up / 345 B down",
        );
        assert!(line.contains("SENT     POST    api.test"), "{line}");
        assert!(status_tooltip(1, 2).contains("refused"));
    }

    #[test]
    fn entry_line_is_aligned_and_newest_url_last() {
        let line = entry_line(
            std::time::SystemTime::UNIX_EPOCH,
            NetEntryKind::Refused,
            "GET",
            "https://a.test/x",
        );
        assert!(line.contains("REFUSED  GET     https://a.test/x"), "{line}");
        assert!(line.ends_with("https://a.test/x"));
        let (time, _) = line.split_once("  ").expect("time column");
        assert_eq!(time.len(), "HH:MM:SS".len(), "{time}");
    }
}
