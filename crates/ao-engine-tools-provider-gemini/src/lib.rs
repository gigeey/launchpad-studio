//! Gemini provider implementation for the runner's `ProviderClient` seam.
//!
//! This crate houses the Gemini-specific request body builder, SSE parser,
//! message normalizer, positional-ordering tracker, and cancellation plumbing.
//! The public surface is intentionally narrow: callers construct a [`GeminiClient`]
//! via [`GeminiClient::from_config`] (integration tests) or
//! [`GeminiClient::from_loaded_config`] (the dogfood app), then pass it to the
//! runner as a `Box<dyn ProviderClient>` or `Arc<dyn ProviderClient>`.

mod auth;
mod messages;
mod ordering;
mod request;
mod response;
mod stop_reason;
mod usage;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use ao_engine_tools_provider_config::{GeminiConfig, ProviderConfig, ProviderConfigError};
use ao_engine_tools_runner::{
    message::MessageNormalizer,
    provider::{CompletionEvent, CompletionRequest, CompletionStream, ProviderClient, ProviderError},
};
use async_trait::async_trait;
use futures_util::StreamExt;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The Gemini provider client.
///
/// Constructed via [`GeminiClient::from_config`] (for tests and explicit
/// configuration) or [`GeminiClient::from_loaded_config`] (reads
/// `<data-root>/providers.toml`). Hold it behind `Arc<dyn ProviderClient>`
/// to pass to the runner.
#[derive(Debug)]
pub struct GeminiClient {
    config: GeminiConfig,
    http: reqwest::Client,
    normalizer: messages::GeminiMessageNormalizer,
    tracker: Arc<Mutex<ordering::ToolCallOrderTracker>>,
    turn_counter: AtomicUsize,
}

/// Error that can occur when constructing a [`GeminiClient`] from the
/// on-disk config.
#[derive(Debug, Error)]
pub enum ClientCreateError {
    /// The config file could not be loaded (missing, unreadable, malformed).
    #[error("failed to load provider config: {0}")]
    Config(#[from] ProviderConfigError),
    /// The config file exists but has no `[gemini]` section.
    #[error("gemini provider is not configured in providers.toml")]
    MissingProvider,
    /// The `[gemini]` section exists but `api_key` is empty.
    #[error("gemini api_key must not be empty")]
    EmptyApiKey,
}

impl GeminiClient {
    /// Build a client from an already-loaded [`GeminiConfig`].
    ///
    /// Returns [`ClientCreateError::EmptyApiKey`] when `api_key` is empty so
    /// that the failure surfaces at construction time rather than at the first
    /// HTTP request.
    pub fn from_config(config: GeminiConfig) -> Result<Self, ClientCreateError> {
        if config.api_key.is_empty() {
            return Err(ClientCreateError::EmptyApiKey);
        }
        let tracker = Arc::new(Mutex::new(ordering::ToolCallOrderTracker::new()));
        Ok(Self {
            config,
            http: reqwest::Client::new(),
            normalizer: messages::GeminiMessageNormalizer::with_tracker(Arc::clone(&tracker)),
            tracker,
            turn_counter: AtomicUsize::new(0),
        })
    }

    /// Build a client by reading `<data-root>/providers.toml`.
    ///
    /// Returns [`ClientCreateError::Config`] if the file is missing or
    /// malformed, [`ClientCreateError::MissingProvider`] if the `[gemini]`
    /// section is absent, and [`ClientCreateError::EmptyApiKey`] if `api_key`
    /// is present but empty.
    pub fn from_loaded_config() -> Result<Self, ClientCreateError> {
        let provider_config = ProviderConfig::load()?;
        let gemini_config = provider_config
            .gemini
            .ok_or(ClientCreateError::MissingProvider)?;
        Self::from_config(gemini_config)
    }

    /// Returns the provider identifier string.
    pub fn provider_id(&self) -> &str {
        "gemini"
    }
}

/// Map a [`response::GeminiError`] to a [`ProviderError`] at the public seam.
fn map_gemini_error(e: response::GeminiError) -> ProviderError {
    match e {
        response::GeminiError::Decode { context } => ProviderError::Transport(context),
        response::GeminiError::RateLimit { message, .. } => {
            ProviderError::Transport(format!("429: {message}"))
        }
        response::GeminiError::Auth { message } => {
            ProviderError::Transport(format!("auth error: {message}"))
        }
        response::GeminiError::Provider { status, body } => {
            ProviderError::Transport(format!("{status}: {body}"))
        }
        response::GeminiError::Transport(e) => ProviderError::Transport(e.to_string()),
    }
}

/// Parse `Retry-After` header value as a `Duration` (seconds only; no HTTP-date).
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
}

/// Extract `error.message` from a Gemini error response body, falling back to
/// the raw body string when the JSON shape is absent or unreadable.
fn extract_error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| body.to_owned())
}

#[async_trait]
impl ProviderClient for GeminiClient {
    async fn complete(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let body = request::build_request_body(&request, &self.normalizer)
            .map_err(|e| ProviderError::Transport(format!("request build failed: {e}")))?;

        let url = auth::endpoint_url(&self.config);
        let response = auth::apply_auth(self.http.post(&url), &self.config)
            .json(&body)
            .send()
            .await
            .map_err(|e| map_gemini_error(response::GeminiError::Transport(e)))?;

        if response.status() != reqwest::StatusCode::OK {
            let status = response.status().as_u16();
            let retry_after = parse_retry_after(response.headers());
            let body_text = response.text().await.unwrap_or_default();
            let err = match status {
                429 => response::GeminiError::RateLimit {
                    message: extract_error_message(&body_text),
                    retry_after,
                },
                401 | 403 => response::GeminiError::Auth {
                    message: extract_error_message(&body_text),
                },
                _ => response::GeminiError::Provider {
                    status,
                    body: body_text,
                },
            };
            return Err(map_gemini_error(err));
        }

        let turn_index = self.turn_counter.fetch_add(1, Ordering::Relaxed);
        let tracker = Arc::clone(&self.tracker);
        let (tx, rx) = mpsc::channel::<Result<CompletionEvent, ProviderError>>(64);
        let byte_stream = Box::pin(response.bytes_stream());

        tokio::spawn(async move {
            let event_stream = Box::pin(response::parse_sse_stream(byte_stream));
            run_translator(event_stream, tx, cancel, tracker, turn_index).await;
        });

        Ok(CompletionStream::new(rx))
    }

    fn message_normalizer(&self) -> &dyn MessageNormalizer {
        &self.normalizer
    }
}

/// Translate a stream of [`response::GeminiStreamEvent`] items into canonical
/// [`CompletionEvent`] items sent on `tx`.
///
/// Runs until the stream is exhausted, a terminal event is received, or the
/// `cancel` token fires. Emits events in parts-array source order for each SSE
/// event. Terminal events emit `Usage` (when present) then `TurnComplete`.
///
/// `global_part_index` tracks the absolute position across ALL parts in ALL
/// SSE events for this turn (text parts included). This matches Gemini's
/// `parts[]` indexing so the denormalizer can re-pair `functionResponse`
/// entries by position.
///
/// ## Part → emission mapping
///
/// | Gemini part           | Canonical emission                                      |
/// |-----------------------|---------------------------------------------------------|
/// | `{ "text": "..." }`   | `AssistantText(text)` immediately                       |
/// | `{ "functionCall" }`  | `ToolUse { id, name, input }` recorded in `tracker`     |
/// | other / unsupported   | silently skipped                                        |
async fn run_translator<S>(
    mut event_stream: S,
    tx: mpsc::Sender<Result<CompletionEvent, ProviderError>>,
    cancel: CancellationToken,
    tracker: Arc<Mutex<ordering::ToolCallOrderTracker>>,
    turn_index: usize,
) where
    S: futures_util::Stream<Item = Result<response::GeminiStreamEvent, response::GeminiError>>
        + Unpin,
{
    let mut global_part_index: usize = 0;
    let mut has_function_call = false;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            ev = event_stream.next() => match ev {
                None => break,
                Some(Err(e)) => {
                    let _ = tx.send(Err(ProviderError::Transport(e.to_string()))).await;
                    break;
                }
                Some(Ok(event)) => {
                    for part in &event.parts {
                        let part_index = global_part_index;
                        global_part_index += 1;

                        if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                            if tx.send(Ok(CompletionEvent::AssistantText(text.to_owned()))).await.is_err() {
                                return;
                            }
                        } else if let Some(fc) = part.get("functionCall") {
                            has_function_call = true;
                            let name = fc.get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_owned();
                            let input = fc.get("args")
                                .cloned()
                                .unwrap_or(serde_json::Value::Object(Default::default()));
                            let id = tracker
                                .lock()
                                .expect("tracker poisoned")
                                .record(turn_index, part_index, &name);
                            if tx.send(Ok(CompletionEvent::ToolUse { id, name, input })).await.is_err() {
                                return;
                            }
                        }
                    }

                    if let Some(reason) = &event.finish_reason {
                        if let Some(usage_val) = &event.usage {
                            let u = usage::map_usage_metadata(usage_val);
                            if tx.send(Ok(CompletionEvent::Usage(u))).await.is_err() {
                                return;
                            }
                        }
                        let stop_reason = stop_reason::map_finish_reason(reason, has_function_call);
                        let _ = tx.send(Ok(CompletionEvent::TurnComplete { stop_reason })).await;
                        break;
                    }
                }
            }
        }
    }
}

/// Test helper: drive the translator against a scripted event list and collect
/// all emitted items.
#[cfg(test)]
pub(crate) async fn run_translator_for_test(
    events: Vec<Result<response::GeminiStreamEvent, response::GeminiError>>,
    cancel: CancellationToken,
    tracker: Arc<Mutex<ordering::ToolCallOrderTracker>>,
    turn_index: usize,
) -> Vec<Result<CompletionEvent, ProviderError>> {
    let (tx, mut rx) = mpsc::channel(64);
    let stream = Box::pin(futures_util::stream::iter(events));
    run_translator(stream, tx, cancel, tracker, turn_index).await;
    let mut results = Vec::new();
    while let Some(item) = rx.recv().await {
        results.push(item);
    }
    results
}

#[cfg(test)]
mod client_tests {
    use super::*;

    fn config_valid() -> GeminiConfig {
        GeminiConfig {
            api_key: "AIza-TEST-KEY".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            model: "gemini-1.5-pro".into(),
        }
    }

    #[test]
    fn from_config_succeeds_with_valid_key() {
        assert!(GeminiClient::from_config(config_valid()).is_ok());
    }

    #[test]
    fn from_config_rejects_empty_api_key() {
        let config = GeminiConfig {
            api_key: "".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            model: "gemini-1.5-pro".into(),
        };
        let err = GeminiClient::from_config(config).unwrap_err();
        assert!(matches!(err, ClientCreateError::EmptyApiKey));
    }
}
