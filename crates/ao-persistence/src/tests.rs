//! Unit tests for the `ao-persistence` crate root.
//!
//! Declared from `lib.rs` as `#[cfg(test)] mod tests;` — `tests.rs` is the
//! same module as the inline `mod tests` block it replaces, so private items
//! of the crate root remain in scope here via `use super::*`.

use super::*;
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::Utc;
use std::collections::HashMap;

fn make_test_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Test Agent {}", id),
        description: "A test agent".to_string(),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            normalizer: None,
            output_format: OutputFormat::Text,
            input_mode: InputMode::Arg,
            model_arg: None,
            model_aliases: HashMap::new(),
            system_prompt_arg: None,
            session_arg: None,
            resume_args: vec![],
            session_id_fields: vec![],
            clear_env: false,
            no_output_timeout_ms: 30000,
            file_capabilities: None,
        }),
        model: None,
        skills: vec![],
        system_prompt: None,
        tools: None,
        env: HashMap::new(),
        max_instances: 1,
        timeout_seconds: 300,
        working_dir: None,
        home_dir: None,
        serialize: true,
        workflows: None,
        template: None,
        runner_mode: Default::default(),
        native_provider: None,
        thinking: None,
        max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        enabled_plugins: HashMap::new(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        owning_team_id: None,
        delegates_to: vec![],
        persona: None,
        special_instructions: None,
        legacy_system_prompt: None,
        max_delegation_depth: None,
        channels: vec![],
        max_turns: None,
    }
}

fn make_transcript_entry(content: &str) -> TranscriptEntry {
    TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("user".to_string()),
        content: content.to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    }
}

fn setup_temp_data_root() -> (tempfile::TempDir, paths::DataRoot) {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    let data_root = paths::DataRoot::new(tmp.path());
    (tmp, data_root)
}

// --- PersistenceLayer::init_with_root: default threads are lazy, not eager ---

/// Boot no longer pre-creates a default-thread row for every agent on disk.
/// This pins both halves of that: nothing is written at init, and the first
/// read materializes a row aliasing the transcript the agent already had —
/// byte-for-byte, so the lazy path is observationally identical to the eager
/// pass it replaced.
#[tokio::test]
async fn init_with_root_leaves_default_threads_to_the_first_read() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();

    let profile = make_test_profile("pre-existing-agent");
    AgentProfileStore::new(data_root.clone()).create(&profile).await.unwrap();

    // The agent already has transcript bytes from before this boot.
    let transcript_path = data_root.agent_transcript_path("pre-existing-agent");
    let entry = TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::System("user".to_string()),
        content: "written before boot".to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    let line = serde_json::to_string(&entry).unwrap();
    tokio::fs::write(&transcript_path, format!("{}\n", line)).await.unwrap();
    let bytes_before = tokio::fs::read(&transcript_path).await.unwrap();

    let layer = PersistenceLayer::init_with_root(data_root.clone()).await.unwrap();

    // No eager pass: booting alone must not have materialized the row.
    let default_id = thread_store::ThreadStore::default_thread_id("pre-existing-agent");
    assert!(
        layer.threads.get(&default_id).await.unwrap().is_none(),
        "init must not pre-create default-thread rows"
    );

    // The first read materializes it, and it aliases the pre-existing file.
    let listed = layer.threads.list_for_agent("pre-existing-agent").await.unwrap();
    let default = listed
        .iter()
        .find(|t| t.kind == ao_protocol::thread::ThreadKind::Default)
        .expect("first read must materialize the default thread");
    assert_eq!(default.id, default_id);
    assert_eq!(default.transcript_path, transcript_path.to_string_lossy());

    // Materializing it touched no transcript bytes.
    assert_eq!(bytes_before, tokio::fs::read(&transcript_path).await.unwrap());
}

// --- PersistenceLayer::init_with_root: Slack channel_origin backfill ---

#[tokio::test]
async fn init_with_root_backfills_channel_origin_for_pre_existing_slack_threads() {
    use ao_protocol::agent::{ChannelBinding, ChannelKindConfig, SlackConversationMode};
    use ao_protocol::slack_conversation_registry::SlackConversationRow;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();

    // Seed an agent with an enabled Slack binding, in the shape a real
    // profile had before this fix existed: `bridge_thread_id` is `None`
    // because Slack's runtime path never populates it.
    let mut profile = make_test_profile("slack-agent");
    profile.channels.push(ChannelBinding {
        binding_id: "slack-1".to_string(),
        kind: ChannelKind::Slack,
        enabled: true,
        bridge_thread_id: None,
        allowed_senders: vec![],
        pending_pairing_code: None,
        kind_config: ChannelKindConfig::Slack {
            allowed_channels: vec![],
            allowed_users: vec![],
            connection_id: None,
            conversation_mode: SlackConversationMode::PerConversation,
        },
    });
    AgentProfileStore::new(data_root.clone()).create(&profile).await.unwrap();

    // Seed a thread that pre-dates `channel_origin` — exactly what a
    // real Slack per-conversation bridge thread looked like before this
    // fix shipped.
    let threads = ThreadStore::load(data_root.clone()).await.unwrap();
    let thread = threads.build_fresh_thread("slack-agent", Some("💬 Slack — C1".to_string()));
    let thread_id = thread.id.clone();
    assert!(thread.channel_origin.is_none());
    threads.create(thread).await.unwrap();

    // Seed the conversation-registry row a real inbound Slack message
    // would have written, pointing at that same thread.
    let slack_conversations = SlackConversationRegistryStore::new(data_root.clone());
    let now = Utc::now();
    slack_conversations
        .set(
            "T1",
            "C1",
            None,
            &SlackConversationRow {
                agent_id: "slack-agent".to_string(),
                thread_id: thread_id.clone(),
                created_at: now,
                last_seen_at: now,
            },
            now,
        )
        .await
        .unwrap();

    // A fresh boot (as if the server restarted after this fix shipped)
    // must backfill `channel_origin` onto that pre-existing thread.
    let layer = PersistenceLayer::init_with_root(data_root).await.unwrap();
    let backfilled = layer.threads.get(&thread_id).await.unwrap().expect("thread must still exist");
    let origin = backfilled.channel_origin.expect("channel_origin must be backfilled on boot");
    assert_eq!(origin.kind, ChannelKind::Slack);
    assert_eq!(origin.binding_id, "slack-1");
}

#[tokio::test]
async fn init_with_root_backfill_skips_a_row_whose_agent_has_no_slack_binding() {
    use ao_protocol::slack_conversation_registry::SlackConversationRow;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();

    // No agent profile is created at all — the row's `agent_id` is
    // simply unresolvable. Must not fail startup.
    let threads = ThreadStore::load(data_root.clone()).await.unwrap();
    let thread = threads.build_fresh_thread("ghost-agent", None);
    let thread_id = thread.id.clone();
    threads.create(thread).await.unwrap();

    let slack_conversations = SlackConversationRegistryStore::new(data_root.clone());
    let now = Utc::now();
    slack_conversations
        .set(
            "T1",
            "C1",
            None,
            &SlackConversationRow {
                agent_id: "ghost-agent".to_string(),
                thread_id: thread_id.clone(),
                created_at: now,
                last_seen_at: now,
            },
            now,
        )
        .await
        .unwrap();

    let layer = PersistenceLayer::init_with_root(data_root).await.unwrap();
    let thread = layer.threads.get(&thread_id).await.unwrap().expect("thread must still exist");
    assert!(thread.channel_origin.is_none(), "an unresolvable row must be skipped, not guessed at");
}

// --- PersistenceLayer::init_with_root: assignment_origin backfill ---

fn cron_assignment_row(
    id: &str,
    agent_id: &str,
    thread_policy: AssignmentThreadPolicy,
) -> ao_protocol::assignment::Assignment {
    let now = Utc::now();
    ao_protocol::assignment::Assignment {
        id: id.to_string(),
        agent_id: agent_id.to_string(),
        name: "Daily brief".to_string(),
        instruction: "Summarize today.".to_string(),
        working_directory: None,
        trigger: ao_protocol::assignment::AssignmentTrigger::Cron {
            cron_expr: "0 8 * * *".to_string(),
            is_recurring: true,
        },
        bindings: vec![],
        output_mode: ao_protocol::assignment::OutputMode::Background,
        thread_policy,
        dedicated_thread_id: None,
        enabled: true,
        expires_at: None,
        next_fire_at: None,
        last_run_at: None,
        last_event_cursor: None,
        liveness: ao_protocol::assignment::LivenessState::default(),
        created_ts: now,
        updated_ts: now,
    }
}

fn assignment_run_row(id: &str, assignment_id: &str, agent_id: &str, thread_id: &str) -> ao_protocol::assignment::AssignmentRun {
    ao_protocol::assignment::AssignmentRun {
        id: id.to_string(),
        assignment_id: assignment_id.to_string(),
        agent_id: agent_id.to_string(),
        trigger_kind: ao_protocol::assignment::AssignmentTriggerKind::Cron,
        trigger_payload: None,
        status: ao_protocol::assignment::AssignmentRunStatus::Succeeded,
        output_summary: None,
        thread_id: Some(thread_id.to_string()),
        queued_at: Utc::now(),
        started_ts: None,
        finished_ts: None,
        error: None,
    }
}

#[tokio::test]
async fn init_with_root_backfills_assignment_origin_for_a_pre_existing_fresh_run_thread() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();

    let profile = make_test_profile("fresh-agent");
    AgentProfileStore::new(data_root.clone()).create(&profile).await.unwrap();

    // A run's own thread, created before `assignment_origin` existed —
    // exactly what a real `Fresh`-policy run's thread looked like.
    let threads = ThreadStore::load(data_root.clone()).await.unwrap();
    let thread = threads.build_fresh_thread("fresh-agent", Some("Daily brief — run".to_string()));
    let thread_id = thread.id.clone();
    threads.create(thread).await.unwrap();

    let assignments = AssignmentStore::load(data_root.clone()).await.unwrap();
    assignments
        .add(cron_assignment_row("assign-fresh", "fresh-agent", AssignmentThreadPolicy::Fresh))
        .await
        .unwrap();
    let assignment_runs = AssignmentRunStore::new(data_root.clone());
    assignment_runs
        .append("assign-fresh", &assignment_run_row("run-1", "assign-fresh", "fresh-agent", &thread_id))
        .await
        .unwrap();

    let layer = PersistenceLayer::init_with_root(data_root).await.unwrap();
    let backfilled = layer.threads.get(&thread_id).await.unwrap().expect("thread must still exist");
    let origin = backfilled.assignment_origin.expect("assignment_origin must be backfilled on boot");
    assert_eq!(origin.assignment_id, "assign-fresh");
    assert_eq!(origin.run_id.as_deref(), Some("run-1"));
}

#[tokio::test]
async fn init_with_root_backfill_never_marks_the_default_thread_even_if_policy_later_changed_to_fresh() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();

    let profile = make_test_profile("switched-agent");
    AgentProfileStore::new(data_root.clone()).create(&profile).await.unwrap();
    let threads = ThreadStore::load(data_root.clone()).await.unwrap();
    let default_thread = threads.ensure_default_thread("switched-agent").await.unwrap();

    // Simulates a stale `Main`-era row: the assignment's policy is
    // `Fresh` today, but this particular run fired back when it was
    // `Main`, so its `thread_id` — recorded on the run at fire time —
    // is the agent's own shared default thread, not a run-owned one.
    let assignments = AssignmentStore::load(data_root.clone()).await.unwrap();
    assignments
        .add(cron_assignment_row("assign-switched", "switched-agent", AssignmentThreadPolicy::Fresh))
        .await
        .unwrap();
    let assignment_runs = AssignmentRunStore::new(data_root.clone());
    assignment_runs
        .append(
            "assign-switched",
            &assignment_run_row("run-old-main", "assign-switched", "switched-agent", &default_thread.id),
        )
        .await
        .unwrap();

    let layer = PersistenceLayer::init_with_root(data_root).await.unwrap();
    let thread = layer.threads.get(&default_thread.id).await.unwrap().expect("default thread must still exist");
    assert!(
        thread.assignment_origin.is_none(),
        "a leftover Main-era run must never cause the shared default thread to be marked assignment-owned"
    );
}

// --- DataRoot / paths tests ---

#[tokio::test]
async fn test_ensure_directories_creates_all_dirs() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();

    assert!(tokio::fs::metadata(data_root.agents_dir()).await.is_ok());
    assert!(tokio::fs::metadata(data_root.messages_metadata_dir()).await.is_ok());
    assert!(tokio::fs::metadata(data_root.messages_data_dir()).await.is_ok());
    assert!(
        tokio::fs::metadata(data_root.messages_data_dir().join("tasks"))
            .await
            .is_ok()
    );
    assert!(tokio::fs::metadata(data_root.memory_dir()).await.is_ok());
}

#[tokio::test]
async fn test_ensure_directories_idempotent() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    // Call again — should not error
    data_root.ensure_directories().await.unwrap();
}

#[test]
fn test_path_helpers() {
    let (_tmp, data_root) = setup_temp_data_root();
    let root = data_root.root().clone();
    assert_eq!(data_root.agents_dir(), root.join("agents"));
    assert_eq!(
        data_root.messages_metadata_dir(),
        root.join("messages").join("metadata")
    );
    assert_eq!(
        data_root.messages_data_dir(),
        root.join("messages").join("data")
    );
    assert_eq!(
        data_root.agent_transcript_path("my-agent"),
        root.join("messages").join("data").join("my-agent.jsonl")
    );
    assert_eq!(
        data_root.snapshot_path(),
        root.join("messages").join("metadata").join("snapshot.json")
    );
    assert_eq!(data_root.memory_dir(), root.join("memory"));
    assert_eq!(
        data_root.memory_agent_path("my-agent"),
        root.join("memory").join("agents").join("my-agent.jsonl")
    );
    assert_eq!(
        data_root.memory_global_path(),
        root.join("memory").join("global.jsonl")
    );
}

// --- AgentProfileStore tests ---

#[tokio::test]
async fn test_create_then_read_back() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    let profile = make_test_profile("agent-1");
    store.create(&profile).await.unwrap();

    let loaded = store.get("agent-1").await.unwrap().expect("Should find agent");
    assert_eq!(loaded.id, "agent-1");
    assert_eq!(loaded.name, "Test Agent agent-1");
    assert_eq!(loaded, profile);
}

#[tokio::test]
async fn test_create_three_then_list() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    for id in &["a1", "a2", "a3"] {
        store.create(&make_test_profile(id)).await.unwrap();
    }

    let all = store.list().await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn test_create_then_delete_then_get_returns_none() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    store.create(&make_test_profile("del-me")).await.unwrap();
    let deleted = store.delete("del-me").await.unwrap();
    assert!(deleted);

    let found = store.get("del-me").await.unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn test_delete_nonexistent_returns_false() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    let deleted = store.delete("no-such-agent").await.unwrap();
    assert!(!deleted);
}

#[tokio::test]
async fn test_create_duplicate_fails() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    store.create(&make_test_profile("dup")).await.unwrap();
    let err = store.create(&make_test_profile("dup")).await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::AgentAlreadyExists(_))
    ));
}

#[tokio::test]
async fn test_validate_id_rejects_invalid() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    // Spaces are invalid
    let mut profile = make_test_profile("ok");
    profile.id = "has spaces".to_string();
    assert!(store.create(&profile).await.is_err());

    // Dots are invalid
    profile.id = "has.dots".to_string();
    assert!(store.create(&profile).await.is_err());

    // Slashes are invalid
    profile.id = "has/slash".to_string();
    assert!(store.create(&profile).await.is_err());

    // Empty is invalid
    profile.id = "".to_string();
    assert!(store.create(&profile).await.is_err());
}

#[tokio::test]
async fn test_update_existing() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    let mut profile = make_test_profile("upd");
    store.create(&profile).await.unwrap();

    profile.name = "Updated Name".to_string();
    store.update(&profile).await.unwrap();

    let loaded = store.get("upd").await.unwrap().unwrap();
    assert_eq!(loaded.name, "Updated Name");
}

#[tokio::test]
async fn test_update_nonexistent_fails() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    let err = store.update(&make_test_profile("ghost")).await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::AgentNotFound(_))
    ));
}

#[tokio::test]
async fn test_get_nonexistent_returns_none() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    let result = store.get("nonexistent").await.unwrap();
    assert!(result.is_none());
}

// --- clone_agent_profile tests ---

#[tokio::test]
async fn test_clone_agent_profile_creates_distinct_row() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    let mut parent = make_test_profile("parent-agent");
    parent.system_prompt = Some("you are a careful assistant".to_string());
    parent.model = Some("opus".to_string());
    parent.skills = vec!["code-review".to_string(), "docs".to_string()];
    parent
        .env
        .insert("FOO".to_string(), "bar".to_string());
    parent.max_instances = 7;
    parent.timeout_seconds = 999;
    store.create(&parent).await.unwrap();

    let (new_id, clone) = store.clone_agent_profile("parent-agent").await.unwrap();

    // Fresh, parseable UUID id distinct from the parent's
    assert_ne!(new_id, parent.id);
    assert_eq!(clone.id, new_id);
    assert!(uuid::Uuid::parse_str(&new_id).is_ok());

    // Name is suffixed with " - copy"
    assert_eq!(clone.name, format!("{} - copy", parent.name));

    // Clone is assigned a fresh random emoji, not the parent's.
    assert!(clone.emoji.is_some());

    // All other profile fields copied verbatim
    assert_eq!(clone.description, parent.description);
    assert_eq!(clone.provider, parent.provider);
    assert_eq!(clone.model, parent.model);
    assert_eq!(clone.skills, parent.skills);
    assert_eq!(clone.system_prompt, parent.system_prompt);
    assert_eq!(clone.tools, parent.tools);
    assert_eq!(clone.env, parent.env);
    assert_eq!(clone.max_instances, parent.max_instances);
    assert_eq!(clone.timeout_seconds, parent.timeout_seconds);
    assert_eq!(clone.working_dir, parent.working_dir);
    assert_eq!(clone.home_dir, parent.home_dir);
    assert_eq!(clone.serialize, parent.serialize);
    assert_eq!(clone.workflows, parent.workflows);

    // Persisted: round-trip read returns the same clone profile
    let loaded = store.get(&new_id).await.unwrap().expect("clone row exists");
    assert_eq!(loaded, clone);

    // Parent row is unmodified
    let parent_loaded = store.get("parent-agent").await.unwrap().unwrap();
    assert_eq!(parent_loaded, parent);
}

#[tokio::test]
async fn test_clone_agent_profile_missing_parent_returns_not_found() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    let err = store.clone_agent_profile("ghost-parent").await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::AgentNotFound(_))
    ));
}

// --- clone_agent_home tests ---

#[tokio::test]
async fn test_clone_agent_home_default_copies_contents_isolated() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root.clone());

    // Seed the parent's default home with nested content.
    let parent = make_test_profile("parent-home");
    let parent_home = data_root.agent_home_dir(&parent.id);
    tokio::fs::create_dir_all(parent_home.join("skills")).await.unwrap();
    tokio::fs::create_dir_all(parent_home.join("memory/nested")).await.unwrap();
    tokio::fs::write(parent_home.join("CLAUDE.md"), b"parent rules")
        .await
        .unwrap();
    tokio::fs::write(parent_home.join("skills/code-review.md"), b"original skill")
        .await
        .unwrap();
    tokio::fs::write(parent_home.join("memory/nested/note.txt"), b"a memory")
        .await
        .unwrap();

    let cloned = store
        .clone_agent_home(&parent, "child-home")
        .await
        .unwrap();

    let child_home = data_root.agent_home_dir("child-home");
    assert!(matches!(
        &cloned,
        profiles::ClonedHome::NewDefault(p) if p == &child_home
    ));
    assert_eq!(cloned.path(), child_home.as_path());

    // All files copied.
    assert_eq!(
        tokio::fs::read_to_string(child_home.join("CLAUDE.md"))
            .await
            .unwrap(),
        "parent rules"
    );
    assert_eq!(
        tokio::fs::read_to_string(child_home.join("skills/code-review.md"))
            .await
            .unwrap(),
        "original skill"
    );
    assert_eq!(
        tokio::fs::read_to_string(child_home.join("memory/nested/note.txt"))
            .await
            .unwrap(),
        "a memory"
    );

    // Mutating the clone's copy must not affect the parent.
    tokio::fs::write(
        child_home.join("skills/code-review.md"),
        b"edited in clone only",
    )
    .await
    .unwrap();
    assert_eq!(
        tokio::fs::read_to_string(parent_home.join("skills/code-review.md"))
            .await
            .unwrap(),
        "original skill"
    );
}

#[tokio::test]
async fn test_clone_agent_home_default_with_no_existing_home_scaffolds_empty() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root.clone());

    // Parent has default home but nothing was ever written to disk for it.
    let parent = make_test_profile("parent-empty");

    let cloned = store
        .clone_agent_home(&parent, "child-empty")
        .await
        .unwrap();

    let child_home = data_root.agent_home_dir("child-empty");
    assert!(matches!(&cloned, profiles::ClonedHome::NewDefault(p) if p == &child_home));
    // The scaffolded dirs still exist so subsequent code can rely on them.
    assert!(child_home.join("skills").is_dir());
    assert!(child_home.join("rules").is_dir());
}

#[tokio::test]
async fn test_clone_agent_home_custom_path_is_reused_without_copying() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root.clone());

    // Custom home directory lives outside the managed agent_homes tree.
    let custom_tmp = tempfile::tempdir().unwrap();
    let custom_home = custom_tmp.path().to_path_buf();
    tokio::fs::write(custom_home.join("marker.txt"), b"shared")
        .await
        .unwrap();

    let mut parent = make_test_profile("parent-custom");
    parent.home_dir = Some(custom_home.to_string_lossy().into_owned());

    let cloned = store
        .clone_agent_home(&parent, "child-custom")
        .await
        .unwrap();

    assert!(matches!(
        &cloned,
        profiles::ClonedHome::SharedCustom(p) if p == &custom_home
    ));

    // No new directory was created for the clone under the managed root.
    let managed_child = data_root.agent_home_dir("child-custom");
    assert!(
        !managed_child.exists(),
        "custom-home clones must not materialize a managed default dir"
    );

    // Parent's custom home is untouched.
    assert_eq!(
        tokio::fs::read_to_string(custom_home.join("marker.txt"))
            .await
            .unwrap(),
        "shared"
    );
}

#[tokio::test]
async fn test_clone_agent_home_default_when_home_dir_equals_managed_path() {
    // Even if home_dir is set explicitly to the managed default, it should
    // be treated as a default home and copied (not reused).
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root.clone());

    let mut parent = make_test_profile("parent-explicit");
    let parent_home = data_root.agent_home_dir(&parent.id);
    parent.home_dir = Some(parent_home.to_string_lossy().into_owned());
    tokio::fs::create_dir_all(&parent_home).await.unwrap();
    tokio::fs::write(parent_home.join("marker.txt"), b"explicit")
        .await
        .unwrap();

    let cloned = store
        .clone_agent_home(&parent, "child-explicit")
        .await
        .unwrap();

    let child_home = data_root.agent_home_dir("child-explicit");
    assert!(matches!(
        &cloned,
        profiles::ClonedHome::NewDefault(p) if p == &child_home
    ));
    assert_eq!(
        tokio::fs::read_to_string(child_home.join("marker.txt"))
            .await
            .unwrap(),
        "explicit"
    );
}

#[tokio::test]
async fn test_clone_agent_home_unwritable_target_surfaces_error() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root.clone());

    let parent = make_test_profile("parent-fail");
    let parent_home = data_root.agent_home_dir(&parent.id);
    tokio::fs::create_dir_all(&parent_home).await.unwrap();
    tokio::fs::write(parent_home.join("note.txt"), b"hello")
        .await
        .unwrap();

    // Pre-create the clone's intended home path as a regular file so
    // create_dir_all fails with NotADirectory / AlreadyExists.
    let new_id = "child-fail";
    let new_home = data_root.agent_home_dir(new_id);
    tokio::fs::create_dir_all(data_root.agent_homes_dir())
        .await
        .unwrap();
    tokio::fs::write(&new_home, b"not a directory").await.unwrap();

    let err = store
        .clone_agent_home(&parent, new_id)
        .await
        .expect_err("copy should fail when target path is a file");
    assert!(
        matches!(err, ao_protocol::error::AoError::Io(_)),
        "expected Io error, got {err:?}"
    );
}

// --- clone_agent orchestrator tests ---

#[tokio::test]
async fn test_clone_agent_default_home_happy_path() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root.clone());

    let mut parent = make_test_profile("parent-orchestrate");
    parent.system_prompt = Some("be careful".to_string());
    store.create(&parent).await.unwrap();

    let parent_home = data_root.agent_home_dir(&parent.id);
    tokio::fs::create_dir_all(parent_home.join("skills"))
        .await
        .unwrap();
    tokio::fs::write(parent_home.join("skills/review.md"), b"parent content")
        .await
        .unwrap();

    let clone = store.clone_agent(&parent.id).await.unwrap();

    // New id, expected name, other fields preserved.
    assert_ne!(clone.id, parent.id);
    assert!(uuid::Uuid::parse_str(&clone.id).is_ok());
    assert_eq!(clone.name, format!("{} - copy", parent.name));
    assert_eq!(clone.system_prompt, parent.system_prompt);

    // Default-home clones leave home_dir = None so the clone keeps
    // resolving against the managed default directory.
    assert_eq!(clone.home_dir, None);

    // Persisted row matches the returned profile.
    let loaded = store.get(&clone.id).await.unwrap().expect("clone exists");
    assert_eq!(loaded, clone);

    // Home directory was copied and isolated from parent.
    let expected_home = data_root.agent_home_dir(&clone.id);
    assert_eq!(
        tokio::fs::read_to_string(expected_home.join("skills/review.md"))
            .await
            .unwrap(),
        "parent content"
    );
    tokio::fs::write(expected_home.join("skills/review.md"), b"edited child")
        .await
        .unwrap();
    assert_eq!(
        tokio::fs::read_to_string(parent_home.join("skills/review.md"))
            .await
            .unwrap(),
        "parent content"
    );

    // Parent row is unchanged.
    let parent_loaded = store.get(&parent.id).await.unwrap().unwrap();
    assert_eq!(parent_loaded, parent);
}

#[tokio::test]
async fn test_clone_agent_custom_home_shares_parent_path() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root.clone());

    let custom_tmp = tempfile::tempdir().unwrap();
    let custom_home = custom_tmp.path().to_path_buf();

    let mut parent = make_test_profile("parent-custom-orch");
    parent.home_dir = Some(custom_home.to_string_lossy().into_owned());
    store.create(&parent).await.unwrap();

    let clone = store.clone_agent(&parent.id).await.unwrap();

    assert_eq!(
        clone.home_dir.as_deref(),
        Some(custom_home.to_string_lossy().as_ref())
    );

    // No managed default directory was created for the clone.
    assert!(!data_root.agent_home_dir(&clone.id).exists());

    let loaded = store.get(&clone.id).await.unwrap().unwrap();
    assert_eq!(loaded.home_dir, clone.home_dir);
}

#[tokio::test]
async fn test_clone_agent_missing_parent_returns_not_found() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root);

    let err = store.clone_agent("ghost-parent").await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::AgentNotFound(_))
    ));
}

#[tokio::test]
async fn test_clone_agent_home_failure_rolls_back_profile_row() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = profiles::AgentProfileStore::new(data_root.clone());

    let parent = make_test_profile("parent-rollback");
    store.create(&parent).await.unwrap();
    let parent_snapshot = store.get(&parent.id).await.unwrap().unwrap();

    // Replace the agent_homes directory with a regular file so ensure_agent_home
    // fails for any new agent id: create_dir_all cannot create a subpath of a file.
    let homes = data_root.agent_homes_dir();
    tokio::fs::remove_dir_all(&homes).await.unwrap();
    tokio::fs::write(&homes, b"not a directory").await.unwrap();

    let before: Vec<String> = store
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();

    let err = store
        .clone_agent(&parent.id)
        .await
        .expect_err("clone must fail when agent_homes_dir is unusable");
    assert!(
        matches!(err, ao_protocol::error::AoError::Io(_)),
        "expected Io error, got {err:?}"
    );

    // No new agent row was left behind (parent still the only row).
    let after: Vec<String> = store
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(after, before);

    // Parent is completely unchanged.
    let parent_after = store.get(&parent.id).await.unwrap().unwrap();
    assert_eq!(parent_after, parent_snapshot);
}

// --- TranscriptStore tests ---

#[tokio::test]
async fn test_transcript_append_and_read_all() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    for i in 0..5 {
        let entry = make_transcript_entry(&format!("message {}", i));
        store.append("test-agent", &entry).await.unwrap();
    }

    let entries = store.read_all("test-agent").await.unwrap();
    assert_eq!(entries.len(), 5);
    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(entry.content, format!("message {}", i));
    }
}

#[tokio::test]
async fn test_transcript_append_creates_file() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    // File does not exist yet — append should auto-create
    let entry = make_transcript_entry("first message");
    store.append("new-agent", &entry).await.unwrap();

    let entries = store.read_all("new-agent").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content, "first message");
}

#[tokio::test]
async fn test_transcript_read_nonexistent_returns_empty() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    let entries = store.read_all("no-such-agent").await.unwrap();
    assert!(entries.is_empty());
}

#[tokio::test]
async fn test_transcript_read_recent() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    // Write 10 entries
    for i in 0..10 {
        let entry = make_transcript_entry(&format!("message {}", i));
        store.append("recent-agent", &entry).await.unwrap();
    }

    // read_recent(3) should return the last 3 entries (messages 7, 8, 9)
    let recent = store.read_recent("recent-agent", 3).await.unwrap();
    assert_eq!(recent.len(), 3);
    assert_eq!(recent[0].content, "message 7");
    assert_eq!(recent[1].content, "message 8");
    assert_eq!(recent[2].content, "message 9");

    // read_recent with n > total returns all entries
    let all = store.read_recent("recent-agent", 100).await.unwrap();
    assert_eq!(all.len(), 10);

    // read_recent on nonexistent agent returns empty vec
    let empty = store.read_recent("no-such-agent", 5).await.unwrap();
    assert!(empty.is_empty());
}

// --- TranscriptStore::search tests ---

#[tokio::test]
async fn test_transcript_search_matching_results() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    let messages = ["hello world", "foo bar", "hello again", "baz qux", "hello final"];
    for msg in &messages {
        store.append("search-agent", &make_transcript_entry(msg)).await.unwrap();
    }

    let results = store.search("search-agent", "hello", 10).await.unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].content, "hello world");
    assert_eq!(results[1].content, "hello again");
    assert_eq!(results[2].content, "hello final");
}

#[tokio::test]
async fn test_transcript_search_case_insensitive() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    store.append("ci-agent", &make_transcript_entry("Hello World")).await.unwrap();
    store.append("ci-agent", &make_transcript_entry("HELLO AGAIN")).await.unwrap();
    store.append("ci-agent", &make_transcript_entry("no match")).await.unwrap();

    let results = store.search("ci-agent", "hello", 10).await.unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].content, "Hello World");
    assert_eq!(results[1].content, "HELLO AGAIN");
}

#[tokio::test]
async fn test_transcript_search_no_matches_returns_empty() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    store.append("no-match-agent", &make_transcript_entry("foo")).await.unwrap();
    store.append("no-match-agent", &make_transcript_entry("bar")).await.unwrap();

    let results = store.search("no-match-agent", "xyz", 10).await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_transcript_search_empty_query_returns_last_n() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    for i in 0..10 {
        store.append("empty-q-agent", &make_transcript_entry(&format!("msg {}", i))).await.unwrap();
    }

    let results = store.search("empty-q-agent", "", 3).await.unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].content, "msg 7");
    assert_eq!(results[1].content, "msg 8");
    assert_eq!(results[2].content, "msg 9");
}

#[tokio::test]
async fn test_transcript_search_limit_is_respected() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    for i in 0..10 {
        store.append("limit-agent", &make_transcript_entry(&format!("hello {}", i))).await.unwrap();
    }

    // All 10 match "hello", but limit to 3 -> last 3
    let results = store.search("limit-agent", "hello", 3).await.unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].content, "hello 7");
    assert_eq!(results[1].content, "hello 8");
    assert_eq!(results[2].content, "hello 9");
}

#[tokio::test]
async fn test_transcript_search_nonexistent_agent_returns_empty() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    let results = store.search("ghost-agent", "hello", 10).await.unwrap();
    assert!(results.is_empty());
}

// --- SnapshotStore tests ---

#[tokio::test]
async fn test_snapshot_update_then_get() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = snapshot::SnapshotStore::load(data_root).await.unwrap();

    store
        .update_agent_entry("agent-1", |entry| {
            entry.name = "Agent One".to_string();
            entry.message_count = 42;
        })
        .await
        .unwrap();

    let snap = store.get().await;
    let agent = snap.agents.get("agent-1").unwrap();
    assert_eq!(agent.name, "Agent One");
    assert_eq!(agent.message_count, 42);
}

#[tokio::test]
async fn test_snapshot_remove_agent() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = snapshot::SnapshotStore::load(data_root).await.unwrap();

    store
        .update_agent_entry("agent-rm", |entry| {
            entry.name = "To Remove".to_string();
        })
        .await
        .unwrap();

    store.remove_agent_entry("agent-rm").await.unwrap();

    let snap = store.get().await;
    assert!(snap.agents.get("agent-rm").is_none());
}

#[tokio::test]
async fn test_snapshot_survives_restart() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();

    // Create and populate a snapshot store
    {
        let store = snapshot::SnapshotStore::load(data_root.clone()).await.unwrap();
        store
            .update_agent_entry("persist-agent", |entry| {
                entry.name = "Persistent".to_string();
                entry.message_count = 10;
            })
            .await
            .unwrap();
    }

    // Create a new store from the same path — should load persisted data
    let store2 = snapshot::SnapshotStore::load(data_root).await.unwrap();
    let snap = store2.get().await;
    let agent = snap.agents.get("persist-agent").unwrap();
    assert_eq!(agent.name, "Persistent");
    assert_eq!(agent.message_count, 10);
}

// --- PersistenceLayer tests ---

#[tokio::test]
async fn test_persistence_layer_init_creates_directories() {
    let (_tmp, data_root) = setup_temp_data_root();
    let layer = PersistenceLayer::init_with_root(data_root).await.unwrap();

    assert!(tokio::fs::metadata(layer.data_root.agents_dir()).await.is_ok());
    assert!(tokio::fs::metadata(layer.data_root.messages_metadata_dir()).await.is_ok());
    assert!(tokio::fs::metadata(layer.data_root.messages_data_dir()).await.is_ok());
}

// --- MemoryStore tests ---

#[tokio::test]
async fn test_memory_list_empty_for_nonexistent_file() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    let entries = store.list("no-such-agent").await.unwrap();
    assert!(entries.is_empty());
}

#[tokio::test]
async fn test_memory_add_creates_file_and_round_trips() {
    use ao_protocol::memory::MemorySource;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    let entry = store
        .add("agent-1", "user prefers dark mode", MemorySource::Manual)
        .await
        .unwrap();
    assert_eq!(entry.content, "user prefers dark mode");
    assert_eq!(entry.source, Some(MemorySource::Manual));
    assert!(!entry.id.is_empty());

    let entries = store.list("agent-1").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, entry.id);
    assert_eq!(entries[0].content, "user prefers dark mode");
}

#[tokio::test]
async fn test_memory_add_multiple_entries() {
    use ao_protocol::memory::MemorySource;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    store
        .add("agent-1", "memory one", MemorySource::Manual)
        .await
        .unwrap();
    store
        .add("agent-1", "memory two", MemorySource::Agent)
        .await
        .unwrap();
    store
        .add("agent-1", "memory three", MemorySource::GlobalPromotion)
        .await
        .unwrap();

    let entries = store.list("agent-1").await.unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].content, "memory one");
    assert_eq!(entries[1].content, "memory two");
    assert_eq!(entries[2].content, "memory three");
}

#[tokio::test]
async fn test_memory_delete_removes_entry() {
    use ao_protocol::memory::MemorySource;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    let e1 = store
        .add("agent-1", "keep me", MemorySource::Manual)
        .await
        .unwrap();
    let e2 = store
        .add("agent-1", "delete me", MemorySource::Manual)
        .await
        .unwrap();

    let deleted = store.delete("agent-1", &e2.id).await.unwrap();
    assert!(deleted);

    let entries = store.list("agent-1").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, e1.id);
    assert_eq!(entries[0].content, "keep me");
}

#[tokio::test]
async fn test_memory_delete_nonexistent_returns_false() {
    use ao_protocol::memory::MemorySource;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    store
        .add("agent-1", "some memory", MemorySource::Manual)
        .await
        .unwrap();

    let deleted = store.delete("agent-1", "nonexistent-id").await.unwrap();
    assert!(!deleted);

    // Entries are still intact
    let entries = store.list("agent-1").await.unwrap();
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn test_memory_delete_from_nonexistent_file_returns_false() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    let deleted = store.delete("no-such-agent", "some-id").await.unwrap();
    assert!(!deleted);
}

#[tokio::test]
async fn test_memory_agents_are_isolated() {
    use ao_protocol::memory::MemorySource;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    store
        .add("agent-a", "memory for A", MemorySource::Manual)
        .await
        .unwrap();
    store
        .add("agent-b", "memory for B", MemorySource::Manual)
        .await
        .unwrap();

    let a_entries = store.list("agent-a").await.unwrap();
    let b_entries = store.list("agent-b").await.unwrap();
    assert_eq!(a_entries.len(), 1);
    assert_eq!(a_entries[0].content, "memory for A");
    assert_eq!(b_entries.len(), 1);
    assert_eq!(b_entries[0].content, "memory for B");
}

// --- Global memory tests ---

#[tokio::test]
async fn test_global_memory_list_empty() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    let entries = store.list_global().await.unwrap();
    assert!(entries.is_empty());
}

#[tokio::test]
async fn test_global_memory_add_and_list() {
    use ao_protocol::memory::MemorySource;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    let entry = store
        .add_global("global fact", MemorySource::Agent)
        .await
        .unwrap();
    assert_eq!(entry.content, "global fact");

    let entries = store.list_global().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, entry.id);
}

#[tokio::test]
async fn test_global_memory_delete() {
    use ao_protocol::memory::MemorySource;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    let e1 = store
        .add_global("keep", MemorySource::Manual)
        .await
        .unwrap();
    let e2 = store
        .add_global("remove", MemorySource::Manual)
        .await
        .unwrap();

    let deleted = store.delete_global(&e2.id).await.unwrap();
    assert!(deleted);

    let entries = store.list_global().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, e1.id);
}

#[tokio::test]
async fn test_global_memory_delete_nonexistent() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = memory::MemoryStore::new(data_root);

    let deleted = store.delete_global("nonexistent-id").await.unwrap();
    assert!(!deleted);
}

// --- Per-agent team transcript path tests ---

#[test]
fn test_team_agent_transcript_path() {
    let data_root = paths::DataRoot::new("/tmp/studio_test");
    let path = data_root.team_agent_transcript_path("team-alpha", "researcher");
    assert_eq!(
        path.to_str().unwrap(),
        "/tmp/studio_test/messages/data/team_team-alpha_researcher.jsonl"
    );
}

#[test]
fn test_team_agent_transcript_path_isolation() {
    let data_root = paths::DataRoot::new("/tmp/studio_test");
    let alpha_path = data_root.team_agent_transcript_path("team-alpha", "researcher");
    let beta_path = data_root.team_agent_transcript_path("team-beta", "researcher");
    let standalone_path = data_root.agent_transcript_path("researcher");
    let team_path = data_root.team_transcript_path("team-alpha");

    // All paths must be different
    assert_ne!(alpha_path, beta_path);
    assert_ne!(alpha_path, standalone_path);
    assert_ne!(alpha_path, team_path);
}

#[tokio::test]
async fn test_per_agent_team_transcript_append_and_read() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = transcript::TranscriptStore::new(data_root);

    let key = "team_team-alpha_researcher";
    let entry = ao_protocol::transcript::TranscriptEntry {
        ts: chrono::Utc::now(),
        role: ao_protocol::transcript::TranscriptRole::Agent {
            agent: "researcher".to_string(),
        },
        content: "Research result".to_string(),
        event_type: "delegation_result".to_string(),
        metadata: None,
        hidden_from_user: false,
    };

    store.append(key, &entry).await.unwrap();
    let entries = store.read_all(key).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].content, "Research result");

    // Same agent in different team has separate transcript
    let other_key = "team_team-beta_researcher";
    let other_entries = store.read_all(other_key).await.unwrap();
    assert!(other_entries.is_empty());

    // Standalone agent transcript is also separate
    let standalone_entries = store.read_all("researcher").await.unwrap();
    assert!(standalone_entries.is_empty());
}

// --- TasklistStore tests ---

fn make_test_tasklist(team_id: &str, tasklist_id: &str) -> ao_protocol::tasklist::Tasklist {
    use ao_protocol::tasklist::{
        Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner, TasklistStatus,
    };
    Tasklist {
        id: tasklist_id.to_string(),
        owner: TasklistOwner::Team { team_id: team_id.to_string() },
        team_id: Some(team_id.to_string()),
        title: "Investigate Splunk".to_string(),
        description: "Find the root cause".to_string(),
        status: TasklistStatus::Active,
        groups: vec![
            TaskGroup {
                id: "g1".to_string(),
                mode: TaskGroupMode::Par,
                tasks: vec![
                    Task {
                        id: "t1".to_string(),
                        owner_agent_id: "researcher-a".to_string(),
                        prompt: "Pull dashboard A".to_string(),
                        expected_outputs: vec!["a.md".to_string()],
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
                    },
                    Task {
                        id: "t2".to_string(),
                        owner_agent_id: "researcher-b".to_string(),
                        prompt: "Pull dashboard B".to_string(),
                        expected_outputs: vec!["b.md".to_string()],
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
                    },
                ],
            },
            TaskGroup {
                id: "g2".to_string(),
                mode: TaskGroupMode::Seq,
                tasks: vec![Task {
                    id: "t3".to_string(),
                    owner_agent_id: "analyst".to_string(),
                    prompt: "Analyze".to_string(),
                    expected_outputs: vec!["analysis.md".to_string()],
                    status: TaskStatus::Pending,
                    group_id: "g2".to_string(),
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
                }],
            },
        ],
        workspace_dir: format!("/tmp/teams/{team_id}/tasklists/{tasklist_id}/workspace"),
        transcripts_dir: format!(
            "/tmp/teams/{team_id}/tasklists/{tasklist_id}/transcripts"
        ),
        project_id: None,
        created_at: chrono::Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        thread_id: None,
        }
}

#[tokio::test]
async fn test_tasklist_create_writes_meta_workspace_and_transcripts() {
    use ao_protocol::tasklist::Tasklist;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root.clone());

    let tasklist = make_test_tasklist("team-a", "tl-1");
    store.create(&tasklist).await.unwrap();

    let meta_path = data_root.tasklist_meta_path("team-a", "tl-1");
    let workspace = data_root.tasklist_workspace_dir("team-a", "tl-1");
    let transcripts = data_root.tasklist_transcripts_dir("team-a", "tl-1");

    assert!(tokio::fs::metadata(&meta_path).await.is_ok());
    assert!(tokio::fs::metadata(&workspace).await.unwrap().is_dir());
    assert!(tokio::fs::metadata(&transcripts).await.unwrap().is_dir());

    // Round trip via raw read so we exercise the JSON shape.
    let raw = tokio::fs::read_to_string(&meta_path).await.unwrap();
    let parsed: Tasklist = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed, tasklist);
}

#[tokio::test]
async fn test_tasklist_get_and_list_round_trip() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    let mut first = make_test_tasklist("team-a", "tl-1");
    first.status = ao_protocol::tasklist::TasklistStatus::Completed;
    first.created_at = chrono::Utc::now() - chrono::Duration::seconds(60);
    store.create(&first).await.unwrap();

    let second = make_test_tasklist("team-a", "tl-2");
    store.create(&second).await.unwrap();

    let loaded_first = store.get("team-a", "tl-1").await.unwrap().unwrap();
    assert_eq!(loaded_first, first);

    let listed = store.list("team-a").await.unwrap();
    assert_eq!(listed.len(), 2);
    // Newer tasklist comes first.
    assert_eq!(listed[0].id, "tl-2");
    assert_eq!(listed[1].id, "tl-1");

    // Sibling team has no tasklists.
    let other = store.list("team-b").await.unwrap();
    assert!(other.is_empty());
}

#[tokio::test]
async fn test_tasklist_get_missing_returns_none() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    assert!(store.get("ghost-team", "ghost-id").await.unwrap().is_none());
}

#[tokio::test]
async fn test_tasklist_create_refuses_when_active_already_exists() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    let first = make_test_tasklist("team-a", "tl-1");
    store.create(&first).await.unwrap();

    let second = make_test_tasklist("team-a", "tl-2");
    let err = store.create(&second).await;
    match err {
        Err(ao_protocol::error::AoError::TasklistAlreadyActive {
            team_id,
            tasklist_id,
        }) => {
            assert_eq!(team_id, "team-a");
            assert_eq!(tasklist_id, "tl-1");
        }
        other => panic!("expected TasklistAlreadyActive, got {other:?}"),
    }

    // Sibling team is independent.
    let other_team = make_test_tasklist("team-b", "tl-1");
    store.create(&other_team).await.unwrap();
}

#[tokio::test]
async fn test_tasklist_create_allowed_after_previous_completes() {
    use ao_protocol::tasklist::TasklistStatus;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    store.create(&make_test_tasklist("team-a", "tl-1")).await.unwrap();
    store
        .set_status("team-a", "tl-1", TasklistStatus::Completed)
        .await
        .unwrap();

    store.create(&make_test_tasklist("team-a", "tl-2")).await.unwrap();
    let active = store.find_active("team-a").await.unwrap().unwrap();
    assert_eq!(active.id, "tl-2");
}

#[tokio::test]
async fn test_tasklist_set_status_active_to_terminal_persists() {
    use ao_protocol::tasklist::TasklistStatus;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    store.create(&make_test_tasklist("team-a", "tl-1")).await.unwrap();

    let updated = store
        .set_status("team-a", "tl-1", TasklistStatus::Completed)
        .await
        .unwrap();
    assert_eq!(updated.status, TasklistStatus::Completed);

    let reloaded = store.get("team-a", "tl-1").await.unwrap().unwrap();
    assert_eq!(reloaded.status, TasklistStatus::Completed);
}

#[tokio::test]
async fn test_tasklist_set_status_rejects_terminal_to_terminal() {
    use ao_protocol::tasklist::TasklistStatus;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    store.create(&make_test_tasklist("team-a", "tl-1")).await.unwrap();
    store
        .set_status("team-a", "tl-1", TasklistStatus::Completed)
        .await
        .unwrap();

    let err = store
        .set_status("team-a", "tl-1", TasklistStatus::Failed)
        .await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::InvalidTasklistTransition(_))
    ));
}

#[tokio::test]
async fn test_tasklist_set_status_missing_returns_not_found() {
    use ao_protocol::tasklist::TasklistStatus;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    let err = store
        .set_status("team-a", "ghost", TasklistStatus::Completed)
        .await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::TasklistNotFound(_))
    ));
}

#[tokio::test]
async fn test_task_status_transitions_pending_to_completed() {
    use ao_protocol::tasklist::TaskStatus;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    store.create(&make_test_tasklist("team-a", "tl-1")).await.unwrap();

    let after_start = store
        .set_task_status("team-a", "tl-1", "t1", TaskStatus::InProgress)
        .await
        .unwrap();
    assert_eq!(after_start.groups[0].tasks[0].status, TaskStatus::InProgress);

    let after_done = store
        .set_task_status("team-a", "tl-1", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    assert_eq!(after_done.groups[0].tasks[0].status, TaskStatus::Completed);
}

#[tokio::test]
async fn test_task_status_transition_rejects_invalid() {
    use ao_protocol::tasklist::TaskStatus;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    store.create(&make_test_tasklist("team-a", "tl-1")).await.unwrap();

    // Pending -> Completed is invalid; tasks must run before they can complete.
    let err = store
        .set_task_status("team-a", "tl-1", "t1", TaskStatus::Completed)
        .await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::InvalidTasklistTransition(_))
    ));
}

#[tokio::test]
async fn test_task_status_unknown_task_returns_not_found() {
    use ao_protocol::tasklist::TaskStatus;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    store.create(&make_test_tasklist("team-a", "tl-1")).await.unwrap();

    let err = store
        .set_task_status("team-a", "tl-1", "ghost-task", TaskStatus::InProgress)
        .await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::TaskNotFound(_))
    ));
}

#[tokio::test]
async fn test_tasklist_mutate_persists_arbitrary_fields() {
    use ao_protocol::tasklist::TaskStatus;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    store.create(&make_test_tasklist("team-a", "tl-1")).await.unwrap();

    let updated = store
        .mutate("team-a", "tl-1", |tl| {
            let task = &mut tl.groups[0].tasks[0];
            task.attempt_count = 2;
            task.error_log.push("missing a.md".to_string());
            task.status = TaskStatus::Blocked;
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(updated.groups[0].tasks[0].attempt_count, 2);
    assert_eq!(
        updated.groups[0].tasks[0].error_log,
        vec!["missing a.md".to_string()]
    );

    let reloaded = store.get("team-a", "tl-1").await.unwrap().unwrap();
    assert_eq!(reloaded, updated);
}

#[tokio::test]
async fn test_tasklist_validate_ids_rejects_invalid() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    use ao_protocol::tasklist::TasklistOwner;

    let mut tl = make_test_tasklist("team-a", "tl-1");

    tl.id = "has spaces".to_string();
    assert!(store.create(&tl).await.is_err());

    // The owner is the authoritative team id — `Tasklist::team_id` is a legacy
    // mirror. Validation, the active-slot check, and the write path must all
    // key off `owner` so they can never disagree about where the tasklist goes.
    tl.id = "tl-1".to_string();
    tl.owner = TasklistOwner::Team {
        team_id: "team/slash".to_string(),
    };
    assert!(store.create(&tl).await.is_err());

    tl.id = "".to_string();
    tl.owner = TasklistOwner::Team {
        team_id: "team-a".to_string(),
    };
    assert!(store.create(&tl).await.is_err());
}

/// `create` is the team-owned constructor; agent-owned tasklists must use
/// `create_for_agent`. Passing one here previously succeeded silently and
/// wrote it under `{root}/teams/` — `team_id` is `None` for an agent owner,
/// `unwrap_or_default()` made that `""`, and `Path::join("")` collapses.
#[tokio::test]
async fn test_tasklist_create_rejects_an_agent_owned_tasklist() {
    use ao_protocol::tasklist::TasklistOwner;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root.clone());

    let mut tl = make_test_tasklist("team-a", "tl-agent");
    tl.owner = TasklistOwner::Agent {
        agent_id: "agent-1".to_string(),
    };
    tl.team_id = None;

    let err = store.create(&tl).await.expect_err("must reject agent owner");
    assert!(
        err.to_string().contains("create_for_agent"),
        "error should point at the right constructor, got: {err}"
    );

    // Nothing was written anywhere — in particular no `teams/` subtree.
    assert!(!tokio::fs::try_exists(data_root.teams_dir()).await.unwrap());
}

#[tokio::test]
async fn test_tasklist_copilot_binding_round_trips() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    let tasklist = make_test_tasklist("team-a", "tl-1");
    store.create(&tasklist).await.unwrap();

    // Fresh tasklist has no co-pilot bound.
    assert!(
        store
            .get_copilot_agent_id("team-a", "tl-1")
            .await
            .unwrap()
            .is_none()
    );

    // First binding wins.
    let bound = store
        .bind_copilot_agent_id("team-a", "tl-1", "copilot-agent-1")
        .await
        .unwrap();
    assert_eq!(bound, "copilot-agent-1");

    // Round-trips through the meta file.
    let loaded = store.get("team-a", "tl-1").await.unwrap().unwrap();
    assert_eq!(loaded.copilot_agent_id.as_deref(), Some("copilot-agent-1"));

    let fetched = store.get_copilot_agent_id("team-a", "tl-1").await.unwrap();
    assert_eq!(fetched.as_deref(), Some("copilot-agent-1"));
}

#[tokio::test]
async fn test_tasklist_copilot_bind_is_idempotent() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    let tasklist = make_test_tasklist("team-a", "tl-1");
    store.create(&tasklist).await.unwrap();

    let first = store
        .bind_copilot_agent_id("team-a", "tl-1", "copilot-agent-1")
        .await
        .unwrap();
    // Second call with a different agent id returns the original binding —
    // never reassigns. Models the race where two parallel /copilot calls
    // each try to mint a fresh agent.
    let second = store
        .bind_copilot_agent_id("team-a", "tl-1", "copilot-agent-2")
        .await
        .unwrap();

    assert_eq!(first, "copilot-agent-1");
    assert_eq!(second, "copilot-agent-1");

    let loaded = store.get("team-a", "tl-1").await.unwrap().unwrap();
    assert_eq!(loaded.copilot_agent_id.as_deref(), Some("copilot-agent-1"));
}

#[tokio::test]
async fn test_tasklist_copilot_bind_missing_tasklist_errors() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    let err = store
        .bind_copilot_agent_id("team-a", "ghost", "copilot")
        .await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::TasklistNotFound(ref id)) if id == "ghost"
    ));

    // Fetch on a missing tasklist yields None (consistent with `get`).
    assert!(
        store
            .get_copilot_agent_id("team-a", "ghost")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_tasklist_find_by_copilot_agent_id_resolves_team_and_tasklist() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    // Two teams, one tasklist each (the create chokepoint rejects a
    // second active tasklist per team — see TasklistAlreadyActive). One
    // bound, the other deliberately unbound to pin "no false positives".
    let tl_a = make_test_tasklist("team-a", "tl-a1");
    let tl_b = make_test_tasklist("team-b", "tl-b1");
    store.create(&tl_a).await.unwrap();
    store.create(&tl_b).await.unwrap();

    store
        .bind_copilot_agent_id("team-a", "tl-a1", "copilot-A")
        .await
        .unwrap();
    // tl-b1 is intentionally NOT bound.

    // Bound agent resolves to the right (team, tasklist) pair.
    let found = store
        .find_by_copilot_agent_id("copilot-A")
        .await
        .unwrap()
        .expect("copilot-A should resolve");
    assert_eq!(found.team_id.as_deref(), Some("team-a"));
    assert_eq!(found.id, "tl-a1");
    assert_eq!(found.copilot_agent_id.as_deref(), Some("copilot-A"));

    // Unknown agent id resolves to None — the unbound tasklist on team-b
    // does not produce a spurious match.
    assert!(store
        .find_by_copilot_agent_id("never-bound")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .find_by_copilot_agent_id("copilot-B")
        .await
        .unwrap()
        .is_none());
}

/// The co-pilot reverse lookup must resolve agent-owned (project-scoped)
/// tasklists, which live under `tasks/agents/{agent_id}/tasklists/` rather than
/// `teams/`.
///
/// This matters because the live `GET /projects/{id}/tasklists/{tid}/copilot`
/// route binds a co-pilot to exactly such a tasklist (see `routes/projects.rs`,
/// which writes through `mutate_by_owner` on a `TasklistOwner::Agent`). While
/// the lookup walked `teams/` only, every consumer of it was inert for project
/// tasklists: co-pilot context injection, the mailbox poller's sleep sweep
/// (which evicted such co-pilots as orphans), and the
/// `<tasklist action="append">` tag handler.
#[tokio::test]
async fn test_find_by_copilot_agent_id_resolves_agent_owned_tasklists() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    // Mirror what the project co-pilot route does: an agent-owned tasklist
    // whose `copilot_agent_id` is written through the owner-aware mutator.
    let owner = ao_protocol::tasklist::TasklistOwner::Agent {
        agent_id: "agent-a".to_string(),
    };
    store
        .create_for_agent(&make_agent_tasklist("agent-a", "tl-1"))
        .await
        .unwrap();
    store
        .mutate_by_owner(&owner, "tl-1", |tl| {
            tl.copilot_agent_id = Some("copilot-P".to_string());
            Ok(())
        })
        .await
        .unwrap();

    // The binding IS persisted...
    let bound = store
        .get_by_owner(&owner, "tl-1")
        .await
        .unwrap()
        .expect("agent-owned tasklist exists");
    assert_eq!(bound.copilot_agent_id.as_deref(), Some("copilot-P"));

    // ...and the reverse lookup resolves it from the agent tree.
    let found = store
        .find_by_copilot_agent_id("copilot-P")
        .await
        .unwrap()
        .expect("agent-owned co-pilot binding must be resolvable");
    assert_eq!(found.id, "tl-1");
    assert_eq!(found.owner, owner);

    // A co-pilot id nobody is bound to still resolves to None rather than
    // matching the first agent-owned tasklist it walks past.
    assert!(store
        .find_by_copilot_agent_id("copilot-unbound")
        .await
        .unwrap()
        .is_none());
}

/// Both trees are searched, and a team-owned binding still resolves after the
/// agent-tree walk was added. Guards against the agent branch shadowing or
/// short-circuiting the original team lookup.
#[tokio::test]
async fn test_find_by_copilot_agent_id_still_resolves_team_owned_alongside_agent_owned() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    // An agent-owned tasklist bound to one co-pilot...
    store
        .create_for_agent(&make_agent_tasklist("agent-a", "tl-agent"))
        .await
        .unwrap();
    let agent_owner = ao_protocol::tasklist::TasklistOwner::Agent {
        agent_id: "agent-a".to_string(),
    };
    store
        .mutate_by_owner(&agent_owner, "tl-agent", |tl| {
            tl.copilot_agent_id = Some("copilot-agent".to_string());
            Ok(())
        })
        .await
        .unwrap();

    // ...and a team-owned tasklist bound to another.
    store.create(&make_test_tasklist("team-a", "tl-team")).await.unwrap();
    store
        .mutate("team-a", "tl-team", |tl| {
            tl.copilot_agent_id = Some("copilot-team".to_string());
            Ok(())
        })
        .await
        .unwrap();

    let from_team = store
        .find_by_copilot_agent_id("copilot-team")
        .await
        .unwrap()
        .expect("team-owned binding must still resolve");
    assert_eq!(from_team.id, "tl-team");

    let from_agent = store
        .find_by_copilot_agent_id("copilot-agent")
        .await
        .unwrap()
        .expect("agent-owned binding must resolve");
    assert_eq!(from_agent.id, "tl-agent");
}

#[tokio::test]
async fn test_tasklist_list_all_across_teams_returns_all_statuses() {
    // `list_all_across_teams` is the one used by the co-pilot mailbox
    // poller's startup rebuild — it must include tasklists in
    // every status (not just `Active`) so the heartbeat path
    // (`is_tasklist_active` returning true on a recently-opened
    // Completed/Failed tasklist) sees them.
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = tasklist_store::TasklistStore::new(data_root);

    // Two teams, each with one tasklist; flip one to a non-Active status
    // so the result must include both even though only one is Active.
    let tl_a = make_test_tasklist("team-a", "tl-a1");
    let tl_b = make_test_tasklist("team-b", "tl-b1");
    store.create(&tl_a).await.unwrap();
    store.create(&tl_b).await.unwrap();
    store
        .set_status(
            "team-b",
            "tl-b1",
            ao_protocol::tasklist::TasklistStatus::Cancelled,
        )
        .await
        .unwrap();

    let all = store.list_all_across_teams().await.unwrap();
    let ids: std::collections::HashSet<String> =
        all.iter().map(|t| t.id.clone()).collect();
    assert!(ids.contains("tl-a1"));
    assert!(ids.contains("tl-b1"));
    assert_eq!(all.len(), 2);

    // For comparison, list_active_across_teams must drop the cancelled one.
    let active_only = store.list_active_across_teams().await.unwrap();
    assert_eq!(active_only.len(), 1);
    assert_eq!(active_only[0].id, "tl-a1");
}

#[tokio::test]
async fn test_tasklist_list_all_across_teams_on_empty_root_returns_empty() {
    let (_tmp, data_root) = setup_temp_data_root();
    // Skip ensure_directories so teams/ doesn't exist — must not error.
    let store = tasklist_store::TasklistStore::new(data_root);
    let all = store.list_all_across_teams().await.unwrap();
    assert!(all.is_empty());
}

#[tokio::test]
async fn test_tasklist_find_by_copilot_agent_on_empty_root_returns_none() {
    let (_tmp, data_root) = setup_temp_data_root();
    // Skip ensure_directories so teams/ doesn't exist — the helper must
    // not error on a fresh data root.
    let store = tasklist_store::TasklistStore::new(data_root);
    let found = store.find_by_copilot_agent_id("anyone").await.unwrap();
    assert!(found.is_none());
}

#[test]
fn test_tasklist_path_helpers() {
    let (_tmp, data_root) = setup_temp_data_root();
    let root = data_root.root().clone();
    assert_eq!(
        data_root.team_tasklists_dir("team-a"),
        root.join("teams").join("team-a").join("tasklists")
    );
    assert_eq!(
        data_root.tasklist_dir("team-a", "tl-1"),
        root.join("teams").join("team-a").join("tasklists").join("tl-1")
    );
    assert_eq!(
        data_root.tasklist_meta_path("team-a", "tl-1"),
        root.join("teams")
            .join("team-a")
            .join("tasklists")
            .join("tl-1")
            .join("tasklist.json")
    );
    assert_eq!(
        data_root.tasklist_workspace_dir("team-a", "tl-1"),
        root.join("teams")
            .join("team-a")
            .join("tasklists")
            .join("tl-1")
            .join("workspace")
    );
    assert_eq!(
        data_root.tasklist_transcripts_dir("team-a", "tl-1"),
        root.join("teams")
            .join("team-a")
            .join("tasklists")
            .join("tl-1")
            .join("transcripts")
    );
}

// --- Agent-scope tasklist path-helper tests ---

#[test]
fn test_agent_tasklist_path_helpers() {
    let (_tmp, data_root) = setup_temp_data_root();
    let root = data_root.root().clone();
    let base = root.join("tasks").join("agents").join("agent-a").join("tasklists");

    assert_eq!(data_root.agent_tasklists_dir("agent-a"), base);
    assert_eq!(
        data_root.agent_tasklist_dir("agent-a", "tl-1"),
        base.join("tl-1")
    );
    assert_eq!(
        data_root.agent_tasklist_meta_path("agent-a", "tl-1"),
        base.join("tl-1").join("tasklist.json")
    );
    assert_eq!(
        data_root.agent_tasklist_workspace_dir("agent-a", "tl-1"),
        base.join("tl-1").join("workspace")
    );
    assert_eq!(
        data_root.agent_tasklist_transcripts_dir("agent-a", "tl-1"),
        base.join("tl-1").join("transcripts")
    );
    assert_eq!(
        data_root.agent_tasklist_transcript_path("agent-a", "tl-1", "task-7"),
        base.join("tl-1").join("transcripts").join("task-7.jsonl")
    );
}

fn make_agent_tasklist(
    agent_id: &str,
    tasklist_id: &str,
) -> ao_protocol::tasklist::Tasklist {
    use ao_protocol::tasklist::{
        Task, TaskGroup, TaskGroupMode, TaskStatus, Tasklist, TasklistOwner, TasklistStatus,
    };
    Tasklist {
        id: tasklist_id.to_string(),
        owner: TasklistOwner::Agent { agent_id: agent_id.to_string() },
        team_id: None,
        title: "Agent Work".to_string(),
        description: String::new(),
        status: TasklistStatus::Active,
        groups: vec![TaskGroup {
            id: "g1".to_string(),
            mode: TaskGroupMode::Seq,
            tasks: vec![Task {
                id: "t1".to_string(),
                owner_agent_id: agent_id.to_string(),
                prompt: "Do something".to_string(),
                expected_outputs: vec![],
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
            }],
        }],
        workspace_dir: format!("/tmp/agent/{agent_id}/tasklists/{tasklist_id}/workspace"),
        transcripts_dir: format!("/tmp/agent/{agent_id}/tasklists/{tasklist_id}/transcripts"),
        project_id: None,
        created_at: chrono::Utc::now(),
        last_active_at: None,
        copilot_agent_id: None,
        last_opened_at: None,
        thread_id: None,
        }
}

#[tokio::test]
async fn test_agent_tasklist_create_writes_dirs_and_meta() {
    use ao_protocol::tasklist::Tasklist;

    let (_tmp, data_root) = setup_temp_data_root();
    let store = tasklist_store::TasklistStore::new(data_root.clone());

    let tl = make_agent_tasklist("agent-a", "tl-1");
    store.create_for_agent(&tl).await.unwrap();

    let meta = data_root.agent_tasklist_meta_path("agent-a", "tl-1");
    let workspace = data_root.agent_tasklist_workspace_dir("agent-a", "tl-1");
    let transcripts = data_root.agent_tasklist_transcripts_dir("agent-a", "tl-1");

    assert!(tokio::fs::metadata(&meta).await.is_ok());
    assert!(tokio::fs::metadata(&workspace).await.unwrap().is_dir());
    assert!(tokio::fs::metadata(&transcripts).await.unwrap().is_dir());

    let raw = tokio::fs::read_to_string(&meta).await.unwrap();
    let parsed: Tasklist = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed.id, "tl-1");
    assert!(matches!(
        parsed.owner,
        ao_protocol::tasklist::TasklistOwner::Agent { ref agent_id } if agent_id == "agent-a"
    ));
}

#[tokio::test]
async fn test_agent_tasklist_get_and_list_round_trip() {
    let (_tmp, data_root) = setup_temp_data_root();
    let store = tasklist_store::TasklistStore::new(data_root);

    let mut first = make_agent_tasklist("agent-a", "tl-1");
    first.status = ao_protocol::tasklist::TasklistStatus::Completed;
    first.created_at = chrono::Utc::now() - chrono::Duration::seconds(60);
    store.create_for_agent(&first).await.unwrap();

    let second = make_agent_tasklist("agent-a", "tl-2");
    store.create_for_agent(&second).await.unwrap();

    let loaded = store.get_for_agent("agent-a", "tl-1").await.unwrap().unwrap();
    assert_eq!(loaded, first);

    let listed = store.list_for_agent("agent-a").await.unwrap();
    assert_eq!(listed.len(), 2);
    // Newer tasklist comes first.
    assert_eq!(listed[0].id, "tl-2");
    assert_eq!(listed[1].id, "tl-1");

    // Different agent has no tasklists.
    let other = store.list_for_agent("agent-b").await.unwrap();
    assert!(other.is_empty());
}

#[tokio::test]
async fn test_agent_tasklist_get_missing_returns_none() {
    let (_tmp, data_root) = setup_temp_data_root();
    let store = tasklist_store::TasklistStore::new(data_root);
    assert!(store.get_for_agent("no-agent", "no-tl").await.unwrap().is_none());
}

#[tokio::test]
async fn test_concurrent_mutations_do_not_lose_updates() {
    use std::sync::Arc;

    // Two writers touch DIFFERENT fields of the same tasklist concurrently.
    // The per-tasklist write lock must serialize the read-modify-write so
    // neither write clobbers the other (the lost-update race that dropped
    // classifier assignments and reverted completed statuses in PAR mode).
    let (_tmp, data_root) = setup_temp_data_root();
    let store = Arc::new(tasklist_store::TasklistStore::new(data_root));
    store
        .create_for_agent(&make_agent_tasklist("agent-a", "tl-1"))
        .await
        .unwrap();

    let s1 = Arc::clone(&store);
    let w1 = tokio::spawn(async move {
        s1.mutate_for_agent("agent-a", "tl-1", |tl| {
            tl.groups[0].tasks[0].attempt_count = 7;
            Ok(())
        })
        .await
        .unwrap();
    });
    let s2 = Arc::clone(&store);
    let w2 = tokio::spawn(async move {
        s2.mutate_for_agent("agent-a", "tl-1", |tl| {
            tl.groups[0].tasks[0].error_log.push("note".to_string());
            Ok(())
        })
        .await
        .unwrap();
    });
    w1.await.unwrap();
    w2.await.unwrap();

    // Both independent writes must survive.
    let tl = store.get_for_agent("agent-a", "tl-1").await.unwrap().unwrap();
    assert_eq!(tl.groups[0].tasks[0].attempt_count, 7);
    assert_eq!(tl.groups[0].tasks[0].error_log, vec!["note".to_string()]);
}

#[tokio::test]
async fn test_agent_tasklist_active_for_agent_returns_non_terminal() {
    use ao_protocol::tasklist::TasklistStatus;

    let (_tmp, data_root) = setup_temp_data_root();
    let store = tasklist_store::TasklistStore::new(data_root);

    // No active tasklist yet.
    assert!(store.active_for_agent("agent-a").await.unwrap().is_none());

    let tl = make_agent_tasklist("agent-a", "tl-1");
    store.create_for_agent(&tl).await.unwrap();
    let active = store.active_for_agent("agent-a").await.unwrap().unwrap();
    assert_eq!(active.id, "tl-1");

    // After completing, active_for_agent returns None.
    let mut completed = tl.clone();
    completed.status = TasklistStatus::Completed;
    // Write it back via mutate_for_agent.
    store
        .mutate_for_agent("agent-a", "tl-1", |tl| {
            tl.status = TasklistStatus::Completed;
            Ok(())
        })
        .await
        .unwrap();
    assert!(store.active_for_agent("agent-a").await.unwrap().is_none());
}

#[tokio::test]
async fn test_agent_tasklist_mutate_persists_changes() {
    let (_tmp, data_root) = setup_temp_data_root();
    let store = tasklist_store::TasklistStore::new(data_root);

    let tl = make_agent_tasklist("agent-a", "tl-1");
    store.create_for_agent(&tl).await.unwrap();

    let updated = store
        .mutate_for_agent("agent-a", "tl-1", |tl| {
            tl.groups[0].tasks[0].attempt_count = 3;
            tl.groups[0].tasks[0].error_log.push("boom".to_string());
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(updated.groups[0].tasks[0].attempt_count, 3);

    let reloaded = store.get_for_agent("agent-a", "tl-1").await.unwrap().unwrap();
    assert_eq!(reloaded.groups[0].tasks[0].attempt_count, 3);
    assert_eq!(reloaded.groups[0].tasks[0].error_log, vec!["boom"]);
}

#[tokio::test]
async fn test_agent_tasklist_create_refuses_when_active_exists() {
    let (_tmp, data_root) = setup_temp_data_root();
    let store = tasklist_store::TasklistStore::new(data_root);

    store.create_for_agent(&make_agent_tasklist("agent-a", "tl-1")).await.unwrap();

    let err = store.create_for_agent(&make_agent_tasklist("agent-a", "tl-2")).await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::TasklistAlreadyActive { .. })
    ));

    // Different agent is independent.
    store.create_for_agent(&make_agent_tasklist("agent-b", "tl-1")).await.unwrap();
}

#[tokio::test]
async fn test_agent_tasklist_concurrent_mutate_last_write_wins() {
    let (_tmp, data_root) = setup_temp_data_root();
    let store = std::sync::Arc::new(tasklist_store::TasklistStore::new(data_root));

    let tl = make_agent_tasklist("agent-a", "tl-1");
    store.create_for_agent(&tl).await.unwrap();

    let s1 = store.clone();
    let s2 = store.clone();

    let (r1, r2) = tokio::join!(
        s1.mutate_for_agent("agent-a", "tl-1", |tl| {
            tl.groups[0].tasks[0].attempt_count = 1;
            Ok(())
        }),
        s2.mutate_for_agent("agent-a", "tl-1", |tl| {
            tl.groups[0].tasks[0].attempt_count = 2;
            Ok(())
        }),
    );

    // Both writes must succeed; final value is one of {1, 2}.
    r1.unwrap();
    r2.unwrap();
    let final_tl = store.get_for_agent("agent-a", "tl-1").await.unwrap().unwrap();
    assert!(
        final_tl.groups[0].tasks[0].attempt_count == 1
            || final_tl.groups[0].tasks[0].attempt_count == 2
    );
}

#[tokio::test]
async fn test_try_begin_task_claims_once_then_rejects() {
    // The dispatch claim is the guard against double-execution: a stale
    // in-memory snapshot may still believe a task is Pending after a
    // concurrent advance already started it. The claim must flip
    // Pending -> InProgress exactly once and reject every later attempt.
    use ao_protocol::tasklist::{TaskStatus, TasklistOwner};

    let (_tmp, data_root) = setup_temp_data_root();
    let store = tasklist_store::TasklistStore::new(data_root);

    let tl = make_agent_tasklist("agent-a", "tl-1");
    store.create_for_agent(&tl).await.unwrap();
    let owner = TasklistOwner::Agent { agent_id: "agent-a".to_string() };

    // First claim wins and flips the task to InProgress.
    assert!(
        store.try_begin_task_by_owner(&owner, "tl-1", "t1").await.unwrap(),
        "first claim on a Pending task must succeed",
    );
    let after = store.get_for_agent("agent-a", "tl-1").await.unwrap().unwrap();
    assert_eq!(after.groups[0].tasks[0].status, TaskStatus::InProgress);

    // A second claim (the stale-snapshot re-dispatch) must be rejected so
    // the task never runs twice — this is the "Japan\n\nJapan" repro guard.
    assert!(
        !store.try_begin_task_by_owner(&owner, "tl-1", "t1").await.unwrap(),
        "claiming an already-InProgress task must be rejected",
    );

    // A terminal task must also be unclaimable: a late advance that arrives
    // after the task already completed must not revive and re-dispatch it.
    store
        .set_task_status_by_owner(&owner, "tl-1", "t1", TaskStatus::Completed)
        .await
        .unwrap();
    assert!(
        !store.try_begin_task_by_owner(&owner, "tl-1", "t1").await.unwrap(),
        "claiming a terminal (Completed) task must be rejected",
    );
}

#[tokio::test]
async fn test_try_begin_task_concurrent_single_winner() {
    // Race many concurrent claims for the same Pending task — the exact
    // shape of the double-dispatch bug, where several advance() calls (one
    // per classifier write-back plus sibling-terminal re-drives) all read
    // the same Pending snapshot and try to start the task at once. The
    // check-and-set under the per-tasklist lock must elect exactly one.
    use ao_protocol::tasklist::TasklistOwner;

    let (_tmp, data_root) = setup_temp_data_root();
    let store = std::sync::Arc::new(tasklist_store::TasklistStore::new(data_root));

    let tl = make_agent_tasklist("agent-a", "tl-1");
    store.create_for_agent(&tl).await.unwrap();
    let owner = TasklistOwner::Agent { agent_id: "agent-a".to_string() };

    let mut handles = Vec::new();
    for _ in 0..16 {
        let s = store.clone();
        let o = owner.clone();
        handles.push(tokio::spawn(async move {
            s.try_begin_task_by_owner(&o, "tl-1", "t1").await.unwrap()
        }));
    }

    let mut winners = 0;
    for h in handles {
        if h.await.unwrap() {
            winners += 1;
        }
    }
    assert_eq!(winners, 1, "exactly one concurrent claim may win the dispatch");
}

/// Seed an agent tasklist whose single task `t1` is already `InProgress`,
/// which is the only state `try_reclaim_dispatch_by_owner` acts on. Returns
/// the store and the owner handle; the task's `dispatch_token` is still 0
/// because beginning a task does not bump it.
async fn setup_in_progress_task(
    data_root: paths::DataRoot,
) -> (tasklist_store::TasklistStore, ao_protocol::tasklist::TasklistOwner) {
    use ao_protocol::tasklist::TasklistOwner;

    let store = tasklist_store::TasklistStore::new(data_root);
    store.create_for_agent(&make_agent_tasklist("agent-a", "tl-1")).await.unwrap();
    let owner = TasklistOwner::Agent { agent_id: "agent-a".to_string() };
    assert!(store.try_begin_task_by_owner(&owner, "tl-1", "t1").await.unwrap());
    (store, owner)
}

#[tokio::test]
async fn test_try_reclaim_dispatch_claims_and_bumps_both_counters() {
    // The happy path of the reclaim check-and-set: a caller that presents the
    // token it read before deciding the task needed recovery wins the claim,
    // and both counters advance by exactly one. The returned snapshot must be
    // the POST-bump state so callers render "Attempt N" text that matches
    // what was actually persisted.
    use tasklist_store::ReclaimDispatchOutcome;

    let (_tmp, data_root) = setup_temp_data_root();
    let (store, owner) = setup_in_progress_task(data_root).await;

    let outcome = store
        .try_reclaim_dispatch_by_owner(&owner, "tl-1", "t1", 0, 5, |n| {
            format!("Attempt {n}: recovery")
        })
        .await
        .unwrap();

    match outcome {
        ReclaimDispatchOutcome::Claimed { attempt_count, dispatch_token, task } => {
            assert_eq!(attempt_count, 1, "attempt_count must advance by exactly 1");
            assert_eq!(dispatch_token, 1, "dispatch_token must advance by exactly 1");
            // The snapshot is post-bump, not the pre-lock read.
            assert_eq!(task.attempt_count, 1);
            assert_eq!(task.dispatch_token, 1);
            assert_eq!(task.error_log, vec!["Attempt 1: recovery"]);
        }
        other => panic!("expected Claimed, got {other:?}"),
    }

    let reloaded = store.get_for_agent("agent-a", "tl-1").await.unwrap().unwrap();
    assert_eq!(reloaded.groups[0].tasks[0].attempt_count, 1);
    assert_eq!(reloaded.groups[0].tasks[0].dispatch_token, 1);
}

#[tokio::test]
async fn test_try_reclaim_dispatch_stale_token_is_rejected_with_no_side_effects() {
    // This is the check-and-set itself. Concurrency is not needed to
    // reproduce the race: replaying the SAME token a second time is exactly
    // what a losing racer presents, because the winner already bumped the
    // live value out from under it.
    //
    // The losing racer must have ZERO side effects — no attempt_count burn,
    // no token bump, no error_log entry. If it did mutate, two recoverers
    // firing on one stall would consume two of the task's attempts and could
    // drive it to Failed at half the configured max_attempts.
    use tasklist_store::ReclaimDispatchOutcome;

    let (_tmp, data_root) = setup_temp_data_root();
    let (store, owner) = setup_in_progress_task(data_root).await;

    // The winner claims with the live token, moving it 0 -> 1.
    let won = store
        .try_reclaim_dispatch_by_owner(&owner, "tl-1", "t1", 0, 5, |n| {
            format!("Attempt {n}: winner")
        })
        .await
        .unwrap();
    assert!(matches!(won, ReclaimDispatchOutcome::Claimed { .. }));

    let after_winner = store.get_for_agent("agent-a", "tl-1").await.unwrap().unwrap();
    let after_winner = &after_winner.groups[0].tasks[0];
    assert_eq!(after_winner.attempt_count, 1);
    assert_eq!(after_winner.dispatch_token, 1);
    let error_log_after_winner = after_winner.error_log.clone();

    // The loser replays the now-stale token it captured before the winner ran.
    let lost = store
        .try_reclaim_dispatch_by_owner(&owner, "tl-1", "t1", 0, 5, |n| {
            format!("Attempt {n}: loser")
        })
        .await
        .unwrap();
    assert!(
        matches!(lost, ReclaimDispatchOutcome::Stale),
        "replaying a stale dispatch_token must lose the reclaim race, got {lost:?}",
    );

    // The load-bearing assertion: the loser changed nothing.
    let reloaded = store.get_for_agent("agent-a", "tl-1").await.unwrap().unwrap();
    let task = &reloaded.groups[0].tasks[0];
    assert_eq!(
        task.attempt_count, 1,
        "a stale reclaim must not burn an attempt",
    );
    assert_eq!(
        task.dispatch_token, 1,
        "a stale reclaim must not bump the dispatch token",
    );
    assert_eq!(
        task.error_log, error_log_after_winner,
        "a stale reclaim must not append to the error log",
    );
    assert_eq!(task.status, ao_protocol::tasklist::TaskStatus::InProgress);
}

#[tokio::test]
async fn test_try_reclaim_dispatch_rejects_task_not_in_progress() {
    // A task that some other actor already resolved is not reclaimable, even
    // when the caller's token still matches. Recovery must not revive it.
    use ao_protocol::tasklist::TasklistOwner;
    use tasklist_store::ReclaimDispatchOutcome;

    let (_tmp, data_root) = setup_temp_data_root();
    let store = tasklist_store::TasklistStore::new(data_root);
    store.create_for_agent(&make_agent_tasklist("agent-a", "tl-1")).await.unwrap();
    let owner = TasklistOwner::Agent { agent_id: "agent-a".to_string() };

    // Never begun, so the task is still Pending with a token of 0.
    let outcome = store
        .try_reclaim_dispatch_by_owner(&owner, "tl-1", "t1", 0, 5, |n| {
            format!("Attempt {n}: should not run")
        })
        .await
        .unwrap();
    match outcome {
        ReclaimDispatchOutcome::NotInProgress { observed } => {
            // The carried status is the one read under the lock, so it must
            // be the status the task actually holds.
            assert_eq!(observed, ao_protocol::tasklist::TaskStatus::Pending);
        }
        other => panic!("a non-InProgress task must not be reclaimable, got {other:?}"),
    }

    let reloaded = store.get_for_agent("agent-a", "tl-1").await.unwrap().unwrap();
    let task = &reloaded.groups[0].tasks[0];
    assert_eq!(task.status, ao_protocol::tasklist::TaskStatus::Pending);
    assert_eq!(task.attempt_count, 0, "rejected reclaim must not mutate");
    assert_eq!(task.dispatch_token, 0, "rejected reclaim must not mutate");
    assert!(task.error_log.is_empty(), "rejected reclaim must not mutate");

    // Reject a second, DIFFERENT non-InProgress status. One case alone cannot
    // distinguish a field that reports the real under-lock observation from
    // one wired to a constant; two divergent seeds can.
    store
        .set_task_status_by_owner(&owner, "tl-1", "t1", ao_protocol::tasklist::TaskStatus::Blocked)
        .await
        .unwrap();
    let outcome = store
        .try_reclaim_dispatch_by_owner(&owner, "tl-1", "t1", 0, 5, |n| {
            format!("Attempt {n}: should not run")
        })
        .await
        .unwrap();
    match outcome {
        ReclaimDispatchOutcome::NotInProgress { observed } => {
            assert_eq!(
                observed,
                ao_protocol::tasklist::TaskStatus::Blocked,
                "the carried status must track the task's real status, not a fixed value",
            );
        }
        other => panic!("a non-InProgress task must not be reclaimable, got {other:?}"),
    }
}

#[tokio::test]
async fn test_try_reclaim_dispatch_exhaustion_persists_failed() {
    // The bump and the max_attempts evaluation happen against the value read
    // inside the same locked section, so the reclaim that crosses the
    // threshold also writes Failed. The caller is told to drive the terminal
    // hook rather than dispatch.
    use tasklist_store::ReclaimDispatchOutcome;

    let (_tmp, data_root) = setup_temp_data_root();
    let (store, owner) = setup_in_progress_task(data_root).await;

    const MAX_ATTEMPTS: u32 = 3;
    store
        .mutate_for_agent("agent-a", "tl-1", |tl| {
            tl.groups[0].tasks[0].attempt_count = MAX_ATTEMPTS - 1;
            Ok(())
        })
        .await
        .unwrap();

    let outcome = store
        .try_reclaim_dispatch_by_owner(&owner, "tl-1", "t1", 0, MAX_ATTEMPTS, |n| {
            format!("Attempt {n}: final")
        })
        .await
        .unwrap();

    match outcome {
        ReclaimDispatchOutcome::Exhausted { attempt_count } => {
            assert_eq!(attempt_count, MAX_ATTEMPTS);
        }
        other => panic!("expected Exhausted, got {other:?}"),
    }

    // Re-read from the store: the Failed transition must have hit disk in the
    // same locked write, not merely mutated an in-memory copy.
    let reloaded = store.get_for_agent("agent-a", "tl-1").await.unwrap().unwrap();
    let task = &reloaded.groups[0].tasks[0];
    assert_eq!(
        task.status,
        ao_protocol::tasklist::TaskStatus::Failed,
        "the exhausting reclaim must persist Failed",
    );
    assert_eq!(task.attempt_count, MAX_ATTEMPTS);
    assert_eq!(task.error_log.last().unwrap(), "Attempt 3: final");
}

#[tokio::test]
async fn test_existing_team_tasklist_persistence_unchanged() {
    // Regression test: team-scoped tasklists still use the team path tree
    // after agent-scope routing was added to write_meta_atomic.
    let (_tmp, data_root) = setup_temp_data_root();
    let store = tasklist_store::TasklistStore::new(data_root.clone());

    let tl = make_test_tasklist("team-x", "tl-x");
    store.create(&tl).await.unwrap();

    // Meta file is on the team path, not the agent path.
    let team_meta = data_root.tasklist_meta_path("team-x", "tl-x");
    assert!(tokio::fs::metadata(&team_meta).await.is_ok());

    let reloaded = store.get("team-x", "tl-x").await.unwrap().unwrap();
    assert_eq!(reloaded, tl);
}

// --- ProjectStore tests ---

fn make_test_project(id: &str) -> ao_protocol::project::Project {
    use ao_protocol::project::{Project, ProjectStatus};
    let now = chrono::Utc::now();
    Project {
        id: id.to_string(),
        name: format!("Test Project {}", id),
        emoji: None,
        goal: "Build something great".to_string(),
        spec: None,
        agent_id: "test-agent".to_string(),
        working_dir: None,
        attachments: vec![],
        status: ProjectStatus::Interviewing,
        summary: None,
        verifications: vec![],
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn test_project_create_then_get() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = projects::ProjectStore::new(data_root);

    let p = make_test_project("proj-1");
    store.create(&p).await.unwrap();

    let loaded = store.get("proj-1").await.unwrap().expect("project exists");
    assert_eq!(loaded.id, "proj-1");
    assert_eq!(loaded.goal, p.goal);
}

#[tokio::test]
async fn test_project_create_three_then_list() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = projects::ProjectStore::new(data_root);

    for id in &["p1", "p2", "p3"] {
        store.create(&make_test_project(id)).await.unwrap();
    }

    let all = store.list().await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn test_project_create_then_delete_then_get_returns_none() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = projects::ProjectStore::new(data_root);

    store.create(&make_test_project("del-proj")).await.unwrap();
    let deleted = store.delete("del-proj").await.unwrap();
    assert!(deleted);

    let found = store.get("del-proj").await.unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn test_project_delete_nonexistent_returns_false() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = projects::ProjectStore::new(data_root);

    let deleted = store.delete("no-such-project").await.unwrap();
    assert!(!deleted);
}

#[tokio::test]
async fn test_project_create_duplicate_fails() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = projects::ProjectStore::new(data_root);

    store.create(&make_test_project("dup")).await.unwrap();
    let err = store.create(&make_test_project("dup")).await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::ProjectAlreadyExists(_))
    ));
}

#[tokio::test]
async fn test_project_save_updates_existing() {
    use ao_protocol::project::ProjectStatus;

    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = projects::ProjectStore::new(data_root);

    let mut p = make_test_project("upd-proj");
    store.create(&p).await.unwrap();

    p.status = ProjectStatus::Active;
    p.spec = Some("Refined spec".to_string());
    store.save(&p).await.unwrap();

    let loaded = store.get("upd-proj").await.unwrap().unwrap();
    assert!(matches!(loaded.status, ProjectStatus::Active));
    assert_eq!(loaded.spec.as_deref(), Some("Refined spec"));
}

#[tokio::test]
async fn test_project_save_nonexistent_fails() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = projects::ProjectStore::new(data_root);

    let err = store.save(&make_test_project("ghost")).await;
    assert!(matches!(
        err,
        Err(ao_protocol::error::AoError::ProjectNotFound(_))
    ));
}

#[tokio::test]
async fn test_project_get_nonexistent_returns_none() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    let store = projects::ProjectStore::new(data_root);

    let result = store.get("no-such-id").await.unwrap();
    assert!(result.is_none());
}

#[test]
fn test_project_path_helpers() {
    let (_tmp, data_root) = setup_temp_data_root();
    let root = data_root.root().clone();
    assert_eq!(data_root.projects_dir(), root.join("projects"));
    assert_eq!(
        data_root.project_path("my-project"),
        root.join("projects").join("my-project.yaml")
    );
}

#[tokio::test]
async fn test_ensure_directories_creates_projects_dir() {
    let (_tmp, data_root) = setup_temp_data_root();
    data_root.ensure_directories().await.unwrap();
    assert!(tokio::fs::metadata(data_root.projects_dir()).await.is_ok());
}
