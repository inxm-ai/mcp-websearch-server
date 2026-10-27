//! [`Renderer`] over a stygian-browser pool of headless Chromium.
//!
//! Stealth is off (`StealthLevel::None`): fingerprint spoofing from a
//! customer's egress IP is not this server's to switch on. Every request the
//! page makes is paused through CDP `Fetch` interception and held to the
//! address policy (scheme, credentials, IP literal, and every address the
//! host resolves to) before it continues, so redirects and subresources
//! cannot reach private or cluster addresses; images, fonts and media are
//! not loaded at all. Cookies are cleared before every page (so none set by
//! an earlier page is ever sent) and again after it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EnableParams, EventRequestPaused, FailRequestParams, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::{
    ClearBrowserCookiesParams, ErrorReason, ResourceType as CdpResourceType,
};
use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;
use chromiumoxide::Page;
use futures::StreamExt;
use stygian_browser::config::{PoolConfig, StealthLevel};
use stygian_browser::page::{PageHandle, ResourceFilter, ResourceType, WaitUntil};
use stygian_browser::{BrowserConfig, BrowserPool};
use tokio::sync::OnceCell;
use tracing::Instrument;
use url::Url;

use crate::address_policy::{check_url, is_public_ip, Refusal};
use crate::extract::{RenderError, RenderedPage, Renderer};

/// Warm browsers kept once the pool exists; more start on demand up to the
/// configured pool size.
const WARM_BROWSERS: usize = 1;
/// Page APIs that open connections Fetch interception does not see.
const DISABLE_UNINTERCEPTED_CONNECTIONS: &str = "(() => {
  for (const name of ['WebSocket', 'WebTransport', 'RTCPeerConnection', 'webkitRTCPeerConnection']) {
    try { Object.defineProperty(window, name, { value: undefined, writable: false, configurable: false }); } catch (_) {}
  }
})();";
/// Keeps WebRTC from opening UDP sockets that bypass interception.
const WEBRTC_POLICY_ARG: &str = "--force-webrtc-ip-handling-policy=disable_non_proxied_udp";
const BLANK_PAGE_SCHEMES: &[&str] = &["about", "chrome-error"];
/// Main-document statuses from here on are error pages, not content.
const FIRST_ERROR_STATUS: u16 = 400;

pub struct StygianRenderer {
    pool: OnceCell<Arc<BrowserPool>>,
    pool_size: usize,
    render_timeout: Duration,
}

impl StygianRenderer {
    /// A renderer whose pool starts on first use, so listing tools never
    /// waits for Chromium.
    pub fn new(pool_size: usize, render_timeout: Duration) -> Self {
        Self {
            pool: OnceCell::new(),
            pool_size: pool_size.max(1),
            render_timeout,
        }
    }

    fn browser_config(&self) -> BrowserConfig {
        let defaults = BrowserConfig::default();
        let mut args = defaults.args.clone();
        args.push(WEBRTC_POLICY_ARG.to_string());
        BrowserConfig {
            stealth_level: StealthLevel::None,
            headless: true,
            // Never route through a proxy: the address check must see the
            // real destination.
            proxy: None,
            proxy_bypass_list: None,
            proxy_source: None,
            // The pod is the sandbox: Chromium's own needs user namespaces
            // the cluster does not grant to a container running as root.
            disable_sandbox: true,
            args,
            pool: PoolConfig {
                min_size: WARM_BROWSERS.min(self.pool_size),
                max_size: self.pool_size,
                ..PoolConfig::default()
            },
            ..defaults
        }
    }

    async fn pool(&self) -> Result<&Arc<BrowserPool>, RenderError> {
        self.pool
            .get_or_try_init(|| async {
                BrowserPool::new(self.browser_config())
                    .await
                    .map_err(|error| RenderError::Unavailable(error.to_string()))
            })
            .await
    }
}

/// Images, fonts and media never load; the text does not need them.
fn resource_filter() -> ResourceFilter {
    ResourceFilter::block_images_and_fonts().block(ResourceType::Media)
}

/// Whether every address `host` resolves to is public (an empty answer is
/// not).
async fn resolves_publicly(host: &str, port: u16) -> bool {
    match tokio::net::lookup_host((host, port)).await {
        Ok(addresses) => {
            let addresses: Vec<_> = addresses.collect();
            !addresses.is_empty() && addresses.iter().all(|address| is_public_ip(address.ip()))
        }
        Err(_) => false,
    }
}

/// The address-policy verdict for one paused request; `None` lets it
/// continue. Hosts are resolved once per page.
async fn refusal_for(
    request_url: &str,
    resolved: &mut HashMap<(String, u16), bool>,
) -> Option<Refusal> {
    let url = match Url::parse(request_url) {
        Ok(url) => url,
        Err(_) => return Some(Refusal::MissingHost),
    };
    if url.scheme() == "data" || url.scheme() == "blob" {
        return None;
    }
    if let Err(refusal) = check_url(&url) {
        return Some(refusal);
    }
    let host = url
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = url.port_or_known_default()?;
    let key = (host, port);
    let public = match resolved.get(&key) {
        Some(public) => *public,
        None => {
            let public = resolves_publicly(&key.0, port).await;
            resolved.insert(key, public);
            public
        }
    };
    (!public).then_some(Refusal::NoPublicAddress)
}

/// Pauses every request the page makes and lets through only what the
/// resource filter and the address policy allow. Returns the first refusal
/// of a document request (a navigation or redirect), which the caller turns
/// into the page's failure.
async fn guard_requests(
    page: &Page,
) -> Result<(tokio::task::JoinHandle<()>, Arc<Mutex<Option<Refusal>>>), RenderError> {
    let pattern = RequestPattern::builder().url_pattern("*").build();
    let enable = EnableParams::builder()
        .patterns(vec![pattern])
        .handle_auth_requests(false)
        .build();
    let mut events = page
        .event_listener::<EventRequestPaused>()
        .await
        .map_err(|error| RenderError::Browser(error.to_string()))?;
    page.execute(enable)
        .await
        .map_err(|error| RenderError::Browser(error.to_string()))?;

    let document_refusal = Arc::new(Mutex::new(None));
    let recorded_refusal = Arc::clone(&document_refusal);
    let page = page.clone();
    let filter = resource_filter();
    let guard = async move {
        let mut resolved = HashMap::new();
        while let Some(event) = events.next().await {
            let request_id = event.request_id.clone();
            let verdict = if filter.should_block(event.resource_type.as_ref()) {
                Err(ErrorReason::BlockedByClient)
            } else {
                match refusal_for(&event.request.url, &mut resolved).await {
                    None => Ok(()),
                    Some(refusal) => {
                        tracing::info!(url = %event.request.url, %refusal, "render request refused");
                        if event.resource_type == CdpResourceType::Document {
                            recorded_refusal
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get_or_insert(refusal);
                        }
                        Err(ErrorReason::AccessDenied)
                    }
                }
            };
            let answered = match verdict {
                Ok(()) => page
                    .execute(ContinueRequestParams::new(request_id))
                    .await
                    .map(|_| ()),
                Err(reason) => page
                    .execute(FailRequestParams::new(request_id, reason))
                    .await
                    .map(|_| ()),
            };
            if let Err(error) = answered {
                tracing::debug!(error = %error, "could not answer a paused request");
            }
        }
    };
    let task = tokio::spawn(guard.instrument(tracing::Span::current()));
    Ok((task, document_refusal))
}

impl StygianRenderer {
    async fn render_on(
        &self,
        page: &mut PageHandle,
        url: &Url,
    ) -> Result<RenderedPage, RenderError> {
        let (guard, document_refusal) = guard_requests(page.inner()).await?;
        let outcome = async {
            page.inner()
                .execute(AddScriptToEvaluateOnNewDocumentParams::new(
                    DISABLE_UNINTERCEPTED_CONNECTIONS,
                ))
                .await
                .map_err(|error| RenderError::Browser(error.to_string()))?;
            // Pages that keep polling never reach network idle; what has
            // rendered by the deadline is still read.
            let navigation = page
                .navigate(url.as_str(), WaitUntil::NetworkIdle, self.render_timeout)
                .await;
            if let Some(refusal) = document_refusal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return Err(RenderError::Refused(refusal));
            }
            let final_url = page
                .url()
                .await
                .map_err(|error| RenderError::Browser(error.to_string()))?;
            let final_url = Url::parse(&final_url)
                .map_err(|error| RenderError::Browser(format!("final URL: {error}")))?;
            if BLANK_PAGE_SCHEMES.contains(&final_url.scheme()) {
                return Err(match navigation {
                    Err(error) if error.to_string().contains("timed out") => RenderError::Timeout,
                    Err(error) => RenderError::Browser(error.to_string()),
                    Ok(()) => RenderError::Browser("the page did not load".to_string()),
                });
            }
            check_url(&final_url).map_err(RenderError::Refused)?;
            let status = page
                .status_code()
                .map_err(|error| RenderError::Browser(error.to_string()))?;
            if let Some(status) = status.filter(|status| *status >= FIRST_ERROR_STATUS) {
                return Err(RenderError::HttpStatus(status));
            }
            if let Err(error) = &navigation {
                tracing::debug!(error = %error, "navigation did not settle; reading what rendered");
            }
            let html = page
                .content()
                .await
                .map_err(|error| RenderError::Browser(error.to_string()))?;
            Ok(RenderedPage { final_url, html })
        }
        .await;
        guard.abort();
        outcome
    }
}

#[async_trait]
impl Renderer for StygianRenderer {
    async fn render(&self, url: &Url) -> Result<RenderedPage, RenderError> {
        check_url(url).map_err(RenderError::Refused)?;
        let pool = self.pool().await?;
        let handle = pool
            .acquire()
            .await
            .map_err(|error| RenderError::Unavailable(error.to_string()))?;
        let outcome = async {
            let browser = handle
                .browser()
                .ok_or_else(|| RenderError::Unavailable("no browser in the handle".to_string()))?;
            let mut page = browser
                .new_page()
                .await
                .map_err(|error| RenderError::Browser(error.to_string()))?;
            page.inner()
                .execute(ClearBrowserCookiesParams::default())
                .await
                .map_err(|error| RenderError::Browser(format!("clearing cookies: {error}")))?;
            let rendered = self.render_on(&mut page, url).await;
            // Hygiene only: the next page clears cookies before it loads.
            if let Err(error) = page.inner().execute(ClearBrowserCookiesParams::default()).await {
                tracing::warn!(error = %error, "could not clear the browser's cookies after a page");
            }
            if let Err(error) = page.close().await {
                tracing::debug!(error = %error, "could not close the page");
            }
            rendered
        }
        .await;
        handle.release().await;
        outcome
    }
}
