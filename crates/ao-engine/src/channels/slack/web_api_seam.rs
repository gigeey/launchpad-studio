//! The Slack Web API HTTP boundary [`super::test_connection`]'s
//! orchestration logic runs against — the Slack analogue of
//! [`crate::channels::discord::gateway_seam::GatewaySeam`]. [`SlackApiSeam`]
//! exists so Test Connection's per-check logic is unit-testable against a
//! scripted fake ([`super::fake_seam::FakeSlackApiSeam`]) without a live
//! call to `slack.com`. [`ReqwestSlackApiSeam`] is the only implementation
//! that actually calls Slack.
//!
//! [`SlackApiSeam::post_message`] is the outbound half — a single
//! `chat.postMessage` call — added alongside the two Test Connection checks
//! since all three are the same "authenticated Web API POST" shape. Slack's
//! outbound relay (`super::run_outbound_observer`) never calls it directly:
//! [`send_chunked_message`] is the entry point it uses, splitting a reply at
//! [`SLACK_CHUNK_THRESHOLD_CHARS`] via the shared
//! [`crate::channels::relay::chunker::chunk_text`] first — Slack's `text`
//! field tops out around 4,000 characters, and chunking is
//! mechanical enough to belong next to the call it feeds rather than in the
//! relay wiring itself.

use async_trait::async_trait;
use thiserror::Error;

use ao_protocol::slack_test_connection::{SlackCheckFailure, SlackFailureKind};

use crate::channels::relay::chunker::chunk_text;

const SLACK_API_BASE: &str = "https://slack.com/api";

/// Chunking threshold for an outbound reply — comfortably under Slack's
/// ~4,000-character `text` cap, leaving headroom for
/// whatever `mrkdwn` rendering later adds on top of the raw text this
/// chunks today.
pub(crate) const SLACK_CHUNK_THRESHOLD_CHARS: usize = 3000;

/// One Slack Web API call's failure, classified so a caller can tell "we
/// never reached Slack" apart from "Slack reached, and rejected the token."
/// See [`SlackFailureKind`] (this type converts into it) for the full
/// rationale.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SlackApiCallError {
    /// The HTTP round trip itself didn't complete — DNS, connection, TLS, a
    /// timeout, or a non-2xx status this call has no way to attribute to
    /// the token (Slack's Web API answers auth rejections with HTTP 200 and
    /// `ok: false`, so a non-2xx here means something upstream of Slack's
    /// own auth check went wrong).
    #[error("network error contacting Slack: {0}")]
    Network(String),
    /// Slack answered the call and rejected it: `ok: false` with an
    /// `error` code (`invalid_auth`, `missing_scope`, `token_revoked`,
    /// `account_inactive`, ...).
    #[error("Slack rejected the request: {0}")]
    Auth(String),
}

impl From<SlackApiCallError> for SlackCheckFailure {
    fn from(err: SlackApiCallError) -> Self {
        match err {
            SlackApiCallError::Network(message) => SlackCheckFailure { kind: SlackFailureKind::Network, message },
            SlackApiCallError::Auth(message) => SlackCheckFailure { kind: SlackFailureKind::Auth, message },
        }
    }
}

/// Everything [`super::test_connection::run_test_connection`] needs out of a
/// successful `auth.test` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackAuthTestResult {
    /// Workspace (team) display name.
    pub team: String,
    pub team_id: String,
    /// The authenticated identity's display name — for a bot token, this is
    /// the bot's own handle.
    pub user: String,
    /// The authenticated identity's user id — for a bot token, this is the
    /// `bot_user_id` needed downstream for the bot-echo guard.
    pub user_id: String,
    /// Scopes granted to the token used for this call, read off the
    /// response's `x-oauth-scopes` header (Slack does not return scopes in
    /// the JSON body itself). This is the only source this seam has for
    /// "what scopes does this token actually have" — there is no separate
    /// introspection endpoint a bot token can call.
    pub granted_scopes: Vec<String>,
}

/// The three Slack Web API calls Test Connection needs. `pub`, not
/// `pub(crate)`, so `ao-server`'s route handler (and its own tests, via
/// [`super::fake_seam::FakeSlackApiSeam`]) can drive it without reaching
/// into this crate's internals.
#[async_trait]
pub trait SlackApiSeam: Send + Sync {
    /// `POST auth.test` with `bot_token` as a bearer token. Confirms the bot
    /// token works and captures workspace/bot identity plus (via the
    /// response header) the token's granted scopes.
    async fn auth_test(&self, bot_token: &str) -> Result<SlackAuthTestResult, SlackApiCallError>;

    /// `POST apps.connections.open` with `app_token` as a bearer token.
    /// Confirms the app-level token works. Deliberately returns nothing but
    /// success/failure: this seam never connects to the `wss://` URL Slack
    /// hands back — doing so would open the actual Socket Mode connection,
    /// which is the runner, not a one-shot setup-time check.
    async fn connections_open(&self, app_token: &str) -> Result<(), SlackApiCallError>;

    /// `POST chat.postMessage` with `bot_token` as a bearer token, sending
    /// one already-length-checked `text` chunk to `channel`. `thread_ts`,
    /// when given, threads the reply under that root — "the agent
    /// always replies in-thread" — and is `None` only for a DM, which has no
    /// Slack thread concept of its own.
    async fn post_message(
        &self,
        bot_token: &str,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<(), SlackApiCallError>;
}

/// Splits `text` at [`SLACK_CHUNK_THRESHOLD_CHARS`] and sends each
/// non-blank chunk via [`SlackApiSeam::post_message`], in order. Stops and
/// returns the first chunk's failure rather than sending the rest out of
/// order — mirroring
/// [`crate::channels::discord::outbound::relay_reply`]'s chunk loop. A chunk
/// that lands entirely on whitespace (a chunk-boundary artifact, not
/// meaningful content) is skipped rather than sent, since Slack rejects an
/// effectively-empty `text`.
pub(crate) async fn send_chunked_message(
    seam: &dyn SlackApiSeam,
    bot_token: &str,
    channel: &str,
    thread_ts: Option<&str>,
    text: &str,
) -> Result<(), SlackApiCallError> {
    for chunk in chunk_text(text, SLACK_CHUNK_THRESHOLD_CHARS) {
        if chunk.trim().is_empty() {
            continue;
        }
        seam.post_message(bot_token, channel, thread_ts, chunk).await?;
    }
    Ok(())
}

/// Real [`SlackApiSeam`]: calls `slack.com/api` over HTTPS.
pub struct ReqwestSlackApiSeam {
    http: reqwest::Client,
}

/// Bound on a single Test Connection call. Generous relative to Slack's
/// documented response times, but firm enough that a hung connection can't
/// stall the HTTP route handler indefinitely.
const HTTP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

impl ReqwestSlackApiSeam {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            // Only errors on a broken TLS backend or resolver setup, never
            // on config values like a fixed timeout.
            .expect("slack web api client with a fixed timeout must always build");
        Self { http }
    }
}

impl Default for ReqwestSlackApiSeam {
    fn default() -> Self {
        Self::new()
    }
}

/// Parses Slack's `x-oauth-scopes` response header — a comma-separated
/// scope list Slack attaches to every authenticated Web API response,
/// reflecting the scopes granted to whichever token made the call. This is
/// the only way to read a bot token's granted scopes back without a
/// separate OAuth install flow to record them at install time.
fn granted_scopes_from_headers(headers: &reqwest::header::HeaderMap) -> Vec<String> {
    headers
        .get("x-oauth-scopes")
        .and_then(|value| value.to_str().ok())
        .map(|raw| raw.split(',').map(|scope| scope.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Shared response handling for both calls: a transport failure or non-2xx
/// status is [`SlackApiCallError::Network`]; a parsed `{"ok": false, ...}`
/// body is [`SlackApiCallError::Auth`]; otherwise the parsed body is handed
/// to `on_ok`.
async fn call_slack_api<T>(
    response: Result<reqwest::Response, reqwest::Error>,
    on_ok: impl FnOnce(&serde_json::Value, Vec<String>) -> T,
) -> Result<T, SlackApiCallError> {
    let response = response.map_err(|e| SlackApiCallError::Network(e.to_string()))?;
    let granted_scopes = granted_scopes_from_headers(response.headers());
    let status = response.status();
    if !status.is_success() {
        return Err(SlackApiCallError::Network(format!("unexpected HTTP status {status}")));
    }
    let body: serde_json::Value = response.json().await.map_err(|e| SlackApiCallError::Network(e.to_string()))?;
    if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let error = body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown_error").to_string();
        return Err(SlackApiCallError::Auth(error));
    }
    Ok(on_ok(&body, granted_scopes))
}

#[async_trait]
impl SlackApiSeam for ReqwestSlackApiSeam {
    async fn auth_test(&self, bot_token: &str) -> Result<SlackAuthTestResult, SlackApiCallError> {
        let response = self
            .http
            .post(format!("{SLACK_API_BASE}/auth.test"))
            .header("Authorization", format!("Bearer {bot_token}"))
            .send()
            .await;
        call_slack_api(response, |body, granted_scopes| SlackAuthTestResult {
            team: body.get("team").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            team_id: body.get("team_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            user: body.get("user").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            user_id: body.get("user_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            granted_scopes,
        })
        .await
    }

    async fn connections_open(&self, app_token: &str) -> Result<(), SlackApiCallError> {
        let response = self
            .http
            .post(format!("{SLACK_API_BASE}/apps.connections.open"))
            .header("Authorization", format!("Bearer {app_token}"))
            .send()
            .await;
        call_slack_api(response, |_body, _granted_scopes| ()).await
    }

    async fn post_message(
        &self,
        bot_token: &str,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<(), SlackApiCallError> {
        let mut body = serde_json::json!({ "channel": channel, "text": text });
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = serde_json::Value::String(thread_ts.to_string());
        }
        let response = self
            .http
            .post(format!("{SLACK_API_BASE}/chat.postMessage"))
            .header("Authorization", format!("Bearer {bot_token}"))
            .json(&body)
            .send()
            .await;
        call_slack_api(response, |_body, _granted_scopes| ()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_error_converts_to_a_network_failure() {
        let failure: SlackCheckFailure = SlackApiCallError::Network("connection refused".to_string()).into();
        assert_eq!(failure.kind, SlackFailureKind::Network);
        assert_eq!(failure.message, "connection refused");
    }

    #[test]
    fn auth_error_converts_to_an_auth_failure() {
        let failure: SlackCheckFailure = SlackApiCallError::Auth("invalid_auth".to_string()).into();
        assert_eq!(failure.kind, SlackFailureKind::Auth);
        assert_eq!(failure.message, "invalid_auth");
    }

    #[test]
    fn granted_scopes_from_headers_splits_and_trims_the_comma_separated_list() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-oauth-scopes", "chat:write, users:read ,im:history".parse().unwrap());
        assert_eq!(
            granted_scopes_from_headers(&headers),
            vec!["chat:write".to_string(), "users:read".to_string(), "im:history".to_string()]
        );
    }

    #[test]
    fn granted_scopes_from_headers_is_empty_when_the_header_is_absent() {
        let headers = reqwest::header::HeaderMap::new();
        assert!(granted_scopes_from_headers(&headers).is_empty());
    }

    // --- send_chunked_message ---

    use std::sync::Mutex as StdMutex;

    /// Records every `post_message` call instead of hitting `slack.com` —
    /// scoped to this module's own chunking tests rather than extending
    /// [`super::super::fake_seam::FakeSlackApiSeam`], which exists for
    /// Test Connection's `auth_test`/`connections_open` checks and has no
    /// callers that need `post_message` scripted today.
    #[derive(Default)]
    struct RecordingSeam {
        calls: StdMutex<Vec<(String, Option<String>, String)>>,
        fail_on_call: Option<usize>,
    }

    #[async_trait]
    impl SlackApiSeam for RecordingSeam {
        async fn auth_test(&self, _bot_token: &str) -> Result<SlackAuthTestResult, SlackApiCallError> {
            unimplemented!("not exercised by these tests")
        }

        async fn connections_open(&self, _app_token: &str) -> Result<(), SlackApiCallError> {
            unimplemented!("not exercised by these tests")
        }

        async fn post_message(
            &self,
            _bot_token: &str,
            channel: &str,
            thread_ts: Option<&str>,
            text: &str,
        ) -> Result<(), SlackApiCallError> {
            let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
            let call_index = calls.len();
            calls.push((channel.to_string(), thread_ts.map(str::to_string), text.to_string()));
            if self.fail_on_call == Some(call_index) {
                return Err(SlackApiCallError::Network("simulated failure".to_string()));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn send_chunked_message_sends_short_text_as_a_single_chunk() {
        let seam = RecordingSeam::default();
        send_chunked_message(&seam, "xoxb-test", "C123", Some("111.000"), "hello there").await.unwrap();

        let calls = seam.calls.into_inner().unwrap();
        assert_eq!(calls, vec![("C123".to_string(), Some("111.000".to_string()), "hello there".to_string())]);
    }

    #[tokio::test]
    async fn send_chunked_message_splits_long_text_into_multiple_ordered_sends() {
        let seam = RecordingSeam::default();
        let text = "x".repeat(SLACK_CHUNK_THRESHOLD_CHARS * 2 + 10);

        send_chunked_message(&seam, "xoxb-test", "C123", None, &text).await.unwrap();

        let calls = seam.calls.into_inner().unwrap();
        assert!(calls.len() > 1, "expected more than one chunked send");
        for (channel, thread_ts, chunk) in &calls {
            assert_eq!(channel, "C123");
            assert_eq!(*thread_ts, None);
            assert!(chunk.chars().count() <= SLACK_CHUNK_THRESHOLD_CHARS);
        }
        assert_eq!(calls.iter().map(|(_, _, chunk)| chunk.as_str()).collect::<String>(), text);
    }

    #[tokio::test]
    async fn send_chunked_message_skips_a_whitespace_only_chunk() {
        let seam = RecordingSeam::default();
        // A trailing blank line beyond the threshold lands on its own chunk.
        let text = format!("{}\n\n   ", "y".repeat(SLACK_CHUNK_THRESHOLD_CHARS - 1));

        send_chunked_message(&seam, "xoxb-test", "C123", None, &text).await.unwrap();

        let calls = seam.calls.into_inner().unwrap();
        assert!(calls.iter().all(|(_, _, chunk)| !chunk.trim().is_empty()), "no whitespace-only chunk should be sent");
    }

    #[tokio::test]
    async fn send_chunked_message_stops_at_the_first_failing_chunk() {
        let seam = RecordingSeam { fail_on_call: Some(1), ..Default::default() };
        let text = "x".repeat(SLACK_CHUNK_THRESHOLD_CHARS * 3);

        let result = send_chunked_message(&seam, "xoxb-test", "C123", None, &text).await;

        assert!(result.is_err());
        let calls = seam.calls.into_inner().unwrap();
        assert_eq!(calls.len(), 2, "must stop after the failing chunk rather than sending the rest out of order");
    }
}
