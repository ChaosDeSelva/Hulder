//! Reads and writes clippings under `<root>/.hulder/clippings/` through
//! `fs::Fs`. No gpui here so a non-UI crate can drive the same files.
//!
//! The guards mirror `saga::store`: every path is a plain relative path
//! under `clippings/`, neither `.hulder` nor anything between it and the
//! target may be a symlink, Markdown goes through `Fs::atomic_write`, and
//! reads and writes are capped. Agents write into `.hulder/`, so a planted
//! link must never redirect a capture elsewhere.

use crate::model::{
    ASSETS_DIR, Clipping, DIR, Kind, MAX_ASSET_BYTES, MAX_LISTED_FILES, MAX_MARKDOWN_BYTES,
    asset_file_name, file_name, is_valid_clipping_name, numbered_stem, public_source, slug_for,
    stem,
};
use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Local, SecondsFormat};
use fs::Fs;
use futures::StreamExt as _;
use gpui::BackgroundExecutor;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

pub const HULDER_DIR: &str = ".hulder";
/// How many `-2`, `-3`, ... suffixes we try before giving up on a name.
const MAX_NAME_ATTEMPTS: usize = 1000;

/// Builds the Markdown body once the store knows where the screenshot
/// landed; the argument is the asset path relative to the clipping file,
/// `None` when there is no screenshot.
pub type BodyBuilder = Box<dyn FnOnce(Option<&Path>) -> String + Send + 'static>;

pub struct ClippingDraft {
    pub kind: Kind,
    /// A URL for picks and fetches, an absolute path for PDFs.
    pub source: String,
    pub selector: Option<String>,
    /// Drives the slug; the source is used when it is `None`.
    pub title: Option<String>,
    pub screenshot_png: Option<Vec<u8>>,
    /// Further front matter lines; see `Clipping::extra`.
    pub extra: Vec<(String, String)>,
    pub body: BodyBuilder,
}

impl ClippingDraft {
    /// A draft whose body needs no asset path.
    pub fn text(kind: Kind, source: impl Into<String>, body: String) -> Self {
        Self {
            kind,
            source: source.into(),
            selector: None,
            title: None,
            screenshot_png: None,
            extra: Vec::new(),
            body: Box::new(move |_| body),
        }
    }

    pub fn with_extra(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra.push((key.into(), value.into()));
        self
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

enum GuardedWrite {
    Written(PathBuf),
    /// The target already existed and the write was not allowed to replace it.
    Collision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedClipping {
    pub path: PathBuf,
    pub asset_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClippingStore {
    root: PathBuf,
}

/// A relative path under `clippings/` with only plain components.
pub fn resolve(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.is_empty() {
        bail!("empty clipping path");
    }
    let relative_path = Path::new(relative);
    if relative_path.is_absolute() || relative.starts_with('/') || relative.starts_with('\\') {
        bail!("clipping path {relative:?} must be relative");
    }
    let dir = root.join(HULDER_DIR).join(DIR);
    let mut resolved = dir.clone();
    for component in relative_path.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                if name.contains('\\') || name.contains('/') || name.contains('\0') {
                    bail!("clipping path {relative:?} has a separator inside a name");
                }
                resolved.push(name.as_ref());
            }
            Component::CurDir => continue,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("clipping path {relative:?} must not leave clippings/")
            }
        }
    }
    if resolved == dir {
        bail!("clipping path {relative:?} names the folder itself");
    }
    Ok(resolved)
}

async fn is_symlink(fs: &dyn Fs, path: &Path) -> Result<bool> {
    Ok(fs
        .metadata(path)
        .await
        .with_context(|| format!("stat {}", path.display()))?
        .is_some_and(|metadata| metadata.is_symlink))
}

async fn exists(fs: &dyn Fs, path: &Path) -> Result<bool> {
    Ok(fs.metadata(path).await?.is_some())
}

impl ClippingStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join(HULDER_DIR).join(DIR)
    }

    pub fn assets_dir(&self) -> PathBuf {
        self.dir().join(ASSETS_DIR)
    }

    pub fn resolve(&self, relative: &str) -> Result<PathBuf> {
        resolve(&self.root, relative)
    }

    /// Resolves `relative` and refuses when `.hulder` or anything from it
    /// down to the target is a symlink, then checks that the target's parent
    /// (when it exists) canonicalizes inside the canonical clippings
    /// directory. Missing parts are fine: the caller is about to create
    /// them. Callers re-run this right before every read or write so the
    /// window between check and use is as small as the `Fs` API allows.
    pub async fn guarded(&self, fs: &dyn Fs, relative: &str) -> Result<PathBuf> {
        let target = self.resolve(relative)?;
        let mut cursor = self.root.join(HULDER_DIR);
        if is_symlink(fs, &cursor).await? {
            bail!("{} is a symlink; refusing to use it", cursor.display());
        }
        let Ok(inside) = target.strip_prefix(&cursor) else {
            bail!("clipping path {relative:?} resolved outside .hulder");
        };
        for component in inside.components() {
            cursor.push(component);
            if is_symlink(fs, &cursor).await? {
                bail!("{} is a symlink; refusing to use it", cursor.display());
            }
        }
        let dir = self.dir();
        if let Some(parent) = target.parent()
            && exists(fs, parent).await?
        {
            let canonical_parent = fs.canonicalize(parent).await?;
            let canonical_dir = fs.canonicalize(&dir).await?;
            if !canonical_parent.starts_with(&canonical_dir) {
                bail!(
                    "{} resolves outside {}; refusing to use it",
                    parent.display(),
                    dir.display()
                );
            }
        }
        Ok(target)
    }

    /// Writes bytes the way `Fs::atomic_write` writes text: into a fresh
    /// temp name in the same directory, then a rename onto the target after
    /// a fresh guard, so a link planted at the target between the check and
    /// the rename is replaced, never followed. Without `overwrite` the rename
    /// is `create_new`-like and an existing target is a `Collision`, so two
    /// saves racing for one name cannot clobber each other. A failed write
    /// or rename leaves no temp file behind.
    async fn write_bytes_guarded(
        &self,
        fs: &dyn Fs,
        relative: &str,
        bytes: &[u8],
        overwrite: bool,
    ) -> Result<GuardedWrite> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let temp_relative = format!("{relative}.{nonce:x}.tmp");
        let temp_path = self.guarded(fs, &temp_relative).await?;
        let written = fs.write(&temp_path, bytes).await;
        let renamed = match written {
            Ok(()) => match self.guarded(fs, relative).await {
                Ok(target) => fs
                    .rename(
                        &temp_path,
                        &target,
                        fs::RenameOptions {
                            overwrite,
                            ignore_if_exists: false,
                            create_parents: false,
                        },
                    )
                    .await
                    .map(|()| target),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        match renamed {
            Ok(target) => Ok(GuardedWrite::Written(target)),
            Err(error) => {
                fs.remove_file(
                    &temp_path,
                    fs::RemoveOptions {
                        recursive: false,
                        ignore_if_not_exists: true,
                    },
                )
                .await
                .with_context(|| format!("cleaning up {}", temp_path.display()))?;
                let target = self.resolve(relative)?;
                if !overwrite && exists(fs, &target).await? {
                    return Ok(GuardedWrite::Collision);
                }
                Err(error)
            }
        }
    }

    async fn remove_quietly(&self, fs: &dyn Fs, path: &Path) {
        if let Err(error) = fs.remove_file(path, fs::RemoveOptions::default()).await {
            log::warn!("clippings: cleaning up {}: {error:#}", path.display());
        }
    }

    /// The first `<date>-<slug>` (then `-2`, `-3`, ...) with neither a
    /// Markdown file nor an asset already using it.
    pub async fn unique_stem(&self, fs: &dyn Fs, date: &str, slug: &str) -> Result<String> {
        let base = stem(date, slug);
        if !is_valid_clipping_name(&file_name(&base)) {
            bail!("generated clipping name {:?} is not valid", file_name(&base));
        }
        for attempt in 1..=MAX_NAME_ATTEMPTS {
            let candidate = if attempt == 1 {
                base.clone()
            } else {
                numbered_stem(&base, attempt)
            };
            let markdown = self.guarded(fs, &file_name(&candidate)).await?;
            let asset = self
                .guarded(fs, &format!("{ASSETS_DIR}/{}", asset_file_name(&candidate, "png")))
                .await?;
            if !exists(fs, &markdown).await? && !exists(fs, &asset).await? {
                return Ok(candidate);
            }
        }
        bail!("more than {MAX_NAME_ATTEMPTS} clippings named {base:?} today")
    }

    /// Writes the screenshot (when there is one) and then the Markdown file,
    /// both by temp-file-and-rename so a link at the final component is
    /// replaced rather than followed, each after a fresh guard.
    pub async fn save(
        &self,
        fs: &dyn Fs,
        draft: ClippingDraft,
        captured: DateTime<Local>,
    ) -> Result<SavedClipping> {
        if let Some(screenshot) = &draft.screenshot_png
            && screenshot.len() as u64 > MAX_ASSET_BYTES
        {
            bail!(
                "screenshot is {} bytes, larger than the {MAX_ASSET_BYTES} byte limit",
                screenshot.len()
            );
        }
        let date = captured.format("%Y-%m-%d").to_string();
        let (source, query_present) = public_source(draft.kind, &draft.source);
        let slug = slug_for(draft.kind, &source, draft.title.as_deref());

        let dir = self.guarded(fs, &file_name(&stem(&date, &slug))).await?;
        let dir = dir
            .parent()
            .context("clipping path has no parent")?
            .to_path_buf();
        fs.create_dir(&dir).await?;

        // The name is claimed with an empty file created `create_new`-style;
        // a save racing for the same name sees the collision and moves on to
        // the next number instead of overwriting.
        let mut claimed = None;
        for _ in 0..MAX_NAME_ATTEMPTS {
            let stem = self.unique_stem(fs, &date, &slug).await?;
            match self
                .write_bytes_guarded(fs, &file_name(&stem), b"", false)
                .await?
            {
                GuardedWrite::Written(path) => {
                    claimed = Some((stem, path));
                    break;
                }
                GuardedWrite::Collision => continue,
            }
        }
        let Some((stem, path)) = claimed else {
            bail!("could not claim a clipping name for {slug:?} today");
        };

        let mut asset_path = None;
        let mut asset_relative = None;
        if let Some(screenshot) = draft.screenshot_png.filter(|bytes| !bytes.is_empty()) {
            let relative = format!("{ASSETS_DIR}/{}", asset_file_name(&stem, "png"));
            let written = async {
                let assets_dir = self.guarded(fs, ASSETS_DIR).await?;
                fs.create_dir(&assets_dir).await?;
                self.write_bytes_guarded(fs, &relative, &screenshot, false)
                    .await
            }
            .await;
            match written {
                Ok(GuardedWrite::Written(asset)) => asset_path = Some(asset),
                Ok(GuardedWrite::Collision) => {
                    self.remove_quietly(fs, &path).await;
                    bail!("{relative} already exists");
                }
                Err(error) => {
                    self.remove_quietly(fs, &path).await;
                    return Err(error);
                }
            }
            asset_relative = Some(PathBuf::from(relative));
        }

        let body = (draft.body)(asset_relative.as_deref());
        let clipping = Clipping {
            source,
            captured: captured.to_rfc3339_opts(SecondsFormat::Secs, true),
            kind: draft.kind,
            selector: draft.selector,
            query_present,
            extra: draft.extra,
            body,
        };
        let text = clipping.serialize();
        let filled = if text.len() as u64 > MAX_MARKDOWN_BYTES {
            Err(anyhow::anyhow!(
                "clipping is {} bytes, larger than the {MAX_MARKDOWN_BYTES} byte limit",
                text.len()
            ))
        } else {
            self.write_bytes_guarded(fs, &file_name(&stem), text.as_bytes(), true)
                .await
        };
        let filled = filled.and_then(|write| match write {
            GuardedWrite::Written(path) => Ok(path),
            GuardedWrite::Collision => {
                Err(anyhow::anyhow!("{} vanished while being filled", path.display()))
            }
        });
        match filled {
            Ok(path) => Ok(SavedClipping { path, asset_path }),
            Err(error) => {
                self.remove_quietly(fs, &path).await;
                if let Some(asset) = &asset_path {
                    self.remove_quietly(fs, asset).await;
                }
                Err(error)
            }
        }
    }

    /// Clipping files directly under `clippings/`, newest name first, never
    /// following symlinks, at most `MAX_LISTED_FILES` of them.
    pub async fn list(&self, fs: &dyn Fs) -> Result<Vec<PathBuf>> {
        let dir = self.dir();
        let hulder = self.root.join(HULDER_DIR);
        if is_symlink(fs, &hulder).await? || is_symlink(fs, &dir).await? {
            bail!("{} is a symlink; refusing to use it", dir.display());
        }
        let mut files = Vec::new();
        if !fs.is_dir(&dir).await {
            return Ok(files);
        }
        let mut stream = fs.read_dir(&dir).await?;
        let mut seen = 0usize;
        while let Some(entry) = stream.next().await {
            seen += 1;
            if seen > MAX_LISTED_FILES {
                log::warn!(
                    "clippings: more than {MAX_LISTED_FILES} entries in {}; ignoring the rest",
                    dir.display()
                );
                break;
            }
            let path = entry?;
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !is_valid_clipping_name(&name) {
                continue;
            }
            let Some(metadata) = fs.metadata(&path).await? else {
                continue;
            };
            if metadata.is_symlink || metadata.is_dir {
                log::warn!("clippings: skipping {}", path.display());
                continue;
            }
            files.push(path);
        }
        files.sort();
        files.reverse();
        Ok(files)
    }

    /// Reads one clipping by file name, refusing anything that is not a
    /// valid clipping name, a symlink, or larger than the Markdown cap. The
    /// cap is on the bytes actually read, so a file growing after the stat
    /// cannot bypass it.
    pub async fn read(
        &self,
        fs: &dyn Fs,
        executor: &BackgroundExecutor,
        name: &str,
    ) -> Result<String> {
        if !is_valid_clipping_name(name) {
            bail!("{name:?} is not a clipping name (YYYY-MM-DD-<slug>.md)");
        }
        let path = self.guarded(fs, name).await?;
        // The open itself refuses a link or special file at the final
        // component, so nothing planted between the guard and the open can
        // redirect the read.
        let reader = fs.open_sync_nofollow(&path).await?;
        let bytes = executor
            .spawn(async move {
                let mut bytes = Vec::new();
                reader
                    .take(MAX_MARKDOWN_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                anyhow::Ok(bytes)
            })
            .await?;
        if bytes.len() as u64 > MAX_MARKDOWN_BYTES {
            bail!(
                "{} is larger than the {MAX_MARKDOWN_BYTES} byte limit",
                path.display()
            );
        }
        String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use std::sync::Arc;

    fn captured() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 13, 9, 30, 0)
            .single()
            .expect("valid time")
    }

    async fn repo(cx: &TestAppContext) -> Arc<FakeFs> {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({ ".hulder": { "NOTES.md": "# Notes\n" } }))
            .await;
        fs
    }

    fn pick_draft(png: Vec<u8>) -> ClippingDraft {
        ClippingDraft {
            kind: Kind::Pick,
            source: "https://example.com/".into(),
            selector: Some("#main > p.lead".into()),
            title: Some("#main > p.lead".into()),
            screenshot_png: Some(png),
            extra: Vec::new(),
            body: Box::new(|asset| match asset {
                Some(asset) => format!("# pick\n\n![shot]({})\n", asset.display()),
                None => "# pick\n\nno shot\n".into(),
            }),
        }
    }

    #[gpui::test]
    async fn save_writes_markdown_and_asset_with_relative_link(cx: &mut TestAppContext) {
        let fs = repo(cx).await;
        let store = ClippingStore::new("/repo");
        let saved = store
            .save(fs.as_ref(), pick_draft(vec![1, 2, 3]), captured())
            .await
            .expect("save");
        assert_eq!(
            saved.path,
            PathBuf::from("/repo/.hulder/clippings/2026-10-13-main-p-lead.md")
        );
        assert_eq!(
            saved.asset_path,
            Some(PathBuf::from(
                "/repo/.hulder/clippings/assets/2026-10-13-main-p-lead.png"
            ))
        );
        let text = fs.load(&saved.path).await.expect("load");
        let clipping = Clipping::parse(&text).expect("parse");
        assert_eq!(clipping.kind, Kind::Pick);
        assert_eq!(clipping.source, "https://example.com/");
        assert_eq!(clipping.selector.as_deref(), Some("#main > p.lead"));
        assert!(clipping.captured.starts_with("2026-10-13T"));
        assert!(
            clipping
                .body
                .contains("![shot](assets/2026-10-13-main-p-lead.png)")
        );
        let png = fs
            .load_bytes(saved.asset_path.as_ref().expect("asset"))
            .await
            .expect("png");
        assert_eq!(png, vec![1, 2, 3]);
    }

    #[gpui::test]
    async fn same_slug_twice_gets_a_number_and_no_asset_without_screenshot(
        cx: &mut TestAppContext,
    ) {
        let fs = repo(cx).await;
        let store = ClippingStore::new("/repo");
        let first = ClippingDraft::text(Kind::Fetch, "https://docs.rs/gpui", "one".into())
            .with_title("GPUI docs");
        let second = ClippingDraft::text(Kind::Fetch, "https://docs.rs/gpui", "two".into())
            .with_title("GPUI docs");
        let first = store.save(fs.as_ref(), first, captured()).await.expect("one");
        let second = store
            .save(fs.as_ref(), second, captured())
            .await
            .expect("two");
        assert_eq!(
            first.path,
            PathBuf::from("/repo/.hulder/clippings/2026-10-13-gpui-docs.md")
        );
        assert_eq!(
            second.path,
            PathBuf::from("/repo/.hulder/clippings/2026-10-13-gpui-docs-2.md")
        );
        assert_eq!(second.asset_path, None);
        assert!(!fs.is_dir(&store.assets_dir()).await);
        let text = fs.load(&second.path).await.expect("load");
        assert!(text.ends_with("---\ntwo"));
        assert!(!text.contains("selector:"));
    }

    #[gpui::test]
    async fn saved_source_and_name_carry_no_query_or_userinfo(cx: &mut TestAppContext) {
        let fs = repo(cx).await;
        let store = ClippingStore::new("/repo");
        let draft = ClippingDraft::text(
            Kind::Fetch,
            "https://alice:hunter2@example.com/reset?reset-token=abc123#frag",
            "body".into(),
        )
        .with_extra("status", "200");
        let saved = store.save(fs.as_ref(), draft, captured()).await.expect("save");
        assert_eq!(
            saved.path,
            PathBuf::from("/repo/.hulder/clippings/2026-10-13-example-com-reset.md")
        );
        let text = fs.load(&saved.path).await.expect("load");
        for secret in ["abc123", "hunter2", "alice", "frag", "reset-token"] {
            assert!(!text.contains(secret), "{secret} leaked into {text}");
        }
        let clipping = Clipping::parse(&text).expect("parse");
        assert_eq!(clipping.source, "https://example.com/reset");
        assert!(clipping.query_present);
        assert_eq!(clipping.extra, vec![("status".to_string(), "200".to_string())]);
        assert!(
            fs.files()
                .iter()
                .all(|path| !path.to_string_lossy().ends_with(".tmp")),
            "{:?}",
            fs.files()
        );
    }

    #[gpui::test(iterations = 10)]
    async fn concurrent_saves_with_the_same_name_never_overwrite(cx: &mut TestAppContext) {
        let fs = repo(cx).await;
        let store = ClippingStore::new("/repo");
        let first = ClippingDraft::text(Kind::Fetch, "https://docs.rs/gpui", "one".into())
            .with_title("GPUI docs");
        let second = ClippingDraft::text(Kind::Fetch, "https://docs.rs/gpui", "two".into())
            .with_title("GPUI docs");
        let (first, second) = futures::join!(
            store.save(fs.as_ref(), first, captured()),
            store.save(fs.as_ref(), second, captured())
        );
        let first = first.expect("one");
        let second = second.expect("two");
        assert_ne!(first.path, second.path);
        let first_text = fs.load(&first.path).await.expect("load one");
        let second_text = fs.load(&second.path).await.expect("load two");
        assert!(first_text.ends_with("---\none"), "{first_text}");
        assert!(second_text.ends_with("---\ntwo"), "{second_text}");
        assert_eq!(store.list(fs.as_ref()).await.expect("list").len(), 2);
    }

    #[gpui::test]
    async fn oversize_body_and_asset_are_refused(cx: &mut TestAppContext) {
        let fs = repo(cx).await;
        let store = ClippingStore::new("/repo");
        let huge_png = vec![0u8; MAX_ASSET_BYTES as usize + 1];
        let error = store
            .save(fs.as_ref(), pick_draft(huge_png), captured())
            .await
            .expect_err("asset too big");
        assert!(error.to_string().contains("larger than"), "{error:#}");

        let huge_body = "x".repeat(MAX_MARKDOWN_BYTES as usize + 1);
        let error = store
            .save(
                fs.as_ref(),
                ClippingDraft::text(Kind::Pdf, "/a/b.pdf", huge_body),
                captured(),
            )
            .await
            .expect_err("body too big");
        assert!(error.to_string().contains("larger than"), "{error:#}");
        assert!(store.list(fs.as_ref()).await.expect("list").is_empty());
        assert!(!fs.is_dir(&store.assets_dir()).await);
    }

    #[gpui::test]
    async fn list_keeps_only_clipping_files_newest_first(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({
                ".hulder": {
                    "clippings": {
                        "2026-10-12-older.md": "---\nsource: a\nkind: fetch\n---\n",
                        "2026-10-13-newer.md": "---\nsource: b\nkind: fetch\n---\n",
                        "README.md": "not a clipping",
                        "2026-10-13-Upper.md": "bad name",
                        "2026-10-13-note.txt": "bad extension",
                        "assets": { "2026-10-13-newer.png": "png" },
                    }
                }
            }),
        )
        .await;
        fs.insert_symlink(
            "/repo/.hulder/clippings/2026-10-13-link.md",
            PathBuf::from("/etc/passwd"),
        )
        .await;
        let store = ClippingStore::new("/repo");
        assert_eq!(
            store.list(fs.as_ref()).await.expect("list"),
            vec![
                PathBuf::from("/repo/.hulder/clippings/2026-10-13-newer.md"),
                PathBuf::from("/repo/.hulder/clippings/2026-10-12-older.md"),
            ]
        );
        let executor = cx.executor();
        assert_eq!(
            store
                .read(fs.as_ref(), &executor, "2026-10-12-older.md")
                .await
                .expect("read"),
            "---\nsource: a\nkind: fetch\n---\n"
        );
        for name in [
            "README.md",
            "2026-10-13-link.md",
            "../NOTES.md",
            "assets/2026-10-13-newer.png",
            "2026-10-13-missing.md",
        ] {
            assert!(
                store.read(fs.as_ref(), &executor, name).await.is_err(),
                "{name}"
            );
        }
        let empty = ClippingStore::new("/nowhere");
        assert!(empty.list(fs.as_ref()).await.expect("list").is_empty());
    }

    #[gpui::test]
    async fn listing_stops_at_the_cap(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({ ".hulder": { "clippings": {} } }))
            .await;
        for index in 0..(MAX_LISTED_FILES + 5) {
            fs.insert_file(
                format!("/repo/.hulder/clippings/2026-10-13-n{index:05}.md"),
                b"---\nsource: a\nkind: fetch\n---\n".to_vec(),
            )
            .await;
        }
        let store = ClippingStore::new("/repo");
        assert_eq!(
            store.list(fs.as_ref()).await.expect("list").len(),
            MAX_LISTED_FILES
        );
    }

    #[gpui::test]
    async fn symlinked_folders_are_refused(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({ ".hulder": { "NOTES.md": "n" }, "elsewhere": {} }),
        )
        .await;
        fs.insert_symlink(
            "/repo/.hulder/clippings",
            PathBuf::from("/repo/elsewhere"),
        )
        .await;
        let store = ClippingStore::new("/repo");
        assert!(store.list(fs.as_ref()).await.is_err());
        assert!(
            store
                .save(
                    fs.as_ref(),
                    ClippingDraft::text(Kind::Fetch, "https://x.test/", "b".into()),
                    captured()
                )
                .await
                .is_err()
        );
        assert!(fs.files().iter().all(|path| !path.starts_with("/repo/elsewhere")));

        assert!(resolve(Path::new("/repo"), "").is_err());
        assert!(resolve(Path::new("/repo"), "/abs.md").is_err());
        assert!(resolve(Path::new("/repo"), "a/../../x.md").is_err());
        assert!(resolve(Path::new("/repo"), ".").is_err());
        assert_eq!(
            resolve(Path::new("/repo"), "./2026-10-13-a.md").expect("plain"),
            PathBuf::from("/repo/.hulder/clippings/2026-10-13-a.md")
        );
    }

    #[gpui::test]
    async fn symlinked_file_is_never_read_or_written_through(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({
                ".hulder": { "clippings": { "assets": {} } },
                "secret.md": "keep me",
                "secret.png": "keep me too",
            }),
        )
        .await;
        fs.insert_symlink(
            "/repo/.hulder/clippings/2026-10-13-main-p-lead.md",
            PathBuf::from("/repo/secret.md"),
        )
        .await;
        fs.insert_symlink(
            "/repo/.hulder/clippings/assets/2026-10-13-main-p-lead.png",
            PathBuf::from("/repo/secret.png"),
        )
        .await;
        let store = ClippingStore::new("/repo");

        let error = store
            .read(fs.as_ref(), &cx.executor(), "2026-10-13-main-p-lead.md")
            .await
            .expect_err("symlinked clipping");
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert!(store.list(fs.as_ref()).await.expect("list").is_empty());

        let error = store
            .save(fs.as_ref(), pick_draft(vec![9]), captured())
            .await
            .expect_err("a link at the target name refuses the save");
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(
            fs.load(Path::new("/repo/secret.md")).await.expect("load"),
            "keep me"
        );
        assert_eq!(
            fs.load(Path::new("/repo/secret.png")).await.expect("load"),
            "keep me too"
        );
        assert!(
            fs.files()
                .iter()
                .all(|path| !path.to_string_lossy().ends_with(".tmp")),
            "temp files are renamed away"
        );
    }

    #[gpui::test]
    async fn symlinked_assets_dir_is_refused(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({ ".hulder": { "clippings": {} }, "elsewhere": {} }),
        )
        .await;
        fs.insert_symlink(
            "/repo/.hulder/clippings/assets",
            PathBuf::from("/repo/elsewhere"),
        )
        .await;
        let store = ClippingStore::new("/repo");
        let error = store
            .save(fs.as_ref(), pick_draft(vec![9]), captured())
            .await
            .expect_err("assets is a symlink");
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert!(
            fs.files()
                .iter()
                .all(|path| !path.starts_with("/repo/elsewhere")),
            "{:?}",
            fs.files()
        );
        assert!(store.list(fs.as_ref()).await.expect("list").is_empty());
        assert!(
            store.guarded(fs.as_ref(), "assets/x.png").await.is_err(),
            "guard refuses the link before any write"
        );
    }
}
