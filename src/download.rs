//! `download_file`: one file by URL, text as a string and binary as base64.

use base64::Engine;
use url::Url;

use crate::address_policy::{check_url, Refusal};
use crate::contract::DownloadedFile;
use crate::extract::media_type;
use crate::fetch::{FetchError, HttpFetcher};
use crate::tool_failure::FailureKind;

/// Largest file returned; a bigger one fails instead of arriving cut.
pub const MAX_DOWNLOAD_BYTES: usize = 10 * 1024 * 1024;
/// Reported when the server sends no content type.
pub const UNKNOWN_CONTENT_TYPE: &str = "application/octet-stream";
const TEXT_MEDIA_TYPES: &[&str] = &[
    "application/json",
    "application/xml",
    "application/javascript",
    "application/xhtml+xml",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DownloadFailure {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("refused: {0}")]
    Refused(Refusal),
    #[error("HTTP status {0}")]
    HttpStatus(u16),
    #[error("the file is larger than {MAX_DOWNLOAD_BYTES} bytes")]
    TooLarge,
    #[error(transparent)]
    Fetch(FetchError),
}

impl DownloadFailure {
    /// How the failure is reported: the caller's URL (invalid, refused, too
    /// large) is an argument problem; a server that answered badly or not
    /// at all is an upstream one.
    pub fn kind(&self) -> FailureKind {
        match self {
            DownloadFailure::InvalidUrl(_)
            | DownloadFailure::Refused(_)
            | DownloadFailure::TooLarge => FailureKind::InvalidArgument,
            DownloadFailure::HttpStatus(_) | DownloadFailure::Fetch(_) => {
                FailureKind::UpstreamFailed
            }
        }
    }
}

/// Whether a file of this content type is returned as text.
pub fn is_text_content(content_type: &str) -> bool {
    let essence = media_type(content_type);
    essence.starts_with("text/")
        || essence.ends_with("+json")
        || essence.ends_with("+xml")
        || TEXT_MEDIA_TYPES.contains(&essence.as_str())
}

/// The file as returned to the caller: text as-is (invalid UTF-8 replaced),
/// anything else base64-encoded.
pub fn encode_file(content_type: &str, body: &[u8]) -> DownloadedFile {
    let content_type = if content_type.trim().is_empty() {
        UNKNOWN_CONTENT_TYPE.to_string()
    } else {
        content_type.trim().to_string()
    };
    let content = if is_text_content(&content_type) {
        String::from_utf8_lossy(body).into_owned()
    } else {
        base64::engine::general_purpose::STANDARD.encode(body)
    };
    DownloadedFile {
        content,
        content_type,
    }
}

pub async fn download(
    http: &dyn HttpFetcher,
    url: &str,
) -> Result<DownloadedFile, DownloadFailure> {
    let url =
        Url::parse(url.trim()).map_err(|error| DownloadFailure::InvalidUrl(error.to_string()))?;
    check_url(&url).map_err(DownloadFailure::Refused)?;
    let fetched = http
        .get(&url, MAX_DOWNLOAD_BYTES)
        .await
        .map_err(|error| match error {
            FetchError::Refused(refusal) => DownloadFailure::Refused(refusal),
            other => DownloadFailure::Fetch(other),
        })?;
    if !(200..300).contains(&fetched.status) {
        return Err(DownloadFailure::HttpStatus(fetched.status));
    }
    if fetched.truncated {
        return Err(DownloadFailure::TooLarge);
    }
    Ok(encode_file(&fetched.content_type, &fetched.body))
}
