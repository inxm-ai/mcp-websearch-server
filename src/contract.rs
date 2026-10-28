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
    /// Languages to search in: ISO 639-1 language codes, optionally with a
    /// region, e.g. ["de"], ["de-DE"], or ["de", "fr", "en", "es"] for
    /// several at once (results are mixed across them; 4 at most by
    /// default). Pass the language the user writes in. Language codes, not
    /// country codes: "en" not "us", "zh" not "cn". Without it, results are
    /// in English.
    #[serde(default)]
    pub languages: Option<Vec<String>>,
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

/// Arguments for `extract`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ExtractRequest {
    /// Absolute http(s) URLs of the pages to read; any number.
    pub urls: Vec<String>,
}

/// One page read to text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExtractedPage {
    /// The page's readable text: main content, without navigation, scripts
    /// and page chrome.
    pub text: String,
    pub url: String,
    pub favicon: String,
}

/// A URL that yielded no text, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FailedUrl {
    pub url: String,
    pub error: String,
}

/// Result of `extract`, `crawl` and `crawl_company_website`: every requested
/// or visited URL appears in exactly one of the two lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExtractResponse {
    pub extracted: Vec<ExtractedPage>,
    pub failed: Vec<FailedUrl>,
}

/// Arguments for `download_file`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DownloadRequest {
    /// Absolute http(s) URL of the file.
    pub url: String,
}

/// Result of `download_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DownloadedFile {
    /// Text files as text; anything else base64-encoded.
    pub content: String,
    /// The file's MIME type as the server sent it.
    pub content_type: String,
}

fn default_crawl_breadth() -> u32 {
    10
}

fn default_crawl_depth() -> u32 {
    2
}

/// Arguments for `crawl`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CrawlRequest {
    /// Absolute http(s) URL the crawl starts from.
    pub start_url: String,
    /// What the crawl is looking for; pages a model judges irrelevant to it
    /// are left out.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Accepted for compatibility; not used.
    #[serde(default)]
    pub query: Option<String>,
    /// Return each page's full text instead of its first 5,000 characters.
    #[serde(default)]
    pub full_content: bool,
    /// Most links followed from each page (at least 1).
    #[serde(default = "default_crawl_breadth")]
    pub max_breadth: u32,
    /// How many links deep to go from the start page (at least 1).
    #[serde(default = "default_crawl_depth")]
    pub max_depth: u32,
    /// Accepted for compatibility; pages are always read in full fidelity.
    #[serde(default)]
    pub extract_depth: Option<String>,
}

fn default_company_breadth() -> u32 {
    20
}

fn default_company_depth() -> u32 {
    1
}

fn default_company_limit() -> u32 {
    50
}

/// Arguments for `crawl_company_website`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CrawlCompanyRequest {
    /// The company's website, e.g. "https://inxm.ai".
    pub url: String,
    /// What the crawl is looking for; pages a model judges irrelevant to it
    /// are left out.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Regexes on the URL path; when given, only matching pages are
    /// followed, e.g. ["/about.*", "/pricing.*"].
    #[serde(default)]
    pub select_paths: Option<Vec<String>>,
    /// Regexes on the URL path; matching pages are not followed.
    #[serde(default)]
    pub exclude_paths: Option<Vec<String>>,
    /// Follow links to other sites too.
    #[serde(default)]
    pub allow_external: bool,
    /// Return each page's full text instead of its first 5,000 characters.
    #[serde(default)]
    pub full_content: bool,
    /// Most links followed from each page (at least 1).
    #[serde(default = "default_company_breadth")]
    pub max_breadth: u32,
    /// How many links deep to go from the start page (at least 1).
    #[serde(default = "default_company_depth")]
    pub max_depth: u32,
    /// Most pages read (at least 1).
    #[serde(default = "default_company_limit")]
    pub limit: u32,
}

/// How much research `web_research` does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResearchModel {
    /// Fast model; up to 3 searches and 8 sources.
    #[default]
    Mini,
    /// Default model; up to 6 searches and 16 sources.
    Pro,
    /// Same as `mini`.
    Auto,
}

/// Arguments for `web_research`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ResearchRequest {
    /// The research question or topic.
    pub input: String,
    /// `mini` (default) or `pro` for twice the searches and sources.
    #[serde(default)]
    pub model: Option<ResearchModel>,
    /// Languages to search in: ISO 639-1 language codes, optionally with a
    /// region, e.g. ["de"] or ["de", "en"] (4 at most by default). Pass the
    /// language the user writes in. Language codes, not country codes: "en"
    /// not "us", "zh" not "cn". Without it, searches run in English.
    #[serde(default)]
    pub languages: Option<Vec<String>>,
}

/// A source the report cites; every one was read successfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResearchSource {
    pub url: String,
    pub title: String,
    pub favicon: Option<String>,
}

/// Result of `web_research`: a markdown report and the sources it cites as
/// `[n]`, numbered in this order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResearchResult {
    pub content: String,
    pub sources: Vec<ResearchSource>,
}

/// Social platforms tavily's `search_social_media` knew.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Tiktok,
    Facebook,
    Instagram,
    Reddit,
    Linkedin,
    X,
    #[default]
    Combined,
}

fn default_social_max_results() -> u32 {
    10
}

/// Arguments for `search_social_media`, accepted for compatibility.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SocialMediaRequest {
    pub query: String,
    #[serde(default)]
    pub platform: Platform,
    #[serde(default)]
    pub include_raw_content: bool,
    #[serde(default = "default_social_max_results")]
    pub max_results: u32,
    #[serde(default)]
    pub include_answer: bool,
    #[serde(default)]
    pub time_range: Option<TimeRange>,
}

/// One social media hit (the shape tavily returned).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SocialMediaResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub raw_content: Option<String>,
    pub favicon: String,
}

/// Result shape of `search_social_media` (never produced by this server).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SocialMediaSearchResults {
    pub query: String,
    pub platform: Platform,
    pub answer: String,
    pub results: Vec<SocialMediaResult>,
}
