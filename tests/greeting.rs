//! Behavior tests for the example greeting tool.
//!
//! The pattern to copy when adding tools: call the pure logic directly and
//! assert on the returned contract type — never through the server object,
//! never with a mock of owned code.

use mcp_websearch_server::contract::{GreetingResult, Language};
use mcp_websearch_server::server::build_greeting;

#[test]
fn greeting_addresses_the_caller_by_name() {
    let result = build_greeting("Ada");
    assert_eq!(
        result,
        GreetingResult {
            greeting: "Hello, Ada!".to_string(),
            language: Language::En,
        }
    );
}
