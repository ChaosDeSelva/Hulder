pub mod chrome_runner;
pub mod hoard_link;
pub mod open_url_modal;
pub mod pick;

use anyhow::{Context as _, Result};
use chrome_runner::ChromeRunner;
use gpui::{App, AppContext as _, Context, Window, actions};
use open_url_modal::OpenUrlModal;
use clippings::{ClippingDraft, Kind};
use pick::{Pick, pick_markdown};
use settings::{RegisterSetting, Settings};
use std::path::{Path, PathBuf};
use util::ResultExt as _;
use workspace::{OpenOptions, Toast, Workspace, notifications::NotificationId};

actions!(
    browser,
    [
        /// Opens a URL in Chrome under Hulder's control.
        Open,
        /// Picks an element in the open Chrome page.
        PickElement,
        /// Stops waiting for a pick and leaves inspect mode.
        CancelPick,
        /// Opens the last picked element in a Markdown buffer.
        OpenPickInBuffer,
        /// Sends the last picked element to an agent.
        SendPickToAgent,
    ]
);

#[derive(Clone, Debug, RegisterSetting)]
pub struct BrowserSettings {
    pub binary: Option<PathBuf>,
}

impl Settings for BrowserSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let browser = content.browser.clone().unwrap();
        Self {
            binary: browser.binary.map(PathBuf::from),
        }
    }
}

pub fn init(cx: &mut App) {
    chrome_runner::init(cx);
    hoard_link::register(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &Open, window, cx| {
            let workspace_handle = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, |window, cx| {
                OpenUrlModal::new(workspace_handle, window, cx)
            });
        });
        workspace.register_action(|workspace, _: &PickElement, window, cx| {
            pick_element(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &CancelPick, _window, cx| {
            let cancelled =
                ChromeRunner::global(cx).update(cx, |runner, cx| runner.cancel_pick(cx));
            let message = if cancelled {
                "Pick cancelled"
            } else {
                "No pick in progress"
            };
            toast(workspace, message, cx);
        });
        workspace.register_action(|workspace, _: &OpenPickInBuffer, window, cx| {
            open_last_pick(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &SendPickToAgent, _window, cx| {
            toast(workspace, "Send pick to agent is not yet wired", cx);
        });
    })
    .detach();
}

struct BrowserToast;

pub(crate) fn toast(
    workspace: &mut Workspace,
    message: impl Into<String>,
    cx: &mut Context<Workspace>,
) {
    let toast = Toast::new(NotificationId::unique::<BrowserToast>(), message.into()).autohide();
    workspace.show_toast(toast, cx);
}

fn pick_element(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let runner = ChromeRunner::global(cx);
    if !runner.read(cx).is_open() {
        toast(workspace, "No browser open; run browser: open first", cx);
        return;
    }
    let pick = runner.update(cx, |runner, cx| runner.pick_element(cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let outcome = pick.await;
        workspace
            .update_in(cx, |workspace, window, cx| match outcome {
                Ok((pick, screenshot_path)) => {
                    open_pick(pick, screenshot_path, window, cx);
                }
                Err(error) => toast(workspace, format!("Pick failed: {error:#}"), cx),
            })
            .log_err();
    })
    .detach();
}

fn open_last_pick(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let runner = ChromeRunner::global(cx);
    let Some((pick, screenshot_path)) = runner.read(cx).last_pick().cloned() else {
        toast(workspace, "No element picked yet", cx);
        return;
    };
    // The pick has not changed since it was saved, so show that file again
    // rather than writing a second copy; a deleted file falls through to a
    // fresh save.
    if let Some(saved) = runner.read(cx).last_pick_clipping().map(Path::to_path_buf) {
        let fs = workspace.project().read(cx).fs().clone();
        cx.spawn_in(window, async move |workspace, cx| {
            if fs.is_file(&saved).await {
                let opened = workspace.update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(saved.clone(), OpenOptions::default(), window, cx)
                });
                match opened {
                    Ok(open) => {
                        if let Err(error) = open.await {
                            workspace
                                .update(cx, |workspace, cx| {
                                    toast(
                                        workspace,
                                        format!("Could not open {}: {error:#}", saved.display()),
                                        cx,
                                    )
                                })
                                .log_err();
                        }
                        return;
                    }
                    Err(error) => log::warn!("browser: reopening pick clipping: {error:#}"),
                }
            }
            workspace
                .update_in(cx, |_, window, cx| open_pick(pick, screenshot_path, window, cx))
                .log_err();
        })
        .detach();
        return;
    }
    open_pick(pick, screenshot_path, window, cx);
}

/// Saves the pick as a clipping (spec section 13) and opens that file; when
/// clippings are off or there is no local worktree, or the save fails, the
/// pick opens in the scratch buffer so nothing captured is lost.
fn open_pick(
    pick: Pick,
    screenshot_path: Option<PathBuf>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let draft = ClippingDraft {
        kind: Kind::Pick,
        source: pick.url.clone(),
        selector: Some(pick.selector.clone()),
        title: Some(pick.selector.clone()),
        screenshot_png: Some(pick.screenshot_png.clone()),
        extra: Vec::new(),
        body: Box::new({
            let pick = pick.clone();
            move |asset| pick_markdown(&pick, asset)
        }),
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let outcome = clippings::save_and_open(workspace.clone(), draft, cx).await;
        let saved_path = match &outcome {
            Ok(path) => path.clone(),
            Err(error) => error.saved_path().map(Path::to_path_buf),
        };
        if let Some(saved_path) = saved_path {
            cx.update(|_, cx| {
                ChromeRunner::global(cx)
                    .update(cx, |runner, _| runner.remember_pick_clipping(saved_path));
            })
            .log_err();
        }
        match outcome {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| toast(workspace, error.user_message(), cx))
                    .log_err();
            }
        }
        let markdown = pick_markdown(&pick, screenshot_path.as_deref());
        workspace
            .update_in(cx, |workspace, window, cx| {
                open_pick_buffer(workspace, markdown, pick.selector, window, cx);
            })
            .log_err();
    })
    .detach();
}

/// Mirrors how `zed` opens builtin JSON schemas: a project buffer that is
/// not searchable, shown in a read-only editor.
fn open_pick_buffer(
    workspace: &mut Workspace,
    markdown: String,
    title: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let languages = workspace.app_state().languages.clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let result: Result<()> = async {
            let language = languages.language_for_name("Markdown").await.ok();
            let buffer = workspace
                .update(cx, |workspace, cx| {
                    workspace
                        .project()
                        .update(cx, |project, cx| project.create_buffer(language, false, cx))
                })?
                .await?;
            workspace.update_in(cx, |workspace, window, cx| {
                buffer.update(cx, |buffer, cx| {
                    buffer.edit([(0..0, markdown)], None, cx);
                });
                let editor = cx.new(|cx| {
                    let mut editor = editor::Editor::for_buffer(buffer, None, window, cx);
                    editor.set_read_only(true);
                    editor
                        .buffer()
                        .update(cx, |buffer, cx| buffer.set_title(title, cx));
                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })
        }
        .await;
        result.context("opening the pick buffer").log_err();
    })
    .detach();
}
