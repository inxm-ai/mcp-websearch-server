//! Owned modules for mcp-websearch-server.
//!
//! `main.rs` stays the executable shim — imports and startup only. Tool
//! registration and wiring live in [`server`]; typed tool contracts live in
//! [`contract`] (see AGENTS.md for the full layering convention).

pub mod contract;
pub mod server;
