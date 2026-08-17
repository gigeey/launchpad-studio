//! Per-conversation context injection for tasklist co-pilot agents.
//!
//! When the message-routing path delivers a [`QueuedMessage`] to an agent
//! whose profile is the [`COPILOT_PROFILE_ID`] template AND that agent is
//! bound to a tasklist via [`TasklistStore::find_by_copilot_agent_id`], the
//! pump augments `message.content` with a `<copilot-context>` block
//! containing:
//! 1. A serialized summary of the current tasklist (id, title, status, and
//!    each task's id/owner/status/prompt).
//! 2. The last [`RECENT_CHANGELOG_LIMIT`] entries from the tasklist's hidden
//!    changelog.
//!
//! Injection fires for both user-typed messages and inbound
//! `<task-item-notification>` `QueuedMessage`s — anything that flows through
//! `AgentQueueManager::pump` is eligible.
//!
//! Transparency: the original `QueuedMessage.content` and the user-visible
//! transcript are NOT mutated. The augmented string is only what
//! `AgentRunner::run` sees as its `prompt` argument; what the user wrote is
//! preserved on disk via the route handler's `transcripts.append` call.
//! No FE schema or route changes are required.

use std::sync::Arc;

use ao_persistence::PersistenceLayer;
use ao_protocol::changelog::ChangelogEntry;
use ao_protocol::tasklist::{TaskGroupMode, TaskStatus, Tasklist, TasklistStatus};

use crate::prompt_sections::COPILOT_PROFILE_ID;

/// Number of most-recent changelog entries injected into each routed
/// message. Small by design — large enough to give the co-pilot recent
/// activity context, small enough not to dominate the prompt or burn
/// excessive token budget.
pub const RECENT_CHANGELOG_LIMIT: usize = 10;

/// Resolve the supplied agent's tasklist binding (when its profile template
/// is the co-pilot one) and return `content` with a freshly-built
/// `<copilot-context>` block prepended. Returns the original content
/// unchanged when:
///   - `agent_template` is not [`COPILOT_PROFILE_ID`].
///   - No tasklist is bound to this agent.
///   - The persistence layer fails (errors logged + swallowed so a transient
///     lookup hiccup can't drop the user-visible message).
pub async fn inject_copilot_context(
    persistence: &Arc<PersistenceLayer>,
    agent_template: Option<&str>,
    agent_id: &str,
    content: &str,
) -> String {
    if agent_template != Some(COPILOT_PROFILE_ID) {
        return content.to_string();
    }

    let tasklist = match persistence
        .tasklists
        .find_by_copilot_agent_id(agent_id)
        .await
    {
        Ok(Some(tl)) => tl,
        Ok(None) => return content.to_string(),
        Err(e) => {
            tracing::warn!(
                agent_id = %agent_id,
                "co-pilot context injection: tasklist lookup failed: {}",
                e
            );
            return content.to_string();
        }
    };

    let recent = match persistence
        .changelogs
        .read_recent(&tasklist.owner, &tasklist.id, RECENT_CHANGELOG_LIMIT)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                agent_id = %agent_id,
                tasklist_id = %tasklist.id,
                "co-pilot context injection: changelog read failed: {}",
                e
            );
            Vec::new()
        }
    };

    let block = build_copilot_context_block(&tasklist, &recent);
    format!("{block}\n\n{content}")
}

/// Render the full `<copilot-context>` block for a given tasklist + recent
/// changelog tail. Public so unit tests (and any future caller that wants to
/// preview the injected block without doing the persistence dance) can
/// exercise the formatting directly.
pub fn build_copilot_context_block(
    tasklist: &Tasklist,
    recent: &[ChangelogEntry],
) -> String {
    let mut buf = String::new();
    buf.push_str("<copilot-context>\n");
    buf.push_str(&format_tasklist_summary(tasklist));
    buf.push_str("\n\n");
    buf.push_str(&format_recent_changelog(recent));
    buf.push_str("\n</copilot-context>");
    buf
}

fn format_tasklist_summary(tl: &Tasklist) -> String {
    let mut buf = String::new();
    buf.push_str("## Current Tasklist\n");
    buf.push_str(&format!("- id: `{}`\n", tl.id));
    buf.push_str(&format!("- title: {}\n", tl.title));
    buf.push_str(&format!("- status: `{}`\n", tasklist_status_str(tl.status)));

    if tl.groups.is_empty() {
        buf.push_str("- tasks: _(none yet)_");
        return buf;
    }

    buf.push_str("- tasks:\n");
    for group in &tl.groups {
        let mode = match group.mode {
            TaskGroupMode::Par => "PAR",
            TaskGroupMode::Seq => "SEQ",
        };
        buf.push_str(&format!(
            "  - group `{gid}` ({mode}, {count} task{plural}):\n",
            gid = group.id,
            mode = mode,
            count = group.tasks.len(),
            plural = if group.tasks.len() == 1 { "" } else { "s" },
        ));
        if group.tasks.is_empty() {
            buf.push_str("    - _(empty)_\n");
            continue;
        }
        for task in &group.tasks {
            let owner = if task.owner_agent_id.is_empty() {
                "_unowned_".to_string()
            } else {
                format!("`{}`", task.owner_agent_id)
            };
            buf.push_str(&format!(
                "    - `{id}` [{status}] owner: {owner} — {prompt}\n",
                id = task.id,
                status = task_status_str(task.status),
                owner = owner,
                prompt = truncate_one_line(&task.prompt, 200),
            ));
        }
    }
    // Trim the trailing newline so the join in build_copilot_context_block
    // produces exactly one blank line before the changelog section.
    while buf.ends_with('\n') {
        buf.pop();
    }
    buf
}

fn format_recent_changelog(entries: &[ChangelogEntry]) -> String {
    let mut buf = String::new();
    buf.push_str(&format!(
        "## Recent Changelog (last {})\n",
        RECENT_CHANGELOG_LIMIT
    ));
    if entries.is_empty() {
        buf.push_str("_(no entries yet)_");
        return buf;
    }
    for entry in entries {
        buf.push_str(&format!(
            "- {ts} `{task_id}` agent `{agent_id}` status `{status}` — {summary}\n",
            ts = entry.ts.to_rfc3339(),
            task_id = entry.task_id,
            agent_id = entry.agent_id,
            status = entry.status,
            summary = truncate_one_line(&entry.summary, 200),
        ));
        if let Some(d) = &entry.details {
            buf.push_str(&format!("  - details: {}\n", truncate_one_line(d, 400)));
        }
    }
    while buf.ends_with('\n') {
        buf.pop();
    }
    buf
}

fn task_status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Skipped => "skipped",
        TaskStatus::Stopped => "stopped",
    }
}

fn tasklist_status_str(status: TasklistStatus) -> &'static str {
    match status {
        TasklistStatus::Active => "active",
        TasklistStatus::Paused => "paused",
        TasklistStatus::Completed => "completed",
        TasklistStatus::Cancelled => "cancelled",
        TasklistStatus::Failed => "failed",
    }
}

fn truncate_one_line(s: &str, max: usize) -> String {
    let collapsed: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let head: String = collapsed.chars().take(max).collect();
    format!("{}…", head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::tasklist::{Task, TaskGroup};
    use chrono::{TimeZone, Utc};

    fn sample_tasklist(team_id: &str, id: &str) -> Tasklist {
        use ao_protocol::tasklist::TasklistOwner;
        Tasklist {
            id: id.to_string(),
            owner: TasklistOwner::Team { team_id: team_id.to_string() },
            team_id: Some(team_id.to_string()),
            title: "Ship Feature X".to_string(),
            description: String::new(),
            status: TasklistStatus::Active,
            groups: vec![TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Seq,
                tasks: vec![
                    Task {
                        id: "task-1".to_string(),
                        owner_agent_id: "alice".to_string(),
                        prompt: "Design schema".to_string(),
                        expected_outputs: vec![],
                        status: TaskStatus::Completed,
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
                    },
                    Task {
                        id: "task-2".to_string(),
                        owner_agent_id: "bob".to_string(),
                        prompt: "Implement handler".to_string(),
                        expected_outputs: vec![],
                        status: TaskStatus::InProgress,
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
                    },
                ],
            }],
            workspace_dir: "/tmp/ws".to_string(),
            transcripts_dir: "/tmp/tx".to_string(),
            created_at: Utc::now(),
            last_active_at: None,
            copilot_agent_id: Some("copilot-x".to_string()),
            last_opened_at: None,
            project_id: None,
            thread_id: None,
            }
    }

    fn sample_entry(task_id: &str, agent_id: &str, summary: &str, ts_secs: i64) -> ChangelogEntry {
        ChangelogEntry {
            task_id: task_id.to_string(),
            tasklist_id: "tl-1".to_string(),
            agent_id: agent_id.to_string(),
            status: "complete".to_string(),
            summary: summary.to_string(),
            details: None,
            ts: Utc.timestamp_opt(ts_secs, 0).unwrap(),
        }
    }

    #[test]
    fn build_block_contains_tasklist_id_title_status_and_each_task() {
        let tl = sample_tasklist("team-a", "tl-1");
        let block = build_copilot_context_block(&tl, &[]);

        assert!(block.starts_with("<copilot-context>\n"));
        assert!(block.ends_with("</copilot-context>"));

        // Tasklist summary fields.
        assert!(block.contains("## Current Tasklist"));
        assert!(block.contains("id: `tl-1`"), "block missing tasklist id: {block}");
        assert!(block.contains("title: Ship Feature X"));
        assert!(block.contains("status: `active`"));

        // Per-task entries with id, status, owner, prompt.
        assert!(block.contains("`task-1` [completed] owner: `alice`"));
        assert!(block.contains("Design schema"));
        assert!(block.contains("`task-2` [in_progress] owner: `bob`"));
        assert!(block.contains("Implement handler"));

        // Group framing.
        assert!(block.contains("group `g1` (SEQ, 2 tasks)"));
    }

    #[test]
    fn build_block_renders_changelog_entries_in_order() {
        let tl = sample_tasklist("team-a", "tl-1");
        let entries = vec![
            sample_entry("task-1", "alice", "wrote schema doc", 1_700_000_000),
            sample_entry("task-2", "bob", "drafted handler", 1_700_000_100),
        ];
        let block = build_copilot_context_block(&tl, &entries);

        assert!(block.contains("## Recent Changelog (last 10)"));
        assert!(block.contains("`task-1` agent `alice` status `complete` — wrote schema doc"));
        assert!(block.contains("`task-2` agent `bob` status `complete` — drafted handler"));
        // First entry must appear before the second (chronological).
        let first = block.find("wrote schema doc").unwrap();
        let second = block.find("drafted handler").unwrap();
        assert!(first < second);
    }

    #[test]
    fn build_block_handles_empty_changelog_with_explicit_marker() {
        let tl = sample_tasklist("team-a", "tl-1");
        let block = build_copilot_context_block(&tl, &[]);
        assert!(block.contains("_(no entries yet)_"));
    }

    #[test]
    fn build_block_handles_empty_groups_with_explicit_marker() {
        let mut tl = sample_tasklist("team-a", "tl-1");
        tl.groups.clear();
        let block = build_copilot_context_block(&tl, &[]);
        assert!(block.contains("tasks: _(none yet)_"));
    }

    /// The injected block must not advertise a team roster: teams were
    /// removed, so there is no source for one and the co-pilot must not be
    /// told to dispatch to member agent ids.
    #[test]
    fn build_block_does_not_render_a_team_roster() {
        let tl = sample_tasklist("team-a", "tl-1");
        let block = build_copilot_context_block(&tl, &[]);
        assert!(!block.contains("Team Roster"));
        assert!(!block.contains("roster"));
    }

    /// Builds an `Arc<PersistenceLayer>` rooted in a fresh tempdir. Returns
    /// the layer plus the tempdir guard so the caller can keep the dir alive
    /// for the lifetime of the test.
    async fn make_persistence() -> (Arc<PersistenceLayer>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
        let layer = PersistenceLayer::init_with_root(data_root)
            .await
            .expect("persistence init");
        (Arc::new(layer), tmp)
    }

    /// Happy path: bound co-pilot agent gets a well-formed
    /// `<copilot-context>` block prepended to its delivered message.
    #[tokio::test]
    async fn inject_prepends_well_formed_block_for_bound_copilot() {
        let (persistence, _tmp) = make_persistence().await;

        // Seed the tasklist + binding.
        let tl = sample_tasklist("team-15", "tl-15");
        persistence.tasklists.create(&tl).await.unwrap();
        persistence
            .tasklists
            .bind_copilot_agent_id("team-15", "tl-15", "copilot-x")
            .await
            .unwrap();

        // Seed two changelog entries so the "last N entries" behavior is
        // exercised.
        persistence
            .changelogs
            .append(
                &tl.owner,
                "tl-15",
                &sample_entry("task-1", "alice", "wrote schema doc", 1_700_000_000),
            )
            .await
            .unwrap();
        persistence
            .changelogs
            .append(
                &tl.owner,
                "tl-15",
                &sample_entry("task-2", "bob", "drafted handler", 1_700_000_100),
            )
            .await
            .unwrap();

        let original = "What's the status of task-2?";
        let injected = inject_copilot_context(
            &persistence,
            Some(COPILOT_PROFILE_ID),
            "copilot-x",
            original,
        )
        .await;

        // Block frames the prepended content; the user's original message
        // appears verbatim AFTER the closing tag.
        assert!(injected.starts_with("<copilot-context>\n"));
        let close = injected.find("</copilot-context>").expect("closing tag");
        let suffix = &injected[close + "</copilot-context>".len()..];
        assert_eq!(suffix.trim_start(), original);

        // Tasklist summary: id, title, status, tasks with statuses.
        assert!(injected.contains("id: `tl-15`"));
        assert!(injected.contains("title: Ship Feature X"));
        assert!(injected.contains("`task-1` [completed]"));
        assert!(injected.contains("`task-2` [in_progress]"));

        // Changelog tail: both entries surfaced, both in chronological order.
        assert!(injected.contains("wrote schema doc"));
        assert!(injected.contains("drafted handler"));
        assert!(
            injected.find("wrote schema doc").unwrap()
                < injected.find("drafted handler").unwrap()
        );
    }

    /// The injection path must NOT fire for an agent whose template
    /// isn't the co-pilot one, even if the agent_id happens to coincide
    /// with a co-pilot binding (defensive: never inject into non-co-pilot
    /// agents because they don't have the prompt context to read it).
    #[tokio::test]
    async fn inject_does_not_fire_for_non_copilot_template() {
        let (persistence, _tmp) = make_persistence().await;

        let tl = sample_tasklist("team-15", "tl-15");
        persistence.tasklists.create(&tl).await.unwrap();
        persistence
            .tasklists
            .bind_copilot_agent_id("team-15", "tl-15", "copilot-x")
            .await
            .unwrap();

        // Same agent_id as the binding, but profile template is something else.
        let original = "hello";
        let out = inject_copilot_context(&persistence, Some("plain-agent"), "copilot-x", original)
            .await;
        assert_eq!(out, original);

        // No template at all (e.g. user-authored agent without a template
        // marker) is also a no-op.
        let out = inject_copilot_context(&persistence, None, "copilot-x", original).await;
        assert_eq!(out, original);
    }

    /// Co-pilot template but the agent isn't bound to any tasklist (e.g.
    /// stale agent or test fixture) → pass-through.
    #[tokio::test]
    async fn inject_passes_through_when_agent_has_no_binding() {
        let (persistence, _tmp) = make_persistence().await;

        let original = "hello";
        let out = inject_copilot_context(
            &persistence,
            Some(COPILOT_PROFILE_ID),
            "unbound-copilot",
            original,
        )
        .await;
        assert_eq!(out, original);
    }

    /// Both user-typed messages and inbound `<task-item-notification>`
    /// QueuedMessages flow through the same `pump()` chokepoint, so this
    /// test pins that the helper is content-agnostic: an XML-shaped
    /// notification body gets the same prepend treatment as plain text.
    #[tokio::test]
    async fn inject_prepends_block_for_task_item_notification_payload() {
        let (persistence, _tmp) = make_persistence().await;
        let tl = sample_tasklist("team-15", "tl-15");
        persistence.tasklists.create(&tl).await.unwrap();
        persistence
            .tasklists
            .bind_copilot_agent_id("team-15", "tl-15", "copilot-x")
            .await
            .unwrap();

        let xml = "<task-item-notification>\n  <status>complete</status>\n  <summary>did the thing</summary>\n</task-item-notification>";
        let injected = inject_copilot_context(
            &persistence,
            Some(COPILOT_PROFILE_ID),
            "copilot-x",
            xml,
        )
        .await;

        // The original notification body is preserved verbatim after the
        // injected block — the FE's wire format is untouched.
        assert!(injected.contains("<copilot-context>"));
        assert!(injected.ends_with(xml));
    }

    #[test]
    fn build_block_truncates_long_prompts_and_collapses_newlines() {
        let mut tl = sample_tasklist("team-a", "tl-1");
        // Multi-line prompt with a 300-char body: must collapse to one
        // line and end with an ellipsis since 300 > 200.
        tl.groups[0].tasks[0].prompt =
            format!("first line\nsecond line {}", "x".repeat(300));
        let block = build_copilot_context_block(&tl, &[]);

        // No literal newline appears inside the per-task line (verify by
        // checking the substring up to the ellipsis sits on one line).
        let needle = "`task-1` [completed]";
        let start = block.find(needle).expect("task-1 line should appear");
        let line_end = block[start..].find('\n').expect("line should terminate");
        let line = &block[start..start + line_end];
        assert!(!line.contains('\n'));
        assert!(line.ends_with('…'), "long prompt should be truncated: {line}");
    }
}
