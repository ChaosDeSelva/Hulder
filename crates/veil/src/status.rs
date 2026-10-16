use crate::{Toggle, Veil};
use gpui::{Action as _, App, Entity, Render, Subscription, Window};
use ui::{Color, IconButton, IconName, Tooltip, prelude::*};

/// Status bar button showing whether text is visible or veiled. Click toggles the veil. Without a veil global (a
/// test harness that never called `veil::init`) it renders nothing.
pub struct VeilButton {
    veil: Option<Entity<Veil>>,
    _subscription: Option<Subscription>,
}

impl VeilButton {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let veil = Veil::try_global(cx);
        let subscription = veil
            .as_ref()
            .map(|veil| cx.observe(veil, |_, _, cx| cx.notify()));
        Self {
            veil,
            _subscription: subscription,
        }
    }
}

impl Render for VeilButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(veil) = &self.veil else {
            return div().into_any_element();
        };
        let locked = veil.read(cx).locked();
        IconButton::new(
            "veil-status",
            if locked {
                IconName::EyeOff
            } else {
                IconName::Eye
            },
        )
        .aria_label(if locked {
            "Show text — unlock Veil"
        } else {
            "Hide text — enable Veil"
        })
        .icon_color(if locked { Color::Accent } else { Color::Muted })
        .tooltip(move |_window, cx| {
            Tooltip::with_meta(
                if locked {
                    "Show text — unlock Veil"
                } else {
                    "Hide text — enable Veil"
                },
                Some(&Toggle),
                "Draws all text as runes and blocks input until the passkey is entered; the \
                     layout stays visible. Images, PDF pages and the Chrome window are not veiled.",
                cx,
            )
        })
        .on_click(|_, window, cx| window.dispatch_action(Toggle.boxed_clone(), cx))
        .into_any_element()
    }
}

impl workspace::StatusItemView for VeilButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}
