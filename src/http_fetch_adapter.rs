//! [`HttpFetcher`] over reqwest, enforcing the address policy on every
//! request: hostnames resolve to public addresses only, IP literals are
//! checked before they are requested, proxies are ignored so the check
//! always applies, and no cookies or credentials are sent. Redirects are
//! not followed here: `fetch::get_following_redirects` follows them one
//! checked hop at a time.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use reqwest::redirect;
use url::Url;

use crate::address_policy::{check_url, is_public_ip, Refusal};
use crate::fetch::{FetchError, FetchedBody, HttpFetcher};

pub const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; inxm-websearch/",
    env!("CARGO_PKG_VERSION"),
    "; +https://inxm.ai)"
);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Limit for one request, body included.
pub const PER_URL_TIMEOUT: Duration = Duration::from_secs(20);
const ACCEPT_PAGES: &str = "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8";

/// Resolves hostnames to public addresses only, so a public name pointing
/// at a cluster address is not reachable.
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let resolved: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = resolved
                .into_iter()
                .filter(|address| is_public_ip(address.ip()))
                .collect();
            if public.is_empty() {
                return Err(
                    Box::new(Refusal::NoPublicAddress) as Box<dyn std::error::Error + Send + Sync>
                );
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

pub struct ReqwestFetcher {
    client: reqwest::Client,
}

impl ReqwestFetcher {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .no_proxy()
            .dns_resolver(Arc::new(PublicOnlyResolver))
            .redirect(redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(PER_URL_TIMEOUT)
            .build()?;
        Ok(Self { client })
    }
}

/// The address-policy refusal behind a reqwest error, if that is what
/// stopped the request.
fn refusal_in(error: &reqwest::Error) -> Option<Refusal> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if let Some(refusal) = current.downcast_ref::<Refusal>() {
            return Some(refusal.clone());
        }
        source = current.source();
    }
    None
}

fn fetch_error(error: &reqwest::Error) -> FetchError {
    if let Some(refusal) = refusal_in(error) {
        return FetchError::Refused(refusal);
    }
    if error.is_timeout() {
        return FetchError::Timeout;
    }
    let kind = if error.is_connect() {
        "could not connect"
    } else {
        "request failed"
    };
    tracing::debug!(error = %error, "fetch failed");
    FetchError::Network(kind.to_string())
}

#[async_trait]
impl HttpFetcher for ReqwestFetcher {
    async fn get(&self, url: &Url, max_bytes: usize) -> Result<FetchedBody, FetchError> {
        check_url(url).map_err(FetchError::Refused)?;
        let mut response = self
            .client
            .get(url.clone())
            .header(ACCEPT, ACCEPT_PAGES)
            .send()
            .await
            .map_err(|error| fetch_error(&error))?;
        let status = response.status().as_u16();
        let final_url = response.url().clone();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut body = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| fetch_error(&error))?
        {
            let room = max_bytes.saturating_sub(body.len());
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(FetchedBody {
            final_url,
            status,
            content_type,
            location,
            body,
            truncated,
        })
    }
}
