//! Provider seam — the minimal trait surface the query loop uses to
//! talk to a model backend.
//!
//! [`ProviderClient`] has a single method, [`ProviderClient::complete`],
//! that takes a [`CompletionRequest`] (messages + system prompt + tool
//! catalog + mode hint) plus a [`CancellationToken`] and returns a
//! [`CompletionStream`] of [`CompletionEvent`] items. The events are the
//! lowest common denominator across providers: assistant text chunks,
//! tool-use intents, an explicit turn boundary, and a non-fatal stream
//! error variant. Hard transport failures surface as [`ProviderError`]
//! at stream construction or as `Err(...)` items inside the stream.
//!
//! Concrete provider clients (Anthropic, OpenAI, Gemini) live in a
//! separate downstream crate; this seam is intentionally narrow so
//! provider-specific request / response shapes never leak into the
//! query loop. The runner ships only a scripted [`MockProviderClient`]
//! used by unit tests and the crate-level integration test.

use std::collections::HashSet;

use async_trait::async_trait;
use ao_engine_tools_core::PermissionMode;
use ao_protocol::agent::{ReasoningEffort, ThinkingConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::message::{Message, MessageNormalizer};

mod stop_reason;
pub use stop_reason::StopReason;

mod usage;
pub use usage::Usage;

/// Description of a tool the provider may emit `ToolUse` events for.
/// Matches the shape every supported backend can consume — a name, a
/// short description, and a JSON schema for the input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Inputs handed to a provider for a single completion call.
///
/// `messages` carries the typed canonical transcript. Provider clients
/// translate to their wire format inside their own crate; the runner
/// never serialises the transcript itself. `mode` is a hint — providers
/// that support a system-level "plan" or "bypass" posture may surface
/// it; the rest ignore the field.
///
/// `deferred_tools` is the set of tool names that carry `LoadPolicy::Deferred`
/// and have NOT yet been resolved by ToolSearch. The Anthropic request builder
/// uses this set to add a `defer_loading: true` flag to those tool entries;
/// the OpenAI/Gemini builders use it to omit them entirely.
///
/// `loaded_deferred_tools` is the set of deferred tool names that ToolSearch
/// has already resolved in this session. Tools in this set are treated as
/// always-loaded by all request builders regardless of their `LoadPolicy`.
#[derive(Debug, Clone, Default)]
pub struct CompletionRequest {
    pub messages: Vec<Message>,
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolSpec>,
    pub mode: PermissionMode,
    /// Deferred tools not yet resolved by ToolSearch — emitted with
    /// `defer_loading: true` by Anthropic, omitted by OpenAI/Gemini.
    pub deferred_tools: HashSet<String>,
    /// Deferred tools already resolved via ToolSearch — treated as
    /// always-loaded by all request builders.
    pub loaded_deferred_tools: HashSet<String>,
    /// Provider-neutral reasoning channel configuration. `None` falls
    /// back to whatever default the provider would have produced
    /// without an explicit opt-in. Anthropic's request builder maps a
    /// `Some(ThinkingConfig)` here to the `thinking` field on the
    /// Messages API body; OpenAI/Gemini ignore it pending native
    /// reasoning-channel support in those crates.
    pub thinking: Option<ThinkingConfig>,
}

/// One event in a streamed completion. The runner needs to know about
/// assistant text appended to the turn, a tool call the model wants the
/// runner to execute, the reasoning channel lifecycle (start, deltas,
/// end), the turn boundary, and a non-fatal soft error the provider
/// chose to surface mid-stream.
#[derive(Debug, Clone, PartialEq)]
pub enum CompletionEvent {
    /// A chunk of assistant text. Multiple chunks within a turn are
    /// concatenated by the query loop.
    AssistantText(String),
    /// The model emitted a tool-use block. `id` is the call id the
    /// runner echoes back in the matching tool_result. `input` is the
    /// raw, unvalidated argument JSON.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// The provider opened a dedicated reasoning channel. Emitted once
    /// per thinking block, before any [`CompletionEvent::ThinkingDelta`]
    /// arrives. Providers that signal "thinking happened but the text is
    /// suppressed" (Anthropic's `display = "omitted"`) still emit this
    /// event so UIs can mount a "Thinking…" indicator without waiting on
    /// deltas that will never arrive.
    ThinkingStart,
    /// A chunk of reasoning text from an in-progress thinking block.
    /// Anthropic chunks these at multi-character boundaries (not
    /// character-by-character like [`CompletionEvent::AssistantText`]);
    /// concatenating all deltas within one thinking block yields the
    /// full reasoning trace for that block.
    ThinkingDelta { text: String },
    /// The provider closed the current reasoning channel. `elapsed_ms` is
    /// the wall-clock duration measured from the matching
    /// [`CompletionEvent::ThinkingStart`], so UIs can render a
    /// "Thought for Ns" footer when the bubble collapses.
    ThinkingEnd { elapsed_ms: u64 },
    /// The full assembled reasoning block for the just-closed thinking
    /// channel. Carries the concatenated `text` (or `None` when the
    /// provider suppressed the reasoning, e.g. `display = "omitted"`) and
    /// the cryptographic `signature` returned with the stream. Emitted
    /// once per thinking block, AFTER [`CompletionEvent::ThinkingEnd`] for
    /// the same block, so UI consumers that only care about the
    /// streaming triplet can ignore this event cleanly.
    ///
    /// The query loop captures these blocks per turn and folds them into
    /// the assistant message it builds at the end of the turn — Anthropic
    /// rejects a follow-up turn whose transcript echoes a `thinking`
    /// block without the original signature when the prior turn also
    /// emitted `tool_use`. Providers without a reasoning channel never
    /// emit this event.
    ThinkingBlock {
        text: Option<String>,
        signature: Option<String>,
    },
    /// A safety-redacted reasoning block emitted by the provider. Carries
    /// the opaque encrypted `data` payload that stands in for the withheld
    /// plaintext reasoning. Like [`CompletionEvent::ThinkingBlock`], the
    /// query loop folds this into the assistant message it builds at
    /// end-of-turn — in stream order relative to any signed thinking blocks
    /// — so the next request echoes it back verbatim and stays legal under
    /// Anthropic's multi-turn continuity rule. There is no streaming
    /// triplet for redacted blocks (no text deltas arrive), so this is the
    /// only event a redacted block produces. Providers without a reasoning
    /// channel never emit it.
    RedactedThinkingBlock { data: String },
    /// Token-usage accounting for the current turn. Emitted at most once
    /// per turn; some providers emit zero times. Always emitted before
    /// [`CompletionEvent::TurnComplete`] when emitted. The query loop
    /// ignores this event in v1 with a no-op match arm.
    Usage(Usage),
    /// The provider has finished the current turn. Carries a [`StopReason`]
    /// so the query loop and downstream consumers can react to the terminal
    /// condition without an additional event. The query loop ignores
    /// `stop_reason` in v1 and uses this event purely as a drain boundary.
    TurnComplete { stop_reason: StopReason },
    /// A non-fatal error the provider chose to keep streaming around.
    /// Hard failures are reported as `Err(ProviderError)` items
    /// instead.
    Error(String),
}

/// Hard provider failure — either at stream construction
/// ([`ProviderClient::complete`] returning `Err`) or as an `Err` item
/// inside the stream. Soft mid-stream errors that the provider chose
/// to keep the turn open through use [`CompletionEvent::Error`]
/// instead.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Network / authentication / serialization failure talking to the
    /// upstream provider.
    #[error("provider transport error: {0}")]
    Transport(String),
    /// The session was cancelled before the provider produced a
    /// terminal `TurnComplete`.
    #[error("provider call cancelled")]
    Cancelled,
    /// The scripted [`MockProviderClient`] was called more times than
    /// turns it was constructed with.
    #[error("scripted provider exhausted: no more turns to play")]
    ScriptExhausted,
    /// The provider is not configured (e.g. missing API key or absent section
    /// in the provider config file).
    #[error("provider not configured: {0}")]
    NotConfigured(String),
}

/// Stream of [`CompletionEvent`] items returned from
/// [`ProviderClient::complete`]. Concretely a thin wrapper around a
/// `tokio::sync::mpsc::Receiver` so we don't need a `futures` dep just
/// to satisfy `Stream`. Drive it by repeatedly awaiting
/// [`CompletionStream::recv`] until it returns `None` (channel closed:
/// the producer either reached `TurnComplete` or was cancelled).
#[derive(Debug)]
pub struct CompletionStream {
    rx: mpsc::Receiver<Result<CompletionEvent, ProviderError>>,
}

impl CompletionStream {
    /// Construct a `CompletionStream` from a raw mpsc receiver.
    ///
    /// Intended for provider crate implementations that spawn a reader task
    /// and return the stream end to the runner. The channel item type matches
    /// [`CompletionEvent`] / [`ProviderError`] — the same types the runner
    /// already drains via [`CompletionStream::recv`].
    pub fn new(rx: mpsc::Receiver<Result<CompletionEvent, ProviderError>>) -> Self {
        Self { rx }
    }

    /// Pull the next event. Returns `None` once the producer has
    /// finished emitting (turn complete or cancelled / dropped).
    pub async fn recv(&mut self) -> Option<Result<CompletionEvent, ProviderError>> {
        self.rx.recv().await
    }
}

/// Async trait every provider backend implements. Implementations must be
/// `Send + Sync` so the runner can hold one in an `Arc<dyn ProviderClient>`.
#[async_trait]
pub trait ProviderClient: Send + Sync {
    async fn complete(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError>;

    /// Return a reference to this provider's message normalizer.
    ///
    /// The normalizer converts between canonical [`Message`]s and the
    /// provider's wire format. Returned by reference so the runner can pass
    /// it to the request builder without a per-call allocation. Every
    /// implementor must override this method — there is no trait-default,
    /// so the compiler enforces that every provider supplies one.
    fn message_normalizer(&self) -> &dyn MessageNormalizer;

    /// A stable, non-secret fingerprint of the credential used to authenticate
    /// with this provider. Returned as a hex string derived from a one-way hash
    /// of the API key — the raw key is never stored or logged.
    ///
    /// Used to tag persisted reasoning blocks so that a credential rotation
    /// invalidates them on reconstruction: Anthropic signatures are bound to
    /// both the model and the API key, so a key rotation leaves a stale
    /// signature even when the model is unchanged.
    ///
    /// Defaults to `None` for providers that do not produce reasoning blocks or
    /// where a credential fingerprint is not applicable.
    fn key_fingerprint(&self) -> Option<String> {
        None
    }

    /// The concrete model identifier this provider will use when the agent
    /// profile does not specify one (`agent.model = None`). The engine resolves
    /// `None` to this value before stamping reasoning-block metadata and before
    /// comparing on reconstruct, so default-model agents can replay their
    /// reasoning blocks on resume when the provider's default model is unchanged.
    ///
    /// Defaults to `None` for providers that do not produce reasoning blocks or
    /// do not have a meaningful default-model concept.
    fn default_model(&self) -> Option<String> {
        None
    }

    /// This provider's persisted `providers.toml` default for
    /// `max_output_tokens` (the second tier of the same per-agent ??
    /// persisted-config ?? provider-default precedence [`default_model`]
    /// documents). `None` means the request builder's own hardcoded
    /// fallback applies. Defaults to `None` for providers that don't carry
    /// a persisted opinion.
    fn default_max_output_tokens(&self) -> Option<u32> {
        None
    }

    /// This provider's persisted `providers.toml` default for
    /// `max_context_tokens`. Same precedence tier as
    /// [`default_max_output_tokens`]; `None` means no cap.
    fn default_max_context_tokens(&self) -> Option<u32> {
        None
    }

    /// This provider's persisted `providers.toml` default for
    /// `reasoning_effort`. Same precedence tier as
    /// [`default_max_output_tokens`]; `None` means no reasoning-effort
    /// opinion (the per-turn `ThinkingConfig` mechanism, if any, still
    /// applies independently).
    fn default_reasoning_effort(&self) -> Option<ReasoningEffort> {
        None
    }
}

/// Resolve the model identifier a provider request should carry.
///
/// Precedence (highest first):
/// 1. `agent_model` — an explicit per-agent override (`AgentProfile.model`).
///    Always honored verbatim; this stays free text on purpose so arbitrary
///    or custom model IDs work without engine changes.
/// 2. `provider.default_model()` — the provider's own resolved default. A
///    provider's implementation of this method already folds together its
///    `providers.toml`-persisted model (when the operator set one) and its
///    hardcoded fallback, so this one call covers both.
///
/// Returns `None` only when neither source has an opinion — providers that
/// don't implement [`ProviderClient::default_model`] and carry no agent
/// override. Callers should let the provider's own request-building path
/// apply its transport-level default in that case.
///
/// A future per-thread override is a higher-priority third source: splice it
/// in ahead of `agent_model` (`thread_model.or(agent_model).or_else(...)`) —
/// a one-line change to this function's body, not to its callers.
pub fn resolve_model(agent_model: Option<String>, provider: &dyn ProviderClient) -> Option<String> {
    resolve(agent_model, provider.default_model())
}

/// Resolve the `max_output_tokens` tuning knob a provider request should
/// carry. Same precedence as [`resolve_model`]: an explicit per-agent
/// override wins, otherwise the provider's own resolved default (which
/// already folds together its `providers.toml`-persisted value and its
/// hardcoded fallback) applies.
pub fn resolve_max_output_tokens(agent_value: Option<u32>, provider: &dyn ProviderClient) -> Option<u32> {
    resolve(agent_value, provider.default_max_output_tokens())
}

/// Resolve the `max_context_tokens` tuning knob a provider request should
/// carry. Same precedence as [`resolve_model`].
pub fn resolve_max_context_tokens(agent_value: Option<u32>, provider: &dyn ProviderClient) -> Option<u32> {
    resolve(agent_value, provider.default_max_context_tokens())
}

/// Resolve the `reasoning_effort` tuning knob a provider request should
/// carry. Same precedence as [`resolve_model`].
pub fn resolve_reasoning_effort(
    agent_value: Option<ReasoningEffort>,
    provider: &dyn ProviderClient,
) -> Option<ReasoningEffort> {
    resolve(agent_value, provider.default_reasoning_effort())
}

/// The shared two-tier precedence core every `resolve_*` function in this
/// module reduces to: an explicit override wins outright; otherwise fall
/// back to whatever the next tier already resolved. Generic so `model`,
/// `max_output_tokens`, `max_context_tokens`, and `reasoning_effort` all
/// route through the exact same resolution logic instead of four
/// hand-written copies of `.or_else(...)` that could drift apart.
fn resolve<T>(agent_value: Option<T>, provider_default: Option<T>) -> Option<T> {
    agent_value.or(provider_default)
}

// =============================================================================
// Scripted mock — used by the runner's own unit tests and (via the `mock`
// feature) by the crate-level integration test in `tests/end_to_end.rs`.
// =============================================================================

/// Buffer size for the mpsc channel that backs a scripted stream.
/// Small enough that backpressure kicks in on long scripts so cancel
/// can pre-empt promptly; large enough that small scripts drain in one
/// shot.
#[cfg(any(test, feature = "mock"))]
const MOCK_CHANNEL_BUFFER: usize = 8;

/// A deterministic, scripted provider for tests. Each `complete` call
/// pops the next inner Vec from the script and replays its events
/// over a fresh stream; once the script is empty subsequent calls
/// return [`ProviderError::ScriptExhausted`].
///
/// Cancellation is honored: when the supplied [`CancellationToken`]
/// fires, the in-flight replay task drops its sender and the stream
/// closes after at most one already-buffered event drains. The
/// observable cutoff stays well under 100ms in practice — see the
/// matching test in [`tests`].
#[cfg(any(test, feature = "mock"))]
pub struct MockProviderClient {
    scripts: std::sync::Mutex<std::collections::VecDeque<Vec<CompletionEvent>>>,
    normalizer: crate::message::normalizer::MockNormalizer,
    /// The `system_prompt` from the most recent `complete()` call — lets
    /// tests assert on what the runner actually composed and sent, not just
    /// on the scripted reply.
    last_system_prompt: std::sync::Mutex<Option<String>>,
}

#[cfg(any(test, feature = "mock"))]
impl MockProviderClient {
    /// Build a mock from a per-turn event script. The outer Vec is
    /// turns; the inner Vec is the events for that turn in order.
    pub fn new(turns: Vec<Vec<CompletionEvent>>) -> Self {
        Self {
            scripts: std::sync::Mutex::new(turns.into()),
            normalizer: crate::message::normalizer::MockNormalizer,
            last_system_prompt: std::sync::Mutex::new(None),
        }
    }

    /// Number of turns still to play.
    pub fn remaining_turns(&self) -> usize {
        self.scripts.lock().expect("mock scripts lock").len()
    }

    /// The `system_prompt` passed to the most recent `complete()` call.
    pub fn last_system_prompt(&self) -> Option<String> {
        self.last_system_prompt.lock().expect("mock last_system_prompt lock").clone()
    }
}

#[cfg(any(test, feature = "mock"))]
#[async_trait]
impl ProviderClient for MockProviderClient {
    async fn complete(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        *self.last_system_prompt.lock().expect("mock last_system_prompt lock") =
            request.system_prompt.clone();
        let turn = {
            let mut scripts = self.scripts.lock().expect("mock scripts lock");
            scripts.pop_front()
        };
        let turn = match turn {
            Some(t) => t,
            None => return Err(ProviderError::ScriptExhausted),
        };

        let (tx, rx) = mpsc::channel(MOCK_CHANNEL_BUFFER);
        tokio::spawn(async move {
            for event in turn {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    res = tx.send(Ok(event)) => {
                        if res.is_err() {
                            // Receiver dropped — stop replaying.
                            break;
                        }
                    }
                }
            }
        });
        Ok(CompletionStream { rx })
    }

    fn message_normalizer(&self) -> &dyn MessageNormalizer {
        &self.normalizer
    }
}

#[cfg(test)]
mod tests;
