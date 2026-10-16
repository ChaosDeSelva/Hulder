//! Resolution of `[[Wiki Link]]` names to Markdown files on disk.

use std::path::{Path, PathBuf};

/// File names a wiki link may refer to, in order of preference: the name as
/// written, then its lowercase dashed form (`[[Wiki Link]]` -> `wiki-link.md`).
pub fn candidate_file_names(name: &str) -> Vec<String> {
    let name = name.trim();
    let written = format!("{name}.md");
    let dashed = format!(
        "{}.md",
        name.split_whitespace()
            .collect::<Vec<_>>()
            .join("-")
            .to_lowercase()
    );
    if dashed == written {
        vec![written]
    } else {
        vec![written, dashed]
    }
}

/// Every path a wiki link may resolve to, in the order they are tried: each
/// candidate name in the linking file's directory, then in the worktree root.
/// Names with path separators are rejected so a link cannot escape either.
pub fn candidate_paths(
    name: &str,
    file_directory: Option<&Path>,
    worktree_root: Option<&Path>,
) -> Vec<PathBuf> {
    let name = name.trim();
    if name.is_empty() || name.contains(['/', '\\']) {
        return Vec::new();
    }
    let candidates = candidate_file_names(name);
    let mut directories = vec![];
    for directory in [file_directory, worktree_root].into_iter().flatten() {
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    directories
        .into_iter()
        .flat_map(|directory| {
            candidates
                .iter()
                .map(move |file_name| directory.join(file_name))
        })
        .collect()
}

/// Finds the file a wiki link points at. `exists` is injected so the search
/// order is testable without a filesystem.
pub fn resolve_wiki_link(
    name: &str,
    file_directory: Option<&Path>,
    worktree_root: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidate_paths(name, file_directory, worktree_root)
        .into_iter()
        .find(|path| exists(path))
}

/// Page names written as `[[Name]]` or `[[Name|Alias]]` in `source`, deduplicated.
pub fn wiki_link_names(source: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut rest = source;
    while let Some(open) = rest.find("[[") {
        rest = &rest[open + 2..];
        let Some(close) = rest.find("]]") else {
            break;
        };
        let inner = &rest[..close];
        rest = &rest[close + 2..];
        let name = inner.split('|').next().unwrap_or(inner).trim();
        if name.is_empty() || name.contains(['[', ']', '\n']) {
            continue;
        }
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn fs(paths: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |path| set.contains(path)
    }

    #[test]
    fn prefers_name_as_written_in_the_same_directory() {
        let exists = fs(&[
            "/repo/docs/Wiki Link.md",
            "/repo/docs/wiki-link.md",
            "/repo/Wiki Link.md",
        ]);
        assert_eq!(
            resolve_wiki_link(
                "Wiki Link",
                Some(Path::new("/repo/docs")),
                Some(Path::new("/repo")),
                exists
            ),
            Some(PathBuf::from("/repo/docs/Wiki Link.md"))
        );
    }

    #[test]
    fn falls_back_to_dashed_name_then_worktree_root() {
        let exists = fs(&["/repo/docs/wiki-link.md"]);
        assert_eq!(
            resolve_wiki_link(
                "Wiki Link",
                Some(Path::new("/repo/docs")),
                Some(Path::new("/repo")),
                exists
            ),
            Some(PathBuf::from("/repo/docs/wiki-link.md"))
        );
        let exists = fs(&["/repo/wiki-link.md"]);
        assert_eq!(
            resolve_wiki_link(
                "Wiki Link",
                Some(Path::new("/repo/docs")),
                Some(Path::new("/repo")),
                exists
            ),
            Some(PathBuf::from("/repo/wiki-link.md"))
        );
    }

    #[test]
    fn unresolved_and_invalid_names() {
        let exists = fs(&["/repo/Other.md"]);
        assert_eq!(
            resolve_wiki_link(
                "Wiki Link",
                Some(Path::new("/repo")),
                Some(Path::new("/repo")),
                &exists
            ),
            None
        );
        assert_eq!(
            resolve_wiki_link("../escape", Some(Path::new("/repo")), None, &exists),
            None
        );
        assert_eq!(
            resolve_wiki_link("  ", Some(Path::new("/repo")), None, &exists),
            None
        );
        assert_eq!(resolve_wiki_link("Other", None, None, &exists), None);
    }

    #[test]
    fn candidate_paths_do_not_repeat_the_root_directory() {
        assert_eq!(
            candidate_paths("notes", Some(Path::new("/repo")), Some(Path::new("/repo"))),
            vec![PathBuf::from("/repo/notes.md")]
        );
        assert_eq!(candidate_file_names("My  Page"), vec!["My  Page.md", "my-page.md"]);
    }

    #[test]
    fn scans_names_from_source() {
        assert_eq!(
            wiki_link_names("a [[One]] b [[Two|alias]] [[One]] [[ ]] [[x\ny]] [[Three"),
            vec!["One", "Two"]
        );
    }
}
