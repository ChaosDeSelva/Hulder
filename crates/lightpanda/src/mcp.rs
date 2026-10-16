use crate::ShareWithAgents;
use crate::reader::{disabled_message, open_text_buffer, toast};
use crate::settings::LightpandaSettings;
use anyhow::Context as _;
use fs::Fs;
use gpui::{Context, TaskExt as _, Window};
use settings::Settings as _;
use std::path::Path;
use workspace::{Toast, Workspace, notifications::NotificationId};

pub const FILE_NAME: &str = ".mcp.json";

/// Adds (or replaces) the `lightpanda` server entry, keeping every other key as is.
pub fn merge_mcp_config(existing: Option<&str>, binary: &Path) -> anyhow::Result<String> {
    // Agents launch lightpanda themselves, so the opt-out the editor sets in
    // its own child processes has to travel with the config.
    merge_server(
        existing,
        "lightpanda",
        &binary.to_string_lossy(),
        &["mcp"],
        &[("LIGHTPANDA_DISABLE_TELEMETRY", "true")],
    )
}

/// Adds (or replaces) one `mcpServers.<name>` entry in a `.mcp.json` document,
/// keeping every other key as is. Shared by every Hulder feature that offers
/// itself to agents, so the file is always rewritten the same way.
pub fn merge_server(
    existing: Option<&str>,
    name: &str,
    command: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> anyhow::Result<String> {
    let mut root: serde_json::Value = match existing {
        Some(text) => serde_json::from_str(text)
            .context(".mcp.json is not valid JSON; fix it by hand first")?,
        None => serde_json::json!({}),
    };
    let servers = root
        .as_object_mut()
        .context(".mcp.json must be a JSON object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .context("mcpServers must be a JSON object")?;
    let mut entry = serde_json::json!({ "command": command, "args": args });
    if !env.is_empty() {
        let env_map: serde_json::Map<String, serde_json::Value> = env
            .iter()
            .map(|(key, value)| ((*key).to_string(), serde_json::Value::from(*value)))
            .collect();
        entry["env"] = serde_json::Value::Object(env_map);
    }
    servers.insert(name.into(), entry);
    Ok(serde_json::to_string_pretty(&root)? + "\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    AsPreviewed,
    Remerged,
}

/// A planted `.mcp.json -> ~/.ssh/config` must never be followed by the
/// write, so a symlink is refused outright, and so is anything that resolves
/// outside the project folder. An absent file is fine: it will be created.
pub async fn check_mcp_path(fs: &dyn Fs, root: &Path, path: &Path) -> anyhow::Result<()> {
    let Some(metadata) = fs.metadata(path).await? else {
        return Ok(());
    };
    anyhow::ensure!(
        !metadata.is_symlink,
        "{FILE_NAME} is a symlink; edit it by hand"
    );
    let canonical_root = fs.canonicalize(root).await?;
    let canonical_path = fs.canonicalize(path).await?;
    anyhow::ensure!(
        canonical_path.starts_with(&canonical_root),
        "{FILE_NAME} resolves outside the project folder ({}); edit it by hand",
        canonical_path.display()
    );
    Ok(())
}

/// Writes the previewed merge only if the file still reads as it did at
/// preview time; otherwise the lightpanda entry is merged into whatever is
/// there now, so an edit made meanwhile is never overwritten. The path is
/// re-checked here because the file may have been swapped since the preview.
pub async fn apply_mcp_config(
    fs: &dyn Fs,
    root: &Path,
    path: &Path,
    previewed_original: &str,
    merged: &str,
    binary: &Path,
) -> anyhow::Result<Applied> {
    check_mcp_path(fs, root, path).await?;
    let current = if fs.is_file(path).await {
        Some(fs.load(path).await?)
    } else {
        None
    };
    let (text, applied) = if current.as_deref() == Some(previewed_original) {
        (merged.to_string(), Applied::AsPreviewed)
    } else {
        (
            merge_mcp_config(current.as_deref(), binary)?,
            Applied::Remerged,
        )
    };
    fs.atomic_write(path.to_path_buf(), text).await?;
    Ok(applied)
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ShareWithAgents, window, cx| {
        share_with_agents(workspace, window, cx);
    });
}

struct ShareToast;

/// Writes `.mcp.json` in the first worktree. A file Hulder did not create is
/// never overwritten silently: the merged result opens as a buffer first and a
/// toast offers to apply it.
pub fn share_with_agents(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let settings = LightpandaSettings::get_global(cx).clone();
    if !settings.enabled {
        toast(workspace, disabled_message().to_string(), cx);
        return;
    }
    let project = workspace.project().clone();
    let Some(root) = project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    else {
        toast(workspace, "Open a project folder first.".into(), cx);
        return;
    };
    let fs = project.read(cx).fs().clone();
    let path = root.join(FILE_NAME);

    cx.spawn_in(window, async move |workspace, cx| {
        if let Err(error) = check_mcp_path(fs.as_ref(), &root, &path).await {
            workspace.update(cx, |workspace, cx| {
                toast(workspace, format!("Lightpanda: {error:#}"), cx);
            })?;
            return anyhow::Ok(());
        }
        let existing = if fs.is_file(&path).await {
            Some(fs.load(&path).await?)
        } else {
            None
        };
        let merged = match merge_mcp_config(existing.as_deref(), &settings.binary) {
            Ok(merged) => merged,
            Err(error) => {
                workspace.update(cx, |workspace, cx| {
                    toast(workspace, format!("Lightpanda: {error:#}"), cx);
                })?;
                return anyhow::Ok(());
            }
        };

        let Some(original) = existing else {
            fs.atomic_write(path.clone(), merged).await?;
            workspace.update(cx, |workspace, cx| {
                toast(
                    workspace,
                    format!("Wrote {}: agents can now browse through Lightpanda.", path.display()),
                    cx,
                );
            })?;
            return anyhow::Ok(());
        };
        open_text_buffer(
            workspace.clone(),
            format!("{FILE_NAME} (merged preview)"),
            merged.clone(),
            cx,
        )
        .await?;
        workspace.update(cx, |workspace, cx| {
            let fs = fs.clone();
            let root = root.clone();
            let path = path.clone();
            let binary = settings.binary.clone();
            let toast = Toast::new(
                NotificationId::unique::<ShareToast>(),
                format!("Apply the merged {FILE_NAME}? The preview tab shows the result."),
            )
            .on_click("Apply", move |_window, cx| {
                let fs = fs.clone();
                let root = root.clone();
                let path = path.clone();
                let binary = binary.clone();
                let original = original.clone();
                let merged = merged.clone();
                cx.spawn(async move |cx| {
                    let written = apply_mcp_config(
                        fs.as_ref(),
                        &root,
                        &path,
                        &original,
                        &merged,
                        &binary,
                    )
                    .await;
                    cx.update(|cx| {
                        workspace::with_active_or_new_workspace(cx, move |workspace, _, cx| {
                            let message = match written {
                                Ok(Applied::AsPreviewed) => {
                                    format!("Wrote {}.", path.display())
                                }
                                Ok(Applied::Remerged) => format!(
                                    "{} changed since the preview; merged the lightpanda entry into the current file.",
                                    path.display()
                                ),
                                Err(error) => {
                                    format!("Could not write {}: {error:#}", path.display())
                                }
                            };
                            workspace.dismiss_toast(&NotificationId::unique::<ShareToast>(), cx);
                            crate::reader::toast(workspace, message, cx);
                        });
                    });
                })
                .detach();
            });
            workspace.show_toast(toast, cx);
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_file_when_absent() {
        let out = merge_mcp_config(None, Path::new("/usr/local/bin/lightpanda")).expect("ok");
        let value: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(
            value["mcpServers"]["lightpanda"]["command"],
            "/usr/local/bin/lightpanda"
        );
        assert_eq!(value["mcpServers"]["lightpanda"]["args"][0], "mcp");
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn entry_disables_telemetry_for_agents_too() {
        let out = merge_mcp_config(None, Path::new("/p")).expect("ok");
        let value: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(
            value["mcpServers"]["lightpanda"]["env"]["LIGHTPANDA_DISABLE_TELEMETRY"],
            "true"
        );
    }

    #[test]
    fn merges_without_clobbering_other_servers() {
        let existing = r#"{"mcpServers":{"other":{"command":"x"}},"keep":true}"#;
        let out = merge_mcp_config(Some(existing), Path::new("/p")).expect("ok");
        let value: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(value["mcpServers"]["other"]["command"], "x");
        assert_eq!(value["mcpServers"]["lightpanda"]["command"], "/p");
        assert_eq!(value["keep"], true);
    }

    const ROOT: &str = "/project";

    async fn seeded(cx: &mut gpui::TestAppContext, original: &str) -> std::sync::Arc<fs::FakeFs> {
        let fs = fs::FakeFs::new(cx.executor());
        fs.atomic_write(Path::new(ROOT).join(FILE_NAME), original.to_string())
            .await
            .expect("seed");
        fs
    }

    #[gpui::test]
    async fn apply_remerges_when_the_file_changed_since_the_preview(cx: &mut gpui::TestAppContext) {
        let original = r#"{"mcpServers":{"other":{"command":"x"}}}"#;
        let fs = seeded(cx, original).await;
        let path = Path::new(ROOT).join(FILE_NAME);
        let merged = merge_mcp_config(Some(original), Path::new("/p")).expect("merge");

        // Someone edits the file between the preview and the click.
        let edited = r#"{"mcpServers":{"other":{"command":"x"},"newer":{"command":"y"}}}"#;
        fs.atomic_write(path.clone(), edited.to_string())
            .await
            .expect("edit");

        let applied = apply_mcp_config(
            fs.as_ref(),
            Path::new(ROOT),
            &path,
            original,
            &merged,
            Path::new("/p"),
        )
        .await
        .expect("apply");
        assert_eq!(applied, Applied::Remerged);
        let value: serde_json::Value =
            serde_json::from_str(&fs.load(&path).await.expect("load")).expect("json");
        assert_eq!(value["mcpServers"]["newer"]["command"], "y");
        assert_eq!(value["mcpServers"]["other"]["command"], "x");
        assert_eq!(value["mcpServers"]["lightpanda"]["command"], "/p");
    }

    #[gpui::test]
    async fn apply_writes_the_preview_into_a_regular_file(cx: &mut gpui::TestAppContext) {
        let original = r#"{"keep":true}"#;
        let fs = seeded(cx, original).await;
        let path = Path::new(ROOT).join(FILE_NAME);
        let merged = merge_mcp_config(Some(original), Path::new("/p")).expect("merge");
        let applied = apply_mcp_config(
            fs.as_ref(),
            Path::new(ROOT),
            &path,
            original,
            &merged,
            Path::new("/p"),
        )
        .await
        .expect("apply");
        assert_eq!(applied, Applied::AsPreviewed);
        assert_eq!(fs.load(&path).await.expect("load"), merged);
    }

    #[gpui::test]
    async fn symlinked_mcp_json_is_refused_and_left_alone(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let real = Path::new("/home/me/.ssh/config");
        let link = Path::new(ROOT).join(FILE_NAME);
        let original = "Host *\n";
        fs.atomic_write(real.to_path_buf(), original.to_string())
            .await
            .expect("seed");
        fs.create_dir(Path::new(ROOT)).await.expect("dir");
        fs.create_symlink(&link, real.to_path_buf())
            .await
            .expect("symlink");

        let error = check_mcp_path(fs.as_ref(), Path::new(ROOT), &link)
            .await
            .expect_err("symlink refused before preview");
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");

        let error = apply_mcp_config(
            fs.as_ref(),
            Path::new(ROOT),
            &link,
            original,
            "{}",
            Path::new("/p"),
        )
        .await
        .expect_err("symlink refused on apply");
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert_eq!(fs.load(real).await.expect("load"), original);
        let metadata = fs
            .metadata(&link)
            .await
            .expect("metadata")
            .expect("link exists");
        assert!(metadata.is_symlink);
    }

    #[gpui::test]
    async fn mcp_json_outside_the_project_is_refused(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.create_dir(Path::new(ROOT)).await.expect("dir");
        let elsewhere = Path::new("/elsewhere").join(FILE_NAME);
        fs.atomic_write(elsewhere.clone(), "{}".to_string())
            .await
            .expect("seed");
        let error = check_mcp_path(fs.as_ref(), Path::new(ROOT), &elsewhere)
            .await
            .expect_err("outside root refused");
        assert!(format!("{error:#}").contains("outside"), "{error:#}");
    }

    #[gpui::test]
    async fn absent_mcp_json_passes_the_path_check(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.create_dir(Path::new(ROOT)).await.expect("dir");
        check_mcp_path(
            fs.as_ref(),
            Path::new(ROOT),
            &Path::new(ROOT).join(FILE_NAME),
        )
        .await
        .expect("absent file is fine");
    }

    #[test]
    fn merge_server_adds_any_named_server_beside_lightpanda() {
        let with_panda = merge_mcp_config(None, Path::new("/p")).expect("ok");
        let out = merge_server(Some(&with_panda), "hulder", "/usr/local/bin/hulder", &["mcp"], &[])
            .expect("ok");
        let value: serde_json::Value = serde_json::from_str(&out).expect("json");
        assert_eq!(value["mcpServers"]["lightpanda"]["command"], "/p");
        assert_eq!(value["mcpServers"]["hulder"]["command"], "/usr/local/bin/hulder");
        assert_eq!(value["mcpServers"]["hulder"]["args"], serde_json::json!(["mcp"]));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn refuses_invalid_json() {
        assert!(merge_mcp_config(Some("{nope"), Path::new("/p")).is_err());
        assert!(merge_mcp_config(Some("[]"), Path::new("/p")).is_err());
        assert!(merge_mcp_config(Some(r#"{"mcpServers": 3}"#), Path::new("/p")).is_err());
    }
}
