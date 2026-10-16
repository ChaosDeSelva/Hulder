//! A workspace tab for `.pdf` files: pages rasterized natively, scrolled
//! vertically, zoomed with cmd-+/-, with the text one action away.

mod pdf_document;
mod pdf_text;

use std::{
    io::Read as _,
    ops::Range,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, Result};
use collections::HashMap;
use editor::Editor;
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ImageSource, InteractiveElement, IntoElement, ParentElement, Pixels, Point, Render,
    RenderImage, ScrollHandle, StatefulInteractiveElement, Styled, Task, WeakEntity, Window,
    actions, canvas, div, img, px,
};
use language::File as _;
use settings::Settings as _;
use project::{Project, ProjectEntryId, ProjectPath};
use smallvec::SmallVec;
use ui::{Label, LabelCommon as _, prelude::*};
use util::ResultExt as _;
use workspace::{
    ItemSettings, Pane, Toast, Workspace,
    invalid_item_view::InvalidItemView,
    item::{Item, ItemEvent, ProjectItem, TabContentParams},
    notifications::{NotificationId, NotifyResultExt as _, NotifyTaskExt as _},
};

pub use pdf_document::PdfDocument;
use pdf_document::RenderedPage;
use pdf_text::{
    CLIPPING_TEXT_BYTES, MAX_CACHED_PAGES, PAGE_GAP, PAGE_MARGIN, ZOOM_STEP, clamp_zoom,
    current_page, fit_width_zoom, lru_evictions, max_zoom_for_pages, page_layout,
    pages_to_markdown, scale_key, spacer_heights, truncate_for_clipping, visible_pages,
};

actions!(
    pdf_viewer,
    [
        /// Zooms the PDF in.
        ZoomIn,
        /// Zooms the PDF out.
        ZoomOut,
        /// Fits pages to the tab width (the default).
        FitWidth,
        /// Shows pages at one point per pixel.
        ResetZoom,
        /// Copies the text of the page being read.
        CopyPageText,
        /// Copies the text of every page.
        CopyDocumentText,
        /// Opens the document's text as a read-only Markdown buffer, one heading per page.
        OpenAsMarkdown
    ]
);

/// Pages rendered around the viewport in addition to the visible ones.
const PAGE_LOOKAHEAD: usize = 1;

pub fn init(cx: &mut App) {
    workspace::register_project_item::<PdfView>(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenAsMarkdown, window, cx| {
            if let Some(view) = workspace.active_item_as::<PdfView>(cx) {
                open_as_markdown(workspace, &view, window, cx);
            }
        });
    })
    .detach();
}

/// The viewer reads files itself, so it only ever opens local worktrees.
fn ensure_local(is_local: bool, path: &ProjectPath) -> Result<()> {
    if !is_local {
        anyhow::bail!(
            "{} is on a remote project; PDF preview only opens local files",
            path.path.as_unix_str()
        );
    }
    Ok(())
}

fn is_pdf_path(path: &ProjectPath) -> bool {
    path.path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

/// The loaded document behind one or more tabs.
pub struct PdfItem {
    file: Arc<project::File>,
    document: Arc<PdfDocument>,
    texts: Option<Arc<Vec<String>>>,
}

impl PdfItem {
    pub fn project_path(&self, cx: &App) -> ProjectPath {
        ProjectPath {
            worktree_id: self.file.worktree_id(cx),
            path: self.file.path.clone(),
        }
    }

    pub fn abs_path(&self, cx: &App) -> Option<std::path::PathBuf> {
        Some(self.file.as_local()?.abs_path(cx))
    }

    pub fn document(&self) -> &Arc<PdfDocument> {
        &self.document
    }
}

impl project::ProjectItem for PdfItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        if !is_pdf_path(path) {
            return None;
        }
        let project = project.clone();
        let path = path.clone();
        Some(cx.spawn(async move |cx| {
            // Only a local worktree entry is read, through the project's own
            // Fs, and only its bytes reach CoreGraphics and PDFKit; no path or
            // URL from anywhere else is ever handed to the frameworks.
            let (fs, file, abs_path) = project.read_with(cx, |project, cx| {
                let worktree = project
                    .worktree_for_id(path.worktree_id, cx)
                    .context("worktree for PDF path")?;
                ensure_local(project.is_local() && worktree.read(cx).is_local(), &path)?;
                let entry = worktree
                    .read(cx)
                    .entry_for_path(&path.path)
                    .cloned()
                    .with_context(|| format!("{} is not in the project", path.path.as_unix_str()))?;
                let abs_path = worktree.read(cx).absolutize(&entry.path);
                anyhow::Ok((
                    project.fs().clone(),
                    project::File::for_entry(entry, worktree),
                    abs_path,
                ))
            })?;
            let name = abs_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // The size is checked before anything is read and again on the
            // bytes actually read, capped one past the limit, so a file that
            // grows after the stat cannot fill memory.
            let metadata = fs
                .metadata(&abs_path)
                .await?
                .with_context(|| format!("{name} does not exist"))?;
            pdf_text::reject_oversized(metadata.len as usize, &name)?;
            let reader = fs.open_sync_nofollow(&abs_path).await?;
            let document = cx
                .background_spawn(async move {
                    let mut bytes = Vec::new();
                    reader
                        .take(pdf_text::MAX_PDF_BYTES as u64 + 1)
                        .read_to_end(&mut bytes)
                        .with_context(|| format!("reading {name}"))?;
                    pdf_document::load(bytes, &name)
                })
                .await?;
            anyhow::Ok(cx.new(|_| PdfItem {
                file,
                document: Arc::new(document),
                texts: None,
            }))
        }))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.file.entry_id
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        Some(self.project_path(cx))
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Zoom {
    FitWidth,
    Fixed(f32),
}

/// Flags a background render as pointless once its slot is evicted, so a
/// fast scroll does not pile up drawing work for pages no longer wanted.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

enum PageSlot {
    Rendering {
        _task: Task<()>,
        _cancel: CancelOnDrop,
    },
    Ready(Arc<RenderImage>),
    /// Remembers where the scroll was when the render failed; moving away
    /// and back gives the page another try.
    Failed { scroll_top: f32 },
}

impl PageSlot {
    fn ready_image(&self) -> Option<Arc<RenderImage>> {
        match self {
            PageSlot::Ready(image) => Some(image.clone()),
            _ => None,
        }
    }
}

type PageKey = (usize, u32);

/// Frees the GPU texture of every `Ready` slot removed from the cache.
fn drop_slot_images(slots: impl IntoIterator<Item = PageSlot>, cx: &mut App) {
    for slot in slots {
        if let Some(image) = slot.ready_image() {
            cx.drop_image(image, None);
        }
    }
}

pub struct PdfView {
    item: Entity<PdfItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    zoom: Zoom,
    viewport: Option<Bounds<Pixels>>,
    scale_factor: f32,
    scroll_handle: ScrollHandle,
    /// Bitmaps keyed by (page, rasterization scale key), at most
    /// `MAX_CACHED_PAGES` of them; `recency` orders the keys oldest first.
    pages: HashMap<PageKey, PageSlot>,
    recency: Vec<PageKey>,
}

impl PdfView {
    pub fn new(item: Entity<PdfItem>, project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        cx.on_release(|this, cx| {
            let slots: Vec<PageSlot> = this.pages.drain().map(|(_, slot)| slot).collect();
            drop_slot_images(slots, cx);
        })
        .detach();
        Self {
            item,
            project,
            focus_handle: cx.focus_handle(),
            zoom: Zoom::FitWidth,
            viewport: None,
            scale_factor: 1.0,
            scroll_handle: ScrollHandle::new(),
            pages: HashMap::default(),
            recency: Vec::new(),
        }
    }

    fn document(&self, cx: &App) -> Arc<PdfDocument> {
        self.item.read(cx).document.clone()
    }

    /// The zoom actually drawn: the requested one, held under the bitmap cap.
    fn effective_zoom(&self, cx: &App) -> f32 {
        let document = self.document(cx);
        let requested = match self.zoom {
            Zoom::Fixed(zoom) => zoom,
            Zoom::FitWidth => {
                let width = self.viewport.map_or(0.0, |bounds| f32::from(bounds.size.width));
                let page_width = document
                    .page_sizes()
                    .first()
                    .map_or(0.0, |(width, _)| *width);
                fit_width_zoom(width, page_width)
            }
        };
        requested.min(max_zoom_for_pages(document.page_sizes(), self.scale_factor))
    }

    fn touch_page(&mut self, key: PageKey) {
        self.recency.retain(|existing| *existing != key);
        self.recency.push(key);
    }

    /// Drops the least recently used bitmaps over the cap, never one of the
    /// pages being drawn: evicting those would re-request them next frame.
    fn evict_stale_pages(&mut self, wanted: &Range<usize>, cx: &mut App) {
        let evicted = lru_evictions(&self.recency, MAX_CACHED_PAGES, |(page, _)| {
            wanted.contains(page)
        });
        let slots: Vec<PageSlot> = evicted
            .iter()
            .filter_map(|key| self.pages.remove(key))
            .collect();
        drop_slot_images(slots, cx);
        self.recency
            .retain(|key| self.pages.contains_key(key));
    }

    /// Clears every bitmap drawn at another scale after a zoom change.
    fn drop_pages_at_other_scales(&mut self, key: u32, cx: &mut App) {
        let stale: Vec<PageKey> = self
            .pages
            .keys()
            .filter(|(_, page_key)| *page_key != key)
            .copied()
            .collect();
        let slots: Vec<PageSlot> = stale
            .iter()
            .filter_map(|key| self.pages.remove(key))
            .collect();
        drop_slot_images(slots, cx);
    }

    fn set_zoom(&mut self, zoom: Zoom, cx: &mut Context<Self>) {
        let previous = self.effective_zoom(cx);
        self.zoom = zoom;
        let next = self.effective_zoom(cx);
        if previous > 0.0 && next != previous {
            // Keep the same document position under the viewport's top edge.
            let offset = self.scroll_handle.offset();
            self.scroll_handle.set_offset(Point {
                x: offset.x,
                y: offset.y * (next / previous),
            });
        }
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        let zoom = clamp_zoom(self.effective_zoom(cx) * ZOOM_STEP);
        self.set_zoom(Zoom::Fixed(zoom), cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        let zoom = clamp_zoom(self.effective_zoom(cx) / ZOOM_STEP);
        self.set_zoom(Zoom::Fixed(zoom), cx);
    }

    fn fit_width(&mut self, _: &FitWidth, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(Zoom::FitWidth, cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(Zoom::Fixed(1.0), cx);
    }

    /// Extracted page texts, computed once per document in the background.
    fn texts(&mut self, cx: &mut Context<Self>) -> Task<Result<Arc<Vec<String>>>> {
        if let Some(texts) = self.item.read(cx).texts.clone() {
            return Task::ready(Ok(texts));
        }
        let document = self.document(cx);
        let item = self.item.clone();
        cx.spawn(async move |_, cx| {
            let texts = cx
                .background_spawn(async move { document.page_texts() })
                .await?;
            let texts = Arc::new(texts);
            item.update(cx, |item, _| item.texts = Some(texts.clone()));
            anyhow::Ok(texts)
        })
    }

    fn current_page_index(&self, cx: &App) -> usize {
        let zoom = self.effective_zoom(cx);
        let layout = page_layout(self.document(cx).page_sizes(), zoom);
        let viewport_height = self.viewport.map_or(0.0, |bounds| f32::from(bounds.size.height));
        current_page(&layout, -f32::from(self.scroll_handle.offset().y), viewport_height)
    }

    fn copy_page_text(&mut self, _: &CopyPageText, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.current_page_index(cx);
        self.copy_text(window, cx, move |texts| texts.get(page).cloned().unwrap_or_default());
    }

    fn copy_document_text(
        &mut self,
        _: &CopyDocumentText,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_text(window, cx, |texts| texts.join("\n\n"));
    }

    fn copy_text(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
        select: impl FnOnce(&[String]) -> String + 'static,
    ) {
        let texts = self.texts(cx);
        cx.spawn(async move |_, cx| {
            let result = texts.await.map(|texts| select(&texts));
            cx.update(|cx| match result {
                Ok(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                Err(error) => {
                    Err::<(), _>(error).notify_app_err(cx);
                }
            });
        })
        .detach();
    }

    fn ensure_page_rendered(
        &mut self,
        page: usize,
        scale: f32,
        key: u32,
        scroll_top: f32,
        cx: &mut Context<Self>,
    ) {
        self.touch_page((page, key));
        match self.pages.get(&(page, key)) {
            Some(PageSlot::Failed { scroll_top: failed_at }) if *failed_at != scroll_top => {}
            Some(_) => return,
            None => {}
        }
        let document = self.document(cx);
        let cancelled = Arc::new(AtomicBool::new(false));
        let task = cx.spawn({
            let cancelled = cancelled.clone();
            async move |this, cx| {
                let rendered = cx
                    .background_spawn(async move {
                        if cancelled.load(Ordering::Relaxed) {
                            anyhow::bail!("render cancelled");
                        }
                        document.render_page(page, scale)
                    })
                    .await;
                this.update(cx, |this, cx| {
                    // The slot may have been evicted while drawing; do not resurrect it.
                    if !matches!(this.pages.get(&(page, key)), Some(PageSlot::Rendering { .. })) {
                        return;
                    }
                    let slot = match rendered.and_then(render_image_from_page) {
                        Ok(image) => PageSlot::Ready(Arc::new(image)),
                        Err(error) => {
                            log::error!("rendering PDF page {}: {error:#}", page + 1);
                            PageSlot::Failed { scroll_top }
                        }
                    };
                    this.pages.insert((page, key), slot);
                    cx.notify();
                })
                .log_err();
            }
        });
        self.pages.insert(
            (page, key),
            PageSlot::Rendering {
                _task: task,
                _cancel: CancelOnDrop(cancelled),
            },
        );
    }

    fn render_pages(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let document = self.document(cx);
        let page_sizes = document.page_sizes().to_vec();
        self.scale_factor = window.scale_factor();
        let zoom = self.effective_zoom(cx);
        let scale = zoom * self.scale_factor;
        let key = scale_key(scale);
        self.drop_pages_at_other_scales(key, cx);

        let layout = page_layout(&page_sizes, zoom);
        let viewport_height = self.viewport.map_or(0.0, |bounds| f32::from(bounds.size.height));
        let scroll_top = -f32::from(self.scroll_handle.offset().y);
        let wanted = visible_pages(&layout, scroll_top, viewport_height, PAGE_LOOKAHEAD);
        for page in wanted.clone() {
            self.ensure_page_rendered(page, scale, key, scroll_top, cx);
        }
        self.evict_stale_pages(&wanted, cx);

        let border = cx.theme().colors().border;
        let (before, after) = spacer_heights(&layout, wanted.clone());
        let spacer = |height: f32| (height > 0.0).then(|| div().flex_none().w_full().h(px(height)));
        let pages = page_sizes
            .iter()
            .enumerate()
            .skip(wanted.start)
            .take(wanted.len())
            .map(|(index, (width, height))| {
                let image = self
                    .pages
                    .get(&(index, key))
                    .and_then(PageSlot::ready_image);
                div()
                    .flex_none()
                    .w(px(width * zoom))
                    .h(px(height * zoom))
                    .bg(gpui::white())
                    .border_1()
                    .border_color(border)
                    .when_some(image, |page, image| {
                        page.child(img(ImageSource::Render(image)).size_full())
                    })
            });

        div()
            .id("pdf-pages")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .child(
                v_flex()
                    .w_full()
                    .items_center()
                    .py(px(PAGE_GAP))
                    .px(px(PAGE_MARGIN))
                    .gap(px(PAGE_GAP))
                    .children(spacer(before))
                    .children(pages)
                    .children(spacer(after)),
            )
            .into_any_element()
    }
}

fn render_image_from_page(page: RenderedPage) -> Result<RenderImage> {
    let buffer = image::ImageBuffer::from_raw(page.width, page.height, page.bgra)
        .context("page bitmap has the wrong length")?;
    Ok(RenderImage::new(SmallVec::from_elem(
        image::Frame::new(buffer),
        1,
    )))
}

fn open_as_markdown(
    workspace: &mut Workspace,
    view: &Entity<PdfView>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let texts = view.update(cx, |view, cx| view.texts(cx));
    let project = workspace.project().clone();
    let title = view.read(cx).tab_content_text(0, cx);
    let title = format!("{title} (text)");
    let source = view.read(cx).item.read(cx).abs_path(cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let texts = texts.await?;
        let markdown = pages_to_markdown(&texts);
        if let Some(source) = &source
            && save_clipping(&workspace, source, &markdown, cx).await?
        {
            return Ok(());
        }
        let languages = project.read_with(cx, |project, _| project.languages().clone());
        let language = languages.language_for_name("Markdown").await.ok();
        let buffer = project
            .update(cx, |project, cx| project.create_buffer(language, false, cx))
            .await?;
        workspace.update_in(cx, |workspace, window, cx| {
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, markdown)], None, cx);
            });
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_buffer(buffer, Some(project.clone()), window, cx);
                editor.set_read_only(true);
                editor
                    .buffer()
                    .update(cx, |buffer, cx| buffer.set_title(title, cx));
                editor
            });
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
        })
    })
    .detach_and_notify_err(workspace.weak_handle(), window, cx);
}

struct PdfClippingToast;

fn toast(workspace: &WeakEntity<Workspace>, message: String, cx: &mut gpui::AsyncWindowContext) {
    workspace
        .update(cx, |workspace, cx| {
            let toast = Toast::new(NotificationId::unique::<PdfClippingToast>(), message).autohide();
            workspace.show_toast(toast, cx);
        })
        .log_err();
}

/// Saves the document text as a clipping (spec section 13) and opens that
/// file. `false` means the scratch buffer should open instead: clippings are
/// off, there is no local worktree, or the save failed (toasted, so the text
/// still reaches the user). The text is cut to the clipping cap with a notice:
/// the viewer keeps more text than a clipping may hold.
async fn save_clipping(
    workspace: &WeakEntity<Workspace>,
    source: &Path,
    markdown: &str,
    cx: &mut gpui::AsyncWindowContext,
) -> Result<bool> {
    let (markdown, truncated) =
        truncate_for_clipping(markdown.to_string(), CLIPPING_TEXT_BYTES);
    let draft = clippings::ClippingDraft::text(
        clippings::Kind::Pdf,
        source.to_string_lossy(),
        markdown,
    );
    match clippings::save_and_open(workspace.clone(), draft, cx).await {
        Ok(Some(_)) => {
            if truncated {
                toast(
                    workspace,
                    "Clipping saved with the text cut to the 1 MB limit".to_string(),
                    cx,
                );
            }
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(error) => {
            log::warn!("pdf_viewer: {error}");
            toast(workspace, error.user_message(), cx);
            Ok(false)
        }
    }
}

pub enum PdfViewEvent {}

impl EventEmitter<PdfViewEvent> for PdfView {}

impl Focusable for PdfView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if PdfDocument::SUPPORTED {
            self.render_pages(window, cx)
        } else {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new("PDF preview is macOS only").color(Color::Muted))
                .into_any_element()
        };
        let view = cx.entity().downgrade();
        div()
            .track_focus(&self.focus_handle)
            .key_context("PdfViewer")
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::fit_width))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::copy_page_text))
            .on_action(cx.listener(Self::copy_document_text))
            .relative()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            // Measures the tab so fit-width and page visibility use real bounds;
            // the first frame paints with none and the measurement triggers the next.
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        view.update(cx, |view, cx| {
                            if view.viewport != Some(bounds) {
                                view.viewport = Some(bounds);
                                cx.notify();
                            }
                        })
                        .log_err();
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(content)
    }
}

impl Item for PdfView {
    type Event = PdfViewEvent;

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.item.entity_id(), self.item.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let path = self.item.read(cx).abs_path(cx)?;
        Some(path.to_string_lossy().into_owned().into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(params.text_color())
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.item.read(cx).file.file_name(cx).to_string().into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.item.read(cx).abs_path(cx)?;
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let item = self.item.clone();
        let project = self.project.clone();
        let zoom = self.zoom;
        Task::ready(Some(cx.new(|cx| {
            let mut view = PdfView::new(item, project, cx);
            view.zoom = zoom;
            view
        })))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.item.read(cx).file.disk_state().is_deleted()
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

impl ProjectItem for PdfView {
    type Item = PdfItem;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, project, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        error: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView> {
        Some(InvalidItemView::new(abs_path, is_local, error, window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::FakeFs;
    use settings::SettingsStore;
    use util::rel_path::rel_path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    #[test]
    fn remote_worktrees_are_refused() {
        let path = ProjectPath {
            worktree_id: project::WorktreeId::from_usize(1),
            path: rel_path("docs/paper.pdf").into(),
        };
        assert!(ensure_local(true, &path).is_ok());
        let error = ensure_local(false, &path).expect_err("remote must be refused");
        assert!(error.to_string().contains("docs/paper.pdf"), "{error}");
        assert!(error.to_string().contains("local"), "{error}");
    }

    #[gpui::test]
    async fn opens_only_pdfs_from_the_project_fs(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/root"),
            serde_json::json!({ "notes.txt": "plain", "huge.pdf": "" }),
        )
        .await;
        fs.insert_file("/root/tiny.pdf", pdf_document::tests::minimal_pdf())
            .await;
        fs.insert_file("/root/huge.pdf", vec![b'%'; pdf_text::MAX_PDF_BYTES + 1])
            .await;
        let project = Project::test(fs, [Path::new("/root")], cx).await;
        let worktree_id = cx.update(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("test project should contain a worktree")
                .read(cx)
                .id()
        });
        let project_path = |name: &str| ProjectPath {
            worktree_id,
            path: rel_path(name).into(),
        };

        assert!(
            cx.update(|cx| <PdfItem as project::ProjectItem>::try_open(
                &project,
                &project_path("notes.txt"),
                cx
            ))
            .is_none(),
            "non-PDF paths must be left to other items"
        );

        let oversized = cx
            .update(|cx| <PdfItem as project::ProjectItem>::try_open(
                &project,
                &project_path("huge.pdf"),
                cx
            ))
            .expect("pdf path is claimed")
            .await
            .expect_err("oversized file must be refused");
        assert!(oversized.to_string().contains("up to 200 MB"), "{oversized}");

        let opened = cx
            .update(|cx| <PdfItem as project::ProjectItem>::try_open(
                &project,
                &project_path("tiny.pdf"),
                cx
            ))
            .expect("pdf path is claimed")
            .await;
        if PdfDocument::SUPPORTED {
            let item = opened.expect("tiny PDF should open");
            cx.update(|cx| {
                let item = item.read(cx);
                assert_eq!(item.document.page_count(), 1);
                assert_eq!(item.project_path(cx), project_path("tiny.pdf"));
            });
        }
    }

    #[test]
    fn only_pdf_extensions_open_here() {
        let path = |name: &str| ProjectPath {
            worktree_id: project::WorktreeId::from_usize(1),
            path: rel_path(name).into(),
        };
        assert!(is_pdf_path(&path("paper.pdf")));
        assert!(is_pdf_path(&path("dir/PAPER.PDF")));
        assert!(!is_pdf_path(&path("paper.pdf.txt")));
        assert!(!is_pdf_path(&path("pdf")));
    }
}
