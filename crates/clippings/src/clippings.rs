pub mod capture;
pub mod model;
pub mod settings;
pub mod store;

pub use capture::{SaveAndOpenError, prepare, save_and_open, store_for};
pub use model::*;
pub use settings::ClippingsSettings;
pub use store::{BodyBuilder, ClippingDraft, ClippingStore, SavedClipping};
