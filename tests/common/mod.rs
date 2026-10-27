//! Scripted fakes shared by the behavior tests.

#![allow(dead_code)] // each test crate uses a different subset

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use mcp_websearch_server::fetch::{FetchError, FetchedBody, HttpFetcher};
use mcp_websearch_server::model::{ChatModel, ModelError, ModelTier};
use url::Url;

const REDIRECT_STATUS: u16 = 302;

/// Answers each URL from a fixed table; unknown URLs time out. Records
/// every URL requested.
pub struct ScriptedHttp {
    pages: HashMap<String, (u16, String, Vec<u8>)>,
    /// URL → the `Location` it answers with a 302.
    redirects: HashMap<String, String>,
    pub requested: Mutex<Vec<String>>,
}

impl ScriptedHttp {
    pub fn new(pages: &[(&str, u16, &str, &str)]) -> Self {
        Self {
            pages: pages
                .iter()
                .map(|(url, status, content_type, body)| {
                    (
                        url.to_string(),
                        (*status, content_type.to_string(), body.as_bytes().to_vec()),
                    )
                })
                .collect(),
            redirects: HashMap::new(),
            requested: Mutex::new(Vec::new()),
        }
    }

    pub fn with_redirect(mut self, from: &str, to: &str) -> Self {
        self.redirects.insert(from.to_string(), to.to_string());
        self
    }

    pub fn requested(&self) -> Vec<String> {
        self.requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl HttpFetcher for ScriptedHttp {
    async fn get(&self, url: &Url, max_bytes: usize) -> Result<FetchedBody, FetchError> {
        self.requested
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(url.to_string());
        if let Some(target) = self.redirects.get(url.as_str()) {
            return Ok(FetchedBody {
                final_url: url.clone(),
                status: REDIRECT_STATUS,
                content_type: String::new(),
                location: Some(target.clone()),
                body: Vec::new(),
                truncated: false,
            });
        }
        let (status, content_type, body) =
            self.pages.get(url.as_str()).ok_or(FetchError::Timeout)?;
        Ok(FetchedBody {
            final_url: url.clone(),
            location: None,
            status: *status,
            content_type: content_type.clone(),
            body: body.iter().copied().take(max_bytes).collect(),
            truncated: body.len() > max_bytes,
        })
    }
}

/// Answers by the first rule whose needle the prompt contains; records
/// every (tier, user prompt).
pub struct ScriptedModel {
    rules: Vec<(String, Result<String, ModelError>)>,
    pub calls: Mutex<Vec<(ModelTier, String)>>,
}

impl ScriptedModel {
    pub fn new(rules: &[(&str, Result<&str, ModelError>)]) -> Self {
        Self {
            rules: rules
                .iter()
                .map(|(needle, answer)| (needle.to_string(), answer.clone().map(str::to_string)))
                .collect(),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<(ModelTier, String)> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ChatModel for ScriptedModel {
    async fn complete(
        &self,
        tier: ModelTier,
        system: &str,
        user: &str,
    ) -> Result<String, ModelError> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((tier, user.to_string()));
        let prompt = format!("{system}\n{user}");
        self.rules
            .iter()
            .find(|(needle, _)| prompt.contains(needle.as_str()))
            .map(|(_, answer)| answer.clone())
            .unwrap_or_else(|| Err(ModelError::Malformed("no scripted answer".into())))
    }
}

/// Never answers: every request hangs until the caller gives up.
pub struct StalledHttp;

#[async_trait]
impl HttpFetcher for StalledHttp {
    async fn get(&self, _url: &Url, _max_bytes: usize) -> Result<FetchedBody, FetchError> {
        futures::future::pending().await
    }
}

/// Never answers a completion.
pub struct StalledModel;

#[async_trait]
impl ChatModel for StalledModel {
    async fn complete(
        &self,
        _tier: ModelTier,
        _system: &str,
        _user: &str,
    ) -> Result<String, ModelError> {
        futures::future::pending().await
    }
}
