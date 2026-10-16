//! The Veil: a privacy screen. While locked every window draws its text as Elder Futhark and
//! looks otherwise exactly as it did; a transparent overlay swallows the mouse and the
//! keyboard. A temporary password lifts it; quitting the app clears it as a recovery path.
//! The eye icon at the status bar's left end opens the readable unlock popover.

pub mod runes;
pub mod status;
pub mod store;

use anyhow::Result;
use editor::{Editor, EditorEvent};
use gpui::{
    Animation, AnimationExt as _, AnyWindowHandle, App, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Global, KeyDownEvent, Keystroke, Modifiers, Render, SharedString,
    Subscription, Task, TextVeil, TextVeilSource, TextVeilState, WeakEntity, Window, WindowId,
    actions, div, px, rems,
};
use parking_lot::Mutex;
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use theme::ActiveTheme as _;
use ui::{Color, IconButton, IconName, Label, LabelSize, Tooltip, h_flex, prelude::*, v_flex};
use util::ResultExt as _;
use workspace::{DismissDecision, ModalView, Workspace};
use zeroize::Zeroizing;

pub use status::VeilButton;
pub use store::{DAMAGED_MESSAGE, StoreState, VeilStore, Verification, lockout_duration};

actions!(
    veil,
    [
        /// Veils every window, or opens the unlock popover when already veiled.
        Toggle,
        /// Sets or changes the veil passkey.
        SetPasskey
    ]
);

/// The native title every window shows while locked, so a dock or window switcher reveals no
/// file or project name.
pub const NEUTRAL_TITLE: &str = "Hulder";

/// Whether a keystroke must be swallowed while locked. Anything that types text (it carries a
/// `key_char`) reaches the unlock field whatever modifiers the layout needed for it: option,
/// shift and dead-key sequences all produce a `key_char`. A chord with command or control, or a
/// bare chord with option or function, would reach a workspace binding and is dropped, except
/// the few editing chords the unlock field itself needs. Actions that still resolve from the
/// remaining keystrokes are filtered again by [`allows_action`].
pub fn blocks_keystroke(keystroke: &Keystroke) -> bool {
    let modifiers = keystroke.modifiers;
    let command_or_control = modifiers.platform || modifiers.control;
    if modifiers.platform
        && !modifiers.control
        && !modifiers.alt
        && !modifiers.shift
        && keystroke.key == "q"
    {
        return false;
    }
    if keystroke.key_char.is_some() {
        return command_or_control;
    }
    if !(command_or_control || modifiers.alt || modifiers.function) {
        return false;
    }
    let platform_only =
        modifiers.platform && !modifiers.control && !modifiers.alt && !modifiers.function;
    !(platform_only && matches!(keystroke.key.as_str(), "v" | "a" | "backspace"))
}

/// Why `passkey` could not be typed into the unlock field while locked, if it could not. Every
/// character is pushed through [`blocks_keystroke`] as a layout might produce it: plain, with
/// shift, with option, and with both.
pub fn untypeable_reason(passkey: &str) -> Option<String> {
    for ch in passkey.chars() {
        if ch.is_control() {
            return Some(format!(
                "the passkey cannot contain control characters (U+{:04X})",
                ch as u32
            ));
        }
        let typed = ch.to_string();
        let modifier_sets = [
            Modifiers::default(),
            Modifiers::shift(),
            Modifiers::alt(),
            Modifiers {
                alt: true,
                shift: true,
                ..Modifiers::default()
            },
        ];
        let typeable = modifier_sets.into_iter().all(|modifiers| {
            !blocks_keystroke(&Keystroke {
                key: typed.clone(),
                key_char: Some(typed.clone()),
                modifiers,
            })
        });
        if !typeable {
            return Some(format!("'{ch}' cannot be typed while veiled"));
        }
    }
    None
}

/// The only actions that may run while locked: the unlock field's own editing, confirming and
/// cancelling, and the toggle (which only opens the unlock popover).
pub fn allows_action(name: &str) -> bool {
    matches!(
        name,
        "zed::Quit"
            | "veil::Toggle"
            | "menu::Confirm"
            | "menu::Cancel"
            | "editor::Backspace"
            | "editor::Delete"
            | "editor::DeleteToBeginningOfLine"
            | "editor::DeleteToEndOfLine"
            | "editor::DeleteToPreviousWordStart"
            | "editor::DeleteToNextWordEnd"
            | "editor::MoveLeft"
            | "editor::MoveRight"
            | "editor::MoveToBeginningOfLine"
            | "editor::MoveToEndOfLine"
            | "editor::MoveToBeginning"
            | "editor::MoveToEnd"
            | "editor::SelectLeft"
            | "editor::SelectRight"
            | "editor::SelectToBeginningOfLine"
            | "editor::SelectToEndOfLine"
            | "editor::SelectAll"
            | "editor::Paste"
            | "editor::Newline"
            | "editor::Cancel"
    )
}

/// Whether `keystroke` is bound to [`Toggle`] in `window`, so the chord that opens the unlock
/// popover gets past the keystroke interceptor.
fn is_toggle_chord(window: &Window, keystroke: &Keystroke) -> bool {
    window
        .bindings_for_action(&Toggle)
        .iter()
        .any(|binding| match binding.keystrokes() {
            [only] => keystroke.should_match(only),
            _ => false,
        })
}

fn runic_veil() -> TextVeil {
    Arc::new(runes::transliterate)
}

fn wait_message(duration: Duration) -> SharedString {
    format!("wait {} s", duration.as_secs().max(1)).into()
}

struct GlobalVeil(Entity<Veil>);

impl Global for GlobalVeil {}

pub fn init(cx: &mut App) {
    init_with_store(Arc::new(VeilStore::session()), cx);
}

pub fn init_with_store(store: Arc<VeilStore>, cx: &mut App) {
    // The one synchronous read: whether the app comes back veiled must be known before the
    // first window draws, or a restored window would show its text for a frame.
    let state = store.state();
    let veil = cx.new(|cx| Veil::new(store, state, cx));
    cx.set_global(GlobalVeil(veil.clone()));
    // gpui reads the veil from this global at the start of every draw in every window, so a
    // window opened while locked is veiled on its first frame and no window can opt out.
    let shared = veil.read(cx).shared.clone();
    cx.set_global(TextVeilState::new(Arc::new(VeilSource(shared))));
    // A relaunch comes back veiled: `lock` runs before any workspace exists, so every restored
    // window gets its overlay and text veil through the `observe_new` path below.
    if veil.read(cx).state.is_locked() {
        veil.update(cx, |veil, cx| veil.lock(cx));
    }

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        workspace.register_action(|_, _: &Toggle, window, cx| {
            let workspace = cx.entity();
            let window_handle = window.window_handle();
            App::defer(cx, move |cx| Veil::toggle(workspace, window_handle, cx));
        });
        workspace.register_action(|_, _: &SetPasskey, window, cx| {
            let workspace = cx.entity();
            let window_handle = window.window_handle();
            App::defer(cx, move |cx| {
                Veil::open_passkey_modal(workspace, window_handle, false, cx);
            });
        });

        let veil = Veil::global(cx);
        let workspace_entity = cx.entity();
        let window_handle = window.window_handle();
        let locked = veil.update(cx, |veil, _| {
            veil.workspaces
                .push((workspace_entity.downgrade(), window_handle));
            veil.locked
        });
        if locked {
            let (store, shared) =
                veil.read_with(cx, |veil, _| (veil.store.clone(), veil.shared.clone()));
            let overlay = build_overlay(veil.downgrade(), store, shared, window, cx);
            veil.update(cx, |veil, _| {
                veil.overlays.push((window_handle, overlay.clone()));
                veil.shared.set_overlay_windows(&veil.overlays);
            });
            workspace.set_overlay(Some(overlay.into()), cx);
        }
    })
    .detach();
}

/// State the gpui text veil, the keystroke interceptor and the action interceptor read without
/// touching the entity, since they can fire while the entity itself is mid-update.
#[derive(Default)]
struct SharedState {
    locked: AtomicBool,
    /// Bumped on every lock and unlock so windows refetch the veil and drop their caches.
    generation: AtomicU64,
    /// Windows that carry a [`VeilOverlay`]. Every other window is shielded wholesale by gpui
    /// while locked: no mouse, and the keystroke interceptor drops every key.
    overlay_windows: Mutex<HashSet<WindowId>>,
}

impl SharedState {
    fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }

    fn set_locked(&self, locked: bool) {
        self.locked.store(locked, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn has_overlay(&self, window: WindowId) -> bool {
        self.overlay_windows.lock().contains(&window)
    }

    fn set_overlay_windows(&self, overlays: &[(AnyWindowHandle, Entity<VeilOverlay>)]) {
        *self.overlay_windows.lock() = overlays
            .iter()
            .map(|(handle, _)| handle.window_id())
            .collect();
    }
}

/// What gpui consults for the veil: runes while locked, nothing otherwise.
struct VeilSource(Arc<SharedState>);

impl TextVeilSource for VeilSource {
    fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::Acquire)
    }

    fn veil(&self) -> Option<TextVeil> {
        self.0.is_locked().then(runic_veil)
    }

    fn shields_mouse(&self, window: WindowId) -> bool {
        !self.0.has_overlay(window)
    }

    fn neutral_window_title(&self) -> Option<SharedString> {
        Some(NEUTRAL_TITLE.into())
    }
}

pub struct Veil {
    locked: bool,
    /// The last thing `veil.json` said. Refreshed off the main thread after every write.
    state: StoreState,
    store: Arc<VeilStore>,
    shared: Arc<SharedState>,
    workspaces: Vec<(WeakEntity<Workspace>, AnyWindowHandle)>,
    overlays: Vec<(AnyWindowHandle, Entity<VeilOverlay>)>,
    /// Store work runs in a chain so writes land in the order they were requested.
    store_task: Option<Task<()>>,
    _interceptor: Subscription,
    _action_interceptor: Subscription,
}

impl Veil {
    fn new(store: Arc<VeilStore>, state: StoreState, cx: &mut Context<Self>) -> Self {
        let shared = Arc::new(SharedState::default());
        let interceptor = cx.intercept_keystrokes({
            let shared = shared.clone();
            move |event, window, cx| {
                if !shared.is_locked() {
                    return;
                }
                if !shared.has_overlay(window.window_handle().window_id()) {
                    cx.stop_propagation();
                    return;
                }
                if blocks_keystroke(&event.keystroke) && !is_toggle_chord(window, &event.keystroke)
                {
                    cx.stop_propagation();
                }
            }
        });
        let action_interceptor = cx.intercept_actions({
            let shared = shared.clone();
            move |action, _window, cx| {
                if !shared.is_locked() {
                    return;
                }
                if !allows_action(action.name()) {
                    cx.stop_propagation();
                }
            }
        });
        Self {
            locked: false,
            state,
            store,
            shared,
            workspaces: Vec::new(),
            overlays: Vec::new(),
            store_task: None,
            _interceptor: interceptor,
            _action_interceptor: action_interceptor,
        }
    }

    pub fn global(cx: &App) -> Entity<Veil> {
        cx.global::<GlobalVeil>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Veil>> {
        cx.try_global::<GlobalVeil>().map(|global| global.0.clone())
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    pub fn state(&self) -> &StoreState {
        &self.state
    }

    pub fn failed_attempts(&self) -> u32 {
        self.state.failed_attempts()
    }

    pub fn store(&self) -> &Arc<VeilStore> {
        &self.store
    }

    /// Runs `work` against the store on a background thread, after any store work already
    /// queued, and hands the result back on the main thread.
    fn with_store<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce(&VeilStore) -> R + Send + 'static,
        done: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) {
        let previous = self.store_task.take();
        let store = self.store.clone();
        self.store_task = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let result = cx.background_spawn(async move { work(&store) }).await;
            this.update(cx, |this, cx| done(this, result, cx)).log_err();
        }));
    }

    /// Re-reads `veil.json` off the main thread and caches what it says.
    pub fn refresh_state(&mut self, cx: &mut Context<Self>) {
        self.with_store(cx, VeilStore::state, |this, state, cx| {
            this.state = state;
            cx.notify();
        });
    }

    /// Runs outside any window or entity update: it touches every workspace and window.
    fn toggle(workspace: Entity<Workspace>, window_handle: AnyWindowHandle, cx: &mut App) {
        let veil = Self::global(cx);
        let locked = veil.read(cx).locked;
        if locked {
            veil.update(cx, |veil, cx| veil.open_unlock_popover(window_handle, cx));
        } else {
            Self::open_passkey_modal(workspace, window_handle, true, cx);
        }
    }

    fn open_passkey_modal(
        workspace: Entity<Workspace>,
        window_handle: AnyWindowHandle,
        lock_afterwards: bool,
        cx: &mut App,
    ) {
        let veil = Self::global(cx);
        if veil.read(cx).locked {
            return;
        }
        window_handle
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        PasskeyModal::new(veil, lock_afterwards, window, cx)
                    });
                });
            })
            .log_err();
    }

    pub fn lock(&mut self, cx: &mut Context<Self>) {
        if self.locked || !self.state.has_passkey() {
            return;
        }
        self.locked = true;
        if let StoreState::Present { locked, .. } = &mut self.state {
            *locked = true;
        }
        self.workspaces
            .retain(|(workspace, _)| workspace.upgrade().is_some());

        let veil = cx.weak_entity();
        let mut overlays = Vec::new();
        for (workspace, window_handle) in self.workspaces.clone() {
            let veil = veil.clone();
            let store = self.store.clone();
            let shared = self.shared.clone();
            let overlay = window_handle.update(cx, |_, window, cx| {
                let overlay = build_overlay(veil, store, shared, window, cx);
                workspace.update(cx, |workspace, cx| {
                    workspace.set_overlay(Some(overlay.clone().into()), cx);
                    overlay
                })
            });
            if let Some(overlay) = overlay.log_err().and_then(|overlay| overlay.log_err()) {
                overlays.push((window_handle, overlay));
            }
        }
        self.overlays = overlays;
        self.shared.set_overlay_windows(&self.overlays);
        self.shared.set_locked(true);
        if self.store.is_session() {
            self.store.set_locked(true).log_err();
            self.state = self.store.state();
        } else {
            self.with_store(
                cx,
                |store| {
                    if let Err(error) = store.set_locked(true) {
                        log::error!("could not persist the veil lock: {error:#}");
                    }
                    store.state()
                },
                |this, state, cx| {
                    this.state = state;
                    cx.notify();
                },
            );
        }
        cx.refresh_windows();
        cx.notify();
    }

    pub fn unlock(&mut self, cx: &mut Context<Self>) {
        if !self.locked {
            return;
        }
        self.locked = false;
        self.shared.set_locked(false);
        if let StoreState::Present {
            locked,
            failed_attempts,
            locked_until,
        } = &mut self.state
        {
            *locked = false;
            *failed_attempts = 0;
            *locked_until = 0;
        }
        self.overlays.clear();
        self.shared.set_overlay_windows(&self.overlays);
        for (workspace, window_handle) in self.workspaces.clone() {
            window_handle
                .update(cx, |_, window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.set_overlay(None, cx);
                        let pane = workspace.active_pane().clone();
                        pane.read(cx).focus_handle(cx).focus(window, cx);
                    })
                })
                .log_err();
        }
        if self.store.is_session() {
            self.store.set_locked(false).log_err();
            self.state = self.store.state();
        } else {
            self.with_store(
                cx,
                |store| {
                    if let Err(error) = store.set_locked(false) {
                        log::error!("could not persist the veil unlock: {error:#}");
                    }
                    store.state()
                },
                |this, state, cx| {
                    this.state = state;
                    cx.notify();
                },
            );
        }
        cx.refresh_windows();
        cx.notify();
    }

    fn open_unlock_popover(&self, window_handle: AnyWindowHandle, cx: &mut App) {
        let Some((_, overlay)) = self
            .overlays
            .iter()
            .find(|(handle, _)| *handle == window_handle)
        else {
            return;
        };
        let overlay = overlay.clone();
        window_handle
            .update(cx, |_, window, cx| {
                overlay.update(cx, |overlay, cx| overlay.open_popover(window, cx));
            })
            .log_err();
    }
}

fn build_overlay(
    veil: WeakEntity<Veil>,
    store: Arc<VeilStore>,
    shared: Arc<SharedState>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<VeilOverlay> {
    let overlay = cx.new(|cx| VeilOverlay::new(veil, store, shared, window, cx));
    overlay.update(cx, |overlay, cx| overlay.focus(window, cx));
    overlay
}

/// A single-line editor that draws mask characters instead of the typed text, since the rune
/// veil is off while a passkey is being set.
fn passkey_editor(placeholder: &str, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        editor.set_masked(true, cx);
        editor
    })
}

/// Takes the typed passkey out of `editor`, leaving it empty. The returned buffer is wiped
/// when dropped; the editor's own rope is cleared but not scrubbed.
fn take_passkey(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) -> Zeroizing<String> {
    let passkey = Zeroizing::new(editor.read(cx).text(cx));
    editor.update(cx, |editor, cx| editor.clear(window, cx));
    passkey
}

/// Handles Enter whether or not the keymap maps it to `menu::Confirm` for this context.
fn is_plain_enter(event: &KeyDownEvent) -> bool {
    event.keystroke.key == "enter" && !event.keystroke.modifiers.modified()
}

fn is_plain_escape(event: &KeyDownEvent) -> bool {
    event.keystroke.key == "escape" && !event.keystroke.modifiers.modified()
}

/// The full-window lock surface. It draws nothing of its own but the hood icon at the status
/// bar's left end, so the window looks exactly as it did, in runes. Every mouse event stops
/// here; keys go to its own focus handle (where they do nothing) until the popover is open.
pub struct VeilOverlay {
    veil: WeakEntity<Veil>,
    store: Arc<VeilStore>,
    shared: Arc<SharedState>,
    focus_handle: FocusHandle,
    popover: Option<(Entity<UnlockPopover>, Subscription)>,
    _focus_out: Subscription,
}

impl VeilOverlay {
    fn new(
        veil: WeakEntity<Veil>,
        store: Arc<VeilStore>,
        shared: Arc<SharedState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let focus_out = cx.on_focus_out(&focus_handle, window, |this, _, window, cx| {
            if this.shared.is_locked() && this.popover.is_none() {
                this.focus(window, cx);
            }
        });
        Self {
            veil,
            store,
            shared,
            focus_handle,
            popover: None,
            _focus_out: focus_out,
        }
    }

    pub fn popover(&self) -> Option<&Entity<UnlockPopover>> {
        self.popover.as_ref().map(|(popover, _)| popover)
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        match self.popover() {
            Some(popover) => {
                let focus_handle = popover.read(cx).focus_handle(cx);
                focus_handle.focus(window, cx);
            }
            None => self.focus_handle.focus(window, cx),
        }
    }

    pub fn open_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.popover.is_none() {
            let popover = cx.new(|cx| {
                UnlockPopover::new(
                    self.veil.clone(),
                    self.store.clone(),
                    self.shared.clone(),
                    window,
                    cx,
                )
            });
            let dismissed =
                cx.subscribe_in(&popover, window, |this, _, _: &DismissEvent, window, cx| {
                    this.close_popover(window, cx)
                });
            self.popover = Some((popover, dismissed));
        }
        self.focus(window, cx);
        cx.notify();
    }

    fn close_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.popover = None;
        self.focus(window, cx);
        cx.notify();
    }
}

impl Focusable for VeilOverlay {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.popover() {
            Some(popover) => popover.read(cx).focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }
}

impl Render for VeilOverlay {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The same padding the status bar gives its items, so the hood sits where the status
        // item is drawn underneath.
        let status_padding = DynamicSpacing::Base04.rems(cx);
        let hood = h_flex()
            .absolute()
            .bottom_0()
            .left_0()
            .p(status_padding)
            .child(
                IconButton::new("veil-unlock", IconName::EyeOff)
                    .aria_label("Show text — unlock Veil")
                    .icon_color(Color::Accent)
                    .tooltip(|_window, cx| {
                        Tooltip::with_meta(
                            "Unveil",
                            Some(&Toggle),
                            "The window is veiled: text is drawn as runes and input is blocked \
                             until the passkey is entered. The layout underneath stays visible.",
                            cx,
                        )
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_popover(window, cx);
                        cx.stop_propagation();
                    })),
            );

        div()
            .id("veil-overlay")
            .key_context("VeilOverlay")
            .track_focus(&self.focus_handle)
            .size_full()
            .occlude()
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .child(hood)
            .when_some(self.popover().cloned(), |this, popover| {
                this.child(
                    div()
                        .absolute()
                        .left(status_padding)
                        .bottom(rems(2.5))
                        .child(popover),
                )
            })
    }
}

/// The small unlock card anchored above the hood icon: one masked field, Enter to unlock,
/// Escape to close and stay locked. Its labels and password controls remain readable.
pub struct UnlockPopover {
    plain_text: Arc<gpui::WindowTextSystem>,
    veil: WeakEntity<Veil>,
    store: Arc<VeilStore>,
    shared: Arc<SharedState>,
    editor: Entity<Editor>,
    message: Option<SharedString>,
    shake_count: usize,
    pending_attempt: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl UnlockPopover {
    fn new(
        veil: WeakEntity<Veil>,
        store: Arc<VeilStore>,
        shared: Arc<SharedState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = passkey_editor("passkey", window, cx);
        let editor_events = cx.subscribe_in(&editor, window, Self::on_editor_event);
        Self {
            plain_text: Arc::new(gpui::WindowTextSystem::new(cx.text_system().clone())),
            veil,
            store,
            shared,
            editor,
            message: None,
            shake_count: 0,
            pending_attempt: None,
            _subscriptions: vec![editor_events],
        }
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        self.editor.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn on_editor_event(
        &mut self,
        _: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, EditorEvent::Blurred) && self.shared.is_locked() {
            self.focus(window, cx);
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm_action(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm(window, cx);
    }

    pub fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_attempt.is_some() {
            return;
        }
        let passkey = take_passkey(&self.editor, window, cx);
        if passkey.is_empty() {
            return;
        }
        self.message = Some("verifying".into());
        let store = self.store.clone();
        self.pending_attempt = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { store.verify(&passkey) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.pending_attempt = None;
                this.finish_attempt(result, window, cx);
            })
            .log_err();
        }));
        cx.notify();
    }

    fn finish_attempt(
        &mut self,
        result: Result<Verification>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(Verification::Verified) => {
                self.message = None;
                // Unlocking drops the overlay and this popover with it, so it must not run
                // while this entity is still being updated.
                App::defer(cx, |cx| {
                    Veil::global(cx).update(cx, |veil, cx| veil.unlock(cx));
                });
            }
            Ok(Verification::Rejected { wait }) => {
                self.veil
                    .update(cx, |veil, cx| veil.refresh_state(cx))
                    .log_err();
                self.shake_count += 1;
                self.message = Some(if wait.is_zero() {
                    "wrong passkey".into()
                } else {
                    format!("wrong passkey, {}", wait_message(wait)).into()
                });
                self.focus(window, cx);
            }
            Ok(Verification::LockedOut { remaining }) => {
                self.shake_count += 1;
                self.message = Some(wait_message(remaining));
                self.focus(window, cx);
            }
            Err(error) => {
                self.veil
                    .update(cx, |veil, cx| veil.refresh_state(cx))
                    .log_err();
                self.message = Some(error.to_string().into());
                self.focus(window, cx);
            }
        }
        cx.notify();
    }

    /// The explicit message from the last attempt, or what the store state implies: a damaged
    /// file, or a lockout still running.
    fn current_message(&self, cx: &App) -> Option<SharedString> {
        if let Some(message) = &self.message {
            return Some(message.clone());
        }
        let state = self.veil.upgrade()?.read(cx).state.clone();
        if state.is_damaged() {
            return Some(DAMAGED_MESSAGE.into());
        }
        state.lockout_remaining().map(wait_message)
    }
}

impl EventEmitter<DismissEvent> for UnlockPopover {}

impl Focusable for UnlockPopover {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for UnlockPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let border = colors.border;
        let error_border = colors.border_focused;
        let message = self.current_message(cx);
        let field = h_flex()
            .w(rems(16.))
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(colors.editor_background)
            .child(div().flex_1().min_w_0().child(self.editor.clone()))
            .child(
                IconButton::new(
                    "veil-password-visibility",
                    if self.editor.read(cx).is_masked(cx) {
                        IconName::Eye
                    } else {
                        IconName::EyeOff
                    },
                )
                .aria_label(if self.editor.read(cx).is_masked(cx) {
                    "Show password"
                } else {
                    "Hide password"
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    this.editor.update(cx, |editor, cx| {
                        editor.set_masked(!editor.is_masked(cx), cx)
                    });
                    this.editor.read(cx).focus_handle(cx).focus(window, cx);
                    cx.notify();
                })),
            );
        let field = if self.shake_count > 0 {
            field
                .with_animation(
                    SharedString::from(format!("veil-shake-{}", self.shake_count)),
                    Animation::new(Duration::from_millis(400)),
                    move |field, delta| {
                        let offset = (delta * std::f32::consts::PI * 4.).sin() * (1. - delta) * 10.;
                        field.ml(px(offset)).border_color(if delta < 0.8 {
                            error_border
                        } else {
                            border
                        })
                    },
                )
                .into_any_element()
        } else {
            field.into_any_element()
        };

        ReadableControls {
            text_system: self.plain_text.clone(),
            child: v_flex()
                .id("veil-unlock-popover")
                .key_context("VeilUnlock")
                .occlude()
                .elevation_2(cx)
                .p_2()
                .gap_2()
                .on_action(cx.listener(Self::cancel))
                .on_action(cx.listener(Self::confirm_action))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if is_plain_enter(event) {
                        this.confirm(window, cx);
                        cx.stop_propagation();
                    } else if is_plain_escape(event) {
                        cx.emit(DismissEvent);
                        cx.stop_propagation();
                    }
                }))
                .child(Label::new("Unlock Veil"))
                .child(field)
                .child(
                    Label::new("Forgot it? Quit and reopen Hulder to reset.")
                        .size(LabelSize::Small),
                )
                .when_some(message, |this, message| {
                    this.child(
                        Label::new(message)
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                })
                .into_any_element(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasskeyStep {
    ConfirmCurrent,
    EnterNew,
}

/// Sets or changes the passkey. When one exists the current key is asked for first; the new
/// key is typed twice.
pub struct PasskeyModal {
    veil: Entity<Veil>,
    editor: Entity<Editor>,
    repeat_editor: Entity<Editor>,
    step: PasskeyStep,
    message: Option<SharedString>,
    lock_afterwards: bool,
    pending: Option<Task<()>>,
}

impl PasskeyModal {
    fn new(
        veil: Entity<Veil>,
        lock_afterwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let has_passkey = veil.read(cx).state.has_passkey();
        let step = if has_passkey && !lock_afterwards {
            PasskeyStep::ConfirmCurrent
        } else {
            PasskeyStep::EnterNew
        };
        let editor = passkey_editor(Self::placeholder(step), window, cx);
        let repeat_editor = passkey_editor("repeat the new passkey", window, cx);
        Self {
            veil,
            editor,
            repeat_editor,
            step,
            message: None,
            lock_afterwards,
            pending: None,
        }
    }

    fn placeholder(step: PasskeyStep) -> &'static str {
        match step {
            PasskeyStep::ConfirmCurrent => "current passkey",
            PasskeyStep::EnterNew => "new passkey",
        }
    }

    fn clear_editors(&self, window: &mut Window, cx: &mut App) {
        for editor in [&self.editor, &self.repeat_editor] {
            editor.update(cx, |editor, cx| editor.clear(window, cx));
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_editors(window, cx);
        cx.emit(DismissEvent);
    }

    fn confirm_action(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm(window, cx);
    }

    /// Moves between the two fields of the new-passkey step.
    fn focus_other_field(&self, window: &mut Window, cx: &mut App) {
        let repeat_focused = self
            .repeat_editor
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let next = if repeat_focused {
            &self.editor
        } else {
            &self.repeat_editor
        };
        next.read(cx).focus_handle(cx).focus(window, cx);
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let store = self.veil.read(cx).store.clone();
        match self.step {
            PasskeyStep::ConfirmCurrent => {
                let current = take_passkey(&self.editor, window, cx);
                if current.is_empty() {
                    return;
                }
                self.message = Some("verifying".into());
                self.pending = Some(cx.spawn_in(window, async move |this, cx| {
                    let result = cx
                        .background_spawn(async move { store.verify(&current) })
                        .await;
                    this.update_in(cx, |this, window, cx| {
                        this.pending = None;
                        match result {
                            Ok(Verification::Verified) => this.advance_to_new(window, cx),
                            Ok(Verification::Rejected { wait }) => {
                                this.message = Some(if wait.is_zero() {
                                    "wrong passkey".into()
                                } else {
                                    format!("wrong passkey, {}", wait_message(wait)).into()
                                });
                            }
                            Ok(Verification::LockedOut { remaining }) => {
                                this.message = Some(wait_message(remaining));
                            }
                            Err(error) => this.message = Some(error.to_string().into()),
                        }
                        this.veil.update(cx, |veil, cx| veil.refresh_state(cx));
                        cx.notify();
                    })
                    .log_err();
                }));
            }
            PasskeyStep::EnterNew => {
                // Enter in the first field with nothing repeated yet just moves on.
                if self.editor.read(cx).focus_handle(cx).is_focused(window)
                    && !self.editor.read(cx).text(cx).is_empty()
                    && self.repeat_editor.read(cx).text(cx).is_empty()
                {
                    self.focus_other_field(window, cx);
                    return;
                }
                let new_passkey = take_passkey(&self.editor, window, cx);
                let repeated = take_passkey(&self.repeat_editor, window, cx);
                if new_passkey.is_empty() {
                    self.message = Some("the passkey must be at least one character".into());
                    self.editor.read(cx).focus_handle(cx).focus(window, cx);
                    cx.notify();
                    return;
                }
                if let Some(reason) = untypeable_reason(&new_passkey) {
                    self.message = Some(reason.into());
                    self.editor.read(cx).focus_handle(cx).focus(window, cx);
                    cx.notify();
                    return;
                }
                if *new_passkey != *repeated {
                    self.message = Some("the passkeys do not match".into());
                    self.editor.read(cx).focus_handle(cx).focus(window, cx);
                    cx.notify();
                    return;
                }
                self.message = Some("working".into());
                self.pending = Some(cx.spawn_in(window, async move |this, cx| {
                    let result = cx
                        .background_spawn(async move {
                            store.set_passkey(&new_passkey)?;
                            Ok::<_, anyhow::Error>(store.state())
                        })
                        .await;
                    this.update_in(cx, |this, _window, cx| {
                        this.pending = None;
                        match result {
                            Ok(state) => {
                                this.veil.update(cx, |veil, cx| {
                                    veil.state = state;
                                    cx.notify();
                                });
                                cx.emit(DismissEvent);
                                if this.lock_afterwards {
                                    App::defer(cx, |cx| {
                                        Veil::global(cx).update(cx, |veil, cx| veil.lock(cx));
                                    });
                                }
                            }
                            Err(error) => {
                                this.message = Some(error.to_string().into());
                            }
                        }
                        cx.notify();
                    })
                    .log_err();
                }));
            }
        }
        cx.notify();
    }

    fn advance_to_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step = PasskeyStep::EnterNew;
        self.message = None;
        self.editor.update(cx, |editor, cx| {
            editor.clear(window, cx);
            editor.set_placeholder_text(Self::placeholder(PasskeyStep::EnterNew), window, cx);
        });
    }
}

impl ModalView for PasskeyModal {
    fn on_before_dismiss(&mut self, _: &mut Window, _: &mut Context<Self>) -> DismissDecision {
        DismissDecision::Dismiss(true)
    }
}

impl Focusable for PasskeyModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for PasskeyModal {}

impl Render for PasskeyModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match self.step {
            PasskeyStep::ConfirmCurrent => "Enter the current veil passkey",
            PasskeyStep::EnterNew => "Choose a password for this lock",
        };
        let border = cx.theme().colors().border;
        let field = |editor: Entity<Editor>| {
            div()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(border)
                .child(editor)
        };
        v_flex()
            .w(rems(24.))
            .elevation_2(cx)
            .key_context("VeilPasskey")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm_action))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if is_plain_enter(event) {
                    this.confirm(window, cx);
                    cx.stop_propagation();
                } else if event.keystroke.key == "tab" && this.step == PasskeyStep::EnterNew {
                    this.focus_other_field(window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .child(Label::new(title))
                    .child(Label::new("Temporary privacy screen. Restart Hulder to reset it. Use your system lock for real security.").size(LabelSize::Small))
                    .child(field(self.editor.clone()))
                    .when(self.step == PasskeyStep::EnterNew, |this| {
                        this.child(field(self.repeat_editor.clone()))
                    })
                    .child(
                        Label::new(
                            "Veiled text is drawn as runes. Images, PDF pages and Chrome are not veiled.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .when_some(self.message.clone(), |this, message| {
                        this.child(
                            Label::new(message)
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{MouseButton, TestAppContext, VisualTestContext, WindowHandle, point};
    use project::{FakeFs, Project};
    use std::{cell::Cell, rc::Rc};
    use workspace::{AppState, MultiWorkspace};

    actions!(veil_test, [Probe]);

    fn typed(key: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: Some(key.into()),
            modifiers,
        }
    }

    fn chord(key: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            key: key.into(),
            key_char: None,
            modifiers,
        }
    }

    #[test]
    fn anything_that_types_text_gets_through() {
        assert!(!blocks_keystroke(&typed("a", Modifiers::default())));
        assert!(!blocks_keystroke(&typed("A", Modifiers::shift())));
        // option-l on a German layout, option-shift-7 on a Swiss one, and so on.
        assert!(!blocks_keystroke(&typed("@", Modifiers::alt())));
        assert!(!blocks_keystroke(&typed("{", Modifiers::alt())));
        assert!(!blocks_keystroke(&typed("|", Modifiers::alt())));
        assert!(!blocks_keystroke(&typed("\\", Modifiers::alt())));
        assert!(!blocks_keystroke(&typed("~", Modifiers::alt())));
        assert!(!blocks_keystroke(&typed(
            "€",
            Modifiers {
                alt: true,
                shift: true,
                ..Modifiers::default()
            }
        )));
        assert!(!blocks_keystroke(&typed("é", Modifiers::default())));
    }

    #[test]
    fn chords_without_text_are_blocked_unless_the_field_needs_them() {
        assert!(!blocks_keystroke(&chord("backspace", Modifiers::default())));
        assert!(!blocks_keystroke(&chord("left", Modifiers::shift())));
        assert!(blocks_keystroke(&chord("p", Modifiers::command_shift())));
        assert!(blocks_keystroke(&chord("l", Modifiers::alt())));
        assert!(blocks_keystroke(&chord("f1", Modifiers::function())));
        assert!(!blocks_keystroke(&chord("v", Modifiers::command())));
        assert!(!blocks_keystroke(&chord("a", Modifiers::command())));
        assert!(!blocks_keystroke(&chord("backspace", Modifiers::command())));
        assert!(blocks_keystroke(&chord("v", Modifiers::control())));
        // Text typed with command or control held would still be a chord elsewhere.
        assert!(blocks_keystroke(&typed("v", Modifiers::command())));
        assert!(blocks_keystroke(&typed("c", Modifiers::control())));
    }

    #[test]
    fn a_passkey_is_accepted_only_when_every_char_can_be_typed() {
        assert_eq!(untypeable_reason("hunter2"), None);
        assert_eq!(untypeable_reason("p@ss{w|rd}\\~€ äöü é"), None);
        assert!(untypeable_reason("tab\there").is_some());
        assert!(untypeable_reason("new\nline").is_some());
        assert!(untypeable_reason("\u{1b}").is_some());
    }

    #[test]
    fn only_unlock_field_actions_are_allowed() {
        assert!(allows_action("menu::Confirm"));
        assert!(allows_action("veil::Toggle"));
        assert!(allows_action("editor::Backspace"));
        assert!(!allows_action("workspace::CloseWindow"));
        assert!(!allows_action("command_palette::Toggle"));
        assert!(allows_action("zed::Quit"));
        assert!(!blocks_keystroke(&chord("q", Modifiers::command())));
        assert!(!allows_action("editor::OpenUrl"));
    }

    fn init_test(cx: &mut TestAppContext) -> (tempfile::TempDir, Arc<VeilStore>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(VeilStore::with_iterations(dir.path().join("veil.json"), 16));
        store.set_passkey("hunter2").expect("set passkey");
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            init_with_store(store.clone(), cx);
        });
        (dir, store)
    }

    fn has_active_modal(window: WindowHandle<MultiWorkspace>, cx: &mut TestAppContext) -> bool {
        window
            .update(cx, |multi, window, cx| {
                let workspace = multi.workspace().clone();
                workspace.update(cx, |workspace, cx| workspace.has_active_modal(window, cx))
            })
            .expect("window")
    }

    fn is_locked(cx: &mut TestAppContext) -> bool {
        cx.update(|cx| Veil::global(cx).read(cx).locked())
    }

    fn lock(cx: &mut TestAppContext) {
        cx.update(|cx| Veil::global(cx).update(cx, |veil, cx| veil.lock(cx)));
        cx.run_until_parked();
    }

    fn overlay_for(
        window: WindowHandle<MultiWorkspace>,
        cx: &mut TestAppContext,
    ) -> Entity<VeilOverlay> {
        cx.update(|cx| {
            Veil::global(cx)
                .read(cx)
                .overlays
                .iter()
                .find(|(handle, _)| *handle == *window)
                .map(|(_, overlay)| overlay.clone())
                .expect("an overlay for the window")
        })
    }

    fn open_popover(
        overlay: &Entity<VeilOverlay>,
        cx: &mut VisualTestContext,
    ) -> Entity<UnlockPopover> {
        overlay.update_in(cx, |overlay, window, cx| overlay.open_popover(window, cx));
        cx.run_until_parked();
        overlay.read_with(cx, |overlay, _| {
            overlay.popover().cloned().expect("popover")
        })
    }

    fn submit(popover: &Entity<UnlockPopover>, passkey: &str, cx: &mut VisualTestContext) {
        popover.update_in(cx, |popover, window, cx| {
            popover
                .editor
                .update(cx, |editor, cx| editor.set_text(passkey, window, cx));
            popover.confirm(window, cx);
        });
        cx.run_until_parked();
    }

    async fn workspace_window(
        fs: &Arc<FakeFs>,
        cx: &mut TestAppContext,
    ) -> (WindowHandle<MultiWorkspace>, Entity<Workspace>) {
        // Each test workspace registers client handlers, so every window needs its own project.
        let project = Project::test(fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi, _| multi.workspace().clone())
            .expect("window");
        (window, workspace)
    }

    #[gpui::test]
    async fn readable_unlock_cache_does_not_unveil_the_workspace(cx: &mut TestAppContext) {
        let (_dir, _store) = init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let (handle, _) = workspace_window(&fs, cx).await;
        lock(cx);
        handle
            .update(cx, |_, window, cx| {
                let original = window.text_system().clone();
                let plain = Arc::new(gpui::WindowTextSystem::new(cx.text_system().clone()));
                window.with_text_system(plain.clone(), |window| {
                    assert!(Arc::ptr_eq(window.text_system(), &plain));
                    assert!(window.text_veil().is_none());
                });
                assert!(Arc::ptr_eq(window.text_system(), &original));
                assert!(Veil::global(cx).read(cx).locked());
            })
            .expect("window");
    }

    #[gpui::test]
    async fn lock_covers_every_workspace_and_the_passkey_lifts_it(cx: &mut TestAppContext) {
        let (_dir, store) = init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let (first, first_workspace) = workspace_window(&fs, cx).await;
        let (_second, second_workspace) = workspace_window(&fs, cx).await;
        let first_cx = &mut VisualTestContext::from_window(*first, cx);

        lock(cx);
        assert!(is_locked(cx));
        assert!(store.is_locked(), "the lock was not persisted");
        for workspace in [&first_workspace, &second_workspace] {
            assert!(cx.update(|cx| workspace.read(cx).overlay().is_some()));
        }

        let overlay = overlay_for(first, cx);
        assert!(
            overlay.read_with(cx, |overlay, _| overlay.popover.is_none()),
            "the overlay must show no passkey field until the hood is used"
        );
        let popover = open_popover(&overlay, first_cx);
        assert!(cx.update(|cx| popover.read(cx).editor.read(cx).is_masked(cx)));
        submit(&popover, "wrong", first_cx);
        assert!(is_locked(cx));
        assert!(cx.update(|cx| popover.read(cx).editor.read(cx).text(cx).is_empty()));
        assert_eq!(
            cx.update(|cx| Veil::global(cx).read(cx).failed_attempts()),
            1
        );
        assert_eq!(
            popover.read_with(first_cx, |popover, _| popover.shake_count),
            1
        );

        submit(&popover, "hunter2", first_cx);
        assert!(!is_locked(cx));
        assert!(!store.is_locked(), "the unlock was not persisted");
        for workspace in [&first_workspace, &second_workspace] {
            assert!(cx.update(|cx| workspace.read(cx).overlay().is_none()));
        }
    }

    #[gpui::test]
    async fn toggle_while_locked_opens_the_unlock_popover(cx: &mut TestAppContext) {
        let (_dir, _store) = init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let (window, workspace) = workspace_window(&fs, cx).await;
        let visual_cx = &mut VisualTestContext::from_window(*window, cx);

        lock(cx);
        let overlay = overlay_for(window, cx);
        assert!(overlay.read_with(cx, |overlay, _| overlay.popover.is_none()));

        cx.update(|cx| Veil::toggle(workspace.clone(), *window, cx));
        cx.run_until_parked();
        assert!(is_locked(cx));
        let popover = overlay.read_with(cx, |overlay, _| overlay.popover().cloned());
        let popover = popover.expect("toggle while locked opens the popover");
        assert!(cx.update(|cx| workspace.read(cx).overlay().is_some()));
        assert!(!has_active_modal(window, cx));
        let field_focused = visual_cx.update(|window, cx| {
            popover
                .read(cx)
                .editor
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        });
        assert!(field_focused, "the unlock field did not take focus");

        // Escape closes the popover and keeps the veil.
        popover.update_in(visual_cx, |popover, window, cx| {
            popover.cancel(&menu::Cancel, window, cx)
        });
        cx.run_until_parked();
        assert!(overlay.read_with(cx, |overlay, _| overlay.popover.is_none()));
        assert!(is_locked(cx));

        cx.update(|cx| Veil::open_passkey_modal(workspace.clone(), *window, false, cx));
        cx.run_until_parked();
        assert!(!has_active_modal(window, cx));
        assert!(is_locked(cx));
    }

    #[gpui::test]
    async fn window_opened_while_locked_is_veiled_and_cannot_close(cx: &mut TestAppContext) {
        let (_dir, _store) = init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let (_first, _) = workspace_window(&fs, cx).await;
        lock(cx);

        let (window, workspace) = workspace_window(&fs, cx).await;
        let visual_cx = &mut VisualTestContext::from_window(*window, cx);
        visual_cx.run_until_parked();

        assert!(cx.update(|cx| workspace.read(cx).overlay().is_some()));
        let veil = window
            .update(cx, |_, window, _| window.text_veil())
            .expect("window")
            .expect("the new window draws through the veil");
        assert_eq!(veil("abc"), "ᚨᛒᚲ");
        visual_cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            visual_cx.update(|window, _| window.window_title()),
            NEUTRAL_TITLE
        );

        visual_cx.dispatch_action(workspace::CloseWindow);
        visual_cx.run_until_parked();
        assert_eq!(cx.windows().len(), 2, "the new window closed while locked");
        assert!(is_locked(cx));
    }

    #[gpui::test]
    async fn lock_survives_a_restart(cx: &mut TestAppContext) {
        let (_dir, store) = init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let (_first, _) = workspace_window(&fs, cx).await;
        lock(cx);
        assert!(store.is_locked());

        // Relaunch: a fresh Veil from the same store, before any window exists.
        cx.update(|cx| init_with_store(store.clone(), cx));
        assert!(is_locked(cx));

        let (window, workspace) = workspace_window(&fs, cx).await;
        cx.run_until_parked();
        assert!(cx.update(|cx| workspace.read(cx).overlay().is_some()));
        assert!(is_locked(cx));

        let overlay = overlay_for(window, cx);
        let visual_cx = &mut VisualTestContext::from_window(*window, cx);
        let popover = open_popover(&overlay, visual_cx);
        submit(&popover, "hunter2", visual_cx);
        assert!(!is_locked(cx));
        assert!(!store.is_locked());
    }

    #[gpui::test]
    async fn a_damaged_store_locks_and_says_so(cx: &mut TestAppContext) {
        let (_dir, store) = init_test(cx);
        std::fs::write(store.path(), "{ not json").expect("damage");
        // Relaunch against the damaged file.
        cx.update(|cx| init_with_store(store.clone(), cx));
        assert!(is_locked(cx));
        assert!(cx.update(|cx| Veil::global(cx).read(cx).state().is_damaged()));

        let fs = FakeFs::new(cx.executor());
        let (window, _) = workspace_window(&fs, cx).await;
        cx.run_until_parked();
        let overlay = overlay_for(window, cx);
        let visual_cx = &mut VisualTestContext::from_window(*window, cx);
        let popover = open_popover(&overlay, visual_cx);
        assert_eq!(
            popover.read_with(visual_cx, |popover, cx| popover.current_message(cx)),
            Some(DAMAGED_MESSAGE.into())
        );
        submit(&popover, "hunter2", visual_cx);
        assert!(is_locked(cx), "a damaged file must never unlock");
    }

    #[gpui::test]
    async fn passkey_modal_editor_is_masked_and_cleared_on_cancel(cx: &mut TestAppContext) {
        let (_dir, _store) = init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let (window, workspace) = workspace_window(&fs, cx).await;
        let visual_cx = &mut VisualTestContext::from_window(*window, cx);

        cx.update(|cx| Veil::open_passkey_modal(workspace.clone(), *window, false, cx));
        cx.run_until_parked();
        let modal = cx
            .update(|cx| workspace.read(cx).active_modal::<PasskeyModal>(cx))
            .expect("the passkey modal is open");
        let editor = modal.read_with(visual_cx, |modal, _| modal.editor.clone());
        assert!(cx.update(|cx| editor.read(cx).is_masked(cx)));

        editor.update_in(visual_cx, |editor, window, cx| {
            editor.set_text("secret", window, cx)
        });
        modal.update_in(visual_cx, |modal, window, cx| {
            modal.cancel(&menu::Cancel, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.update(|cx| editor.read(cx).text(cx).is_empty()));
        assert!(!has_active_modal(window, cx));
    }

    fn set_new_passkey(
        modal: &Entity<PasskeyModal>,
        new_passkey: &str,
        repeated: &str,
        cx: &mut VisualTestContext,
    ) {
        modal.update_in(cx, |modal, window, cx| {
            modal
                .editor
                .update(cx, |editor, cx| editor.set_text(new_passkey, window, cx));
            modal
                .repeat_editor
                .update(cx, |editor, cx| editor.set_text(repeated, window, cx));
            modal
                .repeat_editor
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
            modal.confirm(window, cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn session_toggle_asks_for_a_new_password_after_each_unlock(cx: &mut TestAppContext) {
        let store = Arc::new(VeilStore::session());
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            init_with_store(store.clone(), cx);
        });
        let fs = FakeFs::new(cx.executor());
        let (window, workspace) = workspace_window(&fs, cx).await;
        let visual_cx = &mut VisualTestContext::from_window(*window, cx);
        for password in ["first-session-password", "second-session-password"] {
            cx.update(|cx| Veil::toggle(workspace.clone(), *window, cx));
            cx.run_until_parked();
            assert!(!is_locked(cx));
            let modal = cx
                .update(|cx| workspace.read(cx).active_modal::<PasskeyModal>(cx))
                .expect("new password prompt");
            assert_eq!(
                modal.read_with(visual_cx, |modal, _| modal.step),
                PasskeyStep::EnterNew
            );
            set_new_passkey(&modal, password, password, visual_cx);
            assert!(is_locked(cx));
            let overlay = overlay_for(window, cx);
            let popover = open_popover(&overlay, visual_cx);
            submit(&popover, password, visual_cx);
            assert!(!is_locked(cx));
            assert_eq!(store.state(), StoreState::Missing);
        }
    }

    #[gpui::test]
    async fn a_new_passkey_must_be_repeated_and_typeable(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(VeilStore::with_iterations(dir.path().join("veil.json"), 16));
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            init_with_store(store.clone(), cx);
        });
        let fs = FakeFs::new(cx.executor());
        let (window, workspace) = workspace_window(&fs, cx).await;
        let visual_cx = &mut VisualTestContext::from_window(*window, cx);

        cx.update(|cx| Veil::open_passkey_modal(workspace.clone(), *window, true, cx));
        cx.run_until_parked();
        let modal = cx
            .update(|cx| workspace.read(cx).active_modal::<PasskeyModal>(cx))
            .expect("the passkey modal is open");
        assert_eq!(
            modal.read_with(visual_cx, |modal, _| modal.step),
            PasskeyStep::EnterNew
        );

        set_new_passkey(&modal, "hunter2", "hunter3", visual_cx);
        assert_eq!(
            modal.read_with(visual_cx, |modal, _| modal.message.clone()),
            Some("the passkeys do not match".into())
        );
        assert!(!store.has_passkey(), "a mismatched passkey was saved");
        assert!(has_active_modal(window, cx));

        set_new_passkey(&modal, "tab\there", "tab\there", visual_cx);
        assert!(
            modal
                .read_with(visual_cx, |modal, _| modal.message.clone())
                .is_some_and(|message| message.contains("control characters")),
        );
        assert!(!store.has_passkey(), "an untypeable passkey was saved");

        set_new_passkey(&modal, "p@ss€", "p@ss€", visual_cx);
        assert!(store.has_passkey());
        assert!(!has_active_modal(window, cx));
        assert!(is_locked(cx), "the modal was opened to lock afterwards");
        assert!(cx.update(|cx| Veil::global(cx).read(cx).state().has_passkey()));
    }

    #[gpui::test]
    async fn actions_are_swallowed_while_locked(cx: &mut TestAppContext) {
        let (_dir, _store) = init_test(cx);
        let probe_fired = Rc::new(Cell::new(0));
        cx.update({
            let probe_fired = probe_fired.clone();
            move |cx| {
                cx.on_action(move |_: &Probe, _| probe_fired.set(probe_fired.get() + 1));
            }
        });
        let fs = FakeFs::new(cx.executor());
        let (window, _) = workspace_window(&fs, cx).await;
        let visual_cx = &mut VisualTestContext::from_window(*window, cx);

        visual_cx.dispatch_action(Probe);
        visual_cx.run_until_parked();
        assert_eq!(probe_fired.get(), 1);

        lock(cx);

        visual_cx.dispatch_action(Probe);
        visual_cx.dispatch_action(workspace::CloseWindow);
        visual_cx.dispatch_action(zed_actions::command_palette::Toggle);
        visual_cx.run_until_parked();
        assert_eq!(probe_fired.get(), 1, "a global action ran while locked");
        assert_eq!(cx.windows().len(), 1, "the window closed while locked");
        assert!(is_locked(cx));

        // A menu action with no active window goes through the same interceptor.
        cx.update(|cx| cx.dispatch_action(&Probe));
        assert_eq!(probe_fired.get(), 1, "a global dispatch ran while locked");

        let overlay = overlay_for(window, cx);
        let popover = open_popover(&overlay, visual_cx);
        submit(&popover, "hunter2", visual_cx);
        assert!(!is_locked(cx));

        visual_cx.dispatch_action(Probe);
        visual_cx.run_until_parked();
        assert_eq!(probe_fired.get(), 2);
    }

    struct KeyCounter {
        focus_handle: FocusHandle,
        keys: Rc<Cell<usize>>,
        clicks: Rc<Cell<usize>>,
    }

    impl Render for KeyCounter {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("key-counter")
                .track_focus(&self.focus_handle)
                .size_full()
                .on_key_down(
                    cx.listener(|this, _: &KeyDownEvent, _, _| this.keys.set(this.keys.get() + 1)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| this.clicks.set(this.clicks.get() + 1)),
                )
        }
    }

    #[gpui::test]
    async fn windows_without_a_workspace_take_no_input_while_locked(cx: &mut TestAppContext) {
        let (_dir, _store) = init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let (_workspace_window, _) = workspace_window(&fs, cx).await;
        let keys = Rc::new(Cell::new(0));
        let clicks = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let keys = keys.clone();
            let clicks = clicks.clone();
            move |_, cx| KeyCounter {
                focus_handle: cx.focus_handle(),
                keys,
                clicks,
            }
        });
        let visual_cx = &mut VisualTestContext::from_window(*window, cx);
        window
            .update(visual_cx, |view, window, cx| {
                view.focus_handle.focus(window, cx);
                window.set_window_title("settings");
            })
            .expect("window");
        let press_and_click = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_keystrokes("a");
            cx.simulate_mouse_down(
                point(px(20.), px(20.)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.run_until_parked();
        };

        press_and_click(visual_cx);
        assert_eq!((keys.get(), clicks.get()), (1, 1));

        lock(cx);
        press_and_click(visual_cx);
        assert_eq!(
            (keys.get(), clicks.get()),
            (1, 1),
            "a non-workspace window took input while locked"
        );
        assert_eq!(
            visual_cx.update(|window, _| window.window_title()),
            NEUTRAL_TITLE
        );

        cx.update(|cx| Veil::global(cx).update(cx, |veil, cx| veil.unlock(cx)));
        cx.run_until_parked();
        press_and_click(visual_cx);
        assert_eq!((keys.get(), clicks.get()), (2, 2));
        assert_eq!(
            visual_cx.update(|window, _| window.window_title()),
            "settings"
        );
    }

    #[gpui::test]
    async fn the_status_item_renders_without_a_veil_global(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        assert!(cx.update(|cx| Veil::try_global(cx).is_none()));
        let rendered = window
            .update(cx, |_, window, cx| {
                let button = cx.new(VeilButton::new);
                button.update(cx, |button, cx| {
                    button.render(window, cx).into_any_element();
                });
                true
            })
            .expect("window");
        assert!(rendered);
    }
}

struct ReadableControls {
    text_system: Arc<gpui::WindowTextSystem>,
    child: gpui::AnyElement,
}

impl gpui::IntoElement for ReadableControls {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for ReadableControls {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<gpui::ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        (
            window.with_text_system(self.text_system.clone(), |window| {
                self.child.request_layout(window, cx)
            }),
            (),
        )
    }
    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<gpui::Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_text_system(self.text_system.clone(), |window| {
            self.child.prepaint(window, cx)
        });
    }
    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<gpui::Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_text_system(self.text_system.clone(), |window| {
            self.child.paint(window, cx)
        });
        self.text_system.finish_frame();
    }
}
