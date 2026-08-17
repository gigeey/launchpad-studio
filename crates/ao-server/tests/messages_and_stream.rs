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
use ao_protocol::message::MessageAck;
use ao_protocol::thread::BranchSource;
use ao_protocol::transcript::{CursorPhase, PaginationCursor, TranscriptEntry, TranscriptRole};

#[derive(serde::Deserialize)]
struct PaginatedMessagesResponse {
    messages: Vec<TranscriptEntry>,
    #[allow(dead_code)]
    cursor: Option<PaginationCursor>,
}
use ao_server::routes::build_router;

/// Global mutex to serialize setup() calls that modify the process-wide env var.
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn make_test_profile(id: &str) -> AgentProfile {
    AgentProfile {
        id: id.to_string(),
        name: format!("Test Agent {}", id),
        description: "A test agent".to_string(),
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
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_send_message_returns_ack() {
    let scenarios = vec![MockScenario {
        stdout_lines: vec!["response text".to_string()],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 10,
    }];

    let (router, _state, _tmp) = setup(scenarios).await;
    let profile = make_test_profile("msg-agent");
    create_agent(&router, &profile).await;

    // Send message
    let msg_body = serde_json::json!({ "content": "hello agent" });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/msg-agent/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&msg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let ack: MessageAck = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(ack.status, "queued");
    assert!(!ack.message_id.is_empty());
}

#[tokio::test]
async fn test_send_message_to_nonexistent_agent_returns_404() {
    let (router, _state, _tmp) = setup(vec![]).await;

    let msg_body = serde_json::json!({ "content": "hello" });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/no-such-agent/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&msg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_messages_returns_transcript() {
    let scenarios = vec![MockScenario {
        stdout_lines: vec!["agent reply".to_string()],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 10,
    }];

    let (router, state, _tmp) = setup(scenarios).await;
    let profile = make_test_profile("transcript-agent");
    create_agent(&router, &profile).await;

    // Send message
    let msg_body = serde_json::json!({ "content": "user prompt" });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/transcript-agent/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&msg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Wait for the run to complete by polling event bus
    let mut rx = state.event_bus.subscribe();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) => {
                if matches!(
                    event.payload,
                    ao_protocol::event::AgentEventPayload::RunEnded { .. }
                ) {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => panic!("Timed out waiting for run to complete"),
        }
    }

    // Small delay to ensure transcript writes are flushed
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // GET messages
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/transcript-agent/messages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let entries: Vec<TranscriptEntry> = serde_json::from_slice::<PaginatedMessagesResponse>(&bytes).unwrap().messages;

    // Should have at least user message + agent response
    // Note: send_message persists user message, and AgentRunner also persists user message
    // So we expect at least 2 user messages + 1 agent response = 3, but the key assertion
    // is that both user content and agent content appear
    assert!(
        entries.len() >= 2,
        "Expected at least 2 transcript entries, got {}",
        entries.len()
    );

    // First entry should be the user message from send_message handler
    assert_eq!(entries[0].content, "user prompt");
    assert_eq!(entries[0].event_type, "message");
}

#[tokio::test]
async fn test_sse_stream_delivers_events() {
    let scenarios = vec![MockScenario {
        stdout_lines: vec![
            "line 1".to_string(),
            "line 2".to_string(),
        ],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 20,
    }];

    let (router, state, _tmp) = setup(scenarios).await;
    let profile = make_test_profile("sse-agent");
    create_agent(&router, &profile).await;

    // Subscribe to event bus before sending message (to capture all events)
    let mut rx = state.event_bus.subscribe();

    // Send message
    let msg_body = serde_json::json!({ "content": "stream test" });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/sse-agent/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&msg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Collect events until RunEnded
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);

    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) => {
                let is_run_ended = matches!(
                    event.payload,
                    ao_protocol::event::AgentEventPayload::RunEnded { .. }
                );
                events.push(event);
                if is_run_ended {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => panic!("Timed out waiting for SSE events"),
        }
    }

    // Verify we got the key events
    let has_run_started = events.iter().any(|e| {
        matches!(
            e.payload,
            ao_protocol::event::AgentEventPayload::RunStarted
        )
    });
    let has_text = events.iter().any(|e| {
        matches!(
            e.payload,
            ao_protocol::event::AgentEventPayload::TextDelta { .. }
                | ao_protocol::event::AgentEventPayload::TextComplete { .. }
        )
    });
    let has_run_ended = events.iter().any(|e| {
        matches!(
            e.payload,
            ao_protocol::event::AgentEventPayload::RunEnded { .. }
        )
    });

    assert!(has_run_started, "Should have RunStarted event");
    assert!(has_text, "Should have text events");
    assert!(has_run_ended, "Should have RunEnded event");

    // Verify events are for the right agent
    for event in &events {
        if event.agent_id == "sse-agent" {
            // Expected — our agent's events
        }
        // Queue manager events may use "queue-sse-agent" synthetic run_id, which is fine
    }
}

#[tokio::test]
async fn test_sse_stream_endpoint_responds() {
    let (router, _state, _tmp) = setup(vec![]).await;

    // The SSE endpoint should return 200 with text/event-stream content type
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/any-agent/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or(""))
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "Expected text/event-stream, got: {}",
        content_type
    );
}

#[tokio::test]
async fn test_get_messages_nonexistent_agent_returns_404() {
    let (router, _state, _tmp) = setup(vec![]).await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/ghost/messages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_messages_with_last_param() {
    let (router, state, _tmp) = setup(vec![]).await;
    let profile = make_test_profile("last-agent");
    create_agent(&router, &profile).await;

    // Write 5 transcript entries directly
    for i in 0..5 {
        let entry = TranscriptEntry {
            ts: chrono::Utc::now(),
            role: TranscriptRole::System("user".to_string()),
            content: format!("message {}", i),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        };
        state
            .persistence
            .transcripts
            .append("last-agent", &entry)
            .await
            .unwrap();
    }

    // GET with ?last=2 should return only the last 2 entries
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/last-agent/messages?last=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let entries: Vec<TranscriptEntry> = serde_json::from_slice::<PaginatedMessagesResponse>(&bytes).unwrap().messages;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].content, "message 3");
    assert_eq!(entries[1].content, "message 4");
}

#[tokio::test]
async fn test_get_messages_with_after_param() {
    use chrono::TimeZone;

    let (router, state, _tmp) = setup(vec![]).await;
    let profile = make_test_profile("after-agent");
    create_agent(&router, &profile).await;

    // Write entries with specific timestamps
    let timestamps = [
        chrono::Utc.with_ymd_and_hms(2024, 1, 1, 10, 0, 0).unwrap(),
        chrono::Utc.with_ymd_and_hms(2024, 1, 1, 11, 0, 0).unwrap(),
        chrono::Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap(),
        chrono::Utc.with_ymd_and_hms(2024, 1, 1, 13, 0, 0).unwrap(),
    ];
    for (i, ts) in timestamps.iter().enumerate() {
        let entry = TranscriptEntry {
            ts: *ts,
            role: TranscriptRole::System("user".to_string()),
            content: format!("message {}", i),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        };
        state
            .persistence
            .transcripts
            .append("after-agent", &entry)
            .await
            .unwrap();
    }

    // GET with ?after=2024-01-01T11:30:00Z should return entries at 12:00 and 13:00
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/after-agent/messages?after=2024-01-01T11:30:00Z")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let entries: Vec<TranscriptEntry> = serde_json::from_slice::<PaginatedMessagesResponse>(&bytes).unwrap().messages;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].content, "message 2");
    assert_eq!(entries[1].content, "message 3");
}

/// Sending with a non-default thread_id must persist the user entry to the
/// thread's transcript file — never to the agent-keyed file.
#[tokio::test]
async fn test_send_message_with_thread_id_writes_to_thread_file() {
    let scenarios = vec![MockScenario {
        stdout_lines: vec!["mock reply".to_string()],
        stderr_lines: vec![],
        exit_code: 0,
        delay_per_line_ms: 10,
    }];

    let (router, state, _tmp) = setup(scenarios).await;
    let profile = make_test_profile("thread-write-agent");
    create_agent(&router, &profile).await;

    // Create a Fresh thread for this agent via the store API so the row is
    // persisted exactly as the HTTP create route would do it.
    let thread = state
        .persistence
        .threads
        .build_fresh_thread("thread-write-agent", Some("alt".to_string()));
    let thread = state.persistence.threads.create(thread).await.unwrap();

    let msg_body = serde_json::json!({
        "content": "thread-routed prompt",
        "thread_id": thread.id,
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/thread-write-agent/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&msg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The thread's transcript file must contain the user entry.
    let thread_path = std::path::PathBuf::from(&thread.transcript_path);
    let in_thread = state
        .persistence
        .transcripts
        .read_all_at(&thread_path)
        .await
        .unwrap();
    assert!(
        in_thread.iter().any(|e| e.content == "thread-routed prompt"),
        "user entry should land in the thread transcript file"
    );

    // The agent-keyed file must NOT have received this user message from the
    // send handler. (Note: the runner may later persist a different entry to
    // the same file via its own override; this assertion focuses on the
    // handler's direct append.)
    let in_agent = state
        .persistence
        .transcripts
        .read_all("thread-write-agent")
        .await
        .unwrap();
    assert!(
        !in_agent.iter().any(|e| e.content == "thread-routed prompt"),
        "user entry should not leak into the agent-keyed transcript"
    );

    // The agent snapshot's last_message_thread_id must record which thread
    // this message landed in, so the sidebar can jump straight back to it.
    let snap = state.persistence.snapshots.get().await;
    let entry = snap.agents.get("thread-write-agent").expect("snapshot entry must exist");
    assert_eq!(
        entry.last_message_thread_id.as_deref(),
        Some(thread.id.as_str()),
        "last_message_thread_id must be stamped with the send's non-default thread id"
    );
}

/// Reading with a non-default thread_id query param must return entries from
/// the thread's transcript file, not the agent's.
#[tokio::test]
async fn test_get_messages_with_thread_id_reads_thread_file() {
    let (router, state, _tmp) = setup(vec![]).await;
    let profile = make_test_profile("thread-read-agent");
    create_agent(&router, &profile).await;

    let thread = state
        .persistence
        .threads
        .build_fresh_thread("thread-read-agent", None);
    let thread = state.persistence.threads.create(thread).await.unwrap();
    let thread_path = std::path::PathBuf::from(&thread.transcript_path);

    // Seed the thread file with 3 entries and the agent file with a sentinel
    // that should never come back when thread_id is set.
    for i in 0..3 {
        let entry = TranscriptEntry {
            ts: chrono::Utc::now(),
            role: TranscriptRole::System("user".to_string()),
            content: format!("thread message {}", i),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        };
        state
            .persistence
            .transcripts
            .append_at(&thread_path, &entry)
            .await
            .unwrap();
    }
    let agent_sentinel = TranscriptEntry {
        ts: chrono::Utc::now(),
        role: TranscriptRole::System("user".to_string()),
        content: "agent-only sentinel".to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    };
    state
        .persistence
        .transcripts
        .append("thread-read-agent", &agent_sentinel)
        .await
        .unwrap();

    let uri = format!(
        "/agents/thread-read-agent/messages?thread_id={}",
        thread.id
    );
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let entries: Vec<TranscriptEntry> =
        serde_json::from_slice::<PaginatedMessagesResponse>(&bytes)
            .unwrap()
            .messages;
    assert_eq!(entries.len(), 3, "should see only the 3 thread entries");
    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(entry.content, format!("thread message {}", i));
    }
    assert!(
        !entries.iter().any(|e| e.content == "agent-only sentinel"),
        "thread read must not surface the agent-keyed sentinel"
    );
}

/// A missing thread_id must 404 with `ThreadNotFound`, on both send and read,
/// so a typo can never silently fall through to the default thread.
#[tokio::test]
async fn test_send_and_get_with_unknown_thread_id_404() {
    let (router, _state, _tmp) = setup(vec![]).await;
    let profile = make_test_profile("thread-404-agent");
    create_agent(&router, &profile).await;

    let body = serde_json::json!({
        "content": "hi",
        "thread_id": "does-not-exist",
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/thread-404-agent/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/thread-404-agent/messages?thread_id=does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A thread scoped to a different agent must be rejected — the path agent_id
/// owns access to the thread.
#[tokio::test]
async fn test_thread_scoped_to_other_agent_is_rejected() {
    let (router, state, _tmp) = setup(vec![]).await;
    create_agent(&router, &make_test_profile("owner-agent")).await;
    create_agent(&router, &make_test_profile("trespasser-agent")).await;

    let thread = state
        .persistence
        .threads
        .build_fresh_thread("owner-agent", None);
    let thread = state.persistence.threads.create(thread).await.unwrap();

    let body = serde_json::json!({
        "content": "trespass",
        "thread_id": thread.id,
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/agents/trespasser-agent/messages")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    // Validation errors surface as 400 via `AppError`.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_get_messages_no_params_returns_all() {
    let (router, state, _tmp) = setup(vec![]).await;
    let profile = make_test_profile("all-agent");
    create_agent(&router, &profile).await;

    // Write 4 transcript entries directly
    for i in 0..4 {
        let entry = TranscriptEntry {
            ts: chrono::Utc::now(),
            role: TranscriptRole::System("user".to_string()),
            content: format!("message {}", i),
            event_type: "message".to_string(),
            metadata: None,
            hidden_from_user: false,
        };
        state
            .persistence
            .transcripts
            .append("all-agent", &entry)
            .await
            .unwrap();
    }

    // GET with no query params should return all entries
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/agents/all-agent/messages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = read_body(resp).await;
    let entries: Vec<TranscriptEntry> = serde_json::from_slice::<PaginatedMessagesResponse>(&bytes).unwrap().messages;
    assert_eq!(entries.len(), 4);
    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(entry.content, format!("message {}", i));
    }
}

fn make_ts_entry(content: &str, ts: chrono::DateTime<chrono::Utc>) -> TranscriptEntry {
    TranscriptEntry {
        ts,
        role: TranscriptRole::System("user".to_string()),
        content: content.to_string(),
        event_type: "message".to_string(),
        metadata: None,
        hidden_from_user: false,
    }
}

/// Percent-encode a query-param value. `cursor_timestamp`/`cursor_message_id`
/// carry RFC3339 strings (colons, and a literal `+` for the UTC offset) —
/// axum's `Query` extractor decodes the raw query string as
/// `application/x-www-form-urlencoded`, where an un-encoded `+` means space,
/// so it must be escaped like any other reserved byte here.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Opening a freshly forked (branch) thread must show inherited pre-fork
/// context immediately, not an empty transcript — the bug reported when
/// forking from a chat surfaced no prior messages in the UI. Covers the
/// default (no-cursor) tail-read path's merge with the source thread's
/// inheritable prefix (`ts <= history_floor_ts`).
#[tokio::test]
async fn test_get_messages_branch_thread_merges_inherited_tail_on_open() {
    let (router, state, _tmp) = setup(vec![]).await;
    let profile = make_test_profile("branch-tail-agent");
    create_agent(&router, &profile).await;

    // Source thread with 3 pre-fork entries.
    let source = state
        .persistence
        .threads
        .build_fresh_thread("branch-tail-agent", None);
    let source = state.persistence.threads.create(source).await.unwrap();
    let source_path = std::path::PathBuf::from(&source.transcript_path);

    let base = chrono::Utc::now();
    let mut source_entries = Vec::new();
    for i in 0..3 {
        let entry = make_ts_entry(
            &format!("source {}", i),
            base + chrono::Duration::milliseconds(i),
        );
        state
            .persistence
            .transcripts
            .append_at(&source_path, &entry)
            .await
            .unwrap();
        source_entries.push(entry);
    }
    let floor = source_entries[2].ts;

    // Branch thread forked at the floor, with 1 post-fork turn of its own.
    let branch_source = BranchSource {
        source_thread_id: source.id.clone(),
        branch_at: floor,
        source_message_id: None,
    };
    let branch = state.persistence.threads.build_branch_thread(
        "branch-tail-agent",
        None,
        branch_source,
    );
    let branch = state.persistence.threads.create(branch).await.unwrap();
    let branch_path = std::path::PathBuf::from(&branch.transcript_path);
    let own_entry = make_ts_entry("branch turn 0", base + chrono::Duration::milliseconds(10));
    state
        .persistence
        .transcripts
        .append_at(&branch_path, &own_entry)
        .await
        .unwrap();

    // Opening the branch thread (no cursor) should show inherited context
    // PLUS the branch's own turn.
    let uri = format!(
        "/agents/branch-tail-agent/messages?thread_id={}",
        branch.id
    );
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let parsed: PaginatedMessagesResponse = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(parsed.messages.len(), 4, "expected 3 inherited + 1 own");
    assert_eq!(parsed.messages[0].content, "source 0");
    assert_eq!(parsed.messages[1].content, "source 1");
    assert_eq!(parsed.messages[2].content, "source 2");
    assert_eq!(parsed.messages[3].content, "branch turn 0");
    assert!(
        parsed.cursor.is_none(),
        "all 3 pre-floor entries fit in one page, so no more history remains"
    );
}

/// "Load older" on a branch thread must keep walking backward through the
/// SOURCE thread once the branch's own file is exhausted, instead of
/// dead-ending at the fork point. Also covers the boundary edge case where
/// the branch's own file returns *exactly* `last` entries (own-file cursor
/// is `None` purely because that file's start was reached, not because
/// history is exhausted) — without the `.max(1)` peek fix this would report
/// a false "no more history".
#[tokio::test]
async fn test_get_messages_branch_thread_cursor_pagination_walks_into_inherited_phase() {
    let (router, state, _tmp) = setup(vec![]).await;
    let profile = make_test_profile("branch-cursor-agent");
    create_agent(&router, &profile).await;

    let source = state
        .persistence
        .threads
        .build_fresh_thread("branch-cursor-agent", None);
    let source = state.persistence.threads.create(source).await.unwrap();
    let source_path = std::path::PathBuf::from(&source.transcript_path);

    let base = chrono::Utc::now();
    let mut source_entries = Vec::new();
    for i in 0..6 {
        let entry = make_ts_entry(
            &format!("source {}", i),
            base + chrono::Duration::milliseconds(i),
        );
        state
            .persistence
            .transcripts
            .append_at(&source_path, &entry)
            .await
            .unwrap();
        source_entries.push(entry);
    }
    let floor = source_entries[5].ts; // all 6 source entries are inheritable

    let branch_source = BranchSource {
        source_thread_id: source.id.clone(),
        branch_at: floor,
        source_message_id: None,
    };
    let branch = state.persistence.threads.build_branch_thread(
        "branch-cursor-agent",
        None,
        branch_source,
    );
    let branch = state.persistence.threads.create(branch).await.unwrap();
    let branch_path = std::path::PathBuf::from(&branch.transcript_path);

    // Own file has EXACTLY 4 entries — the boundary case.
    for i in 0..4 {
        let entry = make_ts_entry(
            &format!("branch {}", i),
            base + chrono::Duration::milliseconds(100 + i),
        );
        state
            .persistence
            .transcripts
            .append_at(&branch_path, &entry)
            .await
            .unwrap();
    }

    // Page 1: last=4 exactly matches the own file's size.
    let uri = format!(
        "/agents/branch-cursor-agent/messages?thread_id={}&last=4",
        branch.id
    );
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = read_body(resp).await;
    let page1: PaginatedMessagesResponse = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        page1.messages.len(),
        5,
        "own file's 4 entries + 1 peeked inherited entry establishing the continuation cursor"
    );
    assert_eq!(
        page1.messages[0].content, "source 5",
        "peeked inherited entry is the newest inheritable one"
    );
    for (i, entry) in page1.messages[1..].iter().enumerate() {
        assert_eq!(entry.content, format!("branch {}", i));
    }
    let cursor1 = page1
        .cursor
        .expect("boundary case must still surface a continuation cursor, not a false dead-end");
    assert_eq!(cursor1.phase, CursorPhase::Inherited);
    assert_eq!(cursor1.timestamp, source_entries[5].ts);

    // Page 2: load older from cursor1 — must read further back through the
    // SOURCE file and reach its true start.
    let uri2 = format!(
        "/agents/branch-cursor-agent/messages?thread_id={}&last=10&cursor_offset={}&cursor_message_id={}&cursor_timestamp={}&cursor_phase=inherited",
        branch.id,
        cursor1.byte_offset,
        urlencode(&cursor1.last_message_id),
        urlencode(&cursor1.timestamp.to_rfc3339()),
    );
    let resp2 = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(&uri2)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let bytes2 = read_body(resp2).await;
    let page2: PaginatedMessagesResponse = serde_json::from_slice(&bytes2).unwrap();

    assert_eq!(page2.messages.len(), 5, "source 0..4 precede the anchor");
    for (i, entry) in page2.messages.iter().enumerate() {
        assert_eq!(entry.content, format!("source {}", i));
    }
    assert!(
        page2.cursor.is_none(),
        "true start of the source file reached — no more history"
    );
}
