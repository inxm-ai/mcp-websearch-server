//! Settings read from the environment once, at startup.
//!
//! The bridge starts this server with its own environment, so the chart's
//! `env` values arrive here unchanged.

use anyhow::Context;
use url::Url;

/// In-cluster Service the chart gives the SearXNG Deployment.
pub const DEFAULT_SEARXNG_URL: &str = "http://mcp-websearch-server-searxng:8080";

#[derive(Debug, Clone)]
pub struct Config {
    pub searxng_url: Url,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let searxng_url = env_or("SEARXNG_URL", DEFAULT_SEARXNG_URL);
        Ok(Self {
            searxng_url: Url::parse(&searxng_url)
                .with_context(|| format!("SEARXNG_URL `{searxng_url}` is not a URL"))?,
        })
    }
}
