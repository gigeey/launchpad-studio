//! OpenAI provider implementation for the runner's `ProviderClient` seam.
//!
//! This crate houses the OpenAI-specific Chat Completions request body builder,
//! hand-rolled SSE parser, message normalizer, and cancellation plumbing. The
//! public surface is intentionally narrow: callers construct an [`OpenAIClient`]
//! via [`OpenAIClient::from_config`] (integration tests) or
//! [`OpenAIClient::from_loaded_config`] (the dogfood app), then pass it to the
//! runner as a `Box<dyn ProviderClient>` or `Arc<dyn ProviderClient>`.

mod auth;
mod messages;
mod request;
mod response;
mod stop_reason;
mod usage;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use ao_engine_tools_provider_config::{OpenAIConfig, ProviderConfigError};
use ao_protocol::agent::ReasoningEffort;
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

/// Accumulated state for one tool call whose argument chunks arrive across
/// multiple [`response::OpenAIEvent::ToolCallDelta`] events.
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments_buf: String,
}

/// The OpenAI provider client.
///
/// Constructed via [`OpenAIClient::from_config`] (for tests and explicit
/// configuration) or [`OpenAIClient::from_loaded_config`] (reads
/// `<data-root>/providers.toml`). Hold it behind `Arc<dyn ProviderClient>`
/// to pass to the runner.
#[derive(Debug)]
pub struct OpenAIClient {
    config: OpenAIConfig,
    http: reqwest::Client,
    normalizer: messages::OpenAINormalizer,
}

/// Error that can occur when constructing an [`OpenAIClient`] from the
/// on-disk config.
#[derive(Debug, Error)]
pub enum ClientCreateError {
    /// The config file could not be loaded (missing, unreadable, malformed).
    #[error("failed to load provider config: {0}")]
    Config(#[from] ProviderConfigError),
    /// The config file exists but has no section for the requested provider
    /// (`"openai"` or `"openrouter"`, matching `providers.toml`'s section name).
    #[error("{0} provider is not configured in providers.toml")]
    MissingProvider(&'static str),
}

impl OpenAIClient {
    /// Build a client from an already-loaded [`OpenAIConfig`].
    ///
    /// Preferred in integration tests where the caller controls the config.
    pub fn from_config(config: OpenAIConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            normalizer: messages::OpenAINormalizer,
        }
    }

    /// Build a client by reading `<data-root>/providers.toml`.
    ///
    /// Returns [`ClientCreateError::Config`] if the file is missing or
    /// malformed, and [`ClientCreateError::MissingProvider`] if the
    /// `[openai]` section is absent.
    pub fn from_loaded_config() -> Result<Self, ClientCreateError> {
        let provider_config = ao_engine_tools_provider_config::ProviderConfig::load()?;
        let openai_config = provider_config
            .openai
            .ok_or(ClientCreateError::MissingProvider("openai"))?;
        Ok(Self::from_config(openai_config))
    }

    /// Build a client for OpenRouter by reading `<data-root>/providers.toml`'s
    /// `[openrouter]` section.
    ///
    /// OpenRouter's chat-completion API is OpenAI-compatible, so this reuses
    /// the same [`OpenAIClient`] transport as [`Self::from_loaded_config`] —
    /// only the section read (and its own base URL / default model) differs.
    /// Returns [`ClientCreateError::Config`] if the file is missing or
    /// malformed, and [`ClientCreateError::MissingProvider`] if the
    /// `[openrouter]` section is absent.
    pub fn from_loaded_config_openrouter() -> Result<Self, ClientCreateError> {
        let provider_config = ao_engine_tools_provider_config::ProviderConfig::load()?;
        let openrouter_config = provider_config
            .openrouter
            .ok_or(ClientCreateError::MissingProvider("openrouter"))?;
        Ok(Self::from_config(openrouter_config.into()))
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
    /// request, same precedence contract as [`Self::with_model`]. Callers
    /// pass the result of
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
impl ProviderClient for OpenAIClient {
    async fn complete(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let body = request::build(&self.config, &request, &self.normalizer)
            .map_err(|e| ProviderError::Transport(format!("request build failed: {e}")))?;

        let response = auth::apply_headers(
            self.http.post(auth::endpoint_url(&self.config)),
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
            let event_stream = Box::pin(response::parse_sse_stream(byte_stream));
            run_translator(event_stream, tx, cancel).await;
        });

        Ok(CompletionStream::new(rx))
    }

    fn message_normalizer(&self) -> &dyn MessageNormalizer {
        &self.normalizer
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

/// Translate a stream of [`response::OpenAIEvent`] items into canonical
/// [`CompletionEvent`] items sent on `tx`.
///
/// Runs until the [`response::OpenAIEvent::Done`] sentinel, a stream error,
/// or the `cancel` token fires. The mpsc sender is dropped on return, closing
/// the [`CompletionStream`] on the receiver side.
///
/// ## Event → emission mapping
///
/// | OpenAI event          | Canonical emission(s)                                 |
/// |-----------------------|-------------------------------------------------------|
/// | `TextDelta`           | `AssistantText(chunk)` immediately                    |
/// | `ToolCallDelta`       | accumulated per index; emitted at `FinishReason`      |
/// | `FinishReason("tool_calls")` | `ToolUse { id, name, input }` per index (sorted) |
/// | `FinishReason(other)` | cached only; no emission until `Done`                 |
/// | `Usage`               | `Usage(u)` via `usage::extract_usage`                 |
/// | `Done`                | `TurnComplete { stop_reason }` then exit              |
///
/// Malformed JSON in a tool call's accumulated arguments emits
/// `CompletionEvent::Error(...)` for that index and continues with other
/// indices — the turn still completes normally.
async fn run_translator<S>(
    mut event_stream: S,
    tx: mpsc::Sender<Result<CompletionEvent, ProviderError>>,
    cancel: CancellationToken,
) where
    S: futures_util::Stream<Item = Result<response::OpenAIEvent, ProviderError>> + Unpin,
{
    let mut tool_calls: HashMap<u32, PartialToolCall> = HashMap::new();
    let mut cached_finish_reason: Option<String> = None;

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
                Some(Ok(event)) => match event {
                    response::OpenAIEvent::TextDelta { content } => {
                        let _ = tx.send(Ok(CompletionEvent::AssistantText(content))).await;
                    }
                    response::OpenAIEvent::ToolCallDelta { index, id, name, arguments_chunk } => {
                        let entry = tool_calls.entry(index).or_insert_with(|| PartialToolCall {
                            id: None,
                            name: None,
                            arguments_buf: String::new(),
                        });
                        if let Some(v) = id {
                            entry.id = Some(v);
                        }
                        if let Some(v) = name {
                            entry.name = Some(v);
                        }
                        if let Some(chunk) = arguments_chunk {
                            entry.arguments_buf.push_str(&chunk);
                        }
                    }
                    response::OpenAIEvent::FinishReason { reason } => {
                        cached_finish_reason = Some(reason.clone());
                        if reason == "tool_calls" {
                            // Drain accumulated tool calls in ascending index order.
                            let mut indices: Vec<u32> = tool_calls.keys().copied().collect();
                            indices.sort_unstable();
                            for idx in indices {
                                if let Some(tc) = tool_calls.remove(&idx) {
                                    let id = tc.id.unwrap_or_default();
                                    let name = tc.name.unwrap_or_default();
                                    match serde_json::from_str::<serde_json::Value>(
                                        &tc.arguments_buf,
                                    ) {
                                        Ok(input) => {
                                            let _ = tx
                                                .send(Ok(CompletionEvent::ToolUse {
                                                    id,
                                                    name,
                                                    input,
                                                }))
                                                .await;
                                        }
                                        Err(_) => {
                                            let _ = tx
                                                .send(Ok(CompletionEvent::Error(format!(
                                                    "malformed tool_call arguments for index {idx}: {}",
                                                    tc.arguments_buf
                                                ))))
                                                .await;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    response::OpenAIEvent::Usage { value } => {
                        if let Some(u) = usage::extract_usage(&value) {
                            let _ = tx.send(Ok(CompletionEvent::Usage(u))).await;
                        }
                    }
                    response::OpenAIEvent::Done => {
                        let stop_reason = cached_finish_reason
                            .as_deref()
                            .map(stop_reason::map_finish_reason)
                            .unwrap_or(StopReason::Natural);
                        let _ = tx
                            .send(Ok(CompletionEvent::TurnComplete { stop_reason }))
                            .await;
                        break;
                    }
                },
            }
        }
    }
    // tx drops here → channel closes → CompletionStream returns None
}

/// Test helper: run the translator state machine against a scripted event list
/// and collect all emitted items. Not compiled outside `#[cfg(test)]`.
#[cfg(test)]
pub(crate) async fn run_translator_for_test(
    events: Vec<Result<response::OpenAIEvent, ProviderError>>,
    cancel: CancellationToken,
) -> Vec<Result<CompletionEvent, ProviderError>> {
    let (tx, mut rx) = mpsc::channel(64);
    let stream = Box::pin(futures_util::stream::iter(events));
    run_translator(stream, tx, cancel).await;
    let mut results = Vec::new();
    while let Some(item) = rx.recv().await {
        results.push(item);
    }
    results
}
