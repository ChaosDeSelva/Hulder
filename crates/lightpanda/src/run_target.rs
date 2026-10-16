use crate::OpenRunTarget;
use crate::reader::{disabled_message, toast};
use crate::runner::{PandaRunner, RunnerState};
use crate::settings::LightpandaSettings;
use collections::HashMap;
use gpui::{Context, TaskExt as _, Window};
use settings::Settings as _;
use std::time::Duration;
use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId, TaskTemplate};
use terminal_view::terminal_panel::TerminalPanel;
use workspace::Workspace;

pub const LABEL: &str = "Run tests against Lightpanda";

/// The task template that points Playwright-style runners at the local CDP endpoint.
pub fn lightpanda_task(ws_url: &str, test_command: &str) -> TaskTemplate {
    TaskTemplate {
        label: LABEL.to_string(),
        command: test_command.to_string(),
        env: HashMap::from_iter([
            ("LIGHTPANDA_WS".to_string(), ws_url.to_string()),
            ("PLAYWRIGHT_CDP_ENDPOINT".to_string(), ws_url.to_string()),
        ]),
        reveal: RevealStrategy::Always,
        reveal_target: RevealTarget::Dock,
        use_new_terminal: true,
        allow_concurrent_runs: true,
        ..TaskTemplate::default()
    }
}

/// `npm run test:e2e` when the script exists, else `npm test`.
pub fn detect_test_command(package_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(package_json).ok()?;
    let scripts = value.get("scripts")?.as_object()?;
    if scripts.contains_key("test:e2e") {
        Some("npm run test:e2e".into())
    } else if scripts.contains_key("test") {
        Some("npm test".into())
    } else {
        None
    }
}

pub fn spawn_in_terminal(template: TaskTemplate) -> SpawnInTerminal {
    SpawnInTerminal {
        id: TaskId("lightpanda-run-target".into()),
        full_label: template.label.clone(),
        label: template.label.clone(),
        command_label: template.command.clone(),
        command: Some(template.command),
        args: template.args,
        env: template.env,
        use_new_terminal: template.use_new_terminal,
        allow_concurrent_runs: template.allow_concurrent_runs,
        reveal: template.reveal,
        reveal_target: template.reveal_target,
        ..SpawnInTerminal::default()
    }
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &OpenRunTarget, window, cx| {
        open_run_target(workspace, window, cx);
    });
}

/// Ensures the runner is up, reads the first worktree's package.json, and runs
/// the detected test command with the CDP endpoint in its environment.
pub fn open_run_target(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !LightpandaSettings::get_global(cx).enabled {
        toast(workspace, disabled_message().to_string(), cx);
        return;
    }
    let runner = PandaRunner::global(cx);
    runner.update(cx, |runner, cx| {
        if !matches!(
            runner.state,
            RunnerState::Running { .. } | RunnerState::Starting
        ) {
            runner.start(cx);
        }
    });
    let project = workspace.project().clone();
    let package_json = project
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().join("package.json"));
    let fs = project.read(cx).fs().clone();

    cx.spawn_in(window, async move |workspace, cx| {
        let mut ws_url: Option<String> = None;
        for _ in 0..20 {
            ws_url = runner.read_with(cx, |runner, _| runner.ws_url());
            if ws_url.is_some() {
                break;
            }
            let failed = runner.read_with(cx, |runner, _| match &runner.state {
                RunnerState::Failed { reason } => Some(reason.clone()),
                _ => None,
            });
            if let Some(reason) = failed {
                workspace.update(cx, |workspace, cx| {
                    toast(workspace, format!("Lightpanda did not start: {reason}"), cx);
                })?;
                return anyhow::Ok(());
            }
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
        }
        let Some(ws_url) = ws_url else {
            workspace.update(cx, |workspace, cx| {
                toast(workspace, "Lightpanda did not come up within 5 seconds.".into(), cx);
            })?;
            return anyhow::Ok(());
        };

        let test_command = match package_json {
            Some(path) => fs
                .load(&path)
                .await
                .ok()
                .and_then(|text| detect_test_command(&text)),
            None => None,
        };
        let Some(test_command) = test_command else {
            workspace.update(cx, |workspace, cx| {
                toast(
                    workspace,
                    format!(
                        "No `test:e2e` or `test` script in package.json. Run your tests with LIGHTPANDA_WS={ws_url}"
                    ),
                    cx,
                );
            })?;
            return anyhow::Ok(());
        };

        workspace.update_in(cx, |workspace, window, cx| {
            let Some(panel) = workspace.panel::<TerminalPanel>(cx) else {
                toast(workspace, "Terminal panel is not available.".into(), cx);
                return;
            };
            let task = spawn_in_terminal(lightpanda_task(&ws_url, &test_command));
            panel.update(cx, |panel, cx| {
                panel
                    .add_terminal_task(task, RevealStrategy::Always, window, cx)
                    .detach_and_log_err(cx);
            });
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_prefers_e2e_script() {
        assert_eq!(
            detect_test_command(r#"{"scripts":{"test":"vitest","test:e2e":"playwright test"}}"#),
            Some("npm run test:e2e".into())
        );
        assert_eq!(
            detect_test_command(r#"{"scripts":{"test":"vitest"}}"#),
            Some("npm test".into())
        );
        assert_eq!(detect_test_command(r#"{"name":"x"}"#), None);
        assert_eq!(detect_test_command("not json"), None);
    }

    #[test]
    fn task_sets_both_env_vars() {
        let task = lightpanda_task("ws://127.0.0.1:9222", "npm test");
        assert_eq!(
            task.env.get("LIGHTPANDA_WS").map(String::as_str),
            Some("ws://127.0.0.1:9222")
        );
        assert_eq!(
            task.env.get("PLAYWRIGHT_CDP_ENDPOINT").map(String::as_str),
            Some("ws://127.0.0.1:9222")
        );
        assert_eq!(task.command, "npm test");
        assert_eq!(task.label, LABEL);
    }

    #[test]
    fn spawn_carries_env_and_command() {
        let spawn = spawn_in_terminal(lightpanda_task("ws://127.0.0.1:1", "npm test"));
        assert_eq!(spawn.command.as_deref(), Some("npm test"));
        assert_eq!(spawn.env.len(), 2);
        assert_eq!(spawn.reveal, RevealStrategy::Always);
    }
}
