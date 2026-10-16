use crate::model::{
    GLOSSES_DIR, Gloss, GlossParseError, LineRange, MAX_GLOSSES_PER_FILE, gloss_file_path,
    gloss_file_rel_path, parse_gloss_file, serialize_gloss_file, source_path_for_gloss_file,
};
use anyhow::{Context as _, Result, bail};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, BackgroundExecutor, Context, Entity, EventEmitter, Global, Subscription,
    Task, WeakEntity,
};
use lru::LruCache;
use path::rel_path::RelPath;
use project::{Project, ProjectPath, WorktreeId};
use std::{
    io::Read as _,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};
use util::ResultExt as _;

struct GlobalGlossStore(Entity<GlossStore>);
impl Global for GlobalGlossStore {}

pub fn init(cx: &mut App) {
    let store = cx.new(GlossStore::new);
    cx.set_global(GlobalGlossStore(store));
    cx.observe_new::<Project>(|_, _window, cx| {
        let project = cx.entity();
        if let Some(store) = GlossStore::try_global(cx) {
            store.update(cx, |store, cx| store.register_project(project, cx));
        }
    })
    .detach();
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlossStoreEvent {
    Changed {
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
    },
}

/// Most files whose glosses stay cached; the least recently asked for are dropped first.
pub const CACHE_CAPACITY: usize = 256;

/// What the store knows about one file's glosses once its gloss file has been read.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LoadedGlosses {
    pub glosses: Vec<Gloss>,
    /// Sections of the gloss file that could not be parsed.
    pub errors: Vec<GlossParseError>,
    /// Glosses past [`MAX_GLOSSES_PER_FILE`] that were dropped from `glosses`.
    pub hidden: usize,
}

impl LoadedGlosses {
    pub fn is_empty(&self) -> bool {
        self.glosses.is_empty() && self.errors.is_empty() && self.hidden == 0
    }

    fn from_text(text: &str, gloss_path: &Path) -> Self {
        let parsed = parse_gloss_file(text);
        let mut glosses = parsed.glosses;
        let hidden = glosses.len().saturating_sub(MAX_GLOSSES_PER_FILE);
        if hidden > 0 {
            log::warn!(
                "{gloss_path:?} has {} glosses; only the first {MAX_GLOSSES_PER_FILE} are shown",
                glosses.len()
            );
            glosses.truncate(MAX_GLOSSES_PER_FILE);
        }
        for error in &parsed.errors {
            log::warn!("{gloss_path:?}: {error}");
        }
        Self {
            glosses,
            errors: parsed.errors,
            hidden,
        }
    }
}

enum CacheEntry {
    // The task is held so dropping the entry cancels an in-flight load.
    Loading(#[allow(dead_code)] Task<()>),
    Loaded(Arc<LoadedGlosses>),
}

/// Cache of gloss files, loaded lazily and dropped when the worktree reports a change to
/// the file on disk.
pub struct GlossStore {
    projects: Vec<WeakEntity<Project>>,
    cache: LruCache<(WorktreeId, Arc<RelPath>), CacheEntry>,
    generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<GlossStoreEvent> for GlossStore {}

impl GlossStore {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            projects: Vec::new(),
            cache: LruCache::new(
                NonZeroUsize::new(CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN),
            ),
            generation: 0,
            _subscriptions: Vec::new(),
        }
    }

    #[cfg(test)]
    fn cached_file_count(&self) -> usize {
        self.cache.len()
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalGlossStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalGlossStore>()
            .map(|global| global.0.clone())
    }

    /// Increases every time cached glosses change; cheap to compare for render caches.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn register_project(&mut self, project: Entity<Project>, cx: &mut Context<Self>) {
        self.projects
            .retain(|existing| existing.upgrade().is_some());
        if self
            .projects
            .iter()
            .any(|existing| existing.entity_id() == project.entity_id())
        {
            return;
        }
        self._subscriptions
            .push(cx.subscribe(&project, Self::handle_project_event));
        self.projects.push(project.downgrade());
    }

    fn handle_project_event(
        &mut self,
        _project: Entity<Project>,
        event: &project::Event,
        cx: &mut Context<Self>,
    ) {
        match event {
            project::Event::WorktreeUpdatedEntries(worktree_id, changes) => {
                for (path, _, _) in changes.iter() {
                    let Some(source) = source_path_for_gloss_file(path) else {
                        continue;
                    };
                    let source: Arc<RelPath> = source.as_rel_path().into_arc();
                    let was_cached = self.cache.pop(&(*worktree_id, source.clone())).is_some();
                    if was_cached {
                        self.mark_changed(*worktree_id, source, cx);
                    }
                }
            }
            project::Event::WorktreeRemoved(worktree_id) => {
                let removed: Vec<_> = self
                    .cache
                    .iter()
                    .filter(|((cached_worktree_id, _), _)| cached_worktree_id == worktree_id)
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in removed {
                    self.cache.pop(&key);
                }
            }
            _ => {}
        }
    }

    fn mark_changed(
        &mut self,
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        cx: &mut Context<Self>,
    ) {
        self.generation += 1;
        cx.emit(GlossStoreEvent::Changed { worktree_id, path });
        cx.notify();
    }

    fn worktree_root(&self, worktree_id: WorktreeId, cx: &App) -> Option<(Arc<Path>, Arc<dyn Fs>)> {
        self.projects.iter().find_map(|project| {
            let project = project.upgrade()?;
            let project = project.read(cx);
            let worktree = project.worktree_for_id(worktree_id, cx)?;
            Some((worktree.read(cx).abs_path(), project.fs().clone()))
        })
    }

    /// Worktree-relative location of the gloss file describing `path`.
    pub fn gloss_file_project_path(path: &ProjectPath) -> Option<ProjectPath> {
        let gloss_file = gloss_file_rel_path(&path.path).log_err()?;
        Some(ProjectPath {
            worktree_id: path.worktree_id,
            path: gloss_file.as_rel_path().into_arc(),
        })
    }

    /// Glosses of one file. The first call for a file starts an async load and returns an
    /// empty result; the store emits [`GlossStoreEvent::Changed`] for the file when a load
    /// lands with anything to show (a load that finds nothing is silent).
    pub fn glosses_for(&mut self, path: &ProjectPath, cx: &mut Context<Self>) -> Arc<LoadedGlosses> {
        let key = (path.worktree_id, path.path.clone());
        if let Some(entry) = self.cache.get(&key) {
            return match entry {
                CacheEntry::Loaded(loaded) => loaded.clone(),
                CacheEntry::Loading(_) => Arc::default(),
            };
        }

        let Some((root, fs)) = self.worktree_root(path.worktree_id, cx) else {
            return Arc::default();
        };
        if gloss_file_path(&root, &path.path).log_err().is_none() {
            return Arc::default();
        }

        let executor = cx.background_executor().clone();
        let task = cx.spawn({
            let path = path.clone();
            async move |this, cx| {
                let loaded = async {
                    let gloss_path = checked_gloss_path(fs.as_ref(), &root, &path.path).await?;
                    load_gloss_file(&fs, &executor, &gloss_path).await
                }
                .await
                .with_context(|| format!("loading glosses for {:?}", path.path))
                .log_err()
                .unwrap_or_default();
                this.update(cx, |this, cx| {
                    let has_content = !loaded.is_empty();
                    this.cache.put(
                        (path.worktree_id, path.path.clone()),
                        CacheEntry::Loaded(Arc::new(loaded)),
                    );
                    if has_content {
                        this.mark_changed(path.worktree_id, path.path, cx);
                    }
                })
                .ok();
            }
        });
        self.cache.put(key, CacheEntry::Loading(task));
        Arc::default()
    }

    /// Appends a gloss to the file for `path` and writes it to disk.
    pub fn add_gloss(
        &mut self,
        path: ProjectPath,
        lines: Option<LineRange>,
        anchor: String,
        body: String,
        author: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some((root, fs)) = self.worktree_root(path.worktree_id, cx) else {
            return Task::ready(Err(anyhow::anyhow!(
                "no worktree {:?} registered with the gloss store",
                path.worktree_id
            )));
        };
        if let Err(error) = gloss_file_path(&root, &path.path) {
            return Task::ready(Err(error));
        }
        let gloss = Gloss {
            lines,
            anchor: anchor.trim().to_string(),
            created: chrono::Utc::now(),
            author,
            body,
        };
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let gloss_path = checked_gloss_path(fs.as_ref(), &root, &path.path).await?;
            let mut loaded = load_gloss_file(&fs, &executor, &gloss_path)
                .await
                .with_context(|| format!("loading gloss file {gloss_path:?}"))?;
            if !loaded.errors.is_empty() {
                bail!(
                    "{gloss_path:?} has {} unreadable section(s); fix the file before adding to it",
                    loaded.errors.len()
                );
            }
            if loaded.hidden > 0 {
                bail!("{gloss_path:?} already has more than {MAX_GLOSSES_PER_FILE} glosses");
            }
            loaded.glosses.push(gloss);
            let text = serialize_gloss_file(&loaded.glosses);
            if text.len() as u64 > MAX_GLOSS_FILE_BYTES {
                bail!(
                    "gloss file {gloss_path:?} would be over the {MAX_GLOSS_FILE_BYTES} byte limit"
                );
            }
            if let Some(parent) = gloss_path.parent() {
                fs.create_dir(parent)
                    .await
                    .with_context(|| format!("creating {parent:?}"))?;
            }
            // Re-check every directory after creating them, right before the write: the
            // rename behind `atomic_write` cannot follow a link at the final component,
            // but a link swapped in for a parent directory would carry the file outside.
            let gloss_path = checked_gloss_path(fs.as_ref(), &root, &path.path).await?;
            fs.atomic_write(gloss_path.clone(), text)
                .await
                .with_context(|| format!("writing gloss file {gloss_path:?}"))?;
            this.update(cx, |this, cx| {
                this.cache.put(
                    (path.worktree_id, path.path.clone()),
                    CacheEntry::Loaded(Arc::new(loaded)),
                );
                this.mark_changed(path.worktree_id, path.path, cx);
            })
        })
    }

    /// Every gloss file in the worktree with its parsed glosses, sorted by source path.
    /// Reads the directory directly so ignored or unscanned `.hulder` folders still list.
    pub fn all_for_worktree(
        &self,
        worktree_id: WorktreeId,
        cx: &App,
    ) -> Task<Result<Vec<(Arc<RelPath>, Vec<Gloss>)>>> {
        self.all_for_worktree_with_limits(
            worktree_id,
            MAX_GLOSS_FILES_PER_WORKTREE,
            MAX_GLOSS_BYTES_PER_WORKTREE,
            cx,
        )
    }

    fn all_for_worktree_with_limits(
        &self,
        worktree_id: WorktreeId,
        max_files: usize,
        max_bytes: u64,
        cx: &App,
    ) -> Task<Result<Vec<(Arc<RelPath>, Vec<Gloss>)>>> {
        let Some((root, fs)) = self.worktree_root(worktree_id, cx) else {
            return Task::ready(Err(anyhow::anyhow!(
                "no worktree {worktree_id:?} registered with the gloss store"
            )));
        };
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let glosses_dir = root.join(GLOSSES_DIR);
            refuse_symlink(fs.as_ref(), &root.join(".hulder")).await?;
            let mut results = Vec::new();
            let mut bytes_read = 0u64;
            let mut pending = vec![glosses_dir.clone()];
            'walk: while let Some(directory) = pending.pop() {
                let Some(metadata) = fs.metadata(&directory).await? else {
                    continue;
                };
                if metadata.is_symlink || !metadata.is_dir {
                    continue;
                }
                let mut entries = fs.read_dir(&directory).await?;
                while let Some(entry) = entries.next().await {
                    let entry = entry?;
                    let Some(metadata) = fs.metadata(&entry).await? else {
                        continue;
                    };
                    if metadata.is_symlink {
                        log::warn!("skipping symlinked gloss path {entry:?}");
                        continue;
                    }
                    if metadata.is_dir {
                        pending.push(entry);
                        continue;
                    }
                    let relative = entry
                        .strip_prefix(&*root)
                        .with_context(|| format!("{entry:?} is outside {root:?}"))?;
                    let relative = RelPath::new(relative, util::paths::PathStyle::local())?;
                    let Some(source) = source_path_for_gloss_file(&relative) else {
                        continue;
                    };
                    if results.len() >= max_files {
                        log::warn!(
                            "stopping at {max_files} gloss files under {glosses_dir:?}; the rest are skipped"
                        );
                        break 'walk;
                    }
                    let remaining = max_bytes.saturating_sub(bytes_read);
                    let per_file_limit = MAX_GLOSS_FILE_BYTES.min(remaining);
                    let text = match read_bounded(&fs, &executor, &entry, per_file_limit).await {
                        Ok(text) => text,
                        Err(error) => {
                            log::warn!("skipping gloss file {entry:?}: {error:#}");
                            if remaining < MAX_GLOSS_FILE_BYTES {
                                log::warn!(
                                    "reached the {max_bytes} byte gloss budget under {glosses_dir:?}; the rest are skipped"
                                );
                                break 'walk;
                            }
                            continue;
                        }
                    };
                    bytes_read += text.len() as u64;
                    let loaded = LoadedGlosses::from_text(&text, &entry);
                    results.push((source.as_rel_path().into_arc(), loaded.glosses));
                }
            }
            results.sort_by(|(left, _), (right, _)| left.cmp(right));
            Ok(results)
        })
    }
}

/// Largest gloss file the store will read.
pub const MAX_GLOSS_FILE_BYTES: u64 = 1024 * 1024;

async fn refuse_symlink(fs: &dyn Fs, path: &Path) -> Result<()> {
    if let Some(metadata) = fs.metadata(path).await?
        && metadata.is_symlink
    {
        bail!("refusing symlink at {path:?}");
    }
    Ok(())
}

/// Resolves the on-disk gloss file for `source`, refusing a symlink at any existing
/// component from `<root>/.hulder` down to the file itself, and checking that the
/// parent directory canonicalizes inside the canonical glosses directory.
pub async fn checked_gloss_path(fs: &dyn Fs, root: &Path, source: &RelPath) -> Result<PathBuf> {
    let gloss_path = gloss_file_path(root, source)?;
    let hulder_dir = root.join(".hulder");
    let glosses_dir = root.join(GLOSSES_DIR);
    let relative = gloss_path
        .strip_prefix(&glosses_dir)
        .with_context(|| format!("{gloss_path:?} is outside {glosses_dir:?}"))?;
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("gloss path {gloss_path:?} has a non-normal component");
    }
    refuse_symlink(fs, &hulder_dir).await?;
    let mut current = glosses_dir.clone();
    refuse_symlink(fs, &current).await?;
    for component in relative.components() {
        current.push(component);
        refuse_symlink(fs, &current).await?;
    }
    if let Some(parent) = gloss_path.parent()
        && fs.metadata(parent).await?.is_some()
    {
        let canonical_parent = fs.canonicalize(parent).await?;
        let canonical_glosses_dir = fs.canonicalize(&glosses_dir).await?;
        if !canonical_parent.starts_with(&canonical_glosses_dir) {
            bail!("gloss directory {parent:?} resolves outside {glosses_dir:?}");
        }
    }
    Ok(gloss_path)
}

/// Most gloss files [`GlossStore::all_for_worktree`] will read for one worktree.
pub const MAX_GLOSS_FILES_PER_WORKTREE: usize = 2000;
/// Most bytes [`GlossStore::all_for_worktree`] will read for one worktree.
pub const MAX_GLOSS_BYTES_PER_WORKTREE: u64 = 16 * 1024 * 1024;

/// Reads at most `limit` bytes of `path`; more than `limit` bytes is an error. The open
/// and the read both run on a background thread, so a special file cannot stall the UI.
async fn read_bounded(
    fs: &Arc<dyn Fs>,
    executor: &BackgroundExecutor,
    path: &Path,
    limit: u64,
) -> Result<String> {
    let bytes = executor
        .spawn({
            let fs = fs.clone();
            let path = path.to_path_buf();
            async move { read_bounded_now(fs.as_ref(), &path, limit).await }
        })
        .await?;
    if bytes.len() as u64 > limit {
        bail!("gloss file {path:?} is over the {limit} byte limit");
    }
    String::from_utf8(bytes).with_context(|| format!("gloss file {path:?} is not UTF-8"))
}

/// Opens `path` without following a link and reads up to `limit + 1` bytes on the calling
/// thread. The bound is on the bytes actually read, so a file growing after a size check
/// cannot bypass it.
async fn read_bounded_now(fs: &dyn Fs, path: &Path, limit: u64) -> Result<Vec<u8>> {
    let reader = fs.open_sync_nofollow(path).await?;
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {path:?}"))?;
    Ok(bytes)
}

/// Reads and parses a gloss file, refusing symlinks and files over
/// [`MAX_GLOSS_FILE_BYTES`]. A missing file is an empty result.
pub async fn load_gloss_file(
    fs: &Arc<dyn Fs>,
    executor: &BackgroundExecutor,
    gloss_path: &Path,
) -> Result<LoadedGlosses> {
    if fs.metadata(gloss_path).await?.is_none() {
        return Ok(LoadedGlosses::default());
    }
    let text = read_bounded(fs, executor, gloss_path, MAX_GLOSS_FILE_BYTES).await?;
    Ok(LoadedGlosses::from_text(&text, gloss_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use path::rel_path::rel_path;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    const ONE_GLOSS: &str = "## Gloss\nline: 2-2\nanchor: fn main() {\ncreated: 2026-10-13T09:30:00Z\n\nEntry point.\n\n";

    async fn setup(
        cx: &mut TestAppContext,
        tree: serde_json::Value,
    ) -> (
        Arc<FakeFs>,
        Entity<Project>,
        Entity<GlossStore>,
        ProjectPath,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), tree).await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let store = cx.new(GlossStore::new);
        store.update(cx, |store, cx| store.register_project(project.clone(), cx));
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let path = ProjectPath {
            worktree_id,
            path: rel_path("src/main.rs").into_arc(),
        };
        (fs, project, store, path)
    }

    #[gpui::test]
    async fn test_loads_existing_gloss_file_and_notifies(cx: &mut TestAppContext) {
        let (_fs, _project, store, path) = setup(
            cx,
            json!({
                "src": { "main.rs": "// header\nfn main() {\n}\n" },
                ".hulder": { "glosses": { "src": { "main.rs.md": ONE_GLOSS } } }
            }),
        )
        .await;

        let generation_before = store.read_with(cx, |store, _| store.generation());
        let first = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        assert!(first.is_empty(), "first call starts the load");
        cx.run_until_parked();

        let loaded = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        assert_eq!(loaded.glosses.len(), 1);
        assert_eq!(loaded.glosses[0].body, "Entry point.");
        assert_eq!(loaded.glosses[0].lines, Some(LineRange::single(2)));
        let generation_after = store.read_with(cx, |store, _| store.generation());
        assert!(generation_after > generation_before);
    }

    #[gpui::test]
    async fn test_add_gloss_writes_canonical_file(cx: &mut TestAppContext) {
        let (fs, _project, store, path) = setup(
            cx,
            json!({ "src": { "main.rs": "// header\nfn main() {\n}\n" } }),
        )
        .await;

        store
            .update(cx, |store, cx| {
                store.add_gloss(
                    path.clone(),
                    Some(LineRange::new(2, 3)),
                    "fn main() {".into(),
                    "Watch this".into(),
                    None,
                    cx,
                )
            })
            .await
            .unwrap();

        let text = fs
            .load(path!("/root/.hulder/glosses/src/main.rs.md").as_ref())
            .await
            .unwrap();
        assert!(
            text.starts_with("## Gloss\nline: 2-3\nanchor: fn main() {\ncreated: "),
            "{text}"
        );
        assert!(text.ends_with("\n\nWatch this\n\n"), "{text}");

        let glosses = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        assert_eq!(glosses.glosses.len(), 1);
        assert_eq!(glosses.glosses[0].body, "Watch this");

        store
            .update(cx, |store, cx| {
                store.add_gloss(
                    path.clone(),
                    None,
                    String::new(),
                    "Whole file".into(),
                    Some("claude".into()),
                    cx,
                )
            })
            .await
            .unwrap();
        let glosses = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        assert_eq!(glosses.glosses.len(), 2);
        assert_eq!(glosses.glosses[1].author.as_deref(), Some("claude"));
        assert_eq!(glosses.glosses[1].lines, None);
    }

    #[gpui::test]
    async fn test_symlinked_gloss_file_is_refused(cx: &mut TestAppContext) {
        let (fs, _project, store, path) = setup(
            cx,
            json!({
                "src": { "main.rs": "fn main() {}\n" },
                ".hulder": { "glosses": { "src": {} } }
            }),
        )
        .await;
        fs.insert_tree(path!("/outside"), json!({ "secret.md": ONE_GLOSS }))
            .await;
        fs.insert_symlink(
            path!("/root/.hulder/glosses/src/main.rs.md"),
            PathBuf::from(path!("/outside/secret.md")),
        )
        .await;

        store.update(cx, |store, cx| store.glosses_for(&path, cx));
        cx.run_until_parked();
        let loaded = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        assert!(loaded.is_empty(), "symlinked gloss file must not be read");

        let error = store
            .update(cx, |store, cx| {
                store.add_gloss(path.clone(), None, String::new(), "nope".into(), None, cx)
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
        let secret = fs.load(path!("/outside/secret.md").as_ref()).await.unwrap();
        assert_eq!(secret, ONE_GLOSS, "symlink target must be untouched");
    }

    #[gpui::test]
    async fn test_symlinked_intermediate_directory_is_refused(cx: &mut TestAppContext) {
        let (fs, _project, store, path) = setup(
            cx,
            json!({
                "src": { "main.rs": "fn main() {}\n" },
                ".hulder": { "glosses": {} }
            }),
        )
        .await;
        fs.insert_tree(
            path!("/outside"),
            json!({ "dir": { "main.rs.md": ONE_GLOSS } }),
        )
        .await;
        fs.insert_symlink(
            path!("/root/.hulder/glosses/src"),
            PathBuf::from(path!("/outside/dir")),
        )
        .await;

        store.update(cx, |store, cx| store.glosses_for(&path, cx));
        cx.run_until_parked();
        let loaded = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        assert!(
            loaded.is_empty(),
            "gloss file behind a symlinked directory must not be read"
        );

        let error = store
            .update(cx, |store, cx| {
                store.add_gloss(path.clone(), None, String::new(), "nope".into(), None, cx)
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
        let outside = fs
            .load(path!("/outside/dir/main.rs.md").as_ref())
            .await
            .unwrap();
        assert_eq!(outside, ONE_GLOSS, "symlink target must be untouched");
    }

    #[gpui::test]
    async fn test_oversized_gloss_file_is_not_read(cx: &mut TestAppContext) {
        let big = format!("{ONE_GLOSS}{}", "x".repeat(MAX_GLOSS_FILE_BYTES as usize));
        let (_fs, _project, store, path) = setup(
            cx,
            json!({
                "src": { "main.rs": "fn main() {}\n" },
                ".hulder": { "glosses": { "src": { "main.rs.md": big } } }
            }),
        )
        .await;
        store.update(cx, |store, cx| store.glosses_for(&path, cx));
        cx.run_until_parked();
        let loaded = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        assert!(loaded.is_empty());
    }

    #[gpui::test]
    async fn test_external_edit_invalidates_cache(cx: &mut TestAppContext) {
        let (fs, _project, store, path) = setup(
            cx,
            json!({
                "src": { "main.rs": "// header\nfn main() {\n}\n" },
                ".hulder": { "glosses": { "src": { "main.rs.md": ONE_GLOSS } } }
            }),
        )
        .await;
        store.update(cx, |store, cx| store.glosses_for(&path, cx));
        cx.run_until_parked();
        assert_eq!(
            store
                .update(cx, |store, cx| store.glosses_for(&path, cx))
                .glosses
                .len(),
            1
        );

        let two =
            format!("{ONE_GLOSS}## Gloss\nanchor: \ncreated: 2026-10-13T11:00:00Z\n\nSecond.\n\n");
        fs.save(
            path!("/root/.hulder/glosses/src/main.rs.md").as_ref(),
            &two.as_str().into(),
            text::LineEnding::Unix,
        )
        .await
        .unwrap();
        cx.run_until_parked();

        store.update(cx, |store, cx| store.glosses_for(&path, cx));
        cx.run_until_parked();
        let reloaded = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        assert_eq!(
            reloaded.glosses.len(),
            2,
            "cache must be dropped when the file changes on disk"
        );
        assert_eq!(reloaded.glosses[1].body, "Second.");
    }

    #[gpui::test]
    async fn test_all_for_worktree_honors_file_and_byte_caps(cx: &mut TestAppContext) {
        let (_fs, _project, store, path) = setup(
            cx,
            json!({
                "src": { "a.rs": "", "b.rs": "", "c.rs": "" },
                ".hulder": { "glosses": { "src": {
                    "a.rs.md": ONE_GLOSS, "b.rs.md": ONE_GLOSS, "c.rs.md": ONE_GLOSS
                } } }
            }),
        )
        .await;

        let capped_by_count = store
            .read_with(cx, |store, cx| {
                store.all_for_worktree_with_limits(path.worktree_id, 2, u64::MAX, cx)
            })
            .await
            .unwrap();
        assert_eq!(capped_by_count.len(), 2, "file cap stops the walk");

        let budget = (ONE_GLOSS.len() * 2) as u64;
        let capped_by_bytes = store
            .read_with(cx, |store, cx| {
                store.all_for_worktree_with_limits(path.worktree_id, usize::MAX, budget, cx)
            })
            .await
            .unwrap();
        assert_eq!(capped_by_bytes.len(), 2, "byte budget stops the walk");
    }

    #[gpui::test]
    async fn test_all_for_worktree_lists_every_gloss_file(cx: &mut TestAppContext) {
        let (_fs, _project, store, path) = setup(
            cx,
            json!({
                "src": { "main.rs": "fn main() {}\n", "lib.rs": "pub fn lib() {}\n" },
                "README.md": "hi\n",
                ".hulder": { "glosses": {
                    "src": { "main.rs.md": ONE_GLOSS, "lib.rs.md": ONE_GLOSS },
                    "README.md.md": "## Gloss\nanchor: \ncreated: 2026-10-13T11:00:00Z\n\nDocs.\n\n",
                    "notes.txt": "not a gloss file"
                } }
            }),
        )
        .await;

        let listed = store
            .read_with(cx, |store, cx| store.all_for_worktree(path.worktree_id, cx))
            .await
            .unwrap();
        let paths: Vec<&str> = listed.iter().map(|(path, _)| path.as_unix_str()).collect();
        assert_eq!(paths, vec!["README.md", "src/lib.rs", "src/main.rs"]);
        assert_eq!(listed[0].1[0].body, "Docs.");
        assert_eq!(listed[2].1.len(), 1);
    }

    #[gpui::test]
    async fn test_bad_sections_are_reported_and_block_appends(cx: &mut TestAppContext) {
        let text = format!("{ONE_GLOSS}## Gloss\nline: nope\ncreated: 2026-10-13T11:00:00Z\n\nbroken\n\n");
        let (_fs, _project, store, path) = setup(
            cx,
            json!({
                "src": { "main.rs": "// header\nfn main() {\n}\n" },
                ".hulder": { "glosses": { "src": { "main.rs.md": text } } }
            }),
        )
        .await;
        store.update(cx, |store, cx| store.glosses_for(&path, cx));
        cx.run_until_parked();
        let loaded = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        assert_eq!(loaded.glosses.len(), 1, "the good section still loads");
        assert_eq!(loaded.errors.len(), 1);
        assert_eq!(loaded.errors[0].section, 2);

        let error = store
            .update(cx, |store, cx| {
                store.add_gloss(path.clone(), None, String::new(), "more".into(), None, cx)
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unreadable"), "{error}");
    }

    #[gpui::test]
    async fn test_glosses_past_the_cap_are_counted_not_shown(cx: &mut TestAppContext) {
        let many: String = (0..MAX_GLOSSES_PER_FILE + 3)
            .map(|index| {
                format!("## Gloss\nanchor: \ncreated: 2026-10-13T09:30:{:02}Z\n\nnote {index}\n\n", index % 60)
            })
            .collect();
        let (_fs, _project, store, path) = setup(
            cx,
            json!({
                "src": { "main.rs": "fn main() {}\n" },
                ".hulder": { "glosses": { "src": { "main.rs.md": many } } }
            }),
        )
        .await;
        store.update(cx, |store, cx| store.glosses_for(&path, cx));
        cx.run_until_parked();
        let loaded = store.update(cx, |store, cx| store.glosses_for(&path, cx));
        assert_eq!(loaded.glosses.len(), MAX_GLOSSES_PER_FILE);
        assert_eq!(loaded.hidden, 3);
    }

    #[gpui::test]
    async fn test_empty_loads_are_silent_and_the_cache_is_bounded(cx: &mut TestAppContext) {
        let (_fs, _project, store, path) = setup(
            cx,
            json!({
                "src": { "main.rs": "fn main() {}\n", "lib.rs": "" },
                ".hulder": { "glosses": { "src": { "lib.rs.md": ONE_GLOSS } } }
            }),
        )
        .await;
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let _subscription = cx.update({
            let events = events.clone();
            |cx| {
                cx.subscribe(&store, move |_, event: &GlossStoreEvent, _| {
                    events.lock().expect("events lock").push(event.clone());
                })
            }
        });

        store.update(cx, |store, cx| store.glosses_for(&path, cx));
        cx.run_until_parked();
        assert!(
            events.lock().expect("events lock").is_empty(),
            "a file without glosses must not notify every editor"
        );

        let with_gloss = ProjectPath {
            worktree_id: path.worktree_id,
            path: rel_path("src/lib.rs").into_arc(),
        };
        store.update(cx, |store, cx| store.glosses_for(&with_gloss, cx));
        cx.run_until_parked();
        let seen = events.lock().expect("events lock").clone();
        assert_eq!(
            seen,
            vec![GlossStoreEvent::Changed {
                worktree_id: with_gloss.worktree_id,
                path: with_gloss.path
            }]
        );

        for index in 0..CACHE_CAPACITY + 10 {
            let other = ProjectPath {
                worktree_id: path.worktree_id,
                path: rel_path(&format!("src/file{index}.rs")).into_arc(),
            };
            store.update(cx, |store, cx| store.glosses_for(&other, cx));
        }
        cx.run_until_parked();
        let cached = store.read_with(cx, |store, _| store.cached_file_count());
        assert!(cached <= CACHE_CAPACITY, "{cached} entries cached");
    }

    #[gpui::test]
    async fn test_add_gloss_refuses_to_grow_past_the_file_limit(cx: &mut TestAppContext) {
        let (fs, _project, store, path) = setup(
            cx,
            json!({ "src": { "main.rs": "fn main() {}\n" } }),
        )
        .await;
        let body = "x".repeat(MAX_GLOSS_FILE_BYTES as usize);
        let error = store
            .update(cx, |store, cx| {
                store.add_gloss(path.clone(), None, String::new(), body, None, cx)
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("byte limit"), "{error}");
        assert!(
            fs.metadata(path!("/root/.hulder/glosses/src/main.rs.md").as_ref())
                .await
                .unwrap()
                .is_none(),
            "nothing is written when the file would be too large"
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_reading_a_fifo_gloss_file_does_not_block(cx: &mut TestAppContext) {
        use std::os::unix::ffi::OsStrExt as _;
        use std::time::Duration;

        let directory = tempfile::tempdir().expect("tempdir");
        let fifo = directory.path().join("main.rs.md");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("cstring");
        // SAFETY: a valid NUL-terminated path; mkfifo has no other preconditions.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let real_fs: Arc<dyn Fs> = fs::RealFs::new(None, cx.executor());
        let (sender, receiver) = std::sync::mpsc::channel();
        // The read runs on its own thread so a regression that blocks on the FIFO shows
        // up as a timeout here instead of hanging the whole test binary.
        std::thread::spawn(move || {
            let result = futures::executor::block_on(read_bounded_now(
                real_fs.as_ref(),
                &fifo,
                MAX_GLOSS_FILE_BYTES,
            ));
            sender
                .send(result.map_err(|error| error.to_string()))
                .expect("send");
        });
        let result = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("reading a FIFO must return without a writer");
        let error = result.expect_err("a FIFO is not a gloss file");
        assert!(error.contains("not a regular file"), "{error}");
    }
}
