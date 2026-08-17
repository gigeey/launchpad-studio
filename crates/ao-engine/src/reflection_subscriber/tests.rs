use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};
use ao_persistence::paths::DataRoot;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_protocol::memory::{MemoryScope, MemorySource, MemoryStatus};
use ao_protocol::preferences::UserPreferences;
use ao_protocol::reflection_trigger::{ReflectionTrigger, ReflectionTriggerReason};
use ao_protocol::thread::ThreadKind;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::{Duration, Utc};

use crate::memory_promotion::MemoryPromotionJudge;

use super::*;

async fn setup_persistence() -> (tempfile::TempDir, Arc<PersistenceLayer>) {
    let tmp = tempfile::tempdir().unwrap();
    let data_root = DataRoot::new(tmp.path());
    let persistence = Arc::new(PersistenceLayer::init_with_root(data_root).await.unwrap());
    (tmp, persistence)
}

fn make_agent(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Agent {id}"),
        description: "test agent".to_string(),
        emoji: None,
        provider: ProviderConfig::Cli(CliProviderConfig {
            command: "echo".to_string(),
            args: vec![],
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
        enabled_plugins: HashMap::new(),
        enabled_launchpad_global_skills: None,
        enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
        owning_team_id: None,
        native_provider: None,
        thinking: None,
        max_output_tokens: None,
        max_context_tokens: None,
        reasoning_effort: None,
        delegates_to: vec![],
        persona: None,
        special_instructions: None,
        legacy_system_prompt: None,
        max_delegation_depth: None,
        channels: vec![],
        max_turns: None,
    }
}

fn entry(ts: chrono::DateTime<Utc>, content: &str) -> TranscriptEntry {
    TranscriptEntry {
        ts,
        role: TranscriptRole::System("user".to_string()),
        content: content.to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    }
}

fn turn(text: &str) -> Vec<CompletionEvent> {
    vec![
        CompletionEvent::AssistantText(text.to_string()),
        CompletionEvent::TurnComplete {
            stop_reason: StopReason::Natural,
        },
    ]
}

/// A resolver that always hands back the same scripted provider, and records
/// every `AgentProfile.id` it was asked to resolve for.
fn recording_resolver(
    provider: Arc<MockProviderClient>,
    seen: Arc<Mutex<Vec<String>>>,
) -> ProviderResolver {
    Arc::new(move |profile: &AgentProfile| {
        seen.lock().unwrap().push(profile.id.clone());
        Some(provider.clone() as Arc<dyn ProviderClient>)
    })
}

fn trigger_for(agent_id: &str, transcript_path: &str) -> ReflectionTrigger {
    ReflectionTrigger {
        reason: ReflectionTriggerReason::AnchorRotated,
        agent_id: agent_id.to_string(),
        transcript_path: transcript_path.to_string(),
        ts: Utc::now(),
    }
}

fn trigger_with_reason(
    reason: ReflectionTriggerReason,
    agent_id: &str,
    transcript_path: &str,
) -> ReflectionTrigger {
    ReflectionTrigger {
        reason,
        agent_id: agent_id.to_string(),
        transcript_path: transcript_path.to_string(),
        ts: Utc::now(),
    }
}

/// Writes a thread-scope `MemoryEntry` straight to its JSONL file, bypassing
/// `MemoryStore::add_thread` (which always stamps `Utc::now()`), so a test
/// can construct an entry old enough to already clear
/// `crate::memory_promotion::MIN_PROMOTION_SURVIVAL` without a real-time
/// sleep. `tmp` must be the same [`tempfile::TempDir`] the test's
/// `PersistenceLayer` was built from, so the two agree on where thread
/// memory lives on disk.
async fn backdated_thread_entry(
    tmp: &tempfile::TempDir,
    thread_id: &str,
    content: &str,
    age: Duration,
) -> ao_protocol::memory::MemoryEntry {
    let data_root = DataRoot::new(tmp.path());
    let path = data_root.memory_thread_path(thread_id);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.unwrap();
    }
    let ts = Utc::now() - age;
    let entry = ao_protocol::memory::MemoryEntry {
        id: Uuid::new_v4().to_string(),
        content: content.to_string(),
        created_at: ts,
        source: Some(MemorySource::Agent),
        scope: MemoryScope::Thread,
        scope_key: Some(thread_id.to_string()),
        updated_at: ts,
        deleted_at: None,
        confidence: 1.0,
        status: MemoryStatus::Active,
        superseded_by: None,
        pinned: false,
        decay_score: 1.0,
    };
    let line = serde_json::to_string(&entry).unwrap();
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .unwrap();
    file.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    entry
}

// --- (a) reads the transcript_path delta, not a trimmed window; advances --
// --- the watermark -----------------------------------------------------

#[tokio::test]
async fn empty_delta_is_a_cheap_no_op_that_never_touches_the_provider() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    let now = Utc::now();
    persistence
        .transcripts
        .append("agent-1", &entry(now, "hello"))
        .await
        .unwrap();
    // Watermark already at/after every entry -> delta is empty.
    persistence
        .threads
        .advance_distillation_watermark(&thread.id, now + Duration::seconds(1))
        .await
        .unwrap();

    // Zero scripted turns: if `run()` ever called the provider this would
    // surface as a ScriptExhausted error instead of a clean no-op.
    let provider = Arc::new(MockProviderClient::new(vec![]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    let outcome = subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();
    assert_eq!(outcome.candidates_staged, 0);
    assert_eq!(outcome.advanced_watermark_to, None);
}

#[tokio::test]
async fn reads_only_the_delta_since_the_watermark_and_advances_to_its_end() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    let t0 = Utc::now() - Duration::minutes(10);
    let t1 = t0 + Duration::minutes(1); // pre-watermark, must be excluded
    let t2 = t0 + Duration::minutes(2); // in delta
    let t3 = t0 + Duration::minutes(3); // in delta, latest

    persistence.transcripts.append("agent-1", &entry(t1, "old content, already distilled")).await.unwrap();
    persistence.transcripts.append("agent-1", &entry(t2, "new content one")).await.unwrap();
    persistence.transcripts.append("agent-1", &entry(t3, "new content two")).await.unwrap();

    persistence
        .threads
        .advance_distillation_watermark(&thread.id, t1)
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn("[]")]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider.clone(), seen),
    );

    let outcome = subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();

    // The watermark advanced to the LAST entry actually in the delta (t3),
    // not to "now" or to the pre-watermark entry — proving the delta slice
    // (not some other window) drove the pass.
    assert_eq!(outcome.advanced_watermark_to, Some(t3));
    assert_eq!(provider.remaining_turns(), 0, "the provider must have been called exactly once");

    let reloaded = persistence.threads.get(&thread.id).await.unwrap().unwrap();
    assert_eq!(reloaded.distilled_through_ts, Some(t3));
}

#[tokio::test]
async fn unknown_transcript_path_errors_without_advancing_anything() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    let err = subscriber
        .run(trigger_for("agent-1", "/nowhere/unknown.jsonl"))
        .await
        .unwrap_err();
    assert!(err.contains("no thread found"));
}

// --- (b) a proposed candidate lands in the trust gate staged/not-live ----

#[tokio::test]
async fn a_proposed_memory_candidate_is_staged_pending_and_never_written_live() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "user: I like concise commit messages"))
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"[{"kind":"memory","content":"User prefers concise commit messages."}]"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    let outcome = subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();
    assert_eq!(outcome.candidates_staged, 1);

    let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, ReflectionCandidateStatus::Pending);
    assert_eq!(pending[0].kind, ArtifactKind::Memory);
    assert_eq!(pending[0].content, "User prefers concise commit messages.");

    // Never applied to the live memory store.
    assert!(persistence.memory.list("agent-1").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_proposed_skill_candidate_is_staged_with_no_contradiction_check() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "ran the build-verify-fix loop three times"))
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"[{"kind":"skill","content":"Run cargo build, then cargo test, fix, repeat."}]"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();

    let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, ArtifactKind::Skill);
    assert_eq!(pending[0].contradicts, None);
}

// --- (b.1) confidence-gated routing to Thread scope --------

#[tokio::test]
async fn below_threshold_memory_candidate_lands_in_thread_scope_not_staging() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "a one-off detail specific to this chat"))
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"[{"kind":"memory","content":"Only relevant to this thread.","confidence":0.1}]"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    let outcome = subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();
    assert_eq!(outcome.candidates_staged, 1, "still counted as handled, even though not staged");

    // Never reaches the durable staging store.
    let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
    assert!(pending.is_empty(), "a below-threshold candidate must not appear in staging");

    // Lands in the originating thread's ephemeral Thread-scope memory instead.
    let thread_entries = persistence.memory.list_thread(&thread.id).await.unwrap();
    assert_eq!(thread_entries.len(), 1);
    assert_eq!(thread_entries[0].content, "Only relevant to this thread.");

    // Never applied to durable agent-scope memory either.
    assert!(persistence.memory.list("agent-1").await.unwrap().is_empty());
}

#[tokio::test]
async fn at_threshold_memory_candidate_still_reaches_staging_unchanged() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "a durable, generally useful preference"))
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"[{"kind":"memory","content":"Generally useful across threads.","confidence":0.5}]"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    let outcome = subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();
    assert_eq!(outcome.candidates_staged, 1);

    let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1, "a confidence exactly at the threshold keeps today's path");
    assert_eq!(pending[0].content, "Generally useful across threads.");
    assert_eq!(pending[0].status, ReflectionCandidateStatus::Pending);

    // Never lands in thread memory.
    assert!(persistence.memory.list_thread(&thread.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn above_threshold_memory_candidate_still_reaches_staging_unchanged() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "a clearly recurring convention"))
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"[{"kind":"memory","content":"Confidently durable.","confidence":0.9}]"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();

    let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].content, "Confidently durable.");

    assert!(persistence.memory.list_thread(&thread.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn below_threshold_skill_candidate_is_unaffected_and_still_stages() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "ran a one-off procedure"))
        .await
        .unwrap();

    // Skill candidates have no thread-scope tier to route into, so a low
    // confidence must not change their path at all.
    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"[{"kind":"skill","content":"A low-confidence procedure.","confidence":0.1}]"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();

    let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, ArtifactKind::Skill);

    assert!(persistence.memory.list_thread(&thread.id).await.unwrap().is_empty());
}

// --- (c) a memory candidate contradicting a Manual entry is flagged, -----
// --- not applied ---------------------------------------------------------

#[tokio::test]
async fn memory_candidate_contradicting_a_manual_entry_is_flagged_and_never_applied() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    let manual = persistence
        .memory
        .add("agent-1", "User prefers tabs over spaces", MemorySource::Manual)
        .await
        .unwrap();

    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "user restated their indentation preference"))
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"[{"kind":"memory","content":"User prefers tabs over spaces for indentation."}]"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();

    let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].contradicts, Some(manual.id.clone()));
    assert_eq!(pending[0].status, ReflectionCandidateStatus::Pending);

    // The Manual entry itself must be untouched — never superseded/applied over.
    let live = persistence.memory.list("agent-1").await.unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, manual.id);
    assert_eq!(live[0].status, ao_protocol::memory::MemoryStatus::Active);
    assert_eq!(live[0].superseded_by, None);
}

// --- (d) consumes an injected provider / resolved profile; never builds --
// --- its own client -------------------------------------------------------

#[tokio::test]
async fn run_only_ever_drives_the_provider_handed_back_by_the_resolver() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "some content"))
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn("[]")]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(Arc::clone(&provider), Arc::clone(&seen)),
    );

    subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();

    // The resolver was invoked exactly once, and the exact scripted turn it
    // handed back was the one consumed — proving the pass never constructs
    // (or reaches) any other provider client.
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(provider.remaining_turns(), 0);
}

#[tokio::test]
async fn no_provider_from_the_resolver_errors_without_advancing_the_watermark() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "some content"))
        .await
        .unwrap();

    let resolver: ProviderResolver = Arc::new(|_profile: &AgentProfile| None);
    let subscriber = ReflectionSubscriber::new(Arc::clone(&persistence), resolver);

    let err = subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap_err();
    assert!(err.contains("no provider configured"));

    let reloaded = persistence.threads.get(&thread.id).await.unwrap().unwrap();
    assert!(
        reloaded.distilled_through_ts.is_none(),
        "a failed pass must leave the watermark untouched so the same delta retries next trigger"
    );
}

// --- (e) reflection_agent_id preference selects the profile; falls back --
// --- to the thread's own agent when unset ---------------------------------

#[tokio::test]
async fn unset_reflection_agent_id_falls_back_to_the_threads_own_agent() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    persistence.agents.create(&make_agent("cheap-reflector")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "some content"))
        .await
        .unwrap();

    // No preferences saved at all -> UserPreferences::default() -> None.
    let provider = Arc::new(MockProviderClient::new(vec![turn("[]")]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, Arc::clone(&seen)),
    );

    subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();

    assert_eq!(seen.lock().unwrap().as_slice(), ["agent-1"]);
}

#[tokio::test]
async fn set_reflection_agent_id_selects_that_profile_instead() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    persistence.agents.create(&make_agent("cheap-reflector")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "some content"))
        .await
        .unwrap();

    persistence
        .preferences
        .save(&UserPreferences {
            reflection_agent_id: Some("cheap-reflector".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn("[]")]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, Arc::clone(&seen)),
    );

    subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();

    assert_eq!(seen.lock().unwrap().as_slice(), ["cheap-reflector"]);
}

#[tokio::test]
async fn missing_reflection_agent_id_profile_errors_without_advancing_the_watermark() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "some content"))
        .await
        .unwrap();

    persistence
        .preferences
        .save(&UserPreferences {
            reflection_agent_id: Some("does-not-exist".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    let err = subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap_err();
    assert!(err.contains("does-not-exist"));

    let reloaded = persistence.threads.get(&thread.id).await.unwrap().unwrap();
    assert!(reloaded.distilled_through_ts.is_none());
}

// --- on_reflection_trigger runs off-turn (fire-and-forget) ---------------

#[tokio::test]
async fn on_reflection_trigger_returns_immediately_and_still_completes_the_pass() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "some content"))
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"[{"kind":"memory","content":"Something worth keeping."}]"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    // The trait method is sync and must return without awaiting the pass.
    subscriber.on_reflection_trigger(trigger_for("agent-1", &thread.transcript_path));

    // Give the spawned task a chance to run to completion.
    for _ in 0..50 {
        if !persistence
            .reflection_staging
            .list_pending("agent-1")
            .await
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
}

// --- promotion judge fires only on thread archive ------------------------

#[tokio::test]
async fn archive_trigger_promotes_a_thread_scope_entry_into_the_staging_queue() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    // A thread-scope note the low-confidence path would have written on
    // an earlier trigger — never staged, never live, just sitting in the
    // ephemeral Thread-scope tier.
    write_thread_entry(&persistence.memory, &thread.id, "The user likes tabs, not spaces.", None)
        .await
        .unwrap();

    // Pass 1: empty transcript delta -> cheap no-op, never touches its
    // provider (an empty script would error if it somehow were called).
    let reflection_provider = Arc::new(MockProviderClient::new(vec![]));
    let seen = Arc::new(Mutex::new(Vec::new()));

    let judge_provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"promote","generalized_content":"Prefer tabs over spaces for indentation.","rationale":"durable preference"}"#,
    )]));
    let judge_seen = Arc::new(Mutex::new(Vec::new()));
    let judge = Arc::new(MemoryPromotionJudge::new(
        Arc::clone(&persistence),
        recording_resolver(judge_provider, judge_seen),
    ));

    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(reflection_provider, seen),
    )
    .with_promotion_judge(judge);

    subscriber.on_reflection_trigger(ReflectionTrigger {
        reason: ReflectionTriggerReason::Archived,
        agent_id: "agent-1".to_string(),
        transcript_path: thread.transcript_path.clone(),
        ts: Utc::now(),
    });

    // The sweep runs on a spawned task — poll for its staged output.
    let mut promoted = None;
    for _ in 0..50 {
        let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
        if let Some(c) = pending
            .into_iter()
            .find(|c| c.content == "Prefer tabs over spaces for indentation.")
        {
            promoted = Some(c);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let promoted = promoted.expect(
        "a thread-scope insight must be promoted into the durable-scope staging queue on thread archive",
    );
    assert_eq!(promoted.target_scope, MemoryScope::Agent);
    assert_eq!(promoted.source_thread_id, thread.id);
    assert_eq!(promoted.status, ReflectionCandidateStatus::Pending);
}

#[tokio::test]
async fn non_archive_triggers_skip_the_judge_until_an_entry_clears_the_survival_window() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    // Freshly written — nowhere near MIN_PROMOTION_SURVIVAL yet. A
    // non-Archived trigger DOES now run the periodic sweep (unlike before
    // this test's original assertion), but the survival gate keeps this
    // entry out of the judge's hands until it settles.
    write_thread_entry(&persistence.memory, &thread.id, "Some thread-scope note.", None)
        .await
        .unwrap();

    let reflection_provider = Arc::new(MockProviderClient::new(vec![]));
    let seen = Arc::new(Mutex::new(Vec::new()));

    // Scripted with a turn the judge would consume IF it were ever called —
    // asserting it is untouched below proves the gate, not just an absence
    // of a crash.
    let judge_provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"promote","generalized_content":"Should never be reached.","rationale":"n/a"}"#,
    )]));
    let judge_seen = Arc::new(Mutex::new(Vec::new()));
    let judge = Arc::new(MemoryPromotionJudge::new(
        Arc::clone(&persistence),
        recording_resolver(judge_provider.clone(), Arc::clone(&judge_seen)),
    ));

    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(reflection_provider, seen),
    )
    .with_promotion_judge(judge);

    for reason in [ReflectionTriggerReason::AnchorRotated, ReflectionTriggerReason::IdleTimeout] {
        subscriber.on_reflection_trigger(trigger_with_reason(reason, "agent-1", &thread.transcript_path));
    }

    // Give any spawned task a chance to run.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        judge_seen.lock().unwrap().is_empty(),
        "an entry under MIN_PROMOTION_SURVIVAL must never reach the judge"
    );
    assert_eq!(
        judge_provider.remaining_turns(),
        1,
        "the judge's scripted turn must be untouched — a too-new entry must never invoke it"
    );
    assert!(persistence.reflection_staging.list_pending("agent-1").await.unwrap().is_empty());
}

#[tokio::test]
async fn default_thread_periodic_sweep_promotes_an_eligible_entry_without_archiving() {
    let (tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    assert_eq!(thread.kind, ThreadKind::Default);

    // Old enough to already clear MIN_PROMOTION_SURVIVAL (10 min).
    backdated_thread_entry(&tmp, &thread.id, "The user likes tabs, not spaces.", Duration::minutes(11))
        .await;

    let reflection_provider = Arc::new(MockProviderClient::new(vec![]));
    let seen = Arc::new(Mutex::new(Vec::new()));

    let judge_provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"promote","generalized_content":"Prefer tabs over spaces for indentation.","rationale":"durable preference"}"#,
    )]));
    let judge_seen = Arc::new(Mutex::new(Vec::new()));
    let judge = Arc::new(MemoryPromotionJudge::new(
        Arc::clone(&persistence),
        recording_resolver(judge_provider, judge_seen),
    ));

    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(reflection_provider, seen),
    )
    .with_promotion_judge(judge);

    // A `Default` thread can never archive (`ThreadStore::archive` refuses
    // it) — this fires an ordinary, non-Archived reflection trigger, the
    // only kind a `Default` thread ever gets.
    subscriber.on_reflection_trigger(trigger_with_reason(
        ReflectionTriggerReason::AnchorRotated,
        "agent-1",
        &thread.transcript_path,
    ));

    let mut promoted = None;
    for _ in 0..50 {
        let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
        if let Some(c) = pending
            .into_iter()
            .find(|c| c.content == "Prefer tabs over spaces for indentation.")
        {
            promoted = Some(c);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let promoted = promoted.expect(
        "a Default thread must promote an eligible thread-scope insight via the periodic sweep, \
         without ever being archived",
    );
    assert_eq!(promoted.target_scope, MemoryScope::Agent);
    assert_eq!(promoted.source_thread_id, thread.id);

    let reloaded = persistence.threads.get(&thread.id).await.unwrap().unwrap();
    assert!(reloaded.archived_at.is_none(), "the thread must never have been archived");
    assert!(
        reloaded.promotion_swept_at.is_some(),
        "the periodic sweep must advance the debounce watermark"
    );
}

#[tokio::test]
async fn periodic_sweep_debounces_within_the_interval_and_does_not_rejudge() {
    let (tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    backdated_thread_entry(&tmp, &thread.id, "An old, stable note.", Duration::minutes(11)).await;

    let reflection_provider = Arc::new(MockProviderClient::new(vec![]));
    let seen = Arc::new(Mutex::new(Vec::new()));

    // Exactly ONE scripted turn: if the judge were invoked a second time
    // within the debounce interval, the mock has nothing left to return and
    // `judge.promote` would error (logged, not panicked) instead of
    // recording a second profile resolution.
    let judge_provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"promote","generalized_content":"A durable preference.","rationale":"n/a"}"#,
    )]));
    let judge_seen = Arc::new(Mutex::new(Vec::new()));
    let judge = Arc::new(MemoryPromotionJudge::new(
        Arc::clone(&persistence),
        recording_resolver(judge_provider, Arc::clone(&judge_seen)),
    ));

    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(reflection_provider, seen),
    )
    .with_promotion_judge(judge);

    subscriber.on_reflection_trigger(trigger_with_reason(
        ReflectionTriggerReason::AnchorRotated,
        "agent-1",
        &thread.transcript_path,
    ));

    // Wait for the first (due) sweep to actually judge the entry.
    for _ in 0..50 {
        if !judge_seen.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        judge_seen.lock().unwrap().len(),
        1,
        "the first due sweep must judge the eligible entry"
    );

    // A second trigger fired immediately after — still well within
    // PROMOTION_SWEEP_INTERVAL (30 min) — must not run the judge again.
    subscriber.on_reflection_trigger(trigger_with_reason(
        ReflectionTriggerReason::IdleTimeout,
        "agent-1",
        &thread.transcript_path,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert_eq!(
        judge_seen.lock().unwrap().len(),
        1,
        "a second trigger within PROMOTION_SWEEP_INTERVAL must not re-invoke the judge"
    );
    assert_eq!(
        persistence.reflection_staging.list_pending("agent-1").await.unwrap().len(),
        1,
        "a debounced re-sweep must not stage a duplicate candidate"
    );
}

#[tokio::test]
async fn a_recently_edited_thread_entry_is_not_promoted_even_if_originally_old() {
    let (tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    let old_entry =
        backdated_thread_entry(&tmp, &thread.id, "Original note.", Duration::minutes(11)).await;

    // A later thread-scope correction bumps `updated_at` to now (thread
    // scope has no supersede/contradiction tracking of its own — see
    // `MemoryStore`'s "Thread scope" section — so a contradiction surfaces
    // as an edit like this one), resetting the survival window before this
    // note ever reaches the judge.
    persistence
        .memory
        .edit_thread(&thread.id, &old_entry.id, "Corrected note.")
        .await
        .unwrap();

    let reflection_provider = Arc::new(MockProviderClient::new(vec![]));
    let seen = Arc::new(Mutex::new(Vec::new()));

    let judge_provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"promote","generalized_content":"Should never be reached.","rationale":"n/a"}"#,
    )]));
    let judge_seen = Arc::new(Mutex::new(Vec::new()));
    let judge = Arc::new(MemoryPromotionJudge::new(
        Arc::clone(&persistence),
        recording_resolver(judge_provider.clone(), Arc::clone(&judge_seen)),
    ));

    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(reflection_provider, seen),
    )
    .with_promotion_judge(judge);

    subscriber.on_reflection_trigger(trigger_with_reason(
        ReflectionTriggerReason::AnchorRotated,
        "agent-1",
        &thread.transcript_path,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        judge_seen.lock().unwrap().is_empty(),
        "a just-edited entry must not be promoted, even though it was originally old enough"
    );
    assert_eq!(judge_provider.remaining_turns(), 1);
}

// --- over-cap thread-route drop is observable, not counted as staged -----

#[tokio::test]
async fn over_cap_thread_route_candidate_is_dropped_not_counted_as_staged() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "a very long one-off detail"))
        .await
        .unwrap();

    // Over THREAD_ENTRY_CHAR_HARD (2000) so `route_to_thread_memory` drops
    // it instead of writing it to thread memory.
    let oversized_content = "x".repeat(2001);
    let provider = Arc::new(MockProviderClient::new(vec![turn(&format!(
        r#"[{{"kind":"memory","content":"{oversized_content}","confidence":0.1}}]"#
    ))]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    let outcome = subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();

    assert_eq!(
        outcome.candidates_staged, 0,
        "content that was actually thrown away must never inflate candidates_staged"
    );
    assert_eq!(outcome.dropped_count, 1, "the drop must be observable via dropped_count");

    assert!(persistence.memory.list_thread(&thread.id).await.unwrap().is_empty());
    assert!(persistence.reflection_staging.list_pending("agent-1").await.unwrap().is_empty());
}

// --- dedup also checks candidates already Pending in staging -------------

#[tokio::test]
async fn duplicate_of_a_pending_staged_candidate_is_not_appended_twice() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "user restated their commit message preference"))
        .await
        .unwrap();

    // Two turns, each proposing an (almost) identical memory candidate —
    // simulating two separate reflection triggers firing before either
    // candidate has been reviewed.
    let provider = Arc::new(MockProviderClient::new(vec![
        turn(r#"[{"kind":"memory","content":"User prefers concise commit messages."}]"#),
        turn(r#"[{"kind":"memory","content":"User prefers concise commit messages."}]"#),
    ]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscriber = ReflectionSubscriber::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    let first = subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();
    assert_eq!(first.candidates_staged, 1);
    assert_eq!(first.dropped_count, 0);

    // A second entry so the delta is non-empty on the next pass (the
    // watermark advanced past the first).
    persistence
        .transcripts
        .append("agent-1", &entry(Utc::now(), "user restated it again"))
        .await
        .unwrap();

    let second = subscriber
        .run(trigger_for("agent-1", &thread.transcript_path))
        .await
        .unwrap();
    assert_eq!(
        second.candidates_staged, 0,
        "a near-duplicate of an already-pending candidate must not be staged again"
    );
    assert_eq!(second.dropped_count, 1);

    let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1, "only the first candidate may remain staged");
    assert_eq!(pending[0].content, "User prefers concise commit messages.");
}
