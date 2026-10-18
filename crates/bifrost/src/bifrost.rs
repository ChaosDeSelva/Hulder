pub mod dispatch;
pub mod gloss_tools;
pub use bifrost_core::guard;
pub use bifrost_core::protocol;
pub mod raven_tools;
pub mod server;
pub mod share;

use crate::dispatch::{Routed, optional_string, optional_u64, route, string_param, unknown_method};
use crate::protocol::{Request, Response, RpcError, parse_request};
use editor::Editor;
use glosses::GlossStore;
use gpui::{AnyWindowHandle, App, AppContext as _, AsyncApp, Entity, Global, WeakEntity, actions};
use hulder_net::NetLog;
use language::DiagnosticEntryRef;
use project::{Project, ProjectPath};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use workspace::{OpenOptions, Workspace};

actions!(
    bifrost,
    [
        /// Writes a hulder MCP server entry into the project's .mcp.json so terminal agents reach the editor over Bifröst.
        ShareWithAgents,
    ]
);

#[derive(Default)]
pub struct Bifrost {
    /// Every open workspace with the window that hosts it, oldest first.
    workspaces: Vec<(WeakEntity<Workspace>, AnyWindowHandle)>,
    pub connections: u64,
    pub calls: u64,
}

struct GlobalBifrost(Entity<Bifrost>);

impl Global for GlobalBifrost {}

/// The workspace and worktree root a call belongs to, chosen by the caller's
/// working directory rather than by which window happens to be in front.
pub struct Picked {
    pub workspace: Entity<Workspace>,
    pub window: AnyWindowHandle,
    pub root: PathBuf,
}

/// Among `roots` (one list of worktree roots per workspace), the workspace
/// and root that contain `cwd`; the longest matching root wins so a nested
/// worktree beats the one enclosing it.
pub fn workspace_for_cwd(roots: &[Vec<PathBuf>], cwd: &Path) -> Option<(usize, PathBuf)> {
    roots
        .iter()
        .enumerate()
        .flat_map(|(index, roots)| roots.iter().map(move |root| (index, root)))
        .filter(|(_, root)| cwd.starts_with(root))
        .max_by_key(|(_, root)| root.as_os_str().len())
        .map(|(index, root)| (index, root.clone()))
}

fn canonical_or_given(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

impl Bifrost {
    pub fn try_global(cx: &App) -> Option<Entity<Bifrost>> {
        cx.try_global::<GlobalBifrost>()
            .map(|global| global.0.clone())
    }

    pub fn register(&mut self, workspace: WeakEntity<Workspace>, window: AnyWindowHandle) {
        self.prune();
        self.workspaces.push((workspace, window));
    }

    fn prune(&mut self) {
        self.workspaces
            .retain(|(workspace, _)| workspace.upgrade().is_some());
    }

    /// The workspace whose project contains `cwd`. A cwd outside every open
    /// project is refused rather than guessed.
    pub fn pick_workspace(&mut self, cwd: &Path, cx: &App) -> Result<Picked, RpcError> {
        self.prune();
        let cwd = canonical_or_given(cwd);
        let alive: Vec<(Entity<Workspace>, AnyWindowHandle)> = self
            .workspaces
            .iter()
            .filter_map(|(workspace, window)| Some((workspace.upgrade()?, *window)))
            .collect();
        if alive.is_empty() {
            return Err(RpcError::new("no_workspace", "Hulder has no window open"));
        }
        let roots: Vec<Vec<PathBuf>> = alive
            .iter()
            .map(|(workspace, _)| {
                let project = workspace.read(cx).project().read(cx);
                project
                    .visible_worktrees(cx)
                    .map(|worktree| canonical_or_given(&worktree.read(cx).abs_path()))
                    .collect()
            })
            .collect();
        let (index, root) = workspace_for_cwd(&roots, &cwd).ok_or_else(|| {
            RpcError::new(
                "no_workspace_for_cwd",
                format!(
                    "no Hulder window has a project folder containing {}; open that folder in Hulder first",
                    cwd.display()
                ),
            )
        })?;
        let (workspace, window) = alive
            .into_iter()
            .nth(index)
            .ok_or_else(|| RpcError::internal("workspace list changed while picking"))?;
        Ok(Picked {
            workspace,
            window,
            root,
        })
    }
}

pub fn init(cx: &mut App) {
    let bifrost = cx.new(|_| Bifrost::default());
    cx.set_global(GlobalBifrost(bifrost.clone()));

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        share::register(workspace);
        let Some(window) = window else {
            return;
        };
        let handle = window.window_handle();
        let weak = cx.weak_entity();
        if let Some(bifrost) = Bifrost::try_global(cx) {
            bifrost.update(cx, |bifrost, _| bifrost.register(weak, handle));
        }
    })
    .detach();

    let path = server::socket_path();
    let listener = match server::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            log::error!("bifrost: not listening: {error:#}");
            return;
        }
    };
    log::info!("bifrost: listening on {}", path.display());
    cx.on_app_quit(move |_| {
        let path = path.clone();
        async move {
            if let Err(error) = server::remove_socket(&path) {
                log::warn!("bifrost: {error:#}");
            }
        }
    })
    .detach();

    cx.spawn(async move |cx| {
        let mut consecutive_failures = 0u32;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    consecutive_failures = 0;
                    let peer = server::peer_pid(&stream);
                    let connection = cx.update(|cx| {
                        let connection = bifrost.update(cx, |bifrost, _| {
                            bifrost.connections += 1;
                            bifrost.connections
                        });
                        let peer = peer.map_or_else(|| "unknown".to_string(), |pid| pid.to_string());
                        NetLog::record_bridge(
                            cx,
                            "accept",
                            format!("connection #{connection} pid {peer}"),
                        );
                        connection
                    });
                    let bifrost = bifrost.clone();
                    cx.spawn(async move |cx| {
                        let handler = {
                            let cx = cx.clone();
                            move |line: String| {
                                let bifrost = bifrost.clone();
                                let mut cx = cx.clone();
                                async move { handle_line(line, bifrost, &mut cx).await }
                            }
                        };
                        if let Err(error) = server::serve_connection(stream, handler).await {
                            log::warn!("bifrost: connection #{connection} ended: {error:#}");
                        }
                    })
                    .detach();
                }
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let delay = server::accept_backoff(consecutive_failures);
                    log::warn!(
                        "bifrost: accept failed ({consecutive_failures} in a row), retrying in {delay:?}: {error:#}"
                    );
                    cx.background_executor().timer(delay).await;
                }
            }
        }
    })
    .detach();
}

async fn handle_line(line: String, bifrost: Entity<Bifrost>, cx: &mut AsyncApp) -> String {
    let request = match parse_request(&line) {
        Ok(request) => request,
        Err(response) => return response.to_line(),
    };
    cx.update(|cx| {
        bifrost.update(cx, |bifrost, _| bifrost.calls += 1);
        NetLog::record_bridge(cx, "call", request.method.clone());
    });
    let result = handle_request(&request, &bifrost, cx).await;
    Response::from_result(request.id.clone(), result).to_line()
}

async fn handle_request(
    request: &Request,
    bifrost: &Entity<Bifrost>,
    cx: &mut AsyncApp,
) -> Result<Value, RpcError> {
    let routed = route(&request.method);
    if routed == Routed::Unknown {
        return Err(unknown_method(&request.method));
    }
    let cwd = request.cwd.as_deref().ok_or_else(|| {
        RpcError::new(
            "no_cwd",
            "the request names no working directory; update the hulder CLI that relays it",
        )
    })?;
    let picked = cx.update(|cx| {
        bifrost.update(cx, |bifrost, cx| bifrost.pick_workspace(Path::new(cwd), cx))
    })?;
    let Picked {
        workspace,
        window,
        root,
    } = picked;
    let project = cx.update(|cx| workspace.read(cx).project().clone());
    let fs = cx.update(|cx| project.read(cx).fs().clone());
    let executor = cx.background_executor().clone();

    match routed {
        Routed::Saga => {
            let store = cx.update(|cx| saga::store_for(root, cx));
            let method = request.method.clone();
            let params = request.params.clone();
            cx.background_spawn(async move {
                dispatch::dispatch_saga(&store, fs.as_ref(), &executor, &method, &params).await
            })
            .await
        }
        Routed::Editor => editor_call(request, workspace, window, root, cx).await,
        Routed::Raven => {
            let method = request.method.clone();
            let params = request.params.clone();
            cx.background_spawn(async move { raven_tools::dispatch_raven(&root, &method, &params) })
                .await
        }
        Routed::Gloss => {
            let raw = string_param(&request.params, "path")?;
            let absolute = guard::resolve(&root, raw)?;
            let target = cx.update(|cx| {
                let project = project.read(cx);
                let project_path = project.project_path_for_absolute_path(&absolute, cx)?;
                let worktree_root = project
                    .worktree_for_id(project_path.worktree_id, cx)?
                    .read(cx)
                    .abs_path()
                    .to_path_buf();
                Some(gloss_tools::GlossTarget {
                    absolute,
                    project_path,
                    worktree_root,
                })
            });
            let target = target.ok_or_else(|| {
                RpcError::new("not_in_project", format!("{raw} is not in the project"))
            })?;
            let store = cx.update(|cx| GlossStore::try_global(cx));
            gloss_tools::dispatch_gloss(
                &request.method,
                &request.params,
                target,
                &fs,
                &executor,
                store,
                cx,
            )
            .await
        }
        Routed::Unknown => Err(unknown_method(&request.method)),
    }
}

/// True when `path` resolves under `root` through the same guard the path
/// tools use, so a buffer from another folder, a symlink out, or a file
/// with no path never leaves the project.
pub fn in_project(root: &Path, path: &Path) -> bool {
    guard::resolve(root, &path.to_string_lossy()).is_ok()
}

fn not_in_project() -> RpcError {
    RpcError::new(
        "not_in_project",
        "the active editor shows a file outside the project folder",
    )
}

async fn editor_call(
    request: &Request,
    workspace: Entity<Workspace>,
    window: AnyWindowHandle,
    root: PathBuf,
    cx: &mut AsyncApp,
) -> Result<Value, RpcError> {
    let params = &request.params;
    match request.method.as_str() {
        "project_root" => Ok(json!({ "root": root })),
        "editor_current_file" => cx.update(|cx| current_file(&workspace, &root, cx)),
        "editor_selection" => cx.update(|cx| selection(&workspace, &root, cx)),
        "editor_open_files" => Ok(cx.update(|cx| open_files(&workspace, &root, cx))),
        "editor_open" => open(params, workspace, window, root, cx).await,
        "editor_diagnostics" => diagnostics(params, workspace, root, cx).await,
        other => Err(unknown_method(other)),
    }
}

fn active_editor(workspace: &Entity<Workspace>, cx: &App) -> Result<Entity<Editor>, RpcError> {
    workspace
        .read(cx)
        .active_item_as::<Editor>(cx)
        .ok_or_else(|| RpcError::new("no_editor", "no text editor is active"))
}

fn editor_path(editor: &Editor, project: &Entity<Project>, cx: &App) -> Option<PathBuf> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?;
    let project_path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };
    project.read(cx).absolute_path(&project_path, cx)
}

/// The active editor's path, only when it is inside the project.
fn guarded_editor_path(
    editor: &Editor,
    project: &Entity<Project>,
    root: &Path,
    cx: &App,
) -> Result<PathBuf, RpcError> {
    editor_path(editor, project, cx)
        .filter(|path| in_project(root, path))
        .ok_or_else(not_in_project)
}

fn current_file(
    workspace: &Entity<Workspace>,
    root: &Path,
    cx: &mut App,
) -> Result<Value, RpcError> {
    let editor = active_editor(workspace, cx)?;
    let project = workspace.read(cx).project().clone();
    editor.update(cx, |editor, cx| {
        let path = guarded_editor_path(editor, &project, root, cx)?;
        let language = editor.buffer().read(cx).as_singleton().and_then(|buffer| {
            buffer
                .read(cx)
                .language()
                .map(|language| language.name().to_string())
        });
        let snapshot = editor.display_snapshot(cx);
        let cursor = editor.selections.newest::<text::Point>(&snapshot).head();
        Ok(json!({
            "path": path,
            "language": language,
            "line": cursor.row + 1,
            "column": cursor.column + 1,
        }))
    })
}

fn selection(workspace: &Entity<Workspace>, root: &Path, cx: &mut App) -> Result<Value, RpcError> {
    let editor = active_editor(workspace, cx)?;
    let project = workspace.read(cx).project().clone();
    editor.update(cx, |editor, cx| {
        let path = guarded_editor_path(editor, &project, root, cx)?;
        let snapshot = editor.display_snapshot(cx);
        let selection = editor.selections.newest::<text::Point>(&snapshot);
        let text: String = editor
            .buffer()
            .read(cx)
            .snapshot(cx)
            .text_for_range(selection.range())
            .collect();
        Ok(json!({
            "path": path,
            "text": text,
            "range": {
                "start_line": selection.start.row + 1,
                "start_column": selection.start.column + 1,
                "end_line": selection.end.row + 1,
                "end_column": selection.end.column + 1,
            },
        }))
    })
}

fn open_files(workspace: &Entity<Workspace>, root: &Path, cx: &App) -> Value {
    let workspace = workspace.read(cx);
    let project = workspace.project().read(cx);
    let paths: BTreeSet<String> = workspace
        .items(cx)
        .filter_map(|item| item.project_path(cx))
        .filter_map(|project_path| project.absolute_path(&project_path, cx))
        .filter(|path| in_project(root, path))
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    json!({ "files": paths })
}

async fn open(
    params: &Value,
    workspace: Entity<Workspace>,
    window: AnyWindowHandle,
    root: PathBuf,
    cx: &mut AsyncApp,
) -> Result<Value, RpcError> {
    let raw = string_param(params, "path")?;
    let path = guard::resolve(&root, raw)?;
    if !path.is_file() {
        return Err(RpcError::not_found(format!("{raw} is not a file")));
    }
    let line = optional_u64(params, "line")?;
    let task = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_abs_path(
                    path.clone(),
                    OpenOptions {
                        focus: Some(true),
                        ..OpenOptions::default()
                    },
                    window,
                    cx,
                )
            })
        })
        .map_err(RpcError::internal)?;
    let item = task.await.map_err(RpcError::internal)?;
    if let (Some(line), Some(editor)) = (line, item.downcast::<Editor>()) {
        let row = u32::try_from(line.saturating_sub(1)).unwrap_or(u32::MAX);
        cx.update_window(window, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.go_to_singleton_buffer_point(text::Point::new(row, 0), window, cx);
            })
        })
        .map_err(RpcError::internal)?;
    }
    Ok(json!({ "path": path, "line": line }))
}

fn severity_name(severity: lsp::DiagnosticSeverity) -> &'static str {
    if severity == lsp::DiagnosticSeverity::ERROR {
        "error"
    } else if severity == lsp::DiagnosticSeverity::WARNING {
        "warning"
    } else if severity == lsp::DiagnosticSeverity::INFORMATION {
        "information"
    } else if severity == lsp::DiagnosticSeverity::HINT {
        "hint"
    } else {
        "unknown"
    }
}

fn diagnostic_json(entry: DiagnosticEntryRef<'_, text::Point>) -> Value {
    json!({
        "line": entry.range.start.row + 1,
        "column": entry.range.start.column + 1,
        "end_line": entry.range.end.row + 1,
        "end_column": entry.range.end.column + 1,
        "severity": severity_name(entry.diagnostic.severity),
        "message": entry.diagnostic.message.as_str(),
        "source": entry.diagnostic.source,
    })
}

async fn diagnostics(
    params: &Value,
    workspace: Entity<Workspace>,
    root: PathBuf,
    cx: &mut AsyncApp,
) -> Result<Value, RpcError> {
    let project = cx.update(|cx| workspace.read(cx).project().clone());
    let Some(raw) = optional_string(params, "path")? else {
        return Ok(cx.update(|cx| {
            let project = project.read(cx);
            let files: Vec<Value> = project
                .diagnostic_summaries(false, cx)
                .filter_map(|(project_path, server_id, summary)| {
                    let path = project.absolute_path(&project_path, cx)?;
                    in_project(&root, &path).then(|| {
                        json!({
                            "path": path,
                            "server": server_id.0,
                            "errors": summary.error_count,
                            "warnings": summary.warning_count,
                        })
                    })
                })
                .collect();
            json!({ "files": files })
        }));
    };
    let path = guard::resolve(&root, raw)?;
    let project_path = cx
        .update(|cx| project.read(cx).project_path_for_absolute_path(&path, cx))
        .ok_or_else(|| RpcError::not_found(format!("{raw} is not in the project")))?;
    let buffer = cx
        .update(|cx| project.update(cx, |project, cx| project.open_buffer(project_path, cx)))
        .await
        .map_err(RpcError::internal)?;
    Ok(cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        let entries: Vec<Value> = snapshot
            .diagnostics_in_range::<_, text::Point>(0..snapshot.len(), false)
            .map(diagnostic_json)
            .collect();
        json!({ "path": path, "diagnostics": entries })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_names_cover_the_lsp_set() {
        assert_eq!(severity_name(lsp::DiagnosticSeverity::ERROR), "error");
        assert_eq!(severity_name(lsp::DiagnosticSeverity::WARNING), "warning");
        assert_eq!(
            severity_name(lsp::DiagnosticSeverity::INFORMATION),
            "information"
        );
        assert_eq!(severity_name(lsp::DiagnosticSeverity::HINT), "hint");
    }

    #[test]
    fn workspace_is_picked_by_the_cwd_not_the_front_window() {
        let roots = vec![
            vec![PathBuf::from("/home/me/front")],
            vec![
                PathBuf::from("/home/me/work"),
                PathBuf::from("/home/me/other"),
            ],
            vec![PathBuf::from("/home/me/work/nested")],
        ];
        assert_eq!(
            workspace_for_cwd(&roots, Path::new("/home/me/work/src")),
            Some((1, PathBuf::from("/home/me/work")))
        );
        assert_eq!(
            workspace_for_cwd(&roots, Path::new("/home/me/work/nested/deep")),
            Some((2, PathBuf::from("/home/me/work/nested"))),
            "the longest root wins"
        );
        assert_eq!(
            workspace_for_cwd(&roots, Path::new("/home/me/other")),
            Some((1, PathBuf::from("/home/me/other")))
        );
        assert_eq!(
            workspace_for_cwd(&roots, Path::new("/home/me/workspace")),
            None
        );
        assert_eq!(workspace_for_cwd(&roots, Path::new("/tmp")), None);
        assert_eq!(workspace_for_cwd(&[], Path::new("/home/me/front")), None);
    }

    #[test]
    fn editor_paths_outside_the_project_are_filtered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canon");
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        std::fs::write(root.join("src/a.rs"), "").expect("write");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("b.rs"), "").expect("write");
        std::os::unix::fs::symlink(outside.path(), root.join("link")).expect("symlink");

        assert!(in_project(&root, &root.join("src/a.rs")));
        assert!(
            in_project(&root, &root.join("src/new.rs")),
            "unsaved files under root count"
        );
        assert!(!in_project(&root, &outside.path().join("b.rs")));
        assert!(
            !in_project(&root, &root.join("link/b.rs")),
            "a link out of the root is refused"
        );
        assert!(!in_project(&root, &root.join("../x.rs")));
        assert!(!in_project(&root, Path::new("")));
        assert_eq!(not_in_project().code, "not_in_project");
    }
}
