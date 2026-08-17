mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use ao_protocol::tasklist::{AssignmentMode, TaskAssignment, TaskStatus, TasklistOwner};
use async_trait::async_trait;
use serde_json::Value;

pub struct TodoUpdate;

#[async_trait]
impl EngineTool for TodoUpdate {
    fn name(&self) -> &str {
        "TodoUpdate"
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

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let svc = match &ctx.tasklist_service {
            Some(s) => Arc::clone(s),
            None => {
                return Ok(ToolOutput::error(
                    "Tasklist service not available in this context.",
                    false,
                ));
            }
        };

        let active = match svc.agent_active(&ctx.agent_id).await {
            Ok(Some(tl)) => tl,
            Ok(None) => {
                return Ok(ToolOutput::error(
                    "No active tasklist found. Use TodoCreate to create one first.",
                    true,
                ));
            }
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to check for active tasklist: {e}"),
                    false,
                ));
            }
        };

        let task_id = match input.get("task_id").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => {
                return Ok(ToolOutput::error(
                    "missing or empty required field: task_id",
                    true,
                ));
            }
        };

        let prompt_update = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let owner_update = match input.get("owner").and_then(|v| v.as_str()) {
            Some(raw) => {
                match super::owner_resolution::resolve_owner(
                    ctx.agent_profile_store.as_ref(),
                    &ctx.agent_id,
                    raw,
                )
                .await
                {
                    Ok(resolved) => Some(resolved),
                    Err(out) => return Ok(out),
                }
            }
            None => None,
        };

        let expected_outputs = input.get("expected_outputs").and_then(|v| v.as_array()).map(
            |arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            },
        );

        if prompt_update.is_none() && owner_update.is_none() && expected_outputs.is_none() {
            return Ok(ToolOutput::error(
                "at least one of prompt, owner, or expected_outputs must be provided",
                true,
            ));
        }

        // Snapshot task state before the update to evaluate the re-classify gate.
        // status and assignment.mode are not changed by update_task_for_agent, so
        // reading them from the pre-update snapshot is equivalent to post-update.
        let pre_update_task = active
            .groups
            .iter()
            .flat_map(|g| g.tasks.iter())
            .find(|t| t.id == task_id)
            .cloned();

        // Capture whether the prompt/owner changed before moving the values into the call.
        let prompt_changed = prompt_update.is_some();
        let new_prompt_for_classify = prompt_update.clone();
        let owner_changed = owner_update.is_some();
        let new_owner_for_pin = owner_update.clone();

        match svc
            .update_task_for_agent(
                &ctx.agent_id,
                &active.id,
                &task_id,
                prompt_update,
                owner_update,
                expected_outputs,
            )
            .await
        {
            Ok(_) => {
                if let Some(pre_task) = pre_update_task {
                    // Write-path fix: `update_task_for_agent` only writes the base
                    // `owner_agent_id` field, but the feeder's
                    // `resolve_executor_agent_id` reads `task.assignment.owner_agent_id`
                    // as authoritative for Agent-owned tasklists (falling back to the
                    // base field only when `assignment` is absent). Without this, an
                    // owner-only update never reroutes dispatch. Pin a fresh assignment
                    // here, mirroring TodoAdd's pin-at-creation semantics: an explicit
                    // owner change is a deliberate pin the classifier must never
                    // re-stomp. Team-owned tasklists are untouched — the feeder reads
                    // the base field directly for those, which the update above already
                    // covers. This fires regardless of task status: even for a
                    // non-Pending task (already dispatched or terminal) we still write
                    // the assignment so a later retry/re-dispatch honors the new owner;
                    // we never touch `status`, so a running executor is never preempted.
                    if let (Some(new_owner), TasklistOwner::Agent { .. }) =
                        (new_owner_for_pin.as_ref(), &active.owner)
                    {
                        let current_token = pre_task.classifier_token;
                        match svc
                            .set_assignment(
                                &ctx.agent_id,
                                &active.id,
                                &task_id,
                                Some(TaskAssignment {
                                    owner_agent_id: new_owner.clone(),
                                    mode: AssignmentMode::Pinned,
                                }),
                                current_token,
                            )
                            .await
                        {
                            Ok(true) => tracing::debug!(
                                task_id = %task_id,
                                owner = %new_owner,
                                "TodoUpdate: pinned new owner assignment"
                            ),
                            Ok(false) => tracing::debug!(
                                task_id = %task_id,
                                "TodoUpdate: owner-pin CAS stale (concurrent classifier/edit), skipping"
                            ),
                            Err(e) => tracing::warn!(
                                task_id = %task_id,
                                "TodoUpdate: owner-pin set_assignment error: {}",
                                e
                            ),
                        }
                    }

                    // Re-classify gate: only fires for Pending + Classified + prompt
                    // changed, and only when this call did not also pin a new owner —
                    // an explicit owner change already resolves the assignment above,
                    // and the classifier must never re-stomp a pin.
                    let should_reclassify = !owner_changed
                        && pre_task.status == TaskStatus::Pending
                        && matches!(
                            pre_task.assignment.as_ref().map(|a| a.mode),
                            Some(AssignmentMode::Classified)
                        )
                        && prompt_changed;

                    if should_reclassify {
                        if let Some(classifier) = ctx.classifier.as_ref() {
                            let current_token = pre_task.classifier_token;
                            match svc
                                .set_assignment(
                                    &ctx.agent_id,
                                    &active.id,
                                    &task_id,
                                    None,
                                    current_token,
                                )
                                .await
                            {
                                Ok(true) => {
                                    // CAS succeeded: bump token and spawn fresh classifier.
                                    let prompt_text =
                                        new_prompt_for_classify.as_deref().unwrap_or("");
                                    let mut parts = prompt_text.splitn(2, ": ");
                                    let task_title = parts.next().unwrap_or("").to_string();
                                    let task_desc = parts.next().unwrap_or("").to_string();
                                    tokio::spawn(super::classify_with_retry(
                                        Arc::clone(classifier),
                                        Arc::clone(&svc),
                                        ctx.classifier_in_flight.clone(),
                                        ctx.agent_id.clone(),
                                        active.id.clone(),
                                        task_id.clone(),
                                        ctx.agent_id.clone(),
                                        task_title,
                                        task_desc,
                                        current_token + 1,
                                    ));
                                    tracing::debug!(
                                        task_id = %task_id,
                                        "TodoUpdate: re-classify spawned after prompt edit"
                                    );
                                }
                                Ok(false) => tracing::debug!(
                                    task_id = %task_id,
                                    "TodoUpdate: re-classify CAS stale, skipping spawn"
                                ),
                                Err(e) => tracing::warn!(
                                    task_id = %task_id,
                                    "TodoUpdate: re-classify set_assignment error: {}",
                                    e
                                ),
                            }
                        }
                    }
                }
                Ok(ToolOutput::text(format!(
                    "task '{}' updated successfully",
                    task_id
                )))
            }
            Err(AoError::TaskNotFound(_)) => Ok(ToolOutput::error(
                &format!("task '{}' not found in the active tasklist", task_id),
                true,
            )),
            Err(e) => Ok(ToolOutput::error(
                &format!("failed to update task: {e}"),
                false,
            )),
        }
    }
}
