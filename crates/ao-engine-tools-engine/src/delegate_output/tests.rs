use super::*;

use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, BackgroundAgentRegistry, ChildRunner, RunnerEvent, SubagentDefinition,
    SubagentRegistry, SubagentSpawner, TaskFinalReport, TaskFinalStatus,
};
use ao_engine_tools_core::RunnerContext;
use ao_protocol::error::AoError;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::Utc;
use serde_json::json;
use tokio::sync::broadcast;

// --- mock runners ---

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
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
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
            let terminal = match report.status {
                TaskFinalStatus::Cancelled => RunnerEvent::Cancelled {
                    background_agent_id: bg_id,
                },
                TaskFinalStatus::Failed => RunnerEvent::Failed {
                    background_agent_id: bg_id,
                    error: report.error_message.clone().unwrap_or_default(),
                },
                TaskFinalStatus::Completed => RunnerEvent::Completed {
                    background_agent_id: bg_id,
                },
            };
            let _ = event_tx.send(terminal);
            Ok(report)
        })
    }
}

/// Emits events then blocks until its cancel token fires.
struct BlockingChildRunner {
    phase1_events: Vec<String>,
}

impl ChildRunner for BlockingChildRunner {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        let texts = self.phase1_events.clone();
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            for text in texts {
                let _ = event_tx.send(RunnerEvent::AssistantText {
                    background_agent_id: bg_id.clone(),
                    text,
                });
            }
            child_ctx.cancel.cancelled().await;
            let _ = event_tx.send(RunnerEvent::Cancelled {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::cancelled())
        })
    }
}

/// Emits phase-1 events, waits for a gate signal, emits phase-2, then completes.
struct TwoPhaseRunner {
    gate: Arc<tokio::sync::Notify>,
}

impl ChildRunner for TwoPhaseRunner {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
        let gate = self.gate.clone();
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            let _ = event_tx.send(RunnerEvent::AssistantText {
                background_agent_id: bg_id.clone(),
                text: "phase1".into(),
            });
            gate.notified().await;
            let _ = event_tx.send(RunnerEvent::AssistantText {
                background_agent_id: bg_id.clone(),
                text: "phase2".into(),
            });
            let _ = event_tx.send(RunnerEvent::Completed {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::completed(Some("final answer".to_string())))
        })
    }
}

// --- helpers ---

fn make_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("sess", "agent", PathBuf::from("/tmp"))
}

/// A registry seeded with a single "Explore" test fixture. No built-in
/// catalog ships with the engine, so tests that spawn via the registry-based
/// catalog path need at least one registered type to resolve against.
fn registry_with_explore_fixture() -> SubagentRegistry {
    let mut reg = SubagentRegistry::new();
    reg.register(SubagentDefinition {
        id: "Explore".to_string(),
        description: "Test catalog subagent".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    reg
}

fn make_spawner(runner: impl ChildRunner + 'static) -> Arc<SubagentSpawner> {
    Arc::new(
        SubagentSpawner::new(Arc::new(registry_with_explore_fixture()))
            .with_child_runner(Arc::new(runner)),
    )
}

/// Spawn a background child directly through the spawner primitive (the same
/// path an async `Delegate` uses) and return its delegation id. The handle is
/// left live in `ctx.background_agents` for `DelegateOutput` to poll.
async fn spawn_background(spawner: Arc<SubagentSpawner>, ctx: &RunnerContext) -> String {
    let (bg_id, _rx) = spawner
        .spawn(ctx, "Explore", "go".to_string())
        .await
        .expect("spawn must succeed");
    bg_id.to_string()
}

// --- tests ---

#[tokio::test]
async fn poll_while_running_drains_new_events() {
    let spawner = make_spawner(BlockingChildRunner {
        phase1_events: vec!["hello".to_string(), "world".to_string()],
    });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    // Yield to let the child emit its events before we poll.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let tool = DelegateOutput;
    let out = tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("running"));
            let events = v["events"].as_array().unwrap();
            assert_eq!(events.len(), 2, "expected 2 events from poll, got: {events:?}");
        }
        _ => panic!("expected Structured output, got: {out:?}"),
    }

    assert_eq!(
        ctx.background_agents.live_count().await,
        1,
        "handle must remain in registry after running poll"
    );
}

#[tokio::test]
async fn second_poll_returns_only_fresh_events() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let spawner = make_spawner(TwoPhaseRunner { gate: gate.clone() });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    // Yield to let phase-1 event arrive.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let tool = DelegateOutput;

    // First poll: child is waiting for the gate, so status must be "running".
    let poll1 = tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();
    let events1 = match &poll1 {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("running"), "first poll must be running");
            v["events"].as_array().unwrap().clone()
        }
        _ => panic!("expected Structured for poll1, got: {poll1:?}"),
    };
    assert_eq!(events1.len(), 1, "first poll must return exactly the phase-1 event");
    assert_eq!(
        events1[0]["text"].as_str(),
        Some("phase1"),
        "first poll event must be 'phase1'"
    );

    // Signal gate and yield to let phase-2 run and child complete.
    gate.notify_one();
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Second poll: must see only phase-2 events plus completed status.
    let poll2 = tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();
    match &poll2 {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("completed"),
                "second poll must be completed"
            );
            let events2 = v["events"].as_array().unwrap();
            let has_phase2 = events2.iter().any(|e| e["text"].as_str() == Some("phase2"));
            assert!(has_phase2, "second poll must contain the phase-2 event");
            assert_eq!(
                v["final_result"].as_str(),
                Some("final answer"),
                "must return final_result from the completed report"
            );
        }
        _ => panic!("expected Structured for poll2, got: {poll2:?}"),
    }

    // Handle must be reaped after the completed poll.
    assert_eq!(ctx.background_agents.live_count().await, 0);
}

#[tokio::test]
async fn poll_after_completion_returns_final_result_and_reaps() {
    let spawner = make_spawner(ScriptedChildRunner {
        text_events: vec!["step1".to_string()],
        report: TaskFinalReport::completed(Some("the answer".to_string())),
    });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    // Yield to let the child complete.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let tool = DelegateOutput;
    let out = tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("completed"));
            assert_eq!(
                v["final_result"].as_str(),
                Some("the answer"),
                "final_result must match report"
            );
        }
        _ => panic!("expected Structured output, got: {out:?}"),
    }

    assert_eq!(
        ctx.background_agents.live_count().await,
        0,
        "handle must be reaped after completed poll"
    );
}

/// A well-formed id that is in neither the registry nor on disk is *not* an
/// error — we simply have not observed anything about it. Only a malformed id
/// proves caller error.
#[tokio::test]
async fn poll_on_unregistered_id_is_indeterminate_not_error() {
    let _guard = crate::test_env::DataDirGuard::new();
    let ctx = make_ctx();
    let unknown_id = BackgroundAgentId::new().to_string();

    let tool = DelegateOutput;
    let out = tool.invoke(json!({"id": unknown_id}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("indeterminate"),
                "an unobserved id must be indeterminate, not an error"
            );
            assert_eq!(
                v["reason"].as_str(),
                Some(super::REASON_NO_TRANSCRIPT),
                "reason must distinguish 'nothing persisted' from 'no terminal event'"
            );
        }
        other => panic!("expected Structured indeterminate, got: {other:?}"),
    }
}

/// The one remaining error case: an id that cannot be a delegation id at all.
#[tokio::test]
async fn poll_on_malformed_id_still_returns_error() {
    let _guard = crate::test_env::DataDirGuard::new();
    let ctx = make_ctx();

    let out = DelegateOutput
        .invoke(json!({"id": "not-a-uuid"}), &ctx)
        .await
        .unwrap();

    assert!(
        matches!(out, ToolOutput::Error { .. }),
        "a malformed id is provable caller error and must stay an error, got: {out:?}"
    );
}

#[tokio::test]
async fn poll_on_cancelled_child_returns_status_cancelled() {
    let spawner = make_spawner(ScriptedChildRunner {
        text_events: vec![],
        report: TaskFinalReport::cancelled(),
    });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    // Yield to let the child resolve.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let tool = DelegateOutput;
    let out = tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("cancelled"));
        }
        _ => panic!("expected Structured output, got: {out:?}"),
    }

    assert_eq!(
        ctx.background_agents.live_count().await,
        0,
        "handle must be reaped after cancelled poll"
    );
}

#[tokio::test]
async fn poll_on_failed_child_surfaces_status_failed_with_error() {
    let spawner = make_spawner(ScriptedChildRunner {
        text_events: vec![],
        report: TaskFinalReport::failed("anthropic api returned 401 unauthorized"),
    });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    // Yield to let the child resolve.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let tool = DelegateOutput;
    let out = tool.invoke(json!({"id": bg_id}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("failed"),
                "a failed child must surface status=failed, not completed/cancelled"
            );
            assert_eq!(
                v["error"].as_str(),
                Some("anthropic api returned 401 unauthorized"),
                "the failure error message must be surfaced, not swallowed"
            );
        }
        _ => panic!("expected Structured output, got: {out:?}"),
    }

    assert_eq!(
        ctx.background_agents.live_count().await,
        0,
        "handle must be reaped after a failed poll"
    );
}

#[test]
fn tool_name_is_delegate_output() {
    assert_eq!(DelegateOutput.name(), "DelegateOutput");
}

#[test]
fn is_concurrency_safe() {
    assert!(DelegateOutput.is_concurrency_safe());
}

/// Simulates the MCP per-request context swap: the context that spawned an
/// async delegate is dropped at end of request N, and a fresh context sharing
/// the same registry is created for request N+1. DelegateOutput must still find
/// the parked handle and return the final result.
#[tokio::test]
async fn async_delegate_result_survives_context_swap() {
    use std::collections::HashMap;
    use ao_protocol::agent::{
        AgentProfile, AgentRunnerMode, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };

    let shared_registry = Arc::new(BackgroundAgentRegistry::new(8));

    // Request N: context that calls spawn_named_async.
    let spawning_ctx =
        RunnerContext::new_with_cwd("sess", "parent-agent", PathBuf::from("/tmp"))
            .with_background_agents(Arc::clone(&shared_registry));

    let target_profile = AgentProfile {
        id: "test-delegate".to_string(),
        name: "TestDelegate".to_string(),
        description: "integration test agent".to_string(),
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
        runner_mode: AgentRunnerMode::default(),
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
    };

    let spawner = make_spawner(ScriptedChildRunner {
        text_events: vec![],
        report: TaskFinalReport::completed(Some("async result text".to_string())),
    });

    let out = spawner
        .spawn_named_async(
            &spawning_ctx,
            &target_profile,
            "do the task".to_string(),
            false,
            "TestDelegate".to_string(),
        )
        .await;

    let delegation_id = match &out {
        ToolOutput::Text(t) => t
            .split("delegation_id=")
            .nth(1)
            .and_then(|s| s.split(')').next())
            .map(|s| s.to_string())
            .expect("output must contain delegation_id=<uuid>)"),
        other => panic!("expected Text from spawn_named_async, got: {other:?}"),
    };

    // End of request N: drop the spawning context.
    drop(spawning_ctx);

    // Request N+1: fresh context sharing the same registry.
    let polling_ctx =
        RunnerContext::new_with_cwd("sess", "parent-agent", PathBuf::from("/tmp"))
            .with_background_agents(Arc::clone(&shared_registry));

    // Yield to let child and wrapper tasks complete.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let tool = DelegateOutput;
    let poll_out = tool
        .invoke(json!({"id": delegation_id}), &polling_ctx)
        .await
        .unwrap();

    match poll_out {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("completed"),
                "status must be completed after context swap"
            );
            assert_eq!(
                v["final_result"].as_str(),
                Some("async result text"),
                "final_result must match the async delegate's output"
            );
        }
        other => panic!("expected Structured output after context swap, got: {other:?}"),
    }

    assert_eq!(
        shared_registry.live_count().await,
        0,
        "handle must be reaped after the completed poll"
    );
}

// --- wait_seconds tests ---

#[tokio::test]
async fn wait_seconds_zero_behavior_unchanged() {
    // Explicit wait_seconds=0 must behave like omitting it: returns "running"
    // without blocking and without a hint field.
    let spawner = make_spawner(BlockingChildRunner {
        phase1_events: vec!["e1".into()],
    });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let out = DelegateOutput.invoke(json!({"id": bg_id, "wait_seconds": 0}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(ref v) => {
            assert_eq!(v["status"].as_str(), Some("running"), "wait_seconds=0 must return running");
            assert!(
                v.get("hint").map(|h| h.is_null()).unwrap_or(true),
                "wait_seconds=0 must not include hint, got: {out:?}"
            );
        }
        _ => panic!("expected Structured, got: {out:?}"),
    }
    assert_eq!(ctx.background_agents.live_count().await, 1, "handle must stay live");
}

#[tokio::test]
async fn wait_completes_early_when_child_finishes_during_wait() {
    // wait_seconds=30 with a child that completes mid-wait: must return terminal
    // result immediately (not block for 30s).
    let gate = Arc::new(tokio::sync::Notify::new());
    let spawner = make_spawner(TwoPhaseRunner { gate: gate.clone() });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    // Let phase-1 event arrive; child blocks on gate.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Signal gate after a yield — child completes during the wait below.
    let gate_clone = gate.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        gate_clone.notify_one();
    });

    let out = DelegateOutput.invoke(json!({"id": bg_id, "wait_seconds": 30.0}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("completed"),
                "must resolve as completed, not time out"
            );
            assert_eq!(v["final_result"].as_str(), Some("final answer"));
        }
        _ => panic!("expected Structured completed, got: {out:?}"),
    }
    assert_eq!(ctx.background_agents.live_count().await, 0, "handle must be reaped");
}

#[tokio::test]
async fn wait_deadline_expired_returns_running_with_hint_and_events() {
    // wait_seconds=0.05 (50 ms) with a permanently-blocking child: must return
    // status=running with a hint and include events buffered before the deadline.
    let spawner = make_spawner(BlockingChildRunner {
        phase1_events: vec!["partial_output".into()],
    });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    // Let the child emit its phase-1 event.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let out = DelegateOutput.invoke(json!({"id": bg_id, "wait_seconds": 0.05}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("running"), "deadline expiry must return running");
            assert!(
                v["hint"].as_str().is_some(),
                "deadline expiry must include hint, got: {v:?}"
            );
            let events = v["events"].as_array().expect("events must be an array");
            assert_eq!(events.len(), 1, "must include the pre-deadline event");
        }
        _ => panic!("expected Structured running+hint, got: {out:?}"),
    }
    assert_eq!(
        ctx.background_agents.live_count().await,
        1,
        "handle must remain live after deadline expiry"
    );
}

#[tokio::test]
async fn cancellation_during_wait_returns_cancelled() {
    // wait_seconds=30 with a blocking child that gets cancelled mid-wait:
    // must wake up and return status=cancelled without blocking for 30s.
    let spawner = make_spawner(BlockingChildRunner {
        phase1_events: vec![],
    });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    // Snapshot the cancel token before DelegateOutput removes the handle.
    let bg_agent_id: BackgroundAgentId = bg_id.parse().unwrap();
    let snapshot = ctx.background_agents.get(&bg_agent_id).await.unwrap();
    let cancel = snapshot.cancel.clone();

    // Fire the child's cancel token from a separate task after one yield,
    // so DelegateOutput is already inside its wait when the cancel arrives.
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancel.cancel();
    });

    let out = DelegateOutput.invoke(json!({"id": bg_id, "wait_seconds": 30.0}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("cancelled"),
                "cancelled child must surface status=cancelled"
            );
        }
        _ => panic!("expected Structured cancelled, got: {out:?}"),
    }
    assert_eq!(
        ctx.background_agents.live_count().await,
        0,
        "handle must be reaped after cancellation"
    );
}

// --- transcript recovery tests (no live handle in registry) ---

fn transcript_entry(bg_id: &BackgroundAgentId, event_type: &str, content: &str) -> String {
    let entry = TranscriptEntry {
        ts: Utc::now(),
        role: TranscriptRole::Agent {
            agent: bg_id.to_string(),
        },
        content: content.to_string(),
        event_type: event_type.to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    serde_json::to_string(&entry).expect("serialize transcript entry")
}

async fn write_sidechain(data_dir: &std::path::Path, bg_id: &BackgroundAgentId, lines: &[String]) {
    let dir = data_dir.join("messages").join("data");
    tokio::fs::create_dir_all(&dir).await.expect("create transcript dir");
    let path = dir.join(format!("{}.jsonl", bg_id));
    let contents = lines.join("\n") + "\n";
    tokio::fs::write(&path, contents).await.expect("write transcript");
}

#[tokio::test]
async fn transcript_fallback_completed() {
    let guard = crate::test_env::DataDirGuard::new();
    let bg_id = BackgroundAgentId::new();

    write_sidechain(
        guard.data_dir(),
        &bg_id,
        &[
            transcript_entry(&bg_id, "response", "the final answer"),
            transcript_entry(&bg_id, "session_completed", "completed"),
        ],
    )
    .await;

    let ctx = make_ctx();
    let tool = DelegateOutput;
    let out = tool.invoke(json!({"id": bg_id.to_string()}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("completed"), "status must be completed");
            assert_eq!(
                v["final_result"].as_str(),
                Some("the final answer"),
                "final_result must come from last response entry"
            );
            assert!(
                v["recovered_from_transcript"].as_str().is_some(),
                "recovery note must be present"
            );
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

#[tokio::test]
async fn transcript_fallback_failed() {
    let guard = crate::test_env::DataDirGuard::new();
    let bg_id = BackgroundAgentId::new();

    write_sidechain(
        guard.data_dir(),
        &bg_id,
        &[
            transcript_entry(&bg_id, "response", "partial output"),
            transcript_entry(&bg_id, "session_failed", "provider returned 500"),
        ],
    )
    .await;

    let ctx = make_ctx();
    let tool = DelegateOutput;
    let out = tool.invoke(json!({"id": bg_id.to_string()}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("failed"), "status must be failed");
            assert_eq!(
                v["error"].as_str(),
                Some("provider returned 500"),
                "error must carry the session_failed content"
            );
            assert_eq!(
                v["final_result"].as_str(),
                Some("partial output"),
                "partial final_result must be surfaced"
            );
            assert!(
                v["recovered_from_transcript"].as_str().is_some(),
                "recovery note must be present"
            );
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

#[tokio::test]
async fn transcript_fallback_cancelled() {
    let guard = crate::test_env::DataDirGuard::new();
    let bg_id = BackgroundAgentId::new();

    write_sidechain(
        guard.data_dir(),
        &bg_id,
        &[
            transcript_entry(&bg_id, "response", "work in progress"),
            transcript_entry(&bg_id, "session_cancelled", "cancelled"),
        ],
    )
    .await;

    let ctx = make_ctx();
    let tool = DelegateOutput;
    let out = tool.invoke(json!({"id": bg_id.to_string()}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("cancelled"), "status must be cancelled");
            assert!(
                v["recovered_from_transcript"].as_str().is_some(),
                "recovery note must be present"
            );
        }
        other => panic!("expected Structured, got: {other:?}"),
    }
}

/// REACHABILITY TEST — the one that would have caught the original defect.
///
/// Reproduces the ordinary production situation exactly: the registry entry is
/// ABSENT (as it is after any CLI continuation-step boundary) while the delegate
/// is alive and its transcript is growing with no terminal event yet. Driven
/// through `DelegateOutput::invoke` — the real tool entry point an agent calls —
/// not through `recover_from_transcript` directly, because the defect was never
/// that the helper computed the wrong thing in isolation; it was that the live
/// path *arrived* at a branch which asserted failure.
#[tokio::test]
async fn invoke_reports_indeterminate_for_live_delegate_dropped_from_registry() {
    let guard = crate::test_env::DataDirGuard::new();
    let bg_id = BackgroundAgentId::new();

    // A transcript that is being appended to, with no terminal event: the
    // delegate is still working.
    write_sidechain(
        guard.data_dir(),
        &bg_id,
        &[
            transcript_entry(&bg_id, "async_launched", "Explore"),
            transcript_entry(&bg_id, "response", "partial work"),
            transcript_entry(&bg_id, "tool_use", "Read"),
        ],
    )
    .await;

    let ctx = make_ctx();
    // Precondition of the whole scenario: nothing in the registry to find.
    assert_eq!(
        ctx.background_agents.live_count().await,
        0,
        "scenario requires an empty registry (the CLI step-boundary drop)"
    );

    let out = DelegateOutput
        .invoke(json!({"id": bg_id.to_string()}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("indeterminate"),
                "a live delegate with no terminal event must NOT be reported as failed"
            );
            assert_eq!(
                v["reason"].as_str(),
                Some(super::REASON_NO_TERMINAL_EVENT),
                "reason must distinguish this from a missing transcript"
            );
            assert_eq!(
                v["event_count"].as_u64(),
                Some(3),
                "event_count must report every observed event"
            );
            assert!(
                v["last_event_at"].as_str().is_some(),
                "last_event_at must be populated from the final transcript line"
            );
            assert!(
                v["last_activity_age_seconds"].as_i64().is_some(),
                "last_activity_age_seconds must be populated"
            );
            assert_eq!(
                v["final_result"].as_str(),
                Some("partial work"),
                "partial output observed so far must still be surfaced"
            );
            let hint = v["hint"].as_str().expect("hint must be present");
            assert!(
                hint.contains("still running or was orphaned"),
                "hint must read as an observation, got: {hint}"
            );
            assert!(
                hint.contains("last activity"),
                "hint must report the last-activity age, got: {hint}"
            );

            // The regression guard: the word "failed" must not appear anywhere
            // in the payload, and no `error` field may be set.
            let payload = serde_json::to_string(&v).expect("serialize payload");
            assert!(
                !payload.contains("failed"),
                "no part of an indeterminate result may say 'failed', got: {payload}"
            );
            assert!(
                v.get("error").is_none() || v["error"].is_null(),
                "indeterminate must not populate an error field, got: {v:?}"
            );
        }
        other => panic!("expected Structured indeterminate, got: {other:?}"),
    }
}

/// A transcript containing ONLY the spawn marker must also be indeterminate —
/// the marker must never be mistaken for an outcome.
#[tokio::test]
async fn invoke_reports_indeterminate_for_spawn_marker_only_transcript() {
    let guard = crate::test_env::DataDirGuard::new();
    let bg_id = BackgroundAgentId::new();

    write_sidechain(
        guard.data_dir(),
        &bg_id,
        &[transcript_entry(&bg_id, "async_launched", "Explore")],
    )
    .await;

    let ctx = make_ctx();
    let out = DelegateOutput
        .invoke(json!({"id": bg_id.to_string()}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("indeterminate"),
                "a spawn marker alone is not an outcome"
            );
            assert_eq!(
                v["reason"].as_str(),
                Some(super::REASON_NO_TERMINAL_EVENT),
                "a marker-only transcript exists, so the reason is 'no terminal event'"
            );
            assert_eq!(v["event_count"].as_u64(), Some(1), "the marker counts as one event");
        }
        other => panic!("expected Structured indeterminate, got: {other:?}"),
    }
}

/// The spawn marker's event type must not satisfy the terminal predicate that
/// gates every outcome-asserting status.
#[test]
fn spawn_marker_event_type_is_not_terminal() {
    assert!(
        !super::is_terminal_event_type("async_launched"),
        "the spawn marker must never read as a terminal event"
    );
    for progress in ["response", "tool_use", "text_complete"] {
        assert!(
            !super::is_terminal_event_type(progress),
            "{progress} must not read as terminal"
        );
    }
    for terminal in ["session_completed", "session_cancelled", "session_failed"] {
        assert!(
            super::is_terminal_event_type(terminal),
            "{terminal} must read as terminal"
        );
    }
}

#[tokio::test]
async fn transcript_fallback_missing_file_is_indeterminate_with_distinct_reason() {
    let _guard = crate::test_env::DataDirGuard::new();
    let bg_id = BackgroundAgentId::new();
    // No file written at all — nothing has been persisted yet.

    let ctx = make_ctx();
    let out = DelegateOutput
        .invoke(json!({"id": bg_id.to_string()}), &ctx)
        .await
        .unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(
                v["status"].as_str(),
                Some("indeterminate"),
                "a missing transcript is not proof of failure"
            );
            assert_eq!(
                v["reason"].as_str(),
                Some(super::REASON_NO_TRANSCRIPT),
                "missing-file reason must differ from the no-terminal-event reason"
            );
            assert_ne!(
                super::REASON_NO_TRANSCRIPT,
                super::REASON_NO_TERMINAL_EVENT,
                "the two indeterminate sub-cases must stay distinguishable"
            );
            assert!(
                v["last_event_at"].is_null(),
                "no file means no last_event_at"
            );
            assert_eq!(v["event_count"].as_u64(), Some(0), "no file means no events");
        }
        other => panic!("expected Structured indeterminate, got: {other:?}"),
    }
}

// --- stats field tests ---

/// Verifies that a completed result includes duration_ms and num_turns when the
/// runner attaches stats via TaskFinalReport::with_stats.
#[tokio::test]
async fn completed_result_includes_stats_when_runner_sets_them() {
    struct StatsRunner {
        duration_ms: u64,
        num_turns: u32,
    }
    impl ChildRunner for StatsRunner {
        fn launch(
            &self,
            _child_ctx: RunnerContext,
            _initial_prompt: String,
            background_agent_id: BackgroundAgentId,
            event_tx: broadcast::Sender<RunnerEvent>,
            _target_profile: Option<ao_protocol::agent::AgentProfile>,
        ) -> tokio::task::JoinHandle<Result<TaskFinalReport, AoError>> {
            let bg_id = background_agent_id;
            let d = self.duration_ms;
            let t = self.num_turns;
            tokio::spawn(async move {
                let _ = event_tx.send(RunnerEvent::Completed {
                    background_agent_id: bg_id,
                });
                Ok(TaskFinalReport::completed(Some("the answer".to_string()))
                    .with_stats(d, t))
            })
        }
    }

    let spawner = make_spawner(StatsRunner { duration_ms: 1500, num_turns: 3 });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let out = DelegateOutput.invoke(json!({"id": bg_id}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("completed"), "status must be completed");
            assert_eq!(v["final_result"].as_str(), Some("the answer"));
            let stats = &v["stats"];
            assert!(
                !stats.is_null(),
                "completed result must include stats when runner sets them; got: {v}"
            );
            assert_eq!(
                stats["duration_ms"].as_u64(),
                Some(1500),
                "stats.duration_ms must match the runner's value"
            );
            assert_eq!(
                stats["num_turns"].as_u64().map(|n| n as u32),
                Some(3),
                "stats.num_turns must match the runner's value"
            );
        }
        _ => panic!("expected Structured output, got: {out:?}"),
    }
    assert_eq!(ctx.background_agents.live_count().await, 0);
}

/// Verifies that when a runner does not set stats, the stats field is null
/// rather than absent.
#[tokio::test]
async fn completed_result_stats_null_when_runner_does_not_set_them() {
    let spawner = make_spawner(ScriptedChildRunner {
        text_events: vec![],
        report: TaskFinalReport::completed(Some("answer without stats".to_string())),
    });
    let ctx = make_ctx();
    let bg_id = spawn_background(spawner, &ctx).await;

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let out = DelegateOutput.invoke(json!({"id": bg_id}), &ctx).await.unwrap();

    match out {
        ToolOutput::Structured(v) => {
            assert_eq!(v["status"].as_str(), Some("completed"), "status must be completed");
            assert!(
                v["stats"].is_null(),
                "stats must be null when runner does not set them; got: {v}"
            );
        }
        _ => panic!("expected Structured output, got: {out:?}"),
    }
}
