pub mod add;
pub mod cancel;
pub mod check_zombies;
pub mod comment;
pub mod complete;
pub mod create;
pub mod delete;
pub mod list;
mod owner_resolution;
pub mod requeue;
pub mod resume;
pub mod resume_task;
pub mod start;
pub mod stop_task;
pub mod update;

use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::{
    ClassifierHandle, ClassifierInFlight, ClassifyOutcome, TasklistServiceHandle,
};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_CLASSIFY_RETRIES: u32 = 3;
pub(crate) const CLASSIFY_RETRY_DELAYS_SECS: [u64; 3] = [5, 15, 45];

/// Computes the actual sleep between classifier retry attempts as `base_secs`
/// multiplied by a deterministic per-task jitter factor in `[0.9, 1.1]` —
/// i.e. ±10% spread.
///
/// The factor is derived from a hash of `task_id` and `attempt` so a single
/// task's sequence is reproducible across runs, but two sibling tasks spawned
/// in the same tokio instant land on different timer cells and stop hammering
/// the model in lockstep. Pure stdlib — no `rand` dependency.
fn jittered_retry_delay(base_secs: u64, task_id: &str, attempt: u32) -> Duration {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    task_id.hash(&mut h);
    attempt.hash(&mut h);
    // `frac` in `[0.0, 1.0)`, mapped to a multiplier in `[0.9, 1.1]`.
    let frac = (h.finish() % 1000) as f64 / 1000.0;
    let multiplier = 0.9 + 0.2 * frac;
    let secs_f = (base_secs as f64) * multiplier;
    // Clamp to a sensible floor of 1s so jitter cannot collapse a delay to 0.
    Duration::from_millis((secs_f * 1000.0).max(1000.0) as u64)
}

/// Runs a classifier call with bounded retries and writes the assignment back
/// via the CAS token. Always run as a detached `tokio::spawn` task; the
/// classifier reconciler will re-spawn this for any task that finishes with
/// `assignment: None` on its next tick.
///
/// `in_flight` is the process-wide dedup registry. When `Some`, this function
/// claims the `(agent, tasklist, task)` slot at entry; if the slot is already
/// claimed (another spawn is in flight for the same task) it returns silently
/// without touching the classifier. The claim is released on every exit path
/// via RAII, so the reconciler can retry the slot on its next tick.
///
/// On `Permanent` or retry-budget-exhausted outcomes the function exits cleanly
/// — it does NOT panic or mark the task as terminal. The classifier's job is
/// solely "assign a runner"; task lifecycle (success / failure / escalation)
/// is owned by the task feeder, which observes the still-`None` assignment on
/// the next reconciler tick and triggers another classification attempt.
///
/// Public because frontend-facing HTTP routes (`append_task`,
/// `create_tasklist`) need the same spawn semantics as the engine-side
/// `TodoCreate` / `TodoAdd` / `TodoUpdate` tools.
pub async fn classify_with_retry(
    classifier: Arc<dyn ClassifierHandle + Send + Sync>,
    svc: Arc<dyn TasklistServiceHandle + Send + Sync>,
    in_flight: Option<Arc<ClassifierInFlight>>,
    agent_id: String,
    tasklist_id: String,
    task_id: String,
    parent_agent_id: String,
    task_title: String,
    task_description: String,
    expected_token: u64,
) {
    // Claim the dedup slot up-front. If another spawn already owns it (e.g.
    // the event-driven TodoCreate spawn is still working through retries and
    // the reconciler just ticked) we drop on the floor — the in-flight spawn
    // will finish or release for the next tick to pick up.
    let _claim = match in_flight.as_ref() {
        Some(registry) => match registry.claim(&agent_id, &tasklist_id, &task_id) {
            Some(claim) => Some(claim),
            None => {
                tracing::debug!(
                    task_id = %task_id,
                    tasklist_id = %tasklist_id,
                    "classifier: slot already in flight, skipping spawn",
                );
                return;
            }
        },
        None => None,
    };

    let mut attempt: u32 = 0;
    loop {
        if attempt > 0 {
            let base = CLASSIFY_RETRY_DELAYS_SECS[(attempt - 1).min(2) as usize];
            tokio::time::sleep(jittered_retry_delay(base, &task_id, attempt)).await;
        }

        match classifier
            .classify(&parent_agent_id, &task_id, &task_title, &task_description)
            .await
        {
            ClassifyOutcome::Assigned(assignment) => {
                let owner_for_log = assignment.owner_agent_id.clone();
                match svc
                    .set_assignment(
                        &agent_id,
                        &tasklist_id,
                        &task_id,
                        Some(assignment),
                        expected_token,
                    )
                    .await
                {
                    Ok(true) => tracing::info!(
                        task_id = %task_id,
                        tasklist_id = %tasklist_id,
                        owner_agent_id = %owner_for_log,
                        attempt = attempt,
                        "classifier: assignment written",
                    ),
                    Ok(false) => tracing::info!(
                        task_id = %task_id,
                        tasklist_id = %tasklist_id,
                        attempt = attempt,
                        "classifier: stale token, write-back discarded",
                    ),
                    Err(e) => tracing::warn!(
                        task_id = %task_id,
                        tasklist_id = %tasklist_id,
                        "classifier: set_assignment error: {}",
                        e,
                    ),
                }
                return;
            }
            ClassifyOutcome::Permanent(reason) => {
                // Permanent here means "this attempt cannot succeed" (e.g.
                // parent agent missing at call time, schema-violating output).
                // We still want the reconciler to retry — conditions might
                // change between ticks (agent profile reloaded, address book
                // updated). Logging at warn level so an actually-broken
                // configuration shows up in operator logs.
                tracing::warn!(
                    task_id = %task_id,
                    tasklist_id = %tasklist_id,
                    attempt = attempt,
                    "classifier: permanent failure on this attempt — reconciler will retry: {}",
                    reason,
                );
                return;
            }
            ClassifyOutcome::Retryable(reason) => {
                if attempt < MAX_CLASSIFY_RETRIES {
                    tracing::info!(
                        task_id = %task_id,
                        tasklist_id = %tasklist_id,
                        attempt = attempt,
                        "classifier: retryable ({}), will retry",
                        reason,
                    );
                    attempt += 1;
                    continue;
                }
                tracing::warn!(
                    task_id = %task_id,
                    tasklist_id = %tasklist_id,
                    "classifier: retry budget exhausted after {} attempts — reconciler will retry on next tick: {}",
                    MAX_CLASSIFY_RETRIES + 1,
                    reason,
                );
                return;
            }
        }
    }
}

/// A single item in a TodoCreate/TodoAdd request.
#[derive(Debug, Clone)]
pub struct TodoItem {
    pub title: String,
    pub brief: String,
    pub owner: Option<String>,
}

/// Execution mode for a group of todo items (sequential vs parallel dispatch).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum TodoMode {
    #[default]
    Seq,
    Par,
}

/// Dispatch mode for a TodoCreate call: whether the tool call blocks until the
/// tasklist reaches a terminal state (sync) or returns immediately (async).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TodoDispatchMode {
    #[default]
    Sync,
    Async,
}
