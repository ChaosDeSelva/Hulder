# Hulder guide

Hulder is an independently maintained, local-first IDE. It is not affiliated with or endorsed by Zed Industries.

## Everyday editing

Open a folder using File → Open. Use the command palette to find actions and their configured shortcuts. The Settings and Keymap commands are in the Hulder menu. Project search, split panes, Git changes, diagnostics and terminals use the native editor foundation.

Settings live in `~/.config/hulder`; macOS data lives in `~/Library/Application Support/Hulder` and logs in `~/Library/Logs/Hulder`. Project-level `.zed` configuration and internal action names remain compatible with the inherited engine.

## Local tools

Herd attaches to local Herdr sessions. Saga keeps project notes and tasks. Factory opens the local Mimir task inspector; it requires a running controller and its token file. Factory is optional: ordinary editing does not require the factory services. See FACTORY_INTEGRATION.md in the source checkout for setup.

## Network and updates

Hulder's shared HTTP client refuses outbound requests. Explicit Raven requests, external tools, language servers, Git, browsers and terminal processes have their own network behavior. Factory uses an authenticated loopback connection. The editor has no supported Zed cloud account, subscription or upstream automatic-update service. The separate Hulder updater checks its public GitHub releases on launch and daily while running. Downloads require confirmation.

## Help and attribution

Use View Dependency Licenses for bundled dependency notices. The source README and LICENSE-GPL, LICENSE-APACHE and any component-specific licenses describe the inherited licensing. Hulder derives from Zed; upstream documentation can explain compatible editor concepts, but does not describe Hulder's services or release support.

## Release status

macOS is the initial supported build target. A development build is not a notarized public release. See RELEASE.md for packaging and verification.

## Kit discovery in nested projects

Kit lists recognized configuration across the first visible workspace tree, including nested project directories. Each row shows its source path, and clicking opens that file. Discovery is an inventory, not a claim that every listed rule or server applies to every agent session. Agent configuration scope is still determined by that agent and its working directory.

Supported discovery includes CLAUDE.md, AGENTS.md, .rules, .mcp.json, Claude settings, commands and agent definitions, plus SKILL.md files in .claude/skills, .agents/skills, .codex/skills and skills. Root missing rule indicators remain specific to the workspace root. Nested MCP servers with the same name remain separate.

Dependency, build and environment directories such as node_modules, target, dist, build, vendor, .git and .venv are excluded. Kit uses the editor worktree snapshot; files excluded from that snapshot and additional workspace roots are not included. Listing a nested configuration does not load or execute its tools.

## App updates

Use Hulder > Check for Updates to check immediately. Automatic Update Checks lets you turn scheduled checks on or off; the preference is saved as auto_update in user settings. Automatic checks run shortly after launch and every 24 hours of runtime while the IDE is open. GitHub preview releases are included. Network errors during automatic checks are quiet; a manual check reports errors.

Choose Download update when prompted. Hulder verifies the GitHub asset SHA256, its pinned Developer ID team and Apple notarization, then reveals the installer in Finder. Open the disk image, quit Hulder, drag the new app to Applications and reopen it. Updates do not overwrite the running app or force a restart. Downloaded installers are stored in the Hulder data directory under updates; remove them after installing to reclaim disk space. GitHub receives normal release requests, without project content or account credentials.
