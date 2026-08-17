use std::collections::HashMap;
use std::sync::Arc;

use ao_engine_tools_core::skill_registry::{SkillEntry, SkillRegistry};
use ao_engine_tools_core::{
    AgentProfileCacheInvalidator, EngineTool, NoopAgentProfileCacheInvalidator, RunnerContext,
    ToolOutput,
};
use ao_persistence::paths::DataRoot;
use ao_persistence::profiles::AgentProfileStore;
use ao_persistence::snapshot::SnapshotStore;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Mutex;

use super::AgentAuthor;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn make_profile(id: &str, name: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: name.to_string(),
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

/// Records every `invalidate` call so tests can assert the cache-invalidation
/// seam actually fires (rather than only trusting the return value).
#[derive(Default)]
struct SpyInvalidator {
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl AgentProfileCacheInvalidator for SpyInvalidator {
    async fn invalidate(&self, agent_id: &str) {
        self.calls.lock().await.push(agent_id.to_string());
    }
}

async fn setup(tmp: &TempDir) -> (Arc<AgentProfileStore>, Arc<SnapshotStore>) {
    let data_root = DataRoot::new(tmp.path());
    std::fs::create_dir_all(data_root.agents_dir()).unwrap();
    let store = Arc::new(AgentProfileStore::new(data_root.clone()));
    let snapshots = Arc::new(SnapshotStore::load(data_root).await.unwrap());
    (store, snapshots)
}

fn make_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("session", "caller-agent", std::env::temp_dir())
}

fn make_ctx_with_skill(skill_name: &str) -> RunnerContext {
    let mut registry = SkillRegistry::empty();
    registry.insert(skill_name.to_string(), SkillEntry::Err("stub entry".to_string()));
    make_ctx().with_skill_registry(Arc::new(registry))
}

// ─── create ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_writes_profile_and_returns_id() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    let tool = AgentAuthor::with_deps(
        Arc::clone(&store),
        Arc::clone(&snapshots),
        Arc::new(NoopAgentProfileCacheInvalidator),
    );
    let ctx = make_ctx();

    let out = tool
        .invoke(
            json!({"op": "create", "name": "Researcher", "description": "Finds things out"}),
            &ctx,
        )
        .await
        .unwrap();

    let id = match out {
        ToolOutput::Structured(v) => v["id"].as_str().unwrap().to_string(),
        other => panic!("expected structured output, got {other:?}"),
    };
    assert!(!id.is_empty());

    let persisted = store.get(&id).await.unwrap().expect("profile should exist on disk");
    assert_eq!(persisted.name, "Researcher");
    assert_eq!(persisted.description, "Finds things out");
    assert_eq!(persisted.template.as_deref(), Some("claude"));

    // Home directory scaffolding ran.
    let home = store.data_root().agent_home_dir(&id);
    assert!(home.join("skills").is_dir());
    assert!(home.join("rules").is_dir());

    // Snapshot entry was synced.
    let snapshot = snapshots.get().await;
    assert_eq!(snapshot.agents.get(&id).unwrap().name, "Researcher");
}

#[tokio::test]
async fn create_without_name_is_a_recoverable_error() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    let tool = AgentAuthor::with_deps(store, snapshots, Arc::new(NoopAgentProfileCacheInvalidator));
    let ctx = make_ctx();

    let out = tool
        .invoke(json!({"op": "create", "description": "no name"}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected error output, got {other:?}"),
    }
}

// ─── update ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn update_persona_archives_prior_value_and_invalidates_cache() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    let mut profile = make_profile("agent-1", "Assistant");
    profile.persona = Some("Old persona".to_string());
    profile.special_instructions = Some("Old rules".to_string());
    store.create(&profile).await.unwrap();

    let invalidator = Arc::new(SpyInvalidator::default());
    let tool = AgentAuthor::with_deps(
        Arc::clone(&store),
        Arc::clone(&snapshots),
        invalidator.clone() as Arc<dyn AgentProfileCacheInvalidator>,
    );
    let ctx = make_ctx();

    let out = tool
        .invoke(json!({"op": "update", "id": "agent-1", "persona": "New persona"}), &ctx)
        .await
        .unwrap();

    let updated = match out {
        ToolOutput::Structured(v) => v,
        other => panic!("expected structured output, got {other:?}"),
    };
    assert_eq!(updated["persona"], "New persona");
    assert_eq!(
        updated["legacy_system_prompt"],
        "Old persona\n\nOld rules",
        "prior persona+special_instructions must be archived before the new persona lands"
    );
    // special_instructions was not part of this call — patch semantics leave it untouched.
    assert_eq!(updated["special_instructions"], "Old rules");

    let persisted = store.get("agent-1").await.unwrap().unwrap();
    assert_eq!(persisted.persona.as_deref(), Some("New persona"));
    assert_eq!(
        persisted.legacy_system_prompt.as_deref(),
        Some("Old persona\n\nOld rules")
    );

    assert_eq!(invalidator.calls.lock().await.as_slice(), ["agent-1"]);
}

#[tokio::test]
async fn update_archives_legacy_system_prompt_when_persona_fields_were_unset() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    let mut profile = make_profile("agent-2", "Legacy Agent");
    profile.system_prompt = Some("You are a legacy-prompted agent.".to_string());
    store.create(&profile).await.unwrap();

    let tool = AgentAuthor::with_deps(
        Arc::clone(&store),
        snapshots,
        Arc::new(NoopAgentProfileCacheInvalidator),
    );
    let ctx = make_ctx();

    tool.invoke(
        json!({"op": "update", "id": "agent-2", "persona": "Freshly composed persona"}),
        &ctx,
    )
    .await
    .unwrap();

    let persisted = store.get("agent-2").await.unwrap().unwrap();
    assert_eq!(
        persisted.legacy_system_prompt.as_deref(),
        Some("You are a legacy-prompted agent.")
    );
    assert_eq!(persisted.persona.as_deref(), Some("Freshly composed persona"));
}

#[tokio::test]
async fn update_missing_agent_is_a_recoverable_error() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    let tool = AgentAuthor::with_deps(store, snapshots, Arc::new(NoopAgentProfileCacheInvalidator));
    let ctx = make_ctx();

    let out = tool
        .invoke(json!({"op": "update", "id": "does-not-exist", "name": "x"}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected error output, got {other:?}"),
    }
}

// ─── capability gate ─────────────────────────────────────────────────────────

#[tokio::test]
async fn skills_untouched_when_allow_capability_changes_is_absent() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    let mut profile = make_profile("agent-3", "Toolsmith");
    profile.skills = vec!["existing-skill".to_string()];
    store.create(&profile).await.unwrap();

    let tool = AgentAuthor::with_deps(
        Arc::clone(&store),
        snapshots,
        Arc::new(NoopAgentProfileCacheInvalidator),
    );
    // No skill registered here — if the gate were bypassed, this call would
    // otherwise fail on unknown-skill validation, proving the field was
    // genuinely skipped rather than merely empty-validated.
    let ctx = make_ctx();

    let out = tool
        .invoke(
            json!({"op": "update", "id": "agent-3", "skills": ["totally-unknown-skill"]}),
            &ctx,
        )
        .await
        .unwrap();

    assert!(matches!(out, ToolOutput::Structured(_)));
    let persisted = store.get("agent-3").await.unwrap().unwrap();
    assert_eq!(
        persisted.skills,
        vec!["existing-skill".to_string()],
        "skills must be untouched when allow_capability_changes is not set"
    );
}

#[tokio::test]
async fn unknown_skill_is_rejected_when_capability_changes_allowed() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    let profile = make_profile("agent-4", "Toolsmith");
    store.create(&profile).await.unwrap();

    let tool = AgentAuthor::with_deps(
        Arc::clone(&store),
        snapshots,
        Arc::new(NoopAgentProfileCacheInvalidator),
    );
    let ctx = make_ctx_with_skill("known-skill");

    let out = tool
        .invoke(
            json!({
                "op": "update",
                "id": "agent-4",
                "allow_capability_changes": true,
                "skills": ["known-skill", "unknown-skill"]
            }),
            &ctx,
        )
        .await
        .unwrap();

    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(recoverable);
            assert!(message.contains("unknown-skill"));
        }
        other => panic!("expected error output, got {other:?}"),
    }

    // The rejected call must not have written a partial skills list.
    let persisted = store.get("agent-4").await.unwrap().unwrap();
    assert!(persisted.skills.is_empty());
}

#[tokio::test]
async fn known_skills_apply_when_capability_changes_allowed() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    let profile = make_profile("agent-5", "Toolsmith");
    store.create(&profile).await.unwrap();

    let tool = AgentAuthor::with_deps(
        Arc::clone(&store),
        snapshots,
        Arc::new(NoopAgentProfileCacheInvalidator),
    );
    let ctx = make_ctx_with_skill("known-skill");

    let out = tool
        .invoke(
            json!({
                "op": "update",
                "id": "agent-5",
                "allow_capability_changes": true,
                "skills": ["known-skill"]
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(matches!(out, ToolOutput::Structured(_)));
    let persisted = store.get("agent-5").await.unwrap().unwrap();
    assert_eq!(persisted.skills, vec!["known-skill".to_string()]);
}

// ─── get / list ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_returns_full_profile() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    let profile = make_profile("agent-6", "Reader");
    store.create(&profile).await.unwrap();

    let tool = AgentAuthor::with_deps(store, snapshots, Arc::new(NoopAgentProfileCacheInvalidator));
    let ctx = make_ctx();

    let out = tool.invoke(json!({"op": "get", "id": "agent-6"}), &ctx).await.unwrap();
    let value = match out {
        ToolOutput::Structured(v) => v,
        other => panic!("expected structured output, got {other:?}"),
    };
    assert_eq!(value["id"], "agent-6");
    assert_eq!(value["name"], "Reader");
}

#[tokio::test]
async fn get_missing_agent_is_a_recoverable_error() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    let tool = AgentAuthor::with_deps(store, snapshots, Arc::new(NoopAgentProfileCacheInvalidator));
    let ctx = make_ctx();

    let out = tool
        .invoke(json!({"op": "get", "id": "does-not-exist"}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Error { recoverable, .. } => assert!(recoverable),
        other => panic!("expected error output, got {other:?}"),
    }
}

#[tokio::test]
async fn list_returns_every_agent_summary() {
    let tmp = TempDir::new().unwrap();
    let (store, snapshots) = setup(&tmp).await;
    store.create(&make_profile("agent-7", "First")).await.unwrap();
    store.create(&make_profile("agent-8", "Second")).await.unwrap();

    let tool = AgentAuthor::with_deps(store, snapshots, Arc::new(NoopAgentProfileCacheInvalidator));
    let ctx = make_ctx();

    let out = tool.invoke(json!({"op": "list"}), &ctx).await.unwrap();
    let agents = match out {
        ToolOutput::Structured(v) => v["agents"].as_array().unwrap().clone(),
        other => panic!("expected structured output, got {other:?}"),
    };
    assert_eq!(agents.len(), 2);
    let ids: Vec<&str> = agents.iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"agent-7"));
    assert!(ids.contains(&"agent-8"));
}

// ─── stub (no deps configured) ─────────────────────────────────────────────

#[tokio::test]
async fn stub_without_deps_returns_recoverable_error() {
    let tool = AgentAuthor::new();
    let ctx = make_ctx();

    let out = tool.invoke(json!({"op": "list"}), &ctx).await.unwrap();
    match out {
        ToolOutput::Error { recoverable, message } => {
            assert!(!recoverable, "unwired stub is not something the model can retry past");
            assert!(message.contains("AgentAuthor requires an agent store"));
        }
        other => panic!("expected error output, got {other:?}"),
    }
}
