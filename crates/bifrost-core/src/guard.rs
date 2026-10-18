use crate::protocol::RpcError;
use std::path::{Component, Path, PathBuf};

/// Resolves a tool path argument against the project root and refuses anything
/// that lands outside the root. Symlinks are followed for every component that
/// exists, so a link inside the root that points elsewhere is refused too; the
/// trailing components that do not exist yet are checked lexically.
pub fn resolve(root: &Path, raw: &str) -> Result<PathBuf, RpcError> {
    if raw.trim().is_empty() {
        return Err(RpcError::invalid_params("path must not be empty"));
    }
    let root = root
        .canonicalize()
        .map_err(|error| RpcError::internal(format!("project root: {error}")))?;
    let joined = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    let resolved = canonicalize_existing_prefix(&joined)?;
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(RpcError::new(
            "forbidden_path",
            format!("{raw} is outside the project root"),
        ))
    }
}

fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf, RpcError> {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|name| name.to_os_string()) else {
            return Err(RpcError::new(
                "forbidden_path",
                "path has no existing prefix",
            ));
        };
        rest.push(name);
        if !existing.pop() {
            return Err(RpcError::new(
                "forbidden_path",
                "path has no existing prefix",
            ));
        }
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|error| RpcError::internal(format!("{}: {error}", existing.display())))?;
    for name in rest.into_iter().rev() {
        match Path::new(&name).components().next() {
            Some(Component::Normal(_)) => resolved.push(name),
            Some(Component::ParentDir) => {
                return Err(RpcError::new("forbidden_path", "path escapes with .."));
            }
            _ => {}
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/a.rs"), "").expect("write");
        dir
    }

    #[test]
    fn relative_and_absolute_inside_root_resolve() {
        let dir = root();
        let canonical = dir.path().canonicalize().expect("canon");
        assert_eq!(
            resolve(dir.path(), "src/a.rs").expect("relative"),
            canonical.join("src/a.rs")
        );
        let absolute = canonical.join("src/a.rs");
        assert_eq!(
            resolve(dir.path(), &absolute.to_string_lossy()).expect("absolute"),
            absolute
        );
    }

    #[test]
    fn not_yet_existing_file_under_hulder_resolves() {
        let dir = root();
        let resolved = resolve(dir.path(), ".hulder/notes/new.md").expect("new");
        assert!(resolved.starts_with(dir.path().canonicalize().expect("canon")));
        assert!(resolved.ends_with(".hulder/notes/new.md"));
    }

    #[test]
    fn escapes_are_refused() {
        let dir = root();
        assert_eq!(
            resolve(dir.path(), "../x").expect_err("dotdot").code,
            "forbidden_path"
        );
        assert_eq!(
            resolve(dir.path(), "src/../../x").expect_err("dotdot").code,
            "forbidden_path"
        );
        assert_eq!(
            resolve(dir.path(), "/etc/passwd")
                .expect_err("absolute")
                .code,
            "forbidden_path"
        );
        assert_eq!(
            resolve(dir.path(), "nope/../../escape.md")
                .expect_err("missing then dotdot")
                .code,
            "forbidden_path"
        );
        assert_eq!(
            resolve(dir.path(), "").expect_err("empty").code,
            "invalid_params"
        );
    }

    #[test]
    fn symlink_pointing_outside_is_refused() {
        let dir = root();
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");
        assert_eq!(
            resolve(dir.path(), "link/file.md")
                .expect_err("via link")
                .code,
            "forbidden_path"
        );
    }
}
