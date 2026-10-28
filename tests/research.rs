//! `web_research`: query planning, source selection and the bounded loop,
//! with the model, the search engine and the web scripted.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mcp_websearch_server::contract::{ResearchModel, SearchHit};
use mcp_websearch_server::extract::PageReader;
use mcp_websearch_server::model::{ModelError, ModelTier};
use mcp_websearch_server::research::{
    drop_unknown_citations, limits, parse_queries, select_sources, Phase, ProgressSink,
    ResearchFailure, Researcher,
};
use mcp_websearch_server::search::{
    EngineQuery, EngineResponse, EngineResult, SearchCache, SearchEngine, SearchError,
    CACHE_CAPACITY, CACHE_TTL,
};

mod common;
use common::{ScriptedHttp, ScriptedModel};

fn hit(url: &str) -> SearchHit {
    SearchHit {
        title: format!("title {url}"),
        url: url.to_string(),
        snippet: String::new(),
        favicon: String::new(),
    }
}

#[test]
fn pro_doubles_the_mini_limits_on_the_default_model() {
    let mini = limits(ResearchModel::Mini);
    let pro = limits(ResearchModel::Pro);
    assert_eq!(
        (mini.searches, mini.sources, mini.tier),
        (3, 8, ModelTier::Fast)
    );
    assert_eq!(
        (pro.searches, pro.sources, pro.tier),
        (6, 16, ModelTier::Default)
    );
    assert_eq!(limits(ResearchModel::Auto), mini);
}

#[test]
fn planned_queries_lose_list_markers_and_duplicates_and_are_capped() {
    let answer = "<think>plan</think>\n1. AI adoption manufacturing Europe\n- \"AI in German factories\"\n\n2) ai adoption manufacturing europe\n* EU AI Act manufacturing\n• extra query";
    assert_eq!(
        parse_queries(answer, "request", 3),
        vec![
            "AI adoption manufacturing Europe",
            "AI in German factories",
            "EU AI Act manufacturing",
        ]
    );
    assert_eq!(
        parse_queries("  \n", " the request ", 3),
        vec!["the request"]
    );
}

#[test]
fn sources_are_taken_round_robin_without_duplicates() {
    let lists = vec![
        vec![hit("https://a/1"), hit("https://a/2"), hit("https://a/3")],
        vec![hit("https://a/1/"), hit("https://b/2")],
        vec![hit("https://c/1")],
    ];
    let urls: Vec<String> = select_sources(&lists, 4)
        .into_iter()
        .map(|hit| hit.url)
        .collect();
    assert_eq!(
        urls,
        vec!["https://a/1", "https://c/1", "https://a/2", "https://b/2"]
    );
}

#[test]
fn citations_to_unknown_sources_are_removed() {
    assert_eq!(
        drop_unknown_citations("Growth [1][3], see [2] and [0]; arrays like [x] stay.", 2),
        "Growth [1], see [2] and ; arrays like [x] stay."
    );
}

/// Every query returns the same ranked URLs.
struct FixedEngine {
    urls: Vec<String>,
}

#[async_trait]
impl SearchEngine for FixedEngine {
    async fn query(&self, query: &str, _: EngineQuery) -> Result<EngineResponse, SearchError> {
        Ok(EngineResponse {
            results: self
                .urls
                .iter()
                .map(|url| EngineResult {
                    url: format!("{url}?q={}", query.len()),
                    title: format!("About {url}"),
                    content: String::new(),
                    published_date: None,
                })
                .collect(),
            unresponsive_engines: Vec::new(),
        })
    }
}

#[derive(Default)]
struct RecordedProgress {
    phases: Mutex<Vec<Phase>>,
}

#[async_trait]
impl ProgressSink for RecordedProgress {
    async fn phase(&self, phase: Phase) {
        self.phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(phase);
    }
}

const PROSE: &str = "European manufacturers report rising use of machine learning for \
    predictive maintenance and quality inspection, while smaller firms cite skills and data \
    readiness as the main obstacles to adoption across the sector.";

fn researcher(
    model: Arc<ScriptedModel>,
    pages: &[(&str, u16, &str, &str)],
    urls: &[&str],
) -> Researcher {
    Researcher {
        search_engine: Arc::new(FixedEngine {
            urls: urls.iter().map(|url| url.to_string()).collect(),
        }),
        search_cache: Arc::new(SearchCache::new(CACHE_TTL, CACHE_CAPACITY)),
        reader: Arc::new(PageReader::new(Arc::new(ScriptedHttp::new(pages)), None)),
        model,
    }
}

#[tokio::test]
async fn research_cites_only_pages_that_were_read() {
    let article = format!("<p>{PROSE}</p>");
    let model = Arc::new(ScriptedModel::new(&[
        ("You plan web research", Ok("manufacturing AI Europe")),
        (
            "You write research reports",
            Ok("# AI in manufacturing\n\nAdoption is rising [1], see also [7]."),
        ),
    ]));
    let researcher = researcher(
        model.clone(),
        &[
            ("https://ok.example/report?q=23", 200, "text/html", &article),
            ("https://gone.example/page?q=23", 404, "text/html", ""),
        ],
        &["https://ok.example/report", "https://gone.example/page"],
    );
    let progress = RecordedProgress::default();

    let result = researcher
        .research("state of AI adoption", ResearchModel::Mini, &[], &progress)
        .await
        .unwrap_or_else(|error| panic!("research failed: {error}"));

    assert_eq!(
        result.content,
        "# AI in manufacturing\n\nAdoption is rising [1], see also ."
    );
    let sources: Vec<(&str, &str)> = result
        .sources
        .iter()
        .map(|source| (source.url.as_str(), source.title.as_str()))
        .collect();
    assert_eq!(
        sources,
        vec![(
            "https://ok.example/report?q=23",
            "About https://ok.example/report"
        )]
    );
    assert_eq!(
        *progress
            .phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![Phase::Searching, Phase::Reading, Phase::Writing]
    );
    let calls = model.calls();
    assert_eq!(
        calls.len(),
        2,
        "one planning and one writing call, nothing more"
    );
    assert!(calls.iter().all(|(tier, _)| *tier == ModelTier::Fast));
    assert!(calls[1].1.contains("[1] About https://ok.example/report"));
    assert!(!calls[1].1.contains("gone.example"));
}

#[tokio::test]
async fn research_reads_at_most_the_source_limit() {
    let article = format!("<p>{PROSE}</p>");
    let urls: Vec<String> = (0..20)
        .map(|index| format!("https://s{index}.example/a"))
        .collect();
    let pages: Vec<(String, String)> = urls
        .iter()
        .map(|url| (format!("{url}?q=1"), article.clone()))
        .collect();
    let page_table: Vec<(&str, u16, &str, &str)> = pages
        .iter()
        .map(|(url, body)| (url.as_str(), 200, "text/html", body.as_str()))
        .collect();
    let model = Arc::new(ScriptedModel::new(&[
        ("You plan web research", Ok("q")),
        ("You write research reports", Ok("# Report")),
    ]));
    let url_refs: Vec<&str> = urls.iter().map(String::as_str).collect();
    let researcher = researcher(model, &page_table, &url_refs);

    let result = researcher
        .research(
            "topic",
            ResearchModel::Mini,
            &[],
            &RecordedProgress::default(),
        )
        .await
        .unwrap_or_else(|error| panic!("research failed: {error}"));

    assert_eq!(result.sources.len(), 8);
}

#[tokio::test]
async fn research_fails_when_nothing_could_be_read_or_the_model_is_down() {
    let model = Arc::new(ScriptedModel::new(&[("You plan web research", Ok("q"))]));
    let unreadable = researcher(model, &[], &["https://down.example/a"]);
    assert_eq!(
        unreadable
            .research(
                "topic",
                ResearchModel::Mini,
                &[],
                &RecordedProgress::default()
            )
            .await,
        Err(ResearchFailure::NothingRead)
    );

    let down = Arc::new(ScriptedModel::new(&[(
        "You plan web research",
        Err(ModelError::NotConfigured),
    )]));
    assert_eq!(
        researcher(down, &[], &[])
            .research(
                "topic",
                ResearchModel::Pro,
                &[],
                &RecordedProgress::default()
            )
            .await,
        Err(ResearchFailure::Model(ModelError::NotConfigured))
    );
}

/// A [`FixedEngine`] that also records each query's language.
struct LanguageRecordingEngine {
    inner: FixedEngine,
    languages: Mutex<Vec<Option<String>>>,
}

#[async_trait]
impl SearchEngine for LanguageRecordingEngine {
    async fn query(
        &self,
        query: &str,
        engine_query: EngineQuery,
    ) -> Result<EngineResponse, SearchError> {
        self.languages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(engine_query.language.clone());
        self.inner.query(query, engine_query).await
    }
}

#[tokio::test]
async fn research_searches_in_each_language_it_is_given() {
    let article = format!("<p>{PROSE}</p>");
    let engine = Arc::new(LanguageRecordingEngine {
        inner: FixedEngine {
            urls: vec!["https://ok.example/bericht".to_string()],
        },
        languages: Mutex::new(Vec::new()),
    });
    let researcher = Researcher {
        search_engine: engine.clone(),
        search_cache: Arc::new(SearchCache::new(CACHE_TTL, CACHE_CAPACITY)),
        reader: Arc::new(PageReader::new(
            Arc::new(ScriptedHttp::new(&[(
                // FixedEngine adds the query's length: "KI Fertigung" is 12.
                "https://ok.example/bericht?q=12",
                200,
                "text/html",
                &article,
            )])),
            None,
        )),
        model: Arc::new(ScriptedModel::new(&[
            ("You plan web research", Ok("KI Fertigung")),
            ("You write research reports", Ok("# Bericht")),
        ])),
    };

    researcher
        .research(
            "KI in der Fertigung",
            ResearchModel::Mini,
            &["de".to_string(), "en".to_string()],
            &RecordedProgress::default(),
        )
        .await
        .unwrap_or_else(|error| panic!("research failed: {error}"));

    let languages = engine
        .languages
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        languages,
        vec![Some("de".to_string()), Some("en".to_string())],
        "the planned query is searched once per language"
    );
}
