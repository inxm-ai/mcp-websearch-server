//! The plain-HTTP port every reading tool shares, and what it answers.

use async_trait::async_trait;
use url::Url;

use crate::address_policy::Refusal;

/// A response body read up to a byte limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedBody {
    /// Where the redirect chain ended.
    pub final_url: Url,
    pub status: u16,
    /// The `Content-Type` header, empty when absent.
    pub content_type: String,
    pub body: Vec<u8>,
    /// The body was longer than the limit and was cut.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    #[error("refused: {0}")]
    Refused(Refusal),
    #[error("timed out")]
    Timeout,
    #[error("network error: {0}")]
    Network(String),
}

/// GETs public http(s) URLs under the address policy, without credentials
/// or cookies.
#[async_trait]
pub trait HttpFetcher: Send + Sync {
    /// GETs `url`, following at most a bounded redirect chain, and reads at
    /// most `max_bytes` of the body.
    async fn get(&self, url: &Url, max_bytes: usize) -> Result<FetchedBody, FetchError>;
}
