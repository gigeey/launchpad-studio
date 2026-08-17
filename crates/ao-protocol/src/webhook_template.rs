//! Renders a `Webhook` route's `prompt_template` against the inbound event
//! payload: dot-path field refs like `{pull_request.title}`, plus a
//! `{__raw__}` escape hatch that expands to the full payload as truncated
//! JSON for anything the named placeholders don't cover. The
//! rendered string becomes the instruction handed to the fired agent,
//! replacing the assignment's static `instruction` for that one run.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::webhook_filter::get_dot_path;

/// Placeholder token that expands to the full payload as truncated JSON.
const RAW_PLACEHOLDER: &str = "__raw__";

/// Cap on the `{__raw__}` JSON dump, in characters, so an oversized payload
/// (e.g. a GitHub PR event embedding a large diff) can't blow out the
/// rendered instruction.
const RAW_PAYLOAD_TRUNCATE_CHARS: usize = 4000;

/// Default `prompt_template` for the GitHub "review this PR" day-one flow: a
/// `deliver: Agent` webhook route with `events: ["pull_request"]` renders
/// this against the inbound payload and hands the result to the assignment's
/// agent as its instruction. The instruction only tells the agent *what* to
/// do — run `gh pr diff` then `gh pr comment` — it never invokes `gh`
/// itself; the agent carries that out with its own shell tool access. This
/// is distinct from the `github_comment` deliver target, which posts a
/// rendered template directly without starting an agent at all.
pub const DEFAULT_GITHUB_PR_REVIEW_TEMPLATE: &str = "A pull request was {action} on {repository.full_name}: \"{pull_request.title}\" (#{pull_request.number}) by {sender.login}.\n\nReview this pull request:\n1. Run `gh pr diff {pull_request.number} --repo {repository.full_name}` to see the changes.\n2. Review the diff for correctness, clarity, and potential bugs.\n3. Run `gh pr comment {pull_request.number} --repo {repository.full_name} --body \"<your review>\"` to post your review as a comment.\n\nKeep the comment concise and specific to what you actually found in the diff.";

fn placeholder_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"\{([A-Za-z0-9_.]+)\}").expect("static placeholder pattern is valid"))
}

/// Render `template`, replacing every `{dot.path}` placeholder with the
/// stringified value found at that path in `payload`, and `{__raw__}` with
/// the full payload as truncated pretty JSON. A named path that resolves to
/// nothing (missing key, out-of-range index, or JSON `null`) renders as an
/// empty string — a route author who typos a field sees a gap in the
/// rendered instruction, not a render failure that would block delivery
/// entirely.
pub fn render_prompt_template(template: &str, payload: &Value) -> String {
    placeholder_pattern()
        .replace_all(template, |caps: &regex::Captures| render_token(&caps[1], payload))
        .into_owned()
}

fn render_token(token: &str, payload: &Value) -> String {
    if token == RAW_PLACEHOLDER {
        return truncated_raw_payload(payload);
    }
    match get_dot_path(payload, token) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn truncated_raw_payload(payload: &Value) -> String {
    let json = serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string());
    if json.chars().count() <= RAW_PAYLOAD_TRUNCATE_CHARS {
        json
    } else {
        let truncated: String = json.chars().take(RAW_PAYLOAD_TRUNCATE_CHARS).collect();
        format!("{truncated}\n… (truncated)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_payload() -> Value {
        json!({
            "action": "opened",
            "pull_request": { "title": "Fix the flaky retry loop", "number": 42 },
            "sender": { "login": "octocat" },
        })
    }

    #[test]
    fn renders_named_dot_path_fields() {
        let payload = sample_payload();
        let out = render_prompt_template("Review PR #{pull_request.number}: {pull_request.title}", &payload);
        assert_eq!(out, "Review PR #42: Fix the flaky retry loop");
    }

    #[test]
    fn renders_multiple_distinct_fields_and_literal_text() {
        let payload = sample_payload();
        let out = render_prompt_template(
            "{sender.login} {action} PR #{pull_request.number}.",
            &payload,
        );
        assert_eq!(out, "octocat opened PR #42.");
    }

    #[test]
    fn missing_named_field_renders_as_empty_string() {
        let payload = sample_payload();
        let out = render_prompt_template("Assignee: [{pull_request.assignee}]", &payload);
        assert_eq!(out, "Assignee: []");
    }

    #[test]
    fn missing_top_level_field_renders_as_empty_string() {
        let payload = sample_payload();
        let out = render_prompt_template("Repo: [{repository.name}]", &payload);
        assert_eq!(out, "Repo: []");
    }

    #[test]
    fn null_field_renders_as_empty_string() {
        let payload = json!({ "assignee": null });
        let out = render_prompt_template("[{assignee}]", &payload);
        assert_eq!(out, "[]");
    }

    #[test]
    fn raw_placeholder_dumps_full_payload_as_json() {
        let payload = sample_payload();
        let out = render_prompt_template("Context:\n{__raw__}", &payload);
        assert!(out.starts_with("Context:\n"));
        assert!(out.contains("\"action\": \"opened\""));
        assert!(out.contains("\"title\": \"Fix the flaky retry loop\""));
    }

    #[test]
    fn raw_placeholder_combines_with_named_fields() {
        let payload = sample_payload();
        let out = render_prompt_template("PR #{pull_request.number} — full event:\n{__raw__}", &payload);
        assert!(out.starts_with("PR #42 — full event:\n"));
        assert!(out.contains("\"number\": 42"));
    }

    #[test]
    fn raw_placeholder_truncates_oversized_payload() {
        let big = json!({ "text": "x".repeat(RAW_PAYLOAD_TRUNCATE_CHARS * 2) });
        let out = render_prompt_template("{__raw__}", &big);
        assert!(out.ends_with("… (truncated)"));
        assert!(out.len() < big.to_string().len());
    }

    #[test]
    fn template_with_no_placeholders_passes_through_unchanged() {
        let payload = sample_payload();
        assert_eq!(
            render_prompt_template("Static instruction, no tokens.", &payload),
            "Static instruction, no tokens."
        );
    }

    #[test]
    fn numeric_and_boolean_fields_stringify_without_quotes() {
        let payload = json!({ "count": 7, "merged": true });
        assert_eq!(render_prompt_template("{count} items, merged={merged}", &payload), "7 items, merged=true");
    }

    #[test]
    fn default_github_pr_review_template_renders_against_realistic_payload() {
        let payload = json!({
            "action": "opened",
            "number": 42,
            "pull_request": { "number": 42, "title": "Fix the flaky retry loop" },
            "repository": { "full_name": "acme/widgets" },
            "sender": { "login": "octocat" },
        });
        let out = render_prompt_template(DEFAULT_GITHUB_PR_REVIEW_TEMPLATE, &payload);
        assert!(out.contains("acme/widgets"));
        assert!(out.contains("Fix the flaky retry loop"));
        assert!(out.contains("(#42)"));
        assert!(out.contains("gh pr diff 42 --repo acme/widgets"));
        assert!(out.contains("gh pr comment 42 --repo acme/widgets"));
        assert!(!out.contains('{'), "no placeholder should survive rendering, got: {out}");
    }
}
