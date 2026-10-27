//! Tool registration and bridge wiring for mcp-websearch-server.
//!
//! The one place that builds adapters and hands them to the pure modules as
//! ports.

use std::sync::Arc;
use std::time::Instant;

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    tool, tool_handler, tool_router, ServerHandler,
};
use tracing::{field, Instrument};

use crate::config::Config;
use crate::contract::{SearchRequest, SearchResponse};
use crate::search::{run_search, SearchCache, SearchEngine, CACHE_TTL};
use crate::searxng_adapter::SearxngClient;
use crate::tool_failure::{FailureKind, ToolFailure};

/// Records how long the tool call took on its span.
fn record_latency(span: &tracing::Span, started: Instant) {
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    span.record("tool.latency_ms", latency_ms);
}

#[derive(Clone)]
pub struct WebSearch {
    search_engine: Arc<dyn SearchEngine>,
    search_cache: Arc<SearchCache>,
}

impl WebSearch {
    /// Builds the server and its adapters from `config`.
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            search_engine: Arc::new(SearxngClient::new(&config.searxng_url)?),
            search_cache: Arc::new(SearchCache::new(CACHE_TTL)),
        })
    }
}

#[tool_router]
impl WebSearch {
    /// Search the web for up-to-date information. At most 10 results, each
    /// with title, URL and snippet; with `topic: "news"` snippets start with
    /// the publication date. `answer` is always empty.
    #[tool(
        description = "Search the web for real-time information about any topic. Use this tool when you need up-to-date information that might not be available in your training data, or when you need to verify current facts. Returns at most 10 results with title, URL and snippet; with topic \"news\" snippets start with the publication date when known. `answer` is always empty.\n\nExamples:\n{\"query\": \"What are the latest advancements in AI technology?\", \"topic\": \"general\", \"time_range\": \"month\"}\n{\"query\": \"Recent news about space exploration\", \"topic\": \"news\", \"days\": 3}\n{\"query\": \"Current trends in renewable energy\", \"topic\": \"general\", \"time_range\": \"week\"}",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<SearchResponse>, ToolFailure> {
        let span = tracing::info_span!(
            "tool.search",
            tool.latency_ms = field::Empty,
            search.cache_hit = field::Empty,
            search.unresponsive_engines = field::Empty,
            search.result_count = field::Empty,
        );
        let started = Instant::now();
        let outcome = run_search(
            self.search_engine.as_ref(),
            &self.search_cache,
            &request,
            started,
        )
        .instrument(span.clone())
        .await;
        record_latency(&span, started);
        let outcome = outcome
            .map_err(|error| ToolFailure::new(FailureKind::UpstreamFailed, error.to_string()))?;
        span.record("search.cache_hit", outcome.cache_hit);
        span.record(
            "search.unresponsive_engines",
            outcome.unresponsive_engines.join(","),
        );
        span.record("search.result_count", outcome.response.results.len());
        Ok(Json(outcome.response))
    }
}

// rustfmt wraps or joins this attribute depending on the substituted server
// name's length; skip it so a generated repo is fmt-clean for any name. Safe
// to drop (with a `cargo fmt` run) once this line stops changing.
#[rustfmt::skip]
#[tool_handler(name = "mcp-websearch-server", instructions = "Web search, page extraction, file download, site crawling and multi-step web research over self-hosted search (SearXNG) and a page renderer.")]
impl ServerHandler for WebSearch {}
