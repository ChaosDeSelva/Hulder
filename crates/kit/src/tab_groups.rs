use crate::tab_groups_data::{self, Library, append_unique};
use editor::Editor;
use gpui::{
    Action, App, AppContext, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Global, Pixels, Render, Subscription, Task, WeakEntity, Window, actions, px,
};
use std::{collections::HashSet, path::PathBuf};
use ui::{Tooltip, prelude::*};
use util::ResultExt;
use workspace::{
    OpenOptions, SaveIntent, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(tab_groups, [TogglePanel]);
struct SharedLibrary(Entity<LibraryStore>);
impl Global for SharedLibrary {}
struct LibraryStore {
    data: Library,
    ready: bool,
    pending: usize,
    error: Option<String>,
    write: Option<Task<()>>,
    missing: HashSet<PathBuf>,
}
impl LibraryStore {
    fn new(cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |store, cx| {
            let result = cx.background_spawn(async { tab_groups_data::load(&paths::data_dir().join("tab-groups.json")).map(|data| { let missing = missing_files(&data); (data,missing) }) }).await;
            store.update(cx, |store,cx| {
                match result {
                    Ok((data, missing)) => { store.data = data; store.missing = missing; store.ready = true; }
                    Err(error) => store.error = Some(format!("Could not load groups: {error}. The original file has been preserved.")),
                }
                cx.notify();
            }).log_err();
        }).detach();
        Self {
            data: Library::default(),
            ready: false,
            pending: 0,
            error: None,
            write: None,
            missing: HashSet::new(),
        }
    }
    fn change(
        &mut self,
        change: impl FnOnce(&mut Library) -> anyhow::Result<()>,
        cx: &mut Context<Self>,
    ) {
        if !self.ready {
            return;
        }
        let mut data = self.data.clone();
        if let Err(error) = change(&mut data) {
            self.error = Some(error.to_string());
            cx.notify();
            return;
        }
        self.data = data.clone();
        self.pending += 1;
        self.error = None;
        let previous = self.write.take();
        self.write = Some(cx.spawn(async move |store, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let result = cx
                .background_spawn(async move {
                    tab_groups_data::save(&paths::data_dir().join("tab-groups.json"), &data)
                        .map(|_| missing_files(&data))
                })
                .await;
            store
                .update(cx, |store, cx| {
                    store.pending = store.pending.saturating_sub(1);
                    match result {
                        Ok(missing) => store.missing = missing,
                        Err(error) => {
                            store.error = Some(format!("Groups could not be saved: {error}"))
                        }
                    }
                    cx.notify();
                })
                .log_err();
        }));
        cx.notify();
    }
}
fn missing_files(data: &Library) -> HashSet<PathBuf> {
    data.bookmarks
        .iter()
        .chain(data.groups.iter().flat_map(|group| group.files.iter()))
        .filter(|path| !path.is_file())
        .cloned()
        .collect()
}
pub fn init(cx: &mut App) {
    let store = cx.new(LibraryStore::new);
    let quit_store = store.clone();
    cx.on_app_quit(move |cx| {
        let pending = quit_store.update(cx, |store, _| store.write.take());
        async move {
            if let Some(pending) = pending {
                pending.await;
            }
        }
    })
    .detach();
    cx.set_global(SharedLibrary(store));
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &TogglePanel, window, cx| {
            workspace.toggle_panel_focus::<GroupsPanel>(window, cx);
        });
    })
    .detach();
}

pub struct GroupsPanel {
    workspace: WeakEntity<Workspace>,
    store: Entity<LibraryStore>,
    name: Entity<Editor>,
    selected: Option<String>,
    focus: FocusHandle,
    notice: String,
    busy: bool,
    _subscription: Subscription,
}
impl GroupsPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |_, window, cx| {
                cx.new(|cx| {
                    let store = cx.global::<SharedLibrary>().0.clone();
                    let subscription = cx.observe(&store, |_, _, cx| cx.notify());
                    let name = cx.new(|cx| {
                        let mut editor = Editor::single_line(window, cx);
                        editor.set_placeholder_text("Group name", window, cx);
                        editor
                    });
                    Self {
                        workspace: workspace.clone(),
                        store,
                        name,
                        selected: None,
                        focus: cx.focus_handle(),
                        notice: String::new(),
                        busy: false,
                        _subscription: subscription,
                    }
                })
            })
        })
    }
    fn capture(&self, cx: &App) -> anyhow::Result<(Vec<PathBuf>, usize)> {
        self.workspace.read_with(cx, |workspace, cx| {
            let files = workspace.open_item_abs_paths(cx);
            let skipped = workspace.items(cx).count().saturating_sub(files.len());
            (files, skipped)
        })
    }
    fn save_group(&mut self, cx: &mut Context<Self>) {
        match self.capture(cx) {
            Ok((files, skipped)) => {
                let name = self.name.read(cx).text(cx);
                self.store.update(cx, |store, cx| {
                    store.change(|data| data.create(&name, files), cx)
                });
                self.notice = format!(
                    "Captured saved file tabs. {skipped} untitled or nonfile tabs skipped."
                );
            }
            Err(error) => self.notice = error.to_string(),
        }
        cx.notify();
    }
    fn bookmark(&mut self, cx: &mut Context<Self>) {
        let path = self.workspace.read_with(cx, |workspace, cx| {
            let item = workspace.active_item(cx)?;
            workspace
                .project()
                .read(cx)
                .absolute_path(&item.project_path(cx)?, cx)
        });
        match path {
            Ok(Some(path)) => self.store.update(cx, |store, cx| {
                store.change(
                    |data| {
                        append_unique(&mut data.bookmarks, vec![path]);
                        Ok(())
                    },
                    cx,
                )
            }),
            _ => self.notice = "Select a saved local file tab to bookmark it.".into(),
        }
        cx.notify();
    }
    fn open(
        &mut self,
        files: Vec<PathBuf>,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.notice = "Opening saved files…".into();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |panel, cx| {
            let result: anyhow::Result<String> = async {
                let checks = cx
                    .background_spawn(async move {
                        let mut valid = Vec::new();
                        let mut missing = Vec::new();
                        for path in files {
                            if path.is_file() {
                                valid.push(path);
                            } else {
                                missing.push(path.display().to_string());
                            }
                        }
                        (valid, missing)
                    })
                    .await;
                let (files, missing) = checks;
                anyhow::ensure!(
                    missing.is_empty(),
                    "Missing or unavailable files (nothing replaced): {}",
                    format!(
                        "{} ({} total)",
                        missing
                            .iter()
                            .take(3)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", "),
                        missing.len()
                    )
                );
                anyhow::ensure!(!files.is_empty(), "This group has no files.");
                let previous = workspace.read_with(cx, |workspace, cx| {
                    workspace
                        .panes()
                        .iter()
                        .map(|pane| {
                            let ids = pane
                                .read(cx)
                                .items()
                                .filter(|item| {
                                    item.project_path(cx)
                                        .and_then(|path| {
                                            workspace.project().read(cx).absolute_path(&path, cx)
                                        })
                                        .is_none_or(|path| !files.contains(&path))
                                })
                                .map(|item| item.item_id())
                                .collect::<Vec<_>>();
                            (pane.clone(), ids)
                        })
                        .collect::<Vec<_>>()
                })?;
                let count = files.len();
                for file in files {
                    let item = workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace.open_abs_path(file, OpenOptions::default(), window, cx)
                        })?
                        .await?;
                    workspace.update(cx, |workspace, cx| {
                        for pane in workspace.panes() {
                            pane.update(cx, |pane, _| {
                                pane.unpreview_item_if_preview(item.item_id())
                            });
                        }
                    })?;
                }
                if replace {
                    for (pane, ids) in previous {
                        pane.update_in(cx, |pane, window, cx| {
                            pane.close_items(window, cx, SaveIntent::Close, &|id| ids.contains(&id))
                        })?
                        .await?;
                        if pane.read_with(cx, |pane,_|pane.items().any(|item|ids.contains(&item.item_id()))) {
                            return Ok(format!("Opened {count} files. Replacement cancelled; remaining tabs were kept."));
                        }
                    }
                }
                Ok(format!(
                    "Opened {count} files. Any tabs kept by a cancelled close remain open."
                ))
            }
            .await;
            panel
                .update(cx, |panel, cx| {
                    panel.busy = false;
                    panel.notice = match result {
                        Ok(message) => message,
                        Err(error) => format!("Could not complete opening: {error}"),
                    };
                    cx.notify();
                })
                .log_err();
        })
        .detach();
        cx.notify();
    }
}
impl Render for GroupsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let local = self
            .workspace
            .read_with(cx, |workspace, cx| workspace.project().read(cx).is_local())
            .unwrap_or(false);
        let ready = store.ready && !self.busy && local;
        let error = store.error.clone();
        let persistence = if !store.ready {
            "Loading saved groups"
        } else if store.pending > 0 {
            "Saving changes…"
        } else if store.error.is_some() {
            "Check the error below"
        } else {
            "Saved on this Mac"
        };
        let data = store.data.clone();
        let selected = self.selected.as_ref().and_then(|name| {
            data.groups
                .iter()
                .enumerate()
                .find(|(_, group)| &group.name == name)
                .map(|(index, group)| (index, group.clone()))
        });
        v_flex().size_full().track_focus(&self.focus).gap_2().p_3()
            .child(Label::new("Groups and bookmarks").size(LabelSize::Large))
            .child(Label::new(persistence).color(Color::Muted))
            .when(!local, |this|this.child(Label::new("Open a local workspace to save or reopen these files.").color(Color::Warning)))
            .child(div().p_2().border_1().border_color(cx.theme().colors().border_variant).rounded_md().child(self.name.clone()))
            .child(h_flex().gap_2().flex_wrap()
                .child(Button::new("save-tabs","Save open tabs").disabled(!ready).on_click(cx.listener(|view,_,_,cx| view.save_group(cx))))
                .child(Button::new("bookmark-active","Bookmark active file").disabled(!ready).on_click(cx.listener(|view,_,_,cx| view.bookmark(cx)))))
            .when_some(error,|this,error| this.child(Label::new(error).color(Color::Error)))
            .when(!self.notice.is_empty(),|this| this.child(Label::new(self.notice.clone()).color(Color::Muted)))
            .child(v_flex().id("groups-scroll").flex_1().min_h_0().overflow_y_scroll().gap_2()
                .child(h_flex().gap_2().child(Label::new("Tab groups"))
                    .child(Button::new("refresh-groups", "Refresh file status").disabled(!ready).on_click(cx.listener(|view,_,_,cx|view.store.update(cx,|store,cx|store.change(|_|Ok(()),cx))))))
                .when(data.groups.is_empty(),|this| this.child(Label::new("Open some files, enter a name and save your first group.").color(Color::Muted)))
                .children(data.groups.iter().enumerate().map(|(index,group)| {
                    let name=group.name.clone();
                    h_flex().gap_1().child(Button::new(("group",index),format!("{} · {} files",group.name,group.files.len())).truncate(true).when(self.selected.as_deref()==Some(group.name.as_str()), |button|button.style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))).on_click(cx.listener(move |view,_,window,cx| { view.selected=Some(name.clone()); view.name.update(cx,|editor,cx| editor.set_text(name.clone(),window,cx)); cx.notify(); })))
                        .child(IconButton::new(("group-up",index),IconName::ArrowUp).disabled(index==0 || !ready).tooltip(Tooltip::text("Move group up")).on_click(cx.listener(move |view,_,_,cx| { view.selected=None; view.store.update(cx,|store,cx|store.change(|data|{ if index>0 && index<data.groups.len(){data.groups.swap(index,index-1);} Ok(()) },cx)); })))
                        .child(IconButton::new(("group-down",index),IconName::ArrowDown).disabled(index+1==data.groups.len() || !ready).tooltip(Tooltip::text("Move group down")).on_click(cx.listener(move |view,_,_,cx| { view.selected=None; view.store.update(cx,|store,cx|store.change(|data|{ if index+1<data.groups.len(){data.groups.swap(index,index+1);} Ok(()) },cx)); })))
                }))
                .when_some(selected,|this,(index,group)| {
                    let files=group.files.clone(); let replace_files=files.clone();
                    this.child(v_flex().gap_2().py_3().border_t_1().border_b_1().border_color(cx.theme().colors().border_variant)
                        .child(Label::new(group.name))
                        .child(h_flex().gap_1().flex_wrap()
                            .child(Button::new("open-group","Open all").disabled(!ready).on_click(cx.listener(move |view,_,window,cx| view.open(files.clone(),false,window,cx))))
                            .child(Button::new("replace-tabs","Replace tabs").disabled(!ready).on_click(cx.listener(move |view,_,window,cx| view.open(replace_files.clone(),true,window,cx))))
                            .child(Button::new("rename-group","Rename").disabled(!ready).on_click(cx.listener(move |view,_,_,cx| { let name=view.name.read(cx).text(cx); view.store.update(cx,|store,cx| store.change(|data|data.rename(index,&name),cx)); view.selected=Some(name.trim().to_owned()); })))
                            .child(Button::new("add-tabs","Add open tabs").disabled(!ready).on_click(cx.listener(move |view,_,_,cx| { match view.capture(cx) { Ok((files,skipped))=> {view.store.update(cx,|store,cx|store.change(|data|{if let Some(group)=data.groups.get_mut(index){append_unique(&mut group.files,files);} Ok(())},cx));view.notice=format!("{skipped} untitled or nonfile tabs skipped.");},Err(error)=>view.notice=error.to_string() } cx.notify(); })))
                            .child(Button::new("delete-group","Delete group").disabled(!ready).tooltip(Tooltip::text("Remove the saved group only; files stay on disk")).on_click(cx.listener(move |view,_,_,cx| {view.selected=None;view.store.update(cx,|store,cx|store.change(|data|{if index<data.groups.len(){data.groups.remove(index);}Ok(())},cx));}))))
                        .children(group.files.iter().enumerate().map(|(file_index,path)| self.file_row(path.clone(),Some(index),file_index,group.files.len(),ready,cx))))
                })
                .child(Label::new("Bookmarks"))
                .when(data.bookmarks.is_empty(),|this|this.child(Label::new("Bookmark the active file for quick access.").color(Color::Muted)))
                .children(data.bookmarks.iter().enumerate().map(|(index,path)|self.file_row(path.clone(),None,index,data.bookmarks.len(),ready,cx))))
    }
}
impl GroupsPanel {
    fn file_row(
        &self,
        path: PathBuf,
        group: Option<usize>,
        index: usize,
        count: usize,
        ready: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let label = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let missing = self.store.read(cx).missing.contains(&path);
        let prefix = if group.is_some() {
            "group-file"
        } else {
            "bookmark"
        };
        v_flex()
            .gap_1()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new((prefix, index), label)
                            .truncate(true)
                            .tooltip(Tooltip::text(path.display().to_string()))
                            .disabled(!ready)
                            .on_click(cx.listener({
                                let path = path.clone();
                                move |view, _, window, cx| {
                                    view.open(vec![path.clone()], false, window, cx)
                                }
                            })),
                    )
                    .child(
                        IconButton::new(
                            (gpui::SharedString::from(format!("{prefix}-up")), index),
                            IconName::ArrowUp,
                        )
                        .disabled(index == 0 || !ready)
                        .tooltip(Tooltip::text("Move file up"))
                        .on_click(
                            cx.listener(move |view, _, _, cx| view.edit_files(group, index, 0, cx)),
                        ),
                    )
                    .child(
                        IconButton::new(
                            (gpui::SharedString::from(format!("{prefix}-down")), index),
                            IconName::ArrowDown,
                        )
                        .disabled(index + 1 == count || !ready)
                        .tooltip(Tooltip::text("Move file down"))
                        .on_click(
                            cx.listener(move |view, _, _, cx| view.edit_files(group, index, 1, cx)),
                        ),
                    )
                    .child(
                        IconButton::new(
                            (gpui::SharedString::from(format!("{prefix}-remove")), index),
                            IconName::Close,
                        )
                        .disabled(!ready)
                        .tooltip(Tooltip::text("Remove from saved list; keep file on disk"))
                        .on_click(
                            cx.listener(move |view, _, _, cx| view.edit_files(group, index, 2, cx)),
                        ),
                    ),
            )
            .when(missing, |this| {
                this.child(Label::new("Missing or unavailable file").color(Color::Error))
                    .child(
                        Button::new(
                            (gpui::SharedString::from(format!("{prefix}-repair")), index),
                            "Replace path with active file",
                        )
                        .disabled(!ready)
                        .on_click(
                            cx.listener(move |view, _, _, cx| view.repair_file(group, index, cx)),
                        ),
                    )
            })
            .child(
                Label::new(path.display().to_string())
                    .truncate()
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }
    fn repair_file(&mut self, group: Option<usize>, index: usize, cx: &mut Context<Self>) {
        let path = self.workspace.read_with(cx, |workspace, cx| {
            let item = workspace.active_item(cx)?;
            workspace
                .project()
                .read(cx)
                .absolute_path(&item.project_path(cx)?, cx)
        });
        match path {
            Ok(Some(path)) => self.store.update(cx, |store, cx| {
                store.change(
                    |data| {
                        let files = match group {
                            Some(group) => {
                                &mut data
                                    .groups
                                    .get_mut(group)
                                    .ok_or_else(|| anyhow::anyhow!("Group no longer exists"))?
                                    .files
                            }
                            None => &mut data.bookmarks,
                        };
                        anyhow::ensure!(
                            !files.contains(&path),
                            "This file is already saved in this list."
                        );
                        let target = files
                            .get_mut(index)
                            .ok_or_else(|| anyhow::anyhow!("Saved file no longer exists"))?;
                        *target = path;
                        Ok(())
                    },
                    cx,
                )
            }),
            _ => {
                self.notice =
                    "Open the relocated file in an editor tab first, then replace its saved path."
                        .into()
            }
        }
        cx.notify();
    }
    fn edit_files(
        &mut self,
        group: Option<usize>,
        index: usize,
        operation: u8,
        cx: &mut Context<Self>,
    ) {
        self.store.update(cx, |store, cx| {
            store.change(
                |data| {
                    let files = match group {
                        Some(group) => {
                            &mut data
                                .groups
                                .get_mut(group)
                                .ok_or_else(|| anyhow::anyhow!("Group no longer exists"))?
                                .files
                        }
                        None => &mut data.bookmarks,
                    };
                    if index < files.len() {
                        match operation {
                            0 if index > 0 => files.swap(index, index - 1),
                            1 if index + 1 < files.len() => files.swap(index, index + 1),
                            2 => {
                                files.remove(index);
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                },
                cx,
            )
        });
    }
}
impl Focusable for GroupsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl EventEmitter<PanelEvent> for GroupsPanel {}
impl Panel for GroupsPanel {
    fn persistent_name() -> &'static str {
        "GroupsPanel"
    }
    fn panel_key() -> &'static str {
        "GroupsPanel"
    }
    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Left
    }
    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Left
    }
    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}
    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(360.)
    }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Folder)
    }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Groups and bookmarks")
    }
    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(TogglePanel)
    }
    fn activation_priority(&self) -> u32 {
        6
    }
}
