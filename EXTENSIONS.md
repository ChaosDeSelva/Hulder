# Hulder extensions and add ons

Hulder has two extension paths. Existing language and appearance extensions use the inherited WebAssembly extension API. Hulder feature add ons use a declarative API for native panels, commands, settings and project templates.

## Existing extensions

Open Extensions to install, update or remove catalog extensions. Install Dev Extension selects a local extension source folder. To install from a repository, clone that repository yourself, review its source and select the cloned extension folder. Development extensions may require Rust and grammar build tools. Updating the checkout does not establish compatibility; rebuild and test it before use.

Existing capabilities include language grammars, language servers, themes, icon themes, snippets, context servers, slash commands and debugger integrations. These depend on the extension and its declared capabilities. Hulder uses locally installed language servers. The catalogue uses a separate HTTPS connection restricted to public catalogue and archive hosts; it does not enable the shared editor cloud client. Language server downloads through extension runtime HTTP remain blocked, so install the required server locally. Compatibility with every upstream catalog entry is not guaranteed.

## Feature add ons

Open Extensions and choose Feature add ons, or run Hulder Addons: Open from the command palette. Select Install or update from folder and choose a folder containing hulder-addon.json. Repository installation follows the same reviewed clone workflow. There is no automatic repository fetch or marketplace account requirement.

Installation copies the manifest into the hulder-addons folder inside your Hulder configuration directory. The source folder is not executed or watched. New installations and every update start disabled. Enable an add on to display its contributions. Edit its settings and select Save settings before running a command. Reload refreshes changes from another window. Remove deletes the installed manifest and preferences, but does not stop processes you already launched or remove projects you created.

Updates use the same identifier and replace the installed manifest after confirmation. Matching settings are preserved. To roll back, install the previously reviewed version from its folder and enable it again. Different API versions and unknown fields or permissions are rejected rather than guessed.

## API version 1

The manifest is a UTF8 JSON object of at most 1 MiB. Required fields are api_version (1), id, name, version (semantic version) and description. Identifiers contain lowercase ASCII letters, digits, underscores or hyphens. Unknown fields are errors.

Optional fields:

* permissions: process and create_project are the only accepted values.
* settings: a map from identifiers to string defaults. Values are local preferences, not a credential vault.
* commands: objects with id, label, executable and args (an array of strings).
* panels: objects with title, text and commands (an array of declared command identifiers). Panels use Hulder native UI components; they cannot supply HTML, scripts or arbitrary native code.
* templates: objects with id, label, directory and files (a map from relative file paths to UTF8 file contents).

A setting reference such as ${settings.runtime} must occupy the entire executable or argument value. Substitution does not evaluate a shell expression. Project commands use the active local project directory. Open and trust a project folder before running them. Remote projects are not supported by this API version.

Commands appear in the add on view and can be bound in keymaps using this action:

```json
["hulder_addons::Run", {"addon": "love", "command": "run"}]
```

Every command requires an explicit prompt showing the command and working directory. Project trust and installed state are checked again after approval. A changed or disabled add on cannot use an approval issued for an earlier configuration. Output and exit status appear in a normal terminal tab. Save modified buffers before running; the add on API does not automatically save them.

The process permission authorizes launching a normal user process after confirmation. It is not a process sandbox: the executable has the same filesystem and network access as other terminal commands. There is no separate network capability or hidden network client in the declarative host. Install only add ons from publishers you trust.

The create_project permission allows writing a supplied template into a new child folder under the parent you select. Existing folders are never overwritten. Absolute paths, parent traversal, Git metadata and conflicting file paths are rejected. No install hooks or project scripts are executed during creation. If a filesystem error interrupts the final move, a partial new project may remain and the error is shown.

Panels are intentionally declarative: text, commands and template controls, with settings rendered by the host. Embedded browsers, arbitrary GUI widgets, background daemons, runtime event subscriptions and custom editor rendering are not part of API 1. These require separately designed permissions and lifecycle contracts.

## Reference add on

The examples/addons/love folder contains LÖVE game tools. It creates a playable game, exposes a runtime path setting and supplies Run game and Check LÖVE version commands. Install LÖVE separately. Lua language support remains a separate extension. The example does not claim a visual scene editor, full LÖVE type definitions or a game debugger.

## Compatibility and testing

Run cargo test --locked -p extensions_ui --lib for feature add on validation and persistence tests. Run cargo test --locked -p extension_host --lib for inherited extension host regression tests. Build the IDE and exercise installation, enable, settings, template creation, command cancellation, execution, disable, update and removal in an isolated profile before publishing changes.

API 1 is a source preview contract. A future incompatible contract must use a different api_version. Add on permissions and input validation are enforced by the host, not left to author conventions.

## Language coverage

Built in syntax grammars cover Bash, C, C++, CSS, Go, JSON, Markdown, Python, Rust, JavaScript, TypeScript, TSX, YAML and related formats. Install a compatible language extension for additional grammars, including Lua and Odin. Syntax colouring requires a grammar and highlight queries; it does not require a language server or compiler. Confirm the language name in the editor status bar when a file is not recognised. File association settings can map custom suffixes.

Lua uses the Lua extension and .lua files. Odin uses the Odin extension and .odin files. Completion and diagnostics additionally require lua-language-server or ols installed locally and discoverable by the extension. A compiler, debugger and project tasks are separate tools. No claim is made that every programming language or every catalogue extension has been tested. A language without a compatible grammar needs an extension before it can receive syntax colouring.
