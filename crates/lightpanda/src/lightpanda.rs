pub mod fetch;
pub mod hoard_fetch;
pub mod mcp;
pub mod panel;
pub mod reader;
pub mod run_target;
pub mod runner;
pub mod settings;
pub mod status;

pub use fetch::Dump;
pub use runner::{PandaRunner, RunnerState};
pub use settings::LightpandaSettings;

use ::settings::Settings as _;
use gpui::{App, actions};
use workspace::Workspace;

actions!(
    lightpanda,
    [
        /// Prompts for a URL and opens it as Markdown fetched by Lightpanda.
        OpenUrl,
        /// Prompts for a URL and opens its semantic text fetched by Lightpanda.
        OpenUrlSemantic,
        /// Starts or stops the local Lightpanda CDP server.
        ToggleServer,
        /// Runs the project's tests against the local Lightpanda CDP endpoint.
        OpenRunTarget,
        /// Writes a lightpanda MCP server entry into the project's .mcp.json.
        ShareWithAgents,
        /// Toggles focus on the Panda panel.
        TogglePanel,
    ]
);

pub fn init(cx: &mut App) {
    runner::init(cx);
    reader::init(cx);
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        reader::UrlPrompt::register(workspace);
        run_target::register(workspace);
        mcp::register(workspace);
        workspace.register_action(|workspace, _: &ToggleServer, _window, cx| {
            if !settings::LightpandaSettings::get_global(cx).enabled {
                reader::toast(workspace, reader::disabled_message().to_string(), cx);
                return;
            }
            PandaRunner::global(cx).update(cx, |runner, cx| runner.toggle(cx));
        });
    })
    .detach();
}
