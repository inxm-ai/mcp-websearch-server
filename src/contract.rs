//! Typed tool contracts — every tool returns a [`rmcp::Json`] wrapping a type
//! from this module.
//!
//! Field names and defaults copy mcp-tavily-server's tools exactly, so plans
//! written against tavily keep working after the server swap.
//!
//! The typed-return fitness check (tests/typed_tool_returns.rs) enforces
//! this: a tool returning a bare `String` or `CallToolResult` fails the
//! harness.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which kind of results `search` looks for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Topic {
    #[default]
    General,
    News,
}

/// How far back results may be published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TimeRange {
    Day,
    Week,
    Month,
    Year,
}

/// tavily's default look-back for `topic=news` when no `time_range` is given.
pub const DEFAULT_NEWS_DAYS: u32 = 7;

fn default_news_days() -> u32 {
    DEFAULT_NEWS_DAYS
}

/// Arguments for `search`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchRequest {
    /// What to search for, e.g. "Arsenal official statement".
    pub query: String,
    /// `general` for web pages, `news` for recent reporting.
    #[serde(default)]
    pub topic: Topic,
    /// Only results published within this range.
    #[serde(default)]
    pub time_range: Option<TimeRange>,
    /// With `topic=news` and no `time_range`: how many days back to look
    /// (at least 1).
    #[serde(default = "default_news_days")]
    pub days: u32,
    /// Accepted for compatibility; the search backend has no date-range
    /// filter, so use `time_range` instead.
    #[serde(default)]
    pub start_date: Option<String>,
    /// Accepted for compatibility; see `start_date`.
    #[serde(default)]
    pub end_date: Option<String>,
}

/// One web search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    /// Text the engines show for the hit, prefixed with its publication date
    /// (`YYYY-MM-DD — `) when one is known.
    pub snippet: String,
    pub favicon: String,
}

/// Result of `search`: at most 10 hits, best first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchResponse {
    pub query: String,
    /// Always empty: this server does not generate answers.
    pub answer: String,
    pub results: Vec<SearchHit>,
}
