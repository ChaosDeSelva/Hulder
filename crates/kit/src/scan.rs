//! Pure discovery of what an agent will use in a project: skills, subagent
//! definitions, MCP servers and rule files. Driven by a list of relative unix
//! paths plus a reader closure so the app can feed it a worktree snapshot and
//! tests can feed it a temp dir.

use anyhow::Context as _;
use collections::HashMap;

/// A skill found at `.claude/skills/<dir>/SKILL.md` or `skills/<dir>/SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The first "Use when" / "Trigger" line, from the body or the description.
    pub trigger: Option<String>,
    pub path: String,
}

/// A subagent definition found at `.claude/agents/<name>.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    pub name: String,
    pub model: Option<String>,
    pub tools: Option<String>,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpSource {
    /// From the project's `.mcp.json`.
    Project,
    /// From `~/.claude.json`.
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub source: McpSource,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub label: String,
    pub path: String,
    /// Extra context such as the number of hook events in a settings file.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    pub found: Vec<Rule>,
    /// Files an agent would read that this project does not have.
    pub missing: Vec<&'static str>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KitScan {
    pub skills: Vec<Skill>,
    pub agents: Vec<AgentDef>,
    pub mcp: Vec<McpServer>,
    pub rules: Rules,
    pub hooks: Vec<crate::hooks::Hook>,
    pub hook_errors: Vec<(String, String)>,
}

pub const MCP_FILE: &str = ".mcp.json";
const SETTINGS_FILES: [&str; 4] = [
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".codex/hooks.json",
    ".codex/config.toml",
];
const RULE_FILES: [&str; 3] = ["CLAUDE.md", "AGENTS.md", ".rules"];
const MUST_HAVE: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Whether `path` is one the scan reads, so the app only loads those files.
pub fn is_relevant(path: &str) -> bool {
    config_path(path).is_some()
}

pub fn relevant_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| is_relevant(path))
        .cloned()
        .collect()
}

fn config_path(path: &str) -> Option<&str> {
    if path.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return None;
    }
    let config = std::iter::once(path)
        .chain(path.match_indices('/').map(|(index, _)| &path[index + 1..]))
        .find(|candidate| {
            is_skill_path(candidate)
                || is_agent_path(candidate)
                || *candidate == MCP_FILE
                || RULE_FILES.contains(candidate)
                || SETTINGS_FILES.contains(candidate)
                || is_command_path(candidate)
        })?;
    let scope = &path[..path.len() - config.len()];
    if scope.split('/').any(|part| {
        matches!(
            part,
            ".git"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | ".venv"
                | "venv"
                | "vendor"
                | ".next"
                | "__pycache__"
        )
    }) {
        return None;
    }
    Some(config)
}

fn is_skill_path(path: &str) -> bool {
    let Some(rest) = [
        ".claude/skills/",
        ".agents/skills/",
        ".codex/skills/",
        "skills/",
    ]
    .iter()
    .find_map(|prefix| path.strip_prefix(prefix)) else {
        return false;
    };
    let mut components = rest.split('/');
    matches!((components.next(), components.next(), components.next()),
        (Some(dir), Some("SKILL.md"), None) if !dir.is_empty())
}

fn is_agent_path(path: &str) -> bool {
    path.strip_prefix(".claude/agents/")
        .is_some_and(|rest| !rest.contains('/') && rest.ends_with(".md"))
}

fn is_command_path(path: &str) -> bool {
    path.strip_prefix(".claude/commands/")
        .is_some_and(|rest| !rest.contains('/') && rest.ends_with(".md"))
}

/// Splits YAML-ish front matter (`---` fenced `key: value` lines) from the body.
/// Malformed or missing front matter yields an empty map and the whole text.
pub fn parse_front_matter(text: &str) -> (HashMap<String, String>, &str) {
    let mut fields = HashMap::default();
    let Some(after_open) = text.strip_prefix("---") else {
        return (fields, text);
    };
    let Some(after_open) = after_open
        .strip_prefix('\n')
        .or_else(|| after_open.strip_prefix("\r\n"))
    else {
        return (fields, text);
    };
    let mut body_start = None;
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            body_start = Some(offset + line.len());
            break;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            if !key.is_empty() && !key.starts_with(' ') {
                fields.insert(key.to_string(), unquote(value.trim()).to_string());
            }
        }
        offset += line.len();
    }
    match body_start {
        Some(start) => (fields, after_open.get(start..).unwrap_or("")),
        None => (HashMap::default(), text),
    }
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// The first line that tells an agent when to use a skill.
pub fn trigger_line(description: &str, body: &str) -> Option<String> {
    let starts_trigger = |line: &str| {
        let lower = line
            .trim_start_matches(['-', '*', '>', ' ', '#'])
            .to_ascii_lowercase();
        lower.starts_with("use when") || lower.starts_with("trigger")
    };
    body.lines()
        .map(str::trim)
        .find(|line| starts_trigger(line))
        .map(|line| {
            line.trim_start_matches(['-', '*', '>', ' ', '#'])
                .to_string()
        })
        .or_else(|| {
            let lower = description.to_ascii_lowercase();
            let start = lower.find("use when")?;
            Some(description.get(start..)?.trim().to_string())
        })
}

fn skill_dir_name(path: &str) -> &str {
    path.rsplit('/').nth(1).unwrap_or(path)
}

fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name)
}

fn parse_servers(json: &str, source: McpSource, path: &str) -> anyhow::Result<Vec<McpServer>> {
    let root: serde_json::Value = serde_json::from_str(json)?;
    let Some(servers) = root.get("mcpServers").and_then(|value| value.as_object()) else {
        return Ok(Vec::new());
    };
    Ok(servers
        .iter()
        .map(|(name, entry)| McpServer {
            name: name.clone(),
            command: entry
                .get("command")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            args: entry
                .get("args")
                .and_then(|value| value.as_array())
                .map(|args| {
                    args.iter()
                        .filter_map(|arg| arg.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            source,
            path: path.to_string(),
        })
        .collect())
}

fn settings_detail(json: &str) -> Option<String> {
    let root: serde_json::Value = serde_json::from_str(json).ok()?;
    let hooks = root.get("hooks")?.as_object()?;
    let count = hooks.len();
    Some(if count == 1 {
        "1 hook event".to_string()
    } else {
        format!("{count} hook events")
    })
}

/// Scans `paths` (relative, unix separators, files only) with `read` giving a
/// file's text, and `user_config` holding `~/.claude.json` when it exists.
pub fn scan(
    paths: &[String],
    read: &dyn Fn(&str) -> Option<String>,
    user_config: Option<&str>,
) -> KitScan {
    let mut sorted: Vec<&String> = paths.iter().filter(|path| is_relevant(path)).collect();
    sorted.sort();

    let mut result = KitScan::default();
    for path in sorted {
        let path = path.as_str();
        let Some(config) = config_path(path) else {
            continue;
        };
        if is_skill_path(config) {
            let text = read(path).unwrap_or_default();
            let (fields, body) = parse_front_matter(&text);
            let description = fields.get("description").cloned().unwrap_or_default();
            result.skills.push(Skill {
                name: fields
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| skill_dir_name(path).to_string()),
                trigger: trigger_line(&description, body),
                description,
                path: path.to_string(),
            });
        } else if is_agent_path(config) {
            let text = read(path).unwrap_or_default();
            let (fields, _) = parse_front_matter(&text);
            result.agents.push(AgentDef {
                name: fields
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| file_stem(path).to_string()),
                model: fields.get("model").cloned(),
                tools: fields.get("tools").cloned(),
                path: path.to_string(),
            });
        } else if config == MCP_FILE {
            match read(path).map(|json| parse_servers(&json, McpSource::Project, path)) {
                Some(Ok(servers)) => result.mcp.extend(servers),
                Some(Err(error)) => log::warn!("kit: cannot parse {path}: {error:#}"),
                None => {}
            }
        } else if RULE_FILES.contains(&config) {
            result.rules.found.push(Rule {
                label: path.to_string(),
                path: path.to_string(),
                detail: None,
            });
        } else if SETTINGS_FILES.contains(&config) {
            if let Some(text) = read(path) {
                let agent = if config.starts_with(".codex/") {
                    "Codex"
                } else {
                    "Claude"
                };
                match crate::hooks::parse(&text, path, agent) {
                    Ok(hooks) => result.hooks.extend(hooks),
                    Err(error) => result.hook_errors.push((path.to_string(), error)),
                }
            }
            result.rules.found.push(Rule {
                label: path.to_string(),
                path: path.to_string(),
                detail: read(path).and_then(|json| settings_detail(&json)),
            });
        } else if is_command_path(config) {
            result.rules.found.push(Rule {
                label: format!("/{}", file_stem(path)),
                path: path.to_string(),
                detail: Some("command".to_string()),
            });
        }
    }

    if let Some(json) = user_config {
        match parse_servers(json, McpSource::User, "~/.claude.json") {
            Ok(servers) => {
                for server in servers {
                    if !result
                        .mcp
                        .iter()
                        .any(|existing| existing.path == MCP_FILE && existing.name == server.name)
                    {
                        result.mcp.push(server);
                    }
                }
            }
            Err(error) => log::warn!("kit: cannot parse ~/.claude.json: {error:#}"),
        }
    }

    result
        .rules
        .found
        .sort_by_cached_key(|rule| (rule_rank(&rule.path), rule.path.clone()));
    result.rules.missing = MUST_HAVE
        .into_iter()
        .filter(|name| !paths.iter().any(|path| path == name))
        .collect();
    result
}

/// Root rule files in `RULE_FILES` order, then settings, then commands.
fn rule_rank(path: &str) -> usize {
    RULE_FILES
        .iter()
        .position(|name| *name == path)
        .unwrap_or(if SETTINGS_FILES.contains(&path) {
            RULE_FILES.len()
        } else {
            RULE_FILES.len() + 1
        })
}

/// Adds (or replaces) one `mcpServers` entry, keeping every other key as is.
pub fn merge_mcp_server(
    existing: Option<&str>,
    name: &str,
    entry: serde_json::Value,
) -> anyhow::Result<String> {
    let mut root: serde_json::Value = match existing {
        Some(text) => serde_json::from_str(text)
            .with_context(|| format!("{MCP_FILE} is not valid JSON; fix it by hand first"))?,
        None => serde_json::json!({}),
    };
    let servers = root
        .as_object_mut()
        .with_context(|| format!("{MCP_FILE} must be a JSON object"))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    servers
        .as_object_mut()
        .context("mcpServers must be a JSON object")?
        .insert(name.to_string(), entry);
    Ok(serde_json::to_string_pretty(&root)? + "\n")
}

/// Whether `.mcp.json` already lists a server under `name`.
pub fn has_mcp_server(existing: Option<&str>, name: &str) -> bool {
    existing
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|root| root.get("mcpServers")?.get(name).map(|_| ()))
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn nested_hook_sources_and_errors_are_visible() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "apps/game/.claude/settings.local.json",
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"true"}]}]}}"#,
        );
        write(
            dir.path(),
            "apps/game/.codex/config.toml",
            "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype='command'\ncommand='true'",
        );
        write(dir.path(), ".claude/settings.json", "invalid");
        let scan = scan_dir(dir.path(), None);
        assert_eq!(scan.hooks.len(), 2);
        assert_eq!(scan.hook_errors.len(), 1);
        assert!(
            scan.hooks
                .iter()
                .all(|hook| hook.path.starts_with("apps/game/"))
        );
        assert!(!is_relevant("node_modules/vendor/.codex/hooks.json"));
    }

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    fn walk(root: &Path) -> Vec<String> {
        fn visit(root: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("read_dir") {
                let entry = entry.expect("entry");
                let path = entry.path();
                if path.is_dir() {
                    visit(root, &path, out);
                } else {
                    let relative = path.strip_prefix(root).expect("relative");
                    out.push(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let mut out = Vec::new();
        visit(root, root, &mut out);
        out
    }

    fn scan_dir(root: &Path, user_config: Option<&str>) -> KitScan {
        let paths = walk(root);
        let read = |relative: &str| std::fs::read_to_string(root.join(relative)).ok();
        scan(&paths, &read, user_config)
    }

    #[test]
    fn finds_skills_in_both_locations_with_front_matter_and_trigger() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            ".claude/skills/review/SKILL.md",
            "---\nname: Review\ndescription: \"Reviews code: carefully\"\n---\n\n# Review\n\nUse when a PR needs eyes.\n",
        );
        write(
            dir.path(),
            "skills/deploy/SKILL.md",
            "---\ndescription: Ships things. Use when deploying to prod.\n---\nbody\n",
        );
        write(dir.path(), "skills/README.md", "not a skill");
        write(dir.path(), "skills/nested/deep/SKILL.md", "too deep");

        let scan = scan_dir(dir.path(), None);
        assert_eq!(
            scan.skills,
            vec![
                Skill {
                    name: "Review".into(),
                    description: "Reviews code: carefully".into(),
                    trigger: Some("Use when a PR needs eyes.".into()),
                    path: ".claude/skills/review/SKILL.md".into(),
                },
                Skill {
                    name: "deploy".into(),
                    description: "Ships things. Use when deploying to prod.".into(),
                    trigger: Some("Use when deploying to prod.".into()),
                    path: "skills/deploy/SKILL.md".into(),
                },
            ]
        );
    }

    #[test]
    fn front_matter_without_fence_or_unterminated_is_ignored() {
        let (fields, body) = parse_front_matter("# Title\nname: nope\n");
        assert!(fields.is_empty());
        assert_eq!(body, "# Title\nname: nope\n");

        let (fields, body) = parse_front_matter("---\nname: open\nno end");
        assert!(fields.is_empty());
        assert_eq!(body, "---\nname: open\nno end");

        let (fields, body) = parse_front_matter("---\r\nname: crlf\r\n---\r\nbody");
        assert_eq!(fields.get("name").map(String::as_str), Some("crlf"));
        assert_eq!(body, "body");
    }

    #[test]
    fn finds_agents_with_model_and_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            ".claude/agents/builder.md",
            "---\nname: Builder\nmodel: opus\ntools: Read, Edit\n---\nDoes builds.\n",
        );
        write(
            dir.path(),
            ".claude/agents/bare.md",
            "No front matter here.\n",
        );
        write(
            dir.path(),
            ".claude/agents/sub/ignored.md",
            "---\nname: x\n---\n",
        );

        let scan = scan_dir(dir.path(), None);
        assert_eq!(
            scan.agents,
            vec![
                AgentDef {
                    name: "bare".into(),
                    model: None,
                    tools: None,
                    path: ".claude/agents/bare.md".into(),
                },
                AgentDef {
                    name: "Builder".into(),
                    model: Some("opus".into()),
                    tools: Some("Read, Edit".into()),
                    path: ".claude/agents/builder.md".into(),
                },
            ]
        );
    }

    #[test]
    fn lists_mcp_servers_from_project_and_user_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            ".mcp.json",
            r#"{"mcpServers": {"hulder": {"command": "hulder", "args": ["mcp"]}, "weird": {}}}"#,
        );
        let user = r#"{"mcpServers": {"hulder": {"command": "other"}, "panda": {"command": "/usr/local/bin/lightpanda", "args": ["mcp", "--port", "9222"]}}}"#;

        let scan = scan_dir(dir.path(), Some(user));
        assert_eq!(
            scan.mcp,
            vec![
                McpServer {
                    name: "hulder".into(),
                    command: "hulder".into(),
                    args: vec!["mcp".into()],
                    source: McpSource::Project,
                    path: MCP_FILE.into(),
                },
                McpServer {
                    name: "weird".into(),
                    command: String::new(),
                    args: Vec::new(),
                    source: McpSource::Project,
                    path: MCP_FILE.into(),
                },
                McpServer {
                    name: "panda".into(),
                    command: "/usr/local/bin/lightpanda".into(),
                    args: vec!["mcp".into(), "--port".into(), "9222".into()],
                    source: McpSource::User,
                    path: "~/.claude.json".into(),
                },
            ]
        );
    }

    #[test]
    fn invalid_mcp_json_yields_no_servers_but_still_scans_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), ".mcp.json", "{ not json");
        write(dir.path(), "CLAUDE.md", "rules");
        let scan = scan_dir(dir.path(), Some("also not json"));
        assert!(scan.mcp.is_empty());
        assert_eq!(scan.rules.found.len(), 1);
    }

    #[test]
    fn rules_checklist_and_missing_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "CLAUDE.md", "rules");
        write(dir.path(), ".rules", "rules");
        write(
            dir.path(),
            ".claude/settings.json",
            r#"{"hooks": {"PreToolUse": [], "Stop": []}}"#,
        );
        write(
            dir.path(),
            ".claude/settings.local.json",
            r#"{"permissions": {}}"#,
        );
        write(dir.path(), ".claude/commands/ship.md", "ship it");
        write(
            dir.path(),
            "src/CLAUDE.md",
            "nested files are not root rules",
        );

        let scan = scan_dir(dir.path(), None);
        let labels: Vec<(&str, Option<&str>)> = scan
            .rules
            .found
            .iter()
            .map(|rule| (rule.label.as_str(), rule.detail.as_deref()))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("CLAUDE.md", None),
                (".rules", None),
                (".claude/settings.json", Some("2 hook events")),
                (".claude/settings.local.json", None),
                ("/ship", Some("command")),
                ("src/CLAUDE.md", None),
            ]
        );
        assert_eq!(scan.rules.missing, vec!["AGENTS.md"]);

        let empty = scan_dir(tempfile::tempdir().expect("tempdir").path(), None);
        assert_eq!(empty.rules.missing, vec!["AGENTS.md", "CLAUDE.md"]);
    }

    #[test]
    fn relevant_paths_filters_to_what_the_scan_reads() {
        let paths: Vec<String> = [
            "src/main.rs",
            ".claude/skills/a/SKILL.md",
            ".claude/agents/b.md",
            ".mcp.json",
            "AGENTS.md",
            ".claude/commands/c.md",
            "node_modules/.claude/agents/x.md",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            relevant_paths(&paths),
            vec![
                ".claude/skills/a/SKILL.md",
                ".claude/agents/b.md",
                ".mcp.json",
                "AGENTS.md",
                ".claude/commands/c.md",
            ]
        );
    }

    #[test]
    fn nested_projects_keep_distinct_config_paths_and_user_servers() {
        let dir = tempfile::tempdir().expect("tempdir");
        for project in ["apps/web", "services/api"] {
            write(
                dir.path(),
                &format!("{project}/.claude/skills/review/SKILL.md"),
                "---\nname: Review\n---\nUse when reviewing.\n",
            );
            write(
                dir.path(),
                &format!("{project}/.claude/agents/builder.md"),
                "builder",
            );
            write(
                dir.path(),
                &format!("{project}/.mcp.json"),
                r#"{"mcpServers":{"shared":{"command":"worker"}}}"#,
            );
            write(dir.path(), &format!("{project}/AGENTS.md"), "rules");
            write(
                dir.path(),
                &format!("{project}/.claude/settings.json"),
                r#"{"hooks":{"Stop":[]}}"#,
            );
            write(
                dir.path(),
                &format!("{project}/.claude/commands/check.md"),
                "check",
            );
        }
        write(dir.path(), "apps/web/.agents/skills/test/SKILL.md", "test");
        write(
            dir.path(),
            "services/api/.codex/skills/test/SKILL.md",
            "test",
        );
        write(
            dir.path(),
            "apps/web/node_modules/pkg/AGENTS.md",
            "dependency",
        );
        let result = scan_dir(
            dir.path(),
            Some(r#"{"mcpServers":{"shared":{"command":"global"}}}"#),
        );
        assert_eq!(result.skills.len(), 4);
        assert_eq!(result.agents.len(), 2);
        assert_eq!(
            result
                .mcp
                .iter()
                .map(|server| server.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "apps/web/.mcp.json",
                "services/api/.mcp.json",
                "~/.claude.json"
            ]
        );
        assert_eq!(result.rules.found.len(), 6);
        assert_eq!(result.rules.missing, vec!["AGENTS.md", "CLAUDE.md"]);
        assert!(result.skills.iter().all(|skill| skill.path.starts_with("apps/") || skill.path.starts_with("services/")));
    }

    #[test]
    fn skips_dependency_build_and_unsafe_paths() {
        for path in [
            "node_modules/pkg/AGENTS.md",
            "apps/web/target/.mcp.json",
            ".git/CLAUDE.md",
            "../AGENTS.md",
            "/AGENTS.md",
            "apps//AGENTS.md",
            "apps/web/.venv/CLAUDE.md",
        ] {
            assert!(!is_relevant(path), "{path}");
        }
        assert!(is_relevant("apps/web/AGENTS.md"));
        assert!(is_relevant("apps/web/.claude/skills/build/SKILL.md"));
        assert!(is_relevant("skills/build/SKILL.md"));
        assert!(is_relevant("apps/web/.claude/commands/check.md"));
    }

    #[test]
    fn merge_mcp_server_keeps_siblings_and_rejects_bad_json() {
        let existing = r#"{"other": 1, "mcpServers": {"panda": {"command": "lightpanda"}}}"#;
        let merged = merge_mcp_server(
            Some(existing),
            "hulder",
            serde_json::json!({"command": "hulder", "args": ["mcp"]}),
        )
        .expect("merge");
        let root: serde_json::Value = serde_json::from_str(&merged).expect("json");
        assert_eq!(root["other"], 1);
        assert_eq!(root["mcpServers"]["panda"]["command"], "lightpanda");
        assert_eq!(root["mcpServers"]["hulder"]["args"][0], "mcp");
        assert!(merged.ends_with('\n'));

        let fresh = merge_mcp_server(None, "hulder", serde_json::json!({"command": "hulder"}))
            .expect("merge");
        assert!(has_mcp_server(Some(&fresh), "hulder"));
        assert!(!has_mcp_server(Some(&fresh), "panda"));
        assert!(!has_mcp_server(None, "hulder"));

        assert!(merge_mcp_server(Some("{ nope"), "hulder", serde_json::json!({})).is_err());
        assert!(merge_mcp_server(Some("[]"), "hulder", serde_json::json!({})).is_err());
    }
}
