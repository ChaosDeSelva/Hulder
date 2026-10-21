use crate::update_release::{self, Update};
use gpui::{App, AppContext as _, Global, PromptLevel, actions};
use settings::{RegisterSetting, Settings};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

// This client intentionally contacts only Hulder's release feed; the shared
// editor HTTP client remains blocked for inherited upstream services.
#[derive(Clone, Copy, RegisterSetting)]
struct AutomaticUpdates(bool);
impl Settings for AutomaticUpdates {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self(content.auto_update.unwrap_or(true))
    }
}
#[derive(Default)]
struct UpdateState {
    busy: bool,
    last_check: Option<Instant>,
}
impl Global for UpdateState {}
actions!(hulder, [CheckForUpdates, AutomaticUpdateChecks]);

pub fn init(fs: Arc<dyn fs::Fs>, cx: &mut App) {
    cx.set_global(UpdateState::default());
    cx.on_action(|_: &CheckForUpdates, cx| check(true, cx));
    cx.on_action(move |_: &AutomaticUpdateChecks, cx| {
        let enabled = AutomaticUpdates::get_global(cx).0;
        let Some(window) = cx.active_window() else { return; };
        let fs = fs.clone();
        match window.update(cx, |_, window, cx| {
            let answer = window.prompt(PromptLevel::Info, "Automatic update checks", Some(if enabled {
                "Enabled: Hulder checks GitHub after opening and daily while running. Preview releases are included. Downloads require your choice."
            } else { "Disabled: use Check for Updates whenever you want. Enabling checks contacts GitHub after opening and daily while running." }), &[if enabled { "Turn off" } else { "Turn on" }, "Cancel"], cx);
            cx.spawn(async move |cx| {
                if answer.await == Ok(0) {
                    cx.update(|cx| settings::update_settings_file(fs, cx, move |settings, _| settings.auto_update = Some(!enabled)));
                }
            }).detach();
        }) { Ok(()) => {}, Err(error) => log::warn!("Update preferences: {error:#}") }
    });
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_secs(10))
            .await;
        loop {
            cx.update(|cx| {
                if AutomaticUpdates::get_global(cx).0
                    && cx.active_window().is_some()
                    && cx
                        .global::<UpdateState>()
                        .last_check
                        .is_none_or(|time| time.elapsed() >= Duration::from_secs(86400))
                {
                    check(false, cx);
                }
            });
            cx.background_executor()
                .timer(Duration::from_secs(60))
                .await;
        }
    })
    .detach();
}
fn message(title: &str, detail: &str, cx: &mut App) {
    if let Some(window) = cx.active_window() {
        if let Err(error) = window.update(cx, |_, window, cx| {
            drop(window.prompt(PromptLevel::Info, title, Some(detail), &["OK"], cx));
        }) {
            log::warn!("Update dialog: {error:#}");
        }
    }
}
fn check(manual: bool, cx: &mut App) {
    if cx.global::<UpdateState>().busy {
        if manual {
            message(
                "Update in progress",
                "A check or download is already running.",
                cx,
            );
        }
        return;
    }
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        if manual {
            message(
                "Hulder updates",
                "Installer updates currently support Apple silicon Macs. Source releases are available on GitHub.",
                cx,
            );
        }
        return;
    }
    let state = cx.global_mut::<UpdateState>();
    state.busy = true;
    state.last_check = Some(Instant::now());
    cx.spawn(async move |cx| {
        let result = cx.background_spawn(async { update_release::fetch(update_release::VERSION) }).await;
        cx.update(|cx| {
            cx.global_mut::<UpdateState>().busy = false;
            match result {
                Ok(Some(update)) => offer(update, cx),
                Ok(None) if manual => message("Hulder is up to date", &format!("Installed release: {}. No newer compatible installer is published.", update_release::VERSION), cx),
                Ok(None) => {},
                Err(error) if manual => message("Could not check for updates", &format!("{error:#}\nTry again when online. GitHub may also be rate limiting requests."), cx),
                Err(error) => log::info!("Hulder update check: {error:#}"),
            }
        });
    }).detach();
}
fn offer(update: Update, cx: &mut App) {
    let Some(window) = cx.active_window() else {
        return;
    };
    cx.global_mut::<UpdateState>().busy = true;
    match window.update(cx, |_, window, cx| {
        let answer = window.prompt(PromptLevel::Info, "Hulder update available", Some(&format!("Version {} is available. Download the verified installer, then quit Hulder and drag the new app into Applications when ready. Your work stays open until you quit.", update.version)), &["Download update", "Later"], cx);
        cx.spawn(async move |cx| {
            if answer.await != Ok(0) {
                cx.update(|cx| cx.global_mut::<UpdateState>().busy = false);
                return;
            }
            cx.update(|cx| message("Downloading Hulder", "The installer is downloading in the background. A dialog will appear when verification finishes.", cx));
            let root = paths::data_dir().join("updates");
            let result = cx.background_spawn(async move { update_release::download(&update, &root) }).await;
            cx.update(|cx| {
                cx.global_mut::<UpdateState>().busy = false;
                match result {
                    Ok(path) => {
                        cx.reveal_path(&path);
                        message("Update ready to install", &format!("Checksum, publisher signature and Apple notarization verified.\n\nOpen {}. Quit Hulder, then drag the new app to Applications and replace the old copy. Reopen Hulder afterward.", path.display()), cx);
                    }
                    Err(error) => message("Update download failed", &format!("{error:#}\nThe installed app was not changed."), cx),
                }
            });
        }).detach();
    }) {
        Ok(()) => {},
        Err(error) => { cx.global_mut::<UpdateState>().busy = false; log::warn!("Update offer: {error:#}"); }
    }
}
