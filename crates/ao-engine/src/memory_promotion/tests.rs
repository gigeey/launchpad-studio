use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ao_engine_tools_engine::memory::write_thread_entry;
use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};
use ao_persistence::paths::DataRoot;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_protocol::memory::{MemoryScope, MemorySource};
use ao_protocol::preferences::UserPreferences;

use super::*;

// --- MIN_PROMOTION_SURVIVAL / is_promotion_eligible (pure) -----------------

fn active_entry(updated_at: DateTime<Utc>) -> MemoryEntry {
    MemoryEntry {
        id: "entry-1".to_string(),
        content: "some content".to_string(),
        created_at: updated_at,
        source: Some(MemorySource::Agent),
        scope: MemoryScope::Thread,
        scope_key: Some("thread-1".to_string()),
        updated_at,
        deleted_at: None,
        confidence: 1.0,
        status: MemoryStatus::Active,
        superseded_by: None,
        pinned: false,
        decay_score: 1.0,
    }
}

#[test]
fn too_new_entry_is_not_promotion_eligible() {
    let now = Utc::now();
    let entry = active_entry(now - Duration::minutes(1));
    assert!(!is_promotion_eligible(&entry, now));
}

#[test]
fn entry_at_exactly_the_survival_window_is_eligible() {
    let now = Utc::now();
    let entry = active_entry(now - MIN_PROMOTION_SURVIVAL);
    assert!(is_promotion_eligible(&entry, now));
}

#[test]
fn entry_well_past_the_survival_window_is_eligible() {
    let now = Utc::now();
    let entry = active_entry(now - Duration::hours(1));
    assert!(is_promotion_eligible(&entry, now));
}

#[test]
fn a_recent_edit_resets_the_survival_window_even_for_an_old_entry() {
    let now = Utc::now();
    let mut entry = active_entry(now - Duration::hours(1));
    // Simulates a later thread-scope write correcting/contradicting this
    // entry via `MemoryStore::edit_thread`, which bumps `updated_at` to the
    // edit time without changing `created_at`.
    entry.updated_at = now - Duration::seconds(30);
    assert!(!is_promotion_eligible(&entry, now));
}

#[test]
fn non_active_entry_is_never_promotion_eligible() {
    let now = Utc::now();
    for status in [MemoryStatus::Superseded, MemoryStatus::Archived] {
        let mut entry = active_entry(now - Duration::hours(1));
        entry.status = status;
        assert!(!is_promotion_eligible(&entry, now));
    }
}

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

fn turn(text: &str) -> Vec<CompletionEvent> {
    vec![
        CompletionEvent::AssistantText(text.to_string()),
        CompletionEvent::TurnComplete {
            stop_reason: StopReason::Natural,
        },
    ]
}

/// A resolver that always hands back the same scripted provider, and
/// records every `AgentProfile.id` it was asked to resolve for — mirrors
/// `reflection_subscriber::tests::recording_resolver` exactly, proving this
/// orchestrator drives the model only through what the resolver hands back.
fn recording_resolver(
    provider: Arc<MockProviderClient>,
    seen: Arc<Mutex<Vec<String>>>,
) -> ProviderResolver {
    Arc::new(move |profile: &AgentProfile| {
        seen.lock().unwrap().push(profile.id.clone());
        Some(provider.clone() as Arc<dyn ao_engine_tools_runner::provider::ProviderClient>)
    })
}

async fn thread_entry(persistence: &PersistenceLayer, thread_id: &str, content: &str) -> ao_protocol::memory::MemoryEntry {
    write_thread_entry(&persistence.memory, thread_id, content, None)
        .await
        .unwrap();
    persistence
        .memory
        .list_thread(thread_id)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.content == content)
        .unwrap()
}

#[tokio::test]
async fn a_generalizable_entry_is_promoted_into_the_staging_store() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    let entry = thread_entry(&persistence, &thread.id, "The user likes tabs, not spaces, in this file.").await;

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"promote","generalized_content":"Prefer tabs over spaces for indentation.","rationale":"a stated durable preference"}"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let judge = MemoryPromotionJudge::new(
        Arc::clone(&persistence),
        recording_resolver(provider.clone(), seen),
    );

    let outcome = judge.promote("agent-1", &thread.id, &entry).await.unwrap();
    match outcome {
        PromotionOutcome::Promoted(candidate) => {
            assert_eq!(candidate.content, "Prefer tabs over spaces for indentation.");
            assert_eq!(candidate.agent_id, "agent-1");
            assert_eq!(candidate.source_thread_id, thread.id);
            assert_eq!(candidate.kind, ao_protocol::outcome::ArtifactKind::Memory);
        }
        PromotionOutcome::Rejected { rationale } => panic!("expected Promoted, got Rejected: {rationale}"),
    }

    let pending = persistence.reflection_staging.list_pending("agent-1").await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].content, "Prefer tabs over spaces for indentation.");
    assert_eq!(provider.remaining_turns(), 0);
}

#[tokio::test]
async fn a_thread_specific_entry_is_rejected_without_staging_anything() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();

    let entry = thread_entry(&persistence, &thread.id, "Ticket ABC-123 needed a manual DB fix.").await;

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"reject","rationale":"specific to this thread's one-off ticket"}"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let judge = MemoryPromotionJudge::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    let outcome = judge.promote("agent-1", &thread.id, &entry).await.unwrap();
    match outcome {
        PromotionOutcome::Rejected { rationale } => {
            assert!(rationale.contains("this thread"));
        }
        PromotionOutcome::Promoted(candidate) => panic!("expected Rejected, got Promoted: {candidate:?}"),
    }

    assert!(persistence.reflection_staging.list_pending("agent-1").await.unwrap().is_empty());
}

#[tokio::test]
async fn unset_reflection_agent_id_falls_back_to_the_entrys_own_agent() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    persistence.agents.create(&make_agent("cheap-reflector")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    let entry = thread_entry(&persistence, &thread.id, "some content").await;

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"reject","rationale":"n/a"}"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let judge = MemoryPromotionJudge::new(
        Arc::clone(&persistence),
        recording_resolver(provider, Arc::clone(&seen)),
    );

    judge.promote("agent-1", &thread.id, &entry).await.unwrap();

    assert_eq!(seen.lock().unwrap().as_slice(), ["agent-1"]);
}

#[tokio::test]
async fn set_reflection_agent_id_selects_that_profile_instead() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    persistence.agents.create(&make_agent("cheap-reflector")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    let entry = thread_entry(&persistence, &thread.id, "some content").await;

    persistence
        .preferences
        .save(&UserPreferences {
            reflection_agent_id: Some("cheap-reflector".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    let provider = Arc::new(MockProviderClient::new(vec![turn(
        r#"{"verdict":"reject","rationale":"n/a"}"#,
    )]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let judge = MemoryPromotionJudge::new(
        Arc::clone(&persistence),
        recording_resolver(provider, Arc::clone(&seen)),
    );

    judge.promote("agent-1", &thread.id, &entry).await.unwrap();

    assert_eq!(seen.lock().unwrap().as_slice(), ["cheap-reflector"]);
}

#[tokio::test]
async fn no_provider_from_the_resolver_errors_without_staging_anything() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    let entry = thread_entry(&persistence, &thread.id, "some content").await;

    let resolver: ProviderResolver = Arc::new(|_profile: &AgentProfile| None);
    let judge = MemoryPromotionJudge::new(Arc::clone(&persistence), resolver);

    let err = judge.promote("agent-1", &thread.id, &entry).await.unwrap_err();
    assert!(err.contains("no provider configured"));
    assert!(persistence.reflection_staging.list_pending("agent-1").await.unwrap().is_empty());
}

#[tokio::test]
async fn missing_reflection_agent_id_profile_errors() {
    let (_tmp, persistence) = setup_persistence().await;
    persistence.agents.create(&make_agent("agent-1")).await.unwrap();
    let thread = persistence.threads.ensure_default_thread("agent-1").await.unwrap();
    let entry = thread_entry(&persistence, &thread.id, "some content").await;

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
    let judge = MemoryPromotionJudge::new(
        Arc::clone(&persistence),
        recording_resolver(provider, seen),
    );

    let err = judge.promote("agent-1", &thread.id, &entry).await.unwrap_err();
    assert!(err.contains("does-not-exist"));
}
