use crate::{
    Anchor, DisplayRow, Editor, SelectionEffects,
    display_map::ToDisplayPoint,
    editor_settings::EditorSettingsScrollbarProxy,
    hover_popover::{hide_hover, hover_markdown_style},
    scroll::Autoscroll,
};
use collections::HashMap;
use glosses::{
    AddGloss, Anchoring, Gloss, GlossStore, GlossStoreEvent, LineRange, LoadedGlosses,
    MAX_GLOSS_BODY_BYTES, ReanchorIndex,
};
use gpui::{
    AnyElement, App, Bounds, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Render, ScrollHandle,
    Size, StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, canvas, div, px,
};
use markdown::{CopyButtonVisibility, Markdown, MarkdownElement};
use project::ProjectPath;
use std::{cell::Cell, ops::Range, rc::Rc, sync::Arc};
use text::{Bias, Point};
use ui::{
    Button, ButtonSize, ButtonStyle, IconButton, Label, Scrollbars, Tooltip, WithScrollbar,
    prelude::*,
};
use workspace::{ModalView, Toast, notifications::NotificationId};

/// A gloss placed in the current text of a singleton buffer.
#[derive(Clone, Debug)]
pub struct GlossMark {
    pub gloss: Gloss,
    /// 0-based buffer row where the gutter mark is drawn.
    pub row: u32,
    /// 0-based buffer rows the gloss covers, when anchored.
    pub rows: Option<Range<u32>>,
    pub detached: bool,
}

pub(crate) struct GlossMarkCache {
    edit_count: usize,
    marks: Arc<[GlossMark]>,
    loaded: Arc<LoadedGlosses>,
}

/// Messages about the gloss file as a whole, shown above every gloss of it.
pub fn file_notices(loaded: &LoadedGlosses) -> Vec<SharedString> {
    let mut notices = Vec::new();
    if !loaded.errors.is_empty() {
        let count = loaded.errors.len();
        let noun = if count == 1 { "section" } else { "sections" };
        notices.push(
            format!("{count} {noun} of the gloss file could not be parsed: {}", loaded.errors[0])
                .into(),
        );
    }
    if loaded.hidden > 0 {
        notices.push(format!("{} more glosses not shown", loaded.hidden).into());
    }
    notices
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlossIndicator {
    pub mark_index: usize,
    pub detached: bool,
}

pub struct GlossPopover {
    pub anchor: Anchor,
    markdown: Entity<Markdown>,
    scroll_handle: ScrollHandle,
    gloss: Gloss,
    detached: bool,
    gloss_file: Option<ProjectPath>,
    pub notices: Vec<SharedString>,
    pub(crate) last_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

struct GlossSaveFailed;

impl Editor {
    pub(crate) fn gloss_project_path(&self, cx: &App) -> Option<ProjectPath> {
        let buffer = self.buffer().read(cx).as_singleton()?;
        let file = project::File::from_dyn(buffer.read(cx).file())?;
        Some(ProjectPath {
            worktree_id: file.worktree_id(cx),
            path: file.path.clone(),
        })
    }

    /// Called when the gloss store reports a change to one file; only the editor showing
    /// that file drops its marks.
    pub(crate) fn handle_gloss_store_event(&mut self, event: &GlossStoreEvent, cx: &mut Context<Self>) {
        let GlossStoreEvent::Changed { worktree_id, path } = event;
        let own = self.gloss_project_path(cx);
        if own.is_some_and(|own| own.worktree_id == *worktree_id && own.path == *path) {
            self.gloss_marks = None;
            cx.notify();
        }
    }

    /// Glosses of the edited file, re-anchored against the current text. Cached until the
    /// buffer is edited or the gloss store reports a change to this file.
    pub fn gloss_marks(&mut self, cx: &mut Context<Self>) -> Arc<[GlossMark]> {
        self.gloss_marks_with_file(cx).0
    }

    pub(crate) fn gloss_marks_with_file(
        &mut self,
        cx: &mut Context<Self>,
    ) -> (Arc<[GlossMark]>, Arc<LoadedGlosses>) {
        let Some(store) = GlossStore::try_global(cx) else {
            return (Arc::from([]), Arc::default());
        };
        let Some(path) = self.gloss_project_path(cx) else {
            return (Arc::from([]), Arc::default());
        };
        let Some(buffer) = self.buffer().read(cx).as_singleton() else {
            return (Arc::from([]), Arc::default());
        };
        let edit_count = self.buffer().read(cx).snapshot(cx).edit_count();
        if let Some(cache) = &self.gloss_marks
            && cache.edit_count == edit_count
        {
            return (cache.marks.clone(), cache.loaded.clone());
        }

        let loaded = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        let snapshot = buffer.read(cx).snapshot();
        let max_row = snapshot.max_point().row;
        let line_count = max_row.saturating_add(1);
        let text = snapshot.text();
        // One pass over the text serves every gloss; probing the buffer per gloss made a
        // file with thousands of glosses unusable.
        let mut index = ReanchorIndex::new(&text, line_count);
        let marks: Arc<[GlossMark]> = loaded
            .glosses
            .iter()
            .map(|gloss| match index.reanchor(gloss) {
                None => GlossMark {
                    gloss: gloss.clone(),
                    row: 0,
                    rows: None,
                    detached: false,
                },
                Some(Anchoring::Anchored(range)) => GlossMark {
                    gloss: gloss.clone(),
                    row: range.start.saturating_sub(1),
                    rows: Some(range.start.saturating_sub(1)..range.end),
                    detached: false,
                },
                Some(Anchoring::Detached) => GlossMark {
                    gloss: gloss.clone(),
                    row: gloss
                        .lines
                        .map_or(0, |lines| lines.start.saturating_sub(1))
                        .min(max_row),
                    rows: None,
                    detached: true,
                },
            })
            .collect();
        self.gloss_marks = Some(GlossMarkCache {
            edit_count,
            marks: marks.clone(),
            loaded: loaded.clone(),
        });
        (marks, loaded)
    }

    pub fn active_gloss_rows(
        &mut self,
        range: Range<DisplayRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> HashMap<DisplayRow, GlossIndicator> {
        let mut rows = HashMap::default();
        let marks = self.gloss_marks(cx);
        if marks.is_empty() {
            return rows;
        }
        let snapshot = self.snapshot(window, cx);
        for (mark_index, mark) in marks.iter().enumerate() {
            let display_row = Point::new(mark.row, 0)
                .to_display_point(&snapshot.display_snapshot)
                .row();
            if !range.contains(&display_row) {
                continue;
            }
            let indicator = GlossIndicator {
                mark_index,
                detached: mark.detached,
            };
            // An anchored gloss wins the slot over a detached one on the same row.
            match rows.get(&display_row) {
                Some(existing) if !existing.detached => {}
                _ => {
                    rows.insert(display_row, indicator);
                }
            }
        }
        rows
    }

    pub(crate) fn render_gloss_indicator(
        &self,
        row: DisplayRow,
        indicator: GlossIndicator,
        cx: &mut Context<Self>,
    ) -> IconButton {
        let (color, label): (Color, SharedString) = if indicator.detached {
            (Color::Muted, "detached: anchor not found".into())
        } else {
            (Color::Accent, "Show gloss".into())
        };
        IconButton::new(("gloss indicator", row.0 as usize), IconName::Gloss)
            .icon_size(IconSize::XSmall)
            .size(ButtonSize::None)
            .icon_color(color)
            .style(ButtonStyle::Transparent)
            .on_click(cx.listener(move |editor, _, window, cx| {
                editor.toggle_gloss_popover(indicator.mark_index, window, cx);
            }))
            .tooltip(move |_window, cx| Tooltip::simple(label.clone(), cx))
    }

    pub fn toggle_gloss_popover(
        &mut self,
        mark_index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (marks, loaded) = self.gloss_marks_with_file(cx);
        let Some(mark) = marks.get(mark_index) else {
            return;
        };
        let already_shown = self
            .hover_state
            .gloss_popover
            .as_ref()
            .is_some_and(|popover| popover.gloss == mark.gloss);
        hide_hover(self, cx);
        if already_shown {
            return;
        }

        let snapshot = self.buffer().read(cx).snapshot(cx);
        let anchor = snapshot.anchor_at(Point::new(mark.row, 0), Bias::Left);
        let body: SharedString = if mark.gloss.body.trim().is_empty() {
            "*(empty gloss)*".into()
        } else {
            mark.gloss.body.clone().into()
        };
        let markdown = cx.new(|cx| Markdown::new(body, None, None, cx));
        let gloss_file = self
            .gloss_project_path(cx)
            .and_then(|path| GlossStore::gloss_file_project_path(&path));
        self.hover_state.gloss_popover = Some(GlossPopover {
            anchor,
            markdown,
            scroll_handle: ScrollHandle::new(),
            gloss: mark.gloss.clone(),
            detached: mark.detached,
            gloss_file,
            notices: file_notices(&loaded),
            last_bounds: Rc::default(),
        });
        cx.notify();
    }

    fn open_gloss_file(
        &mut self,
        gloss: Gloss,
        gloss_file: ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        hide_hover(self, cx);
        let open = workspace.update(cx, |workspace, cx| {
            workspace.open_path(gloss_file, None, true, window, cx)
        });
        cx.spawn_in(window, async move |_, cx| {
            let item = open.await?;
            let Some(editor) = item.downcast::<Editor>() else {
                return anyhow::Ok(());
            };
            editor.update_in(cx, |editor, window, cx| {
                let text = editor.buffer().read(cx).snapshot(cx).text();
                if let Some(row) = glosses::model::section_line(&text, &gloss) {
                    let point = Point::new(row, 0);
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([point..point]),
                    );
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn add_gloss(&mut self, _: &AddGloss, window: &mut Window, cx: &mut Context<Self>) {
        if GlossStore::try_global(cx).is_none() {
            return;
        }
        let Some(path) = self.gloss_project_path(cx) else {
            return;
        };
        let Some(buffer) = self.buffer().read(cx).as_singleton() else {
            return;
        };
        let Some(workspace) = self.workspace() else {
            return;
        };

        let selection = self.selections.newest::<Point>(&self.display_snapshot(cx));
        let start_row = selection.start.row;
        let mut end_row = selection.end.row;
        if end_row > start_row && selection.end.column == 0 {
            end_row -= 1;
        }
        let snapshot = buffer.read(cx).snapshot();
        let start_row = start_row.min(snapshot.max_point().row);
        let end_row = end_row.clamp(start_row, snapshot.max_point().row);
        let anchor: String = snapshot
            .text_for_range(
                Point::new(start_row, 0)..Point::new(start_row, snapshot.line_len(start_row)),
            )
            .collect();
        let lines = LineRange::new(start_row + 1, end_row + 1);

        let workspace_handle = workspace.downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                GlossPrompt::new(
                    path,
                    lines,
                    anchor.trim().to_string(),
                    workspace_handle,
                    window,
                    cx,
                )
            });
        });
    }
}

impl GlossPopover {
    pub(crate) fn focused(&self, window: &Window, cx: &App) -> bool {
        self.markdown.focus_handle(cx).is_focused(window)
    }

    pub(crate) fn render(
        &mut self,
        max_size: Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let bounds_cell = self.last_bounds.clone();
        let created = self.gloss.created.format("%Y-%m-%d").to_string();
        let meta = match self.gloss.author.as_deref() {
            Some(author) => format!("Gloss · {author} · {created}"),
            None => format!("Gloss · {created}"),
        };
        let lines = self.gloss.lines.map(|lines| {
            if lines.start == lines.end {
                format!("line {}", lines.start)
            } else {
                format!("lines {}-{}", lines.start, lines.end)
            }
        });
        let gloss = self.gloss.clone();
        let gloss_file = self.gloss_file.clone();
        let notices = self.notices.clone();

        div()
            .id("gloss_popover")
            .occlude()
            .elevation_2(cx)
            .child(
                canvas(
                    move |bounds, _window, _cx| {
                        bounds_cell.set(Some(bounds));
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_mouse_move(move |_, _, cx: &mut App| {
                this.update(cx, |editor, _| {
                    editor.hover_state.closest_mouse_distance = Some(px(0.0));
                    editor.hover_state.hiding_delay_task = None;
                    editor.hover_state.hint_hover_task = None;
                })
                .ok();
                cx.stop_propagation()
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .px_2()
                    .pt_1()
                    .gap_2()
                    .child(Label::new(meta).size(LabelSize::Small).color(Color::Muted))
                    .when_some(lines, |this, lines| {
                        this.child(Label::new(lines).size(LabelSize::Small).color(Color::Muted))
                    })
                    .when(self.detached, |this| {
                        this.child(
                            Label::new("detached: anchor not found")
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    }),
            )
            .children(notices.into_iter().map(|notice| {
                div().px_2().child(
                    Label::new(notice)
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            }))
            .child(
                div()
                    .id("gloss-md-container")
                    .overflow_y_scroll()
                    .max_w(max_size.width)
                    .max_h(max_size.height)
                    .track_scroll(&self.scroll_handle)
                    .child(
                        MarkdownElement::new(
                            self.markdown.clone(),
                            hover_markdown_style(window, cx),
                        )
                        .scroll_handle(self.scroll_handle.clone())
                        .code_block_renderer(markdown::CodeBlockRenderer::Default {
                            copy_button_visibility: CopyButtonVisibility::Hidden,
                            wrap_button_visibility: markdown::WrapButtonVisibility::Hidden,
                            border: false,
                        })
                        .p_2(),
                    ),
            )
            .custom_scrollbars(
                Scrollbars::for_settings::<EditorSettingsScrollbarProxy>()
                    .tracked_scroll_handle(&self.scroll_handle),
                window,
                cx,
            )
            .when_some(gloss_file, |this, gloss_file| {
                this.child(
                    h_flex().px_2().pb_1().justify_end().child(
                        Button::new("edit-gloss", "Edit")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(move |editor, _, window, cx| {
                                editor.open_gloss_file(
                                    gloss.clone(),
                                    gloss_file.clone(),
                                    window,
                                    cx,
                                );
                            })),
                    ),
                )
            })
            .into_any_element()
    }
}

/// Single-line prompt for the text of a new gloss.
pub struct GlossPrompt {
    path: ProjectPath,
    lines: LineRange,
    anchor: String,
    workspace: WeakEntity<workspace::Workspace>,
    text_editor: Entity<Editor>,
    _subscriptions: Vec<Subscription>,
}

impl GlossPrompt {
    pub fn new(
        path: ProjectPath,
        lines: LineRange,
        anchor: String,
        workspace: WeakEntity<workspace::Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let text_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Gloss text (Markdown)", window, cx);
            editor
        });
        let dismiss_on_blur = cx.subscribe(&text_editor, |_, _, event: &crate::EditorEvent, cx| {
            if matches!(event, crate::EditorEvent::Blurred) {
                cx.emit(DismissEvent);
            }
        });
        Self {
            path,
            lines,
            anchor,
            workspace,
            text_editor,
            _subscriptions: vec![dismiss_on_blur],
        }
    }

    pub fn text(&self, cx: &App) -> String {
        self.text_editor.read(cx).text(cx)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_text_for_test(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.text_editor
            .update(cx, |editor, cx| editor.set_text(text, window, cx));
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let body = self.text(cx).trim().to_string();
        if body.is_empty() {
            cx.emit(DismissEvent);
            return;
        }
        let workspace = self.workspace.clone();
        if body.len() > MAX_GLOSS_BODY_BYTES {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(
                            NotificationId::unique::<GlossSaveFailed>(),
                            format!(
                                "Gloss text is over {} KiB; shorten it or edit the gloss file",
                                MAX_GLOSS_BODY_BYTES / 1024
                            ),
                        ),
                        cx,
                    );
                })
                .ok();
            return;
        }
        let Some(store) = GlossStore::try_global(cx) else {
            cx.emit(DismissEvent);
            return;
        };
        let save = store.update(cx, |store, cx| {
            store.add_gloss(
                self.path.clone(),
                Some(self.lines),
                self.anchor.clone(),
                body,
                None,
                cx,
            )
        });
        cx.spawn(async move |_, cx| {
            if let Err(error) = save.await {
                log::error!("failed to save gloss: {error:#}");
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_toast(
                            Toast::new(
                                NotificationId::unique::<GlossSaveFailed>(),
                                format!("Failed to save gloss: {error}"),
                            ),
                            cx,
                        );
                    })
                    .ok();
            }
        })
        .detach();
        cx.emit(DismissEvent);
    }
}

impl ModalView for GlossPrompt {}

impl Focusable for GlossPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.text_editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for GlossPrompt {}

impl Render for GlossPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if self.lines.start == self.lines.end {
            format!("Gloss line {}", self.lines.start)
        } else {
            format!("Gloss lines {}-{}", self.lines.start, self.lines.end)
        };
        v_flex()
            .w(rems(32.))
            .elevation_2(cx)
            .key_context("GlossPrompt")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                div()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .px_2()
                    .py_1()
                    .child(self.text_editor.clone()),
            )
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .child(Label::new(title).color(Color::Muted))
                    .child(
                        Label::new(self.anchor.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
    }
}
