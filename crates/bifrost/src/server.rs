pub use bifrost_core::server::*;

pub fn socket_path() -> std::path::PathBuf {
    paths::config_dir().join(SOCKET_FILE)
}
