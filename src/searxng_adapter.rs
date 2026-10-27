//! [`SearchEngine`] over SearXNG's JSON API (`/search?format=json`).

use std::time::Duration;

use async_trait::async_trait;
use url::Url;

use crate::contract::TimeRange;
use crate::search::{EngineQuery, EngineResponse, SearchEngine, SearchError};

/// Limit for one SearXNG query; SearXNG itself waits for its engines for a
/// few seconds and answers with whatever arrived.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SEARCH_PATH: &str = "search";
const JSON_FORMAT: &str = "json";

pub struct SearxngClient {
    http: reqwest::Client,
    search_url: Url,
}

impl SearxngClient {
    /// A client for the SearXNG instance at `base_url`, e.g.
    /// `http://mcp-websearch-server-searxng:8080`.
    pub fn new(base_url: &Url) -> anyhow::Result<Self> {
        let mut base = base_url.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            search_url: base.join(SEARCH_PATH)?,
        })
    }
}

fn time_range_parameter(time_range: TimeRange) -> &'static str {
    match time_range {
        TimeRange::Day => "day",
        TimeRange::Week => "week",
        TimeRange::Month => "month",
        TimeRange::Year => "year",
    }
}

#[async_trait]
impl SearchEngine for SearxngClient {
    async fn query(
        &self,
        query: &str,
        engine_query: EngineQuery,
    ) -> Result<EngineResponse, SearchError> {
        let mut parameters = vec![
            ("q", query),
            ("format", JSON_FORMAT),
            ("categories", engine_query.category.as_searxng()),
        ];
        if let Some(time_range) = engine_query.time_range {
            parameters.push(("time_range", time_range_parameter(time_range)));
        }
        let response = self
            .http
            .get(self.search_url.clone())
            .query(&parameters)
            .send()
            .await
            .map_err(|error| SearchError::Unreachable(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(SearchError::Status(status.as_u16()));
        }
        response
            .json::<EngineResponse>()
            .await
            .map_err(|error| SearchError::Decode(error.to_string()))
    }
}
