//! Anthropic provider implementation for the runner's `ProviderClient` seam.
//!
//! This crate houses the Anthropic-specific request body builder, hand-rolled
//! SSE parser, message normalizer, and cancellation plumbing. The public surface
//! is intentionally narrow: callers construct an [`AnthropicClient`] via
//! [`AnthropicClient::from_config`] (integration tests) or
//! [`AnthropicClient::from_loaded_config`] (the dogfood CLI), then pass it to
//! the runner as a `Box<dyn ProviderClient>` or `Arc<dyn ProviderClient>`.

mod auth;
mod messages;
mod request;
mod response;
mod stop_reason;
mod usage;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::Instant;

use ao_engine_tools_provider_config::{AnthropicConfig, ProviderConfigError};
use ao_protocol::agent::ReasoningEffort;
use sha2::{Digest, Sha256};
use ao_engine_tools_runner::{
    message::MessageNormalizer,
    provider::{
        CompletionEvent, CompletionRequest, CompletionStream, ProviderClient, ProviderError,
        StopReason,
    },
};
use async_trait::async_trait;
use futures_util::StreamExt;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use response::{AnthropicEvent, ContentBlockKind, DeltaKind};

/// The Anthropic provider client.
///
/// Constructed via [`AnthropicClient::from_config`] (for tests and explicit
/// configuration) or [`AnthropicClient::from_loaded_config`] (reads
/// `<data-root>/providers.toml`). Hold it behind `Arc<dyn ProviderClient>`
/// to pass to the runner.
pub struct AnthropicClient {
    config: AnthropicConfig,
    http: reqwest::Client,
    normalizer: messages::AnthropicNormalizer,
}

/// Error that can occur when constructing an [`AnthropicClient`] from the
/// on-disk config.
#[derive(Debug, Error)]
pub enum ClientCreateError {
    /// The config file could not be loaded (missing, unreadable, malformed).
    #[error("failed to load provider config: {0}")]
    Config(#[from] ProviderConfigError),
    /// The config file exists but has no `[anthropic]` section.
    #[error("anthropic provider is not configured in providers.toml")]
    MissingAnthropicSection,
}

impl AnthropicClient {
    /// Build a client from an already-loaded [`AnthropicConfig`].
    ///
    /// Preferred in integration tests where the caller controls the config.
    pub fn from_config(config: AnthropicConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            normalizer: messages::AnthropicNormalizer,
        }
    }

    /// Build a client by reading `<data-root>/providers.toml`.
    ///
    /// Returns [`ClientCreateError::Config`] if the file is missing or
    /// malformed, and [`ClientCreateError::MissingAnthropicSection`] if the
    /// `[anthropic]` section is absent.
    pub fn from_loaded_config() -> Result<Self, ClientCreateError> {
        let provider_config = ao_engine_tools_provider_config::ProviderConfig::load()?;
        let anthropic_config = provider_config
            .anthropic
            .ok_or(ClientCreateError::MissingAnthropicSection)?;
        Ok(Self::from_config(anthropic_config))
    }

    /// Override the model this client sends on every request, taking
    /// precedence over whatever `providers.toml` (or this crate's hardcoded
    /// fallback) supplied at construction. `None` leaves the loaded/default
    /// model untouched. Callers pass the result of
    /// `ao_engine_tools_runner::provider::resolve_model` here so an
    /// agent-level (and, later, thread-level) override actually reaches the
    /// wire instead of only affecting reasoning-block bookkeeping.
    pub fn with_model(mut self, model: Option<String>) -> Self {
        if let Some(model) = model {
            self.config.model = model;
        }
        self
    }

    /// Override the `max_output_tokens` cap this client sends on every
    /// request, same precedence contract as [`Self::with_model`]. `None`
    /// leaves the loaded/persisted value (or its absence) untouched —
    /// callers pass the result of
    /// `ao_engine_tools_runner::provider::resolve_max_output_tokens`.
    pub fn with_max_output_tokens(mut self, max_output_tokens: Option<u32>) -> Self {
        if let Some(v) = max_output_tokens {
            self.config.max_output_tokens = Some(v);
        }
        self
    }

    /// Override the `max_context_tokens` client-side history budget this
    /// client enforces on every request, same precedence contract as
    /// [`Self::with_model`]. Callers pass the result of
    /// `ao_engine_tools_runner::provider::resolve_max_context_tokens`.
    pub fn with_max_context_tokens(mut self, max_context_tokens: Option<u32>) -> Self {
        if let Some(v) = max_context_tokens {
            self.config.max_context_tokens = Some(v);
        }
        self
    }

    /// Override the `reasoning_effort` level this client sends on every
    /// request, same precedence contract as [`Self::with_model`]. Callers
    /// pass the result of
    /// `ao_engine_tools_runner::provider::resolve_reasoning_effort`.
    pub fn with_reasoning_effort(mut self, reasoning_effort: Option<ReasoningEffort>) -> Self {
        if let Some(v) = reasoning_effort {
            self.config.reasoning_effort = Some(v);
        }
        self
    }
}

#[async_trait]
impl ProviderClient for AnthropicClient {
    async fn complete(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let body = request::build(&self.config, &self.normalizer, &request)
            .map_err(|e| ProviderError::Transport(format!("request build failed: {e}")))?;

        let response = auth::apply_headers(
            self.http
                .post(format!("{}/v1/messages", self.config.base_url)),
            &self.config,
        )
        .json(&body)
        .send()
        .await
        .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if response.status() != reqwest::StatusCode::OK {
            let status = response.status();
            let body_text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Transport(format!(
                "{}: {}",
                status.as_u16(),
                body_text
            )));
        }

        let (tx, rx) = mpsc::channel::<Result<CompletionEvent, ProviderError>>(64);
        let byte_stream = Box::pin(response.bytes_stream());

        tokio::spawn(async move {
            let mut event_stream = Box::pin(response::parse_sse_stream(byte_stream));

            // Per-block kind table: index → ContentBlockKind (for stop dispatch)
            let mut pending: HashMap<u32, ContentBlockKind> = HashMap::new();
            // Per-block JSON accumulator for tool_use input_json_delta
            let mut input_buffers: HashMap<u32, String> = HashMap::new();
            // Per-block wall-clock anchor for thinking blocks. Recorded on
            // `content_block_start[type=thinking]`, consumed on the matching
            // `content_block_stop` to compute `ThinkingEnd.elapsed_ms`. Keyed by
            // block index so concurrent thinking blocks (interleaved with tool
            // use in the same turn) don't smear durations onto each other.
            let mut thinking_anchors: HashMap<u32, Instant> = HashMap::new();
            // Per-block reasoning text accumulator. Each `thinking_delta`
            // appends to the slot at the block's index; on `content_block_stop`
            // the slot is consumed into the `CompletionEvent::ThinkingBlock`
            // sent after `ThinkingEnd` for replay on the next turn.
            let mut thinking_text_buffers: HashMap<u32, String> = HashMap::new();
            // Per-block signature captured from the `signature_delta`. Stored
            // separately from the text buffer because Anthropic emits the
            // signature exactly once per block (not chunked).
            let mut thinking_signatures: HashMap<u32, String> = HashMap::new();
            // Per-block redacted reasoning payload. Captured from the
            // `content_block_start` event (the opaque blob arrives inline,
            // not via deltas) and consumed on the matching
            // `content_block_stop` into a `RedactedThinkingBlock` replay
            // event. Keyed by index so it never collides with concurrent
            // signed thinking blocks in the same turn.
            let mut redacted_thinking_data: HashMap<u32, String> = HashMap::new();
            // Cached from MessageDelta; emitted inside TurnComplete on MessageStop
            let mut cached_stop_reason: Option<StopReason> = None;

            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    ev = event_stream.next() => match ev {
                        None => break,
                        Some(Err(e)) => {
                            let _ = tx.send(Err(e)).await;
                            break;
                        }
                        Some(Ok(event)) => {
                            match event {
                                AnthropicEvent::MessageStart { usage } => {
                                    if let Some(u) = usage::extract_usage(&usage) {
                                        let _ = tx.send(Ok(CompletionEvent::Usage(u))).await;
                                    }
                                }
                                AnthropicEvent::ContentBlockStart { index, content_block } => {
                                    // Thinking blocks need both the kind table
                                    // entry (so stop knows which kind to dispatch
                                    // on) and the wall-clock anchor for the
                                    // matching `ThinkingEnd.elapsed_ms`. The
                                    // canonical `ThinkingStart` is emitted right
                                    // here so the UI mounts a "Thinking…"
                                    // indicator even when `display = "omitted"`
                                    // suppresses every subsequent delta.
                                    match &content_block {
                                        ContentBlockKind::Thinking => {
                                            thinking_anchors.insert(index, Instant::now());
                                            let _ = tx.send(Ok(CompletionEvent::ThinkingStart)).await;
                                        }
                                        ContentBlockKind::RedactedThinking { data } => {
                                            // No UI surface: a redacted block
                                            // carries no readable reasoning, so
                                            // we don't mount the Thinking…
                                            // indicator. Stash the payload for
                                            // the replay event emitted on stop.
                                            redacted_thinking_data.insert(index, data.clone());
                                        }
                                        _ => {}
                                    }
                                    pending.insert(index, content_block);
                                }
                                AnthropicEvent::ContentBlockDelta { index, delta } => {
                                    match delta {
                                        DeltaKind::TextDelta { text } => {
                                            let _ = tx.send(Ok(CompletionEvent::AssistantText(text))).await;
                                        }
                                        DeltaKind::InputJsonDelta { partial_json } => {
                                            input_buffers.entry(index).or_default().push_str(&partial_json);
                                        }
                                        DeltaKind::ThinkingDelta { text } => {
                                            // Belt-and-braces: if a delta arrives
                                            // before we saw `content_block_start`
                                            // (e.g. a future provider variant or
                                            // a parser bug elsewhere), still mount
                                            // the indicator so the UI doesn't
                                            // strand a Thinking… footer with no
                                            // header. No-op when we already
                                            // recorded an anchor.
                                            if !thinking_anchors.contains_key(&index) {
                                                thinking_anchors.insert(index, Instant::now());
                                                let _ = tx.send(Ok(CompletionEvent::ThinkingStart)).await;
                                            }
                                            // Mirror the chunk into the
                                            // per-block text buffer so the
                                            // post-stop `ThinkingBlock`
                                            // event can replay the full
                                            // reasoning back to the API on
                                            // the next turn.
                                            thinking_text_buffers
                                                .entry(index)
                                                .or_default()
                                                .push_str(&text);
                                            let _ = tx
                                                .send(Ok(CompletionEvent::ThinkingDelta { text }))
                                                .await;
                                        }
                                        DeltaKind::SignatureDelta { signature } => {
                                            // Capture the signature for
                                            // replay; no UI surface, but the
                                            // post-stop `ThinkingBlock`
                                            // carries it so the runner's
                                            // assistant turn echoes it back
                                            // verbatim.
                                            tracing::trace!(
                                                index,
                                                signature_len = signature.len(),
                                                "anthropic SSE: signature_delta captured"
                                            );
                                            thinking_signatures.insert(index, signature);
                                        }
                                    }
                                }
                                AnthropicEvent::ContentBlockStop { index } => {
                                    if let Some(anchor) = thinking_anchors.remove(&index) {
                                        let elapsed_ms = anchor.elapsed().as_millis() as u64;
                                        let _ = tx
                                            .send(Ok(CompletionEvent::ThinkingEnd { elapsed_ms }))
                                            .await;
                                        // Drain the per-block text + signature
                                        // accumulators into the replay event.
                                        // Empty text collapses to `None` so a
                                        // signature-only block (the
                                        // `display = "omitted"` shape) round-trips
                                        // canonically — matters because the
                                        // runner inspects `text.is_some()` when
                                        // deciding whether the bubble shows any
                                        // reasoning to the user.
                                        let text = thinking_text_buffers
                                            .remove(&index)
                                            .filter(|s| !s.is_empty());
                                        let signature = thinking_signatures
                                            .remove(&index)
                                            .filter(|s| !s.is_empty());
                                        let _ = tx
                                            .send(Ok(CompletionEvent::ThinkingBlock {
                                                text,
                                                signature,
                                            }))
                                            .await;
                                    }
                                    if let Some(data) = redacted_thinking_data.remove(&index) {
                                        // Redacted blocks produce no streaming
                                        // triplet; the replay event is the only
                                        // signal, carrying the opaque payload
                                        // the runner echoes back next turn.
                                        let _ = tx
                                            .send(Ok(CompletionEvent::RedactedThinkingBlock { data }))
                                            .await;
                                    }
                                    if let Some(ContentBlockKind::ToolUse { id, name }) = pending.remove(&index) {
                                        let json_str = input_buffers.remove(&index).unwrap_or_default();
                                        let input = serde_json::from_str(&json_str)
                                            .unwrap_or(serde_json::Value::Object(Default::default()));
                                        let _ = tx.send(Ok(CompletionEvent::ToolUse { id, name, input })).await;
                                    }
                                }
                                AnthropicEvent::MessageDelta { stop_reason, usage } => {
                                    if let Some(reason) = stop_reason {
                                        cached_stop_reason = Some(stop_reason::map_stop_reason(&reason));
                                    }
                                    if let Some(usage_val) = usage {
                                        if let Some(u) = usage::extract_usage(&usage_val) {
                                            let _ = tx.send(Ok(CompletionEvent::Usage(u))).await;
                                        }
                                    }
                                }
                                AnthropicEvent::MessageStop => {
                                    let sr = cached_stop_reason.unwrap_or(StopReason::Natural);
                                    let _ = tx.send(Ok(CompletionEvent::TurnComplete { stop_reason: sr })).await;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            // tx drops here → channel closes → runner sees recv() → None
        });

        Ok(CompletionStream::new(rx))
    }

    fn message_normalizer(&self) -> &dyn MessageNormalizer {
        &self.normalizer
    }

    fn key_fingerprint(&self) -> Option<String> {
        let mut hasher = Sha256::new();
        hasher.update(self.config.api_key.as_bytes());
        Some(format!("{:x}", hasher.finalize()))
    }

    fn default_model(&self) -> Option<String> {
        Some(self.config.model.clone())
    }

    fn default_max_output_tokens(&self) -> Option<u32> {
        self.config.max_output_tokens
    }

    fn default_max_context_tokens(&self) -> Option<u32> {
        self.config.max_context_tokens
    }

    fn default_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.config.reasoning_effort
    }
}
