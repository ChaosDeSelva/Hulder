use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// Lightpanda headless browser. Off by default: when enabled, Hulder may run
/// `lightpanda` on your behalf and every page it loads is logged in the Hollow Back.
#[with_fallible_options]
#[derive(Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug)]
pub struct LightpandaSettingsContent {
    /// Whether Hulder may run Lightpanda at all.
    ///
    /// Default: false
    pub enabled: Option<bool>,
    /// Path to the lightpanda binary.
    ///
    /// Default: ~/.local/bin/lightpanda
    pub binary: Option<String>,
    /// CDP port for `lightpanda serve`.
    ///
    /// Default: 9222
    pub port: Option<u16>,
    /// Pass --obey-robots to `lightpanda serve`.
    ///
    /// Default: true
    pub obey_robots: Option<bool>,
}
