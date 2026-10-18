use anyhow::{Context as _, Result, bail};
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let Some(argument) = arguments.next() else {
        bail!("Usage: bifrost --socket /absolute/path/to/hulder.sock");
    };
    if argument == "--help" || argument == "-h" {
        println!(
            "Bifrost local MCP bridge\n\nUsage: bifrost --socket /absolute/path/to/hulder.sock\n\nRun from the project directory open in Hulder. JSON RPC uses stdin and stdout."
        );
        return Ok(());
    }
    if argument != "--socket" {
        bail!("Expected --socket followed by the Hulder socket path");
    }
    let socket = PathBuf::from(arguments.next().context("Missing socket path")?);
    if !socket.is_absolute() || arguments.next().is_some() {
        bail!("Provide one absolute socket path");
    }
    bifrost_core::mcp::run(socket)
}
