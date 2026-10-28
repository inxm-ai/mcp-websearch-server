//! Behavior tests for `search`: topic mapping, result merging, snippets and
//! the cache, with SearXNG replaced by a scripted engine.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mcp_websearch_server::contract::{SearchRequest, TimeRange, Topic, DEFAULT_NEWS_DAYS};
use mcp_websearch_server::search::{
    effective_time_range, engine_queries, interleave, languages, merge_results, run_search,
    snippet, Category, EngineQuery, EngineResponse, EngineResult, InvalidLanguages, SearchCache,
    SearchEngine, SearchError, CACHE_CAPACITY, CACHE_TTL, MAX_RESULTS,
};

fn request(query: &str, topic: Topic, time_range: Option<TimeRange>) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        topic,
        time_range,
        days: DEFAULT_NEWS_DAYS,
        start_date: None,
        end_date: None,
        languages: None,
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
        engine_queries(Topic::General, Some(TimeRange::Week), None),
        vec![EngineQuery {
            category: Category::General,
            time_range: Some(TimeRange::Week),
            language: None,
        }]
    );
}

#[test]
fn news_topic_searches_news_then_general_in_the_same_range() {
    assert_eq!(
        engine_queries(Topic::News, Some(TimeRange::Day), Some("de")),
        vec![
            EngineQuery {
                category: Category::News,
                time_range: Some(TimeRange::Day),
                language: Some("de".into()),
            },
            EngineQuery {
                category: Category::General,
                time_range: Some(TimeRange::Day),
                language: Some("de".into()),
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
        languages: Vec::new(),
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
            .push(engine_query.clone());
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
        self.calls().len()
    }

    fn calls(&self) -> Vec<EngineQuery> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn owned(raw: &[&str]) -> Vec<String> {
    raw.iter().map(|raw| raw.to_string()).collect()
}

#[test]
fn languages_take_codes_with_an_optional_region_normalised_once_each() {
    let cases: [(Option<&[&str]>, &[&str]); 7] = [
        (None, &[]),
        (Some(&[]), &[]),
        (Some(&["", "  "]), &[]),
        (Some(&["de"]), &["de"]),
        (Some(&[" DE ", "de_de", "EN-gb"]), &["de", "de-DE", "en-GB"]),
        (Some(&["de", "fr", "en", "es"]), &["de", "fr", "en", "es"]),
        (Some(&["de", "DE", "fr", "de"]), &["de", "fr"]),
    ];
    for (raw, expected) in cases {
        let raw = raw.map(owned);
        assert_eq!(
            languages(raw.as_deref(), 4),
            Ok(owned(expected)),
            "languages({raw:?})"
        );
    }
}

#[test]
fn languages_refuse_what_searxng_would_not_read() {
    for raw in [
        "german", "d", "deu", "de-DEU", "de-D3", "d3", "de-DE-x", "auto", "de DE", "-DE",
    ] {
        assert_eq!(
            languages(Some(&owned(&["en", raw])), 4),
            Err(InvalidLanguages::Code(raw.to_string())),
            "languages([en, {raw:?}])"
        );
    }
}

#[test]
fn all_is_refused_because_it_gives_bing_no_market() {
    for raw in ["all", "ALL", " All "] {
        assert_eq!(
            languages(Some(&owned(&[raw])), 4),
            Err(InvalidLanguages::All),
            "languages([{raw:?}])"
        );
    }
}

#[test]
fn languages_are_capped_after_duplicates_collapse() {
    assert_eq!(
        languages(Some(&owned(&["de", "fr", "en", "es", "it"])), 4),
        Err(InvalidLanguages::TooMany { count: 5, max: 4 })
    );
    assert_eq!(
        languages(Some(&owned(&["de", "fr", "en", "es", "de"])), 4),
        Ok(owned(&["de", "fr", "en", "es"]))
    );
}

#[test]
fn interleaving_takes_one_from_each_list_in_turn() {
    let list = |urls: &[&str]| urls.iter().map(|url| hit(url, None)).collect::<Vec<_>>();
    let urls: Vec<String> = interleave(vec![
        list(&["de1", "de2", "de3"]),
        list(&["fr1"]),
        list(&[]),
        list(&["en1", "en2"]),
    ])
    .into_iter()
    .map(|result| result.url)
    .collect();
    assert_eq!(urls, owned(&["de1", "fr1", "en1", "de2", "en2", "de3"]));
}

#[tokio::test]
async fn each_language_is_searched_and_the_answer_mixes_them() {
    let engine = ScriptedEngine {
        news: Ok(Vec::new()),
        general: Ok(vec![hit("https://web.example/1", None)]),
        calls: Mutex::new(Vec::new()),
    };
    let cache = SearchCache::new(CACHE_TTL, CACHE_CAPACITY);
    let search = SearchRequest {
        languages: Some(owned(&["de", "fr"])),
        ..request("Bundesliga", Topic::News, None)
    };
    run_search(&engine, &cache, &search, Instant::now())
        .await
        .unwrap_or_else(|error| panic!("search failed: {error}"));
    let calls: Vec<(Category, Option<String>)> = engine
        .calls()
        .into_iter()
        .map(|call| (call.category, call.language))
        .collect();
    assert_eq!(
        calls,
        vec![
            (Category::News, Some("de".to_string())),
            (Category::General, Some("de".to_string())),
            (Category::News, Some("fr".to_string())),
            (Category::General, Some("fr".to_string())),
        ]
    );
}

#[tokio::test]
async fn without_languages_one_pass_runs_in_searxngs_default() {
    let engine = ScriptedEngine {
        news: Ok(Vec::new()),
        general: Ok(vec![hit("https://web.example/1", None)]),
        calls: Mutex::new(Vec::new()),
    };
    let cache = SearchCache::new(CACHE_TTL, CACHE_CAPACITY);
    run_search(
        &engine,
        &cache,
        &request("inxm", Topic::General, None),
        Instant::now(),
    )
    .await
    .unwrap_or_else(|error| panic!("search failed: {error}"));
    let languages: Vec<Option<String>> = engine
        .calls()
        .into_iter()
        .map(|call| call.language)
        .collect();
    assert_eq!(languages, vec![None]);
}

/// Answers each language with its own URLs, so their share of the answer
/// shows.
struct PerLanguageEngine;

#[async_trait]
impl SearchEngine for PerLanguageEngine {
    async fn query(
        &self,
        _query: &str,
        engine_query: EngineQuery,
    ) -> Result<EngineResponse, SearchError> {
        let language = engine_query.language.unwrap_or_default();
        Ok(EngineResponse {
            results: (1..=10)
                .map(|n| hit(&format!("https://{language}.example/{n}"), None))
                .collect(),
            unresponsive_engines: Vec::new(),
        })
    }
}

#[tokio::test]
async fn several_languages_share_the_capped_answer() {
    let cache = SearchCache::new(CACHE_TTL, CACHE_CAPACITY);
    let search = SearchRequest {
        languages: Some(owned(&["de", "fr", "en", "es"])),
        ..request("q", Topic::General, None)
    };
    let outcome = run_search(&PerLanguageEngine, &cache, &search, Instant::now())
        .await
        .unwrap_or_else(|error| panic!("search failed: {error}"));
    let hosts: Vec<&str> = outcome
        .response
        .results
        .iter()
        .map(|result| result.url.split('/').nth(2).unwrap_or_default())
        .collect();
    assert_eq!(hosts.len(), MAX_RESULTS);
    assert_eq!(
        hosts,
        vec![
            "de.example",
            "fr.example",
            "en.example",
            "es.example",
            "de.example",
            "fr.example",
            "en.example",
            "es.example",
            "de.example",
            "fr.example",
        ]
    );
}

#[tokio::test]
async fn a_search_in_another_language_is_not_answered_from_the_cache() {
    let engine = ScriptedEngine {
        news: Ok(Vec::new()),
        general: Ok(vec![hit("https://web.example/1", None)]),
        calls: Mutex::new(Vec::new()),
    };
    let cache = SearchCache::new(CACHE_TTL, CACHE_CAPACITY);
    let now = Instant::now();
    let english = request("inxm", Topic::General, None);
    let german = SearchRequest {
        languages: Some(owned(&["de"])),
        ..english.clone()
    };

    let first = run_search(&engine, &cache, &english, now).await;
    let second = run_search(&engine, &cache, &german, now).await;
    let third = run_search(&engine, &cache, &german, now).await;

    assert!(!first.unwrap_or_else(|error| panic!("{error}")).cache_hit);
    assert!(!second.unwrap_or_else(|error| panic!("{error}")).cache_hit);
    assert!(third.unwrap_or_else(|error| panic!("{error}")).cache_hit);
    assert_eq!(engine.call_count(), 2);
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
        languages: Vec::new(),
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
