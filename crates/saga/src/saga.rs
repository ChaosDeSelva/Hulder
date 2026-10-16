pub mod model;
pub mod panel;
pub mod store;

pub use model::*;
pub use panel::{
    CyclePriority, CycleStatus, MarkDone, NewLore, NewTale, NewVerse, OpenSelected, SagaPanel,
    SendToAgent, StartAgentOnTale, ToggleFocus, ToggleVerse, init,
};
pub use store::{
    MAX_SEARCH_HITS, SagaConfig, SagaStore, SearchHit, Snapshot, TaleEntry, WriteLock,
    new_write_lock,
};

use gpui::{App, Global};

struct GlobalWriteLock(WriteLock);

impl Global for GlobalWriteLock {}

/// The one write lock every Saga writer in this app shares. Before `init`
/// runs (tests, tools) each call gets a fresh lock.
pub fn shared_write_lock(cx: &App) -> WriteLock {
    cx.try_global::<GlobalWriteLock>()
        .map(|global| global.0.clone())
        .unwrap_or_else(new_write_lock)
}

pub(crate) fn init_write_lock(cx: &mut App) {
    if !cx.has_global::<GlobalWriteLock>() {
        cx.set_global(GlobalWriteLock(new_write_lock()));
    }
}

/// Builds the store for `root` with the app-wide write lock.
pub fn store_for(root: impl Into<std::path::PathBuf>, cx: &App) -> SagaStore {
    SagaStore::new(root).with_write_lock(shared_write_lock(cx))
}
