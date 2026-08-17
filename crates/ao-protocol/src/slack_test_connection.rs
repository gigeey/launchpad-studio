//! Slack "Test Connection" report shape — the structured result of the three
//! checks `ao_engine::channels::slack::test_connection::run_test_connection`
//! runs against a pair of stored tokens: `auth.test` identity capture, a
//! per-scope diff, and an `apps.connections.open` handshake check.
//!
//! These types are pure data so both `ao-engine` (which produces a report)
//! and `ao-server` (which serializes one straight into an HTTP response) can
//! share one shape without either depending on the other's internals.
//! [`SlackTestConnectionReport`] never carries token material — only
//! identity that was already safe to display (team/bot names and ids) and
//! per-check pass/fail outcomes.

use serde::{Deserialize, Serialize};

/// The full per-check result of one Test Connection run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlackTestConnectionReport {
    /// Result of the `auth.test` call against the bot token.
    pub auth_check: SlackCheckOutcome,
    /// Identity captured from a successful `auth.test` call. `None` when
    /// `auth_check.passed` is `false` — there is nothing to report.
    pub identity: Option<SlackIdentitySummary>,
    /// Every required bot scope, each marked granted or not. Populated even
    /// when `auth_check` failed (all scopes read as not-granted in that
    /// case) so the UI always has the full checklist to render.
    pub scopes: Vec<SlackScopeGrant>,
    /// Result of the `apps.connections.open` handshake call against the
    /// app-level token.
    pub connections_open_check: SlackCheckOutcome,
}

/// Non-secret identity captured from a successful `auth.test` call. Mirrors
/// [`crate::slack_connection::SlackConnection`]'s fields (minus `team_name`
/// vs. `team` naming) — a caller that gets a report back persists this
/// straight into that record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlackIdentitySummary {
    pub team_name: String,
    pub team_id: String,
    /// The bot's own display name, as `auth.test` reports it for the
    /// identity behind the bot token.
    pub bot_handle: String,
    pub bot_user_id: String,
}

/// One required scope's grant status, e.g. `{"scope": "chat:write",
/// "granted": true}`. A flat list rather than a single pass/fail so a user
/// missing one of eight scopes can see exactly which one to add.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlackScopeGrant {
    pub scope: String,
    pub granted: bool,
}

/// Pass/fail outcome of a single check, with an optional cause. `failure`
/// is `None` exactly when `passed` is `true`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlackCheckOutcome {
    pub passed: bool,
    pub failure: Option<SlackCheckFailure>,
}

impl SlackCheckOutcome {
    pub fn passed() -> Self {
        Self { passed: true, failure: None }
    }

    pub fn failed(failure: SlackCheckFailure) -> Self {
        Self { passed: false, failure: Some(failure) }
    }
}

/// A failed check's cause, classified so a caller (and the UI) can tell "we
/// couldn't reach Slack" apart from "Slack reached, and rejected us" without
/// parsing `message` — a bare "Test failed" with no cause is the support-load
/// generator this type exists to avoid.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlackCheckFailure {
    pub kind: SlackFailureKind,
    /// Human-readable detail — Slack's own `error` code for an
    /// [`SlackFailureKind::Auth`] failure, or the transport error's message
    /// for a [`SlackFailureKind::Network`] one. Never contains a token.
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SlackFailureKind {
    /// The request never completed a round trip to Slack — DNS, connection,
    /// TLS, or timeout failure. Retrying may help; the token is unproven
    /// either way.
    Network,
    /// Slack was reached and responded, but rejected the request — a bad or
    /// revoked token, a missing scope, a deactivated account, and so on.
    Auth,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_json_round_trips_on_full_success() {
        let report = SlackTestConnectionReport {
            auth_check: SlackCheckOutcome::passed(),
            identity: Some(SlackIdentitySummary {
                team_name: "Acme Corp".to_string(),
                team_id: "T0123ABCD".to_string(),
                bot_handle: "launchpad-bot".to_string(),
                bot_user_id: "U0456WXYZ".to_string(),
            }),
            scopes: vec![SlackScopeGrant { scope: "chat:write".to_string(), granted: true }],
            connections_open_check: SlackCheckOutcome::passed(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: SlackTestConnectionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }

    #[test]
    fn failure_kind_serializes_as_snake_case() {
        let failure = SlackCheckFailure { kind: SlackFailureKind::Network, message: "connection refused".to_string() };
        let json = serde_json::to_string(&failure).unwrap();
        assert!(json.contains("\"kind\":\"network\""), "got: {json}");
    }

    #[test]
    fn failed_outcome_never_reports_passed() {
        let outcome = SlackCheckOutcome::failed(SlackCheckFailure {
            kind: SlackFailureKind::Auth,
            message: "invalid_auth".to_string(),
        });
        assert!(!outcome.passed);
        assert!(outcome.failure.is_some());
    }
}
