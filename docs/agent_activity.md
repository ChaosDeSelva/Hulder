# Hooks and agent activity

Kit includes a Hooks tab for Claude and Codex configuration. Select a hook to open its source in the editor. The list shows its agent, event, matcher, handler type, source path and configured or disabled state. Common credential patterns in descriptions are masked. Review the source file to see its complete contents.

Project discovery includes nested projects and excludes generated dependency folders. Claude settings.json and settings.local.json are supported. Codex hooks.json, inline config.toml hooks and legacy notify configuration are supported. User configuration respects CLAUDE_CONFIG_DIR and CODEX_HOME. Selecting Hooks refreshes user configuration as well as project configuration.

A configured entry does not establish that the agent trusts, enables or executes it. Plugin manifests, managed policy and indirect plugin hook files are not resolved by this view. Consult the agent for its effective configuration.

## Inspect activity

Select the wrench in the bottom right status bar. The Agent activity tab reads local Claude and Codex transcripts for the open workspace and its subprojects. It does not execute hooks, inject configuration, send data to a server or modify session history.

Use the Claude, Codex or Hooks filters to narrow the list. Select an entry for its recorded timestamp, project, source, input, output and recorded interval when both timestamps are available. Configuration source buttons open files in the editor. These are navigation links, not claims that a particular configuration caused the selected call.

The Flow view groups clickable tool and hook records by transcript session. Select any node to inspect its details on the right. Arrows show recorded order within that session, not causality or dependencies. Paired calls are ordered by their start timestamp; missing timestamps cannot establish order. Session headers repeat when a flow continues onto another page. Filters show only matching records, so the displayed sequence may omit other activity. Switch to List for newest records first. The detail pane scrolls independently; expanded input and output each have a bounded scroll area. Plain text output preserves recorded line breaks.

Updates run every five seconds while the view exists. Pause updates to inspect a stable snapshot. Refresh requests a new snapshot. Lists use pages of thirty entries and retain at most five hundred entries, shared across discovered sessions so a busy session cannot displace every quieter session. The Sessions row shows retained call counts and lets you select one session or All sessions. Claude subagent transcripts with the same session ID remain together. Counts describe retained records, not complete session history. Each provider scans a bounded directory inventory and reads at most twenty four recent session files, with at most one MiB of recent content per file. Notices identify reduced history. This is a recent activity inspector, not a complete audit archive.

## Evidence and privacy

Observed means the transcript contains a call without a corresponding result in the retained history. It does not mean the process is still running. Result recorded means a response exists; it does not certify that the requested task succeeded. Failed requires an explicit error flag or hook exit code. Claude hook summaries are shown as summaries. Successful hooks that the agent does not record cannot appear here.

Payloads remain hidden until you select Show masked input and output. Credential field names and common token patterns are masked before display. Arbitrary secrets embedded in prose cannot be detected reliably. Do not share screenshots or source transcripts without reviewing them. The inspector does not provide an export command.

## Verification

Parser tests cover Claude calls and failures, Codex call pairing and recorded intervals, project path boundaries, partial records, explicit hook summaries, session identity, credential masking and transcript symlinks. Kit tests cover nested hook sources, invalid configuration and generated directory exclusions. Native screen reader acceptance remains a separate manual check.

## Tool origins and configured customizations

Agent filters and category filters combine. Built in recognizes a conservative set of tool names for each agent. MCP tools require a recorded `mcp__server__tool` namespace, including the Codex `functions.` prefix when present. Hooks require explicit hook records. Anything else stays Unknown, including orphan results without a tool name. Classification is based on transcript names and is not a guarantee of the implementation behind a call.

Category labels have distinct theme colors: information for Built in, accent for MCP tools, warning for Hooks and muted for Unknown. Text labels remain visible so color is not the only signal. The detail pane explains the classification and shows the MCP server where available. The Custom filter combines MCP tools and recorded hooks; it does not mean that you authored them. A Codex custom_tool_call record type alone does not establish a user customization.

Configured customizations is a separate view of discovered hook definitions and source files, including disabled definitions. They are marked Configured only or Disabled configuration, never counted as executions and never matched to calls by guesswork. Kit provides additional configuration discovery. The inspector does not determine who changed a file or resolve the agent's effective merged configuration.

## Recorded relationships

Flow shows chronological order. Relationships shows an indented call tree using explicit parent tool IDs, or a recorded Claude Agent result paired with the same session's subagent ID. Select a node to see evidence and jump to its parent or children. These jumps clear filters so the target record remains visible. Duplicate, unresolved and cyclic references do not create edges. An unresolved parent ID remains visible in details. Message predecessor IDs are not treated as tool dependencies.

Codex parent thread metadata creates separate Parent session and Child session navigation links. It does not establish call level causality. Session metadata is preserved when reading a truncated transcript tail. Relationships remain limited to retained records; a missing parent is not evidence that the call had no parent.

## Configuration history

Configuration history records a baseline when a discovered source is first observed, then detects changes, removal and reappearance while the inspector is open. The timestamp is observation time, not edit time. Changes before the baseline, changes made and reverted between polls, and changes while Hulder is closed cannot be reconstructed.

History is stored under Hulder's data directory in `agent-activity`, separately for each workspace root set. It stores source paths, field names and SHA256 fingerprints, not configuration values, old command bodies or payloads. Treat this metadata as private too. File access is restricted to the local user on Unix. Atomic replacement and a workspace file lock protect history updates. Corrupt history is preserved and reported rather than silently replaced.

The view retains 200 change events across 256 files, with at most 128 field fingerprints per file and 50 changed field names per event. JSON pointer paths identify changed fields. Large or unreadable sources produce notices without being reported as deleted. A whole file fingerprint detects formatting and changes outside the bounded field inventory. The source button opens the current file; this feature does not restore old values. It does not prove which configuration an agent used for a historical call.

## Research and compatibility

The implementation follows the distinctions documented in [Claude subagent messages](https://code.claude.com/docs/en/agent-sdk/subagents), [Claude hook events](https://code.claude.com/docs/en/hooks), [Claude configuration precedence](https://code.claude.com/docs/en/settings) and [Codex App Server session relationships](https://learn.chatgpt.com/docs/app-server). Local transcript metadata was also inspected without reading tool payloads for the relationship inventory. The inspector reads local transcript files; it does not start an App Server or insert hook logging into your agents. Optional metadata varies with agent versions, so unsupported records remain unlinked or Unknown.

Native validation uses a disposable transcript fixture for explicit links and configuration edits, alongside real Claude activity for normal calls and recorded hooks. Synthetic fixtures are acceptance data, not claims that an agent actually executed those calls. Manual screen reader acceptance remains pending.
