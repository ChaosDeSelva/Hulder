use crate::ToggleServer;
use crate::runner::{PandaRunner, RunnerState};
use crate::settings::LightpandaSettings;
use gpui::{Action, App, Context, Entity, IntoElement, Render, Subscription, Window, div};
use settings::Settings as _;
use ui::{Tooltip, prelude::*};
use workspace::{HideStatusItem, ItemHandle, StatusItemView};

pub struct PandaStatus {
    runner: Entity<PandaRunner>,
    _subscriptions: Vec<Subscription>,
}

impl PandaStatus {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let runner = PandaRunner::global(cx);
        let subscriptions = vec![
            cx.observe(&runner, |_, _, cx| cx.notify()),
            cx.observe_global::<settings::SettingsStore>(|_, cx| cx.notify()),
        ];
        Self {
            runner,
            _subscriptions: subscriptions,
        }
    }
}

pub fn runner_color(state: &RunnerState) -> Color {
    match state {
        RunnerState::Stopped => Color::Muted,
        RunnerState::Starting => Color::Warning,
        RunnerState::Running { .. } => Color::Success,
        RunnerState::Failed { .. } => Color::Error,
    }
}

pub fn runner_tooltip(state: &RunnerState) -> String {
    match state {
        RunnerState::Stopped => "Lightpanda is stopped. Click to start the CDP server.".into(),
        RunnerState::Starting => "Lightpanda is starting…".into(),
        RunnerState::Running { port } => {
            format!("Lightpanda is serving on ws://127.0.0.1:{port}. Click to stop.")
        }
        RunnerState::Failed { reason } => format!("Lightpanda failed: {reason}. Click to retry."),
    }
}

impl Render for PandaStatus {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !LightpandaSettings::get_global(cx).enabled {
            return div().into_any_element();
        }
        let state = self.runner.read(cx).state.clone();
        let tooltip = runner_tooltip(&state);
        IconButton::new("panda-status", IconName::Bear)
            .icon_size(IconSize::Small)
            .icon_color(runner_color(&state))
            .tooltip(move |_window, cx| Tooltip::simple(tooltip.clone(), cx))
            .on_click(cx.listener(|_, _, window, cx| {
                window.dispatch_action(ToggleServer.boxed_clone(), cx)
            }))
            .into_any_element()
    }
}

impl StatusItemView for PandaStatus {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    // Already conditional on `lightpanda.enabled`.
    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_names_port_and_failure_reason() {
        assert!(
            runner_tooltip(&RunnerState::Running { port: 9222 }).contains("ws://127.0.0.1:9222")
        );
        assert!(
            runner_tooltip(&RunnerState::Failed {
                reason: "address in use".into()
            })
            .contains("address in use")
        );
        assert_eq!(runner_color(&RunnerState::Stopped), Color::Muted);
        assert_eq!(
            runner_color(&RunnerState::Running { port: 1 }),
            Color::Success
        );
    }
}
