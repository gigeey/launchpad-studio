use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

use std::future::Future;

use ao_protocol::assignment::{
    Assignment, AssignmentTrigger, QuiescenceReason, MAX_ACTIVE_AGENT_WATCHES_PER_AGENT,
};
use ao_protocol::error::AoError;
use ao_protocol::thread::ThreadId;

use crate::cron_util::compute_next_fire_at;
use crate::paths::DataRoot;

/// In-memory store for assignment metadata, backed by an atomic JSON file at
/// [`DataRoot::assignments_path`].
///
/// One row per assignment. The full list is held under an
/// `Arc<RwLock<Vec<Assignment>>>` and every mutation persists the whole list
/// with a temp-then-rename write so a crash mid-write never leaves a
/// half-written file.
pub struct AssignmentStore {
    data_root: DataRoot,
    assignments: Arc<RwLock<Vec<Assignment>>>,
}

/// Outcome of one tick's evaluation of a single assignment, as recorded by
/// [`AssignmentStore::mark_evaluated`]. Every evaluation either fires or it
/// doesn't — when it doesn't, `Quiescent` carries the closed-set reason why
/// (see [`QuiescenceReason`]), so `LivenessState::last_quiescence` is never
/// set to "didn't fire" without also saying why.
///
/// `#[must_use]`: an evaluate function's entire point is to name why a tick
/// did or didn't fire — a caller that drops this value silently reintroduces
/// the exact liveness gap this type exists to close.
#[must_use]
#[derive(Debug, Clone, PartialEq)]
pub enum EvaluationOutcome {
    /// The assignment fired this tick.
    Fired,
    /// The assignment did not fire this tick, for the given reason.
    Quiescent(QuiescenceReason),
}

impl AssignmentStore {
    /// Load rows from `{data_root}/assignments.json`; start empty on first boot.
    pub async fn load(data_root: DataRoot) -> Result<Self, AoError> {
        let path = data_root.assignments_path();
        let assignments = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let contents = tokio::fs::read_to_string(&path).await?;
            if contents.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str::<Vec<Assignment>>(&contents)
                    .map_err(|e| AoError::Json(e.to_string()))?
            }
        } else {
            Vec::new()
        };
        Ok(Self {
            data_root,
            assignments: Arc::new(RwLock::new(assignments)),
        })
    }

    /// Persist the in-memory list atomically (write to temp file then rename).
    async fn save(&self) -> Result<(), AoError> {
        let assignments = {
            let guard = self.assignments.read().await;
            guard.clone()
        };
        let path = self.data_root.assignments_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::to_string_pretty(&assignments)
            .map_err(|e| AoError::Json(e.to_string()))?;
        let tmp_path = path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, json).await?;
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    /// All assignments owned by a given agent.
    pub async fn list_for_agent(&self, agent_id: &str) -> Vec<Assignment> {
        let guard = self.assignments.read().await;
        guard
            .iter()
            .filter(|a| a.agent_id == agent_id)
            .cloned()
            .collect()
    }

    /// Every assignment across every agent, in no particular order. Used by
    /// the startup `assignment_origin` backfill, which needs to walk every
    /// assignment regardless of owner.
    pub async fn list_all(&self) -> Vec<Assignment> {
        let guard = self.assignments.read().await;
        guard.clone()
    }

    /// All enabled assignments with a `Cron` trigger; used by the schedule
    /// runner's per-second tick.
    pub async fn list_all_enabled_cron(&self) -> Vec<Assignment> {
        let guard = self.assignments.read().await;
        guard
            .iter()
            .filter(|a| a.enabled && matches!(a.trigger, AssignmentTrigger::Cron { .. }))
            .cloned()
            .collect()
    }

    /// Fetch one assignment by id. Returns `None` if not found.
    pub async fn get(&self, id: &str) -> Option<Assignment> {
        let guard = self.assignments.read().await;
        guard.iter().find(|a| a.id == id).cloned()
    }

    /// Insert a new assignment row. Errors on id collision.
    pub async fn add(&self, assignment: Assignment) -> Result<(), AoError> {
        {
            let mut guard = self.assignments.write().await;
            if guard.iter().any(|a| a.id == assignment.id) {
                return Err(AoError::ValidationError(format!(
                    "Assignment already exists: {}",
                    assignment.id
                )));
            }
            guard.push(assignment);
        }
        self.save().await
    }

    /// Replace an existing assignment row by id. Errors if the id is missing.
    pub async fn update(&self, updated: Assignment) -> Result<(), AoError> {
        {
            let mut guard = self.assignments.write().await;
            if let Some(existing) = guard.iter_mut().find(|a| a.id == updated.id) {
                *existing = updated;
            } else {
                return Err(AoError::Internal(format!(
                    "Assignment not found: {}",
                    updated.id
                )));
            }
        }
        self.save().await
    }

    /// Drop an assignment row by id. Idempotent (returns `Ok` if missing). The
    /// run history JSONL file is intentionally left in place so past runs are
    /// preserved.
    pub async fn remove(&self, id: &str) -> Result<(), AoError> {
        {
            let mut guard = self.assignments.write().await;
            guard.retain(|a| a.id != id);
        }
        self.save().await
    }

    /// Called after a cron assignment fires. Updates `last_run_at` and
    /// recomputes `next_fire_at`, or disables a one-shot assignment after its
    /// single firing. `timezone` is the user's IANA timezone string forwarded
    /// from the caller (e.g. "America/Los_Angeles"). A no-op if the id is
    /// missing.
    ///
    /// Every call here is, by construction, a fire (`fire_assignment` is the
    /// only production caller) — so this always routes its liveness
    /// bookkeeping through [`Self::apply_evaluation`]'s `Fired` arm, the same
    /// mutation [`Self::mark_evaluated`] and [`Self::mark_polled`]'s own fire
    /// path use, rather than touching `liveness` with separate logic that
    /// could drift out of sync with theirs.
    pub async fn mark_fired(&self, id: &str, timezone: Option<&str>) -> Result<(), AoError> {
        {
            let mut guard = self.assignments.write().await;
            if let Some(assignment) = guard.iter_mut().find(|a| a.id == id) {
                let now = Utc::now();
                assignment.last_run_at = Some(now);
                Self::apply_evaluation(assignment, now, EvaluationOutcome::Fired);
                match &assignment.trigger {
                    AssignmentTrigger::Cron {
                        cron_expr,
                        is_recurring,
                    } => {
                        if *is_recurring {
                            assignment.next_fire_at =
                                compute_next_fire_at(Some(cron_expr), timezone);
                        } else {
                            // One-shot cron: disable after firing once.
                            assignment.enabled = false;
                            assignment.next_fire_at = None;
                        }
                    }
                    AssignmentTrigger::Webhook { .. } => {
                        // Webhook assignments carry no schedule state.
                        assignment.next_fire_at = None;
                    }
                    AssignmentTrigger::ConnectorEvent { .. } => {
                        // `mark_fired` is never called for a ConnectorEvent
                        // fire in practice (the poll loop uses `mark_polled`
                        // instead; the inbound webhook-bridge path fires with
                        // `AssignmentTriggerKind::Webhook`, which skips this
                        // call entirely). Left as a no-op rather than
                        // clobbering `next_fire_at` so a stray call can never
                        // disrupt the independent poll schedule.
                    }
                    AssignmentTrigger::AgentWatch { .. } => {
                        // Same reasoning as `ConnectorEvent` above: the
                        // detect loop reschedules via `mark_polled`, never
                        // this call.
                    }
                }
            }
        }
        self.save().await
    }

    /// All enabled assignments with a `ConnectorEvent` trigger; used by the
    /// schedule runner's per-second tick poll loop.
    pub async fn list_all_enabled_connector_event(&self) -> Vec<Assignment> {
        let guard = self.assignments.read().await;
        guard
            .iter()
            .filter(|a| a.enabled && matches!(a.trigger, AssignmentTrigger::ConnectorEvent { .. }))
            .cloned()
            .collect()
    }

    /// All enabled assignments with an `AgentWatch` trigger; used by the
    /// schedule runner's per-second tick detect loop (Tier 2 of the
    /// detection ladder).
    pub async fn list_all_enabled_agent_watch(&self) -> Vec<Assignment> {
        let guard = self.assignments.read().await;
        guard
            .iter()
            .filter(|a| a.enabled && matches!(a.trigger, AssignmentTrigger::AgentWatch { .. }))
            .cloned()
            .collect()
    }

    /// Count of enabled `AgentWatch` assignments owned by `agent_id`. Backs
    /// [`Self::enforce_agent_watch_cap`].
    pub async fn count_enabled_agent_watch_for_agent(&self, agent_id: &str) -> usize {
        let guard = self.assignments.read().await;
        guard
            .iter()
            .filter(|a| {
                a.agent_id == agent_id
                    && a.enabled
                    && matches!(a.trigger, AssignmentTrigger::AgentWatch { .. })
            })
            .count()
    }

    /// Enforce [`MAX_ACTIVE_AGENT_WATCHES_PER_AGENT`] at create/patch time.
    ///
    /// A no-op unless the row being mutated is *newly* becoming an enabled
    /// `AgentWatch` — i.e. `enabled` is true, `trigger` is `AgentWatch`, and
    /// `was_already_counted` is false. `was_already_counted` should reflect
    /// the row's state *before* this mutation: `false` for a brand-new row
    /// (nothing to exclude), or `existing.enabled &&
    /// matches!(existing.trigger, AgentWatch)` when patching an existing row.
    /// Passing the pre-mutation state this way means a plain edit to an
    /// already-enabled `AgentWatch` row (rename, instruction tweak, etc.)
    /// never re-triggers the cap check — only a genuine create-or-enable
    /// transition does. The cap itself is create/enable-time only: disabling
    /// a row, or any other change, never retroactively touches existing rows.
    pub async fn enforce_agent_watch_cap(
        &self,
        agent_id: &str,
        trigger: &AssignmentTrigger,
        enabled: bool,
        was_already_counted: bool,
    ) -> Result<(), AoError> {
        let becomes_active_agent_watch = enabled && matches!(trigger, AssignmentTrigger::AgentWatch { .. });
        if !becomes_active_agent_watch || was_already_counted {
            return Ok(());
        }
        let count = self.count_enabled_agent_watch_for_agent(agent_id).await;
        if count >= MAX_ACTIVE_AGENT_WATCHES_PER_AGENT {
            return Err(AoError::ValidationError(format!(
                "agent \"{agent_id}\" already has {count} active AgentWatch assignments (max {MAX_ACTIVE_AGENT_WATCHES_PER_AGENT}) — disable one before enabling another"
            )));
        }
        Ok(())
    }

    /// Every assignment (enabled or not) whose `Webhook` trigger names
    /// `route_name` as its inbound route, in store order (insertion order —
    /// stable, so route-level secret resolution against the first match is
    /// deterministic across calls). Enabled-ness is left to the caller: the
    /// gateway needs to see a disabled row sharing a route to still resolve
    /// that route's secret and reject/accept requests consistently, even
    /// though it will skip actually firing the disabled row.
    pub async fn list_webhook_assignments_by_route(&self, route_name: &str) -> Vec<Assignment> {
        let guard = self.assignments.read().await;
        guard
            .iter()
            .filter(|a| matches!(&a.trigger, AssignmentTrigger::Webhook { route_name: Some(r), .. } if r == route_name))
            .cloned()
            .collect()
    }

    /// Every assignment (enabled or not) with a `Webhook` trigger that names
    /// a `route_name`. Used only by the gateway's best-effort startup sweep
    /// to discover the distinct set of routes worth checking — the
    /// per-request path resolves a single route via
    /// [`Self::list_webhook_assignments_by_route`] instead of scanning
    /// everything on every POST.
    pub async fn list_all_named_webhook_routes(&self) -> Vec<Assignment> {
        let guard = self.assignments.read().await;
        guard
            .iter()
            .filter(|a| matches!(&a.trigger, AssignmentTrigger::Webhook { route_name: Some(_), .. }))
            .cloned()
            .collect()
    }

    /// Called after the poll loop checks a `ConnectorEvent` assignment's
    /// connector, whether or not a new event was found.
    ///
    /// Always advances `next_fire_at` by `poll_interval_secs` so the tick
    /// doesn't re-poll until the interval elapses. When `cursor` is `Some`,
    /// advances `last_event_cursor` to it — this covers both the "seed the
    /// baseline on first poll" case and "record the newly observed cursor"
    /// case; the caller decides whether that cursor change also constitutes
    /// a fire. When `fired` is `true`, also stamps `last_run_at`, mirroring
    /// what `mark_fired` does for `Cron`. A no-op if the id is missing.
    ///
    /// Liveness bookkeeping: on `fired == true` this routes through
    /// [`Self::apply_evaluation`]'s `Fired` arm, identically to
    /// [`Self::mark_fired`] and a `mark_evaluated(id, Fired)` call — the same
    /// one mutation, so the three can't disagree about what a fire does to
    /// `LivenessState`. On `fired == false` this still stamps
    /// `liveness.last_evaluated_at` (every tick that reaches this call did
    /// evaluate the assignment, whether or not it fired), but — unlike
    /// [`Self::mark_evaluated`]'s `Quiescent` arm — it does NOT set
    /// `liveness.last_quiescence`: this method's signature carries no
    /// [`QuiescenceReason`], because none of its current callers
    /// (`ScheduleRunner::tick_connector_events`) have one to give it yet.
    /// Callers that DO know the reason should call
    /// [`Self::mark_evaluated`] directly instead of this method once they're
    /// updated to construct one; until then, a poll that didn't fire leaves
    /// `last_quiescence` exactly as it was set by whatever last actually
    /// recorded a reason, rather than silently overwriting it with nothing.
    pub async fn mark_polled(
        &self,
        id: &str,
        cursor: Option<String>,
        fired: bool,
        poll_interval_secs: u64,
    ) -> Result<(), AoError> {
        {
            let mut guard = self.assignments.write().await;
            if let Some(assignment) = guard.iter_mut().find(|a| a.id == id) {
                let now = Utc::now();
                if fired {
                    assignment.last_run_at = Some(now);
                    Self::apply_evaluation(assignment, now, EvaluationOutcome::Fired);
                } else {
                    assignment.liveness.last_evaluated_at = Some(now);
                }
                if let Some(cursor) = cursor {
                    assignment.last_event_cursor = Some(cursor);
                }
                assignment.next_fire_at =
                    Some(now + chrono::Duration::seconds(poll_interval_secs as i64));
            }
        }
        self.save().await
    }

    /// Records that the tick loop evaluated assignment `id` this tick,
    /// whether or not that evaluation fired. ALWAYS stamps
    /// `liveness.last_evaluated_at` to now. On `EvaluationOutcome::Fired`,
    /// also increments `liveness.fire_count` and clears
    /// `liveness.last_quiescence` (a fresh fire supersedes whatever reason
    /// blocked the previous tick). On `EvaluationOutcome::Quiescent(reason)`,
    /// records `reason` as `liveness.last_quiescence` and leaves `fire_count`
    /// untouched. A no-op if the id is missing.
    ///
    /// This is the primitive [`Self::mark_fired`] and [`Self::mark_polled`]'s
    /// fire path route through (via the shared [`Self::apply_evaluation`]
    /// mutation) — there is exactly one place that decides what a fire vs. a
    /// quiescent tick does to `LivenessState`, so the three entry points
    /// cannot drift apart and disagree.
    pub async fn mark_evaluated(&self, id: &str, outcome: EvaluationOutcome) -> Result<(), AoError> {
        {
            let mut guard = self.assignments.write().await;
            if let Some(assignment) = guard.iter_mut().find(|a| a.id == id) {
                let now = Utc::now();
                Self::apply_evaluation(assignment, now, outcome);
            }
        }
        self.save().await
    }

    /// Pure liveness mutation shared by [`Self::mark_evaluated`],
    /// [`Self::mark_fired`], and [`Self::mark_polled`]'s fire path — the
    /// single place `LivenessState` is ever written, so those three public
    /// entry points can never disagree about what a given outcome does to
    /// it. Takes `now` from the caller rather than calling `Utc::now()`
    /// itself so a caller that also stamps `last_run_at` this tick uses one
    /// consistent timestamp for both.
    fn apply_evaluation(assignment: &mut Assignment, now: DateTime<Utc>, outcome: EvaluationOutcome) {
        assignment.liveness.last_evaluated_at = Some(now);
        match outcome {
            EvaluationOutcome::Fired => {
                assignment.liveness.fire_count += 1;
                assignment.liveness.last_quiescence = None;
            }
            EvaluationOutcome::Quiescent(reason) => {
                assignment.liveness.last_quiescence = Some(reason);
            }
        }
    }

    /// Atomically resolve a `Dedicated`-policy assignment's reused thread id:
    /// if `dedicated_thread_id` is already set, returns it unchanged;
    /// otherwise runs `create_thread` to build a brand-new thread and
    /// persists its id onto the assignment before returning it.
    ///
    /// The whole read-check-create-write sequence runs under a single write
    /// lock so two concurrent callers (e.g. a burst of near-simultaneous
    /// inbound webhook fires) can never both observe `None` and both create
    /// a thread — the second caller's `create_thread` never even runs; it
    /// re-checks under the lock and returns the id the first caller just
    /// claimed. Without this, a webhook burst would orphan one of the two
    /// created threads and silently drop whichever write lost the race.
    ///
    /// Errors with `AssignmentNotFound` if the id is missing. `create_thread`
    /// itself may fail (thread-store I/O); the write lock is dropped before
    /// persisting to disk either way, but the assignment's dedicated id is
    /// only mutated in memory, and only saved to disk, once `create_thread`
    /// succeeds.
    pub async fn claim_dedicated_thread_id<F, Fut>(
        &self,
        assignment_id: &str,
        create_thread: F,
    ) -> Result<ThreadId, AoError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<ThreadId, AoError>>,
    {
        // Fast path: already claimed. Take the read lock first so the common
        // case (every fire after the first) never contends with writers.
        {
            let guard = self.assignments.read().await;
            let assignment = guard
                .iter()
                .find(|a| a.id == assignment_id)
                .ok_or_else(|| AoError::AssignmentNotFound(assignment_id.to_string()))?;
            if let Some(existing) = &assignment.dedicated_thread_id {
                return Ok(existing.clone());
            }
        }

        let mut guard = self.assignments.write().await;
        let idx = guard
            .iter()
            .position(|a| a.id == assignment_id)
            .ok_or_else(|| AoError::AssignmentNotFound(assignment_id.to_string()))?;
        // Re-check under the write lock: another caller may have won the
        // race and claimed a thread between our read-lock check above and
        // acquiring this write lock.
        if let Some(existing) = &guard[idx].dedicated_thread_id {
            return Ok(existing.clone());
        }
        let new_id = create_thread().await?;
        guard[idx].dedicated_thread_id = Some(new_id.clone());
        guard[idx].updated_ts = Utc::now();
        drop(guard);
        self.save().await?;
        Ok(new_id)
    }

    /// Forget a stale `dedicated_thread_id` (e.g. the user deleted the
    /// thread out from under the assignment) so the next fire re-claims a
    /// fresh one via [`Self::claim_dedicated_thread_id`]. A no-op if the
    /// assignment is missing or already has no dedicated thread recorded.
    pub async fn clear_dedicated_thread_id(&self, assignment_id: &str) -> Result<(), AoError> {
        {
            let mut guard = self.assignments.write().await;
            match guard.iter_mut().find(|a| a.id == assignment_id) {
                Some(assignment) if assignment.dedicated_thread_id.is_some() => {
                    assignment.dedicated_thread_id = None;
                    assignment.updated_ts = Utc::now();
                }
                _ => return Ok(()),
            }
        }
        self.save().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_protocol::assignment::{AssignmentTrigger, LivenessState, OutputMode};
    use chrono::Utc;

    fn setup() -> (tempfile::TempDir, DataRoot) {
        let tmp = tempfile::tempdir().unwrap();
        let data_root = DataRoot::new(tmp.path());
        (tmp, data_root)
    }

    async fn ready_store() -> (tempfile::TempDir, DataRoot, AssignmentStore) {
        let (tmp, data_root) = setup();
        data_root.ensure_directories().await.unwrap();
        let store = AssignmentStore::load(data_root.clone()).await.unwrap();
        (tmp, data_root, store)
    }

    fn cron_assignment(id: &str, agent_id: &str, recurring: bool) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: format!("Assignment {id}"),
            instruction: "do the thing".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Cron {
                cron_expr: "0 9 * * *".to_string(),
                is_recurring: recurring,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: ao_protocol::assignment::AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: compute_next_fire_at(Some("0 9 * * *"), None),
            last_run_at: None,
            last_event_cursor: None,
            liveness: LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn connector_event_assignment(
        id: &str,
        agent_id: &str,
        poll_interval_secs: u64,
    ) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: format!("Connector {id}"),
            instruction: "summarize the new item".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::ConnectorEvent {
                server_name: "gmail".to_string(),
                poll: ao_protocol::assignment::ConnectorPollSpec {
                    tool_name: "list_starred".to_string(),
                    arguments: serde_json::json!({}),
                    cursor_path: None,
                },
                poll_interval_secs,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: ao_protocol::assignment::AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: Some(now),
            last_run_at: None,
            last_event_cursor: None,
            liveness: LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn agent_watch_assignment(id: &str, agent_id: &str, poll_interval_secs: u64) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: format!("Watch {id}"),
            instruction: "summarize the new item".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::AgentWatch {
                instruction: "check my inbox for a new email from finance".to_string(),
                poll_interval_secs,
                connector_scope: None,
                contract: None,
                extraction: None,
                extraction_tool: None,
                extraction_args: None,
                extraction_output_schema_declared: false,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: ao_protocol::assignment::AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: Some(now),
            last_run_at: None,
            last_event_cursor: None,
            liveness: LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn webhook_assignment(id: &str, agent_id: &str, token: Option<&str>) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: format!("Webhook {id}"),
            instruction: "handle inbound".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Webhook {
                token: token.map(|t| t.to_string()),
                route_name: None,
                secret_ref: None,
                events: vec![],
                filters: None,
                prompt_template: None,
                deliver: ao_protocol::assignment::WebhookDeliverTarget::default(),
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: ao_protocol::assignment::AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: None,
            last_run_at: None,
            last_event_cursor: None,
            liveness: LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn webhook_route_assignment(id: &str, agent_id: &str, route_name: &str, secret_ref: Option<&str>) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: agent_id.to_string(),
            name: format!("Webhook {id}"),
            instruction: "handle inbound".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Webhook {
                token: None,
                route_name: Some(route_name.to_string()),
                secret_ref: secret_ref.map(|s| s.to_string()),
                events: vec![],
                filters: None,
                prompt_template: None,
                deliver: ao_protocol::assignment::WebhookDeliverTarget::default(),
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: ao_protocol::assignment::AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: None,
            last_run_at: None,
            last_event_cursor: None,
            liveness: LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    #[tokio::test]
    async fn load_returns_empty_store_when_no_file() {
        let (_tmp, _root, store) = ready_store().await;
        assert!(store.list_for_agent("agent-1").await.is_empty());
        assert!(store.get("nope").await.is_none());
    }

    #[tokio::test]
    async fn add_get_and_list_for_agent_round_trip() {
        let (_tmp, data_root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();
        store.add(webhook_assignment("a2", "agent-1", Some("tok"))).await.unwrap();
        store.add(cron_assignment("a3", "agent-2", true)).await.unwrap();

        let got = store.get("a1").await.expect("a1 exists");
        assert_eq!(got.id, "a1");

        let agent1 = store.list_for_agent("agent-1").await;
        assert_eq!(agent1.len(), 2);
        let agent2 = store.list_for_agent("agent-2").await;
        assert_eq!(agent2.len(), 1);

        // Round-trip via disk: a fresh store sees the same three rows.
        let reload = AssignmentStore::load(data_root).await.unwrap();
        assert_eq!(reload.list_for_agent("agent-1").await.len(), 2);
        assert_eq!(reload.list_for_agent("agent-2").await.len(), 1);
    }

    #[tokio::test]
    async fn add_rejects_duplicate_id() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("dup", "agent-1", true)).await.unwrap();
        let err = store.add(cron_assignment("dup", "agent-1", true)).await.unwrap_err();
        assert!(matches!(err, AoError::ValidationError(_)));
    }

    #[tokio::test]
    async fn update_replaces_existing_row() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();

        let mut updated = store.get("a1").await.unwrap();
        updated.name = "Renamed".to_string();
        updated.enabled = false;
        store.update(updated).await.unwrap();

        let got = store.get("a1").await.unwrap();
        assert_eq!(got.name, "Renamed");
        assert!(!got.enabled);
    }

    #[tokio::test]
    async fn working_directory_and_expires_at_round_trip_through_add_get_update() {
        let (_tmp, _root, store) = ready_store().await;
        let mut assignment = cron_assignment("a1", "agent-1", true);
        assignment.working_directory = Some("/repo/project".to_string());
        assignment.expires_at = Some(Utc::now() + chrono::Duration::days(7));
        store.add(assignment.clone()).await.unwrap();

        let got = store.get("a1").await.unwrap();
        assert_eq!(got.working_directory.as_deref(), Some("/repo/project"));
        assert_eq!(got.expires_at, assignment.expires_at);

        let mut updated = got;
        updated.working_directory = Some("/repo/other".to_string());
        updated.expires_at = None;
        store.update(updated).await.unwrap();

        let refetched = store.get("a1").await.unwrap();
        assert_eq!(refetched.working_directory.as_deref(), Some("/repo/other"));
        assert!(refetched.expires_at.is_none());
    }

    #[tokio::test]
    async fn update_missing_id_errors() {
        let (_tmp, _root, store) = ready_store().await;
        let err = store.update(cron_assignment("ghost", "agent-1", true)).await.unwrap_err();
        assert!(matches!(err, AoError::Internal(_)));
    }

    #[tokio::test]
    async fn remove_is_idempotent() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();

        store.remove("a1").await.unwrap();
        assert!(store.get("a1").await.is_none());

        // Removing a missing row is a no-op, not an error.
        store.remove("a1").await.unwrap();
        store.remove("never-existed").await.unwrap();
    }

    #[tokio::test]
    async fn list_all_enabled_cron_filters_by_trigger_and_enabled() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("cron-on", "agent-1", true)).await.unwrap();
        store.add(webhook_assignment("hook", "agent-1", None)).await.unwrap();

        let mut disabled = cron_assignment("cron-off", "agent-1", true);
        disabled.enabled = false;
        store.add(disabled).await.unwrap();

        let enabled_cron = store.list_all_enabled_cron().await;
        assert_eq!(enabled_cron.len(), 1);
        assert_eq!(enabled_cron[0].id, "cron-on");
    }

    #[tokio::test]
    async fn list_webhook_assignments_by_route_matches_only_that_route_and_includes_disabled() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(webhook_route_assignment("hook-a1", "agent-1", "github-prs", Some("secret-1"))).await.unwrap();
        store.add(webhook_route_assignment("hook-a2", "agent-2", "github-prs", None)).await.unwrap();
        store.add(webhook_route_assignment("hook-other", "agent-1", "other-route", Some("secret-2"))).await.unwrap();
        store.add(cron_assignment("cron", "agent-1", true)).await.unwrap();

        let mut disabled = webhook_route_assignment("hook-disabled", "agent-1", "github-prs", None);
        disabled.enabled = false;
        store.add(disabled).await.unwrap();

        let matches = store.list_webhook_assignments_by_route("github-prs").await;
        let ids: std::collections::BTreeSet<_> = matches.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, std::collections::BTreeSet::from(["hook-a1", "hook-a2", "hook-disabled"]));

        assert!(store.list_webhook_assignments_by_route("no-such-route").await.is_empty());
    }

    #[tokio::test]
    async fn list_all_named_webhook_routes_excludes_unnamed_and_non_webhook_triggers() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(webhook_route_assignment("hook-named", "agent-1", "github-prs", Some("secret-1"))).await.unwrap();
        store.add(webhook_assignment("hook-unnamed", "agent-1", Some("legacy-tok"))).await.unwrap();
        store.add(cron_assignment("cron", "agent-1", true)).await.unwrap();

        let named = store.list_all_named_webhook_routes().await;
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].id, "hook-named");
    }

    #[tokio::test]
    async fn list_all_enabled_connector_event_filters_by_trigger_and_enabled() {
        let (_tmp, _root, store) = ready_store().await;
        store
            .add(connector_event_assignment("connector-on", "agent-1", 60))
            .await
            .unwrap();
        store.add(cron_assignment("cron", "agent-1", true)).await.unwrap();

        let mut disabled = connector_event_assignment("connector-off", "agent-1", 60);
        disabled.enabled = false;
        store.add(disabled).await.unwrap();

        let enabled_connector = store.list_all_enabled_connector_event().await;
        assert_eq!(enabled_connector.len(), 1);
        assert_eq!(enabled_connector[0].id, "connector-on");
    }

    #[tokio::test]
    async fn list_all_enabled_agent_watch_filters_by_trigger_and_enabled() {
        let (_tmp, _root, store) = ready_store().await;
        store
            .add(agent_watch_assignment("watch-on", "agent-1", 300))
            .await
            .unwrap();
        store.add(cron_assignment("cron", "agent-1", true)).await.unwrap();
        store
            .add(connector_event_assignment("connector-on", "agent-1", 60))
            .await
            .unwrap();

        let mut disabled = agent_watch_assignment("watch-off", "agent-1", 300);
        disabled.enabled = false;
        store.add(disabled).await.unwrap();

        let enabled_watch = store.list_all_enabled_agent_watch().await;
        assert_eq!(enabled_watch.len(), 1);
        assert_eq!(enabled_watch[0].id, "watch-on");
    }

    #[tokio::test]
    async fn mark_fired_agent_watch_leaves_next_fire_at_untouched() {
        // Mirrors `mark_fired_connector_event_leaves_next_fire_at_untouched`:
        // the detect loop reschedules via `mark_polled`, never `mark_fired`.
        // This only guards `mark_fired`'s own match arm against ever
        // clobbering the independent poll schedule if it were called by
        // mistake.
        let (_tmp, _root, store) = ready_store().await;
        store.add(agent_watch_assignment("a1", "agent-1", 300)).await.unwrap();
        let before = store.get("a1").await.unwrap().next_fire_at;

        store.mark_fired("a1", None).await.unwrap();

        let after = store.get("a1").await.unwrap();
        assert!(after.last_run_at.is_some(), "mark_fired always stamps last_run_at");
        assert_eq!(after.next_fire_at, before, "poll schedule must be untouched");
    }

    #[tokio::test]
    async fn mark_polled_reschedules_and_advances_cursor_without_touching_last_run_at() {
        let (_tmp, _root, store) = ready_store().await;
        store
            .add(connector_event_assignment("a1", "agent-1", 60))
            .await
            .unwrap();

        store
            .mark_polled("a1", Some("cursor-1".to_string()), false, 60)
            .await
            .unwrap();

        let got = store.get("a1").await.unwrap();
        assert_eq!(got.last_event_cursor.as_deref(), Some("cursor-1"));
        assert!(
            got.last_run_at.is_none(),
            "a poll that found no new event must not stamp last_run_at"
        );
        assert!(got.next_fire_at.unwrap() > Utc::now());
    }

    #[tokio::test]
    async fn mark_polled_with_fired_stamps_last_run_at() {
        let (_tmp, _root, store) = ready_store().await;
        store
            .add(connector_event_assignment("a1", "agent-1", 60))
            .await
            .unwrap();

        store
            .mark_polled("a1", Some("cursor-2".to_string()), true, 60)
            .await
            .unwrap();

        let got = store.get("a1").await.unwrap();
        assert_eq!(got.last_event_cursor.as_deref(), Some("cursor-2"));
        assert!(got.last_run_at.is_some(), "a fired poll must stamp last_run_at");
    }

    #[tokio::test]
    async fn mark_polled_with_no_cursor_only_reschedules() {
        let (_tmp, _root, store) = ready_store().await;
        let mut seeded = connector_event_assignment("a1", "agent-1", 60);
        seeded.last_event_cursor = Some("existing".to_string());
        store.add(seeded).await.unwrap();

        // Simulates a failed/empty poll: no cursor observed, so the existing
        // one must survive untouched, but the schedule still advances.
        store.mark_polled("a1", None, false, 60).await.unwrap();

        let got = store.get("a1").await.unwrap();
        assert_eq!(got.last_event_cursor.as_deref(), Some("existing"));
        assert!(got.next_fire_at.unwrap() > Utc::now());
    }

    #[tokio::test]
    async fn mark_polled_missing_id_is_noop() {
        let (_tmp, _root, store) = ready_store().await;
        store.mark_polled("absent", Some("c".to_string()), true, 60).await.unwrap();
    }

    #[tokio::test]
    async fn mark_fired_connector_event_leaves_next_fire_at_untouched() {
        // A ConnectorEvent assignment fired via the webhook-bridge path calls
        // fire_assignment with AssignmentTriggerKind::Webhook, which never
        // invokes mark_fired at all (see fire_assignment's Cron-only guard).
        // This test only guards mark_fired's own match arm: if it were ever
        // called directly against a ConnectorEvent row, it must not disturb
        // the independent poll schedule.
        let (_tmp, _root, store) = ready_store().await;
        store
            .add(connector_event_assignment("a1", "agent-1", 60))
            .await
            .unwrap();
        let before = store.get("a1").await.unwrap().next_fire_at;

        store.mark_fired("a1", None).await.unwrap();

        let after = store.get("a1").await.unwrap();
        assert!(after.last_run_at.is_some(), "mark_fired always stamps last_run_at");
        assert_eq!(after.next_fire_at, before, "poll schedule must be untouched");
    }

    #[tokio::test]
    async fn mark_fired_recurring_recomputes_next_fire_at() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();

        store.mark_fired("a1", None).await.unwrap();

        let got = store.get("a1").await.unwrap();
        assert!(got.last_run_at.is_some());
        assert!(got.enabled, "recurring cron stays enabled");
        assert!(got.next_fire_at.is_some(), "recurring cron recomputes next fire");
    }

    #[tokio::test]
    async fn mark_fired_one_shot_disables_and_clears_next_fire() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("once", "agent-1", false)).await.unwrap();

        store.mark_fired("once", None).await.unwrap();

        let got = store.get("once").await.unwrap();
        assert!(got.last_run_at.is_some());
        assert!(!got.enabled, "one-shot cron disables after firing");
        assert!(got.next_fire_at.is_none());
    }

    #[tokio::test]
    async fn mark_fired_webhook_keeps_next_fire_none() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(webhook_assignment("wh", "agent-1", Some("tok"))).await.unwrap();

        store.mark_fired("wh", None).await.unwrap();

        let got = store.get("wh").await.unwrap();
        assert!(got.last_run_at.is_some());
        assert!(got.enabled);
        assert!(got.next_fire_at.is_none());
    }

    #[tokio::test]
    async fn mark_fired_missing_id_is_noop() {
        let (_tmp, _root, store) = ready_store().await;
        // No row with this id — must not error.
        store.mark_fired("absent", None).await.unwrap();
    }

    #[tokio::test]
    async fn claim_dedicated_thread_id_creates_once_and_reuses_thereafter() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make = |calls: std::sync::Arc<std::sync::atomic::AtomicUsize>| {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok::<_, AoError>("thread-new".to_string())
                }
            }
        };

        let first = store
            .claim_dedicated_thread_id("a1", make(calls.clone()))
            .await
            .unwrap();
        assert_eq!(first, "thread-new");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Second call must reuse the claimed id without invoking create_thread again.
        let second = store
            .claim_dedicated_thread_id("a1", make(calls.clone()))
            .await
            .unwrap();
        assert_eq!(second, "thread-new");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "create_thread must not run again once a thread is claimed"
        );

        let persisted = store.get("a1").await.unwrap();
        assert_eq!(persisted.dedicated_thread_id.as_deref(), Some("thread-new"));
    }

    #[tokio::test]
    async fn claim_dedicated_thread_id_missing_assignment_errors() {
        let (_tmp, _root, store) = ready_store().await;
        let err = store
            .claim_dedicated_thread_id("ghost", || async { Ok::<_, AoError>("x".to_string()) })
            .await
            .unwrap_err();
        assert!(matches!(err, AoError::AssignmentNotFound(_)));
    }

    #[tokio::test]
    async fn clear_dedicated_thread_id_allows_reclaim() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();
        store
            .claim_dedicated_thread_id("a1", || async { Ok::<_, AoError>("thread-old".to_string()) })
            .await
            .unwrap();

        store.clear_dedicated_thread_id("a1").await.unwrap();
        let cleared = store.get("a1").await.unwrap();
        assert!(cleared.dedicated_thread_id.is_none());

        let reclaimed = store
            .claim_dedicated_thread_id("a1", || async { Ok::<_, AoError>("thread-new".to_string()) })
            .await
            .unwrap();
        assert_eq!(reclaimed, "thread-new");
    }

    #[tokio::test]
    async fn clear_dedicated_thread_id_missing_assignment_is_noop() {
        let (_tmp, _root, store) = ready_store().await;
        store.clear_dedicated_thread_id("ghost").await.unwrap();
    }

    // --- mark_evaluated / LivenessState ---

    #[tokio::test]
    async fn mark_evaluated_fired_sets_last_evaluated_at_and_increments_fire_count() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();
        let before = store.get("a1").await.unwrap();
        assert!(before.liveness.last_evaluated_at.is_none());
        assert_eq!(before.liveness.fire_count, 0);

        store
            .mark_evaluated("a1", EvaluationOutcome::Fired)
            .await
            .unwrap();

        let after = store.get("a1").await.unwrap();
        assert!(
            after.liveness.last_evaluated_at.is_some(),
            "a fire outcome must still stamp last_evaluated_at"
        );
        assert_eq!(after.liveness.fire_count, 1);
        assert!(after.liveness.last_quiescence.is_none());
    }

    #[tokio::test]
    async fn mark_evaluated_quiescent_sets_last_evaluated_at_and_last_quiescence_without_firing() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();

        let reason = QuiescenceReason::NotDue { next_fire_at: None };
        store
            .mark_evaluated("a1", EvaluationOutcome::Quiescent(reason.clone()))
            .await
            .unwrap();

        let after = store.get("a1").await.unwrap();
        assert!(
            after.liveness.last_evaluated_at.is_some(),
            "a no-fire outcome must ALSO stamp last_evaluated_at, not just a fire outcome"
        );
        assert_eq!(
            after.liveness.fire_count, 0,
            "fire_count must only increment on an actual fire"
        );
        assert_eq!(after.liveness.last_quiescence, Some(reason));
    }

    #[tokio::test]
    async fn mark_evaluated_fire_count_increments_only_on_fire_across_a_mixed_sequence() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();

        store
            .mark_evaluated(
                "a1",
                EvaluationOutcome::Quiescent(QuiescenceReason::NotDue { next_fire_at: None }),
            )
            .await
            .unwrap();
        store
            .mark_evaluated(
                "a1",
                EvaluationOutcome::Quiescent(QuiescenceReason::NotDue { next_fire_at: None }),
            )
            .await
            .unwrap();
        assert_eq!(store.get("a1").await.unwrap().liveness.fire_count, 0);

        store.mark_evaluated("a1", EvaluationOutcome::Fired).await.unwrap();
        assert_eq!(store.get("a1").await.unwrap().liveness.fire_count, 1);

        store
            .mark_evaluated(
                "a1",
                EvaluationOutcome::Quiescent(QuiescenceReason::NotDue { next_fire_at: None }),
            )
            .await
            .unwrap();
        assert_eq!(
            store.get("a1").await.unwrap().liveness.fire_count,
            1,
            "a subsequent quiescent tick must not touch fire_count"
        );
    }

    #[tokio::test]
    async fn mark_evaluated_fire_clears_a_previously_recorded_quiescence() {
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();

        store
            .mark_evaluated(
                "a1",
                EvaluationOutcome::Quiescent(QuiescenceReason::FireFailed {
                    reason: "boom".to_string(),
                }),
            )
            .await
            .unwrap();
        assert!(store.get("a1").await.unwrap().liveness.last_quiescence.is_some());

        store.mark_evaluated("a1", EvaluationOutcome::Fired).await.unwrap();

        assert!(
            store.get("a1").await.unwrap().liveness.last_quiescence.is_none(),
            "a fresh fire must clear whatever quiescence reason preceded it"
        );
    }

    #[tokio::test]
    async fn mark_evaluated_missing_id_is_noop() {
        let (_tmp, _root, store) = ready_store().await;
        store
            .mark_evaluated("absent", EvaluationOutcome::Fired)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mark_fired_routes_through_the_same_liveness_mutation_as_mark_evaluated() {
        // mark_fired is a production call site that never carries an
        // EvaluationOutcome explicitly (fire_assignment only ever fires) —
        // this asserts it still leaves LivenessState exactly as a direct
        // mark_evaluated(id, Fired) call would, i.e. the two paths agree.
        let (_tmp, _root, store) = ready_store().await;
        store.add(cron_assignment("a1", "agent-1", true)).await.unwrap();

        store.mark_fired("a1", None).await.unwrap();

        let got = store.get("a1").await.unwrap();
        assert!(got.liveness.last_evaluated_at.is_some());
        assert_eq!(got.liveness.fire_count, 1);
        assert!(got.liveness.last_quiescence.is_none());
    }

    #[tokio::test]
    async fn mark_polled_fired_true_also_updates_liveness_like_mark_evaluated() {
        let (_tmp, _root, store) = ready_store().await;
        store
            .add(connector_event_assignment("a1", "agent-1", 60))
            .await
            .unwrap();

        store
            .mark_polled("a1", Some("cursor-1".to_string()), true, 60)
            .await
            .unwrap();

        let got = store.get("a1").await.unwrap();
        assert!(got.liveness.last_evaluated_at.is_some());
        assert_eq!(got.liveness.fire_count, 1);
    }

    #[tokio::test]
    async fn mark_polled_fired_false_still_stamps_last_evaluated_at() {
        // mark_polled's no-fire branch has no QuiescenceReason to give
        // mark_evaluated (see its doc), but it must still record that the
        // tick loop looked at this assignment at all.
        let (_tmp, _root, store) = ready_store().await;
        store
            .add(connector_event_assignment("a1", "agent-1", 60))
            .await
            .unwrap();

        store
            .mark_polled("a1", None, false, 60)
            .await
            .unwrap();

        let got = store.get("a1").await.unwrap();
        assert!(got.liveness.last_evaluated_at.is_some());
        assert_eq!(got.liveness.fire_count, 0);
    }
}
