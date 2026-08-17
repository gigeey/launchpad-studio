//! Tasklist-mode runtime helpers used by `AgentRunner` when a run is dispatched
//! by the TaskFeeder.
//!
//! Two pure helpers live here so they can be unit-tested without spinning up a
//! full agent run:
//!
//! - [`build_tasklist_preamble`] formats the system-prompt block injected into
//!   tasklist-mode runs (team name, tasklist title, workspace path, assigned
//!   task + expected outputs, `<task action="...">` tag reference, and explicit
//!   limited-user-interaction guidance).
//! - [`autonomous_pacing_section`] is the shared no-user-available block that
//!   every autonomous session embeds.
//!
//! Transcript-path resolution deliberately does NOT live here. It is done at
//! the call site in `agent_runner::cli` (`transcript_path_override`), which is
//! the only place that knows a tasklist run's ownership — and ownership is
//! what decides which tree the file belongs in.

use ao_protocol::tasklist::{Task, Tasklist};

/// System-prompt section shared by every autonomous session.
///
/// Instructs the model to operate without waiting for user input, explains
/// permission-ask semantics in autonomous mode, and gives usage guidance for
/// the `Sleep` tool (available exclusively in autonomous sessions).
///
/// Tasklist-mode runs embed this via [`build_tasklist_preamble`]. Scheduled
/// tasks and background subagents receive it as a top-level suffix on their
/// system prompt.
pub fn autonomous_pacing_section() -> &'static str {
    "## Limited User Interaction

You are NOT in a normal interactive chat. There is no user available to answer follow-up questions in real time. Do not stop and ask the user for clarification. Either:

1. Complete the task autonomously and emit the appropriate completion signal, or
2. If the task is genuinely impossible, emit a failure signal with a clear reason, or
3. If you need information that only a coordinator can provide, emit a clarification request and continue making progress on what you can.

If a tool prompts for permission, the sandbox blocks a write, an MCP server is unavailable, or any environmental constraint prevents you from proceeding, that is a valid failure case. Do **not** silently stall or retry indefinitely — emit the fail signal and put the specific cause in the reason field so the operator can address it.

**Permission handling:** When a tool call requires interactive approval and no auto-approve rule covers it, the call is denied with a recoverable error. Adjust your plan and try an alternative approach rather than retrying the same call.

**Sleep:** Use the `Sleep` tool to wait between polling steps, scheduled bursts, or when blocking on an external condition. Sleep only when you genuinely have nothing to do until a future time. Avoid sleeping for trivially short durations or before operations that can be attempted immediately."
}

/// Build the tasklist-mode preamble appended to the agent's system prompt for
/// the duration of a single tasklist task run.
///
/// The block names the team, tasklist, workspace path, the assigned task
/// prompt and its declared `expected_outputs`, documents the `<task>` tag
/// surface the agent uses to drive state transitions, and the
/// shared autonomous-pacing guidance.
pub fn build_tasklist_preamble(team_name: &str, tasklist: &Tasklist, task: &Task) -> String {
    let outputs = if task.expected_outputs.is_empty() {
        "(none declared)".to_string()
    } else {
        task.expected_outputs
            .iter()
            .map(|o| format!("- {}", o))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let description = if tasklist.description.trim().is_empty() {
        String::new()
    } else {
        format!("\nTasklist description: {}\n", tasklist.description.trim())
    };

    format!(
        r#"# Tasklist Mode

You are running as part of a coordinated tasklist. The TaskFeeder has dispatched a single task to you. Stay focused on completing it.

## Context

- Team: {team_name}
- Tasklist: {tasklist_title} (id: {tasklist_id}){description}- Shared workspace (write declared outputs here): {workspace_dir}
- Progress log: {workspace_dir}/progress.jsonl
- Sibling task outputs: {workspace_dir}/tasks/{{task_id}}/output.txt

## Prior work

If this is not the first task in this tasklist, prior tasks' outputs are at
`{workspace_dir}/tasks/<task_id>/output.txt`. The progress log
(`{workspace_dir}/progress.jsonl`) lists every terminal task with its
summary, status, and output path — read it before starting if context
from prior tasks would inform your work.

## Durable-output rule (verify / audit / punch-list tasks)

If this task involves verifying, auditing, tracing seams, or producing a punch-list of findings,
the `<summary>` field alone is not sufficient — summaries are compressed and can be lost if the
runner dies or the coordinator summarizes without re-reading disk. Before marking complete, you MUST:

1. **Write findings to the workspace output file** (`{workspace_dir}/tasks/{{task_id}}/output.txt`).
   If not already listed under expected outputs, write it anyway.
2. **Append findings to a durable notes file** in the source repository, alongside
   wherever that project already keeps its documentation. This makes them
   recoverable without re-running verification.

Both steps are required whenever the task intent includes verification, audit, smoke, seam tracing,
or punch-list capture.

## Your Assigned Task

- Task id: {task_id}
- Prompt: {task_prompt}

### Expected outputs
Files you must write under the shared workspace before completing this task:
{outputs}

## Tag Reference

Use `<task>` tags in your response to drive tasklist state. The TaskFeeder will read these tags after your run ends and advance the tasklist accordingly.

**For `complete` and `fail`: the `<task>` tag MUST have a body containing a `<task-item-notification>` block. Self-closing `<task ... />` is NOT valid for `complete` or `fail` — it will be rejected as a parse failure and you will be reprompted.** The notification carries the human-readable summary the runtime needs; without it the task cannot be accepted.

- Mark this task complete (after writing every expected output):
  ```xml
  <task action="complete" task_id="{task_id}">
    <task-item-notification>
      <status>complete</status>
      <summary>One-line past-tense summary of what you did (≤200 chars).</summary>
      <details>Optional longer body — paths produced, anomalies, follow-ups. Omit if nothing to add.</details>
    </task-item-notification>
  </task>
  ```
- Fail this task (cannot proceed even with reprompts):
  ```xml
  <task action="fail" task_id="{task_id}" reason="short failure reason">
    <task-item-notification>
      <status>failed</status>
      <summary>One-line past-tense summary of why this failed.</summary>
      <details>Optional details on what was attempted and what blocked you.</details>
    </task-item-notification>
  </task>
  ```
- Ask the coordinator for clarification (logged; does not stall the tasklist; self-closing IS valid here):
  ```xml
  <task action="request_clarification" task_id="{task_id}" question="what you need" />
  ```

### Notification fields

- `status` (required) — one of `complete`, `failed`, `needs_clarification`. Mirror the action in the wrapping `<task action="…">` tag.
- `summary` (required) — single line, ≤200 chars, past tense.
- `details` (optional) — free-form/markdown; omit the element entirely if you have nothing to add.

{autonomous_pacing}

Stay scoped to this single task — do not work on other tasks in the tasklist."#,
        team_name = team_name,
        tasklist_title = tasklist.title,
        tasklist_id = tasklist.id,
        description = description,
        workspace_dir = tasklist.workspace_dir,
        task_id = task.id,
        task_prompt = task.prompt.trim(),
        outputs = outputs,
        autonomous_pacing = autonomous_pacing_section(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Utc;
    use ao_protocol::tasklist::{Task, TaskStatus, Tasklist, TasklistStatus};

    fn task(id: &str, prompt: &str, outputs: Vec<&str>) -> Task {
        Task {
            id: id.to_string(),
            owner_agent_id: "agent-x".to_string(),
            prompt: prompt.to_string(),
            expected_outputs: outputs.into_iter().map(|s| s.to_string()).collect(),
            status: TaskStatus::Pending,
            group_id: "g1".to_string(),
            attempt_count: 0,
            error_log: vec![],
            comments: vec![],
            attachments: vec![],
            remind_me: None,
            parse_failed: false,
            notification_parse_retry_count: 0,
            assignment: None,
            classifier_token: 0,
            dispatch_token: 0,
        }
    }

    fn tasklist(id: &str, team_id: &str, title: &str, description: &str) -> Tasklist {
        Tasklist {
            id: id.to_string(),
            owner: ao_protocol::tasklist::TasklistOwner::Team { team_id: team_id.to_string() },
            team_id: Some(team_id.to_string()),
            title: title.to_string(),
            description: description.to_string(),
            status: TasklistStatus::Active,
            groups: vec![],
            workspace_dir: format!("/data/teams/{team_id}/tasklists/{id}/workspace"),
            transcripts_dir: format!("/data/teams/{team_id}/tasklists/{id}/transcripts"),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: None,
            last_opened_at: None,
            project_id: None,
            thread_id: None,
        }
    }

    // --- build_tasklist_preamble ---

    #[test]
    fn preamble_contains_team_tasklist_workspace_and_task_details() {
        let tl = tasklist("tl-research", "alpha", "Splunk Investigation", "");
        let t = task(
            "t-collect",
            "Collect splunk logs for the last 24h",
            vec!["splunk_logs.json", "summary.md"],
        );
        let preamble = build_tasklist_preamble("Alpha Team", &tl, &t);

        // Team + tasklist identity
        assert!(preamble.contains("Team: Alpha Team"), "missing team name: {preamble}");
        assert!(
            preamble.contains("Splunk Investigation"),
            "missing tasklist title"
        );
        assert!(preamble.contains("tl-research"), "missing tasklist id");

        // Workspace path
        assert!(
            preamble.contains("/data/teams/alpha/tasklists/tl-research/workspace"),
            "missing workspace dir: {preamble}"
        );

        // Assigned task prompt + id
        assert!(preamble.contains("t-collect"), "missing task id");
        assert!(
            preamble.contains("Collect splunk logs for the last 24h"),
            "missing task prompt"
        );

        // Expected outputs
        assert!(preamble.contains("- splunk_logs.json"));
        assert!(preamble.contains("- summary.md"));
    }

    #[test]
    fn preamble_contains_task_tag_reference_and_user_interaction_guidance() {
        let tl = tasklist("tl-x", "alpha", "X", "");
        let t = task("t-y", "Do Y", vec![]);
        let preamble = build_tasklist_preamble("Alpha", &tl, &t);

        // <task action="..."> tag reference uses the NESTED body form for
        // complete/fail (self-closing for those is a parse failure).
        assert!(
            preamble.contains(r#"<task action="complete" task_id="t-y">"#),
            "preamble must show nested-body form for complete (no self-closing /> for this task_id):\n{preamble}"
        );
        assert!(
            !preamble.contains(r#"<task action="complete" task_id="t-y" />"#),
            "preamble must NOT show the self-closing form for complete — that is a parse failure"
        );
        assert!(
            preamble.contains("<task-item-notification>"),
            "preamble must show the required <task-item-notification> body"
        );
        assert!(
            preamble.contains("<status>complete</status>")
                && preamble.contains("<status>failed</status>"),
            "preamble must show the status values for both complete and fail examples"
        );
        assert!(
            preamble.contains("<summary>"),
            "preamble must show the required <summary> field in the notification"
        );
        assert!(preamble.contains(r#"<task action="fail""#));
        assert!(preamble.contains(r#"<task action="request_clarification""#));
        // request_clarification is the ONLY action where self-closing remains valid
        assert!(
            preamble.contains(r#"<task action="request_clarification" task_id="t-y" question="what you need" />"#),
            "request_clarification should still document the self-closing form"
        );

        // Limited user interaction guidance: the preamble must explicitly
        // tell the agent NOT to stop and ask the user.
        assert!(
            preamble.to_lowercase().contains("limited user interaction")
                || preamble
                    .to_lowercase()
                    .contains("not in a normal interactive chat"),
            "missing limited-user-interaction guidance"
        );
        assert!(
            preamble
                .to_lowercase()
                .contains("do not stop and ask the user"),
            "missing explicit do-not-ask-user instruction"
        );
    }

    #[test]
    fn preamble_handles_empty_expected_outputs() {
        let tl = tasklist("tl-empty", "alpha", "Empty", "");
        let t = task("t-empty", "Just think", vec![]);
        let preamble = build_tasklist_preamble("Alpha", &tl, &t);
        assert!(preamble.contains("(none declared)"));
    }

    #[test]
    fn preamble_includes_description_when_present() {
        let tl = tasklist(
            "tl-desc",
            "alpha",
            "Investigation",
            "Run a multi-agent splunk investigation.",
        );
        let t = task("t-d", "Do it", vec!["out.md"]);
        let preamble = build_tasklist_preamble("Alpha", &tl, &t);
        assert!(
            preamble.contains("Run a multi-agent splunk investigation."),
            "tasklist description should appear in preamble: {preamble}"
        );
    }

    #[test]
    fn preamble_contains_progress_and_output_pointers() {
        let tl = tasklist("tl-ptr", "alpha", "Pointer Test", "");
        let t = task("t-ptr", "Read prior work", vec![]);
        let preamble = build_tasklist_preamble("Alpha", &tl, &t);

        assert!(
            preamble.contains("progress.jsonl"),
            "preamble must name the progress log: {preamble}"
        );
        // {task_id} is a literal placeholder in the preamble, not interpolated.
        assert!(
            preamble.contains("tasks/{task_id}/output.txt"),
            "preamble must contain the literal sibling-output pattern: {preamble}"
        );
        assert!(
            preamble.contains("## Prior work"),
            "preamble must contain Prior work section: {preamble}"
        );
    }
}
