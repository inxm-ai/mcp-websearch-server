//! Typed tool contracts — every tool returns a [`rmcp::Json`] wrapping a type
//! from this module.
//!
//! The typed-return fitness check (tests/typed_tool_returns.rs) enforces
//! this: a tool returning a bare `String` or `CallToolResult` fails the
//! harness.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A greeting produced for a caller, with the language it is written in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GreetingResult {
    pub greeting: String,
    pub language: Language,
}

/// Closed vocabulary — an enum, never a loose string (see AGENTS.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    En,
}
