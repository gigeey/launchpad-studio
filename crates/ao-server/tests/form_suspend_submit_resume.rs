//! Integration test: suspend→submit→resume across the LiveFormBridge seam.
//!
//! Seams covered by the single test below:
//! - `LiveFormBridge::ask_form()` suspends on a oneshot channel and emits
//!   `UserEvent::FormRequest` through the event bus.
//! - `EventBusAgentSink` converts that to `AgentEventPayload::FormRequest`,
//!   carrying the live `form_id` the route needs.
//! - `POST /agents/{id}/form-answer` (HTTP route) looks the bridge up in
//!   `form_bridge_registry` and calls `deliver_form_answer()`.
//! - The suspended `ask_form()` future wakes and returns the `FormResponse`
//!   with the submitted answers.
//! - `POST /agents/{id}/async-forms/{form_id}/answer` appends a `form_answer`
//!   transcript entry and clears the pending form from the snapshot
//!   (durable-transcript async path).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt;

use ao_engine::event_bus::EventBusAgentSink;
use ao_engine::{AppState, LiveFormBridge};
use ao_engine_tools_core::{
    EventSink, FormAnswer, FormBridge, FormField, FormFieldKind, FormRequest, FormResponse,
};
use ao_process::mock::MockProcessSupervisor;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
use ao_protocol::event::AgentEventPayload;
use ao_server::routes::build_router;

static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_test_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Form Test Agent {id}"),
        description: "Integration test agent for form suspend/submit/resume".to_string(),
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
        enabled_plugins: HashMap::new(),
        runner_mode: Default::default(),
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

async fn setup() -> (axum::Router, Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tmp dir");
    let state = {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(vec![]);
        Arc::new(AppState::new_with_mock(mock).await.expect("init AppState"))
    };
    let router = build_router(Arc::clone(&state));
    (router, state, tmp)
}

/// Full suspend→submit→resume path plus async-form durable-transcript path.
#[tokio::test]
async fn form_suspend_submit_resume_delivers_answer_through_live_bridge() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "agent-form-e2e";

    // Agent profile needed by the async-forms route (agent lookup).
    state
        .persistence
        .agents
        .create(&make_test_profile(agent_id))
        .await
        .expect("create agent");

    // Subscribe to the event bus BEFORE wiring the bridge so no FormRequest
    // events are missed between bridge registration and the await below.
    let mut rx = state.event_bus.subscribe();

    // Build a LiveFormBridge backed by the real event bus.
    let sink: Arc<dyn EventSink + Send + Sync> = Arc::new(EventBusAgentSink {
        bus: Arc::clone(&state.event_bus),
        agent_id: agent_id.to_string(),
        thread_id: None,
    });
    let bridge = Arc::new(LiveFormBridge::new(sink));
    state
        .form_bridge_registry
        .register(agent_id, Arc::clone(&bridge));

    // Spawn a task that calls ask_form() — mirrors what AskUserQuestionWithForm
    // (sync mode) does inside the runner. The task suspends until
    // deliver_form_answer() is called from outside.
    let bridge_for_task = Arc::clone(&bridge);
    let form_task = tokio::spawn(async move {
        bridge_for_task
            .ask_form(FormRequest {
                id: String::new(),
                agent_id: agent_id.to_string(),
                session_id: "sess-e2e".to_string(),
                title: "Rate your session".to_string(),
                intro: Some("One question".to_string()),
                fields: vec![FormField {
                    id: "rating".to_string(),
                    kind: FormFieldKind::Text {
                        placeholder: Some("1-5".to_string()),
                    },
                    label: "Rating".to_string(),
                    description: None,
                    required: true,
                }],
            })
            .await
    });

    // Wait for the event bus to broadcast FormRequest — this gives us the
    // live form_id generated inside ask_form().
    let live_form_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(event) if event.agent_id == agent_id => {
                    if let AgentEventPayload::FormRequest { form_id, .. } = event.payload {
                        break form_id;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("event bus closed"),
            }
        }
    })
    .await
    .expect("FormRequest event must arrive within 5 s");

    // Bridge must have exactly one pending form while ask_form() suspends.
    assert_eq!(bridge.pending_count(), 1, "one form pending in bridge");

    // Submit the answer via the HTTP route — this is the seam under test.
    let submit_body = serde_json::json!({
        "form_id": &live_form_id,
        "answers": {
            "rating": { "kind": "text", "value": "4" }
        }
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{agent_id}/form-answer"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&submit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "form-answer route must return 200");

    // The suspended ask_form() future must now resolve with the delivered answer.
    let response = tokio::time::timeout(Duration::from_secs(5), form_task)
        .await
        .expect("form_task must resolve within 5 s")
        .expect("task must not panic")
        .expect("ask_form must return Ok");

    assert_eq!(response.form_id, live_form_id, "returned form_id matches submitted form_id");
    match response.answers.get("rating") {
        Some(ao_engine_tools_core::FormAnswer::Text(v)) => {
            assert_eq!(v, "4", "answer text must match submitted value");
        }
        other => panic!("expected FormAnswer::Text, got {other:?}"),
    }

    // No forms remain pending after delivery.
    assert_eq!(bridge.pending_count(), 0, "bridge must have no pending forms after delivery");

    // Deregister the bridge (mirrors the RAII guard in the MCP route handler).
    state
        .form_bridge_registry
        .deregister(agent_id, &bridge);

    // ── Async-form path: durable-transcript seam ──────────────────────────────
    //
    // Verify that POST /async-forms/{form_id}/answer:
    //   1. appends a form_answer transcript entry with correct metadata, and
    //   2. clears the pending form from the agent snapshot.
    //
    // This covers the async-form testing gap.

    let async_form_id = "async-form-e2e-001";
    state
        .persistence
        .snapshots
        .set_pending_form(agent_id, None, async_form_id.to_string(), serde_json::json!({}))
        .await
        .expect("set pending form");

    let async_req = serde_json::json!({
        "values": { "comment": "great session" }
    });
    let async_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{agent_id}/async-forms/{async_form_id}/answer"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&async_req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        async_resp.status(),
        StatusCode::OK,
        "async-forms answer route must return 200"
    );

    // Transcript must contain the form_answer entry.
    let entries = state
        .persistence
        .transcripts
        .read_all(agent_id)
        .await
        .expect("read transcript");
    let form_answer_entry = entries
        .iter()
        .find(|e| e.event_type == "form_answer")
        .expect("form_answer transcript entry must be written");
    let meta = form_answer_entry
        .metadata
        .as_ref()
        .expect("form_answer entry must have metadata");
    assert_eq!(meta["form_id"], serde_json::json!(async_form_id));
    assert_eq!(meta["values"]["comment"], serde_json::json!("great session"));

    // Snapshot must have the pending form cleared.
    let snapshot = state.persistence.snapshots.get().await;
    let still_pending = snapshot
        .agents
        .get(agent_id)
        .map(|s| !s.pending_forms.is_empty())
        .unwrap_or(false);
    assert!(!still_pending, "pending form must be cleared after async answer");
}

/// Regression test: when two bridges are registered under the same agent (as
/// happens when the model batches tool calls and each MCP request creates its
/// own bridge), deregistering the fast-tool bridge must not cancel the form
/// bridge that is still awaiting an operator answer.
///
/// This test exercises the exact failure mode from the original bug: the old
/// single-entry registry would lose the form bridge when the parallel request
/// clobbered or removed it, causing the operator's submission to 404 and the
/// ask_form future to time out. With the multi-bridge registry, each bridge is
/// tracked independently by pointer identity.
#[tokio::test]
async fn parallel_fast_tool_deregister_does_not_cancel_sibling_form_bridge() {
    let (_, state, _tmp) = setup().await;
    let agent_id = "agent-parallel-bridges";

    let mut rx = state.event_bus.subscribe();

    // Bridge A — the form bridge that will suspend waiting for an answer.
    let sink_a: Arc<dyn EventSink + Send + Sync> = Arc::new(EventBusAgentSink {
        bus: Arc::clone(&state.event_bus),
        agent_id: agent_id.to_string(),
        thread_id: None,
    });
    let bridge_a = Arc::new(LiveFormBridge::new(sink_a));

    // Bridge B — simulates a parallel fast tool call (e.g. ToolSearch) that
    // creates its own bridge under the same agent_id, completes quickly, and
    // deregisters before the form is answered.
    let sink_b: Arc<dyn EventSink + Send + Sync> = Arc::new(EventBusAgentSink {
        bus: Arc::clone(&state.event_bus),
        agent_id: agent_id.to_string(),
        thread_id: None,
    });
    let bridge_b = Arc::new(LiveFormBridge::new(sink_b));

    // Register both under the same agent — mirrors two concurrent MCP requests.
    state
        .form_bridge_registry
        .register(agent_id, Arc::clone(&bridge_a));
    state
        .form_bridge_registry
        .register(agent_id, Arc::clone(&bridge_b));

    // Spawn bridge_a.ask_form() — suspends until answered.
    let bridge_a_for_task = Arc::clone(&bridge_a);
    let form_task = tokio::spawn(async move {
        bridge_a_for_task
            .ask_form(FormRequest {
                id: String::new(),
                agent_id: agent_id.to_string(),
                session_id: "sess-parallel".to_string(),
                title: "Parallel form test".to_string(),
                intro: None,
                fields: vec![FormField {
                    id: "check".to_string(),
                    kind: FormFieldKind::Text {
                        placeholder: Some("answer".to_string()),
                    },
                    label: "Check".to_string(),
                    description: None,
                    required: true,
                }],
            })
            .await
    });

    // Capture the live form_id emitted by bridge_a.
    let form_a_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(event) if event.agent_id == agent_id => {
                    if let ao_protocol::event::AgentEventPayload::FormRequest {
                        form_id, ..
                    } = event.payload
                    {
                        break form_id;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("event bus closed"),
            }
        }
    })
    .await
    .expect("FormRequest from bridge_a must arrive within 5 s");

    assert_eq!(bridge_a.pending_count(), 1, "bridge_a has one pending form");

    // Simulate the fast tool (bridge_b) finishing — deregister and cancel it.
    state
        .form_bridge_registry
        .deregister(agent_id, &bridge_b);
    bridge_b.cancel_pending();

    // bridge_a's pending form must survive bridge_b's deregistration.
    assert_eq!(
        bridge_a.pending_count(),
        1,
        "bridge_a must still have its pending form after bridge_b deregistered"
    );

    // Deliver the answer via the registry — only bridge_a remains, and it owns form_a_id.
    let mut answers = HashMap::new();
    answers.insert("check".to_string(), FormAnswer::Text("pass".to_string()));
    let response = FormResponse {
        form_id: form_a_id.clone(),
        answers,
        ..Default::default()
    };
    state
        .form_bridge_registry
        .deliver(agent_id, &form_a_id, response)
        .expect("deliver must succeed — bridge_a still owns the form");

    let result = tokio::time::timeout(Duration::from_secs(5), form_task)
        .await
        .expect("form_task must resolve within 5 s")
        .expect("task must not panic")
        .expect("ask_form must return Ok");

    assert_eq!(result.form_id, form_a_id, "returned form_id matches submitted");
    match result.answers.get("check") {
        Some(FormAnswer::Text(v)) => assert_eq!(v, "pass", "answer text must match"),
        other => panic!("expected FormAnswer::Text, got {other:?}"),
    }

    // Cleanup.
    state
        .form_bridge_registry
        .deregister(agent_id, &bridge_a);
}

/// The form UI's action row (Cancel / Regenerate / Something else) delivers
/// through the exact same `POST /agents/{id}/form-answer` route and bridge as
/// a normal submission — it's just a different `FormResponse` shape. This
/// confirms an `action` click resolves the suspended `ask_form()` future with
/// `action` set and `answers` empty, end to end through the real HTTP route.
#[tokio::test]
async fn form_action_button_delivers_through_live_bridge() {
    let (router, state, _tmp) = setup().await;
    let agent_id = "agent-form-action-e2e";

    state
        .persistence
        .agents
        .create(&make_test_profile(agent_id))
        .await
        .expect("create agent");

    let mut rx = state.event_bus.subscribe();
    let sink: Arc<dyn EventSink + Send + Sync> = Arc::new(EventBusAgentSink {
        bus: Arc::clone(&state.event_bus),
        agent_id: agent_id.to_string(),
        thread_id: None,
    });
    let bridge = Arc::new(LiveFormBridge::new(sink));
    state
        .form_bridge_registry
        .register(agent_id, Arc::clone(&bridge));

    let bridge_for_task = Arc::clone(&bridge);
    let form_task = tokio::spawn(async move {
        bridge_for_task
            .ask_form(FormRequest {
                id: String::new(),
                agent_id: agent_id.to_string(),
                session_id: "sess-action-e2e".to_string(),
                title: "Pick a deploy target".to_string(),
                intro: None,
                fields: vec![FormField {
                    id: "target".to_string(),
                    kind: FormFieldKind::Text { placeholder: None },
                    label: "Target".to_string(),
                    description: None,
                    required: true,
                }],
            })
            .await
    });

    let live_form_id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(event) if event.agent_id == agent_id => {
                    if let AgentEventPayload::FormRequest { form_id, .. } = event.payload {
                        break form_id;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => panic!("event bus closed"),
            }
        }
    })
    .await
    .expect("FormRequest event must arrive within 5 s");

    // Click "Regenerate" instead of submitting: no answers, an action, and a note.
    let submit_body = serde_json::json!({
        "form_id": &live_form_id,
        "answers": {},
        "action": "regenerate",
        "note": "not what I meant"
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{agent_id}/form-answer"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&submit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "form-answer route must return 200 for an action click");

    let response = tokio::time::timeout(Duration::from_secs(5), form_task)
        .await
        .expect("form_task must resolve within 5 s")
        .expect("task must not panic")
        .expect("ask_form must return Ok");

    assert_eq!(response.form_id, live_form_id);
    assert!(response.answers.is_empty(), "an action response must carry no answers");
    assert_eq!(response.action, Some(ao_engine_tools_core::FormAction::Regenerate));
    assert_eq!(response.note.as_deref(), Some("not what I meant"));

    state.form_bridge_registry.deregister(agent_id, &bridge);
}
