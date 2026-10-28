//! `search`: maps a tavily-shaped request onto SearXNG category queries,
//! merges their results and caches the answer.
//!
//! Pure apart from the [`SearchEngine`] port; `searxng_adapter` implements it.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use tracing::{field, Instrument};
use url::Url;

use crate::contract::{SearchHit, SearchRequest, SearchResponse, TimeRange, Topic};

/// Hits returned per search.
pub const MAX_RESULTS: usize = 10;
/// How long a search answer is reused, so a plan's fan-out does not re-hit
/// the engines with the same query.
pub const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Most search answers held at once; past it the oldest is evicted, so a
/// stream of unique queries cannot grow the cache without bound.
pub const CACHE_CAPACITY: usize = 1_000;

const DAYS_PER_WEEK: u32 = 7;
const DAYS_PER_MONTH: u32 = 31;
/// Separates the publication date from the engine's text in a snippet.
const DATE_PREFIX_SEPARATOR: &str = " — ";
/// Length of the `YYYY-MM-DD` part of an ISO-8601 timestamp.
const ISO_DATE_LEN: usize = 10;

/// A SearXNG result category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    General,
    News,
}

impl Category {
    /// The category name in SearXNG's `categories` parameter.
    pub fn as_searxng(self) -> &'static str {
        match self {
            Category::General => "general",
            Category::News => "news",
        }
    }
}

/// One query sent to SearXNG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineQuery {
    pub category: Category,
    pub time_range: Option<TimeRange>,
    /// A [`languages`] code; `None` leaves SearXNG's own default (the
    /// chart's `searxng.defaultLanguage`).
    pub language: Option<String>,
}

/// Why a request's `languages` cannot be searched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidLanguages {
    #[error("languages are ISO 639-1 codes, optionally with a region, such as \"de\" or \"de-DE\"; got {0:?}")]
    Code(String),
    /// SearXNG's `all` gives Bing no market, which then picks one itself
    /// (Chinese, from a datacenter IP).
    #[error("\"all\" is not a language: name the languages to search, e.g. [\"de\", \"en\"]")]
    All,
    #[error("at most {max} languages per call; got {count}")]
    TooMany { count: usize, max: usize },
}

const LANGUAGE_LETTERS: usize = 2;
const REGION_LETTERS: usize = 2;
const ALL_LANGUAGES: &str = "all";

/// One language as SearXNG spells it: `de`, `de-DE`. `_` is read as `-` and
/// case is normalised, so `DE_de` becomes `de-DE`.
fn language(raw: &str) -> Result<String, InvalidLanguages> {
    if raw.eq_ignore_ascii_case(ALL_LANGUAGES) {
        return Err(InvalidLanguages::All);
    }
    let invalid = || InvalidLanguages::Code(raw.to_string());
    let mut parts = raw.split(['-', '_']);
    let code = parts.next().unwrap_or_default();
    let region = parts.next();
    let letters = |part: &str, count: usize| {
        part.len() == count
            && part
                .chars()
                .all(|character| character.is_ascii_alphabetic())
    };
    if parts.next().is_some() || !letters(code, LANGUAGE_LETTERS) {
        return Err(invalid());
    }
    let code = code.to_ascii_lowercase();
    match region {
        None => Ok(code),
        Some(region) if letters(region, REGION_LETTERS) => {
            Ok(format!("{code}-{}", region.to_ascii_uppercase()))
        }
        Some(_) => Err(invalid()),
    }
}

/// The languages a request searches in, checked and normalised, each once,
/// in the caller's order. None, or only blanks, is no language: SearXNG's
/// default applies.
pub fn languages(raw: Option<&[String]>, max: usize) -> Result<Vec<String>, InvalidLanguages> {
    let mut checked: Vec<String> = Vec::new();
    for raw in raw.unwrap_or_default().iter().map(|raw| raw.trim()) {
        if raw.is_empty() {
            continue;
        }
        let language = language(raw)?;
        if !checked.contains(&language) {
            checked.push(language);
        }
    }
    if checked.len() > max {
        return Err(InvalidLanguages::TooMany {
            count: checked.len(),
            max,
        });
    }
    Ok(checked)
}

/// The hosts of `urls`, in order, comma-separated, for a span: what a
/// search brought back, without the full URLs. Unparsable URLs show as `?`.
pub fn result_hosts<'a>(urls: impl IntoIterator<Item = &'a str>) -> String {
    urls.into_iter()
        .map(|url| {
            Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
                .unwrap_or_else(|| "?".to_string())
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Takes one result from each list in turn, so with several languages each
/// gets a share of the capped answer.
pub fn interleave(lists: Vec<Vec<EngineResult>>) -> Vec<EngineResult> {
    let mut iterators: Vec<_> = lists.into_iter().map(Vec::into_iter).collect();
    let mut interleaved = Vec::new();
    loop {
        let before = interleaved.len();
        interleaved.extend(iterators.iter_mut().filter_map(Iterator::next));
        if interleaved.len() == before {
            return interleaved;
        }
    }
}

/// The time range a request searches: its own `time_range`, or for news the
/// tavily `days` look-back rounded up to the next range SearXNG knows.
pub fn effective_time_range(request: &SearchRequest) -> Option<TimeRange> {
    match (request.time_range, request.topic) {
        (Some(range), _) => Some(range),
        (None, Topic::General) => None,
        (None, Topic::News) => Some(match request.days.max(1) {
            1 => TimeRange::Day,
            days if days <= DAYS_PER_WEEK => TimeRange::Week,
            days if days <= DAYS_PER_MONTH => TimeRange::Month,
            _ => TimeRange::Year,
        }),
    }
}

/// The SearXNG queries for a topic. SearXNG's news category alone is thin
/// (often a single engine answers), so `news` also searches the general
/// category in the same time range; news results come first.
pub fn engine_queries(
    topic: Topic,
    time_range: Option<TimeRange>,
    language: Option<&str>,
) -> Vec<EngineQuery> {
    let query = |category| EngineQuery {
        category,
        time_range,
        language: language.map(str::to_string),
    };
    match topic {
        Topic::General => vec![query(Category::General)],
        Topic::News => vec![query(Category::News), query(Category::General)],
    }
}

/// SearXNG's JSON answer (`format=json`), reduced to what this server reads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EngineResponse {
    #[serde(default)]
    pub results: Vec<EngineResult>,
    /// `[engine, reason]` pairs for engines that failed this query.
    #[serde(default)]
    pub unresponsive_engines: Vec<Vec<String>>,
}

/// One SearXNG result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct EngineResult {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub content: String,
    #[serde(default, rename = "publishedDate")]
    pub published_date: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("the search backend could not be reached: {0}")]
    Unreachable(String),
    #[error("the search backend answered HTTP {0}")]
    Status(u16),
    #[error("the search backend answered something other than search results: {0}")]
    Decode(String),
}

impl SearchError {
    /// A short name for traces (`error.type`).
    pub fn kind(&self) -> &'static str {
        match self {
            SearchError::Unreachable(_) => "unreachable",
            SearchError::Status(_) => "http_status",
            SearchError::Decode(_) => "decode",
        }
    }
}

/// Port to the metasearch backend.
#[async_trait]
pub trait SearchEngine: Send + Sync {
    async fn query(
        &self,
        query: &str,
        engine_query: EngineQuery,
    ) -> Result<EngineResponse, SearchError>;
}

/// The date part of an ISO-8601 timestamp, when it looks like one.
fn published_day(published: &str) -> Option<&str> {
    let day = published.get(..ISO_DATE_LEN)?;
    let looks_like_date = day
        .chars()
        .enumerate()
        .all(|(index, character)| match index {
            4 | 7 => character == '-',
            _ => character.is_ascii_digit(),
        });
    looks_like_date.then_some(day)
}

/// The snippet shown for a hit: the engine's text, prefixed with its
/// publication day when known.
pub fn snippet(content: &str, published_date: Option<&str>) -> String {
    let content = content.trim();
    match published_date.and_then(published_day) {
        Some(day) if content.is_empty() => day.to_string(),
        Some(day) => format!("{day}{DATE_PREFIX_SEPARATOR}{content}"),
        None => content.to_string(),
    }
}

/// Key that treats `https://a/b/` and `https://a/b#x` as the same page.
fn dedup_key(url: &str) -> &str {
    let without_fragment = url.split('#').next().unwrap_or(url);
    without_fragment.trim_end_matches('/')
}

/// Merges result lists in order, keeping the first hit per URL (a later
/// duplicate only lends its publication date), capped at `cap`.
pub fn merge_results(result_lists: &[Vec<EngineResult>], cap: usize) -> Vec<SearchHit> {
    let mut merged: Vec<EngineResult> = Vec::new();
    let mut position_by_key: HashMap<String, usize> = HashMap::new();
    for result in result_lists.iter().flatten() {
        if result.url.trim().is_empty() {
            continue;
        }
        let key = dedup_key(&result.url).to_string();
        match position_by_key.get(&key) {
            Some(&position) => {
                if merged[position].published_date.is_none() {
                    merged[position].published_date = result.published_date.clone();
                }
            }
            None => {
                position_by_key.insert(key, merged.len());
                merged.push(result.clone());
            }
        }
    }
    merged
        .into_iter()
        .take(cap)
        .map(|result| SearchHit {
            snippet: snippet(&result.content, result.published_date.as_deref()),
            title: result.title.trim().to_string(),
            url: result.url,
            favicon: String::new(),
        })
        .collect()
}

/// Names of the engines that failed, deduplicated, in first-seen order.
pub fn unresponsive_engine_names(responses: &[EngineResponse]) -> Vec<String> {
    let mut seen = HashSet::new();
    responses
        .iter()
        .flat_map(|response| response.unresponsive_engines.iter())
        .filter_map(|pair| pair.first().cloned())
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

/// What a search cache entry is looked up by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub query: String,
    pub topic: Topic,
    pub time_range: Option<TimeRange>,
    pub languages: Vec<String>,
}

/// In-memory search answers, each reused for `ttl`, at most `capacity` of
/// them.
pub struct SearchCache {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<HashMap<CacheKey, (Instant, SearchResponse)>>,
}

impl SearchCache {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<CacheKey, (Instant, SearchResponse)>> {
        // A panic while holding the lock leaves a map that is still valid.
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn get(&self, key: &CacheKey, now: Instant) -> Option<SearchResponse> {
        self.entries()
            .get(key)
            .filter(|(stored_at, _)| now.saturating_duration_since(*stored_at) < self.ttl)
            .map(|(_, response)| response.clone())
    }

    /// Stores `response`, dropping entries that have expired and, when the
    /// cache is still full, the one stored longest ago.
    pub fn put(&self, key: CacheKey, response: SearchResponse, now: Instant) {
        let mut entries = self.entries();
        entries.retain(|_, (stored_at, _)| now.saturating_duration_since(*stored_at) < self.ttl);
        if !entries.contains_key(&key) && entries.len() >= self.capacity {
            let oldest = entries
                .iter()
                .min_by_key(|(_, (stored_at, _))| *stored_at)
                .map(|(oldest, _)| oldest.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        entries.insert(key, (now, response));
    }

    pub fn len(&self) -> usize {
        self.entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }
}

/// A search answer plus what telemetry records about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    pub response: SearchResponse,
    pub cache_hit: bool,
    pub unresponsive_engines: Vec<String>,
}

/// Runs `request` against `engine`, answering from `cache` when it can. A
/// search fails only when every category query fails; complete, non-empty
/// answers are cached. `request.languages` must already be checked with
/// [`languages`]; each is searched, and their results interleaved.
pub async fn run_search(
    engine: &dyn SearchEngine,
    cache: &SearchCache,
    request: &SearchRequest,
    now: Instant,
) -> Result<SearchOutcome, SearchError> {
    let time_range = effective_time_range(request);
    let key = CacheKey {
        query: request.query.trim().to_string(),
        topic: request.topic,
        time_range,
        languages: request.languages.clone().unwrap_or_default(),
    };
    if let Some(response) = cache.get(&key, now) {
        return Ok(SearchOutcome {
            response,
            cache_hit: true,
            unresponsive_engines: Vec::new(),
        });
    }

    // No language: one pass in SearXNG's default.
    let passes: Vec<Option<&str>> = if key.languages.is_empty() {
        vec![None]
    } else {
        key.languages
            .iter()
            .map(|language| Some(language.as_str()))
            .collect()
    };
    let queries: Vec<(usize, EngineQuery)> = passes
        .iter()
        .enumerate()
        .flat_map(|(pass, language)| {
            engine_queries(request.topic, time_range, *language)
                .into_iter()
                .map(move |engine_query| (pass, engine_query))
        })
        .collect();
    // One span per SearXNG query, so a trace shows what each language and
    // category brought back.
    let spans: Vec<tracing::Span> = queries
        .iter()
        .map(|(_, engine_query)| {
            tracing::info_span!(
                "search.engine_query",
                otel.status_code = field::Empty,
                error.type = field::Empty,
                search.category = engine_query.category.as_searxng(),
                search.language = engine_query.language.as_deref().unwrap_or(""),
                search.result_count = field::Empty,
                search.result_hosts = field::Empty,
                search.unresponsive_engines = field::Empty,
            )
        })
        .collect();
    let answers =
        futures::future::join_all(queries.iter().zip(&spans).map(|((_, engine_query), span)| {
            engine
                .query(&key.query, engine_query.clone())
                .instrument(span.clone())
        }))
        .await;
    let failed_queries = answers.iter().filter(|answer| answer.is_err()).count();
    let mut responses = Vec::new();
    let mut results_by_pass: Vec<Vec<EngineResult>> = vec![Vec::new(); passes.len()];
    let mut last_error = None;
    for (((pass, _), span), answer) in queries.iter().zip(&spans).zip(answers) {
        match answer {
            Ok(response) => {
                span.record("search.result_count", response.results.len());
                span.record(
                    "search.result_hosts",
                    result_hosts(response.results.iter().map(|result| result.url.as_str())),
                );
                span.record(
                    "search.unresponsive_engines",
                    unresponsive_engine_names(std::slice::from_ref(&response)).join(","),
                );
                results_by_pass[*pass].extend(response.results.iter().cloned());
                responses.push(response);
            }
            Err(error) => {
                span.record("otel.status_code", "ERROR");
                span.record("error.type", error.kind());
                tracing::warn!(parent: span, error = %error, "a search category query failed");
                last_error = Some(error);
            }
        }
    }
    if responses.is_empty() {
        if let Some(error) = last_error {
            return Err(error);
        }
    }

    let interleaved = interleave(results_by_pass);
    let response = SearchResponse {
        query: request.query.clone(),
        answer: String::new(),
        results: merge_results(&[interleaved], MAX_RESULTS),
    };
    if failed_queries == 0 && !response.results.is_empty() {
        cache.put(key, response.clone(), now);
    }
    Ok(SearchOutcome {
        response,
        cache_hit: false,
        unresponsive_engines: unresponsive_engine_names(&responses),
    })
}
