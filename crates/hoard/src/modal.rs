//! `hoard::Toggle`: list profiles, add one by hand, delete one, or borrow a
//! cookie jar from the Chrome Hulder launched. Secret fields are masked and
//! the modal never shows a stored secret back.

use crate::borrow;
use crate::model::{self, Cookie, Profile, ProfileKind, Secret};
use crate::{ChromeBridge, Hoard, chrome_bridge};
use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, Styled, Task, Window, rems,
};
use std::sync::Arc;
use theme::ActiveTheme as _;
use ui::{
    Button, Checkbox, Clickable, Color, ContextMenu, DropdownMenu, Label,
    LabelCommon, LabelSize, ToggleState, div, h_flex, prelude::*, v_flex,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

enum Mode {
    List,
    Add(AddForm),
    Borrow(BorrowForm),
}

struct AddForm {
    kind: ProfileKind,
    name: Entity<Editor>,
    hosts: Entity<Editor>,
    first: Entity<Editor>,
    second: Entity<Editor>,
}

struct BorrowForm {
    url: Entity<Editor>,
    name: Entity<Editor>,
    jar: Vec<Cookie>,
    hosts: Vec<(String, bool)>,
}

pub struct HoardModal {
    hoard: Entity<Hoard>,
    focus_handle: FocusHandle,
    mode: Mode,
    status: Option<String>,
    busy: Option<Task<()>>,
    _subscription: gpui::Subscription,
}

fn editor(placeholder: &str, masked: bool, window: &mut Window, cx: &mut App) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder, window, cx);
        if masked {
            editor.set_masked(true, cx);
        }
        editor
    })
}

/// The two secret fields of each kind: (first placeholder, second placeholder, mask first).
pub fn secret_fields(kind: ProfileKind) -> (&'static str, Option<&'static str>, bool) {
    match kind {
        ProfileKind::Bearer => ("token", None, true),
        ProfileKind::Basic => ("user", Some("password"), false),
        ProfileKind::Header => ("header name, e.g. X-Api-Key", Some("value"), false),
        ProfileKind::Cookies => (
            "cookie jar JSON: [{\"name\",\"value\",\"domain\",\"path\",\"secure\",\"httpOnly\",\"expires\"}]",
            None,
            false,
        ),
    }
}

/// Builds the secret from the two fields; the kind decides which is which.
pub fn secret_from_fields(kind: ProfileKind, first: &str, second: &str) -> Result<Secret> {
    let first = first.trim();
    let second = second.trim();
    match kind {
        ProfileKind::Bearer => {
            anyhow::ensure!(!first.is_empty(), "enter the bearer token");
            Ok(Secret::Bearer {
                token: first.to_string(),
            })
        }
        ProfileKind::Basic => {
            anyhow::ensure!(!first.is_empty(), "enter the user name");
            Ok(Secret::Basic {
                user: first.to_string(),
                password: second.to_string(),
            })
        }
        ProfileKind::Header => {
            anyhow::ensure!(
                !first.is_empty() && !first.contains(':') && !first.contains(char::is_whitespace),
                "enter a header name without spaces or ':'"
            );
            anyhow::ensure!(!second.is_empty(), "enter the header value");
            Ok(Secret::Header {
                name: first.to_string(),
                value: second.to_string(),
            })
        }
        ProfileKind::Cookies => {
            let jar: Vec<Cookie> = serde_json::from_str(first).context("cookie jar JSON")?;
            anyhow::ensure!(!jar.is_empty(), "the cookie jar is empty");
            Ok(Secret::Cookies {
                jar: model::drop_expired(jar, model::now_unix()),
            })
        }
    }
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

impl HoardModal {
    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        workspace.toggle_modal(window, cx, |window, cx| Self::new(window, cx));
    }

    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let hoard = Hoard::global(cx);
        let subscription = cx.observe(&hoard, |_, _, cx| cx.notify());
        Self {
            hoard,
            focus_handle: cx.focus_handle(),
            mode: Mode::List,
            status: None,
            busy: None,
            _subscription: subscription,
        }
    }

    fn start_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (first, second, mask) = secret_fields(ProfileKind::Bearer);
        let form = AddForm {
            kind: ProfileKind::Bearer,
            name: editor("profile name", false, window, cx),
            hosts: editor("hosts: api.example.com, *.example.com", false, window, cx),
            first: editor(first, mask, window, cx),
            second: editor(second.unwrap_or(""), false, window, cx),
        };
        let focus = form.name.focus_handle(cx);
        self.mode = Mode::Add(form);
        self.status = None;
        window.focus(&focus, cx);
        cx.notify();
    }

    fn set_kind(&mut self, kind: ProfileKind, window: &mut Window, cx: &mut Context<Self>) {
        let Mode::Add(form) = &mut self.mode else {
            return;
        };
        form.kind = kind;
        let (first, second, mask) = secret_fields(kind);
        form.first = editor(first, mask, window, cx);
        form.second = editor(second.unwrap_or(""), false, window, cx);
        cx.notify();
    }

    fn start_borrow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = BorrowForm {
            url: editor("https://app.example.com/login", false, window, cx),
            name: editor("profile name", false, window, cx),
            jar: Vec::new(),
            hosts: Vec::new(),
        };
        let focus = form.url.focus_handle(cx);
        self.mode = Mode::Borrow(form);
        self.status = None;
        window.focus(&focus, cx);
        cx.notify();
    }

    fn back_to_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::List;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn save_new_profile(&mut self, cx: &mut Context<Self>) {
        let Mode::Add(form) = &self.mode else {
            return;
        };
        let result: Result<(Profile, Secret)> = (|| {
            let name = model::validate_name(&form.name.read(cx).text(cx))?;
            let hosts = model::parse_hosts(&form.hosts.read(cx).text(cx))?;
            let secret = secret_from_fields(
                form.kind,
                &form.first.read(cx).text(cx),
                &form.second.read(cx).text(cx),
            )?;
            Ok((
                Profile {
                    name,
                    kind: form.kind,
                    hosts,
                    created: today(),
                    header_name: secret.header_name().map(str::to_string),
                    keep_session_cookies: false,
                },
                secret,
            ))
        })();
        let (profile, secret) = match result {
            Ok(parts) => parts,
            Err(error) => {
                self.status = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        let add = self
            .hoard
            .update(cx, |hoard, cx| hoard.add_profile(profile, secret, cx));
        self.run(
            cx.background_spawn(async move {
                add.await?;
                Ok("Profile stored in the Keychain".to_string())
            }),
            true,
            cx,
        );
    }

    fn delete(&mut self, name: String, cx: &mut Context<Self>) {
        let delete = self
            .hoard
            .update(cx, |hoard, cx| hoard.delete_profile(&name, cx));
        self.run(
            cx.background_spawn(async move {
                delete.await?;
                Ok(format!("Deleted {name}"))
            }),
            false,
            cx,
        );
    }

    fn toggle_keep(&mut self, name: String, keep: bool, cx: &mut Context<Self>) {
        let update = self
            .hoard
            .update(cx, |hoard, cx| hoard.set_keep_session_cookies(&name, keep, cx));
        self.run(
            cx.background_spawn(async move {
                update.await?;
                Ok(String::new())
            }),
            false,
            cx,
        );
    }

    /// Runs a background step and shows its outcome; `back` returns to the
    /// list on success.
    fn run(&mut self, task: Task<Result<String>>, back: bool, cx: &mut Context<Self>) {
        self.status = Some("Working…".into());
        cx.notify();
        self.busy = Some(cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| {
                match outcome {
                    Ok(message) => {
                        this.status = (!message.is_empty()).then_some(message);
                        if back {
                            this.mode = Mode::List;
                        }
                    }
                    Err(error) => this.status = Some(format!("{error:#}")),
                }
                this.busy = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn bridge(&mut self, cx: &mut Context<Self>) -> Option<Arc<dyn ChromeBridge>> {
        let bridge = chrome_bridge(cx);
        if bridge.is_none() {
            self.status = Some("Chrome support is not available in this build".into());
            cx.notify();
        }
        bridge
    }

    fn open_chrome(&mut self, cx: &mut Context<Self>) {
        let Mode::Borrow(form) = &self.mode else {
            return;
        };
        let url = form.url.read(cx).text(cx);
        let Some(bridge) = self.bridge(cx) else {
            return;
        };
        let open = bridge.open(url, cx);
        self.run(
            cx.background_spawn(async move {
                open.await?;
                Ok("Chrome is open: log in, then fetch the cookies".to_string())
            }),
            false,
            cx,
        );
    }

    fn fetch_cookies(&mut self, cx: &mut Context<Self>) {
        let Some(bridge) = self.bridge(cx) else {
            return;
        };
        let Some(client) = bridge.client(cx) else {
            self.status = Some("No Hulder Chrome is open; open one first".into());
            cx.notify();
            return;
        };
        self.status = Some("Reading cookies…".into());
        cx.notify();
        self.busy = Some(cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move {
                    borrow::fetch_all_cookies(&client, model::now_unix()).await
                })
                .await;
            this.update(cx, |this, cx| {
                match fetched {
                    Ok(jar) => {
                        if let Mode::Borrow(form) = &mut this.mode {
                            form.hosts = borrow::hosts_in(&jar)
                                .into_iter()
                                .map(|host| (host, false))
                                .collect();
                            this.status = Some(format!(
                                "{} cookies across {} hosts; tick the hosts to keep",
                                jar.len(),
                                form.hosts.len()
                            ));
                            form.jar = jar;
                        }
                    }
                    Err(error) => this.status = Some(format!("{error:#}")),
                }
                this.busy = None;
                cx.notify();
            })
            .ok();
        }));
    }

    fn borrow(&mut self, cx: &mut Context<Self>) {
        let Mode::Borrow(form) = &self.mode else {
            return;
        };
        let ticked: Vec<String> = form
            .hosts
            .iter()
            .filter(|(_, ticked)| *ticked)
            .map(|(host, _)| host.clone())
            .collect();
        let jar = borrow::select_hosts(form.jar.clone(), &ticked);
        let name = model::validate_name(&form.name.read(cx).text(cx));
        let (name, profile) = match name {
            Ok(_) if jar.is_empty() => {
                self.status = Some("Tick at least one host with cookies".into());
                cx.notify();
                return;
            }
            Ok(name) => {
                let profile = Profile {
                    name: name.clone(),
                    kind: ProfileKind::Cookies,
                    hosts: borrow::profile_hosts(&jar),
                    created: today(),
                    header_name: None,
                    keep_session_cookies: true,
                };
                (name, profile)
            }
            Err(error) => {
                self.status = Some(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        let Some(bridge) = self.bridge(cx) else {
            return;
        };
        let client = bridge.client(cx);
        let add = self.hoard.update(cx, |hoard, cx| {
            hoard.add_profile(profile, Secret::Cookies { jar }, cx)
        });
        self.run(
            cx.background_spawn(async move {
                add.await?;
                // The throwaway profile must not keep a copy of the session.
                let client = client.ok_or_else(|| {
                    anyhow!("stored {name}, but the Chrome session closed before its cookies could be cleared")
                })?;
                borrow::clear_browser_cookies(&client).await?;
                Ok(format!("Stored {name} and cleared the Chrome profile's cookies"))
            }),
            true,
            cx,
        );
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.mode, Mode::List) {
            cx.emit(DismissEvent);
        } else {
            self.back_to_list(window, cx);
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::List => cx.emit(DismissEvent),
            Mode::Add(_) => self.save_new_profile(cx),
            Mode::Borrow(_) => {}
        }
    }

    fn render_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let hoard = self.hoard.read(cx);
        let profiles: Vec<Profile> = hoard.profiles().to_vec();
        let load_error = hoard.load_error.clone();
        v_flex()
            .gap_1()
            .children(load_error.map(|error| {
                Label::new(format!("profiles.json: {error}"))
                    .size(LabelSize::Small)
                    .color(Color::Error)
            }))
            .when(profiles.is_empty(), |this| {
                this.child(
                    Label::new("The Hoard is empty. Add a profile or borrow one from Chrome.")
                        .color(Color::Muted),
                )
            })
            .children(profiles.into_iter().enumerate().map(|(index, profile)| {
                let name = profile.name.clone();
                let delete_name = profile.name.clone();
                let keep_name = profile.name.clone();
                h_flex()
                    .gap_2()
                    .px_1()
                    .child(Label::new(name).size(LabelSize::Default))
                    .child(
                        Label::new(profile.kind.label())
                            .size(LabelSize::Small)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new(profile.hosts.join(", "))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when(profile.kind == ProfileKind::Cookies, |this| {
                        this.child(
                            Checkbox::new(
                                ("hoard-keep", index),
                                if profile.keep_session_cookies {
                                    ToggleState::Selected
                                } else {
                                    ToggleState::Unselected
                                },
                            )
                            .label("keep session cookies")
                            .on_click(cx.listener(move |this, state, _window, cx| {
                                this.toggle_keep(
                                    keep_name.clone(),
                                    *state == ToggleState::Selected,
                                    cx,
                                );
                            })),
                        )
                    })
                    .child(
                        Button::new(("hoard-delete", index), "Delete")
                            .color(Color::Error)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.delete(delete_name.clone(), cx);
                            })),
                    )
            }))
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .child(
                        Button::new("hoard-add", "Add profile")
                            .on_click(cx.listener(|this, _, window, cx| this.start_add(window, cx))),
                    )
                    .child(
                        Button::new("hoard-borrow", "Borrow from Chrome").on_click(
                            cx.listener(|this, _, window, cx| this.start_borrow(window, cx)),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_add(&self, form: &AddForm, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let this = cx.entity().downgrade();
        let kind_menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            for kind in ProfileKind::ALL {
                let kind_action = kind;
                menu = menu.entry(kind.label(), None, {
                    let this = this.clone();
                    move |window, cx| {
                        this.update(cx, |this, cx| this.set_kind(kind_action, window, cx))
                            .log_err();
                    }
                });
            }
            menu
        });
        let (_, second, _) = secret_fields(form.kind);
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(Label::new("Kind").color(Color::Muted))
                    .child(DropdownMenu::new("hoard-kind", form.kind.label(), kind_menu)),
            )
            .child(self.field(form.name.clone(), cx))
            .child(self.field(form.hosts.clone(), cx))
            .child(self.field(form.first.clone(), cx))
            .when(second.is_some(), |this| this.child(self.field(form.second.clone(), cx)))
            .child(
                Label::new(
                    "Profiles apply over https only, except to localhost. Secrets go to the Keychain.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("hoard-save", "Save")
                            .on_click(cx.listener(|this, _, _window, cx| this.save_new_profile(cx))),
                    )
                    .child(
                        Button::new("hoard-cancel", "Back").on_click(
                            cx.listener(|this, _, window, cx| this.back_to_list(window, cx)),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_borrow(&self, form: &BorrowForm, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .gap_1()
            .child(
                Label::new(
                    "1. Open the login page in Hulder's Chrome and sign in. 2. Fetch the cookies. 3. Tick hosts and borrow; the Chrome profile is then cleared.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(self.field(form.url.clone(), cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("hoard-open-chrome", "Open Chrome")
                            .on_click(cx.listener(|this, _, _window, cx| this.open_chrome(cx))),
                    )
                    .child(
                        Button::new("hoard-fetch", "Fetch cookies")
                            .on_click(cx.listener(|this, _, _window, cx| this.fetch_cookies(cx))),
                    ),
            )
            .children(form.hosts.iter().enumerate().map(|(index, (host, ticked))| {
                Checkbox::new(
                    ("hoard-host", index),
                    if *ticked {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label(host.clone())
                .on_click(cx.listener(move |this, state, _window, cx| {
                    if let Mode::Borrow(form) = &mut this.mode
                        && let Some((_, ticked)) = form.hosts.get_mut(index)
                    {
                        *ticked = *state == ToggleState::Selected;
                        cx.notify();
                    }
                }))
            }))
            .child(self.field(form.name.clone(), cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("hoard-borrow-now", "Borrow")
                            .on_click(cx.listener(|this, _, _window, cx| this.borrow(cx))),
                    )
                    .child(
                        Button::new("hoard-borrow-back", "Back").on_click(
                            cx.listener(|this, _, window, cx| this.back_to_list(window, cx)),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn field(&self, editor: Entity<Editor>, cx: &App) -> gpui::Div {
        div()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .px_2()
            .py_1()
            .child(editor)
    }
}

impl Render for HoardModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.mode {
            Mode::List => self.render_list(cx),
            Mode::Add(form) => {
                let form = AddForm {
                    kind: form.kind,
                    name: form.name.clone(),
                    hosts: form.hosts.clone(),
                    first: form.first.clone(),
                    second: form.second.clone(),
                };
                self.render_add(&form, window, cx)
            }
            Mode::Borrow(form) => {
                let form = BorrowForm {
                    url: form.url.clone(),
                    name: form.name.clone(),
                    jar: Vec::new(),
                    hosts: form.hosts.clone(),
                };
                self.render_borrow(&form, cx)
            }
        };
        v_flex()
            .key_context("HoardModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .w(rems(44.))
            .p_2()
            .gap_2()
            .elevation_2(cx)
            .child(Label::new("The Hoard").size(LabelSize::Large))
            .child(body)
            .children(self.status.clone().map(|status| {
                Label::new(status)
                    .size(LabelSize::Small)
                    .color(if self.busy.is_some() {
                        Color::Muted
                    } else {
                        Color::Default
                    })
            }))
    }
}

impl Focusable for HoardModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for HoardModal {}
impl ModalView for HoardModal {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_fields_follow_the_kind() {
        assert!(matches!(
            secret_from_fields(ProfileKind::Bearer, " t0k ", "").expect("bearer"),
            Secret::Bearer { token } if token == "t0k"
        ));
        assert!(secret_from_fields(ProfileKind::Bearer, "", "").is_err());
        assert!(matches!(
            secret_from_fields(ProfileKind::Basic, "u", "p").expect("basic"),
            Secret::Basic { user, password } if user == "u" && password == "p"
        ));
        assert!(secret_from_fields(ProfileKind::Header, "Bad Name", "v").is_err());
        assert!(secret_from_fields(ProfileKind::Header, "X-Key", "").is_err());
        assert!(matches!(
            secret_from_fields(ProfileKind::Header, "X-Key", "v").expect("header"),
            Secret::Header { name, value } if name == "X-Key" && value == "v"
        ));
        let jar = r#"[{"name":"s","value":"1","domain":"a.test"}]"#;
        assert!(matches!(
            secret_from_fields(ProfileKind::Cookies, jar, "").expect("cookies"),
            Secret::Cookies { jar } if jar.len() == 1 && jar[0].path == "/"
        ));
        assert!(secret_from_fields(ProfileKind::Cookies, "[]", "").is_err());
        assert!(secret_from_fields(ProfileKind::Cookies, "not json", "").is_err());
        assert_eq!(secret_fields(ProfileKind::Basic).1, Some("password"));
    }
}
