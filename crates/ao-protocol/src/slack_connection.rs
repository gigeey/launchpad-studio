//! Workspace-level Slack connection record.
//!
//! A Slack binding does not carry its own credentials. It carries a
//! `connection_id` that points at one of these records, and the record in
//! turn holds nothing but identity — `team_id`, `team_name`, and
//! `bot_user_id`. The two actual secrets (the bot token and the app-level
//! token) never live here; they stay in
//! `ao_engine_tools_provider_config::channel_secret_store::ChannelSecretStore`
//! under the `SLACK_BOT_TOKEN_SECRET_ROLE` / `SLACK_APP_TOKEN_SECRET_ROLE`
//! roles, keyed the same way every other channel secret is.
//!
//! Today exactly one binding ever points at a given connection (Slack ships
//! one app per agent, so one binding is one workspace install). The record
//! is still split out as its own store rather than folded onto the binding,
//! because that split is what keeps a future "N bindings share one
//! workspace app" migration a data change — point a second binding's
//! `connection_id` at the same record — instead of a credential-plumbing
//! rewrite. Deleting a binding therefore must not be the only way a
//! workspace credential goes away: the connection record (and the secrets
//! it implies) outlives any single binding that references it, and is
//! cleaned up independently by whatever owns connection lifecycle.
use serde::{Deserialize, Serialize};

/// Identity of one Slack app installed into one workspace. Persisted by
/// `ao_persistence::slack_connection_store::SlackConnectionStore`, keyed
/// externally by an opaque `connection_id` a `ChannelKindConfig::Slack`
/// binding stores a reference to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlackConnection {
    /// The Slack workspace this app is installed into (`T…`).
    pub team_id: String,
    /// Human-readable workspace name, captured from `auth.test` at Test
    /// Connection time purely for display — never used as a lookup key.
    pub team_name: String,
    /// This app's bot user id (`U…`), also captured from `auth.test`. Needed
    /// downstream to drop inbound events the bot sent itself.
    pub bot_user_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_connection_round_trips_through_json() {
        let connection = SlackConnection {
            team_id: "T0123ABCD".to_string(),
            team_name: "Acme Corp".to_string(),
            bot_user_id: "U0456WXYZ".to_string(),
        };
        let json = serde_json::to_string(&connection).unwrap();
        let back: SlackConnection = serde_json::from_str(&json).unwrap();
        assert_eq!(connection, back);
    }
}
