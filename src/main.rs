//! Executable shim — the bridge's default stdio entrypoint.
//!
//! Keep this file to imports and startup only; tools, contracts, and wiring
//! live in the owned modules (see AGENTS.md).

use mcp_websearch_server::config::Config;
use mcp_websearch_server::server::WebSearch;
use mcp_websearch_server::telemetry_adapter;
use rmcp::{transport::stdio, ServiceExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::from_env()?;
    // stdout is the JSON-RPC wire for the stdio transport — logs go to
    // stderr, spans to the collector. Dropping `_telemetry` flushes spans.
    let _telemetry = telemetry_adapter::init(config.otlp_endpoint.as_deref())?;

    let server = WebSearch::new(&config)?.serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}
