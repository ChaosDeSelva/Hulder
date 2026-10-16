//! The Kit: a tabbed strip at the bottom of the Grove listing what an agent
//! will use in this project (skills, agents, MCP servers and rule files).

pub mod scan;

use anyhow::Context as _;
use collections::{HashMap, HashSet};
use fs::{Fs, MTime};
use gpui::{
    App, BackgroundExecutor, ClickEvent, Context, Entity, FocusHandle, Focusable, Global, Render,
    Subscription, Task, WeakEntity, Window,
};
use herd_panel::{HerdView, Selection};
use herdr::{AgentRow, AgentStatus, HerdState, WorkspaceRow};
use project::Project;
use std::{
    io::Read as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use ui::{Indicator, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{OpenOptions, Toast, Workspace, notifications::NotificationId};

pub use scan::{AgentDef, KitScan, McpServer, McpSource, Rule, Rules, Skill};

/// How long after the last worktree change the Kit rescans.
pub const REFRESH_DEBOUNCE: Duration = Duration::from_millis(300);
/// Largest project file (skill, agent, rule, settings, `.mcp.json`) the Kit reads.
pub const MAX_PROJECT_FILE_BYTES: u64 = 1024 * 1024;
/// Largest `~/.claude.json` the Kit parses.
pub const MAX_USER_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

/// Written by "Create AGENTS.md". `crates/saga` and its template do not exist
/// on this branch, so this is a short generic one.
pub const AGENTS_TEMPLATE: &str = "# AGENTS.md

Instructions for coding agents working in this repository.

## Build and test

- Build: `<command>`
- Test: `<command>`
- Lint: `<command>`

## Conventions

- Keep changes small and focused; one concern per commit.
- Add or update tests next to the code they cover.
- Do not commit generated files.

## Layout

- `src/` — application code
- `tests/` — integration tests
";

const HULDER_SERVER: &str = "hulder";

struct KitHerd(Entity<HerdState>);
impl Global for KitHerd {}

/// Lets the Kit reach herdr's state. `HerdState::global` panics when
/// `herdr::state::init` never ran (tests), so the Kit only looks this up with
/// `try_global` and shows no live agents when it is absent.
pub fn init(cx: &mut App) {
    cx.set_global(KitHerd(HerdState::global(cx)));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KitTab {
    Skills,
    Agents,
    Mcp,
    Rules,
}

impl KitTab {
    pub const ALL: [KitTab; 4] = [KitTab::Skills, KitTab::Agents, KitTab::Mcp, KitTab::Rules];

    pub fn label(self) -> &'static str {
        match self {
            KitTab::Skills => "Skills",
            KitTab::Agents => "Agents",
            KitTab::Mcp => "MCP",
            KitTab::Rules => "Rules",
        }
    }
}

/// `~/.claude.json` as last read, keyed by its mtime and size so an
/// unchanged file is not parsed again on every worktree change.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UserConfigCache {
    pub mtime: Option<MTime>,
    pub len: u64,
    pub text: Option<String>,
}

/// What one background scan produced.
pub struct ScanOutcome {
    pub scan: KitScan,
    /// Names of MCP servers whose command matches a live process.
    pub running: HashSet<String>,
    /// Raw `.mcp.json`, kept to decide whether the hulder entry exists.
    pub mcp_text: Option<String>,
    pub user_config: UserConfigCache,
}

/// The latest accepted scan, with the generation it came from.
#[derive(Default)]
pub struct ScanResults {
    pub scan: KitScan,
    pub running: HashSet<String>,
    pub mcp_text: Option<String>,
    pub user_config: UserConfigCache,
    /// Incremented per scheduled scan; results from an older scan are dropped.
    pub generation: usize,
}

impl ScanResults {
    /// Stores `outcome` unless it belongs to an older scan than the latest scheduled one.
    pub fn apply(&mut self, generation: usize, outcome: ScanOutcome) -> bool {
        if generation != self.generation {
            return false;
        }
        self.scan = outcome.scan;
        self.running = outcome.running;
        self.mcp_text = outcome.mcp_text;
        self.user_config = outcome.user_config;
        true
    }
}

/// Decides whether a refresh runs now or waits for the strip to expand.
/// While collapsed nothing is visible, so worktree churn costs no scan;
/// the first expand after a skipped request catches up.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RefreshGate {
    pub collapsed: bool,
    pub stale: bool,
}

impl RefreshGate {
    /// Called for every refresh request; true means run it now.
    pub fn request(&mut self) -> bool {
        if self.collapsed {
            self.stale = true;
            return false;
        }
        self.stale = false;
        true
    }

    /// Called when the strip collapses or expands; true means run a refresh
    /// now because one was skipped while collapsed.
    pub fn set_collapsed(&mut self, collapsed: bool) -> bool {
        self.collapsed = collapsed;
        let catch_up = !collapsed && self.stale;
        if catch_up {
            self.stale = false;
        }
        catch_up
    }
}

/// Reads at most `limit` bytes of `path` without following a link at the
/// file, on a background thread; more than `limit` bytes is an error.
pub async fn read_capped_nofollow(
    fs: &dyn Fs,
    executor: &BackgroundExecutor,
    path: &Path,
    limit: u64,
) -> anyhow::Result<String> {
    let reader = fs.open_sync_nofollow(path).await?;
    let bytes = executor
        .spawn(async move {
            let mut bytes = Vec::new();
            reader.take(limit + 1).read_to_end(&mut bytes)?;
            anyhow::Ok(bytes)
        })
        .await?;
    anyhow::ensure!(
        bytes.len() as u64 <= limit,
        "{} is larger than the {limit} byte limit",
        path.display()
    );
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

/// `~/.claude.json`, re-read only when its mtime or size moved since
/// `cached`. A missing, symlinked or oversized file counts as absent.
pub async fn load_user_config(
    fs: &dyn Fs,
    executor: &BackgroundExecutor,
    path: &Path,
    cached: UserConfigCache,
) -> UserConfigCache {
    let metadata = match fs.metadata(path).await {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return UserConfigCache::default(),
        Err(error) => {
            log::warn!("kit: cannot stat {}: {error:#}", path.display());
            return UserConfigCache::default();
        }
    };
    if metadata.is_symlink || metadata.is_dir {
        log::warn!(
            "kit: {} is a symlink or directory; ignoring it",
            path.display()
        );
        return UserConfigCache::default();
    }
    if cached.text.is_some() && cached.mtime == Some(metadata.mtime) && cached.len == metadata.len {
        return cached;
    }
    match read_capped_nofollow(fs, executor, path, MAX_USER_CONFIG_BYTES).await {
        Ok(text) => UserConfigCache {
            mtime: Some(metadata.mtime),
            len: metadata.len,
            text: Some(text),
        },
        Err(error) => {
            log::warn!("kit: ignoring {}: {error:#}", path.display());
            UserConfigCache::default()
        }
    }
}

pub struct Kit {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    herd: Option<Entity<HerdState>>,
    tab: KitTab,
    results: ScanResults,
    gate: RefreshGate,
    refresh_task: Task<()>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

struct KitToast;

fn toast(workspace: &mut Workspace, message: String, cx: &mut Context<Workspace>) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<KitToast>(), message).autohide(),
        cx,
    );
}

/// Names (executable file names) of every live process on this machine.
fn live_process_names() -> Vec<String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    system
        .processes()
        .values()
        .flat_map(|process| {
            let exe_name = process
                .exe()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned());
            [
                Some(process.name().to_string_lossy().into_owned()),
                exe_name,
            ]
        })
        .flatten()
        .collect()
}

impl Kit {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let fs = project.read(cx).fs().clone();
        let herd = cx.try_global::<KitHerd>().map(|herd| herd.0.clone());
        let subscriptions = herd
            .iter()
            .map(|herd| cx.observe(herd, |_, _, cx| cx.notify()))
            .collect();
        let mut this = Self {
            workspace,
            project,
            fs,
            herd,
            tab: KitTab::Skills,
            results: ScanResults::default(),
            gate: RefreshGate::default(),
            refresh_task: Task::ready(()),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.schedule_refresh(cx);
        this
    }

    pub fn tab(&self) -> KitTab {
        self.tab
    }

    pub fn set_tab(&mut self, tab: KitTab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    pub fn results(&self) -> &ScanResults {
        &self.results
    }

    /// Tells the Kit whether its strip is collapsed, so it stops rescanning
    /// a project nobody is looking at and catches up when expanded.
    pub fn set_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.gate.set_collapsed(collapsed) {
            self.schedule_refresh(cx);
        }
    }

    fn project_root(&self, cx: &App) -> Option<Arc<Path>> {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path())
    }

    /// Rescans after `REFRESH_DEBOUNCE`; a newer call supersedes a pending
    /// one, and a collapsed strip defers the scan until it is expanded.
    pub fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        if !self.gate.request() {
            return;
        }
        self.results.generation += 1;
        let generation = self.results.generation;
        self.refresh_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            let inputs = this
                .update(cx, |this, cx| {
                    let worktree = this.project.read(cx).visible_worktrees(cx).next()?;
                    let worktree = worktree.read(cx);
                    Some((
                        worktree.abs_path(),
                        worktree.snapshot(),
                        this.fs.clone(),
                        this.results.user_config.clone(),
                    ))
                })
                .ok()
                .flatten();
            let Some((root, snapshot, fs, cached_user_config)) = inputs else {
                this.update(cx, |this, cx| {
                    this.apply_scan(
                        generation,
                        ScanOutcome {
                            scan: KitScan::default(),
                            running: HashSet::default(),
                            mcp_text: None,
                            user_config: UserConfigCache::default(),
                        },
                        cx,
                    )
                })
                .ok();
                return;
            };
            let user_config_path = paths::home_dir().join(".claude.json");
            let executor = cx.background_executor().clone();
            let outcome = cx
                .background_spawn(async move {
                    // Only the handful of paths the scan knows about are kept;
                    // the rest of the tree streams past without being collected.
                    let paths: Vec<String> = snapshot
                        .files(true, 0)
                        .map(|entry| entry.path.as_unix_str().to_string())
                        .filter(|path| scan::is_relevant(path))
                        .collect();
                    let mut texts: HashMap<String, String> = HashMap::default();
                    for path in &paths {
                        let full = root.join(path);
                        match read_capped_nofollow(
                            fs.as_ref(),
                            &executor,
                            &full,
                            MAX_PROJECT_FILE_BYTES,
                        )
                        .await
                        {
                            Ok(text) => {
                                texts.insert(path.clone(), text);
                            }
                            Err(error) => log::warn!("kit: skipping {}: {error:#}", full.display()),
                        }
                    }
                    let user_config = load_user_config(
                        fs.as_ref(),
                        &executor,
                        &user_config_path,
                        cached_user_config,
                    )
                    .await;
                    let scan = scan::scan(
                        &paths,
                        &|path| texts.get(path).cloned(),
                        user_config.text.as_deref(),
                    );
                    let running = Self::running_commands(live_process_names(), &scan.mcp);
                    ScanOutcome {
                        scan,
                        running,
                        mcp_text: texts.remove(scan::MCP_FILE),
                        user_config,
                    }
                })
                .await;
            this.update(cx, |this, cx| this.apply_scan(generation, outcome, cx))
                .ok();
        });
    }

    fn apply_scan(&mut self, generation: usize, outcome: ScanOutcome, cx: &mut Context<Self>) {
        if self.results.apply(generation, outcome) {
            cx.notify();
        }
    }

    /// Live herdr agents whose cwd is `root` or below.
    pub fn live_agents_for_root(workspaces: &[WorkspaceRow], root: &Path) -> Vec<AgentRow> {
        workspaces
            .iter()
            .flat_map(|workspace| workspace.agents.iter())
            .filter(|agent| {
                agent
                    .cwd
                    .as_deref()
                    .is_some_and(|cwd| Path::new(cwd).starts_with(root))
            })
            .cloned()
            .collect()
    }

    /// Names of the servers whose command's file name matches a live process.
    pub fn running_commands(
        process_names: impl IntoIterator<Item = String>,
        servers: &[McpServer],
    ) -> HashSet<String> {
        let live: HashSet<String> = process_names.into_iter().collect();
        servers
            .iter()
            .filter(|server| {
                Path::new(&server.command)
                    .file_name()
                    .is_some_and(|name| live.contains(&*name.to_string_lossy()))
            })
            .map(|server| server.name.clone())
            .collect()
    }

    fn live_agents(&self, cx: &App) -> Vec<AgentRow> {
        let (Some(herd), Some(root)) = (&self.herd, self.project_root(cx)) else {
            return Vec::new();
        };
        Self::live_agents_for_root(&herd.read(cx).workspaces, &root)
    }

    fn has_hulder_server(&self) -> bool {
        scan::has_mcp_server(self.results.mcp_text.as_deref(), HULDER_SERVER)
    }

    fn open_relative_path(&self, relative: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.project_root(cx) else {
            return;
        };
        let abs_path = root.join(relative);
        self.open_abs_path(abs_path, window, cx);
    }

    fn open_abs_path(&self, abs_path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(task) = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.open_abs_path(
                    abs_path,
                    OpenOptions {
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

    fn open_live_agent(&self, agent: &AgentRow, window: &mut Window, cx: &mut Context<Self>) {
        let selection = Selection::Agent {
            terminal_id: agent.terminal_id.clone(),
            label: agent.label(),
        };
        self.workspace
            .update(cx, |workspace, cx| {
                HerdView::open(workspace, Some(selection), window, cx);
            })
            .log_err();
    }

    /// Writes a file in the project root and reports the outcome as a toast.
    fn write_in_root(
        &self,
        relative: &'static str,
        make_text: impl FnOnce(Option<&str>) -> anyhow::Result<String> + Send + 'static,
        success: impl FnOnce(&Path) -> String + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.project_root(cx) else {
            self.workspace
                .update(cx, |workspace, cx| {
                    toast(workspace, "Open a project folder first.".into(), cx)
                })
                .log_err();
            return;
        };
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| {
            let path = root.join(relative);
            let written: anyhow::Result<()> = async {
                let (path, existing) = checked_root_file(fs.clone(), &root, relative).await?;
                let text = make_text(existing.as_deref())?;
                fs.atomic_write(path.clone(), text)
                    .await
                    .with_context(|| format!("writing {}", path.display()))
            }
            .await;
            let message = match written {
                Ok(()) => success(&path),
                Err(error) => format!("Kit: {error:#}"),
            };
            workspace.update(cx, |workspace, cx| toast(workspace, message, cx))
        })
        .detach_and_log_err(cx);
    }

    /// The same writer the Go menu's "Share Editor with Agents" uses, so
    /// both produce one `.mcp.json` entry pointing at the absolute CLI.
    fn add_hulder_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                bifrost::share::share_with_agents(workspace, window, cx);
            })
            .log_err();
    }

    fn add_panda_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                lightpanda::mcp::share_with_agents(workspace, window, cx);
            })
            .log_err();
    }

    fn create_agents_md(&mut self, cx: &mut Context<Self>) {
        self.write_in_root(
            "AGENTS.md",
            |existing| {
                anyhow::ensure!(existing.is_none(), "AGENTS.md already exists");
                Ok(AGENTS_TEMPLATE.to_string())
            },
            |path| format!("Created {}.", path.display()),
            cx,
        );
    }

    fn status_icon(status: AgentStatus) -> (IconName, Color) {
        match status {
            AgentStatus::Working => (IconName::PlayFilled, Color::Accent),
            AgentStatus::Blocked => (IconName::Warning, Color::Warning),
            AgentStatus::Done => (IconName::Check, Color::Success),
            AgentStatus::Idle => (IconName::Circle, Color::Muted),
            AgentStatus::Unknown => (IconName::CircleHelp, Color::Disabled),
        }
    }

    fn render_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let counts = |tab: KitTab| match tab {
            KitTab::Skills => self.results.scan.skills.len(),
            KitTab::Agents => self.results.scan.agents.len() + self.live_agents(cx).len(),
            KitTab::Mcp => self.results.scan.mcp.len(),
            KitTab::Rules => self.results.scan.rules.found.len(),
        };
        h_flex()
            .w_full()
            .px_1()
            .py_0p5()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .children(KitTab::ALL.into_iter().enumerate().map(|(index, tab)| {
                let count = counts(tab);
                let label = if count > 0 {
                    format!("{} {count}", tab.label())
                } else {
                    tab.label().to_string()
                };
                Button::new(("kit-tab", index), label)
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .toggle_state(self.tab == tab)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
            }))
    }

    fn empty(message: &'static str) -> AnyElement {
        div()
            .px_2()
            .py_1()
            .child(
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn row(
        id: (&'static str, usize),
        icon: Icon,
        title: String,
        subtitle: Option<String>,
    ) -> ListItem {
        ListItem::new(id).spacing(ListItemSpacing::Sparse).child(
            v_flex()
                .min_w_0()
                .child(
                    h_flex()
                        .gap_1()
                        .child(icon)
                        .child(Label::new(title).size(LabelSize::Small)),
                )
                .children(subtitle.filter(|text| !text.is_empty()).map(|text| {
                    Label::new(text)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .line_clamp(1)
                })),
        )
    }

    fn render_skills(&self, cx: &Context<Self>) -> AnyElement {
        if self.results.scan.skills.is_empty() {
            return Self::empty("No skills. Add .claude/skills/<name>/SKILL.md.");
        }
        v_flex()
            .children(
                self.results
                    .scan
                    .skills
                    .iter()
                    .enumerate()
                    .map(|(index, skill)| {
                        let path = skill.path.clone();
                        let subtitle = skill
                            .trigger
                            .clone()
                            .unwrap_or_else(|| skill.description.clone());
                        Self::row(
                            ("kit-skill", index),
                            Icon::new(IconName::Sparkle)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                            skill.name.clone(),
                            Some(subtitle),
                        )
                        .tooltip(Tooltip::text(skill.description.clone()))
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                this.open_relative_path(&path, window, cx);
                            },
                        ))
                    }),
            )
            .into_any_element()
    }

    fn render_agents(&self, cx: &Context<Self>) -> AnyElement {
        let live = self.live_agents(cx);
        if self.results.scan.agents.is_empty() && live.is_empty() {
            return Self::empty("No agents. Add .claude/agents/<name>.md or start one in herdr.");
        }
        v_flex()
            .children(live.into_iter().enumerate().map(|(index, agent)| {
                let (icon, color) = Self::status_icon(agent.status);
                let subtitle = agent.cwd.clone().unwrap_or_default();
                Self::row(
                    ("kit-live-agent", index),
                    Icon::new(icon).size(IconSize::Small).color(color),
                    agent.label(),
                    Some(subtitle),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.open_live_agent(&agent, window, cx);
                }))
            }))
            .children(
                self.results
                    .scan
                    .agents
                    .iter()
                    .enumerate()
                    .map(|(index, agent)| {
                        let path = agent.path.clone();
                        let subtitle = [agent.model.as_deref(), agent.tools.as_deref()]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(" · ");
                        Self::row(
                            ("kit-agent", index),
                            Icon::new(IconName::Person)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                            agent.name.clone(),
                            Some(subtitle),
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                this.open_relative_path(&path, window, cx);
                            },
                        ))
                    }),
            )
            .into_any_element()
    }

    fn render_mcp(&self, cx: &Context<Self>) -> AnyElement {
        let has_hulder = self.has_hulder_server();
        let buttons = h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .child(
                Button::new("kit-add-hulder", "+ Bifröst")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .disabled(has_hulder)
                    .tooltip(Tooltip::text(if has_hulder {
                        "hulder is already in .mcp.json"
                    } else {
                        "Add the hulder CLI (`hulder mcp`) to .mcp.json so agents reach the editor over Bifröst"
                    }))
                    .on_click(cx.listener(|this, _, window, cx| this.add_hulder_server(window, cx))),
            )
            .child(
                Button::new("kit-add-panda", "+ panda")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text("Add Lightpanda to .mcp.json"))
                    .on_click(cx.listener(|this, _, window, cx| this.add_panda_server(window, cx))),
            );
        let list = if self.results.scan.mcp.is_empty() {
            Self::empty("No MCP servers in .mcp.json or ~/.claude.json.")
        } else {
            v_flex()
                .children(
                    self.results
                        .scan
                        .mcp
                        .iter()
                        .enumerate()
                        .map(|(index, server)| {
                            let running = self.results.running.contains(&server.name);
                            let source = match server.source {
                                McpSource::Project => None,
                                McpSource::User => Some(" (user)"),
                            };
                            let subtitle = std::iter::once(server.command.as_str())
                                .chain(server.args.iter().map(String::as_str))
                                .collect::<Vec<_>>()
                                .join(" ");
                            let item = Self::row(
                                ("kit-mcp", index),
                                Icon::new(IconName::Server)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                                format!("{}{}", server.name, source.unwrap_or_default()),
                                Some(subtitle),
                            )
                            .end_slot::<Indicator>(
                                running.then(|| Indicator::dot().color(Color::Success)),
                            )
                            .tooltip(Tooltip::text(if running {
                                "A process with this command is running"
                            } else {
                                "No process with this command is running"
                            }));
                            match server.source {
                                McpSource::Project => item.on_click(cx.listener(
                                    |this, _: &ClickEvent, window, cx| {
                                        this.open_relative_path(scan::MCP_FILE, window, cx);
                                    },
                                )),
                                McpSource::User => item.on_click(cx.listener(
                                    |this, _: &ClickEvent, window, cx| {
                                        this.open_abs_path(
                                            paths::home_dir().join(".claude.json"),
                                            window,
                                            cx,
                                        );
                                    },
                                )),
                            }
                        }),
                )
                .into_any_element()
        };
        v_flex().child(list).child(buttons).into_any_element()
    }

    fn render_rules(&self, cx: &Context<Self>) -> AnyElement {
        let rules = &self.results.scan.rules;
        v_flex()
            .children(rules.found.iter().enumerate().map(|(index, rule)| {
                let path = rule.path.clone();
                Self::row(
                    ("kit-rule", index),
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Success),
                    rule.label.clone(),
                    rule.detail.clone(),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.open_relative_path(&path, window, cx);
                }))
            }))
            .children(rules.missing.iter().enumerate().map(|(index, name)| {
                let name = *name;
                Self::row(
                    ("kit-missing-rule", index),
                    Icon::new(IconName::Circle)
                        .size(IconSize::Small)
                        .color(Color::Disabled),
                    name.to_string(),
                    Some("missing".to_string()),
                )
                .end_slot::<Button>((name == "AGENTS.md").then(|| {
                    Button::new("kit-create-agents-md", "Create AGENTS.md")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.create_agents_md(cx)))
                }))
            }))
            .when(rules.found.is_empty() && rules.missing.is_empty(), |this| {
                this.child(Self::empty("No project open."))
            })
            .into_any_element()
    }
}

impl Focusable for Kit {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Kit {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.tab {
            KitTab::Skills => self.render_skills(cx),
            KitTab::Agents => self.render_agents(cx),
            KitTab::Mcp => self.render_mcp(cx),
            KitTab::Rules => self.render_rules(cx),
        };
        v_flex()
            .id("kit")
            .key_context("Kit")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .child(self.render_tabs(cx))
            .child(
                div()
                    .id("kit-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

/// Resolves `relative` inside `root` and loads it if present. A symlink at the
/// target is refused so a planted link cannot make the Kit read or overwrite a
/// file outside the project; the parent must stay inside the canonical root.
async fn checked_root_file(
    fs: Arc<dyn Fs>,
    root: &Path,
    relative: &str,
) -> anyhow::Result<(PathBuf, Option<String>)> {
    let path = root.join(relative);
    anyhow::ensure!(
        Path::new(relative)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_))),
        "{relative} is not a plain file name"
    );
    if let Some(parent) = path.parent() {
        let canonical_root = fs.canonicalize(root).await?;
        let canonical_parent = fs.canonicalize(parent).await?;
        anyhow::ensure!(
            canonical_parent.starts_with(&canonical_root),
            "{} is outside the project",
            path.display()
        );
    }
    let Some(metadata) = fs.metadata(&path).await? else {
        return Ok((path, None));
    };
    anyhow::ensure!(
        !metadata.is_symlink,
        "{} is a symlink; refusing to write through it",
        path.display()
    );
    anyhow::ensure!(!metadata.is_dir, "{} is a directory", path.display());
    let existing = fs.load(&path).await?;
    Ok((path, Some(existing)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn checked_root_file_refuses_symlinks(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/outside", serde_json::json!({ "secret": "token" }))
            .await;
        fs.insert_tree("/repo", serde_json::json!({ "plain.json": "{}" }))
            .await;
        fs.insert_symlink(
            Path::new("/repo/.mcp.json"),
            PathBuf::from("/outside/secret"),
        )
        .await;

        let error = checked_root_file(fs.clone(), Path::new("/repo"), ".mcp.json")
            .await
            .expect_err("symlink must be refused");
        assert!(error.to_string().contains("symlink"), "{error:#}");

        let (path, existing) = checked_root_file(fs.clone(), Path::new("/repo"), "plain.json")
            .await
            .expect("plain file loads");
        assert_eq!(path, PathBuf::from("/repo/plain.json"));
        assert_eq!(existing.as_deref(), Some("{}"));

        let (_, missing) = checked_root_file(fs, Path::new("/repo"), "absent.json")
            .await
            .expect("missing file is fine");
        assert_eq!(missing, None);
    }

    fn agent(terminal_id: &str, cwd: Option<&str>) -> AgentRow {
        AgentRow {
            pane_id: format!("p-{terminal_id}"),
            tab_id: "tab".into(),
            terminal_id: terminal_id.into(),
            workspace_id: "w1".into(),
            agent: Some("claude".into()),
            display_agent: None,
            name: None,
            title: None,
            terminal_title_stripped: None,
            status: AgentStatus::Idle,
            cwd: cwd.map(String::from),
        }
    }

    #[test]
    fn live_agents_are_those_whose_cwd_is_inside_the_root() {
        let workspaces = vec![
            WorkspaceRow {
                id: "w1".into(),
                label: "one".into(),
                focused: false,
                worktree_path: None,
                branch: None,
                agents: vec![
                    agent("inside", Some("/repo/crates/kit")),
                    agent("root", Some("/repo")),
                    agent("sibling", Some("/repository")),
                    agent("none", None),
                ],
            },
            WorkspaceRow {
                id: "w2".into(),
                label: "two".into(),
                focused: false,
                worktree_path: None,
                branch: None,
                agents: vec![agent("elsewhere", Some("/other"))],
            },
        ];
        let live = Kit::live_agents_for_root(&workspaces, Path::new("/repo"));
        let ids: Vec<&str> = live
            .iter()
            .map(|agent| agent.terminal_id.as_str())
            .collect();
        assert_eq!(ids, vec!["inside", "root"]);
    }

    #[test]
    fn running_commands_match_the_file_name_of_the_command() {
        let servers = vec![
            McpServer {
                name: "panda".into(),
                command: "/usr/local/bin/lightpanda".into(),
                args: vec!["mcp".into()],
                source: McpSource::Project,
            },
            McpServer {
                name: "hulder".into(),
                command: "hulder".into(),
                args: Vec::new(),
                source: McpSource::Project,
            },
            McpServer {
                name: "empty".into(),
                command: String::new(),
                args: Vec::new(),
                source: McpSource::User,
            },
        ];
        let running = Kit::running_commands(
            ["lightpanda".to_string(), "zsh".to_string(), String::new()],
            &servers,
        );
        assert_eq!(running, HashSet::from_iter(["panda".to_string()]));
    }

    #[test]
    fn results_drop_outcomes_from_an_older_generation() {
        let outcome = |name: &str| ScanOutcome {
            scan: KitScan {
                skills: vec![Skill {
                    name: name.into(),
                    description: String::new(),
                    trigger: None,
                    path: "skills/x/SKILL.md".into(),
                }],
                ..Default::default()
            },
            running: HashSet::default(),
            mcp_text: None,
            user_config: UserConfigCache::default(),
        };
        let mut results = ScanResults {
            generation: 2,
            ..Default::default()
        };
        assert!(!results.apply(1, outcome("stale")));
        assert!(results.scan.skills.is_empty());
        assert!(results.apply(2, outcome("fresh")));
        assert_eq!(results.scan.skills[0].name, "fresh");
        assert!(!results.apply(1, outcome("late")));
        assert_eq!(results.scan.skills[0].name, "fresh");
    }

    #[test]
    fn collapsed_strip_defers_refresh_until_expanded() {
        let mut gate = RefreshGate::default();
        assert!(gate.request(), "expanded: run now");
        assert!(!gate.set_collapsed(true));
        assert!(!gate.request(), "collapsed: skipped");
        assert!(!gate.request());
        assert!(
            gate.set_collapsed(false),
            "expanding after a skip catches up"
        );
        assert!(!gate.stale);
        assert!(!gate.set_collapsed(true));
        assert!(!gate.set_collapsed(false), "nothing was skipped this time");
    }

    #[gpui::test]
    async fn user_config_is_cached_by_mtime_and_refused_when_linked_or_huge(
        cx: &mut TestAppContext,
    ) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/home",
            serde_json::json!({ ".claude.json": "{\"mcpServers\":{}}", "secret.json": "{}" }),
        )
        .await;
        let executor = cx.executor();
        let path = Path::new("/home/.claude.json");

        let first =
            load_user_config(fs.as_ref(), &executor, path, UserConfigCache::default()).await;
        assert_eq!(first.text.as_deref(), Some("{\"mcpServers\":{}}"));
        assert!(first.mtime.is_some());

        let stale = UserConfigCache {
            text: Some("STALE".into()),
            ..first.clone()
        };
        let cached = load_user_config(fs.as_ref(), &executor, path, stale.clone()).await;
        assert_eq!(cached, stale, "an unchanged file is not read again");

        fs.insert_file(path, b"{\"mcpServers\":{\"x\":{}}}".to_vec())
            .await;
        let reread = load_user_config(fs.as_ref(), &executor, path, stale).await;
        assert_eq!(reread.text.as_deref(), Some("{\"mcpServers\":{\"x\":{}}}"));
        assert_ne!(reread.mtime, first.mtime);

        fs.insert_file(path, vec![b'x'; MAX_USER_CONFIG_BYTES as usize + 1])
            .await;
        let huge = load_user_config(fs.as_ref(), &executor, path, UserConfigCache::default()).await;
        assert_eq!(huge, UserConfigCache::default());

        fs.remove_file(path, fs::RemoveOptions::default())
            .await
            .expect("remove");
        fs.insert_symlink(path, PathBuf::from("/home/secret.json"))
            .await;
        let linked =
            load_user_config(fs.as_ref(), &executor, path, UserConfigCache::default()).await;
        assert_eq!(
            linked,
            UserConfigCache::default(),
            "a symlink is never followed"
        );

        let missing = load_user_config(
            fs.as_ref(),
            &executor,
            Path::new("/home/none.json"),
            UserConfigCache::default(),
        )
        .await;
        assert_eq!(missing, UserConfigCache::default());
    }

    #[gpui::test]
    async fn project_file_reads_are_capped_and_never_follow_links(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            serde_json::json!({
                "CLAUDE.md": "rules",
                "big.md": "x".repeat(MAX_PROJECT_FILE_BYTES as usize + 1),
                "secret": "token",
            }),
        )
        .await;
        fs.insert_symlink(Path::new("/repo/AGENTS.md"), PathBuf::from("/repo/secret"))
            .await;
        let executor = cx.executor();
        let fine = read_capped_nofollow(
            fs.as_ref(),
            &executor,
            Path::new("/repo/CLAUDE.md"),
            MAX_PROJECT_FILE_BYTES,
        )
        .await
        .expect("plain file");
        assert_eq!(fine, "rules");
        let error = read_capped_nofollow(
            fs.as_ref(),
            &executor,
            Path::new("/repo/big.md"),
            MAX_PROJECT_FILE_BYTES,
        )
        .await
        .expect_err("too big");
        assert!(error.to_string().contains("limit"), "{error:#}");
        let error = read_capped_nofollow(
            fs.as_ref(),
            &executor,
            Path::new("/repo/AGENTS.md"),
            MAX_PROJECT_FILE_BYTES,
        )
        .await
        .expect_err("symlink");
        assert!(error.to_string().contains("symlink"), "{error:#}");
    }
}
