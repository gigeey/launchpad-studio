use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::Utc;
use futures_util::stream::{self, StreamExt};
use tokio_stream::wrappers::BroadcastStream;

use ao_engine::AppState;
use ao_protocol::event::{AgentEvent, AgentEventPayload};

/// Map an AgentEventPayload variant to its SSE event type name.
fn event_type_name(payload: &AgentEventPayload) -> &'static str {
    match payload {
        AgentEventPayload::RunStarted => "run_started",
        AgentEventPayload::RunEnded { .. } => "run_ended",
        AgentEventPayload::TextDelta { .. } => "text_delta",
        AgentEventPayload::TextComplete { .. } => "text_complete",
        AgentEventPayload::ThinkingStarted => "thinking_started",
        AgentEventPayload::ThinkingDelta { .. } => "thinking_delta",
        AgentEventPayload::ThinkingEnded { .. } => "thinking_ended",
        AgentEventPayload::ToolCallStarted { .. } => "tool_call_started",
        AgentEventPayload::ToolCallCompleted { .. } => "tool_call_completed",
        AgentEventPayload::MessageReceived { .. } => "message_received",
        AgentEventPayload::MessageProcessingStarted { .. } => "message_processing_started",
        AgentEventPayload::AgentBusy { .. } => "agent_busy",
        AgentEventPayload::Error { .. } => "error",
        AgentEventPayload::Usage { .. } => "usage",
        AgentEventPayload::DelegationStarted { .. } => "delegation_started",
        AgentEventPayload::DelegationCompleted { .. } => "delegation_completed",
        AgentEventPayload::TeamRoundStarted { .. } => "team_round_started",
        AgentEventPayload::TeamRoundCompleted { .. } => "team_round_completed",
        AgentEventPayload::WorkflowTaskCreated { .. } => "workflow_task_created",
        AgentEventPayload::PhaseStarted { .. } => "phase_started",
        AgentEventPayload::PhaseCompleted { .. } => "phase_completed",
        AgentEventPayload::PhaseSkipped { .. } => "phase_skipped",
        AgentEventPayload::PhaseFailed { .. } => "phase_failed",
        AgentEventPayload::PhasePaused { .. } => "phase_paused",
        AgentEventPayload::WorkflowPhaseProgress { .. } => "workflow_phase_progress",
        AgentEventPayload::WorkflowCompleted { .. } => "workflow_completed",
        AgentEventPayload::WorkflowTaskStarted { .. } => "workflow_task_started",
        AgentEventPayload::WorkflowTaskFailed { .. } => "workflow_task_failed",
        AgentEventPayload::WorkflowTaskStopped { .. } => "workflow_task_stopped",
        AgentEventPayload::WorkflowTaskReopened { .. } => "workflow_task_reopened",
        AgentEventPayload::SystemMessage { .. } => "system_message",
        AgentEventPayload::AgentActionStarted { .. } => "agent_action_started",
        AgentEventPayload::AgentActionCompleted { .. } => "agent_action_completed",
        AgentEventPayload::ToolUseStarted { .. } => "tool_use_started",
        AgentEventPayload::ToolUseCompleted { .. } => "tool_use_completed",
        AgentEventPayload::HiddenTranscriptEntry { .. } => "hidden_transcript_entry",
        AgentEventPayload::TasklistCreated { .. } => "tasklist.created",
        AgentEventPayload::TasklistTaskUpdated { .. } => "tasklist.task_updated",
        AgentEventPayload::TasklistCompleted { .. } => "tasklist.completed",
        AgentEventPayload::TasklistFailed { .. } => "tasklist.failed",
        AgentEventPayload::TasklistStatusChanged { .. } => "tasklist.status_changed",
        AgentEventPayload::TasklistTaskAdded { .. } => "tasklist.task_added",
        AgentEventPayload::TasklistWoke { .. } => "tasklist.woke",
        AgentEventPayload::TasklistSlept { .. } => "tasklist.slept",
        AgentEventPayload::MemorySaved { .. } => "memory_saved",
        AgentEventPayload::ToolProgress { .. } => "tool_progress",
        AgentEventPayload::TodoListCreated { .. } => "todo_list.created",
        AgentEventPayload::TodoListComplete { .. } => "todo_list.complete",
        AgentEventPayload::DelegateStarted { .. } => "delegate.started",
        AgentEventPayload::DelegateComplete { .. } => "delegate.complete",
        AgentEventPayload::TaskDeferred { .. } => "task.deferred",
        AgentEventPayload::FormRequest { .. } => "form_request",
        AgentEventPayload::FormPosted { .. } => "form_posted",
        AgentEventPayload::FormResolved { .. } => "form_resolved",
        AgentEventPayload::ProjectStateChanged { .. } => "project.state_changed",
        AgentEventPayload::AgentSnapshotUpdated { .. } => "agent.snapshot_updated",
        AgentEventPayload::ThreadRenamed { .. } => "thread_renamed",
        AgentEventPayload::ThreadCreated { .. } => "thread_created",
    }
}

/// Convert an AgentEvent to an SSE Event.
fn agent_event_to_sse(event: &AgentEvent) -> Result<Event, Infallible> {
    let event_name = event_type_name(&event.payload);
    let data = serde_json::to_string(event).unwrap_or_default();
    Ok(Event::default().event(event_name).data(data))
}

/// GET /tasks/{id}/stream — SSE streaming endpoint for all events related to a task.
/// Filters events where agent_id starts with "task:{task_id}:phase:".
pub async fn stream_task_events(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let prefix = format!("task:{}:phase:", task_id);

    // Check for active runs across all phases of this task
    let active_entries = state.instance_registry.active_runs_by_prefix(&prefix).await;
    let mut initial_events: Vec<AgentEvent> = Vec::new();

    for (_registry_key, run_ids) in &active_entries {
        for run_id in run_ids {
            if let Some(record) = state.process_supervisor.get_record(run_id) {
                initial_events.push(AgentEvent {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    run_id: run_id.clone(),
                    seq: 0,
                    ts: Utc::now(),
                    agent_id: _registry_key.clone(),
                    thread_id: None,
                    payload: AgentEventPayload::AgentBusy {
                        run_id: run_id.clone(),
                        started_at: record.started_at,
                    },
                });
            }
        }
    }

    let initial_stream = stream::iter(initial_events).map(|event| agent_event_to_sse(&event));

    let rx = state.event_bus.subscribe();
    let broadcast_stream = BroadcastStream::new(rx);

    let task_id_clone = task_id.clone();
    let event_stream = broadcast_stream.filter_map(move |result| {
        let prefix = prefix.clone();
        let tid = task_id_clone.clone();
        async move {
            match result {
                // Match phase agent events (chat messages, text deltas, etc.)
                Ok(ref event) if event.agent_id.starts_with(&prefix) => {
                    Some(agent_event_to_sse(event))
                }
                // Match workflow events (phase_started, phase_completed, etc.)
                // These are emitted with run_id = task_id
                Ok(ref event) if event.run_id == tid => {
                    Some(agent_event_to_sse(event))
                }
                _ => None,
            }
        }
    });

    let combined = initial_stream.chain(event_stream);
    Sse::new(combined).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// GET /projects/{project_id}/stream — SSE stream for project-channel events.
pub async fn stream_project_events(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let synthetic_agent_id = format!("project:{project_id}");
    let registry_prefix = format!("project:{}:", project_id);
    let active_entries = state.instance_registry.active_runs_by_prefix(&registry_prefix).await;
    let mut initial_events: Vec<AgentEvent> = Vec::new();

    for (_registry_key, run_ids) in &active_entries {
        for run_id in run_ids {
            if let Some(record) = state.process_supervisor.get_record(run_id) {
                initial_events.push(AgentEvent {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    run_id: run_id.clone(),
                    seq: 0,
                    ts: Utc::now(),
                    agent_id: synthetic_agent_id.clone(),
                    thread_id: None,
                    payload: AgentEventPayload::AgentBusy {
                        run_id: run_id.clone(),
                        started_at: record.started_at,
                    },
                });
            }
        }
    }

    let initial_stream = stream::iter(initial_events).map(|event| agent_event_to_sse(&event));

    let rx = state.event_bus.subscribe();
    let broadcast_stream = BroadcastStream::new(rx);

    let event_stream = broadcast_stream.filter_map(move |result| {
        let agent_id = synthetic_agent_id.clone();
        async move {
            match result {
                Ok(event) if event.agent_id == agent_id => Some(agent_event_to_sse(&event)),
                _ => None,
            }
        }
    });

    let combined = initial_stream.chain(event_stream);
    Sse::new(combined).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Build the connect-time replay events for `/system/stream`: a synthetic
/// `AgentBusy` for every active run across every registry channel (plain
/// agent ids and the `team:`/`project:`/`task:…:phase:`/`tasklist:` synthetic
/// keys alike), plus a synthetic `DelegationStarted` for team-scoped runs —
/// the same initial-state events each per-entity endpoint (`stream_events`,
/// `stream_project_events`, …) emits on its own connect. That makes this stream
/// a true superset of every per-entity endpoint, so a client multiplexing all
/// channels through this one connection sees the same connect-time state
/// those endpoints provide. Split out from `stream_system_events` so the
/// replay logic can be exercised directly in tests without going through SSE
/// framing.
async fn build_system_replay_events(state: &AppState) -> Vec<AgentEvent> {
    let all_entries = state.instance_registry.all_active_runs().await;
    let mut initial_events: Vec<AgentEvent> = Vec::new();

    for (key, run_ids) in &all_entries {
        // Team-scoped keys are `team:{team_id}:{agent_name}` — emit a synthetic
        // DelegationStarted so a client multiplexing through this stream
        // populates activeAgentNames before the matching AgentBusy arrives.
        //
        // There is no longer any way to create a team, so a key of this shape
        // can only come from a tasklist persisted before the team endpoints
        // were removed. Handling is kept so those runs still replay correctly
        // rather than streaming an AgentBusy with no delegation context; see
        // `TasklistOwner::Team` in ao-protocol for why the owner variant
        // itself still exists.
        if let Some(rest) = key.strip_prefix("team:") {
            if let Some((_team_id, agent_name)) = rest.split_once(':') {
                initial_events.push(AgentEvent {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    run_id: format!("reconnect-{}", agent_name),
                    seq: 0,
                    ts: Utc::now(),
                    agent_id: key.clone(),
                    thread_id: None,
                    payload: AgentEventPayload::DelegationStarted {
                        delegation_id: format!("reconnect-{}", agent_name),
                        target_agent_id: agent_name.to_string(),
                        task_summary: String::new(),
                    },
                });
            }
        }

        for run_id in run_ids {
            if let Some(record) = state.process_supervisor.get_record(run_id) {
                let thread_id = state.instance_registry.thread_for_run(run_id).await;
                initial_events.push(AgentEvent {
                    event_id: uuid::Uuid::new_v4().to_string(),
                    run_id: run_id.clone(),
                    seq: 0,
                    ts: Utc::now(),
                    agent_id: key.clone(),
                    thread_id,
                    payload: AgentEventPayload::AgentBusy {
                        run_id: run_id.clone(),
                        started_at: record.started_at,
                    },
                });
            }
        }
    }

    // Reconfirm any async delegation still running in this server process, so
    // a client reconnecting mid-run re-lights `runningDelegatesByThread`
    // instead of leaving it to the `DelegateStarted` it may have missed while
    // disconnected. Each `McpAgentSession` owns one `BackgroundAgentRegistry`
    // (concurrent spawns of the same agent get separate sessions), so this
    // scans every session rather than one per agent_id. A server restart
    // drops `mcp_sessions` along with everything else — nothing to replay in
    // that case, which is exactly what lets the frontend's reconnect-grace
    // timer clear a delegation that died with the old process instead of
    // spinning forever.
    for session in state.mcp_sessions.all_sessions() {
        for snapshot in session.background_agents.active().await {
            initial_events.push(AgentEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                run_id: format!("reconnect-delegate-{}", snapshot.id),
                seq: 0,
                ts: Utc::now(),
                agent_id: session.agent_id.clone(),
                thread_id: session.thread_id.clone(),
                payload: AgentEventPayload::DelegateStarted {
                    delegate_name: snapshot.subagent_name,
                    delegation_id: snapshot.id.to_string(),
                    spawned_at: snapshot.spawned_at,
                },
            });
        }
    }

    initial_events
}

/// GET /system/stream — Global SSE stream that forwards all events without filtering.
/// The sidebar subscribes to this so it can detect activity on agents it isn't
/// currently tracking (e.g. scheduled tasks firing on an idle agent).
///
/// See `build_system_replay_events` for the connect-time replay this emits
/// before chaining the live broadcast stream.
pub async fn stream_system_events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let initial_events = build_system_replay_events(&state).await;
    let initial_stream = stream::iter(initial_events).map(|event| agent_event_to_sse(&event));

    let rx = state.event_bus.subscribe();
    let broadcast_stream = BroadcastStream::new(rx);

    let event_stream = broadcast_stream.filter_map(|result| async move {
        match result {
            Ok(event) => Some(agent_event_to_sse(&event)),
            _ => None,
        }
    });

    let combined = initial_stream.chain(event_stream);

    Sse::new(combined).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(payload: AgentEventPayload) -> AgentEvent {
        AgentEvent {
            event_id: "evt-1".into(),
            run_id: "run-1".into(),
            seq: 0,
            ts: Utc::now(),
            agent_id: "agent-1".into(),
            thread_id: None,
            payload,
        }
    }

    #[test]
    fn agent_action_started_maps_to_agent_action_started_name() {
        let payload = AgentEventPayload::AgentActionStarted {
            action_id: "action-1".into(),
            kind: "memory_save".into(),
            summary: "Saving memory…".into(),
        };
        assert_eq!(event_type_name(&payload), "agent_action_started");
    }

    #[test]
    fn agent_action_completed_maps_to_agent_action_completed_name() {
        let payload = AgentEventPayload::AgentActionCompleted {
            action_id: "action-1".into(),
        };
        assert_eq!(event_type_name(&payload), "agent_action_completed");
    }

    /// Verifies the full SSE emission path: event_type_name is used by
    /// agent_event_to_sse, so a successful conversion plus correct name lookup
    /// confirms the SSE stream labels these events correctly.
    #[test]
    fn agent_action_started_sse_event_round_trips() {
        let event = make_event(AgentEventPayload::AgentActionStarted {
            action_id: "action-1".into(),
            kind: "memory_save".into(),
            summary: "Saving memory…".into(),
        });
        assert_eq!(event_type_name(&event.payload), "agent_action_started");
        let _ = agent_event_to_sse(&event).expect("sse conversion is infallible");
        let data = serde_json::to_string(&event).unwrap();
        assert!(data.contains("\"type\":\"AgentActionStarted\""));
        assert!(data.contains("\"action_id\":\"action-1\""));
    }

    #[test]
    fn agent_action_completed_sse_event_round_trips() {
        let event = make_event(AgentEventPayload::AgentActionCompleted {
            action_id: "action-1".into(),
        });
        assert_eq!(event_type_name(&event.payload), "agent_action_completed");
        let _ = agent_event_to_sse(&event).expect("sse conversion is infallible");
        let data = serde_json::to_string(&event).unwrap();
        assert!(data.contains("\"type\":\"AgentActionCompleted\""));
        assert!(data.contains("\"action_id\":\"action-1\""));
    }

    // ───────────────────────────────────────────────────────────────────
    // Tool-event chip-parity pins
    //
    // Two parallel runner paths emit chip-shaped events under different
    // payload variants:
    //
    //   API runner (NativeAgentRunner → TimelineAdapter):
    //     SessionEvent::ToolUse / ToolResult
    //       → AgentEventPayload::ToolCallStarted / ToolCallCompleted
    //       → SSE name "tool_call_started" / "tool_call_completed"
    //
    //   CLI runner (CliAgentRunner → XML transport parser):
    //     parser recognises <tool_use> open/close
    //       → AgentEventPayload::ToolUseStarted / ToolUseCompleted
    //       → SSE name "tool_use_started" / "tool_use_completed"
    //
    // The frontend (`useSSE.ts`) subscribes to both name pairs and routes
    // them through `addInFlightToolCall` vs `addInFlightToolUse` so chips
    // render uniformly regardless of which runner emitted them. If anyone
    // ever drifts the payload variant ↔ SSE name mapping, half of agents
    // silently lose their tool-use chips. These tests catch that at the
    // serialization seam before it ships.
    // ───────────────────────────────────────────────────────────────────

    #[test]
    fn tool_call_started_maps_to_tool_call_started_name() {
        let payload = AgentEventPayload::ToolCallStarted {
            tool_name: "Read".into(),
            tool_input: Some(serde_json::json!({ "path": "/tmp/x" })),
            label: None,
            tool_use_id: None,
        };
        assert_eq!(event_type_name(&payload), "tool_call_started");
    }

    #[test]
    fn tool_call_completed_maps_to_tool_call_completed_name() {
        let payload = AgentEventPayload::ToolCallCompleted {
            tool_name: "Read".into(),
            output: Some("contents".into()),
            tool_use_id: None,
            is_error: false,
        };
        assert_eq!(event_type_name(&payload), "tool_call_completed");
    }

    #[test]
    fn tool_use_started_maps_to_tool_use_started_name() {
        let payload = AgentEventPayload::ToolUseStarted {
            tool_use_id: "tu-1".into(),
            tool_name: "DateTime".into(),
            agent_id: "agent-1".into(),
            run_id: "run-1".into(),
            timestamp: Utc::now(),
        };
        assert_eq!(event_type_name(&payload), "tool_use_started");
    }

    #[test]
    fn tool_use_completed_maps_to_tool_use_completed_name() {
        let payload = AgentEventPayload::ToolUseCompleted {
            tool_use_id: "tu-1".into(),
            tool_name: "DateTime".into(),
            input: serde_json::json!({}),
            agent_id: "agent-1".into(),
            run_id: "run-1".into(),
            timestamp: Utc::now(),
        };
        assert_eq!(event_type_name(&payload), "tool_use_completed");
    }

    /// Full round-trip for `ToolCallStarted`: name lookup + payload JSON
    /// shape match what `useSSE.ts::addEventListener("tool_call_started")`
    /// parses out of `event.payload.data`.
    #[test]
    fn tool_call_started_sse_event_round_trips() {
        let event = make_event(AgentEventPayload::ToolCallStarted {
            tool_name: "Grep".into(),
            tool_input: Some(serde_json::json!({ "pattern": "x" })),
            label: None,
            tool_use_id: None,
        });
        assert_eq!(event_type_name(&event.payload), "tool_call_started");
        let _ = agent_event_to_sse(&event).expect("sse conversion is infallible");
        let data = serde_json::to_string(&event).unwrap();
        assert!(data.contains("\"type\":\"ToolCallStarted\""));
        assert!(data.contains("\"tool_name\":\"Grep\""));
    }

    /// Asserts that a native `RunSkill` `ToolCallStarted` with a resolved label
    /// survives serialize → SSE conversion with `label` present in the JSON
    /// data, so `useSSE.ts` can read it and skip frontend label derivation.
    #[test]
    fn run_skill_label_reaches_sse_data() {
        let event = make_event(AgentEventPayload::ToolCallStarted {
            tool_name: "RunSkill".into(),
            tool_input: Some(serde_json::json!({ "skill": "verify-studio" })),
            label: Some("Loading skill: verify-studio".into()),
            tool_use_id: None,
        });
        assert_eq!(event_type_name(&event.payload), "tool_call_started");
        let _ = agent_event_to_sse(&event).expect("sse conversion is infallible");
        let data = serde_json::to_string(&event).unwrap();
        assert!(data.contains("\"type\":\"ToolCallStarted\""));
        assert!(data.contains("\"tool_name\":\"RunSkill\""));
        assert!(data.contains("\"label\":\"Loading skill: verify-studio\""));
    }

    /// Full round-trip for `ToolUseStarted`: confirms the XML-path payload
    /// carries `tool_use_id` + `tool_name` in the shape `useSSE.ts` reads.
    #[test]
    fn tool_use_started_sse_event_round_trips() {
        let event = make_event(AgentEventPayload::ToolUseStarted {
            tool_use_id: "tu-42".into(),
            tool_name: "WorkflowActionCreate".into(),
            agent_id: "agent-1".into(),
            run_id: "run-1".into(),
            timestamp: Utc::now(),
        });
        assert_eq!(event_type_name(&event.payload), "tool_use_started");
        let _ = agent_event_to_sse(&event).expect("sse conversion is infallible");
        let data = serde_json::to_string(&event).unwrap();
        assert!(data.contains("\"type\":\"ToolUseStarted\""));
        assert!(data.contains("\"tool_use_id\":\"tu-42\""));
        assert!(data.contains("\"tool_name\":\"WorkflowActionCreate\""));
    }

    // ───────────────────────────────────────────────────────────────────
    // /system/stream replay superset (`build_system_replay_events`)
    //
    // `/system/stream` must replay the same connect-time AgentBusy (and,
    // for team channels, DelegationStarted) events that each per-entity
    // endpoint synthesizes on its own connect — otherwise a client
    // multiplexing every channel through this one stream would regress the
    // reconnect-grace / thread-tagged-typing behavior those endpoints
    // provide today. See `build_system_replay_events`.
    // ───────────────────────────────────────────────────────────────────

    use ao_process::mock::MockProcessSupervisor;
    use ao_process::supervisor::SpawnInput;

    async fn setup_state_with_mock() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = ao_persistence::PersistenceLayer::init();
            // Enough empty scenarios for every `register_active_run` spawn a
            // test in this module performs — each spawn consumes exactly one.
            let scenario = ao_process::mock::MockScenario {
                stdout_lines: vec![],
                stderr_lines: vec![],
                exit_code: 0,
                delay_per_line_ms: 0,
            };
            let mock = MockProcessSupervisor::new(vec![scenario; 16]);
            AppState::new_with_mock(mock)
                .await
                .expect("AppState init")
        };
        (Arc::new(state), tmp)
    }

    /// Registers `run_id` as active under `key` in both the instance
    /// registry (so `all_active_runs` surfaces it) and the mock process
    /// supervisor (so `get_record` resolves a `started_at`, matching what a
    /// real run looks like) — mirrors the two systems every per-entity
    /// endpoint's own replay logic already reads from.
    async fn register_active_run(state: &AppState, key: &str, run_id: &str) {
        state.instance_registry.register_run(&key.to_string(), run_id).await;
        state
            .process_supervisor
            .spawn(SpawnInput {
                run_id: Some(run_id.to_string()),
                backend_id: "mock".to_string(),
                scope_key: None,
                argv: vec![],
                cwd: None,
                env: None,
                stdin_data: None,
                timeout_ms: None,
                no_output_timeout_ms: None,
                tools_in_flight: None,
                form_suspended: None,
            })
            .await
            .expect("mock spawn should not fail");
    }

    /// A mid-run `/system/stream` connect must replay `AgentBusy` for active
    /// runs under every channel-key shape — plain agent ids and the
    /// `team:`/`project:`/`task:…:phase:`/`tasklist:` synthetic keys alike —
    /// and additionally replay `DelegationStarted` for team-scoped keys, so
    /// the stream is a true superset of every per-entity endpoint's replay.
    #[tokio::test]
    async fn system_stream_replays_agent_busy_for_every_channel_shape() {
        let (state, _tmp) = setup_state_with_mock().await;

        register_active_run(&state, "agent-1", "run-agent").await;
        register_active_run(&state, "team:t1:agent-2", "run-team").await;
        register_active_run(&state, "project:p1", "run-project").await;
        register_active_run(&state, "task:tk1:phase:0", "run-task").await;
        register_active_run(&state, "tasklist:tl1", "run-tasklist").await;

        let events = build_system_replay_events(&state).await;

        let busy_agent_ids: std::collections::HashSet<String> = events
            .iter()
            .filter_map(|e| match &e.payload {
                AgentEventPayload::AgentBusy { .. } => Some(e.agent_id.clone()),
                _ => None,
            })
            .collect();

        assert!(busy_agent_ids.contains("agent-1"), "plain agent id must replay AgentBusy");
        assert!(
            busy_agent_ids.contains("team:t1:agent-2"),
            "team-scoped key must replay AgentBusy"
        );
        assert!(
            busy_agent_ids.contains("project:p1"),
            "project-scoped key must replay AgentBusy"
        );
        assert!(
            busy_agent_ids.contains("task:tk1:phase:0"),
            "task-phase key must replay AgentBusy"
        );
        assert!(
            busy_agent_ids.contains("tasklist:tl1"),
            "tasklist-scoped key must replay AgentBusy"
        );

        // Team channel additionally gets a DelegationStarted — and it must
        // precede the matching AgentBusy so a client populates
        // activeAgentNames before the busy event arrives.
        let team_delegation_idx = events.iter().position(|e| {
            matches!(&e.payload, AgentEventPayload::DelegationStarted { target_agent_id, .. } if target_agent_id == "agent-2")
                && e.agent_id == "team:t1:agent-2"
        });
        let team_busy_idx = events.iter().position(|e| {
            matches!(&e.payload, AgentEventPayload::AgentBusy { run_id, .. } if run_id == "run-team")
        });
        assert!(team_delegation_idx.is_some(), "team key must replay DelegationStarted");
        assert!(team_busy_idx.is_some());
        assert!(
            team_delegation_idx.unwrap() < team_busy_idx.unwrap(),
            "DelegationStarted must precede AgentBusy for the same team channel"
        );

        // Non-team keys must NOT get a synthetic DelegationStarted.
        let non_team_delegations = events
            .iter()
            .filter(|e| matches!(&e.payload, AgentEventPayload::DelegationStarted { .. }))
            .count();
        assert_eq!(non_team_delegations, 1, "only the team-scoped key should replay DelegationStarted");
    }

    /// `all_active_runs` enumerates every channel key, and every enumerated
    /// key with a resolvable process-supervisor record round-trips into a
    /// replay event — no key is silently dropped by the replay builder.
    #[tokio::test]
    async fn system_stream_replay_covers_every_registry_key() {
        let (state, _tmp) = setup_state_with_mock().await;

        register_active_run(&state, "agent-solo", "run-1").await;
        register_active_run(&state, "agent-solo", "run-2").await;
        register_active_run(&state, "project:p9", "run-3").await;

        let all = state.instance_registry.all_active_runs().await;
        assert_eq!(all.len(), 2, "two distinct registry keys should be tracked");

        let events = build_system_replay_events(&state).await;
        let busy_run_ids: std::collections::HashSet<String> = events
            .iter()
            .filter_map(|e| match &e.payload {
                AgentEventPayload::AgentBusy { run_id, .. } => Some(run_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(busy_run_ids.len(), 3, "every active run_id across every key must replay");
        assert!(busy_run_ids.contains("run-1"));
        assert!(busy_run_ids.contains("run-2"));
        assert!(busy_run_ids.contains("run-3"));
    }

    // ───────────────────────────────────────────────────────────────────
    // Async-delegate reconnect replay (`DelegateStarted`)
    //
    // A client that reconnects mid-run must reconfirm any async delegation
    // still live in this server process, so the frontend's reconnect-grace
    // timer cancels the clear instead of treating a mere network blip as if
    // the delegate died. See `ao_engine_tools_core::background_agents`.
    // ───────────────────────────────────────────────────────────────────

    use chrono::DateTime;

    use ao_engine_tools_core::background_agents::handle::{
        BackgroundAgentHandle, BackgroundAgentId, TaskFinalReport,
    };

    fn make_test_handle(name: &str, spawned_at: DateTime<Utc>) -> BackgroundAgentHandle {
        let (_tx, rx) = tokio::sync::broadcast::channel(1);
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_clone = cancel.clone();
        let join = tokio::spawn(async move {
            cancel_clone.cancelled().await;
            Ok::<TaskFinalReport, ao_protocol::error::AoError>(TaskFinalReport::cancelled())
        });
        BackgroundAgentHandle {
            id: BackgroundAgentId::new(),
            subagent_name: name.to_string(),
            spawned_at,
            cancel,
            events: rx,
            join,
        }
    }

    #[tokio::test]
    async fn system_stream_replays_delegate_started_for_live_background_delegations() {
        let (state, _tmp) = setup_state_with_mock().await;

        let session = state
            .mcp_sessions
            .register_session_with_chains(
                "sess-delegate-replay".to_string(),
                "agent-delegate-owner".to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
                vec![],
                vec![],
                None,
                Some("thread-delegate-1".to_string()),
            )
            .expect("session registration should succeed");

        // Fixed, well-in-the-past spawn time so the assertion below can tell
        // "the snapshot's real spawned_at" apart from "whatever Utc::now()
        // happened to be when the replay ran" — the whole point of this test.
        let real_spawned_at = Utc::now() - chrono::Duration::minutes(5);
        let handle = make_test_handle("Researcher", real_spawned_at);
        let delegation_id = handle.id.clone();
        session.background_agents.insert(handle).await.unwrap();

        let events = build_system_replay_events(&state).await;

        let started = events
            .iter()
            .find(|e| matches!(&e.payload, AgentEventPayload::DelegateStarted { .. }))
            .expect("must replay a DelegateStarted event for the live handle");

        assert_eq!(started.agent_id, "agent-delegate-owner");
        assert_eq!(started.thread_id.as_deref(), Some("thread-delegate-1"));
        match &started.payload {
            AgentEventPayload::DelegateStarted {
                delegate_name,
                delegation_id: id,
                spawned_at,
            } => {
                assert_eq!(delegate_name, "Researcher");
                assert_eq!(id, &delegation_id.to_string());
                assert_eq!(
                    *spawned_at, real_spawned_at,
                    "replay must carry the snapshot's real spawned_at through unchanged"
                );
                assert_ne!(
                    *spawned_at,
                    started.ts,
                    "spawned_at must be the delegate's real spawn time, not the replay event's \
                     own Utc::now() envelope timestamp"
                );
            }
            other => panic!("expected DelegateStarted, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn system_stream_replays_nothing_when_no_session_has_live_delegations() {
        let (state, _tmp) = setup_state_with_mock().await;

        state
            .mcp_sessions
            .register_session(
                "sess-no-delegates".to_string(),
                "agent-idle".to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
            )
            .expect("session registration should succeed");

        let events = build_system_replay_events(&state).await;

        assert!(
            !events.iter().any(|e| matches!(&e.payload, AgentEventPayload::DelegateStarted { .. })),
            "a session with no live background delegates must not replay DelegateStarted"
        );
    }

    /// Simulates the exact "zombie spinner" scenario the replay guards
    /// against: after a server restart, `mcp_sessions` is a fresh, empty
    /// store (the old process's sessions are gone) — so a delegation that
    /// died with the old process must NOT be replayed, letting the
    /// frontend's reconnect-grace timer clear it instead of spinning forever.
    #[tokio::test]
    async fn system_stream_replays_nothing_after_a_simulated_restart() {
        let (state, _tmp) = setup_state_with_mock().await;

        // A brand new AppState (as a restart would produce) has an empty
        // mcp_sessions store regardless of what the "old" process had live.
        assert!(state.mcp_sessions.all_sessions().is_empty());

        let events = build_system_replay_events(&state).await;
        assert!(!events.iter().any(|e| matches!(&e.payload, AgentEventPayload::DelegateStarted { .. })));
    }
}

/// GET /agents/{agent_id}/tasklists/{tasklist_id}/stream — SSE endpoint for
/// per-task subagent run events isolated from the parent's main chat channel.
///
/// Agent-owned tasklist runs emit all events with
/// `agent_id = "tasklist:{tasklist_id}"` rather than the parent agent ID.
/// This endpoint is the corresponding subscription point for that channel,
/// consumed by the TodoPanel to show live per-task activity.
pub async fn stream_agent_tasklist_events(
    State(state): State<Arc<AppState>>,
    Path((_agent_id, tasklist_id)): Path<(String, String)>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let channel = format!("tasklist:{}", tasklist_id);

    let rx = state.event_bus.subscribe();
    let broadcast_stream = BroadcastStream::new(rx);

    let event_stream = broadcast_stream.filter_map(move |result| {
        let ch = channel.clone();
        async move {
            match result {
                Ok(event) if event.agent_id == ch => Some(agent_event_to_sse(&event)),
                _ => None,
            }
        }
    });

    Sse::new(event_stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// GET /agents/{agent_id}/stream — SSE streaming endpoint for real-time agent events.
pub async fn stream_events(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let target_agent_id = agent_id.clone();

    // Check for active runs to emit initial AgentBusy event(s)
    let active_run_ids = state.instance_registry.active_runs(&agent_id).await;
    let mut initial_events: Vec<AgentEvent> = Vec::new();

    for run_id in &active_run_ids {
        if let Some(record) = state.process_supervisor.get_record(run_id) {
            // Which thread this run belongs to, so a client that's currently
            // looking at (or reconnecting into) a *different* thread of this
            // agent doesn't mistake this replayed AgentBusy for its own
            // thread being active — see `InstanceRegistry::thread_for_run`.
            // `None` here means "this run's thread is unknown or is the
            // agent's default thread", same as any other event.
            let thread_id = state.instance_registry.thread_for_run(run_id).await;
            initial_events.push(AgentEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                run_id: run_id.clone(),
                seq: 0,
                ts: Utc::now(),
                agent_id: agent_id.clone(),
                thread_id,
                payload: AgentEventPayload::AgentBusy {
                    run_id: run_id.clone(),
                    started_at: record.started_at,
                },
            });
        }
    }

    // Create initial stream from AgentBusy events
    let initial_stream = stream::iter(initial_events).map(|event| agent_event_to_sse(&event));

    // Subscribe to EventBus and filter for this agent
    let rx = state.event_bus.subscribe();
    let broadcast_stream = BroadcastStream::new(rx);

    let event_stream = broadcast_stream.filter_map(move |result| {
        let agent_id = target_agent_id.clone();
        async move {
            match result {
                Ok(event) if event.agent_id == agent_id => Some(agent_event_to_sse(&event)),
                _ => None,
            }
        }
    });

    // Chain initial events with the broadcast stream
    let combined = initial_stream.chain(event_stream);

    Sse::new(combined).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
