use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::background_agents::child_runner::ChildRunner;
use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, RunnerEvent, SubagentDefinition, SubagentRegistry, SubagentSpawner,
    TaskFinalReport,
};
use ao_engine_tools_core::RunnerContext;
use ao_engine_tools_runner::background_agents::FileSidechainPersister;
use ao_protocol::error::AoError;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// Scripted child that emits a known sequence of events then completes.
struct ScriptedChild {
    texts: Vec<String>,
}

impl ChildRunner for ScriptedChild {
    fn launch(
        &self,
        _child_ctx: RunnerContext,
        _initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        let texts = self.texts.clone();
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            for text in texts {
                let _ = event_tx.send(RunnerEvent::AssistantText {
                    background_agent_id: bg_id.clone(),
                    text,
                });
            }
            let _ = event_tx.send(RunnerEvent::Completed {
                background_agent_id: bg_id,
            });
            Ok(TaskFinalReport::completed(None))
        })
    }
}

fn make_parent_ctx() -> RunnerContext {
    RunnerContext::new_with_cwd("parent-session", "parent-agent", PathBuf::from("/tmp"))
}

/// No built-in catalog ships with the engine, so every spawner here owns its
/// own registered fixture rather than depending on a catalog entry.
fn registry_with_test_fixture() -> SubagentRegistry {
    let mut reg = SubagentRegistry::new();
    reg.register(SubagentDefinition {
        id: "test-agent".to_string(),
        description: "Test fixture agent for sidechain tests".to_string(),
        allowed_tools: vec![],
        system_prompt_fragment: String::new(),
        model_override: None,
    });
    reg
}

/// Spawning a child with a FileSidechainPersister writes a JSONL transcript
/// file under <data_root>/messages/data/<child_agent_id>.jsonl. Every entry
/// carries parent_agent_id, background_agent_id, subagent_type, and spawned_at
/// in its metadata field.
#[tokio::test]
async fn spawning_child_writes_transcript_with_parent_agent_id() {
    let temp = tempfile::TempDir::new().unwrap();
    let persister = FileSidechainPersister::new(temp.path());

    let spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(ScriptedChild {
            texts: vec!["hello from child".to_string()],
        }))
        .with_sidechain_persister(persister);

    let parent_ctx = make_parent_ctx();
    let (bg_id, mut rx) = spawner
        .spawn(&parent_ctx, "test-agent", "find relevant code".to_string())
        .await
        .expect("spawn must succeed");

    // Drain events until the terminal event arrives.
    loop {
        match rx.recv().await {
            Ok(RunnerEvent::Completed { .. }) | Ok(RunnerEvent::Cancelled { .. }) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    // Allow the sidecar persistence task to finish writing to disk.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Transcript file must exist at the expected path.
    let transcript_path = temp
        .path()
        .join("messages")
        .join("data")
        .join(format!("{}.jsonl", bg_id));

    assert!(
        transcript_path.exists(),
        "transcript file must be written at {transcript_path:?}"
    );

    // Parse all JSONL entries.
    let raw = std::fs::read_to_string(&transcript_path).unwrap();
    let entries: Vec<serde_json::Value> = raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("each line must be valid JSON"))
        .collect();

    assert!(
        !entries.is_empty(),
        "transcript must contain at least one entry"
    );

    // Every entry must carry the required metadata fields.
    for entry in &entries {
        let meta = entry["metadata"]
            .as_object()
            .expect("every entry must have a metadata object");

        assert_eq!(
            meta["parent_agent_id"].as_str().unwrap(),
            "parent-agent",
            "parent_agent_id must match the parent's agent_id"
        );
        assert_eq!(
            meta["background_agent_id"].as_str().unwrap(),
            bg_id.as_str(),
            "background_agent_id must match the child's id"
        );
        assert_eq!(
            meta["subagent_type"].as_str().unwrap(),
            "test-agent",
            "subagent_type must be persisted"
        );
        assert!(
            meta.contains_key("spawned_at"),
            "spawned_at must be present in metadata"
        );
    }

    // The AssistantText event content must appear as a text_complete entry.
    let has_text_entry = entries.iter().any(|e| {
        e["content"].as_str() == Some("hello from child")
            && e["event_type"].as_str() == Some("text_complete")
    });
    assert!(
        has_text_entry,
        "transcript must include the AssistantText event with correct content and event_type"
    );

    // A session_completed entry must be present (from the Completed event).
    let has_completed_entry = entries
        .iter()
        .any(|e| e["event_type"].as_str() == Some("session_completed"));
    assert!(
        has_completed_entry,
        "transcript must include a session_completed entry"
    );
}

/// The path layout follows the standard convention: each child's events are
/// stored at <data_root>/messages/data/<child_agent_id>.jsonl, discoverable
/// by agent_id without directory traversal.
#[tokio::test]
async fn transcript_path_follows_convention() {
    let temp = tempfile::TempDir::new().unwrap();
    let persister = FileSidechainPersister::new(temp.path());

    let spawner = SubagentSpawner::new(Arc::new(registry_with_test_fixture()))
        .with_child_runner(Arc::new(ScriptedChild { texts: vec![] }))
        .with_sidechain_persister(persister);

    let parent_ctx = make_parent_ctx();
    let (bg_id, mut rx) = spawner
        .spawn(&parent_ctx, "test-agent", "probe the layout".to_string())
        .await
        .expect("spawn must succeed");

    loop {
        match rx.recv().await {
            Ok(RunnerEvent::Completed { .. }) | Ok(RunnerEvent::Cancelled { .. }) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The file must sit directly under messages/data/, not in a session
    // subdirectory.  The parent_agent_id link in metadata allows the UI to
    // associate it with the parent without a directory hierarchy.
    let expected = temp
        .path()
        .join("messages")
        .join("data")
        .join(format!("{}.jsonl", bg_id));

    assert!(
        expected.exists(),
        "file must be at the Phase 3 convention path: {expected:?}"
    );

    // No extra nesting — the data dir must contain exactly this file.
    let data_dir = temp.path().join("messages").join("data");
    let entries = tokio::fs::read_dir(&data_dir)
        .await
        .unwrap()
        .next_entry()
        .await
        .unwrap()
        .expect("at least one entry");
    assert_eq!(
        entries.file_name().to_string_lossy(),
        format!("{}.jsonl", bg_id)
    );
}

// --- spawn marker ---

/// A child that registers and then emits nothing at all, so anything found in
/// the transcript must have been written by the spawn path itself.
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
            // Hold the sender so the persistence sidecar stays open, and never
            // emit — this models a delegate that is still working.
            let _keep_open = event_tx;
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Ok(TaskFinalReport::completed(None))
        })
    }
}

fn minimal_profile() -> ao_protocol::agent::AgentProfile {
    serde_json::from_str(
        r#"{"id":"Explore","name":"Explore","description":"","provider":{"type":"Cli","command":"claude","args":[]},"model":null,"system_prompt":null,"tools":null,"max_instances":1,"timeout_seconds":300,"serialize":true}"#,
    )
    .expect("minimal profile must deserialize")
}

/// An async spawn must leave a transcript on disk *before it returns the id*,
/// so a poll that arrives before the child's first event finds a file rather
/// than nothing. Without this marker the transcript is created lazily on the
/// first child event, and every poll landing in that window sees no file.
///
/// The child here emits nothing, so a non-empty transcript proves the spawn
/// path wrote the marker itself — not that some child event raced in.
#[tokio::test]
async fn async_spawn_writes_non_terminal_marker_before_returning() {
    let temp = tempfile::TempDir::new().unwrap();
    let persister = FileSidechainPersister::new(temp.path());

    let spawner = SubagentSpawner::new(Arc::new(SubagentRegistry::new()))
        .with_child_runner(Arc::new(SilentChild))
        .with_sidechain_persister(persister);

    let parent_ctx = make_parent_ctx();
    let profile = minimal_profile();

    let bg_id = spawner
        .spawn_named_async_id(
            &parent_ctx,
            &profile,
            "do the thing".to_string(),
            false,
            "Explore".to_string(),
        )
        .await
        .expect("async spawn must succeed");

    // No sleep: the marker must already be on disk by the time the id is handed
    // back. A sleep here would hide exactly the race this marker closes.
    let transcript_path = temp
        .path()
        .join("messages")
        .join("data")
        .join(format!("{}.jsonl", bg_id));

    assert!(
        transcript_path.exists(),
        "transcript must exist as soon as spawn returns, at {transcript_path:?}"
    );

    let raw = std::fs::read_to_string(&transcript_path).expect("read transcript");
    assert!(
        !raw.trim().is_empty(),
        "transcript must be non-empty once the marker is written"
    );

    let entries: Vec<serde_json::Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each line must be valid JSON"))
        .collect();

    assert_eq!(
        entries.len(),
        1,
        "exactly one marker entry expected from a silent child, got: {entries:?}"
    );

    let marker = &entries[0];
    assert_eq!(
        marker["event_type"].as_str(),
        Some("async_launched"),
        "the marker must use the non-terminal async_launched event type"
    );

    // The marker must not be mistaken for an outcome. These are the event types
    // DelegateOutput's transcript recovery treats as terminal; the marker must
    // match none of them.
    for terminal in ["session_completed", "session_cancelled", "session_failed"] {
        assert_ne!(
            marker["event_type"].as_str(),
            Some(terminal),
            "spawn marker must never read as the terminal event {terminal}"
        );
    }

    // The marker still carries the metadata the UI needs to link the sidechain.
    let meta = marker["metadata"]
        .as_object()
        .expect("marker must carry metadata");
    assert_eq!(meta["parent_agent_id"].as_str(), Some("parent-agent"));
    assert_eq!(meta["background_agent_id"].as_str(), Some(bg_id.as_str()));

    // And the handle is registered — the marker is written after the insert, so
    // its presence must never imply an unregistered delegate.
    assert_eq!(
        parent_ctx.background_agents.live_count().await,
        1,
        "the handle must be registered before the marker is written"
    );
}
