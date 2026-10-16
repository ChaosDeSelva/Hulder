//! The Raven as a center item: request list with a filter on the left,
//! the request editor and the response pane on the right. Modelled on the
//! Herd view. Only `raven::Send` (Cmd-Enter) talks to the network.

use crate::collection::{Collection, Environments, METHODS, Request, serialize_request};
use crate::send::{language_for_content_type, pretty_body};
use crate::{
    ImportCurl, NewRequest, SaveAsClipping, SaveRequest, SelectNext, SelectPrevious, Send,
    SendOutcome, log, store,
};
use anyhow::{Context as _, Result};
use editor::Editor;
use fs::Fs;
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    Task, WeakEntity, Window, px,
};
use language::{Buffer, LanguageRegistry};
use project::Project;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use ui::{
    ActiveTheme as _, Button, Clickable, Color, ContextMenu, DropdownMenu, Icon, IconName, Label,
    LabelCommon, LabelSize, ListItem, ListItemSpacing, Toggleable, h_flex, prelude::*, v_flex,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

/// Parses the headers editor: one `Name: value` per line; blank and
/// comment lines are skipped.
pub fn parse_headers(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_string(), value.trim().to_string()))
        })
        .collect()
}

pub fn headers_text(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rows of the left list: `(collection index, request index)` pairs that
/// match `filter` (case-insensitive, against name, method and url).
pub fn filtered_rows(collections: &[Collection], filter: &str) -> Vec<(usize, usize)> {
    let needle = filter.trim().to_ascii_lowercase();
    let mut rows = Vec::new();
    for (collection_index, collection) in collections.iter().enumerate() {
        for (request_index, request) in collection.requests.iter().enumerate() {
            if needle.is_empty()
                || request.name.to_ascii_lowercase().contains(&needle)
                || request.method.to_ascii_lowercase().contains(&needle)
                || request.url.to_ascii_lowercase().contains(&needle)
                || collection.name.to_ascii_lowercase().contains(&needle)
            {
                rows.push((collection_index, request_index));
            }
        }
    }
    rows
}

pub fn move_selection(
    rows: &[(usize, usize)],
    current: Option<(usize, usize)>,
    delta: isize,
) -> Option<(usize, usize)> {
    if rows.is_empty() {
        return None;
    }
    let current_index = current
        .and_then(|current| rows.iter().position(|row| *row == current))
        .map(|index| index as isize)
        .unwrap_or(-1);
    let next = (current_index + delta).clamp(0, rows.len() as isize - 1) as usize;
    rows.get(next).copied()
}

/// A unique request name inside a collection: `name`, then `name 2`, ...
pub fn unique_name(collection: &Collection, base: &str) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "request" } else { base };
    if !collection.requests.iter().any(|request| request.name == base) {
        return base.to_string();
    }
    (2..)
        .map(|counter| format!("{base} {counter}"))
        .find(|candidate| !collection.requests.iter().any(|request| &request.name == candidate))
        .unwrap_or_else(|| base.to_string())
}

pub fn status_line(outcome: &SendOutcome) -> String {
    let truncated = if outcome.response.truncated {
        " (truncated at 8 MiB)"
    } else {
        ""
    };
    let profile = outcome
        .hoard_profile
        .as_deref()
        .map(|profile| format!(" · Hoard: {profile}"))
        .unwrap_or_default();
    format!(
        "{} · {} ms · {} bytes{truncated}{profile}",
        outcome.response.status,
        outcome.response.duration.as_millis(),
        outcome.response.body.len()
    )
}

struct ResponsePane {
    status: String,
    headers: String,
    body: Entity<Editor>,
}

pub struct RavenView {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    languages: Arc<LanguageRegistry>,
    root: Option<PathBuf>,
    collections: Vec<Collection>,
    environments: Environments,
    active_environment: Option<String>,
    selected: Option<(usize, usize)>,
    filter: Entity<Editor>,
    method: String,
    url: Entity<Editor>,
    headers: Entity<Editor>,
    body: Entity<Editor>,
    response: Option<ResponsePane>,
    last_outcome: Option<SendOutcome>,
    status: Option<String>,
    sending: Option<Task<()>>,
    focus_handle: FocusHandle,
    _reload: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl RavenView {
    pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        match workspace.item_of_type::<RavenView>(cx) {
            Some(existing) => {
                workspace.activate_item(&existing, true, true, window, cx);
            }
            None => {
                let view = cx.new(|cx| RavenView::new(workspace, window, cx));
                workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
            }
        }
    }

    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let active_is_raven = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<RavenView>())
            .is_some();
        if active_is_raven {
            workspace.active_pane().update(cx, |pane, cx| {
                pane.activate_previous_item(
                    &workspace::pane::ActivatePreviousItem::default(),
                    window,
                    cx,
                );
            });
        } else {
            Self::open(workspace, window, cx);
        }
    }

    fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let subscription = cx.subscribe(&project, |this, _, event: &project::Event, cx| {
            let touched = match event {
                project::Event::WorktreeUpdatedEntries(_, changes) => changes.iter().any(|(path, _, _)| {
                    let path = path.as_unix_str();
                    path.starts_with(".hulder/ravens")
                }),
                project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => true,
                _ => false,
            };
            if touched {
                this.reload(cx);
            }
        });
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter requests…", window, cx);
            editor
        });
        let filter_subscription = cx.subscribe(&filter, |_, _, event: &editor::EditorEvent, cx| {
            if matches!(event, editor::EditorEvent::BufferEdited) {
                cx.notify();
            }
        });
        let url = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("{{base}}/path or https://host/path", window, cx);
            editor
        });
        let headers = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text("Header-Name: value", window, cx);
            editor
        });
        let body = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 14, window, cx);
            editor.set_placeholder_text("Request body", window, cx);
            editor
        });
        let mut this = Self {
            workspace: workspace.weak_handle(),
            fs: project.read(cx).fs().clone(),
            languages: workspace.app_state().languages.clone(),
            root: None,
            project,
            collections: Vec::new(),
            environments: Environments::default(),
            active_environment: None,
            selected: None,
            filter,
            method: "GET".into(),
            url,
            headers,
            body,
            response: None,
            last_outcome: None,
            status: None,
            sending: None,
            focus_handle: cx.focus_handle(),
            _reload: Task::ready(()),
            _subscriptions: vec![subscription, filter_subscription],
        };
        this.reload(cx);
        this
    }

    fn project_root(&self, cx: &App) -> Option<PathBuf> {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    }

    /// Reads the collections and environments off the main thread.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let root = self.project_root(cx);
        self.root = root.clone();
        let Some(root) = root else {
            self.collections.clear();
            self.status = Some("Open a project folder to use the Raven".into());
            cx.notify();
            return;
        };
        self._reload = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let collections = store::list_collections(&root)?;
                    let environments = store::load_environments(&root)?;
                    anyhow::Ok((collections, environments))
                })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok((collections, environments)) => {
                        this.collections = collections;
                        if this.active_environment.as_ref().is_some_and(|active| {
                            !environments.0.contains_key(active)
                        }) {
                            this.active_environment = None;
                        }
                        if this.active_environment.is_none() {
                            this.active_environment = environments.names().into_iter().next();
                        }
                        this.environments = environments;
                        if let Some(selected) = this.selected
                            && this.request_at(selected).is_none()
                        {
                            this.selected = None;
                        }
                    }
                    Err(error) => this.status = Some(format!("{error:#}")),
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn request_at(&self, (collection, request): (usize, usize)) -> Option<&Request> {
        self.collections.get(collection)?.requests.get(request)
    }

    fn rows(&self, cx: &App) -> Vec<(usize, usize)> {
        filtered_rows(&self.collections, &self.filter.read(cx).text(cx))
    }

    fn select(&mut self, row: (usize, usize), window: &mut Window, cx: &mut Context<Self>) {
        let Some(request) = self.request_at(row).cloned() else {
            return;
        };
        self.selected = Some(row);
        self.method = request.method.clone();
        self.url
            .update(cx, |editor, cx| editor.set_text(request.url.clone(), window, cx));
        self.headers.update(cx, |editor, cx| {
            editor.set_text(headers_text(&request.headers), window, cx)
        });
        self.body
            .update(cx, |editor, cx| editor.set_text(request.body.clone(), window, cx));
        self.status = None;
        cx.notify();
    }

    /// The request as currently edited, named after the selection.
    fn edited_request(&self, cx: &App) -> Option<Request> {
        let selected = self.selected?;
        let name = self.request_at(selected)?.name.clone();
        Some(Request {
            name,
            method: self.method.clone(),
            url: self.url.read(cx).text(cx).trim().to_string(),
            headers: parse_headers(&self.headers.read(cx).text(cx)),
            body: self.body.read(cx).text(cx),
        })
    }

    fn write_collection(&mut self, collection_index: usize, cx: &mut Context<Self>) {
        let (Some(root), Some(collection)) = (self.root.clone(), self.collections.get(collection_index).cloned()) else {
            return;
        };
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            let written = store::write_collection(fs.as_ref(), &root, &collection).await;
            this.update(cx, |this, cx| {
                this.status = Some(match written {
                    Ok(path) => format!("Saved {}", path.display()),
                    Err(error) => format!("{error:#}"),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save_request(&mut self, _: &SaveRequest, _window: &mut Window, cx: &mut Context<Self>) {
        let (Some((collection_index, request_index)), Some(request)) =
            (self.selected, self.edited_request(cx))
        else {
            self.status = Some("Select a request first".into());
            cx.notify();
            return;
        };
        if let Some(slot) = self
            .collections
            .get_mut(collection_index)
            .and_then(|collection| collection.requests.get_mut(request_index))
        {
            *slot = request;
        }
        self.write_collection(collection_index, cx);
    }

    fn add_request(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        if self.root.is_none() {
            self.status = Some("Open a project folder first".into());
            cx.notify();
            return;
        }
        let collection_index = match self.selected {
            Some((collection_index, _)) => collection_index,
            None if !self.collections.is_empty() => 0,
            None => {
                self.collections.push(Collection {
                    name: "requests".into(),
                    requests: Vec::new(),
                });
                self.collections.len() - 1
            }
        };
        let Some(collection) = self.collections.get_mut(collection_index) else {
            return;
        };
        let name = unique_name(collection, &request.name);
        collection.requests.push(Request { name, ..request });
        let row = (collection_index, collection.requests.len() - 1);
        self.select(row, window, cx);
        self.write_collection(collection_index, cx);
    }

    fn new_request(&mut self, _: &NewRequest, window: &mut Window, cx: &mut Context<Self>) {
        self.add_request(Request::new("new request"), window, cx);
    }

    fn import_curl(&mut self, _: &ImportCurl, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap_or_default();
        match crate::curl::parse_curl(&text) {
            Ok(request) => self.add_request(request, window, cx),
            Err(error) => {
                self.status = Some(format!("Import cURL: {error:#}"));
                cx.notify();
            }
        }
    }

    fn send(&mut self, _: &Send, window: &mut Window, cx: &mut Context<Self>) {
        if self.sending.is_some() {
            return;
        }
        let (Some(root), Some(request)) = (self.root.clone(), self.edited_request(cx)) else {
            self.status = Some("Select or create a request first".into());
            cx.notify();
            return;
        };
        let variables: BTreeMap<String, String> = self
            .active_environment
            .as_deref()
            .map(|environment| self.environments.variables(environment))
            .unwrap_or_default();
        self.status = Some(format!("Sending {} {}…", request.method, request.url));
        self.response = None;
        cx.notify();
        let send = crate::send_request(self.fs.clone(), root, request, variables, cx);
        self.sending = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = send.await;
            this.update_in(cx, |this, window, cx| {
                this.sending = None;
                match outcome {
                    Ok(outcome) => this.show_response(outcome, window, cx),
                    Err(error) => this.status = Some(format!("Send failed: {error:#}")),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn show_response(&mut self, outcome: SendOutcome, window: &mut Window, cx: &mut Context<Self>) {
        let content_type = outcome.response.content_type().map(str::to_string);
        let text = pretty_body(&outcome.response.body, content_type.as_deref());
        let buffer = cx.new(|cx| Buffer::local(text, cx));
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(buffer.clone(), None, window, cx);
            editor.set_read_only(true);
            editor
        });
        if let Some(language_name) = language_for_content_type(content_type.as_deref()) {
            let languages = self.languages.clone();
            cx.spawn(async move |_, cx| {
                if let Ok(language) = languages.language_for_name(language_name).await {
                    buffer.update(cx, |buffer, cx| buffer.set_language(Some(language), cx));
                }
            })
            .detach();
        }
        self.status = Some(status_line(&outcome));
        self.response = Some(ResponsePane {
            status: status_line(&outcome),
            headers: headers_text(&outcome.redacted.headers),
            body: editor,
        });
        self.last_outcome = Some(outcome);
    }

    fn save_as_clipping(&mut self, _: &SaveAsClipping, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(root), Some(outcome)) = (self.root.clone(), self.last_outcome.clone()) else {
            self.status = Some("Send a request first".into());
            cx.notify();
            return;
        };
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let written = cx
                .background_spawn({
                    let redacted = outcome.redacted.clone();
                    async move {
                        log::save_clipping(fs.as_ref(), &root, &redacted, chrono::Local::now())
                            .await
                    }
                })
                .await;
            let message = match &written {
                Ok(path) => format!("Clipping saved: {}", path.display()),
                Err(error) => format!("Clipping failed: {error:#}"),
            };
            this.update(cx, |this, cx| {
                this.status = Some(message.clone());
                cx.notify();
            })
            .ok();
            if let Ok(path) = written {
                open_clipping(workspace, path, cx).await.log_err();
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        if let Some(next) = move_selection(&rows, self.selected, 1) {
            self.select(next, window, cx);
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        if let Some(previous) = move_selection(&rows, self.selected, -1) {
            self.select(previous, window, cx);
        }
    }

    fn render_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let rows = self.rows(cx);
        let mut list = v_flex().gap_px();
        let mut last_collection = None;
        for (collection_index, request_index) in rows {
            let Some(collection) = self.collections.get(collection_index) else {
                continue;
            };
            let Some(request) = collection.requests.get(request_index) else {
                continue;
            };
            if last_collection != Some(collection_index) {
                last_collection = Some(collection_index);
                list = list.child(
                    h_flex().px_1().pt_1().gap_1().child(
                        Label::new(format!("{}.http", collection.name))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                );
            }
            let row = (collection_index, request_index);
            let selected = self.selected == Some(row);
            list = list.child(
                ListItem::new(("raven-request", collection_index * 10_000 + request_index))
                    .spacing(ListItemSpacing::Sparse)
                    .indent_level(1)
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select(row, window, cx);
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(request.method.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(Label::new(request.name.clone())),
                    ),
            );
        }
        if self.collections.is_empty() {
            list = list.child(
                Label::new("No requests yet. New request or Import cURL to start; files go to .hulder/ravens/.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        list.into_any_element()
    }

    fn render_method_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> DropdownMenu {
        let this = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            for method in METHODS {
                let this = this.clone();
                menu = menu.entry(*method, None, move |_window, cx| {
                    this.update(cx, |this, cx| {
                        this.method = method.to_string();
                        cx.notify();
                    })
                    .log_err();
                });
            }
            menu
        });
        DropdownMenu::new("raven-method", self.method.clone(), menu)
    }

    fn render_environment_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> DropdownMenu {
        let this = cx.entity().downgrade();
        let names = self.environments.names();
        let menu = ContextMenu::build(window, cx, |mut menu, _, _| {
            if names.is_empty() {
                menu = menu.entry("no environments in env.json", None, |_, _| {});
            }
            for name in names {
                let this = this.clone();
                let chosen = name.clone();
                menu = menu.entry(name, None, move |_window, cx| {
                    this.update(cx, |this, cx| {
                        this.active_environment = Some(chosen.clone());
                        cx.notify();
                    })
                    .log_err();
                });
            }
            menu
        });
        let label = self
            .active_environment
            .clone()
            .unwrap_or_else(|| "env".into());
        DropdownMenu::new("raven-environment", label, menu)
    }

    fn boxed(&self, child: impl IntoElement, cx: &App) -> gpui::Div {
        gpui::div()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .rounded_sm()
            .px_2()
            .py_1()
            .child(child)
    }

    fn render_request(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let sending = self.sending.is_some();
        v_flex()
            .gap_1()
            .p_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(self.render_method_menu(window, cx))
                    .child(gpui::div().flex_1().child(self.boxed(self.url.clone(), cx)))
                    .child(self.render_environment_menu(window, cx))
                    .child(
                        Button::new("raven-send", if sending { "Sending…" } else { "Send" })
                            .disabled(sending)
                            .on_click(cx.listener(|this, _, window, cx| this.send(&Send, window, cx))),
                    ),
            )
            .child(Label::new("Headers").size(LabelSize::Small).color(Color::Muted))
            .child(self.boxed(self.headers.clone(), cx))
            .child(Label::new("Body").size(LabelSize::Small).color(Color::Muted))
            .child(self.boxed(self.body.clone(), cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(Button::new("raven-save", "Save").on_click(
                        cx.listener(|this, _, window, cx| this.save_request(&SaveRequest, window, cx)),
                    ))
                    .child(Button::new("raven-new", "New request").on_click(
                        cx.listener(|this, _, window, cx| this.new_request(&NewRequest, window, cx)),
                    ))
                    .child(Button::new("raven-import", "Import cURL").on_click(
                        cx.listener(|this, _, window, cx| this.import_curl(&ImportCurl, window, cx)),
                    ))
                    .child(Button::new("raven-clip", "Save as Clipping").on_click(
                        cx.listener(|this, _, window, cx| {
                            this.save_as_clipping(&SaveAsClipping, window, cx)
                        }),
                    )),
            )
            .into_any_element()
    }

    fn render_response(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(response) = &self.response else {
            return v_flex()
                .flex_1()
                .p_2()
                .child(
                    Label::new(
                        self.status
                            .clone()
                            .unwrap_or_else(|| "Cmd-Enter sends. Nothing is sent otherwise.".into()),
                    )
                    .color(Color::Muted),
                )
                .into_any_element();
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new(response.status.clone()))
            .child(
                Label::new(response.headers.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .bg(cx.theme().colors().editor_background)
                    .child(response.body.clone()),
            )
            .into_any_element()
    }
}

async fn open_clipping(
    workspace: WeakEntity<Workspace>,
    path: PathBuf,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<()> {
    let open = workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_abs_path(
            path,
            workspace::OpenOptions {
                focus: Some(true),
                ..workspace::OpenOptions::default()
            },
            window,
            cx,
        )
    })?;
    open.await.map(|_| ()).context("opening the clipping")
}

impl Render for RavenView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status.clone();
        h_flex()
            .id("raven")
            .key_context("RavenView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::save_request))
            .on_action(cx.listener(Self::new_request))
            .on_action(cx.listener(Self::import_curl))
            .on_action(cx.listener(Self::save_as_clipping))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .size_full()
            .child(
                v_flex()
                    .w(px(280.))
                    .h_full()
                    .border_r_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().panel_background)
                    .child(gpui::div().p_1().child(self.boxed(self.filter.clone(), cx)))
                    .child(
                        gpui::div()
                            .id("raven-list")
                            .flex_1()
                            .overflow_y_scroll()
                            .px_1()
                            .child(self.render_list(cx)),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(self.render_request(window, cx))
                    .children(status.filter(|_| self.response.is_some()).map(|status| {
                        Label::new(status)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .ml_2()
                    }))
                    .child(self.render_response(cx)),
            )
    }
}

impl Focusable for RavenView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for RavenView {}

impl Item for RavenView {
    type Event = ();

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Raven".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Send))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}

/// Text form of a request for the clipboard or a buffer.
pub fn request_text(request: &Request) -> String {
    serialize_request(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collections() -> Vec<Collection> {
        vec![
            Collection {
                name: "auth".into(),
                requests: vec![Request {
                    name: "login".into(),
                    method: "POST".into(),
                    url: "{{base}}/login".into(),
                    headers: Vec::new(),
                    body: String::new(),
                }],
            },
            Collection {
                name: "users".into(),
                requests: vec![
                    Request::new("list users"),
                    Request {
                        name: "delete user".into(),
                        method: "DELETE".into(),
                        url: "{{base}}/users/1".into(),
                        headers: Vec::new(),
                        body: String::new(),
                    },
                ],
            },
        ]
    }

    #[test]
    fn headers_round_trip_through_the_editor_text() {
        let headers = vec![
            ("Accept".to_string(), "*/*".to_string()),
            ("X-Key".to_string(), "a:b".to_string()),
        ];
        let text = headers_text(&headers);
        assert_eq!(text, "Accept: */*\nX-Key: a:b");
        assert_eq!(parse_headers(&format!("{text}\n# comment\n\nbroken line\n")), headers);
    }

    #[test]
    fn filter_matches_name_method_url_and_collection() {
        let collections = collections();
        assert_eq!(filtered_rows(&collections, "").len(), 3);
        assert_eq!(filtered_rows(&collections, "users"), vec![(1, 0), (1, 1)]);
        assert_eq!(filtered_rows(&collections, "DELETE"), vec![(1, 1)]);
        assert_eq!(filtered_rows(&collections, "/login"), vec![(0, 0)]);
        assert!(filtered_rows(&collections, "nothing").is_empty());
        let rows = filtered_rows(&collections, "");
        assert_eq!(move_selection(&rows, None, 1), Some((0, 0)));
        assert_eq!(move_selection(&rows, Some((0, 0)), 1), Some((1, 0)));
        assert_eq!(move_selection(&rows, Some((1, 1)), 1), Some((1, 1)));
        assert_eq!(move_selection(&[], None, 1), None);
    }

    #[test]
    fn new_request_names_are_unique_in_a_collection() {
        let collection = &collections()[1];
        assert_eq!(unique_name(collection, "list users"), "list users 2");
        assert_eq!(unique_name(collection, "fresh"), "fresh");
        assert_eq!(unique_name(collection, "  "), "request");
    }
}
