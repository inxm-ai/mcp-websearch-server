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

use async_trait::async_trait;
use regex::Regex;
use texting_robots::Robot;
use tokio::time::Instant;
use url::Url;

use crate::address_policy::check_url;
use crate::contract::{ExtractResponse, ExtractedPage, FailedUrl};
use crate::extract::{ExtractFailure, Page, PageReader};
use crate::fetch::{get_following_redirects, AnyHop, HopGate, HttpFetcher};
use crate::model::{parse_yes_no, ChatModel, ModelError, ModelTier};

/// Characters of each page's text returned unless `full_content` is set.
pub const TRUNCATED_TEXT_CHARS: usize = 5_000;
/// Pages read by `crawl`, which has no `limit` argument.
pub const DEFAULT_CRAWL_LIMIT: usize = 50;
/// Ceilings on the caller's bounds; larger requests are clamped to them.
pub const MAX_CRAWL_BREADTH: usize = 50;
pub const MAX_CRAWL_DEPTH: usize = 5;
pub const MAX_CRAWL_LIMIT: usize = 200;
/// Page text one crawl keeps across all pages. With pages cut to 5,000
/// characters it is never reached; it bounds `full_content` crawls, whose
/// pages can each hold megabytes.
pub const MAX_CRAWL_TEXT_BYTES: usize = 20 * 1024 * 1024;
/// With `instructions`, this share of the crawl's time is kept for scoring.
const SCORING_SHARE_DIVISOR: u32 = 4;
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
const TEXT_BUDGET_SPENT: &str = "not read: the crawl's text budget is spent";

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
    /// Most bytes of page text the crawl keeps; reaching it ends the crawl.
    pub text_budget_bytes: usize,
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

/// A caller's count argument as a bound between 1 and `max`.
pub fn bounded_count(value: u32, max: usize) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX).clamp(1, max)
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
    match get_following_redirects(http, &robots_url, MAX_ROBOTS_BYTES, &AnyHop).await {
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

/// The crawl's robots.txt rules, fetched once per origin. It is also the
/// crawl's [`HopGate`], so a redirect or a rendered page's navigation into a
/// disallowed path is refused before it is requested, not after.
struct RobotsGate<'a> {
    http: &'a dyn HttpFetcher,
    rules: Mutex<HashMap<String, Arc<RobotsRules>>>,
}

impl<'a> RobotsGate<'a> {
    fn new(http: &'a dyn HttpFetcher) -> Self {
        Self {
            http,
            rules: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, url: &Url) -> Option<Arc<RobotsRules>> {
        self.rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&url.origin().ascii_serialization())
            .cloned()
    }

    /// The rules for `url`'s origin, fetching robots.txt on first use.
    async fn rules_for(&self, url: &Url) -> Arc<RobotsRules> {
        if let Some(rules) = self.cached(url) {
            return rules;
        }
        let rules = Arc::new(robots_rules(self.http, url).await);
        self.rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(url.origin().ascii_serialization(), Arc::clone(&rules));
        rules
    }
}

/// Why robots.txt keeps `url` from being crawled, if it does.
fn robots_refusal(rules: &RobotsRules, url: &Url) -> Option<&'static str> {
    match rules {
        RobotsRules::DisallowAll => Some(ROBOTS_UNAVAILABLE),
        rules if !rules.allows(url) => Some(ROBOTS_DISALLOWED),
        _ => None,
    }
}

#[async_trait]
impl HopGate for RobotsGate<'_> {
    async fn allow(&self, url: &Url) -> Result<(), String> {
        let rules = self.rules_for(url).await;
        match robots_refusal(&rules, url) {
            Some(reason) => Err(reason.to_string()),
            None => Ok(()),
        }
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

/// Whether pages are scored against instructions.
fn scores_pages(plan: &CrawlPlan) -> bool {
    plan.instructions
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty())
}

/// What the crawl keeps of a page once its links are queued: its URL and
/// the text it returns, nothing else.
struct KeptPage {
    url: Url,
    text: String,
}

fn to_extracted(page: KeptPage) -> ExtractedPage {
    ExtractedPage {
        text: page.text,
        url: page.url.to_string(),
        favicon: String::new(),
    }
}

/// Cuts `text` to its first `chars` characters, in place.
fn keep_chars(text: &mut String, chars: usize) {
    if let Some((end, _)) = text.char_indices().nth(chars) {
        text.truncate(end);
    }
}

/// Cuts `text` to at most `bytes` bytes, at a character boundary, in place.
fn keep_bytes(text: &mut String, bytes: usize) {
    if text.len() <= bytes {
        return;
    }
    let mut end = bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
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
    /// The text budget ran out and ended the crawl.
    pub text_budget_spent: bool,
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
        let mut pages: Vec<KeptPage> = Vec::new();
        let mut kept_bytes = 0;
        let mut text_budget_spent = false;
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
                    text_budget_spent,
                    scored_irrelevant: 0,
                    scoring_failures: 0,
                };
            }
        };
        // The site is the one the caller named: a start page that redirects
        // elsewhere does not move the crawl to that other site.
        let start_site = site_of(&start).unwrap_or_default();
        let fetch_deadline = if scores_pages(plan) {
            let budget = deadline.saturating_duration_since(Instant::now());
            deadline - budget / SCORING_SHARE_DIVISOR
        } else {
            deadline
        };
        let mut seen: HashSet<String> = HashSet::from([start.to_string()]);
        let mut queue: VecDeque<(Url, usize)> = VecDeque::from([(start, 0)]);
        let robots = RobotsGate::new(self.http.as_ref());

        while let Some((url, depth)) = queue.pop_front() {
            if pages_read >= plan.limit {
                break;
            }
            if Instant::now() >= fetch_deadline {
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
            let host = url.host_str().unwrap_or_default().to_string();
            // Fetch robots.txt for a new origin as this host's turn, within
            // the deadline; known rules answer at once.
            let rules = match robots.cached(&url) {
                Some(rules) => rules,
                None => {
                    let fetched = tokio::time::timeout_at(fetch_deadline, async {
                        let _host_turn = self.gate.hold(&host).await;
                        robots.rules_for(&url).await
                    })
                    .await;
                    let Ok(rules) = fetched else {
                        timed_out = true;
                        failed.push(FailedUrl {
                            url: url.to_string(),
                            error: CRAWL_TIMED_OUT.to_string(),
                        });
                        break;
                    };
                    rules
                }
            };
            if let Some(reason) = robots_refusal(&rules, &url) {
                failed.push(FailedUrl {
                    url: url.to_string(),
                    error: reason.to_string(),
                });
                continue;
            }

            pages_read += 1;
            let read = tokio::time::timeout_at(fetch_deadline, async {
                let _host_turn = self.gate.hold(&host).await;
                // Redirects and page navigations obey robots.txt too. A hop
                // to another host fetches that host's robots.txt without
                // its turn: taking a second host's turn here could deadlock.
                self.reader.read_gated(url.as_str(), &robots).await
            })
            .await;
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
            let Page {
                url: page_url,
                mut text,
                links,
                ..
            } = page;
            // Keep only what is returned: the cut text, within the budget.
            if !plan.full_content {
                keep_chars(&mut text, TRUNCATED_TEXT_CHARS);
            }
            let budget_left = plan.text_budget_bytes.saturating_sub(kept_bytes);
            text_budget_spent = text.len() >= budget_left;
            keep_bytes(&mut text, budget_left);
            // Cutting keeps the original buffer; copy the kept text into one
            // of its own size, and count what stays allocated.
            if text.capacity() > text.len() {
                text = text.as_str().to_owned();
            }
            kept_bytes += text.capacity();
            if !text_budget_spent && depth < plan.max_depth {
                // The frontier never holds more than the pages left to read.
                let room = plan.limit.saturating_sub(pages_read + queue.len());
                for link in next_links(&links, plan, &start_site, &seen)
                    .into_iter()
                    .take(room)
                {
                    seen.insert(link.to_string());
                    queue.push_back((link, depth + 1));
                }
            }
            drop(links);
            pages.push(KeptPage {
                url: page_url,
                text,
            });
            if text_budget_spent {
                failed.extend(queue.drain(..).map(|(url, _)| FailedUrl {
                    url: url.to_string(),
                    error: TEXT_BUDGET_SPENT.to_string(),
                }));
                break;
            }
        }

        let (extracted, scored_irrelevant, scoring_failures, scoring_timed_out) =
            self.score(plan, pages, deadline).await;
        CrawlOutcome {
            response: ExtractResponse { extracted, failed },
            pages_read,
            timed_out: timed_out || scoring_timed_out,
            scored_irrelevant,
            scoring_failures,
            text_budget_spent,
        }
    }

    /// Drops pages the model scores as not relevant to the instructions;
    /// without instructions every page is kept. A page the model could not
    /// score, or that the deadline left unscored, is kept and counted.
    /// Returns the pages kept, how many were dropped, how many went
    /// unscored, and whether the deadline cut scoring short.
    async fn score(
        &self,
        plan: &CrawlPlan,
        pages: Vec<KeptPage>,
        deadline: Instant,
    ) -> (Vec<ExtractedPage>, usize, usize, bool) {
        let Some(instructions) = plan
            .instructions
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        else {
            let extracted = pages.into_iter().map(to_extracted).collect();
            return (extracted, 0, 0, false);
        };
        // `None`: the deadline came before this page was scored.
        let mut verdicts: Vec<Option<Result<bool, ModelError>>> = Vec::with_capacity(pages.len());
        let mut timed_out = false;
        for batch in pages.chunks(SCORING_CONCURRENCY) {
            let scored = tokio::time::timeout_at(
                deadline,
                futures::future::join_all(batch.iter().map(|page| {
                    is_relevant(
                        self.model.as_ref(),
                        instructions,
                        page.url.as_str(),
                        &page.text,
                    )
                })),
            )
            .await;
            match scored {
                Ok(scored) => verdicts.extend(scored.into_iter().map(Some)),
                Err(_elapsed) => {
                    timed_out = true;
                    break;
                }
            }
        }
        verdicts.resize_with(pages.len(), || None);
        let mut extracted = Vec::new();
        let mut irrelevant = 0;
        let mut failures = 0;
        for (page, verdict) in pages.into_iter().zip(verdicts) {
            match verdict {
                Some(Ok(true)) => extracted.push(to_extracted(page)),
                Some(Ok(false)) => irrelevant += 1,
                Some(Err(error)) => {
                    tracing::warn!(url = %page.url, error = %error, "relevance scoring failed; page kept");
                    failures += 1;
                    extracted.push(to_extracted(page));
                }
                None => {
                    failures += 1;
                    extracted.push(to_extracted(page));
                }
            }
        }
        (extracted, irrelevant, failures, timed_out)
    }
}
