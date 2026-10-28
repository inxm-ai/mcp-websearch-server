//! Settings read from the environment once, at startup.
//!
//! The bridge starts this server with its own environment, so the chart's
//! `env` values arrive here unchanged.

use std::time::Duration;

use anyhow::Context;
use url::Url;

/// In-cluster Service the chart gives the SearXNG Deployment.
pub const DEFAULT_SEARXNG_URL: &str = "http://ext-mcp-websearch-server-searxng:8080";

/// The model alias tgi-core routes; the bridge's chart sets the same.
pub const DEFAULT_MODEL: &str = "tgi";
const DEFAULT_RENDER_POOL_SIZE: usize = 2;
const DEFAULT_RENDER_TIMEOUT_SECS: u64 = 20;
/// Most languages one search may ask for; each is another round of engine
/// queries.
const DEFAULT_SEARCH_MAX_LANGUAGES: usize = 4;

#[derive(Debug, Clone)]
pub struct Config {
    pub searxng_url: Url,
    /// Whether pages plain HTTP cannot read are rendered in Chromium.
    pub render_enabled: bool,
    /// Most browsers rendering at once.
    pub render_pool_size: usize,
    /// Limit for loading one page in the browser.
    pub render_timeout: Duration,
    /// tgi-core's OpenAI base URL; without it, crawl scoring and research
    /// fail as not configured.
    pub tgi_url: Option<Url>,
    pub default_model: String,
    /// The OTel collector's OTLP/gRPC endpoint; no export when unset.
    pub otlp_endpoint: Option<String>,
    /// Most languages one `search` or `web_research` call may name.
    pub search_max_languages: usize,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn parsed_env<T>(name: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr + ToString,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let value = env_or(name, &default.to_string());
    value
        .trim()
        .parse()
        .with_context(|| format!("{name} `{value}` is not valid"))
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let searxng_url = env_or("SEARXNG_URL", DEFAULT_SEARXNG_URL);
        Ok(Self {
            searxng_url: Url::parse(&searxng_url)
                .with_context(|| format!("SEARXNG_URL `{searxng_url}` is not a URL"))?,
            render_enabled: parsed_env("RENDER_ENABLED", true)?,
            render_pool_size: parsed_env("RENDER_POOL_SIZE", DEFAULT_RENDER_POOL_SIZE)?,
            render_timeout: Duration::from_secs(parsed_env(
                "RENDER_TIMEOUT_SECS",
                DEFAULT_RENDER_TIMEOUT_SECS,
            )?),
            tgi_url: std::env::var("TGI_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| {
                    Url::parse(value.trim())
                        .with_context(|| format!("TGI_URL `{value}` is not a URL"))
                })
                .transpose()?,
            default_model: env_or("DEFAULT_MODEL", DEFAULT_MODEL),
            otlp_endpoint: std::env::var("OTLP_ENDPOINT")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            search_max_languages: parsed_env("SEARCH_MAX_LANGUAGES", DEFAULT_SEARCH_MAX_LANGUAGES)?,
        })
    }
}
