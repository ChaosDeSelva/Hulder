use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

#[with_fallible_options]
#[derive(Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug)]
pub struct BrowserSettingsContent {
    /// Path to the browser binary used by `browser::Open`. When unset, Hulder
    /// looks for Google Chrome and then Microsoft Edge in /Applications.
    ///
    /// Default: null
    pub binary: Option<String>,
}
