//! `extract`: reads pages as readable text. Stage one is a plain HTTP GET;
//! a page whose HTML holds no readable body is reported as such.

use std::sync::Arc;

use futures::StreamExt;
use tracing::{field, Instrument};
use url::Url;

use crate::address_policy::{check_url, Refusal};
use crate::contract::{ExtractResponse, ExtractedPage, FailedUrl};
use crate::fetch::{FetchError, HttpFetcher};
use crate::html::{links, readable_text};

/// Bytes of a page read; the rest is dropped unread.
pub const MAX_PAGE_BYTES: usize = 5 * 1024 * 1024;
/// Below this much readable text a page counts as having no readable body.
pub const MIN_READABLE_CHARS: usize = 200;
/// Pages read at once by one `extract` call.
pub const EXTRACT_CONCURRENCY: usize = 4;
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

/// Which stage produced a page's text, recorded per URL on the tool span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractStage {
    Http,
    Failed,
}

impl ExtractStage {
    pub fn as_str(self) -> &'static str {
        match self {
            ExtractStage::Http => "http",
            ExtractStage::Failed => "failed",
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
}

impl From<FetchError> for ExtractFailure {
    fn from(error: FetchError) -> Self {
        match error {
            FetchError::Refused(refusal) => ExtractFailure::Refused(refusal),
            FetchError::Timeout => ExtractFailure::Timeout,
            FetchError::Network(message) => ExtractFailure::Network(message),
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

/// A page read to text, with the links it holds (for crawling).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub url: Url,
    pub text: String,
    pub links: Vec<Url>,
    pub stage: ExtractStage,
}

/// Reads pages through the address policy.
pub struct PageReader {
    http: Arc<dyn HttpFetcher>,
}

impl PageReader {
    pub fn new(http: Arc<dyn HttpFetcher>) -> Self {
        Self { http }
    }

    /// Reads `url` to text, recording which stage answered on an
    /// `extract.page` span.
    pub async fn read(&self, url: &str) -> Result<Page, ExtractFailure> {
        let span = tracing::info_span!(
            "extract.page",
            url.full = %url,
            extract.stage = field::Empty,
            extract.error = field::Empty,
        );
        let result = self.read_unrecorded(url).instrument(span.clone()).await;
        match &result {
            Ok(page) => span.record("extract.stage", page.stage.as_str()),
            Err(failure) => {
                span.record("extract.error", failure.to_string());
                span.record("extract.stage", ExtractStage::Failed.as_str())
            }
        };
        result
    }

    async fn read_unrecorded(&self, url: &str) -> Result<Page, ExtractFailure> {
        let url = Url::parse(url.trim())
            .map_err(|error| ExtractFailure::InvalidUrl(error.to_string()))?;
        check_url(&url).map_err(ExtractFailure::Refused)?;
        let fetched = self.http.get(&url, MAX_PAGE_BYTES).await?;
        if !(200..300).contains(&fetched.status) {
            return Err(ExtractFailure::HttpStatus(fetched.status));
        }
        let body = String::from_utf8_lossy(&fetched.body);
        match content_kind(&fetched.content_type) {
            ContentKind::Unsupported => Err(ExtractFailure::UnsupportedContentType(media_type(
                &fetched.content_type,
            ))),
            ContentKind::PlainText => Ok(Page {
                text: body.trim().to_string(),
                links: Vec::new(),
                url: fetched.final_url,
                stage: ExtractStage::Http,
            }),
            ContentKind::Html => {
                let text = readable_text(&body);
                if text.is_empty() {
                    return Err(ExtractFailure::NoReadableText);
                }
                Ok(Page {
                    links: links(&body, &fetched.final_url),
                    text,
                    url: fetched.final_url,
                    stage: ExtractStage::Http,
                })
            }
        }
    }
}

/// How many URLs each stage answered, for the tool span.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageCounts {
    pub http: usize,
    pub failed: usize,
}

/// Reads `urls` concurrently, keeping the caller's order within
/// `extracted` and `failed`.
pub async fn extract_urls(reader: &PageReader, urls: &[String]) -> (ExtractResponse, StageCounts) {
    let outcomes: Vec<(String, Result<Page, ExtractFailure>)> =
        futures::stream::iter(urls.iter().cloned())
            .map(|url: String| async move {
                let outcome = reader.read(&url).await;
                (url, outcome)
            })
            .buffered(EXTRACT_CONCURRENCY)
            .collect()
            .await;
    let mut response = ExtractResponse {
        extracted: Vec::new(),
        failed: Vec::new(),
    };
    let mut counts = StageCounts::default();
    for (requested_url, outcome) in outcomes {
        match outcome {
            Ok(page) => {
                counts.http += 1;
                response.extracted.push(ExtractedPage {
                    text: page.text,
                    url: requested_url,
                    favicon: String::new(),
                });
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
    (response, counts)
}
