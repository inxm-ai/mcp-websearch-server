//! Tool-level failures reach the caller as `isError` results with a typed
//! reason, never as protocol errors.

use mcp_websearch_server::server::social_media_not_available;
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::CallToolResponse;

#[test]
fn search_social_media_answers_tool_not_available() {
    let failure = social_media_not_available();
    assert_eq!(
        failure.to_string(),
        "tool_not_available: search_social_media is not available on this server yet"
    );
    let response = failure
        .into_call_tool_result()
        .unwrap_or_else(|error| panic!("became a protocol error: {error:?}"));
    let CallToolResponse::Complete(result) = response else {
        panic!("not a complete tool result");
    };
    assert_eq!(result.is_error, Some(true));
}
