use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

/// The Saga: notes, tales, verses and clippings kept as Markdown under `.hulder/`.
#[with_fallible_options]
#[derive(Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema, MergeFrom, Debug)]
pub struct SagaSettingsContent {
    /// Save element picks, Lightpanda fetches and PDF text as files under
    /// `.hulder/clippings/` and open those files. When false they open in
    /// scratch buffers instead.
    ///
    /// Default: true
    pub save_clippings: Option<bool>,
}
