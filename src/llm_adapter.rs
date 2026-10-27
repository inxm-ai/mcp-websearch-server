//! [`ChatModel`] over app-tgi-core's OpenAI-compatible chat completions,
//! reached in-cluster with the bridge's `TGI_URL` and `DEFAULT_MODEL`; no
//! credentials of its own.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::model::{ChatModel, ModelError, ModelTier};

const CAPABILITY_HEADER: &str = "x-inxm-model-capability";
const COMPLETIONS_PATH: &str = "chat/completions";
/// Limit for one completion; reports run longer than yes/no scoring.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const TEMPERATURE: f64 = 0.0;

pub struct TgiChatModel {
    http: reqwest::Client,
    endpoint: Url,
    model: String,
}

impl TgiChatModel {
    /// `base_url` is tgi-core's OpenAI base, e.g.
    /// `http://app-tgi-core.orchestrator.svc.cluster.local:8000/v1`.
    pub fn new(base_url: &Url, model: &str) -> anyhow::Result<Self> {
        let mut base = base_url.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()?,
            endpoint: base.join(COMPLETIONS_PATH)?,
            model: model.to_string(),
        })
    }
}

#[derive(Deserialize)]
struct Completion {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl ChatModel for TgiChatModel {
    async fn complete(
        &self,
        tier: ModelTier,
        system: &str,
        user: &str,
    ) -> Result<String, ModelError> {
        let body = json!({
            "model": self.model,
            "stream": false,
            "temperature": TEMPERATURE,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let response = self
            .http
            .post(self.endpoint.clone())
            .header(CAPABILITY_HEADER, tier.capability())
            .json(&body)
            .send()
            .await
            .map_err(|error| ModelError::Unreachable(error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ModelError::Status(status.as_u16()));
        }
        let completion: Completion = response
            .json()
            .await
            .map_err(|error| ModelError::Malformed(error.to_string()))?;
        completion
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .ok_or_else(|| ModelError::Malformed("no message content".to_string()))
    }
}

/// Stands in when `TGI_URL` is unset: every call fails as not configured,
/// so tools that need a model say so instead of guessing.
pub struct UnconfiguredModel;

#[async_trait]
impl ChatModel for UnconfiguredModel {
    async fn complete(
        &self,
        _tier: ModelTier,
        _system: &str,
        _user: &str,
    ) -> Result<String, ModelError> {
        Err(ModelError::NotConfigured)
    }
}
