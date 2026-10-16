# Hulder guide

Hulder is an independently maintained, local-first IDE. It is not affiliated with or endorsed by Zed Industries.

## Everyday editing

Open a folder using File → Open. Use the command palette to find actions and their configured shortcuts. The Settings and Keymap commands are in the Hulder menu. Project search, split panes, Git changes, diagnostics and terminals use the native editor foundation.

Settings live in `~/.config/hulder`; macOS data lives in `~/Library/Application Support/Hulder` and logs in `~/Library/Logs/Hulder`. Project-level `.zed` configuration and internal action names remain compatible with the inherited engine.

## Local tools

Herd attaches to local Herdr sessions. Saga keeps project notes and tasks. Factory opens the local Mimir task inspector; it requires a running controller and its token file. Factory is optional: ordinary editing does not require the factory services. See FACTORY_INTEGRATION.md in the source checkout for setup.

## Network and updates

Hulder's shared HTTP client refuses outbound requests. Explicit Raven requests, external tools, language servers, Git, browsers and terminal processes have their own network behavior. Factory uses an authenticated loopback connection. The editor has no supported Zed cloud account, subscription or upstream automatic-update service. Install new Hulder releases explicitly.

## Help and attribution

Use View Dependency Licenses for bundled dependency notices. The source README and LICENSE-GPL, LICENSE-APACHE and any component-specific licenses describe the inherited licensing. Hulder derives from Zed; upstream documentation can explain compatible editor concepts, but does not describe Hulder's services or release support.

## Release status

macOS is the initial supported build target. A development build is not a notarized public release. See RELEASE.md for packaging and verification.
