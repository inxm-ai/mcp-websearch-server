//! The plain-HTTP port every reading tool shares, what it answers, and the
//! redirect following on top of it.
//!
//! Redirects are followed here, one hop at a time, rather than inside the
//! HTTP client, so that every hop passes the address policy and the
//! caller's [`HopGate`] (the crawler's robots.txt rules) before it is
//! requested.

use async_trait::async_trait;
use url::Url;

use crate::address_policy::{check_url, Refusal};

/// Redirect hops followed before a request is given up.
pub const MAX_REDIRECTS: usize = 5;
/// Statuses whose `Location` is followed.
const REDIRECT_STATUSES: &[u16] = &[301, 302, 303, 307, 308];

/// One response, its body read up to a byte limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedBody {
    /// The URL that answered: the requested one, or after
    /// [`get_following_redirects`] the end of the redirect chain.
    pub final_url: Url,
    pub status: u16,
    /// The `Content-Type` header, empty when absent.
    pub content_type: String,
    /// The `Location` header, when the response has one.
    pub location: Option<String>,
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
    /// A redirect hop the caller's [`HopGate`] refused; the reason is the
    /// gate's own.
    #[error("{0}")]
    HopRefused(String),
}

/// GETs public http(s) URLs under the address policy, without credentials
/// or cookies, and without following redirects.
#[async_trait]
pub trait HttpFetcher: Send + Sync {
    /// GETs `url` and reads at most `max_bytes` of the body. A redirect
    /// comes back as its own response, with `location` set.
    async fn get(&self, url: &Url, max_bytes: usize) -> Result<FetchedBody, FetchError>;
}

/// What a caller allows beyond the address policy, asked before every
/// redirect hop (and every navigation of a rendered page).
#[async_trait]
pub trait HopGate: Send + Sync {
    /// `Err` with the reason shown to the caller when `url` must not be
    /// requested.
    async fn allow(&self, url: &Url) -> Result<(), String>;
}

/// Allows every hop the address policy allows: `extract`'s rule.
pub struct AnyHop;

#[async_trait]
impl HopGate for AnyHop {
    async fn allow(&self, _url: &Url) -> Result<(), String> {
        Ok(())
    }
}

/// Where a redirect response points, when it is one.
fn redirect_target(fetched: &FetchedBody) -> Option<Url> {
    if !REDIRECT_STATUSES.contains(&fetched.status) {
        return None;
    }
    let location = fetched.location.as_deref()?;
    fetched.final_url.join(location.trim()).ok()
}

/// GETs `url`, following at most [`MAX_REDIRECTS`] redirects; every hop
/// passes the address policy and `gate` before it is requested.
pub async fn get_following_redirects(
    http: &dyn HttpFetcher,
    url: &Url,
    max_bytes: usize,
    gate: &dyn HopGate,
) -> Result<FetchedBody, FetchError> {
    let mut current = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        let fetched = http.get(&current, max_bytes).await?;
        let Some(next) = redirect_target(&fetched) else {
            return Ok(fetched);
        };
        check_url(&next).map_err(FetchError::Refused)?;
        gate.allow(&next).await.map_err(FetchError::HopRefused)?;
        current = next;
    }
    Err(FetchError::Refused(Refusal::TooManyRedirects(
        MAX_REDIRECTS,
    )))
}
