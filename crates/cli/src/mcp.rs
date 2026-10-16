//! `hulder mcp`: a stdio MCP server that relays every tool call to the Bifröst
//! socket served by the running Hulder (`crates/bifrost`). Newline-delimited
//! JSON-RPC 2.0 on stdin/stdout, which is what Claude Code and friends expect
//! from a stdio server. No network: the only I/O besides stdio is the local
//! Unix socket.

use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub const PROTOCOL_VERSION: &str = "2024-11-05";
pub const NOT_RUNNING: &str = "Hulder is not running";
pub const SOCKET_FILE: &str = "hulder.sock";

/// Bounds a single relayed call so a wedged editor cannot hang the agent.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

pub fn default_socket_path() -> PathBuf {
    paths::config_dir().join(SOCKET_FILE)
}

/// One tool definition for `tools/list`.
struct Tool {
    name: &'static str,
    description: &'static str,
    schema: Value,
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn catalog() -> Vec<Tool> {
    let no_args = || object_schema(json!({}), &[]);
    vec![
        Tool {
            name: "project_root",
            description: "Absolute path of the project folder open in Hulder.",
            schema: no_args(),
        },
        Tool {
            name: "editor_current_file",
            description: "Path, language and cursor position of the file in the active editor.",
            schema: no_args(),
        },
        Tool {
            name: "editor_selection",
            description: "Selected text in the active editor with its path and line/column range.",
            schema: no_args(),
        },
        Tool {
            name: "editor_open",
            description: "Open a file in Hulder, optionally jumping to a 1-based line. The path must be inside the project.",
            schema: object_schema(
                json!({
                    "path": { "type": "string", "description": "File path, absolute or relative to the project root." },
                    "line": { "type": "integer", "minimum": 1, "description": "1-based line to place the cursor on." }
                }),
                &["path"],
            ),
        },
        Tool {
            name: "editor_diagnostics",
            description: "Language server diagnostics: per-file error and warning counts, or every diagnostic in one file when a path is given.",
            schema: object_schema(
                json!({
                    "path": { "type": "string", "description": "File path inside the project. Omit for a per-file summary." }
                }),
                &[],
            ),
        },
        Tool {
            name: "editor_open_files",
            description: "Absolute paths of every file open in the Hulder window.",
            schema: no_args(),
        },
        Tool {
            name: "saga_list_tales",
            description: "List tickets (tales) in .hulder/tickets, optionally filtered by status.",
            schema: object_schema(
                json!({
                    "status": { "type": "string", "enum": ["open", "doing", "blocked", "done"] }
                }),
                &[],
            ),
        },
        Tool {
            name: "saga_get_tale",
            description: "Read one ticket (tale) with its front matter and body.",
            schema: object_schema(
                json!({ "id": { "type": "string", "description": "Tale id such as 0001 or 1." } }),
                &["id"],
            ),
        },
        Tool {
            name: "saga_create_tale",
            description: "Create a ticket (tale) file in .hulder/tickets with a generated id and slug.",
            schema: object_schema(
                json!({
                    "title": { "type": "string" },
                    "body": { "type": "string", "description": "Markdown body." },
                    "priority": { "type": "string", "enum": ["low", "normal", "high"] },
                    "tags": { "type": "array", "items": { "type": "string" } }
                }),
                &["title"],
            ),
        },
        Tool {
            name: "saga_set_status",
            description: "Set a ticket's status.",
            schema: object_schema(
                json!({
                    "id": { "type": "string" },
                    "status": { "type": "string", "enum": ["open", "doing", "blocked", "done"] }
                }),
                &["id", "status"],
            ),
        },
        Tool {
            name: "saga_append_chronicle",
            description: "Append a dated line to a ticket's '## Chronicle' section.",
            schema: object_schema(
                json!({ "id": { "type": "string" }, "text": { "type": "string" } }),
                &["id", "text"],
            ),
        },
        Tool {
            name: "saga_list_verses",
            description: "List the checkbox todos (verses) in .hulder/TODO.md with their line numbers.",
            schema: no_args(),
        },
        Tool {
            name: "saga_toggle_verse",
            description: "Toggle the checkbox on the given 1-based line of .hulder/TODO.md.",
            schema: object_schema(
                json!({ "line": { "type": "integer", "minimum": 1 } }),
                &["line"],
            ),
        },
        Tool {
            name: "saga_add_verse",
            description: "Append an unchecked todo (verse) to .hulder/TODO.md.",
            schema: object_schema(json!({ "text": { "type": "string" } }), &["text"]),
        },
        Tool {
            name: "saga_list_lore",
            description: "List note (lore) files: .hulder/NOTES.md and .hulder/notes/*.md.",
            schema: no_args(),
        },
        Tool {
            name: "saga_read_lore",
            description: "Read one note (lore) file by name, e.g. NOTES.md or notes/2026-10-13.md.",
            schema: object_schema(json!({ "name": { "type": "string" } }), &["name"]),
        },
        Tool {
            name: "saga_search",
            description: "Case-insensitive search across every Markdown file under .hulder.",
            schema: object_schema(json!({ "query": { "type": "string" } }), &["query"]),
        },
        Tool {
            name: "saga_list_clippings",
            description: "List clipping files (web picks, Lightpanda fetches, PDF text) under .hulder/clippings/, newest first.",
            schema: no_args(),
        },
        Tool {
            name: "saga_read_clipping",
            description: "Read one clipping by file name, e.g. 2026-10-13-gpui-docs.md.",
            schema: object_schema(json!({ "name": { "type": "string" } }), &["name"]),
        },
        Tool {
            name: "saga_glosses",
            description: "Glosses (notes) attached to one project file, with line ranges and anchor text.",
            schema: object_schema(
                json!({
                    "path": { "type": "string", "description": "File path inside the project, absolute or relative to the project root." }
                }),
                &["path"],
            ),
        },
        Tool {
            name: "saga_add_gloss",
            description: "Leave a gloss (a Markdown note) on a project file, at a line range or on the whole file. It is shown with the author's name.",
            schema: object_schema(
                json!({
                    "path": { "type": "string", "description": "File path inside the project." },
                    "lines": { "type": "string", "description": "1-based line range such as \"42-48\" or \"42\". Omit for a file-level gloss." },
                    "body": { "type": "string", "description": "Markdown body of the note." },
                    "author": { "type": "string", "description": "Who leaves the note; defaults to \"agent\"." }
                }),
                &["path", "body"],
            ),
        },
    ]
}

pub fn tool_names() -> Vec<&'static str> {
    catalog().into_iter().map(|tool| tool.name).collect()
}

pub fn tools() -> Vec<Value> {
    catalog()
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.schema,
            })
        })
        .collect()
}

/// How a tool call reaches the editor. The real one dials the Bifröst socket;
/// tests substitute a fake.
pub trait Transport {
    fn call(&mut self, method: &str, params: Value) -> std::result::Result<Value, String>;
}

pub struct SocketTransport {
    path: PathBuf,
    next_id: u64,
    /// Sent with every request so Hulder answers for the window whose
    /// project contains the agent, not whichever window is in front.
    cwd: Option<PathBuf>,
}

impl SocketTransport {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            next_id: 1,
            cwd: std::env::current_dir().ok(),
        }
    }
}

impl Transport for SocketTransport {
    fn call(&mut self, method: &str, params: Value) -> std::result::Result<Value, String> {
        let mut stream = UnixStream::connect(&self.path).map_err(|_| NOT_RUNNING.to_string())?;
        stream
            .set_read_timeout(Some(CALL_TIMEOUT))
            .map_err(|error| format!("socket: {error}"))?;
        stream
            .set_write_timeout(Some(CALL_TIMEOUT))
            .map_err(|error| format!("socket: {error}"))?;
        let id = self.next_id.to_string();
        self.next_id += 1;
        let request = json!({ "id": id, "method": method, "params": params, "cwd": self.cwd });
        let mut line = request.to_string();
        line.push('\n');
        stream
            .write_all(line.as_bytes())
            .map_err(|error| format!("writing to Hulder: {error}"))?;
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        reader
            .read_line(&mut reply)
            .map_err(|error| format!("reading from Hulder: {error}"))?;
        if reply.trim().is_empty() {
            return Err("Hulder closed the connection before replying".to_string());
        }
        let response: Value =
            serde_json::from_str(&reply).map_err(|error| format!("unparseable reply: {error}"))?;
        if let Some(error) = response.get("error") {
            let code = error["code"].as_str().unwrap_or("error");
            let message = error["message"].as_str().unwrap_or("");
            return Err(format!("{message} ({code})"));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "empty reply from Hulder".to_string())
    }
}

pub struct Server<T: Transport> {
    transport: T,
}

fn rpc_result(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn rpc_error(id: Value, code: i64, message: String) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

impl<T: Transport> Server<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Answers one JSON-RPC line. Notifications (no `id`) get `None`.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(error) => {
                return Some(rpc_error(
                    Value::Null,
                    -32700,
                    format!("parse error: {error}"),
                ));
            }
        };
        let id = message.get("id").cloned();
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to something we sent, or garbage; either way nothing to answer.
            return id.map(|id| rpc_error(id, -32600, "missing method".to_string()));
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            // Notifications such as notifications/initialized or notifications/cancelled.
            return None;
        };
        Some(match method {
            "initialize" => rpc_result(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "hulder", "version": env!("CARGO_PKG_VERSION") },
                }),
            ),
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({ "tools": tools() })),
            "tools/call" => rpc_result(id, self.call_tool(&params)),
            other => rpc_error(id, -32601, format!("method not found: {other}")),
        })
    }

    fn call_tool(&mut self, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return tool_result("tools/call needs a tool name".to_string(), true);
        };
        if !tool_names().contains(&name) {
            return tool_result(format!("unknown tool {name:?}"), true);
        }
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(arguments @ Value::Object(_)) => arguments.clone(),
            Some(_) => return tool_result("arguments must be an object".to_string(), true),
        };
        match self.transport.call(name, arguments) {
            Ok(result) => {
                let text =
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
                tool_result(text, false)
            }
            Err(message) => tool_result(message, true),
        }
    }
}

/// Serves stdin until it closes. Each reply is one line followed by a flush, so
/// the agent never waits on a buffered answer.
pub fn run(socket_path: PathBuf) -> Result<()> {
    let mut server = Server::new(SocketTransport::new(socket_path));
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).context("reading stdin")?;
        if read == 0 {
            return Ok(());
        }
        if let Some(reply) = server.handle_line(&line) {
            stdout
                .write_all(reply.as_bytes())
                .context("writing stdout")?;
            stdout.write_all(b"\n").context("writing stdout")?;
            stdout.flush().context("flushing stdout")?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// Mirrors the Bifröst server: one reply line per request line, any number per connection.
    fn fake_socket(dir: &std::path::Path) -> PathBuf {
        let path = dir.join(SOCKET_FILE);
        let listener = UnixListener::bind(&path).expect("bind");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().expect("clone");
                    let reader = BufReader::new(stream);
                    for line in reader.lines() {
                        let Ok(line) = line else { break };
                        let request: Value = serde_json::from_str(&line).expect("json");
                        let id = request["id"].as_str().expect("id");
                        let expected_cwd = std::env::current_dir().expect("cwd");
                        assert_eq!(
                            request["cwd"].as_str().map(PathBuf::from),
                            Some(expected_cwd),
                            "every relayed request names the relay's cwd"
                        );
                        let reply = match request["method"].as_str().expect("method") {
                            "project_root" => json!({ "id": id, "result": { "root": "/p" } }),
                            "editor_open" => {
                                json!({ "id": id, "error": { "code": "forbidden_path", "message": "nope" } })
                            }
                            other => {
                                json!({ "id": id, "error": { "code": "unknown_method", "message": other } })
                            }
                        };
                        writer
                            .write_all(format!("{reply}\n").as_bytes())
                            .expect("write");
                    }
                });
            }
        });
        path
    }

    fn reply_json(server: &mut Server<SocketTransport>, line: &str) -> Value {
        let reply = server.handle_line(line).expect("a reply");
        serde_json::from_str(&reply).expect("reply is json")
    }

    #[test]
    fn golden_transcript_against_fake_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = fake_socket(dir.path());
        let mut server = Server::new(SocketTransport::new(path));

        let init = reply_json(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"claude-code","version":"1.0"}}}"#,
        );
        assert_eq!(init["jsonrpc"], "2.0");
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(init["result"]["capabilities"]["tools"].is_object());
        assert_eq!(init["result"]["serverInfo"]["name"], "hulder");

        assert_eq!(
            server.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            None
        );

        let list = reply_json(
            &mut server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        );
        let listed = list["result"]["tools"].as_array().expect("tools array");
        assert_eq!(listed.len(), 21);
        for tool in listed {
            assert!(tool["name"].is_string());
            assert!(tool["description"].is_string());
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        let names: Vec<&str> = listed
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        for expected in [
            "project_root",
            "editor_current_file",
            "editor_selection",
            "editor_open",
            "editor_diagnostics",
            "editor_open_files",
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
            "saga_glosses",
            "saga_add_gloss",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} missing from tools/list"
            );
        }
        let open = listed
            .iter()
            .find(|tool| tool["name"] == "editor_open")
            .expect("editor_open");
        assert_eq!(open["inputSchema"]["required"], json!(["path"]));

        let call = reply_json(
            &mut server,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"project_root","arguments":{}}}"#,
        );
        assert_eq!(call["result"]["isError"], false);
        assert_eq!(call["result"]["content"][0]["type"], "text");
        let text = call["result"]["content"][0]["text"].as_str().expect("text");
        let parsed: Value = serde_json::from_str(text).expect("text is json");
        assert_eq!(parsed["root"], "/p");

        let failed = reply_json(
            &mut server,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"editor_open","arguments":{"path":"../x"}}}"#,
        );
        assert_eq!(failed["result"]["isError"], true);
        assert_eq!(
            failed["result"]["content"][0]["text"],
            "nope (forbidden_path)"
        );

        let unknown_tool = reply_json(
            &mut server,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"run_command"}}"#,
        );
        assert_eq!(unknown_tool["result"]["isError"], true);

        let ping = reply_json(&mut server, r#"{"jsonrpc":"2.0","id":6,"method":"ping"}"#);
        assert_eq!(ping["result"], json!({}));

        let missing = reply_json(
            &mut server,
            r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#,
        );
        assert_eq!(missing["error"]["code"], -32601);

        let garbage = reply_json(&mut server, "{not json");
        assert_eq!(garbage["error"]["code"], -32700);
        assert!(garbage["id"].is_null());
    }

    #[test]
    fn without_hulder_every_tool_call_is_an_error_result() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut server = Server::new(SocketTransport::new(dir.path().join("absent.sock")));
        let init = reply_json(
            &mut server,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        );
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);
        let list = reply_json(
            &mut server,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        );
        assert_eq!(list["result"]["tools"].as_array().map(Vec::len), Some(21));
        let call = reply_json(
            &mut server,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"saga_list_tales","arguments":{}}}"#,
        );
        assert_eq!(call["result"]["isError"], true);
        assert_eq!(call["result"]["content"][0]["text"], NOT_RUNNING);
    }
}
