//! Thin client for the Telegram Bot API.

use std::time::Duration;

use serde::Deserialize;
use tracing::info;

const API_BASE: &str = "https://api.telegram.org";

/// Overrides the Telegram API base URL. Unset in production; tests point
/// this at a local mock server instead of the real Telegram Bot API.
const API_BASE_ENV_VAR: &str = "LAUNCHPAD_TELEGRAM_API_BASE_URL";

/// How much longer than Telegram's own long-poll `timeout` the outer HTTP
/// request is allowed to run before `reqwest` gives up. Telegram holds the
/// connection open for up to `timeout` seconds waiting for an update; without
/// this margin a slow-but-healthy poll would be indistinguishable from a
/// hung connection.
const LONG_POLL_TIMEOUT_MARGIN_SECS: u64 = 10;

/// Bot identity returned by `getMe`.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramBotInfo {
    pub id: i64,
    pub is_bot: bool,
    pub username: String,
    pub first_name: String,
}

/// One pending update from `getUpdates`. Only the fields the inbound bridge
/// needs are modeled — Telegram's `Update` object also carries
/// `edited_message`, `callback_query`, and several other kinds this bridge
/// doesn't act on yet, so `message` is optional and unrecognized updates are
/// simply skipped by the caller (their `update_id` still advances the poll
/// offset).
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramUpdate {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub chat: TelegramChat,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub from: Option<TelegramUser>,
    /// `@mention`/`text_mention`/`bot_command`/URL/etc. markup ranges over
    /// `text`. Telegram omits this key entirely when a message carries none,
    /// so it defaults to empty rather than failing deserialization.
    #[serde(default)]
    pub entities: Vec<TelegramMessageEntity>,
    /// The message this one was sent in reply to, if any — lets a consumer
    /// tell whether a reply was aimed at the bot's own prior message (via
    /// this field's `from`) versus someone else's. Boxed since a message can
    /// reply to another message, unbounded recursion depth in principle.
    #[serde(default)]
    pub reply_to_message: Option<Box<TelegramMessage>>,
}

/// Telegram's `chat.type` discriminant. Distinguishes a 1:1 private chat
/// from a group/supergroup/channel, which the inbound bridge needs to decide
/// whether every message should be treated as directed at the bot (private)
/// or only ones that `@mention` it / reply to it (group and up).
/// `#[serde(other)]` catches any value Telegram adds later so an unrecognized
/// chat type never breaks deserialization of the rest of the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelegramChatType {
    Private,
    Group,
    Supergroup,
    Channel,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub chat_type: TelegramChatType,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramUser {
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
}

/// One `entities` item on a Telegram message — bold/italic markup, an
/// `@username` mention, a `text_mention` (an inline mention of a user who
/// has no `@username`, so Telegram embeds the user directly in `user`
/// instead), a `/command`, a URL, etc.
///
/// `offset` and `length` are measured in UTF-16 code units into the
/// message's `text` (Telegram's own indexing scheme), not UTF-8 bytes or
/// Rust `char`s — a consumer that wants the substring this entity covers
/// must index into a UTF-16 encoding of `text`, or it can slice a
/// multi-byte character (e.g. an emoji) in half.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramMessageEntity {
    #[serde(rename = "type")]
    pub entity_type: TelegramMessageEntityType,
    pub offset: i64,
    pub length: i64,
    /// Only present when `entity_type` is `TextMention` — the user that
    /// entity refers to.
    #[serde(default)]
    pub user: Option<TelegramUser>,
}

/// Telegram's `MessageEntity.type` discriminant. `#[serde(other)]` catches
/// any value Telegram adds later (or one this bridge doesn't otherwise act
/// on) so an unrecognized entity type never breaks deserialization of the
/// rest of the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelegramMessageEntityType {
    Mention,
    TextMention,
    BotCommand,
    Url,
    Hashtag,
    Cashtag,
    Email,
    PhoneNumber,
    Bold,
    Italic,
    Underline,
    Strikethrough,
    Spoiler,
    Code,
    Pre,
    TextLink,
    CustomEmoji,
    #[serde(other)]
    Other,
}

#[derive(Debug, thiserror::Error)]
pub enum TelegramApiError {
    /// Telegram rejected the token (HTTP 401).
    #[error("invalid Telegram bot token")]
    InvalidToken,

    /// A non-401 error response, or a `200` body with `ok: false`. Carries
    /// the HTTP status so callers can tell a rate limit (429) apart from a
    /// server-side hiccup (5xx) for backoff purposes.
    #[error("Telegram API request failed with status {0}")]
    ApiStatus(u16),

    #[error("request to Telegram failed: {0}")]
    Request(#[from] reqwest::Error),
}

#[derive(Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
}

/// Client for the subset of the Telegram Bot API the agent-setup surface
/// needs. Holds no per-bot state — the token is passed per call so one
/// client can validate tokens for any number of agents.
#[derive(Debug, Clone)]
pub struct TelegramClient {
    http: reqwest::Client,
    base_url: String,
}

impl Default for TelegramClient {
    fn default() -> Self {
        Self::new()
    }
}

impl TelegramClient {
    pub fn new() -> Self {
        let base_url = std::env::var(API_BASE_ENV_VAR).unwrap_or_else(|_| API_BASE.to_string());
        Self { http: reqwest::Client::new(), base_url }
    }

    /// Resolves the bot identity for `token` via Telegram's `getMe`
    /// endpoint. Any non-success response (including HTTP 401 for a
    /// malformed or revoked token) is reported as
    /// [`TelegramApiError::InvalidToken`].
    pub async fn get_me(&self, token: &str) -> Result<TelegramBotInfo, TelegramApiError> {
        let url = format!("{}/bot{token}/getMe", self.base_url);
        let response = self.http.get(&url).send().await?;

        if !response.status().is_success() {
            return Err(TelegramApiError::InvalidToken);
        }

        let body: TelegramResponse<TelegramBotInfo> = response.json().await?;
        parse_get_me_body(body)
    }

    /// Long-polls Telegram's `getUpdates` for new messages sent to `token`'s
    /// bot. `offset` should be one past the highest `update_id` already
    /// processed (Telegram then treats every lower id as acknowledged and
    /// never redelivers it); pass `None` to receive whatever is currently
    /// pending. `timeout_secs` is Telegram's own long-poll wait — the
    /// request blocks server-side for up to that long before returning an
    /// empty result.
    pub async fn get_updates(
        &self,
        token: &str,
        offset: Option<i64>,
        timeout_secs: u32,
    ) -> Result<Vec<TelegramUpdate>, TelegramApiError> {
        let url = format!("{}/bot{token}/getUpdates", self.base_url);
        let mut query = vec![("timeout", timeout_secs.to_string())];
        if let Some(offset) = offset {
            query.push(("offset", offset.to_string()));
        }

        let response = self
            .http
            .get(&url)
            .query(&query)
            .timeout(Duration::from_secs(
                u64::from(timeout_secs) + LONG_POLL_TIMEOUT_MARGIN_SECS,
            ))
            .send()
            .await?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(TelegramApiError::InvalidToken);
        }
        if !status.is_success() {
            return Err(TelegramApiError::ApiStatus(status.as_u16()));
        }

        let body: TelegramResponse<Vec<TelegramUpdate>> = response.json().await?;
        match (body.ok, body.result) {
            (true, Some(updates)) => Ok(updates),
            _ => Err(TelegramApiError::ApiStatus(status.as_u16())),
        }
    }

    /// Sends `text` to `chat_id` via Telegram's `sendMessage`. `parse_mode`
    /// is forwarded to Telegram as-is when set (e.g. `Some("HTML")` to have
    /// Telegram render a supported markup subset instead of showing `text`
    /// as literal characters); pass `None` for a plain-text send. Error
    /// handling mirrors [`Self::get_updates`]: HTTP 401 is reported as
    /// [`TelegramApiError::InvalidToken`], any other non-2xx status or a
    /// `200` body with `ok: false` as [`TelegramApiError::ApiStatus`].
    pub async fn send_message(
        &self,
        token: &str,
        chat_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<(), TelegramApiError> {
        let url = format!("{}/bot{token}/sendMessage", self.base_url);
        let mut body = serde_json::json!({ "chat_id": chat_id, "text": text });
        if let Some(mode) = parse_mode {
            body["parse_mode"] = serde_json::Value::String(mode.to_string());
        }
        let response = self.http.post(&url).json(&body).send().await?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(TelegramApiError::InvalidToken);
        }
        if !status.is_success() {
            return Err(TelegramApiError::ApiStatus(status.as_u16()));
        }

        let body: TelegramResponse<serde_json::Value> = response.json().await?;
        if body.ok {
            Ok(())
        } else {
            Err(TelegramApiError::ApiStatus(status.as_u16()))
        }
    }

    /// Sends a `sendChatAction` ping for `chat_id` — e.g. `"typing"` — via
    /// Telegram's Bot API. Used to keep the native typing indicator alive
    /// while a turn is still running, since Telegram auto-expires it after a
    /// few seconds of inactivity. Error handling mirrors [`Self::send_message`]:
    /// HTTP 401 is reported as [`TelegramApiError::InvalidToken`], any other
    /// non-2xx status or a `200` body with `ok: false` as
    /// [`TelegramApiError::ApiStatus`].
    pub async fn send_chat_action(
        &self,
        token: &str,
        chat_id: i64,
        action: &str,
    ) -> Result<(), TelegramApiError> {
        let url = format!("{}/bot{token}/sendChatAction", self.base_url);
        let response = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "chat_id": chat_id, "action": action }))
            .send()
            .await?;

        let status = response.status();
        // Diagnostic: dump Telegram's literal status + body for this call so
        // a live run can show whether Telegram is actually reporting
        // `ok:true` (vs. accepting the request only to reject the action
        // itself) even when the outer HTTP status is 2xx.
        let body_bytes = response.bytes().await?;
        info!(
            chat_id = %chat_id,
            action = %action,
            status = %status.as_u16(),
            body = %String::from_utf8_lossy(&body_bytes),
            "telegram sendChatAction raw response"
        );

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(TelegramApiError::InvalidToken);
        }
        if !status.is_success() {
            return Err(TelegramApiError::ApiStatus(status.as_u16()));
        }

        let body: TelegramResponse<serde_json::Value> = match serde_json::from_slice(&body_bytes)
        {
            Ok(body) => body,
            Err(_) => return Err(TelegramApiError::ApiStatus(status.as_u16())),
        };
        if body.ok {
            Ok(())
        } else {
            Err(TelegramApiError::ApiStatus(status.as_u16()))
        }
    }
}

fn parse_get_me_body(
    body: TelegramResponse<TelegramBotInfo>,
) -> Result<TelegramBotInfo, TelegramApiError> {
    match (body.ok, body.result) {
        (true, Some(info)) => Ok(info),
        _ => Err(TelegramApiError::InvalidToken),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `TelegramClient::new()` reads a process-wide env var to redirect
    // requests at a mock server. Shared across `telegram`'s submodules —
    // see `super::super::test_env` — since `bridge` and `outbound`'s tests
    // mutate the same env var and would otherwise race under parallel test
    // threads.
    use crate::telegram::test_env::lock as lock_env;

    struct EnvGuard {
        prior: Option<String>,
    }

    impl EnvGuard {
        fn set(base_url: &str) -> Self {
            let prior = std::env::var(API_BASE_ENV_VAR).ok();
            std::env::set_var(API_BASE_ENV_VAR, base_url);
            Self { prior }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(API_BASE_ENV_VAR, v),
                None => std::env::remove_var(API_BASE_ENV_VAR),
            }
        }
    }

    #[tokio::test]
    async fn get_me_returns_bot_info_on_success() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bot123:abc/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "id": 42,
                    "is_bot": true,
                    "username": "axew_research_bot",
                    "first_name": "Axew Research",
                }
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let info = client.get_me("123:abc").await.expect("get_me should succeed");
        assert_eq!(info.username, "axew_research_bot");
        assert_eq!(info.id, 42);
    }

    #[tokio::test]
    async fn get_me_reports_invalid_token_on_http_401() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/botbad-token/getMe"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Unauthorized",
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let err = client.get_me("bad-token").await.unwrap_err();
        assert!(matches!(err, TelegramApiError::InvalidToken));
    }

    #[tokio::test]
    async fn get_updates_parses_a_text_message_and_sends_offset() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bot123:abc/getUpdates"))
            .and(query_param("offset", "7"))
            .and(query_param("timeout", "30"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": [
                    {
                        "update_id": 7,
                        "message": {
                            "message_id": 99,
                            "chat": { "id": 555, "type": "private" },
                            "text": "hello from telegram",
                            "from": { "id": 111, "username": "axew" }
                        }
                    }
                ]
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let updates = client
            .get_updates("123:abc", Some(7), 30)
            .await
            .expect("get_updates should succeed");

        assert_eq!(updates.len(), 1);
        let message = updates[0].message.as_ref().expect("message present");
        assert_eq!(updates[0].update_id, 7);
        assert_eq!(message.chat.id, 555);
        assert_eq!(message.chat.chat_type, TelegramChatType::Private);
        assert_eq!(message.text.as_deref(), Some("hello from telegram"));
        assert_eq!(message.from.as_ref().unwrap().username.as_deref(), Some("axew"));
        assert!(message.entities.is_empty());
        assert!(message.reply_to_message.is_none());
    }

    #[tokio::test]
    async fn get_updates_parses_a_supergroup_message_with_a_mention_entity() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bot123:abc/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": [
                    {
                        "update_id": 8,
                        "message": {
                            "message_id": 100,
                            "chat": { "id": -100555, "type": "supergroup" },
                            "text": "hey @axew_research_bot can you help?",
                            "from": { "id": 111, "username": "axew" },
                            "entities": [
                                { "type": "mention", "offset": 4, "length": 18 }
                            ]
                        }
                    }
                ]
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let updates = client
            .get_updates("123:abc", None, 30)
            .await
            .expect("get_updates should succeed");

        let message = updates[0].message.as_ref().expect("message present");
        assert_eq!(message.chat.chat_type, TelegramChatType::Supergroup);
        assert_eq!(message.entities.len(), 1);
        assert_eq!(message.entities[0].entity_type, TelegramMessageEntityType::Mention);
        assert_eq!(message.entities[0].offset, 4);
        assert_eq!(message.entities[0].length, 18);
        assert!(message.entities[0].user.is_none());
    }

    #[tokio::test]
    async fn get_updates_parses_a_reply_to_message() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bot123:abc/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": [
                    {
                        "update_id": 9,
                        "message": {
                            "message_id": 101,
                            "chat": { "id": 555, "type": "private" },
                            "text": "thanks!",
                            "from": { "id": 111, "username": "axew" },
                            "reply_to_message": {
                                "message_id": 99,
                                "chat": { "id": 555, "type": "private" },
                                "text": "here's your answer",
                                "from": { "id": 42, "username": "axew_research_bot" }
                            }
                        }
                    }
                ]
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let updates = client
            .get_updates("123:abc", None, 30)
            .await
            .expect("get_updates should succeed");

        let message = updates[0].message.as_ref().expect("message present");
        let reply_to = message.reply_to_message.as_ref().expect("reply_to_message present");
        assert_eq!(reply_to.message_id, 99);
        assert_eq!(reply_to.from.as_ref().unwrap().id, 42);
        assert_eq!(reply_to.from.as_ref().unwrap().username.as_deref(), Some("axew_research_bot"));
    }

    #[tokio::test]
    async fn get_updates_parses_a_text_mention_entity_carrying_a_user() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bot123:abc/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": [
                    {
                        "update_id": 10,
                        "message": {
                            "message_id": 102,
                            "chat": { "id": -100555, "type": "group" },
                            "text": "hey Alex can you help?",
                            "from": { "id": 111, "username": "axew" },
                            "entities": [
                                {
                                    "type": "text_mention",
                                    "offset": 4,
                                    "length": 4,
                                    "user": { "id": 222, "username": "alex_no_username_actually" }
                                }
                            ]
                        }
                    }
                ]
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let updates = client
            .get_updates("123:abc", None, 30)
            .await
            .expect("get_updates should succeed");

        let message = updates[0].message.as_ref().expect("message present");
        assert_eq!(message.chat.chat_type, TelegramChatType::Group);
        let entity = &message.entities[0];
        assert_eq!(entity.entity_type, TelegramMessageEntityType::TextMention);
        let user = entity.user.as_ref().expect("text_mention entity carries a user");
        assert_eq!(user.id, 222);
    }

    #[tokio::test]
    async fn get_updates_omits_offset_query_param_when_none() {
        use wiremock::matchers::{method, path, query_param_is_missing};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bot123:abc/getUpdates"))
            .and(query_param_is_missing("offset"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ok": true, "result": [] })),
            )
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let updates = client
            .get_updates("123:abc", None, 30)
            .await
            .expect("get_updates should succeed");
        assert!(updates.is_empty());
    }

    #[tokio::test]
    async fn get_updates_reports_invalid_token_on_http_401() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/botbad-token/getUpdates"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Unauthorized",
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let err = client.get_updates("bad-token", None, 30).await.unwrap_err();
        assert!(matches!(err, TelegramApiError::InvalidToken));
    }

    #[tokio::test]
    async fn get_updates_reports_api_status_on_non_401_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bot123:abc/getUpdates"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Conflict: terminated by other getUpdates request",
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let err = client.get_updates("123:abc", None, 30).await.unwrap_err();
        assert!(matches!(err, TelegramApiError::ApiStatus(409)));
    }

    #[tokio::test]
    async fn send_message_succeeds_on_ok_response() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:abc/sendMessage"))
            .and(body_json(serde_json::json!({ "chat_id": 555, "text": "hello back" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 1 }
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        client
            .send_message("123:abc", 555, "hello back", None)
            .await
            .expect("send_message should succeed");
    }

    #[tokio::test]
    async fn send_message_reports_invalid_token_on_http_401() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botbad-token/sendMessage"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Unauthorized",
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let err = client
            .send_message("bad-token", 555, "hello", None)
            .await
            .unwrap_err();
        assert!(matches!(err, TelegramApiError::InvalidToken));
    }

    #[tokio::test]
    async fn send_message_reports_api_status_on_non_401_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:abc/sendMessage"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Bad Request: chat not found",
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let err = client
            .send_message("123:abc", 555, "hello", None)
            .await
            .unwrap_err();
        assert!(matches!(err, TelegramApiError::ApiStatus(400)));
    }

    #[tokio::test]
    async fn send_message_includes_parse_mode_when_provided() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:abc/sendMessage"))
            .and(body_json(serde_json::json!({
                "chat_id": 555,
                "text": "<b>hello</b>",
                "parse_mode": "HTML"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 1 }
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        client
            .send_message("123:abc", 555, "<b>hello</b>", Some("HTML"))
            .await
            .expect("send_message should succeed");
    }

    #[tokio::test]
    async fn send_message_omits_parse_mode_when_none() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:abc/sendMessage"))
            .and(body_json(serde_json::json!({ "chat_id": 555, "text": "plain" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 1 }
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        client
            .send_message("123:abc", 555, "plain", None)
            .await
            .expect("send_message should succeed");
    }

    #[tokio::test]
    async fn send_chat_action_succeeds_on_ok_response() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:abc/sendChatAction"))
            .and(body_json(serde_json::json!({ "chat_id": 555, "action": "typing" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": true
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        client
            .send_chat_action("123:abc", 555, "typing")
            .await
            .expect("send_chat_action should succeed");
    }

    #[tokio::test]
    async fn send_chat_action_reports_invalid_token_on_http_401() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _lock = lock_env();
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botbad-token/sendChatAction"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "ok": false,
                "description": "Unauthorized",
            })))
            .mount(&mock_server)
            .await;
        let _env = EnvGuard::set(&mock_server.uri());

        let client = TelegramClient::new();
        let err = client
            .send_chat_action("bad-token", 555, "typing")
            .await
            .unwrap_err();
        assert!(matches!(err, TelegramApiError::InvalidToken));
    }

    #[test]
    fn parses_successful_get_me_body() {
        let raw = r#"{"ok":true,"result":{"id":123,"is_bot":true,"username":"axew_research_bot","first_name":"Axew Research"}}"#;
        let body: TelegramResponse<TelegramBotInfo> = serde_json::from_str(raw).unwrap();
        let info = parse_get_me_body(body).unwrap();
        assert_eq!(info.username, "axew_research_bot");
        assert_eq!(info.id, 123);
        assert!(info.is_bot);
    }

    #[test]
    fn rejects_ok_false_get_me_body() {
        let raw = r#"{"ok":false,"description":"Unauthorized"}"#;
        let body: TelegramResponse<TelegramBotInfo> = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            parse_get_me_body(body),
            Err(TelegramApiError::InvalidToken)
        ));
    }
}
