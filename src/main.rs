//! Executable shim — the bridge's default stdio entrypoint.
//!
//! Keep this file to imports and startup only; tools, contracts, and wiring
//! live in the owned modules (see AGENTS.md).

use mcp_websearch_server::config::Config;
use mcp_websearch_server::server::WebSearch;
use rmcp::{transport::stdio, ServiceExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout is the JSON-RPC wire for the stdio transport — every log line
    // must go to stderr instead, or it corrupts the protocol stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config = Config::from_env()?;
    let server = WebSearch::new(&config)?.serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}
