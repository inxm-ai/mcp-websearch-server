//! Tool registration and bridge wiring for mcp-websearch-server.

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;

use crate::contract::{GreetingResult, Language};

/// Pure logic — keep it separately callable so behavior tests never need the
/// server (Logic Sandwich: pure logic here, I/O only at adapter edges).
pub fn build_greeting(name: &str) -> GreetingResult {
    GreetingResult {
        greeting: format!("Hello, {name}!"),
        language: Language::En,
    }
}

/// Arguments for the `greet` tool — field names, types, and doc comments
/// flow directly into the tool's JSON Schema.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GreetRequest {
    /// Display name to address.
    pub name: String,
}

#[derive(Clone, Default)]
pub struct Greeter;

#[tool_router]
impl Greeter {
    /// Produce a personalized greeting for `name`.
    ///
    /// Use this when the user asks to be greeted or welcomed. `name` is the
    /// display name to address; the result carries the greeting text and the
    /// language it is written in.
    #[tool(description = "Produce a personalized greeting for `name`.")]
    async fn greet(
        &self,
        Parameters(GreetRequest { name }): Parameters<GreetRequest>,
    ) -> Result<Json<GreetingResult>, McpError> {
        Ok(Json(build_greeting(&name)))
    }
}

// rustfmt wraps or joins this attribute depending on the substituted server
// name's length; skip it so a generated repo is fmt-clean for any name. Safe
// to drop (with a `cargo fmt` run) once this line stops changing.
#[rustfmt::skip]
#[tool_handler(name = "mcp-websearch-server", instructions = "Greets callers by name.")]
impl ServerHandler for Greeter {}
