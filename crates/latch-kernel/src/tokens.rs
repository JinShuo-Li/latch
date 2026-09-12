//! Conservative, provider/model-aware token estimation.
//!
//! Latch never had an exact tokenizer for arbitrary providers. Every
//! user-visible context number must be an honest *estimate* where the kernel
//! has not yet received provider-reported usage. This estimator deliberately
//! over-estimates: budgets that are safe with the estimate are safe in
//! practice. Provider-reported usage from a completed request is authoritative
//! and replaces the estimate in the UI.
//!
//! The estimator is byte-for-byte deterministic and handles multi-byte text:
//! ASCII text is priced at roughly four characters per token, ASCII
//! punctuation at two, and non-ASCII characters at one to two tokens each
//! (CJK-aware models get a tighter profile).

use latch_protocol::{ModelMessage, ToolDefinition};
use serde_json::Value;
use unicode_width::UnicodeWidthChar;

/// Tokenizer family used to price text. `Generic` is the conservative
/// fallback for unknown providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenProfile {
    /// OpenAI-family cl100k/o200k behaviour.
    OpenAi,
    /// Anthropic Claude behaviour.
    Anthropic,
    /// Models with CJK-optimized vocabularies (DeepSeek, Qwen, GLM, Kimi).
    CjkOptimized,
    /// Unknown provider: the most conservative weights.
    Generic,
}

impl TokenProfile {
    /// Fraction of a token charged per ASCII alphanumeric/space character.
    fn ascii_alnum_weight(self) -> f64 {
        match self {
            Self::OpenAi | Self::Anthropic => 0.25,
            Self::CjkOptimized => 0.27,
            Self::Generic => 0.28,
        }
    }
    /// Fraction of a token charged per ASCII punctuation character.
    fn ascii_punct_weight(self) -> f64 {
        0.5
    }
    /// Tokens charged per narrow non-ASCII character (Latin accents, combining
    /// marks, Cyrillic, Greek, Arabic).
    fn narrow_weight(self) -> f64 {
        match self {
            Self::CjkOptimized => 0.5,
            _ => 1.0,
        }
    }
    /// Tokens charged per wide non-ASCII character (CJK, full-width forms,
    /// emoji).
    fn wide_weight(self) -> f64 {
        match self {
            Self::CjkOptimized => 1.0,
            _ => 2.0,
        }
    }
}

/// Deterministic estimator configured for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenEstimator {
    profile: TokenProfile,
}

impl Default for TokenEstimator {
    fn default() -> Self {
        Self::generic()
    }
}

impl TokenEstimator {
    #[must_use]
    pub const fn new(profile: TokenProfile) -> Self {
        Self { profile }
    }

    #[must_use]
    pub const fn generic() -> Self {
        Self {
            profile: TokenProfile::Generic,
        }
    }

    /// Picks a profile from the provider model string. Unknown models fall
    /// back to the conservative `Generic` profile.
    #[must_use]
    pub fn for_model(model: &str) -> Self {
        let model = model.to_ascii_lowercase();
        let profile = if model.contains("deepseek")
            || model.contains("qwen")
            || model.contains("glm")
            || model.contains("kimi")
            || model.contains("minimax")
        {
            TokenProfile::CjkOptimized
        } else if model.contains("claude") || model.contains("anthropic") {
            TokenProfile::Anthropic
        } else if model.starts_with("gpt")
            || model.contains("openai")
            || model.contains("o1")
            || model.contains("o3")
            || model.contains("o4")
        {
            TokenProfile::OpenAi
        } else {
            TokenProfile::Generic
        };
        Self { profile }
    }

    #[must_use]
    pub const fn profile(&self) -> TokenProfile {
        self.profile
    }

    /// Estimated tokens for raw text. Never returns zero for non-empty input.
    #[must_use]
    pub fn estimate(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let mut weighted = 0.0_f64;
        for ch in text.chars() {
            if ch.is_ascii() {
                if ch.is_ascii_alphanumeric() || ch.is_ascii_whitespace() {
                    weighted += self.profile.ascii_alnum_weight();
                } else {
                    weighted += self.profile.ascii_punct_weight();
                }
                continue;
            }
            // Unicode width is the best cheap signal for CJK/full-width/emoji
            // versus narrow accented characters. Control characters count as
            // narrow.
            let wide = UnicodeWidthChar::width(ch).unwrap_or(1) >= 2;
            weighted += if wide {
                self.profile.wide_weight()
            } else {
                self.profile.narrow_weight()
            };
        }
        weighted.ceil().max(1.0) as usize
    }

    /// Estimated tokens for a JSON value, using the same text estimator on its
    /// compact serialization.
    #[must_use]
    pub fn estimate_json(&self, value: &Value) -> usize {
        let text = serde_json::to_string(value).unwrap_or_default();
        self.estimate(&text)
    }

    /// Estimated tokens for a provider-neutral message list, including a small
    /// per-message framing overhead.
    #[must_use]
    pub fn estimate_messages(&self, messages: &[ModelMessage]) -> usize {
        const MESSAGE_OVERHEAD: usize = 4;
        messages
            .iter()
            .map(|message| {
                self.estimate(&message.content)
                    + MESSAGE_OVERHEAD
                    + message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            self.estimate(&call.name) + self.estimate_json(&call.arguments) + 6
                        })
                        .sum::<usize>()
                    + message
                        .reasoning_content
                        .as_deref()
                        .map_or(0, |reasoning| self.estimate(reasoning))
            })
            .sum()
    }

    /// Estimated tokens for tool schemas, including a per-tool framing cost.
    #[must_use]
    pub fn estimate_tools(&self, tools: &[ToolDefinition]) -> usize {
        const TOOL_OVERHEAD: usize = 8;
        tools
            .iter()
            .map(|tool| {
                self.estimate(&tool.name)
                    + self.estimate(&tool.description)
                    + self.estimate_json(&tool.input_schema)
                    + TOOL_OVERHEAD
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use latch_protocol::{ModelMessage, ToolDefinition};
    use serde_json::json;

    #[test]
    fn ascii_estimates_are_conservative_and_monotonic() {
        let estimator = TokenEstimator::for_model("gpt-5-mini");
        let text = "fn main() { println!(\"hello world\"); }";
        let estimate = estimator.estimate(text);
        assert!(estimate >= text.len() / 5, "estimate {estimate} too low");
        assert!(estimate <= text.len(), "estimate {estimate} absurdly high");
        assert!(
            estimator.estimate(&text.repeat(2)) >= estimate * 2 - 1,
            "estimates scale with length"
        );
        assert_eq!(estimator.estimate(""), 0);
        assert_eq!(TokenEstimator::generic().estimate(""), 0);
    }

    #[test]
    fn cjk_text_is_not_underestimated() {
        let text = "实现一个内存 TTL 缓存并验证边界条件";
        let wide_chars = text
            .chars()
            .filter(|ch| UnicodeWidthChar::width(*ch).unwrap_or(1) >= 2)
            .count();
        // Providers with non-CJK-optimized vocabularies must over-estimate
        // CJK: one token per character is not enough, so the profile prices
        // wide characters at two.
        for model in ["gpt-5", "claude-sonnet", "unknown"] {
            let estimate = TokenEstimator::for_model(model).estimate(text);
            assert!(
                estimate >= text.chars().count(),
                "{model}: {estimate} below character count"
            );
        }
        // CJK-optimized profiles are tighter but still account for every wide
        // character.
        let cjk = TokenEstimator::for_model("deepseek-flash").estimate(text);
        assert!(cjk >= wide_chars, "{cjk} below wide-character count");
        let generic = TokenEstimator::generic().estimate(text);
        assert!(cjk <= generic);
    }

    #[test]
    fn profiles_are_selected_by_model_family() {
        assert_eq!(
            TokenEstimator::for_model("deepseek-reasoner").profile(),
            TokenProfile::CjkOptimized
        );
        assert_eq!(
            TokenEstimator::for_model("claude-sonnet-4").profile(),
            TokenProfile::Anthropic
        );
        assert_eq!(
            TokenEstimator::for_model("gpt-5-mini").profile(),
            TokenProfile::OpenAi
        );
        assert_eq!(
            TokenEstimator::for_model("something-local").profile(),
            TokenProfile::Generic
        );
    }

    #[test]
    fn message_and_tool_estimates_include_framing() {
        let estimator = TokenEstimator::generic();
        let messages = vec![ModelMessage::text("user", "hello")];
        assert!(estimator.estimate_messages(&messages) > estimator.estimate("hello"));
        let tools = vec![ToolDefinition {
            name: "read_file".into(),
            description: "read a file".into(),
            input_schema: json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
        }];
        assert!(estimator.estimate_tools(&tools) > estimator.estimate("read_file"));
        assert_eq!(estimator.estimate_messages(&[]), 0);
        assert_eq!(estimator.estimate_tools(&[]), 0);
    }

    #[test]
    fn estimate_messages_counts_reasoning_and_tool_arguments() {
        let estimator = TokenEstimator::generic();
        let plain = vec![ModelMessage {
            role: "assistant".into(),
            content: "thinking".into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
        }];
        let rich = vec![ModelMessage {
            role: "assistant".into(),
            content: "thinking".into(),
            tool_calls: vec![latch_protocol::ToolCall {
                id: "c1".into(),
                name: "write".into(),
                arguments: json!({"path":"src/lib.rs","content":"a".repeat(4000)}),
            }],
            tool_call_id: None,
            reasoning_content: Some("deep reasoning ".repeat(200)),
        }];
        let plain_tokens = estimator.estimate_messages(&plain);
        let rich_tokens = estimator.estimate_messages(&rich);
        assert!(
            rich_tokens > plain_tokens + 1_000,
            "tool arguments and replayed reasoning must be priced: {plain_tokens} vs {rich_tokens}"
        );
    }
}
