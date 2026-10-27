//! The chat-model port (tgi-core) used to score crawled pages and to plan
//! and write research, plus the pure handling of its answers.

use async_trait::async_trait;

/// Which tgi-core model answers: routed by capability, not by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    /// The `fast` capability: yes/no scoring, `web_research` with `mini`.
    Fast,
    /// The default chat model: `web_research` with `pro`.
    Default,
}

impl ModelTier {
    /// The `x-inxm-model-capability` tgi-core routes on.
    pub fn capability(self) -> &'static str {
        match self {
            ModelTier::Fast => "fast",
            ModelTier::Default => "chat",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("the model is not configured (TGI_URL unset)")]
    NotConfigured,
    #[error("the model could not be reached: {0}")]
    Unreachable(String),
    #[error("the model answered HTTP {0}")]
    Status(u16),
    #[error("the model's answer could not be read: {0}")]
    Malformed(String),
}

/// One system + user turn in, the assistant's text out.
#[async_trait]
pub trait ChatModel: Send + Sync {
    async fn complete(
        &self,
        tier: ModelTier,
        system: &str,
        user: &str,
    ) -> Result<String, ModelError>;
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// The answer without a reasoning model's `<think>…</think>` preamble.
pub fn strip_reasoning(answer: &str) -> &str {
    match answer.find(THINK_CLOSE) {
        Some(end) if answer.trim_start().starts_with(THINK_OPEN) => {
            answer[end + THINK_CLOSE.len()..].trim()
        }
        _ => answer.trim(),
    }
}

/// A yes/no answer's verdict; `None` when it is neither.
pub fn parse_yes_no(answer: &str) -> Option<bool> {
    let first_word: String = strip_reasoning(answer)
        .trim_start_matches(|character: char| !character.is_alphabetic())
        .chars()
        .take_while(|character| character.is_alphabetic())
        .collect::<String>()
        .to_lowercase();
    match first_word.as_str() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}
