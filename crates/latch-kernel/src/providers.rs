//! Provider registry and model catalog.
//!
//! One centralized layer answers provider/capability/model questions so the
//! kernel loop, CLI, and TUI never inspect base URLs, DeepSeek model names, or
//! wire parameters. The layering is:
//!
//! ```text
//! Provider configuration
//!         -> Provider capabilities
//!         -> Model catalog / metadata
//!         -> InferenceProfile
//!         -> Agent runtime
//!         -> Provider adapter / wire format
//! ```
//!
//! User configuration overrides built-in metadata, which overrides the
//! conservative provider default. Unknown models stay unknown: no invented
//! context window, pricing, cache semantics, or reasoning parameters.

use crate::config::{
    Config, EffortMapping, ModelConfig, ProviderKind, ProviderProfileConfig, ReasoningReplayPolicy,
    TransportKind,
};
use crate::credentials::{CredentialRef, CredentialStore};
use crate::provider::{
    AnthropicProvider, ModelProvider, OpenAiProvider, OpenAiResponsesProvider, ReasoningReplay,
    ThinkingToggle,
};
use anyhow::{Result, anyhow, bail};
use latch_protocol::{InferenceProfile, ModelPricing, ProviderId, ReasoningEffort};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

/// Static, conservative facts about one provider wire family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub kind: ProviderKind,
    pub display_name: &'static str,
    pub default_base_url: &'static str,
    pub default_credential: &'static str,
    /// Whether the adapter attaches the stable `x-opencode-session` header.
    pub session_header: bool,
    /// Whether the provider exposes a reliable model-list endpoint. Latch only
    /// records the capability today; remote discovery is a later extension.
    pub model_discovery: bool,
}

impl ProviderCapabilities {
    #[must_use]
    pub fn for_kind(kind: ProviderKind) -> Self {
        Self {
            kind,
            display_name: kind.display_name(),
            default_base_url: kind.default_base_url(),
            default_credential: kind.default_credential(),
            session_header: matches!(kind, ProviderKind::OpenCodeGo),
            model_discovery: false,
        }
    }
}

/// Fully resolved metadata for one model on one provider. Every field may be
/// unknown (`None` / empty), and an unknown field stays unknown.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelDescriptor {
    pub provider: ProviderId,
    pub model: String,
    pub display_name: String,
    /// `None` means unknown; callers use the conservative fallback window.
    pub context_window_tokens: Option<usize>,
    /// Efforts the model actually accepts. Empty means the only selectable
    /// state is [`ReasoningEffort::ProviderDefault`].
    pub supported_efforts: Vec<ReasoningEffort>,
    /// The level the provider uses when no effort is sent. Display only; the
    /// adapter still omits the field for [`ReasoningEffort::ProviderDefault`].
    pub default_effort: ReasoningEffort,
    pub reasoning_replay: ReasoningReplay,
    /// Whether this model accepts Anthropic adaptive thinking plus an
    /// `output_config.effort` control.
    pub adaptive_thinking: bool,
    pub transport: TransportKind,
    /// Explicit user per-level wire mapping. Empty means the transport adapter
    /// uses its built-in mapping, which is the normal path for every catalog
    /// model. Resolution stays at the adapter boundary.
    pub effort_map: BTreeMap<ReasoningEffort, EffortMapping>,
    /// Provider-neutral input modalities the model accepts. Always contains
    /// `Text`; `Image` is explicit metadata, never inferred from a model name.
    pub input_modalities: Vec<latch_protocol::InputModality>,
    pub pricing: Option<ModelPricing>,
    pub aliases: Vec<String>,
    /// True when metadata comes from the built-in catalog or explicit user
    /// configuration; false for the conservative unknown-model fallback.
    pub known: bool,
    /// A built-in transport or an explicit user transport resolves the model.
    /// Discovered ids and display-only custom entries stay in setup only.
    pub resolved: bool,
}

impl ModelDescriptor {
    /// Whether the model accepts image input.
    #[must_use]
    pub fn supports_image_input(&self) -> bool {
        self.input_modalities
            .contains(&latch_protocol::InputModality::Image)
    }
    #[must_use]
    pub fn supports_effort(&self, effort: ReasoningEffort) -> bool {
        matches!(effort, ReasoningEffort::ProviderDefault)
            || self.supported_efforts.contains(&effort)
    }

    /// The effort actually used for a request: the requested value when
    /// supported, otherwise the model default. Unknown/unsupported values are
    /// never sent on the wire.
    #[must_use]
    pub fn effective_effort(&self, requested: ReasoningEffort) -> ReasoningEffort {
        if self.supports_effort(requested) {
            requested
        } else {
            self.default_effort
        }
    }

    /// Effort highlighted when a model is first selected. The model default is
    /// only highlightable when it is an explicit supported level; otherwise the
    /// provider default is the safe starting point.
    #[must_use]
    pub fn preferred_effort(&self) -> ReasoningEffort {
        match self.default_effort {
            ReasoningEffort::ProviderDefault => ReasoningEffort::ProviderDefault,
            explicit if self.supported_efforts.contains(&explicit) => explicit,
            _ => ReasoningEffort::ProviderDefault,
        }
    }

    /// Selectable efforts for the UI, ordered shallow to deep, always
    /// including provider default.
    #[must_use]
    pub fn selectable_efforts(&self) -> Vec<ReasoningEffort> {
        let mut efforts = vec![ReasoningEffort::ProviderDefault];
        for effort in ReasoningEffort::LEVELS {
            if self.supported_efforts.contains(&effort) {
                efforts.push(effort);
            }
        }
        efforts
    }

    /// The conservative descriptor used when neither the built-in catalog nor
    /// user configuration knows the model.
    #[must_use]
    pub fn unknown(provider: &ProviderId, kind: ProviderKind, model: &str) -> Self {
        Self {
            provider: provider.clone(),
            model: model.to_owned(),
            display_name: model.to_owned(),
            context_window_tokens: None,
            supported_efforts: Vec::new(),
            default_effort: ReasoningEffort::ProviderDefault,
            reasoning_replay: conservative_replay(kind, model),
            adaptive_thinking: false,
            transport: kind.default_transport(),
            effort_map: BTreeMap::new(),
            input_modalities: vec![latch_protocol::InputModality::Text],
            pricing: None,
            aliases: Vec::new(),
            known: false,
            resolved: false,
        }
    }
}

/// Conservative fallback for models the catalog does not know. This is the
/// only place a broad family heuristic is allowed: explicit built-in or user
/// metadata always wins over it.
fn conservative_replay(kind: ProviderKind, model: &str) -> ReasoningReplay {
    let model = model.to_ascii_lowercase();
    match kind {
        ProviderKind::DeepSeek => ReasoningReplay::Replay,
        ProviderKind::OpenCodeGo | ProviderKind::OpenCodeZen => {
            if model.contains("deepseek") {
                ReasoningReplay::Replay
            } else {
                ReasoningReplay::Omit
            }
        }
        _ => ReasoningReplay::Omit,
    }
}

/// One built-in model row. Only stable, publicly documented facts are listed;
/// pricing is deliberately absent because Latch never invents prices.
///
/// Sources (checked 2026-09):
/// - OpenAI: developers.openai.com/api/docs/models (context windows and effort
///   sets per model) and the reasoning guide. Context values are PUBLIC API
///   model windows (currently 1,050,000 for the GPT-5.4+ generation), not the
///   smaller Codex product/deployment input limits; do not copy a deployment
///   limit into this table. The Responses transport is required for tool
///   calling with reasoning effort on GPT-5.4 and later.
/// - Anthropic: platform.claude.com models overview + effort/thinking docs.
/// - DeepSeek: api-docs.deepseek.com/api/create-chat-completion lists the exact
///   allowed `model` values `deepseek-flash` and `deepseek-v4-pro`, and
///   /quick_start/pricing gives 1M context and the thinking/effort rules.
///   Retired product names are accepted aliases only (see the footnote), never
///   canonical IDs.
/// - OpenCode Go: opencode.ai/docs/go current model list and endpoint table,
///   which lists a transport per model (Chat Completions, Responses, or
///   Messages). Per-model effort sets come from the OpenCode model catalog's
///   `opencode-go` reasoning options; context windows stay unknown.
#[derive(Clone)]
struct BuiltinModel {
    id: &'static str,
    display_name: &'static str,
    context_window_tokens: Option<usize>,
    efforts: &'static [ReasoningEffort],
    default_effort: ReasoningEffort,
    transport: TransportKind,
    adaptive_thinking: bool,
    replay: ReasoningReplay,
    aliases: &'static [&'static str],
    /// Explicit image-input capability from the provider's official model
    /// documentation. Never inferred from the model name or transport.
    image: bool,
}

const NONE: ReasoningEffort = ReasoningEffort::None;
const MINIMAL: ReasoningEffort = ReasoningEffort::Minimal;
const LOW: ReasoningEffort = ReasoningEffort::Low;
const MEDIUM: ReasoningEffort = ReasoningEffort::Medium;
const HIGH: ReasoningEffort = ReasoningEffort::High;
const XHIGH: ReasoningEffort = ReasoningEffort::XHigh;
const MAX: ReasoningEffort = ReasoningEffort::Max;

/// Current OpenAI API reasoning models. The Responses API is required for tool
/// calling with reasoning effort on GPT-5.4 and later, so every built-in uses
/// the Responses transport. Context windows are the public API model windows
/// (1,050,000 since the GPT-5.4 generation — deliberately not the 272K
/// Codex/deployment input threshold, which also appears in pricing tiers).
/// Effort sets and defaults follow each model's official page: Astra supports
/// low..max (no `none`), the GPT-5.6 family supports none..max and defaults to
/// medium, GPT-5.5 defaults to medium, and GPT-5.4 defaults to none.
const OPENAI_PUBLIC_CONTEXT_WINDOW: usize = 1_050_000;

fn builtin_openai() -> Vec<BuiltinModel> {
    let responses = TransportKind::Responses;
    vec![
        BuiltinModel {
            id: "gpt-6-astra",
            display_name: "GPT-6 Astra",
            context_window_tokens: Some(OPENAI_PUBLIC_CONTEXT_WINDOW),
            efforts: &[LOW, MEDIUM, HIGH, XHIGH, MAX],
            // The model page does not state a default; omit the field and let
            // the API decide rather than guessing.
            default_effort: ReasoningEffort::ProviderDefault,
            transport: responses,
            adaptive_thinking: false,
            replay: ReasoningReplay::Omit,
            image: true,
            aliases: &[],
        },
        BuiltinModel {
            id: "gpt-5.6-sol",
            display_name: "GPT-5.6 Sol",
            context_window_tokens: Some(OPENAI_PUBLIC_CONTEXT_WINDOW),
            efforts: &[NONE, LOW, MEDIUM, HIGH, XHIGH, MAX],
            default_effort: MEDIUM,
            transport: responses,
            adaptive_thinking: false,
            replay: ReasoningReplay::Omit,
            image: true,
            aliases: &["gpt-5.6"],
        },
        BuiltinModel {
            id: "gpt-5.6-terra",
            display_name: "GPT-5.6 Terra",
            context_window_tokens: Some(OPENAI_PUBLIC_CONTEXT_WINDOW),
            efforts: &[NONE, LOW, MEDIUM, HIGH, XHIGH, MAX],
            default_effort: MEDIUM,
            transport: responses,
            adaptive_thinking: false,
            replay: ReasoningReplay::Omit,
            image: true,
            aliases: &[],
        },
        BuiltinModel {
            id: "gpt-5.6-luna",
            display_name: "GPT-5.6 Luna",
            context_window_tokens: Some(OPENAI_PUBLIC_CONTEXT_WINDOW),
            efforts: &[NONE, LOW, MEDIUM, HIGH, XHIGH, MAX],
            default_effort: MEDIUM,
            transport: responses,
            adaptive_thinking: false,
            replay: ReasoningReplay::Omit,
            image: true,
            aliases: &[],
        },
        BuiltinModel {
            id: "gpt-5.5",
            display_name: "GPT-5.5",
            context_window_tokens: Some(OPENAI_PUBLIC_CONTEXT_WINDOW),
            efforts: &[NONE, LOW, MEDIUM, HIGH, XHIGH],
            default_effort: MEDIUM,
            transport: responses,
            adaptive_thinking: false,
            replay: ReasoningReplay::Omit,
            image: true,
            aliases: &[],
        },
        BuiltinModel {
            id: "gpt-5.4",
            display_name: "GPT-5.4",
            context_window_tokens: Some(OPENAI_PUBLIC_CONTEXT_WINDOW),
            efforts: &[NONE, LOW, MEDIUM, HIGH, XHIGH],
            default_effort: NONE,
            transport: responses,
            adaptive_thinking: false,
            replay: ReasoningReplay::Omit,
            image: true,
            aliases: &[],
        },
    ]
}

/// Current Anthropic models. Adaptive thinking plus `output_config.effort`
/// covers Opus 5 / Sonnet 5 / Fable 5.1; Haiku 4.5 only supports manual
/// extended thinking, which Latch does not configure, so it advertises no
/// effort. Thinking blocks must be echoed back unchanged (the adapter does).
fn builtin_anthropic() -> Vec<BuiltinModel> {
    let messages = TransportKind::AnthropicMessages;
    let thinking = [
        BuiltinModel {
            id: "claude-fable-5-1",
            display_name: "Claude Fable 5.1",
            context_window_tokens: Some(1_000_000),
            efforts: &[LOW, MEDIUM, HIGH, XHIGH, MAX],
            default_effort: HIGH,
            transport: messages,
            adaptive_thinking: true,
            replay: ReasoningReplay::Replay,
            image: true,
            aliases: &[],
        },
        BuiltinModel {
            id: "claude-opus-5",
            display_name: "Claude Opus 5",
            context_window_tokens: Some(1_000_000),
            efforts: &[LOW, MEDIUM, HIGH, XHIGH, MAX],
            default_effort: HIGH,
            transport: messages,
            adaptive_thinking: true,
            replay: ReasoningReplay::Replay,
            image: true,
            aliases: &[],
        },
        BuiltinModel {
            id: "claude-sonnet-5",
            display_name: "Claude Sonnet 5",
            context_window_tokens: Some(1_000_000),
            efforts: &[LOW, MEDIUM, HIGH, XHIGH, MAX],
            default_effort: HIGH,
            transport: messages,
            adaptive_thinking: true,
            replay: ReasoningReplay::Replay,
            image: true,
            aliases: &[],
        },
    ];
    let mut models = thinking.to_vec();
    models.push(BuiltinModel {
        id: "claude-opus-4-8",
        display_name: "Claude Opus 4.8",
        context_window_tokens: Some(1_000_000),
        efforts: &[LOW, MEDIUM, HIGH, XHIGH, MAX],
        default_effort: HIGH,
        transport: messages,
        adaptive_thinking: true,
        replay: ReasoningReplay::Replay,
        image: true,
        aliases: &[],
    });
    models.push(BuiltinModel {
        id: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        context_window_tokens: Some(1_000_000),
        efforts: &[LOW, MEDIUM, HIGH, MAX],
        default_effort: HIGH,
        transport: messages,
        adaptive_thinking: true,
        replay: ReasoningReplay::Replay,
        image: true,
        aliases: &[],
    });
    models.push(BuiltinModel {
        id: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        context_window_tokens: Some(200_000),
        efforts: &[],
        default_effort: ReasoningEffort::ProviderDefault,
        transport: messages,
        adaptive_thinking: false,
        replay: ReasoningReplay::Replay,
        image: true,
        aliases: &["claude-haiku-4-5-20251001"],
    });
    models
}

/// Official DeepSeek API models. The Chat Completions request schema
/// (api-docs.deepseek.com/api/create-chat-completion) documents exactly two
/// allowed `model` values: `deepseek-flash` and `deepseek-v4-pro`, and the
/// `/models` response example lists the same two. The Models & Pricing
/// footnote and the Vision guide state that the legacy names
/// `deepseek-v4-flash` and `deepseek-v4-flash-vision-exp` are still accepted
/// but their models have been retired: those requests are served by the
/// current DeepSeek-V4.1-Flash model, which itself accepts images and is the
/// canonical `deepseek-flash`. Aliasing them is therefore wire-exact, not a
/// collapse of a separately selectable vision model. `deepseek-chat`/
/// `deepseek-reasoner` and Latch-era `deepseek-v4.1*` names are kept only for
/// existing configs.
///
/// Thinking is on by default (`thinking.type: enabled`), `reasoning_effort`
/// accepts none/low/high/max, tools require full `reasoning_content` replay,
/// and the context window is 1M.
fn builtin_deepseek() -> Vec<BuiltinModel> {
    let chat = TransportKind::ChatCompletions;
    vec![
        BuiltinModel {
            id: "deepseek-flash",
            display_name: "DeepSeek Flash",
            context_window_tokens: Some(1_048_576),
            efforts: &[NONE, LOW, HIGH, MAX],
            default_effort: HIGH,
            transport: chat,
            adaptive_thinking: false,
            replay: ReasoningReplay::Replay,
            image: true,
            aliases: &[
                "deepseek-v4-flash",
                "deepseek-v4-flash-vision-exp",
                "deepseek-v4.1-flash",
                "deepseek-v4.1",
                "deepseek-chat",
                "deepseek-reasoner",
            ],
        },
        BuiltinModel {
            id: "deepseek-v4-pro",
            display_name: "DeepSeek V4 Pro",
            context_window_tokens: Some(1_048_576),
            efforts: &[NONE, LOW, HIGH, MAX],
            default_effort: HIGH,
            transport: chat,
            adaptive_thinking: false,
            replay: ReasoningReplay::Replay,
            image: false,
            aliases: &[],
        },
    ]
}

/// One OpenCode Go catalog row. OpenCode Go publishes a transport per
/// model and the current selectable list, but not context windows, so the
/// catalog carries only what is documented and leaves the window unknown
/// rather than copying either limit.
fn opencode_go_model(
    id: &'static str,
    display_name: &'static str,
    transport: TransportKind,
    efforts: &'static [ReasoningEffort],
    default_effort: ReasoningEffort,
    replay: ReasoningReplay,
    aliases: &'static [&'static str],
) -> BuiltinModel {
    BuiltinModel {
        id,
        display_name,
        context_window_tokens: None,
        efforts,
        default_effort,
        transport,
        adaptive_thinking: false,
        replay,
        image: false,
        aliases,
    }
}

/// OpenCode Go's current documented catalog (opencode.ai/docs/go). Effort
/// sets follow the OpenCode model catalog's per-model `reasoning_options`
/// for `opencode-go`, which is the metadata the OpenCode client itself
/// uses; the Go docs publish transports but not effort sets. DeepSeek
/// models keep required reasoning replay, and every other family does not
/// inherit it. Image input stays conservative text-only: the gateway
/// rejects images for models whose upstream accepts them, so a model that
/// actually serves vision can be opted in per model with
/// `input_modalities = ["text", "image"]`.
fn builtin_opencode_go() -> Vec<BuiltinModel> {
    let chat = TransportKind::ChatCompletions;
    let messages = TransportKind::AnthropicMessages;
    let responses = TransportKind::Responses;
    let mut models = vec![
        // Grok 4.7
        opencode_go_model(
            "grok-4.7",
            "Grok 4.7",
            responses,
            &[LOW, MEDIUM, HIGH, XHIGH],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Grok 4.6
        opencode_go_model(
            "grok-4.6",
            "Grok 4.6",
            responses,
            &[LOW, MEDIUM, HIGH, XHIGH],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // GPT-6 Luna
        opencode_go_model(
            "gpt-6-luna",
            "GPT-6 Luna",
            responses,
            &[NONE, LOW, MEDIUM, HIGH, XHIGH, MAX],
            MEDIUM,
            ReasoningReplay::Omit,
            &[],
        ),
        // GPT-5.6 Luna
        opencode_go_model(
            "gpt-5.6-luna",
            "GPT-5.6 Luna",
            responses,
            &[NONE, LOW, MEDIUM, HIGH, XHIGH, MAX],
            MEDIUM,
            ReasoningReplay::Omit,
            &[],
        ),
        // GLM-5.3-Flash
        opencode_go_model(
            "glm-5.3-flash",
            "GLM-5.3-Flash",
            chat,
            &[LOW, HIGH, MAX],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // GLM-5.3
        opencode_go_model(
            "glm-5.3",
            "GLM-5.3",
            chat,
            &[LOW, HIGH, MAX],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // GLM-5.2
        opencode_go_model(
            "glm-5.2",
            "GLM-5.2",
            chat,
            &[HIGH, MAX],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // GLM-5.1
        opencode_go_model(
            "glm-5.1",
            "GLM-5.1",
            chat,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Kimi K3
        opencode_go_model(
            "kimi-k3",
            "Kimi K3",
            chat,
            &[MAX],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Kimi K2.7 Code
        opencode_go_model(
            "kimi-k2.7-code",
            "Kimi K2.7 Code",
            chat,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Kimi K2.6
        opencode_go_model(
            "kimi-k2.6",
            "Kimi K2.6",
            chat,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // LongCat-2.0
        opencode_go_model(
            "longcat-2.0",
            "LongCat-2.0",
            chat,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // DeepSeek V4.1 Flash
        opencode_go_model(
            "deepseek-v4.1-flash",
            "DeepSeek V4.1 Flash",
            chat,
            &[LOW, HIGH, MAX],
            HIGH,
            ReasoningReplay::Replay,
            &[],
        ),
        // DeepSeek V4 Pro
        opencode_go_model(
            "deepseek-v4-pro",
            "DeepSeek V4 Pro",
            chat,
            &[HIGH, MAX],
            HIGH,
            ReasoningReplay::Replay,
            &[],
        ),
        // DeepSeek V4 Flash
        opencode_go_model(
            "deepseek-v4-flash",
            "DeepSeek V4 Flash",
            chat,
            &[LOW, HIGH, MAX],
            HIGH,
            ReasoningReplay::Replay,
            &["deepseek-v4.1", "deepseek-flash"],
        ),
        // DeepSeek V4 Flash Vision Exp
        opencode_go_model(
            "deepseek-v4-flash-vision-exp",
            "DeepSeek V4 Flash Vision Exp",
            chat,
            &[LOW, HIGH, MAX],
            HIGH,
            ReasoningReplay::Replay,
            &[],
        ),
        // MiMo-V2.6-Flash
        opencode_go_model(
            "mimo-v2.6-flash",
            "MiMo-V2.6-Flash",
            chat,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // MiMo-V2.6-Pro
        opencode_go_model(
            "mimo-v2.6-pro",
            "MiMo-V2.6-Pro",
            chat,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // MiMo-V2.5
        opencode_go_model(
            "mimo-v2.5",
            "MiMo-V2.5",
            chat,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // MiMo-V2.5-Pro
        opencode_go_model(
            "mimo-v2.5-pro",
            "MiMo-V2.5-Pro",
            chat,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // MiniMax M3
        opencode_go_model(
            "minimax-m3",
            "MiniMax M3",
            messages,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // MiniMax M2.7
        opencode_go_model(
            "minimax-m2.7",
            "MiniMax M2.7",
            messages,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // MiniMax M2.5
        opencode_go_model(
            "minimax-m2.5",
            "MiniMax M2.5",
            messages,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Muse Spark 1.3 Contributor
        opencode_go_model(
            "muse-spark-1.3-contributor",
            "Muse Spark 1.3 Contributor",
            responses,
            &[MINIMAL, LOW, MEDIUM, HIGH, XHIGH],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Muse Spark 1.2 Contributor
        opencode_go_model(
            "muse-spark-1.2-contributor",
            "Muse Spark 1.2 Contributor",
            responses,
            &[MINIMAL, LOW, MEDIUM, HIGH, XHIGH],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Qwen3.8 Max
        opencode_go_model(
            "qwen3.8-max",
            "Qwen3.8 Max",
            messages,
            &[LOW, MEDIUM, XHIGH],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Qwen3.8 Flash
        opencode_go_model(
            "qwen3.8-flash",
            "Qwen3.8 Flash",
            messages,
            &[LOW, MEDIUM, XHIGH],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Qwen3.7 Max
        opencode_go_model(
            "qwen3.7-max",
            "Qwen3.7 Max",
            messages,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Qwen3.7 Plus
        opencode_go_model(
            "qwen3.7-plus",
            "Qwen3.7 Plus",
            messages,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Qwen3.6 Plus
        opencode_go_model(
            "qwen3.6-plus",
            "Qwen3.6 Plus",
            messages,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Hy4 preview
        opencode_go_model(
            "hy4-preview",
            "Hy4 preview",
            chat,
            &[NONE, HIGH],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Hy3
        opencode_go_model(
            "hy3",
            "Hy3",
            chat,
            &[NONE, LOW, HIGH],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
        // Space Bunny Free
        opencode_go_model(
            "space-bunny-free",
            "Space Bunny Free",
            chat,
            &[LOW, MEDIUM, HIGH, XHIGH, MAX],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ),
    ];
    // Keep the list stable and readable in UI order.
    models.sort_by(|a, b| a.id.cmp(b.id));
    models
}

/// Zen's documented endpoint table maps each listed model to an existing
/// transport. Deprecated rows are omitted; Gemini rows are added by the Gemini
/// transport commit, and System One (`jev-*`) is omitted because it needs a
/// distinct non-conversational transport. Source: opencode.ai/docs/en/zen/.
fn builtin_opencode_zen() -> Vec<BuiltinModel> {
    let mut models = Vec::new();
    for id in [
        "gpt-6-astra",
        "gpt-6-sol",
        "gpt-6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
        "gpt-5.5-pro",
        "gpt-5.4",
        "gpt-5.4-pro",
        "gpt-5.4-mini",
        "gpt-5.4-nano",
        "gpt-5.3-codex",
        "gpt-5.3-codex-spark",
        "gpt-5.2",
        "gpt-5.1",
        "gpt-5",
        "gpt-5-nano",
        "grok-4.7",
        "grok-4.6",
        "grok-4.5",
        "grok-build-0.1",
        "muse-spark-1.3",
        "muse-spark-1.2",
        "muse-spark-1.3-contributor-free",
    ] {
        models.push(opencode_go_model(
            id,
            id,
            TransportKind::Responses,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ));
    }
    for id in [
        "claude-fable-5-1",
        "claude-fable-5",
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-opus-4-5",
        "claude-sonnet-5",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
    ] {
        models.push(opencode_go_model(
            id,
            id,
            TransportKind::AnthropicMessages,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Replay,
            &[],
        ));
    }
    for id in [
        "qwen3.8-flash",
        "qwen3.7-max",
        "qwen3.7-plus",
        "qwen3.6-plus",
        "qwen3.5-plus",
    ] {
        models.push(opencode_go_model(
            id,
            id,
            TransportKind::AnthropicMessages,
            &[],
            ReasoningEffort::ProviderDefault,
            ReasoningReplay::Omit,
            &[],
        ));
    }
    for id in [
        "qwen3.8-max",
        "deepseek-v4.1-flash",
        "deepseek-v4-pro",
        "deepseek-v4-flash",
        "deepseek-v4-flash-vision-exp",
        "minimax-m3",
        "minimax-m2.7",
        "glm-5.3-flash",
        "glm-5.3",
        "glm-5.2",
        "glm-5.1",
        "kimi-k2.6",
        "kimi-k2.7-code",
        "kimi-k3",
        "big-pickle",
        "space-bunny-free",
        "mimo-v2.6-flash-free",
        "mimo-v2.5-free",
        "ling-3.0-flash-fin-free",
        "nemotron-3-ultra-free",
        "nemotron-3.5-lightning-free",
    ] {
        let replay = if id.starts_with("deepseek-") {
            ReasoningReplay::Replay
        } else {
            ReasoningReplay::Omit
        };
        let mut model = opencode_go_model(
            id,
            id,
            TransportKind::ChatCompletions,
            &[],
            ReasoningEffort::ProviderDefault,
            replay,
            &[],
        );
        model.image = id == "deepseek-v4-flash-vision-exp";
        models.push(model);
    }
    models.sort_by(|a, b| a.id.cmp(b.id));
    models
}

fn builtin_models(kind: ProviderKind) -> Vec<BuiltinModel> {
    match kind {
        ProviderKind::OpenAi => builtin_openai(),
        ProviderKind::Anthropic => builtin_anthropic(),
        ProviderKind::DeepSeek => builtin_deepseek(),
        ProviderKind::OpenCodeGo => builtin_opencode_go(),
        ProviderKind::OpenCodeZen => builtin_opencode_zen(),
        ProviderKind::OpenAiCompatible => Vec::new(),
    }
}

fn builtin_descriptor(
    provider: &ProviderId,
    kind: ProviderKind,
    builtin: &BuiltinModel,
) -> ModelDescriptor {
    ModelDescriptor {
        provider: provider.clone(),
        model: builtin.id.to_owned(),
        display_name: builtin.display_name.to_owned(),
        context_window_tokens: builtin.context_window_tokens,
        supported_efforts: builtin.efforts.to_vec(),
        default_effort: builtin.default_effort,
        reasoning_replay: builtin.replay,
        adaptive_thinking: builtin.adaptive_thinking,
        transport: builtin.transport,
        effort_map: BTreeMap::new(),
        input_modalities: if builtin.image {
            vec![
                latch_protocol::InputModality::Text,
                latch_protocol::InputModality::Image,
            ]
        } else {
            vec![latch_protocol::InputModality::Text]
        },
        pricing: None,
        aliases: builtin
            .aliases
            .iter()
            .map(|alias| (*alias).to_owned())
            .collect(),
        known: true,
        resolved: true,
    }
    .with_provider_defaults(kind)
}

impl ModelDescriptor {
    /// Applies kind-level fallbacks without overriding explicit model facts.
    fn with_provider_defaults(mut self, kind: ProviderKind) -> Self {
        if self.transport == TransportKind::ChatCompletions && kind == ProviderKind::OpenAi {
            // OpenAI custom/unknown models default to Responses; explicit
            // catalog rows always carry their own transport.
            self.transport = TransportKind::Responses;
        }
        self
    }
}

/// The built-in catalog for one provider kind, independent of configuration.
/// Used by `/setup` to offer a small, stable starting set of models.
#[must_use]
pub fn builtin_catalog(kind: ProviderKind) -> Vec<ModelDescriptor> {
    let provider = ProviderId::new(kind.id());
    builtin_models(kind)
        .iter()
        .map(|builtin| builtin_descriptor(&provider, kind, builtin))
        .collect()
}

/// One resolved provider instance: identity, endpoint, credential reference,
/// and the merged model catalog.
#[derive(Debug, Clone)]
pub struct ProviderProfile {
    pub id: ProviderId,
    pub display_name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub credential: CredentialRef,
    pub default_model: String,
    pub capabilities: ProviderCapabilities,
    pub model_discovery: bool,
    enabled_models: Option<Vec<String>>,
    models: BTreeMap<String, ModelDescriptor>,
    aliases: BTreeMap<String, String>,
}

impl ProviderProfile {
    /// Merges built-in metadata, user model overrides, and the conservative
    /// fallback with precedence: explicit user config > built-in > conservative.
    fn build(
        id: &str,
        entry: &ProviderProfileConfig,
        global_models: &BTreeMap<String, ModelConfig>,
    ) -> Result<Self> {
        let kind = entry.kind;
        let provider_id = ProviderId::new(id);
        let base_url = entry
            .base_url
            .clone()
            .map(|url| url.trim_end_matches('/').to_owned())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| kind.default_base_url().to_owned());
        if base_url.is_empty() {
            bail!("provider {id} needs a base_url");
        }
        let credential: CredentialRef = entry
            .credential
            .clone()
            .unwrap_or_else(|| kind.default_credential().to_owned())
            .parse()
            .map_err(|error| anyhow!("provider {id}: {error}"))?;

        let mut models: BTreeMap<String, ModelDescriptor> = BTreeMap::new();
        let mut aliases: BTreeMap<String, String> = BTreeMap::new();
        for builtin in builtin_models(kind) {
            let descriptor = builtin_descriptor(&provider_id, kind, &builtin);
            for alias in &descriptor.aliases {
                aliases.insert(alias.clone(), builtin.id.to_owned());
            }
            models.insert(builtin.id.to_owned(), descriptor);
        }
        // User entries can extend the catalog with models the built-in table
        // does not know, and can override built-in metadata field by field.
        let mut names: Vec<&String> = entry.models.keys().collect();
        for name in global_models.keys() {
            if !entry.models.contains_key(name) {
                names.push(name);
            }
        }
        for name in names {
            let user = entry.models.get(name).or_else(|| global_models.get(name));
            let Some(user) = user else { continue };
            let mut descriptor = models
                .get(name)
                .cloned()
                .unwrap_or_else(|| ModelDescriptor::unknown(&provider_id, kind, name));
            apply_user_metadata(&mut descriptor, user);
            descriptor.known = true;
            descriptor.resolved |= user.transport.is_some();
            for alias in &descriptor.aliases {
                aliases.insert(alias.clone(), name.clone());
            }
            models.insert(name.clone(), descriptor);
        }

        if let Some(selected) = &entry.enabled_models {
            if selected.is_empty() {
                bail!("provider {id} must enable at least one model");
            }
            for model in selected {
                if !models.contains_key(model) {
                    bail!(
                        "provider {id} enables unknown model {model:?}; add a custom model override first"
                    );
                }
            }
        }

        let default_model = entry
            .default_model
            .clone()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_default();
        if entry
            .enabled_models
            .as_ref()
            .is_some_and(|selected| !selected.contains(&default_model))
        {
            bail!("provider {id} default_model {default_model:?} is not enabled");
        }
        Ok(Self {
            id: provider_id,
            display_name: entry
                .display_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| kind.display_name().to_owned()),
            kind,
            base_url,
            credential,
            default_model,
            capabilities: ProviderCapabilities::for_kind(kind),
            model_discovery: entry.model_discovery,
            enabled_models: entry.enabled_models.clone(),
            models,
            aliases,
        })
    }

    /// Canonical model id for a name or alias.
    #[must_use]
    pub fn resolve_model_name(&self, model: &str) -> String {
        let trimmed = model.trim();
        self.aliases
            .get(trimmed)
            .cloned()
            .unwrap_or_else(|| trimmed.to_owned())
    }

    /// Metadata for a model. Unknown models get the conservative descriptor
    /// instead of an invented one.
    #[must_use]
    pub fn model_descriptor(&self, model: &str) -> Option<ModelDescriptor> {
        let canonical = self.resolve_model_name(model);
        if canonical.trim().is_empty() {
            return None;
        }
        if self
            .enabled_models
            .as_ref()
            .is_some_and(|selected| !selected.contains(&canonical))
        {
            return None;
        }
        Some(
            self.models
                .get(&canonical)
                .cloned()
                .unwrap_or_else(|| ModelDescriptor::unknown(&self.id, self.kind, &canonical)),
        )
    }

    #[must_use]
    pub fn available_models(&self) -> Vec<&ModelDescriptor> {
        self.models
            .iter()
            .filter(|(id, _)| {
                self.enabled_models
                    .as_ref()
                    .is_none_or(|selected| selected.contains(id))
            })
            .filter(|(_, model)| model.resolved)
            .map(|(_, model)| model)
            .collect()
    }

    /// All catalog and custom entries, including unresolved and disabled
    /// models. The configuration center needs these for selection editing.
    #[must_use]
    pub fn setup_models(&self) -> Vec<(&ModelDescriptor, bool)> {
        self.models
            .iter()
            .map(|(id, model)| {
                (
                    model,
                    self.enabled_models
                        .as_ref()
                        .is_none_or(|selected| selected.contains(id)),
                )
            })
            .collect()
    }

    #[must_use]
    pub fn configured_model_count(&self) -> usize {
        self.models
            .keys()
            .filter(|id| {
                self.enabled_models
                    .as_ref()
                    .is_none_or(|selected| selected.contains(id))
            })
            .count()
    }
}

fn apply_user_metadata(descriptor: &mut ModelDescriptor, user: &ModelConfig) {
    if let Some(name) = &user.display_name {
        descriptor.display_name = name.clone();
    }
    if let Some(window) = user.context_window_tokens {
        descriptor.context_window_tokens = Some(window);
    }
    if let Some(pricing) = &user.pricing {
        descriptor.pricing = Some(pricing.clone());
    }
    if let Some(efforts) = &user.efforts {
        descriptor.supported_efforts = efforts
            .iter()
            .copied()
            .filter(|effort| !matches!(effort, ReasoningEffort::ProviderDefault))
            .collect();
    }
    if let Some(default_effort) = user.default_effort {
        descriptor.default_effort = default_effort;
    }
    if let Some(policy) = user.reasoning_replay {
        descriptor.reasoning_replay = match policy {
            ReasoningReplayPolicy::Replay => ReasoningReplay::Replay,
            ReasoningReplayPolicy::Omit => ReasoningReplay::Omit,
        };
    }
    if let Some(transport) = user.transport {
        descriptor.transport = transport;
    }
    if let Some(adaptive) = user.adaptive_thinking {
        descriptor.adaptive_thinking = adaptive;
    }
    if !user.effort_map.is_empty() {
        // A user map owns the whole exposed range; validation has already
        // checked coverage and transport expressibility.
        descriptor.effort_map = user.effort_map.clone();
    }
    if !user.aliases.is_empty() {
        descriptor.aliases = user.aliases.clone();
    }
    if let Some(modalities) = &user.input_modalities {
        let mut resolved: Vec<latch_protocol::InputModality> = Vec::new();
        for modality in modalities {
            if !resolved.contains(modality) {
                resolved.push(*modality);
            }
        }
        // Text is always available; a user override only adds or removes
        // image input (a model that cannot read text is not representable).
        if !resolved.contains(&latch_protocol::InputModality::Text) {
            resolved.insert(0, latch_protocol::InputModality::Text);
        }
        descriptor.input_modalities = resolved;
    }
}

/// Central provider/model catalog.
#[derive(Clone)]
pub struct ProviderRegistry {
    profiles: BTreeMap<String, ProviderProfile>,
    /// The profile selected when the user has not chosen one.
    default_provider: String,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.profiles.keys().collect::<Vec<_>>())
            .field("default_provider", &self.default_provider)
            .finish()
    }
}

impl ProviderRegistry {
    /// Builds the registry from configuration, migrating the legacy single
    /// `[provider]` table when no `[providers.*]` entries are present.
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut entries: BTreeMap<String, ProviderProfileConfig> = BTreeMap::new();
        if config.providers.is_empty() {
            let legacy = &config.provider;
            let kind = ProviderKind::parse(&legacy.kind, legacy.base_url.as_deref())
                .ok_or_else(|| anyhow!("unsupported provider kind {:?}", legacy.kind))?;
            let credential = legacy
                .api_key_env
                .clone()
                .map(|env| format!("env:{env}"))
                .unwrap_or_else(|| kind.default_credential().to_owned());
            let mut legacy_models = BTreeMap::new();
            if !builtin_models(kind)
                .iter()
                .any(|model| model.id == legacy.model)
            {
                let mut override_ = config
                    .models
                    .get(&legacy.model)
                    .cloned()
                    .unwrap_or_default();
                override_.transport.get_or_insert(kind.default_transport());
                legacy_models.insert(legacy.model.clone(), override_);
            }
            entries.insert(
                kind.id().to_owned(),
                ProviderProfileConfig {
                    kind,
                    display_name: None,
                    base_url: legacy.base_url.clone(),
                    credential: Some(credential),
                    default_model: Some(legacy.model.clone()),
                    enabled_models: None,
                    models: legacy_models,
                    model_discovery: false,
                },
            );
        } else {
            entries = config.providers.clone();
        }
        let mut profiles = BTreeMap::new();
        for (id, entry) in &entries {
            let profile = ProviderProfile::build(id, entry, &config.models)?;
            profiles.insert(id.clone(), profile);
        }
        let default_provider = config
            .inference
            .provider
            .clone()
            .filter(|id| profiles.contains_key(id))
            .or_else(|| profiles.keys().next().cloned())
            .ok_or_else(|| anyhow!("no provider is configured"))?;
        Ok(Self {
            profiles,
            default_provider,
        })
    }

    #[must_use]
    pub fn available_providers(&self) -> Vec<&ProviderProfile> {
        self.profiles.values().collect()
    }

    #[must_use]
    pub fn provider(&self, id: &str) -> Option<&ProviderProfile> {
        self.profiles.get(id)
    }

    #[must_use]
    pub fn default_provider(&self) -> &ProviderProfile {
        self.profiles
            .get(&self.default_provider)
            .expect("default provider exists")
    }

    #[must_use]
    pub fn provider_capabilities(&self, id: &str) -> Option<ProviderCapabilities> {
        self.profiles.get(id).map(|p| p.capabilities.clone())
    }

    #[must_use]
    pub fn available_models(&self, id: &str) -> Vec<ModelDescriptor> {
        self.profiles
            .get(id)
            .map(|p| p.available_models().into_iter().cloned().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn model_descriptor(&self, id: &str, model: &str) -> Option<ModelDescriptor> {
        self.profiles.get(id)?.model_descriptor(model)
    }

    #[must_use]
    pub fn supported_efforts(&self, id: &str, model: &str) -> Vec<ReasoningEffort> {
        self.model_descriptor(id, model)
            .map(|descriptor| descriptor.supported_efforts)
            .unwrap_or_default()
    }

    #[must_use]
    pub fn default_effort(&self, id: &str, model: &str) -> ReasoningEffort {
        self.model_descriptor(id, model)
            .map(|descriptor| descriptor.default_effort)
            .unwrap_or_default()
    }

    /// The configured default inference profile, validated against the catalog.
    pub fn default_profile(&self, config: &Config) -> Result<(InferenceProfile, ModelDescriptor)> {
        let provider = config
            .inference
            .provider
            .as_deref()
            .filter(|id| self.profiles.contains_key(*id))
            .unwrap_or_else(|| self.default_provider().id.as_str());
        let profile_models = self.profiles.get(provider).expect("provider checked above");
        let model = config
            .inference
            .model
            .clone()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| profile_models.default_model.clone());
        let descriptor = profile_models
            .model_descriptor(&model)
            .ok_or_else(|| anyhow!("provider {provider} has no default model"))?;
        if !descriptor.resolved {
            bail!(
                "provider {provider} model {model:?} has unresolved transport; set it in Advanced before activation"
            );
        }
        let effort = descriptor.effective_effort(config.inference.effort);
        Ok((
            InferenceProfile::new(provider, descriptor.model.clone(), effort),
            descriptor,
        ))
    }

    /// Validates a requested profile, resolves aliases, and clamps unsupported
    /// effort values to the model default. Returns the effective profile and
    /// its descriptor.
    pub fn resolve_profile(
        &self,
        requested: &InferenceProfile,
    ) -> Result<(InferenceProfile, ModelDescriptor)> {
        let provider_id = if requested.provider.is_empty() {
            self.default_provider().id.as_str().to_owned()
        } else {
            requested.provider.0.clone()
        };
        let profile = self
            .profiles
            .get(&provider_id)
            .ok_or_else(|| anyhow!("unknown provider {provider_id:?}"))?;
        let model = if requested.model.trim().is_empty() {
            if profile.default_model.is_empty() {
                bail!("provider {provider_id} needs a default_model before it can be selected");
            }
            profile.default_model.clone()
        } else {
            profile.resolve_model_name(&requested.model)
        };
        let descriptor = profile
            .model_descriptor(&model)
            .ok_or_else(|| anyhow!("provider {provider_id} has no selectable model"))?;
        if !descriptor.resolved {
            bail!(
                "provider {provider_id} model {model:?} has unresolved transport; set it in Advanced before activation"
            );
        }
        let effort = descriptor.effective_effort(requested.effort);
        Ok((
            InferenceProfile::new(provider_id, descriptor.model.clone(), effort),
            descriptor,
        ))
    }

    /// Builds the provider adapter for a resolved profile. Credentials are
    /// resolved here and never stored in the profile or the registry. `media`
    /// resolves durable image references only at the wire boundary.
    pub fn build_provider(
        &self,
        profile: &InferenceProfile,
        descriptor: &ModelDescriptor,
        credentials: &CredentialStore,
        session_id: Uuid,
        media: Option<crate::provider::MediaStore>,
    ) -> Result<Arc<dyn ModelProvider>> {
        let provider = self
            .profiles
            .get(profile.provider.as_str())
            .ok_or_else(|| anyhow!("unknown provider {:?}", profile.provider))?;
        let api_key = credentials.require(&provider.credential)?;
        let effort = descriptor.effective_effort(profile.effort);
        let supports_effort = !descriptor.supported_efforts.is_empty();
        // DeepSeek-family reasoning is toggled explicitly: `provider default`
        // omits the field, an explicit level enables thinking, and an explicit
        // `none` disables it (DeepSeek's `reasoning_effort` has no `none`).
        let thinking = if descriptor.reasoning_replay == ReasoningReplay::Replay
            && descriptor.transport == TransportKind::ChatCompletions
        {
            match effort {
                ReasoningEffort::ProviderDefault => ThinkingToggle::Default,
                ReasoningEffort::None => ThinkingToggle::Disabled,
                _ => ThinkingToggle::Enabled,
            }
        } else {
            ThinkingToggle::Default
        };
        let provider_impl: Arc<dyn ModelProvider> = match descriptor.transport {
            TransportKind::AnthropicMessages => Arc::new(
                AnthropicProvider::new(
                    provider.base_url.clone(),
                    api_key,
                    descriptor.model.clone(),
                )
                .with_identity(provider.id.to_string())
                .with_reasoning(effort, supports_effort, descriptor.adaptive_thinking)
                .with_effort_map(descriptor.effort_map.clone())
                .with_session(session_id)
                .with_media(media.clone()),
            ),
            TransportKind::Responses => Arc::new(
                OpenAiResponsesProvider::new(
                    provider.base_url.clone(),
                    api_key,
                    descriptor.model.clone(),
                )
                .with_identity(provider.id.to_string())
                .with_reasoning(effort, supports_effort)
                .with_effort_map(descriptor.effort_map.clone())
                .with_session(session_id)
                .with_media(media.clone()),
            ),
            TransportKind::ChatCompletions => Arc::new(
                OpenAiProvider::new(provider.base_url.clone(), api_key, descriptor.model.clone())
                    .with_identity(provider.id.to_string())
                    .with_reasoning(effort, descriptor.reasoning_replay, supports_effort)
                    .with_thinking(thinking)
                    .with_effort_map(descriptor.effort_map.clone())
                    .with_session(session_id)
                    .with_media(media.clone()),
            ),
        };
        Ok(provider_impl)
    }
}

/// Convenience used by the CLI/`/setup` writer: the canonical provider table
/// for one provider kind with its default base URL and credential reference.
#[must_use]
pub fn canonical_provider_entry(
    kind: ProviderKind,
    base_url: Option<String>,
) -> ProviderProfileConfig {
    ProviderProfileConfig {
        kind,
        display_name: None,
        base_url,
        credential: Some(kind.default_credential().to_owned()),
        default_model: None,
        enabled_models: None,
        models: BTreeMap::new(),
        model_discovery: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn registry(toml: &str) -> (Config, ProviderRegistry) {
        let config: Config = toml::from_str(toml).unwrap();
        let registry = ProviderRegistry::from_config(&config).unwrap();
        (config, registry)
    }

    #[test]
    fn enabled_models_select_without_copying_builtin_metadata() {
        let source = r#"
            [providers.openai]
            kind = "openai"
            default_model = "gpt-5.5"
            enabled_models = ["gpt-5.5"]

            [providers.openai.models."gpt-5.5"]
            display_name = "Preferred GPT"
        "#;
        let (config, registry) = registry(source);
        let selected = registry.available_models("openai");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].model, "gpt-5.5");
        assert_eq!(selected[0].display_name, "Preferred GPT");
        assert!(registry.model_descriptor("openai", "gpt-5.4").is_none());

        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("enabled_models = [\"gpt-5.5\"]"));
        assert!(!text.contains("context_window_tokens"));
        assert!(!text.contains("transport"));
    }

    #[test]
    fn invalid_selections_are_rejected() {
        for selected in ["[]", "[\"invented\"]", "[\"gpt-5.4\"]"] {
            let source = format!(
                "[providers.openai]\nkind = 'openai'\ndefault_model = 'gpt-5.5'\nenabled_models = {selected}\n"
            );
            let config: Config = toml::from_str(&source).unwrap();
            assert!(
                ProviderRegistry::from_config(&config).is_err(),
                "{selected}"
            );
        }
    }

    #[test]
    fn provider_switch_uses_its_configured_default_and_never_catalog_order() {
        let (config, catalog) = registry(
            r#"
            [providers.openai]
            kind = "openai"
            default_model = "gpt-5.5"

            [providers.deepseek]
            kind = "deepseek"
            default_model = "deepseek-v4-pro"

            [inference]
            provider = "openai"
            model = "gpt-5.5"
            "#,
        );
        let (new_session, _) = catalog.default_profile(&config).unwrap();
        assert_eq!(new_session.provider.as_str(), "openai");
        let (switched, _) = catalog
            .resolve_profile(&InferenceProfile::new(
                "deepseek",
                "",
                ReasoningEffort::ProviderDefault,
            ))
            .unwrap();
        assert_eq!(switched.model, "deepseek-v4-pro");

        let (_config, missing_default) = registry("[providers.deepseek]\nkind = 'deepseek'\n");
        let error = missing_default
            .resolve_profile(&InferenceProfile::new(
                "deepseek",
                "",
                ReasoningEffort::ProviderDefault,
            ))
            .unwrap_err();
        assert!(error.to_string().contains("needs a default_model"));
    }

    #[test]
    fn opencode_zen_catalog_routes_documented_models_to_existing_transports() {
        let (config, registry) = registry(
            "[providers.opencode-zen]\nkind = 'opencode-zen'\ndefault_model = 'gpt-6-astra'\n",
        );
        let provider = registry.provider("opencode-zen").unwrap();
        assert_eq!(provider.base_url, "https://opencode.ai/zen/v1");
        assert_eq!(provider.credential.display(), "env:OPENCODE_API_KEY");
        assert!(
            !provider.capabilities.session_header,
            "Go's header remains Go-only"
        );
        for (model, transport) in [
            ("gpt-6-astra", TransportKind::Responses),
            ("gpt-5.5-pro", TransportKind::Responses),
            ("gpt-5.1", TransportKind::Responses),
            ("grok-4.6", TransportKind::Responses),
            ("muse-spark-1.3-contributor-free", TransportKind::Responses),
            ("claude-sonnet-5", TransportKind::AnthropicMessages),
            ("claude-fable-5-1", TransportKind::AnthropicMessages),
            ("qwen3.8-flash", TransportKind::AnthropicMessages),
            ("qwen3.5-plus", TransportKind::AnthropicMessages),
            ("deepseek-v4-flash", TransportKind::ChatCompletions),
            ("minimax-m3", TransportKind::ChatCompletions),
            ("glm-5.3-flash", TransportKind::ChatCompletions),
            ("kimi-k2.6", TransportKind::ChatCompletions),
            (
                "nemotron-3.5-lightning-free",
                TransportKind::ChatCompletions,
            ),
        ] {
            assert_eq!(
                registry
                    .model_descriptor("opencode-zen", model)
                    .unwrap()
                    .transport,
                transport
            );
        }
        assert!(
            !registry
                .available_models("opencode-zen")
                .iter()
                .any(|model| model.model.starts_with("gemini-"))
        );
        let (default, _) = registry.default_profile(&config).unwrap();
        assert_eq!(default.model, "gpt-6-astra");
    }

    #[test]
    fn legacy_single_provider_migrates_into_the_registry() {
        let (config, registry) = registry(
            r#"
            [provider]
            kind = "openai-compatible"
            model = "custom-model"
            base_url = "https://generic.example.com/v1"
            api_key_env = "CUSTOM_KEY"
            "#,
        );
        let profile = registry.default_provider();
        assert_eq!(profile.id.as_str(), "openai-compatible");
        assert_eq!(profile.kind, ProviderKind::OpenAiCompatible);
        assert_eq!(profile.credential.display(), "env:CUSTOM_KEY");
        let (resolved, descriptor) = registry.default_profile(&config).unwrap();
        assert_eq!(resolved.model, "custom-model");
        assert!(
            descriptor.resolved,
            "legacy model keeps its former transport"
        );
        assert!(descriptor.supported_efforts.is_empty());
        assert!(descriptor.context_window_tokens.is_none());
        assert!(descriptor.pricing.is_none());
        assert_eq!(descriptor.reasoning_replay, ReasoningReplay::Omit);
    }

    #[test]
    fn legacy_opencode_base_url_migrates_to_the_explicit_profile() {
        let (config, registry) = registry(
            r#"
            [provider]
            kind = "openai-compatible"
            model = "deepseek-v4.1-flash"
            base_url = "https://opencode.ai/zen/go"
            api_key_env = "OPENCODE_API_KEY"
            "#,
        );
        let profile = registry.default_provider();
        assert_eq!(profile.kind, ProviderKind::OpenCodeGo);
        assert!(profile.capabilities.session_header);
        let (resolved, descriptor) = registry.default_profile(&config).unwrap();
        assert_eq!(resolved.provider.as_str(), "opencode-go");
        assert_eq!(
            descriptor.reasoning_replay,
            ReasoningReplay::Replay,
            "OpenCode Go serving a DeepSeek model keeps required replay"
        );
    }

    #[test]
    fn multi_provider_config_resolves_profiles_and_precedence() {
        let (config, registry) = registry(
            r#"
            [providers.opencode-go]
            kind = "opencode-go"
            credential = "env:OPENCODE_API_KEY"

            [providers.opencode-go.models."deepseek-v4-flash"]
            display_name = "V4 Flash"
            context_window_tokens = 200000
            efforts = ["low", "high", "max"]
            default_effort = "high"

            [providers.deepseek]
            kind = "deepseek"
            credential = "file:deepseek"

            [inference]
            provider = "deepseek"
            model = "deepseek-v4-pro"
            effort = "max"
            "#,
        );
        assert_eq!(registry.available_providers().len(), 2);
        let (resolved, _descriptor) = registry.default_profile(&config).unwrap();
        assert_eq!(resolved.provider.as_str(), "deepseek");
        assert_eq!(resolved.effort, ReasoningEffort::Max);

        // Explicit user metadata overrides the built-in row.
        let flash = registry
            .model_descriptor("opencode-go", "deepseek-v4-flash")
            .unwrap();
        assert_eq!(flash.display_name, "V4 Flash");
        assert_eq!(flash.context_window_tokens, Some(200_000));
        assert_eq!(flash.default_effort, ReasoningEffort::High);

        // The other provider's model keeps the built-in metadata.
        let plain = registry
            .model_descriptor("deepseek", "deepseek-flash")
            .unwrap();
        assert_eq!(plain.display_name, "DeepSeek Flash");
        assert_eq!(plain.context_window_tokens, Some(1_048_576));
        // The retired DeepSeek name still resolves to the canonical row.
        let legacy = registry
            .model_descriptor("deepseek", "deepseek-v4.1-flash")
            .unwrap();
        assert_eq!(legacy.model, "deepseek-flash");
    }

    #[test]
    fn unknown_models_never_invent_capabilities() {
        let (_config, registry) = registry(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://example.com/v1"
            credential = "env:CUSTOM_KEY"
            "#,
        );
        let descriptor = registry.model_descriptor("custom", "mystery-1").unwrap();
        assert!(!descriptor.known);
        assert!(descriptor.supported_efforts.is_empty());
        assert_eq!(
            descriptor.selectable_efforts(),
            vec![ReasoningEffort::ProviderDefault]
        );
        assert!(descriptor.context_window_tokens.is_none());
        assert!(descriptor.pricing.is_none());
        assert_eq!(descriptor.reasoning_replay, ReasoningReplay::Omit);
    }

    #[test]
    fn unresolved_custom_model_is_setup_only_until_transport_is_overridden() {
        let source = r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://example.com/v1"
            default_model = "acme-pro"
            enabled_models = ["acme-pro"]

            [providers.custom.models."acme-pro"]
            display_name = "Acme Pro"
        "#;
        let config: Config = toml::from_str(source).unwrap();
        let registry = ProviderRegistry::from_config(&config).unwrap();
        assert!(registry.available_models("custom").is_empty());
        let descriptor = registry.model_descriptor("custom", "acme-pro").unwrap();
        assert!(!descriptor.resolved);
        assert!(
            registry
                .default_profile(&config)
                .unwrap_err()
                .to_string()
                .contains("unresolved transport")
        );

        let ready: Config =
            toml::from_str(&format!("{source}\ntransport = 'chat_completions'\n")).unwrap();
        let registry = ProviderRegistry::from_config(&ready).unwrap();
        assert_eq!(registry.available_models("custom").len(), 1);
        assert!(registry.default_profile(&ready).is_ok());
    }

    #[test]
    fn effort_resolution_clamps_unsupported_values() {
        let (_config, registry) = registry(
            r#"
            [providers.openai]
            kind = "openai"
            credential = "env:OPENAI_API_KEY"
            "#,
        );
        let (low, descriptor) = registry
            .resolve_profile(&InferenceProfile::new(
                "openai",
                "gpt-5.5",
                ReasoningEffort::Low,
            ))
            .unwrap();
        assert_eq!(low.effort, ReasoningEffort::Low);
        assert!(descriptor.supports_effort(ReasoningEffort::Low));

        // `max` is not advertised by gpt-5.5; it clamps to the model default
        // (medium) instead of being emitted.
        let (clamped, _) = registry
            .resolve_profile(&InferenceProfile::new(
                "openai",
                "gpt-5.5",
                ReasoningEffort::Max,
            ))
            .unwrap();
        assert_eq!(clamped.effort, ReasoningEffort::Medium);
        // `xhigh` is advertised and preserved.
        let (xhigh, _) = registry
            .resolve_profile(&InferenceProfile::new(
                "openai",
                "gpt-5.5",
                ReasoningEffort::XHigh,
            ))
            .unwrap();
        assert_eq!(xhigh.effort, ReasoningEffort::XHigh);

        // An unknown model has no documented transport and cannot activate.
        let error = registry
            .resolve_profile(&InferenceProfile::new(
                "openai",
                "mystery-1",
                ReasoningEffort::Max,
            ))
            .unwrap_err();
        assert!(error.to_string().contains("unresolved transport"));
    }

    #[test]
    fn aliases_resolve_to_the_canonical_model() {
        let (_config, registry) = registry(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://example.com/v1"
            credential = "env:CUSTOM_KEY"

            [providers.custom.models."my-model"]
            display_name = "My Model"
            aliases = ["mine"]
            transport = "chat_completions"
            "#,
        );
        let (profile, descriptor) = registry
            .resolve_profile(&InferenceProfile::new(
                "custom",
                "mine",
                ReasoningEffort::ProviderDefault,
            ))
            .unwrap();
        assert_eq!(profile.model, "my-model");
        assert_eq!(descriptor.display_name, "My Model");
    }

    #[test]
    fn deepseek_v4_efforts_resolve_low_high_and_max() {
        let (_config, registry) = registry(
            r#"
            [providers.deepseek]
            kind = "deepseek"
            credential = "env:DEEPSEEK_API_KEY"
            "#,
        );
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::High,
            ReasoningEffort::Max,
        ] {
            let (profile, descriptor) = registry
                .resolve_profile(&InferenceProfile::new("deepseek", "deepseek-flash", effort))
                .unwrap();
            assert_eq!(profile.effort, effort);
            assert_eq!(
                descriptor.effective_effort(effort),
                effort,
                "the selected effort reaches the wire unchanged"
            );
            assert_eq!(descriptor.reasoning_replay, ReasoningReplay::Replay);
        }
        // The retired `deepseek-chat` name is an alias of the canonical Flash
        // model and inherits its capabilities.
        let (profile, descriptor) = registry
            .resolve_profile(&InferenceProfile::new(
                "deepseek",
                "deepseek-chat",
                ReasoningEffort::High,
            ))
            .unwrap();
        assert_eq!(profile.model, "deepseek-flash");
        assert_eq!(profile.effort, ReasoningEffort::High);
        assert_eq!(descriptor.context_window_tokens, Some(1_048_576));
    }

    #[test]
    fn opencode_go_capabilities_are_resolved_per_model() {
        let (_config, registry) = registry(
            r#"
            [providers.opencode-go]
            kind = "opencode-go"
            credential = "env:OPENCODE_API_KEY"
            "#,
        );
        // A DeepSeek-family model keeps required reasoning replay; the Go
        // gateway advertises low/high/max for it (no `none`).
        let deepseek = registry
            .model_descriptor("opencode-go", "deepseek-v4-flash")
            .unwrap();
        assert_eq!(deepseek.reasoning_replay, ReasoningReplay::Replay);
        assert_eq!(deepseek.transport, TransportKind::ChatCompletions);
        assert_eq!(
            deepseek.supported_efforts,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max
            ]
        );
        // A GPT model on the same endpoint uses the Responses transport and
        // does not inherit DeepSeek replay.
        let luna = registry
            .model_descriptor("opencode-go", "gpt-5.6-luna")
            .unwrap();
        assert_eq!(luna.transport, TransportKind::Responses);
        assert_eq!(luna.reasoning_replay, ReasoningReplay::Omit);
        assert!(!luna.supported_efforts.is_empty());
        assert_eq!(
            luna.context_window_tokens, None,
            "OpenCode Go does not document context windows"
        );
        // Qwen/MiniMax models use the Anthropic Messages transport.
        let qwen = registry
            .model_descriptor("opencode-go", "qwen3.7-max")
            .unwrap();
        assert_eq!(qwen.transport, TransportKind::AnthropicMessages);
        assert_eq!(qwen.reasoning_replay, ReasoningReplay::Omit);
        assert!(qwen.supported_efforts.is_empty());
        // An unknown model on the same endpoint is not assumed to be DeepSeek:
        // no replay, no efforts, no invented metadata.
        let other = registry
            .model_descriptor("opencode-go", "mystery-model")
            .unwrap();
        assert!(!other.known);
        assert_eq!(other.reasoning_replay, ReasoningReplay::Omit);
        assert!(other.supported_efforts.is_empty());
        assert!(other.context_window_tokens.is_none());
        assert!(other.pricing.is_none());
    }

    #[test]
    fn opencode_go_catalog_matches_the_current_documented_list() {
        let (_config, registry) = registry(
            r#"
            [providers.opencode-go]
            kind = "opencode-go"
            credential = "env:OPENCODE_API_KEY"
            "#,
        );
        let mut models: Vec<String> = registry
            .available_models("opencode-go")
            .into_iter()
            .map(|model| model.model)
            .collect();
        models.sort();
        let mut expected = vec![
            "deepseek-v4-flash",
            "deepseek-v4-flash-vision-exp",
            "deepseek-v4-pro",
            "deepseek-v4.1-flash",
            "glm-5.1",
            "glm-5.2",
            "glm-5.3",
            "glm-5.3-flash",
            "gpt-5.6-luna",
            "gpt-6-luna",
            "grok-4.6",
            "grok-4.7",
            "hy3",
            "hy4-preview",
            "kimi-k2.6",
            "kimi-k2.7-code",
            "kimi-k3",
            "longcat-2.0",
            "mimo-v2.5",
            "mimo-v2.5-pro",
            "mimo-v2.6-flash",
            "mimo-v2.6-pro",
            "minimax-m2.5",
            "minimax-m2.7",
            "minimax-m3",
            "muse-spark-1.2-contributor",
            "muse-spark-1.3-contributor",
            "qwen3.6-plus",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.8-flash",
            "qwen3.8-max",
            "space-bunny-free",
        ];
        expected.sort_unstable();
        assert_eq!(models, expected, "the current OpenCode Go documented list");

        // Per-model effort sets come from the Go reasoning options.
        for (model, efforts) in [
            (
                "grok-4.7",
                vec![
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::XHigh,
                ],
            ),
            ("glm-5.2", vec![ReasoningEffort::High, ReasoningEffort::Max]),
            ("kimi-k3", vec![ReasoningEffort::Max]),
            (
                "qwen3.8-max",
                vec![
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::XHigh,
                ],
            ),
            (
                "hy4-preview",
                vec![ReasoningEffort::None, ReasoningEffort::High],
            ),
            (
                "muse-spark-1.3-contributor",
                vec![
                    ReasoningEffort::Minimal,
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::XHigh,
                ],
            ),
            ("qwen3.7-max", Vec::new()),
        ] {
            assert_eq!(
                registry
                    .model_descriptor("opencode-go", model)
                    .unwrap()
                    .supported_efforts,
                efforts,
                "{model} efforts"
            );
        }
        // `deepseek-v4.1-flash` is its own documented Go model, not an alias of
        // `deepseek-v4-flash`; legacy names keep resolving where they always did.
        assert_eq!(
            registry
                .model_descriptor("opencode-go", "deepseek-v4.1-flash")
                .unwrap()
                .model,
            "deepseek-v4.1-flash"
        );
        assert_eq!(
            registry
                .model_descriptor("opencode-go", "deepseek-flash")
                .unwrap()
                .model,
            "deepseek-v4-flash"
        );
    }

    #[test]
    fn deepseek_catalog_matches_official_api_model_ids() {
        let (_config, registry) = registry(
            r#"
            [providers.deepseek]
            kind = "deepseek"
            credential = "env:DEEPSEEK_API_KEY"
            "#,
        );
        let mut canonical: Vec<String> = registry
            .available_models("deepseek")
            .into_iter()
            .map(|model| model.model)
            .collect();
        canonical.sort();
        assert_eq!(
            canonical,
            vec!["deepseek-flash".to_owned(), "deepseek-v4-pro".to_owned()],
            "the API reference lists exactly these allowed model values"
        );
        for model in registry.available_models("deepseek") {
            assert_eq!(model.transport, TransportKind::ChatCompletions);
            assert_eq!(model.reasoning_replay, ReasoningReplay::Replay);
            assert_eq!(model.context_window_tokens, Some(1_048_576));
            assert_eq!(
                model.selectable_efforts(),
                vec![
                    ReasoningEffort::ProviderDefault,
                    ReasoningEffort::None,
                    ReasoningEffort::Low,
                    ReasoningEffort::High,
                    ReasoningEffort::Max,
                ],
                "{} effort set",
                model.model
            );
            assert_eq!(model.default_effort, ReasoningEffort::High);
        }
    }

    #[test]
    fn deepseek_retired_names_are_aliases_not_distinct_models() {
        let (_config, registry) = registry(
            r#"
            [providers.deepseek]
            kind = "deepseek"
            credential = "env:DEEPSEEK_API_KEY"
            "#,
        );
        // Retired names route to the same current V4.1-Flash model. The Vision
        // guide states the retired vision name is served by the latest Flash
        // model too, so resolving it to deepseek-flash is wire-exact.
        for alias in [
            "deepseek-v4-flash",
            "deepseek-v4-flash-vision-exp",
            "deepseek-v4.1-flash",
            "deepseek-chat",
            "deepseek-reasoner",
            "deepseek-v4.1",
        ] {
            let descriptor = registry.model_descriptor("deepseek", alias).unwrap();
            assert_eq!(descriptor.model, "deepseek-flash", "alias {alias}");
        }
        // `deepseek-v4-pro` stays a distinct canonical model: no alias or
        // heuristic may collapse it into Flash.
        let pro = registry
            .model_descriptor("deepseek", "deepseek-v4-pro")
            .unwrap();
        assert_eq!(pro.model, "deepseek-v4-pro");
        assert_ne!(pro.model, "deepseek-flash");
    }

    #[test]
    fn deepseek_alias_entered_by_a_user_is_sent_as_the_canonical_wire_model() {
        let dir = tempfile::tempdir().unwrap();
        let mut credentials = CredentialStore::open(dir.path().join("secrets.toml")).unwrap();
        credentials.set("deepseek", "sk-test").unwrap();
        let (_config, registry) = registry(
            r#"
            [providers.deepseek]
            kind = "deepseek"
            credential = "file:deepseek"
            "#,
        );
        for (entered, wire) in [
            ("deepseek-flash", "deepseek-flash"),
            ("deepseek-v4-flash", "deepseek-flash"),
            ("deepseek-v4-flash-vision-exp", "deepseek-flash"),
            ("deepseek-v4-pro", "deepseek-v4-pro"),
        ] {
            let (profile, descriptor) = registry
                .resolve_profile(&InferenceProfile::new(
                    "deepseek",
                    entered,
                    ReasoningEffort::High,
                ))
                .unwrap();
            let provider = registry
                .build_provider(&profile, &descriptor, &credentials, Uuid::new_v4(), None)
                .unwrap();
            assert_eq!(provider.model(), wire, "alias {entered}");
            // The serialized request body carries the canonical wire ID, never
            // the name the user originally typed.
            let body = crate::provider::openai_request(
                &latch_protocol::ModelRequest {
                    system: "s".into(),
                    messages: vec![],
                    tools: vec![],
                },
                provider.model(),
                descriptor.reasoning_replay,
            )
            .unwrap();
            assert_eq!(body["model"], wire, "request body for {entered}");
        }
    }

    #[test]
    fn openai_catalog_uses_public_api_context_and_effort_sets() {
        let (_config, registry) = registry(
            r#"
            [providers.openai]
            kind = "openai"
            credential = "env:OPENAI_API_KEY"
            "#,
        );
        let astra = registry.model_descriptor("openai", "gpt-6-astra").unwrap();
        assert_eq!(astra.context_window_tokens, Some(1_050_000));
        assert_eq!(astra.transport, TransportKind::Responses);
        assert_eq!(
            astra.supported_efforts,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
                ReasoningEffort::Max,
            ],
            "Astra does not advertise `none`"
        );
        assert_eq!(astra.default_effort, ReasoningEffort::ProviderDefault);

        // The documented `gpt-5.6` alias resolves to Sol.
        let sol = registry.model_descriptor("openai", "gpt-5.6").unwrap();
        assert_eq!(sol.model, "gpt-5.6-sol");
        assert_eq!(sol.context_window_tokens, Some(1_050_000));
        assert_eq!(sol.default_effort, ReasoningEffort::Medium);

        let gpt54 = registry.model_descriptor("openai", "gpt-5.4").unwrap();
        assert_eq!(gpt54.context_window_tokens, Some(1_050_000));
        assert_eq!(gpt54.default_effort, ReasoningEffort::None);

        for model in registry.available_models("openai") {
            assert_eq!(
                model.transport,
                TransportKind::Responses,
                "{} uses Responses",
                model.model
            );
            assert_eq!(model.context_window_tokens, Some(1_050_000));
        }
    }

    #[test]
    fn anthropic_models_are_selectable_without_wire_leakage() {
        let (_config, registry) = registry(
            r#"
            [providers.anthropic]
            kind = "anthropic"
            credential = "env:ANTHROPIC_API_KEY"
            "#,
        );
        let error = registry
            .resolve_profile(&InferenceProfile::new(
                "anthropic",
                "claude-sonnet-4-5",
                ReasoningEffort::High,
            ))
            .unwrap_err();
        assert!(error.to_string().contains("unresolved transport"));
        let (profile, descriptor) = registry
            .resolve_profile(&InferenceProfile::new(
                "anthropic",
                "claude-haiku-4-5",
                ReasoningEffort::High,
            ))
            .unwrap();
        assert_eq!(profile.model, "claude-haiku-4-5");
        assert_eq!(
            registry.provider("anthropic").unwrap().kind,
            ProviderKind::Anthropic
        );
        // Anthropic effort is not mapped yet: the selection clamps rather than
        // sending an unsupported parameter.
        assert_eq!(profile.effort, ReasoningEffort::ProviderDefault);
        assert!(descriptor.supported_efforts.is_empty());
    }

    #[test]
    fn builtin_catalogs_mark_official_vision_models() {
        let (_config, registry) = registry(
            r#"
            [providers.openai]
            kind = "openai"
            credential = "env:OPENAI_API_KEY"

            [providers.anthropic]
            kind = "anthropic"
            credential = "env:ANTHROPIC_API_KEY"

            [providers.deepseek]
            kind = "deepseek"
            credential = "env:DEEPSEEK_API_KEY"

            [providers.opencode-go]
            kind = "opencode-go"
            credential = "env:OPENCODE_API_KEY"
            "#,
        );
        // The official OpenAI model sizing table documents image input for
        // every current built-in model.
        for model in registry.available_models("openai") {
            assert!(
                model.supports_image_input(),
                "{} is documented as vision-capable",
                model.model
            );
        }
        // The Anthropic models overview states all current models accept
        // image input.
        for model in registry.available_models("anthropic") {
            assert!(
                model.supports_image_input(),
                "{} is documented as vision-capable",
                model.model
            );
        }
        // DeepSeek documents image input for the current Flash model only.
        assert!(
            registry
                .model_descriptor("deepseek", "deepseek-flash")
                .unwrap()
                .supports_image_input()
        );
        assert!(
            !registry
                .model_descriptor("deepseek", "deepseek-v4-pro")
                .unwrap()
                .supports_image_input()
        );
        // OpenCode Go publishes no per-model modalities and live probing shows
        // the gateway rejects images even for upstream vision models, so every
        // Go model stays conservative text-only by default. Explicit user
        // metadata is the only way to opt a Go model into image input, and the
        // kernel still fails locally when the endpoint disagrees.
        for model in registry.available_models("opencode-go") {
            assert!(
                !model.supports_image_input(),
                "{} stays text-only by default",
                model.model
            );
        }
        assert!(
            !registry
                .model_descriptor("opencode-go", "mystery-model")
                .unwrap()
                .supports_image_input(),
            "unknown models default to text-only"
        );
        // An unknown model never gains vision through a name or transport.
        assert!(
            !registry
                .model_descriptor("openai", "gpt-vision-unofficial")
                .unwrap()
                .supports_image_input()
        );
    }

    #[test]
    fn user_configuration_can_opt_models_in_and_out_of_image_input() {
        let (_config, registry) = registry(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://example.com/v1"
            credential = "env:CUSTOM_KEY"

            [providers.custom.models."mystery-vision"]
            input_modalities = ["text", "image"]

            [providers.custom.models."text-only-override"]
            input_modalities = ["text"]

            [providers.openai]
            kind = "openai"
            credential = "env:OPENAI_API_KEY"

            [providers.openai.models."gpt-5.5"]
            input_modalities = ["text"]
            "#,
        );
        let custom = registry
            .model_descriptor("custom", "mystery-vision")
            .unwrap();
        assert!(custom.known);
        assert!(custom.supports_image_input());
        assert!(
            custom
                .input_modalities
                .contains(&latch_protocol::InputModality::Text)
        );
        let text_only = registry
            .model_descriptor("custom", "text-only-override")
            .unwrap();
        assert!(!text_only.supports_image_input());
        // Explicit user metadata wins over the built-in catalog.
        let overridden = registry.model_descriptor("openai", "gpt-5.5").unwrap();
        assert!(!overridden.supports_image_input());
    }

    #[test]
    fn provider_entry_can_extend_the_builtin_catalog() {
        let (_config, registry) = registry(
            r#"
            [providers.deepseek]
            kind = "deepseek"
            credential = "file:ds"

            [providers.deepseek.models."future-model"]
            context_window_tokens = 128000
            efforts = ["low", "high"]
            default_effort = "low"
            reasoning_replay = "replay"
            "#,
        );
        let descriptor = registry
            .model_descriptor("deepseek", "future-model")
            .unwrap();
        assert!(descriptor.known);
        assert_eq!(descriptor.context_window_tokens, Some(128_000));
        assert_eq!(
            descriptor.supported_efforts,
            vec![ReasoningEffort::Low, ReasoningEffort::High]
        );
        assert_eq!(descriptor.reasoning_replay, ReasoningReplay::Replay);
    }

    #[test]
    fn user_effort_map_reaches_the_model_descriptor() {
        let (_config, registry) = registry(
            r#"
            [providers.custom]
            kind = "openai-compatible"
            base_url = "https://example.com/v1"
            default_model = "mapped"

            [providers.custom.models.mapped]
            transport = "chat_completions"
            efforts = ["low", "high"]
            default_effort = "low"

            [providers.custom.models.mapped.effort_map]
            low = { disabled = true }
            high = { value = "9" }
            "#,
        );
        let descriptor = registry.model_descriptor("custom", "mapped").unwrap();
        assert_eq!(
            descriptor.effort_map[&ReasoningEffort::High]
                .form()
                .unwrap(),
            crate::config::EffortForm::Value("9".into())
        );
        assert_eq!(
            descriptor.effort_map[&ReasoningEffort::Low].form().unwrap(),
            crate::config::EffortForm::Disabled
        );
        // The built-in catalog keeps user config free of copied metadata.
        let builtin = crate::providers::builtin_catalog(ProviderKind::OpenAi)
            .into_iter()
            .find(|descriptor| descriptor.model == "gpt-5.5")
            .unwrap();
        assert!(builtin.effort_map.is_empty());
    }
}
