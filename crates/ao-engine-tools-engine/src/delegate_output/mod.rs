mod prompt;
#[cfg(test)]
mod tests;

use std::time::Duration;

use ao_engine_tools_core::background_agents::{BackgroundAgentHandle, BackgroundAgentId, TaskFinalReport, TaskFinalStatus};
use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use ao_protocol::transcript::TranscriptEntry;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use tokio::sync::broadcast::error::TryRecvError;

/// Engine tool that polls a live async delegation for new events or retrieves
/// the final result of a completed one.
///
/// Pairs with an async [`Delegate`](crate::delegate::Delegate) call: that call
/// returns a `delegation_id`, and this tool polls it. Each call drains only the
/// events emitted since the previous poll — the per-handle broadcast receiver
/// acts as the cursor. Re-inserting the handle after a "running" poll preserves
/// the advanced cursor position.
///
/// The handle is reaped (removed from the registry) when the delegation has
/// completed or been cancelled. If the in-memory handle is gone — after a server
/// restart, or (far more commonly) because the parent's per-session registry was
/// dropped at a CLI continuation-step boundary while the delegate kept running —
/// the tool falls back to the persisted sidechain transcript at
/// `<data_root>/messages/data/<id>.jsonl`.
///
/// That fallback reports `completed`, `failed`, or `cancelled` **only** when the
/// transcript actually contains a terminal event line. When it does not, the
/// outcome is reported as `indeterminate` with the observed last-activity age
/// and event count, because an async delegate is an in-process task with no OS
/// pid and its liveness therefore cannot be probed. Asserting `failed` there
/// would mean reporting a healthy, still-running delegate as dead.
pub struct DelegateOutput;

#[async_trait]
impl EngineTool for DelegateOutput {
    fn name(&self) -> &str {
        "DelegateOutput"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let id_str = match input.get("id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return Ok(ToolOutput::error("missing required field: id", true)),
        };

        let bg_id: BackgroundAgentId = match id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                return Ok(ToolOutput::error(
                    format!("invalid background agent id: {e}"),
                    false,
                ));
            }
        };

        let wait_secs = input
            .get("wait_seconds")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
            .clamp(0.0, 120.0);

        // Remove from registry to gain mutable access to events and join handle.
        let mut handle = match ctx.background_agents.remove(&bg_id).await {
            Some(h) => h,
            None => return recover_from_transcript(&bg_id).await,
        };

        // Drain all events emitted since the last poll via the per-handle cursor.
        let mut events: Vec<Value> = Vec::new();
        drain_events(&mut handle, &mut events);

        if handle.join.is_finished() {
            // Child is done — await the already-resolved handle (non-blocking).
            let report = match handle.join.await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return Ok(ToolOutput::error(format!("subagent runner error: {e}"), false)),
                Err(e) => return Ok(ToolOutput::error(format!("subagent task panicked: {e}"), false)),
            };
            // Handle is not re-inserted — it is reaped here.
            return Ok(terminal_output(report, events));
        }

        // Child is still running. Block up to wait_secs before returning.
        if wait_secs > 0.0 {
            let deadline = Duration::from_millis((wait_secs * 1000.0) as u64);
            match tokio::time::timeout(deadline, &mut handle.join).await {
                Ok(join_result) => {
                    // Child finished within the deadline — collect any trailing events.
                    drain_events(&mut handle, &mut events);
                    return match join_result {
                        Ok(Ok(report)) => Ok(terminal_output(report, events)),
                        Ok(Err(e)) => Ok(ToolOutput::error(format!("subagent runner error: {e}"), false)),
                        Err(e) => Ok(ToolOutput::error(format!("subagent task panicked: {e}"), false)),
                    };
                }
                Err(_) => {
                    // Deadline reached — collect any events that arrived during the wait,
                    // re-insert the handle (cursor preserved), and invite the caller to retry.
                    drain_events(&mut handle, &mut events);
                    let _ = ctx.background_agents.insert(handle).await;
                    return Ok(ToolOutput::structured(serde_json::json!({
                        "status": "running",
                        "events": events,
                        "hint": "Delegation is still running. Call DelegateOutput again with wait_seconds instead of polling in a tight loop.",
                    })));
                }
            }
        }

        // Instant poll (wait_seconds == 0): re-insert handle with its advanced cursor.
        // insert can only fail at cap; we freed a slot by removing, so this succeeds.
        let _ = ctx.background_agents.insert(handle).await;
        Ok(ToolOutput::structured(serde_json::json!({
            "status": "running",
            "events": events,
        })))
    }
}

/// Drain all buffered events from the handle's broadcast receiver into `events`.
fn drain_events(handle: &mut BackgroundAgentHandle, events: &mut Vec<Value>) {
    loop {
        match handle.events.try_recv() {
            Ok(event) => {
                if let Ok(v) = serde_json::to_value(&event) {
                    events.push(v);
                }
            }
            // Advance past any overwritten slots and keep draining.
            Err(TryRecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
}

/// Build the structured terminal output for a completed, failed, or cancelled run.
fn terminal_output(report: TaskFinalReport, events: Vec<Value>) -> ToolOutput {
    let stats = build_stats_object(report.duration_ms, report.num_turns);
    match report.status {
        TaskFinalStatus::Cancelled => ToolOutput::structured(serde_json::json!({
            "status": "cancelled",
            "events": events,
            "stats": stats,
        })),
        TaskFinalStatus::Completed => ToolOutput::structured(serde_json::json!({
            "status": "completed",
            "final_result": report.final_assistant_text,
            "events": events,
            "stats": stats,
        })),
        TaskFinalStatus::Failed => ToolOutput::structured(serde_json::json!({
            "status": "failed",
            "error": report.error_message
                .unwrap_or_else(|| "delegation failed without an error message".to_string()),
            "final_result": report.final_assistant_text,
            "events": events,
            "stats": stats,
        })),
    }
}

/// Build the stats JSON object from optional duration and turn-count fields.
///
/// Returns `serde_json::Value::Null` when neither value is available so
/// callers downstream can omit the field or display a meaningful absence.
fn build_stats_object(duration_ms: Option<u64>, num_turns: Option<u32>) -> Value {
    match (duration_ms, num_turns) {
        (None, None) => Value::Null,
        _ => serde_json::json!({
            "duration_ms": duration_ms,
            "num_turns": num_turns,
        }),
    }
}

/// Machine-readable `reason` codes that accompany an `indeterminate` status.
///
/// Each names a physically distinct situation. They are deliberately separate
/// strings: collapsing them back into one would reintroduce exactly the
/// ambiguity this fallback exists to remove.
const REASON_NO_TRANSCRIPT: &str = "no-transcript-found";
const REASON_NO_TERMINAL_EVENT: &str = "running-or-orphaned-no-terminal-event";
const REASON_TRANSCRIPT_UNREADABLE: &str = "transcript-unreadable";
const REASON_DATA_ROOT_UNAVAILABLE: &str = "data-root-unavailable";

/// Whether a transcript `event_type` marks a run as having actually reached a
/// terminal state.
///
/// This predicate is the sole gate on reporting `completed`, `failed`, or
/// `cancelled` from a recovered transcript. Without one of these lines we have
/// observed no outcome and must not invent one. Every progress event —
/// `response`, `tool_use`, `async_launched` (the spawn marker), `text_complete`
/// — is non-terminal by construction.
fn is_terminal_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "session_completed" | "session_cancelled" | "session_failed"
    )
}

/// Render an age in seconds as a compact human string ("42s", "14m", "2h 5m").
fn format_age(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// Build the `indeterminate` output: no terminal event was observed, so the
/// delegation's outcome is genuinely unknown.
///
/// An async delegate is an in-process task with no OS pid, so its liveness
/// cannot be probed. The honest answer is "unknown", reported together with
/// everything we *did* observe (age of the last activity, how many events
/// landed) so the caller can judge for itself. This branch must never assert a
/// verdict — reporting a live delegate as `failed` is the bug it replaces.
fn indeterminate_output(
    reason: &str,
    observation: String,
    last_event_at: Option<chrono::DateTime<Utc>>,
    event_count: usize,
    last_response: Option<String>,
    transcript_path: String,
) -> ToolOutput {
    let age_seconds = last_event_at.map(|ts| (Utc::now() - ts).num_seconds().max(0));
    ToolOutput::structured(serde_json::json!({
        "status": "indeterminate",
        "reason": reason,
        "hint": observation,
        "last_event_at": last_event_at.map(|ts| ts.to_rfc3339()),
        "last_activity_age_seconds": age_seconds,
        "event_count": event_count,
        "final_result": last_response,
        "transcript_path": transcript_path,
    }))
}

/// Fallback path when the in-memory handle is absent — the common case, not an
/// edge case: the registry is owned per-MCP-session, so a CLI-backed parent
/// drops it at its very next continuation step while the delegate keeps running.
///
/// Reads the persisted sidechain transcript at
/// `<data_root>/messages/data/<id>.jsonl`. Reports `completed`/`failed`/
/// `cancelled` **only** when a terminal event line is actually present;
/// everything else is `indeterminate`. A well-formed id with no transcript is
/// not an error — it means nothing has been persisted yet.
async fn recover_from_transcript(bg_id: &BackgroundAgentId) -> Result<ToolOutput, AoError> {
    let data_root = match ao_protocol::data_root::resolve_data_root() {
        Ok(p) => p,
        Err(e) => {
            // Our own inability to locate the transcript says nothing about the
            // delegate. Report unknown, not failure, and not a bad id.
            return Ok(indeterminate_output(
                REASON_DATA_ROOT_UNAVAILABLE,
                format!(
                    "Cannot determine the outcome of delegation '{bg_id}': the data root \
                     could not be resolved ({e}), so its transcript is unreachable. This \
                     is an environment problem, not evidence about the delegation."
                ),
                None,
                0,
                None,
                String::new(),
            ));
        }
    };

    let path = data_root
        .join("messages")
        .join("data")
        .join(format!("{}.jsonl", bg_id));

    let contents = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No transcript yet. The first event may simply not have landed —
            // this is NOT proof that the id is invalid.
            return Ok(indeterminate_output(
                REASON_NO_TRANSCRIPT,
                format!(
                    "No transcript found for delegation '{bg_id}' at {}. Either no event has \
                     been persisted yet, or the id belongs to another data root. Outcome \
                     unknown — do not assume it failed.",
                    path.display()
                ),
                None,
                0,
                None,
                path.display().to_string(),
            ));
        }
        Err(e) => {
            return Ok(indeterminate_output(
                REASON_TRANSCRIPT_UNREADABLE,
                format!(
                    "Transcript for delegation '{bg_id}' at {} could not be read ({e}). \
                     Outcome unknown.",
                    path.display()
                ),
                None,
                0,
                None,
                path.display().to_string(),
            ));
        }
    };

    let mut last_response: Option<String> = None;
    // (event_type, content) of the first terminal line encountered
    let mut terminal: Option<(String, String)> = None;
    let mut last_event_at: Option<chrono::DateTime<Utc>> = None;
    let mut event_count: usize = 0;

    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) else {
            continue;
        };
        // Every parsed line counts as observed activity, terminal or not — this
        // is what lets the indeterminate branch report a real last-activity age.
        event_count += 1;
        last_event_at = Some(entry.ts);
        if terminal.is_none() {
            if is_terminal_event_type(&entry.event_type) {
                terminal = Some((entry.event_type, entry.content));
            } else if entry.event_type == "response" {
                // timeline_adapter writes "response" for every text-completion
                // turn (both CLI and native runners). "text_complete" is the
                // internal streaming event name.
                last_response = Some(entry.content);
            }
        }
    }

    let recovery_note = format!("recovered from transcript at {}", path.display());

    match terminal {
        // The headline case: the transcript exists and is growing, but no
        // terminal event has been written. The delegate is most likely still
        // running (or orphaned by a step boundary). Report what we saw, not a
        // verdict.
        None => {
            let age = last_event_at.map(|ts| (Utc::now() - ts).num_seconds().max(0));
            let age_phrase = match age {
                Some(secs) => format!("last activity {} ago", format_age(secs)),
                None => "no parseable events yet".to_string(),
            };
            Ok(indeterminate_output(
                REASON_NO_TERMINAL_EVENT,
                format!(
                    "Delegation '{bg_id}' is still running or was orphaned; {age_phrase} \
                     ({event_count} events). No terminal event in the transcript at {}, so \
                     its outcome is not yet known. Poll again with wait_seconds; do not \
                     treat this as a failure.",
                    path.display()
                ),
                last_event_at,
                event_count,
                last_response,
                path.display().to_string(),
            ))
        }
        Some((event_type, content)) => match event_type.as_str() {
            "session_completed" => Ok(ToolOutput::structured(serde_json::json!({
                "status": "completed",
                "final_result": last_response,
                "recovered_from_transcript": recovery_note,
            }))),
            "session_cancelled" => Ok(ToolOutput::structured(serde_json::json!({
                "status": "cancelled",
                "recovered_from_transcript": recovery_note,
            }))),
            _ => Ok(ToolOutput::structured(serde_json::json!({
                "status": "failed",
                "error": if content.is_empty() {
                    "delegation failed without an error message".to_string()
                } else {
                    content
                },
                "final_result": last_response,
                "recovered_from_transcript": recovery_note,
            }))),
        },
    }
}
