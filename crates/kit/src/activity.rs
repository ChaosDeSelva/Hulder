use crate::activity_data::{self, Snapshot};
use crate::activity_history;
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, Render, SharedString, Task, WeakEntity,
    Window,
};
use std::{path::PathBuf, time::Duration};
use ui::{ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

pub struct ActivityButton {
    workspace: WeakEntity<Workspace>,
}
impl ActivityButton {
    pub fn new(workspace: &Workspace) -> Self {
        Self {
            workspace: workspace.weak_handle(),
        }
    }
}
impl Render for ActivityButton {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.clone();
        IconButton::new("agent-activity", IconName::Wrench)
            .aria_label("Agent activity")
            .tooltip(Tooltip::text("Agent activity and tool calls"))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if let Some(view) = workspace.item_of_type::<ActivityView>(cx) {
                            workspace.activate_item(&view, true, true, window, cx);
                        } else {
                            let handle = workspace.weak_handle();
                            let view = cx.new(|cx| ActivityView::new(handle, cx));
                            workspace.add_item_to_active_pane(
                                Box::new(view),
                                None,
                                true,
                                window,
                                cx,
                            );
                        }
                    })
                    .log_err();
            })
    }
}
impl workspace::StatusItemView for ActivityButton {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn workspace::ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }
    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

pub struct ActivityView {
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    snapshot: Snapshot,
    selected: Option<String>,
    filter: &'static str,
    category: &'static str,
    session: Option<String>,
    configured: bool,
    history_view: bool,
    history: Vec<activity_history::Change>,
    history_page: usize,
    relationship_mode: bool,
    page: usize,
    flow: bool,
    paused: bool,
    busy: bool,
    reveal: bool,
    _polling: Task<()>,
    _loading: Task<()>,
}
impl ActivityView {
    fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let polling = cx.spawn(async move |view, cx| {
            loop {
                if view
                    .update(cx, |view, cx| {
                        if !view.paused {
                            view.refresh(cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        });
        Self {
            workspace,
            focus: cx.focus_handle(),
            snapshot: Snapshot::default(),
            selected: None,
            filter: "All",
            category: "All tools",
            session: None,
            configured: false,
            history_view: false,
            history: Vec::new(),
            history_page: 0,
            relationship_mode: false,
            page: 0,
            flow: true,
            paused: false,
            busy: false,
            reveal: false,
            _polling: polling,
            _loading: Task::ready(()),
        }
    }
    fn select_record(&mut self, key: String, cx: &mut Context<Self>) {
        self.session = None;
        self.filter = "All";
        self.category = "All tools";
        self.configured = false;
        self.history_view = false;
        self.reveal = false;
        let entries: Vec<_> = self.snapshot.entries.iter().collect();
        let links = activity_data::relationships(&self.snapshot.entries);
        let ordered = if self.relationship_mode {
            activity_data::relationship_order(&entries, &links)
                .into_iter()
                .map(|(entry, _)| entry)
                .collect()
        } else if self.flow {
            activity_data::flow_order(&entries)
        } else {
            entries
        };
        self.page = ordered
            .iter()
            .position(|entry| entry.key == key)
            .unwrap_or(0)
            / 30;
        self.selected = Some(key);
        cx.notify();
    }
    fn open_configuration(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(task) = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.open_abs_path(
                    path,
                    workspace::OpenOptions {
                        focus: Some(true),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .log_err()
        {
            task.detach_and_log_err(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let roots: Vec<PathBuf> = workspace
            .read(cx)
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect();
        let home = paths::home_dir();
        let codex = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let homes = vec![
            ("Codex".into(), codex.join("sessions")),
            ("Claude".into(), claude.join("projects")),
        ];
        let history_path =
            activity_history::workspace_file(&paths::data_dir().join("agent-activity"), &roots);
        self.busy = true;
        self._loading =
            cx.spawn(async move |view, cx| {
                let (snapshot, history) = cx
                    .background_spawn(async move {
                        let mut snapshot = activity_data::load(&roots, &homes);
                        let history = match activity_history::observe(
                            &history_path,
                            &snapshot.configurations,
                            &chrono::Utc::now().to_rfc3339(),
                        ) {
                            Ok((history, notices)) => {
                                snapshot.notices.extend(notices);
                                history
                            }
                            Err(error) => {
                                snapshot
                                    .notices
                                    .push(format!("Configuration history unavailable: {error}"));
                                Vec::new()
                            }
                        };
                        (snapshot, history)
                    })
                    .await;
                view.update(cx, |view, cx| {
                    view.snapshot = snapshot;
                    view.history = history;
                    view.busy = false;
                    if view.selected.as_ref().is_some_and(|key| {
                        !view.snapshot.entries.iter().any(|entry| &entry.key == key)
                    }) {
                        view.selected = None;
                        view.reveal = false;
                    }
                    cx.notify();
                })
                .log_err();
            });
        cx.notify();
    }
}
fn category_color(category: activity_data::ToolCategory) -> Color {
    match category {
        activity_data::ToolCategory::BuiltIn => Color::Info,
        activity_data::ToolCategory::Mcp => Color::Accent,
        activity_data::ToolCategory::Hook => Color::Warning,
        activity_data::ToolCategory::Unknown => Color::Muted,
    }
}

fn activity_section(title: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .w_full()
        .mt_2()
        .pt_4()
        .pb_1()
        .border_t_1()
        .border_color(cx.theme().colors().border)
        .child(Label::new(title).size(LabelSize::Large))
}

impl Render for ActivityView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries: Vec<_> = self
            .snapshot
            .entries
            .iter()
            .filter(|entry| {
                (self.filter == "All" || entry.agent == self.filter)
                    && entry.matches_category(self.category)
                    && self
                        .session
                        .as_ref()
                        .is_none_or(|session| *session == entry.session_key())
            })
            .collect();
        let mut sessions = std::collections::BTreeMap::new();
        for entry in &self.snapshot.entries {
            let session = sessions
                .entry(entry.session_key())
                .or_insert((entry, 0usize));
            session.1 += 1;
        }
        let links = activity_data::relationships(&self.snapshot.entries);
        let tree = activity_data::relationship_order(&entries, &links);
        let entries = if self.relationship_mode {
            tree.iter().map(|(entry, _)| *entry).collect()
        } else if self.flow {
            activity_data::flow_order(&entries)
        } else {
            entries
        };
        let pages = entries.len().div_ceil(30).max(1);
        self.page = self.page.min(pages - 1);
        let history_pages = self.history.len().div_ceil(20).max(1);
        self.history_page = self.history_page.min(history_pages - 1);
        let detail = self
            .selected
            .as_ref()
            .and_then(|key| self.snapshot.entries.iter().find(|entry| &entry.key == key));
        v_flex().id("activity-inspector").track_focus(&self.focus).size_full().p_4().gap_3().overflow_hidden()
            .child(h_flex().w_full().pb_3().border_b_1().border_color(cx.theme().colors().border).gap_2().flex_wrap().child(div().flex_1().child(Label::new("Agent activity").size(LabelSize::Large)))
                .child(Button::new("activity-pause", if self.paused { "Resume updates" } else { "Pause updates" }).on_click(cx.listener(|view, _, _, cx| { view.paused = !view.paused; if view.paused { view._loading = Task::ready(()); view.busy = false; } cx.notify(); })))
                .child(Button::new("activity-refresh", if self.busy { "Reading…" } else { "Refresh" }).disabled(self.busy).on_click(cx.listener(|view, _, _, cx| view.refresh(cx)))))
            .child(Label::new("Local transcripts for this workspace and its subprojects. Observed does not mean completed. Hook activity appears only when explicitly recorded.").color(Color::Muted))
            .child(h_flex().w_full().justify_between().gap_4().flex_wrap()
                .child(h_flex().gap_2().children(["All", "Claude", "Codex"].into_iter().map(|filter| {
                Button::new(filter, filter).toggle_state(self.filter == filter).on_click(cx.listener(move |view, _, _, cx| { view.filter = filter; view.page = 0; view.selected = None; view.reveal = false; cx.notify(); }))
            })))
            .child(h_flex().gap_2().flex_wrap().children(["All tools", "Built in", "MCP tools", "Hooks", "Custom", "Unknown"].into_iter().map(|category| {
                Button::new(category, category).toggle_state(self.category == category).on_click(cx.listener(move |view, _, _, cx| { view.category = category; view.page = 0; view.selected = None; view.reveal = false; view.configured = false; view.history_view = false; cx.notify(); }))
            }))))
            .when(self.category == "Custom", |view| view.child(Label::new("Custom groups MCP tools and recorded hooks. It does not establish who authored them.").color(Color::Muted)))
            .child(h_flex().w_full().justify_between().gap_4().flex_wrap()
                .child(h_flex().gap_2().flex_wrap()
                .child(Button::new("activity-configured", "Configured customizations").toggle_state(self.configured).on_click(cx.listener(|view, _, _, cx| { view.configured = !view.configured; view.history_view = false; view.selected = None; view.reveal = false; cx.notify(); })))
                .child(Button::new("activity-history", "Configuration history").toggle_state(self.history_view).on_click(cx.listener(|view, _, _, cx| { view.history_view = !view.history_view; view.configured = false; view.selected = None; view.reveal = false; cx.notify(); }))))
            .child(h_flex().gap_2().flex_wrap()
                .child(Button::new("activity-flow", "Flow").toggle_state(self.flow && !self.relationship_mode).on_click(cx.listener(|view, _, _, cx| { view.flow = true; view.relationship_mode = false; view.page = 0; view.selected = None; view.reveal = false; cx.notify(); })))
                .child(Button::new("activity-relationships", "Relationships").toggle_state(self.relationship_mode).on_click(cx.listener(|view, _, _, cx| { view.flow = true; view.relationship_mode = true; view.page = 0; view.selected = None; view.reveal = false; cx.notify(); })))
                .child(Button::new("activity-list-mode", "List").toggle_state(!self.flow).on_click(cx.listener(|view, _, _, cx| { view.flow = false; view.relationship_mode = false; view.page = 0; view.selected = None; view.reveal = false; cx.notify(); })))
                ))
            .child(Label::new(if self.relationship_mode { "Recorded parent links only · indentation shows children · unlinked records remain roots" } else if self.flow { "Recorded order within each session · arrows do not imply dependencies" } else { "Newest records first" }).color(Color::Muted))
            .when(!self.snapshot.notices.is_empty(), |view| view.child(h_flex().w_full().gap_4().flex_wrap().children(self.snapshot.notices.iter().map(|notice| Label::new(notice.clone()).size(LabelSize::Small).color(Color::Warning)))))
            .child(h_flex().w_full().gap_3().items_start()
                .child(Label::new(format!("Sessions ({})", sessions.len())).color(Color::Muted))
                .child(h_flex().id("activity-sessions").flex_1().min_w_0().max_h(gpui::px(100.0)).overflow_y_scroll().flex_wrap().gap_2()
                    .child(Button::new("all-sessions", "All sessions").toggle_state(self.session.is_none()).on_click(cx.listener(|view, _, _, cx| { view.session = None; view.page = 0; view.selected = None; view.reveal = false; cx.notify(); })))
                    .children(sessions.iter().enumerate().map(|(index, (key, (entry, count)))| {
                        let key = key.clone();
                        let identifier = if entry.session_id.is_empty() { entry.source.file_stem().unwrap_or_default().to_string_lossy().into_owned() } else { entry.session_id.clone() };
                        Button::new(("activity-session", index), format!("{} · {} · {} calls", entry.agent, identifier.chars().take(12).collect::<String>(), count))
                            .toggle_state(self.session.as_ref() == Some(&key))
                            .on_click(cx.listener(move |view, _, _, cx| { view.session = Some(key.clone()); view.filter = "All"; view.category = "All tools"; view.page = 0; view.selected = None; view.reveal = false; view.configured = false; view.history_view = false; cx.notify(); }))
                    }))))
            .child(h_flex().w_full().flex_1().min_h_0().pt_4().border_t_1().border_color(cx.theme().colors().border).gap_4().items_stretch()
                .child(v_flex().w_1_3().flex_shrink_0().h_full().min_w_0().gap_2().pr_3()
                    .border_r_1().border_color(cx.theme().colors().border)
                    .child(div().w_full().pb_3().border_b_1().border_color(cx.theme().colors().border).child(Label::new(format!("{} · {} records", if self.flow { "Session flow" } else { "Recent activity" }, entries.len())).color(Color::Muted)))
                    .child(
                    v_flex().id("activity-list").flex_1().min_h_0().overflow_y_scroll().gap_1()
                        .when(entries.is_empty(), |view| view.child(Label::new(if self.snapshot.entries.is_empty() { "No recorded activity found for this workspace in retained history." } else { "No records match these filters. Choose All and All tools to see the retained history." })))
                        .children(entries.iter().skip(self.page * 30).take(30).enumerate().map(|(index, entry)| {
                            let key = entry.key.clone();
                            let absolute_index = self.page * 30 + index;
                            let same_session = absolute_index.checked_sub(1).and_then(|previous| entries.get(previous)).is_some_and(|previous| previous.source == entry.source && previous.agent == entry.agent);
                            let depth = tree.iter().find(|(item, _)| item.key == entry.key).map(|(_, depth)| *depth).unwrap_or(0);
                            v_flex().w_full().flex_shrink_0().gap_1().pb_2().border_b_1().border_color(cx.theme().colors().border_variant).when(self.relationship_mode, |view| view.pl(gpui::px((depth.min(8) * 12) as f32)))
                                .when(self.flow && (index == 0 || !same_session), |view| view.child(
                                    v_flex().px_3().py_2().gap_1().rounded_md().bg(cx.theme().colors().editor_background)
                                        .child(Label::new(format!("{} session{}", entry.agent, if same_session { " · continued" } else { "" })).color(Color::Accent))
                                        .child(Label::new(entry.source.display().to_string()).size(LabelSize::Small).color(Color::Muted).line_clamp(2))))
                                .when(self.flow && !self.relationship_mode && same_session && index > 0, |view| view.child(h_flex().justify_center().child(Icon::new(IconName::ArrowDown).color(Color::Muted))))
                                .when(self.relationship_mode && depth > 0, |view| view.child(Label::new(format!("↳ Recorded child · depth {depth}")).size(LabelSize::Small).color(Color::Accent)))
                                .child(ListItem::new(("activity-entry", index)).spacing(ListItemSpacing::Sparse)
                                .toggle_state(self.selected.as_ref() == Some(&key))
                                .child(v_flex().w_full().min_w_0().gap_1()
                                    .child(h_flex().w_full().justify_between().gap_2().flex_wrap()
                                        .child(Label::new(entry.name.clone()).line_clamp(1))
                                        .child(Label::new(entry.category().label()).size(LabelSize::Small).color(category_color(entry.category()))))
                                    .when(self.flow, |view| view.child(Label::new(entry.timestamp.clone()).size(LabelSize::Small).color(Color::Muted)))
                                    .child(Label::new(format!("{} · {}", entry.agent, entry.status)).size(LabelSize::Small).color(Color::Muted)))
                                .on_click(cx.listener(move |view, _, _, cx| { view.selected = Some(key.clone()); view.configured = false; view.history_view = false; view.reveal = false; cx.notify(); })))
                        })))
                    .child(h_flex().w_full().pt_3().border_t_1().border_color(cx.theme().colors().border).gap_2()
                        .child(Button::new("activity-previous", "Previous").disabled(self.page == 0).on_click(cx.listener(|view, _, _, cx| { view.page = view.page.saturating_sub(1); view.selected = None; view.reveal = false; cx.notify(); })))
                        .child(Label::new(format!("{} / {pages}", self.page + 1)))
                        .child(Button::new("activity-next", "Next").disabled(self.page + 1 >= pages).on_click(cx.listener(|view, _, _, cx| { view.page += 1; view.selected = None; view.reveal = false; cx.notify(); })))))
                .child(v_flex().id("activity-detail").flex_1().h_full().min_w_0().min_h_0().px_3().overflow_y_scroll()
                    .child(v_flex().w_full().flex_shrink_0().gap_3()
                    .when(self.configured, |view| view
                        .child(Label::new("Configured customizations").size(LabelSize::Large))
                        .child(Label::new("Configuration only · execution not confirmed. These sources are not attributed to the selected call. Open Kit for additional skills, rules and MCP configuration.").color(Color::Muted))
                        .when(self.snapshot.configured_hooks.is_empty(), |view| view.child(Label::new("No hook definitions found in the discovered configuration sources.")))
                        .children(self.snapshot.configured_hooks.iter().enumerate().map(|(index, hook)| {
                            let path = PathBuf::from(&hook.path);
                            v_flex().w_full().gap_2().py_3().border_b_1().border_color(cx.theme().colors().border_variant)
                                .child(Label::new(format!("{} · {} · {}", hook.agent, hook.event, if hook.disabled { "Disabled configuration" } else { "Configured only" })).color(Color::Warning))
                                .child(Label::new(format!("Handler: {} · Matcher: {}", hook.kind, hook.matcher)).color(Color::Muted))
                                .child(Button::new(("configured-hook", index), hook.path.clone()).truncate(true).on_click(cx.listener(move |view, _, window, cx| view.open_configuration(path.clone(), window, cx))))
                        }))
                        .children(self.snapshot.configurations.iter().enumerate().map(|(index, path)| {
                            let path = path.clone();
                            Button::new(("configured-source", index), path.display().to_string()).truncate(true).on_click(cx.listener(move |view, _, window, cx| view.open_configuration(path.clone(), window, cx)))
                        })))
                    .when(self.history_view, |view| view
                        .child(Label::new("Configuration history").size(LabelSize::Large))
                        .child(Label::new("Observed while this inspector is open. Baseline is the first observation, not the creation date. Values and command text are not stored; field fingerprints identify changes. At most 200 events and 256 files are retained.").color(Color::Muted))
                        .when(self.history.is_empty(), |view| view.child(Label::new("No configuration observations yet. See notices for read errors.")))
                        .children(self.history.iter().skip(self.history_page * 20).take(20).enumerate().map(|(index, change)| {
                            let path = change.path.clone();
                            v_flex().w_full().gap_2().py_3().border_b_1().border_color(cx.theme().colors().border_variant)
                                .child(Label::new(format!("{} · {}", change.kind, change.observed_at)))
                                .child(Button::new(("history-source", index), path.display().to_string()).truncate(true).disabled(change.kind == "Removed").on_click(cx.listener(move |view, _, window, cx| view.open_configuration(path.clone(), window, cx))))
                                .children(change.fields.iter().map(|field| Label::new(format!("Changed field: {field}")).size(LabelSize::Small).color(Color::Muted)))
                        }))
                        .child(h_flex().gap_2()
                            .child(Button::new("history-previous", "Previous changes").disabled(self.history_page == 0).on_click(cx.listener(|view, _, _, cx| { view.history_page = view.history_page.saturating_sub(1); cx.notify(); })))
                            .child(Label::new(format!("{} / {history_pages}", self.history_page + 1)))
                            .child(Button::new("history-next", "Next changes").disabled(self.history_page + 1 >= history_pages).on_click(cx.listener(|view, _, _, cx| { view.history_page += 1; cx.notify(); })))))
                    .when(detail.is_none() && !self.configured && !self.history_view, |view| view.child(Label::new("Activity details").size(LabelSize::Large)).child(Label::new("Select a record to see its input, output and configuration sources.").color(Color::Muted)))
                    .when_some(detail, |view, entry| view
                        .child(h_flex().w_full().pb_3().border_b_1().border_color(cx.theme().colors().border).justify_between().gap_3().flex_wrap()
                            .child(Label::new(entry.name.clone()).size(LabelSize::Large))
                            .child(Label::new("Call details").color(Color::Muted)))
                        .child(Label::new(entry.category().label()).color(category_color(entry.category())))
                        .child(Label::new(entry.category_evidence()).color(Color::Muted))
                        .when_some(entry.mcp_server(), |view, server| view.child(Label::new(format!("MCP server: {server}"))))
                        .child(Label::new(format!("{} · {}", entry.agent, entry.status)))
                         .child(Label::new(format!("Call ID: {}", entry.call_id)).color(Color::Muted))
                        .when(!entry.session_id.is_empty(), |view| view.child(Label::new(format!("Session: {}", entry.session_id)).color(Color::Muted)))
                        .child(activity_section("Recorded relationships", cx))
                        .when(!links.iter().any(|link| link.child == entry.key || link.parent == entry.key), |view| view.child(Label::new("No resolved parent or child call in retained history.").color(Color::Muted)))
                        .when(!entry.parent_tool_id.is_empty() && !links.iter().any(|link| link.child == entry.key), |view| view.child(Label::new(format!("Unresolved parent tool ID: {} · absent, ambiguous or cyclic", entry.parent_tool_id)).color(Color::Muted)))
                        .child(v_flex().id("related-calls").max_h(gpui::px(200.0)).overflow_y_scroll().gap_1().children(links.iter().filter(|link| link.child == entry.key || link.parent == entry.key).take(50).enumerate().filter_map(|(index, link)| {
                            let parent = link.child == entry.key;
                            let key = if parent { &link.parent } else { &link.child };
                            self.snapshot.entries.iter().find(|candidate| &candidate.key == key).map(|candidate| {
                                let key = key.clone();
                                Button::new(("related-call", index), format!("{}: {} · {} · {}", if parent { "Parent" } else { "Child" }, candidate.name, candidate.timestamp, link.evidence)).truncate(true).on_click(cx.listener(move |view, _, _, cx| view.select_record(key.clone(), cx)))
                            })
                        })))
                        .when(!entry.parent_thread_id.is_empty(), |view| view.child(Label::new(format!("Recorded parent session: {}", entry.parent_thread_id)).color(Color::Muted)))
                        .children(activity_data::related_sessions(entry, &self.snapshot.entries).into_iter().enumerate().map(|(index, (candidate, relation))| {
                            let key = candidate.key.clone();
                            Button::new(("related-session", index), format!("{relation}: {}", candidate.session_id)).truncate(true).on_click(cx.listener(move |view, _, _, cx| view.select_record(key.clone(), cx)))
                        }))
                        .child(activity_section("Timing and source", cx))
                        .child(Label::new(format!("Recorded {}", entry.timestamp)))
                        .child(Label::new(entry.duration.map(|duration| format!("Recorded interval: {duration} ms")).unwrap_or_else(|| "Duration not recorded".into())))
                        .child(Label::new(format!("Project: {}", entry.cwd)))
                        .child(Label::new(format!("Source: {}", entry.source.display())).color(Color::Muted).line_clamp(2))
                        .child(activity_section("Configuration sources", cx))
                        .child(Label::new("Inspect your customizations below. A configured hook is not proof that it ran for this call.").color(Color::Muted))
                        .children(self.snapshot.configurations.iter().enumerate().map(|(index, path)| {
                            let path = path.clone();
                            Button::new(("activity-configuration", index), path.display().to_string()).truncate(true)
                                .on_click(cx.listener(move |view, _, window, cx| view.open_configuration(path.clone(), window, cx)))
                        }))
                        .child(activity_section("Input and output", cx))
                        .child(Label::new("Payloads may contain private project content. Common credential fields are masked; masking is not exhaustive.").color(Color::Muted))
                        .child(Button::new("activity-reveal", if self.reveal { "Hide payloads" } else { "Show masked input and output" }).on_click(cx.listener(|view, _, _, cx| { view.reveal = !view.reveal; cx.notify(); })))
                        .when(self.reveal, |view| view.child(Label::new("Input")).child(div().id("activity-input").w_full().flex_shrink_0().max_h(gpui::px(320.0)).overflow_y_scroll().p_3().rounded_md().bg(cx.theme().colors().editor_background).child(div().w_full().child(entry.input.clone()))).child(Label::new("Output")).child(div().id("activity-output").w_full().flex_shrink_0().max_h(gpui::px(320.0)).overflow_y_scroll().p_3().rounded_md().bg(cx.theme().colors().editor_background).child(div().w_full().child(entry.output.clone()))))
                    ))))
    }
}
impl Focusable for ActivityView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl EventEmitter<()> for ActivityView {}
impl Item for ActivityView {
    type Event = ();
    fn to_item_events(_: &(), _: &mut dyn FnMut(ItemEvent)) {}
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        "Agent activity".into()
    }
    fn tab_icon(&self, _: &Window, _: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Wrench))
    }
    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}
