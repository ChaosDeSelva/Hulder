//! Reads and writes the Saga under `<root>/.hulder/` through `fs::Fs`. No
//! gpui here so a non-UI crate can drive the same files.
//!
//! Every path we touch goes through [`SagaStore::guarded`]: it must be a
//! plain relative path under `.hulder/`, and neither it nor any directory
//! between `.hulder` and it may be a symlink. Agents write into this folder,
//! so a planted link must never redirect a read or write elsewhere.

use crate::model::{
    AGENTS_FILE, AGENTS_TEMPLATE, BIN_DIR, CONFIG_FILE, NOTES_DIR, NOTES_FILE, NOTES_TEMPLATE,
    TICKETS_DIR, TODO_FILE, TODO_TEMPLATE, Tale, Verse, append_verse, id_from_file_name,
    lore_file_name, lore_files, next_id, parse_verses, slugify, toggle_verse,
};
use anyhow::{Context as _, Result, anyhow, bail};
use clippings::ClippingStore;
use fs::{CreateOptions, Fs};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub const DIR: &str = ".hulder";
/// Bounds for directory walks so a runaway or hostile tree cannot stall the
/// panel: `.hulder/notes/x/y` is the deepest layout we ever create.
pub const MAX_WALK_DEPTH: usize = 3;
pub const MAX_WALK_ENTRIES: usize = 5000;
/// Largest file we read under `.hulder`; anything bigger is an error rather
/// than a stall.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// `.git/info/exclude` is a few lines; refuse to rewrite a huge one.
pub const MAX_EXCLUDE_BYTES: u64 = 1024 * 1024;
/// Most lines `search` returns; a broad query over a big Saga stops here.
pub const MAX_SEARCH_HITS: usize = 200;
/// How many ids `create_tale` tries after the first collides with a file
/// another writer created in the meantime.
const MAX_ID_ATTEMPTS: u32 = 16;

/// Every write under `.hulder` runs under this lock, so two writers in one
/// process (the panel and a Bifröst call, say) can never interleave edits
/// to `TODO.md` or pick the same tale id. Stores built with `new` get a
/// private lock; the app hands every store the same one through
/// [`SagaStore::with_write_lock`]. Other processes are kept out of the id
/// race by the `create_new` open in `create_tale`.
pub type WriteLock = Arc<futures::lock::Mutex<()>>;

pub fn new_write_lock() -> WriteLock {
    Arc::new(futures::lock::Mutex::new(()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchHit {
    /// Relative to the project root, e.g. `.hulder/TODO.md`.
    pub path: String,
    /// 1-based.
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SagaConfig {
    pub tracked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaleEntry {
    pub path: PathBuf,
    pub tale: Tale,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Sorted by id.
    pub tales: Vec<TaleEntry>,
    pub todo: String,
    pub verses: Vec<Verse>,
    /// Absolute paths, in `lore_files` order.
    pub lore: Vec<PathBuf>,
    /// Absolute paths of `.hulder/clippings/*.md`, newest name first.
    pub clippings: Vec<PathBuf>,
    pub config: Option<SagaConfig>,
}

#[derive(Debug, Clone)]
pub struct SagaStore {
    root: PathBuf,
    write_lock: WriteLock,
}

impl PartialEq for SagaStore {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}

impl Eq for SagaStore {}

/// The only lines Hulder ever adds to `.git/info/exclude`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcludeLine {
    Saga,
    Bin,
    /// Captures can hold page content and secrets; they never get committed.
    Clippings,
}

impl ExcludeLine {
    pub fn as_str(self) -> &'static str {
        match self {
            ExcludeLine::Saga => ".hulder/",
            ExcludeLine::Bin => ".hulder/bin/",
            ExcludeLine::Clippings => ".hulder/clippings/",
        }
    }
}

/// A relative path under `.hulder/` with only plain components: no root,
/// no `.`/`..`, no empty names.
pub fn resolve(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    if relative.is_empty() {
        bail!("empty saga path");
    }
    if relative_path.is_absolute() || relative.starts_with('/') || relative.starts_with('\\') {
        bail!("saga path {relative:?} must be relative");
    }
    let mut resolved = root.join(DIR);
    for component in relative_path.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                if name.contains('\\') || name.contains('/') {
                    bail!("saga path {relative:?} has a separator inside a name");
                }
                resolved.push(name.as_ref());
            }
            Component::CurDir => continue,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("saga path {relative:?} must not leave .hulder")
            }
        }
    }
    if resolved == root.join(DIR) {
        bail!("saga path {relative:?} names the folder itself");
    }
    Ok(resolved)
}

/// `NNNN-slug.md`: digits, a dash, then lowercase ascii letters, digits or
/// dashes, and the `.md` extension. Anything else is not ours to touch.
pub fn is_valid_ticket_file_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    let digits = stem.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || id_from_file_name(name).is_none() {
        return false;
    }
    let Some(slug) = stem[digits..].strip_prefix('-') else {
        return false;
    };
    !slug.is_empty()
        && slug.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

/// A plain file name: no separators, no `..`, not hidden.
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Reads a file that was already guarded, refusing one larger than `limit`
/// so a hostile `.hulder` cannot make the panel allocate without bound.
async fn load_bounded(fs: &dyn Fs, path: &Path, limit: u64) -> Result<String> {
    let metadata = fs
        .metadata(path)
        .await?
        .with_context(|| format!("{} does not exist", path.display()))?;
    if metadata.is_symlink {
        bail!("{} is a symlink; refusing to read it", path.display());
    }
    if metadata.len > limit {
        bail!(
            "{} is {} bytes, larger than the {limit} byte limit",
            path.display(),
            metadata.len
        );
    }
    fs.load(path).await
}

async fn is_symlink(fs: &dyn Fs, path: &Path) -> Result<bool> {
    Ok(fs
        .metadata(path)
        .await
        .with_context(|| format!("stat {}", path.display()))?
        .is_some_and(|metadata| metadata.is_symlink))
}

impl SagaStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            write_lock: new_write_lock(),
        }
    }

    /// Shares the write lock with every other store built the same way.
    pub fn with_write_lock(mut self, write_lock: WriteLock) -> Self {
        self.write_lock = write_lock;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join(DIR)
    }

    pub fn tickets_dir(&self) -> PathBuf {
        self.dir().join(TICKETS_DIR)
    }

    pub fn todo_path(&self) -> PathBuf {
        self.dir().join(TODO_FILE)
    }

    pub fn resolve(&self, relative: &str) -> Result<PathBuf> {
        resolve(&self.root, relative)
    }

    /// Resolves `relative` and refuses when `.hulder` or anything from it
    /// down to the target is a symlink. Missing parts are fine: the caller
    /// is about to create them.
    pub async fn guarded(&self, fs: &dyn Fs, relative: &str) -> Result<PathBuf> {
        let target = self.resolve(relative)?;
        let mut cursor = self.dir();
        if is_symlink(fs, &cursor).await? {
            bail!("{} is a symlink; refusing to use it", cursor.display());
        }
        let Ok(inside) = target.strip_prefix(&cursor) else {
            bail!("saga path {relative:?} resolved outside .hulder");
        };
        for component in inside.components() {
            cursor.push(component);
            if is_symlink(fs, &cursor).await? {
                bail!("{} is a symlink; refusing to use it", cursor.display());
            }
        }
        Ok(target)
    }

    /// True for a worktree-relative path inside `.hulder/`, the filter the
    /// panel applies to worktree change events.
    pub fn is_saga_relative_path(relative: &str) -> bool {
        relative == DIR || relative.starts_with(&format!("{DIR}/"))
    }

    pub async fn exists(&self, fs: &dyn Fs) -> bool {
        fs.is_dir(&self.dir()).await
    }

    /// Lists plain (non-symlink) entries of one directory under `.hulder`,
    /// as `(absolute path, is_dir)`. Symlinked entries are skipped with a
    /// log line so they can never redirect a later read.
    async fn list_dir(&self, fs: &dyn Fs, relative: &str) -> Result<Vec<(PathBuf, bool)>> {
        let dir = self.guarded(fs, relative).await?;
        let mut entries = Vec::new();
        if !fs.is_dir(&dir).await {
            return Ok(entries);
        }
        let mut stream = fs.read_dir(&dir).await?;
        while let Some(entry) = stream.next().await {
            let path = entry?;
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !is_plain_name(&name) {
                log::warn!("saga: skipping {} (unusual name)", path.display());
                continue;
            }
            let Some(metadata) = fs.metadata(&path).await? else {
                continue;
            };
            if metadata.is_symlink {
                log::warn!("saga: skipping symlink {}", path.display());
                continue;
            }
            entries.push((path, metadata.is_dir));
        }
        Ok(entries)
    }

    /// Walks `.hulder` breadth first, never following symlinks, at most
    /// `MAX_WALK_DEPTH` levels deep and `MAX_WALK_ENTRIES` entries in total.
    /// Returns relative file paths with `/` separators.
    async fn walk_files(&self, fs: &dyn Fs) -> Result<Vec<String>> {
        let mut files = Vec::new();
        let mut pending = vec![(String::new(), 0usize)];
        let mut seen = 0usize;
        while let Some((relative_dir, depth)) = pending.pop() {
            let listing = if relative_dir.is_empty() {
                self.list_root(fs).await?
            } else {
                self.list_dir(fs, &relative_dir).await?
            };
            for (path, is_dir) in listing {
                seen += 1;
                if seen > MAX_WALK_ENTRIES {
                    log::warn!(
                        "saga: more than {MAX_WALK_ENTRIES} entries under .hulder; ignoring the rest"
                    );
                    return Ok(files);
                }
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let relative = if relative_dir.is_empty() {
                    name
                } else {
                    format!("{relative_dir}/{name}")
                };
                if is_dir {
                    if depth + 1 >= MAX_WALK_DEPTH {
                        log::warn!(
                            "saga: {} is deeper than {MAX_WALK_DEPTH} levels; skipped",
                            path.display()
                        );
                        continue;
                    }
                    pending.push((relative, depth + 1));
                } else {
                    files.push(relative);
                }
            }
        }
        files.sort();
        Ok(files)
    }

    async fn list_root(&self, fs: &dyn Fs) -> Result<Vec<(PathBuf, bool)>> {
        let dir = self.dir();
        if is_symlink(fs, &dir).await? {
            bail!("{} is a symlink; refusing to use it", dir.display());
        }
        let mut entries = Vec::new();
        if !fs.is_dir(&dir).await {
            return Ok(entries);
        }
        let mut stream = fs.read_dir(&dir).await?;
        while let Some(entry) = stream.next().await {
            let path = entry?;
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !is_plain_name(&name) {
                continue;
            }
            let Some(metadata) = fs.metadata(&path).await? else {
                continue;
            };
            if metadata.is_symlink {
                log::warn!("saga: skipping symlink {}", path.display());
                continue;
            }
            entries.push((path, metadata.is_dir));
        }
        Ok(entries)
    }

    async fn load_guarded(&self, fs: &dyn Fs, relative: &str) -> Result<Option<String>> {
        let path = self.guarded(fs, relative).await?;
        if !fs.is_file(&path).await {
            return Ok(None);
        }
        Ok(Some(load_bounded(fs, &path, MAX_FILE_BYTES).await?))
    }

    /// Writes through `Fs::atomic_write`, which creates a temp file in the
    /// same directory and renames it over the target: a symlink planted at
    /// the final component is replaced, never followed. The parent chain is
    /// re-checked for symlinks right before that rename, which is as close to
    /// the write as the `Fs` API allows.
    async fn write_guarded(&self, fs: &dyn Fs, relative: &str, text: String) -> Result<PathBuf> {
        let path = self.guarded(fs, relative).await?;
        fs.atomic_write(path.clone(), text).await?;
        Ok(path)
    }

    /// Reads one file under `.hulder` by absolute path with every guard
    /// `load` applies: inside the folder, no symlink on the way, at most
    /// `MAX_FILE_BYTES`. A missing file is an error.
    pub async fn read_at(&self, fs: &dyn Fs, path: &Path) -> Result<String> {
        let Ok(inside) = path.strip_prefix(self.dir()) else {
            bail!("{} is not under {}", path.display(), self.dir().display());
        };
        let relative = inside
            .to_str()
            .with_context(|| format!("{} is not UTF-8", path.display()))?;
        self.load_guarded(fs, relative)
            .await?
            .with_context(|| format!("{} does not exist", path.display()))
    }

    /// Lore names relative to `.hulder/`, in `lore_files` order.
    pub async fn lore_names(&self, fs: &dyn Fs) -> Result<Vec<String>> {
        let relative_files = self.walk_files(fs).await?;
        Ok(lore_files(relative_files.iter().map(String::as_str))
            .into_iter()
            .map(str::to_string)
            .collect())
    }

    /// Reads one lore file by its name relative to `.hulder/`. Anything that
    /// is not lore (tales, `TODO.md`, clippings, `bin/`, non-Markdown) is
    /// refused, as is a missing file.
    pub async fn read_lore(&self, fs: &dyn Fs, name: &str) -> Result<String> {
        if lore_files([name]).is_empty() {
            bail!("{name:?} is not a lore file (NOTES.md or notes/<file>.md)");
        }
        self.load_guarded(fs, name)
            .await?
            .with_context(|| format!("lore {name:?} does not exist"))
    }

    /// Case-insensitive line search over every Markdown file under
    /// `.hulder` except build output, stopping at `MAX_SEARCH_HITS`. Files
    /// that cannot be read (too big, vanished, a planted link) are skipped
    /// with a log line rather than failing the whole search.
    pub async fn search(&self, fs: &dyn Fs, query: &str) -> Result<Vec<SearchHit>> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            bail!("search needs a query");
        }
        let mut hits = Vec::new();
        for relative in self.walk_files(fs).await? {
            if !relative.ends_with(".md") || relative.starts_with(&format!("{BIN_DIR}/")) {
                continue;
            }
            let text = match self.load_guarded(fs, &relative).await {
                Ok(Some(text)) => text,
                Ok(None) => continue,
                Err(error) => {
                    log::warn!("saga: search skipping {relative}: {error:#}");
                    continue;
                }
            };
            for (index, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&query) {
                    hits.push(SearchHit {
                        path: format!("{DIR}/{relative}"),
                        line: index + 1,
                        text: line.to_string(),
                    });
                    if hits.len() >= MAX_SEARCH_HITS {
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(hits)
    }

    pub async fn load(&self, fs: &dyn Fs) -> Result<Snapshot> {
        if !self.exists(fs).await {
            return Ok(Snapshot::default());
        }
        let dir = self.dir();

        let mut tales = Vec::new();
        for (path, is_dir) in self.list_dir(fs, TICKETS_DIR).await? {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if is_dir || !is_valid_ticket_file_name(&name) {
                continue;
            }
            let parsed = match load_bounded(fs, &path, MAX_FILE_BYTES).await {
                Ok(text) => Tale::parse(&text),
                Err(error) => Err(error),
            };
            match parsed {
                Ok(tale) => tales.push(TaleEntry { path, tale }),
                Err(error) => log::warn!("saga: skipping {}: {error:#}", path.display()),
            }
        }
        tales.sort_by_key(|entry| entry.tale.id);

        let todo = match self.load_guarded(fs, TODO_FILE).await {
            Ok(todo) => todo.unwrap_or_default(),
            Err(error) => {
                log::warn!("saga: skipping {TODO_FILE}: {error:#}");
                String::new()
            }
        };
        let verses = parse_verses(&todo);

        let relative_files = self.walk_files(fs).await?;
        let lore = lore_files(relative_files.iter().map(String::as_str))
            .into_iter()
            .map(|relative| dir.join(relative))
            .collect();

        let clippings = match ClippingStore::new(&self.root).list(fs).await {
            Ok(clippings) => clippings,
            Err(error) => {
                log::warn!("saga: clippings not listed: {error:#}");
                Vec::new()
            }
        };

        let config = match self.load_guarded(fs, CONFIG_FILE).await {
            Ok(Some(text)) => match serde_json::from_str(&text) {
                Ok(config) => Some(config),
                Err(error) => {
                    log::warn!("saga: ignoring {CONFIG_FILE}: {error:#}");
                    None
                }
            },
            Ok(None) => None,
            Err(error) => {
                log::warn!("saga: ignoring {CONFIG_FILE}: {error:#}");
                None
            }
        };

        Ok(Snapshot {
            tales,
            todo,
            verses,
            lore,
            clippings,
            config,
        })
    }

    /// Creates the folder layout on first use and records the tracking
    /// choice. Safe to call again: existing files are left alone and exclude
    /// lines are never duplicated.
    pub async fn initialize(&self, fs: &dyn Fs, tracked: bool) -> Result<()> {
        let _write = self.write_lock.lock().await;
        for subdir in [TICKETS_DIR, NOTES_DIR] {
            let path = self.guarded(fs, subdir).await?;
            fs.create_dir(&path).await?;
        }
        for (name, template) in [
            (NOTES_FILE, NOTES_TEMPLATE),
            (TODO_FILE, TODO_TEMPLATE),
            (AGENTS_FILE, AGENTS_TEMPLATE),
        ] {
            if self.load_guarded(fs, name).await?.is_none() {
                self.write_guarded(fs, name, template.to_string()).await?;
            }
        }
        let config = serde_json::to_string_pretty(&SagaConfig { tracked })? + "\n";
        self.write_guarded(fs, CONFIG_FILE, config).await?;

        if !tracked {
            ensure_excluded(fs, &self.root, ExcludeLine::Saga).await?;
        }
        ensure_excluded(fs, &self.root, ExcludeLine::Bin).await?;
        ensure_excluded(fs, &self.root, ExcludeLine::Clippings).await?;
        Ok(())
    }

    async fn ticket_file_names(&self, fs: &dyn Fs) -> Result<Vec<String>> {
        Ok(self
            .list_dir(fs, TICKETS_DIR)
            .await?
            .into_iter()
            .filter(|(_, is_dir)| !is_dir)
            .filter_map(|(path, _)| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .filter(|name| id_from_file_name(name).is_some())
            .collect())
    }

    /// Creates a tale with the next free id. The file is opened with
    /// `create_new` first, so a tale another process writes between the
    /// listing and ours fails the open instead of being overwritten; the id
    /// is then bumped and the open retried.
    pub async fn create_tale(&self, fs: &dyn Fs, title: &str, today: &str) -> Result<TaleEntry> {
        let _write = self.write_lock.lock().await;
        let tale = Tale::new(0, title, today);
        tale.validate_front_matter()?;
        let slug = slugify(title);
        let tickets_dir = self.guarded(fs, TICKETS_DIR).await?;
        fs.create_dir(&tickets_dir).await?;
        let mut names = self.ticket_file_names(fs).await?;
        for _attempt in 0..MAX_ID_ATTEMPTS {
            let id = next_id(names.iter().map(String::as_str));
            let file_name = Tale::file_name(id, &slug);
            if !is_valid_ticket_file_name(&file_name) {
                bail!("generated ticket name {file_name:?} is not valid");
            }
            let relative = format!("{TICKETS_DIR}/{file_name}");
            let path = self.guarded(fs, &relative).await?;
            let created = fs
                .create_file(
                    &path,
                    CreateOptions {
                        overwrite: false,
                        ignore_if_exists: false,
                    },
                )
                .await;
            if let Err(error) = created {
                names = self.ticket_file_names(fs).await?;
                if names.iter().any(|name| id_from_file_name(name) == Some(id)) {
                    log::info!("saga: tale id {id} was taken meanwhile; retrying");
                    continue;
                }
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
            let tale = Tale { id, ..tale };
            let path = self.write_guarded(fs, &relative, tale.serialize()).await?;
            return Ok(TaleEntry { path, tale });
        }
        bail!("could not find a free tale id after {MAX_ID_ATTEMPTS} attempts")
    }

    /// Rewrites a tale in place. The entry's path must be a valid ticket
    /// file directly under `tickets/`.
    pub async fn write_tale(&self, fs: &dyn Fs, entry: &TaleEntry) -> Result<()> {
        let _write = self.write_lock.lock().await;
        entry.tale.validate_front_matter()?;
        let relative = self.ticket_relative_path(&entry.path)?;
        self.write_guarded(fs, &relative, entry.tale.serialize())
            .await?;
        Ok(())
    }

    fn ticket_relative_path(&self, path: &Path) -> Result<String> {
        let Ok(inside) = path.strip_prefix(self.tickets_dir()) else {
            bail!(
                "{} is not under {}",
                path.display(),
                self.tickets_dir().display()
            );
        };
        let name = inside.to_string_lossy();
        if !is_valid_ticket_file_name(&name) {
            bail!("{} is not a ticket file name", path.display());
        }
        Ok(format!("{TICKETS_DIR}/{name}"))
    }

    pub async fn toggle_verse(&self, fs: &dyn Fs, line: usize) -> Result<()> {
        let _write = self.write_lock.lock().await;
        let todo = self
            .load_guarded(fs, TODO_FILE)
            .await?
            .with_context(|| format!("{TODO_FILE} does not exist"))?;
        let toggled = toggle_verse(&todo, line)
            .ok_or_else(|| anyhow!("line {line} of {TODO_FILE} is not a verse"))?;
        self.write_guarded(fs, TODO_FILE, toggled).await?;
        Ok(())
    }

    pub async fn add_verse(&self, fs: &dyn Fs, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            bail!("a verse needs text");
        }
        if text.chars().any(char::is_control) {
            bail!("a verse must not contain control characters");
        }
        let _write = self.write_lock.lock().await;
        let todo = self
            .load_guarded(fs, TODO_FILE)
            .await?
            .unwrap_or_else(|| TODO_TEMPLATE.to_string());
        self.write_guarded(fs, TODO_FILE, append_verse(&todo, text))
            .await?;
        Ok(())
    }

    /// Creates today's lore file, or returns the existing one.
    pub async fn create_lore(&self, fs: &dyn Fs, today: &str) -> Result<PathBuf> {
        let _write = self.write_lock.lock().await;
        if !is_plain_name(today) || !today.chars().all(|c| c.is_ascii_digit() || c == '-') {
            bail!("lore date {today:?} is not a plain YYYY-MM-DD name");
        }
        let relative = lore_file_name(today);
        if let Some(_existing) = self.load_guarded(fs, &relative).await? {
            return self.guarded(fs, &relative).await;
        }
        let notes_dir = self.guarded(fs, NOTES_DIR).await?;
        fs.create_dir(&notes_dir).await?;
        self.write_guarded(fs, &relative, format!("# {today}\n\n"))
            .await
    }
}

/// Appends one of the two fixed lines to `<root>/.git/info/exclude` when that
/// file already exists and does not list it yet. A missing exclude file means
/// this is not a git checkout (or an unusual one), and we leave that alone;
/// a symlink anywhere in `.git/info/exclude` is refused.
pub async fn ensure_excluded(fs: &dyn Fs, root: &Path, line: ExcludeLine) -> Result<()> {
    let mut cursor = root.to_path_buf();
    for part in [".git", "info", "exclude"] {
        cursor.push(part);
        if is_symlink(fs, &cursor).await? {
            bail!(
                "{} is a symlink; refusing to edit the git exclude file",
                cursor.display()
            );
        }
    }
    let exclude_path = cursor;
    if !fs.is_file(&exclude_path).await {
        return Ok(());
    }
    let pattern = line.as_str();
    let existing = load_bounded(fs, &exclude_path, MAX_EXCLUDE_BYTES).await?;
    if existing.lines().any(|existing| existing.trim() == pattern) {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(pattern);
    updated.push('\n');
    fs.atomic_write(exclude_path, updated).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Priority, Status};
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use std::sync::Arc;

    fn tale_text(id: u32, title: &str, status: &str) -> String {
        format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: normal\ntags: []\ncreated: 2026-10-13\nupdated: 2026-10-13\n---\n\n## Chronicle\n"
        )
    }

    async fn populated_fs(cx: &TestAppContext) -> Arc<FakeFs> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({
                ".git": { "info": { "exclude": "# comment\n" } },
                ".hulder": {
                    "NOTES.md": "# Notes\n",
                    "TODO.md": "# Todo\n\n- [ ] one\n- [x] two\n",
                    "AGENTS.md": "agents",
                    "saga.json": "{\"tracked\": true}\n",
                    "ideas.md": "ideas",
                    "notes": { "2026-10-13.md": "# day\n" },
                    "tickets": {
                        "0002-second.md": tale_text(2, "Second", "doing"),
                        "0001-first.md": tale_text(1, "First", "open"),
                        "broken.md": "not a tale",
                        "0003-Upper.md": tale_text(3, "Bad name", "open"),
                        "README.txt": "ignored",
                    },
                    "bin": { "out.md": "built" },
                    "clippings": {
                        "2026-10-12-old-page.md": "---\nsource: a\nkind: fetch\n---\n",
                        "2026-10-13-new-page.md": "---\nsource: b\nkind: fetch\n---\n",
                        "assets": { "2026-10-13-new-page.png": "png" },
                    },
                },
            }),
        )
        .await;
        fs
    }

    #[gpui::test]
    async fn loads_snapshot_sorted_by_id(cx: &mut TestAppContext) {
        let fs = populated_fs(cx).await;
        let store = SagaStore::new("/repo");
        let snapshot = store.load(fs.as_ref()).await.expect("load");
        let ids: Vec<u32> = snapshot.tales.iter().map(|entry| entry.tale.id).collect();
        assert_eq!(
            ids,
            vec![1, 2],
            "broken and badly named tickets are skipped"
        );
        assert_eq!(snapshot.tales[1].tale.status, Status::Doing);
        assert_eq!(
            snapshot.tales[0].path,
            PathBuf::from("/repo/.hulder/tickets/0001-first.md")
        );
        assert_eq!(snapshot.verses.len(), 2);
        assert!(snapshot.verses[1].done);
        assert_eq!(
            snapshot.lore,
            vec![
                PathBuf::from("/repo/.hulder/NOTES.md"),
                PathBuf::from("/repo/.hulder/ideas.md"),
                PathBuf::from("/repo/.hulder/notes/2026-10-13.md"),
            ],
            "clippings are not lore"
        );
        assert_eq!(
            snapshot.clippings,
            vec![
                PathBuf::from("/repo/.hulder/clippings/2026-10-13-new-page.md"),
                PathBuf::from("/repo/.hulder/clippings/2026-10-12-old-page.md"),
            ]
        );
        assert_eq!(snapshot.config, Some(SagaConfig { tracked: true }));
    }

    #[gpui::test]
    async fn missing_folder_loads_empty_snapshot(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/empty", json!({ "src": {} })).await;
        let store = SagaStore::new("/empty");
        assert!(!store.exists(fs.as_ref()).await);
        let snapshot = store.load(fs.as_ref()).await.expect("load");
        assert_eq!(snapshot, Snapshot::default());
    }

    #[gpui::test]
    async fn initialize_local_only_creates_layout_and_excludes(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({ ".git": { "info": { "exclude": "target/" } } }),
        )
        .await;
        let store = SagaStore::new("/repo");
        store.initialize(fs.as_ref(), false).await.expect("init");
        store
            .initialize(fs.as_ref(), false)
            .await
            .expect("init again");

        for name in ["NOTES.md", "TODO.md", "AGENTS.md", "saga.json"] {
            assert!(fs.is_file(&store.dir().join(name)).await, "{name}");
        }
        assert!(fs.is_dir(&store.tickets_dir()).await);
        assert!(fs.is_dir(&store.dir().join("notes")).await);
        assert_eq!(
            fs.load(Path::new("/repo/.hulder/AGENTS.md"))
                .await
                .expect("agents"),
            AGENTS_TEMPLATE
        );
        let config: SagaConfig = serde_json::from_str(
            &fs.load(Path::new("/repo/.hulder/saga.json"))
                .await
                .expect("config"),
        )
        .expect("json");
        assert!(!config.tracked);
        assert_eq!(
            fs.load(Path::new("/repo/.git/info/exclude"))
                .await
                .expect("exclude"),
            "target/\n.hulder/\n.hulder/bin/\n.hulder/clippings/\n"
        );
    }

    #[gpui::test]
    async fn initialize_tracked_excludes_bin_and_clippings(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({ ".git": { "info": { "exclude": "" } }, ".hulder": { "NOTES.md": "mine" } }),
        )
        .await;
        let store = SagaStore::new("/repo");
        store.initialize(fs.as_ref(), true).await.expect("init");
        assert_eq!(
            fs.load(Path::new("/repo/.git/info/exclude"))
                .await
                .expect("exclude"),
            ".hulder/bin/\n.hulder/clippings/\n"
        );
        assert_eq!(
            fs.load(Path::new("/repo/.hulder/NOTES.md"))
                .await
                .expect("notes"),
            "mine"
        );
    }

    #[gpui::test]
    async fn initialize_without_exclude_file_touches_nothing_outside(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({ "src": {} })).await;
        let store = SagaStore::new("/repo");
        store.initialize(fs.as_ref(), false).await.expect("init");
        assert!(!fs.is_file(Path::new("/repo/.git/info/exclude")).await);
        assert!(!fs.is_dir(Path::new("/repo/.git")).await);
        assert!(store.exists(fs.as_ref()).await);
    }

    #[gpui::test]
    async fn exclude_edit_refuses_symlinked_git_parts(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({ "elsewhere": { "info": { "exclude": "" } }, ".hulder": {} }),
        )
        .await;
        fs.insert_symlink("/repo/.git", PathBuf::from("/repo/elsewhere"))
            .await;
        let store = SagaStore::new("/repo");
        let error = store
            .initialize(fs.as_ref(), false)
            .await
            .expect_err("symlinked .git must be refused");
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(
            fs.load(Path::new("/repo/elsewhere/info/exclude"))
                .await
                .expect("read"),
            ""
        );
    }

    #[gpui::test]
    async fn creates_tales_with_increasing_ids(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({})).await;
        let store = SagaStore::new("/repo");
        store.initialize(fs.as_ref(), true).await.expect("init");
        let first = store
            .create_tale(fs.as_ref(), "Fix it", "2026-10-13")
            .await
            .expect("first");
        assert_eq!(
            first.path,
            PathBuf::from("/repo/.hulder/tickets/0001-fix-it.md")
        );
        let second = store
            .create_tale(fs.as_ref(), "Fix it again!", "2026-10-13")
            .await
            .expect("second");
        assert_eq!(second.tale.id, 2);
        assert!(second.path.ends_with("0002-fix-it-again.md"));

        let mut entry = second;
        entry.tale.status = Status::Doing;
        entry.tale.priority = Priority::High;
        entry.tale.append_chronicle("2026-10-14 started by claude");
        store.write_tale(fs.as_ref(), &entry).await.expect("write");
        let snapshot = store.load(fs.as_ref()).await.expect("load");
        assert_eq!(snapshot.tales[1].tale, entry.tale);

        let mut outside = entry.clone();
        outside.path = PathBuf::from("/repo/.hulder/tickets/../../etc/passwd");
        assert!(store.write_tale(fs.as_ref(), &outside).await.is_err());
        outside.path = PathBuf::from("/repo/.hulder/tickets/0002-fix-it-again.md/x");
        assert!(store.write_tale(fs.as_ref(), &outside).await.is_err());
    }

    #[gpui::test]
    async fn verses_and_lore_writes(cx: &mut TestAppContext) {
        let fs = populated_fs(cx).await;
        let store = SagaStore::new("/repo");
        store.toggle_verse(fs.as_ref(), 2).await.expect("toggle");
        store.add_verse(fs.as_ref(), "three").await.expect("add");
        assert_eq!(
            fs.load(&store.todo_path()).await.expect("todo"),
            "# Todo\n\n- [x] one\n- [x] two\n- [ ] three\n"
        );
        assert!(store.toggle_verse(fs.as_ref(), 0).await.is_err());

        let today = store
            .create_lore(fs.as_ref(), "2026-10-14")
            .await
            .expect("lore");
        assert_eq!(today, PathBuf::from("/repo/.hulder/notes/2026-10-14.md"));
        assert_eq!(fs.load(&today).await.expect("read"), "# 2026-10-14\n\n");
        let existing = store
            .create_lore(fs.as_ref(), "2026-10-13")
            .await
            .expect("existing");
        assert_eq!(fs.load(&existing).await.expect("read"), "# day\n");
        assert!(store.create_lore(fs.as_ref(), "../escape").await.is_err());
    }

    #[gpui::test]
    async fn refuses_symlinked_todo(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({ "secret.txt": "- [ ] shh\n", ".hulder": { "NOTES.md": "" } }),
        )
        .await;
        fs.insert_symlink("/repo/.hulder/TODO.md", PathBuf::from("/repo/secret.txt"))
            .await;
        let store = SagaStore::new("/repo");
        let error = store
            .add_verse(fs.as_ref(), "x")
            .await
            .expect_err("symlinked TODO.md must be refused");
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(
            fs.load(Path::new("/repo/secret.txt")).await.expect("read"),
            "- [ ] shh\n"
        );
        let snapshot = store
            .load(fs.as_ref())
            .await
            .expect("a symlinked TODO.md is skipped, not followed");
        assert_eq!(snapshot.todo, "");
        assert!(snapshot.verses.is_empty());
    }

    #[gpui::test]
    async fn walk_skips_symlinked_dirs_and_caps_depth(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({
                "outside": { "leak.md": "leak" },
                ".hulder": {
                    "NOTES.md": "",
                    "notes": {
                        "a.md": "",
                        "deep": { "b.md": "", "deeper": { "c.md": "", "deepest": { "d.md": "" } } },
                    },
                },
            }),
        )
        .await;
        fs.insert_symlink("/repo/.hulder/notes/linked", PathBuf::from("/repo/outside"))
            .await;
        fs.insert_symlink(
            "/repo/.hulder/linked.md",
            PathBuf::from("/repo/outside/leak.md"),
        )
        .await;
        let store = SagaStore::new("/repo");
        let snapshot = store.load(fs.as_ref()).await.expect("load");
        let lore: Vec<String> = snapshot
            .lore
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            lore,
            vec![
                "/repo/.hulder/NOTES.md",
                "/repo/.hulder/notes/a.md",
                "/repo/.hulder/notes/deep/b.md",
            ],
            "symlinks skipped, depth capped at {MAX_WALK_DEPTH}"
        );
    }

    #[gpui::test]
    async fn symlink_planted_at_write_target_is_not_followed(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({
                "victim.md": "untouched",
                ".hulder": { "tickets": {}, "notes": {} },
            }),
        )
        .await;
        fs.insert_symlink(
            "/repo/.hulder/tickets/0001-x.md",
            PathBuf::from("/repo/victim.md"),
        )
        .await;
        fs.insert_symlink(
            "/repo/.hulder/notes/2026-10-13.md",
            PathBuf::from("/repo/victim.md"),
        )
        .await;
        let store = SagaStore::new("/repo");
        let entry = TaleEntry {
            path: PathBuf::from("/repo/.hulder/tickets/0001-x.md"),
            tale: Tale::new(1, "x", "2026-10-13"),
        };
        assert!(store.write_tale(fs.as_ref(), &entry).await.is_err());
        assert!(store.create_lore(fs.as_ref(), "2026-10-13").await.is_err());
        assert!(
            store.load(fs.as_ref()).await.is_ok(),
            "symlinked tickets are skipped on load"
        );
        assert_eq!(
            fs.load(Path::new("/repo/victim.md")).await.expect("read"),
            "untouched"
        );
    }

    #[gpui::test]
    async fn oversized_files_are_refused(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let huge = "x".repeat(MAX_FILE_BYTES as usize + 1);
        fs.insert_tree(
            "/repo",
            json!({
                ".git": { "info": { "exclude": "x".repeat(MAX_EXCLUDE_BYTES as usize + 1) } },
                ".hulder": { "TODO.md": huge },
            }),
        )
        .await;
        let store = SagaStore::new("/repo");
        let snapshot = store
            .load(fs.as_ref())
            .await
            .expect("an oversized TODO.md is skipped, not loaded");
        assert!(snapshot.verses.is_empty());
        let error = store
            .initialize(fs.as_ref(), false)
            .await
            .expect_err("exclude too big");
        assert!(error.to_string().contains("limit"), "{error:#}");
    }

    #[test]
    fn resolve_rejects_escapes() {
        let root = Path::new("/repo");
        assert_eq!(
            resolve(root, "tickets/0001-a.md").expect("ok"),
            PathBuf::from("/repo/.hulder/tickets/0001-a.md")
        );
        assert_eq!(
            resolve(root, "./NOTES.md").expect("ok"),
            PathBuf::from("/repo/.hulder/NOTES.md")
        );
        for bad in [
            "../x",
            "tickets/../../x",
            "/etc/passwd",
            "",
            ".",
            "notes/../NOTES.md",
        ] {
            assert!(resolve(root, bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn ticket_file_name_validation() {
        assert!(is_valid_ticket_file_name("0001-fix-it.md"));
        assert!(is_valid_ticket_file_name("12345-a1.md"));
        for bad in [
            "0001-.md",
            "0001-Fix.md",
            "fix.md",
            "0001-fix.txt",
            "0001-fix it.md",
            "0001-../x.md",
            "0001-a/b.md",
        ] {
            assert!(!is_valid_ticket_file_name(bad), "{bad:?}");
        }
        assert!(is_plain_name("2026-10-13.md"));
        assert!(!is_plain_name(".hidden"));
        assert!(!is_plain_name("a/b"));
        assert!(!is_plain_name(".."));
    }

    #[test]
    fn saga_relative_path_filter() {
        assert!(SagaStore::is_saga_relative_path(".hulder"));
        assert!(SagaStore::is_saga_relative_path(
            ".hulder/tickets/0001-x.md"
        ));
        assert!(!SagaStore::is_saga_relative_path(".hulderx/y"));
        assert!(!SagaStore::is_saga_relative_path("src/main.rs"));
    }

    #[gpui::test]
    async fn bad_files_are_skipped_and_logged(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let huge_tale = format!(
            "{}{}",
            tale_text(2, "Huge", "open"),
            "x".repeat(MAX_FILE_BYTES as usize + 1)
        );
        fs.insert_tree(
            "/repo",
            json!({
                ".hulder": {
                    "saga.json": "{ not json",
                    "TODO.md": "- [ ] one\n",
                    "tickets": {
                        "0001-fine.md": tale_text(1, "Fine", "open"),
                        "0002-huge.md": huge_tale,
                        "0003-fine.md": tale_text(3, "Also fine", "open"),
                    },
                },
            }),
        )
        .await;
        let store = SagaStore::new("/repo");
        let snapshot = store
            .load(fs.as_ref())
            .await
            .expect("load survives bad files");
        let ids: Vec<u32> = snapshot.tales.iter().map(|entry| entry.tale.id).collect();
        assert_eq!(ids, vec![1, 3], "the oversized tale is skipped");
        assert_eq!(snapshot.verses.len(), 1);
        assert_eq!(
            snapshot.config, None,
            "a bad saga.json is treated as absent"
        );
    }

    #[gpui::test]
    async fn create_tale_takes_ids_from_file_names_and_skips_collisions(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({
                ".hulder": {
                    "tickets": {
                        "0007-unparseable.md": "not a tale at all",
                        "0003-ok.md": tale_text(3, "Ok", "open"),
                    },
                },
            }),
        )
        .await;
        let store = SagaStore::new("/repo");
        let created = store
            .create_tale(fs.as_ref(), "Next", "2026-10-13")
            .await
            .expect("create");
        assert_eq!(
            created.tale.id, 8,
            "ids come from file names, parseable or not"
        );

        // A file with the next id lands between the listing and the write.
        fs.insert_file(
            "/repo/.hulder/tickets/0009-raced.md",
            tale_text(9, "Raced", "open").into_bytes(),
        )
        .await;
        let created = store
            .create_tale(fs.as_ref(), "After race", "2026-10-13")
            .await
            .expect("create");
        assert_eq!(created.tale.id, 10);
        assert!(created.path.ends_with("0010-after-race.md"));
        assert_eq!(
            fs.load(Path::new("/repo/.hulder/tickets/0009-raced.md"))
                .await
                .expect("raced tale untouched"),
            tale_text(9, "Raced", "open")
        );
    }

    #[gpui::test]
    async fn control_characters_in_front_matter_are_refused(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({ ".hulder": { "tickets": {} } }))
            .await;
        let store = SagaStore::new("/repo");
        let error = store
            .create_tale(fs.as_ref(), "Fix\nstatus: done", "2026-10-13")
            .await
            .expect_err("newline in title");
        assert!(error.to_string().contains("control"), "{error:#}");

        let mut entry = store
            .create_tale(fs.as_ref(), "Plain", "2026-10-13")
            .await
            .expect("create");
        entry.tale.tags = vec!["ui".into(), "a\r\nb".into()];
        let error = store
            .write_tale(fs.as_ref(), &entry)
            .await
            .expect_err("newline in tag");
        assert!(error.to_string().contains("control"), "{error:#}");
        let snapshot = store.load(fs.as_ref()).await.expect("load");
        assert_eq!(snapshot.tales[0].tale.tags, Vec::<String>::new());
    }

    #[gpui::test]
    async fn search_is_capped_and_skips_bin_and_oversized_files(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let many: String = (0..(MAX_SEARCH_HITS + 50))
            .map(|index| format!("needle {index}\n"))
            .collect();
        fs.insert_tree(
            "/repo",
            json!({
                ".hulder": {
                    "NOTES.md": "a needle here\n",
                    "TODO.md": "- [ ] find the Needle\n",
                    "saga.json": "{\"tracked\": true, \"needle\": 1}",
                    "bin": { "out.md": "needle in build output" },
                    "notes": { "many.md": many },
                    "tickets": {
                        "0001-a.md": format!("{}needle\n", tale_text(1, "A", "open")),
                        "0002-big.md": "x".repeat(MAX_FILE_BYTES as usize + 1),
                    },
                },
            }),
        )
        .await;
        let store = SagaStore::new("/repo");
        let hits = store.search(fs.as_ref(), "NEEDLE").await.expect("search");
        assert_eq!(hits.len(), MAX_SEARCH_HITS);
        assert!(hits.iter().all(|hit| hit.path.starts_with(".hulder/")));
        assert!(hits.iter().all(|hit| !hit.path.contains("bin/")));
        assert!(hits.iter().all(|hit| !hit.path.ends_with("saga.json")));
        assert!(hits.iter().all(|hit| !hit.path.contains("0002-big")));
        assert!(
            hits.iter()
                .any(|hit| hit.path == ".hulder/NOTES.md" && hit.line == 1)
        );
        assert!(hits.iter().any(|hit| hit.path == ".hulder/TODO.md"));
        assert!(store.search(fs.as_ref(), "  ").await.is_err());
    }

    #[gpui::test]
    async fn lore_names_and_reads_go_through_the_guard(cx: &mut TestAppContext) {
        let fs = populated_fs(cx).await;
        fs.insert_symlink(
            "/repo/.hulder/notes/linked.md",
            PathBuf::from("/repo/.hulder/NOTES.md"),
        )
        .await;
        let store = SagaStore::new("/repo");
        assert_eq!(
            store.lore_names(fs.as_ref()).await.expect("names"),
            vec!["NOTES.md", "ideas.md", "notes/2026-10-13.md"]
        );
        assert_eq!(
            store
                .read_lore(fs.as_ref(), "NOTES.md")
                .await
                .expect("read"),
            "# Notes\n"
        );
        assert_eq!(
            store
                .read_lore(fs.as_ref(), "notes/2026-10-13.md")
                .await
                .expect("read"),
            "# day\n"
        );
        for bad in [
            "notes/linked.md",
            "notes/none.md",
            "TODO.md",
            "tickets/0001-first.md",
            "../secret.md",
            "clippings/2026-10-13-new-page.md",
            "bin/out.md",
            "saga.json",
        ] {
            assert!(store.read_lore(fs.as_ref(), bad).await.is_err(), "{bad}");
        }
        assert_eq!(
            store
                .read_at(
                    fs.as_ref(),
                    Path::new("/repo/.hulder/tickets/0001-first.md")
                )
                .await
                .expect("read tale by path"),
            tale_text(1, "First", "open")
        );
        assert!(
            store
                .read_at(fs.as_ref(), Path::new("/repo/.git/info/exclude"))
                .await
                .is_err(),
            "paths outside .hulder are refused"
        );
    }
}
