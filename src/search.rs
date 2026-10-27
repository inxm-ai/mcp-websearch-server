//! `search`: maps a tavily-shaped request onto SearXNG category queries,
//! merges their results and caches the answer.
//!
//! Pure apart from the [`SearchEngine`] port; `searxng_adapter` implements it.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;

use crate::contract::{SearchHit, SearchRequest, SearchResponse, TimeRange, Topic};

/// Hits returned per search.
pub const MAX_RESULTS: usize = 10;
/// How long a search answer is reused, so a plan's fan-out does not re-hit
/// the engines with the same query.
pub const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineQuery {
    pub category: Category,
    pub time_range: Option<TimeRange>,
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
pub fn engine_queries(topic: Topic, time_range: Option<TimeRange>) -> Vec<EngineQuery> {
    let general = EngineQuery {
        category: Category::General,
        time_range,
    };
    match topic {
        Topic::General => vec![general],
        Topic::News => vec![
            EngineQuery {
                category: Category::News,
                time_range,
            },
            general,
        ],
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
}

/// In-memory search answers, each reused for `ttl`.
pub struct SearchCache {
    ttl: Duration,
    entries: Mutex<HashMap<CacheKey, (Instant, SearchResponse)>>,
}

impl SearchCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
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

    /// Stores `response`, dropping entries that have expired.
    pub fn put(&self, key: CacheKey, response: SearchResponse, now: Instant) {
        let mut entries = self.entries();
        entries.retain(|_, (stored_at, _)| now.saturating_duration_since(*stored_at) < self.ttl);
        entries.insert(key, (now, response));
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
/// answers are cached.
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
    };
    if let Some(response) = cache.get(&key, now) {
        return Ok(SearchOutcome {
            response,
            cache_hit: true,
            unresponsive_engines: Vec::new(),
        });
    }

    let queries = engine_queries(request.topic, time_range);
    let answers = futures::future::join_all(
        queries
            .iter()
            .map(|engine_query| engine.query(&key.query, *engine_query)),
    )
    .await;
    let failed_queries = answers.iter().filter(|answer| answer.is_err()).count();
    let mut responses = Vec::new();
    let mut last_error = None;
    for answer in answers {
        match answer {
            Ok(response) => responses.push(response),
            Err(error) => {
                tracing::warn!(error = %error, "a search category query failed");
                last_error = Some(error);
            }
        }
    }
    if responses.is_empty() {
        if let Some(error) = last_error {
            return Err(error);
        }
    }

    let result_lists: Vec<Vec<EngineResult>> = responses
        .iter()
        .map(|response| response.results.clone())
        .collect();
    let response = SearchResponse {
        query: request.query.clone(),
        answer: String::new(),
        results: merge_results(&result_lists, MAX_RESULTS),
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
