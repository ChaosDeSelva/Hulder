use anyhow::{Result, bail};
use async_trait::async_trait;
use collections::HashMap;
use dap::{StartDebuggingRequestArguments, adapters::DebugTaskDefinition};
use gpui::AsyncApp;
use serde_json::Value;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};
use task::{DebugScenario, ZedDebugConfig};

use crate::*;

/// Drives the `lldb-dap` binary that ships with Xcode and the Xcode Command
/// Line Tools (and with LLVM on other platforms). Nothing is ever downloaded.
#[derive(Default)]
pub(crate) struct LldbDapDebugAdapter;

impl LldbDapDebugAdapter {
    const ADAPTER_NAME: &'static str = "LLDB";
    const BINARY_NAME: &'static str = "lldb-dap";
    const KNOWN_PATHS: [&'static str; 2] = [
        "/Applications/Xcode.app/Contents/Developer/usr/bin/lldb-dap",
        "/Library/Developer/CommandLineTools/usr/bin/lldb-dap",
    ];
}

/// Picks the first candidate that exists. Candidates are given in priority
/// order: user setting, `PATH`, `xcrun --find`, then the known install paths.
pub(crate) fn resolve_binary(
    candidates: impl IntoIterator<Item = Option<PathBuf>>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates
        .into_iter()
        .flatten()
        .find(|candidate| exists(candidate))
}

async fn xcrun_find(binary: &str) -> Option<PathBuf> {
    let output = util::command::new_command("xcrun")
        .args(["--find", binary])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// lldb-dap 1600 (Xcode 16) only accepts `env` as `["KEY=VALUE", ...]`;
/// an object written by a user or by `config_from_zed_format` is rewritten.
pub(crate) fn normalize_env(configuration: &mut Value) {
    let Some(object) = configuration.as_object_mut() else {
        return;
    };
    let Some(env) = object.get("env").and_then(Value::as_object) else {
        return;
    };
    let pairs: Vec<Value> = env
        .iter()
        .map(|(key, value)| {
            let value = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            Value::String(format!("{key}={value}"))
        })
        .collect();
    object.insert("env".into(), Value::Array(pairs));
}

#[async_trait(?Send)]
impl DebugAdapter for LldbDapDebugAdapter {
    fn name(&self) -> DebugAdapterName {
        DebugAdapterName(Self::ADAPTER_NAME.into())
    }

    async fn config_from_zed_format(&self, zed_scenario: ZedDebugConfig) -> Result<DebugScenario> {
        let mut object = serde_json::Map::default();
        object.insert("name".into(), zed_scenario.label.to_string().into());

        match &zed_scenario.request {
            dap::DebugRequest::Attach(attach) => {
                object.insert("request".into(), "attach".into());
                object.insert("pid".into(), attach.process_id.into());
            }
            dap::DebugRequest::Launch(launch) => {
                object.insert("request".into(), "launch".into());
                object.insert("program".into(), launch.program.clone().into());
                if !launch.args.is_empty() {
                    object.insert("args".into(), launch.args.clone().into());
                }
                if !launch.env.is_empty() {
                    object.insert("env".into(), launch.env_json());
                }
                if let Some(stop_on_entry) = zed_scenario.stop_on_entry {
                    object.insert("stopOnEntry".into(), stop_on_entry.into());
                }
                if let Some(cwd) = launch.cwd.as_ref() {
                    object.insert("cwd".into(), cwd.to_string_lossy().into_owned().into());
                }
            }
        }

        let mut config = Value::Object(object);
        normalize_env(&mut config);

        Ok(DebugScenario {
            adapter: zed_scenario.adapter,
            label: zed_scenario.label,
            build: None,
            config,
            tcp_connection: None,
        })
    }

    fn dap_schema(&self) -> serde_json::Value {
        json!({
            "oneOf": [
                {
                    "allOf": [
                        {
                            "type": "object",
                            "required": ["request"],
                            "properties": {
                                "request": {
                                    "type": "string",
                                    "enum": ["launch"],
                                    "description": "Request to launch a new process"
                                }
                            }
                        },
                        {
                            "type": "object",
                            "properties": {
                                "program": {
                                    "type": "string",
                                    "description": "Path to the program to debug"
                                },
                                "args": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Command line arguments passed to the program",
                                    "default": []
                                },
                                "cwd": {
                                    "type": "string",
                                    "description": "Working directory for the debugged program; defaults to the worktree root"
                                },
                                "env": {
                                    "type": ["object", "array"],
                                    "description": "Environment variables for the debugged program, as an object or as [\"KEY=VALUE\"] strings",
                                    "items": { "type": "string" }
                                },
                                "stopOnEntry": {
                                    "type": "boolean",
                                    "description": "Stop at the program's entry point",
                                    "default": false
                                },
                                "initCommands": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "LLDB commands executed before the target is created"
                                },
                                "preRunCommands": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "LLDB commands executed after the target is created, before launch"
                                },
                                "disableASLR": {
                                    "type": "boolean",
                                    "description": "Disable address space layout randomization",
                                    "default": true
                                }
                            },
                            "required": ["program"]
                        }
                    ]
                },
                {
                    "allOf": [
                        {
                            "type": "object",
                            "required": ["request"],
                            "properties": {
                                "request": {
                                    "type": "string",
                                    "enum": ["attach"],
                                    "description": "Request to attach to an existing process"
                                }
                            }
                        },
                        {
                            "type": "object",
                            "properties": {
                                "pid": {
                                    "type": "number",
                                    "description": "The process ID to attach to"
                                },
                                "program": {
                                    "type": "string",
                                    "description": "Path to the program whose process should be attached to, used when pid is omitted"
                                },
                                "waitFor": {
                                    "type": "boolean",
                                    "description": "Wait for the process named by program to launch",
                                    "default": false
                                }
                            },
                            "anyOf": [
                                { "required": ["pid"] },
                                { "required": ["program"] }
                            ]
                        }
                    ]
                }
            ]
        })
    }

    async fn get_binary(
        &self,
        delegate: &Arc<dyn DapDelegate>,
        config: &DebugTaskDefinition,
        user_installed_path: Option<PathBuf>,
        user_args: Option<Vec<String>>,
        user_env: Option<HashMap<String, String>>,
        _: &mut AsyncApp,
    ) -> Result<DebugAdapterBinary> {
        // A configured path that does not exist is a mistake to report, not
        // something to silently fall through to whatever PATH offers.
        if let Some(configured) = user_installed_path.as_ref()
            && !configured.is_file()
        {
            let message = format!(
                "dap.{}.binary is set to {} but no file exists there; fix or remove the setting.",
                Self::ADAPTER_NAME,
                configured.display()
            );
            delegate.output_to_console(message.clone());
            bail!(message);
        }
        let from_path = delegate.which(OsStr::new(Self::BINARY_NAME)).await;
        let from_xcrun = if user_installed_path.is_none() && from_path.is_none() {
            xcrun_find(Self::BINARY_NAME).await
        } else {
            None
        };
        let known_paths = Self::KNOWN_PATHS
            .iter()
            .map(|path| Some(PathBuf::from(path)));
        let candidates = [user_installed_path, from_path, from_xcrun]
            .into_iter()
            .chain(known_paths);

        let Some(binary_path) = resolve_binary(candidates, |path| path.is_file()) else {
            let message = format!(
                "Could not find {}. Install the Xcode Command Line Tools (`xcode-select --install`) or LLVM, or set dap.{}.binary in settings.",
                Self::BINARY_NAME,
                Self::ADAPTER_NAME
            );
            delegate.output_to_console(message.clone());
            bail!(message);
        };

        let mut configuration = config.config.clone();
        if let Some(object) = configuration.as_object_mut() {
            object
                .entry("cwd")
                .or_insert_with(|| delegate.worktree_root_path().to_string_lossy().into());
        }
        normalize_env(&mut configuration);

        let mut envs = delegate.shell_env().await;
        envs.extend(user_env.unwrap_or_default());

        Ok(DebugAdapterBinary {
            command: Some(binary_path.to_string_lossy().into_owned()),
            arguments: user_args.unwrap_or_default(),
            envs,
            cwd: Some(delegate.worktree_root_path().to_path_buf()),
            connection: None,
            request_args: StartDebuggingRequestArguments {
                request: self.request_kind(&configuration).await?,
                configuration,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;
    use task::{AttachRequest, DebugRequest, LaunchRequest};

    fn path(text: &str) -> PathBuf {
        PathBuf::from(text)
    }

    #[test]
    fn user_setting_wins_when_it_exists() {
        let found = resolve_binary(
            [
                Some(path("/custom/lldb-dap")),
                Some(path("/usr/bin/lldb-dap")),
                Some(path("/xcrun/lldb-dap")),
            ],
            |_| true,
        );
        assert_eq!(found, Some(path("/custom/lldb-dap")));
    }

    #[test]
    fn missing_user_setting_falls_through_to_path() {
        let found = resolve_binary(
            [
                Some(path("/custom/lldb-dap")),
                Some(path("/usr/bin/lldb-dap")),
                Some(path("/xcrun/lldb-dap")),
            ],
            |candidate| candidate != Path::new("/custom/lldb-dap"),
        );
        assert_eq!(found, Some(path("/usr/bin/lldb-dap")));
    }

    #[test]
    fn xcrun_result_is_used_before_known_paths() {
        let candidates = [None, None, Some(path("/xcrun/lldb-dap"))]
            .into_iter()
            .chain(
                LldbDapDebugAdapter::KNOWN_PATHS
                    .iter()
                    .map(|known| Some(path(known))),
            );
        let found = resolve_binary(candidates, |_| true);
        assert_eq!(found, Some(path("/xcrun/lldb-dap")));
    }

    #[test]
    fn known_paths_are_the_last_resort_in_order() {
        let candidates = [None, None, None].into_iter().chain(
            LldbDapDebugAdapter::KNOWN_PATHS
                .iter()
                .map(|known| Some(path(known))),
        );
        let found = resolve_binary(candidates, |candidate| {
            candidate == Path::new(LldbDapDebugAdapter::KNOWN_PATHS[1])
        });
        assert_eq!(found, Some(path(LldbDapDebugAdapter::KNOWN_PATHS[1])));
    }

    #[test]
    fn nothing_found_is_none() {
        let found = resolve_binary([None, Some(path("/nowhere/lldb-dap"))], |_| false);
        assert_eq!(found, None);
    }

    #[test]
    fn env_object_becomes_key_value_strings() {
        let mut config = json!({ "env": { "RUST_LOG": "debug", "N": 1 } });
        normalize_env(&mut config);
        let mut env: Vec<String> = config["env"]
            .as_array()
            .expect("array")
            .iter()
            .map(|value| value.as_str().expect("string").to_owned())
            .collect();
        env.sort();
        assert_eq!(env, vec!["N=1".to_owned(), "RUST_LOG=debug".to_owned()]);
    }

    #[test]
    fn env_array_and_missing_env_are_left_alone() {
        let mut config = json!({ "env": ["A=1"] });
        normalize_env(&mut config);
        assert_eq!(config["env"], json!(["A=1"]));

        let mut config = json!({ "program": "x" });
        normalize_env(&mut config);
        assert!(config.get("env").is_none());
    }

    #[gpui::test]
    async fn launch_config_maps_fields(_cx: &mut TestAppContext) {
        let scenario = LldbDapDebugAdapter
            .config_from_zed_format(ZedDebugConfig {
                adapter: "LLDB".into(),
                label: "Debug it".into(),
                request: DebugRequest::Launch(LaunchRequest {
                    program: "/bin/prog".into(),
                    cwd: Some(path("/work")),
                    args: vec!["--flag".into()],
                    env: [("KEY".to_owned(), "VALUE".to_owned())]
                        .into_iter()
                        .collect(),
                }),
                stop_on_entry: Some(true),
            })
            .await
            .expect("scenario");

        assert_eq!(scenario.adapter.as_ref(), "LLDB");
        assert_eq!(
            scenario.config,
            json!({
                "name": "Debug it",
                "request": "launch",
                "program": "/bin/prog",
                "args": ["--flag"],
                "env": ["KEY=VALUE"],
                "stopOnEntry": true,
                "cwd": "/work",
            })
        );
    }

    #[gpui::test]
    async fn attach_config_maps_pid(_cx: &mut TestAppContext) {
        let scenario = LldbDapDebugAdapter
            .config_from_zed_format(ZedDebugConfig {
                adapter: "LLDB".into(),
                label: "Attach".into(),
                request: DebugRequest::Attach(AttachRequest {
                    process_id: Some(4242),
                }),
                stop_on_entry: None,
            })
            .await
            .expect("scenario");

        assert_eq!(
            scenario.config,
            json!({ "name": "Attach", "request": "attach", "pid": 4242 })
        );
    }

    #[gpui::test]
    async fn get_binary_refuses_a_missing_user_setting(cx: &mut TestAppContext) {
        let delegate = crate::test_mocks::MockDelegate::new();
        let definition = DebugTaskDefinition {
            label: "Debug".into(),
            adapter: "LLDB".into(),
            config: json!({ "request": "launch", "program": "/bin/prog" }),
            tcp_connection: None,
        };
        let error = LldbDapDebugAdapter
            .get_binary(
                &delegate,
                &definition,
                Some(path("/nonexistent/lldb-dap")),
                None,
                None,
                &mut cx.to_async(),
            )
            .await
            .expect_err("missing configured binary");
        let message = format!("{error:#}");
        assert!(message.contains("dap.LLDB.binary"), "{message}");
        assert!(message.contains("/nonexistent/lldb-dap"), "{message}");
    }

    #[gpui::test]
    async fn get_binary_uses_user_setting_and_defaults_cwd(cx: &mut TestAppContext) {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let fake_binary = temp_dir.path().join("lldb-dap");
        std::fs::write(&fake_binary, b"").expect("write fake binary");

        let delegate = crate::test_mocks::MockDelegate::new();
        let definition = DebugTaskDefinition {
            label: "Debug".into(),
            adapter: "LLDB".into(),
            config: json!({ "request": "launch", "program": "/bin/prog", "env": { "A": "1" } }),
            tcp_connection: None,
        };

        let binary = LldbDapDebugAdapter
            .get_binary(
                &delegate,
                &definition,
                Some(fake_binary.clone()),
                None,
                None,
                &mut cx.to_async(),
            )
            .await
            .expect("binary");

        assert_eq!(
            binary.command.as_deref(),
            Some(fake_binary.to_string_lossy().as_ref())
        );
        assert!(binary.arguments.is_empty());
        assert_eq!(binary.request_args.configuration["cwd"], json!("/tmp/test"));
        assert_eq!(binary.request_args.configuration["env"], json!(["A=1"]));
        assert_eq!(
            binary.request_args.request,
            dap::StartDebuggingRequestArgumentsRequest::Launch
        );
    }
}
