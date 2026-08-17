//! End-to-end integration test for the engine tools.
//!
//! Exercises all eight new tools through the full `run_session` pipeline.
//! Two `#[tokio::test]` scenarios:
//!
//! 1. **Happy path** — Brief → Config set/get → TodoWrite → EnterPlanMode →
//!    Write (denied) → ExitPlanMode → EnterWorktree → ExitWorktree →
//!    AskUserQuestionWithForm (answered). All outcomes asserted.
//!
//! 2. **Cancellation mid-AskUserQuestionWithForm** — Brief → form; the
//!    session cancel token fires while the tool is suspended. Asserts
//!    `cancelled == true`, no leaked oneshot senders, cwd at session-start.
//!
//! The existing 3A baseline (`tests/end_to_end.rs`) is NOT modified.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ao_engine_tools_core::{
    DenialTracker, EventSink, FormAnswer, FormBridge, FormResponse, IoTool, NoopDenialTracker,
    PermissionMode, Registry, RunnerContext, SessionKind, ToolOutput, UserEvent,
};
use ao_engine_tools_engine::register_all as register_engine_tools;
use ao_engine_tools_runner::hooks::config::RunnerSettings;
use ao_engine_tools_runner::prompt_bridge::{LiveFormBridge, StubBridge, UserPromptBridge};
use ao_engine_tools_runner::message::{ContentBlock, Message};
use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};
use ao_engine_tools_runner::query_loop::{run_session, RunnerConfig, SessionOutcome};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::time::timeout;

// ── env-var mutex (protects LAUNCHPAD_STUDIO_DATA_DIR across parallel tests) ──

static ENV_MUTEX: Mutex<()> = Mutex::new(());

// ── recording event sink ──────────────────────────────────────────────────────

struct RecordingSink {
    events: Arc<Mutex<Vec<UserEvent>>>,
    form_notify: Arc<tokio::sync::Notify>,
}

impl RecordingSink {
    fn new() -> (
        Self,
        Arc<Mutex<Vec<UserEvent>>>,
        Arc<tokio::sync::Notify>,
    ) {
        let events = Arc::new(Mutex::new(Vec::<UserEvent>::new()));
        let notify = Arc::new(tokio::sync::Notify::new());
        (
            Self {
                events: events.clone(),
                form_notify: notify.clone(),
            },
            events,
            notify,
        )
    }
}

#[async_trait]
impl EventSink for RecordingSink {
    async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
        let is_form_request = matches!(event, UserEvent::FormRequest { .. });
        self.events.lock().unwrap().push(event);
        if is_form_request {
            self.form_notify.notify_one();
        }
        Ok(())
    }
}

// ── stub Write tool (mutates_filesystem = true so plan mode denies it) ────────

struct WriteStub;

#[async_trait]
impl IoTool for WriteStub {
    fn name(&self) -> &str {
        "Write"
    }
    fn description(&self) -> &str {
        "Write content to a file."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "contents": {"type": "string"}
            },
            "required": ["path", "contents"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn mutates_filesystem(&self) -> bool {
        true
    }
    async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        Ok(ToolOutput::text("wrote"))
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn collect_tool_results(outcome: &SessionOutcome) -> Vec<Value> {
    outcome
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { tool_use_id, content, is_error } => {
                let content_str = content.iter().find_map(|b| {
                    if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None }
                }).unwrap_or("");
                // Parse as JSON for structured payloads; fall back to string.
                let content_val: Value = serde_json::from_str(content_str)
                    .unwrap_or_else(|_| Value::String(content_str.to_string()));
                Some(json!({
                    "tool_use_id": tool_use_id,
                    "content": content_val,
                    "is_error": is_error,
                }))
            }
            _ => None,
        })
        .collect()
}

// ── test 1: happy path ────────────────────────────────────────────────────────

#[tokio::test]
async fn happy_path_all_eight_tools() {
    let data_root = tempfile::tempdir().expect("data root tempdir");

    // Point Config's data root at the test tempdir.
    {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        #[allow(deprecated)]
        std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", data_root.path());
    }

    let (sink, events, form_notify) = RecordingSink::new();
    let sink_arc: Arc<dyn EventSink + Send + Sync> = Arc::new(sink);

    let form_bridge = Arc::new(LiveFormBridge::new(sink_arc.clone()));

    let mut registry = Registry::new();
    register_engine_tools(&mut registry);
    registry.register_io(Arc::new(WriteStub));

    let runner_ctx = RunnerContext::new("session-phase3-happy", "agent-phase3")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_event_sink(sink_arc)
        .with_form_bridge(form_bridge.clone() as Arc<dyn FormBridge + Send + Sync>);

    let todos = runner_ctx.todos.clone();
    let permissions = runner_ctx.permissions.clone();
    let cwd_arc = runner_ctx.cwd.clone();
    let session_start_cwd = cwd_arc.read().unwrap().clone();

    let blocked_path = data_root.path().join("blocked.txt");
    let blocked_path_str = blocked_path.to_string_lossy().into_owned();

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "t01".into(),
                name: "Brief".into(),
                input: json!({"summary": "investigate the foo bug"}),
            },
            CompletionEvent::ToolUse {
                id: "t02".into(),
                name: "Config".into(),
                input: json!({"action": "set", "key": "theme", "value": "dark"}),
            },
            CompletionEvent::ToolUse {
                id: "t03".into(),
                name: "Config".into(),
                input: json!({"action": "get", "key": "theme"}),
            },
            CompletionEvent::ToolUse {
                id: "t04".into(),
                name: "TodoWrite".into(),
                input: json!({"todos": [
                    {"id": "todo-1", "content": "fix foo",    "status": "in_progress"},
                    {"id": "todo-2", "content": "write test", "status": "pending"},
                    {"id": "todo-3", "content": "deploy",     "status": "completed"}
                ]}),
            },
            CompletionEvent::ToolUse {
                id: "t05".into(),
                name: "EnterPlanMode".into(),
                input: json!({}),
            },
            CompletionEvent::ToolUse {
                id: "t06".into(),
                name: "Write".into(),
                input: json!({"path": blocked_path_str, "contents": "x"}),
            },
            CompletionEvent::ToolUse {
                id: "t07".into(),
                name: "ExitPlanMode".into(),
                input: json!({}),
            },
            CompletionEvent::ToolUse {
                id: "t08".into(),
                name: "EnterWorktree".into(),
                input: json!({"name": "phase3-e2e"}),
            },
            CompletionEvent::ToolUse {
                id: "t09".into(),
                name: "ExitWorktree".into(),
                input: json!({"action": "remove"}),
            },
            CompletionEvent::ToolUse {
                id: "t10".into(),
                name: "AskUserQuestionWithForm".into(),
                input: json!({
                    "title": "continue?",
                    "questions": [{
                        "id": "decision",
                        "type": "radio",
                        "label": "continue?",
                        "options": [
                            {"id": "yes", "label": "yes"},
                            {"id": "no", "label": "no"}
                        ]
                    }]
                }),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];

    let provider = Arc::new(MockProviderClient::new(script));

    // Spawn a task that delivers the form answer once the FormRequest event arrives.
    let bridge_for_task = form_bridge.clone();
    let events_for_task = events.clone();
    let notify_for_task = form_notify.clone();
    tokio::spawn(async move {
        notify_for_task.notified().await;
        let form_id = events_for_task
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|e| match e {
                UserEvent::FormRequest { id, .. } => Some(id.clone()),
                _ => None,
            })
            .expect("FormRequest event must be recorded before notify fires");
        let mut answers = HashMap::new();
        answers.insert(
            "decision".to_string(),
            FormAnswer::Selections(vec!["yes".to_string()]),
        );
        bridge_for_task
            .deliver_form_answer(
                &form_id,
                FormResponse {
                    form_id: form_id.clone(),
                    answers,
                    ..Default::default()
                },
            )
            .expect("deliver_form_answer must succeed");
    });

    let config = RunnerConfig {
        provider,
        bridge: Arc::new(StubBridge) as Arc<dyn UserPromptBridge>,
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings: RunnerSettings::default(),
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    };

    let outcome = timeout(
        Duration::from_secs(15),
        run_session(Vec::new(), runner_ctx, config),
    )
    .await
    .expect("session did not finish in time")
    .expect("session ok");

    // ── basic outcome ──────────────────────────────────────────────────────────
    assert!(!outcome.cancelled);
    assert_eq!(outcome.final_assistant_text, "done");

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 10, "one tool_result per tool_use");

    // t01 Brief
    assert_eq!(results[0]["tool_use_id"], "t01");
    assert_eq!(results[0]["is_error"], false);

    // t02 Config set
    assert_eq!(results[1]["tool_use_id"], "t02");
    assert_eq!(results[1]["is_error"], false);

    // t03 Config get → value equals "dark"
    assert_eq!(results[2]["tool_use_id"], "t03");
    assert_eq!(results[2]["is_error"], false);
    assert_eq!(results[2]["content"], "dark", "Config get must return the value set earlier");

    // t04 TodoWrite
    assert_eq!(results[3]["tool_use_id"], "t04");
    assert_eq!(results[3]["is_error"], false);

    // t05 EnterPlanMode
    assert_eq!(results[4]["tool_use_id"], "t05");
    assert_eq!(results[4]["is_error"], false);

    // t06 Write — must be denied by the plan-mode gate
    assert_eq!(results[5]["tool_use_id"], "t06");
    assert_eq!(results[5]["is_error"], true);
    let write_msg = results[5]["content"].as_str().expect("write denial message");
    assert!(
        write_msg.contains("plan mode"),
        "denial message must mention plan mode, got: {write_msg}"
    );
    assert!(
        !blocked_path.exists(),
        "blocked.txt must not have been created"
    );

    // t07 ExitPlanMode
    assert_eq!(results[6]["tool_use_id"], "t07");
    assert_eq!(results[6]["is_error"], false);

    // t08 EnterWorktree
    assert_eq!(results[7]["tool_use_id"], "t08");
    assert_eq!(results[7]["is_error"], false);

    // t09 ExitWorktree
    assert_eq!(results[8]["tool_use_id"], "t09");
    assert_eq!(results[8]["is_error"], false);

    // t10 AskUserQuestionWithForm → radio answered "yes"
    assert_eq!(results[9]["tool_use_id"], "t10");
    assert_eq!(results[9]["is_error"], false);
    assert_eq!(
        results[9]["content"]["answers"]["decision"]["kind"],
        "selections",
        "form must return a selections answer for the radio field"
    );
    assert_eq!(
        results[9]["content"]["answers"]["decision"]["values"][0],
        "yes",
        "form must return the delivered option id"
    );
    assert!(
        results[9]["content"]["form_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "form result must carry the minted form id"
    );

    // ── state assertions ───────────────────────────────────────────────────────

    // TodoStore has 3 items for this agent
    assert_eq!(todos.get("agent-phase3").len(), 3);

    // Permission mode restored to Default after ExitPlanMode
    assert_eq!(permissions.mode(), PermissionMode::Default);

    // CWD restored to session-start after ExitWorktree
    assert_eq!(*cwd_arc.read().unwrap(), session_start_cwd);

    // No leaked form channels
    assert_eq!(form_bridge.pending_count(), 0);

    // ── event sequence assertion ───────────────────────────────────────────────

    let recorded = events.lock().unwrap().clone();
    let sig_kinds: Vec<&str> = recorded
        .iter()
        .filter_map(|e| match e {
            UserEvent::Brief { .. } => Some("Brief"),
            UserEvent::TodosUpdated { .. } => Some("TodosUpdated"),
            UserEvent::PermissionModeChanged {
                to: PermissionMode::Plan,
                ..
            } => Some("EnterPlan"),
            UserEvent::PermissionModeChanged { .. } => Some("ExitPlan"),
            UserEvent::CwdChanged { .. } => Some("CwdChanged"),
            UserEvent::FormRequest { .. } => Some("FormRequest"),
            _ => None,
        })
        .collect();

    assert_eq!(
        sig_kinds,
        vec![
            "Brief",
            "TodosUpdated",
            "EnterPlan",
            "ExitPlan",
            "CwdChanged", // EnterWorktree
            "CwdChanged", // ExitWorktree
            "FormRequest",
        ],
        "event sequence mismatch: {sig_kinds:?}"
    );

    // Clean up env var so other tests are unaffected.
    {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        #[allow(deprecated)]
        std::env::remove_var("LAUNCHPAD_STUDIO_DATA_DIR");
    }
}

// ── test 2: cancellation mid-AskUserQuestionWithForm ──────────────────────────

#[tokio::test]
async fn cancellation_mid_ask_user_question_with_form() {
    let (sink, _events, form_notify) = RecordingSink::new();
    let sink_arc: Arc<dyn EventSink + Send + Sync> = Arc::new(sink);

    let form_bridge = Arc::new(LiveFormBridge::new(sink_arc.clone()));

    let mut registry = Registry::new();
    register_engine_tools(&mut registry);

    let runner_ctx = RunnerContext::new("session-phase3-cancel", "agent-phase3-cancel")
        .unwrap()
        .with_registry(Arc::new(registry))
        .with_event_sink(sink_arc)
        .with_form_bridge(form_bridge.clone() as Arc<dyn FormBridge + Send + Sync>);

    let cwd_arc = runner_ctx.cwd.clone();
    let session_start_cwd = cwd_arc.read().unwrap().clone();
    // Clone the cancel token so we can fire it from a sibling task.
    let cancel = runner_ctx.cancel.clone();

    let script = vec![vec![
        CompletionEvent::ToolUse {
            id: "c1".into(),
            name: "Brief".into(),
            input: json!({"summary": "starting"}),
        },
        CompletionEvent::ToolUse {
            id: "c2".into(),
            name: "AskUserQuestionWithForm".into(),
            input: json!({
                "title": "proceed?",
                "questions": [{
                    "id": "decision",
                    "type": "radio",
                    "label": "proceed?",
                    "options": [
                        {"id": "yes", "label": "yes"},
                        {"id": "no", "label": "no"}
                    ]
                }]
            }),
        },
        CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
    ]];

    let provider = Arc::new(MockProviderClient::new(script));

    // Spawn a task that fires the cancel token once the FormRequest event arrives.
    let notify_clone = form_notify.clone();
    tokio::spawn(async move {
        notify_clone.notified().await;
        cancel.cancel();
    });

    let config = RunnerConfig {
        provider,
        bridge: Arc::new(StubBridge) as Arc<dyn UserPromptBridge>,
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings: RunnerSettings::default(),
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    };

    let outcome = timeout(
        Duration::from_secs(10),
        run_session(Vec::new(), runner_ctx, config),
    )
    .await
    .expect("session did not finish in time")
    .expect("session ok");

    assert!(outcome.cancelled, "session must be cancelled");

    // form_bridge.cancel_pending (called from on_session_end) must have drained
    // the channel map — the suspended ask_form's sender must not leak.
    assert_eq!(form_bridge.pending_count(), 0, "no leaked oneshot senders");

    // No worktree was entered, so cwd must remain at session-start.
    assert_eq!(*cwd_arc.read().unwrap(), session_start_cwd);
}
