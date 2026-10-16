# Hulder and the Viking factory

Hulder is the editing and terminal surface. Heimdall is the operator console; Mimir controls execution, Sindri owns plans and delivery, and Brokkr records verification. No editor model subscription or GitHub account is required by this integration.

Use Hulder's task picker to run **Factory: open Heimdall**, **Factory: check services**, or **Factory: start**. The first opens the loopback dashboard in your normal browser. Sign in there; no tokens are stored in editor tasks. The status task checks reachability, not authentication or agent readiness.

For the start task, set `FACTORY_LAUNCHER` to the absolute path of your `factory.mjs` in the environment used to launch Hulder. Alternatively run `FACTORY_LAUNCHER=/your/path/factory.mjs ./script/factory start` in Hulder's terminal. Stop the factory with Control-C in that terminal.

To inspect an agent's work, open the task's worktree path shown in Heimdall with Hulder. Review changed files there. When using Mimir's Herdr backend, Hulder's existing Herd view can attach to those Herdr sessions. CLI, SSH and tmux tasks are monitored through Heimdall; they are not represented as native Herd sessions.

The existing Bifröst MCP bridge exposes editor context and Saga notes to agents. This addition does not give the bridge factory control permissions, forward credentials, or change Hulder's HTTP policy. Opening the dashboard is an explicit OS browser action.

## Native Factory view

Choose **View → Factory** to open the native GPUI inspector. Select **Choose credentials** and choose the running Mimir controller's `auth-token` file. Credentials are read locally, never copied into project settings. The default endpoint is `127.0.0.1:7700`; launch with `HULDER_FACTORY_PORT` to use a different local port. `HULDER_FACTORY_TOKEN_FILE` optionally supplies the credential path at launch.

The view refreshes every five seconds. Click a task to inspect its brief, acceptance criteria, sessions, approvals and artifact metadata. **Inspect source record** reveals the complete task-detail JSON, including dependencies. **Open task worktree** opens its existing local directory in Hulder. **Open Heimdall** opens the local operator console for approval, stop and retry controls. The native inspector and MCP tools are read-only.

Disconnect clears displayed source data. Switching credentials clears prior controller data before refreshing. An unavailable controller produces an explicit error while retaining the last successful observation; polling recovers when it returns. The dedicated client permits only loopback snapshot/task reads, has a five-second request deadline and a 2 MiB response cap, and does not change the editor's blocked global HTTP client.

Use Mimir with authenticated `GET /v1/tasks/:id` support, including its `spec.brief` field. Existing launch-only binaries must be rebuilt to include the native view. For accessibility-assisted interaction, start Hulder with its existing `ZED_EXPERIMENTAL_A11Y=1` option.

Validation: the native build and live localhost task inspection, credential-picker reconnection, controller outage recovery and opening a real disposable Git worktree were exercised on macOS. This does not establish remote-host or additional vendor-agent acceptance.

## Factory context in the editor

`script/factory fleet` opens the fleet and supervisor view.
`script/factory-mcp.mjs` is a read-only MCP context server for local Mimir
health, runtime capabilities and individual task inspection. Merge
`factory-context-server.example.json` into your editor context-server settings,
using an absolute script path if the editor launches outside this checkout.
Point `MIMIR_TOKEN_FILE` at the local controller token file; do not commit a token.
The bridge has no mutation tools and rejects remote origins and path traversal.
It is an additional local context server, not a change to Bifröst or Herd.

The `site/` directory and manually triggered Project Pages workflow prepare
the Hulder documentation site for its final public repository.
