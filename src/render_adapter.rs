//! [`Renderer`] over a stygian-browser pool of headless Chromium.
//!
//! Stealth is off (`StealthLevel::None`): fingerprint spoofing from a
//! customer's egress IP is not this server's to switch on. Every request the
//! browser makes, from the page and from any popup or tab it opens, is
//! paused through CDP `Fetch` interception on the browser session and held
//! to the address policy (scheme, credentials, IP literal, and every address
//! the host resolves to) before it continues, so redirects, subresources and
//! new windows cannot reach private or cluster addresses; images, fonts and
//! media are not loaded at all. Cookies are cleared before every page (so none set by
//! an earlier page is ever sent) and again after it.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EnableParams, EventRequestPaused, FailRequestParams, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::{
    ClearBrowserCookiesParams, ErrorReason, ResourceType as CdpResourceType,
};
use chromiumoxide::cdp::browser_protocol::page::{AddScriptToEvaluateOnNewDocumentParams, FrameId};
use chromiumoxide::cdp::browser_protocol::target::TargetId;
use chromiumoxide::Browser;
use futures::StreamExt;
use stygian_browser::config::{PoolConfig, StealthLevel};
use stygian_browser::page::{PageHandle, ResourceFilter, ResourceType, WaitUntil};
use stygian_browser::{BrowserConfig, BrowserPool};
use tokio::sync::OnceCell;
use url::Url;

use crate::address_policy::{check_url, is_public_ip, loggable_url, Refusal};
use crate::extract::{RenderError, RenderedPage, Renderer};
use crate::fetch::HopGate;

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

/// Which addresses the browser may reach. Only the tests below serve
/// fixtures from loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    PublicOnly,
    #[cfg(test)]
    LoopbackFixture,
}

/// Paths a `Reach::LoopbackFixture` server treats as private.
#[cfg(test)]
const FIXTURE_PRIVATE_PREFIX: &str = "/private";

impl Reach {
    /// The checks that need no DNS: the address policy, or for fixtures any
    /// loopback URL outside the private paths.
    fn check_url(self, url: &Url) -> Result<(), Refusal> {
        match self {
            Reach::PublicOnly => check_url(url),
            #[cfg(test)]
            Reach::LoopbackFixture => {
                if url.path().starts_with(FIXTURE_PRIVATE_PREFIX) {
                    Err(Refusal::PrivateAddress)
                } else {
                    Ok(())
                }
            }
        }
    }
}

pub struct StygianRenderer {
    pool: OnceCell<Arc<BrowserPool>>,
    pool_size: usize,
    render_timeout: Duration,
    reach: Reach,
}

impl StygianRenderer {
    /// A renderer whose pool starts on first use, so listing tools never
    /// waits for Chromium.
    pub fn new(pool_size: usize, render_timeout: Duration) -> Self {
        Self {
            pool: OnceCell::new(),
            pool_size: pool_size.max(1),
            render_timeout,
            reach: Reach::PublicOnly,
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
    reach: Reach,
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
    if let Err(refusal) = reach.check_url(&url) {
        return Some(refusal);
    }
    if reach != Reach::PublicOnly {
        return None;
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

/// Pauses every request the browser makes, whichever target makes it, and
/// lets through only what the resource filter and the address policy allow.
/// Interception sits on the browser session because chromiumoxide attaches
/// to and resumes every new target (a popup, a `target=_blank` link or form)
/// itself, before a page-level guard could be installed on it. Returns the
/// first refusal of a document request in `main_frame` (a navigation or
/// redirect of the page being read), which the caller turns into the page's
/// failure, and the guard itself, which answers paused requests for as long
/// as it is polled. Main-frame navigations to anything but `start` must also
/// pass `gate` (for a crawl, robots.txt).
///
/// Interception stays enabled after the guard stops: `Fetch.disable` would
/// continue requests still paused, and only the guard may decide those.
async fn guard_requests<'a>(
    reach: Reach,
    browser: &'a Browser,
    main_frame: FrameId,
    start: Url,
    gate: &'a dyn HopGate,
) -> Result<
    (
        impl Future<Output = ()> + 'a,
        Arc<Mutex<Option<RenderError>>>,
    ),
    RenderError,
> {
    let pattern = RequestPattern::builder().url_pattern("*").build();
    let enable = EnableParams::builder()
        .patterns(vec![pattern])
        .handle_auth_requests(false)
        .build();
    let mut events = browser
        .event_listener::<EventRequestPaused>()
        .await
        .map_err(|error| RenderError::Browser(error.to_string()))?;
    browser
        .execute(enable)
        .await
        .map_err(|error| RenderError::Browser(error.to_string()))?;

    let document_refusal = Arc::new(Mutex::new(None));
    let recorded_refusal = Arc::clone(&document_refusal);
    let filter = resource_filter();
    let guard = async move {
        let mut resolved = HashMap::new();
        while let Some(event) = events.next().await {
            let request_id = event.request_id.clone();
            let page_navigation =
                event.resource_type == CdpResourceType::Document && event.frame_id == main_frame;
            let refusal = if filter.should_block(event.resource_type.as_ref()) {
                None
            } else if let Some(refusal) =
                refusal_for(reach, &event.request.url, &mut resolved).await
            {
                tracing::info!(url = %loggable_url(&event.request.url), %refusal, "render request refused");
                Some(RenderError::Refused(refusal))
            } else if page_navigation {
                navigation_refusal(&event.request.url, &start, gate).await
            } else {
                None
            };
            if let (Some(refusal), true) = (&refusal, page_navigation) {
                recorded_refusal
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_or_insert_with(|| refusal.clone());
            }
            let verdict = if filter.should_block(event.resource_type.as_ref()) {
                Err(ErrorReason::BlockedByClient)
            } else if refusal.is_some() {
                Err(ErrorReason::AccessDenied)
            } else {
                Ok(())
            };
            let answered = match verdict {
                Ok(()) => browser
                    .execute(ContinueRequestParams::new(request_id))
                    .await
                    .map(|_| ()),
                Err(reason) => browser
                    .execute(FailRequestParams::new(request_id, reason))
                    .await
                    .map(|_| ()),
            };
            if let Err(error) = answered {
                tracing::debug!(error = %error, "could not answer a paused request");
            }
        }
    };
    Ok((guard, document_refusal))
}

/// Whether `gate` refuses a navigation of the page's main frame; the page's
/// own URL was approved before the render started.
async fn navigation_refusal(
    request_url: &str,
    start: &Url,
    gate: &dyn HopGate,
) -> Option<RenderError> {
    let Ok(mut url) = Url::parse(request_url) else {
        return Some(RenderError::Refused(Refusal::MissingHost));
    };
    url.set_fragment(None);
    let mut start = start.clone();
    start.set_fragment(None);
    if url == start {
        return None;
    }
    gate.allow(&url).await.err().map(RenderError::HopRefused)
}

/// Closes every page in `browser` except `keep`: the popups and tabs a
/// rendered page opened.
async fn close_other_pages(browser: &Browser, keep: &TargetId) {
    let pages = match browser.pages().await {
        Ok(pages) => pages,
        Err(error) => {
            tracing::warn!(error = %error, "could not list the browser's pages");
            return;
        }
    };
    for page in pages.into_iter().filter(|page| page.target_id() != keep) {
        if let Err(error) = page.close().await {
            tracing::warn!(error = %error, "could not close a page the rendered page opened");
        }
    }
}

impl StygianRenderer {
    async fn render_on(
        &self,
        browser: &Browser,
        page: &mut PageHandle,
        url: &Url,
        gate: &dyn HopGate,
    ) -> Result<RenderedPage, RenderError> {
        let main_frame = page
            .inner()
            .mainframe()
            .await
            .map_err(|error| RenderError::Browser(error.to_string()))?
            .ok_or_else(|| RenderError::Browser("the page has no main frame".to_string()))?;
        let (guard, document_refusal) =
            guard_requests(self.reach, browser, main_frame, url.clone(), gate).await?;
        let page_target = page.inner().target_id().clone();
        let read = async {
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
                return Err(refusal);
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
            self.reach
                .check_url(&final_url)
                .map_err(RenderError::Refused)?;
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
        };
        // Close whatever the page opened while the guard still answers for
        // it, so no popup outlives the guard.
        let work = async {
            let outcome = read.await;
            close_other_pages(browser, &page_target).await;
            outcome
        };
        tokio::pin!(work);
        tokio::select! {
            outcome = &mut work => outcome,
            // The browser's event stream ended: nothing is left to pause.
            () = guard => work.await,
        }
    }
}

#[async_trait]
impl Renderer for StygianRenderer {
    async fn render(&self, url: &Url, gate: &dyn HopGate) -> Result<RenderedPage, RenderError> {
        self.reach.check_url(url).map_err(RenderError::Refused)?;
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
            let rendered = self.render_on(browser.browser(), &mut page, url, gate).await;
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

#[cfg(test)]
mod tests {
    //! Against a real Chromium, so ignored by default: run with
    //! `cargo test --lib -- --ignored` where Chrome or Chromium is installed.

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tracing::Instrument;
    use url::Url;

    use super::{Reach, StygianRenderer};
    use crate::address_policy::Refusal;
    use crate::extract::{RenderError, Renderer};
    use crate::fetch::{AnyHop, HopGate};

    /// Refuses `GATED_PATH`, as robots.txt would.
    struct RefuseGated;

    #[async_trait::async_trait]
    impl HopGate for RefuseGated {
        async fn allow(&self, url: &Url) -> Result<(), String> {
            if url.path() == GATED_PATH {
                Err("disallowed by robots.txt".to_string())
            } else {
                Ok(())
            }
        }
    }

    /// A page that tries every way of opening a new browsing context onto a
    /// private path.
    const HOSTILE_PAGE: &str = r#"<!doctype html><html><body><main><p>fixture</p>
<a id="link" href="/private-link" target="_blank">x</a>
<form id="form" action="/private-form" target="_blank"></form>
<script>
  window.open('/private-open');
  document.getElementById('link').click();
  document.getElementById('form').submit();
</script></main></body></html>"#;
    /// Answers with a redirect to a private path.
    const REDIRECTING_PATH: &str = "/redirect";
    /// Answers with a redirect to `GATED_PATH`, which the test's gate refuses.
    const GATED_REDIRECT_PATH: &str = "/hop";
    const GATED_PATH: &str = "/gated";
    /// Long enough for a popup that escaped to make its request.
    const SETTLE: Duration = Duration::from_secs(2);

    /// Serves `HOSTILE_PAGE` for every path and records the paths asked for.
    async fn fixture_server() -> (Url, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("bind: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("address: {error}"));
        let requested = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requested);
        let serve = async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = vec![0_u8; 8192];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                recorded
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(path.clone());
                let response = if path == REDIRECTING_PATH {
                    "HTTP/1.1 302 Found\r\nlocation: /private-redirect\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
                } else if path == GATED_REDIRECT_PATH {
                    format!("HTTP/1.1 302 Found\r\nlocation: {GATED_PATH}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{HOSTILE_PAGE}",
                        HOSTILE_PAGE.len()
                    )
                };
                let _ = socket.write_all(response.as_bytes()).await;
            }
        };
        tokio::spawn(serve.instrument(tracing::Span::current()));
        let url = Url::parse(&format!("http://{address}/"))
            .unwrap_or_else(|error| panic!("fixture URL: {error}"));
        (url, requested)
    }

    #[tokio::test]
    #[ignore = "needs Chrome or Chromium"]
    async fn pages_cannot_open_new_windows_onto_private_addresses() {
        let (url, requested) = fixture_server().await;
        let renderer = StygianRenderer {
            reach: Reach::LoopbackFixture,
            ..StygianRenderer::new(1, Duration::from_secs(10))
        };

        let rendered = renderer.render(&url, &AnyHop).await;
        tokio::time::sleep(SETTLE).await;

        assert!(rendered.is_ok(), "the fixture itself renders: {rendered:?}");
        let requested = requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(
            requested.iter().all(|path| !path.starts_with("/private")),
            "a new browsing context reached a private path: {requested:?}"
        );
    }

    #[tokio::test]
    #[ignore = "needs Chrome or Chromium"]
    async fn a_redirect_of_the_page_to_a_private_address_fails_the_render() {
        let (url, requested) = fixture_server().await;
        let renderer = StygianRenderer {
            reach: Reach::LoopbackFixture,
            ..StygianRenderer::new(1, Duration::from_secs(10))
        };
        let redirecting = url
            .join(REDIRECTING_PATH)
            .unwrap_or_else(|error| panic!("redirect URL: {error}"));

        let rendered = renderer.render(&redirecting, &AnyHop).await;

        assert_eq!(
            rendered.map(|page| page.final_url),
            Err(RenderError::Refused(Refusal::PrivateAddress))
        );
        let requested = requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(requested, vec![REDIRECTING_PATH.to_string()]);
    }

    #[tokio::test]
    #[ignore = "needs Chrome or Chromium"]
    async fn a_navigation_the_gate_refuses_is_never_requested() {
        let (url, requested) = fixture_server().await;
        let renderer = StygianRenderer {
            reach: Reach::LoopbackFixture,
            ..StygianRenderer::new(1, Duration::from_secs(10))
        };
        let hop = url
            .join(GATED_REDIRECT_PATH)
            .unwrap_or_else(|error| panic!("hop URL: {error}"));

        let rendered = renderer.render(&hop, &RefuseGated).await;

        assert_eq!(
            rendered.map(|page| page.final_url),
            Err(RenderError::HopRefused(
                "disallowed by robots.txt".to_string()
            ))
        );
        let requested = requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(requested, vec![GATED_REDIRECT_PATH.to_string()]);
    }
}
