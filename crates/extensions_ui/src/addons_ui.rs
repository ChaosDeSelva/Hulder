use crate::addons::{self, Installed};
use anyhow::{Context as _, Result, ensure};
use editor::{Editor, EditorElement, EditorStyle};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, PathPromptOptions, PromptLevel, Render, SharedString, Styled, WeakEntity,
    Window, actions,
};
use project::trusted_worktrees::TrustedWorktrees;
use std::{collections::BTreeMap, path::PathBuf};
use task::{SpawnInTerminal, TaskId};
use terminal_view::TerminalView;
use ui::{Button, Clickable, Disableable, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

actions!(
    hulder_addons,
    [
        /// Opens installed feature add ons and their native panels.
        Open,
    ]
);

#[derive(
    Clone, Debug, Default, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action,
)]
#[action(namespace = hulder_addons)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub addon: String,
    pub command: String,
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &Open, window, cx| {
            open(workspace, window, cx);
        });
        workspace.register_action(|workspace, action: &Run, window, cx| {
            let view = open(workspace, window, cx);
            let action = action.clone();
            window.defer(cx, move |window, cx| {
                view.update(cx, |view, cx| {
                    view.run(&action.addon, &action.command, window, cx)
                });
            });
        });
    })
    .detach();
}

fn open(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<AddonsView> {
    if let Some(view) = workspace.item_of_type::<AddonsView>(cx) {
        workspace.activate_item(&view, true, true, window, cx);
        view
    } else {
        let workspace_handle = workspace.weak_handle();
        let view = cx.new(|cx| {
            let mut view = AddonsView {
                workspace: workspace_handle,
                focus: cx.focus_handle(),
                installed: Vec::new(),
                inputs: BTreeMap::new(),
                message: String::new(),
                busy: false,
            };
            view.reload(window, cx);
            view
        });
        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
        view
    }
}

fn store() -> PathBuf {
    paths::config_dir().join("hulder-addons")
}

struct AddonsView {
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    installed: Vec<Installed>,
    inputs: BTreeMap<(String, String), Entity<Editor>>,
    message: String,
    busy: bool,
}

impl AddonsView {
    fn command_button(
        id: &str,
        command: &addons::Command,
        section: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let addon = id.to_owned();
        let command_id = command.id.clone();
        Button::new(
            SharedString::from(format!("run-{id}-{section}-{}", command.id)),
            command.label.clone(),
        )
        .on_click(cx.listener(move |this, _, window, cx| this.run(&addon, &command_id, window, cx)))
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match addons::load(&store()) {
            Ok(installed) => {
                self.installed = installed;
                self.inputs.clear();
                for entry in &self.installed {
                    for (key, value) in &entry.settings {
                        let input = cx.new(|cx| {
                            let mut input = Editor::single_line(window, cx);
                            input.set_text(value.clone(), window, cx);
                            input
                        });
                        self.inputs
                            .insert((entry.manifest.id.clone(), key.clone()), input);
                    }
                }
            }
            Err(error) => {
                self.installed.clear();
                self.message = format!("Unable to load add ons: {error:#}");
            }
        }
        cx.notify();
    }

    fn install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a folder containing hulder-addon.json".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                let Some(paths) = selected.await?? else { return Ok(()); };
                let path = paths.into_iter().next().context("No folder selected")?;
                let manifest = cx.background_spawn(async move { addons::read_manifest(&path) }).await?;
                let detail = format!("{} {}\n{}\nPermissions: {:?}\nThe manifest will be copied. New installations and updates start disabled. Review the publisher before enabling.", manifest.name, manifest.version, manifest.description, manifest.permissions);
                let answer = this.update_in(cx, |_, window, cx| window.prompt(PromptLevel::Warning, "Install or update this add on?", Some(&detail), &["Cancel", "Install"], cx))?;
                if answer.await? != 1 { return Ok(()); }
                cx.background_spawn(async move { addons::install(&store(), manifest) }).await?;
                Ok(())
            }.await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                this.message = match result { Ok(()) => "Installation finished. Enable an installed add on to use it.".into(), Err(error) => format!("Installation failed: {error:#}") };
                this.reload(window, cx);
            }).log_err();
        }).detach();
    }

    fn change(
        &mut self,
        id: &str,
        remove: bool,
        toggle: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result: Result<()> = (|| {
            let mut installed = addons::load(&store())?
                .into_iter()
                .find(|entry| entry.manifest.id == id)
                .context("Add on no longer installed")?;
            if remove {
                return addons::remove(&store(), id);
            }
            if toggle {
                installed.enabled = !installed.enabled;
            }
            for (key, value) in &mut installed.settings {
                if let Some(input) = self.inputs.get(&(id.into(), key.clone())) {
                    *value = input.read(cx).text(cx);
                }
            }
            addons::save(&store(), &installed)
        })();
        self.message = match result {
            Ok(()) => {
                if remove {
                    "Add on removed. Existing terminal processes are not stopped.".into()
                } else {
                    "Add on preferences saved.".into()
                }
            }
            Err(error) => format!("Unable to save: {error:#}"),
        };
        self.reload(window, cx);
    }

    fn run(&mut self, id: &str, command_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let prepare: Result<_> = (|| {
            let installed = addons::load(&store())?
                .into_iter()
                .find(|entry| entry.manifest.id == id)
                .context("Add on not installed")?;
            ensure!(installed.enabled, "Enable this add on first");
            let command = installed
                .manifest
                .commands
                .iter()
                .find(|command| command.id == command_id)
                .context("Unknown add on command")?;
            let (executable, args) = command.resolve(&installed.settings)?;
            let project = self
                .workspace
                .upgrade()
                .context("Workspace closed")?
                .read(cx)
                .project()
                .clone();
            let root = project
                .read(cx)
                .active_project_directory(cx)
                .context("Open a project folder first")?;
            Self::check_trust(&project, &root, cx)?;
            let task = SpawnInTerminal {
                id: TaskId(format!("hulder-addon:{id}:{command_id}")),
                label: command.label.clone(),
                full_label: command.label.clone(),
                command: Some(executable.clone()),
                command_label: format!("{executable:?} {args:?}"),
                args,
                cwd: Some(root.to_path_buf()),
                use_new_terminal: true,
                show_summary: true,
                show_command: true,
                ..Default::default()
            };
            Ok((installed, project, root, task))
        })();
        let (installed, project, root, task) = match prepare {
            Ok(value) => value,
            Err(error) => {
                self.message = format!("Cannot run command: {error:#}");
                cx.notify();
                return;
            }
        };
        let detail = format!(
            "{}\nFolder: {}\nCommand: {}\nThis process has your user account's filesystem and network access. Only run commands from publishers you trust. Save changed files before running.",
            installed.manifest.name,
            root.display(),
            task.command_label
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            "Run this add on command?",
            Some(&detail),
            &["Cancel", "Run"],
            cx,
        );
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<()> = async {
                if answer.await? != 1 {
                    return Ok(());
                }
                let current = cx
                    .background_spawn(async move { addons::load(&store()) })
                    .await?;
                let entry = current
                    .into_iter()
                    .find(|entry| entry.manifest.id == installed.manifest.id)
                    .context("Add on removed")?;
                addons::verify_approval(&entry, &installed)?;
                cx.update(|_, cx| Self::check_trust(&project, &root, cx))??;
                let creating = project.update(cx, |project_value, cx| {
                    Ok::<_, anyhow::Error>(project_value.create_terminal_task(task, cx))
                })?;
                let terminal = creating.await?;
                workspace.update_in(cx, |workspace_value, window, cx| {
                    let database_id = workspace_value.database_id();
                    let view = cx.new(|cx| {
                        TerminalView::new(
                            terminal,
                            workspace.clone(),
                            database_id,
                            project.downgrade(),
                            window,
                            cx,
                        )
                    });
                    workspace_value.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })?;
                Ok(())
            }
            .await;
            if let Err(error) = result {
                this.update(cx, |this, cx| {
                    this.message = format!("Command failed: {error:#}");
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    fn check_trust(
        project: &Entity<project::Project>,
        root: &std::path::Path,
        cx: &mut App,
    ) -> Result<()> {
        let project_value = project.read(cx);
        ensure!(
            project_value.is_local(),
            "Feature add ons currently support local projects only"
        );
        let (worktree, _) = project_value
            .find_worktree(root, cx)
            .context("Open a project folder first")?;
        let worktree_id = worktree.read(cx).id();
        let worktree_store = project_value.worktree_store();
        let trusted =
            TrustedWorktrees::try_get_global(cx).context("Project trust is unavailable")?;
        ensure!(
            trusted.update(cx, |trusted, cx| trusted.can_trust(
                &worktree_store,
                worktree_id,
                cx
            )),
            "Trust the project before running add on commands"
        );
        Ok(())
    }

    fn template(
        &mut self,
        id: &str,
        template_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prepared: Result<_> = (|| {
            let entry = addons::load(&store())?
                .into_iter()
                .find(|entry| entry.manifest.id == id)
                .context("Add on not installed")?;
            ensure!(entry.enabled, "Enable this add on first");
            let template = entry
                .manifest
                .templates
                .iter()
                .find(|template| template.id == template_id)
                .context("Unknown template")?
                .clone();
            Ok(template)
        })();
        let template = match prepared {
            Ok(template) => template,
            Err(error) => {
                self.message = error.to_string();
                cx.notify();
                return;
            }
        };
        let addon_id = id.to_owned();
        let template_id = template_id.to_owned();
        let selected = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(format!("Create {} inside selected folder", template.directory).into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<Option<PathBuf>> = async {
                let Some(paths) = selected.await?? else {
                    return Ok(None);
                };
                let parent = paths.into_iter().next().context("No parent folder")?;
                cx.background_spawn(async move {
                    let entry = addons::load(&store())?
                        .into_iter()
                        .find(|entry| entry.manifest.id == addon_id)
                        .context("Add on removed")?;
                    ensure!(entry.enabled, "Add on disabled");
                    let current = entry
                        .manifest
                        .templates
                        .iter()
                        .find(|value| value.id == template_id)
                        .context("Template removed")?;
                    ensure!(
                        serde_json::to_vec(current)? == serde_json::to_vec(&template)?,
                        "Template changed; select it again"
                    );
                    addons::create_project(&parent, &template).map(Some)
                })
                .await
            }
            .await;
            this.update(cx, |this, cx| {
                this.message = match result {
                    Ok(Some(path)) => format!(
                        "Project created at {}. Open that folder to begin.",
                        path.display()
                    ),
                    Ok(None) => "Creation cancelled.".into(),
                    Err(error) => format!("Project creation failed: {error:#}"),
                };
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }
}

impl Render for AddonsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body=v_flex().gap_4().p_6().w_full()
            .child(h_flex().flex_wrap().gap_3().child(Headline::new("Hulder add ons"))
                .child(Button::new("install-addon","Install or update from folder").disabled(self.busy).on_click(cx.listener(|this,_,window,cx|this.install(window,cx))))
                .child(Button::new("reload-addons","Reload").on_click(cx.listener(|this,_,window,cx|this.reload(window,cx)))))
            .child(Label::new("Feature panels, commands and templates. Clone a repository, then select its add on folder to install. Language and theme extensions remain in Extensions."))
            .child(Label::new(self.message.clone()));
        if self.installed.is_empty() {
            body = body.child(Label::new("No feature add ons installed."));
        }
        for entry in self.installed.clone() {
            let id = entry.manifest.id.clone();
            let toggle_id = id.clone();
            let remove_id = id.clone();
            let save_id = id.clone();
            let mut card = v_flex()
                .gap_3()
                .p_4()
                .rounded_lg()
                .border_1()
                .border_color(cx.theme().colors().border)
                .child(Headline::new(format!(
                    "{} {}",
                    entry.manifest.name, entry.manifest.version
                )))
                .child(Label::new(entry.manifest.description.clone()))
                .child(Label::new(format!(
                    "API {} · Permissions: {:?}",
                    entry.manifest.api_version, entry.manifest.permissions
                )))
                .child(
                    h_flex()
                        .gap_3()
                        .child(
                            Button::new(
                                SharedString::from(format!("toggle-{id}")),
                                if entry.enabled { "Disable" } else { "Enable" },
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.change(&toggle_id, false, true, window, cx)
                                },
                            )),
                        )
                        .child(
                            Button::new(SharedString::from(format!("remove-{id}")), "Remove")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.change(&remove_id, true, false, window, cx)
                                })),
                        ),
                );
            for key in entry.settings.keys() {
                if let Some(input) = self.inputs.get(&(id.clone(), key.clone())) {
                    card = card.child(
                        v_flex().gap_2().child(Label::new(key.clone())).child(
                            div()
                                .p_2()
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .child(EditorElement::new(
                                    input,
                                    EditorStyle {
                                        background: cx.theme().colors().editor_background,
                                        local_player: cx.theme().players().local(),
                                        text: gpui::TextStyle {
                                            color: cx.theme().colors().text,
                                            font_size: gpui::rems(1.).into(),
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    },
                                )),
                        ),
                    );
                }
            }
            if !entry.settings.is_empty() {
                card = card.child(
                    Button::new(SharedString::from(format!("save-{id}")), "Save settings")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.change(&save_id, false, false, window, cx)
                        })),
                );
            }
            if entry.enabled {
                for (panel_index, panel) in entry.manifest.panels.iter().enumerate() {
                    let mut panel_view = v_flex()
                        .gap_3()
                        .p_3()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .child(Headline::new(panel.title.clone()))
                        .child(Label::new(panel.text.clone()));
                    for command_id in &panel.commands {
                        if let Some(command) = entry
                            .manifest
                            .commands
                            .iter()
                            .find(|command| &command.id == command_id)
                        {
                            panel_view = panel_view.child(Self::command_button(
                                &id,
                                command,
                                &format!("panel-{panel_index}"),
                                cx,
                            ));
                        }
                    }
                    card = card.child(panel_view);
                }
                for command in &entry.manifest.commands {
                    if !entry
                        .manifest
                        .panels
                        .iter()
                        .any(|panel| panel.commands.contains(&command.id))
                    {
                        card = card.child(Self::command_button(&id, command, "standalone", cx));
                    }
                }
                for template in &entry.manifest.templates {
                    let addon = id.clone();
                    let template_id = template.id.clone();
                    card = card.child(
                        Button::new(
                            SharedString::from(format!("template-{id}-{}", template.id)),
                            format!("Create {}", template.label),
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.template(&addon, &template_id, window, cx)
                            },
                        )),
                    );
                }
            }
            body = body.child(card);
        }
        div()
            .id("hulder-addons")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .child(body)
    }
}
impl Focusable for AddonsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl EventEmitter<()> for AddonsView {}
impl Item for AddonsView {
    type Event = ();
    fn to_item_events(_event: &(), _f: &mut dyn FnMut(ItemEvent)) {}
    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Add ons".into()
    }
    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}
