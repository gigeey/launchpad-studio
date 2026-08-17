//! The REST send boundary [`super::outbound`]'s chunking and
//! `allowed_mentions` construction are unit-tested against — the outbound
//! analogue of [`super::gateway_seam::GatewaySeam`] for the inbound
//! connection. [`ReqwestSendSeam`] is the only implementation that actually
//! calls the Discord REST API; `outbound`'s tests drive [`DiscordSendSeam`]
//! against an in-memory fake instead, so chunk boundaries and the
//! `allowed_mentions` payload are provable without a live network call.

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SendSeamError {
    #[error("discord REST send failed: {0}")]
    Request(String),
    #[error("discord REST send returned a non-success status: {status}")]
    Status { status: u16 },
}

/// One outbound REST call: `POST /channels/{channel_id}/messages`. Takes the
/// already-built JSON body — [`super::outbound`] owns chunking and
/// `allowed_mentions` construction, kept pure and unit-testable there — so
/// this seam is only ever responsible for the HTTP transport itself, never
/// message shaping.
#[async_trait]
pub trait DiscordSendSeam: Send + Sync {
    async fn send(&self, token: &str, channel_id: &str, body: &serde_json::Value) -> Result<(), SendSeamError>;
}

/// Real [`DiscordSendSeam`]: posts straight to the Discord REST API,
/// authenticated the same way the inbound side's guild-member-roles lookup
/// is (see `runner::resolve_dm_member_roles`) — an `Authorization: Bot
/// <token>` header, never the token in a log line.
pub struct ReqwestSendSeam {
    http: reqwest::Client,
}

impl ReqwestSendSeam {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl DiscordSendSeam for ReqwestSendSeam {
    async fn send(&self, token: &str, channel_id: &str, body: &serde_json::Value) -> Result<(), SendSeamError> {
        let url = format!("https://discord.com/api/v10/channels/{channel_id}/messages");
        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bot {token}"))
            .json(body)
            .send()
            .await
            .map_err(|e| SendSeamError::Request(e.to_string()))?;
        if !response.status().is_success() {
            return Err(SendSeamError::Status { status: response.status().as_u16() });
        }
        Ok(())
    }
}
