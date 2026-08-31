//! Provider-neutral request/response types.

use cognomen::{Cognomen, Label};
use serde::{Deserialize, Serialize};

/// Message role in a chat turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Cognomen)]
#[cognomen(lower)]
pub enum Role {
    /// Message from the user.
    User,
    /// Message from the assistant (the model).
    Assistant,
}

/// One message in the chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Role.
    pub role: Role,
    /// Message text.
    pub content: String,
}

impl ChatMessage {
    /// Convenience constructor.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    /// Convenience constructor.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Provider-neutral chat completion request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Optional system prompt.
    pub system: Option<String>,
    /// User + assistant messages.
    pub messages: Vec<ChatMessage>,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Sampling temperature (0.0..2.0). `None` to defer to provider default.
    pub temperature: Option<f32>,
}

impl ChatRequest {
    /// Build a request from a single user prompt.
    #[must_use]
    pub fn user_prompt(prompt: impl Into<String>) -> Self {
        Self {
            system: None,
            messages: vec![ChatMessage::user(prompt)],
            max_tokens: 1024,
            temperature: None,
        }
    }

    /// Override the max-tokens cap.
    #[must_use]
    pub const fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }
}

/// Token usage report. Providers return at least input/output counts.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed by the prompt.
    pub input_tokens: u32,
    /// Tokens generated.
    pub output_tokens: u32,
}

/// Provider-neutral response.
#[derive(Debug, Clone, Serialize)]
pub struct ChatResponse {
    /// Model-assistant text output.
    pub text: String,
    /// Token usage, if reported.
    pub usage: Option<Usage>,
    /// Model identifier echoed by the provider.
    pub model: String,
}

/// Operator-facing reasoning / thinking effort.
///
/// Serde rejects unknown values at config load so a typo does not become a
/// 400 on first consolidation. Each provider maps this onto its native
/// request field:
///
/// - OpenAI Chat Completions / generic openai-compat: `reasoning_effort`
///   (`ultra`/`persistent` clamp to `max`; those strings are not in the
///   official Chat Completions enum)
/// - OpenRouter: `reasoning.effort` (and excludes reasoning from `content`)
/// - xAI Grok Chat Completions: `reasoning_effort` (`low`/`medium`/`high`/
///   `xhigh`; reasoning cannot be disabled)
/// - Anthropic Messages: `output_config.effort` plus adaptive/disabled
///   `thinking` on models that accept those fields
/// - ChatGPT/Codex Responses: `reasoning.effort` (same OpenAI wire clamp)
///
/// Gemini and Copilot ignore the key.
///
/// Omitting the config key (Rust `None`) leaves the model default;
/// [`Self::None`] is the wire value `none` (disable reasoning where the
/// backend allows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Cognomen)]
#[cognomen(lower)]
pub enum ReasoningEffort {
    /// Disable reasoning (`none`).
    None,
    /// Lowest billed reasoning (`minimal`).
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort (`xhigh`).
    XHigh,
    /// Maximum reasoning effort (`max`).
    Max,
    /// Codex-advertised `ultra` effort.
    Ultra,
    /// Codex-advertised `persistent` effort.
    Persistent,
}

impl ReasoningEffort {
    /// Canonical wire-format string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        Label::as_str(&self)
    }

    /// xAI Grok Chat Completions only accepts `low`/`medium`/`high`/`xhigh`
    /// and cannot disable reasoning. Closest supported value is used.
    #[must_use]
    pub const fn grok_chat_effort(self) -> Self {
        match self {
            Self::None | Self::Minimal => Self::Low,
            Self::Max | Self::Ultra | Self::Persistent => Self::XHigh,
            other => other,
        }
    }

    /// OpenAI Chat Completions / Responses and OpenRouter accept
    /// `none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`. `ultra` and
    /// `persistent` are not in that enum.
    #[must_use]
    pub const fn openai_wire_effort(self) -> Self {
        match self {
            Self::Ultra | Self::Persistent => Self::Max,
            other => other,
        }
    }

    /// Anthropic `output_config.effort` is `low`/`medium`/`high`/`xhigh`/
    /// `max`. `none` is represented by disabling thinking instead.
    #[must_use]
    pub const fn anthropic_output_effort(self) -> Option<Self> {
        match self {
            Self::None => None,
            Self::Minimal => Some(Self::Low),
            Self::Ultra | Self::Persistent => Some(Self::Max),
            other => Some(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ReasoningEffort;
    use cognomen::Variants;
    use rstest::rstest;

    #[test]
    fn reasoning_effort_roundtrips() {
        assert_eq!(
            ReasoningEffort::LABELS,
            &[
                "none",
                "minimal",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultra",
                "persistent",
            ]
        );
        for &effort in ReasoningEffort::VARIANTS {
            let raw = effort.as_str();
            let parsed: ReasoningEffort = serde_json::from_value(serde_json::json!(raw)).unwrap();
            assert_eq!(parsed, effort);
            assert_eq!(effort, raw);
            assert_eq!(
                serde_json::to_value(effort).unwrap(),
                serde_json::json!(raw)
            );
        }
    }

    #[rstest]
    #[case::uppercase("HIGH")]
    #[case::unknown("ludicrous")]
    fn reasoning_effort_rejects_invalid(#[case] raw: &str) {
        assert!(serde_json::from_value::<ReasoningEffort>(serde_json::json!(raw)).is_err());
    }

    #[rstest]
    #[case::none(ReasoningEffort::None, ReasoningEffort::Low)]
    #[case::minimal(ReasoningEffort::Minimal, ReasoningEffort::Low)]
    #[case::max(ReasoningEffort::Max, ReasoningEffort::XHigh)]
    #[case::ultra(ReasoningEffort::Ultra, ReasoningEffort::XHigh)]
    fn grok_clamps_unsupported_effort(
        #[case] input: ReasoningEffort,
        #[case] expected: ReasoningEffort,
    ) {
        assert_eq!(input.grok_chat_effort(), expected);
    }

    #[rstest]
    #[case::none(ReasoningEffort::None, None)]
    #[case::minimal(ReasoningEffort::Minimal, Some(ReasoningEffort::Low))]
    #[case::ultra(ReasoningEffort::Ultra, Some(ReasoningEffort::Max))]
    #[case::high(ReasoningEffort::High, Some(ReasoningEffort::High))]
    fn anthropic_maps_output_effort(
        #[case] input: ReasoningEffort,
        #[case] expected: Option<ReasoningEffort>,
    ) {
        assert_eq!(input.anthropic_output_effort(), expected);
    }

    #[rstest]
    #[case::ultra(ReasoningEffort::Ultra, ReasoningEffort::Max)]
    #[case::persistent(ReasoningEffort::Persistent, ReasoningEffort::Max)]
    #[case::xhigh(ReasoningEffort::XHigh, ReasoningEffort::XHigh)]
    fn openai_clamps_non_enum_effort(
        #[case] input: ReasoningEffort,
        #[case] expected: ReasoningEffort,
    ) {
        assert_eq!(input.openai_wire_effort(), expected);
    }
}
