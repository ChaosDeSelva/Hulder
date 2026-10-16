//! Glosses: marginalia for code.
//!
//! A gloss is a Markdown note attached to a file or to a line range of it. Glosses are
//! plain text under `.hulder/glosses/<relative path>.md` inside the worktree, so agents
//! and git see them with no integration.
//!
//! # API for other crates (Saga, Bifröst, editor, project panel)
//!
//! - [`init`] creates the global [`GlossStore`] and registers every `Project` that is
//!   created afterwards.
//! - [`GlossStore::global`] returns the store entity; subscribe to
//!   [`GlossStoreEvent::Changed`] and filter on your own path to re-render when that
//!   file's glosses load or change. A load that finds nothing emits no event.
//! - [`GlossStore::glosses_for`] returns the cached [`LoadedGlosses`] of one file (at most
//!   [`MAX_GLOSSES_PER_FILE`] glosses, plus parse errors and the hidden count) and
//!   triggers an async load the first time it is asked; until the load lands it is empty.
//! - [`GlossStore::add_gloss`] appends a gloss to a file and writes it to disk.
//! - [`GlossStore::all_for_worktree`] lists every gloss file of a worktree with its
//!   parsed glosses, for listings such as the Saga panel or an agent tool.
//! - [`ReanchorIndex`] finds where glosses live in the current text in one pass over the
//!   file; glosses whose anchor cannot be found are `Detached` and are never deleted.
//! - [`model::parse_gloss_file`] / [`model::serialize_gloss_file`] are the file format.
//!
//! The [`AddGloss`] action is handled by the editor and bound to `cmd-ctrl-g`.

pub mod model;
pub mod store;

use gpui::{App, actions};

pub use model::{
    Anchoring, Gloss, GlossParseError, LineRange, MAX_GLOSS_BODY_BYTES, MAX_GLOSSES_PER_FILE,
    ParsedGlossFile, ReanchorIndex,
};
pub use store::{GlossStore, GlossStoreEvent, LoadedGlosses};

actions!(
    glosses,
    [
        /// Adds a gloss (a Markdown note) to the current selection or cursor line.
        AddGloss
    ]
);

pub fn init(cx: &mut App) {
    store::init(cx);
}
