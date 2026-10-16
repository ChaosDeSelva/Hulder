use crate::fetch::Dump;
use crate::reader::{FetchHistory, FetchRecord, disabled_message, open_reader, open_text_buffer};
use crate::runner::{PandaRunner, RunnerState};
use crate::settings::LightpandaSettings;
use crate::status::{runner_color, runner_tooltip};
use crate::{ShareWithAgents, TogglePanel, ToggleServer};
use editor::Editor;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement, Pixels, Render, Styled, Subscription, Task, WeakEntity,
    Window, div, px,
};
use settings::Settings as _;
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &TogglePanel, window, cx| {
            workspace.toggle_panel_focus::<PandaPanel>(window, cx);
        });
    })
    .detach();
}

pub struct PandaPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    url_editor: Entity<Editor>,
    dump: Dump,
    runner: Entity<PandaRunner>,
    history: Entity<FetchHistory>,
    _subscriptions: Vec<Subscription>,
}

pub fn record_time(record: &FetchRecord) -> String {
    chrono::DateTime::<chrono::Local>::from(record.at)
        .format("%H:%M:%S")
        .to_string()
}

impl PandaPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                cx.new(|cx| Self::new(workspace, window, cx))
            })
        })
    }

    fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let runner = PandaRunner::global(cx);
        let history = FetchHistory::global(cx);
        let url_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("https://example.com/docs", window, cx);
            editor
        });
        let subscriptions = vec![
            cx.observe(&runner, |_, _, cx| cx.notify()),
            cx.observe(&history, |_, _, cx| cx.notify()),
            cx.observe_global::<settings::SettingsStore>(|_, cx| cx.notify()),
        ];
        Self {
            workspace: workspace.weak_handle(),
            focus_handle: cx.focus_handle(),
            url_editor,
            dump: Dump::Markdown,
            runner,
            history,
            _subscriptions: subscriptions,
        }
    }

    fn fetch(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let url = self.url_editor.read(cx).text(cx);
        if url.trim().is_empty() {
            return;
        }
        let dump = self.dump;
        self.workspace
            .update(cx, |workspace, cx| {
                open_reader(workspace, url, dump, window, cx)
            })
            .ok();
    }

    fn set_dump(&mut self, dump: Dump, cx: &mut Context<Self>) {
        self.dump = dump;
        cx.notify();
    }

    fn reopen(&mut self, record: FetchRecord, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            open_text_buffer(workspace, record.title, record.text, cx).await
        })
        .detach_and_log_err(cx);
    }

    fn render_disabled(&self) -> impl IntoElement {
        v_flex().p_2().child(
            Label::new(disabled_message())
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
    }

    fn render_fetch_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dump = self.dump;
        v_flex()
            .gap_1()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(self.url_editor.clone()),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("panda-fetch", "Fetch")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.fetch(&menu::Confirm, window, cx)
                            })),
                    )
                    .child(
                        Button::new("panda-as-markdown", "as Markdown")
                            .toggle_state(dump == Dump::Markdown)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.set_dump(Dump::Markdown, cx)),
                            ),
                    )
                    .child(
                        Button::new("panda-as-semantic", "as semantic text")
                            .toggle_state(dump == Dump::SemanticText)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.set_dump(Dump::SemanticText, cx)),
                            ),
                    ),
            )
    }

    fn render_history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let records = self.history.read(cx).records().to_vec();
        let list = if records.is_empty() {
            v_flex().p_2().child(
                Label::new("No fetches yet this session.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        } else {
            v_flex().children(records.into_iter().enumerate().map(|(ix, record)| {
                let time = record_time(&record);
                let url = record.url.clone();
                let title = record.title.clone();
                ListItem::new(("panda-fetch", ix))
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .tooltip(Tooltip::text(format!("{url} · {}", record.dump.label())))
                    .child(
                        v_flex()
                            .child(Label::new(title).size(LabelSize::Small))
                            .child(
                                Label::new(format!("{time}  {url}"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.reopen(record.clone(), window, cx)
                    }))
            }))
        };
        v_flex()
            .id("panda-history")
            .flex_1()
            .overflow_y_scroll()
            .child(
                div().px_2().pt_2().child(
                    Label::new("This session")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .child(list)
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.runner.read(cx).state.clone();
        let (verb, tooltip) = match state {
            RunnerState::Running { .. } | RunnerState::Starting => ("Stop", runner_tooltip(&state)),
            RunnerState::Stopped | RunnerState::Failed { .. } => ("Start", runner_tooltip(&state)),
        };
        let detail = match &state {
            RunnerState::Running { port } => format!("ws://127.0.0.1:{port}"),
            RunnerState::Failed { reason } => reason.clone(),
            _ => String::new(),
        };
        v_flex()
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Bear)
                            .size(IconSize::Small)
                            .color(runner_color(&state)),
                    )
                    .child(Label::new(format!("Runner: {}", state.label())).size(LabelSize::Small))
                    .child(
                        Button::new("panda-runner-toggle", verb)
                            .tooltip(move |_window, cx| Tooltip::simple(tooltip.clone(), cx))
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.dispatch_action(ToggleServer.boxed_clone(), cx)
                            })),
                    ),
            )
            .when(!detail.is_empty(), |this| {
                this.child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
            .child(
                Button::new("panda-share", "Share with agents (.mcp.json)").on_click(cx.listener(
                    |_, _, window, cx| window.dispatch_action(ShareWithAgents.boxed_clone(), cx),
                )),
            )
    }
}

impl Render for PandaPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = LightpandaSettings::get_global(cx).enabled;
        v_flex()
            .key_context("PandaPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::fetch))
            .size_full()
            .when(!enabled, |this| this.child(self.render_disabled()))
            .when(enabled, |this| {
                this.child(self.render_fetch_bar(cx))
                    .child(self.render_history(cx))
                    .child(self.render_footer(cx))
            })
    }
}

impl Focusable for PandaPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PandaPanel {}

impl Panel for PandaPanel {
    fn persistent_name() -> &'static str {
        "PandaPanel"
    }

    fn panel_key() -> &'static str {
        "PandaPanel"
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.url_editor.focus_handle(cx)
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Bear)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Panda")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(TogglePanel)
    }

    fn activation_priority(&self) -> u32 {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_time_is_clock_only() {
        let record = FetchRecord {
            at: std::time::SystemTime::UNIX_EPOCH,
            title: "t".into(),
            url: "u".into(),
            dump: Dump::Markdown,
            text: String::new(),
        };
        assert_eq!(record_time(&record).len(), "HH:MM:SS".len());
    }
}
