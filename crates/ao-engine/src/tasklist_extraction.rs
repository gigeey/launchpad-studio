//! Extracts `<task>` and `<tasklist>` action tags from agent output.
//!
//! Mirrors the extractor pattern used by `workflow_extraction` and
//! `schedule_extraction`: regex-driven pass over the full turn buffer at
//! `TextComplete` time, returning the cleaned text plus a list of actions
//! and a list of structured parse errors. Unrecognized actions and tags with
//! missing required attributes surface as `TagParseError`s rather than
//! crashing the run.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use ao_protocol::tasklist::TaskGroupMode;

/// A `<task action="...">` tag emitted by an agent that owns the assigned
/// task in a tasklist run. Drives state-machine transitions (complete / fail)
/// or — for `request_clarification` — is recorded to the run log only
/// (mailbox wiring deferred to a later story).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskTagAction {
    Complete {
        task_id: String,
    },
    Fail {
        task_id: String,
        reason: String,
    },
    RequestClarification {
        task_id: String,
        question: String,
    },
}

/// A `<tasklist action="...">` tag. Authorization is enforced at dispatch
/// time, not at parse time:
///   - `Create` is coordinator-only (per team).
///   - `Append` is co-pilot-only (the bound tasklist is inferred from the
///     emitting agent's binding; no `team`/`title`/`tasklist_id` attrs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TasklistTagAction {
    Create {
        team: String,
        title: String,
        description: Option<String>,
        groups: Vec<TasklistGroupSpec>,
    },
    Append {
        groups: Vec<TasklistGroupSpec>,
    },
}

/// Group description carried in the body of `<tasklist action="create">`. The
/// dispatch handler promotes each spec to a real `TaskGroup` with a
/// generated `id` and a `Pending` task per `TasklistTaskSpec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasklistGroupSpec {
    pub mode: TaskGroupMode,
    #[serde(default)]
    pub tasks: Vec<TasklistTaskSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasklistTaskSpec {
    pub owner_agent_id: String,
    pub prompt: String,
    #[serde(default)]
    pub expected_outputs: Vec<String>,
}

/// A structured failure from tag parsing. Surfaced to the agent as a
/// follow-up message and to logs; never panics or aborts the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagParseError {
    pub tag: &'static str,
    pub message: String,
}

/// A parsed `<task-item-notification>` block emitted by a CLI agent in the
/// same final assistant message as its `<task action="…">` completion tag.
/// The block is the contract documented in
/// `prompts/sections/task_notification_format.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskItemNotification {
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Outcome of looking for a `<task-item-notification>` block in an agent
/// message. The three-way split lets callers route accordingly: `Missing`
/// is an empty signal (no notification was emitted), while `Malformed`
/// carries a structured reason so an auto-reprompt can quote it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationParseResult {
    /// No `<task-item-notification>` block was found in the text.
    Missing,
    /// Block was found and parsed successfully.
    Parsed(TaskItemNotification),
    /// Block was found but failed to parse (unclosed tag, missing required
    /// field, empty required field, etc.). The string explains why.
    Malformed(String),
}

/// Combined parse of a final assistant message: the terminal `<task action="…">`
/// tag (if any) and the `<task-item-notification>` block. Both are expected to
/// appear in the same message per the documented contract; this struct
/// surfaces them together so downstream handlers can decide what to do when
/// one is missing or malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAgentMessage {
    /// The first terminal task action found in the message (Complete / Fail /
    /// RequestClarification). `None` means no terminal tag was present.
    pub completion: Option<TaskTagAction>,
    /// Outcome of parsing the notification block.
    pub notification: NotificationParseResult,
}

// `<task ...>...</task>` with body. The leading `<task\s+` pattern requires
// whitespace right after `task`, which keeps `<tasklist ...>` from matching.
// The closing `</task>` literal also doesn't accidentally match `</tasklist>`
// because the regex requires `>` immediately after `task`.
static TASK_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<task\s+([^>]*)>(.*?)</task>"#)
        .expect("task body regex should compile")
});

// Attribute zone uses `[^>]` (not `[^/]`) so values containing `/` — paths,
// URLs, glob patterns — don't break the match. Trailing `\s*/>` still pins the
// self-close.
static TASK_SELF_CLOSING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<task\s+([^>]*?)\s*/>"#)
        .expect("task self-closing regex should compile")
});

static TASKLIST_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<tasklist\s+([^>]*)>(.*?)</tasklist>"#)
        .expect("tasklist body regex should compile")
});

static TASKLIST_SELF_CLOSING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<tasklist\s+([^>]*?)\s*/>"#)
        .expect("tasklist self-closing regex should compile")
});

static ATTR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(\w+)="([^"]*)""#).expect("attr regex should compile"));

// Whole `<task-item-notification>...</task-item-notification>` block. No
// attributes are documented on the opening tag, so the regex pins them out.
static TASK_ITEM_NOTIFICATION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<task-item-notification>(.*?)</task-item-notification>"#)
        .expect("task-item-notification regex should compile")
});

// Used to detect a malformed/unclosed block: if the opening tag substring
// appears in the text but the full-block regex above didn't match, we treat
// it as malformed rather than missing.
static TASK_ITEM_NOTIFICATION_OPEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<task-item-notification[\s>]"#)
        .expect("task-item-notification open regex should compile")
});

static NOTIFICATION_STATUS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<status>(.*?)</status>"#).expect("notification status regex should compile")
});
static NOTIFICATION_SUMMARY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<summary>(.*?)</summary>"#)
        .expect("notification summary regex should compile")
});
static NOTIFICATION_DETAILS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<details>(.*?)</details>"#)
        .expect("notification details regex should compile")
});

fn parse_attrs(s: &str) -> HashMap<String, String> {
    ATTR_RE
        .captures_iter(s)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

fn build_task_action(
    attrs: &HashMap<String, String>,
    body: Option<&str>,
) -> Result<TaskTagAction, TagParseError> {
    let action = attrs.get("action").ok_or_else(|| TagParseError {
        tag: "task",
        message: "missing required attribute 'action'".to_string(),
    })?;
    let task_id = attrs.get("task_id").ok_or_else(|| TagParseError {
        tag: "task",
        message: "missing required attribute 'task_id'".to_string(),
    })?;

    match action.as_str() {
        "complete" => Ok(TaskTagAction::Complete {
            task_id: task_id.clone(),
        }),
        "fail" => {
            let reason = attrs
                .get("reason")
                .cloned()
                .or_else(|| body.map(|b| b.trim().to_string()).filter(|s| !s.is_empty()))
                .unwrap_or_default();
            Ok(TaskTagAction::Fail {
                task_id: task_id.clone(),
                reason,
            })
        }
        "request_clarification" => {
            let question = attrs
                .get("question")
                .cloned()
                .or_else(|| body.map(|b| b.trim().to_string()).filter(|s| !s.is_empty()))
                .unwrap_or_default();
            Ok(TaskTagAction::RequestClarification {
                task_id: task_id.clone(),
                question,
            })
        }
        other => Err(TagParseError {
            tag: "task",
            message: format!(
                "unrecognized action '{other}' for <task>; expected one of complete, fail, request_clarification"
            ),
        }),
    }
}

fn parse_groups_body(body: &str) -> Result<Vec<TasklistGroupSpec>, TagParseError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(TagParseError {
            tag: "tasklist",
            message: "create body must contain a YAML or JSON groups list".to_string(),
        });
    }
    serde_yaml::from_str::<Vec<TasklistGroupSpec>>(trimmed).map_err(|e| TagParseError {
        tag: "tasklist",
        message: format!("invalid groups body: {e}"),
    })
}

fn build_tasklist_action(
    attrs: &HashMap<String, String>,
    body: Option<&str>,
) -> Result<TasklistTagAction, TagParseError> {
    let action = attrs.get("action").ok_or_else(|| TagParseError {
        tag: "tasklist",
        message: "missing required attribute 'action'".to_string(),
    })?;

    match action.as_str() {
        "create" => {
            let team = attrs.get("team").cloned().ok_or_else(|| TagParseError {
                tag: "tasklist",
                message: "missing required attribute 'team'".to_string(),
            })?;
            let title = attrs.get("title").cloned().ok_or_else(|| TagParseError {
                tag: "tasklist",
                message: "missing required attribute 'title'".to_string(),
            })?;
            let description = attrs.get("description").cloned();
            let body = body.ok_or_else(|| TagParseError {
                tag: "tasklist",
                message: "create requires a body with the groups list".to_string(),
            })?;
            let groups = parse_groups_body(body)?;
            Ok(TasklistTagAction::Create {
                team,
                title,
                description,
                groups,
            })
        }
        "append" => {
            let body = body.ok_or_else(|| TagParseError {
                tag: "tasklist",
                message: "append requires a body with the groups list".to_string(),
            })?;
            let groups = parse_groups_body(body)?;
            Ok(TasklistTagAction::Append { groups })
        }
        other => Err(TagParseError {
            tag: "tasklist",
            message: format!(
                "unrecognized action '{other}' for <tasklist>; expected one of create, append"
            ),
        }),
    }
}

/// Extract every `<task ...>` tag (body and self-closing) from `text`.
/// Returns the cleaned text (with all matched tags stripped), the list of
/// successfully parsed actions, and a list of parse errors for malformed or
/// unrecognized tags. Errors do NOT abort extraction — surrounding tags are
/// still parsed.
pub fn extract_task_actions(text: &str) -> (String, Vec<TaskTagAction>, Vec<TagParseError>) {
    let mut actions = Vec::new();
    let mut errors = Vec::new();

    for cap in TASK_TAG_RE.captures_iter(text) {
        let attrs = parse_attrs(&cap[1]);
        // The body of `<task action="…">…</task>` is now expected to contain
        // the nested `<task-item-notification>` block. Strip it before
        // treating the remaining text as a free-form reason/question — the
        // notification itself is parsed separately by
        // `extract_task_item_notification` over the original text.
        let body_stripped = TASK_ITEM_NOTIFICATION_RE.replace_all(&cap[2], "");
        let body = body_stripped.trim();
        match build_task_action(&attrs, Some(body)) {
            Ok(a) => actions.push(a),
            Err(e) => errors.push(e),
        }
    }
    for cap in TASK_SELF_CLOSING_RE.captures_iter(text) {
        let attrs = parse_attrs(&cap[1]);
        match build_task_action(&attrs, None) {
            Ok(a) => actions.push(a),
            Err(e) => errors.push(e),
        }
    }

    let cleaned = strip_task_tags(text);
    (cleaned, actions, errors)
}

/// Extract every `<tasklist ...>` tag from `text`. Same contract as
/// [`extract_task_actions`] — actions and errors returned independently.
pub fn extract_tasklist_actions(
    text: &str,
) -> (String, Vec<TasklistTagAction>, Vec<TagParseError>) {
    let mut actions = Vec::new();
    let mut errors = Vec::new();

    for cap in TASKLIST_TAG_RE.captures_iter(text) {
        let attrs = parse_attrs(&cap[1]);
        let body = &cap[2];
        match build_tasklist_action(&attrs, Some(body)) {
            Ok(a) => actions.push(a),
            Err(e) => errors.push(e),
        }
    }
    for cap in TASKLIST_SELF_CLOSING_RE.captures_iter(text) {
        let attrs = parse_attrs(&cap[1]);
        match build_tasklist_action(&attrs, None) {
            Ok(a) => actions.push(a),
            Err(e) => errors.push(e),
        }
    }

    let cleaned = strip_tasklist_tags(text);
    (cleaned, actions, errors)
}

/// Look for a single `<task-item-notification>` block in `text` and return
/// what we found. Distinguishes missing-vs-malformed so callers can route
/// silent absences differently from broken blocks (the auto-reprompt path
/// only fires on malformed; missing is treated as "no notification was
/// requested for this turn").
pub fn extract_task_item_notification(text: &str) -> NotificationParseResult {
    let body = match TASK_ITEM_NOTIFICATION_RE.captures(text) {
        Some(cap) => cap.get(1).map(|m| m.as_str()).unwrap_or(""),
        None => {
            if TASK_ITEM_NOTIFICATION_OPEN_RE.is_match(text) {
                return NotificationParseResult::Malformed(
                    "<task-item-notification> opening tag found but no matching closing tag"
                        .to_string(),
                );
            }
            return NotificationParseResult::Missing;
        }
    };

    let status = match NOTIFICATION_STATUS_RE.captures(body) {
        Some(cap) => cap[1].trim().to_string(),
        None => {
            return NotificationParseResult::Malformed(
                "<task-item-notification> is missing the required <status> field".to_string(),
            );
        }
    };
    if status.is_empty() {
        return NotificationParseResult::Malformed(
            "<task-item-notification> has an empty <status> field".to_string(),
        );
    }

    let summary = match NOTIFICATION_SUMMARY_RE.captures(body) {
        Some(cap) => cap[1].trim().to_string(),
        None => {
            return NotificationParseResult::Malformed(
                "<task-item-notification> is missing the required <summary> field".to_string(),
            );
        }
    };
    if summary.is_empty() {
        return NotificationParseResult::Malformed(
            "<task-item-notification> has an empty <summary> field".to_string(),
        );
    }

    let details = NOTIFICATION_DETAILS_RE
        .captures(body)
        .map(|cap| cap[1].trim().to_string())
        .filter(|s| !s.is_empty());

    NotificationParseResult::Parsed(TaskItemNotification {
        status,
        summary,
        details,
    })
}

/// Parse both the terminal `<task action="…">` tag and the
/// `<task-item-notification>` block from a final assistant message. The
/// completion is the *first* terminal task action found (in practice agents
/// emit at most one per turn). The notification result preserves the
/// missing-vs-malformed distinction so downstream handlers can pick the
/// right path: append-changelog on `Parsed`, no-op on `Missing`, and
/// auto-reprompt on `Malformed`.
pub fn parse_completion_with_notification(text: &str) -> ParsedAgentMessage {
    let (_cleaned, actions, _errors) = extract_task_actions(text);
    let completion = actions.into_iter().next();
    let notification = extract_task_item_notification(text);
    ParsedAgentMessage {
        completion,
        notification,
    }
}

pub fn strip_task_tags(text: &str) -> String {
    let cleaned = TASK_TAG_RE.replace_all(text, "");
    let cleaned = TASK_SELF_CLOSING_RE.replace_all(&cleaned, "");
    cleaned.into_owned()
}

pub fn strip_tasklist_tags(text: &str) -> String {
    let cleaned = TASKLIST_TAG_RE.replace_all(text, "");
    let cleaned = TASKLIST_SELF_CLOSING_RE.replace_all(&cleaned, "");
    cleaned.into_owned()
}

/// Strip every `<task-item-notification>...</task-item-notification>` block
/// from `text`. Used by the agent_runner's parse-success path so the raw
/// notification XML doesn't leak into the user-visible response after the
/// changelog/mailbox handlers have consumed it.
pub fn strip_task_item_notification(text: &str) -> String {
    TASK_ITEM_NOTIFICATION_RE
        .replace_all(text, "")
        .into_owned()
}

/// Re-emit a parsed [`TaskItemNotification`] as the canonical XML block. Used
/// by the parse-success path to format the payload routed to a `remind_me`
/// agent's mailbox so the receiver sees the same shape an upstream agent
/// would emit.
pub fn format_task_item_notification(n: &TaskItemNotification) -> String {
    let mut out = String::new();
    out.push_str("<task-item-notification>\n");
    out.push_str("<status>");
    out.push_str(&n.status);
    out.push_str("</status>\n");
    out.push_str("<summary>");
    out.push_str(&n.summary);
    out.push_str("</summary>\n");
    if let Some(details) = &n.details {
        out.push_str("<details>");
        out.push_str(details);
        out.push_str("</details>\n");
    }
    out.push_str("</task-item-notification>");
    out
}

/// Format a `<tasklist>` parse error as an actionable message for both the
/// agent reprompt (so it can self-correct on retry) and the user-facing
/// system bubble. Includes a worked YAML body example since the most common
/// failure mode is the agent emitting a YAML map instead of a list.
pub fn format_tasklist_parse_error(err: &TagParseError) -> String {
    format!(
        "[<tasklist> tag parse failed: {}.\n\n\
         The body must be a YAML list of group specs (note the leading `-`).\n\
         Coordinator (create a new tasklist):\n\
         <tasklist action=\"create\" team=\"…\" title=\"…\">\n\
         - mode: PAR\n\
         \x20\x20tasks:\n\
         \x20\x20\x20\x20- owner_agent_id: agent_id\n\
         \x20\x20\x20\x20\x20\x20prompt: \"What this agent should do\"\n\
         \x20\x20\x20\x20\x20\x20expected_outputs: [\"file.md\"]\n\
         </tasklist>\n\n\
         Co-pilot (append to the bound tasklist; no team/title attrs):\n\
         <tasklist action=\"append\">\n\
         - mode: PAR\n\
         \x20\x20tasks:\n\
         \x20\x20\x20\x20- owner_agent_id: agent_id_from_roster\n\
         \x20\x20\x20\x20\x20\x20prompt: \"What this agent should do\"\n\
         </tasklist>\n\n\
         Re-emit the tag with a corrected body.]",
        err.message
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // === <task> parsing ===

    #[test]
    fn no_tags_returns_empty() {
        let (cleaned, actions, errors) =
            extract_task_actions("Just some normal agent output without any tags.");
        assert_eq!(cleaned, "Just some normal agent output without any tags.");
        assert!(actions.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn complete_self_closing() {
        let input = r#"All set. <task action="complete" task_id="task-1" />"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert!(errors.is_empty());
        assert_eq!(actions, vec![TaskTagAction::Complete {
            task_id: "task-1".to_string(),
        }]);
    }

    #[test]
    fn complete_body_form() {
        let input = r#"<task action="complete" task_id="task-1"></task>"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert!(errors.is_empty());
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0], TaskTagAction::Complete {
            task_id: "task-1".to_string(),
        });
    }

    #[test]
    fn fail_with_reason_attr() {
        let input =
            r#"<task action="fail" task_id="task-2" reason="model rejected the prompt" />"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert!(errors.is_empty());
        assert_eq!(
            actions,
            vec![TaskTagAction::Fail {
                task_id: "task-2".to_string(),
                reason: "model rejected the prompt".to_string(),
            }]
        );
    }

    #[test]
    fn fail_reason_in_body() {
        let input = r#"<task action="fail" task_id="task-2">
the workspace was missing required inputs
</task>"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert!(errors.is_empty());
        assert_eq!(
            actions,
            vec![TaskTagAction::Fail {
                task_id: "task-2".to_string(),
                reason: "the workspace was missing required inputs".to_string(),
            }]
        );
    }

    #[test]
    fn request_clarification_question_attr() {
        let input = r#"<task action="request_clarification" task_id="task-3" question="which input file should I use?" />"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert!(errors.is_empty());
        assert_eq!(
            actions,
            vec![TaskTagAction::RequestClarification {
                task_id: "task-3".to_string(),
                question: "which input file should I use?".to_string(),
            }]
        );
    }

    #[test]
    fn missing_action_returns_structured_error() {
        let input = r#"<task task_id="task-1" />"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert!(actions.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tag, "task");
        assert!(errors[0].message.contains("action"));
    }

    #[test]
    fn missing_task_id_returns_structured_error() {
        let input = r#"<task action="complete" />"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert!(actions.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("task_id"));
    }

    #[test]
    fn unrecognized_action_returns_structured_error() {
        let input = r#"<task action="explode" task_id="task-1" />"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert!(actions.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tag, "task");
        assert!(errors[0].message.contains("unrecognized action"));
    }

    #[test]
    fn task_regex_does_not_match_tasklist_tags() {
        let input = r#"<tasklist action="create" team="t" title="x">[]</tasklist>"#;
        let (cleaned, actions, errors) = extract_task_actions(input);
        // tasklist tag must be preserved (not consumed as a task tag) and no
        // task actions should be picked up.
        assert!(cleaned.contains("<tasklist"));
        assert!(actions.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn strips_recognized_tags_from_text() {
        let input = r#"Done.
<task action="complete" task_id="task-1" />
After."#;
        let (cleaned, _actions, _errors) = extract_task_actions(input);
        assert!(cleaned.contains("Done."));
        assert!(cleaned.contains("After."));
        assert!(!cleaned.contains("<task"));
    }

    // === <tasklist> parsing ===

    #[test]
    fn create_yaml_body() {
        let input = r#"<tasklist action="create" team="alpha" title="Investigate spike">
- mode: PAR
  tasks:
    - owner_agent_id: researcher
      prompt: "look at recent metrics"
      expected_outputs: ["metrics.md"]
    - owner_agent_id: log_grepper
      prompt: "scan error logs"
      expected_outputs: ["errors.md"]
- mode: SEQ
  tasks:
    - owner_agent_id: analyst
      prompt: "synthesize results"
      expected_outputs: ["analysis.md"]
</tasklist>"#;
        let (_cleaned, actions, errors) = extract_tasklist_actions(input);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            TasklistTagAction::Create {
                team,
                title,
                description,
                groups,
            } => {
                assert_eq!(team, "alpha");
                assert_eq!(title, "Investigate spike");
                assert!(description.is_none());
                assert_eq!(groups.len(), 2);
                assert_eq!(groups[0].mode, TaskGroupMode::Par);
                assert_eq!(groups[0].tasks.len(), 2);
                assert_eq!(groups[0].tasks[0].owner_agent_id, "researcher");
                assert_eq!(groups[0].tasks[0].expected_outputs, vec!["metrics.md"]);
                assert_eq!(groups[1].mode, TaskGroupMode::Seq);
                assert_eq!(groups[1].tasks.len(), 1);
                assert_eq!(groups[1].tasks[0].owner_agent_id, "analyst");
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn create_json_body() {
        let input = r#"<tasklist action="create" team="alpha" title="JSON form" description="e2e">
[
  {"mode": "PAR", "tasks": [{"owner_agent_id": "a", "prompt": "p", "expected_outputs": ["o.txt"]}]}
]
</tasklist>"#;
        let (_cleaned, actions, errors) = extract_tasklist_actions(input);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            TasklistTagAction::Create {
                description,
                groups,
                ..
            } => {
                assert_eq!(description.as_deref(), Some("e2e"));
                assert_eq!(groups.len(), 1);
                assert_eq!(groups[0].mode, TaskGroupMode::Par);
                assert_eq!(groups[0].tasks[0].expected_outputs, vec!["o.txt"]);
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn append_yaml_body_no_attrs() {
        let input = r#"<tasklist action="append">
- mode: PAR
  tasks:
    - owner_agent_id: alice
      prompt: "echo hi"
    - owner_agent_id: bob
      prompt: "echo hello"
</tasklist>"#;
        let (_cleaned, actions, errors) = extract_tasklist_actions(input);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            TasklistTagAction::Append { groups } => {
                assert_eq!(groups.len(), 1);
                assert_eq!(groups[0].mode, TaskGroupMode::Par);
                assert_eq!(groups[0].tasks.len(), 2);
                assert_eq!(groups[0].tasks[0].owner_agent_id, "alice");
                assert_eq!(groups[0].tasks[1].owner_agent_id, "bob");
            }
            other => panic!("expected Append, got {other:?}"),
        }
    }

    #[test]
    fn append_empty_body_is_error() {
        let input = r#"<tasklist action="append"></tasklist>"#;
        let (_cleaned, actions, errors) = extract_tasklist_actions(input);
        assert!(actions.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("YAML or JSON"));
    }

    #[test]
    fn create_missing_team_attr_is_error() {
        let input = r#"<tasklist action="create" title="x">
- mode: PAR
  tasks: []
</tasklist>"#;
        let (_cleaned, actions, errors) = extract_tasklist_actions(input);
        assert!(actions.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("team"));
    }

    #[test]
    fn create_missing_title_attr_is_error() {
        let input = r#"<tasklist action="create" team="alpha">
- mode: PAR
  tasks: []
</tasklist>"#;
        let (_cleaned, actions, errors) = extract_tasklist_actions(input);
        assert!(actions.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("title"));
    }

    #[test]
    fn create_empty_body_is_error() {
        let input = r#"<tasklist action="create" team="alpha" title="t"></tasklist>"#;
        let (_cleaned, actions, errors) = extract_tasklist_actions(input);
        assert!(actions.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("YAML or JSON"));
    }

    #[test]
    fn create_invalid_yaml_is_error() {
        let input = r#"<tasklist action="create" team="alpha" title="t">
this: is: not: valid: yaml
</tasklist>"#;
        let (_cleaned, actions, errors) = extract_tasklist_actions(input);
        assert!(actions.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tag, "tasklist");
        assert!(errors[0].message.contains("invalid groups body"));
    }

    #[test]
    fn unknown_tasklist_action_is_error() {
        let input = r#"<tasklist action="bogus" team="a" title="t">[]</tasklist>"#;
        let (_cleaned, actions, errors) = extract_tasklist_actions(input);
        assert!(actions.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("unrecognized action"));
    }

    #[test]
    fn multiple_task_tags_in_one_response() {
        let input = r#"first <task action="complete" task_id="t1" /> middle <task action="fail" task_id="t2" reason="bad" /> end"#;
        let (cleaned, actions, errors) = extract_task_actions(input);
        assert!(errors.is_empty());
        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0], TaskTagAction::Complete { task_id } if task_id == "t1"));
        assert!(
            matches!(&actions[1], TaskTagAction::Fail { task_id, reason } if task_id == "t2" && reason == "bad")
        );
        assert!(cleaned.contains("first"));
        assert!(cleaned.contains("middle"));
        assert!(cleaned.contains("end"));
    }

    #[test]
    fn self_closing_attr_value_with_slash() {
        // Regression: an earlier `[^/]*?` attribute zone made any self-closing
        // <task ... /> whose attribute value contained `/` (paths, URLs, globs)
        // silently fail to match — the agent's reason was dropped, the run
        // looked like "no terminal action", and the feeder reprompted twice.
        let input = r#"<task action="fail" task_id="t1" reason="missing /Users/me/.launchpad_studio/teams/x/workspace/y" />"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(
            actions,
            vec![TaskTagAction::Fail {
                task_id: "t1".to_string(),
                reason: "missing /Users/me/.launchpad_studio/teams/x/workspace/y".to_string(),
            }]
        );
    }

    #[test]
    fn tasklist_self_closing_attr_value_with_slash() {
        // Same regression for the tasklist self-closing regex. (The body form
        // never had this bug — `[^>]` was already used there.)
        let input = r#"<tasklist action="bogus" team="t/with/slash" title="x" />"#;
        let (_cleaned, _actions, errors) = extract_tasklist_actions(input);
        // Slash-bearing attrs should at least reach the action handler. We
        // expect the unrecognized-action error here, NOT a missing-match.
        assert_eq!(errors.len(), 1, "expected the regex to match and the action handler to reject; got: {errors:?}");
        assert!(errors[0].message.contains("unrecognized action"));
    }

    #[test]
    fn parse_continues_after_error() {
        // Two tags: one malformed, one valid. Parser should report the error
        // AND surface the valid action.
        let input = r#"<task task_id="t1" /><task action="complete" task_id="t2" />"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0], TaskTagAction::Complete {
            task_id: "t2".to_string(),
        });
        assert_eq!(errors.len(), 1);
    }

    // === <task-item-notification> parsing ===

    #[test]
    fn notification_well_formed_with_details_parses() {
        let input = r#"<task-item-notification>
  <status>complete</status>
  <summary>Wrote splunk_logs.json (1,284 entries) and summary.md.</summary>
  <details>Time range: last 24h. Top error: timeout.</details>
</task-item-notification>"#;
        match extract_task_item_notification(input) {
            NotificationParseResult::Parsed(n) => {
                assert_eq!(n.status, "complete");
                assert_eq!(
                    n.summary,
                    "Wrote splunk_logs.json (1,284 entries) and summary.md."
                );
                assert_eq!(
                    n.details.as_deref(),
                    Some("Time range: last 24h. Top error: timeout.")
                );
            }
            other => panic!("expected Parsed, got {other:?}"),
        }
    }

    #[test]
    fn notification_well_formed_without_details_parses() {
        let input = r#"<task-item-notification>
  <status>failed</status>
  <summary>Could not write outputs because the workspace was missing.</summary>
</task-item-notification>"#;
        match extract_task_item_notification(input) {
            NotificationParseResult::Parsed(n) => {
                assert_eq!(n.status, "failed");
                assert_eq!(
                    n.summary,
                    "Could not write outputs because the workspace was missing."
                );
                assert!(n.details.is_none());
            }
            other => panic!("expected Parsed, got {other:?}"),
        }
    }

    #[test]
    fn notification_missing_returns_missing() {
        // No notification block anywhere in the message. Must NOT be confused
        // with malformed: the auto-reprompt path keys on malformed only.
        let input = "Done. <task action=\"complete\" task_id=\"t1\" />";
        assert_eq!(
            extract_task_item_notification(input),
            NotificationParseResult::Missing
        );
    }

    #[test]
    fn notification_unclosed_tag_is_malformed() {
        // Opening tag present, no matching closing tag -> malformed.
        let input = r#"<task-item-notification>
  <status>complete</status>
  <summary>oops, forgot to close</summary>"#;
        match extract_task_item_notification(input) {
            NotificationParseResult::Malformed(msg) => {
                assert!(msg.contains("closing tag"), "msg was: {msg}");
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn notification_missing_status_field_is_malformed() {
        let input = r#"<task-item-notification>
  <summary>summary without a status</summary>
</task-item-notification>"#;
        match extract_task_item_notification(input) {
            NotificationParseResult::Malformed(msg) => assert!(msg.contains("<status>")),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn notification_missing_summary_field_is_malformed() {
        let input = r#"<task-item-notification>
  <status>complete</status>
</task-item-notification>"#;
        match extract_task_item_notification(input) {
            NotificationParseResult::Malformed(msg) => assert!(msg.contains("<summary>")),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn notification_empty_required_field_is_malformed() {
        let input = r#"<task-item-notification>
  <status></status>
  <summary>has summary but empty status</summary>
</task-item-notification>"#;
        match extract_task_item_notification(input) {
            NotificationParseResult::Malformed(msg) => assert!(msg.contains("empty")),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    // === parse_completion_with_notification: the four scenarios ===

    #[test]
    fn combined_completion_plus_notification_both_well_formed() {
        let input = r#"All set.

<task action="complete" task_id="t-collect" />

<task-item-notification>
  <status>complete</status>
  <summary>Wrote splunk_logs.json and summary.md.</summary>
  <details>Top error: timeout (37%).</details>
</task-item-notification>"#;
        let parsed = parse_completion_with_notification(input);
        assert_eq!(
            parsed.completion,
            Some(TaskTagAction::Complete {
                task_id: "t-collect".to_string(),
            })
        );
        match parsed.notification {
            NotificationParseResult::Parsed(n) => {
                assert_eq!(n.status, "complete");
                assert_eq!(n.summary, "Wrote splunk_logs.json and summary.md.");
                assert_eq!(n.details.as_deref(), Some("Top error: timeout (37%)."));
            }
            other => panic!("expected Parsed, got {other:?}"),
        }
    }

    #[test]
    fn combined_completion_only_notification_missing() {
        let input = r#"Finished.
<task action="complete" task_id="t-1" />"#;
        let parsed = parse_completion_with_notification(input);
        assert_eq!(
            parsed.completion,
            Some(TaskTagAction::Complete {
                task_id: "t-1".to_string(),
            })
        );
        assert_eq!(parsed.notification, NotificationParseResult::Missing);
    }

    #[test]
    fn combined_completion_with_malformed_notification_unclosed() {
        let input = r#"<task action="complete" task_id="t-2" />
<task-item-notification>
  <status>complete</status>
  <summary>forgot the closer</summary>"#;
        let parsed = parse_completion_with_notification(input);
        assert_eq!(
            parsed.completion,
            Some(TaskTagAction::Complete {
                task_id: "t-2".to_string(),
            })
        );
        assert!(matches!(
            parsed.notification,
            NotificationParseResult::Malformed(_)
        ));
    }

    #[test]
    fn combined_completion_with_malformed_notification_missing_field() {
        let input = r#"<task action="complete" task_id="t-3" />
<task-item-notification>
  <summary>only a summary, status missing</summary>
</task-item-notification>"#;
        let parsed = parse_completion_with_notification(input);
        assert_eq!(
            parsed.completion,
            Some(TaskTagAction::Complete {
                task_id: "t-3".to_string(),
            })
        );
        match parsed.notification {
            NotificationParseResult::Malformed(msg) => assert!(msg.contains("<status>")),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn combined_notification_without_completion_treated_as_completion_miss() {
        // A notification without a completion tag is treated as a parse
        // miss for the completion path. The notification still parses
        // successfully, but `completion` is None so callers can decide how
        // to handle the mismatch (in practice: don't append a changelog
        // entry, don't fire remind_me, treat the turn as non-terminal).
        let input = r#"Some narration with no terminal tag.

<task-item-notification>
  <status>complete</status>
  <summary>this looks like a completion but there's no <task action> tag.</summary>
</task-item-notification>"#;
        let parsed = parse_completion_with_notification(input);
        assert!(parsed.completion.is_none());
        assert!(matches!(
            parsed.notification,
            NotificationParseResult::Parsed(_)
        ));
    }

    #[test]
    fn combined_nested_form_completion_with_notification_inside_body() {
        // The contracted shape: `<task action="complete" task_id="…">` with
        // the `<task-item-notification>` block nested in its body. Both
        // halves must parse successfully and the surrounding `<task>` body
        // must not bleed into a fail-reason or other free-form field.
        let input = r#"All set.

<task action="complete" task_id="t-nested">
  <task-item-notification>
    <status>complete</status>
    <summary>Wrote splunk_logs.json and summary.md.</summary>
    <details>Top error: timeout (37%).</details>
  </task-item-notification>
</task>"#;
        let parsed = parse_completion_with_notification(input);
        assert_eq!(
            parsed.completion,
            Some(TaskTagAction::Complete {
                task_id: "t-nested".to_string(),
            })
        );
        match parsed.notification {
            NotificationParseResult::Parsed(n) => {
                assert_eq!(n.status, "complete");
                assert_eq!(n.summary, "Wrote splunk_logs.json and summary.md.");
                assert_eq!(n.details.as_deref(), Some("Top error: timeout (37%)."));
            }
            other => panic!("expected Parsed, got {other:?}"),
        }
    }

    #[test]
    fn combined_nested_fail_reason_from_attr_not_polluted_by_notification_body() {
        // Regression guard for the new nested form: when `<task action="fail">`
        // wraps a notification block, the embedded XML must NOT leak into
        // `Fail.reason`. Reason must come from the attribute (or be empty if
        // neither the attr nor non-notification body text is present).
        let input = r#"<task action="fail" task_id="t-fn" reason="missing input file">
  <task-item-notification>
    <status>failed</status>
    <summary>Could not produce outputs.</summary>
  </task-item-notification>
</task>"#;
        let (_cleaned, actions, errors) = extract_task_actions(input);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(
            actions,
            vec![TaskTagAction::Fail {
                task_id: "t-fn".to_string(),
                reason: "missing input file".to_string(),
            }]
        );
    }

    #[test]
    fn combined_neither_completion_nor_notification() {
        // Sanity guard: a plain narrative response should produce a clean
        // "nothing terminal happened" result, not an error.
        let input = "I'm still investigating; no result yet.";
        let parsed = parse_completion_with_notification(input);
        assert!(parsed.completion.is_none());
        assert_eq!(parsed.notification, NotificationParseResult::Missing);
    }

    // === <task-item-notification> stripping + reformatting ===

    #[test]
    fn strip_task_item_notification_removes_block() {
        let input = "Hello\n<task-item-notification>\n<status>complete</status>\n<summary>done</summary>\n</task-item-notification>\nWorld";
        let stripped = strip_task_item_notification(input);
        assert!(!stripped.contains("<task-item-notification>"));
        assert!(!stripped.contains("</task-item-notification>"));
        assert!(stripped.contains("Hello"));
        assert!(stripped.contains("World"));
    }

    #[test]
    fn strip_task_item_notification_is_noop_when_absent() {
        let input = "Plain text with no notification block.";
        assert_eq!(strip_task_item_notification(input), input);
    }

    #[test]
    fn format_task_item_notification_round_trips_via_extractor() {
        let original = TaskItemNotification {
            status: "complete".to_string(),
            summary: "Wrote analysis.md".to_string(),
            details: Some("Investigated three signals.".to_string()),
        };
        let xml = format_task_item_notification(&original);
        assert!(xml.starts_with("<task-item-notification>"));
        assert!(xml.ends_with("</task-item-notification>"));
        let parsed = match extract_task_item_notification(&xml) {
            NotificationParseResult::Parsed(n) => n,
            other => panic!("expected Parsed, got {:?}", other),
        };
        assert_eq!(parsed, original);
    }

    #[test]
    fn format_task_item_notification_omits_details_tag_when_none() {
        let n = TaskItemNotification {
            status: "complete".to_string(),
            summary: "no details here".to_string(),
            details: None,
        };
        let xml = format_task_item_notification(&n);
        assert!(!xml.contains("<details>"));
        assert!(!xml.contains("</details>"));
    }
}
