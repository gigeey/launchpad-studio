//! Unit tests for the background-agent spawner.
//!
//! Declared from the parent module as `#[cfg(test)] mod tests;` — this is
//! the same module as the inline `mod tests` block it replaces, so private
//! items of the parent remain in scope here via `use super::*`.

use super::*;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use ao_protocol::error::AoError;

use crate::background_agents::{
    BackgroundAgentId, BackgroundAgentRegistry, SubagentDefinition,
};
use crate::context::RunnerContext;
use crate::memory_loader::StaticMemoryLoader;
use crate::output::ToolOutput;
use crate::registry::Registry;
use crate::tool::{EngineTool, IoTool};

/// A registry seeded with a single "Explore" test fixture.
///
/// No built-in definitions ship with the engine, so guard-check and spawn
/// tests that exercise the registry-based catalog path (as opposed to
/// `spawn_named`, which bypasses the registry entirely) need at least one
/// registered type to resolve against. "Explore" is used purely as a stable,
/// arbitrary id — its fields are irrelevant to the guards under test.
fn registry_with_explore_fixture() -> SubagentRegistry {
    let mut reg = SubagentRegistry::new();
    reg.register(make_explore_definition());
    reg
}

fn make_spawner() -> SubagentSpawner {
    SubagentSpawner::new(Arc::new(registry_with_explore_fixture()))
}

fn make_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("session-1", "agent-1", PathBuf::from("/tmp"))
}

#[tokio::test]
async fn happy_path_all_guards_pass() {
    let spawner = make_spawner();
    let ctx = make_ctx();
    assert!(spawner.check_guards(&ctx, "Explore", None).await.is_ok());
}

#[tokio::test]
async fn unknown_subagent_type_is_refused() {
    let spawner = make_spawner();
    let ctx = make_ctx();
    let err = spawner
        .check_guards(&ctx, "GhostAgent", None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SpawnerError::UnknownSubagentType { id } if id == "GhostAgent"),
        "unexpected error: {err:?}"
    );
    // UnknownSubagentType is not recoverable.
    let output = err.to_tool_output();
    assert!(
        matches!(
            output,
            ToolOutput::Error {
                recoverable: false,
                ..
            }
        ),
        "expected non-recoverable error, got: {output:?}"
    );
}

#[tokio::test]
async fn depth_exceeded_is_refused() {
    // Default cap = 4. A parent at depth 3 (great-grandchild) trying to
    // spawn would place the child at depth 4 — refused.
    let spawner = make_spawner();
    let ctx = make_ctx().with_depth(3);
    let err = spawner
        .check_guards(&ctx, "Explore", None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SpawnerError::DepthExceeded { depth: 4, cap: 4 }),
        "unexpected error: {err:?}"
    );
    let output = err.to_tool_output();
    assert!(
        matches!(
            output,
            ToolOutput::Error {
                recoverable: false,
                ..
            }
        ),
        "expected non-recoverable error, got: {output:?}"
    );
}

#[tokio::test]
async fn depth_at_cap_minus_one_passes() {
    // Parent at depth 2 → child at 3, which is < cap 4 → allowed.
    let spawner = make_spawner();
    let ctx = make_ctx().with_depth(2);
    assert!(spawner.check_guards(&ctx, "Explore", None).await.is_ok());
}

#[tokio::test]
async fn recursion_detected_is_refused() {
    let spawner = make_spawner();
    let mut ctx = make_ctx();
    ctx.spawn_chain = vec!["Explore".to_string()];
    let err = spawner
        .check_guards(&ctx, "Explore", None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SpawnerError::RecursionDetected { subagent_type, chain }
            if subagent_type == "Explore" && chain == &["Explore".to_string()]),
        "unexpected error: {err:?}"
    );
    let output = err.to_tool_output();
    assert!(
        matches!(
            output,
            ToolOutput::Error {
                recoverable: false,
                ..
            }
        ),
        "expected non-recoverable error, got: {output:?}"
    );
}

#[tokio::test]
async fn recursion_check_is_type_specific() {
    // "Explore" in chain but spawning a different type — allowed.
    let mut ctx = make_ctx();
    ctx.spawn_chain = vec!["Explore".to_string()];
    // No built-ins ship with the engine, so register a custom type for this test.
    let mut reg = SubagentRegistry::new();
    reg.register(crate::background_agents::SubagentDefinition {
        id: "CustomAgent".to_string(),
        description: "A custom agent for testing".to_string(),
        allowed_tools: vec!["Read".to_string()],
        system_prompt_fragment: "Be custom.".to_string(),
        model_override: None,
    });
    let spawner = SubagentSpawner::new(Arc::new(reg));
    assert!(spawner
        .check_guards(&ctx, "CustomAgent", None)
        .await
        .is_ok());
}

#[tokio::test]
async fn concurrency_cap_exceeded_is_recoverable() {
    let spawner = make_spawner();
    // A registry with cap=0 is always full.
    let mut ctx = make_ctx();
    ctx.background_agents = Arc::new(BackgroundAgentRegistry::new(0));
    let err = spawner
        .check_guards(&ctx, "Explore", None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SpawnerError::ConcurrencyCapExceeded),
        "unexpected error: {err:?}"
    );
    // ConcurrencyCapExceeded is the only recoverable error.
    let output = err.to_tool_output();
    assert!(
        matches!(
            output,
            ToolOutput::Error {
                recoverable: true,
                ..
            }
        ),
        "expected recoverable error, got: {output:?}"
    );
}

#[tokio::test]
async fn custom_depth_cap_is_respected() {
    // Cap of 2: parent (depth 0) spawns child (depth 1) — ok.
    // Parent at depth 1 tries to spawn (child would be depth 2 >= cap 2) — refused.
    let spawner = make_spawner().with_depth_cap(2);
    let ctx_ok = make_ctx().with_depth(0);
    assert!(spawner.check_guards(&ctx_ok, "Explore", None).await.is_ok());

    let ctx_refused = make_ctx().with_depth(1);
    let err = spawner
        .check_guards(&ctx_refused, "Explore", None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SpawnerError::DepthExceeded { depth: 2, cap: 2 }),
        "unexpected error: {err:?}"
    );
}

// --- effective_depth_cap / profile-based cap tests ---

fn minimal_profile_json(max_delegation_depth: Option<u32>) -> String {
    let depth_field = match max_delegation_depth {
        Some(n) => format!(r#", "max_delegation_depth": {n}"#),
        None => String::new(),
    };
    format!(
        r#"{{"id":"t","name":"T","description":"","provider":{{"type":"Cli","command":"claude","args":[]}},"model":null,"system_prompt":null,"tools":null,"max_instances":1,"timeout_seconds":300,"serialize":true{depth_field}}}"#
    )
}

fn profile_with_cap(n: u32) -> AgentProfile {
    serde_json::from_str(&minimal_profile_json(Some(n))).expect("valid profile")
}

fn profile_no_cap() -> AgentProfile {
    serde_json::from_str(&minimal_profile_json(None)).expect("valid profile")
}

#[test]
fn effective_depth_cap_returns_profile_value_when_set() {
    let profile = profile_with_cap(7);
    assert_eq!(super::effective_depth_cap(&profile), 7);
}

#[test]
fn effective_depth_cap_falls_back_to_default_when_none() {
    let profile = profile_no_cap();
    assert_eq!(super::effective_depth_cap(&profile), DEFAULT_DEPTH_CAP);
}

#[tokio::test]
async fn profile_cap_some_2_refuses_spawn_at_depth_2() {
    // Child profile caps at 2; parent at depth 1 → child would be at depth 2 → refused.
    let spawner = make_spawner();
    let profile = profile_with_cap(2);
    let ctx = make_ctx().with_depth(1);
    let err = spawner
        .check_guards(&ctx, "Explore", Some(&profile))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SpawnerError::DepthExceeded { depth: 2, cap: 2 }),
        "unexpected error: {err:?}"
    );
}

#[tokio::test]
async fn profile_cap_some_10_permits_at_depth_5() {
    // Child profile caps at 10; parent at depth 4 → child would be at depth 5 → allowed.
    let spawner = make_spawner();
    let profile = profile_with_cap(10);
    let ctx = make_ctx().with_depth(4);
    assert!(spawner
        .check_guards(&ctx, "Explore", Some(&profile))
        .await
        .is_ok());
}

// --- build_child_context tests ---

struct NamedIo(&'static str);
#[async_trait]
impl IoTool for NamedIo {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "test io tool"
    }
    fn input_schema(&self) -> Value {
        Value::Object(Default::default())
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn invoke(&self, _: Value, _: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("ok"))
    }
}

struct NamedEngine(&'static str);
#[async_trait]
impl EngineTool for NamedEngine {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "test engine tool"
    }
    fn input_schema(&self) -> Value {
        Value::Object(Default::default())
    }
    async fn invoke(&self, _: Value, _: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("ok"))
    }
}

fn make_explore_definition() -> SubagentDefinition {
    SubagentDefinition {
        id: "Explore".to_string(),
        description: "Explore subagent".to_string(),
        allowed_tools: vec!["Read".to_string(), "Glob".to_string()],
        system_prompt_fragment: "Summarise findings concisely.".to_string(),
        model_override: None,
    }
}

fn make_ctx_with_registry() -> RunnerContext {
    let mut reg = Registry::new();
    reg.register_io(Arc::new(NamedIo("Read")));
    reg.register_io(Arc::new(NamedIo("Glob")));
    reg.register_engine(Arc::new(NamedEngine("Bash")));
    RunnerContext::new_with_cwd("parent-session", "parent-agent", PathBuf::from("/tmp"))
        .with_registry(Arc::new(reg))
}

#[test]
fn filtered_registry_contains_only_allowed_tools() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry();
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert!(
        child.registry.lookup("Read").is_some(),
        "Read should be visible"
    );
    assert!(
        child.registry.lookup("Glob").is_some(),
        "Glob should be visible"
    );
    assert!(
        child.registry.lookup("Bash").is_none(),
        "Bash must be excluded"
    );
}

fn make_wildcard_definition() -> SubagentDefinition {
    SubagentDefinition {
        id: "general-purpose".to_string(),
        description: "Full tool set".to_string(),
        allowed_tools: vec!["*".to_string()],
        system_prompt_fragment: "Do the whole task.".to_string(),
        model_override: None,
    }
}

#[test]
fn wildcard_allowed_tools_yields_full_parent_registry() {
    // A `"*"` entry in allowed_tools must grant the child the parent's full
    // registry rather than an empty (filtered-to-nothing) one.
    let spawner = make_spawner();
    let parent = make_ctx_with_registry();
    let definition = make_wildcard_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert!(
        child.registry.lookup("Read").is_some(),
        "Read must be visible"
    );
    assert!(
        child.registry.lookup("Glob").is_some(),
        "Glob must be visible"
    );
    assert!(
        child.registry.lookup("Bash").is_some(),
        "Bash (and every other parent tool) must be visible under wildcard"
    );
}

#[test]
fn spawn_chain_extended_with_definition_id() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry().with_spawn_chain(vec!["Parent".to_string()]);
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert_eq!(
        child.spawn_chain,
        vec!["Parent".to_string(), "Explore".to_string()],
        "child chain must be parent chain extended by definition.id"
    );
}

#[test]
fn depth_is_incremented_by_one() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry().with_depth(2);
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert_eq!(child.depth, 3);
}

#[test]
fn child_has_fresh_session_id_different_from_parent() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry();
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert_ne!(
        child.session_id, parent.session_id,
        "child must have its own fresh session_id"
    );
    assert!(!child.session_id.is_empty());
}

#[test]
fn child_agent_id_matches_background_agent_id() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry();
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();
    let expected = bg_id.to_string();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert_eq!(child.agent_id, expected);
}

#[test]
fn child_cancel_token_is_independent_of_parent() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry();
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    // Cancelling the parent must NOT cancel the child.
    parent.cancel.cancel();
    assert!(
        !child.cancel.is_cancelled(),
        "child cancel token must be independent"
    );

    // Child token can be cancelled on its own.
    child.cancel.cancel();
    assert!(child.cancel.is_cancelled());
}

#[test]
fn system_prompt_assembles_parent_memory_fragment_in_order() {
    let spawner = make_spawner();
    let memory_blob = "memory: user sentinel\nproject: phase4 sentinel";
    let parent = make_ctx_with_registry()
        .with_system_prompt("parent system prompt")
        .with_memory_loader(StaticMemoryLoader::new(memory_blob));
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    let prompt = child
        .system_prompt
        .expect("child must have a system_prompt");
    let parent_pos = prompt
        .find("parent system prompt")
        .expect("parent prompt must appear");
    let memory_pos = prompt
        .find("memory: user sentinel")
        .expect("memory blob must appear");
    let fragment_pos = prompt
        .find("Summarise findings concisely.")
        .expect("fragment must appear");

    assert!(
        parent_pos < memory_pos,
        "parent prompt must precede memory blob"
    );
    assert!(
        memory_pos < fragment_pos,
        "memory blob must precede system_prompt_fragment"
    );
}

#[test]
fn system_prompt_empty_parent_and_memory_yields_only_fragment() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry();
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    let prompt = child
        .system_prompt
        .expect("child must have a system_prompt");
    assert_eq!(prompt, "Summarise findings concisely.");
}

#[test]
fn child_background_agent_registry_is_fresh_with_parent_cap() {
    let spawner = make_spawner();
    let parent_cap = 5;
    let parent = make_ctx_with_registry()
        .with_background_agents(Arc::new(BackgroundAgentRegistry::new(parent_cap)));
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert_eq!(
        child.background_agents.cap(),
        parent_cap,
        "child registry must inherit the parent's cap"
    );
    assert_ne!(
        Arc::as_ptr(&parent.background_agents),
        Arc::as_ptr(&child.background_agents),
        "child must have its own BackgroundAgentRegistry"
    );
}

#[test]
fn memory_loader_is_shared_arc_between_parent_and_child() {
    let spawner = make_spawner();
    let loader = StaticMemoryLoader::new("shared memory");
    let parent = make_ctx_with_registry().with_memory_loader(loader.clone());
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert_eq!(
        Arc::as_ptr(&parent.memory_loader),
        Arc::as_ptr(&child.memory_loader),
        "parent and child must share the same Arc<dyn MemoryLoader>"
    );
}

// --- delegate_chain tests ---

#[test]
fn delegate_chain_default_is_empty_in_child() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry();
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert_eq!(
        child.delegate_chain,
        vec!["parent-agent".to_string()],
        "child delegate_chain must contain only the parent's agent_id when parent chain is empty"
    );
}

#[test]
fn child_inherits_parent_delegate_chain() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry()
        .with_delegate_chain(vec!["root-agent".to_string(), "mid-agent".to_string()]);
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert!(
        child
            .delegate_chain
            .starts_with(&["root-agent".to_string(), "mid-agent".to_string()]),
        "child must inherit parent's delegate_chain prefix"
    );
}

#[test]
fn delegate_chain_grows_by_one() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry().with_delegate_chain(vec!["agent-a".to_string()]);
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert_eq!(
        child.delegate_chain.len(),
        2,
        "child delegate_chain must be exactly one longer than parent's"
    );
    assert_eq!(
        child.delegate_chain.last().unwrap(),
        "parent-agent",
        "last entry must be the parent's agent_id"
    );
}

// --- parent session info propagation tests ---

#[test]
fn build_child_context_propagates_parent_session_info() {
    let spawner = make_spawner();
    let parent = make_ctx_with_registry();
    let definition = make_explore_definition();
    let bg_id = BackgroundAgentId::new();

    let child = spawner.build_child_context(&parent, &definition, &bg_id);

    assert_eq!(
        child.parent_session_id.as_deref(),
        Some("parent-session"),
        "child must have parent's session_id"
    );
    assert_eq!(
        child.parent_agent_id.as_deref(),
        Some("parent-agent"),
        "child must have parent's agent_id"
    );
    assert_eq!(
        child.parent_current_cwd.as_deref(),
        Some(std::path::Path::new("/tmp")),
        "child must have snapshot of parent's cwd"
    );
}

#[test]
fn build_delegate_context_propagates_parent_session_info() {
    use ao_protocol::agent::AgentProfile;

    let spawner = make_spawner();
    let parent = make_ctx_with_registry();

    fn minimal_profile() -> AgentProfile {
        serde_json::from_str(r#"{"id":"t","name":"T","description":"","provider":{"type":"Cli","command":"claude","args":[]},"model":null,"system_prompt":null,"tools":null,"max_instances":1,"timeout_seconds":300,"serialize":true}"#).unwrap()
    }
    let profile = minimal_profile();

    let child = spawner.build_delegate_context(&parent, &profile);

    assert_eq!(
        child.parent_session_id.as_deref(),
        Some("parent-session"),
        "delegate child must have parent's session_id"
    );
    assert_eq!(
        child.parent_agent_id.as_deref(),
        Some("parent-agent"),
        "delegate child must have parent's agent_id"
    );
    assert_eq!(
        child.parent_current_cwd.as_deref(),
        Some(std::path::Path::new("/tmp")),
        "delegate child must snapshot parent's cwd"
    );
}

#[test]
fn top_level_context_has_no_parent_session_info() {
    let ctx = make_ctx();
    assert!(ctx.parent_session_id.is_none());
    assert!(ctx.parent_agent_id.is_none());
    assert!(ctx.parent_current_cwd.is_none());
}

// --- spawn tests ---

use crate::background_agents::child_runner::ChildRunner;
use crate::background_agents::{RunnerEvent, TaskFinalReport, TaskFinalStatus};
use tokio::sync::broadcast;

struct ScriptedChildRunner {
    text_events: Vec<String>,
    report: TaskFinalReport,
}

impl ChildRunner for ScriptedChildRunner {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, ao_protocol::error::AoError>> {
        let texts = self.text_events.clone();
        let report = self.report.clone();
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            for text in texts {
                let _ = event_tx.send(RunnerEvent::AssistantText {
                    background_agent_id: bg_id.clone(),
                    text,
                });
            }
            let terminal = if report.status == TaskFinalStatus::Cancelled {
                RunnerEvent::Cancelled {
                    background_agent_id: bg_id,
                }
            } else {
                RunnerEvent::Completed {
                    background_agent_id: bg_id,
                }
            };
            let _ = event_tx.send(terminal);
            Ok(report)
        })
    }
}

fn make_spawner_with_mock(report: TaskFinalReport, texts: Vec<String>) -> SubagentSpawner {
    let mock = ScriptedChildRunner {
        text_events: texts,
        report,
    };
    SubagentSpawner::new(Arc::new(registry_with_explore_fixture()))
        .with_child_runner(Arc::new(mock))
}

#[tokio::test]
async fn spawn_happy_path_inserts_live_handle() {
    let spawner =
        make_spawner_with_mock(TaskFinalReport::completed(Some("done".to_string())), vec![]);
    let ctx = make_ctx_with_registry();

    let (id, _rx) = spawner
        .spawn(&ctx, "Explore", "find things".to_string())
        .await
        .expect("spawn must succeed");

    assert!(
        ctx.background_agents.get(&id).await.is_some(),
        "handle must be inserted into parent registry"
    );
    assert_eq!(ctx.background_agents.live_count().await, 1);
}

#[tokio::test]
async fn spawn_events_flow_through_broadcast_channel() {
    let spawner = make_spawner_with_mock(
        TaskFinalReport::completed(Some("result".to_string())),
        vec!["hello from child".to_string()],
    );
    let ctx = make_ctx_with_registry();

    let (_, mut rx) = spawner
        .spawn(&ctx, "Explore", "search".to_string())
        .await
        .expect("spawn must succeed");

    // Let the spawned task run and emit its events.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let first = rx.recv().await.expect("must receive AssistantText event");
    assert!(
        matches!(&first, RunnerEvent::AssistantText { text, .. } if text == "hello from child"),
        "unexpected first event: {first:?}"
    );

    let second = rx.recv().await.expect("must receive terminal event");
    assert!(
        matches!(second, RunnerEvent::Completed { .. }),
        "unexpected second event: {second:?}"
    );
}

// --- spawn_sync forward tests ---

use crate::context::{EventSink, UserEvent};

/// Records every [`UserEvent`] emitted through it so tests can assert on
/// what the parent sink received during a forked skill run.
struct RecordingSink {
    events: Arc<std::sync::Mutex<Vec<UserEvent>>>,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Arc::new(std::sync::Mutex::new(vec![])),
        })
    }

    fn snapshot(&self) -> Vec<UserEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl EventSink for RecordingSink {
    async fn emit(&self, event: UserEvent) -> Result<(), ao_protocol::error::AoError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

/// Emits a mix of AssistantText and ToolUse events, then completes.
struct MixedEventRunner {
    texts: Vec<String>,
    tool_uses: Vec<String>,
    final_text: Option<String>,
}

impl ChildRunner for MixedEventRunner {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, ao_protocol::error::AoError>> {
        let texts = self.texts.clone();
        let tool_uses = self.tool_uses.clone();
        let final_text = self.final_text.clone();
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            for text in texts {
                let _ = event_tx.send(RunnerEvent::AssistantText {
                    background_agent_id: bg_id.clone(),
                    text,
                });
            }
            for tool_name in tool_uses {
                let _ = event_tx.send(RunnerEvent::ToolUse {
                    background_agent_id: bg_id.clone(),
                    tool_name,
                });
            }
            let _ = event_tx.send(RunnerEvent::Completed {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::completed(final_text))
        })
    }
}

/// Blocks until its cancel token fires, then reports cancellation.
struct BlockingRunner;

impl ChildRunner for BlockingRunner {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, ao_protocol::error::AoError>> {
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            child_ctx.cancel.cancelled().await;
            let _ = event_tx.send(RunnerEvent::Cancelled {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::cancelled())
        })
    }
}

fn make_fork_definition() -> SubagentDefinition {
    SubagentDefinition {
        id: "fork-skill".to_string(),
        description: "Fork skill for testing".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    }
}

#[tokio::test]
async fn fork_sync_assistant_text_forwarded_to_parent_sink() {
    let sink = RecordingSink::new();
    let ctx = make_ctx_with_registry()
        .with_event_sink(Arc::clone(&sink) as Arc<dyn EventSink + Send + Sync>);

    let runner = MixedEventRunner {
        texts: vec!["step one".to_string(), "step two".to_string()],
        tool_uses: vec![],
        final_text: Some("done".to_string()),
    };
    let spawner = SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(runner));

    spawner
        .spawn_sync(&ctx, make_fork_definition(), "run".to_string())
        .await;

    // Let forwarding task deliver its events.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let recorded = sink.snapshot();
    let briefs: Vec<String> = recorded
        .into_iter()
        .filter_map(|e| match e {
            UserEvent::Brief { content } => Some(content),
            _ => None,
        })
        .collect();

    assert!(
        briefs.contains(&"step one".to_string()),
        "parent sink must receive 'step one' as Brief; got: {briefs:?}"
    );
    assert!(
        briefs.contains(&"step two".to_string()),
        "parent sink must receive 'step two' as Brief; got: {briefs:?}"
    );
}

#[tokio::test]
async fn fork_sync_tool_use_forwarded_to_parent_sink() {
    let sink = RecordingSink::new();
    let ctx = make_ctx_with_registry()
        .with_event_sink(Arc::clone(&sink) as Arc<dyn EventSink + Send + Sync>);

    let runner = MixedEventRunner {
        texts: vec![],
        tool_uses: vec!["Read".to_string()],
        final_text: Some("done".to_string()),
    };
    let spawner = SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(runner));

    spawner
        .spawn_sync(&ctx, make_fork_definition(), "run".to_string())
        .await;

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let recorded = sink.snapshot();
    let found = recorded.iter().any(|e| match e {
        UserEvent::Brief { content } => content.contains("Read"),
        _ => false,
    });
    assert!(
        found,
        "parent sink must receive a Brief mentioning the tool name; got: {recorded:?}"
    );
}

#[tokio::test]
async fn fork_sync_final_report_returned_as_tool_output() {
    let ctx = make_ctx_with_registry();

    let runner = MixedEventRunner {
        texts: vec!["thinking".to_string()],
        tool_uses: vec![],
        final_text: Some("the answer".to_string()),
    };
    let spawner = SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(runner));

    let out = spawner
        .spawn_sync(&ctx, make_fork_definition(), "run".to_string())
        .await;

    assert!(
        matches!(&out, ToolOutput::Text(t) if t == "the answer"),
        "final report must be returned as ToolOutput::Text; got: {out:?}"
    );
}

#[tokio::test]
async fn fork_sync_parent_cancel_tears_down_child() {
    let ctx = make_ctx_with_registry();
    let spawner = SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(BlockingRunner));

    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancel.cancel();
    });

    let out = spawner
        .spawn_sync(&ctx, make_fork_definition(), "run".to_string())
        .await;

    assert!(
        matches!(&out, ToolOutput::Error { message, .. } if message.contains("cancelled")),
        "parent cancel must propagate as an error; got: {out:?}"
    );
}

// --- spawn_named depth cap + chain growth ---

fn make_noop_spawner() -> SubagentSpawner {
    struct NoopRunner;
    impl ChildRunner for NoopRunner {
        fn launch(
            &self,
            _: RunnerContext,
            _: String,
            _: BackgroundAgentId,
            _: broadcast::Sender<RunnerEvent>,
            _: Option<ao_protocol::agent::AgentProfile>,
        ) -> tokio::task::JoinHandle<Result<TaskFinalReport, ao_protocol::error::AoError>>
        {
            tokio::spawn(async { Ok(TaskFinalReport::completed(None)) })
        }
    }
    SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(NoopRunner))
}

fn minimal_delegate_profile() -> AgentProfile {
    serde_json::from_str(
        r#"{"id":"target","name":"Target","description":"","provider":{"type":"Cli","command":"echo","args":[]},"model":null,"system_prompt":null,"tools":null,"max_instances":1,"timeout_seconds":300,"serialize":true}"#,
    )
    .expect("valid minimal profile")
}

/// Same as [`minimal_delegate_profile`] but with an explicit
/// `max_delegation_depth`, for tests that need a tight depth/chain cap.
fn minimal_delegate_profile_with_cap(max_delegation_depth: u32) -> AgentProfile {
    serde_json::from_str(&format!(
        r#"{{"id":"target","name":"Target","description":"","provider":{{"type":"Cli","command":"echo","args":[]}},"model":null,"system_prompt":null,"tools":null,"max_instances":1,"timeout_seconds":300,"serialize":true,"max_delegation_depth":{max_delegation_depth}}}"#,
    ))
    .expect("valid minimal profile with cap")
}

#[tokio::test]
async fn spawn_named_depth_cap_8_refuses_when_chain_full() {
    // parent delegate_chain already has 7 entries → 7 + 1 == 8 == cap → refused
    let spawner = make_noop_spawner();
    let mut ctx = make_ctx_with_registry();
    ctx.delegate_chain = (0..7).map(|i| format!("agent-{}", i)).collect();

    let profile = minimal_delegate_profile();
    let out = spawner
        .spawn_named(&ctx, &profile, "do it".to_string(), false)
        .await;

    assert!(
        matches!(&out, ToolOutput::Error { message, .. } if message.contains("Delegation chain limit reached")),
        "depth cap must be enforced at 8 hops; got: {out:?}"
    );
}

#[tokio::test]
async fn spawn_named_delegate_chain_grows_by_one_hop() {
    // Verify the child context gets a delegate_chain one longer than the parent's.
    let spawner = make_noop_spawner();
    let parent = make_ctx_with_registry().with_delegate_chain(vec!["root-agent".to_string()]);
    let profile = minimal_delegate_profile();

    // Use build_delegate_context directly — spawn_named wraps it.
    let child = spawner.build_delegate_context(&parent, &profile);

    assert_eq!(
        child.delegate_chain.len(),
        2,
        "child chain must be exactly one longer than parent's"
    );
    assert_eq!(
        child.delegate_chain.last().unwrap(),
        "parent-agent",
        "last entry must be the parent's agent_id"
    );
}

#[tokio::test]
async fn spawn_named_parent_cancel_propagates_to_child() {
    // Verify that when the parent's cancel token fires mid-delegation,
    // spawn_named returns a cancellation error and the child's token fires.
    let spawner = SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(BlockingRunner));
    let profile = minimal_delegate_profile();
    let ctx = make_ctx_with_registry();

    let cancel = ctx.cancel.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancel.cancel();
    });

    let out = spawner
        .spawn_named(&ctx, &profile, "directive".to_string(), false)
        .await;

    assert!(
        matches!(&out, ToolOutput::Error { message, .. } if message.contains("cancelled")),
        "parent cancel must propagate through spawn_named; got: {out:?}"
    );
}

// --- spawn_named_async: delegate_completion_sink notification tests ---

use crate::delegate_completion_sink::DelegateCompletionSink;

/// Recording sink that captures every `notify`/`notify_started` call for
/// test assertions.
struct RecordingCompletionSink {
    calls: Arc<std::sync::Mutex<Vec<(String, String, TaskFinalStatus, String)>>>,
    started_calls: Arc<std::sync::Mutex<Vec<(String, String, DateTime<Utc>)>>>,
}

impl RecordingCompletionSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Arc::new(std::sync::Mutex::new(vec![])),
            started_calls: Arc::new(std::sync::Mutex::new(vec![])),
        })
    }

    fn calls(&self) -> Vec<(String, String, TaskFinalStatus, String)> {
        self.calls.lock().unwrap().clone()
    }

    fn started_calls(&self) -> Vec<(String, String, DateTime<Utc>)> {
        self.started_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl DelegateCompletionSink for RecordingCompletionSink {
    async fn notify_started(
        &self,
        delegate_name: &str,
        delegation_id: &str,
        spawned_at: DateTime<Utc>,
    ) {
        self.started_calls.lock().unwrap().push((
            delegate_name.to_string(),
            delegation_id.to_string(),
            spawned_at,
        ));
    }

    async fn notify(
        &self,
        delegate_name: &str,
        delegation_id: &str,
        report: &crate::background_agents::handle::TaskFinalReport,
        transcript_path: &str,
    ) {
        self.calls.lock().unwrap().push((
            delegate_name.to_string(),
            delegation_id.to_string(),
            report.status.clone(),
            transcript_path.to_string(),
        ));
    }
}

fn make_async_spawner_with_report(report: TaskFinalReport) -> SubagentSpawner {
    let mock = ScriptedChildRunner {
        text_events: vec![],
        report,
    };
    SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(mock))
}

#[tokio::test]
async fn spawn_named_async_sink_called_on_completion() {
    let sink = RecordingCompletionSink::new();
    let spawner = make_async_spawner_with_report(TaskFinalReport::completed(Some(
        "all done".to_string(),
    )));
    let profile = minimal_delegate_profile();
    let ctx = make_ctx_with_registry()
        .with_delegate_completion_sink(Arc::clone(&sink) as Arc<dyn DelegateCompletionSink>);

    let out = spawner
        .spawn_named_async(
            &ctx,
            &profile,
            "do it".to_string(),
            false,
            "target".to_string(),
        )
        .await;

    // The tool output is the immediate acknowledgement (delegation_id line).
    assert!(
        matches!(&out, ToolOutput::Text(t) if t.contains("delegation_id")),
        "expected immediate delegation_id ack, got: {out:?}"
    );

    // Let the notification task run.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let calls = sink.calls();
    assert_eq!(calls.len(), 1, "sink must be called exactly once");
    let (name, _id, status, _path) = &calls[0];
    assert_eq!(name, "target");
    assert_eq!(*status, TaskFinalStatus::Completed);
}

/// `notify_started` must fire synchronously as part of the spawn call
/// itself (handle registration), not from the completion-notification
/// task — so it's already recorded by the time `spawn_named_async`
/// returns, well before the sleep the completion assertions above need.
#[tokio::test]
async fn spawn_named_async_sink_notified_of_start_before_completion() {
    let sink = RecordingCompletionSink::new();
    let spawner = make_async_spawner_with_report(TaskFinalReport::completed(Some(
        "all done".to_string(),
    )));
    let profile = minimal_delegate_profile();
    let ctx = make_ctx_with_registry()
        .with_delegate_completion_sink(Arc::clone(&sink) as Arc<dyn DelegateCompletionSink>);

    spawner
        .spawn_named_async(
            &ctx,
            &profile,
            "do it".to_string(),
            false,
            "target".to_string(),
        )
        .await;

    let started = sink.started_calls();
    assert_eq!(started.len(), 1, "notify_started must fire exactly once");
    assert_eq!(started[0].0, "target");
    assert!(
        (Utc::now() - started[0].2).num_seconds().abs() < 5,
        "spawned_at passed to notify_started must be the real spawn time, not a stale or default value"
    );

    // Completion hasn't been recorded yet — confirms notify_started
    // really does fire at spawn time, not lazily alongside notify.
    assert!(sink.calls().is_empty());
}

/// The sync path (`spawn_named`) must never call `notify_started` — only
/// async delegates bracket with a start signal; sync delegates keep the
/// parent's own turn in-flight, which the ordinary typing indicator
/// already covers.
#[tokio::test]
async fn spawn_named_sync_never_calls_notify_started() {
    let sink = RecordingCompletionSink::new();
    let mock = ScriptedChildRunner {
        text_events: vec![],
        report: TaskFinalReport::completed(Some("done".to_string())),
    };
    let spawner = SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(mock));
    let profile = minimal_delegate_profile();
    let ctx = make_ctx_with_registry()
        .with_delegate_completion_sink(Arc::clone(&sink) as Arc<dyn DelegateCompletionSink>);

    spawner
        .spawn_named(&ctx, &profile, "do it".to_string(), false)
        .await;

    assert!(
        sink.started_calls().is_empty(),
        "sync spawn must never call notify_started"
    );
}

#[tokio::test]
async fn spawn_named_async_sink_called_on_failure() {
    let sink = RecordingCompletionSink::new();
    let spawner = make_async_spawner_with_report(TaskFinalReport::failed("boom"));
    let profile = minimal_delegate_profile();
    let ctx = make_ctx_with_registry()
        .with_delegate_completion_sink(Arc::clone(&sink) as Arc<dyn DelegateCompletionSink>);

    spawner
        .spawn_named_async(
            &ctx,
            &profile,
            "do it".to_string(),
            false,
            "target".to_string(),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let calls = sink.calls();
    assert_eq!(
        calls.len(),
        1,
        "sink must be called exactly once on failure"
    );
    let (_name, _id, status, _path) = &calls[0];
    assert_eq!(*status, TaskFinalStatus::Failed);
}

#[tokio::test]
async fn spawn_named_async_sink_called_on_cancellation() {
    let sink = RecordingCompletionSink::new();
    let spawner = make_async_spawner_with_report(TaskFinalReport::cancelled());
    let profile = minimal_delegate_profile();
    let ctx = make_ctx_with_registry()
        .with_delegate_completion_sink(Arc::clone(&sink) as Arc<dyn DelegateCompletionSink>);

    spawner
        .spawn_named_async(
            &ctx,
            &profile,
            "do it".to_string(),
            false,
            "target".to_string(),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let calls = sink.calls();
    assert_eq!(
        calls.len(),
        1,
        "sink must be called exactly once on cancellation"
    );
    let (_name, _id, status, _path) = &calls[0];
    assert_eq!(*status, TaskFinalStatus::Cancelled);
}

#[tokio::test]
async fn spawn_named_async_no_sink_still_enqueues_pending_message() {
    // Context has no sink: verify pending_user_messages still receives the
    // completion notice (regression guard for the native-runner path).
    let spawner = make_async_spawner_with_report(TaskFinalReport::completed(Some(
        "finished".to_string(),
    )));
    let profile = minimal_delegate_profile();
    let ctx = make_ctx_with_registry(); // no sink

    spawner
        .spawn_named_async(
            &ctx,
            &profile,
            "do it".to_string(),
            false,
            "bot".to_string(),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let drained = ctx
        .pending_user_messages
        .lock()
        .unwrap()
        .drain_for(crate::permissions::SessionKind::Interactive, false);
    assert_eq!(drained.len(), 1, "pending queue must receive one message");
    assert!(
        drained[0].contains("complete"),
        "message must mention completion; got: {:?}",
        drained[0]
    );
}

#[tokio::test]
async fn spawn_named_async_with_sink_also_enqueues_pending_message() {
    // Both paths must fire: pending_user_messages AND the sink.
    let sink = RecordingCompletionSink::new();
    let spawner =
        make_async_spawner_with_report(TaskFinalReport::completed(Some("ok".to_string())));
    let profile = minimal_delegate_profile();
    let ctx = make_ctx_with_registry()
        .with_delegate_completion_sink(Arc::clone(&sink) as Arc<dyn DelegateCompletionSink>);

    spawner
        .spawn_named_async(
            &ctx,
            &profile,
            "do it".to_string(),
            false,
            "bot".to_string(),
        )
        .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Sink was called.
    assert_eq!(sink.calls().len(), 1, "sink must have been called");

    // pending_user_messages also received the message.
    let drained = ctx
        .pending_user_messages
        .lock()
        .unwrap()
        .drain_for(crate::permissions::SessionKind::Interactive, false);
    assert_eq!(
        drained.len(),
        1,
        "pending queue must also receive one message"
    );
}

// --- spawn_named_async_id: structured-id variant ---

#[tokio::test]
async fn spawn_named_async_id_returns_background_agent_id_directly() {
    let spawner = make_async_spawner_with_report(TaskFinalReport::completed(Some(
        "done".to_string(),
    )));
    let profile = minimal_delegate_profile();
    let ctx = make_ctx_with_registry();

    let result = spawner
        .spawn_named_async_id(
            &ctx,
            &profile,
            "do it".to_string(),
            false,
            "artifact-agent".to_string(),
        )
        .await;

    let id = result.expect("spawn_named_async_id should succeed");
    assert!(
        !id.to_string().is_empty(),
        "returned BackgroundAgentId must not be empty"
    );

    // Shares the same completion-notification path as spawn_named_async.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let drained = ctx
        .pending_user_messages
        .lock()
        .unwrap()
        .drain_for(crate::permissions::SessionKind::Interactive, false);
    assert_eq!(
        drained.len(),
        1,
        "pending queue must still receive the completion notice"
    );
}

#[tokio::test]
async fn spawn_named_async_id_depth_cap_exceeded_returns_err() {
    let spawner = make_async_spawner_with_report(TaskFinalReport::completed(None));
    // max_delegation_depth = 1: parent delegate_chain already at len 1
    // means chain.len() + 1 (== 2) >= cap (1) — refused.
    let profile = minimal_delegate_profile_with_cap(1);
    let ctx = make_ctx_with_registry().with_delegate_chain(vec!["someone".to_string()]);

    let result = spawner
        .spawn_named_async_id(
            &ctx,
            &profile,
            "do it".to_string(),
            false,
            "artifact-agent".to_string(),
        )
        .await;

    let err = result.expect_err("depth cap must be enforced");
    assert!(
        matches!(&err, ToolOutput::Error { message, .. } if message.contains("Delegation chain limit reached")),
        "unexpected error: {err:?}"
    );
}
