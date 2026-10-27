//! Behavior tests for `search`: topic mapping, result merging, snippets and
//! the cache, with SearXNG replaced by a scripted engine.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mcp_websearch_server::contract::{SearchRequest, TimeRange, Topic, DEFAULT_NEWS_DAYS};
use mcp_websearch_server::search::{
    effective_time_range, engine_queries, merge_results, run_search, snippet, Category,
    EngineQuery, EngineResponse, EngineResult, SearchCache, SearchEngine, SearchError,
    CACHE_CAPACITY, CACHE_TTL, MAX_RESULTS,
};

fn request(query: &str, topic: Topic, time_range: Option<TimeRange>) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        topic,
        time_range,
        days: DEFAULT_NEWS_DAYS,
        start_date: None,
        end_date: None,
    }
}

fn hit(url: &str, published: Option<&str>) -> EngineResult {
    EngineResult {
        url: url.to_string(),
        title: format!("title of {url}"),
        content: format!("text of {url}"),
        published_date: published.map(str::to_string),
    }
}

#[test]
fn general_topic_searches_only_the_general_category() {
    assert_eq!(
        engine_queries(Topic::General, Some(TimeRange::Week)),
        vec![EngineQuery {
            category: Category::General,
            time_range: Some(TimeRange::Week),
        }]
    );
}

#[test]
fn news_topic_searches_news_then_general_in_the_same_range() {
    assert_eq!(
        engine_queries(Topic::News, Some(TimeRange::Day)),
        vec![
            EngineQuery {
                category: Category::News,
                time_range: Some(TimeRange::Day),
            },
            EngineQuery {
                category: Category::General,
                time_range: Some(TimeRange::Day),
            },
        ]
    );
}

#[test]
fn news_days_round_up_to_a_searxng_time_range() {
    let cases = [
        (0, TimeRange::Day),
        (1, TimeRange::Day),
        (3, TimeRange::Week),
        (7, TimeRange::Week),
        (8, TimeRange::Month),
        (31, TimeRange::Month),
        (90, TimeRange::Year),
    ];
    for (days, expected) in cases {
        let news = SearchRequest {
            days,
            ..request("q", Topic::News, None)
        };
        assert_eq!(effective_time_range(&news), Some(expected), "days={days}");
    }
}

#[test]
fn an_explicit_time_range_wins_and_general_ignores_days() {
    let news_with_range = SearchRequest {
        days: 1,
        ..request("q", Topic::News, Some(TimeRange::Year))
    };
    assert_eq!(
        effective_time_range(&news_with_range),
        Some(TimeRange::Year)
    );
    assert_eq!(
        effective_time_range(&request("q", Topic::General, None)),
        None
    );
}

#[test]
fn merging_keeps_order_drops_duplicate_urls_and_caps() {
    let news = vec![hit("https://a.example/1", Some("2026-10-26T08:00:00"))];
    let general: Vec<EngineResult> = (0..15)
        .map(|index| hit(&format!("https://b.example/{index}"), None))
        .chain([hit("https://a.example/1/", None)])
        .collect();

    let merged = merge_results(&[news, general], MAX_RESULTS);

    assert_eq!(merged.len(), MAX_RESULTS);
    assert_eq!(merged[0].url, "https://a.example/1");
    assert_eq!(
        merged[0].snippet,
        "2026-10-26 — text of https://a.example/1"
    );
    assert_eq!(merged[1].url, "https://b.example/0");
    assert!(merged.iter().all(|result| result.favicon.is_empty()));
}

#[test]
fn a_duplicate_lends_its_publication_date_to_the_first_hit() {
    let general = vec![hit("https://a.example/story", None)];
    let news = vec![hit("https://a.example/story#top", Some("2026-10-25"))];
    let merged = merge_results(&[general, news], MAX_RESULTS);
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged[0].snippet,
        "2026-10-25 — text of https://a.example/story"
    );
}

#[test]
fn snippet_prefixes_only_a_real_date() {
    assert_eq!(snippet(" body ", None), "body");
    assert_eq!(snippet("body", Some("yesterday")), "body");
    assert_eq!(
        snippet("body", Some("2026-10-26 10:00")),
        "2026-10-26 — body"
    );
    assert_eq!(snippet("", Some("2026-10-26")), "2026-10-26");
}

#[test]
fn cache_entries_expire_after_their_ttl() {
    let cache = SearchCache::new(Duration::from_secs(60), CACHE_CAPACITY);
    let key = mcp_websearch_server::search::CacheKey {
        query: "q".into(),
        topic: Topic::General,
        time_range: None,
    };
    let response = mcp_websearch_server::contract::SearchResponse {
        query: "q".into(),
        answer: String::new(),
        results: Vec::new(),
    };
    let start = Instant::now();
    cache.put(key.clone(), response.clone(), start);
    assert_eq!(
        cache.get(&key, start + Duration::from_secs(59)),
        Some(response)
    );
    assert_eq!(cache.get(&key, start + Duration::from_secs(60)), None);
}

/// Answers each category from a script and counts the calls.
struct ScriptedEngine {
    news: Result<Vec<EngineResult>, u16>,
    general: Result<Vec<EngineResult>, u16>,
    calls: Mutex<Vec<EngineQuery>>,
}

#[async_trait]
impl SearchEngine for ScriptedEngine {
    async fn query(
        &self,
        _query: &str,
        engine_query: EngineQuery,
    ) -> Result<EngineResponse, SearchError> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(engine_query);
        let scripted = match engine_query.category {
            Category::News => &self.news,
            Category::General => &self.general,
        };
        scripted
            .clone()
            .map(|results| EngineResponse {
                results,
                unresponsive_engines: vec![vec!["google news".into(), "timeout".into()]],
            })
            .map_err(SearchError::Status)
    }
}

impl ScriptedEngine {
    fn call_count(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

#[tokio::test]
async fn news_search_merges_both_categories_and_is_cached() {
    let engine = ScriptedEngine {
        news: Ok(vec![hit("https://news.example/1", Some("2026-10-26"))]),
        general: Ok(vec![hit("https://web.example/1", None)]),
        calls: Mutex::new(Vec::new()),
    };
    let cache = SearchCache::new(CACHE_TTL, CACHE_CAPACITY);
    let search = request(
        "Arsenal official statement",
        Topic::News,
        Some(TimeRange::Week),
    );
    let now = Instant::now();

    let first = run_search(&engine, &cache, &search, now).await;
    let second = run_search(&engine, &cache, &search, now + Duration::from_secs(1)).await;

    let first = first.unwrap_or_else(|error| panic!("search failed: {error}"));
    assert!(!first.cache_hit);
    assert_eq!(first.unresponsive_engines, vec!["google news".to_string()]);
    let urls: Vec<&str> = first
        .response
        .results
        .iter()
        .map(|result| result.url.as_str())
        .collect();
    assert_eq!(
        urls,
        vec!["https://news.example/1", "https://web.example/1"]
    );
    assert_eq!(first.response.answer, "");

    let second = second.unwrap_or_else(|error| panic!("search failed: {error}"));
    assert!(second.cache_hit);
    assert_eq!(second.response, first.response);
    assert_eq!(
        engine.call_count(),
        2,
        "the second search came from the cache"
    );
}

#[tokio::test]
async fn one_failing_category_degrades_and_is_not_cached() {
    let engine = ScriptedEngine {
        news: Err(502),
        general: Ok(vec![hit("https://web.example/1", None)]),
        calls: Mutex::new(Vec::new()),
    };
    let cache = SearchCache::new(CACHE_TTL, CACHE_CAPACITY);
    let search = request("q", Topic::News, None);
    let now = Instant::now();

    let outcome = run_search(&engine, &cache, &search, now)
        .await
        .unwrap_or_else(|error| panic!("search failed: {error}"));
    assert_eq!(outcome.response.results.len(), 1);

    run_search(&engine, &cache, &search, now)
        .await
        .unwrap_or_else(|error| panic!("search failed: {error}"));
    assert_eq!(engine.call_count(), 4, "a partial answer is not reused");
}

#[tokio::test]
async fn search_fails_when_every_category_fails() {
    let engine = ScriptedEngine {
        news: Err(503),
        general: Err(503),
        calls: Mutex::new(Vec::new()),
    };
    let cache = SearchCache::new(CACHE_TTL, CACHE_CAPACITY);
    let outcome = run_search(
        &engine,
        &cache,
        &request("q", Topic::News, None),
        Instant::now(),
    )
    .await;
    assert!(matches!(outcome, Err(SearchError::Status(503))));
}

fn cache_key(query: &str) -> mcp_websearch_server::search::CacheKey {
    mcp_websearch_server::search::CacheKey {
        query: query.into(),
        topic: Topic::General,
        time_range: None,
    }
}

fn empty_response(query: &str) -> mcp_websearch_server::contract::SearchResponse {
    mcp_websearch_server::contract::SearchResponse {
        query: query.into(),
        answer: String::new(),
        results: Vec::new(),
    }
}

#[test]
fn a_full_cache_evicts_the_answer_stored_longest_ago() {
    let cache = SearchCache::new(Duration::from_secs(600), 2);
    let start = Instant::now();
    cache.put(cache_key("first"), empty_response("first"), start);
    cache.put(
        cache_key("second"),
        empty_response("second"),
        start + Duration::from_secs(1),
    );
    cache.put(
        cache_key("third"),
        empty_response("third"),
        start + Duration::from_secs(2),
    );

    let now = start + Duration::from_secs(3);
    assert_eq!(cache.len(), 2);
    assert_eq!(cache.get(&cache_key("first"), now), None);
    assert!(cache.get(&cache_key("second"), now).is_some());
    assert!(cache.get(&cache_key("third"), now).is_some());
}

#[test]
fn storing_a_cached_query_again_evicts_nothing() {
    let cache = SearchCache::new(Duration::from_secs(600), 2);
    let start = Instant::now();
    cache.put(cache_key("first"), empty_response("first"), start);
    cache.put(cache_key("second"), empty_response("second"), start);
    cache.put(
        cache_key("first"),
        empty_response("first"),
        start + Duration::from_secs(1),
    );

    assert_eq!(cache.len(), 2);
    assert!(cache.get(&cache_key("second"), start).is_some());
}

#[test]
fn unique_queries_never_grow_the_cache_past_its_capacity() {
    let cache = SearchCache::new(CACHE_TTL, CACHE_CAPACITY);
    let start = Instant::now();
    for index in 0..CACHE_CAPACITY * 3 {
        let query = format!("query {index}");
        cache.put(
            cache_key(&query),
            empty_response(&query),
            start + Duration::from_millis(index as u64),
        );
    }
    assert_eq!(cache.len(), CACHE_CAPACITY);
}
