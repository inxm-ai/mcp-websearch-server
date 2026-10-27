//! `web_research`: a bounded loop on tgi-core. Plan up to N search queries
//! from the request, search each, read up to M distinct result pages, and
//! write a markdown report that cites only pages that were read. No further
//! iteration and no tool use by the model.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use crate::contract::{
    ResearchModel, ResearchResult, ResearchSource, SearchHit, SearchRequest, Topic,
    DEFAULT_NEWS_DAYS,
};
use crate::extract::{extract_urls, PageReader};
use crate::model::{strip_reasoning, ChatModel, ModelError, ModelTier};
use crate::search::{run_search, SearchCache, SearchEngine};

/// `mini` limits; `pro` doubles both.
const MINI_SEARCHES: usize = 3;
const MINI_SOURCES: usize = 8;
const PRO_FACTOR: usize = 2;
/// Characters of each source shown to the writing model.
const SOURCE_EXCERPT_CHARS: usize = 6_000;

const PLAN_SYSTEM_PROMPT: &str = "You plan web research. Given a research request, write \
    distinct web search queries that together cover it. Output only the queries, one per \
    line, without numbering, quotes or commentary.";
const REPORT_SYSTEM_PROMPT: &str = "You write research reports in markdown. Use only the \
    numbered sources provided; cite them inline as [n] using their numbers. Do not cite or \
    invent any other source, and say so when the sources do not cover part of the request. \
    Start with a `# ` title.";

/// How much one research call may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResearchLimits {
    pub searches: usize,
    pub sources: usize,
    pub tier: ModelTier,
}

pub fn limits(model: ResearchModel) -> ResearchLimits {
    match model {
        ResearchModel::Mini | ResearchModel::Auto => ResearchLimits {
            searches: MINI_SEARCHES,
            sources: MINI_SOURCES,
            tier: ModelTier::Fast,
        },
        ResearchModel::Pro => ResearchLimits {
            searches: MINI_SEARCHES * PRO_FACTOR,
            sources: MINI_SOURCES * PRO_FACTOR,
            tier: ModelTier::Default,
        },
    }
}

/// The phases reported as MCP progress notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Searching,
    Reading,
    Writing,
}

impl Phase {
    pub const COUNT: u32 = 3;

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Searching => "searching",
            Phase::Reading => "reading",
            Phase::Writing => "writing",
        }
    }

    /// 1-based position, for the progress value.
    pub fn step(self) -> u32 {
        match self {
            Phase::Searching => 1,
            Phase::Reading => 2,
            Phase::Writing => 3,
        }
    }
}

/// Where phase changes are reported.
#[async_trait]
pub trait ProgressSink: Send + Sync {
    async fn phase(&self, phase: Phase);
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResearchFailure {
    #[error("the research model failed: {0}")]
    Model(ModelError),
    #[error("every search failed: {0}")]
    Search(String),
    #[error("no search result could be read")]
    NothingRead,
}

/// Queries from the planning model's answer: one per line, list markers and
/// quotes removed, duplicates dropped, at most `max`. Falls back to the
/// request itself when the answer holds none.
pub fn parse_queries(answer: &str, request: &str, max: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let queries: Vec<String> = strip_reasoning(answer)
        .lines()
        .map(|line| {
            line.trim()
                .trim_start_matches(|character: char| {
                    character.is_ascii_digit() || matches!(character, '.' | ')' | '-' | '*' | '•')
                })
                .trim()
                .trim_matches('"')
                .trim()
                .to_string()
        })
        .filter(|query| !query.is_empty())
        .filter(|query| seen.insert(query.to_lowercase()))
        .take(max)
        .collect();
    if queries.is_empty() {
        vec![request.trim().to_string()]
    } else {
        queries
    }
}

/// Up to `max` distinct results, taken round-robin across the queries'
/// result lists so every query contributes its best hits first.
pub fn select_sources(result_lists: &[Vec<SearchHit>], max: usize) -> Vec<SearchHit> {
    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    let longest = result_lists.iter().map(Vec::len).max().unwrap_or(0);
    for rank in 0..longest {
        for hit in result_lists.iter().filter_map(|results| results.get(rank)) {
            if selected.len() == max {
                return selected;
            }
            if seen.insert(hit.url.trim_end_matches('/').to_string()) {
                selected.push(hit.clone());
            }
        }
    }
    selected
}

/// The writing model's prompt: the request and the numbered sources.
pub fn report_prompt(request: &str, sources: &[(ResearchSource, String)]) -> String {
    let numbered: String = sources
        .iter()
        .enumerate()
        .map(|(index, (source, text))| {
            let excerpt: String = text.chars().take(SOURCE_EXCERPT_CHARS).collect();
            format!(
                "[{}] {}\nURL: {}\n{}\n\n",
                index + 1,
                source.title,
                source.url,
                excerpt
            )
        })
        .collect();
    format!("Research request: {request}\n\nSources:\n\n{numbered}Write the report.")
}

/// The report with citations to sources that do not exist removed.
pub fn drop_unknown_citations(report: &str, source_count: usize) -> String {
    let mut cleaned = String::with_capacity(report.len());
    let mut rest = report;
    while let Some(open) = rest.find('[') {
        cleaned.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        let closes = !digits.is_empty() && after[digits.len()..].starts_with(']');
        let number: usize = digits.parse().unwrap_or(0);
        if closes && (number == 0 || number > source_count) {
            rest = &after[digits.len() + 1..];
        } else {
            cleaned.push('[');
            rest = after;
        }
    }
    cleaned.push_str(rest);
    cleaned
}

/// The ports one research call uses.
pub struct Researcher {
    pub search_engine: Arc<dyn SearchEngine>,
    pub search_cache: Arc<SearchCache>,
    pub reader: Arc<PageReader>,
    pub model: Arc<dyn ChatModel>,
}

impl Researcher {
    pub async fn research(
        &self,
        request: &str,
        model: ResearchModel,
        progress: &dyn ProgressSink,
    ) -> Result<ResearchResult, ResearchFailure> {
        let limits = limits(model);

        progress.phase(Phase::Searching).await;
        let plan = self
            .model
            .complete(
                limits.tier,
                PLAN_SYSTEM_PROMPT,
                &format!(
                    "Research request: {request}\n\nWrite at most {} search queries.",
                    limits.searches
                ),
            )
            .await
            .map_err(ResearchFailure::Model)?;
        let queries = parse_queries(&plan, request, limits.searches);
        let searches = futures::future::join_all(queries.iter().map(|query| {
            let search = SearchRequest {
                query: query.clone(),
                topic: Topic::General,
                time_range: None,
                days: DEFAULT_NEWS_DAYS,
                start_date: None,
                end_date: None,
            };
            async move {
                run_search(
                    self.search_engine.as_ref(),
                    &self.search_cache,
                    &search,
                    Instant::now(),
                )
                .await
            }
        }))
        .await;
        let mut result_lists = Vec::new();
        let mut last_error = None;
        for search in searches {
            match search {
                Ok(outcome) => result_lists.push(outcome.response.results),
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        if result_lists.is_empty() {
            return Err(ResearchFailure::Search(last_error.unwrap_or_default()));
        }
        let candidates = select_sources(&result_lists, limits.sources);

        progress.phase(Phase::Reading).await;
        let urls: Vec<String> = candidates.iter().map(|hit| hit.url.clone()).collect();
        let (read, _) = extract_urls(&self.reader, &urls).await;
        let sources: Vec<(ResearchSource, String)> = read
            .extracted
            .into_iter()
            .map(|page| {
                let title = candidates
                    .iter()
                    .find(|hit| hit.url == page.url)
                    .map(|hit| hit.title.clone())
                    .unwrap_or_default();
                (
                    ResearchSource {
                        url: page.url,
                        title,
                        favicon: None,
                    },
                    page.text,
                )
            })
            .collect();
        if sources.is_empty() {
            return Err(ResearchFailure::NothingRead);
        }

        progress.phase(Phase::Writing).await;
        let report = self
            .model
            .complete(
                limits.tier,
                REPORT_SYSTEM_PROMPT,
                &report_prompt(request, &sources),
            )
            .await
            .map_err(ResearchFailure::Model)?;
        Ok(ResearchResult {
            content: drop_unknown_citations(strip_reasoning(&report), sources.len()),
            sources: sources.into_iter().map(|(source, _)| source).collect(),
        })
    }
}
