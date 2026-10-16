use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// Raven, the API client whose requests live in `.hulder/ravens/`.
#[with_fallible_options]
#[derive(Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug)]
pub struct RavenSettingsContent {
    /// Header names (any case) replaced by `<redacted>` in clippings and
    /// Bifröst replies, on top of Authorization, Cookie, Set-Cookie,
    /// X-Api-Key and every Hoard `header` profile's name.
    ///
    /// Default: []
    pub redact_headers: Option<Vec<String>>,
}
