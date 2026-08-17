//! Test Connection orchestration — runs the three checks a Slack setup screen needs
//! before any bridge code exists: `auth.test` identity capture, a per-scope
//! diff against [`ao_protocol::slack_manifest::SLACK_REQUIRED_BOT_SCOPES`],
//! and an `apps.connections.open` handshake check. Pure orchestration over
//! [`SlackApiSeam`] — no I/O of its own, so [`run_test_connection`] is fully
//! exercised by this module's own tests against
//! [`super::fake_seam::FakeSlackApiSeam`], with no live call to `slack.com`.

use ao_protocol::slack_manifest::SLACK_REQUIRED_BOT_SCOPES;
use ao_protocol::slack_test_connection::{
    SlackCheckFailure, SlackCheckOutcome, SlackIdentitySummary, SlackScopeGrant, SlackTestConnectionReport,
};

use super::web_api_seam::SlackApiSeam;

/// Runs all three checks and returns the full report. Never returns an
/// error itself — every possible outcome (network failure, auth failure,
/// partial success) is represented as a field on the report, since a caller
/// (the HTTP route) needs to render "2 of 3 checks passed," not just
/// succeed or fail as a whole.
pub async fn run_test_connection(
    seam: &dyn SlackApiSeam,
    bot_token: &str,
    app_token: &str,
) -> SlackTestConnectionReport {
    let (auth_check, identity, scopes) = match seam.auth_test(bot_token).await {
        Ok(result) => {
            let identity = SlackIdentitySummary {
                team_name: result.team,
                team_id: result.team_id,
                bot_handle: result.user,
                bot_user_id: result.user_id,
            };
            (SlackCheckOutcome::passed(), Some(identity), diff_scopes(&result.granted_scopes))
        }
        Err(err) => {
            let failure: SlackCheckFailure = err.into();
            (SlackCheckOutcome::failed(failure), None, diff_scopes(&[]))
        }
    };

    let connections_open_check = match seam.connections_open(app_token).await {
        Ok(()) => SlackCheckOutcome::passed(),
        Err(err) => SlackCheckOutcome::failed(err.into()),
    };

    SlackTestConnectionReport { auth_check, identity, scopes, connections_open_check }
}

/// Grades `granted` against [`SLACK_REQUIRED_BOT_SCOPES`], preserving that
/// constant's order so the manifest and a Test Connection report always
/// list scopes the same way.
fn diff_scopes(granted: &[String]) -> Vec<SlackScopeGrant> {
    SLACK_REQUIRED_BOT_SCOPES
        .iter()
        .map(|&scope| SlackScopeGrant { scope: scope.to_string(), granted: granted.iter().any(|g| g == scope) })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use ao_protocol::slack_test_connection::SlackFailureKind;

    use super::super::fake_seam::FakeSlackApiSeam;
    use super::super::web_api_seam::{SlackApiCallError, SlackAuthTestResult};

    fn sample_identity() -> SlackAuthTestResult {
        SlackAuthTestResult {
            team: "Acme Corp".to_string(),
            team_id: "T0123ABCD".to_string(),
            user: "launchpad-bot".to_string(),
            user_id: "U0456WXYZ".to_string(),
            granted_scopes: SLACK_REQUIRED_BOT_SCOPES.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn all_checks_pass_reports_identity_and_every_scope_granted() {
        let seam = FakeSlackApiSeam::all_checks_pass(sample_identity());
        let report = run_test_connection(&seam, "xoxb-fake", "xapp-fake").await;

        assert!(report.auth_check.passed);
        assert!(report.connections_open_check.passed);
        assert!(report.scopes.iter().all(|s| s.granted), "expected every scope granted: {:?}", report.scopes);
        assert_eq!(report.scopes.len(), SLACK_REQUIRED_BOT_SCOPES.len());

        let identity = report.identity.expect("identity present on success");
        assert_eq!(identity.team_name, "Acme Corp");
        assert_eq!(identity.team_id, "T0123ABCD");
        assert_eq!(identity.bot_handle, "launchpad-bot");
        assert_eq!(identity.bot_user_id, "U0456WXYZ");
    }

    #[tokio::test]
    async fn one_missing_scope_surfaces_as_that_specific_scope_red() {
        let mut identity = sample_identity();
        identity.granted_scopes.retain(|s| s != "users:read");
        let seam = FakeSlackApiSeam::all_checks_pass(identity);

        let report = run_test_connection(&seam, "xoxb-fake", "xapp-fake").await;

        let missing = report.scopes.iter().find(|s| s.scope == "users:read").expect("users:read present in report");
        assert!(!missing.granted, "users:read must be reported as not granted");

        for scope in report.scopes.iter().filter(|s| s.scope != "users:read") {
            assert!(scope.granted, "expected {} to still be granted", scope.scope);
        }
    }

    #[tokio::test]
    async fn bad_app_token_fails_the_handshake_check_but_not_auth_test() {
        let seam =
            FakeSlackApiSeam::new(Ok(sample_identity()), Err(SlackApiCallError::Auth("invalid_auth".to_string())));

        let report = run_test_connection(&seam, "xoxb-fake", "xapp-bad").await;

        assert!(report.auth_check.passed, "a bad app token must not affect the bot-token auth check");
        assert!(report.identity.is_some());
        assert!(!report.connections_open_check.passed);
        let failure = report.connections_open_check.failure.expect("failure present");
        assert_eq!(failure.kind, SlackFailureKind::Auth);
        assert_eq!(failure.message, "invalid_auth");
    }

    #[tokio::test]
    async fn network_failure_on_auth_test_is_distinguishable_from_an_auth_failure() {
        let seam = FakeSlackApiSeam::new(Err(SlackApiCallError::Network("connection refused".to_string())), Ok(()));

        let report = run_test_connection(&seam, "xoxb-fake", "xapp-fake").await;

        assert!(!report.auth_check.passed);
        assert!(report.identity.is_none());
        let failure = report.auth_check.failure.expect("failure present");
        assert_eq!(failure.kind, SlackFailureKind::Network);
        assert_eq!(failure.message, "connection refused");
        // With no confirmed identity, every required scope reads as not
        // granted rather than the report silently omitting the checklist.
        assert!(report.scopes.iter().all(|s| !s.granted));
        assert_eq!(report.scopes.len(), SLACK_REQUIRED_BOT_SCOPES.len());
    }

    #[tokio::test]
    async fn auth_failure_on_auth_test_is_reported_as_auth_not_network() {
        let seam = FakeSlackApiSeam::new(Err(SlackApiCallError::Auth("token_revoked".to_string())), Ok(()));

        let report = run_test_connection(&seam, "xoxb-fake", "xapp-fake").await;

        assert!(!report.auth_check.passed);
        let failure = report.auth_check.failure.expect("failure present");
        assert_eq!(failure.kind, SlackFailureKind::Auth);
        assert_eq!(failure.message, "token_revoked");
    }

    #[tokio::test]
    async fn both_checks_can_fail_independently_with_distinct_causes() {
        let seam = FakeSlackApiSeam::new(
            Err(SlackApiCallError::Network("dns failure".to_string())),
            Err(SlackApiCallError::Auth("invalid_auth".to_string())),
        );

        let report = run_test_connection(&seam, "xoxb-fake", "xapp-fake").await;

        assert_eq!(report.auth_check.failure.unwrap().kind, SlackFailureKind::Network);
        assert_eq!(report.connections_open_check.failure.unwrap().kind, SlackFailureKind::Auth);
    }
}
