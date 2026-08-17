//! Posts a rendered webhook delivery as a GitHub pull-request comment via the
//! `gh` CLI — the [`ao_protocol::assignment::WebhookDeliverTarget::GithubComment`]
//! path, called from [`crate::webhook_dispatch::dispatch_webhook_route`].
//!
//! `repo` and `pr_number` are resolved straight from the inbound GitHub
//! `pull_request` payload (`repository.full_name`, `pull_request.number`),
//! never from operator-supplied config, and are validated against a strict
//! charset/numeric shape *before* either value reaches [`tokio::process::Command`].
//! `Command` execs `gh` directly with an argv array — no shell is ever
//! involved, so there is no shell-metacharacter interpretation to exploit —
//! but the validation below is kept anyway as a hard, defense-in-depth gate:
//! a malformed `repo` or `pr_number` is rejected outright rather than trusted
//! to land safely in argv.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use tokio::process::Command;

/// Everything that can go wrong resolving, validating, or executing a
/// `github_comment` delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubCommentError {
    /// The payload has no `repository.full_name` field.
    MissingRepo,
    /// The payload has neither `pull_request.number` nor a top-level
    /// `number` field.
    MissingPrNumber,
    /// `repo` doesn't match the required `owner/name` charset.
    InvalidRepo(String),
    /// `pr_number` isn't a positive integer.
    InvalidPrNumber(String),
    /// The `gh` binary could not be spawned (e.g. not installed).
    SpawnFailed(String),
    /// `gh` ran but exited non-zero.
    CommandFailed(String),
}

impl std::fmt::Display for GithubCommentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GithubCommentError::MissingRepo => write!(f, "payload has no repository.full_name field"),
            GithubCommentError::MissingPrNumber => write!(f, "payload has no pull_request.number field"),
            GithubCommentError::InvalidRepo(r) => {
                write!(f, "repo {r:?} does not match the required owner/name charset")
            }
            GithubCommentError::InvalidPrNumber(p) => write!(f, "PR number {p:?} is not a positive integer"),
            GithubCommentError::SpawnFailed(e) => write!(f, "failed to spawn `gh`: {e}"),
            GithubCommentError::CommandFailed(e) => write!(f, "`gh pr comment` failed: {e}"),
        }
    }
}

impl std::error::Error for GithubCommentError {}

/// `owner/name`: alphanumeric, dot, hyphen, underscore on each side of
/// exactly one slash — GitHub's own repo-naming charset, and narrow enough
/// that no shell metacharacter (`;`, `|`, `&`, backtick, `$`, space, …) can
/// ever match.
fn repo_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$").expect("static repo pattern is valid"))
}

/// Rejects anything but a `owner/name` repo slug.
pub fn validate_repo(repo: &str) -> Result<(), GithubCommentError> {
    if repo_pattern().is_match(repo) {
        Ok(())
    } else {
        Err(GithubCommentError::InvalidRepo(repo.to_string()))
    }
}

/// Rejects anything but a positive base-10 integer — in particular anything
/// carrying a shell metacharacter, since only ASCII digits parse.
pub fn validate_pr_number(pr_number: &str) -> Result<u64, GithubCommentError> {
    match pr_number.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(GithubCommentError::InvalidPrNumber(pr_number.to_string())),
    }
}

/// Pulls the raw (unvalidated) repo slug and PR number out of a GitHub
/// `pull_request` webhook payload: `repository.full_name` and
/// `pull_request.number`, falling back to a top-level `number` field for
/// payload shapes that carry it there instead.
pub fn resolve_github_pr_target(payload: &Value) -> Result<(String, String), GithubCommentError> {
    let repo = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(|v| v.as_str())
        .ok_or(GithubCommentError::MissingRepo)?
        .to_string();

    let pr_field = payload.get("pull_request").and_then(|pr| pr.get("number")).or_else(|| payload.get("number"));
    let pr_number = match pr_field {
        Some(v) if v.is_number() => v.to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => return Err(GithubCommentError::MissingPrNumber),
    };

    Ok((repo, pr_number))
}

/// The exact argv `gh` is invoked with — split out from [`post_github_pr_comment`]
/// so the shape of the call is unit-testable without spawning a process.
pub fn build_gh_comment_args(pr_number: u64, repo: &str, body: &str) -> Vec<String> {
    vec![
        "pr".to_string(),
        "comment".to_string(),
        pr_number.to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--body".to_string(),
        body.to_string(),
    ]
}

/// Runs `gh pr comment <pr_number> --repo <repo> --body <body>`. Callers must
/// validate `repo`/`pr_number` first — this does not re-validate.
pub async fn post_github_pr_comment(repo: &str, pr_number: u64, body: &str) -> Result<(), GithubCommentError> {
    let args = build_gh_comment_args(pr_number, repo, body);
    let output = Command::new("gh").args(&args).output().await.map_err(|e| GithubCommentError::SpawnFailed(e.to_string()))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(GithubCommentError::CommandFailed(String::from_utf8_lossy(&output.stderr).trim().to_string()))
    }
}

/// Resolves the target PR from `payload`, validates it, and posts `body` as
/// a comment — the full `github_comment` deliver path in one call.
pub async fn deliver_github_comment(payload: &Value, body: &str) -> Result<(), GithubCommentError> {
    let (repo, pr_number) = resolve_github_pr_target(payload)?;
    validate_repo(&repo)?;
    let pr_number = validate_pr_number(&pr_number)?;
    post_github_pr_comment(&repo, pr_number, body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn realistic_pr_payload() -> Value {
        json!({
            "action": "opened",
            "number": 42,
            "pull_request": {
                "number": 42,
                "title": "Fix the flaky retry loop",
                "html_url": "https://github.com/acme/widgets/pull/42",
            },
            "repository": {
                "full_name": "acme/widgets",
                "name": "widgets",
                "owner": { "login": "acme" },
            },
            "sender": { "login": "octocat" },
        })
    }

    #[test]
    fn builds_exact_gh_pr_comment_args() {
        let args = build_gh_comment_args(42, "acme/widgets", "Looks good!");
        assert_eq!(args, vec!["pr", "comment", "42", "--repo", "acme/widgets", "--body", "Looks good!"]);
    }

    #[test]
    fn valid_repo_and_pr_number_pass_validation() {
        assert_eq!(validate_repo("acme/widgets"), Ok(()));
        assert_eq!(validate_pr_number("42"), Ok(42));
    }

    #[test]
    fn repo_with_shell_injection_attempt_is_rejected() {
        assert_eq!(
            validate_repo("a/b; rm -rf /"),
            Err(GithubCommentError::InvalidRepo("a/b; rm -rf /".to_string()))
        );
    }

    #[test]
    fn repo_with_backtick_or_ampersand_is_rejected() {
        assert!(validate_repo("acme/widgets`whoami`").is_err());
        assert!(validate_repo("acme/widgets && echo pwned").is_err());
        assert!(validate_repo("acme/widgets|cat /etc/passwd").is_err());
    }

    #[test]
    fn repo_missing_owner_or_name_is_rejected() {
        assert!(validate_repo("acme").is_err());
        assert!(validate_repo("acme/wid/gets").is_err());
        assert!(validate_repo("").is_err());
    }

    #[test]
    fn non_numeric_pr_number_is_rejected() {
        assert!(validate_pr_number("42; rm -rf /").is_err());
        assert!(validate_pr_number("abc").is_err());
        assert!(validate_pr_number("-1").is_err());
        assert!(validate_pr_number("").is_err());
        assert!(validate_pr_number("4.2").is_err());
    }

    #[test]
    fn zero_pr_number_is_rejected() {
        assert!(validate_pr_number("0").is_err());
    }

    #[test]
    fn resolves_repo_and_pr_from_realistic_pull_request_payload() {
        let (repo, pr) = resolve_github_pr_target(&realistic_pr_payload()).unwrap();
        assert_eq!(repo, "acme/widgets");
        assert_eq!(pr, "42");
    }

    #[test]
    fn resolves_pr_number_from_top_level_field_when_pull_request_lacks_it() {
        let payload = json!({
            "number": 7,
            "pull_request": {},
            "repository": { "full_name": "acme/widgets" },
        });
        let (repo, pr) = resolve_github_pr_target(&payload).unwrap();
        assert_eq!(repo, "acme/widgets");
        assert_eq!(pr, "7");
    }

    #[test]
    fn missing_repository_field_fails_resolution() {
        let payload = json!({ "pull_request": { "number": 1 } });
        assert_eq!(resolve_github_pr_target(&payload), Err(GithubCommentError::MissingRepo));
    }

    #[test]
    fn missing_pr_number_fails_resolution() {
        let payload = json!({ "repository": { "full_name": "acme/widgets" }, "pull_request": {} });
        assert_eq!(resolve_github_pr_target(&payload), Err(GithubCommentError::MissingPrNumber));
    }

    #[tokio::test]
    async fn deliver_rejects_malicious_repo_before_spawning_subprocess() {
        let payload = json!({
            "repository": { "full_name": "a/b; rm -rf /" },
            "pull_request": { "number": 1 },
        });
        let result = deliver_github_comment(&payload, "hi").await;
        assert_eq!(result, Err(GithubCommentError::InvalidRepo("a/b; rm -rf /".to_string())));
    }

    #[tokio::test]
    async fn deliver_rejects_non_numeric_pr_before_spawning_subprocess() {
        let payload = json!({
            "repository": { "full_name": "acme/widgets" },
            "pull_request": { "number": "42; rm -rf /" },
        });
        let result = deliver_github_comment(&payload, "hi").await;
        assert!(matches!(result, Err(GithubCommentError::InvalidPrNumber(_))));
    }

    #[tokio::test]
    async fn deliver_fails_closed_when_payload_has_no_repository() {
        let payload = json!({ "action": "opened" });
        let result = deliver_github_comment(&payload, "hi").await;
        assert_eq!(result, Err(GithubCommentError::MissingRepo));
    }
}
