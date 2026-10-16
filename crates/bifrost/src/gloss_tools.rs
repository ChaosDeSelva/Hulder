//! Bifröst's view of the Glosses (spec section 14): list the glosses of one
//! project file and leave one. Reads go through the gloss store's guarded,
//! bounded loader; writes go through the `GlossStore` entity so the editor's
//! cache sees them at once.

use crate::dispatch::{optional_string, optional_u64, string_param, unknown_method};
use crate::protocol::RpcError;
use fs::Fs;
use glosses::GlossStore;
use glosses::model::{Gloss, LineRange, format_timestamp};
use glosses::store::{checked_gloss_path, load_gloss_file};
use std::sync::Arc;
use gpui::{AsyncApp, BackgroundExecutor, Entity};
use path::rel_path::RelPath;
use project::ProjectPath;
use serde_json::{Value, json};
use std::io::Read as _;
use std::path::{Path, PathBuf};

pub const GLOSS_METHODS: &[&str] = &["saga_glosses", "saga_add_gloss"];

/// Largest source file we read to copy the anchor line out of.
pub const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
/// The author recorded on a gloss an agent leaves when it names none.
pub const DEFAULT_AGENT_AUTHOR: &str = "agent";

/// The file a gloss tool acts on, resolved by the caller: `absolute` passed
/// the project guard, `project_path` is how the project knows it, and
/// `worktree_root` is the worktree that owns it.
pub struct GlossTarget {
    pub absolute: PathBuf,
    pub project_path: ProjectPath,
    pub worktree_root: PathBuf,
}

/// `lines` arrives as `"42-48"`, `"42"` or `42`; absent means file level.
pub fn parse_lines(params: &Value) -> Result<Option<LineRange>, RpcError> {
    match params.get("lines") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(_)) => {
            let line = optional_u64(params, "lines")?.unwrap_or(0);
            let line = u32::try_from(line)
                .ok()
                .filter(|line| *line >= 1)
                .ok_or_else(|| RpcError::invalid_params("lines must be a 1-based line number"))?;
            Ok(Some(LineRange::single(line)))
        }
        Some(Value::String(text)) => {
            let (start, end) = match text.split_once('-') {
                Some((start, end)) => (start.trim(), end.trim()),
                None => (text.trim(), text.trim()),
            };
            let parse = |part: &str| {
                part.parse::<u32>()
                    .ok()
                    .filter(|line| *line >= 1)
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!(
                            "lines must look like \"42\" or \"42-48\", got {text:?}"
                        ))
                    })
            };
            Ok(Some(LineRange::new(parse(start)?, parse(end)?)))
        }
        Some(other) => Err(RpcError::invalid_params(format!(
            "lines must be a string or integer, got {other}"
        ))),
    }
}

pub fn gloss_json(gloss: &Gloss) -> Value {
    json!({
        "line_start": gloss.lines.map(|lines| lines.start),
        "line_end": gloss.lines.map(|lines| lines.end),
        "anchor": gloss.anchor,
        "created": format_timestamp(gloss.created),
        "author": gloss.author,
        "body": gloss.body,
    })
}

pub async fn list_glosses(
    fs: &Arc<dyn Fs>,
    executor: &BackgroundExecutor,
    worktree_root: &Path,
    source: &RelPath,
) -> Result<Vec<Gloss>, RpcError> {
    let gloss_path = checked_gloss_path(fs.as_ref(), worktree_root, source)
        .await
        .map_err(|error| RpcError::invalid_params(format!("{error:#}")))?;
    load_gloss_file(fs, executor, &gloss_path)
        .await
        .map(|loaded| loaded.glosses)
        .map_err(RpcError::internal)
}

/// The trimmed text of the first line of `lines`, read from `path` without
/// following a symlink at the file and with a cap on the bytes read.
pub async fn anchor_text(
    fs: &dyn Fs,
    executor: &BackgroundExecutor,
    path: &Path,
    lines: Option<LineRange>,
) -> Result<String, RpcError> {
    let Some(lines) = lines else {
        return Ok(String::new());
    };
    let reader = fs
        .open_sync_nofollow(path)
        .await
        .map_err(|error| RpcError::invalid_params(format!("{error:#}")))?;
    let bytes = executor
        .spawn(async move {
            let mut bytes = Vec::new();
            reader.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes)?;
            anyhow::Ok(bytes)
        })
        .await
        .map_err(RpcError::internal)?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(RpcError::invalid_params(format!(
            "{} is larger than the {MAX_SOURCE_BYTES} byte limit",
            path.display()
        )));
    }
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .nth(lines.start as usize - 1)
        .map(|line| line.trim().to_string())
        .ok_or_else(|| {
            RpcError::invalid_params(format!("{} has no line {}", path.display(), lines.start))
        })
}

pub async fn dispatch_gloss(
    method: &str,
    params: &Value,
    target: GlossTarget,
    fs: &Arc<dyn Fs>,
    executor: &BackgroundExecutor,
    store: Option<Entity<GlossStore>>,
    cx: &mut AsyncApp,
) -> Result<Value, RpcError> {
    let relative = target.project_path.path.as_unix_str().to_string();
    match method {
        "saga_glosses" => {
            let glosses = list_glosses(
                fs,
                executor,
                &target.worktree_root,
                &target.project_path.path,
            )
            .await?;
            Ok(json!({
                "path": relative,
                "glosses": glosses.iter().map(gloss_json).collect::<Vec<_>>(),
            }))
        }
        "saga_add_gloss" => {
            let body = string_param(params, "body")?.trim().to_string();
            if body.is_empty() {
                return Err(RpcError::invalid_params("a gloss needs a body"));
            }
            let lines = parse_lines(params)?;
            let author = optional_string(params, "author")?
                .map(str::trim)
                .filter(|author| !author.is_empty())
                .unwrap_or(DEFAULT_AGENT_AUTHOR);
            if author.chars().any(char::is_control) {
                return Err(RpcError::invalid_params(
                    "author must not contain control characters",
                ));
            }
            let anchor = anchor_text(fs.as_ref(), executor, &target.absolute, lines).await?;
            let store =
                store.ok_or_else(|| RpcError::internal("the gloss store is not running"))?;
            let project_path = target.project_path.clone();
            let author = author.to_string();
            let written = cx.update(|cx| {
                store.update(cx, |store, cx| {
                    store.add_gloss(project_path, lines, anchor.clone(), body, Some(author), cx)
                })
            });
            written.await.map_err(RpcError::internal)?;
            let glosses = list_glosses(
                fs,
                executor,
                &target.worktree_root,
                &target.project_path.path,
            )
            .await?;
            let added = glosses
                .last()
                .map(gloss_json)
                .ok_or_else(|| RpcError::internal("the gloss was not written"))?;
            Ok(json!({ "path": relative, "gloss": added }))
        }
        other => Err(unknown_method(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{Routed, route};
    use fs::FakeFs;
    use gpui::{AppContext as _, TestAppContext};
    use path::rel_path::rel_path;
    use project::Project;
    use settings::SettingsStore;
    use std::sync::Arc;
    use util::path;

    #[test]
    fn gloss_methods_route_as_gloss_and_lines_parse() {
        for method in GLOSS_METHODS {
            assert_eq!(route(method), Routed::Gloss, "{method}");
        }
        assert_eq!(parse_lines(&json!({})).expect("absent"), None);
        assert_eq!(
            parse_lines(&json!({ "lines": "42-48" })).expect("range"),
            Some(LineRange::new(42, 48))
        );
        assert_eq!(
            parse_lines(&json!({ "lines": "7" })).expect("single"),
            Some(LineRange::single(7))
        );
        assert_eq!(
            parse_lines(&json!({ "lines": 3 })).expect("number"),
            Some(LineRange::single(3))
        );
        for bad in [
            json!({ "lines": "0" }),
            json!({ "lines": "a-b" }),
            json!({ "lines": [1] }),
        ] {
            assert!(parse_lines(&bad).is_err(), "{bad}");
        }
    }

    async fn setup(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<Project>,
        Entity<GlossStore>,
        GlossTarget,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "src": { "main.rs": "// header\nfn main() {\n}\n" },
                "outside.rs": "secret line\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let store = cx.new(GlossStore::new);
        store.update(cx, |store, cx| store.register_project(project.clone(), cx));
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("one worktree")
                .read(cx)
                .id()
        });
        let target = GlossTarget {
            absolute: PathBuf::from(path!("/root/src/main.rs")),
            project_path: ProjectPath {
                worktree_id,
                path: rel_path("src/main.rs").into_arc(),
            },
            worktree_root: PathBuf::from(path!("/root")),
        };
        (fs, project, store, target)
    }

    #[gpui::test]
    async fn add_then_list_round_trips_with_anchor_and_author(cx: &mut TestAppContext) {
        let (fs, _project, store, target) = setup(cx).await;
        let executor = cx.executor();
        let mut async_cx = cx.to_async();
        let fs_dyn: Arc<dyn Fs> = fs.clone();

        let empty = dispatch_gloss(
            "saga_glosses",
            &json!({}),
            GlossTarget {
                absolute: target.absolute.clone(),
                project_path: target.project_path.clone(),
                worktree_root: target.worktree_root.clone(),
            },
            &fs_dyn,
            &executor,
            Some(store.clone()),
            &mut async_cx,
        )
        .await
        .expect("list");
        assert_eq!(empty["glosses"].as_array().map(Vec::len), Some(0));

        let added = dispatch_gloss(
            "saga_add_gloss",
            &json!({ "lines": "2-3", "body": "  Entry point.  " }),
            GlossTarget {
                absolute: target.absolute.clone(),
                project_path: target.project_path.clone(),
                worktree_root: target.worktree_root.clone(),
            },
            &fs_dyn,
            &executor,
            Some(store.clone()),
            &mut async_cx,
        )
        .await
        .expect("add");
        assert_eq!(added["path"], "src/main.rs");
        assert_eq!(added["gloss"]["anchor"], "fn main() {");
        assert_eq!(added["gloss"]["author"], DEFAULT_AGENT_AUTHOR);
        assert_eq!(added["gloss"]["line_start"], 2);
        assert_eq!(added["gloss"]["line_end"], 3);
        assert_eq!(added["gloss"]["body"], "Entry point.");

        let text = fs
            .load(path!("/root/.hulder/glosses/src/main.rs.md").as_ref())
            .await
            .expect("gloss file");
        assert!(text.contains("author: agent\n"), "{text}");

        let listed = dispatch_gloss(
            "saga_glosses",
            &json!({}),
            target,
            &fs_dyn,
            &executor,
            Some(store),
            &mut async_cx,
        )
        .await
        .expect("list");
        assert_eq!(listed["glosses"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["glosses"][0]["anchor"], "fn main() {");
    }

    #[gpui::test]
    async fn add_refuses_missing_body_bad_line_and_symlinked_source(cx: &mut TestAppContext) {
        let (fs, _project, store, target) = setup(cx).await;
        let executor = cx.executor();
        let mut async_cx = cx.to_async();
        let fs_dyn: Arc<dyn Fs> = fs.clone();
        let fresh = |target: &GlossTarget| GlossTarget {
            absolute: target.absolute.clone(),
            project_path: target.project_path.clone(),
            worktree_root: target.worktree_root.clone(),
        };

        let error = dispatch_gloss(
            "saga_add_gloss",
            &json!({ "lines": "2" }),
            fresh(&target),
            &fs_dyn,
            &executor,
            Some(store.clone()),
            &mut async_cx,
        )
        .await
        .expect_err("no body");
        assert_eq!(error.code, "invalid_params");

        let error = dispatch_gloss(
            "saga_add_gloss",
            &json!({ "lines": "99", "body": "x" }),
            fresh(&target),
            &fs_dyn,
            &executor,
            Some(store.clone()),
            &mut async_cx,
        )
        .await
        .expect_err("line past the end");
        assert_eq!(error.code, "invalid_params");

        fs.insert_symlink(
            path!("/root/src/linked.rs"),
            PathBuf::from(path!("/root/outside.rs")),
        )
        .await;
        let linked = GlossTarget {
            absolute: PathBuf::from(path!("/root/src/linked.rs")),
            project_path: ProjectPath {
                worktree_id: target.project_path.worktree_id,
                path: rel_path("src/linked.rs").into_arc(),
            },
            worktree_root: target.worktree_root.clone(),
        };
        let error = dispatch_gloss(
            "saga_add_gloss",
            &json!({ "lines": "1", "body": "x" }),
            linked,
            &fs_dyn,
            &executor,
            Some(store),
            &mut async_cx,
        )
        .await
        .expect_err("symlinked source must not be read for the anchor");
        assert!(error.message.contains("symlink"), "{error:?}");
        assert!(
            !fs.is_file(path!("/root/.hulder/glosses/src/linked.rs.md").as_ref())
                .await
        );
    }
}
