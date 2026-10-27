//! Owned modules for mcp-websearch-server.
//!
//! `main.rs` stays the executable shim — imports and startup only. Tool
//! registration and wiring live in [`server`]; typed tool contracts live in
//! [`contract`]; `*_adapter` modules own all network access (see AGENTS.md
//! for the full layering convention).

pub mod config;
pub mod contract;
pub mod search;
pub mod searxng_adapter;
pub mod server;
pub mod tool_failure;
