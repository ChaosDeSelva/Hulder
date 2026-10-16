use crate::factory_client::{self, field, items};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, PathPromptOptions, Render, SharedString, Styled, Task, WeakEntity,
    Window, actions, px,
};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use ui::{
    ActiveTheme as _, Button, ButtonCommon as _, Clickable, Color, Disableable, Icon, IconName,
    Label, LabelCommon, LabelSize, ListItem, ListItemSpacing, StatefulInteractiveElement as _,
    Toggleable as _, h_flex, v_flex,
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

actions!(
    factory,
    [
        /// Opens the local factory's native mission and task inspector.
        Open,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &Open, window, cx| {
            open_factory(workspace, window, cx);
        });
    })
    .detach();
}

fn open_factory(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(view) = workspace.item_of_type::<FactoryView>(cx) {
        workspace.activate_item(&view, true, true, window, cx);
    } else {
        let workspace_handle = workspace.weak_handle();
        let view = cx.new(|cx| FactoryView::new(workspace_handle, cx));
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }
}

pub struct FactoryButton {
    workspace: WeakEntity<Workspace>,
}

impl FactoryButton {
    pub fn new(workspace: &Workspace) -> Self {
        Self {
            workspace: workspace.weak_handle(),
        }
    }
}

impl Render for FactoryButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.clone();
        ui::IconButton::new("factory-status", IconName::FactoryInitial)
            .aria_label("Factory")
            .tooltip(ui::Tooltip::text("Factory — missions and tasks"))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| open_factory(workspace, window, cx))
                    .log_err();
            })
    }
}

impl workspace::StatusItemView for FactoryButton {
    fn set_active_pane_item(
        &mut self,
        _item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

pub struct FactoryView {
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    token_path: Option<PathBuf>,
    snapshot: Value,
    detail: Option<Value>,
    selected: Option<String>,
    error: Option<String>,
    busy: bool,
    show_source: bool,
    generation: u64,
    observed: Option<std::time::SystemTime>,
    _polling: Task<()>,
}

impl FactoryView {
    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let token_path = std::env::var_os("HULDER_FACTORY_TOKEN_FILE").map(PathBuf::from);
        let polling = cx.spawn(async move |view, cx| {
            loop {
                if view.update(cx, |view, cx| view.refresh(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        });
        Self {
            workspace,
            focus: cx.focus_handle(),
            token_path,
            snapshot: Value::Null,
            detail: None,
            selected: None,
            error: None,
            busy: false,
            show_source: false,
            generation: 0,
            observed: None,
            _polling: polling,
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(token_path) = self.token_path.clone() else {
            return;
        };
        self.busy = true;
        let generation = self.generation;
        let selected = self.selected.clone();
        let request = cx.background_spawn(async move {
            let snapshot = factory_client::request(&token_path, "/v1/snapshot")?;
            let detail = match selected {
                Some(id) => Some(factory_client::request(
                    &token_path,
                    &format!("/v1/tasks/{id}"),
                )?),
                None => None,
            };
            anyhow::Ok((snapshot, detail))
        });
        cx.spawn(async move |view, cx| {
            let result = request.await;
            view.update(cx, |view, cx| {
                view.busy = false;
                if view.generation != generation {
                    view.refresh(cx);
                    return;
                }
                match result {
                    Ok((snapshot, detail)) => {
                        view.snapshot = snapshot;
                        view.detail = detail;
                        view.error = None;
                        view.observed = Some(std::time::SystemTime::now());
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select Mimir's auth-token file".into()),
        });
        cx.spawn(async move |view, cx| match selected.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    view.update(cx, |view, cx| {
                        view.generation += 1;
                        view.disconnect(cx);
                        view.token_path = Some(path);
                        view.error = None;
                        view.refresh(cx);
                    })
                    .log_err();
                }
            }
            Ok(Ok(None)) => {}
            _ => {
                view.update(cx, |view, cx| {
                    view.error = Some("Unable to select a credential file".into());
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.token_path = None;
        self.snapshot = Value::Null;
        self.detail = None;
        self.selected = None;
        self.observed = None;
        self.error = None;
        cx.notify();
    }

    fn worktree(&self) -> Option<PathBuf> {
        let selected = self.selected.as_ref()?;
        items(&self.snapshot, "worktrees")
            .into_iter()
            .find(|worktree| {
                worktree.get("taskId").and_then(Value::as_str) == Some(selected.as_str())
            })
            .and_then(|worktree| {
                worktree
                    .get("path")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
            })
            .filter(|path| path.is_absolute() && path.is_dir())
    }

    fn open_worktree(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.worktree() else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let task = workspace::open_paths(
            &[path],
            workspace.read(cx).app_state().clone(),
            workspace::OpenOptions::default(),
            cx,
        );
        cx.spawn_in(window, async move |_, _| {
            task.await.log_err();
        })
        .detach();
    }
}

fn detail_section(title: &str, lines: Vec<String>, empty: &str) -> gpui::Div {
    v_flex()
        .gap_2()
        .child(Label::new(title.to_owned()).size(LabelSize::Large))
        .when(lines.is_empty(), |view| {
            view.child(Label::new(empty.to_owned()).color(Color::Muted))
        })
        .children(lines.into_iter().map(Label::new))
}

impl Render for FactoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let missions = items(&self.snapshot, "missions");
        let tasks = items(&self.snapshot, "tasks");
        let task_count = tasks.len();
        let status = if self.token_path.is_none() {
            "Disconnected · choose your local controller's credential file".to_owned()
        } else if self.error.is_some() {
            "Connection unavailable · last successful observation retained".to_owned()
        } else if self.observed.is_none() {
            "Connecting to local Mimir…".to_owned()
        } else {
            format!(
                "Local Mimir · {} missions · {} tasks · observed {}s ago",
                missions.len(),
                task_count,
                self.observed
                    .and_then(|time| time.elapsed().ok())
                    .map(|age| age.as_secs())
                    .unwrap_or(0)
            )
        };
        let task = self.detail.as_ref().and_then(|detail| detail.get("task"));
        let mut detail = v_flex()
            .id("factory-detail")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .p_4()
            .gap_6();
        if let Some(task) = task {
            let source = self.detail.as_ref().unwrap_or(&Value::Null);
            let brief = source
                .get("spec")
                .map(|spec| field(spec, "brief"))
                .unwrap_or_else(|| "No brief supplied.".into());
            detail = detail
                .child(v_flex().gap_2()
                    .child(Label::new(field(task,"title")).size(LabelSize::Large))
                    .child(Label::new(format!("Status: {} · attempt {} of {}", field(task,"status"),task["attempt"],task["maxAttempts"])).color(Color::Muted))
                    .child(Label::new(brief)))
                .child(Button::new("factory-worktree", "Open task worktree").disabled(self.worktree().is_none())
                    .on_click(cx.listener(|view,_,window,cx| view.open_worktree(window,cx))))
                .child(detail_section("Acceptance criteria", task.get("acceptance").and_then(Value::as_str)
                    .filter(|text| !text.is_empty()).map(|text| vec![text.to_owned()]).unwrap_or_default(), "No acceptance criteria supplied."))
                .child(detail_section("Execution sessions", items(source,"sessions").iter().map(|session|
                    format!("{} · {}",field(session,"id"),field(session,"state"))).collect(), "No execution sessions yet."))
                .child(detail_section("Human approvals", items(source,"approvals").iter().map(|approval|
                    format!("{} · {}",field(approval,"action"),field(approval,"status"))).collect(), "No approval requests recorded."))
                .child(detail_section("Verification and artifacts", items(source,"artifacts").iter().map(|artifact|
                    format!("{} · {}",field(artifact,"kind"),field(artifact,"id"))).collect(), "No verification artifacts recorded."))
                .child(Label::new("Approve, stop or retry in Heimdall. A process exit does not establish verified delivery.").color(Color::Muted))
                .child(Button::new("factory-source",if self.show_source {"Hide source record"} else {"Inspect source record"})
                    .on_click(cx.listener(|view,_,_,cx| {view.show_source=!view.show_source;cx.notify();})))
                .when(self.show_source, |view| view.child(
                    v_flex().id("factory-source-json").overflow_x_scroll().p_3().rounded_md()
                        .bg(cx.theme().colors().panel_background)
                        .child(Label::new(serde_json::to_string_pretty(source).unwrap_or_else(|error| error.to_string())).buffer_font(cx))));
        } else {
            detail = detail.child(v_flex().gap_3().p_4()
                .child(Icon::new(IconName::FactoryInitial).color(Color::Muted))
                .child(Label::new(if self.selected.is_some() {"Loading task details…"} else {"Your factory, in the editor"}).size(LabelSize::Large))
                .child(Label::new(if self.token_path.is_none() {"Choose the local Mimir credential file to connect."}
                    else if task_count == 0 {"No tasks yet. Open Heimdall to start a mission; its tasks will appear here automatically."}
                    else {"Select a task to read its brief, acceptance criteria, execution sessions and evidence."}).color(Color::Muted)));
        }
        v_flex()
            .size_full()
            .min_w_0()
            .track_focus(&self.focus)
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .p_4()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_3()
                            .child(Label::new("Factory").size(LabelSize::Large))
                            .child(
                                Button::new("factory-connect", "Choose credentials")
                                    .on_click(cx.listener(|view, _, _, cx| view.connect(cx))),
                            )
                            .child(
                                Button::new("factory-refresh", "Refresh")
                                    .disabled(self.token_path.is_none() || self.busy)
                                    .on_click(cx.listener(|view, _, _, cx| view.refresh(cx))),
                            )
                            .child(
                                Button::new("factory-disconnect", "Disconnect")
                                    .disabled(self.token_path.is_none())
                                    .on_click(cx.listener(|view, _, _, cx| view.disconnect(cx))),
                            )
                            .child(Button::new("factory-console", "Open Heimdall").on_click(
                                cx.listener(|view, _, _, cx| cx.open_url(&factory_client::console_url(view.selected.as_deref()))),
                            )),
                    )
                    .child(Label::new(status).color(Color::Muted))
                    .children(
                        self.error
                            .clone()
                            .map(|error| Label::new(error).color(Color::Error)),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(
                        v_flex()
                            .id("factory-tasks")
                            .w(px(320.))
                            .min_w(px(160.))
                            .h_full()
                            .overflow_y_scroll()
                            .p_3()
                            .gap_3()
                            .border_r_1()
                            .border_color(cx.theme().colors().border)
                            .bg(cx.theme().colors().panel_background)
                            .child(
                                Label::new(format!("Tasks ({task_count})")).size(LabelSize::Large),
                            )
                            .when(tasks.is_empty(), |view| {
                                view.child(
                                    Label::new("Tasks appear here when a mission is created.")
                                        .color(Color::Muted),
                                )
                            })
                            .children(tasks.into_iter().enumerate().map(|(index, task)| {
                                let id = field(&task, "id");
                                let selected = self.selected.as_ref() == Some(&id);
                                ListItem::new(("factory-task", index))
                                    .aria_role(gpui::Role::Button)
                                    .aria_label(format!(
                                        "{} — {}",
                                        field(&task, "title"),
                                        field(&task, "status")
                                    ))
                                    .spacing(ListItemSpacing::Sparse)
                                    .toggle_state(selected)
                                    .child(
                                        v_flex()
                                            .min_w_0()
                                            .gap_1()
                                            .child(Label::new(field(&task, "title")))
                                            .child(
                                                Label::new(field(&task, "status"))
                                                    .color(Color::Muted),
                                            ),
                                    )
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.selected = Some(id.clone());
                                        view.detail = None;
                                        view.show_source = false;
                                        view.generation += 1;
                                        view.refresh(cx);
                                        cx.notify();
                                    }))
                            })),
                    )
                    .child(detail),
            )
    }
}
impl Focusable for FactoryView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl EventEmitter<()> for FactoryView {}
impl Item for FactoryView {
    type Event = ();
    fn to_item_events(_event: &(), _emit: &mut dyn FnMut(ItemEvent)) {}
    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Factory".into()
    }
    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::FactoryInitial))
    }
    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}
