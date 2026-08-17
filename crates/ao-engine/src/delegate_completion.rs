use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use ao_engine_tools_core::background_agents::handle::{TaskFinalReport, TaskFinalStatus};
use ao_engine_tools_core::delegate_completion_sink::{DelegateCompletionSink, DELEGATE_EXCERPT_CAP};
use ao_persistence::paths::DataRoot;
use ao_persistence::thread_store::ThreadStore;
use ao_persistence::transcript::TranscriptStore;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::message::QueuedMessage;
use ao_protocol::transcript::{TranscriptEntry, TranscriptRole};

use crate::event_bus::EventBus;
use crate::queue_manager::NotificationDispatcher;

/// Production implementation of [`DelegateCompletionSink`] backed by the
/// agent queue manager registry.
///
/// The MCP route handler constructs one of these per request when building the
/// `RunnerContext`.  On delegate completion, `spawn_named_async` calls
/// `notify`, which submits a model-facing [`QueuedMessage`] to the parent
/// agent's durable queue, emits a `DelegateComplete` event on the parent
/// agent's chat channel, and persists a `delegate_complete` transcript marker
/// so the UI pill survives page reloads.
pub struct QueueDelegateCompletionSink {
    dispatcher: Arc<dyn NotificationDispatcher>,
    parent_agent_id: String,
    project_id: Option<String>,
    /// Thread that was active when the `Delegate` tool call that spawned this
    /// background run happened. Tags the `QueuedMessage`/SSE event so the
    /// completion pill and parent wake-up route back to that thread instead
    /// of always falling back to the agent's default-thread transcript.
    thread_id: Option<String>,
    event_bus: Option<Arc<EventBus>>,
    data_root: Option<DataRoot>,
    /// Shared thread store used to resolve `thread_id` to its `Thread` record
    /// (transcript path, kind) at completion time, so the persisted
    /// `delegate_complete` marker can be routed to the thread's own
    /// transcript file for non-default threads.
    threads_store: Option<Arc<ThreadStore>>,
}

impl QueueDelegateCompletionSink {
    pub fn new(
        dispatcher: Arc<dyn NotificationDispatcher>,
        parent_agent_id: impl Into<String>,
    ) -> Self {
        Self {
            dispatcher,
            parent_agent_id: parent_agent_id.into(),
            project_id: None,
            thread_id: None,
            event_bus: None,
            data_root: None,
            threads_store: None,
        }
    }

    pub fn with_project_id(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    pub fn with_thread_id(mut self, thread_id: impl Into<String>) -> Self {
        self.thread_id = Some(thread_id.into());
        self
    }

    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn with_data_root(mut self, data_root: DataRoot) -> Self {
        self.data_root = Some(data_root);
        self
    }

    pub fn with_thread_store(mut self, store: Arc<ThreadStore>) -> Self {
        self.threads_store = Some(store);
        self
    }
}

#[async_trait]
impl DelegateCompletionSink for QueueDelegateCompletionSink {
    async fn notify_started(
        &self,
        delegate_name: &str,
        delegation_id: &str,
        spawned_at: DateTime<Utc>,
    ) {
        let Some(bus) = &self.event_bus else { return };
        let payload = AgentEventPayload::DelegateStarted {
            delegate_name: delegate_name.to_string(),
            delegation_id: delegation_id.to_string(),
            spawned_at,
        };
        bus.emit(
            &self.parent_agent_id,
            &self.parent_agent_id,
            self.thread_id.clone(),
            payload.clone(),
        )
        .await;
        if let Some(pid) = &self.project_id {
            bus.emit(
                &self.parent_agent_id,
                &format!("project:{pid}"),
                self.thread_id.clone(),
                payload,
            )
            .await;
        }
    }

    async fn notify(
        &self,
        delegate_name: &str,
        delegation_id: &str,
        report: &TaskFinalReport,
        transcript_path: &str,
    ) {
        let content =
            build_delegate_notification(delegate_name, delegation_id, report, transcript_path);
        let message = QueuedMessage {
            message_id: Uuid::new_v4().to_string(),
            content,
            queued_at: Utc::now(),
            attachments: vec![],
            source: None,
            focus_path: None,
            thread_id: self.thread_id.clone(),
        };
        if let Err(e) = self
            .dispatcher
            .submit_to_agent(&self.parent_agent_id, message)
            .await
        {
            tracing::warn!(
                parent_agent_id = %self.parent_agent_id,
                delegation_id = %delegation_id,
                error = %e,
                "delegate completion notification: failed to submit message to parent queue",
            );
        }

        let status_str = match report.status {
            TaskFinalStatus::Completed => "completed",
            TaskFinalStatus::Failed => "failed",
            TaskFinalStatus::Cancelled => "cancelled",
        };

        // Emit a UI event on the parent agent's chat channel so the frontend
        // shows a completion pill the instant the delegate finishes.
        if let Some(bus) = &self.event_bus {
            let payload = AgentEventPayload::DelegateComplete {
                delegate_name: delegate_name.to_string(),
                delegation_id: delegation_id.to_string(),
                status: status_str.to_string(),
                duration_ms: report.duration_ms,
                transcript_path: transcript_path.to_string(),
            };
            bus.emit(
                &self.parent_agent_id,
                &self.parent_agent_id,
                self.thread_id.clone(),
                payload.clone(),
            )
            .await;
            if let Some(pid) = &self.project_id {
                bus.emit(
                    &self.parent_agent_id,
                    &format!("project:{pid}"),
                    self.thread_id.clone(),
                    payload,
                )
                .await;
            }
        }

        // Persist a transcript marker so the pill survives page reloads. Clients
        // connected at the moment of completion see the live event above; clients
        // that reload later see this persisted entry and render the same pill.
        if let Some(data_root) = &self.data_root {
            let pill_text = build_pill_text(delegate_name, status_str, report.duration_ms);
            let mut meta = std::collections::HashMap::new();
            meta.insert(
                "status".to_string(),
                serde_json::Value::String(status_str.to_string()),
            );
            meta.insert(
                "delegate_name".to_string(),
                serde_json::Value::String(delegate_name.to_string()),
            );
            let entry = TranscriptEntry {
                ts: Utc::now(),
                role: TranscriptRole::System("system".to_string()),
                content: pill_text,
                event_type: "delegate_complete".to_string(),
                metadata: Some(meta),
                hidden_from_user: false,
            };
            let transcripts = TranscriptStore::new(data_root.clone());
            // Route the on-disk marker to the thread's own transcript file
            // when the Delegate call happened on a non-default thread, so it
            // lands next to the conversation that started the delegate
            // instead of always falling back to the parent agent's legacy
            // transcript.
            let thread = match &self.threads_store {
                Some(store) => store.resolve_non_default(self.thread_id.as_deref()).await,
                None => None,
            };
            let write_result = match thread {
                Some(thread) => {
                    transcripts
                        .append_at(&std::path::PathBuf::from(&thread.transcript_path), &entry)
                        .await
                }
                None => transcripts.append(&self.parent_agent_id, &entry).await,
            };
            if let Err(e) = write_result {
                tracing::warn!(
                    error = %e,
                    parent_agent_id = %self.parent_agent_id,
                    "failed to persist delegate_complete transcript marker",
                );
            }
        }
    }
}

/// Format the pill label shown in the chat timeline for a completed delegate.
pub(crate) fn build_pill_text(
    delegate_name: &str,
    status_str: &str,
    duration_ms: Option<u64>,
) -> String {
    let verb = match status_str {
        "failed" => "failed",
        "cancelled" => "cancelled",
        _ => "completed",
    };
    match duration_ms {
        Some(ms) => {
            let secs = ms as f64 / 1000.0;
            format!("Delegate '{delegate_name}' {verb} · {secs:.1}s")
        }
        None => format!("Delegate '{delegate_name}' {verb}"),
    }
}

/// Largest byte index `<= max` that lands on a valid UTF-8 char boundary in
/// `text`. Used to cap excerpts by byte length without risking a panic when
/// the cap falls inside a multi-byte character (em-dashes, curly quotes, and
/// similar are common in model output).
fn clamp_to_char_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    (0..=max).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0)
}

/// Build the model-facing body of a delegate completion notification.
///
/// Matches the tone and structure of the tasklist-completion message so the
/// parent agent can apply the same decision pattern.  Includes:
/// - delegate name and delegation_id
/// - status (completed / failed / cancelled)
/// - duration (when recorded)
/// - final-text excerpt capped at [`DELEGATE_EXCERPT_CAP`] characters
/// - transcript path
/// - a note that `DelegateOutput` retrieves the full result
pub(crate) fn build_delegate_notification(
    delegate_name: &str,
    delegation_id: &str,
    report: &TaskFinalReport,
    transcript_path: &str,
) -> String {
    let status_word = match report.status {
        TaskFinalStatus::Completed => "completed",
        TaskFinalStatus::Failed => "failed",
        TaskFinalStatus::Cancelled => "cancelled",
    };

    let duration_part = match report.duration_ms {
        Some(ms) => format!(", duration {}ms", ms),
        None => String::new(),
    };

    let transcript_line = if transcript_path.is_empty() {
        String::new()
    } else {
        format!("\ntranscript: {}", transcript_path)
    };

    let result_block = match report.status {
        TaskFinalStatus::Completed => {
            let text = report.final_assistant_text.as_deref().unwrap_or_default();
            let excerpt = if text.len() > DELEGATE_EXCERPT_CAP {
                let boundary = clamp_to_char_boundary(text, DELEGATE_EXCERPT_CAP);
                format!(
                    "{}\n… (output truncated; use DelegateOutput for the full result)",
                    &text[..boundary]
                )
            } else {
                text.to_string()
            };
            if excerpt.is_empty() {
                "\n\nResult: (no output)".to_string()
            } else {
                format!("\n\nResult:\n{}", excerpt)
            }
        }
        TaskFinalStatus::Failed => {
            let err = report
                .error_message
                .as_deref()
                .unwrap_or("(no error message)");
            format!("\n\nError: {}", err)
        }
        TaskFinalStatus::Cancelled => String::new(),
    };

    format!(
        "Delegate '{}' {}{} (delegation_id={}).{}{}\n\nUse DelegateOutput with delegation_id='{}' to retrieve the full result.",
        delegate_name,
        status_word,
        duration_part,
        delegation_id,
        transcript_line,
        result_block,
        delegation_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use ao_engine_tools_core::background_agents::handle::TaskFinalReport;
    use ao_protocol::error::AoError;
    use ao_protocol::message::QueuedMessage;

    use crate::queue_manager::NotificationDispatcher;

    // --- build_delegate_notification unit tests ---

    #[test]
    fn notification_completed_includes_name_and_excerpt() {
        let report = TaskFinalReport::completed(Some("hello world".to_string()));
        let content =
            build_delegate_notification("mybot", "abc-123", &report, "/tmp/abc.jsonl");
        assert!(content.contains("completed"), "must mention completed");
        assert!(content.contains("delegation_id=abc-123"), "must include delegation_id");
        assert!(content.contains("hello world"), "must include excerpt");
        assert!(content.contains("DelegateOutput"), "must mention DelegateOutput");
        assert!(
            content.contains("/tmp/abc.jsonl"),
            "must include transcript path"
        );
    }

    #[test]
    fn notification_completed_truncates_long_excerpt() {
        let long_text = "x".repeat(DELEGATE_EXCERPT_CAP + 100);
        let report = TaskFinalReport::completed(Some(long_text));
        let content = build_delegate_notification("mybot", "abc-123", &report, "");
        assert!(
            content.contains("truncated"),
            "must note truncation for long excerpts"
        );
    }

    /// Regression for a production panic: `text[..DELEGATE_EXCERPT_CAP]` used
    /// to be a raw byte-index slice, which panics ("byte index 2000 is not a
    /// char boundary") whenever the cap falls inside a multi-byte character.
    /// This builds text with a 3-byte em-dash ('—') straddling the cap
    /// exactly, so the naive slice would land mid-character.
    #[test]
    fn notification_completed_truncation_does_not_panic_on_multibyte_boundary() {
        let prefix = "x".repeat(DELEGATE_EXCERPT_CAP - 1);
        let long_text = format!("{prefix}—{}", "y".repeat(100));
        assert!(
            !long_text.is_char_boundary(DELEGATE_EXCERPT_CAP),
            "test setup must place a multi-byte char straddling the cap"
        );

        let report = TaskFinalReport::completed(Some(long_text));
        let content = build_delegate_notification("mybot", "abc-123", &report, "");

        assert!(content.contains("truncated"), "must note truncation");
    }

    #[test]
    fn clamp_to_char_boundary_steps_back_from_a_mid_character_index() {
        let text = format!("{}—{}", "x".repeat(1999), "y".repeat(10));
        // Byte index 2000 falls inside the 3-byte '—' (bytes 1999..2002).
        let boundary = clamp_to_char_boundary(&text, 2000);
        assert!(text.is_char_boundary(boundary));
        assert_eq!(boundary, 1999, "must step back to just before the multi-byte char");
    }

    #[test]
    fn clamp_to_char_boundary_is_a_no_op_when_max_exceeds_len() {
        let text = "short";
        assert_eq!(clamp_to_char_boundary(text, 1000), text.len());
    }

    #[test]
    fn notification_failed_includes_error_message() {
        let report = TaskFinalReport::failed("the runner crashed");
        let content = build_delegate_notification("mybot", "abc-123", &report, "");
        assert!(content.contains("failed"), "must mention failed");
        assert!(
            content.contains("the runner crashed"),
            "must include error message"
        );
    }

    #[test]
    fn notification_cancelled_is_brief() {
        let report = TaskFinalReport::cancelled();
        let content = build_delegate_notification("mybot", "abc-123", &report, "");
        assert!(content.contains("cancelled"), "must mention cancelled");
    }

    #[test]
    fn notification_includes_duration_when_present() {
        let report = TaskFinalReport::completed(Some("done".to_string())).with_stats(1234, 3);
        let content = build_delegate_notification("mybot", "abc-123", &report, "");
        assert!(content.contains("1234ms"), "must include duration");
    }

    #[test]
    fn notification_omits_duration_when_absent() {
        let report = TaskFinalReport::completed(Some("done".to_string()));
        let content = build_delegate_notification("mybot", "abc-123", &report, "");
        assert!(!content.contains("ms,"), "must not mention duration when absent");
    }

    // --- QueueDelegateCompletionSink integration test ---

    /// Recording dispatcher that captures every submit_to_agent call.
    struct RecordingDispatcher {
        submissions: Mutex<Vec<(String, String)>>,
    }

    impl RecordingDispatcher {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                submissions: Mutex::new(vec![]),
            })
        }

        fn submissions(&self) -> Vec<(String, String)> {
            self.submissions.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl NotificationDispatcher for RecordingDispatcher {
        async fn submit_to_agent(
            &self,
            target_agent_id: &str,
            message: QueuedMessage,
        ) -> Result<(), AoError> {
            self.submissions
                .lock()
                .unwrap()
                .push((target_agent_id.to_string(), message.content));
            Ok(())
        }
    }

    #[tokio::test]
    async fn sink_dispatches_message_to_parent_on_completion() {
        let dispatcher = RecordingDispatcher::new();
        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-agent-id",
        );

        let report = TaskFinalReport::completed(Some("great result".to_string()));
        sink.notify("worker", "del-001", &report, "/data/del-001.jsonl")
            .await;

        let subs = dispatcher.submissions();
        assert_eq!(subs.len(), 1, "must submit exactly one message");
        let (target, content) = &subs[0];
        assert_eq!(target, "parent-agent-id");
        assert!(content.contains("completed"), "must mention status");
        assert!(content.contains("del-001"), "must include delegation_id");
        assert!(content.contains("great result"), "must include excerpt");
        assert!(
            content.contains("/data/del-001.jsonl"),
            "must include transcript path"
        );
        assert!(content.contains("DelegateOutput"), "must mention DelegateOutput");
    }

    #[tokio::test]
    async fn sink_dispatches_failure_message() {
        let dispatcher = RecordingDispatcher::new();
        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-id",
        );

        let report = TaskFinalReport::failed("network timeout");
        sink.notify("bot", "del-002", &report, "").await;

        let subs = dispatcher.submissions();
        assert_eq!(subs.len(), 1);
        let (_target, content) = &subs[0];
        assert!(content.contains("failed"));
        assert!(content.contains("network timeout"));
    }

    #[tokio::test]
    async fn sink_dispatches_cancellation_message() {
        let dispatcher = RecordingDispatcher::new();
        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-id",
        );

        let report = TaskFinalReport::cancelled();
        sink.notify("bot", "del-003", &report, "").await;

        let subs = dispatcher.submissions();
        assert_eq!(subs.len(), 1);
        let (_target, content) = &subs[0];
        assert!(content.contains("cancelled"));
    }

    // --- DelegateStarted event tests ---

    #[tokio::test]
    async fn sink_emits_delegate_started_event() {
        use crate::event_bus::EventBus;

        let dispatcher = RecordingDispatcher::new();
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-start",
        )
        .with_event_bus(Arc::clone(&bus));

        let spawned_at = Utc::now();
        sink.notify_started("mybot", "del-start-1", spawned_at).await;

        let event = rx.recv().await.expect("must receive an event");
        assert_eq!(event.agent_id, "parent-start");
        match event.payload {
            ao_protocol::event::AgentEventPayload::DelegateStarted {
                delegate_name,
                delegation_id,
                spawned_at: event_spawned_at,
            } => {
                assert_eq!(delegate_name, "mybot");
                assert_eq!(delegation_id, "del-start-1");
                assert_eq!(event_spawned_at, spawned_at);
            }
            other => panic!("expected DelegateStarted, got {:?}", other),
        }

        // Unlike `notify`, no message is queued to the parent's durable queue —
        // this is a UI-only signal, not something the model should react to.
        assert!(dispatcher.submissions().is_empty());
    }

    #[tokio::test]
    async fn sink_emits_delegate_started_tagged_to_the_originating_thread() {
        use crate::event_bus::EventBus;

        let dispatcher = RecordingDispatcher::new();
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-start-thread",
        )
        .with_event_bus(Arc::clone(&bus))
        .with_thread_id("thread-xyz");

        sink.notify_started("mybot", "del-start-2", Utc::now()).await;

        let event = rx.recv().await.expect("must receive an event");
        assert_eq!(event.thread_id.as_deref(), Some("thread-xyz"));
    }

    #[tokio::test]
    async fn sink_without_event_bus_notify_started_is_a_harmless_no_op() {
        let dispatcher = RecordingDispatcher::new();
        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-no-bus",
        );

        // Must not panic without an event bus wired.
        sink.notify_started("mybot", "del-start-3", Utc::now()).await;
    }

    // --- DelegateComplete event + transcript marker tests ---

    #[tokio::test]
    async fn sink_emits_delegate_complete_event_on_completion() {
        use crate::event_bus::EventBus;

        let dispatcher = RecordingDispatcher::new();
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-evt",
        )
        .with_event_bus(Arc::clone(&bus));

        let report = TaskFinalReport::completed(Some("ok".to_string())).with_stats(2500, 1);
        sink.notify("mybot", "del-evt-1", &report, "/tmp/t.jsonl").await;

        let event = rx.recv().await.expect("must receive an event");
        match event.payload {
            ao_protocol::event::AgentEventPayload::DelegateComplete {
                delegate_name,
                delegation_id,
                status,
                duration_ms,
                transcript_path,
            } => {
                assert_eq!(delegate_name, "mybot");
                assert_eq!(delegation_id, "del-evt-1");
                assert_eq!(status, "completed");
                assert_eq!(duration_ms, Some(2500));
                assert_eq!(transcript_path, "/tmp/t.jsonl");
            }
            other => panic!("expected DelegateComplete, got {:?}", other),
        }
        assert_eq!(event.agent_id, "parent-evt");
    }

    #[tokio::test]
    async fn sink_persists_delegate_complete_transcript_marker() {
        use ao_persistence::paths::DataRoot;
        use ao_persistence::transcript::TranscriptStore;
        use crate::event_bus::EventBus;

        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.unwrap();

        let dispatcher = RecordingDispatcher::new();
        let bus = Arc::new(EventBus::new(64));

        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-marker",
        )
        .with_event_bus(Arc::clone(&bus))
        .with_data_root(data_root.clone());

        let report = TaskFinalReport::completed(Some("result".to_string())).with_stats(1200, 1);
        sink.notify("workerbot", "del-mark-1", &report, "/tmp/m.jsonl").await;

        let transcripts = TranscriptStore::new(data_root);
        let entries = transcripts.read_recent("parent-marker", 50).await.unwrap();
        let marker = entries
            .iter()
            .find(|e| e.event_type == "delegate_complete")
            .expect("must persist a delegate_complete transcript marker");
        assert!(
            marker.content.contains("workerbot"),
            "marker must name the delegate, got: {}",
            marker.content
        );
        assert!(
            marker.content.contains("completed"),
            "marker must state completed, got: {}",
            marker.content
        );
        assert!(
            marker.content.contains("1.2s"),
            "marker must include duration, got: {}",
            marker.content
        );
    }

    #[tokio::test]
    async fn sink_emits_failed_event_correctly() {
        use crate::event_bus::EventBus;

        let dispatcher = RecordingDispatcher::new();
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-fail",
        )
        .with_event_bus(Arc::clone(&bus));

        let report = TaskFinalReport::failed("crash");
        sink.notify("failbot", "del-fail-1", &report, "").await;

        let event = rx.recv().await.expect("must receive an event");
        match event.payload {
            ao_protocol::event::AgentEventPayload::DelegateComplete { status, .. } => {
                assert_eq!(status, "failed");
            }
            other => panic!("expected DelegateComplete, got {:?}", other),
        }
    }

    #[test]
    fn pill_text_formats_correctly() {
        assert_eq!(
            build_pill_text("mybot", "completed", Some(2100)),
            "Delegate 'mybot' completed · 2.1s"
        );
        assert_eq!(
            build_pill_text("mybot", "failed", None),
            "Delegate 'mybot' failed"
        );
        assert_eq!(
            build_pill_text("mybot", "cancelled", Some(500)),
            "Delegate 'mybot' cancelled · 0.5s"
        );
    }

    // --- project channel dual-emit tests ---

    async fn collect_delegate_complete_events(
        rx: &mut tokio::sync::broadcast::Receiver<ao_protocol::event::AgentEvent>,
    ) -> Vec<ao_protocol::event::AgentEvent> {
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(event) if matches!(event.payload, ao_protocol::event::AgentEventPayload::DelegateComplete { .. }) => {
                    out.push(event);
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        out
    }

    #[tokio::test]
    async fn sink_dual_emits_on_project_channel_when_project_id_is_some() {
        use crate::event_bus::EventBus;

        let dispatcher = RecordingDispatcher::new();
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-proj",
        )
        .with_event_bus(Arc::clone(&bus))
        .with_project_id("proj-abc");

        let report = TaskFinalReport::completed(Some("done".to_string()));
        sink.notify("projbot", "del-proj-1", &report, "").await;

        let events = collect_delegate_complete_events(&mut rx).await;
        assert_eq!(events.len(), 2, "must emit on both agent channel and project channel");

        let channels: Vec<&str> = events.iter().map(|e| e.agent_id.as_str()).collect();
        assert!(channels.contains(&"parent-proj"), "must emit on agent channel");
        assert!(channels.contains(&"project:proj-abc"), "must emit on project channel");

        for e in &events {
            match &e.payload {
                ao_protocol::event::AgentEventPayload::DelegateComplete { delegation_id, .. } => {
                    assert_eq!(delegation_id, "del-proj-1");
                }
                other => panic!("expected DelegateComplete, got {:?}", other),
            }
        }
    }

    #[tokio::test]
    async fn sink_emits_only_agent_channel_when_no_project_id() {
        use crate::event_bus::EventBus;

        let dispatcher = RecordingDispatcher::new();
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-no-proj",
        )
        .with_event_bus(Arc::clone(&bus));

        let report = TaskFinalReport::completed(Some("done".to_string()));
        sink.notify("solo", "del-solo-1", &report, "").await;

        let events = collect_delegate_complete_events(&mut rx).await;
        assert_eq!(events.len(), 1, "must emit only on agent channel when no project_id");
        assert_eq!(events[0].agent_id, "parent-no-proj");
    }

    // --- thread-scoping tests ------------------------------------------

    /// Recording dispatcher that captures the full `QueuedMessage` (not just
    /// content) so tests can assert on `thread_id`.
    struct ThreadCapturingDispatcher {
        messages: Mutex<Vec<QueuedMessage>>,
    }

    impl ThreadCapturingDispatcher {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                messages: Mutex::new(vec![]),
            })
        }

        fn messages(&self) -> Vec<QueuedMessage> {
            self.messages.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl NotificationDispatcher for ThreadCapturingDispatcher {
        async fn submit_to_agent(
            &self,
            _target_agent_id: &str,
            message: QueuedMessage,
        ) -> Result<(), AoError> {
            self.messages.lock().unwrap().push(message);
            Ok(())
        }
    }

    /// Regression for the thread-scoping gap: when `with_thread_id` and
    /// `with_thread_store` are wired and the thread resolves to a non-default
    /// (Fresh/Branch) thread, the completion pill must route back to that
    /// thread — raw thread tag on the `QueuedMessage` and emitted event, and
    /// the on-disk marker persisted at the thread's own transcript path
    /// rather than the parent agent's legacy transcript.
    #[tokio::test]
    async fn sink_with_thread_wiring_routes_marker_and_tags_to_thread() {
        use crate::event_bus::EventBus;

        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        data_root.ensure_directories().await.unwrap();

        let dispatcher = ThreadCapturingDispatcher::new();
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();
        let threads = Arc::new(ThreadStore::load(data_root.clone()).await.unwrap());
        let fresh_row = threads.build_fresh_thread("parent-thread-scoped", None);
        let thread = threads.create(fresh_row).await.unwrap();

        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-thread-scoped",
        )
        .with_event_bus(Arc::clone(&bus))
        .with_data_root(data_root.clone())
        .with_thread_store(Arc::clone(&threads))
        .with_thread_id(thread.id.clone());

        let report = TaskFinalReport::completed(Some("done".to_string()));
        sink.notify("worker", "del-thread-1", &report, "/tmp/x.jsonl").await;

        // QueuedMessage carries the raw thread tag so the parent agent's
        // wake-up is attributed to the originating thread.
        let msgs = dispatcher.messages();
        assert_eq!(msgs.len(), 1, "must submit exactly one message");
        assert_eq!(
            msgs[0].thread_id.as_deref(),
            Some(thread.id.as_str()),
            "QueuedMessage.thread_id must carry the raw thread id",
        );

        // The emitted event carries the same raw thread tag.
        let event = rx.recv().await.expect("must receive an event");
        assert_eq!(
            event.thread_id.as_deref(),
            Some(thread.id.as_str()),
            "emitted DelegateComplete event must carry the raw thread id",
        );

        // The on-disk marker lands in the thread's own transcript file...
        let transcripts = TranscriptStore::new(data_root.clone());
        let thread_path = std::path::PathBuf::from(&thread.transcript_path);
        let thread_entries = transcripts.read_recent_at(&thread_path, 50).await.unwrap();
        assert!(
            thread_entries.iter().any(|e| e.event_type == "delegate_complete"),
            "expected a delegate_complete marker in the thread's own transcript file",
        );

        // ...and NOT in the parent agent's legacy transcript.
        let legacy_entries = transcripts
            .read_recent("parent-thread-scoped", 50)
            .await
            .unwrap();
        assert!(
            !legacy_entries.iter().any(|e| e.event_type == "delegate_complete"),
            "delegate_complete marker must NOT be persisted to the parent's legacy transcript \
             when the delegate call happened on a non-default thread",
        );
    }

    /// Regression guard: with no thread wiring at all (the pre-existing
    /// behavior every current caller exercises), `QueuedMessage.thread_id`
    /// and the emitted event's thread tag both stay `None` — byte-for-byte
    /// unchanged from before thread scoping was added.
    #[tokio::test]
    async fn sink_without_thread_wiring_leaves_thread_tags_none() {
        use crate::event_bus::EventBus;

        let dispatcher = ThreadCapturingDispatcher::new();
        let bus = Arc::new(EventBus::new(64));
        let mut rx = bus.subscribe();

        let sink = QueueDelegateCompletionSink::new(
            Arc::clone(&dispatcher) as Arc<dyn NotificationDispatcher>,
            "parent-no-thread",
        )
        .with_event_bus(Arc::clone(&bus));

        let report = TaskFinalReport::completed(Some("done".to_string()));
        sink.notify("worker", "del-no-thread-1", &report, "").await;

        let msgs = dispatcher.messages();
        assert_eq!(msgs.len(), 1);
        assert!(
            msgs[0].thread_id.is_none(),
            "no thread wiring means QueuedMessage.thread_id stays None",
        );

        let event = rx.recv().await.expect("must receive an event");
        assert!(
            event.thread_id.is_none(),
            "no thread wiring means the emitted event's thread tag stays None",
        );
    }
}
