//! Files under `<root>/.hulder/ravens/` and `<root>/.hulder/clippings/`.
//! Reads are plain `std::fs` so Bifröst can call them from a background
//! thread; every path goes through [`resolve`] (relative, plain components,
//! no symlink on any existing component). Writes go through
//! `Fs::atomic_write`, which creates a randomly named temp file in the
//! target directory and renames it over the target, so a planted link can
//! never redirect a write and no temp name can be guessed in advance.

use crate::collection::{Collection, Environments, parse, serialize};
use anyhow::{Context as _, Result, bail};
use fs::{Fs, RenameOptions};
use std::path::{Component, Path, PathBuf};

pub const HULDER_DIR: &str = ".hulder";
pub const RAVENS_DIR: &str = "ravens";
pub const ENV_FILE: &str = "env.json";
pub const LOG_FILE: &str = "log.jsonl";
pub const ROTATED_LOG_FILE: &str = "log.1.jsonl";
pub const HTTP_EXTENSION: &str = "http";
/// Largest `.http`, `env.json` or log file we read.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// `log.jsonl` is rotated to `log.1.jsonl` once it would pass this.
pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

fn refuse_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("{} is a symlink; refusing to touch it", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

/// `relative` is joined under `<root>/.hulder`; absolute paths, `..`,
/// separators inside a name and symlinks anywhere on the way are refused.
pub fn resolve(root: &Path, relative: &str) -> Result<PathBuf> {
    if relative.trim().is_empty() {
        bail!("empty raven path");
    }
    let relative_path = Path::new(relative);
    if relative_path.is_absolute() || relative.starts_with('/') || relative.starts_with('\\') {
        bail!("raven path {relative:?} must be relative");
    }
    let mut path = root.join(HULDER_DIR);
    refuse_symlink(&path)?;
    for component in relative_path.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                if name.contains('\\') || name.contains('/') || name.contains('\0') {
                    bail!("raven path {relative:?} has a separator inside a name");
                }
                path.push(name.as_ref());
                refuse_symlink(&path)?;
            }
            Component::CurDir => {}
            _ => bail!("raven path {relative:?} must not leave .hulder"),
        }
    }
    if path == root.join(HULDER_DIR) {
        bail!("raven path {relative:?} names the folder itself");
    }
    Ok(path)
}

/// A collection or clipping file name: letters, digits, `-`, `_`, `.`,
/// not hidden, no separators.
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
}

fn read_bounded(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("{} is a symlink; refusing to read it", path.display());
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!(
            "{} is {} bytes, larger than the {MAX_FILE_BYTES} byte limit",
            path.display(),
            metadata.len()
        );
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

pub fn ravens_dir(root: &Path) -> PathBuf {
    root.join(HULDER_DIR).join(RAVENS_DIR)
}

/// Every `<name>.http` under `.hulder/ravens/`, sorted by name. Symlinks
/// and odd names are skipped with a log line.
pub fn list_collections(root: &Path) -> Result<Vec<Collection>> {
    let dir = resolve(root, RAVENS_DIR)?;
    let mut collections = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(collections),
        Err(error) => return Err(error).with_context(|| format!("listing {}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = file_name.strip_suffix(&format!(".{HTTP_EXTENSION}")) else {
            continue;
        };
        if !is_plain_name(stem) {
            log::warn!("raven: skipping {} (unusual name)", entry.path().display());
            continue;
        }
        names.push(stem.to_string());
    }
    names.sort();
    for name in names {
        let path = match resolve(root, &format!("{RAVENS_DIR}/{name}.{HTTP_EXTENSION}")) {
            Ok(path) => path,
            Err(error) => {
                log::warn!("raven: skipping {name}: {error:#}");
                continue;
            }
        };
        match read_bounded(&path) {
            Ok(text) => collections.push(parse(&name, &text)),
            Err(error) => log::warn!("raven: skipping {}: {error:#}", path.display()),
        }
    }
    Ok(collections)
}

pub fn load_environments(root: &Path) -> Result<Environments> {
    let path = resolve(root, &format!("{RAVENS_DIR}/{ENV_FILE}"))?;
    if !path.exists() {
        return Ok(Environments::default());
    }
    Environments::parse(&read_bounded(&path)?)
}

/// Writes `text` to `relative` through `Fs::atomic_write` after creating
/// the directories on the way. The parent chain and the target are checked
/// for symlinks by `resolve` right before the write.
pub async fn write_atomic(fs: &dyn Fs, root: &Path, relative: &str, text: &str) -> Result<PathBuf> {
    let path = resolve(root, relative)?;
    let directory = path
        .parent()
        .with_context(|| format!("{} has no parent", path.display()))?;
    fs.create_dir(directory)
        .await
        .with_context(|| format!("creating {}", directory.display()))?;
    // `resolve` checked the chain once; check again now that the directories
    // exist, as close to the rename as the `Fs` API allows.
    resolve(root, relative)?;
    fs.atomic_write(path.clone(), text.to_string())
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub async fn write_collection(fs: &dyn Fs, root: &Path, collection: &Collection) -> Result<PathBuf> {
    if !is_plain_name(&collection.name) {
        bail!("{:?} is not a valid collection name", collection.name);
    }
    write_atomic(
        fs,
        root,
        &format!("{RAVENS_DIR}/{}.{HTTP_EXTENSION}", collection.name),
        &serialize(collection),
    )
    .await
}

/// Appends one line to `log.jsonl`, first renaming it to `log.1.jsonl` when
/// the line would push it past [`MAX_LOG_BYTES`]. The append opens the file
/// with `O_NOFOLLOW` so a planted link is refused rather than followed; no
/// temp file is involved.
pub async fn append_log_line(fs: &dyn Fs, root: &Path, line: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let path = resolve(root, &format!("{RAVENS_DIR}/{LOG_FILE}"))?;
    let directory = path
        .parent()
        .with_context(|| format!("{} has no parent", path.display()))?;
    fs.create_dir(directory)
        .await
        .with_context(|| format!("creating {}", directory.display()))?;
    let current_len = std::fs::symlink_metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if current_len > 0 && current_len + line.len() as u64 + 1 > MAX_LOG_BYTES {
        let rotated = resolve(root, &format!("{RAVENS_DIR}/{ROTATED_LOG_FILE}"))?;
        fs.rename(
            &path,
            &rotated,
            RenameOptions {
                overwrite: true,
                ..RenameOptions::default()
            },
        )
        .await
        .with_context(|| format!("rotating {}", path.display()))?;
    }
    let path = resolve(root, &format!("{RAVENS_DIR}/{LOG_FILE}"))?;
    let line = line.to_string();
    smol::unblock(move || {
        let mut handle = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        writeln!(handle, "{line}").with_context(|| format!("appending to {}", path.display()))
    })
    .await
}

/// The log's lines, oldest first (`log.1.jsonl` then `log.jsonl`).
pub fn read_log_lines(root: &Path) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for file in [ROTATED_LOG_FILE, LOG_FILE] {
        let path = resolve(root, &format!("{RAVENS_DIR}/{file}"))?;
        if !path.exists() {
            continue;
        }
        lines.extend(read_bounded(&path)?.lines().map(str::to_string));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collection::Request;
    use gpui::TestAppContext;
    use std::sync::Arc;

    /// `RealFs` blocks on real disk I/O, which the test scheduler forbids
    /// unless parking is allowed; these tests want the real filesystem for
    /// its symlink behaviour.
    fn real_fs(cx: &TestAppContext) -> Arc<dyn Fs> {
        cx.executor().allow_parking();
        fs::RealFs::new(None, cx.executor())
    }

    fn collection(name: &str) -> Collection {
        Collection {
            name: name.into(),
            requests: vec![Request {
                name: "ping".into(),
                method: "GET".into(),
                url: "https://a.test/ping".into(),
                headers: Vec::new(),
                body: String::new(),
            }],
        }
    }

    #[gpui::test]
    async fn collections_round_trip_through_the_store_sorted(cx: &mut TestAppContext) {
        let fs = real_fs(cx);
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        assert!(list_collections(root).expect("empty").is_empty());
        write_collection(fs.as_ref(), root, &collection("users")).await.expect("write");
        write_collection(fs.as_ref(), root, &collection("auth")).await.expect("write");
        std::fs::write(ravens_dir(root).join("notes.txt"), "ignored").expect("write");
        let listed = list_collections(root).expect("list");
        assert_eq!(
            listed.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["auth", "users"]
        );
        assert_eq!(listed[0].requests[0].url, "https://a.test/ping");
        assert!(write_collection(fs.as_ref(), root, &collection("../escape")).await.is_err());
        assert!(write_collection(fs.as_ref(), root, &collection(".hidden")).await.is_err());
        let leftovers: Vec<_> = std::fs::read_dir(ravens_dir(root))
            .expect("dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| !entry.file_name().to_string_lossy().ends_with(".http"))
            .filter(|entry| entry.file_name() != "notes.txt")
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[gpui::test]
    async fn resolve_refuses_escapes_and_symlinks(cx: &mut TestAppContext) {
        let fs = real_fs(cx);
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        assert!(resolve(root, "../x").is_err());
        assert!(resolve(root, "/etc/passwd").is_err());
        assert!(resolve(root, "ravens/../../x").is_err());
        assert!(resolve(root, "").is_err());
        assert!(resolve(root, ".").is_err());
        assert_eq!(
            resolve(root, "ravens/a.http").expect("ok"),
            root.join(".hulder/ravens/a.http")
        );
        std::fs::create_dir_all(root.join(".hulder")).expect("mkdir");
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), root.join(".hulder/ravens")).expect("link");
        let error = resolve(root, "ravens/a.http").expect_err("symlinked dir");
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert!(write_collection(fs.as_ref(), root, &collection("a")).await.is_err());
        assert!(
            outside.path().read_dir().expect("dir").next().is_none(),
            "nothing written outside"
        );
    }

    #[gpui::test]
    async fn write_atomic_replaces_content_and_refuses_a_symlinked_target(cx: &mut TestAppContext) {
        let fs = real_fs(cx);
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let path = write_atomic(fs.as_ref(), root, "ravens/env.json", "{}").await.expect("write");
        write_atomic(fs.as_ref(), root, "ravens/env.json", "{\"local\":{}}")
            .await
            .expect("rewrite");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{\"local\":{}}");
        let outside = temp.path().join("outside.json");
        std::fs::write(&outside, "keep").expect("write");
        std::fs::remove_file(&path).expect("remove");
        std::os::unix::fs::symlink(&outside, &path).expect("link");
        assert!(write_atomic(fs.as_ref(), root, "ravens/env.json", "evil").await.is_err());
        assert_eq!(std::fs::read_to_string(&outside).expect("read"), "keep", "link target untouched");
        assert!(load_environments(root).is_err(), "symlinked env.json is refused");
        assert!(append_log_line(fs.as_ref(), root, "{}").await.is_ok());
        let log = ravens_dir(root).join(LOG_FILE);
        std::fs::remove_file(&log).expect("remove");
        std::os::unix::fs::symlink(&outside, &log).expect("link");
        assert!(append_log_line(fs.as_ref(), root, "{}").await.is_err(), "symlinked log refused");
        assert_eq!(std::fs::read_to_string(&outside).expect("read"), "keep");
    }

    #[gpui::test]
    async fn log_rotates_past_the_cap(cx: &mut TestAppContext) {
        let fs = real_fs(cx);
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let line = "x".repeat(1024);
        let lines_to_fill = (MAX_LOG_BYTES / 1025) as usize + 2;
        for _ in 0..lines_to_fill {
            append_log_line(fs.as_ref(), root, &line).await.expect("append");
        }
        let log = ravens_dir(root).join(LOG_FILE);
        let rotated = ravens_dir(root).join(ROTATED_LOG_FILE);
        assert!(rotated.exists(), "rotated file exists");
        assert!(std::fs::metadata(&rotated).expect("meta").len() <= MAX_LOG_BYTES);
        assert!(std::fs::metadata(&log).expect("meta").len() < MAX_LOG_BYTES);
        let all = read_log_lines(root).expect("lines");
        assert_eq!(all.len(), lines_to_fill);
    }

}
