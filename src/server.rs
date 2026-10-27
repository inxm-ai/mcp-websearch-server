//! Tool registration and bridge wiring for mcp-websearch-server.
//!
//! The one place that builds adapters and hands them to the pure modules as
//! ports.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::{ProgressNotificationParam, ProgressToken, RequestMetaObject},
    tool, tool_handler, tool_router, Peer, RoleServer, ServerHandler,
};
use tracing::{field, Instrument};

use crate::address_policy::loggable_url;
use crate::config::Config;
use crate::contract::{
    CrawlCompanyRequest, CrawlRequest, DownloadRequest, DownloadedFile, ExtractRequest,
    ExtractResponse, ResearchRequest, ResearchResult, SearchRequest, SearchResponse,
    SocialMediaRequest, SocialMediaSearchResults,
};
use crate::crawl::{
    compile_path_patterns, crawl_timeout, CrawlOutcome, CrawlPlan, Crawler, HostGate,
    DEFAULT_CRAWL_LIMIT,
};
use crate::download::download;
use crate::extract::{extract_urls, PageReader, Renderer};
use crate::fetch::HttpFetcher;
use crate::http_fetch_adapter::ReqwestFetcher;
use crate::llm_adapter::{TgiChatModel, UnconfiguredModel};
use crate::model::ChatModel;
use crate::render_adapter::StygianRenderer;
use crate::research::{Phase, ProgressSink, Researcher};
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
    http: Arc<dyn HttpFetcher>,
    page_reader: Arc<PageReader>,
    crawler: Arc<Crawler>,
    researcher: Arc<Researcher>,
}

/// Reports research phases as MCP progress notifications, when the caller
/// asked for progress.
struct PeerProgress {
    peer: Peer<RoleServer>,
    token: Option<ProgressToken>,
}

#[async_trait]
impl ProgressSink for PeerProgress {
    async fn phase(&self, phase: Phase) {
        tracing::info!(phase = phase.as_str(), "web research phase");
        let Some(token) = &self.token else {
            return;
        };
        let notification = ProgressNotificationParam::new(token.clone(), f64::from(phase.step()))
            .with_total(f64::from(Phase::COUNT))
            .with_message(phase.as_str());
        if let Err(error) = self.peer.notify_progress(notification).await {
            tracing::debug!(error = %error, "could not send a progress notification");
        }
    }
}

/// A caller's count argument as a bound of at least 1.
fn at_least_one(value: u32) -> usize {
    usize::try_from(value.max(1)).unwrap_or(usize::MAX)
}

/// Runs `plan` and records the crawl's shape on the tool span.
async fn run_crawl(crawler: &Crawler, plan: CrawlPlan, span: &tracing::Span) -> ExtractResponse {
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + crawl_timeout(plan.max_breadth, plan.max_depth);
    let outcome: CrawlOutcome = crawler
        .crawl(&plan, deadline)
        .instrument(span.clone())
        .await;
    record_latency(span, started);
    span.record("crawl.pages_read", outcome.pages_read);
    span.record("crawl.timed_out", outcome.timed_out);
    span.record("crawl.scored_irrelevant", outcome.scored_irrelevant);
    span.record("crawl.scoring_failures", outcome.scoring_failures);
    outcome.response
}

impl WebSearch {
    /// Builds the server and its adapters from `config`.
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let http: Arc<dyn HttpFetcher> = Arc::new(ReqwestFetcher::new()?);
        let renderer: Option<Arc<dyn Renderer>> = if config.render_enabled {
            Some(Arc::new(StygianRenderer::new(
                config.render_pool_size,
                config.render_timeout,
            )))
        } else {
            None
        };
        let model: Arc<dyn ChatModel> = match &config.tgi_url {
            Some(tgi_url) => Arc::new(TgiChatModel::new(tgi_url, &config.default_model)?),
            None => Arc::new(UnconfiguredModel),
        };
        let page_reader = Arc::new(PageReader::new(http.clone(), renderer));
        let search_engine: Arc<dyn SearchEngine> =
            Arc::new(SearxngClient::new(&config.searxng_url)?);
        let search_cache = Arc::new(SearchCache::new(CACHE_TTL));
        Ok(Self {
            crawler: Arc::new(Crawler {
                reader: page_reader.clone(),
                http: http.clone(),
                model: model.clone(),
                gate: Arc::new(HostGate::default()),
            }),
            researcher: Arc::new(Researcher {
                search_engine: search_engine.clone(),
                search_cache: search_cache.clone(),
                reader: page_reader.clone(),
                model,
            }),
            search_engine,
            search_cache,
            page_reader,
            http,
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

    /// Read web pages as readable text.
    #[tool(
        description = "Extract and process content from specific web pages. Use this tool when you have URLs and need to get the full text content from those pages: articles, documentation, or any web page you need to analyze or reference. Returns each page's readable text (main content, without navigation and scripts); URLs that cannot be read are listed in `failed` with the reason (refused address, HTTP status, unsupported content type such as PDF, no readable text, timeout). Up to 50 URLs are read per call; any further URLs are listed in `failed` as not read.\n\nExamples:\n{\"urls\": [\"https://inxm.ai\"]}\n{\"urls\": [\"https://techcrunch.com/2024/01/15/ai-startup-funding-round/\", \"https://venturebeat.com/ai/orchestration-platforms-2024/\"]}",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn extract(
        &self,
        Parameters(request): Parameters<ExtractRequest>,
    ) -> Result<Json<ExtractResponse>, ToolFailure> {
        let span = tracing::info_span!(
            "tool.extract",
            tool.latency_ms = field::Empty,
            extract.url_count = request.urls.len(),
            extract.stage.http = field::Empty,
            extract.stage.render = field::Empty,
            extract.stage.failed = field::Empty,
        );
        let started = Instant::now();
        let (response, counts) = extract_urls(&self.page_reader, &request.urls)
            .instrument(span.clone())
            .await;
        record_latency(&span, started);
        span.record("extract.stage.http", counts.http);
        span.record("extract.stage.render", counts.render);
        span.record("extract.stage.failed", counts.failed);
        Ok(Json(response))
    }

    /// Download one file by URL.
    #[tool(
        description = "Download a file from a given URL and return its content: text files (text/*, JSON, XML) as a string, any other file base64-encoded, together with its content type. Files larger than 10 MiB are refused.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn download_file(
        &self,
        Parameters(request): Parameters<DownloadRequest>,
    ) -> Result<Json<DownloadedFile>, ToolFailure> {
        let span = tracing::info_span!(
            "tool.download_file",
            tool.latency_ms = field::Empty,
            url.full = %loggable_url(&request.url),
        );
        let started = Instant::now();
        let file = download(self.http.as_ref(), &request.url)
            .instrument(span.clone())
            .await;
        record_latency(&span, started);
        let file = file.map_err(|failure| ToolFailure::new(failure.kind(), failure.to_string()))?;
        Ok(Json(file))
    }

    /// Crawl a site from a start URL, breadth-first, same site only.
    #[tool(
        description = "Crawl a website starting from a given URL to discover and extract content from multiple pages within the same site. Use this tool when you want to gather comprehensive information from a website, such as articles, blog posts, or documentation spread across various pages. The crawler follows links up to `max_depth` links deep, at most `max_breadth` new links per page and at most 50 pages, honours robots.txt, and returns each page's text (first 5,000 characters unless `full_content`). With `instructions`, pages a model judges irrelevant to them are left out.\n\nExamples:\n{\"start_url\": \"https://inxm.ai\", \"max_breadth\": 5, \"max_depth\": 2}\n{\"start_url\": \"https://example.com/blog\", \"instructions\": \"Find all pages on their Python SDK\", \"max_breadth\": 15, \"max_depth\": 3}\n{\"start_url\": \"https://newswebsite.com\", \"max_breadth\": 20, \"max_depth\": 1}",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn crawl(
        &self,
        Parameters(request): Parameters<CrawlRequest>,
    ) -> Result<Json<ExtractResponse>, ToolFailure> {
        let span = crawl_span("tool.crawl");
        let plan = CrawlPlan {
            start_url: request.start_url,
            instructions: request.instructions,
            select_paths: Vec::new(),
            exclude_paths: Vec::new(),
            allow_external: false,
            full_content: request.full_content,
            max_breadth: at_least_one(request.max_breadth),
            max_depth: at_least_one(request.max_depth),
            limit: DEFAULT_CRAWL_LIMIT,
        };
        Ok(Json(run_crawl(&self.crawler, plan, &span).await))
    }

    /// Crawl a company's own website for business information.
    #[tool(
        description = "Crawl a company's own website to gather business intelligence such as their products, team, mission, or pricing. Use this tool instead of the generic `crawl` tool when researching a specific company (e.g. competitor analysis, sales prospecting, or due diligence): it stays within the company's site by default and lets you narrow the crawl to specific sections via select_paths and exclude_paths (regex patterns matched against the URL path). Reads at most `limit` pages, honours robots.txt, and returns each page's text (first 5,000 characters unless `full_content`). With `instructions`, pages a model judges irrelevant to them are left out.\n\nExamples:\n{\"url\": \"https://inxm.ai\", \"instructions\": \"Find information about the company's product and team\"}\n{\"url\": \"https://example.com\", \"select_paths\": [\"/about.*\", \"/team.*\", \"/pricing.*\"]}\n{\"url\": \"https://competitor.com\", \"instructions\": \"Summarize their product offering and pricing tiers\", \"max_depth\": 2}",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn crawl_company_website(
        &self,
        Parameters(request): Parameters<CrawlCompanyRequest>,
    ) -> Result<Json<ExtractResponse>, ToolFailure> {
        let span = crawl_span("tool.crawl_company_website");
        let invalid = |error: crate::crawl::CrawlError| {
            ToolFailure::new(FailureKind::InvalidArgument, error.to_string())
        };
        let plan = CrawlPlan {
            start_url: request.url,
            instructions: request.instructions,
            select_paths: compile_path_patterns(&request.select_paths.unwrap_or_default())
                .map_err(invalid)?,
            exclude_paths: compile_path_patterns(&request.exclude_paths.unwrap_or_default())
                .map_err(invalid)?,
            allow_external: request.allow_external,
            full_content: request.full_content,
            max_breadth: at_least_one(request.max_breadth),
            max_depth: at_least_one(request.max_depth),
            limit: at_least_one(request.limit),
        };
        Ok(Json(run_crawl(&self.crawler, plan, &span).await))
    }

    /// Research a topic: search, read the best sources, write a report.
    #[tool(
        description = "An advanced research agent that performs comprehensive research on a given topic by conducting multiple searches, reading the best sources, and writing a detailed markdown report. The report cites only sources that were read, as [n] numbered like `sources`. `model`: \"mini\" (default; up to 3 searches and 8 sources) or \"pro\" (a larger model, up to 6 searches and 16 sources). Reports progress as it goes: searching, reading, writing.\n\nIMPORTANT: For anything that requires in-depth information gathering, data synthesis, or extensive analysis of a topic, prefer this tool over search or extraction tools.\n\nExamples:\n{\"input\": \"Comprehensive overview of the latest advancements in renewable energy technologies.\"}\n{\"input\": \"In-depth analysis of the impact of artificial intelligence on modern healthcare systems and patient care.\"}",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn web_research(
        &self,
        peer: Peer<RoleServer>,
        meta: RequestMetaObject,
        Parameters(request): Parameters<ResearchRequest>,
    ) -> Result<Json<ResearchResult>, ToolFailure> {
        let span = tracing::info_span!(
            "tool.web_research",
            tool.latency_ms = field::Empty,
            research.model = field::Empty,
            research.source_count = field::Empty,
        );
        let model = request.model.unwrap_or_default();
        span.record("research.model", format!("{model:?}").to_lowercase());
        let progress = PeerProgress {
            peer,
            token: meta.get_progress_token(),
        };
        let started = Instant::now();
        let result = self
            .researcher
            .research(&request.input, model, &progress)
            .instrument(span.clone())
            .await;
        record_latency(&span, started);
        let result = result.map_err(|failure| {
            ToolFailure::new(FailureKind::UpstreamFailed, failure.to_string())
        })?;
        span.record("research.source_count", result.sources.len());
        Ok(Json(result))
    }

    /// Not available on this server: always fails with `tool_not_available`.
    #[tool(
        description = "Not available on this server yet: social media search always fails with `tool_not_available`. Use `search` for public web results instead.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn search_social_media(
        &self,
        Parameters(_request): Parameters<SocialMediaRequest>,
    ) -> Result<Json<SocialMediaSearchResults>, ToolFailure> {
        Err(social_media_not_available())
    }
}

/// The typed refusal `search_social_media` answers every call with.
pub fn social_media_not_available() -> ToolFailure {
    ToolFailure::new(
        FailureKind::ToolNotAvailable,
        "search_social_media is not available on this server yet",
    )
}

fn crawl_span(name: &'static str) -> tracing::Span {
    tracing::info_span!(
        "tool.crawl",
        otel.name = name,
        tool.latency_ms = field::Empty,
        crawl.pages_read = field::Empty,
        crawl.timed_out = field::Empty,
        crawl.scored_irrelevant = field::Empty,
        crawl.scoring_failures = field::Empty,
    )
}

// rustfmt wraps or joins this attribute depending on the substituted server
// name's length; skip it so a generated repo is fmt-clean for any name. Safe
// to drop (with a `cargo fmt` run) once this line stops changing.
#[rustfmt::skip]
#[tool_handler(name = "mcp-websearch-server", instructions = "Web search, page extraction, file download, site crawling and multi-step web research over self-hosted search (SearXNG) and a page renderer.")]
impl ServerHandler for WebSearch {}
