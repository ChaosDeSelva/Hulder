use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const FILE_LIMIT: usize = 24;
const TAIL_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: String,
    pub agent: String,
    pub name: String,
    pub status: String,
    pub timestamp: String,
    pub duration: Option<i64>,
    started_at: Option<String>,
    pub input: String,
    pub output: String,
    pub source: PathBuf,
    pub cwd: String,
    pub hook: bool,
    pub call_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub parent_tool_id: String,
    pub parent_thread_id: String,
    pub child_agent_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCategory {
    BuiltIn,
    Mcp,
    Hook,
    Unknown,
}
impl ToolCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "Built in",
            Self::Mcp => "MCP tools",
            Self::Hook => "Hooks",
            Self::Unknown => "Unknown",
        }
    }
}
impl Entry {
    pub fn session_key(&self) -> String {
        format!(
            "{}:{}",
            self.agent,
            if self.session_id.is_empty() {
                self.source.display().to_string()
            } else {
                self.session_id.clone()
            }
        )
    }

    pub fn mcp_server(&self) -> Option<&str> {
        let name = self.name.strip_prefix("functions.").unwrap_or(&self.name);
        let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
        (!server.is_empty() && !tool.is_empty()).then_some(server)
    }
    pub fn category(&self) -> ToolCategory {
        if self.hook {
            return ToolCategory::Hook;
        }
        if self.mcp_server().is_some() {
            return ToolCategory::Mcp;
        }
        let known = match self.agent.as_str() {
            "Claude" => matches!(
                self.name.as_str(),
                "Read"
                    | "Write"
                    | "Edit"
                    | "MultiEdit"
                    | "Bash"
                    | "Glob"
                    | "Grep"
                    | "LS"
                    | "NotebookEdit"
                    | "WebFetch"
                    | "WebSearch"
                    | "TodoWrite"
                    | "Task"
                    | "Agent"
                    | "AskUserQuestion"
                    | "EnterPlanMode"
                    | "ExitPlanMode"
                    | "TaskCreate"
                    | "TaskUpdate"
                    | "TaskList"
                    | "TaskGet"
                    | "TaskOutput"
                    | "TaskStop"
                    | "Skill"
                    | "ToolSearch"
            ),
            "Codex" => matches!(
                self.name.strip_prefix("functions.").unwrap_or(&self.name),
                "exec_command"
                    | "write_stdin"
                    | "apply_patch"
                    | "view_image"
                    | "update_plan"
                    | "request_user_input"
                    | "shell"
                    | "shell_command"
                    | "spawn_agent"
                    | "send_input"
                    | "wait"
                    | "close_agent"
                    | "resume_agent"
            ),
            _ => false,
        };
        if known {
            ToolCategory::BuiltIn
        } else {
            ToolCategory::Unknown
        }
    }
    pub fn matches_category(&self, filter: &str) -> bool {
        filter == "All tools"
            || self.category().label() == filter
            || filter == "Custom"
                && matches!(self.category(), ToolCategory::Mcp | ToolCategory::Hook)
    }
    pub fn category_evidence(&self) -> &'static str {
        match self.category() {
            ToolCategory::BuiltIn => {
                "Recognized tool name for this agent. This is name based classification, not proof of the implementation behind it."
            }
            ToolCategory::Mcp => {
                "The recorded tool name contains an MCP server namespace. External does not necessarily mean authored by you."
            }
            ToolCategory::Hook => {
                "The transcript explicitly records hook activity. Configuration alone is not execution evidence."
            }
            ToolCategory::Unknown => {
                "The retained transcript does not identify a recognized tool origin. Unknown is not assumed to be custom."
            }
        }
    }
}

#[derive(Default)]
pub struct Snapshot {
    pub entries: Vec<Entry>,
    pub notices: Vec<String>,
    pub configurations: Vec<PathBuf>,
    pub configured_hooks: Vec<crate::hooks::Hook>,
}

/// Groups sessions without inventing dependencies between transcript records.
pub fn flow_order<'a>(entries: &[&'a Entry]) -> Vec<&'a Entry> {
    let mut sessions = BTreeMap::new();
    for entry in entries {
        sessions
            .entry((&entry.agent, &entry.source))
            .or_insert_with(Vec::new)
            .push(*entry);
    }
    let mut sessions: Vec<_> = sessions.into_values().collect();
    for session in &mut sessions {
        session.sort_by_key(|entry| {
            (
                chrono::DateTime::parse_from_rfc3339(
                    entry.started_at.as_deref().unwrap_or(&entry.timestamp),
                )
                .ok(),
                &entry.key,
            )
        });
    }
    sessions.sort_by(|left, right| {
        let latest = |session: &Vec<&Entry>| {
            session
                .iter()
                .filter_map(|entry| chrono::DateTime::parse_from_rfc3339(&entry.timestamp).ok())
                .max()
        };
        latest(right).cmp(&latest(left)).then_with(|| {
            left.first()
                .map(|entry| &entry.source)
                .cmp(&right.first().map(|entry| &entry.source))
        })
    });
    sessions.into_iter().flatten().collect()
}

#[derive(Clone, Debug)]
pub struct Relationship {
    pub parent: String,
    pub child: String,
    pub evidence: &'static str,
}

pub fn relationships(entries: &[Entry]) -> Vec<Relationship> {
    let mut links = Vec::new();
    for child in entries {
        let candidates: Vec<_> = entries
            .iter()
            .filter(|parent| {
                if parent.key == child.key || parent.agent != child.agent {
                    return false;
                }
                let same_session = if child.session_id.is_empty() {
                    parent.source == child.source
                } else {
                    parent.session_id == child.session_id
                };
                same_session
                    && (!child.parent_tool_id.is_empty() && parent.call_id == child.parent_tool_id
                        || child.parent_tool_id.is_empty()
                            && !child.agent_id.is_empty()
                            && parent.child_agent_id == child.agent_id
                            && matches!(parent.name.as_str(), "Agent" | "Task"))
            })
            .collect();
        if let [parent] = candidates.as_slice() {
            links.push(Relationship {
                parent: parent.key.clone(),
                child: child.key.clone(),
                evidence: if child.parent_tool_id.is_empty() {
                    "Recorded subagent ID and Agent result"
                } else {
                    "Explicit parent tool ID"
                },
            });
        }
    }
    let parents: BTreeMap<_, _> = links
        .iter()
        .map(|link| (link.child.as_str(), link.parent.as_str()))
        .collect();
    links
        .iter()
        .filter(|link| {
            let mut seen = std::collections::BTreeSet::new();
            let mut current = link.child.as_str();
            while let Some(parent) = parents.get(current) {
                if !seen.insert(current) {
                    return false;
                }
                current = parent;
            }
            true
        })
        .cloned()
        .collect()
}

pub fn relationship_order<'a>(
    entries: &[&'a Entry],
    links: &[Relationship],
) -> Vec<(&'a Entry, usize)> {
    let ordered = flow_order(entries);
    let visible: std::collections::BTreeSet<_> =
        ordered.iter().map(|entry| entry.key.as_str()).collect();
    let parents: BTreeMap<_, _> = links
        .iter()
        .filter(|link| {
            visible.contains(link.parent.as_str()) && visible.contains(link.child.as_str())
        })
        .map(|link| (link.child.as_str(), link.parent.as_str()))
        .collect();
    let mut output = Vec::new();
    let mut visited = std::collections::BTreeSet::new();
    let roots = ordered
        .iter()
        .filter(|entry| !parents.contains_key(entry.key.as_str()));
    let mut pending: Vec<_> = roots.map(|entry| (*entry, 0usize)).collect();
    pending.reverse();
    while let Some((entry, depth)) = pending.pop() {
        if !visited.insert(entry.key.as_str()) {
            continue;
        }
        output.push((entry, depth));
        for child in ordered
            .iter()
            .rev()
            .filter(|child| parents.get(child.key.as_str()) == Some(&entry.key.as_str()))
        {
            pending.push((*child, depth + 1));
        }
    }
    // Defensive fallback for malformed imported links. No record disappears.
    for entry in ordered {
        if visited.insert(entry.key.as_str()) {
            output.push((entry, 0));
        }
    }
    output
}

pub fn related_sessions<'a>(entry: &Entry, entries: &'a [Entry]) -> Vec<(&'a Entry, &'static str)> {
    let mut seen = std::collections::BTreeSet::new();
    entries
        .iter()
        .filter_map(|candidate| {
            if candidate.agent != entry.agent
                || candidate.source == entry.source
                || candidate.session_id.is_empty()
                || candidate.session_id == entry.session_id
            {
                return None;
            }
            let relation = if !entry.parent_thread_id.is_empty()
                && candidate.session_id == entry.parent_thread_id
            {
                "Parent session"
            } else if !entry.session_id.is_empty() && candidate.parent_thread_id == entry.session_id
            {
                "Child session"
            } else {
                return None;
            };
            seen.insert(candidate.session_id.as_str())
                .then_some((candidate, relation))
        })
        .collect()
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn redact(value: &Value) -> String {
    fn clean(value: &Value) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .iter()
                    .map(|(key, value)| {
                        let lower = key.to_lowercase();
                        let sensitive = [
                            "password",
                            "secret",
                            "token",
                            "authorization",
                            "api_key",
                            "apikey",
                            "credential",
                            "cookie",
                            "private_key",
                        ]
                        .iter()
                        .any(|word| lower.contains(word));
                        (
                            key.clone(),
                            if sensitive {
                                Value::String("[redacted]".into())
                            } else {
                                clean(value)
                            },
                        )
                    })
                    .collect(),
            ),
            Value::Array(array) => Value::Array(array.iter().map(clean).collect()),
            Value::String(text) => {
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    if parsed.is_object() || parsed.is_array() {
                        return clean(&parsed);
                    }
                }
                let mut text = text.clone();
                static MASKS: std::sync::LazyLock<Vec<Result<regex::Regex, regex::Error>>> =
                    std::sync::LazyLock::new(|| {
                        [
                    r"(?i)(bearer\s+)[a-z0-9._~+/=-]+",
                    r"(?i)((?:password|secret|token|api[_-]?key|authorization|credential)\s*[=:]\s*)[^\s,;]+",
                    r"\b(?:sk-[a-zA-Z0-9_-]{12,}|gh[pousr]_[a-zA-Z0-9]{16,}|AKIA[A-Z0-9]{16})\b",
                ].into_iter().map(regex::Regex::new).collect()
                    });
                for expression in MASKS.iter() {
                    match expression {
                        Ok(expression) => {
                            text = expression.replace_all(&text, "[redacted]").into_owned()
                        }
                        Err(_) => {
                            return Value::String(
                                "Payload hidden because credential masking is unavailable".into(),
                            );
                        }
                    }
                }
                Value::String(text.chars().take(8000).collect())
            }
            _ => value.clone(),
        }
    }
    let result = match clean(value) {
        Value::String(text) => text,
        value => {
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "Cannot display payload".into())
        }
    };
    if result.chars().count() > 8000 {
        format!(
            "{}\n[truncated]",
            result.chars().take(8000).collect::<String>()
        )
    } else {
        result
    }
}

pub fn parse(
    lines: &str,
    source: &Path,
    agent: &str,
    roots: &[PathBuf],
    initial_cwd: &str,
) -> Vec<Entry> {
    let roots: Vec<PathBuf> = roots
        .iter()
        .map(|root| fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect();
    let mut cwd = initial_cwd.to_string();
    let mut session_id = String::new();
    let mut parent_thread_id = String::new();
    let mut resolved_directories = BTreeMap::new();
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    for line in lines.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if string(&value, "type") == "session_meta" {
            cwd = string(&value["payload"], "cwd").into();
            session_id = string(&value["payload"], "id").into();
            parent_thread_id = string(&value["payload"], "parent_thread_id").into();
            if parent_thread_id.is_empty() {
                parent_thread_id = value
                    .pointer("/payload/source/subagent/thread_spawn/parent_thread_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into();
            }
        } else if !string(&value, "cwd").is_empty() {
            cwd = string(&value, "cwd").into();
        }
        if let Some(id) = value
            .get("sessionId")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str)
        {
            session_id = id.into();
        }
        let agent_id = value
            .get("agentId")
            .or_else(|| value.get("agent_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let resolved = resolved_directories
            .entry(cwd.clone())
            .or_insert_with(|| fs::canonicalize(&cwd).unwrap_or_else(|_| PathBuf::from(&cwd)));
        if cwd.is_empty()
            || Path::new(&cwd)
                .components()
                .any(|component| component == std::path::Component::ParentDir)
            || !roots.iter().any(|root| resolved.starts_with(root))
        {
            continue;
        }
        let timestamp = string(&value, "timestamp").to_string();
        let mut record = |id: &str,
                          name: &str,
                          input: Option<&Value>,
                          output: Option<&Value>,
                          failed: bool,
                          hook: bool| {
            if id.is_empty() {
                return;
            }
            let key = format!("{}:{id}", source.display());
            let entry = entries.entry(key.clone()).or_insert_with(|| Entry {
                key,
                agent: agent.into(),
                name: name.into(),
                status: "Observed".into(),
                timestamp: timestamp.clone(),
                duration: None,
                started_at: None,
                input: "Input not present in retained history".into(),
                output: "No completion recorded".into(),
                source: source.into(),
                cwd: cwd.clone(),
                hook,
                call_id: id.into(),
                session_id: session_id.clone(),
                agent_id: agent_id.clone(),
                parent_tool_id: String::new(),
                parent_thread_id: parent_thread_id.clone(),
                child_agent_id: String::new(),
            });
            if let Some(parent) = value.get("parent_tool_use_id").and_then(Value::as_str) {
                entry.parent_tool_id = parent.into();
            }
            if hook && entry.parent_tool_id.is_empty() {
                entry.parent_tool_id = value
                    .get("tool_use_id")
                    .or_else(|| value.pointer("/input/tool_use_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into();
            }
            if let Some(child) = value
                .pointer("/toolUseResult/agentId")
                .and_then(Value::as_str)
            {
                entry.child_agent_id = child.into();
            }
            if !name.is_empty() {
                entry.name = name.into();
            }
            if let Some(input) = input {
                entry.input = redact(input);
                entry.started_at = Some(timestamp.clone());
            }
            if let Some(output) = output {
                entry.output = redact(output);
                entry.status = if failed { "Failed" } else { "Result recorded" }.into();
                if let (Ok(start), Ok(end)) = (
                    chrono::DateTime::parse_from_rfc3339(entry.started_at.as_deref().unwrap_or("")),
                    chrono::DateTime::parse_from_rfc3339(&timestamp),
                ) {
                    let duration = end.signed_duration_since(start).num_milliseconds();
                    if duration >= 0 {
                        entry.duration = Some(duration);
                    }
                }
            }
        };
        if string(&value, "type") == "response_item" {
            let payload = &value["payload"];
            let kind = string(payload, "type");
            if matches!(kind, "function_call" | "custom_tool_call") {
                record(
                    string(payload, "call_id"),
                    string(payload, "name"),
                    payload.get("arguments").or_else(|| payload.get("input")),
                    None,
                    false,
                    false,
                );
            } else if matches!(kind, "function_call_output" | "custom_tool_call_output") {
                record(
                    string(payload, "call_id"),
                    "",
                    None,
                    payload.get("output"),
                    payload.get("is_error").and_then(Value::as_bool) == Some(true),
                    false,
                );
            }
        }
        if let Some(content) = value.pointer("/message/content").and_then(Value::as_array) {
            for item in content {
                match string(item, "type") {
                    "tool_use" => record(
                        string(item, "id"),
                        string(item, "name"),
                        item.get("input"),
                        None,
                        false,
                        false,
                    ),
                    "tool_result" => record(
                        string(item, "tool_use_id"),
                        "",
                        None,
                        item.get("content"),
                        item.get("is_error").and_then(Value::as_bool) == Some(true),
                        false,
                    ),
                    _ => {}
                }
            }
        }
        if string(&value, "subtype") == "stop_hook_summary" {
            record(
                string(&value, "uuid"),
                "Stop hook summary",
                None,
                Some(&value),
                value
                    .get("hookErrors")
                    .and_then(Value::as_array)
                    .is_some_and(|errors| !errors.is_empty()),
                true,
            );
        }
        // Only explicit hook records establish hook activity. Tool calls do not.
        if matches!(string(&value, "subtype"), "hook_started" | "hook_response") {
            record(
                string(&value, "hook_id"),
                string(&value, "hook_name"),
                value.get("input"),
                if string(&value, "subtype") == "hook_response" {
                    Some(&value)
                } else {
                    None
                },
                value
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .is_some_and(|code| code != 0),
                true,
            );
        }
    }
    entries
        .into_values()
        .map(|mut entry| {
            if entry.name.is_empty() {
                entry.name = "Tool result".into();
            }
            entry
        })
        .collect()
}

fn collect(
    directory: &Path,
    depth: usize,
    budget: &mut usize,
    files: &mut Vec<(std::time::SystemTime, PathBuf)>,
) -> std::io::Result<()> {
    if depth == 0 || *budget == 0 {
        return Ok(());
    }
    if fs::symlink_metadata(directory)?.file_type().is_symlink() {
        return Ok(());
    }
    for item in fs::read_dir(directory)? {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let item = item?;
        let kind = item.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect(&item.path(), depth - 1, budget, files)?;
        } else if kind.is_file()
            && item
                .path()
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            files.push((item.metadata()?.modified()?, item.path()));
        }
    }
    Ok(())
}

#[cfg(test)]
fn read_session(path: &Path) -> std::io::Result<(String, String, bool)> {
    read_session_scoped(path, None)
}

fn read_session_scoped(path: &Path, roots: Option<&[PathBuf]>) -> std::io::Result<(String, String, bool)> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(std::io::Error::other("Not a regular transcript"));
    }
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let mut header_bytes = Vec::new();
    (&mut file).take(65536).read_to_end(&mut header_bytes)?;
    let header = String::from_utf8_lossy(&header_bytes);
    let cwd = header
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|value| {
            let cwd = if string(&value, "type") == "session_meta" {
                string(&value["payload"], "cwd")
            } else {
                string(&value, "cwd")
            };
            (!cwd.is_empty()).then(|| cwd.to_string())
        })
        .unwrap_or_default();
    if let Some(roots) = roots {
        let resolved = fs::canonicalize(&cwd).unwrap_or_else(|_| PathBuf::from(&cwd));
        if cwd.is_empty() || !roots.iter().any(|root| resolved.starts_with(root)) {
            return Ok((String::new(), cwd, false));
        }
    }
    let offset = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
    let bytes = if offset > 0 {
        bytes.splitn(2, |byte| *byte == b'\n').nth(1).unwrap_or(&[])
    } else {
        &bytes
    };
    let mut text = String::new();
    if offset > 0 {
        for value in header
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            if string(&value, "type") == "session_meta" {
                let payload = &value["payload"];
                text.push_str(&serde_json::json!({"type":"session_meta","payload":{"id":payload.get("id"),"cwd":payload.get("cwd"),"parent_thread_id":payload.get("parent_thread_id"),"source":payload.get("source")}}).to_string());
                text.push('\n');
                break;
            }
        }
    }
    text.push_str(&String::from_utf8_lossy(bytes));
    Ok((text, cwd, offset > 0))
}

fn retain_sessions(entries: &mut Vec<Entry>, limit: usize) {
    let mut counts = BTreeMap::new();
    let mut ranked: Vec<_> = entries
        .drain(..)
        .enumerate()
        .map(|(index, entry)| {
            let count = counts.entry(entry.session_key()).or_insert(0);
            let rank = *count;
            *count += 1;
            (rank, index, entry)
        })
        .collect();
    ranked.sort_by_key(|(rank, index, _)| (*rank, *index));
    ranked.truncate(limit);
    ranked.sort_by_key(|(_, index, _)| *index);
    entries.extend(ranked.into_iter().map(|(_, _, entry)| entry));
}

pub fn load(roots: &[PathBuf], homes: &[(String, PathBuf)]) -> Snapshot {
    let mut snapshot = Snapshot::default();
    let roots: Vec<_> = roots.iter().map(|root| fs::canonicalize(root).unwrap_or_else(|_| root.clone())).collect();
    for (agent, directory) in homes {
        let mut files = Vec::new();
        let mut budget = 12000;
        if let Err(error) = collect(directory, 6, &mut budget, &mut files) {
            if error.kind() != std::io::ErrorKind::NotFound {
                snapshot
                    .notices
                    .push(format!("Cannot read {agent} session history"));
            }
            continue;
        }
        files.sort_by(|left, right| right.0.cmp(&left.0));
        if files.len() > FILE_LIMIT || budget == 0 {
            snapshot
                .notices
                .push(format!("{agent}: showing recent sessions only"));
        }
        let mut retained_sessions = 0;
        for (_, path) in files {
            if retained_sessions >= FILE_LIMIT { break; }
            match read_session_scoped(&path, Some(&roots)) {
                Ok((text, cwd, truncated)) => {
                    let entries = parse(&text, &path, agent, &roots, &cwd);
                    if !entries.is_empty() { retained_sessions += 1; }
                    if truncated && !entries.is_empty() {
                        snapshot.notices.push(format!(
                            "{agent}: older events omitted from a large session"
                        ));
                    }
                    snapshot.entries.extend(entries);
                }
                Err(_) => snapshot
                    .notices
                    .push(format!("{agent}: a transcript could not be read")),
            }
        }
    }
    snapshot.entries.sort_by(|left, right| {
        right
            .timestamp
            .cmp(&left.timestamp)
            .then(left.key.cmp(&right.key))
    });
    if snapshot.entries.len() > 500 {
        snapshot
            .notices
            .push("Showing up to 500 records shared across retained sessions".into());
        retain_sessions(&mut snapshot.entries, 500);
    }
    let mut configurations = crate::hooks::configuration_paths();
    let canonical_roots: Vec<_> = roots
        .iter()
        .map(|root| fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect();
    for root in &canonical_roots {
        let directories = std::iter::once(root.as_path()).chain(
            snapshot
                .entries
                .iter()
                .flat_map(|entry| Path::new(&entry.cwd).ancestors())
                .filter(|directory| directory.starts_with(root)),
        );
        for directory in directories {
            for relative in [
                ".mcp.json",
                ".claude/settings.json",
                ".claude/settings.local.json",
                ".codex/hooks.json",
                ".codex/config.toml",
            ] {
                configurations.push(directory.join(relative));
            }
        }
    }
    configurations.sort();
    configurations.dedup();
    snapshot.configurations = configurations
        .into_iter()
        .filter(|path| fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()))
        .collect();
    for path in &snapshot.configurations {
        let agent = if path
            .file_name()
            .is_some_and(|name| name == "config.toml" || name == "hooks.json")
        {
            "Codex"
        } else {
            "Claude"
        };
        match fs::File::open(path).and_then(|file| {
            let mut text = String::new();
            file.take(TAIL_BYTES + 1).read_to_string(&mut text)?;
            if text.len() as u64 > TAIL_BYTES {
                return Err(std::io::Error::other("Configuration too large"));
            }
            Ok(text)
        }) {
            Ok(text) => match crate::hooks::parse(&text, &path.to_string_lossy(), agent) {
                Ok(hooks) => snapshot.configured_hooks.extend(hooks),
                Err(_) => snapshot
                    .notices
                    .push(format!("Cannot parse {agent} hook configuration")),
            },
            Err(_) => snapshot
                .notices
                .push(format!("Cannot read {agent} hook configuration")),
        }
    }
    snapshot.notices.sort();
    snapshot.notices.dedup();
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_hook_summary_is_evidence_and_sessions_do_not_collide() {
        let line = r#"{"cwd":"/work","type":"system","subtype":"stop_hook_summary","uuid":"a","hookInfos":[{"command":"true"}],"hookErrors":[]}"#;
        let first = parse(line, Path::new("first"), "Claude", &["/work".into()], "");
        let second = parse(line, Path::new("second"), "Claude", &["/work".into()], "");
        assert!(first[0].hook);
        assert_ne!(first[0].key, second[0].key);
        assert_eq!(first[0].status, "Result recorded");
    }

    #[test]
    fn reader_does_not_follow_transcript_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, "private").unwrap();
        let link = directory.path().join("session.jsonl");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(read_session(&link).is_err());
        }
    }

    #[test]
    fn pairs_codex_calls_and_scopes_by_path_components() {
        let text = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/work/app\"}}\n",
            "{\"timestamp\":\"2026-01-01T00:00:00Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"call_id\":\"1\",\"name\":\"exec\",\"arguments\":\"{}\"}}\n",
            "{\"timestamp\":\"2026-01-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"1\",\"output\":\"done\"}}\n"
        );
        let entries = parse(
            text,
            Path::new("session"),
            "Codex",
            &["/work/app".into()],
            "",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].duration, Some(1000));
        assert_eq!(entries[0].status, "Result recorded");
        assert!(
            parse(
                text,
                Path::new("session"),
                "Codex",
                &["/work/ap".into()],
                ""
            )
            .is_empty()
        );
    }
    #[test]
    fn claude_failure_and_partial_record() {
        let text = "{\"cwd\":\"/work\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Bash\",\"input\":{}}]}}\n{\"cwd\":\"/work\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"a\",\"is_error\":true,\"content\":\"failed\"}]}}\n{partial";
        let entries = parse(text, Path::new("s"), "Claude", &["/work".into()], "");
        assert_eq!(entries[0].status, "Failed");
        assert!(!entries[0].hook);
    }
    #[test]
    fn redacts_nested_and_embedded_secrets() {
        let result = redact(
            &serde_json::json!({"token":"hidden", "input":"{\"password\":\"hidden\"}", "command":"TOKEN=hidden curl"}),
        );
        assert!(!result.contains("hidden"));
    }
    #[test]
    #[cfg(unix)]
    fn matches_workspace_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let line = serde_json::json!({"cwd":alias,"message":{"content":[{"type":"tool_use","id":"1","name":"Read","input":{}}]}}).to_string();
        assert_eq!(parse(&line, Path::new("s"), "Claude", &[real], "").len(), 1);
    }

    #[test]
    fn orphan_results_have_no_invented_duration() {
        let entries = parse(
            r#"{"cwd":"/work","timestamp":"2026-01-01T00:00:00Z","message":{"content":[{"type":"tool_result","tool_use_id":"orphan","content":"done"}]}}"#,
            Path::new("s"),
            "Claude",
            &["/work".into()],
            "",
        );
        assert_eq!(entries[0].duration, None);
    }

    #[test]
    fn does_not_invent_completion() {
        let entries = parse(
            "{\"cwd\":\"/work\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"a\",\"name\":\"Bash\",\"input\":{}}]}}",
            Path::new("s"),
            "Claude",
            &["/work".into()],
            "",
        );
        assert_eq!(entries[0].status, "Observed");
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;

    fn entry(key: &str, source: &str, timestamp: &str, started: Option<&str>) -> Entry {
        Entry {
            key: key.into(),
            agent: "Codex".into(),
            name: "tool".into(),
            status: "Observed".into(),
            timestamp: timestamp.into(),
            duration: None,
            started_at: started.map(str::to_string),
            input: String::new(),
            output: String::new(),
            source: source.into(),
            cwd: "/project".into(),
            hook: false,
            call_id: key.into(),
            session_id: String::new(),
            agent_id: String::new(),
            parent_tool_id: String::new(),
            parent_thread_id: String::new(),
            child_agent_id: String::new(),
        }
    }

    #[test]
    fn retained_history_keeps_quiet_sessions_and_original_order() {
        let mut entries = vec![
            entry("busy1", "busy", "4", None),
            entry("busy2", "busy", "3", None),
            entry("busy3", "busy", "2", None),
            entry("quiet", "quiet", "1", None),
        ];
        retain_sessions(&mut entries, 3);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            vec!["busy1", "busy2", "quiet"]
        );
    }

    #[test]
    fn session_identity_groups_subagents_but_separates_providers() {
        let mut root = entry("root", "root-file", "1", None);
        let mut child = entry("child", "child-file", "2", None);
        root.session_id = "same".into();
        child.session_id = "same".into();
        assert_eq!(root.session_key(), child.session_key());
        child.agent = "Claude".into();
        assert_ne!(root.session_key(), child.session_key());
    }

    #[test]
    fn subagent_identity_does_not_leak_to_root_messages() {
        let lines = concat!(
            "{\"cwd\":\"/work\",\"sessionId\":\"s\",\"agentId\":\"child\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"1\",\"name\":\"Read\",\"input\":{}}]}}\n",
            "{\"cwd\":\"/work\",\"sessionId\":\"s\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"2\",\"name\":\"Read\",\"input\":{}}]}}"
        );
        let calls = parse(lines, Path::new("session"), "Claude", &["/work".into()], "");
        assert_eq!(calls[0].agent_id, "child");
        assert!(calls[1].agent_id.is_empty());
    }

    #[test]
    fn explicit_hook_tool_id_links_to_call() {
        let lines = concat!(
            "{\"cwd\":\"/work\",\"sessionId\":\"s\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"Read\",\"input\":{}}]}}\n",
            "{\"cwd\":\"/work\",\"sessionId\":\"s\",\"subtype\":\"hook_started\",\"hook_id\":\"hook\",\"hook_name\":\"check\",\"input\":{\"tool_use_id\":\"call\"}}"
        );
        let calls = parse(lines, Path::new("session"), "Claude", &["/work".into()], "");
        assert_eq!(relationships(&calls).len(), 1);
    }

    #[test]
    fn explicit_relationships_are_scoped_clickable_and_cycle_safe() {
        let mut parent = entry("parent", "session-a", "", None);
        parent.session_id = "session-a".into();
        let mut child = entry("child", "child-file", "", None);
        child.session_id = "session-a".into();
        child.parent_tool_id = "parent".into();
        let mut unrelated = entry("duplicate", "session-b", "", None);
        unrelated.call_id = "parent".into();
        unrelated.session_id = "session-b".into();
        let entries = vec![child.clone(), unrelated, parent.clone()];
        let links = relationships(&entries);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].parent, "parent");
        let ordered = relationship_order(&entries.iter().collect::<Vec<_>>(), &links);
        let child_index = ordered
            .iter()
            .position(|(entry, _)| entry.key == "child")
            .unwrap();
        assert_eq!(ordered[child_index].1, 1);
        assert_eq!(ordered[child_index - 1].0.key, "parent");
        parent.parent_tool_id = "child".into();
        assert!(relationships(&[parent, child]).is_empty());
    }

    #[test]
    fn claude_agent_results_link_only_same_session_children() {
        let mut parent = entry("parent", "session", "", None);
        parent.agent = "Claude".into();
        parent.name = "Agent".into();
        parent.session_id = "s".into();
        parent.child_agent_id = "agent-1".into();
        let mut child = entry("child", "subagent", "", None);
        child.agent = "Claude".into();
        child.session_id = "s".into();
        child.agent_id = "agent-1".into();
        assert_eq!(relationships(&[parent.clone(), child.clone()]).len(), 1);
        child.session_id = "other".into();
        assert!(relationships(&[parent.clone(), child.clone()]).is_empty());
        child.session_id = "s".into();
        let mut duplicate = parent.clone();
        duplicate.key = "duplicate".into();
        assert!(relationships(&[parent, duplicate, child]).is_empty());
    }

    #[test]
    fn reads_parent_metadata_without_using_message_predecessor_as_causality() {
        let lines = r#"{"cwd":"/work","sessionId":"s","parentUuid":"not-a-tool","parent_tool_use_id":"parent","agentId":"agent-1","message":{"content":[{"type":"tool_use","id":"child","name":"Read","input":{}}]}}"#;
        let calls = parse(lines, Path::new("session"), "Claude", &["/work".into()], "");
        assert_eq!(calls[0].parent_tool_id, "parent");
        assert_eq!(calls[0].session_id, "s");
        assert_eq!(calls[0].agent_id, "agent-1");
        assert!(relationships(&calls).is_empty());
        let lines = lines.replace(",\"parent_tool_use_id\":\"parent\"", "");
        assert!(
            parse(
                &lines,
                Path::new("session"),
                "Claude",
                &["/work".into()],
                ""
            )[0]
            .parent_tool_id
            .is_empty()
        );
    }

    #[test]
    fn codex_thread_relationship_is_not_a_call_dependency() {
        let mut parent = entry("parent", "parent-file", "", None);
        parent.session_id = "thread-p".into();
        let mut child = entry("child", "child-file", "", None);
        child.session_id = "thread-c".into();
        child.parent_thread_id = "thread-p".into();
        let entries = vec![parent.clone(), child.clone()];
        assert!(relationships(&entries).is_empty());
        assert_eq!(related_sessions(&child, &entries)[0].1, "Parent session");
        assert_eq!(related_sessions(&parent, &entries)[0].1, "Child session");
    }

    #[test]
    fn categories_are_conservative_and_agent_specific() {
        let mut call = entry("call", "session", "", None);
        call.agent = "Claude".into();
        call.name = "Read".into();
        assert_eq!(call.category(), ToolCategory::BuiltIn);
        assert!(!call.matches_category("Custom"));
        call.name = "Read sample 1".into();
        assert_eq!(call.category(), ToolCategory::Unknown);
        call.name = "mcp__example__search".into();
        assert_eq!(call.category(), ToolCategory::Mcp);
        assert_eq!(call.mcp_server(), Some("example"));
        assert!(call.matches_category("Custom"));
        assert!(call.matches_category("MCP tools"));
        call.name = "mcp__example__".into();
        assert_eq!(call.category(), ToolCategory::Unknown);
        call.hook = true;
        assert_eq!(call.category(), ToolCategory::Hook);
        assert!(call.matches_category("Custom"));
        call.hook = false;
        call.agent = "Codex".into();
        call.name = "functions.exec_command".into();
        assert_eq!(call.category(), ToolCategory::BuiltIn);
        call.name = "Read".into();
        assert_eq!(call.category(), ToolCategory::Unknown);
        call.name = "functions.mcp__example__search".into();
        assert_eq!(call.category(), ToolCategory::Mcp);
        assert!(call.matches_category("All tools"));
    }

    #[test]
    fn output_pairing_does_not_erase_category() {
        let lines = concat!(
            "{\"cwd\":\"/work\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"1\",\"name\":\"mcp__example__search\",\"input\":{}}]}}\n",
            "{\"cwd\":\"/work\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"1\",\"content\":\"ok\"}]}}"
        );
        let calls = parse(lines, Path::new("session"), "Claude", &["/work".into()], "");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].category(), ToolCategory::Mcp);
        assert_eq!(calls[0].status, "Result recorded");
    }

    #[test]
    fn flow_keeps_sessions_separate_and_orders_calls_by_start() {
        let first = entry(
            "first",
            "session-a",
            "2026-10-23T10:05:00Z",
            Some("2026-10-23T10:00:00Z"),
        );
        let second = entry("second", "session-a", "2026-10-23T10:01:00Z", None);
        let other = entry("other", "session-b", "2026-10-23T10:03:00Z", None);
        let ordered = flow_order(&[&other, &second, &first]);
        assert_eq!(
            ordered
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second", "other"]
        );
    }

    #[test]
    fn flow_handles_empty_missing_and_equal_timestamps_deterministically() {
        assert!(flow_order(&[]).is_empty());
        let first = entry("a", "session", "", None);
        let second = entry("b", "session", "", None);
        assert_eq!(
            flow_order(&[&second, &first])
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }
}

#[cfg(test)] mod review_regressions {
use super::*;
use super as activity_data;
#[test] fn unrelated_recent_sessions_hide_current_project() {
 let dir=tempfile::tempdir().unwrap();
 let root=dir.path().join("project"); std::fs::create_dir(&root).unwrap();
 let sessions=dir.path().join("sessions"); std::fs::create_dir(&sessions).unwrap();
 let fixture=|cwd: &std::path::Path, id: &str| format!("{}\n{}\n",serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}}),serde_json::json!({"type":"response_item","timestamp":"2026-10-24T00:00:00Z","payload":{"type":"function_call","call_id":id,"name":"exec_command","arguments":"{}"}}));
 let target=sessions.join("target.jsonl"); std::fs::write(&target,fixture(&root,"target")).unwrap();
 std::fs::File::open(&target).unwrap().set_modified(std::time::UNIX_EPOCH+std::time::Duration::from_secs(1)).unwrap();
 let homes=vec![("Codex".into(),sessions.clone())];
 assert_eq!(activity_data::load(&[root.clone()],&homes).entries.len(),1);
 for i in 0..24 { std::fs::write(sessions.join(format!("other{i}.jsonl")),fixture(&dir.path().join("unrelated"),&format!("other{i}"))).unwrap(); }
 assert_eq!(activity_data::load(&[root],&homes).entries.len(), 1);
}
}
