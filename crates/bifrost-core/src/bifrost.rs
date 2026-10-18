#[cfg(not(unix))]
compile_error!("Bifrost currently requires Unix domain sockets on macOS or Linux.");

pub mod guard;
pub mod mcp;
pub mod protocol;
pub mod server;
