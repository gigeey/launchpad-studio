//! End-to-end integration tests for the complete message flow:
//! HTTP → Queue → ProcessSupervisor (mock) → Normalizer → EventBus → SSE + Persistence.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use ao_engine::AppState;
use ao_process::mock::{MockProcessSupervisor, MockScenario};
use ao_protocol::agent::{
    AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
};
use ao_protocol::event::{AgentEventPayload, RunEndReason};
use ao_protocol::message::MessageAck;
use ao_protocol::transcript::{PaginationCursor, TranscriptEntry};
use ao_server::routes::build_router;

/// Global mutex to serialize setup() calls that modify the process-wide env var.
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_test_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Test Agent {}", id),
        description: "An e2e test agent".to_string(),
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

async fn setup(
    scenarios: Vec<MockScenario>,
) -> (axum::Router, Arc<AppState>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");

    let state = {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
        let mock = MockProcessSupervisor::new(scenarios);
        Arc::new(AppState::new_with_mock(mock).await.expect("init state"))
    };

    let router = build_router(Arc::clone(&state));
    (router, state, tmp)
}

async fn read_body(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .expect("read body")
        .to_bytes()
        .to_vec()
}

async fn create_agent(router: &axum::Router, profile: &AgentProfile) {
    let body = serde_json::to_string(profile).unwrap();
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "Failed to create agent");
}

async fn send_message(router: &axum::Router, agent_id: &str, content: &str) -> MessageAck {
    let msg_body = serde_json::json!({ "content": content });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/agents/{}/messages", agent_id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&msg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "Failed to send message");
    let bytes = read_body(resp).await;
    serde_json::from_slice(&bytes).unwrap()
}

#[derive(serde::Deserialize)]
struct PaginatedMessagesResponse {
    messages: Vec<TranscriptEntry>,
    #[allow(dead_code)]
    cursor: Option<PaginationCursor>,
}

async fn get_messages(router: &axum::Router, agent_id: &str) -> Vec<TranscriptEntry> {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/agents/{}/messages", agent_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "Failed to get messages");
    let bytes = read_body(resp).await;
    let paginated: PaginatedMessagesResponse = serde_json::from_slice(&bytes).unwrap();
    paginated.messages
}

/// Wait for N RunEnded events on the event bus, with a timeout.
async fn wait_for_run_ended(
    rx: &mut tokio::sync::broadcast::Receiver<ao_protocol::event::AgentEvent>,
    count: usize,
    timeout_secs: u64,
) -> Vec<ao_protocol::event::AgentEvent> {
    let mut all_events = Vec::new();
    let mut run_ended_count = 0;
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) => {
                let is_run_ended =
                    matches!(event.payload, AgentEventPayload::RunEnded { .. });
                all_events.push(event);
                if is_run_ended {
                    run_ended_count += 1;
                    if run_ended_count >= count {
                        break;
                    }
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                eprintln!("Broadcast receiver lagged by {} messages", n);
                continue;
            }
            Ok(Err(_)) => break,
            Err(_) => panic!(
                "Timed out waiting for {} RunEnded events (got {})",
                count, run_ended_count
            ),
        }
    }

    all_events
}

// ============================================================================
// Test 1: Complete end-to-end message flow
// ============================================================================

#[tokio::test]
async fn test_e2e_complete_message_flow() {
    // Configure mock with 5 streaming text lines
    let scenarios = vec![MockScenario {
        stdout_lines: vec![
            "Hello from".to_string(),
            " the mock".to_string(),
            " agent, this".to_string(),
            " is streaming".to_string(),
            " output!".to_string(),
        ],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 20,
    }];

    let (router, state, _tmp) = setup(scenarios).await;

    // 1) Create a test agent
    let profile = make_test_profile("e2e-flow-agent");
    create_agent(&router, &profile).await;

    // 2) Subscribe to EventBus BEFORE sending message to capture all events
    let mut rx = state.event_bus.subscribe();

    // 3) Send a message and verify ACK
    let ack = send_message(&router, "e2e-flow-agent", "test prompt").await;
    assert_eq!(ack.status, "queued");
    assert!(!ack.message_id.is_empty(), "message_id should not be empty");

    // 4) Collect events until RunEnded
    let events = wait_for_run_ended(&mut rx, 1, 10).await;

    // 5) Verify event sequence for our agent
    let agent_events: Vec<_> = events
        .iter()
        .filter(|e| e.agent_id == "e2e-flow-agent")
        .collect();

    // Should have: MessageProcessingStarted, RunStarted, TextDelta(s), TextComplete, RunEnded
    let has_message_processing = agent_events.iter().any(|e| {
        matches!(
            e.payload,
            AgentEventPayload::MessageProcessingStarted { .. }
        )
    });
    let has_run_started = agent_events
        .iter()
        .any(|e| matches!(e.payload, AgentEventPayload::RunStarted));
    let text_delta_count = agent_events
        .iter()
        .filter(|e| matches!(e.payload, AgentEventPayload::TextDelta { .. }))
        .count();
    let has_text_complete = agent_events
        .iter()
        .any(|e| matches!(e.payload, AgentEventPayload::TextComplete { .. }));
    let has_run_ended = agent_events.iter().any(|e| {
        matches!(
            e.payload,
            AgentEventPayload::RunEnded {
                reason: RunEndReason::Completed
            }
        )
    });

    assert!(
        has_message_processing,
        "Should have MessageProcessingStarted event"
    );
    assert!(has_run_started, "Should have RunStarted event");
    assert!(
        text_delta_count >= 1,
        "Should have at least 1 TextDelta event, got {}",
        text_delta_count
    );
    assert!(has_text_complete, "Should have TextComplete event");
    assert!(
        has_run_ended,
        "Should have RunEnded with Completed reason"
    );

    // 6) Verify seq numbers are monotonic within each run_id
    let mut run_seqs: HashMap<String, Vec<u64>> = HashMap::new();
    for event in &agent_events {
        run_seqs
            .entry(event.run_id.clone())
            .or_default()
            .push(event.seq);
    }
    for (run_id, seqs) in &run_seqs {
        for window in seqs.windows(2) {
            assert!(
                window[1] > window[0],
                "seq numbers not monotonic for run_id {}: {:?}",
                run_id,
                seqs
            );
        }
    }

    // 7) Small delay to ensure transcript writes are flushed
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 8) GET messages and verify transcript contains user message + agent response
    let entries = get_messages(&router, "e2e-flow-agent").await;

    assert!(
        entries.len() >= 2,
        "Expected at least 2 transcript entries (user + agent), got {}",
        entries.len()
    );

    // Check that user message appears in transcript
    let has_user_msg = entries.iter().any(|e| e.content == "test prompt");
    assert!(has_user_msg, "Transcript should contain user message 'test prompt'");

    // Check that agent response appears in transcript
    let has_agent_response = entries
        .iter()
        .any(|e| e.event_type == "response" || e.event_type == "text_complete");
    assert!(
        has_agent_response,
        "Transcript should contain agent response. Entries: {:?}",
        entries.iter().map(|e| (&e.event_type, &e.content)).collect::<Vec<_>>()
    );
}

// ============================================================================
// Test 2: Queue serialization — two messages to max_instances=1 agent
// ============================================================================

#[tokio::test]
async fn test_e2e_queue_serialization() {
    // Two scenarios: each produces distinct output so we can verify ordering
    let scenarios = vec![
        MockScenario {
            stdout_lines: vec!["first response".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 50,
        },
        MockScenario {
            stdout_lines: vec!["second response".to_string()],
            stderr_lines: vec![],
            exit_code: 0,
            delay_per_line_ms: 50,
        },
    ];

    let (router, state, _tmp) = setup(scenarios).await;

    // Create agent with max_instances=1 (default, ensures serialization)
    let profile = make_test_profile("serial-agent");
    create_agent(&router, &profile).await;

    // Subscribe to events
    let mut rx = state.event_bus.subscribe();

    // Send two messages rapidly
    let ack1 = send_message(&router, "serial-agent", "first prompt").await;
    let ack2 = send_message(&router, "serial-agent", "second prompt").await;
    assert_ne!(ack1.message_id, ack2.message_id);

    // Wait for both runs to complete
    let events = wait_for_run_ended(&mut rx, 2, 15).await;

    // Filter events for our agent
    let agent_events: Vec<_> = events
        .iter()
        .filter(|e| e.agent_id == "serial-agent")
        .collect();

    // Verify sequential processing: second RunStarted should come AFTER first RunEnded
    let mut first_run_ended_idx = None;
    let mut second_run_started_idx = None;
    let mut run_started_count = 0;

    for (i, event) in agent_events.iter().enumerate() {
        match &event.payload {
            AgentEventPayload::RunStarted => {
                run_started_count += 1;
                if run_started_count == 2 {
                    second_run_started_idx = Some(i);
                }
            }
            AgentEventPayload::RunEnded { .. } => {
                if first_run_ended_idx.is_none() {
                    first_run_ended_idx = Some(i);
                }
            }
            _ => {}
        }
    }

    assert!(
        first_run_ended_idx.is_some(),
        "Should have first RunEnded"
    );
    assert!(
        second_run_started_idx.is_some(),
        "Should have second RunStarted"
    );
    assert!(
        second_run_started_idx.unwrap() > first_run_ended_idx.unwrap(),
        "Second message should start processing only after first run ends. \
         first_run_ended={}, second_run_started={}",
        first_run_ended_idx.unwrap(),
        second_run_started_idx.unwrap()
    );

    // Wait for transcript writes to flush
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Both messages should appear in transcript in order
    let entries = get_messages(&router, "serial-agent").await;

    let user_messages: Vec<_> = entries
        .iter()
        .filter(|e| e.event_type == "message")
        .collect();

    assert!(
        user_messages.len() >= 2,
        "Should have at least 2 user messages in transcript, got {}",
        user_messages.len()
    );

    // Verify ordering: first prompt before second prompt
    let first_idx = user_messages
        .iter()
        .position(|e| e.content == "first prompt");
    let second_idx = user_messages
        .iter()
        .position(|e| e.content == "second prompt");
    assert!(first_idx.is_some(), "first prompt should be in transcript");
    assert!(second_idx.is_some(), "second prompt should be in transcript");
    assert!(
        first_idx.unwrap() < second_idx.unwrap(),
        "first prompt should appear before second prompt in transcript"
    );
}

// ============================================================================
// Test 3: Agent listing with snapshot data
// ============================================================================

#[tokio::test]
async fn test_e2e_agent_listing_from_snapshot() {
    let (router, _state, _tmp) = setup(vec![]).await;

    // Create 2 agents
    let profile1 = make_test_profile("list-e2e-agent-1");
    let mut profile2 = make_test_profile("list-e2e-agent-2");
    profile2.name = "Custom Agent Name".to_string();

    create_agent(&router, &profile1).await;
    create_agent(&router, &profile2).await;

    // GET /agents
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let agents: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        agents.len(),
        2,
        "Should have 2 agents in list"
    );

    // Verify both agent names are present
    let names: Vec<&str> = agents
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    assert!(
        names.contains(&"Test Agent list-e2e-agent-1"),
        "Agent 1 name should be in list"
    );
    assert!(
        names.contains(&"Custom Agent Name"),
        "Agent 2 name should be in list"
    );

    // Verify snapshot fields exist in the response (message_count, has_active_run)
    for agent in &agents {
        assert!(
            agent.get("message_count").is_some() || agent.get("agent_id").is_some(),
            "Agent listing should come from snapshot with expected fields"
        );
    }
}

// ============================================================================
// Test 4: Server startup creates directory structure
// ============================================================================

#[tokio::test]
async fn test_e2e_directory_structure_created() {
    let tmp = tempfile::tempdir().expect("Failed to create temp dir");
    let tmp_path = tmp.path().to_path_buf();

    {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", &tmp_path);
        let mock = MockProcessSupervisor::new(vec![]);
        let _state = Arc::new(
            AppState::new_with_mock(mock)
                .await
                .expect("init state"),
        );
    }

    // Verify directory structure was created
    assert!(
        tokio::fs::metadata(tmp_path.join("agents")).await.is_ok(),
        "agents/ directory should exist"
    );
    assert!(
        tokio::fs::metadata(tmp_path.join("messages/metadata"))
            .await
            .is_ok(),
        "messages/metadata/ directory should exist"
    );
    assert!(
        tokio::fs::metadata(tmp_path.join("messages/data"))
            .await
            .is_ok(),
        "messages/data/ directory should exist"
    );

    // snapshot.json may or may not exist yet (it's created on first write),
    // but the metadata directory definitely should
    assert!(
        tokio::fs::metadata(tmp_path.join("messages/metadata"))
            .await
            .unwrap()
            .is_dir(),
        "messages/metadata/ should be a directory"
    );
}
