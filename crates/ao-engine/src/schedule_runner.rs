use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use ao_engine_tools_core::Registry;
use ao_engine_tools_runner::mcp::{McpError, McpManager, McpServerState, McpServerStatus};
use ao_persistence::assignment_store::EvaluationOutcome;
use ao_persistence::PersistenceLayer;
use ao_protocol::assignment::{
    Assignment, AssignmentTrigger, AssignmentTriggerKind, ConnectorPollSpec, QuiescenceReason,
    TriggerEventContext,
};
use serde_json::{json, Value};

use crate::agent_runner::RunnerDispatcher;
use crate::agent_watch::{
    derive_watch_contract_status, run_agent_watch_tick, AgentWatchDetector, LiveAgentWatchDetector,
};
use crate::assignment_runner::fire_assignment;

/// Minimum poll interval for `ConnectorEvent` assignments. Any smaller
/// configured value is clamped up to this floor before rescheduling, so a
/// misconfigured trigger can never hammer a connector faster than once a
/// minute.
const MIN_POLL_INTERVAL_SECS: u64 = 60;
use crate::event_bus::EventBus;
use crate::queue_manager::{NotificationDispatcher, QueueManagerRegistry};
use crate::sleep_guard::SleepGuard;

/// Fallback sleep guard window (hours) used only if preferences fail to
/// load; `UserPreferences::default()` normally supplies this via
/// `max_sleep_guard_hours`.
const DEFAULT_SLEEP_GUARD_HOURS: f64 = 4.0;

/// Outcome of evaluating one `ConnectorEvent` assignment this tick, returned
/// by [`ScheduleRunner::evaluate_connector_event_assignment`].
///
/// This trigger kind has a wrinkle the `Cron` loop doesn't:
/// [`AssignmentStore::mark_polled`][mp] reschedules `next_fire_at` and
/// advances `last_event_cursor` independently of whether a
/// [`QuiescenceReason`] exists for the tick — two of the six possible
/// non-fire endings (seeding the first cursor baseline, and observing an
/// unchanged one) are polls that legitimately need no reason at all (see
/// `QuiescenceReason`'s own doc for why). So this enum, rather than reusing
/// [`EvaluationOutcome`] directly, names all four shapes a tick can end in;
/// [`ScheduleRunner::tick_connector_events`] is the sole match on it and the
/// sole place any of it gets persisted. The two `Polled*` variants both
/// carry the resulting cursor because a `mark_polled` call is owed either
/// way — only whether a `mark_evaluated` call is *also* owed differs.
///
/// [mp]: ao_persistence::assignment_store::AssignmentStore::mark_polled
///
/// `#[must_use]`: same reasoning as [`EvaluationOutcome`] — dropping this
/// silently skips the `mark_polled`/`mark_evaluated` call the returned
/// variant is owed.
#[must_use]
#[derive(Debug)]
enum ConnectorEventOutcome {
    /// Never reached the poll this tick — expired, not due yet, or the
    /// backing server/handle wasn't reachable. No `mark_polled` call is made
    /// for these (matches this tick's behavior before this reason was
    /// tracked: `next_fire_at` is left untouched so the loop retries again
    /// next second instead of waiting a full poll interval).
    SkippedBeforePoll(QuiescenceReason),
    /// Polled (so the caller owes a `mark_polled` call to reschedule), but
    /// declined to fire for `reason` — the caller owes a `mark_evaluated`
    /// call too. `cursor` mirrors what `mark_polled`'s own `cursor` param
    /// should receive (e.g. `Some` when a fire attempt failed after the
    /// cursor had already changed).
    PolledQuiescent {
        reason: QuiescenceReason,
        cursor: Option<String>,
    },
    /// Polled and rescheduled, with deliberately no [`QuiescenceReason`]:
    /// seeding the first-ever cursor baseline, or observing an unchanged
    /// cursor. Only these two branches may construct this variant — see
    /// `QuiescenceReason`'s own doc for why they're excluded from the closed
    /// reason set.
    PolledNoReasonNeeded { cursor: Option<String> },
    /// Fired this tick. `mark_polled(cursor, fired: true, ..)` already routes
    /// through the same liveness mutation `mark_evaluated(Fired)` would, so
    /// the caller does not also call `mark_evaluated` for this variant.
    Fired { cursor: String },
}

/// Ticks every second and fires cron-triggered assignments whose
/// `next_fire_at` is in the past.
///
/// Historically this runner also fired the now-removed ScheduledTask feature
/// (a separate per-agent reminder concept). Assignments are the sole
/// remaining consumer of the per-second tick.
pub struct ScheduleRunner {
    persistence: Arc<PersistenceLayer>,
    queue_registry: Arc<QueueManagerRegistry>,
    event_bus: Arc<EventBus>,
    /// Used by the `ConnectorEvent` poll loop to resolve live client handles
    /// and connection status for the servers those assignments poll.
    mcp_manager: Arc<McpManager>,
    /// Holds the system (and optionally display) awake when a cron
    /// assignment is due to fire within `max_sleep_guard_hours`, so a
    /// scheduled fire isn't silently delayed until the next time the machine
    /// happens to wake on its own.
    sleep_guard: SleepGuard,
    /// Detector backing the `AgentWatch` detect loop (Tier 2 of the
    /// detection ladder). Defaults to [`LiveAgentWatchDetector`] — kept as a
    /// field rather than constructed fresh per tick so tests can swap in
    /// `agent_watch::ScriptedDetector` to exercise this wiring end-to-end
    /// without a live provider.
    agent_watch_detector: Arc<dyn AgentWatchDetector>,
    /// The same process-wide tool registry `agent_watch_detector` was built
    /// from (`AppState::tools_registry`) — handed straight through to
    /// [`run_agent_watch_tick`] rather than re-derived, so the tick always
    /// sees the same already-authenticated adapters the detector itself
    /// uses.
    tools_registry: Arc<Registry>,
}

impl ScheduleRunner {
    pub fn new(
        persistence: Arc<PersistenceLayer>,
        queue_registry: Arc<QueueManagerRegistry>,
        event_bus: Arc<EventBus>,
        mcp_manager: Arc<McpManager>,
        tools_registry: Arc<Registry>,
        runner_dispatcher: Arc<RunnerDispatcher>,
    ) -> Self {
        let agent_watch_detector = Arc::new(LiveAgentWatchDetector::new(
            Arc::clone(&persistence),
            Arc::clone(&tools_registry),
            runner_dispatcher,
            Arc::clone(&event_bus),
        ));
        Self {
            persistence,
            queue_registry,
            event_bus,
            mcp_manager,
            sleep_guard: SleepGuard::new(DEFAULT_SLEEP_GUARD_HOURS),
            agent_watch_detector,
            tools_registry,
        }
    }

    /// Spawn the runner as a background tokio task. Returns a shutdown sender;
    /// drop it (or send `()`) to stop the loop.
    pub fn run(self) -> watch::Sender<()> {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(());
        info!("ScheduleRunner starting");

        tokio::spawn(async move {
            let mut runner = self;
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        info!("ScheduleRunner shutting down");
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        runner.tick().await;
                    }
                }
            }
        });

        shutdown_tx
    }

    /// Single tick: fire any cron-triggered assignments that are due, and
    /// keep the sleep guard armed for whichever cron assignment is due next.
    async fn tick(&mut self) {
        let prefs = self
            .persistence
            .preferences
            .get()
            .await
            .unwrap_or(None)
            .unwrap_or_default();
        let user_tz = prefs.timezone;

        match prefs.max_sleep_guard_hours {
            Some(hours) => self.sleep_guard.set_window_hours(hours),
            None => self.sleep_guard.set_disabled(true),
        }
        self.sleep_guard.set_keep_display_awake(prefs.keep_display_awake);

        let now = Utc::now();

        // Check cron-triggered assignments and fire any that are due.
        // Assignment runs are dispatched as autonomous (non-interactive) messages
        // via the queue manager.
        let cron_assignments = self.persistence.assignments.list_all_enabled_cron().await;
        let dispatcher = Arc::clone(&self.queue_registry) as Arc<dyn NotificationDispatcher>;

        // Snapshot the nearest not-yet-due fire time before firing this
        // tick's due assignments — a fired assignment's `next_fire_at` is
        // already excluded (it's <= now), so recomputing after firing would
        // just repeat the same answer for the other, still-pending ones.
        let nearest_fire_in = nearest_cron_fire_in(&cron_assignments, now);

        for assignment in &cron_assignments {
            // Type filter, not a quiescence reason: `list_all_enabled_cron`
            // already guarantees this, so it never actually skips anything —
            // kept only so `cron_expr` is available below without an
            // irrefutable-pattern warning.
            let AssignmentTrigger::Cron { cron_expr, .. } = &assignment.trigger else {
                continue;
            };

            let outcome = self
                .evaluate_cron_assignment(assignment, cron_expr, now, user_tz.as_deref(), &dispatcher)
                .await;

            // `Fired` needs no persistence here: `fire_assignment` already
            // routed it through `mark_fired` (Cron-only) inside
            // `evaluate_cron_assignment`, which applies the same
            // `EvaluationOutcome::Fired` mutation `mark_evaluated` would —
            // calling `mark_evaluated` again would double-count
            // `liveness.fire_count`.
            if let EvaluationOutcome::Quiescent(reason) = outcome {
                if let Err(e) = self
                    .persistence
                    .assignments
                    .mark_evaluated(&assignment.id, EvaluationOutcome::Quiescent(reason))
                    .await
                {
                    warn!(assignment_id = %assignment.id, error = %e, "Failed to record cron quiescence reason");
                }
            }
        }

        // Arm (or release) the sleep guard based on the nearest upcoming
        // cron assignment fire time computed above.
        self.sleep_guard.update(nearest_fire_in);

        // Poll connector-event assignments whose interval has elapsed.
        self.tick_connector_events(now, user_tz.as_deref()).await;

        // Evaluate agent-driven watch assignments whose interval has elapsed.
        self.tick_agent_watches(now, user_tz.as_deref()).await;
    }

    /// Evaluates one `Cron` assignment this tick: expiry check, due check,
    /// and — if due — the fire attempt itself. Every exit path returns a
    /// concrete [`EvaluationOutcome`] naming why, so a future branch added
    /// here cannot silently skip without the compiler demanding a
    /// [`QuiescenceReason`] for it (the whole point of this restructure —
    /// see the module-level design note above `ConnectorEventOutcome`).
    /// [`ScheduleRunner::tick`]'s Cron loop is the sole caller, and persists
    /// the `Quiescent` case via `mark_evaluated` immediately after this
    /// returns.
    async fn evaluate_cron_assignment(
        &self,
        assignment: &Assignment,
        cron_expr: &str,
        now: DateTime<Utc>,
        user_tz: Option<&str>,
        dispatcher: &Arc<dyn NotificationDispatcher>,
    ) -> EvaluationOutcome {
        // Skip expired assignments and disable them.
        if let Some(expires_at) = assignment.expires_at {
            if expires_at < now {
                debug!(assignment_id = %assignment.id, "Assignment expired, disabling");
                if let Err(e) = self.disable_assignment(&assignment.id).await {
                    warn!(assignment_id = %assignment.id, error = %e, "Failed to disable expired assignment");
                }
                return EvaluationOutcome::Quiescent(QuiescenceReason::Expired { expires_at });
            }
        }

        let due = assignment.next_fire_at.map(|t| t <= now).unwrap_or(false);
        if !due {
            return EvaluationOutcome::Quiescent(QuiescenceReason::NotDue {
                next_fire_at: assignment.next_fire_at,
            });
        }

        debug!(
            assignment_id = %assignment.id,
            cron_expr = %cron_expr,
            "Firing cron assignment"
        );
        match fire_assignment(
            &self.persistence,
            dispatcher,
            &self.event_bus,
            assignment,
            AssignmentTriggerKind::Cron,
            Some(cron_expr.to_string()),
            user_tz,
            None,
        )
        .await
        {
            Ok(_) => EvaluationOutcome::Fired,
            Err(e) => {
                warn!(
                    assignment_id = %assignment.id,
                    error = %e,
                    "Failed to fire cron assignment"
                );
                EvaluationOutcome::Quiescent(QuiescenceReason::FireFailed { reason: e.to_string() })
            }
        }
    }

    /// Poll every enabled `ConnectorEvent` assignment whose interval has
    /// elapsed and whose backing MCP server is currently connected.
    ///
    /// Semantics (per the Assignments plan):
    /// - Seed-on-first: when `last_event_cursor` is `None`, store the observed
    ///   cursor as the baseline but do **not** fire (avoids replaying the
    ///   entire pre-existing backlog the moment the trigger is created).
    /// - Fire-on-change: on later polls, if the observed cursor differs from
    ///   the stored one, fire and advance the cursor. Unchanged → nothing.
    ///   The fire carries the raw poll result and the changed cursor value
    ///   through to `fire_assignment` as a `TriggerEventContext`, so the fired
    ///   agent receives what actually changed instead of a bare ping.
    /// - Every poll calls `mark_polled` so `next_fire_at` is pushed forward by
    ///   the (floor-clamped) interval, even when nothing fires.
    ///
    /// A server that isn't `Connected` is skipped for this tick (logged, not
    /// errored) so an unauthorized/offline connector never stalls the loop.
    async fn tick_connector_events(&self, now: chrono::DateTime<Utc>, user_tz: Option<&str>) {
        let assignments = self
            .persistence
            .assignments
            .list_all_enabled_connector_event()
            .await;
        if assignments.is_empty() {
            return;
        }

        let statuses = self.mcp_manager.server_statuses().await;
        let dispatcher = Arc::clone(&self.queue_registry) as Arc<dyn NotificationDispatcher>;

        for assignment in &assignments {
            // Type filter, not a quiescence reason — see the identical
            // comment in the Cron loop above.
            let AssignmentTrigger::ConnectorEvent {
                server_name,
                poll,
                poll_interval_secs,
            } = &assignment.trigger
            else {
                continue;
            };

            let interval = (*poll_interval_secs).max(MIN_POLL_INTERVAL_SECS);

            let outcome = self
                .evaluate_connector_event_assignment(
                    assignment,
                    now,
                    server_name,
                    poll,
                    &statuses,
                    user_tz,
                    &dispatcher,
                )
                .await;

            match outcome {
                ConnectorEventOutcome::SkippedBeforePoll(reason) => {
                    if let Err(e) = self
                        .persistence
                        .assignments
                        .mark_evaluated(&assignment.id, EvaluationOutcome::Quiescent(reason))
                        .await
                    {
                        warn!(assignment_id = %assignment.id, error = %e, "Failed to record connector-event quiescence reason");
                    }
                }
                ConnectorEventOutcome::PolledQuiescent { reason, cursor } => {
                    if let Err(e) = self
                        .persistence
                        .assignments
                        .mark_polled(&assignment.id, cursor, false, interval)
                        .await
                    {
                        warn!(assignment_id = %assignment.id, error = %e, "Failed to record connector poll");
                    }
                    if let Err(e) = self
                        .persistence
                        .assignments
                        .mark_evaluated(&assignment.id, EvaluationOutcome::Quiescent(reason))
                        .await
                    {
                        warn!(assignment_id = %assignment.id, error = %e, "Failed to record connector-event quiescence reason");
                    }
                }
                ConnectorEventOutcome::PolledNoReasonNeeded { cursor } => {
                    if let Err(e) = self
                        .persistence
                        .assignments
                        .mark_polled(&assignment.id, cursor, false, interval)
                        .await
                    {
                        warn!(assignment_id = %assignment.id, error = %e, "Failed to record connector poll");
                    }
                }
                ConnectorEventOutcome::Fired { cursor } => {
                    if let Err(e) = self
                        .persistence
                        .assignments
                        .mark_polled(&assignment.id, Some(cursor), true, interval)
                        .await
                    {
                        warn!(assignment_id = %assignment.id, error = %e, "Failed to advance connector cursor");
                    }
                }
            }
        }
    }

    /// Evaluates one `ConnectorEvent` assignment this tick: expiry check,
    /// due check, connectivity/handle checks, the poll call itself, and (on
    /// a changed cursor) the fire attempt. Every exit path returns a
    /// concrete [`ConnectorEventOutcome`], so a future branch added here
    /// cannot silently skip without the compiler demanding either a
    /// [`QuiescenceReason`] or a deliberate, visibly-named
    /// `PolledNoReasonNeeded` construction. [`Self::tick_connector_events`]
    /// is the sole caller and the sole place any of these outcomes gets
    /// persisted (`mark_polled` and/or `mark_evaluated`).
    #[allow(clippy::too_many_arguments)]
    async fn evaluate_connector_event_assignment(
        &self,
        assignment: &Assignment,
        now: chrono::DateTime<Utc>,
        server_name: &str,
        poll: &ConnectorPollSpec,
        statuses: &[McpServerStatus],
        user_tz: Option<&str>,
        dispatcher: &Arc<dyn NotificationDispatcher>,
    ) -> ConnectorEventOutcome {
        // Skip and disable expired assignments, mirroring the cron path.
        if let Some(expires_at) = assignment.expires_at {
            if expires_at < now {
                debug!(assignment_id = %assignment.id, "Connector-event assignment expired, disabling");
                if let Err(e) = self.disable_assignment(&assignment.id).await {
                    warn!(assignment_id = %assignment.id, error = %e, "Failed to disable expired assignment");
                }
                return ConnectorEventOutcome::SkippedBeforePoll(QuiescenceReason::Expired { expires_at });
            }
        }

        // Respect the poll schedule: a freshly created assignment has no
        // `next_fire_at`, so treat `None` as "due now" for the first poll.
        let due = assignment.next_fire_at.map(|t| t <= now).unwrap_or(true);
        if !due {
            return ConnectorEventOutcome::SkippedBeforePoll(QuiescenceReason::NotDue {
                next_fire_at: assignment.next_fire_at,
            });
        }

        // Only poll servers that are live right now.
        let server_status = statuses.iter().find(|s| s.name == *server_name);
        let connected = matches!(server_status, Some(s) if s.state == McpServerState::Connected);
        if !connected {
            debug!(
                assignment_id = %assignment.id,
                server = %server_name,
                "Connector server not connected; skipping poll this tick"
            );
            return ConnectorEventOutcome::SkippedBeforePoll(QuiescenceReason::ServerNotConnected {
                server: server_name.to_string(),
                state: server_status.map(|s| format!("{:?}", s.state)),
            });
        }

        let Some(handle) = self.mcp_manager.client_handle(server_name).await else {
            debug!(
                assignment_id = %assignment.id,
                server = %server_name,
                "No live MCP handle; skipping poll this tick"
            );
            return ConnectorEventOutcome::SkippedBeforePoll(QuiescenceReason::NoLiveHandle {
                server: server_name.to_string(),
            });
        };

        let params = json!({
            "name": poll.tool_name,
            "arguments": poll.arguments,
        });
        let poll_result = handle.call("tools/call", params).await;

        self.interpret_connector_poll_result(assignment, server_name, poll, poll_result, user_tz, dispatcher)
            .await
    }

    /// Interprets the result of one `ConnectorEvent` poll call — already
    /// obtained by [`Self::evaluate_connector_event_assignment`], the only
    /// production caller — into a [`ConnectorEventOutcome`].
    ///
    /// Split out from the rest of the evaluation specifically so
    /// `PollFailed` and `CursorUnresolved` are unit-testable without a live
    /// MCP server: `poll_result` is a plain `Result<Value, McpError>`, so a
    /// test can hand-construct an `Err(McpError::CallError { .. })` or an
    /// `Ok(value)` whose `cursor_path` deliberately doesn't resolve, without
    /// ever needing to spawn a real connector process.
    async fn interpret_connector_poll_result(
        &self,
        assignment: &Assignment,
        server_name: &str,
        poll: &ConnectorPollSpec,
        poll_result: Result<Value, McpError>,
        user_tz: Option<&str>,
        dispatcher: &Arc<dyn NotificationDispatcher>,
    ) -> ConnectorEventOutcome {
        let result = match poll_result {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    assignment_id = %assignment.id,
                    server = %server_name,
                    error = %e,
                    "Connector poll call failed; will retry next interval"
                );
                return ConnectorEventOutcome::PolledQuiescent {
                    reason: QuiescenceReason::PollFailed {
                        server: server_name.to_string(),
                        reason: e.to_string(),
                    },
                    cursor: None,
                };
            }
        };

        let observed = poll
            .cursor_path
            .as_deref()
            .and_then(|path| extract_cursor(&result, path));

        match observed {
            None => {
                // Path didn't resolve (or none configured): record the poll
                // but neither advance the cursor nor fire.
                debug!(
                    assignment_id = %assignment.id,
                    "Connector poll produced no cursor; rescheduling without firing"
                );
                ConnectorEventOutcome::PolledQuiescent {
                    reason: QuiescenceReason::CursorUnresolved {
                        server: server_name.to_string(),
                    },
                    cursor: None,
                }
            }
            Some(cursor) => match &assignment.last_event_cursor {
                // Seed-on-first: store the baseline, do not fire. No
                // `QuiescenceReason` — see `ConnectorEventOutcome`'s doc.
                None => {
                    debug!(
                        assignment_id = %assignment.id,
                        "Seeding connector cursor baseline; no fire on first poll"
                    );
                    ConnectorEventOutcome::PolledNoReasonNeeded { cursor: Some(cursor) }
                }
                // Unchanged: just reschedule. No `QuiescenceReason` either —
                // same reasoning as seed-on-first.
                Some(prev) if *prev == cursor => ConnectorEventOutcome::PolledNoReasonNeeded { cursor: None },
                // Fire-on-change: fire, then advance the cursor.
                Some(_) => {
                    debug!(
                        assignment_id = %assignment.id,
                        "Connector cursor changed; firing assignment"
                    );
                    // Thread the poll result + the changed cursor value
                    // through to the fired agent instead of the old
                    // `None` — without this the agent has zero data
                    // about what actually triggered it.
                    let event_context = TriggerEventContext {
                        summary: format!(
                            "Connector event: `{}` on `{}` — cursor changed to `{}`",
                            poll.tool_name, server_name, cursor
                        ),
                        payload: result,
                    };
                    match fire_assignment(
                        &self.persistence,
                        dispatcher,
                        &self.event_bus,
                        assignment,
                        AssignmentTriggerKind::ConnectorEvent,
                        Some(cursor.clone()),
                        user_tz,
                        Some(event_context),
                    )
                    .await
                    {
                        Ok(_) => ConnectorEventOutcome::Fired { cursor },
                        Err(e) => {
                            warn!(
                                assignment_id = %assignment.id,
                                error = %e,
                                "Failed to fire connector assignment"
                            );
                            ConnectorEventOutcome::PolledQuiescent {
                                reason: QuiescenceReason::FireFailed { reason: e.to_string() },
                                cursor: Some(cursor),
                            }
                        }
                    }
                }
            },
        }
    }

    /// Evaluate every enabled `AgentWatch` assignment whose detect-loop
    /// interval has elapsed (Tier 2 of the detection ladder).
    ///
    /// The fire-vs-quiet decision itself is entirely owned by
    /// [`run_agent_watch_tick`] (it loads/diffs/persists the
    /// `state_scratchpad` and fires when warranted); this loop is only
    /// responsible for the same scheduling concerns `tick_connector_events`
    /// already handles for `ConnectorEvent` — skip if not due yet, disable
    /// if expired, and always reschedule via `mark_polled` afterward
    /// (floor-clamped to [`MIN_POLL_INTERVAL_SECS`], same cost-governance
    /// convention as the connector-event poll). Delegates each assignment's
    /// evaluation to [`Self::evaluate_agent_watch_assignment`], mirroring the
    /// Cron and ConnectorEvent loops above: every exit path there returns a
    /// concrete [`EvaluationOutcome`], so this loop can no longer silently
    /// `continue` past a tick without recording why.
    async fn tick_agent_watches(&self, now: chrono::DateTime<Utc>, user_tz: Option<&str>) {
        let assignments = self
            .persistence
            .assignments
            .list_all_enabled_agent_watch()
            .await;
        if assignments.is_empty() {
            return;
        }

        info!(
            assignment_count = assignments.len(),
            "agent watch: tick start — evaluating enabled AgentWatch assignments"
        );

        let dispatcher = Arc::clone(&self.queue_registry) as Arc<dyn NotificationDispatcher>;

        for assignment in &assignments {
            // Type filter, not a quiescence reason: `list_all_enabled_agent_watch`
            // already guarantees this, so it never actually skips anything —
            // kept only so `instruction`/`poll_interval_secs`/`connector_scope`
            // are available below without an irrefutable-pattern warning.
            // Matches the identical comment in the Cron and ConnectorEvent
            // loops above.
            let AssignmentTrigger::AgentWatch {
                instruction,
                poll_interval_secs,
                connector_scope,
                ..
            } = &assignment.trigger
            else {
                continue;
            };

            let interval = (*poll_interval_secs).max(MIN_POLL_INTERVAL_SECS);

            let outcome = self
                .evaluate_agent_watch_assignment(
                    assignment,
                    instruction,
                    connector_scope.as_deref(),
                    now,
                    interval,
                    user_tz,
                    &dispatcher,
                )
                .await;

            // `Fired` needs no persistence here: the due-path `mark_polled(..,
            // fired: true, ..)` call inside `evaluate_agent_watch_assignment`
            // already applies the same `EvaluationOutcome::Fired` mutation
            // `mark_evaluated` would — calling `mark_evaluated` again would
            // double-count `liveness.fire_count`. Same pattern as the Cron
            // loop's identical comment above.
            if let EvaluationOutcome::Quiescent(reason) = outcome {
                if let Err(e) = self
                    .persistence
                    .assignments
                    .mark_evaluated(&assignment.id, EvaluationOutcome::Quiescent(reason))
                    .await
                {
                    warn!(assignment_id = %assignment.id, error = %e, "agent watch: failed to record liveness reason");
                }
            }
        }
    }

    /// Evaluates one `AgentWatch` assignment this tick: expiry check, due
    /// check, and — if due — the detect-and-maybe-fire tick itself
    /// (delegated to [`run_agent_watch_tick`]) plus the interval reschedule
    /// every due tick owes via `mark_polled`. Every exit path returns a
    /// concrete [`EvaluationOutcome`], so a future branch added here cannot
    /// silently skip without the compiler demanding a [`QuiescenceReason`]
    /// for it — the same contract [`Self::evaluate_cron_assignment`] and
    /// [`Self::evaluate_connector_event_assignment`] already give their own
    /// loops.
    ///
    /// Unlike `evaluate_connector_event_assignment` (which returns a
    /// dedicated [`ConnectorEventOutcome`] so its caller can decide whether a
    /// `mark_polled` call is owed, keyed off a `cursor` this trigger kind
    /// doesn't have), this function owns its own `mark_polled` call directly:
    /// `AgentWatch` always calls it with `cursor: None`, so there is nothing
    /// an intermediate outcome type would buy here that plain
    /// [`EvaluationOutcome`] doesn't already express. `mark_polled` is called
    /// exactly once, on the due path, whether or not that poll fires —
    /// mirroring how `evaluate_cron_assignment` owns its own `fire_assignment`
    /// call rather than surfacing it back to [`Self::tick`].
    ///
    /// [`Self::tick_agent_watches`] is the sole caller, and persists the
    /// `Quiescent` case via `mark_evaluated` immediately after this returns —
    /// see its own comment for why the `Fired` case needs no additional call.
    #[allow(clippy::too_many_arguments)]
    async fn evaluate_agent_watch_assignment(
        &self,
        assignment: &Assignment,
        instruction: &str,
        connector_scope: Option<&str>,
        now: chrono::DateTime<Utc>,
        interval: u64,
        user_tz: Option<&str>,
        dispatcher: &Arc<dyn NotificationDispatcher>,
    ) -> EvaluationOutcome {
        // Skip and disable expired assignments, mirroring the cron and
        // connector-event paths.
        if let Some(expires_at) = assignment.expires_at {
            if expires_at < now {
                info!(
                    assignment_id = %assignment.id,
                    assignment_name = %assignment.name,
                    "agent watch: assignment expired, disabling"
                );
                if let Err(e) = self.disable_assignment(&assignment.id).await {
                    warn!(assignment_id = %assignment.id, error = %e, "agent watch: failed to disable expired assignment");
                }
                return EvaluationOutcome::Quiescent(QuiescenceReason::Expired { expires_at });
            }
        }

        // Respect the poll schedule: a freshly created assignment has no
        // `next_fire_at`, so treat `None` as "due now" for the first poll.
        let due = assignment.next_fire_at.map(|t| t <= now).unwrap_or(true);
        let seconds_until_due = assignment.next_fire_at.map(|t| (t - now).num_seconds());

        info!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            connector_scope = ?connector_scope,
            next_fire_at = ?assignment.next_fire_at,
            seconds_until_due = ?seconds_until_due,
            due,
            "agent watch: evaluating"
        );

        if !due {
            info!(
                assignment_id = %assignment.id,
                assignment_name = %assignment.name,
                seconds_remaining = seconds_until_due.unwrap_or(0),
                "agent watch: skipped — poll interval not yet elapsed"
            );
            return EvaluationOutcome::Quiescent(QuiescenceReason::NotDue {
                next_fire_at: assignment.next_fire_at,
            });
        }

        let fired = run_agent_watch_tick(
            &self.persistence,
            dispatcher,
            &self.event_bus,
            &self.agent_watch_detector,
            &self.tools_registry,
            assignment,
            instruction,
            user_tz,
        )
        .await;

        if let Err(e) = self
            .persistence
            .assignments
            .mark_polled(&assignment.id, None, fired, interval)
            .await
        {
            warn!(assignment_id = %assignment.id, error = %e, "agent watch: failed to reschedule assignment");
        }

        if fired {
            // DELIBERATE: the `mark_polled(.., fired: true, ..)` call just
            // above already routed through the same `apply_evaluation(Fired)`
            // mutation `mark_evaluated` would — returning `Fired` here
            // (instead of persisting a second time) is what keeps the caller
            // from double-counting `liveness.fire_count`. Do not "fix" this
            // by adding another `mark_evaluated` call for the fired case.
            return EvaluationOutcome::Fired;
        }

        let contract = match &assignment.trigger {
            AssignmentTrigger::AgentWatch { contract, .. } => contract.as_ref(),
            _ => None,
        };
        let scratchpad = self
            .persistence
            .assignment_scratchpads
            .get(&assignment.id)
            .await
            .unwrap_or(None);
        let status = derive_watch_contract_status(contract, scratchpad.as_ref());
        EvaluationOutcome::Quiescent(QuiescenceReason::AgentWatchContractNotBound(status))
    }

    async fn disable_assignment(&self, assignment_id: &str) -> Result<(), ao_protocol::error::AoError> {
        if let Some(mut assignment) = self.persistence.assignments.get(assignment_id).await {
            assignment.enabled = false;
            self.persistence.assignments.update(assignment).await
        } else {
            Ok(())
        }
    }
}

/// How long until the soonest not-yet-due cron assignment in `assignments`
/// fires, or `None` if none are pending. Assignments that are already due
/// (`next_fire_at <= now`), have no `next_fire_at`, are expired, or use a
/// non-`Cron` trigger are excluded — the first two are handled by immediate
/// firing in `tick()` rather than by holding the guard, and an expired
/// assignment is disabled this tick and will never fire.
///
/// Pulled out as a pure function (no `self`, no I/O) so the sleep-guard
/// arming decision — an OS-level power assertion that's impractical to
/// assert on directly in a unit test — can still be exercised deterministically
/// by testing the input it's driven by.
fn nearest_cron_fire_in(
    assignments: &[ao_protocol::assignment::Assignment],
    now: chrono::DateTime<Utc>,
) -> Option<Duration> {
    assignments
        .iter()
        .filter(|a| matches!(a.trigger, AssignmentTrigger::Cron { .. }))
        .filter(|a| a.expires_at.map(|exp| exp >= now).unwrap_or(true))
        .filter_map(|a| a.next_fire_at)
        .filter(|fire_at| *fire_at > now)
        .map(|fire_at| (fire_at - now).to_std().unwrap_or(Duration::from_secs(0)))
        .min()
}

/// Walk `value` along a dot-separated `path` and stringify the leaf into a
/// stable cursor key.
///
/// Each segment indexes an object key, or — when it parses as a non-negative
/// integer — an array element (so paths like `content.0.text` work against an
/// MCP tool result). Returns `None` if any segment fails to resolve or the
/// leaf is `null`, which the poll loop treats as "no cursor this poll" (no
/// fire, no advance). A string leaf is returned unquoted; any other scalar or
/// compound leaf is rendered via its JSON encoding so numeric or structured
/// cursors still compare stably across polls.
fn extract_cursor(value: &Value, path: &str) -> Option<String> {
    let mut current = value;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(arr) => {
                let idx: usize = segment.parse().ok()?;
                arr.get(idx)?
            }
            _ => return None,
        };
    }
    match current {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

// TODO: Add PID-based lock file for multi-instance support

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use ao_persistence::paths::DataRoot;
    use ao_protocol::agent::{AgentProfile, CliProviderConfig, InputMode, OutputFormat, ProviderConfig};

    async fn make_persistence() -> (tempfile::TempDir, Arc<PersistenceLayer>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data_root = DataRoot::new(tmp.path());
        let layer = PersistenceLayer::init_with_root(data_root)
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
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
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
            max_turns: None,
        }
    }

    /// Build a full `ScheduleRunner` with a mocked CLI runner (no scenarios —
    /// nothing under test here is expected to actually dispatch a run) so
    /// `tick()` can be exercised end-to-end against a real
    /// `QueueManagerRegistry` instance.
    async fn make_schedule_runner(persistence: &Arc<PersistenceLayer>) -> ScheduleRunner {
        let event_bus = Arc::new(EventBus::new(64));
        let mcp_manager = Arc::new(
            ao_engine_tools_runner::mcp::McpManager::from_config(
                &ao_engine_tools_provider_config::McpServersConfig { servers: vec![] },
            )
            .await,
        );
        let mock_supervisor: Arc<dyn ao_process::supervisor::ProcessSupervisor> =
            Arc::new(ao_process::mock::MockProcessSupervisor::new(vec![]));
        let normalizer_registry = Arc::new(ao_normalizer::registry::NormalizerRegistry::new());
        let command_queue = Arc::new(crate::command_queue::CommandQueue::new());
        let instance_registry = Arc::new(crate::instance_registry::InstanceRegistry::new());
        let running_agents = Arc::new(crate::agent_runner::RunningAgents::new());
        let cli_runner = Arc::new(crate::agent_runner::CliAgentRunner::new(
            mock_supervisor,
            normalizer_registry,
            Arc::clone(&event_bus),
            Arc::clone(persistence),
            command_queue,
            Arc::clone(&instance_registry),
            running_agents,
            Arc::new(ao_engine_tools_core::Registry::new()),
        ));
        let dispatcher = Arc::new(crate::agent_runner::RunnerDispatcher::with_runners(
            cli_runner.clone() as Arc<dyn crate::agent_runner::AgentRunner>,
            cli_runner.clone() as Arc<dyn crate::agent_runner::AgentRunner>,
        ));
        let queue_registry = Arc::new(crate::queue_manager::QueueManagerRegistry::new(
            Arc::clone(&dispatcher),
            Arc::clone(&instance_registry),
            Arc::clone(&event_bus),
            Arc::clone(persistence),
        ));
        ScheduleRunner::new(
            Arc::clone(persistence),
            queue_registry,
            event_bus,
            mcp_manager,
            Arc::new(ao_engine_tools_core::Registry::new()),
            dispatcher,
        )
    }

    fn make_expired_cron_assignment(id: &str, agent_id: &str) -> ao_protocol::assignment::Assignment {
        use ao_protocol::assignment::{AssignmentThreadPolicy, AssignmentTrigger, OutputMode};

        let now = Utc::now();
        ao_protocol::assignment::Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: "Expired assignment".to_string(),
            instruction: "do the thing".to_string(),
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
            expires_at: Some(now - chrono::Duration::hours(1)),
            next_fire_at: Some(now - chrono::Duration::minutes(1)),
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    #[tokio::test]
    async fn tick_disables_cron_assignment_past_expires_at() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-assignment-expired");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = make_expired_cron_assignment("assign-expired", "agent-assignment-expired");
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        let after = persistence.assignments.get("assign-expired").await.unwrap();
        assert!(!after.enabled, "an assignment past its expires_at must be disabled by the tick");
        assert!(
            after.last_run_at.is_none(),
            "an expired assignment must be skipped, not fired"
        );
        assert_eq!(
            after.liveness.last_quiescence,
            Some(QuiescenceReason::Expired {
                expires_at: assignment.expires_at.unwrap()
            }),
            "an expired cron assignment must persist why it didn't fire"
        );
        assert!(
            after.liveness.last_evaluated_at.is_some(),
            "a tick that evaluates an expired assignment must still count as an evaluation"
        );
    }

    #[tokio::test]
    async fn tick_fires_cron_assignment_with_future_expires_at() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-assignment-not-expired");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment =
            make_expired_cron_assignment("assign-not-expired", "agent-assignment-not-expired");
        assignment.expires_at = Some(Utc::now() + chrono::Duration::hours(1));
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        let after = persistence.assignments.get("assign-not-expired").await.unwrap();
        assert!(after.enabled, "an assignment with a future expires_at must remain enabled");
        assert!(
            after.last_run_at.is_some(),
            "a due, non-expired cron assignment must still fire on tick"
        );
        assert_eq!(after.liveness.fire_count, 1, "a real fire must increment fire_count");
        assert!(
            after.liveness.last_quiescence.is_none(),
            "a fire must clear any previously recorded quiescence (there is none here, but this is\
             the same assertion the mixed-sequence persistence test makes end to end through tick())"
        );
        assert!(after.liveness.last_evaluated_at.is_some());
    }

    #[tokio::test]
    async fn tick_cron_not_due_persists_not_due_reason() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-assignment-not-due");
        persistence.agents.create(&agent).await.unwrap();

        let next_fire_at = Utc::now() + chrono::Duration::hours(1);
        let assignment = make_cron_assignment_at(
            "assign-not-due",
            "agent-assignment-not-due",
            Some(next_fire_at),
            None,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        let after = persistence.assignments.get("assign-not-due").await.unwrap();
        assert!(after.last_run_at.is_none(), "a not-yet-due cron assignment must not fire");
        assert_eq!(
            after.liveness.last_quiescence,
            Some(QuiescenceReason::NotDue {
                next_fire_at: Some(next_fire_at)
            }),
            "a not-yet-due cron assignment must persist why it didn't fire — this is the branch \
             that had NO logging at all before this task"
        );
        assert!(after.liveness.last_evaluated_at.is_some());
    }

    #[tokio::test]
    async fn tick_cron_fire_failure_persists_fire_failed_reason() {
        let (_tmp, persistence) = make_persistence().await;
        // Deliberately never create this agent: `fire_assignment`'s dispatch
        // step (`QueueManagerRegistry::submit_message_to_agent_id`) errors
        // with a genuine (not mocked) `AgentNotFound`, giving a real
        // fire-attempt failure to route through `FireFailed`.
        let mut assignment = make_expired_cron_assignment("assign-fire-fails", "ghost-agent");
        assignment.expires_at = None;
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        let after = persistence.assignments.get("assign-fire-fails").await.unwrap();
        assert!(
            after.last_run_at.is_none(),
            "a fire attempt that errors must not count as a fire"
        );
        match after.liveness.last_quiescence {
            Some(QuiescenceReason::FireFailed { .. }) => {}
            other => panic!("expected FireFailed, got {other:?}"),
        }
        assert!(after.liveness.last_evaluated_at.is_some());
    }

    #[tokio::test]
    async fn tick_advances_last_evaluated_at_on_a_non_firing_cron_tick() {
        // The regression this guards against: before this task, an
        // assignment that didn't fire (not-due, expired, or fire-failed)
        // left `liveness.last_evaluated_at` completely untouched — there was
        // no signal at all that the tick loop had even looked at it. This is
        // the case that matters most because it was invisible: an
        // assignment could look "stuck" with no way to tell "being
        // evaluated and declining to fire" apart from "never evaluated at
        // all".
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-liveness-regression");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = make_cron_assignment_at(
            "assign-liveness-regression",
            "agent-liveness-regression",
            Some(Utc::now() + chrono::Duration::hours(1)), // not due
            None,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();
        assert!(
            assignment.liveness.last_evaluated_at.is_none(),
            "sanity: a freshly added assignment starts unevaluated"
        );

        let before_tick = Utc::now();
        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        let after = persistence.assignments.get("assign-liveness-regression").await.unwrap();
        assert!(after.last_run_at.is_none(), "sanity: this tick must not have fired");
        let last_evaluated_at = after
            .liveness
            .last_evaluated_at
            .expect("a tick that evaluates a non-firing assignment must still advance last_evaluated_at");
        assert!(
            last_evaluated_at >= before_tick,
            "last_evaluated_at must reflect this tick, not a stale earlier value"
        );
    }

    // ---------------------------------------------------------------------------
    // `tick_connector_events` — one test per `EXACT SITES` branch, mirroring
    // the Cron tests above. `NoLiveHandle`, `PollFailed`, and
    // `CursorUnresolved` call `evaluate_connector_event_assignment` /
    // `interpret_connector_poll_result` directly rather than going through a
    // full `tick()` — those three need either a status/handle disagreement
    // or a live poll result that a real (but serverless-in-tests)
    // `McpManager` cannot produce without spawning a real connector process,
    // which `cargo test -p ao-engine --lib` cannot rely on being built (it
    // is a `[[bin]]` target of this same crate but `--lib` does not build
    // sibling targets — confirmed empirically while writing these tests).
    // Persistence is still verified in every case, using the exact same
    // `mark_polled`/`mark_evaluated` calls `tick_connector_events` itself
    // makes.
    // ---------------------------------------------------------------------------

    fn make_connector_event_assignment(
        id: &str,
        agent_id: &str,
        server_name: &str,
        cursor_path: Option<&str>,
    ) -> ao_protocol::assignment::Assignment {
        use ao_protocol::assignment::{AssignmentThreadPolicy, AssignmentTrigger, ConnectorPollSpec, OutputMode};

        let now = Utc::now();
        ao_protocol::assignment::Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: "Connector watcher".to_string(),
            instruction: "Summarize the new event.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::ConnectorEvent {
                server_name: server_name.to_string(),
                poll: ConnectorPollSpec {
                    tool_name: "poll".to_string(),
                    arguments: serde_json::json!({}),
                    cursor_path: cursor_path.map(|p| p.to_string()),
                },
                poll_interval_secs: 300,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: Some(now),
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    #[tokio::test]
    async fn tick_connector_event_expired_persists_expired_reason() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-ce-expired");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment =
            make_connector_event_assignment("ce-expired", "agent-ce-expired", "some-server", None);
        let expires_at = Utc::now() - chrono::Duration::hours(1);
        assignment.expires_at = Some(expires_at);
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        let after = persistence.assignments.get("ce-expired").await.unwrap();
        assert!(!after.enabled, "an expired connector-event assignment must be disabled");
        assert_eq!(
            after.liveness.last_quiescence,
            Some(QuiescenceReason::Expired { expires_at })
        );
        assert!(after.liveness.last_evaluated_at.is_some());
    }

    #[tokio::test]
    async fn tick_connector_event_not_due_persists_not_due_reason() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-ce-not-due");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment =
            make_connector_event_assignment("ce-not-due", "agent-ce-not-due", "some-server", None);
        let next_fire_at = Utc::now() + chrono::Duration::hours(1);
        assignment.next_fire_at = Some(next_fire_at);
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        let after = persistence.assignments.get("ce-not-due").await.unwrap();
        assert_eq!(
            after.liveness.last_quiescence,
            Some(QuiescenceReason::NotDue {
                next_fire_at: Some(next_fire_at)
            })
        );
        assert!(after.liveness.last_evaluated_at.is_some());
    }

    #[tokio::test]
    async fn tick_connector_event_server_not_connected_persists_reason() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-ce-not-connected");
        persistence.agents.create(&agent).await.unwrap();

        // `make_schedule_runner` builds its `McpManager` from an empty
        // config, so "nonexistent-server" never appears in
        // `server_statuses()` — exactly the "not registered at all" shape
        // of `ServerNotConnected`.
        let assignment = make_connector_event_assignment(
            "ce-not-connected",
            "agent-ce-not-connected",
            "nonexistent-server",
            None,
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        let after = persistence.assignments.get("ce-not-connected").await.unwrap();
        assert_eq!(
            after.liveness.last_quiescence,
            Some(QuiescenceReason::ServerNotConnected {
                server: "nonexistent-server".to_string(),
                state: None,
            })
        );
        assert!(after.liveness.last_evaluated_at.is_some());
        assert_eq!(
            after.next_fire_at, assignment.next_fire_at,
            "SkippedBeforePoll must not call mark_polled — next_fire_at is untouched, exactly as \
             before this task, so the tick retries again next second rather than waiting a full \
             poll interval"
        );
    }

    #[tokio::test]
    async fn evaluate_connector_event_no_live_handle_persists_reason() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-ce-no-handle");
        persistence.agents.create(&agent).await.unwrap();

        let assignment =
            make_connector_event_assignment("ce-no-handle", "agent-ce-no-handle", "phantom-server", None);
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let runner = make_schedule_runner(&persistence).await;
        // A hand-built status claiming "phantom-server" is Connected — the
        // real `McpManager` (empty config) has no client for it, so
        // `client_handle` returns `None` regardless. This is the one
        // branch that genuinely requires the manager's status cache and
        // its handle table to disagree (see `NoLiveHandle`'s own doc) —
        // calling `evaluate_connector_event_assignment` directly lets the
        // test manufacture that disagreement without a live connector.
        let statuses = vec![McpServerStatus {
            name: "phantom-server".to_string(),
            transport: "stdio".to_string(),
            endpoint: "n/a".to_string(),
            state: McpServerState::Connected,
            error: None,
            tool_names: vec![],
            source: "config".to_string(),
        }];
        let dispatcher = Arc::clone(&runner.queue_registry) as Arc<dyn NotificationDispatcher>;
        let AssignmentTrigger::ConnectorEvent { server_name, poll, .. } = &assignment.trigger else {
            unreachable!()
        };

        let outcome = runner
            .evaluate_connector_event_assignment(
                &assignment,
                Utc::now(),
                server_name,
                poll,
                &statuses,
                None,
                &dispatcher,
            )
            .await;

        let ConnectorEventOutcome::SkippedBeforePoll(reason) = outcome else {
            panic!("expected SkippedBeforePoll, got {outcome:?}");
        };
        assert!(matches!(&reason, QuiescenceReason::NoLiveHandle { server } if server == "phantom-server"));

        // Persist it exactly the way `tick_connector_events` would.
        persistence
            .assignments
            .mark_evaluated(&assignment.id, EvaluationOutcome::Quiescent(reason))
            .await
            .unwrap();
        let after = persistence.assignments.get("ce-no-handle").await.unwrap();
        assert!(matches!(
            after.liveness.last_quiescence,
            Some(QuiescenceReason::NoLiveHandle { ref server }) if server == "phantom-server"
        ));
        assert!(after.liveness.last_evaluated_at.is_some());
    }

    #[tokio::test]
    async fn interpret_connector_poll_result_call_error_persists_poll_failed_reason() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-ce-poll-failed");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = make_connector_event_assignment(
            "ce-poll-failed",
            "agent-ce-poll-failed",
            "flaky-server",
            Some("content.0.text"),
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let runner = make_schedule_runner(&persistence).await;
        let dispatcher = Arc::clone(&runner.queue_registry) as Arc<dyn NotificationDispatcher>;
        let AssignmentTrigger::ConnectorEvent { poll, .. } = &assignment.trigger else {
            unreachable!()
        };

        let poll_result: Result<Value, McpError> = Err(McpError::CallError {
            code: -32603,
            message: "boom".to_string(),
            data: None,
        });

        let outcome = runner
            .interpret_connector_poll_result(&assignment, "flaky-server", poll, poll_result, None, &dispatcher)
            .await;

        let ConnectorEventOutcome::PolledQuiescent { reason, cursor } = outcome else {
            panic!("expected PolledQuiescent, got {outcome:?}");
        };
        assert!(cursor.is_none(), "a failed poll must not advance the cursor");
        assert!(matches!(&reason, QuiescenceReason::PollFailed { server, .. } if server == "flaky-server"));

        // Caller-side persistence, mirroring `tick_connector_events`'s
        // `PolledQuiescent` arm exactly: both `mark_polled` (the fact of
        // polling) and `mark_evaluated` (the reason) — this is the
        // reason-vs-fact split the task called out at schedule_runner.rs's
        // original 288-307.
        persistence
            .assignments
            .mark_polled(&assignment.id, cursor.clone(), false, 300)
            .await
            .unwrap();
        persistence
            .assignments
            .mark_evaluated(&assignment.id, EvaluationOutcome::Quiescent(reason))
            .await
            .unwrap();

        let after = persistence.assignments.get("ce-poll-failed").await.unwrap();
        assert!(matches!(
            after.liveness.last_quiescence,
            Some(QuiescenceReason::PollFailed { ref server, .. }) if server == "flaky-server"
        ));
        assert!(after.liveness.last_evaluated_at.is_some());
        assert!(
            after.next_fire_at.unwrap() > assignment.next_fire_at.unwrap(),
            "a failed poll must still reschedule (the FACT of polling) so the tick retries next \
             interval instead of tight-looping"
        );
    }

    #[tokio::test]
    async fn interpret_connector_poll_result_unresolvable_cursor_persists_reason() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-ce-cursor-unresolved");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = make_connector_event_assignment(
            "ce-cursor-unresolved",
            "agent-ce-cursor-unresolved",
            "some-server",
            Some("content.0.text"), // configured, but the poll result below has no `content` key
        );
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let runner = make_schedule_runner(&persistence).await;
        let dispatcher = Arc::clone(&runner.queue_registry) as Arc<dyn NotificationDispatcher>;
        let AssignmentTrigger::ConnectorEvent { poll, .. } = &assignment.trigger else {
            unreachable!()
        };

        let poll_result: Result<Value, McpError> = Ok(serde_json::json!({ "unrelated": "value" }));

        let outcome = runner
            .interpret_connector_poll_result(&assignment, "some-server", poll, poll_result, None, &dispatcher)
            .await;

        let ConnectorEventOutcome::PolledQuiescent { reason, cursor } = outcome else {
            panic!("expected PolledQuiescent, got {outcome:?}");
        };
        assert!(cursor.is_none());
        assert!(matches!(&reason, QuiescenceReason::CursorUnresolved { server } if server == "some-server"));

        persistence
            .assignments
            .mark_polled(&assignment.id, cursor.clone(), false, 300)
            .await
            .unwrap();
        persistence
            .assignments
            .mark_evaluated(&assignment.id, EvaluationOutcome::Quiescent(reason))
            .await
            .unwrap();

        let after = persistence.assignments.get("ce-cursor-unresolved").await.unwrap();
        assert!(matches!(
            after.liveness.last_quiescence,
            Some(QuiescenceReason::CursorUnresolved { ref server }) if server == "some-server"
        ));
        assert!(after.liveness.last_evaluated_at.is_some());
    }

    // ---------------------------------------------------------------------------
    // `tick_agent_watches` — proves the scheduler wiring itself (due-check,
    // expiry-disable, reschedule via `mark_polled`). The detect loop's own
    // fire/quiet/dedup logic is exhaustively covered by `agent_watch`'s own
    // test suite against `run_agent_watch_tick` directly; these tests only
    // need to show `tick()` actually reaches that loop with a real detector.
    // ---------------------------------------------------------------------------

    fn make_agent_watch_assignment(id: &str, agent_id: &str) -> ao_protocol::assignment::Assignment {
        use ao_protocol::assignment::{AssignmentThreadPolicy, AssignmentTrigger, OutputMode};

        let now = Utc::now();
        ao_protocol::assignment::Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: "New finance email watcher".to_string(),
            instruction: "Summarize the new finance email.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::AgentWatch {
                instruction: "Check my inbox for a new email from finance".to_string(),
                poll_interval_secs: 60,
                connector_scope: None,
                contract: None,
                extraction: None,
                extraction_tool: None,
                extraction_args: None,
                extraction_output_schema_declared: false,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: Some(now),
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    #[tokio::test]
    async fn tick_disables_expired_agent_watch_assignment() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-watch-expired");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment = make_agent_watch_assignment("watch-expired", "agent-watch-expired");
        let expires_at = Utc::now() - chrono::Duration::hours(1);
        assignment.expires_at = Some(expires_at);
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        let after = persistence.assignments.get("watch-expired").await.unwrap();
        assert!(!after.enabled, "an agent-watch assignment past its expires_at must be disabled by the tick");
        assert!(
            after.last_run_at.is_none(),
            "an expired agent-watch assignment must be skipped, not evaluated"
        );
        assert_eq!(
            after.liveness.last_quiescence,
            Some(QuiescenceReason::Expired { expires_at }),
            "an expired agent-watch assignment must persist why it didn't fire — this branch used \
             to `continue` with no liveness recorded at all"
        );
        assert!(
            after.liveness.last_evaluated_at.is_some(),
            "a tick that evaluates an expired agent-watch assignment must still count as an evaluation"
        );
    }

    #[tokio::test]
    async fn tick_skips_agent_watch_assignment_not_yet_due() {
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-watch-not-due");
        persistence.agents.create(&agent).await.unwrap();

        let mut assignment = make_agent_watch_assignment("watch-not-due", "agent-watch-not-due");
        let next_fire_at = Utc::now() + chrono::Duration::hours(1);
        assignment.next_fire_at = Some(next_fire_at);
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        // Swap in a detector that panics if ever polled — proves a
        // not-yet-due assignment is skipped before any detection is
        // attempted, not silently evaluated and found empty.
        runner.agent_watch_detector = Arc::new(crate::agent_watch::ScriptedDetector::new(vec![]));
        runner.tick().await;

        let after = persistence.assignments.get("watch-not-due").await.unwrap();
        assert!(after.last_run_at.is_none());
        assert_eq!(
            after.liveness.last_quiescence,
            Some(QuiescenceReason::NotDue {
                next_fire_at: Some(next_fire_at)
            }),
            "a not-yet-due agent-watch assignment must persist why it didn't fire — this is the \
             most common tick outcome for this trigger kind (polling intervals are minutes, ticks \
             are every second) and the branch that had NO liveness recorded at all before this task"
        );
        assert!(
            after.liveness.last_evaluated_at.is_some(),
            "a tick that evaluates a not-yet-due agent-watch assignment must still count as an evaluation"
        );
    }

    #[tokio::test]
    async fn tick_fires_agent_watch_assignment_via_scripted_detector_and_reschedules() {
        use crate::agent_watch::{AgentWatchCandidate, ScriptedDetector};

        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-watch-fires");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = make_agent_watch_assignment("watch-fires", "agent-watch-fires");
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let candidate = AgentWatchCandidate {
            id: "email-1".to_string(),
            summary: "New email from finance".to_string(),
            payload: serde_json::json!({ "id": "email-1" }),
        };

        let mut runner = make_schedule_runner(&persistence).await;
        runner.agent_watch_detector = Arc::new(ScriptedDetector::new(vec![
            Ok(vec![]),              // first tick: seeds an empty baseline, no fire
            Ok(vec![candidate]),     // second tick: a genuinely new item
        ]));

        runner.tick().await;
        let after_first = persistence.assignments.get("watch-fires").await.unwrap();
        assert!(after_first.last_run_at.is_none(), "seeding the baseline must not fire");
        assert!(
            after_first.next_fire_at.unwrap() > Utc::now(),
            "the tick must reschedule next_fire_at forward even when it doesn't fire"
        );

        // Force the second tick to be due now (mark_polled just pushed it
        // `poll_interval_secs` into the future).
        let mut due_again = after_first.clone();
        due_again.next_fire_at = Some(Utc::now());
        persistence.assignments.update(due_again).await.unwrap();

        runner.tick().await;
        let after_second = persistence.assignments.get("watch-fires").await.unwrap();
        assert!(
            after_second.last_run_at.is_some(),
            "a genuinely new candidate reaching tick() through the full wiring must fire"
        );

        let runs = persistence.assignment_runs.list_for_assignment("watch-fires").await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].trigger_kind, ao_protocol::assignment::AssignmentTriggerKind::AgentWatch);
    }

    // ---------------------------------------------------------------------------
    // `evaluate_agent_watch_assignment`'s non-fire path — populating the
    // shared `LivenessState` field on a due tick that doesn't fire, without
    // touching `run_agent_watch_tick`'s own scratchpad bookkeeping
    // (exhaustively covered by `agent_watch`'s own test suite, unmodified by
    // this task — see `tick_fires_agent_watch_assignment_via_scripted_detector_and_reschedules`
    // just above, which still passes unchanged).
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn tick_agent_watch_seeding_poll_persists_liveness_via_adapter() {
        use crate::agent_watch::ScriptedDetector;

        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-watch-liveness-adapter");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = make_agent_watch_assignment("watch-liveness-adapter", "agent-watch-liveness-adapter");
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        // Seeding tick: no candidates yet, so `run_agent_watch_tick` never
        // fires — mirrors the first tick of
        // `tick_fires_agent_watch_assignment_via_scripted_detector_and_reschedules`.
        runner.agent_watch_detector = Arc::new(ScriptedDetector::new(vec![Ok(vec![])]));
        runner.tick().await;

        let after = persistence.assignments.get("watch-liveness-adapter").await.unwrap();
        assert!(after.last_run_at.is_none(), "sanity: the seeding poll must not fire");
        assert!(
            after.liveness.last_evaluated_at.is_some(),
            "the AgentWatch adapter must populate the shared LivenessState field even though \
             run_agent_watch_tick's own scratchpad bookkeeping is what actually decided this tick"
        );
        assert!(
            matches!(
                after.liveness.last_quiescence,
                Some(QuiescenceReason::AgentWatchContractNotBound(_))
            ),
            "a tick with no bound contract yet must surface that as its quiescence reason, got {:?}",
            after.liveness.last_quiescence
        );
    }

    #[tokio::test]
    async fn tick_agent_watch_fire_does_not_double_count_fire_via_adapter() {
        use crate::agent_watch::{AgentWatchCandidate, ScriptedDetector};

        // Regression guard for the double-count risk `evaluate_agent_watch_assignment`
        // guards against: `mark_polled(fired: true, ..)` already routes
        // through `apply_evaluation(Fired)` — if that function ever stopped
        // returning `EvaluationOutcome::Fired` early on a fire (see its own
        // DELIBERATE comment), this would increment `fire_count` twice for
        // one real fire.
        let (_tmp, persistence) = make_persistence().await;
        let agent = make_agent("agent-watch-liveness-fire");
        persistence.agents.create(&agent).await.unwrap();

        let assignment = make_agent_watch_assignment("watch-liveness-fire", "agent-watch-liveness-fire");
        persistence.assignments.add(assignment.clone()).await.unwrap();

        let candidate = AgentWatchCandidate {
            id: "email-1".to_string(),
            summary: "New email from finance".to_string(),
            payload: serde_json::json!({ "id": "email-1" }),
        };

        let mut runner = make_schedule_runner(&persistence).await;
        runner.agent_watch_detector = Arc::new(ScriptedDetector::new(vec![
            Ok(vec![]),          // seed
            Ok(vec![candidate]), // genuinely new
        ]));

        runner.tick().await;
        let mut due_again = persistence
            .assignments
            .get("watch-liveness-fire")
            .await
            .unwrap();
        due_again.next_fire_at = Some(Utc::now());
        persistence.assignments.update(due_again).await.unwrap();

        runner.tick().await;
        let after = persistence.assignments.get("watch-liveness-fire").await.unwrap();
        assert!(after.last_run_at.is_some(), "sanity: the second tick must fire");
        assert_eq!(
            after.liveness.fire_count, 1,
            "a single real fire must increment fire_count exactly once, not twice"
        );
        assert!(
            after.liveness.last_quiescence.is_none(),
            "a fire must clear any previously recorded quiescence"
        );
    }

    // ---------------------------------------------------------------------------
    // `nearest_cron_fire_in` — the pure helper backing the sleep guard's arm
    // decision. This is deliberately factored out of `tick()` so the
    // decision logic can be tested without touching the real `NoSleep` OS
    // assertion (see `SleepGuard`'s own tests for why that part isn't
    // asserted on directly).
    // ---------------------------------------------------------------------------

    fn make_cron_assignment_at(
        id: &str,
        agent_id: &str,
        next_fire_at: Option<chrono::DateTime<Utc>>,
        expires_at: Option<chrono::DateTime<Utc>>,
    ) -> ao_protocol::assignment::Assignment {
        let mut a = make_expired_cron_assignment(id, agent_id);
        a.next_fire_at = next_fire_at;
        a.expires_at = expires_at;
        a
    }

    #[test]
    fn nearest_cron_fire_in_is_none_with_no_assignments() {
        assert_eq!(nearest_cron_fire_in(&[], Utc::now()), None);
    }

    #[test]
    fn nearest_cron_fire_in_picks_the_soonest_pending_assignment() {
        let now = Utc::now();
        let far = make_cron_assignment_at(
            "far",
            "agent",
            Some(now + chrono::Duration::hours(3)),
            None,
        );
        let near = make_cron_assignment_at(
            "near",
            "agent",
            Some(now + chrono::Duration::minutes(10)),
            None,
        );

        let nearest = nearest_cron_fire_in(&[far, near], now);
        assert_eq!(
            nearest,
            Some((chrono::Duration::minutes(10)).to_std().unwrap()),
            "must return the closer of two pending assignments, not the first in the slice"
        );
    }

    #[test]
    fn nearest_cron_fire_in_excludes_already_due_and_missing_fire_time() {
        let now = Utc::now();
        let already_due = make_cron_assignment_at(
            "due",
            "agent",
            Some(now - chrono::Duration::seconds(1)),
            None,
        );
        let no_fire_time = make_cron_assignment_at("no-fire", "agent", None, None);

        assert_eq!(
            nearest_cron_fire_in(&[already_due, no_fire_time], now),
            None,
            "assignments due now (handled by immediate firing) or with no next_fire_at must not hold the guard"
        );
    }

    #[test]
    fn nearest_cron_fire_in_excludes_expired_assignments() {
        let now = Utc::now();
        // Due to fire soon, but its expiry has already passed — `tick()`
        // disables it this tick instead of firing it, so it must not hold
        // the guard open either.
        let expired = make_cron_assignment_at(
            "expired",
            "agent",
            Some(now + chrono::Duration::minutes(1)),
            Some(now - chrono::Duration::hours(1)),
        );

        assert_eq!(nearest_cron_fire_in(&[expired], now), None);
    }

    #[test]
    fn nearest_cron_fire_in_excludes_non_cron_triggers() {
        let now = Utc::now();
        let mut webhook = make_cron_assignment_at(
            "webhook",
            "agent",
            Some(now + chrono::Duration::minutes(1)),
            None,
        );
        webhook.trigger = AssignmentTrigger::Webhook {
            token: None,
            route_name: None,
            secret_ref: None,
            events: vec![],
            filters: None,
            prompt_template: None,
            deliver: ao_protocol::assignment::WebhookDeliverTarget::Agent,
        };

        assert_eq!(
            nearest_cron_fire_in(&[webhook], now),
            None,
            "only Cron-triggered assignments should factor into the sleep guard window"
        );
    }

    // ---------------------------------------------------------------------------
    // `tick()` end-to-end: confirms `max_sleep_guard_hours` / `keep_display_awake`
    // preferences actually reach the runner's `SleepGuard`, not just that they
    // round-trip through the preferences store. The underlying OS power
    // assertion itself (`SleepGuard::acquire`/`NoSleep`) is NOT exercised
    // here: proving it would mean observing real power state on a live
    // machine, which no unit test can do. That half is verified by hand.
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn tick_applies_sleep_guard_hours_and_display_preference() {
        let (_tmp, persistence) = make_persistence().await;
        let mut prefs = ao_protocol::preferences::UserPreferences::default();
        prefs.max_sleep_guard_hours = Some(2.5);
        prefs.keep_display_awake = true;
        persistence.preferences.save(&prefs).await.unwrap();

        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        assert!(
            !runner.sleep_guard.is_disabled(),
            "a Some(hours) preference must leave the guard enabled"
        );
        assert_eq!(runner.sleep_guard.window_hours(), 2.5);
        assert!(runner.sleep_guard.keep_display_awake());
    }

    #[tokio::test]
    async fn tick_disables_sleep_guard_when_preference_is_none() {
        let (_tmp, persistence) = make_persistence().await;
        let mut prefs = ao_protocol::preferences::UserPreferences::default();
        prefs.max_sleep_guard_hours = None; // user turned the guard off entirely
        persistence.preferences.save(&prefs).await.unwrap();

        // The disabled state survives the preferences round-trip — a saved
        // `None` reloads as `None`, not the `Some(4.0)` default — so `tick`
        // genuinely sees the user's intent to disable the guard.
        let reloaded = persistence.preferences.get().await.unwrap().unwrap();
        assert_eq!(reloaded.max_sleep_guard_hours, None);

        let mut runner = make_schedule_runner(&persistence).await;
        runner.tick().await;

        assert!(
            runner.sleep_guard.is_disabled(),
            "max_sleep_guard_hours: None must disable the guard entirely"
        );
    }
}
