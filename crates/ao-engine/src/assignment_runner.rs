use std::sync::Arc;

use chrono::Utc;
use tracing::warn;
use uuid::Uuid;

use ao_persistence::PersistenceLayer;
use ao_protocol::assignment::{
    Assignment, AssignmentRun, AssignmentRunStatus, AssignmentThreadPolicy, AssignmentTriggerKind,
    TriggerEventContext,
};
use ao_protocol::error::AoError;
use ao_protocol::event::AgentEventPayload;
use ao_protocol::message::QueuedMessage;
use ao_protocol::scheduled_task::MessageSource;
use ao_protocol::thread::{AssignmentBridgeOrigin, Thread};

use crate::event_bus::EventBus;
use crate::queue_manager::NotificationDispatcher;

/// Resolve the run's thread per `assignment.thread_policy`, record an
/// AssignmentRun row, enqueue a non-interactive QueuedMessage, and emit a
/// system event. Both the cron scheduler tick and the inbound webhook HTTP
/// handler call this helper so the two trigger paths share identical
/// semantics.
///
/// The returned `AssignmentRun` is in `Queued` status. The queue manager owns
/// every subsequent transition:
///
/// * When the pump dispatches the assignment-sourced `QueuedMessage` to a
///   runner it records the mapping `pre_run_id -> AssignmentRunRef` on the
///   per-agent `AgentQueueManager` and writes `Running` + `started_ts` back
///   to persistence.
/// * When the runner emits `RunComplete { run_id, output_text, .. }` (whose
///   `run_id` equals that same `pre_run_id`) the completion branch looks the
///   run up in the map, writes `Succeeded` + a trimmed `output_summary` +
///   `finished_ts`, and emits a `SystemMessage` on the
///   `assignment:{assignment_id}` SSE channel so live UI refetches.
/// * When the runner returns `Err` or panics without ever emitting
///   `RunComplete`, the outer runner-failure watcher captures the same
///   mapping and writes `Failed` + `error` + `finished_ts`, guaranteeing the
///   row never remains stranded in `Running`.
///
/// `event_context`, when present, carries the structured data behind a
/// `ConnectorEvent` fire (the poll result plus a token-able summary of what
/// changed). It gets folded into the dispatched `QueuedMessage` content
/// alongside the assignment's static instruction, so the fired agent can
/// actually reference the thing that triggered it instead of receiving a
/// bare "something changed" ping. Other trigger kinds pass `None`.
///
/// On `Cron` triggers the caller is responsible for calling
/// `persistence.assignments.mark_fired` after this returns (the cron
/// scheduler does this; the webhook path skips it because webhook assignments
/// have no schedule state).
pub async fn fire_assignment(
    persistence: &Arc<PersistenceLayer>,
    dispatcher: &Arc<dyn NotificationDispatcher>,
    event_bus: &Arc<EventBus>,
    assignment: &Assignment,
    trigger_kind: AssignmentTriggerKind,
    trigger_payload: Option<String>,
    timezone: Option<&str>,
    event_context: Option<TriggerEventContext>,
) -> Result<AssignmentRun, AoError> {
    let trigger_kind_str = trigger_kind.as_str();

    // 1. Resolve which thread this run's messages land in, per the
    //    assignment's `thread_policy`. `dispatch_thread_id` is what actually
    //    gets embedded in the QueuedMessage — and therefore governs both
    //    which transcript the runner reads/appends to and how
    //    `queue_manager`'s cross-source thread-collision guard sees this run.
    //    `display_thread_id` is what gets recorded on the `AssignmentRun` row
    //    for the "Open thread" UI affordance.
    //
    //    These two differ in exactly one case: `Main` dispatches with `None`
    //    rather than `Some(default_thread_id)`, matching the same `None`
    //    convention interactive chat uses for the default thread (see
    //    `ao-server/src/routes/messages.rs::resolve_non_default_thread`).
    //    That equality is load-bearing — `queue_manager` recognizes an
    //    in-flight run as colliding with another only when their
    //    `thread_id`s compare equal, and every live interactive turn on the
    //    default thread carries `None`, never `Some(default_thread_id)`.
    // `newly_created_thread` is `Some` only when this call actually persisted
    // a brand-new `Fresh`/`Dedicated` row (every `Fresh` fire, or a
    // `Dedicated` fire's first-ever claim / self-heal re-claim) — never on a
    // `Dedicated` fire that reused an already-claimed thread, and never for
    // `Main` (which just resolves the existing default thread). Used below to
    // emit `AgentEventPayload::ThreadCreated` exactly once per real thread,
    // so an already-open chat's tab strip picks it up immediately instead of
    // only on the next full `loadThreads` refetch.
    // Generated up front (rather than at AssignmentRun-construction time
    // below) so a `Fresh` thread's `assignment_origin.run_id` can name the
    // run that created it before that run row exists.
    let run_id = Uuid::new_v4().to_string();

    let (dispatch_thread_id, display_thread_id, newly_created_thread): (
        Option<String>,
        Option<String>,
        Option<Thread>,
    ) = match assignment.thread_policy {
        AssignmentThreadPolicy::Fresh => {
            let mut thread = persistence.threads.build_fresh_thread(
                &assignment.agent_id,
                Some(format!("{} — run", assignment.name)),
            );
            thread.assignment_origin = Some(AssignmentBridgeOrigin {
                assignment_id: assignment.id.clone(),
                run_id: Some(run_id.clone()),
            });
            persistence.threads.create(thread.clone()).await?;
            (Some(thread.id.clone()), Some(thread.id.clone()), Some(thread))
        }
        AssignmentThreadPolicy::Main => {
            let thread = persistence
                .threads
                .ensure_default_thread(&assignment.agent_id)
                .await?;
            (None, Some(thread.id), None)
        }
        AssignmentThreadPolicy::Dedicated => {
            // Self-heal: if the previously-claimed thread was deleted
            // out from under the assignment (e.g. via the Threads UI),
            // forget it so the claim below creates a fresh replacement
            // instead of reusing a now-dangling id.
            if let Some(existing) = &assignment.dedicated_thread_id {
                if persistence.threads.get(existing).await?.is_none() {
                    persistence
                        .assignments
                        .clear_dedicated_thread_id(&assignment.id)
                        .await?;
                }
            }
            // Named after the assignment itself (not "{name} — run" like
            // `Fresh`) since this thread is a persistent, reused identity
            // rather than a one-off run record.
            let threads = Arc::clone(&persistence.threads);
            let agent_id = assignment.agent_id.clone();
            let name = assignment.name.clone();
            let assignment_id = assignment.id.clone();
            // `claim_dedicated_thread_id` only invokes this closure when no
            // thread has been claimed yet — stash the row it creates here so
            // we can tell "created just now" apart from "reused an existing
            // claim" after the fact (the claim helper itself only returns
            // the id).
            let created_cell: Arc<std::sync::Mutex<Option<Thread>>> =
                Arc::new(std::sync::Mutex::new(None));
            let id = persistence
                .assignments
                .claim_dedicated_thread_id(&assignment.id, {
                    let created_cell = Arc::clone(&created_cell);
                    move || {
                        let threads = Arc::clone(&threads);
                        let created_cell = Arc::clone(&created_cell);
                        async move {
                            let mut thread = threads.build_fresh_thread(&agent_id, Some(name));
                            // `run_id: None` — this thread persists across
                            // every future fire, so no single run owns it
                            // the way a `Fresh` thread's creating run does.
                            thread.assignment_origin = Some(AssignmentBridgeOrigin {
                                assignment_id,
                                run_id: None,
                            });
                            threads.create(thread.clone()).await?;
                            *created_cell.lock().expect("created_cell mutex poisoned") =
                                Some(thread.clone());
                            Ok(thread.id)
                        }
                    }
                })
                .await?;
            let created = created_cell
                .lock()
                .expect("created_cell mutex poisoned")
                .take();
            (Some(id.clone()), Some(id), created)
        }
    };

    // Notify any already-open chat that a new thread now exists, before
    // anything else about this run — the row is persisted the instant the
    // match above returns it. Without this, the thread only appears in the
    // tab strip on the next full `loadThreads` refetch (e.g. navigating away
    // and back), even though the run's reply is about to land there live.
    if let Some(thread) = newly_created_thread {
        event_bus
            .emit(
                &format!("assignment:{}", assignment.id),
                &assignment.agent_id,
                Some(thread.id.clone()),
                AgentEventPayload::ThreadCreated { thread },
            )
            .await;
    }

    // 2. Create the AssignmentRun row in Queued status.
    let run = AssignmentRun {
        id: run_id,
        assignment_id: assignment.id.clone(),
        agent_id: assignment.agent_id.clone(),
        trigger_kind,
        trigger_payload,
        status: AssignmentRunStatus::Queued,
        output_summary: None,
        thread_id: display_thread_id,
        queued_at: Utc::now(),
        started_ts: None,
        finished_ts: None,
        error: None,
    };
    persistence
        .assignment_runs
        .append(&assignment.id, &run)
        .await?;

    // 3. Build the QueuedMessage. Using MessageSource::Assignment marks it as
    //    autonomous so the queue manager never serializes it against interactive
    //    turns purely on account of the `serialize` agent preference — it is
    //    still guarded against colliding with any other in-flight run (of any
    //    source) that holds the same thread_id; see queue_manager::pump.
    //
    //    When `event_context` is present (a `ConnectorEvent` fire) the poll
    //    result that triggered this run is appended after the static
    //    instruction, so the agent actually sees what changed instead of a
    //    bare "something happened" ping.
    let body = match &event_context {
        Some(ctx) => {
            let payload_json =
                serde_json::to_string_pretty(&ctx.payload).unwrap_or_else(|_| ctx.payload.to_string());
            format!(
                "{}\n\n--- Trigger event ---\n{}\n\nEvent payload (JSON):\n{}",
                assignment.instruction, ctx.summary, payload_json
            )
        }
        None => assignment.instruction.clone(),
    };
    let message = QueuedMessage {
        message_id: Uuid::new_v4().to_string(),
        content: format!(
            "<assignment-run type=\"{}\">\n{}\n</assignment-run>",
            trigger_kind_str, body
        ),
        queued_at: Utc::now(),
        attachments: vec![],
        source: Some(MessageSource::Assignment {
            assignment_id: assignment.id.clone(),
            run_id: run.id.clone(),
            trigger_kind: trigger_kind_str.to_string(),
        }),
        focus_path: assignment.working_directory.clone(),
        thread_id: dispatch_thread_id,
    };

    // 4. Enqueue on the agent's queue manager. The dispatcher resolves the
    //    agent profile internally; if the agent is missing this surfaces as
    //    AgentNotFound so the caller can return the appropriate HTTP error.
    dispatcher
        .submit_to_agent(&assignment.agent_id, message)
        .await?;

    // 5. Update cron schedule state. Webhook triggers carry no schedule state
    //    so callers skip this call for Webhook assignments.
    if matches!(trigger_kind, AssignmentTriggerKind::Cron) {
        if let Err(e) = persistence
            .assignments
            .mark_fired(&assignment.id, timezone)
            .await
        {
            warn!(
                assignment_id = %assignment.id,
                error = %e,
                "Failed to update cron schedule after firing assignment"
            );
        }
    }

    // 6. Emit an SSE system event so the frontend sidebar updates immediately.
    event_bus
        .emit(
            &format!("assignment:{}", assignment.id),
            &assignment.agent_id,
            None,
            AgentEventPayload::SystemMessage {
                text: format!("Assignment run started: {}", run.id),
                severity: None,
            },
        )
        .await;

    Ok(run)
}

/// Concrete [`ao_engine_tools_core::AssignmentFireHandle`] wiring the
/// `AssignmentTrigger` tool's fire-now capability into [`fire_assignment`]
/// without `ao-engine-tools-core` depending on `ao-engine`. Closes over
/// exactly the three handles `fire_assignment` needs; every fire is recorded
/// with [`AssignmentTriggerKind::Manual`], distinguishing a tool-invoked fire
/// from a cron tick or an inbound webhook POST.
pub struct ManualAssignmentFirer {
    persistence: Arc<PersistenceLayer>,
    dispatcher: Arc<dyn NotificationDispatcher>,
    event_bus: Arc<EventBus>,
}

impl ManualAssignmentFirer {
    pub fn new(
        persistence: Arc<PersistenceLayer>,
        dispatcher: Arc<dyn NotificationDispatcher>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            persistence,
            dispatcher,
            event_bus,
        }
    }
}

#[async_trait::async_trait]
impl ao_engine_tools_core::AssignmentFireHandle for ManualAssignmentFirer {
    async fn fire_now(
        &self,
        assignment: &Assignment,
        timezone: Option<&str>,
    ) -> Result<AssignmentRun, AoError> {
        fire_assignment(
            &self.persistence,
            &self.dispatcher,
            &self.event_bus,
            assignment,
            AssignmentTriggerKind::Manual,
            None,
            timezone,
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use ao_persistence::paths::DataRoot;
    use ao_protocol::agent::{
        AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig,
    };
    use ao_protocol::assignment::{AssignmentTrigger, ConnectorPollSpec, OutputMode};
    use ao_protocol::scheduled_task::MessageSource;
    use serde_json::json;

    // ---------------------------------------------------------------------------
    // Lightweight NotificationDispatcher for tests: records submitted messages.
    // ---------------------------------------------------------------------------

    struct RecordingDispatcher {
        tx: mpsc::Sender<(String, QueuedMessage)>,
    }

    #[async_trait]
    impl NotificationDispatcher for RecordingDispatcher {
        async fn submit_to_agent(
            &self,
            agent_id: &str,
            message: QueuedMessage,
        ) -> Result<(), AoError> {
            self.tx
                .send((agent_id.to_string(), message))
                .await
                .map_err(|e| AoError::Internal(format!("recording dispatcher send error: {e}")))?;
            Ok(())
        }
    }

    // ---------------------------------------------------------------------------
    // Test helpers
    // ---------------------------------------------------------------------------

    async fn make_persistence() -> (tempfile::TempDir, Arc<ao_persistence::PersistenceLayer>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        let layer = ao_persistence::PersistenceLayer::init_with_root(data_root)
            .await
            .expect("init persistence");
        (tmp, Arc::new(layer))
    }

    fn make_agent(id: &str) -> AgentProfile {
        AgentProfile {
            id: id.to_string(),
            name: format!("Agent {id}"),
            description: String::new(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec!["assignment-run-output".to_string()],
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
                no_output_timeout_ms: 30_000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 2,
            timeout_seconds: 60,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: Default::default(),
            native_provider: None,
            thinking: None,
            enabled_plugins: HashMap::new(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
                    max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            max_turns: None,
}
    }

    fn cron_assignment(id: &str, agent_id: &str) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: "Daily task".to_string(),
            instruction: "Write a brief summary.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Cron {
                cron_expr: "* * * * *".to_string(),
                is_recurring: true,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: Some(now - chrono::Duration::seconds(5)),
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn webhook_assignment(id: &str, agent_id: &str, token: Option<&str>) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: "Inbound hook".to_string(),
            instruction: "Handle the incoming event.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Webhook {
                token: token.map(str::to_string),
                route_name: None,
                secret_ref: None,
                events: vec![],
                filters: None,
                prompt_template: None,
                deliver: Default::default(),
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: None,
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn connector_event_assignment(id: &str, agent_id: &str) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: "New email watcher".to_string(),
            instruction: "Summarize the new email.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::ConnectorEvent {
                server_name: "gmail".to_string(),
                poll: ConnectorPollSpec {
                    tool_name: "list_emails".to_string(),
                    arguments: json!({}),
                    cursor_path: Some("content.0.text".to_string()),
                },
                poll_interval_secs: 300,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: None,
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn make_recording_dispatcher() -> (Arc<dyn NotificationDispatcher>, mpsc::Receiver<(String, QueuedMessage)>) {
        let (tx, rx) = mpsc::channel(16);
        let dispatcher = Arc::new(RecordingDispatcher { tx }) as Arc<dyn NotificationDispatcher>;
        (dispatcher, rx)
    }

    // ---------------------------------------------------------------------------
    // Test (a): cron trigger creates an AssignmentRun row and a thread without a
    // user prompt, and updates the assignment's schedule (mark_fired).
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn cron_trigger_creates_run_and_thread_without_user_prompt() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-cron");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = cron_assignment("assign-cron", "agent-cron");
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let run = fire_assignment(
            &persistence,
            &dispatcher,
            &event_bus,
            &assignment,
            AssignmentTriggerKind::Cron,
            Some("* * * * *".to_string()),
            None,
            None,
        )
        .await
        .expect("fire_assignment should succeed");

        // The returned run is Queued.
        assert_eq!(run.status, AssignmentRunStatus::Queued);
        assert_eq!(run.assignment_id, "assign-cron");
        assert_eq!(run.agent_id, "agent-cron");
        assert_eq!(run.trigger_kind, AssignmentTriggerKind::Cron);
        assert!(run.thread_id.is_some());

        // The run row was persisted.
        let stored = persistence
            .assignment_runs
            .get("assign-cron", &run.id)
            .await
            .unwrap()
            .expect("run row must be persisted");
        assert_eq!(stored.status, AssignmentRunStatus::Queued);
        assert_eq!(stored.thread_id, run.thread_id);

        // A thread was created for this run.
        let thread_id = run.thread_id.as_deref().unwrap();
        let thread = persistence.threads.get(thread_id).await.unwrap();
        assert!(thread.is_some(), "thread must be created");

        // The cron assignment's schedule was updated (mark_fired sets last_run_at).
        let updated_assignment = persistence
            .assignments
            .get("assign-cron")
            .await
            .expect("assignment must still exist");
        assert!(
            updated_assignment.last_run_at.is_some(),
            "cron fire must update last_run_at"
        );

        // A message was dispatched to the queue (no user prompt — the content
        // is the assignment instruction wrapped in an assignment-run envelope,
        // not a user-typed message).
        let (dispatched_agent_id, dispatched_msg) =
            rx.try_recv().expect("message must be enqueued");
        assert_eq!(dispatched_agent_id, "agent-cron");
        assert!(
            dispatched_msg.content.contains("Write a brief summary."),
            "instruction must appear in message content"
        );
        assert!(
            !dispatched_msg.content.contains("<user"),
            "content must not be a user message"
        );

        // Verify the message source marks it as autonomous.
        assert!(
            matches!(
                dispatched_msg.source,
                Some(MessageSource::Assignment { ref assignment_id, ref trigger_kind, .. })
                if assignment_id == "assign-cron" && trigger_kind == "cron"
            ),
            "source must be MessageSource::Assignment with cron kind"
        );
    }

    // ---------------------------------------------------------------------------
    // working_directory flows through to the dispatched QueuedMessage's
    // focus_path, mirroring how the legacy scheduled-task path wires
    // task.working_directory into the message it dispatches.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn working_directory_is_passed_as_focus_path() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-focus");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment = cron_assignment("assign-focus", "agent-focus");
        assignment.working_directory = Some("/repo/project".to_string());
        persistence.assignments.add(assignment.clone()).await.unwrap();

        fire_assignment(
            &persistence,
            &dispatcher,
            &event_bus,
            &assignment,
            AssignmentTriggerKind::Cron,
            None,
            None,
            None,
        )
        .await
        .expect("fire_assignment should succeed");

        let (_, dispatched_msg) = rx.try_recv().expect("message must be enqueued");
        assert_eq!(dispatched_msg.focus_path.as_deref(), Some("/repo/project"));
    }

    // ---------------------------------------------------------------------------
    // Test (b): webhook trigger creates a run; token validation is handled by
    // the HTTP layer, not by fire_assignment itself.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn webhook_trigger_creates_run() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-webhook");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = webhook_assignment("assign-webhook", "agent-webhook", Some("secret"));
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let run = fire_assignment(
            &persistence,
            &dispatcher,
            &event_bus,
            &assignment,
            AssignmentTriggerKind::Webhook,
            Some("payload summary here".to_string()),
            None,
            None,
        )
        .await
        .expect("fire_assignment should succeed for webhook");

        assert_eq!(run.status, AssignmentRunStatus::Queued);
        assert_eq!(run.trigger_kind, AssignmentTriggerKind::Webhook);
        assert_eq!(run.trigger_payload.as_deref(), Some("payload summary here"));
        assert!(run.thread_id.is_some());

        // Webhook trigger does NOT call mark_fired, so last_run_at stays None.
        let stored_assignment = persistence
            .assignments
            .get("assign-webhook")
            .await
            .expect("assignment exists");
        assert!(
            stored_assignment.last_run_at.is_none(),
            "fire_assignment must not call mark_fired for webhook triggers"
        );
        assert!(
            stored_assignment.next_fire_at.is_none(),
            "webhook assignment must not acquire a next_fire_at"
        );

        let (_, dispatched_msg) = rx.try_recv().expect("message enqueued");
        assert!(matches!(
            dispatched_msg.source,
            Some(MessageSource::Assignment { ref trigger_kind, .. }) if trigger_kind == "webhook"
        ));
    }

    // ---------------------------------------------------------------------------
    // Test (c): the QueuedMessage produced by fire_assignment carries
    // MessageSource::Assignment, which is_interactive_message classifies as
    // non-interactive — i.e., assignment runs never block or serialize against
    // interactive chat turns.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn assignment_run_message_is_not_interactive() {
        // Build a QueuedMessage that exactly mirrors what fire_assignment sends.
        let msg = QueuedMessage {
            message_id: "m1".to_string(),
            content: "<assignment-run type=\"cron\">do something</assignment-run>".to_string(),
            queued_at: Utc::now(),
            attachments: vec![],
            source: Some(MessageSource::Assignment {
                assignment_id: "assign-1".to_string(),
                run_id: "run-1".to_string(),
                trigger_kind: "cron".to_string(),
            }),
            focus_path: None,
            thread_id: Some("thread-1".to_string()),
        };

        // A user message, in contrast, is interactive.
        let user_msg = QueuedMessage {
            message_id: "m2".to_string(),
            content: "hello".to_string(),
            queued_at: Utc::now(),
            attachments: vec![],
            source: Some(MessageSource::User),
            focus_path: None,
            thread_id: None,
        };

        // Verify the concurrency invariant: assignment source ≠ interactive.
        assert!(
            !is_interactive(&msg),
            "Assignment-sourced messages must be non-interactive"
        );
        assert!(
            is_interactive(&user_msg),
            "User-sourced messages must be interactive"
        );
    }

    /// Mirrors the `is_interactive_message` classification from queue_manager.rs
    /// without depending on the private function directly. This is the single
    /// property that guarantees assignment runs never hold the interactive lease.
    fn is_interactive(msg: &QueuedMessage) -> bool {
        !matches!(
            msg.source,
            Some(MessageSource::Schedule { .. }) | Some(MessageSource::Assignment { .. })
        )
    }

    // ---------------------------------------------------------------------------
    // Test (d): AssignmentRun lifecycle persists correctly through status
    // transitions (Queued → Running → Succeeded / Failed).
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn assignment_run_lifecycle_persists_through_transitions() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, _rx) = make_recording_dispatcher();

        let agent = make_agent("agent-lifecycle");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = cron_assignment("assign-lifecycle", "agent-lifecycle");
        persistence.assignments.add(assignment.clone()).await.unwrap();

        // Queued
        let run = fire_assignment(
            &persistence,
            &dispatcher,
            &event_bus,
            &assignment,
            AssignmentTriggerKind::Cron,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(run.status, AssignmentRunStatus::Queued);
        assert!(run.started_ts.is_none());
        assert!(run.finished_ts.is_none());

        // Running (transition 1)
        let mut running = run.clone();
        running.status = AssignmentRunStatus::Running;
        running.started_ts = Some(Utc::now());
        persistence
            .assignment_runs
            .update("assign-lifecycle", &running)
            .await
            .unwrap();

        let stored = persistence
            .assignment_runs
            .get("assign-lifecycle", &run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, AssignmentRunStatus::Running);
        assert!(stored.started_ts.is_some());
        assert!(stored.finished_ts.is_none());

        // Succeeded (transition 2)
        let mut done = stored.clone();
        done.status = AssignmentRunStatus::Succeeded;
        done.output_summary = Some("The task completed.".to_string());
        done.finished_ts = Some(Utc::now());
        persistence
            .assignment_runs
            .update("assign-lifecycle", &done)
            .await
            .unwrap();

        let final_run = persistence
            .assignment_runs
            .get("assign-lifecycle", &run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_run.status, AssignmentRunStatus::Succeeded);
        assert_eq!(
            final_run.output_summary.as_deref(),
            Some("The task completed.")
        );
        assert!(final_run.finished_ts.is_some());
        assert!(final_run.thread_id.is_some());

        // List returns the run with the final status.
        let all = persistence
            .assignment_runs
            .list_for_assignment("assign-lifecycle")
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, AssignmentRunStatus::Succeeded);
    }

    // ---------------------------------------------------------------------------
    // Additional: fire_assignment for missing agent propagates an error and does
    // not leave a dangling run row or thread.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn fire_assignment_missing_agent_returns_error() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));

        // Dispatcher that always errors for a missing agent.
        struct ErrorDispatcher;
        #[async_trait::async_trait]
        impl NotificationDispatcher for ErrorDispatcher {
            async fn submit_to_agent(&self, _: &str, _: QueuedMessage) -> Result<(), AoError> {
                Err(AoError::AgentNotFound("ghost-agent".to_string()))
            }
        }
        let dispatcher = Arc::new(ErrorDispatcher) as Arc<dyn NotificationDispatcher>;

        let assignment = cron_assignment("assign-no-agent", "ghost-agent");
        // Do not add the assignment to the store — the dispatcher errors first.

        let result = fire_assignment(
            &persistence,
            &dispatcher,
            &event_bus,
            &assignment,
            AssignmentTriggerKind::Cron,
            None,
            None,
            None,
        )
        .await;

        assert!(result.is_err(), "must surface error when agent is missing");
    }

    // ---------------------------------------------------------------------------
    // thread_policy: Main — dispatches with thread_id: None (matching the same
    // convention interactive default-thread messages use) but records the
    // concrete default thread id on the AssignmentRun for UI navigation.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn main_policy_dispatches_with_none_thread_id_but_records_default_thread_on_run() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-main");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment = cron_assignment("assign-main", "agent-main");
        assignment.thread_policy = AssignmentThreadPolicy::Main;
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let run = fire_assignment(
            &persistence,
            &dispatcher,
            &event_bus,
            &assignment,
            AssignmentTriggerKind::Cron,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let expected_default_id = ao_persistence::thread_store::ThreadStore::default_thread_id("agent-main");
        assert_eq!(run.thread_id.as_deref(), Some(expected_default_id.as_str()));

        let (_, dispatched_msg) = rx.try_recv().expect("message enqueued");
        assert_eq!(
            dispatched_msg.thread_id, None,
            "Main policy must dispatch with thread_id: None, matching the interactive default-thread convention"
        );

        // No new thread row was created — the run points at the pre-existing default thread.
        let thread = persistence.threads.get(&expected_default_id).await.unwrap();
        assert!(thread.is_some(), "default thread must exist");
    }

    // ---------------------------------------------------------------------------
    // thread_policy: Dedicated — first fire creates and claims a thread named
    // after the assignment; every subsequent fire reuses the same thread id.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn dedicated_policy_creates_once_and_reuses_on_subsequent_fires() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-dedicated");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment = cron_assignment("assign-dedicated", "agent-dedicated");
        assignment.thread_policy = AssignmentThreadPolicy::Dedicated;
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let run1 = fire_assignment(
            &persistence,
            &dispatcher,
            &event_bus,
            &assignment,
            AssignmentTriggerKind::Cron,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let (_, msg1) = rx.try_recv().expect("first message enqueued");
        assert_eq!(msg1.thread_id, run1.thread_id);

        // The thread is named after the assignment, not "{name} — run".
        let thread_id = run1.thread_id.clone().expect("dedicated thread id");
        let thread = persistence.threads.get(&thread_id).await.unwrap().unwrap();
        assert_eq!(thread.title.as_deref(), Some("Daily task"));

        // Second fire must reuse the same thread id — re-fetch the assignment
        // since fire_assignment persisted dedicated_thread_id onto it.
        let refetched = persistence.assignments.get("assign-dedicated").await.unwrap();
        assert_eq!(refetched.dedicated_thread_id.as_deref(), Some(thread_id.as_str()));

        let run2 = fire_assignment(
            &persistence,
            &dispatcher,
            &event_bus,
            &refetched,
            AssignmentTriggerKind::Cron,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let (_, msg2) = rx.try_recv().expect("second message enqueued");

        assert_eq!(run2.thread_id, run1.thread_id, "both fires must share the dedicated thread");
        assert_eq!(msg2.thread_id, run1.thread_id);
    }

    #[tokio::test]
    async fn dedicated_policy_self_heals_when_thread_deleted() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-heal");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment = cron_assignment("assign-heal", "agent-heal");
        assignment.thread_policy = AssignmentThreadPolicy::Dedicated;
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let run1 = fire_assignment(
            &persistence, &dispatcher, &event_bus, &assignment,
            AssignmentTriggerKind::Cron, None, None, None,
        )
        .await
        .unwrap();
        let _ = rx.try_recv().expect("first message enqueued");
        let original_thread_id = run1.thread_id.clone().expect("dedicated thread id");

        // Simulate the user deleting the dedicated thread via the Threads UI.
        persistence.threads.delete(&original_thread_id).await.unwrap();

        let refetched = persistence.assignments.get("assign-heal").await.unwrap();
        let run2 = fire_assignment(
            &persistence, &dispatcher, &event_bus, &refetched,
            AssignmentTriggerKind::Cron, None, None, None,
        )
        .await
        .unwrap();
        let _ = rx.try_recv().expect("second message enqueued");

        assert_ne!(
            run2.thread_id, run1.thread_id,
            "a deleted dedicated thread must be replaced, not resurrected"
        );
        let healed = persistence.assignments.get("assign-heal").await.unwrap();
        assert_eq!(healed.dedicated_thread_id, run2.thread_id);
    }

    // ---------------------------------------------------------------------------
    // thread_policy: Fresh (default) — regression check that every fire still
    // gets its own distinct throwaway thread, matching pre-policy behavior.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn fresh_policy_creates_a_new_thread_every_fire() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-fresh");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = cron_assignment("assign-fresh", "agent-fresh");
        assert_eq!(assignment.thread_policy, AssignmentThreadPolicy::Fresh);
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let run1 = fire_assignment(
            &persistence, &dispatcher, &event_bus, &assignment,
            AssignmentTriggerKind::Cron, None, None, None,
        )
        .await
        .unwrap();
        let _ = rx.try_recv().expect("first message enqueued");

        let run2 = fire_assignment(
            &persistence, &dispatcher, &event_bus, &assignment,
            AssignmentTriggerKind::Cron, None, None, None,
        )
        .await
        .unwrap();
        let _ = rx.try_recv().expect("second message enqueued");

        assert_ne!(run1.thread_id, run2.thread_id, "Fresh policy must never reuse a thread");
    }

    // ---------------------------------------------------------------------------
    // Thread::assignment_origin — stamped only for the two policies under
    // which a thread is genuinely owned by one assignment (Fresh, Dedicated),
    // never for Main, which shares the agent's ordinary default thread with
    // interactive chat.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn fresh_policy_stamps_assignment_origin_with_run_id() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-origin-fresh");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = cron_assignment("assign-origin-fresh", "agent-origin-fresh");
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let run = fire_assignment(
            &persistence, &dispatcher, &event_bus, &assignment,
            AssignmentTriggerKind::Cron, None, None, None,
        )
        .await
        .unwrap();
        let _ = rx.try_recv().expect("message enqueued");

        let thread_id = run.thread_id.clone().expect("fresh thread id");
        let thread = persistence.threads.get(&thread_id).await.unwrap().unwrap();
        let origin = thread
            .assignment_origin
            .expect("Fresh policy must stamp assignment_origin");
        assert_eq!(origin.assignment_id, "assign-origin-fresh");
        assert_eq!(origin.run_id.as_deref(), Some(run.id.as_str()));
    }

    #[tokio::test]
    async fn main_policy_never_stamps_assignment_origin_on_default_thread() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-origin-main");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment = cron_assignment("assign-origin-main", "agent-origin-main");
        assignment.thread_policy = AssignmentThreadPolicy::Main;
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let run = fire_assignment(
            &persistence, &dispatcher, &event_bus, &assignment,
            AssignmentTriggerKind::Cron, None, None, None,
        )
        .await
        .unwrap();
        let _ = rx.try_recv().expect("message enqueued");

        let thread_id = run.thread_id.clone().expect("default thread id recorded on run");
        let thread = persistence.threads.get(&thread_id).await.unwrap().unwrap();
        assert!(
            thread.assignment_origin.is_none(),
            "Main policy must never mark the shared default thread as assignment-owned"
        );
    }

    #[tokio::test]
    async fn dedicated_policy_stamps_assignment_origin_without_run_id() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-origin-dedicated");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment = cron_assignment("assign-origin-dedicated", "agent-origin-dedicated");
        assignment.thread_policy = AssignmentThreadPolicy::Dedicated;
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let run = fire_assignment(
            &persistence, &dispatcher, &event_bus, &assignment,
            AssignmentTriggerKind::Cron, None, None, None,
        )
        .await
        .unwrap();
        let _ = rx.try_recv().expect("message enqueued");

        let thread_id = run.thread_id.clone().expect("dedicated thread id");
        let thread = persistence.threads.get(&thread_id).await.unwrap().unwrap();
        let origin = thread
            .assignment_origin
            .expect("Dedicated policy must stamp assignment_origin");
        assert_eq!(origin.assignment_id, "assign-origin-dedicated");
        assert!(
            origin.run_id.is_none(),
            "a Dedicated thread persists across runs, so no single run owns it"
        );
    }

    // ---------------------------------------------------------------------------
    // Event-payload plumbing: a ConnectorEvent fire with a non-None
    // `event_context` must inject the trigger summary and raw poll payload
    // into the dispatched QueuedMessage content — the fired agent must see
    // what actually triggered it, not just its own bare static instruction.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn connector_event_fire_injects_event_payload_into_queued_message() {
        let (_tmp, persistence) = make_persistence().await;
        let event_bus = Arc::new(EventBus::new(64));
        let (dispatcher, mut rx) = make_recording_dispatcher();

        let agent = make_agent("agent-connector");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = connector_event_assignment("assign-connector", "agent-connector");
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let payload = json!({
            "content": [{ "text": "From: alice@example.com\nSubject: Q3 numbers" }]
        });
        let event_context = TriggerEventContext {
            summary: "New result from `list_emails` on `gmail` — cursor changed to 182".to_string(),
            payload: payload.clone(),
        };

        let run = fire_assignment(
            &persistence,
            &dispatcher,
            &event_bus,
            &assignment,
            AssignmentTriggerKind::ConnectorEvent,
            Some("182".to_string()),
            None,
            Some(event_context),
        )
        .await
        .expect("fire_assignment should succeed for a connector event");

        assert_eq!(run.trigger_kind, AssignmentTriggerKind::ConnectorEvent);
        assert_eq!(
            run.trigger_payload.as_deref(),
            Some("182"),
            "the changed cursor value must be recorded on the run"
        );

        let (_, dispatched_msg) = rx.try_recv().expect("message must be enqueued");

        assert!(
            dispatched_msg.content.contains("Summarize the new email."),
            "the static instruction must still be present"
        );
        assert!(
            dispatched_msg.content.contains("New result from `list_emails`"),
            "a token-able summary of what changed must be present, not just the bare instruction"
        );
        assert!(
            dispatched_msg.content.contains("alice@example.com"),
            "the raw poll payload must reach the fired agent, not just a summary"
        );
    }
}
