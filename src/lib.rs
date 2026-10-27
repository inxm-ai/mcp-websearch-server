//! Owned modules for mcp-websearch-server.
//!
//! `main.rs` stays the executable shim — imports and startup only. Tool
//! registration and wiring live in [`server`]; typed tool contracts live in
//! [`contract`]; `*_adapter` modules own all network access (see AGENTS.md
//! for the full layering convention).

pub mod address_policy;
pub mod config;
pub mod contract;
pub mod crawl;
pub mod download;
pub mod extract;
pub mod fetch;
pub mod html;
pub mod http_fetch_adapter;
pub mod llm_adapter;
pub mod model;
pub mod render_adapter;
pub mod research;
pub mod search;
pub mod searxng_adapter;
pub mod server;
pub mod tool_failure;
