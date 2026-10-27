//! `crawl` and `crawl_company_website`: one bounded breadth-first crawler.
//!
//! Pages are read through the `extract` reader (both stages). The crawl
//! stays on the start page's host unless external links are allowed, takes
//! at most `max_breadth` new links per page, goes `max_depth` links deep,
//! reads at most `limit` pages, honours `robots.txt` disallow rules, holds
//! at most one request per host in flight, and stops at a wall-clock
//! deadline with what it has read so far.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regex::Regex;
use texting_robots::Robot;
use tokio::time::Instant;
use url::Url;

use crate::address_policy::check_url;
use crate::contract::{ExtractResponse, ExtractedPage, FailedUrl};
use crate::extract::{ExtractFailure, Page, PageReader};
use crate::fetch::HttpFetcher;
use crate::model::{parse_yes_no, ChatModel, ModelError, ModelTier};

/// Characters of each page's text returned unless `full_content` is set.
pub const TRUNCATED_TEXT_CHARS: usize = 5_000;
/// Pages read by `crawl`, which has no `limit` argument.
pub const DEFAULT_CRAWL_LIMIT: usize = 50;
/// The crawl deadline grows with the pages it may read, between a floor
/// that lets small crawls finish and a ceiling that bounds a tool call.
const CRAWL_TIMEOUT_FLOOR: Duration = Duration::from_secs(60);
const CRAWL_TIMEOUT_CEILING: Duration = Duration::from_secs(300);
const CRAWL_SECONDS_PER_PAGE: u64 = 3;
/// The token `robots.txt` groups are matched against.
pub const ROBOTS_AGENT: &str = "inxm-websearch";
const ROBOTS_PATH: &str = "/robots.txt";
const MAX_ROBOTS_BYTES: usize = 512 * 1024;
/// Characters of a page shown to the model when scoring relevance.
const RELEVANCE_EXCERPT_CHARS: usize = 4_000;
/// Pages scored at once.
const SCORING_CONCURRENCY: usize = 4;
/// Links to files the reader would refuse anyway; skipping them saves the
/// crawl budget for pages.
const NON_PAGE_EXTENSIONS: &[&str] = &[
    ".pdf", ".zip", ".gz", ".tar", ".rar", ".7z", ".exe", ".dmg", ".msi", ".jpg", ".jpeg", ".png",
    ".gif", ".webp", ".svg", ".ico", ".mp3", ".mp4", ".mov", ".avi", ".webm", ".woff", ".woff2",
    ".ttf", ".css", ".js", ".json", ".xml", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx",
];
const WWW_PREFIX: &str = "www.";
const ROBOTS_DISALLOWED: &str = "disallowed by robots.txt";
const ROBOTS_UNAVAILABLE: &str = "robots.txt could not be read; the site is not crawled";
const CRAWL_TIMED_OUT: &str = "crawl timed out";

const RELEVANCE_SYSTEM_PROMPT: &str = "You decide whether a web page is relevant to a crawl's \
    instructions. Answer with exactly one word: yes or no.";

/// What to crawl and how far.
#[derive(Debug, Clone)]
pub struct CrawlPlan {
    pub start_url: String,
    pub instructions: Option<String>,
    pub select_paths: Vec<Regex>,
    pub exclude_paths: Vec<Regex>,
    pub allow_external: bool,
    pub full_content: bool,
    pub max_breadth: usize,
    pub max_depth: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CrawlError {
    #[error("`{pattern}` is not a valid path pattern: {reason}")]
    InvalidPathPattern { pattern: String, reason: String },
}

/// Compiles `select_paths` / `exclude_paths` regexes.
pub fn compile_path_patterns(patterns: &[String]) -> Result<Vec<Regex>, CrawlError> {
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern).map_err(|error| CrawlError::InvalidPathPattern {
                pattern: pattern.clone(),
                reason: error.to_string(),
            })
        })
        .collect()
}

/// The wall-clock budget for a crawl of this size.
pub fn crawl_timeout(max_breadth: usize, max_depth: usize) -> Duration {
    let pages = u64::try_from(max_breadth.saturating_mul(max_depth)).unwrap_or(u64::MAX);
    Duration::from_secs(pages.saturating_mul(CRAWL_SECONDS_PER_PAGE))
        .clamp(CRAWL_TIMEOUT_FLOOR, CRAWL_TIMEOUT_CEILING)
}

/// A host without its `www.` prefix, so `inxm.ai` and `www.inxm.ai` count
/// as one site.
pub fn site_of(url: &Url) -> Option<String> {
    url.host_str().map(|host| {
        host.strip_prefix(WWW_PREFIX)
            .unwrap_or(host)
            .to_ascii_lowercase()
    })
}

fn looks_like_a_file(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    NON_PAGE_EXTENSIONS
        .iter()
        .any(|extension| path.ends_with(extension))
}

/// Whether a link's path passes the caller's path filters: it matches a
/// `select_paths` pattern (when any are given) and no `exclude_paths` one.
pub fn path_allowed(url: &Url, select_paths: &[Regex], exclude_paths: &[Regex]) -> bool {
    let path = url.path();
    let selected =
        select_paths.is_empty() || select_paths.iter().any(|pattern| pattern.is_match(path));
    selected && !exclude_paths.iter().any(|pattern| pattern.is_match(path))
}

/// The links of one page the crawl follows: on the start site (unless
/// external links are allowed), not files, passing the path filters, not
/// yet seen, at most `max_breadth` of them, in page order.
pub fn next_links(
    links: &[Url],
    plan: &CrawlPlan,
    start_site: &str,
    seen: &HashSet<String>,
) -> Vec<Url> {
    links
        .iter()
        .filter(|link| plan.allow_external || site_of(link).as_deref() == Some(start_site))
        .filter(|link| !looks_like_a_file(link))
        .filter(|link| path_allowed(link, &plan.select_paths, &plan.exclude_paths))
        .filter(|link| !seen.contains(link.as_str()))
        .take(plan.max_breadth)
        .cloned()
        .collect()
}

/// At most one request per host in flight, across every crawl this server
/// runs.
#[derive(Default)]
pub struct HostGate {
    hosts: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl HostGate {
    async fn hold(&self, host: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut hosts = self
                .hosts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(hosts.entry(host.to_string()).or_default())
        };
        lock.lock_owned().await
    }
}

/// What `robots.txt` says about one origin.
enum RobotsRules {
    AllowAll,
    DisallowAll,
    Rules(Robot),
}

impl RobotsRules {
    fn allows(&self, url: &Url) -> bool {
        match self {
            RobotsRules::AllowAll => true,
            RobotsRules::DisallowAll => false,
            RobotsRules::Rules(robot) => robot.allowed(url.as_str()),
        }
    }
}

/// The robots rules for `url`'s origin (RFC 9309): a missing file (4xx)
/// allows everything; an unreachable one (5xx, network) disallows
/// everything.
async fn robots_rules(http: &dyn HttpFetcher, url: &Url) -> RobotsRules {
    let mut robots_url = url.clone();
    robots_url.set_path(ROBOTS_PATH);
    robots_url.set_query(None);
    robots_url.set_fragment(None);
    match http.get(&robots_url, MAX_ROBOTS_BYTES).await {
        Ok(fetched) if (200..300).contains(&fetched.status) => {
            match Robot::new(ROBOTS_AGENT, &fetched.body) {
                Ok(robot) => RobotsRules::Rules(robot),
                Err(error) => {
                    tracing::debug!(error = %error, "unparseable robots.txt; allowing all");
                    RobotsRules::AllowAll
                }
            }
        }
        Ok(fetched) if (400..500).contains(&fetched.status) => RobotsRules::AllowAll,
        Ok(_) | Err(_) => RobotsRules::DisallowAll,
    }
}

/// The yes/no question asked about one page.
pub fn relevance_question(instructions: &str, url: &str, text: &str) -> String {
    let excerpt: String = text.chars().take(RELEVANCE_EXCERPT_CHARS).collect();
    format!(
        "Crawl instructions: {instructions}\n\nPage URL: {url}\n\nPage text:\n{excerpt}\n\n\
         Is this page relevant to the crawl instructions? Answer yes or no."
    )
}

/// Scores a page once with the fast model; an answer that is neither yes
/// nor no is reported as malformed.
pub async fn is_relevant(
    model: &dyn ChatModel,
    instructions: &str,
    url: &str,
    text: &str,
) -> Result<bool, ModelError> {
    let answer = model
        .complete(
            ModelTier::Fast,
            RELEVANCE_SYSTEM_PROMPT,
            &relevance_question(instructions, url, text),
        )
        .await?;
    parse_yes_no(&answer).ok_or_else(|| ModelError::Malformed(format!("not yes/no: {answer}")))
}

fn to_extracted(page: &Page, full_content: bool) -> ExtractedPage {
    ExtractedPage {
        text: truncate(&page.text, full_content),
        url: page.url.to_string(),
        favicon: String::new(),
    }
}

fn truncate(text: &str, full_content: bool) -> String {
    if full_content {
        text.to_string()
    } else {
        text.chars().take(TRUNCATED_TEXT_CHARS).collect()
    }
}

/// A crawl's result plus what telemetry records about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlOutcome {
    pub response: ExtractResponse,
    pub pages_read: usize,
    pub timed_out: bool,
    /// Pages the model scored as not relevant (dropped).
    pub scored_irrelevant: usize,
    /// Pages kept unscored because the model failed.
    pub scoring_failures: usize,
}

/// The crawler's ports and shared state.
pub struct Crawler {
    pub reader: Arc<PageReader>,
    pub http: Arc<dyn HttpFetcher>,
    pub model: Arc<dyn ChatModel>,
    pub gate: Arc<HostGate>,
}

impl Crawler {
    /// Crawls breadth-first from `plan.start_url` until the frontier, the
    /// page limit or `deadline` runs out.
    pub async fn crawl(&self, plan: &CrawlPlan, deadline: Instant) -> CrawlOutcome {
        let mut failed: Vec<FailedUrl> = Vec::new();
        let mut pages: Vec<Page> = Vec::new();
        let mut timed_out = false;
        let mut pages_read = 0;
        let start = match Url::parse(plan.start_url.trim()) {
            Ok(start) => start,
            Err(error) => {
                failed.push(FailedUrl {
                    url: plan.start_url.clone(),
                    error: ExtractFailure::InvalidUrl(error.to_string()).to_string(),
                });
                return CrawlOutcome {
                    response: ExtractResponse {
                        extracted: Vec::new(),
                        failed,
                    },
                    pages_read,
                    timed_out,
                    scored_irrelevant: 0,
                    scoring_failures: 0,
                };
            }
        };
        let mut seen: HashSet<String> = HashSet::from([start.to_string()]);
        let mut queue: VecDeque<(Url, usize)> = VecDeque::from([(start, 0)]);
        let mut robots_by_origin: HashMap<String, RobotsRules> = HashMap::new();
        let mut start_site: Option<String> = None;

        while let Some((url, depth)) = queue.pop_front() {
            if pages_read >= plan.limit {
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            if let Err(refusal) = check_url(&url) {
                failed.push(FailedUrl {
                    url: url.to_string(),
                    error: ExtractFailure::Refused(refusal).to_string(),
                });
                continue;
            }
            let origin = url.origin().ascii_serialization();
            let host = url.host_str().unwrap_or_default().to_string();
            if !robots_by_origin.contains_key(&origin) {
                let _host_turn = self.gate.hold(&host).await;
                let rules = robots_rules(self.http.as_ref(), &url).await;
                robots_by_origin.insert(origin.clone(), rules);
            }
            let robots_refusal = match robots_by_origin.get(&origin) {
                Some(RobotsRules::DisallowAll) => Some(ROBOTS_UNAVAILABLE),
                Some(rules) if !rules.allows(&url) => Some(ROBOTS_DISALLOWED),
                _ => None,
            };
            if let Some(reason) = robots_refusal {
                failed.push(FailedUrl {
                    url: url.to_string(),
                    error: reason.to_string(),
                });
                continue;
            }

            pages_read += 1;
            let read = {
                let _host_turn = self.gate.hold(&host).await;
                tokio::time::timeout_at(deadline, self.reader.read(url.as_str())).await
            };
            let page = match read {
                Err(_elapsed) => {
                    timed_out = true;
                    failed.push(FailedUrl {
                        url: url.to_string(),
                        error: CRAWL_TIMED_OUT.to_string(),
                    });
                    break;
                }
                Ok(Err(failure)) => {
                    failed.push(FailedUrl {
                        url: url.to_string(),
                        error: failure.to_string(),
                    });
                    continue;
                }
                Ok(Ok(page)) => page,
            };
            seen.insert(page.url.to_string());
            let site = start_site
                .get_or_insert_with(|| site_of(&page.url).unwrap_or_default())
                .clone();
            if depth < plan.max_depth {
                for link in next_links(&page.links, plan, &site, &seen) {
                    seen.insert(link.to_string());
                    queue.push_back((link, depth + 1));
                }
            }
            pages.push(page);
        }

        let (extracted, scored_irrelevant, scoring_failures) = self.score(plan, pages).await;
        CrawlOutcome {
            response: ExtractResponse { extracted, failed },
            pages_read,
            timed_out,
            scored_irrelevant,
            scoring_failures,
        }
    }

    /// Drops pages the model scores as not relevant to the instructions;
    /// without instructions every page is kept. A page the model could not
    /// score is kept and counted.
    async fn score(
        &self,
        plan: &CrawlPlan,
        pages: Vec<Page>,
    ) -> (Vec<ExtractedPage>, usize, usize) {
        let Some(instructions) = plan
            .instructions
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        else {
            let extracted = pages
                .iter()
                .map(|page| to_extracted(page, plan.full_content))
                .collect();
            return (extracted, 0, 0);
        };
        let mut verdicts: Vec<Result<bool, ModelError>> = Vec::with_capacity(pages.len());
        for batch in pages.chunks(SCORING_CONCURRENCY) {
            let scored = futures::future::join_all(batch.iter().map(|page| {
                is_relevant(
                    self.model.as_ref(),
                    instructions,
                    page.url.as_str(),
                    &page.text,
                )
            }))
            .await;
            verdicts.extend(scored);
        }
        let mut extracted = Vec::new();
        let mut irrelevant = 0;
        let mut failures = 0;
        for (page, verdict) in pages.iter().zip(verdicts) {
            match verdict {
                Ok(true) => extracted.push(to_extracted(page, plan.full_content)),
                Ok(false) => irrelevant += 1,
                Err(error) => {
                    tracing::warn!(url = %page.url, error = %error, "relevance scoring failed; page kept");
                    failures += 1;
                    extracted.push(to_extracted(page, plan.full_content));
                }
            }
        }
        (extracted, irrelevant, failures)
    }
}
