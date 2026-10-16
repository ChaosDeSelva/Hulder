use crate::ShareWithAgents;
use anyhow::{Context as _, Result, bail};
use fs::Fs;
use gpui::{BackgroundExecutor, Context, TaskExt as _, Window};
use lightpanda::mcp::{FILE_NAME, check_mcp_path, merge_server};
use lightpanda::reader::toast;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use workspace::Workspace;

pub const SERVER_NAME: &str = "hulder";
pub const INSTALLED_CLI: &str = "/usr/local/bin/hulder";
pub const SERVER_ARGS: [&str; 1] = ["mcp"];

/// `.mcp.json` is a few servers; refuse to merge into a huge one.
pub const MAX_MCP_BYTES: u64 = 1024 * 1024;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ShareWithAgents, window, cx| {
        share_with_agents(workspace, window, cx);
    });
}

/// Picks the first candidate that exists: the installed `hulder` CLI, then the
/// `cli` binary next to the running app, then the running executable itself.
pub fn pick_cli_command(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|path| path.is_file()).cloned()
}

fn cli_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from(INSTALLED_CLI)];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("cli"));
        }
        candidates.push(exe);
    }
    candidates
}

/// The absolute CLI every writer of the hulder server entry points at, so
/// the Kit and the Go menu produce the same `.mcp.json` line.
pub fn cli_command() -> Option<PathBuf> {
    pick_cli_command(&cli_candidates())
}

/// Reads `.mcp.json` without following a link at the file and with a cap on
/// the bytes read, on a background thread. A missing file is `None`.
async fn read_existing(
    fs: &dyn Fs,
    executor: &BackgroundExecutor,
    path: &Path,
) -> Result<Option<String>> {
    if fs.metadata(path).await?.is_none() {
        return Ok(None);
    }
    let reader = fs.open_sync_nofollow(path).await?;
    let bytes = executor
        .spawn(async move {
            let mut bytes = Vec::new();
            reader.take(MAX_MCP_BYTES + 1).read_to_end(&mut bytes)?;
            anyhow::Ok(bytes)
        })
        .await?;
    if bytes.len() as u64 > MAX_MCP_BYTES {
        bail!("{FILE_NAME} is larger than the {MAX_MCP_BYTES} byte limit; edit it by hand");
    }
    Ok(Some(
        String::from_utf8(bytes).with_context(|| format!("{FILE_NAME} is not UTF-8"))?,
    ))
}

/// Merges the `hulder` server into `<root>/.mcp.json` through the same
/// path check Lightpanda uses, re-run right before the write. Other servers
/// in the file are kept.
pub async fn write_server_entry(
    fs: &dyn Fs,
    executor: &BackgroundExecutor,
    root: &Path,
    command: &Path,
) -> Result<PathBuf> {
    let path = root.join(FILE_NAME);
    check_mcp_path(fs, root, &path).await?;
    let existing = read_existing(fs, executor, &path).await?;
    let merged = merge_server(
        existing.as_deref(),
        SERVER_NAME,
        &command.to_string_lossy(),
        &SERVER_ARGS,
        &[],
    )?;
    check_mcp_path(fs, root, &path).await?;
    fs.atomic_write(path.clone(), merged).await?;
    Ok(path)
}

/// Writes the `hulder` server into the first worktree's `.mcp.json` and
/// reports the outcome as a toast.
pub fn share_with_agents(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
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
    let Some(command) = cli_command() else {
        toast(
            workspace,
            "Bifröst: no hulder CLI found to point agents at.".into(),
            cx,
        );
        return;
    };
    let fs = project.read(cx).fs().clone();
    let executor = cx.background_executor().clone();

    cx.spawn_in(window, async move |workspace, cx| {
        let outcome = match write_server_entry(fs.as_ref(), &executor, &root, &command).await {
            Ok(path) => format!(
                "Wrote {}: agents can now reach the editor over Bifröst.",
                path.display()
            ),
            Err(error) => format!("Bifröst: {error:#}"),
        };
        workspace.update(cx, |workspace, cx| toast(workspace, outcome, cx))?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    #[test]
    fn picks_first_existing_candidate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let present = dir.path().join("cli");
        std::fs::write(&present, "").expect("write");
        let candidates = vec![
            dir.path().join("missing"),
            present.clone(),
            dir.path().join("later"),
        ];
        assert_eq!(pick_cli_command(&candidates), Some(present));
        assert_eq!(pick_cli_command(&[dir.path().join("nope")]), None);
    }

    #[gpui::test]
    async fn writes_the_absolute_cli_and_keeps_other_servers(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/repo",
            json!({ ".mcp.json": "{\"mcpServers\":{\"lightpanda\":{\"command\":\"/opt/lp\"}}}" }),
        )
        .await;
        let executor = cx.executor();
        let path = write_server_entry(
            fs.as_ref(),
            &executor,
            Path::new("/repo"),
            Path::new("/usr/local/bin/hulder"),
        )
        .await
        .expect("write");
        assert_eq!(path, PathBuf::from("/repo/.mcp.json"));
        let written: serde_json::Value =
            serde_json::from_str(&fs.load(&path).await.expect("load")).expect("json");
        assert_eq!(
            written["mcpServers"]["hulder"]["command"],
            "/usr/local/bin/hulder"
        );
        assert_eq!(written["mcpServers"]["hulder"]["args"], json!(["mcp"]));
        assert_eq!(written["mcpServers"]["lightpanda"]["command"], "/opt/lp");

        fs.insert_tree("/fresh", json!({ "src": {} })).await;
        write_server_entry(
            fs.as_ref(),
            &executor,
            Path::new("/fresh"),
            Path::new("/usr/local/bin/hulder"),
        )
        .await
        .expect("create");
        assert!(fs.is_file(Path::new("/fresh/.mcp.json")).await);
    }

    #[gpui::test]
    async fn refuses_a_symlinked_or_oversized_mcp_file(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/repo", json!({ "src": {}, "elsewhere.json": "{}" }))
            .await;
        fs.insert_symlink("/repo/.mcp.json", PathBuf::from("/repo/elsewhere.json"))
            .await;
        let executor = cx.executor();
        let error = write_server_entry(
            fs.as_ref(),
            &executor,
            Path::new("/repo"),
            Path::new("/usr/local/bin/hulder"),
        )
        .await
        .expect_err("symlink");
        assert!(error.to_string().contains("symlink"), "{error:#}");
        assert_eq!(
            fs.load(Path::new("/repo/elsewhere.json"))
                .await
                .expect("load"),
            "{}"
        );

        fs.insert_tree(
            "/big",
            json!({ ".mcp.json": "x".repeat(MAX_MCP_BYTES as usize + 1) }),
        )
        .await;
        let error = write_server_entry(
            fs.as_ref(),
            &executor,
            Path::new("/big"),
            Path::new("/usr/local/bin/hulder"),
        )
        .await
        .expect_err("too big");
        assert!(error.to_string().contains("limit"), "{error:#}");
    }
}
