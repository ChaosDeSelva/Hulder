use crate::chrome_runner::ChromeRunner;
use editor::Editor;
use gpui::{
    App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Styled,
    Subscription, WeakEntity, div, prelude::*,
};
use theme::ActiveTheme;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{DismissDecision, ModalView, Workspace};

/// Single-line prompt for `browser::Open`, modelled on the go-to-line modal.
pub struct OpenUrlModal {
    url_editor: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
    _subscriptions: Vec<Subscription>,
}

impl ModalView for OpenUrlModal {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> DismissDecision {
        DismissDecision::Dismiss(true)
    }
}

impl Focusable for OpenUrlModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.url_editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for OpenUrlModal {}

impl OpenUrlModal {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            let placeholder = ChromeRunner::global(cx)
                .read(cx)
                .page_url()
                .map(SharedString::from)
                .unwrap_or_else(|| "https://".into());
            editor.set_placeholder_text(&placeholder, window, cx);
            editor
        });
        let subscription = cx.subscribe_in(&url_editor, window, Self::on_editor_event);
        Self {
            url_editor,
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
        if let editor::EditorEvent::Blurred = event
            && window.is_window_active()
        {
            cx.emit(DismissEvent);
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let url = self.url_editor.read(cx).text(cx);
        if url.trim().is_empty() {
            return;
        }
        let workspace = self.workspace.clone();
        let open = ChromeRunner::global(cx).update(cx, |runner, cx| runner.open(url, cx));
        cx.spawn(async move |_this, cx| {
            if let Err(error) = open.await {
                workspace
                    .update(cx, |workspace, cx| {
                        crate::toast(workspace, format!("Browser: {error:#}"), cx);
                    })
                    .log_err();
            }
        })
        .detach();
        cx.emit(DismissEvent);
    }
}

impl Render for OpenUrlModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(32.))
            .elevation_2(cx)
            .key_context("OpenUrlModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                div()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .px_2()
                    .py_1()
                    .child(self.url_editor.clone()),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .child(Label::new("Open in Chrome (enter to confirm)").color(Color::Muted)),
            )
    }
}
