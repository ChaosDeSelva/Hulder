use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub agent: String,
    pub event: String,
    pub matcher: String,
    pub kind: String,
    pub description: String,
    pub path: String,
    pub disabled: bool,
}

pub fn configuration_paths() -> Vec<PathBuf> {
    let home = paths::home_dir();
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let codex = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    vec![
        claude.join("settings.json"),
        codex.join("hooks.json"),
        codex.join("config.toml"),
    ]
}

pub fn parse(text: &str, path: &str, agent: &str) -> Result<Vec<Hook>, String> {
    let value: Value = if path.ends_with(".toml") {
        let document: toml::Value =
            toml::from_str(text).map_err(|_| "Invalid TOML configuration".to_string())?;
        serde_json::to_value(document).map_err(|_| "Cannot read TOML configuration".to_string())?
    } else {
        serde_json::from_str(text).map_err(|_| "Invalid JSON configuration".to_string())?
    };
    let disabled = value.get("disableAllHooks").and_then(Value::as_bool) == Some(true)
        || value.pointer("/features/hooks").and_then(Value::as_bool) == Some(false);
    let mut hooks = Vec::new();
    if let Some(events) = value.get("hooks") {
        let events = events.as_object().ok_or("Hooks must be an object")?;
        for (event, groups) in events {
            let groups = groups.as_array().ok_or("Hook event must contain a list")?;
            for group in groups {
                let handlers = group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .ok_or("Hook matcher must contain a hooks list")?;
                for handler in handlers {
                    hooks.push(Hook {
                        agent: agent.into(),
                        event: event.clone(),
                        matcher: group
                            .get("matcher")
                            .and_then(Value::as_str)
                            .unwrap_or("All matches")
                            .into(),
                        kind: handler
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("Unknown type")
                            .into(),
                        description: handler
                            .get("command")
                            .or_else(|| handler.get("url"))
                            .or_else(|| handler.get("prompt"))
                            .map(crate::activity_data::redact)
                            .unwrap_or_default(),
                        path: path.into(),
                        disabled: disabled
                            || handler.get("enabled").and_then(Value::as_bool) == Some(false),
                    });
                }
            }
        }
    }
    if value.get("notify").is_some() && agent == "Codex" {
        hooks.push(Hook {
            agent: agent.into(),
            event: "Notification".into(),
            matcher: "Legacy notify configuration".into(),
            kind: "command".into(),
            description: "Legacy notification command; open source to inspect".into(),
            path: path.into(),
            disabled: false,
        });
    }
    Ok(hooks)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_multiple_handlers_without_exposing_commands() {
        let hooks = parse(r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"SECRET=value script"},{"type":"prompt"}]}]}}"#, ".claude/settings.json", "Claude").unwrap();
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].matcher, "Bash");
        assert!(!hooks[0].description.contains("value"));
    }
    #[test]
    fn reads_codex_toml_and_disabled_state() {
        let hooks = parse("[features]\nhooks=false\n[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype='command'\ncommand='true'", ".codex/config.toml", "Codex").unwrap();
        assert!(hooks[0].disabled);
    }
    #[test]
    fn rejects_malformed_configuration() {
        assert!(parse("{", "settings.json", "Claude").is_err());
        assert!(parse(r#"{"hooks":{"Stop":{}}}"#, "settings.json", "Claude").is_err());
    }
}
