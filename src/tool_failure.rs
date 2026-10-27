//! Tool-level failures: the tool ran, and the caller sees why it could not
//! deliver (`isError: true` with a typed reason), as opposed to protocol
//! errors the client renders opaquely.

use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock};
use rmcp::ErrorData as McpError;

/// Why a tool could not deliver; its wire name prefixes the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The tool exists for compatibility but has no implementation here.
    ToolNotAvailable,
    /// A service this tool depends on failed.
    UpstreamFailed,
    /// The caller's arguments cannot be served.
    InvalidArgument,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::ToolNotAvailable => "tool_not_available",
            FailureKind::UpstreamFailed => "upstream_failed",
            FailureKind::InvalidArgument => "invalid_argument",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFailure {
    pub kind: FailureKind,
    pub message: String,
}

impl ToolFailure {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ToolFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl IntoCallToolResult for ToolFailure {
    fn into_call_tool_result(self) -> Result<CallToolResponse, McpError> {
        Ok(CallToolResult::error(vec![ContentBlock::text(self.to_string())]).into())
    }
}
