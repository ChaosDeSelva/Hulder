use crate::protocol::RpcError;
use clippings::ClippingStore;
use fs::Fs;
use gpui::BackgroundExecutor;
use saga::{Priority, SagaStore, Status, TaleEntry, Verse};
use serde_json::{Value, json};
use std::path::Path;

pub const SAGA_METHODS: &[&str] = &[
    "saga_list_tales",
    "saga_get_tale",
    "saga_create_tale",
    "saga_set_status",
    "saga_append_chronicle",
    "saga_list_verses",
    "saga_toggle_verse",
    "saga_add_verse",
    "saga_list_lore",
    "saga_read_lore",
    "saga_search",
    "saga_list_clippings",
    "saga_read_clipping",
];

pub const EDITOR_METHODS: &[&str] = &[
    "project_root",
    "editor_current_file",
    "editor_selection",
    "editor_open",
    "editor_diagnostics",
    "editor_open_files",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed {
    Saga,
    Editor,
    Raven,
    Gloss,
    Unknown,
}

pub fn route(method: &str) -> Routed {
    if SAGA_METHODS.contains(&method) {
        Routed::Saga
    } else if EDITOR_METHODS.contains(&method) {
        Routed::Editor
    } else if crate::raven_tools::RAVEN_METHODS.contains(&method) {
        Routed::Raven
    } else if crate::gloss_tools::GLOSS_METHODS.contains(&method) {
        Routed::Gloss
    } else {
        Routed::Unknown
    }
}

pub fn unknown_method(method: &str) -> RpcError {
    RpcError::new("unknown_method", format!("unknown method {method:?}"))
}

pub fn string_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    optional_string(params, name)?
        .ok_or_else(|| RpcError::invalid_params(format!("missing string param {name:?}")))
}

pub fn optional_string<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(other) => Err(RpcError::invalid_params(format!(
            "param {name:?} must be a string, got {other}"
        ))),
    }
}

pub fn optional_u64(params: &Value, name: &str) -> Result<Option<u64>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_u64().map(Some).ok_or_else(|| {
            RpcError::invalid_params(format!("param {name:?} must be a non-negative integer"))
        }),
        Some(Value::String(text)) => text
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| RpcError::invalid_params(format!("param {name:?} must be an integer"))),
        Some(other) => Err(RpcError::invalid_params(format!(
            "param {name:?} must be an integer, got {other}"
        ))),
    }
}

fn string_list(params: &Value, name: &str) -> Result<Vec<String>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_string).ok_or_else(|| {
                    RpcError::invalid_params(format!("param {name:?} must hold strings"))
                })
            })
            .collect(),
        Some(Value::String(text)) => Ok(text
            .split(',')
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect()),
        Some(other) => Err(RpcError::invalid_params(format!(
            "param {name:?} must be a list of strings, got {other}"
        ))),
    }
}

/// Tale ids arrive as `1`, `"1"` or `"0001"`; all mean the same tale.
fn tale_id_param(params: &Value) -> Result<u32, RpcError> {
    let id = optional_u64(params, "id")?
        .ok_or_else(|| RpcError::invalid_params("missing param \"id\""))?;
    u32::try_from(id).map_err(|_| RpcError::invalid_params(format!("tale id {id} is out of range")))
}

fn status_param(params: &Value, name: &str) -> Result<Option<Status>, RpcError> {
    optional_string(params, name)?
        .map(|text| {
            Status::parse(text).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "status must be one of {}",
                    Status::ALL.map(Status::as_str).join(", ")
                ))
            })
        })
        .transpose()
}

fn priority_param(params: &Value, name: &str) -> Result<Option<Priority>, RpcError> {
    optional_string(params, name)?
        .map(|text| {
            Priority::parse(text).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "priority must be one of {}",
                    Priority::ALL.map(Priority::as_str).join(", ")
                ))
            })
        })
        .transpose()
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(RpcError::internal)
}

fn invalid(error: anyhow::Error) -> RpcError {
    RpcError::invalid_params(format!("{error:#}"))
}

fn missing(error: anyhow::Error) -> RpcError {
    RpcError::not_found(format!("{error:#}"))
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn relative_to_root(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn tale_json(root: &Path, entry: &TaleEntry) -> Value {
    let tale = &entry.tale;
    json!({
        "id": tale.id,
        "title": tale.title,
        "status": tale.status.as_str(),
        "priority": tale.priority.as_str(),
        "tags": tale.tags,
        "created": tale.created,
        "updated": tale.updated,
        "body": tale.body,
        "path": relative_to_root(root, &entry.path),
    })
}

/// Verses are 0-based line indices inside the store and 1-based on the wire.
fn verse_json(verse: &Verse) -> Value {
    json!({
        "line": verse.line + 1,
        "done": verse.done,
        "text": verse.text,
    })
}

async fn find_tale(store: &SagaStore, fs: &dyn Fs, id: u32) -> Result<TaleEntry, RpcError> {
    store
        .load(fs)
        .await?
        .tales
        .into_iter()
        .find(|entry| entry.tale.id == id)
        .ok_or_else(|| RpcError::not_found(format!("no tale with id {id}")))
}

/// Every saga tool is a file operation under `<root>/.hulder/` through the
/// Saga and Clippings stores, which own the path guards and read caps;
/// nothing here touches the editor.
pub async fn dispatch_saga(
    store: &SagaStore,
    fs: &dyn Fs,
    executor: &BackgroundExecutor,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    let root = store.root();
    let result = match method {
        "saga_list_tales" => {
            let status = status_param(params, "status")?;
            let snapshot = store.load(fs).await?;
            let tales: Vec<Value> = snapshot
                .tales
                .iter()
                .filter(|entry| status.is_none_or(|status| entry.tale.status == status))
                .map(|entry| tale_json(root, entry))
                .collect();
            json!({ "tales": tales })
        }
        "saga_get_tale" => {
            let id = tale_id_param(params)?;
            json!({ "tale": tale_json(root, &find_tale(store, fs, id).await?) })
        }
        "saga_create_tale" => {
            let title = string_param(params, "title")?;
            if title.trim().is_empty() {
                return Err(RpcError::invalid_params("a tale needs a title"));
            }
            let body = optional_string(params, "body")?;
            let priority = priority_param(params, "priority")?;
            let tags = string_list(params, "tags")?;
            let mut entry = store
                .create_tale(fs, title, &today())
                .await
                .map_err(invalid)?;
            let needs_rewrite = body.is_some() || priority.is_some() || !tags.is_empty();
            if needs_rewrite {
                if let Some(body) = body {
                    let mut text = body.trim_end().to_string();
                    text.push('\n');
                    entry.tale.body = text;
                    entry.tale.append_chronicle(&format!("{} created", today()));
                }
                if let Some(priority) = priority {
                    entry.tale.priority = priority;
                }
                entry.tale.tags = tags;
                store.write_tale(fs, &entry).await.map_err(invalid)?;
            }
            json!({ "tale": tale_json(root, &entry) })
        }
        "saga_set_status" => {
            let id = tale_id_param(params)?;
            let status = status_param(params, "status")?
                .ok_or_else(|| RpcError::invalid_params("missing param \"status\""))?;
            let mut entry = find_tale(store, fs, id).await?;
            entry.tale.status = status;
            entry.tale.touch(&today());
            store.write_tale(fs, &entry).await.map_err(invalid)?;
            json!({ "tale": tale_json(root, &entry) })
        }
        "saga_append_chronicle" => {
            let id = tale_id_param(params)?;
            let text = string_param(params, "text")?.trim();
            if text.is_empty() {
                return Err(RpcError::invalid_params("nothing to append"));
            }
            let mut entry = find_tale(store, fs, id).await?;
            let today = today();
            entry.tale.append_chronicle(&format!("{today} {text}"));
            entry.tale.touch(&today);
            store.write_tale(fs, &entry).await.map_err(invalid)?;
            json!({ "tale": tale_json(root, &entry) })
        }
        "saga_list_verses" => {
            let snapshot = store.load(fs).await?;
            json!({ "verses": snapshot.verses.iter().map(verse_json).collect::<Vec<_>>() })
        }
        "saga_toggle_verse" => {
            let line = optional_u64(params, "line")?
                .ok_or_else(|| RpcError::invalid_params("missing integer param \"line\""))?;
            let index = usize::try_from(line)
                .ok()
                .and_then(|line| line.checked_sub(1))
                .ok_or_else(|| RpcError::invalid_params("line numbers start at 1"))?;
            store.toggle_verse(fs, index).await.map_err(invalid)?;
            let snapshot = store.load(fs).await?;
            let verse = snapshot
                .verses
                .iter()
                .find(|verse| verse.line == index)
                .ok_or_else(|| {
                    RpcError::internal(format!("line {line} vanished after the toggle"))
                })?;
            json!({ "verse": verse_json(verse) })
        }
        "saga_add_verse" => {
            let text = string_param(params, "text")?;
            store.add_verse(fs, text).await.map_err(invalid)?;
            let snapshot = store.load(fs).await?;
            let verse = snapshot
                .verses
                .last()
                .ok_or_else(|| RpcError::internal("the verse was not written"))?;
            json!({ "verse": verse_json(verse) })
        }
        "saga_list_lore" => json!({ "lore": store.lore_names(fs).await? }),
        "saga_read_lore" => {
            let name = string_param(params, "name")?;
            let text = store.read_lore(fs, name).await.map_err(|error| {
                if error.to_string().contains("does not exist") {
                    missing(error)
                } else {
                    invalid(error)
                }
            })?;
            json!({ "name": name, "text": text })
        }
        "saga_search" => {
            let query = string_param(params, "query")?;
            let hits = store.search(fs, query).await.map_err(invalid)?;
            json!({ "hits": to_json(hits)? })
        }
        "saga_list_clippings" => {
            let names: Vec<String> = ClippingStore::new(root)
                .list(fs)
                .await?
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect();
            json!({ "clippings": names })
        }
        "saga_read_clipping" => {
            let name = string_param(params, "name")?;
            let text = ClippingStore::new(root)
                .read(fs, executor, name)
                .await
                .map_err(|error| {
                    if error.to_string().contains("does not exist") {
                        missing(error)
                    } else {
                        invalid(error)
                    }
                })?;
            json!({ "name": name, "text": text })
        }
        other => return Err(unknown_method(other)),
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use std::sync::Arc;

    async fn call(
        cx: &TestAppContext,
        fs: &Arc<FakeFs>,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let store = SagaStore::new("/repo");
        let executor = cx.executor();
        dispatch_saga(&store, fs.as_ref(), &executor, method, &params).await
    }

    #[test]
    fn routes_every_spec_method() {
        for method in SAGA_METHODS {
            assert_eq!(route(method), Routed::Saga, "{method}");
        }
        for method in EDITOR_METHODS {
            assert_eq!(route(method), Routed::Editor, "{method}");
        }
        assert_eq!(route("run_command"), Routed::Unknown);
        assert_eq!(unknown_method("x").code, "unknown_method");
    }

    #[gpui::test]
    async fn tale_written_by_the_panel_store_reads_back_through_the_tool(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({ ".hulder": { "tickets": {} } }))
            .await;
        let store = SagaStore::new("/repo");
        let entry = store
            .create_tale(fs.as_ref(), "Panel made me", "2026-10-13")
            .await
            .expect("create through the store");
        assert_eq!(entry.tale.id, 1);

        for id in [json!("0001"), json!("1"), json!(1)] {
            let got = call(cx, &fs, "saga_get_tale", json!({ "id": id }))
                .await
                .expect("get");
            assert_eq!(got["tale"]["id"], 1, "{id}");
            assert_eq!(got["tale"]["title"], "Panel made me");
            assert_eq!(got["tale"]["path"], ".hulder/tickets/0001-panel-made-me.md");
        }

        let created = call(
            cx,
            &fs,
            "saga_create_tale",
            json!({ "title": "Tool made me", "tags": ["docs"], "priority": "low", "body": "Body" }),
        )
        .await
        .expect("create through the tool");
        assert_eq!(created["tale"]["id"], 2);
        assert_eq!(created["tale"]["priority"], "low");
        assert_eq!(created["tale"]["tags"][0], "docs");
        assert!(
            created["tale"]["body"]
                .as_str()
                .expect("body")
                .starts_with("Body")
        );
        let snapshot = store.load(fs.as_ref()).await.expect("load");
        assert_eq!(snapshot.tales.len(), 2);
        assert_eq!(snapshot.tales[1].tale.title, "Tool made me");
        assert_eq!(snapshot.tales[1].tale.priority, Priority::Low);
    }

    #[gpui::test]
    async fn fake_request_set_round_trips_through_saga_tools(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({ "src": {} })).await;

        let error = call(cx, &fs, "saga_create_tale", json!({}))
            .await
            .expect_err("no title");
        assert_eq!(error.code, "invalid_params");
        let error = call(cx, &fs, "saga_create_tale", json!({ "title": "a\nb" }))
            .await
            .expect_err("newline in title");
        assert_eq!(error.code, "invalid_params");

        let created = call(
            cx,
            &fs,
            "saga_create_tale",
            json!({ "title": "Write docs" }),
        )
        .await
        .expect("create");
        assert_eq!(created["tale"]["id"], 1);

        let listed = call(cx, &fs, "saga_list_tales", json!({}))
            .await
            .expect("list");
        assert_eq!(listed["tales"].as_array().map(Vec::len), Some(1));
        let listed = call(cx, &fs, "saga_list_tales", json!({ "status": "done" }))
            .await
            .expect("list");
        assert_eq!(listed["tales"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            call(cx, &fs, "saga_list_tales", json!({ "status": "nah" }))
                .await
                .expect_err("bad status")
                .code,
            "invalid_params"
        );
        assert_eq!(
            call(cx, &fs, "saga_get_tale", json!({ "id": "7" }))
                .await
                .expect_err("missing")
                .code,
            "not_found"
        );

        let doing = call(
            cx,
            &fs,
            "saga_set_status",
            json!({ "id": "0001", "status": "doing" }),
        )
        .await
        .expect("status");
        assert_eq!(doing["tale"]["status"], "doing");

        let logged = call(
            cx,
            &fs,
            "saga_append_chronicle",
            json!({ "id": "0001", "text": "agent started" }),
        )
        .await
        .expect("chronicle");
        assert!(
            logged["tale"]["body"]
                .as_str()
                .expect("body")
                .contains("agent started")
        );

        let added = call(cx, &fs, "saga_add_verse", json!({ "text": "a verse" }))
            .await
            .expect("add");
        let line = added["verse"]["line"].as_u64().expect("line");
        assert!(line >= 1);
        let toggled = call(cx, &fs, "saga_toggle_verse", json!({ "line": line }))
            .await
            .expect("toggle");
        assert_eq!(toggled["verse"]["done"], true);
        assert_eq!(
            call(cx, &fs, "saga_toggle_verse", json!({ "line": 50 }))
                .await
                .expect_err("bad line")
                .code,
            "invalid_params"
        );
        assert_eq!(
            call(cx, &fs, "saga_toggle_verse", json!({ "line": 0 }))
                .await
                .expect_err("zero line")
                .code,
            "invalid_params"
        );
        let verses = call(cx, &fs, "saga_list_verses", json!({}))
            .await
            .expect("verses");
        assert_eq!(verses["verses"].as_array().map(Vec::len), Some(1));

        fs.insert_file("/repo/.hulder/NOTES.md", b"lore here".to_vec())
            .await;
        let lore = call(cx, &fs, "saga_list_lore", json!({}))
            .await
            .expect("lore");
        assert_eq!(lore["lore"][0], "NOTES.md");
        let read = call(cx, &fs, "saga_read_lore", json!({ "name": "NOTES.md" }))
            .await
            .expect("read");
        assert_eq!(read["text"], "lore here");
        assert_eq!(
            call(
                cx,
                &fs,
                "saga_read_lore",
                json!({ "name": "../Cargo.toml" })
            )
            .await
            .expect_err("escape")
            .code,
            "invalid_params"
        );
        assert_eq!(
            call(
                cx,
                &fs,
                "saga_read_lore",
                json!({ "name": "notes/none.md" })
            )
            .await
            .expect_err("missing")
            .code,
            "not_found"
        );

        let hits = call(cx, &fs, "saga_search", json!({ "query": "agent started" }))
            .await
            .expect("search");
        assert_eq!(hits["hits"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            hits["hits"][0]["path"],
            ".hulder/tickets/0001-write-docs.md"
        );

        let none = call(cx, &fs, "saga_list_clippings", json!({}))
            .await
            .expect("no clippings yet");
        assert_eq!(none["clippings"].as_array().map(Vec::len), Some(0));
        fs.insert_tree(
            "/repo/.hulder/clippings",
            json!({ "2026-10-13-page.md": "---\nsource: https://x.test/\nkind: fetch\n---\nclipped" }),
        )
        .await;
        let listed = call(cx, &fs, "saga_list_clippings", json!({}))
            .await
            .expect("list clippings");
        assert_eq!(listed["clippings"][0], "2026-10-13-page.md");
        let read = call(
            cx,
            &fs,
            "saga_read_clipping",
            json!({ "name": "2026-10-13-page.md" }),
        )
        .await
        .expect("read clipping");
        assert!(read["text"].as_str().expect("text").ends_with("clipped"));
        assert_eq!(
            call(cx, &fs, "saga_read_clipping", json!({}))
                .await
                .expect_err("no name")
                .code,
            "invalid_params"
        );
        assert_eq!(
            call(
                cx,
                &fs,
                "saga_read_clipping",
                json!({ "name": "../NOTES.md" })
            )
            .await
            .expect_err("escape")
            .code,
            "invalid_params"
        );
        assert_eq!(
            call(
                cx,
                &fs,
                "saga_read_clipping",
                json!({ "name": "2026-10-13-none.md" })
            )
            .await
            .expect_err("missing")
            .code,
            "not_found"
        );
        assert_eq!(
            call(cx, &fs, "saga_explode", json!({}))
                .await
                .expect_err("unknown")
                .code,
            "unknown_method"
        );
    }

    #[gpui::test]
    async fn symlinks_under_hulder_are_never_followed(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({
                "secret.md": "needle outside",
                ".hulder": { "tickets": {}, "notes": {}, "clippings": {} },
            }),
        )
        .await;
        for link in [
            "/repo/.hulder/notes/linked.md",
            "/repo/.hulder/TODO.md",
            "/repo/.hulder/clippings/2026-10-13-link.md",
        ] {
            fs.insert_symlink(link, std::path::PathBuf::from("/repo/secret.md"))
                .await;
        }
        let hits = call(cx, &fs, "saga_search", json!({ "query": "needle" }))
            .await
            .expect("search");
        assert_eq!(hits["hits"].as_array().map(Vec::len), Some(0));
        assert!(
            call(
                cx,
                &fs,
                "saga_read_lore",
                json!({ "name": "notes/linked.md" })
            )
            .await
            .is_err()
        );
        assert!(
            call(
                cx,
                &fs,
                "saga_read_clipping",
                json!({ "name": "2026-10-13-link.md" })
            )
            .await
            .is_err()
        );
        assert!(
            call(cx, &fs, "saga_add_verse", json!({ "text": "x" }))
                .await
                .is_err()
        );
        assert_eq!(
            fs.load(Path::new("/repo/secret.md")).await.expect("read"),
            "needle outside"
        );
    }

    #[test]
    fn param_helpers_reject_wrong_types() {
        let params = json!({ "line": "3", "name": 4, "tags": "a, b", "id": "0012" });
        assert_eq!(optional_u64(&params, "line").expect("parsed"), Some(3));
        assert_eq!(optional_u64(&params, "missing").expect("absent"), None);
        assert!(string_param(&params, "name").is_err());
        assert_eq!(string_list(&params, "tags").expect("list"), vec!["a", "b"]);
        assert_eq!(tale_id_param(&params).expect("id"), 12);
        assert!(tale_id_param(&json!({ "id": "x" })).is_err());
        assert!(tale_id_param(&json!({})).is_err());
    }
}
