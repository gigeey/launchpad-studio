//! Request body builder — converts a [`CompletionRequest`] into the Anthropic
//! Messages API JSON wire shape.
//!
//! The system prompt is carried at the request top level (not inside the
//! messages array) because Anthropic requires that separation. The caller is
//! responsible for stripping any `Message::System` entries from the messages
//! slice before calling `build`; in practice `build` does this itself.
//!
//! Cache-control breakpoints (`cache_control: { type: "ephemeral" }`) are
//! inserted on (a) the system block and (b) the last text block of the most
//! recent user-role message in the messages array. Both insertions are
//! suppressed when the env var `LAUNCHPAD_ANTHROPIC_CACHE_OFF=1` is set —
//! set it for cache-off diff testing. The check is presence-based: any value
//! disables insertion; "1" is the canonical setting.

use ao_engine_tools_provider_config::AnthropicConfig;
use ao_engine_tools_runner::{
    message::{Message, MessageNormalizer, NormalizerError},
    provider::CompletionRequest,
};
use ao_protocol::agent::{ReasoningEffort, ThinkingConfig, ThinkingDisplay, ThinkingMode};
use serde_json::{json, Value};

/// Hardcoded fallback when neither an agent override nor a persisted
/// `providers.toml` value supplies `max_output_tokens` — the bottom tier of
/// the same per-agent ?? persisted-config ?? provider-default precedence
/// [`ao_engine_tools_runner::provider::resolve_model`] documents.
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 8192;

const CACHE_OFF_ENV: &str = "LAUNCHPAD_ANTHROPIC_CACHE_OFF";

fn cache_control_enabled() -> bool {
    std::env::var_os(CACHE_OFF_ENV).is_none()
}

/// Build the Anthropic Messages API POST body from a [`CompletionRequest`].
///
/// The returned `Value` is ready to be serialised as the HTTP request body.
/// `model`, `max_tokens`, `stream`, and (when non-empty) `tools` are all set
/// here. The `system` field is populated from `request.system_prompt` when
/// `Some`, with a `cache_control` breakpoint unless `LAUNCHPAD_ANTHROPIC_CACHE_OFF`
/// is set.
///
/// `Message::System` entries are filtered from the messages slice before
/// passing to the normalizer, since Anthropic carries system content at the
/// request top level. The remaining history is then trimmed to
/// `config.max_context_tokens` (see [`ao_engine_tools_runner::message::truncate_to_context_budget`])
/// before reasoning-block stripping and normalization run.
///
/// `max_tokens` uses `config.max_output_tokens` when set, falling back to
/// [`DEFAULT_MAX_OUTPUT_TOKENS`]. `thinking` is populated from either the
/// per-turn `request.thinking` override or, absent that, `config.reasoning_effort`
/// mapped through [`reasoning_effort_to_thinking`] — see that function for
/// the precedence between the two.
pub fn build(
    config: &AnthropicConfig,
    normalizer: &dyn MessageNormalizer,
    request: &CompletionRequest,
) -> Result<Value, NormalizerError> {
    let non_system: Vec<Message> = request
        .messages
        .iter()
        .filter(|m| !matches!(m, Message::System { .. }))
        .cloned()
        .collect();

    // Trim the oldest history to the operator's context budget, if any,
    // before anything else touches the transcript — see
    // `truncate_to_context_budget` for why this lives client-side rather
    // than as a wire field.
    let mut non_system = ao_engine_tools_runner::message::truncate_to_context_budget(
        &non_system,
        config.max_context_tokens,
    );

    // Drop reasoning blocks from closed assistant turns before they hit the
    // wire. Anthropic validates the signatures of `thinking`/`redacted_thinking`
    // blocks in the latest assistant message and rejects the request when they
    // were modified from the original response — which is exactly what happens
    // when a turn was reconstructed from a persisted transcript. Blocks that
    // belong to an in-flight tool-use cycle (assistant tool_use immediately
    // followed by a tool_result) are preserved; this is the single chokepoint
    // every Anthropic-bound request passes through, so it also covers the live
    // in-session loop and subagent runs.
    ao_engine_tools_runner::message::strip_closed_turn_reasoning(&mut non_system);

    let mut messages_value = normalizer.to_provider(&non_system)?;

    let cache_on = cache_control_enabled();

    let max_output_tokens = config.max_output_tokens.map(u64::from).unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);

    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(config.model));
    body.insert("max_tokens".into(), json!(max_output_tokens));

    if let Some(system) = &request.system_prompt {
        let system_block = if cache_on {
            json!([{ "type": "text", "text": system, "cache_control": {"type": "ephemeral"} }])
        } else {
            json!([{ "type": "text", "text": system }])
        };
        body.insert("system".into(), system_block);
    }

    if cache_on {
        insert_user_cache_control(&mut messages_value);
    }

    body.insert("messages".into(), messages_value);

    // An explicit per-turn `ThinkingConfig` (the older mechanism —
    // `AgentProfile.thinking`, still honored verbatim) always wins over the
    // newer, resolved `reasoning_effort` knob: it is strictly more specific.
    // Only when the caller set neither is thinking left off entirely.
    let effective_thinking: Option<ThinkingConfig> = request.thinking.clone().or_else(|| {
        config
            .reasoning_effort
            .and_then(|effort| reasoning_effort_to_thinking(effort, max_output_tokens))
    });
    if let Some(thinking_body) = thinking_field(effective_thinking.as_ref()) {
        body.insert("thinking".into(), thinking_body);
    }

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                let is_deferred = request.deferred_tools.contains(&t.name)
                    && !request.loaded_deferred_tools.contains(&t.name);
                if is_deferred {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                        "defer_loading": true,
                    })
                } else {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                }
            })
            .collect();
        body.insert("tools".into(), tools.into());
    }

    body.insert("stream".into(), json!(true));

    Ok(Value::Object(body))
}

/// Translate the effective [`ThinkingConfig`] — whichever of the per-turn
/// `request.thinking` override or the resolved `reasoning_effort` knob
/// [`build`] chose — into the JSON shape Anthropic's Messages API expects on
/// the body's top-level `thinking` field. Returns `None` when neither source
/// had one — Anthropic's default in that case is "no extended thinking",
/// which matches the behaviour of older clients that pre-date this field.
///
/// `ThinkingMode::Disabled` returns `None` too: the cleanest way to disable
/// thinking on Anthropic's API is to omit the field entirely. Sending an
/// explicit "off" shape risks future-proofing pain if Anthropic later adds a
/// `Some({"type": "disabled"})` value with non-trivial semantics.
///
/// `display = "summarized"` is the only value with a wire-level cost
/// implication (it asks the provider to digest the reasoning trace), so it's
/// always written explicitly.
fn thinking_field(cfg: Option<&ThinkingConfig>) -> Option<Value> {
    let cfg = cfg?;
    if matches!(cfg.mode, ThinkingMode::Disabled) {
        return None;
    }
    let display_str = match cfg.display {
        ThinkingDisplay::Summarized => "summarized",
        ThinkingDisplay::Raw => "raw",
        ThinkingDisplay::Omitted => "omitted",
    };
    let mut obj = serde_json::Map::new();
    // Anthropic's API spec uses `type` as the discriminant; `adaptive` is the
    // only model-driven value plumbed through the canonical enum so far.
    obj.insert("type".into(), json!("adaptive"));
    obj.insert("display".into(), json!(display_str));
    if let Some(budget) = cfg.budget_tokens {
        obj.insert("budget_tokens".into(), json!(budget));
    }
    Some(Value::Object(obj))
}

/// Map an ordinal [`ReasoningEffort`] level onto a concrete Anthropic
/// `thinking.budget_tokens` value. Anthropic requires `1024 <= budget_tokens
/// < max_tokens`; `max_output_tokens` clamps the per-tier constant so a
/// `High` effort level never emits a `budget_tokens` the configured
/// `max_output_tokens` can't satisfy, regardless of how small an operator
/// set it. When `max_output_tokens` leaves no room even for the 1024-token
/// minimum, thinking is skipped entirely (`None`) rather than emitting a
/// request Anthropic would reject outright.
fn reasoning_effort_to_thinking(effort: ReasoningEffort, max_output_tokens: u64) -> Option<ThinkingConfig> {
    const MIN_BUDGET_TOKENS: u64 = 1024;
    if max_output_tokens <= MIN_BUDGET_TOKENS {
        return None;
    }
    let tier = match effort {
        ReasoningEffort::Low => 1024,
        ReasoningEffort::Medium => 4096,
        ReasoningEffort::High => 10_000,
    };
    let budget = tier.min(max_output_tokens - MIN_BUDGET_TOKENS).max(MIN_BUDGET_TOKENS);
    Some(ThinkingConfig {
        mode: ThinkingMode::Adaptive,
        display: ThinkingDisplay::Summarized,
        budget_tokens: Some(budget as u32),
    })
}

/// Insert `cache_control: { type: "ephemeral" }` on the last content block of
/// the most recent user-role message in `messages`.
///
/// The Anthropic API supports `cache_control` on any content block type — text,
/// image, tool_use, tool_result, or document — so this is deliberately
/// type-agnostic. Placing the breakpoint on the *last* block advances the
/// cached prefix to the full length of the most recent user turn. This matters
/// in two cases the previous text-only placement got wrong:
///
/// 1. **Tool-loop iterations:** after iteration 1, the most recent user message
///    is composed entirely of `tool_result` blocks. A text-only search would
///    no-op, leaving the cache prefix frozen at the original turn-1 user text
///    while each subsequent iteration re-processed the growing transcript as
///    fresh tokens.
/// 2. **Follow-up user turns after a tool loop:** even when the new turn is
///    plain text, the *previous* turn's cache_control needs to have landed on
///    the trailing tool_result so the prefix covers the assistant tool calls
///    and tool results in between.
fn insert_user_cache_control(messages: &mut Value) {
    let arr = match messages.as_array_mut() {
        Some(a) => a,
        None => return,
    };
    for msg in arr.iter_mut().rev() {
        if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
            let content = match msg.get_mut("content").and_then(|c| c.as_array_mut()) {
                Some(c) => c,
                None => return,
            };
            if let Some(last) = content.last_mut() {
                if let Some(obj) = last.as_object_mut() {
                    obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
                }
            }
            return;
        }
    }
}
