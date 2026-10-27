//! `extract`: reads pages as readable text in two stages. Stage one is a
//! plain HTTP GET; only when its HTML holds no readable body (empty,
//! boilerplate only, or a JavaScript app shell) is the page rendered in a
//! browser (stage two) and read again.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use tracing::{field, Instrument};
use url::Url;

use crate::address_policy::{check_url, loggable_url, Refusal};
use crate::contract::{ExtractResponse, ExtractedPage, FailedUrl};
use crate::fetch::{get_following_redirects, AnyHop, FetchError, HopGate, HttpFetcher};
use crate::html::{links, readable_text};

/// Bytes of a page read; the rest is dropped unread.
pub const MAX_PAGE_BYTES: usize = 5 * 1024 * 1024;
/// Below this much readable text a page counts as having no readable body.
pub const MIN_READABLE_CHARS: usize = 200;
/// Pages read at once by one `extract` call.
pub const EXTRACT_CONCURRENCY: usize = 4;
/// URLs one `extract` call reads; with the per-page byte cap this bounds a
/// call's work and memory. Later URLs are reported as not read.
pub const MAX_EXTRACT_URLS: usize = 50;
/// Page text one `extract` call returns across all URLs; the page that
/// reaches it is cut and the URLs after it are reported as not read.
pub const MAX_EXTRACT_TEXT_BYTES: usize = 20 * 1024 * 1024;
/// Phrases a JavaScript app shell shows instead of content.
const SCRIPT_REQUIRED_PHRASES: &[&str] = &[
    "enable javascript",
    "javascript is required",
    "requires javascript",
    "javascript is disabled",
    "turn on javascript",
];
/// Text up to this long that asks for JavaScript is a shell, not an article
/// that happens to mention it.
const SHELL_TEXT_MAX_CHARS: usize = 1_000;

/// Which stage produced a page's text, recorded per URL on its span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractStage {
    Http,
    Render,
}

/// `extract.stage` of a URL that yielded no text.
pub const FAILED_STAGE: &str = "failed";

impl ExtractStage {
    pub fn as_str(self) -> &'static str {
        match self {
            ExtractStage::Http => "http",
            ExtractStage::Render => "render",
        }
    }
}

/// Why a URL yielded no text; the message is shown to the caller.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtractFailure {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("refused: {0}")]
    Refused(Refusal),
    #[error("HTTP status {0}")]
    HttpStatus(u16),
    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),
    #[error("no readable text")]
    NoReadableText,
    #[error("timed out")]
    Timeout,
    #[error("network error: {0}")]
    Network(String),
    /// A redirect the caller's rules refused (for a crawl, robots.txt).
    #[error("{0}")]
    HopRefused(String),
    #[error("no readable text without rendering, and rendering failed: {0}")]
    RenderFailed(RenderError),
    #[error("not read: at most {MAX_EXTRACT_URLS} URLs per call")]
    OverUrlLimit,
    #[error("not read: the call's text budget is spent")]
    TextBudgetSpent,
}

impl From<FetchError> for ExtractFailure {
    fn from(error: FetchError) -> Self {
        match error {
            FetchError::Refused(refusal) => ExtractFailure::Refused(refusal),
            FetchError::Timeout => ExtractFailure::Timeout,
            FetchError::Network(message) => ExtractFailure::Network(message),
            FetchError::HopRefused(reason) => ExtractFailure::HopRefused(reason),
        }
    }
}

/// What a response's content type says about reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Html,
    PlainText,
    Unsupported,
}

/// The media type without parameters, lowercased.
pub fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// HTML is read as a page; other `text/*` as-is; anything else (PDF,
/// office documents, images) is not read. A missing type is sniffed as HTML.
pub fn content_kind(content_type: &str) -> ContentKind {
    match media_type(content_type).as_str() {
        "" | "text/html" | "application/xhtml+xml" => ContentKind::Html,
        essence if essence.starts_with("text/") => ContentKind::PlainText,
        _ => ContentKind::Unsupported,
    }
}

/// Whether stage one's text is no readable body: empty, boilerplate only,
/// or a JavaScript app shell asking for scripts.
pub fn needs_render(text: &str) -> bool {
    let length = text.chars().count();
    if length < MIN_READABLE_CHARS {
        return true;
    }
    let lowered = text.to_lowercase();
    length <= SHELL_TEXT_MAX_CHARS
        && SCRIPT_REQUIRED_PHRASES
            .iter()
            .any(|phrase| lowered.contains(phrase))
}

/// A page as the browser left it after scripts ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPage {
    pub final_url: Url,
    pub html: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("renderer unavailable: {0}")]
    Unavailable(String),
    #[error("refused: {0}")]
    Refused(Refusal),
    #[error("timed out")]
    Timeout,
    /// The page itself answered with an error (often bot protection).
    #[error("HTTP status {0}")]
    HttpStatus(u16),
    /// A navigation the caller's rules refused (for a crawl, robots.txt).
    #[error("{0}")]
    HopRefused(String),
    #[error("browser error: {0}")]
    Browser(String),
}

/// Stage two: loads a page in a browser under the same address policy.
#[async_trait]
pub trait Renderer: Send + Sync {
    /// Loads `url`; every other document the page's main frame navigates to
    /// (a redirect, a script navigation) must pass `gate` first.
    async fn render(&self, url: &Url, gate: &dyn HopGate) -> Result<RenderedPage, RenderError>;
}

/// A page read to text, with the links it holds (for crawling).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub url: Url,
    pub text: String,
    pub links: Vec<Url>,
    pub stage: ExtractStage,
}

/// Reads pages through the address policy: HTTP first, the renderer (when
/// configured) for pages HTTP cannot read.
pub struct PageReader {
    http: Arc<dyn HttpFetcher>,
    renderer: Option<Arc<dyn Renderer>>,
}

impl PageReader {
    pub fn new(http: Arc<dyn HttpFetcher>, renderer: Option<Arc<dyn Renderer>>) -> Self {
        Self { http, renderer }
    }

    /// Reads `url` to text, recording which stage answered on an
    /// `extract.page` span.
    pub async fn read(&self, url: &str) -> Result<Page, ExtractFailure> {
        self.read_gated(url, &AnyHop).await
    }

    /// [`read`](Self::read), with every redirect and page navigation also
    /// passing `gate` before it is requested.
    pub async fn read_gated(&self, url: &str, gate: &dyn HopGate) -> Result<Page, ExtractFailure> {
        let span = tracing::info_span!(
            "extract.page",
            url.full = %loggable_url(url),
            extract.stage = field::Empty,
            extract.error = field::Empty,
            extract.render_error = field::Empty,
        );
        let result = self
            .read_unrecorded(url, gate)
            .instrument(span.clone())
            .await;
        match &result {
            Ok(page) => span.record("extract.stage", page.stage.as_str()),
            Err(failure) => {
                span.record("extract.error", failure.to_string());
                span.record("extract.stage", FAILED_STAGE)
            }
        };
        result
    }

    async fn read_unrecorded(&self, url: &str, gate: &dyn HopGate) -> Result<Page, ExtractFailure> {
        let url = Url::parse(url.trim())
            .map_err(|error| ExtractFailure::InvalidUrl(error.to_string()))?;
        check_url(&url).map_err(ExtractFailure::Refused)?;
        let fetched =
            get_following_redirects(self.http.as_ref(), &url, MAX_PAGE_BYTES, gate).await?;
        if !(200..300).contains(&fetched.status) {
            return Err(ExtractFailure::HttpStatus(fetched.status));
        }
        let body = String::from_utf8_lossy(&fetched.body);
        match content_kind(&fetched.content_type) {
            ContentKind::Unsupported => Err(ExtractFailure::UnsupportedContentType(media_type(
                &fetched.content_type,
            ))),
            ContentKind::PlainText => non_empty(Page {
                text: body.trim().to_string(),
                links: Vec::new(),
                url: fetched.final_url,
                stage: ExtractStage::Http,
            }),
            ContentKind::Html => {
                let http_page = Page {
                    text: readable_text(&body),
                    links: links(&body, &fetched.final_url),
                    url: fetched.final_url,
                    stage: ExtractStage::Http,
                };
                if !needs_render(&http_page.text) {
                    return Ok(http_page);
                }
                self.render_or_keep(http_page, gate).await
            }
        }
    }

    /// Stage two for a page HTTP could not read: the rendered page when it
    /// has more text, otherwise whatever HTTP found; no text at all is a
    /// failure.
    async fn render_or_keep(
        &self,
        http_page: Page,
        gate: &dyn HopGate,
    ) -> Result<Page, ExtractFailure> {
        let Some(renderer) = &self.renderer else {
            return non_empty(http_page);
        };
        match renderer.render(&http_page.url, gate).await {
            Ok(rendered) => {
                let text = readable_text(&rendered.html);
                if text.chars().count() > http_page.text.chars().count() {
                    Ok(Page {
                        links: links(&rendered.html, &rendered.final_url),
                        text,
                        url: rendered.final_url,
                        stage: ExtractStage::Render,
                    })
                } else {
                    non_empty(http_page)
                }
            }
            Err(error) => {
                tracing::Span::current().record("extract.render_error", error.to_string());
                if http_page.text.is_empty() {
                    Err(ExtractFailure::RenderFailed(error))
                } else {
                    Ok(http_page)
                }
            }
        }
    }
}

fn non_empty(page: Page) -> Result<Page, ExtractFailure> {
    if page.text.is_empty() {
        Err(ExtractFailure::NoReadableText)
    } else {
        Ok(page)
    }
}

/// How many URLs each stage answered, for the tool span.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageCounts {
    pub http: usize,
    pub render: usize,
    pub failed: usize,
}

/// Cuts `text` to at most `bytes` bytes, at a character boundary, and frees
/// what the cut leaves unused: `String::truncate` alone keeps the whole
/// original buffer.
pub(crate) fn keep_bytes(text: &mut String, bytes: usize) {
    if text.len() > bytes {
        let mut end = bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    if text.capacity() > text.len() {
        *text = text.as_str().to_owned();
    }
}

/// [`extract_urls_within`] with [`MAX_EXTRACT_TEXT_BYTES`].
pub async fn extract_urls(reader: &PageReader, urls: &[String]) -> (ExtractResponse, StageCounts) {
    extract_urls_within(reader, urls, MAX_EXTRACT_TEXT_BYTES).await
}

/// Reads the first [`MAX_EXTRACT_URLS`] of `urls` concurrently, keeping the
/// caller's order within `extracted` and `failed`, and at most
/// `text_budget_bytes` of text across them: the page that reaches the budget
/// is cut, pages still being read are cancelled, and every URL not read
/// (past the URL limit or the budget) is reported as such.
pub async fn extract_urls_within(
    reader: &PageReader,
    urls: &[String],
    text_budget_bytes: usize,
) -> (ExtractResponse, StageCounts) {
    let (to_read, over_limit) = urls.split_at(urls.len().min(MAX_EXTRACT_URLS));
    let mut response = ExtractResponse {
        extracted: Vec::new(),
        failed: Vec::new(),
    };
    let mut counts = StageCounts::default();
    let mut kept_bytes = 0;
    let mut answered = 0;
    let mut outcomes = futures::stream::iter(to_read.iter().cloned())
        .map(|url: String| async move {
            let outcome = reader.read(&url).await;
            (url, outcome)
        })
        .buffered(EXTRACT_CONCURRENCY);
    while let Some((requested_url, outcome)) = outcomes.next().await {
        answered += 1;
        match outcome {
            Ok(page) => {
                match page.stage {
                    ExtractStage::Http => counts.http += 1,
                    ExtractStage::Render => counts.render += 1,
                }
                // Only the text is kept; the page's links go with `page`.
                let mut text = page.text;
                let budget_left = text_budget_bytes.saturating_sub(kept_bytes);
                let budget_spent = text.len() >= budget_left;
                keep_bytes(&mut text, budget_left);
                kept_bytes += text.capacity();
                response.extracted.push(ExtractedPage {
                    text,
                    url: requested_url,
                    favicon: String::new(),
                });
                if budget_spent {
                    break;
                }
            }
            Err(failure) => {
                counts.failed += 1;
                response.failed.push(FailedUrl {
                    url: requested_url,
                    error: failure.to_string(),
                });
            }
        }
    }
    // Dropping the stream cancels reads still in flight.
    drop(outcomes);
    let not_read = to_read[answered..]
        .iter()
        .map(|url| (url, ExtractFailure::TextBudgetSpent))
        .chain(
            over_limit
                .iter()
                .map(|url| (url, ExtractFailure::OverUrlLimit)),
        );
    for (url, failure) in not_read {
        counts.failed += 1;
        response.failed.push(FailedUrl {
            url: url.clone(),
            error: failure.to_string(),
        });
    }
    (response, counts)
}
