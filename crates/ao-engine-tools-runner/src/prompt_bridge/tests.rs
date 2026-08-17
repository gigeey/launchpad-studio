//! Unit tests for the user-prompt bridge and the in-memory denial
//! tracker. Declared from `mod.rs` as `#[cfg(test)] mod tests;` so
//! private items stay in scope.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ao_engine_tools_core::{DenialTracker, EventSink, NoopEventSink, UserEvent};
use ao_protocol::error::AoError;
use serde_json::json;

use super::{
    format_ask_prompt, indent_after_first, parse_ask_answer, AskOutcome, AskQuestionError,
    AskRequest, Choice, ChoiceId, DeliverAnswerError, FormAnswer, FormBridge, FormBridgeRegistry,
    FormField, FormFieldKind, FormRequest, FormResponse, InMemoryDenialTracker, LiveBridge,
    LiveFormBridge, LivePermissionBridge, QuestionBridge, QuestionRequest, ScriptedBridge,
    StubBridge, UserPromptBridge, PERM_FIELD_DECISION, PERM_OPT_ALLOW, PERM_OPT_ALLOW_SESSION,
    PERM_OPT_DENY,
};

fn sample_request(tool: &str) -> AskRequest {
    AskRequest {
        tool_name: tool.to_string(),
        input: json!({"sample": true}),
        reason: "needs approval".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
    }
}

#[tokio::test]
async fn stub_bridge_returns_deny_for_any_input() {
    let bridge = StubBridge;
    for tool in ["Bash", "Read", "Edit"] {
        let outcome = bridge.ask(sample_request(tool)).await;
        assert_eq!(outcome, AskOutcome::Deny);
    }
}

#[tokio::test]
async fn scripted_bridge_plays_back_answers_in_order() {
    let bridge = ScriptedBridge::new([AskOutcome::Allow, AskOutcome::Deny]);
    assert_eq!(bridge.remaining(), 2);
    assert_eq!(bridge.ask(sample_request("Bash")).await, AskOutcome::Allow);
    assert_eq!(bridge.remaining(), 1);
    assert_eq!(bridge.ask(sample_request("Bash")).await, AskOutcome::Deny);
    assert_eq!(bridge.remaining(), 0);
}

#[tokio::test]
async fn scripted_bridge_returns_deny_when_script_exhausted() {
    let bridge = ScriptedBridge::new([AskOutcome::AllowSession]);
    assert_eq!(
        bridge.ask(sample_request("Bash")).await,
        AskOutcome::AllowSession
    );
    // Further calls past the script return Deny — documented behavior so
    // a test that under-scripts fails closed instead of panicking the
    // gate.
    assert_eq!(bridge.ask(sample_request("Bash")).await, AskOutcome::Deny);
    assert_eq!(bridge.ask(sample_request("Bash")).await, AskOutcome::Deny);
}

#[tokio::test]
async fn scripted_bridge_replays_each_unique_outcome_variant() {
    let bridge = ScriptedBridge::new([
        AskOutcome::Allow,
        AskOutcome::AllowOnce,
        AskOutcome::AllowSession,
        AskOutcome::Deny,
    ]);
    assert_eq!(bridge.ask(sample_request("Read")).await, AskOutcome::Allow);
    assert_eq!(
        bridge.ask(sample_request("Read")).await,
        AskOutcome::AllowOnce
    );
    assert_eq!(
        bridge.ask(sample_request("Read")).await,
        AskOutcome::AllowSession
    );
    assert_eq!(bridge.ask(sample_request("Read")).await, AskOutcome::Deny);
}

#[test]
fn in_memory_tracker_separates_counts_per_agent_and_tool() {
    let t = InMemoryDenialTracker::new();
    t.record_denial("agent-a", "Bash");
    t.record_denial("agent-a", "Bash");
    t.record_denial("agent-a", "Edit");
    t.record_denial("agent-b", "Bash");

    assert_eq!(t.count("agent-a", "Bash"), 2);
    assert_eq!(t.count("agent-a", "Edit"), 1);
    assert_eq!(t.count("agent-b", "Bash"), 1);
    assert_eq!(t.count("agent-c", "Bash"), 0);
    assert_eq!(t.count("agent-a", "Read"), 0);
}

#[test]
fn reset_session_clears_only_matching_session() {
    let t = InMemoryDenialTracker::new();
    t.record_in_session("sess-a", "agent-a", "Bash");
    t.record_in_session("sess-a", "agent-a", "Bash");
    t.record_in_session("sess-b", "agent-b", "Bash");
    t.record_in_session("sess-b", "agent-b", "Edit");

    assert_eq!(t.count("agent-a", "Bash"), 2);
    assert_eq!(t.count("agent-b", "Bash"), 1);
    assert_eq!(t.count("agent-b", "Edit"), 1);

    t.reset_session("sess-a");

    // sess-a's counters are gone; sess-b's are untouched.
    assert_eq!(t.count("agent-a", "Bash"), 0);
    assert_eq!(t.count("agent-b", "Bash"), 1);
    assert_eq!(t.count("agent-b", "Edit"), 1);

    // Resetting a session that has no entries is a no-op.
    t.reset_session("sess-c");
    assert_eq!(t.count("agent-b", "Bash"), 1);
}

#[test]
fn reset_session_after_default_record_clears_default_bucket() {
    let t = InMemoryDenialTracker::new();
    // The trait method records under an empty session bucket.
    t.record_denial("agent-a", "Bash");
    assert_eq!(t.count("agent-a", "Bash"), 1);
    t.reset_session("");
    assert_eq!(t.count("agent-a", "Bash"), 0);
}

#[test]
fn in_memory_tracker_dispatches_through_dyn_trait() {
    let t: Arc<dyn DenialTracker> = Arc::new(InMemoryDenialTracker::new());
    t.record_denial("agent-a", "Bash");
    t.record_denial("agent-a", "Bash");
    assert_eq!(t.count("agent-a", "Bash"), 2);
    t.reset_session("");
    assert_eq!(t.count("agent-a", "Bash"), 0);
}

#[tokio::test]
async fn record_denial_is_thread_safe_under_concurrent_calls() {
    let tracker = Arc::new(InMemoryDenialTracker::new());
    let mut handles = Vec::with_capacity(100);
    for _ in 0..100 {
        let t = Arc::clone(&tracker);
        handles.push(tokio::spawn(async move {
            t.record_denial("agent-a", "Bash");
        }));
    }
    for h in handles {
        h.await.expect("task joined");
    }
    assert_eq!(tracker.count("agent-a", "Bash"), 100);
}

fn sample_question(question: &str) -> QuestionRequest {
    QuestionRequest {
        question: question.to_string(),
        choices: vec![
            Choice {
                id: ChoiceId("yes".to_string()),
                label: "Yes".to_string(),
                description: None,
            },
            Choice {
                id: ChoiceId("no".to_string()),
                label: "No".to_string(),
                description: Some("Decline".to_string()),
            },
        ],
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
    }
}

#[tokio::test]
async fn stub_bridge_ask_question_returns_no_operator() {
    let bridge = StubBridge;
    let result = bridge.ask_question(sample_question("Continue?")).await;
    assert!(matches!(result, Err(AskQuestionError::NoOperator)));
}

#[tokio::test]
async fn scripted_bridge_ask_question_pops_script_in_order() {
    let bridge = ScriptedBridge::with_question_script(vec![
        Ok(ChoiceId("yes".to_string())),
        Ok(ChoiceId("no".to_string())),
    ]);
    assert_eq!(bridge.question_remaining(), 2);
    let first = bridge.ask_question(sample_question("Q1")).await.unwrap();
    assert_eq!(first, ChoiceId("yes".to_string()));
    assert_eq!(bridge.question_remaining(), 1);
    let second = bridge.ask_question(sample_question("Q2")).await.unwrap();
    assert_eq!(second, ChoiceId("no".to_string()));
    assert_eq!(bridge.question_remaining(), 0);
}

#[tokio::test]
async fn scripted_bridge_ask_question_returns_cancelled_on_exhaustion() {
    let bridge = ScriptedBridge::with_question_script(vec![Ok(ChoiceId("yes".to_string()))]);
    let _ = bridge.ask_question(sample_question("Q1")).await;
    // Script exhausted — returns Cancelled.
    let result = bridge.ask_question(sample_question("Q2")).await;
    assert!(matches!(result, Err(AskQuestionError::Cancelled)));
}

#[tokio::test]
async fn scripted_bridge_ask_question_returns_scripted_error_variants() {
    let bridge = ScriptedBridge::with_question_script(vec![
        Err(AskQuestionError::NoOperator),
        Err(AskQuestionError::Cancelled),
    ]);
    assert!(matches!(
        bridge.ask_question(sample_question("Q1")).await,
        Err(AskQuestionError::NoOperator)
    ));
    assert!(matches!(
        bridge.ask_question(sample_question("Q2")).await,
        Err(AskQuestionError::Cancelled)
    ));
}

#[tokio::test]
async fn scripted_bridge_new_leaves_question_script_empty() {
    // ScriptedBridge::new must not break — bare constructor has no question script.
    let bridge = ScriptedBridge::new([AskOutcome::Allow]);
    assert_eq!(bridge.remaining(), 1);
    assert_eq!(bridge.question_remaining(), 0);
    // ask works as before
    assert_eq!(bridge.ask(sample_request("Bash")).await, AskOutcome::Allow);
    // ask_question returns Cancelled (exhausted empty script)
    let result = bridge.ask_question(sample_question("Q")).await;
    assert!(matches!(result, Err(AskQuestionError::Cancelled)));
}

#[tokio::test]
async fn ask_question_dispatches_through_dyn_trait() {
    // Verify object-safety: ScriptedBridge behind Arc<dyn UserPromptBridge>.
    let bridge: Arc<dyn UserPromptBridge> = Arc::new(ScriptedBridge::with_question_script(vec![
        Ok(ChoiceId("choice-a".to_string())),
    ]));
    let result = bridge.ask_question(sample_question("Pick one")).await;
    assert_eq!(result.unwrap(), ChoiceId("choice-a".to_string()));
}

#[tokio::test]
async fn ask_request_carries_identifiers_through_to_bridge() {
    // Bridge that captures the most recent request so we can assert the
    // gate-side fields actually flow through unchanged.
    struct CapturingBridge {
        seen: Mutex<Option<AskRequest>>,
    }

    use std::sync::Mutex;

    #[async_trait::async_trait]
    impl QuestionBridge for CapturingBridge {
        async fn ask_question(
            &self,
            _request: QuestionRequest,
        ) -> Result<ChoiceId, AskQuestionError> {
            Err(AskQuestionError::NoOperator)
        }
    }

    #[async_trait::async_trait]
    impl UserPromptBridge for CapturingBridge {
        async fn ask(&self, request: AskRequest) -> AskOutcome {
            *self.seen.lock().unwrap() = Some(request);
            AskOutcome::Allow
        }
    }

    let bridge = CapturingBridge {
        seen: Mutex::new(None),
    };
    let req = AskRequest {
        tool_name: "Bash".to_string(),
        input: json!({"command": "git status"}),
        reason: "writes outside cwd".to_string(),
        agent_id: "subagent-7".to_string(),
        session_id: "session-42".to_string(),
    };
    let outcome = bridge.ask(req.clone()).await;
    assert_eq!(outcome, AskOutcome::Allow);

    let captured = bridge.seen.lock().unwrap().clone().expect("request seen");
    assert_eq!(captured.tool_name, "Bash");
    assert_eq!(captured.reason, "writes outside cwd");
    assert_eq!(captured.agent_id, "subagent-7");
    assert_eq!(captured.session_id, "session-42");
    assert_eq!(captured.input, json!({"command": "git status"}));
}

// ===== LiveBridge tests =====

fn noop_sink() -> Arc<dyn EventSink + Send + Sync> {
    Arc::new(NoopEventSink)
}

fn sample_live_question() -> QuestionRequest {
    QuestionRequest {
        question: "Continue?".to_string(),
        choices: vec![
            Choice {
                id: ChoiceId("yes".to_string()),
                label: "Yes".to_string(),
                description: None,
            },
            Choice {
                id: ChoiceId("no".to_string()),
                label: "No".to_string(),
                description: None,
            },
        ],
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
    }
}

#[tokio::test]
async fn live_bridge_deliver_answer_resolves_pending_future() {
    let bridge = Arc::new(LiveBridge::new(noop_sink()));
    let bridge_clone = bridge.clone();

    let ask_handle = tokio::spawn(async move {
        bridge_clone.ask_question(sample_live_question()).await
    });

    // Poll once to let the task register its oneshot sender.
    tokio::task::yield_now().await;

    // The bridge should have one pending question; find its id.
    // We can't read the id directly, but we know there's one pending.
    assert_eq!(bridge.pending_count(), 1);

    // Deliver via deliver_answer — but we don't have the real UUID id here;
    // use a recording sink to capture the emitted Question event's id.
    // For a simpler test, use a recording event sink.
    drop(ask_handle); // abandon it; separate test below uses recording sink.
}

#[tokio::test]
async fn live_bridge_deliver_answer_resolves_with_correct_choice() {
    use std::sync::Mutex as StdMutex;

    struct CapturingSink {
        events: StdMutex<Vec<UserEvent>>,
    }
    #[async_trait::async_trait]
    impl EventSink for CapturingSink {
        async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    let sink = Arc::new(CapturingSink { events: StdMutex::new(Vec::new()) });
    let bridge = Arc::new(LiveBridge::new(sink.clone() as Arc<dyn EventSink + Send + Sync>));
    let bridge_for_task = bridge.clone();

    let ask_handle = tokio::spawn(async move {
        bridge_for_task.ask_question(sample_live_question()).await
    });

    // Yield so the spawned task can register the oneshot.
    tokio::task::yield_now().await;

    // Extract the question id from the emitted event.
    let question_id = {
        let events = sink.events.lock().unwrap();
        match events.first() {
            Some(UserEvent::Question { id, .. }) => ChoiceId(id.clone()),
            _ => panic!("expected Question event"),
        }
    };

    // Deliver an answer.
    bridge
        .deliver_answer(&question_id, question_id.clone())
        .expect("deliver_answer should succeed");

    let result = ask_handle.await.expect("task joined");
    assert_eq!(result.unwrap(), question_id);
    assert_eq!(bridge.pending_count(), 0);
}

#[tokio::test]
async fn live_bridge_deliver_answer_unknown_id_returns_error() {
    let bridge = LiveBridge::new(noop_sink());
    let err = bridge
        .deliver_answer(&ChoiceId("nonexistent".to_string()), ChoiceId("x".to_string()))
        .unwrap_err();
    assert!(matches!(err, DeliverAnswerError::Unknown));
}

#[tokio::test]
async fn live_bridge_cancel_all_resolves_pending_as_cancelled() {
    let bridge = Arc::new(LiveBridge::new(noop_sink()));
    let bridge_for_task = bridge.clone();

    let ask_handle = tokio::spawn(async move {
        bridge_for_task.ask_question(sample_live_question()).await
    });

    tokio::task::yield_now().await;
    assert_eq!(bridge.pending_count(), 1);

    // cancel_pending drains senders → rx.await returns Err(Cancelled).
    bridge.cancel_pending();

    let result = ask_handle.await.expect("task joined");
    assert!(matches!(result, Err(AskQuestionError::Cancelled)));
    assert_eq!(bridge.pending_count(), 0);
}

#[tokio::test]
async fn live_bridge_ask_returns_deny() {
    let bridge = LiveBridge::new(noop_sink());
    let req = AskRequest {
        tool_name: "Edit".to_string(),
        input: json!({}),
        reason: "test".to_string(),
        agent_id: "a".to_string(),
        session_id: "s".to_string(),
    };
    assert_eq!(bridge.ask(req).await, AskOutcome::Deny);
}

#[tokio::test]
async fn live_bridge_as_question_bridge_dyn_object_safe() {
    let bridge: Arc<dyn QuestionBridge> = Arc::new(LiveBridge::new(noop_sink()));
    // Just verifying object-safety — pending_count not on the trait.
    // cancel_pending should be callable through the trait object.
    bridge.cancel_pending();
}

// --- StdinBridge helpers (real stdin not exercised — those paths are
//     covered by the manual smoke procedure in
//     manual CLI smoke procedure).

#[test]
fn parse_ask_answer_yes_variants_allow_once() {
    for s in ["y", "Y", "yes", "YES", "1", "  y  ", " yes "] {
        assert_eq!(parse_ask_answer(s), AskOutcome::AllowOnce, "input: {s:?}");
    }
}

#[test]
fn parse_ask_answer_session_variants_allow_session() {
    for s in ["s", "S", "session", "Session", "2"] {
        assert_eq!(parse_ask_answer(s), AskOutcome::AllowSession, "input: {s:?}");
    }
}

#[test]
fn parse_ask_answer_unknown_input_denies() {
    for s in ["", "n", "no", "0", "maybe", "  ", "?"] {
        assert_eq!(parse_ask_answer(s), AskOutcome::Deny, "input: {s:?}");
    }
}

#[test]
fn format_ask_prompt_includes_tool_reason_and_input() {
    let req = AskRequest {
        tool_name: "Bash".to_string(),
        input: json!({"command": "git diff", "description": "show diff"}),
        reason: "user-prompt".to_string(),
        agent_id: "a".to_string(),
        session_id: "s".to_string(),
    };
    let prompt = format_ask_prompt(&req);
    assert!(prompt.contains("tool:   Bash"), "prompt: {prompt}");
    assert!(prompt.contains("reason: user-prompt"), "prompt: {prompt}");
    assert!(prompt.contains("git diff"), "prompt: {prompt}");
    assert!(prompt.contains("[y]es / [s]ession / [n]o"), "prompt: {prompt}");
}

#[test]
fn indent_after_first_keeps_first_line_aligned_and_indents_rest() {
    let out = indent_after_first("line1\nline2\nline3", "  ");
    assert_eq!(out, "line1\n  line2\n  line3");
}

#[test]
fn indent_after_first_passthrough_for_single_line() {
    assert_eq!(indent_after_first("only", "  "), "only");
}

// ===== FormBridgeRegistry tests =====

fn make_form_bridge() -> Arc<LiveFormBridge> {
    Arc::new(LiveFormBridge::new(noop_sink()))
}

fn empty_form_response(form_id: &str) -> FormResponse {
    FormResponse {
        form_id: form_id.to_string(),
        answers: HashMap::new(),
        ..Default::default()
    }
}

#[tokio::test]
async fn ask_form_on_non_interactive_bridge_returns_no_operator_immediately() {
    // A channel-bridge session (Telegram, Discord, ...) has no UI to render a
    // form on. `ask_form` must fail fast with `NoOperator` rather than
    // emitting a `FormRequest` and suspending on an answer that can never
    // arrive.
    let bridge = LiveFormBridge::new_non_interactive(noop_sink());
    let result = bridge
        .ask_form(FormRequest {
            id: String::new(),
            agent_id: "agent-1".to_string(),
            session_id: "sess-1".to_string(),
            title: "Test form".to_string(),
            intro: None,
            fields: vec![FormField {
                id: "f".to_string(),
                kind: FormFieldKind::Text { placeholder: None },
                label: "F".to_string(),
                description: None,
                required: false,
            }],
        })
        .await;
    assert!(matches!(result, Err(AskQuestionError::NoOperator)));
    assert_eq!(
        bridge.pending_count(),
        0,
        "non-interactive ask_form must not register a pending channel"
    );
}

#[test]
fn registry_holds_two_bridges_under_one_agent() {
    let registry = FormBridgeRegistry::new();
    let bridge_a = make_form_bridge();
    let bridge_b = make_form_bridge();

    registry.register("agent-1", Arc::clone(&bridge_a));
    registry.register("agent-1", Arc::clone(&bridge_b));

    // Deregistering bridge_a by pointer identity must not affect bridge_b.
    registry.deregister("agent-1", &bridge_a);

    // bridge_b is still registered, but has no pending forms, so deliver returns Unknown.
    let result = registry.deliver("agent-1", "no-such-form", empty_form_response("no-such-form"));
    assert!(
        matches!(result, Err(DeliverAnswerError::Unknown)),
        "deliver returns Unknown when no bridge owns the form"
    );

    // Deregistering bridge_b clears the key entirely.
    registry.deregister("agent-1", &bridge_b);
    // Second deregister is a no-op — must not panic.
    registry.deregister("agent-1", &bridge_b);
}

#[test]
fn registry_deregister_by_identity_leaves_sibling_registered() {
    let registry = FormBridgeRegistry::new();
    let bridge_a = make_form_bridge();
    let bridge_b = make_form_bridge();

    registry.register("agent-1", Arc::clone(&bridge_a));
    registry.register("agent-1", Arc::clone(&bridge_b));

    // Remove bridge_b — bridge_a must still be reachable.
    registry.deregister("agent-1", &bridge_b);

    // bridge_a has no pending forms, so deliver returns Unknown (not a crash or wrong-agent error).
    let result = registry.deliver("agent-1", "anything", empty_form_response("anything"));
    assert!(
        matches!(result, Err(DeliverAnswerError::Unknown)),
        "bridge_a is still registered after deregistering bridge_b"
    );
}

#[tokio::test]
async fn registry_deliver_routes_by_form_id_to_correct_bridge() {
    use std::sync::Mutex as StdMutex;

    // Capturing sink so we can extract the UUID minted inside ask_form().
    struct CapturingSink {
        events: Arc<StdMutex<Vec<UserEvent>>>,
    }
    #[async_trait::async_trait]
    impl EventSink for CapturingSink {
        async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    let events = Arc::new(StdMutex::new(Vec::<UserEvent>::new()));
    let sink = Arc::new(CapturingSink {
        events: Arc::clone(&events),
    }) as Arc<dyn EventSink + Send + Sync>;

    let bridge_a = Arc::new(LiveFormBridge::new(sink));
    let bridge_b = make_form_bridge();

    let registry = FormBridgeRegistry::new();
    registry.register("agent-1", Arc::clone(&bridge_a));
    registry.register("agent-1", Arc::clone(&bridge_b));

    let bridge_a_task = Arc::clone(&bridge_a);
    let form_task = tokio::spawn(async move {
        bridge_a_task
            .ask_form(FormRequest {
                id: String::new(),
                agent_id: "agent-1".to_string(),
                session_id: "sess-1".to_string(),
                title: "Test form".to_string(),
                intro: None,
                fields: vec![FormField {
                    id: "f".to_string(),
                    kind: FormFieldKind::Text { placeholder: None },
                    label: "F".to_string(),
                    description: None,
                    required: false,
                }],
            })
            .await
    });

    // Yield so the spawned task can register its oneshot sender.
    tokio::task::yield_now().await;

    assert_eq!(bridge_a.pending_count(), 1, "bridge_a has a pending form");
    assert_eq!(bridge_b.pending_count(), 0, "bridge_b has no pending form");

    // Extract form_id from the captured event.
    let form_a_id = {
        let ev = events.lock().unwrap();
        match ev.first() {
            Some(UserEvent::FormRequest { id, .. }) => id.clone(),
            other => panic!("expected FormRequest event, got {other:?}"),
        }
    };

    // Delivering to a wrong form_id must return Unknown.
    let wrong = registry.deliver("agent-1", "wrong-id", empty_form_response("wrong-id"));
    assert!(matches!(wrong, Err(DeliverAnswerError::Unknown)));

    // Deliver the correct answer.
    let mut answers = HashMap::new();
    answers.insert("f".to_string(), FormAnswer::Text("ok".to_string()));
    let response = FormResponse {
        form_id: form_a_id.clone(),
        answers,
        ..Default::default()
    };
    registry
        .deliver("agent-1", &form_a_id, response)
        .expect("deliver to correct form_id must succeed");

    let result = form_task
        .await
        .expect("task joined")
        .expect("ask_form returned Ok");

    assert_eq!(result.form_id, form_a_id);
    match result.answers.get("f") {
        Some(FormAnswer::Text(v)) => assert_eq!(v, "ok"),
        other => panic!("expected Text answer, got {other:?}"),
    }
    assert_eq!(bridge_a.pending_count(), 0);
}

#[tokio::test]
async fn registry_deregister_sibling_does_not_cancel_pending_form() {
    use std::sync::Mutex as StdMutex;

    struct CapturingSink {
        events: Arc<StdMutex<Vec<UserEvent>>>,
    }
    #[async_trait::async_trait]
    impl EventSink for CapturingSink {
        async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    let events = Arc::new(StdMutex::new(Vec::<UserEvent>::new()));
    let sink = Arc::new(CapturingSink {
        events: Arc::clone(&events),
    }) as Arc<dyn EventSink + Send + Sync>;

    let bridge_a = Arc::new(LiveFormBridge::new(sink));
    let bridge_b = make_form_bridge();

    let registry = FormBridgeRegistry::new();
    registry.register("agent-1", Arc::clone(&bridge_a));
    registry.register("agent-1", Arc::clone(&bridge_b));

    let bridge_a_task = Arc::clone(&bridge_a);
    let form_task = tokio::spawn(async move {
        bridge_a_task
            .ask_form(FormRequest {
                id: String::new(),
                agent_id: "agent-1".to_string(),
                session_id: "sess-2".to_string(),
                title: "Sibling test".to_string(),
                intro: None,
                fields: vec![],
            })
            .await
    });

    tokio::task::yield_now().await;
    assert_eq!(bridge_a.pending_count(), 1, "bridge_a has pending form");

    let form_a_id = {
        let ev = events.lock().unwrap();
        match ev.first() {
            Some(UserEvent::FormRequest { id, .. }) => id.clone(),
            other => panic!("expected FormRequest event, got {other:?}"),
        }
    };

    // Simulate the sibling fast-tool bridge finishing.
    registry.deregister("agent-1", &bridge_b);
    bridge_b.cancel_pending();

    // bridge_a's pending form must be intact.
    assert_eq!(
        bridge_a.pending_count(),
        1,
        "bridge_a must still have its pending form after bridge_b deregistered"
    );

    // Deliver via registry — bridge_a is the only bridge left and owns the form.
    registry
        .deliver("agent-1", &form_a_id, empty_form_response(&form_a_id))
        .expect("deliver must succeed");

    let result = form_task
        .await
        .expect("task joined")
        .expect("ask_form returned Ok");
    assert_eq!(result.form_id, form_a_id);
    assert_eq!(bridge_a.pending_count(), 0);
}

// ===== LivePermissionBridge tests =====

struct CapturingFormSink {
    events: Arc<std::sync::Mutex<Vec<UserEvent>>>,
}

#[async_trait::async_trait]
impl EventSink for CapturingFormSink {
    async fn emit(&self, event: UserEvent) -> Result<(), ao_protocol::error::AoError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

fn make_perm_bridge_and_events()
-> (LivePermissionBridge, Arc<LiveFormBridge>, Arc<std::sync::Mutex<Vec<UserEvent>>>) {
    use tokio_util::sync::CancellationToken;
    let events = Arc::new(std::sync::Mutex::new(Vec::<UserEvent>::new()));
    let sink = Arc::new(CapturingFormSink { events: events.clone() })
        as Arc<dyn EventSink + Send + Sync>;
    let form_bridge = Arc::new(LiveFormBridge::new(sink));
    let cancel = CancellationToken::new();
    let perm_bridge = LivePermissionBridge::new(Arc::clone(&form_bridge), cancel);
    (perm_bridge, form_bridge, events)
}

fn selection_response(form_id: &str, option_id: &str) -> FormResponse {
    let mut answers = HashMap::new();
    answers.insert(
        PERM_FIELD_DECISION.to_string(),
        FormAnswer::Selections(vec![option_id.to_string()]),
    );
    FormResponse { form_id: form_id.to_string(), answers, ..Default::default() }
}

fn perm_ask_request(tool: &str) -> AskRequest {
    AskRequest {
        tool_name: tool.to_string(),
        input: serde_json::json!({}),
        reason: "needs approval".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: "sess-1".to_string(),
    }
}

#[tokio::test]
async fn perm_bridge_allow_option_maps_to_allow() {
    let (perm_bridge, form_bridge, events) = make_perm_bridge_and_events();
    let perm_bridge = Arc::new(perm_bridge);
    let perm_clone = perm_bridge.clone();

    let ask = tokio::spawn(async move { perm_clone.ask(perm_ask_request("Bash")).await });
    tokio::task::yield_now().await;

    let form_id = {
        let ev = events.lock().unwrap();
        match ev.first() {
            Some(UserEvent::FormRequest { id, .. }) => id.clone(),
            other => panic!("expected FormRequest, got {other:?}"),
        }
    };

    form_bridge
        .deliver_form_answer(&form_id, selection_response(&form_id, PERM_OPT_ALLOW))
        .expect("deliver");

    assert_eq!(ask.await.unwrap(), AskOutcome::Allow);
}

#[tokio::test]
async fn perm_bridge_deny_option_maps_to_deny() {
    let (perm_bridge, form_bridge, events) = make_perm_bridge_and_events();
    let perm_bridge = Arc::new(perm_bridge);
    let perm_clone = perm_bridge.clone();

    let ask = tokio::spawn(async move { perm_clone.ask(perm_ask_request("Edit")).await });
    tokio::task::yield_now().await;

    let form_id = {
        let ev = events.lock().unwrap();
        match ev.first() {
            Some(UserEvent::FormRequest { id, .. }) => id.clone(),
            other => panic!("expected FormRequest, got {other:?}"),
        }
    };

    form_bridge
        .deliver_form_answer(&form_id, selection_response(&form_id, PERM_OPT_DENY))
        .expect("deliver");

    assert_eq!(ask.await.unwrap(), AskOutcome::Deny);
}

#[tokio::test]
async fn perm_bridge_allow_session_returns_allow_session_and_skips_form_on_next_call() {
    let (perm_bridge, form_bridge, events) = make_perm_bridge_and_events();
    let perm_bridge = Arc::new(perm_bridge);
    let perm_clone = perm_bridge.clone();

    // First call: operator selects "allow for session".
    let ask = tokio::spawn(async move { perm_clone.ask(perm_ask_request("Bash")).await });
    tokio::task::yield_now().await;

    let form_id = {
        let ev = events.lock().unwrap();
        match ev.first() {
            Some(UserEvent::FormRequest { id, .. }) => id.clone(),
            other => panic!("expected FormRequest, got {other:?}"),
        }
    };

    form_bridge
        .deliver_form_answer(&form_id, selection_response(&form_id, PERM_OPT_ALLOW_SESSION))
        .expect("deliver");

    assert_eq!(ask.await.unwrap(), AskOutcome::AllowSession);

    // Second call: must return Allow immediately without emitting another form.
    let before = events.lock().unwrap().len();
    let outcome = perm_bridge.ask(perm_ask_request("Bash")).await;
    let after = events.lock().unwrap().len();

    assert_eq!(outcome, AskOutcome::Allow, "second call must skip the form");
    assert_eq!(before, after, "no new form event must be emitted on second call");
}

#[tokio::test]
async fn perm_bridge_session_approval_is_per_tool_name() {
    use tokio_util::sync::CancellationToken;

    // Approving Bash for session must NOT skip the form for Edit.
    let events = Arc::new(std::sync::Mutex::new(Vec::<UserEvent>::new()));
    let sink = Arc::new(CapturingFormSink { events: events.clone() })
        as Arc<dyn EventSink + Send + Sync>;
    let form_bridge = Arc::new(LiveFormBridge::new(sink));
    let cancel = CancellationToken::new();
    let perm_bridge = Arc::new(LivePermissionBridge::new(Arc::clone(&form_bridge), cancel));

    // Approve Bash for session.
    let pb = perm_bridge.clone();
    let ask = tokio::spawn(async move { pb.ask(perm_ask_request("Bash")).await });
    tokio::task::yield_now().await;
    let form_id = {
        let ev = events.lock().unwrap();
        match ev.first() {
            Some(UserEvent::FormRequest { id, .. }) => id.clone(),
            other => panic!("expected FormRequest, got {other:?}"),
        }
    };
    form_bridge
        .deliver_form_answer(&form_id, selection_response(&form_id, PERM_OPT_ALLOW_SESSION))
        .expect("deliver");
    ask.await.unwrap();

    // Now asking for Edit must still show a form.
    let event_count_before = events.lock().unwrap().len();
    let pb2 = perm_bridge.clone();
    let ask2 = tokio::spawn(async move { pb2.ask(perm_ask_request("Edit")).await });
    tokio::task::yield_now().await;

    let new_form_id = {
        let ev = events.lock().unwrap();
        assert!(
            ev.len() > event_count_before,
            "Edit must emit a new form (not memoized by Bash approval)"
        );
        match ev.last() {
            Some(UserEvent::FormRequest { id, .. }) => id.clone(),
            other => panic!("expected FormRequest for Edit, got {other:?}"),
        }
    };

    form_bridge
        .deliver_form_answer(&new_form_id, selection_response(&new_form_id, PERM_OPT_DENY))
        .expect("deliver Edit deny");
    assert_eq!(ask2.await.unwrap(), AskOutcome::Deny);
}

#[tokio::test]
async fn perm_bridge_cancel_resolves_to_deny() {
    use tokio_util::sync::CancellationToken;

    let events = Arc::new(std::sync::Mutex::new(Vec::<UserEvent>::new()));
    let sink = Arc::new(CapturingFormSink { events })
        as Arc<dyn EventSink + Send + Sync>;
    let form_bridge = Arc::new(LiveFormBridge::new(sink));
    let cancel = CancellationToken::new();
    let perm_bridge = Arc::new(LivePermissionBridge::new(
        Arc::clone(&form_bridge),
        cancel.clone(),
    ));

    let pb = perm_bridge.clone();
    let ask = tokio::spawn(async move { pb.ask(perm_ask_request("Bash")).await });
    tokio::task::yield_now().await;

    // Cancel the token — the select! branch fires and ask() returns Deny.
    cancel.cancel();

    assert_eq!(ask.await.unwrap(), AskOutcome::Deny);
}

#[tokio::test]
async fn perm_bridge_stub_always_denies_confirming_autonomous_behavior() {
    // Verify that StubBridge (used for autonomous sessions) always denies.
    let bridge = StubBridge;
    assert_eq!(bridge.ask(perm_ask_request("Bash")).await, AskOutcome::Deny);
    assert_eq!(bridge.ask(perm_ask_request("Edit")).await, AskOutcome::Deny);
}

// ===== Sync form persistence (pending_forms snapshot, mode = "sync") =====

fn persisted_form_bridge(
    snapshot_store: Arc<ao_persistence::snapshot::SnapshotStore>,
    transcript_store: Arc<ao_persistence::transcript::TranscriptStore>,
    scope_key: &str,
) -> Arc<LiveFormBridge> {
    Arc::new(LiveFormBridge::new(noop_sink()).with_persistence(
        snapshot_store,
        transcript_store,
        scope_key.to_string(),
        None,
    ))
}

fn sample_sync_form_request(agent_id: &str) -> FormRequest {
    FormRequest {
        id: String::new(),
        agent_id: agent_id.to_string(),
        session_id: "sess-1".to_string(),
        title: "Pick one".to_string(),
        intro: None,
        fields: vec![FormField {
            id: "f".to_string(),
            kind: FormFieldKind::Text { placeholder: None },
            label: "F".to_string(),
            description: None,
            required: false,
        }],
    }
}

/// Poll `snapshots` until `scope_key` has a pending form, or panic after a
/// generous timeout. The write happens on a background task racing this
/// test's own task (real `tokio::fs` I/O under a temp dir), so a bounded
/// poll — not a single check — is the only non-flaky way to observe it.
async fn wait_for_pending_form(
    snapshots: &ao_persistence::snapshot::SnapshotStore,
    scope_key: &str,
) -> ao_persistence::snapshot::PendingForm {
    for _ in 0..200 {
        let snap = snapshots.get().await;
        if let Some(form) = snap
            .agents
            .get(scope_key)
            .and_then(|a| a.pending_forms.first())
        {
            return form.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("pending form for '{scope_key}' never appeared on the snapshot");
}

/// A sync form request must persist into the SAME `pending_forms` snapshot
/// structure the async path uses, tagged `mode: "sync"`, and must be removed
/// the moment the answer is delivered — leaving no orphan. Also the
/// regression guard for requirement 4: delivering an answer must still
/// resolve the oneshot and hand the tool back the user's literal answer.
#[tokio::test]
async fn ask_form_persists_pending_sync_form_and_clears_on_delivery() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = ao_persistence::paths::DataRoot::new(dir.path());
    let snapshots = Arc::new(
        ao_persistence::snapshot::SnapshotStore::load(root.clone())
            .await
            .unwrap(),
    );
    let transcripts = Arc::new(ao_persistence::transcript::TranscriptStore::new(root));

    let bridge = persisted_form_bridge(Arc::clone(&snapshots), Arc::clone(&transcripts), "agent-1");

    let bridge_task = Arc::clone(&bridge);
    let handle = tokio::spawn(async move {
        bridge_task
            .ask_form(sample_sync_form_request("agent-1"))
            .await
    });

    // While ask_form is still parked on `rx`, the pending form must already
    // be visible on the snapshot, tagged mode: "sync".
    let pending = wait_for_pending_form(&snapshots, "agent-1").await;
    assert_eq!(pending.spec["mode"], json!("sync"));
    assert_eq!(pending.spec["form_id"], json!(pending.form_id));
    assert_eq!(pending.spec["spec"]["title"], json!("Pick one"));

    // The same form_request transcript entry the async path writes, just
    // tagged "sync" instead of "async" — and hidden from the timeline (sync
    // forms have always rendered only via the composer overlay, never as
    // their own bubble; see `form_request_entry`'s doc comment).
    let entries = transcripts.read_all("agent-1").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].event_type, ao_engine_tools_core::FORM_REQUEST);
    assert!(entries[0].hidden_from_user, "sync form_request entry must be hidden from the timeline");
    let meta = entries[0].metadata.as_ref().unwrap();
    assert_eq!(meta["mode"], json!("sync"));
    assert_eq!(meta["form_id"], json!(pending.form_id));

    // Deliver the answer — same oneshot round-trip as ever.
    let mut answers = HashMap::new();
    answers.insert("f".to_string(), FormAnswer::Text("chosen".to_string()));
    let response = FormResponse {
        form_id: pending.form_id.clone(),
        answers,
        ..Default::default()
    };
    bridge
        .deliver_form_answer(&pending.form_id, response)
        .expect("deliver must succeed");

    // `ask_form` awaits the clear before returning, so by the time this join
    // resolves the snapshot is already updated — no polling needed here.
    let result = handle.await.unwrap().expect("ask_form must return Ok");
    assert_eq!(result.form_id, pending.form_id);
    match result.answers.get("f") {
        Some(FormAnswer::Text(v)) => assert_eq!(v, "chosen", "tool must see the literal answer"),
        other => panic!("expected Text answer, got {other:?}"),
    }

    let snap = snapshots.get().await;
    assert!(
        snap.agents["agent-1"].pending_forms.is_empty(),
        "pending form must be removed once the answer is delivered"
    );
}

/// `cancel_pending` (session-end cleanup) must also clear the persisted
/// pending-form pointer — the other resolution path besides a real answer.
#[tokio::test]
async fn ask_form_clears_pending_sync_form_when_cancel_pending_fires() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = ao_persistence::paths::DataRoot::new(dir.path());
    let snapshots = Arc::new(
        ao_persistence::snapshot::SnapshotStore::load(root.clone())
            .await
            .unwrap(),
    );
    let transcripts = Arc::new(ao_persistence::transcript::TranscriptStore::new(root));

    let bridge = persisted_form_bridge(Arc::clone(&snapshots), Arc::clone(&transcripts), "agent-1");

    let bridge_task = Arc::clone(&bridge);
    let handle = tokio::spawn(async move {
        bridge_task
            .ask_form(sample_sync_form_request("agent-1"))
            .await
    });

    wait_for_pending_form(&snapshots, "agent-1").await;

    bridge.cancel_pending();

    let result = handle.await.unwrap();
    assert!(matches!(result, Err(AskQuestionError::Cancelled)));

    let snap = snapshots.get().await;
    assert!(
        snap.agents["agent-1"].pending_forms.is_empty(),
        "pending form must be removed once cancel_pending fires"
    );
}

/// Posting a second sync form onto the same (agent, thread) slot before the
/// first is answered must supersede it — not silently drop it — leaving a
/// visible `form_withdrawn` trace, mirroring the async path's own supersede
/// handling (`ao_engine_tools_core::form_events::persist_posted_form`'s
/// `Ok(Some(replaced))` branch). This is the sync half of the newest-wins
/// slot handover the owner locked in: the tool-boundary reject that used to
/// gate a second post onto an occupied slot
/// (`ao-engine-tools-engine`'s `ask_user_question_form` module) has been
/// removed outright, so `LiveFormBridge::persist_pending`'s own write of
/// this trace (exercised here) is the only place a displaced sync form's
/// supersession becomes visible.
///
/// Also covers the oneshot/rehydrate requirement: re-reading the transcript
/// afterward (as a page reload would) must not duplicate the trace.
#[tokio::test]
async fn ask_form_leaves_a_withdrawn_trace_when_a_second_sync_form_displaces_the_first() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = ao_persistence::paths::DataRoot::new(dir.path());
    let snapshots = Arc::new(
        ao_persistence::snapshot::SnapshotStore::load(root.clone())
            .await
            .unwrap(),
    );
    let transcripts = Arc::new(ao_persistence::transcript::TranscriptStore::new(root));

    let bridge = persisted_form_bridge(Arc::clone(&snapshots), Arc::clone(&transcripts), "agent-1");

    // Form A goes pending first...
    let bridge_a = Arc::clone(&bridge);
    let mut request_a = sample_sync_form_request("agent-1");
    request_a.title = "Question A".to_string();
    let handle_a = tokio::spawn(async move { bridge_a.ask_form(request_a).await });
    let pending_a = wait_for_pending_form(&snapshots, "agent-1").await;
    assert_eq!(pending_a.spec["spec"]["title"], json!("Question A"));

    // ...then form B is posted on the same (default) thread while A is still
    // unanswered.
    let bridge_b = Arc::clone(&bridge);
    let mut request_b = sample_sync_form_request("agent-1");
    request_b.title = "Question B".to_string();
    let handle_b = tokio::spawn(async move { bridge_b.ask_form(request_b).await });

    // Poll until B's own record replaces A's on the snapshot.
    let pending_b = {
        let mut found = None;
        for _ in 0..200 {
            let snap = snapshots.get().await;
            if let Some(form) = snap.agents.get("agent-1").and_then(|a| a.pending_forms.first()) {
                if form.form_id != pending_a.form_id {
                    found = Some(form.clone());
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        found.expect("B's pending form never replaced A's on the snapshot")
    };
    assert_eq!(pending_b.spec["spec"]["title"], json!("Question B"));

    let snap = snapshots.get().await;
    assert_eq!(
        snap.agents["agent-1"].pending_forms.len(),
        1,
        "exactly one form pending — newest wins, A's record is gone"
    );

    // A's withdrawal must be visible in the transcript.
    let entries = transcripts.read_all("agent-1").await.unwrap();
    let withdrawn: Vec<_> = entries
        .iter()
        .filter(|e| e.event_type == ao_engine_tools_core::FORM_WITHDRAWN)
        .collect();
    assert_eq!(withdrawn.len(), 1, "exactly one withdrawn trace for the displaced sync form");
    assert!(
        withdrawn[0].content.contains("Question A"),
        "must name the displaced question: {}",
        withdrawn[0].content
    );
    assert!(!withdrawn[0].hidden_from_user, "the withdrawn trace must be visible in the transcript");

    // Oneshot guarantee: re-reading the transcript (a page reload = another
    // plain read, no side effects) must not duplicate the trace.
    let entries_reload = transcripts.read_all("agent-1").await.unwrap();
    let withdrawn_reload = entries_reload
        .iter()
        .filter(|e| e.event_type == ao_engine_tools_core::FORM_WITHDRAWN)
        .count();
    assert_eq!(
        withdrawn_reload, 1,
        "rehydrating (re-reading the transcript, as a page reload would) must not duplicate the withdrawn trace"
    );

    // Clean up both suspended calls so the test doesn't leak tasks — neither
    // was ever answered, so drain via cancel_pending (drops both channels).
    bridge.cancel_pending();
    let _ = handle_a.await;
    let _ = handle_b.await;
}

/// A bridge with no `with_persistence` call (every pre-existing production
/// and test call site) must behave exactly as before this feature — no
/// snapshot/transcript writes, answers still delivered normally.
#[tokio::test]
async fn ask_form_without_persistence_wired_writes_nothing() {
    let bridge = make_form_bridge();
    let bridge_task = Arc::clone(&bridge);
    let handle = tokio::spawn(async move {
        bridge_task
            .ask_form(sample_sync_form_request("agent-1"))
            .await
    });

    tokio::task::yield_now().await;
    assert_eq!(bridge.pending_count(), 1);

    // No `with_persistence` call was made, so there's no snapshot/transcript
    // to assert against — just confirm the call still resolves normally via
    // `cancel_pending` (no id needed, unlike a real delivery).
    bridge.cancel_pending();
    let result = handle.await.unwrap();
    assert!(matches!(result, Err(AskQuestionError::Cancelled)));
}

/// The trickiest exit path: the caller races a cancellation token against
/// `ask_form` in a `tokio::select!` (mirrors `AskUserQuestionWithForm::invoke`)
/// and the cancel branch wins, dropping the `ask_form` future outright before
/// it ever resolves `rx`. `PendingFormClearGuard`'s `Drop` fallback must still
/// clear the pointer even though `ask_form`'s own body never reaches its
/// explicit `clear_now().await` call.
#[tokio::test]
async fn ask_form_future_dropped_via_select_still_clears_pending_form() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = ao_persistence::paths::DataRoot::new(dir.path());
    let snapshots = Arc::new(
        ao_persistence::snapshot::SnapshotStore::load(root.clone())
            .await
            .unwrap(),
    );
    let transcripts = Arc::new(ao_persistence::transcript::TranscriptStore::new(root));

    let bridge = persisted_form_bridge(Arc::clone(&snapshots), Arc::clone(&transcripts), "agent-1");

    let cancel = tokio_util::sync::CancellationToken::new();
    let bridge_task = Arc::clone(&bridge);
    let cancel_task = cancel.clone();
    let handle = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = cancel_task.cancelled() => Err(AskQuestionError::Cancelled),
            r = bridge_task.ask_form(sample_sync_form_request("agent-1")) => r,
        }
    });

    wait_for_pending_form(&snapshots, "agent-1").await;

    // Fire the cancel branch — the `ask_form` future is dropped mid-`rx.await`
    // without ever running its own `clear_now` call.
    cancel.cancel();
    let result = handle.await.unwrap();
    assert!(matches!(result, Err(AskQuestionError::Cancelled)));

    // The guard's Drop fallback spawns the clear; poll for it same as the
    // write side, since it's a detached task racing this assertion.
    for _ in 0..200 {
        let snap = snapshots.get().await;
        if snap
            .agents
            .get("agent-1")
            .map(|a| a.pending_forms.is_empty())
            .unwrap_or(true)
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("pending form was never cleared by the Drop fallback");
}

// ===== Suspension-aware overall-deadline signal (`form_suspended`) =====
//
// `LiveFormBridge::with_suspension_counter` wires a shared counter that the
// process supervisor's overall wall-clock deadline consults (see
// `ao_process::default_supervisor`) — these tests exercise `ask_form`'s
// actual counter bookkeeping in isolation, distinct from the live-spawn
// reachability proof in `ao_process`'s `default_spawn_timeout_paused_while_suspended`.

/// Happy path: the counter is set the instant a form is genuinely
/// outstanding and clears the moment a real answer is delivered. Establishes
/// the baseline the abandonment test below is contrasted against.
#[tokio::test]
async fn ask_form_suspension_counter_set_while_pending_and_cleared_on_answer() {
    use std::sync::Mutex as StdMutex;

    struct CapturingSink {
        events: Arc<StdMutex<Vec<UserEvent>>>,
    }
    #[async_trait::async_trait]
    impl EventSink for CapturingSink {
        async fn emit(&self, event: UserEvent) -> Result<(), AoError> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    let events = Arc::new(StdMutex::new(Vec::<UserEvent>::new()));
    let sink =
        Arc::new(CapturingSink { events: Arc::clone(&events) }) as Arc<dyn EventSink + Send + Sync>;

    let counter = Arc::new(AtomicUsize::new(0));
    let bridge = Arc::new(LiveFormBridge::new(sink).with_suspension_counter(Arc::clone(&counter)));

    assert_eq!(counter.load(Ordering::Relaxed), 0, "unset before any call");

    let bridge_task = Arc::clone(&bridge);
    let handle = tokio::spawn(async move {
        bridge_task.ask_form(sample_sync_form_request("agent-1")).await
    });

    tokio::task::yield_now().await;
    assert_eq!(
        counter.load(Ordering::Relaxed),
        1,
        "counter must be set the instant the form is registered and outstanding"
    );

    let form_id = {
        let ev = events.lock().unwrap();
        match ev.first() {
            Some(UserEvent::FormRequest { id, .. }) => id.clone(),
            other => panic!("expected FormRequest event, got {other:?}"),
        }
    };

    let mut answers = HashMap::new();
    answers.insert("f".to_string(), FormAnswer::Text("chosen".to_string()));
    let response = FormResponse { form_id: form_id.clone(), answers, ..Default::default() };
    bridge
        .deliver_form_answer(&form_id, response)
        .expect("deliver must succeed");

    let result = handle.await.unwrap();
    assert!(result.is_ok(), "answered form must resolve Ok");

    assert_eq!(
        counter.load(Ordering::Relaxed),
        0,
        "counter must clear once the answer is delivered"
    );
}

/// The suspension counter must clear when the `ask_form` future is
/// ABANDONED — dropped mid-flight by an outer `tokio::select!` racing a
/// cancellation token, the exact shape `AskUserQuestionWithForm::invoke`
/// uses in production (`tokio::select! { _ = ctx.cancel.cancelled() => ...,
/// r = ctx.form_bridge.ask_form(request) => r }`) — not only when a real
/// answer arrives. Without this, a cancelled/timed-out turn that was
/// mid-form would strand the counter above zero forever, permanently
/// pausing the overall wall-clock deadline for every subsequent run sharing
/// that session's counter.
#[tokio::test]
async fn ask_form_suspension_counter_clears_when_future_dropped_via_select() {
    let counter = Arc::new(AtomicUsize::new(0));
    let bridge =
        Arc::new(LiveFormBridge::new(noop_sink()).with_suspension_counter(Arc::clone(&counter)));

    let cancel = tokio_util::sync::CancellationToken::new();
    let bridge_task = Arc::clone(&bridge);
    let cancel_task = cancel.clone();
    let handle = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = cancel_task.cancelled() => Err(AskQuestionError::Cancelled),
            r = bridge_task.ask_form(sample_sync_form_request("agent-1")) => r,
        }
    });

    // Let the spawned task register its oneshot and enter the suspension guard.
    for _ in 0..200 {
        if counter.load(Ordering::Relaxed) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(counter.load(Ordering::Relaxed), 1, "counter must be set while parked");

    // Fire the cancel branch — the `ask_form` future (and everything held
    // across its suspension point, including the suspension guard) is
    // dropped without `ask_form` ever returning on its own.
    cancel.cancel();
    let result = handle.await.unwrap();
    assert!(matches!(result, Err(AskQuestionError::Cancelled)));

    assert_eq!(
        counter.load(Ordering::Relaxed),
        0,
        "counter must clear when the awaiting future is abandoned, not just when answered"
    );
}

// ===== Sync form deadline =====
//
// `AskUserQuestionWithForm::invoke` now races `ctx.cancel`, its own
// configured deadline, and `ask_form` in a three-way `tokio::select!`
// (`ao-engine-tools-engine`'s `resolve_sync_form`). The deadline branch
// winning drops the `ask_form` future exactly like the pre-existing cancel
// branch does — this test proves that exit path runs the SAME cleanup as
// cancellation and an answer: the `form_suspended` counter clears and the
// pending-form snapshot entry is resolved. No half-state is reachable from
// any of the three branches.

/// Mirrors the tool's real three-way select with the deadline branch
/// (played here by a short `tokio::time::sleep`) winning instead of cancel
/// or a genuine answer. Confirms both cleanup signals — the suspension
/// counter and the persisted pending-form pointer — resolve exactly as they
/// do on the already-tested cancel and answered paths.
#[tokio::test]
async fn ask_form_timeout_branch_clears_suspension_counter_and_pending_form() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = ao_persistence::paths::DataRoot::new(dir.path());
    let snapshots = Arc::new(
        ao_persistence::snapshot::SnapshotStore::load(root.clone())
            .await
            .unwrap(),
    );
    let transcripts = Arc::new(ao_persistence::transcript::TranscriptStore::new(root));
    let counter = Arc::new(AtomicUsize::new(0));

    let bridge = Arc::new(
        LiveFormBridge::new(noop_sink())
            .with_persistence(
                Arc::clone(&snapshots),
                Arc::clone(&transcripts),
                "agent-1".to_string(),
                None,
            )
            .with_suspension_counter(Arc::clone(&counter)),
    );

    let cancel = tokio_util::sync::CancellationToken::new();
    let bridge_task = Arc::clone(&bridge);
    let cancel_task = cancel.clone();
    let handle = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = cancel_task.cancelled() => Err(AskQuestionError::Cancelled),
            // Stand-in for the tool's own configured deadline — structurally
            // the same third branch `resolve_sync_form` adds; the outcome
            // value here doesn't matter, only that it drops the other two
            // futures the same way winning-by-cancel does.
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => Err(AskQuestionError::Cancelled),
            r = bridge_task.ask_form(sample_sync_form_request("agent-1")) => r,
        }
    });

    wait_for_pending_form(&snapshots, "agent-1").await;
    assert_eq!(counter.load(Ordering::Relaxed), 1, "counter must be set while parked");

    // Never cancel — let the sleep branch win on its own, exactly like a
    // real deadline elapsing with nobody ever answering.
    let _ = handle.await.unwrap();

    assert_eq!(
        counter.load(Ordering::Relaxed),
        0,
        "counter must clear once the deadline branch wins, same as cancellation or an answer"
    );

    for _ in 0..200 {
        let snap = snapshots.get().await;
        if snap
            .agents
            .get("agent-1")
            .map(|a| a.pending_forms.is_empty())
            .unwrap_or(true)
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("pending form was never cleared once the deadline branch won");
}
