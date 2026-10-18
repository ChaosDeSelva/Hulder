pub use bifrost_core::mcp::run;

pub fn default_socket_path() -> std::path::PathBuf {
    paths::config_dir().join(bifrost_core::mcp::SOCKET_FILE)
}
