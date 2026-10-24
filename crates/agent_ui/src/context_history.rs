use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Task, Window};
use markdown::{Markdown, MarkdownElement};
use std::{path::{Path, PathBuf}, time::SystemTime};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{Workspace, item::{Item, ItemEvent}};

gpui::actions!(hulder, [OpenContextHistory]);

struct SavedContext {
    path: PathBuf,
    title: String,
    updated: SystemTime,
}

const CONTEXT_PREVIEW_BYTES: usize = 4 * 1024 * 1024;

fn read_context(path: &Path) -> anyhow::Result<String> {
    use std::io::Read;
    anyhow::ensure!(std::fs::symlink_metadata(path)?.is_file(), "Context must be a regular file");
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take((CONTEXT_PREVIEW_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > CONTEXT_PREVIEW_BYTES {
        bytes.truncate(CONTEXT_PREVIEW_BYTES);
        Ok(format!("{}\n\nPreview limited to 4 MiB. Open the context folder to read the complete file.", String::from_utf8_lossy(&bytes)))
    } else {
        Ok(String::from_utf8(bytes)?)
    }
}

fn list_context(folder: &Path) -> anyhow::Result<Vec<SavedContext>> {
    std::fs::create_dir_all(folder)?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "md") { continue; }
        if entry.file_type().is_ok_and(|kind| !kind.is_file()) { continue; }
        let title = match read_context(&path) {
            Ok(text) => text.lines().find_map(|line| line.strip_prefix("File: "))
                .unwrap_or_else(|| text.lines().find(|line| !line.trim().is_empty()).unwrap_or("Saved conversation"))
                .trim_start_matches('#').trim().chars().take(160).collect(),
            Err(_) => format!("Cannot read {}", entry.file_name().to_string_lossy()),
        };
        let updated = entry.metadata().and_then(|metadata| metadata.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        entries.push(SavedContext { path, title, updated });
    }
    entries.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.path.cmp(&b.path)));
    Ok(entries)
}

pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(view) = workspace.item_of_type::<ContextHistory>(cx) {
        view.update(cx, |view, cx| view.refresh(cx));
        workspace.activate_item(&view, true, true, window, cx);
    } else {
        let view = cx.new(|cx| {
            let mut view = ContextHistory { focus: cx.focus_handle(), entries: Vec::new(), selected: None, markdown: None, error: None, loading: false, task: Task::ready(()), page: 0 };
            view.refresh(cx);
            view
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }
}

struct ContextHistory {
    focus: FocusHandle,
    entries: Vec<SavedContext>,
    selected: Option<PathBuf>,
    markdown: Option<Entity<Markdown>>,
    error: Option<String>,
    loading: bool,
    task: Task<()>,
    page: usize,
}

impl ContextHistory {
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        self.selected = None;
        self.markdown = None;
        let folder = paths::data_dir().join("context");
        self.task = cx.spawn(async move |view, cx| {
            let result = cx.background_spawn(async move { list_context(&folder) }).await;
            view.update(cx, |view, cx| {
                view.loading = false;
                match result {
                    Ok(entries) => { view.entries = entries; view.page = view.page.min(view.entries.len().saturating_sub(1) / 50); }
                    Err(error) => view.error = Some(format!("Could not read context folder: {error}")),
                }
                cx.notify();
            }).log_err();
        });
        cx.notify();
    }

    fn select(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.selected = Some(path.clone());
        self.markdown = None;
        self.loading = true;
        self.error = None;
        self.task = cx.spawn(async move |view, cx| {
            let result = cx.background_spawn(async move {
                read_context(&path)
            }).await;
            view.update(cx, |view, cx| {
                view.loading = false;
                match result {
                    Ok(text) => view.markdown = Some(cx.new(|cx| Markdown::new(text.into(), None, None, cx))),
                    Err(error) => view.error = Some(format!("Could not open saved context: {error}")),
                }
                cx.notify();
            }).log_err();
        });
        cx.notify();
    }
}

impl Render for ContextHistory {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().min_h_0().track_focus(&self.focus)
            .child(h_flex().p_3().gap_3().border_b_1().border_color(cx.theme().colors().border_variant)
                .child(Label::new("Context history").size(LabelSize::Large))
                .child(Button::new("refresh-context", "Refresh").on_click(cx.listener(|view, _, _, cx| view.refresh(cx))))
                .child(Button::new("reveal-context", "Open folder").on_click(|_, _, cx| cx.reveal_path(&paths::data_dir().join("context")))))
            .child(div().px_3().pb_3().child(Label::new("Saved Markdown conversations across projects. Use Save context folder in the Agent Panel to add a conversation. Refresh to load newer snapshots.").color(Color::Muted)))
            .when_some(self.error.clone(), |view, error| view.child(div().p_3().child(Label::new(error).color(Color::Error))))
            .when(self.loading, |view| view.child(div().px_3().child(Label::new("Loading context…"))))
            .child(h_flex().flex_1().min_h_0().items_stretch()
                .child(v_flex().w(gpui::px(320.0)).flex_shrink_0().min_h_0().border_r_1().border_color(cx.theme().colors().border_variant)
                    .child(div().id("saved-context-list").flex_1().min_h_0().overflow_y_scroll().p_2()
                        .when(self.entries.is_empty(), |view| view.child(Label::new("No saved conversations yet.")))
                        .children(self.entries.iter().skip(self.page * 50).take(50).enumerate().map(|(index, entry)| {
                            let path = entry.path.clone();
                            let updated: chrono::DateTime<chrono::Local> = entry.updated.into();
                            v_flex().p_2().gap_1().border_b_1().border_color(cx.theme().colors().border_variant)
                                .child(Button::new(("saved-context", index), entry.title.clone()).when(self.selected.as_ref() == Some(&entry.path), |button| button.style(ui::ButtonStyle::Tinted(ui::TintColor::Accent))).truncate(true)
                                    .on_click(cx.listener(move |view, _, _, cx| view.select(path.clone(), cx))))
                                .child(Label::new(updated.format("%b %d, %Y %H:%M").to_string()).color(Color::Muted))
                        })))
                    .child(h_flex().p_2().gap_2()
                        .child(Button::new("previous-context", "Previous").disabled(self.page == 0).on_click(cx.listener(|view, _, _, cx| { view.page = view.page.saturating_sub(1); cx.notify(); })))
                        .child(Label::new(format!("{} / {}", self.page + 1, self.entries.len().div_ceil(50).max(1))))
                        .child(Button::new("next-context", "Next").disabled((self.page + 1) * 50 >= self.entries.len()).on_click(cx.listener(|view, _, _, cx| { view.page += 1; cx.notify(); })))))
                .child(div().id("saved-context-reader").flex_1().min_w_0().min_h_0().overflow_y_scroll().p_4()
                    .when(self.markdown.is_none() && !self.loading, |view| view.child(Label::new("Select a conversation to read its context and responses.")))
                    .when_some(self.markdown.clone(), |view, markdown| view.child(MarkdownElement::new(markdown, markdown::MarkdownStyle { base_text_style: window.text_style(), selection_background_color: cx.theme().colors().element_selection_background, soft_break_as_hard_break: true, ..Default::default() })))))
    }
}
impl Focusable for ContextHistory { fn focus_handle(&self, _: &App) -> FocusHandle { self.focus.clone() } }
impl EventEmitter<()> for ContextHistory {}
impl Item for ContextHistory {
    type Event = ();
    fn to_item_events(_: &(), _: &mut dyn FnMut(ItemEvent)) {}
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString { "Context history".into() }
    fn telemetry_event_text(&self) -> Option<&'static str> { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_keeps_large_and_invalid_files_visible() {
        let folder = std::env::temp_dir().join(format!("hulder-history-errors-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("folder");
        std::fs::write(folder.join("valid.md"), "# Valid").expect("valid");
        std::fs::write(folder.join("invalid.md"), [0xff]).expect("invalid");
        std::fs::write(folder.join("large.md"), "a".repeat(CONTEXT_PREVIEW_BYTES + 1)).expect("large");
        assert_eq!(list_context(&folder).expect("list").len(), 3);
        assert!(read_context(&folder.join("invalid.md")).is_err());
        assert!(read_context(&folder.join("large.md")).expect("preview").contains("Preview limited"));
        std::fs::remove_dir_all(folder).expect("cleanup");
    }
    #[test]
    fn history_reads_saved_markdown_and_ignores_other_files() {
        let folder = std::env::temp_dir().join(format!("hulder-history-{}-{}", std::process::id(), SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("clock").as_nanos()));
        assert!(list_context(&folder).expect("empty folder").is_empty());
        std::fs::write(folder.join("one.md"), "# Context\nFile: src/main.go\n## Assistant\nA condition").expect("fixture");
        std::fs::write(folder.join("ignore.json"), "{}").expect("fixture");
        #[cfg(unix)]
        std::os::unix::fs::symlink(folder.join("one.md"), folder.join("link.md")).expect("symlink");
        let entries = list_context(&folder).expect("history");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "src/main.go");
        assert_eq!(list_context(&folder).expect("reopen").len(), 1);
        std::fs::remove_dir_all(folder).expect("cleanup");
    }
}
