//! The one gpui entry point the producers call: save a capture under the
//! first visible worktree's `.hulder/clippings/` and open that file. When
//! the setting is off or there is no local worktree it returns `None` so
//! the caller can fall back to its scratch buffer.

use crate::settings::ClippingsSettings;
use crate::store::{ClippingDraft, ClippingStore};
use anyhow::Result;
use chrono::Local;
use fs::Fs;
use gpui::{App, AppContext as _, AsyncWindowContext, Entity, WeakEntity};
use project::Project;
use settings::Settings as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use workspace::{OpenOptions, Workspace};

/// The store for a project: its first visible worktree, local only, since
/// the store reads and writes the files itself.
pub fn store_for(project: &Entity<Project>, cx: &App) -> Option<ClippingStore> {
    let project = project.read(cx);
    if !project.is_local() {
        return None;
    }
    project
        .visible_worktrees(cx)
        .next()
        .map(|worktree| ClippingStore::new(worktree.read(cx).abs_path().to_path_buf()))
}

/// What `save_and_open` needs from the workspace, or `None` when captures
/// should stay scratch buffers.
pub fn prepare(workspace: &Workspace, cx: &App) -> Option<(ClippingStore, Arc<dyn Fs>)> {
    if !ClippingsSettings::get_global(cx).save_clippings {
        return None;
    }
    let project = workspace.project();
    let store = store_for(project, cx)?;
    Some((store, project.read(cx).fs().clone()))
}

/// Why `save_and_open` failed: nothing reached disk, or the file is there
/// but could not be shown. Callers word their toast from this, since "not
/// saved" is wrong for the second case.
#[derive(Debug)]
pub enum SaveAndOpenError {
    Save(anyhow::Error),
    Open { path: PathBuf, error: anyhow::Error },
}

impl SaveAndOpenError {
    pub fn user_message(&self) -> String {
        match self {
            SaveAndOpenError::Save(error) => format!("Clipping not saved: {error:#}"),
            SaveAndOpenError::Open { path, error } => format!(
                "Clipping saved to {} but could not be opened: {error:#}",
                path.display()
            ),
        }
    }

    pub fn saved_path(&self) -> Option<&Path> {
        match self {
            SaveAndOpenError::Save(_) => None,
            SaveAndOpenError::Open { path, .. } => Some(path),
        }
    }
}

impl std::fmt::Display for SaveAndOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.user_message())
    }
}

impl std::error::Error for SaveAndOpenError {}

/// Saves `draft` and opens the saved file in the workspace. `Ok(None)` means
/// nothing was written and the caller should open its scratch buffer.
pub async fn save_and_open(
    workspace: WeakEntity<Workspace>,
    draft: ClippingDraft,
    cx: &mut AsyncWindowContext,
) -> Result<Option<PathBuf>, SaveAndOpenError> {
    let prepared = workspace
        .read_with(cx, |workspace, cx| prepare(workspace, cx))
        .map_err(SaveAndOpenError::Save)?;
    let Some((store, fs)) = prepared else {
        return Ok(None);
    };
    let saved = cx
        .background_spawn(async move { store.save(fs.as_ref(), draft, Local::now()).await })
        .await
        .map_err(SaveAndOpenError::Save)?;
    let opened = open_saved(&workspace, saved.path.clone(), cx).await;
    if let Err(error) = opened {
        return Err(SaveAndOpenError::Open {
            path: saved.path,
            error,
        });
    }
    Ok(Some(saved.path))
}

async fn open_saved(
    workspace: &WeakEntity<Workspace>,
    path: PathBuf,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(path, OpenOptions::default(), window, cx)
        })?
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_failures_say_the_clipping_was_saved() {
        let save = SaveAndOpenError::Save(anyhow::anyhow!("disk full"));
        assert_eq!(save.user_message(), "Clipping not saved: disk full");
        assert_eq!(save.saved_path(), None);
        let open = SaveAndOpenError::Open {
            path: PathBuf::from("/repo/.hulder/clippings/2026-10-13-a.md"),
            error: anyhow::anyhow!("no pane"),
        };
        assert_eq!(
            open.user_message(),
            "Clipping saved to /repo/.hulder/clippings/2026-10-13-a.md but could not be opened: no pane"
        );
        assert!(!open.user_message().contains("not saved"));
        assert_eq!(
            open.saved_path(),
            Some(Path::new("/repo/.hulder/clippings/2026-10-13-a.md"))
        );
    }
}
