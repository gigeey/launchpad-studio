//! Cross-crate reachability tests for delegation liveness reporting.
//!
//! These exist because the defect they cover was never a wrong computation in a
//! helper — it was that the live path *arrived* at a branch which asserted
//! failure for a healthy delegate. So every test here drives the real
//! `DelegateOutput::invoke` entry point an agent actually calls, against a
//! transcript produced by the real `FileSidechainPersister`, and asserts only on
//! the public JSON contract a caller can observe.
//!
//! The scenario under test is the ordinary one, not an edge case: an async
//! delegate's registry entry is owned per-MCP-session, so a CLI-backed parent
//! drops it at its very next continuation step while the delegate keeps running.
//! Every poll after that lands on the transcript instead of the live handle.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use ao_engine_tools_core::background_agents::child_runner::ChildRunner;
use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, RunnerEvent, SubagentRegistry, SubagentSpawner, TaskFinalReport,
};
use ao_engine_tools_core::{EngineTool, RunnerContext, ToolOutput};
use ao_engine_tools_engine::DelegateOutput;
use ao_engine_tools_runner::background_agents::FileSidechainPersister;
use ao_protocol::error::AoError;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};
use chrono::Utc;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// Serialises the `LAUNCHPAD_STUDIO_DATA_DIR` override across tests in this
/// binary — they all run on threads of one process.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// RAII guard pinning the data root to a fresh tempdir for one test.
struct DataDirGuard {
    tmp: tempfile::TempDir,
    prior: Option<String>,
    _lock: MutexGuard<'static, ()>,
}

impl DataDirGuard {
    fn new() -> Self {
        let lock = env_lock();
        let tmp = tempfile::tempdir().expect("create data-root tempdir");
        let prior = std::env::var("LAUNCHPAD_STUDIO_DATA_DIR").ok();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        Self {
            tmp,
            prior,
            _lock: lock,
        }
    }

    fn path(&self) -> &std::path::Path {
        self.tmp.path()
    }
}

impl Drop for DataDirGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(v) => std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", v),
            None => std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR"),
        }
    }
}

/// A child that registers and then emits nothing — a delegate still working.
struct SilentChild;

impl ChildRunner for SilentChild {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        _background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        tokio::spawn(async move {
            let _keep_open = event_tx;
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Ok(TaskFinalReport::completed(None))
        })
    }
}

fn make_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("parent-session", "parent-agent", PathBuf::from("/tmp"))
}

fn minimal_profile() -> ao_protocol::agent::AgentProfile {
    serde_json::from_str(
        r#"{"id":"Explore","name":"Explore","description":"","provider":{"type":"Cli","command":"claude","args":[]},"model":null,"system_prompt":null,"tools":null,"max_instances":1,"timeout_seconds":300,"serialize":true}"#,
    )
    .expect("minimal profile must deserialize")
}

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

async fn write_transcript(data_root: &std::path::Path, bg_id: &BackgroundAgentId, lines: &[String]) {
    let dir = data_root.join("messages").join("data");
    tokio::fs::create_dir_all(&dir)
        .await
        .expect("create transcript dir");
    tokio::fs::write(
        dir.join(format!("{}.jsonl", bg_id)),
        lines.join("\n") + "\n",
    )
    .await
    .expect("write transcript");
}

fn structured(out: ToolOutput) -> serde_json::Value {
    match out {
        ToolOutput::Structured(v) => v,
        other => panic!("expected a Structured result, got: {other:?}"),
    }
}

/// THE REACHABILITY TEST — end to end, both halves of the fix together.
///
/// A real async spawn writes a real spawn marker through the real persister;
/// the delegate is still running; the registry entry is then absent (the CLI
/// step-boundary drop). Polling must report `indeterminate`, never `failed`.
///
/// This also proves the two changes are safe as a pair: the spawn marker moves
/// every mid-flight poll out of the "no file" branch and into the "file with no
/// terminal event" branch, which is precisely the branch that used to lie.
#[tokio::test]
async fn live_delegate_dropped_from_registry_reports_indeterminate_not_failed() {
    let guard = DataDirGuard::new();

    let spawner = SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(SilentChild))
        .with_sidechain_persister(
            FileSidechainPersister::resolve().expect("persister must resolve the data root"),
        );

    let spawning_ctx = make_ctx();
    let bg_id = spawner
        .spawn_named_async_id(
            &spawning_ctx,
            &minimal_profile(),
            "do the thing".to_string(),
            false,
            "Explore".to_string(),
        )
        .await
        .expect("async spawn must succeed");

    // The marker must already be on disk — no sleep, or we would hide the very
    // race it closes.
    let transcript = guard
        .path()
        .join("messages")
        .join("data")
        .join(format!("{}.jsonl", bg_id));
    assert!(
        transcript.exists(),
        "spawn marker must create the transcript before spawn returns: {transcript:?}"
    );

    // A FRESH context: the delegate is alive, but nothing is registered here.
    // This is what a CLI-backed parent's next continuation step looks like.
    let polling_ctx = make_ctx();
    assert_eq!(
        polling_ctx.background_agents.live_count().await,
        0,
        "scenario requires an empty registry"
    );

    let out = DelegateOutput
        .invoke(json!({"id": bg_id.to_string()}), &polling_ctx)
        .await
        .expect("invoke must not error");

    let v = structured(out);
    assert_eq!(
        v["status"].as_str(),
        Some("indeterminate"),
        "a live delegate must never be reported as failed; got {v:?}"
    );
    assert_eq!(
        v["reason"].as_str(),
        Some("running-or-orphaned-no-terminal-event"),
        "reason must say a transcript exists but has no terminal event"
    );
    assert!(
        v["last_event_at"].as_str().is_some(),
        "last_event_at must be populated from the marker"
    );
    assert!(
        v["last_activity_age_seconds"].as_i64().is_some(),
        "last_activity_age_seconds must be populated"
    );
    assert_eq!(
        v["event_count"].as_u64(),
        Some(1),
        "the spawn marker counts as one observed event"
    );

    let payload = serde_json::to_string(&v).expect("serialize payload");
    assert!(
        !payload.contains("failed"),
        "no part of an indeterminate result may say 'failed': {payload}"
    );
}

/// Regression guard: narrowing `failed` must not make real failures
/// unreportable. A terminal failure event still yields `failed`.
#[tokio::test]
async fn terminal_failure_event_still_reports_failed() {
    let guard = DataDirGuard::new();
    let bg_id = BackgroundAgentId::new();

    write_transcript(
        guard.path(),
        &bg_id,
        &[
            transcript_entry(&bg_id, "async_launched", "Explore"),
            transcript_entry(&bg_id, "response", "partial output"),
            transcript_entry(&bg_id, "session_failed", "provider returned 500"),
        ],
    )
    .await;

    let ctx = make_ctx();
    let out = DelegateOutput
        .invoke(json!({"id": bg_id.to_string()}), &ctx)
        .await
        .expect("invoke must not error");

    let v = structured(out);
    assert_eq!(
        v["status"].as_str(),
        Some("failed"),
        "an observed terminal failure must still report failed"
    );
    assert_eq!(
        v["error"].as_str(),
        Some("provider returned 500"),
        "the real failure reason must survive"
    );
}

/// A terminal success event still yields `completed`, with the final response.
#[tokio::test]
async fn terminal_success_event_still_reports_completed() {
    let guard = DataDirGuard::new();
    let bg_id = BackgroundAgentId::new();

    write_transcript(
        guard.path(),
        &bg_id,
        &[
            transcript_entry(&bg_id, "async_launched", "Explore"),
            transcript_entry(&bg_id, "response", "the final answer"),
            transcript_entry(&bg_id, "session_completed", "completed"),
        ],
    )
    .await;

    let ctx = make_ctx();
    let out = DelegateOutput
        .invoke(json!({"id": bg_id.to_string()}), &ctx)
        .await
        .expect("invoke must not error");

    let v = structured(out);
    assert_eq!(v["status"].as_str(), Some("completed"));
    assert_eq!(
        v["final_result"].as_str(),
        Some("the final answer"),
        "the final response must be recovered"
    );
}

/// A well-formed id with nothing persisted is indeterminate too — but with a
/// DIFFERENT reason, so "nothing yet" stays distinguishable from "running".
#[tokio::test]
async fn absent_transcript_is_indeterminate_with_distinct_reason() {
    let _guard = DataDirGuard::new();
    let bg_id = BackgroundAgentId::new();

    let ctx = make_ctx();
    let out = DelegateOutput
        .invoke(json!({"id": bg_id.to_string()}), &ctx)
        .await
        .expect("invoke must not error");

    let v = structured(out);
    assert_eq!(
        v["status"].as_str(),
        Some("indeterminate"),
        "a missing transcript is not proof of failure"
    );
    assert_eq!(
        v["reason"].as_str(),
        Some("no-transcript-found"),
        "missing-transcript reason must differ from the no-terminal-event reason"
    );
    assert!(v["last_event_at"].is_null(), "no file means no last_event_at");
    assert_eq!(v["event_count"].as_u64(), Some(0));
}
