//! Crate-level integration test exercising the full query-loop pipeline
//! end-to-end against a scripted provider, a real on-disk
//! `settings.json` (loaded from a tempdir), the bash subprocess hook
//! runner, the permission gate, and the `ScriptedBridge`.
//!
//! Mirrors the canonical acceptance scenario: the model emits a
//! turn with `Read`, `Edit`, and `Bash('git push origin main')`
//! tool-use blocks; the project-local `settings.json` declares a
//! pre-tool-use hook that returns `{"decision":"ask","reason":"..."}`
//! for `Bash(git *)`; the bridge replies `Allow`. The transcript ends
//! with three `tool_result` blocks — Read and Edit succeed, Bash also
//! succeeds because the bridge approved it. The Deny variant denies
//! Bash and asserts the tool body never ran.
//!
//! Edit and Bash are stub `IoTool` implementations local to this test;
//! the real ones live in `ao-engine-tools-io`. Read uses that crate's real
//! `Read` tool, so the integration covers a production tool too.
//!
//! Mock provider feature gate: this test relies on `MockProviderClient`,
//! which is exposed via the `mock` feature on `ao-engine-tools-runner`.
//! That feature is on by default.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::{
    DenialTracker, IoTool, NoopDenialTracker, PermissionMode, Registry, RunnerContext, SessionKind, ToolOutput,
};
use ao_engine_tools_io::{Read, register_all};
use ao_engine_tools_runner::hooks::config::{load_runner_settings, HookEntry, RunnerSettings};
use ao_engine_tools_runner::prompt_bridge::{AskOutcome, ScriptedBridge, StubBridge, UserPromptBridge};
use ao_engine_tools_runner::message::{ContentBlock, Message};
use ao_engine_tools_runner::provider::{CompletionEvent, MockProviderClient, StopReason};
use ao_engine_tools_runner::query_loop::{
    run_session, RunnerConfig, SessionOutcome,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::time::timeout;

// ---------- stub tools ----------

/// Minimal stand-in for the real `Edit` tool. Schema is
/// `{file_path, old_string, new_string}`; the body returns a Text
/// payload describing the operation rather than touching the
/// filesystem. Sufficient to exercise the dispatcher pipeline.
struct EditStub;

#[async_trait]
impl IoTool for EditStub {
    fn name(&self) -> &str {
        "Edit"
    }
    fn description(&self) -> &str {
        "Replace `old_string` with `new_string` in `file_path`."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"}
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    async fn invoke(&self, input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let path = input
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or("");
        Ok(ToolOutput::text(format!("edited:{path}")))
    }
}

/// Minimal stand-in for the real `Bash` tool. Records that it was
/// invoked (via a shared atomic) so the permission-deny scenario can
/// assert the body never ran. Returns the command string back as Text.
struct BashStub {
    invocations: Arc<AtomicUsize>,
}

#[async_trait]
impl IoTool for BashStub {
    fn name(&self) -> &str {
        "Bash"
    }
    fn description(&self) -> &str {
        "Run `command` in a bash subshell."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"]
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    async fn invoke(&self, input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        let cmd = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok(ToolOutput::text(format!("ran:{cmd}")))
    }
}

/// Concurrency-safe `Read` stand-in that records peak in-flight
/// invocations. Used by the 12-Reads-cap test to verify the executor
/// never exceeds the configured cap. Sleeps briefly so multiple slots
/// overlap.
struct CountingReadStub {
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

#[async_trait]
impl IoTool for CountingReadStub {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Counting Read stub used by the cap-enforcement test."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"file_path": {"type": "string"}},
            "required": ["file_path"]
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    async fn invoke(&self, _input: Value, _ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let mut peak = self.peak.load(Ordering::SeqCst);
        while now > peak {
            match self
                .peak
                .compare_exchange(peak, now, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(actual) => peak = actual,
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::text("counted"))
    }
}

// ---------- harness ----------

/// Build a project-local `settings.json` under `<tempdir>/.launchpad_studio/`
/// with a single pre-tool-use hook that intercepts `Bash(git *)` and
/// returns `{"decision":"ask","reason":"..."}` on stdout. The hook is a
/// shell script also written into the tempdir so the path is stable.
fn write_ask_hook_settings(tempdir: &TempDir) -> RunnerSettings {
    let project_dir = tempdir.path().join(".launchpad_studio");
    std::fs::create_dir_all(&project_dir).expect("create project settings dir");

    let hook_script_path = tempdir.path().join("ask_hook.sh");
    let hook_script = "#!/usr/bin/env bash\n\
                       cat >/dev/null\n\
                       echo '{\"decision\":\"ask\",\"reason\":\"git push needs confirmation\"}'\n";
    std::fs::write(&hook_script_path, hook_script).expect("write hook script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_script_path)
            .expect("stat hook script")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_script_path, perms).expect("chmod hook script");
    }

    let settings_json = json!({
        "hooks": {
            "pre_tool_use": [{
                "match": "Bash(git *)",
                "command": format!("bash {}", hook_script_path.display()),
                "timeout_ms": 3000,
            }]
        },
        "permissions": {
            "concurrent_tool_cap": 10,
            "deny_count_threshold": 3
        }
    });
    std::fs::write(
        project_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings_json).expect("serialize settings"),
    )
    .expect("write settings.json");

    // Point the data root at a sibling tempdir so the loader's
    // user-global lookup never strays into the developer's real
    // `~/.launchpad_studio/`.
    let data_root = tempdir.path().join("data_root");
    std::fs::create_dir_all(&data_root).expect("create data root");
    std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", &data_root);

    load_runner_settings(tempdir.path()).expect("load runner settings")
}

fn read_edit_bash_script(bash_command: &str) -> Vec<Vec<CompletionEvent>> {
    vec![
        vec![
            CompletionEvent::AssistantText("planning a small edit".into()),
            CompletionEvent::ToolUse {
                id: "call_read".into(),
                name: "Read".into(),
                input: json!({"file_path": "/tmp/integration-fixture.txt"}),
            },
            CompletionEvent::ToolUse {
                id: "call_edit".into(),
                name: "Edit".into(),
                input: json!({
                    "file_path": "/tmp/integration-fixture.txt",
                    "old_string": "alpha",
                    "new_string": "beta"
                }),
            },
            CompletionEvent::ToolUse {
                id: "call_bash".into(),
                name: "Bash".into(),
                input: json!({"command": bash_command}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        vec![
            CompletionEvent::AssistantText("done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ]
}

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

// ---------- the canonical scenario ----------

#[tokio::test]
async fn read_edit_bash_with_ask_hook_and_bridge_allow_runs_all_three() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let settings = write_ask_hook_settings(&tempdir);

    let mut registry = Registry::new();
    registry.register_io(Arc::new(Read));
    registry.register_io(Arc::new(EditStub));
    let bash_calls = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(BashStub {
        invocations: bash_calls.clone(),
    }));

    let runner_ctx = RunnerContext::new("session-int", "agent-int")
        .unwrap()
        .with_registry(Arc::new(registry));

    // Materialize the file Read will hit so the real Read tool
    // actually returns a Text payload (not an error).
    let read_path = std::path::Path::new("/tmp/integration-fixture.txt");
    std::fs::write(read_path, "alpha\nbeta\n").expect("seed fixture file");

    let provider = Arc::new(MockProviderClient::new(read_edit_bash_script(
        "git push origin main",
    )));
    let bridge = Arc::new(ScriptedBridge::new(vec![AskOutcome::Allow]));
    let bridge_dyn: Arc<dyn UserPromptBridge> = bridge.clone();

    let config = RunnerConfig {
        provider,
        bridge: bridge_dyn,
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings,
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    };

    let outcome = timeout(Duration::from_secs(10), run_session(Vec::new(), runner_ctx, config))
        .await
        .expect("session did not finish in time")
        .expect("session ok");

    assert!(!outcome.cancelled);
    assert_eq!(outcome.turns, 2, "tool turn + final text turn");
    assert_eq!(outcome.final_assistant_text, "done");

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 3, "one tool_result per tool_use");
    assert_eq!(results[0]["tool_use_id"], "call_read");
    assert_eq!(results[1]["tool_use_id"], "call_edit");
    assert_eq!(results[2]["tool_use_id"], "call_bash");

    // Read returned its file content as Text.
    assert_eq!(results[0]["is_error"], false);
    let read_content = results[0]["content"].as_str().expect("read content string");
    assert!(read_content.contains("alpha"), "read content: {read_content}");

    // Edit returned the stub's confirmation.
    assert_eq!(results[1]["is_error"], false);
    assert_eq!(results[1]["content"], "edited:/tmp/integration-fixture.txt");

    // Bash ran and the stub recorded one invocation.
    assert_eq!(results[2]["is_error"], false);
    assert_eq!(results[2]["content"], "ran:git push origin main");
    assert_eq!(bash_calls.load(Ordering::SeqCst), 1);

    // Bridge was consulted exactly once (one Bash tool_use → one Ask).
    assert_eq!(bridge.remaining(), 0, "bridge should have replied once");
}

#[tokio::test]
async fn read_edit_bash_with_ask_hook_and_bridge_deny_blocks_bash() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let settings = write_ask_hook_settings(&tempdir);

    let mut registry = Registry::new();
    registry.register_io(Arc::new(Read));
    registry.register_io(Arc::new(EditStub));
    let bash_calls = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(BashStub {
        invocations: bash_calls.clone(),
    }));

    let runner_ctx = RunnerContext::new("session-int-deny", "agent-int")
        .unwrap()
        .with_registry(Arc::new(registry));

    let read_path = std::path::Path::new("/tmp/integration-fixture.txt");
    std::fs::write(read_path, "alpha\nbeta\n").expect("seed fixture file");

    let provider = Arc::new(MockProviderClient::new(read_edit_bash_script(
        "git push origin main",
    )));
    let bridge: Arc<dyn UserPromptBridge> = Arc::new(ScriptedBridge::new(vec![AskOutcome::Deny]));

    let config = RunnerConfig {
        provider,
        bridge,
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings,
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    };

    let outcome = timeout(Duration::from_secs(10), run_session(Vec::new(), runner_ctx, config))
        .await
        .expect("session did not finish in time")
        .expect("session ok");

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 3);
    // Read + Edit unaffected.
    assert_eq!(results[0]["is_error"], false);
    assert_eq!(results[1]["is_error"], false);
    // Bash denied — the body never ran.
    assert_eq!(results[2]["is_error"], true);
    let bash_content = results[2]["content"].as_str().expect("bash content string");
    assert!(
        bash_content.contains("user denied"),
        "bash content: {bash_content}"
    );
    assert_eq!(bash_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn twelve_parallel_reads_respect_concurrency_cap() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let _settings_unused = write_ask_hook_settings(&tempdir);

    let mut registry = Registry::new();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    registry.register_io(Arc::new(CountingReadStub {
        in_flight: in_flight.clone(),
        peak: peak.clone(),
    }));

    let runner_ctx = RunnerContext::new("session-int-parallel", "agent-int")
        .unwrap()
        .with_registry(Arc::new(registry));

    let mut events = Vec::new();
    for i in 0..12 {
        events.push(CompletionEvent::ToolUse {
            id: format!("read_{i}"),
            name: "Read".into(),
            input: json!({"file_path": format!("/tmp/file_{i}.txt")}),
        });
    }
    events.push(CompletionEvent::TurnComplete { stop_reason: StopReason::Natural });
    let script = vec![
        events,
        vec![
            CompletionEvent::AssistantText("scanned".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];
    let provider = Arc::new(MockProviderClient::new(script));

    // Force the cap to 10 even though the loader returned the same
    // value — make the constraint explicit at the call site so the test
    // remains correct if the default ever changes.
    let mut settings = RunnerSettings::default();
    settings.permissions.concurrent_tool_cap = 10;

    let config = RunnerConfig {
        provider,
        bridge: Arc::new(StubBridge) as Arc<dyn UserPromptBridge>,
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings,
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    };

    let outcome = timeout(Duration::from_secs(10), run_session(Vec::new(), runner_ctx, config))
        .await
        .expect("session did not finish in time")
        .expect("session ok");

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 12);
    for r in &results {
        assert_eq!(r["is_error"], false);
        assert_eq!(r["content"], "counted");
    }
    let peak_observed = peak.load(Ordering::SeqCst);
    assert!(peak_observed > 0, "peak counter was never bumped");
    assert!(
        peak_observed <= 10,
        "executor exceeded the concurrency cap: peak={peak_observed}"
    );
}

// ---------- Read → Edit → Read end-to-end integration tests ----------

#[tokio::test]
async fn edit_after_read_through_runner() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let file_path = tempdir.path().join("test.txt");
    std::fs::write(&file_path, "hello world\n").expect("write fixture");
    let file_str = file_path.to_str().unwrap().to_string();

    let mut registry = Registry::new();
    register_all(&mut registry);

    let runner_ctx = RunnerContext::new("session-e2e-edit", "agent-e2e")
        .expect("ctx")
        .with_registry(Arc::new(registry));
    let ctx_clone = runner_ctx.clone();

    let script = vec![
        // Turn 1: Read then Edit in the same turn.
        // Read is concurrency-safe so it runs as Concurrent([Read]) first,
        // populating read_file_state; then Edit runs as Serial(Edit).
        vec![
            CompletionEvent::ToolUse {
                id: "r1".into(),
                name: "Read".into(),
                input: json!({"file_path": file_str}),
            },
            CompletionEvent::ToolUse {
                id: "e1".into(),
                name: "Edit".into(),
                input: json!({
                    "file_path": file_str,
                    "old_string": "world",
                    "new_string": "tools"
                }),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 2: Read the file again to confirm the edit took effect.
        vec![
            CompletionEvent::ToolUse {
                id: "r2".into(),
                name: "Read".into(),
                input: json!({"file_path": file_str}),
            },
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
        // Turn 3: Final text turn (no tool_uses → session exits).
        vec![
            CompletionEvent::AssistantText("all done".into()),
            CompletionEvent::TurnComplete { stop_reason: StopReason::Natural },
        ],
    ];

    let provider = Arc::new(MockProviderClient::new(script));
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

    let outcome = timeout(Duration::from_secs(10), run_session(Vec::new(), runner_ctx, config))
        .await
        .expect("no timeout")
        .expect("session ok");

    assert!(!outcome.cancelled);
    assert_eq!(outcome.turns, 3);

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 3, "r1 + e1 + r2");

    // r1: Read returned "hello world".
    assert_eq!(results[0]["tool_use_id"], "r1");
    assert_eq!(results[0]["is_error"], false);
    let read1 = results[0]["content"].as_str().expect("read1 content");
    assert!(read1.contains("hello world"), "read1: {read1}");

    // e1: Edit succeeded.
    assert_eq!(results[1]["tool_use_id"], "e1");
    assert_eq!(results[1]["is_error"], false);
    let edit1 = results[1]["content"].as_str().expect("edit1 content");
    assert!(edit1.contains("has been updated"), "edit1: {edit1}");

    // r2: Re-read reflects the edit.
    assert_eq!(results[2]["tool_use_id"], "r2");
    assert_eq!(results[2]["is_error"], false);
    let read2 = results[2]["content"].as_str().expect("read2 content");
    assert!(read2.contains("hello tools"), "read2: {read2}");

    // ctx.read_file_state was populated by the Read invocations (Arc-shared).
    assert!(
        ctx_clone.read_file_state.get(&file_path).is_some(),
        "read_file_state should have an entry for the file after the session"
    );
}

#[tokio::test]
async fn edit_without_read_returns_recoverable_error() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let file_path = tempdir.path().join("test.txt");
    std::fs::write(&file_path, "hello world\n").expect("write fixture");
    let file_str = file_path.to_str().unwrap().to_string();

    let mut registry = Registry::new();
    register_all(&mut registry);

    let runner_ctx = RunnerContext::new("session-no-read", "agent-no-read")
        .expect("ctx")
        .with_registry(Arc::new(registry));

    let script = vec![
        // No prior Read — Edit should be blocked at the tool layer.
        vec![
            CompletionEvent::ToolUse {
                id: "e1".into(),
                name: "Edit".into(),
                input: json!({
                    "file_path": file_str,
                    "old_string": "world",
                    "new_string": "tools"
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

    let outcome = timeout(Duration::from_secs(10), run_session(Vec::new(), runner_ctx, config))
        .await
        .expect("no timeout")
        .expect("session ok");

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["is_error"], true);
    let msg = results[0]["content"].as_str().expect("error content");
    assert!(
        msg.contains("has not been read yet"),
        "expected 'has not been read yet', got: {msg}"
    );
}

#[tokio::test]
async fn edit_after_partial_read_returns_recoverable_error() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let file_path = tempdir.path().join("test.txt");
    std::fs::write(&file_path, "hello world\n").expect("write fixture");
    let file_str = file_path.to_str().unwrap().to_string();

    let mut registry = Registry::new();
    register_all(&mut registry);

    let runner_ctx = RunnerContext::new("session-partial-read", "agent-partial")
        .expect("ctx")
        .with_registry(Arc::new(registry));

    let script = vec![
        // Read with an explicit limit → is_partial_view() = true; then Edit
        // should be blocked because only a partial read was done. offset is
        // 1-based (schema minimum 1), so the windowing is driven by limit.
        vec![
            CompletionEvent::ToolUse {
                id: "r1".into(),
                name: "Read".into(),
                input: json!({"file_path": file_str, "offset": 1, "limit": 1}),
            },
            CompletionEvent::ToolUse {
                id: "e1".into(),
                name: "Edit".into(),
                input: json!({
                    "file_path": file_str,
                    "old_string": "world",
                    "new_string": "tools"
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

    let outcome = timeout(Duration::from_secs(10), run_session(Vec::new(), runner_ctx, config))
        .await
        .expect("no timeout")
        .expect("session ok");

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 2, "r1 (success) + e1 (partial-read error)");

    // r1: partial Read succeeded.
    assert_eq!(results[0]["tool_use_id"], "r1");
    assert_eq!(results[0]["is_error"], false);

    // e1: Edit blocked because the file was only partially read.
    assert_eq!(results[1]["tool_use_id"], "e1");
    assert_eq!(results[1]["is_error"], true);
    let msg = results[1]["content"].as_str().expect("error content");
    assert!(
        msg.contains("partially read") || msg.contains("Re-read"),
        "expected partial-read error, got: {msg}"
    );
}

#[tokio::test]
async fn edit_denied_by_permission_rule() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let file_path = tempdir.path().join("test.txt");
    std::fs::write(&file_path, "hello world\n").expect("write fixture");
    let file_str = file_path.to_str().unwrap().to_string();

    // Write a deny hook that matches every Edit invocation.  This simulates
    // the "no Edit permission rule configured" scenario: the gate fires
    // before invoke() is reached, confirming we did not bypass the
    // permission layer when registering the real Edit tool.
    let hook_script_path = tempdir.path().join("deny_edit.sh");
    let hook_script = "#!/usr/bin/env bash\ncat >/dev/null\necho '{\"decision\":\"deny\",\"reason\":\"Edit denied: no Edit permission rule\"}'\n";
    std::fs::write(&hook_script_path, hook_script).expect("write deny hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_script_path)
            .expect("stat hook script")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_script_path, perms).expect("chmod hook script");
    }

    let mut registry = Registry::new();
    register_all(&mut registry);

    let runner_ctx = RunnerContext::new("session-perm-deny", "agent-perm")
        .expect("ctx")
        .with_registry(Arc::new(registry));

    let mut settings = RunnerSettings::default();
    settings.hooks.pre_tool_use.push(HookEntry {
        r#match: "Edit".to_string(),
        command: format!("bash {}", hook_script_path.display()),
        timeout_ms: 3000,
    });

    let script = vec![
        vec![
            CompletionEvent::ToolUse {
                id: "e1".into(),
                name: "Edit".into(),
                input: json!({
                    "file_path": file_str,
                    "old_string": "world",
                    "new_string": "tools"
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
    let config = RunnerConfig {
        provider,
        bridge: Arc::new(StubBridge) as Arc<dyn UserPromptBridge>,
        denial_tracker: Arc::new(NoopDenialTracker) as Arc<dyn DenialTracker>,
        settings,
        mode: PermissionMode::Default,
        kind: SessionKind::Interactive,
        auto_approve: vec![],
        system_prompt: None,
        event_sink: None,
        thinking: None,
        max_turns: None,
    };

    let outcome = timeout(Duration::from_secs(10), run_session(Vec::new(), runner_ctx, config))
        .await
        .expect("no timeout")
        .expect("session ok");

    let results = collect_tool_results(&outcome);
    assert_eq!(results.len(), 1);
    // Denied at the permission layer (pre-tool-use hook), not the tool layer.
    assert_eq!(results[0]["is_error"], true);
    let msg = results[0]["content"].as_str().expect("error content");
    // The message must NOT come from invoke() (which would say "has not been read yet").
    assert!(
        !msg.contains("has not been read yet"),
        "denial should NOT come from the tool layer, got: {msg}"
    );
    assert!(
        msg.contains("denied") || msg.contains("Denied") || msg.contains("Edit denied"),
        "expected permission-layer denial message, got: {msg}"
    );
}
