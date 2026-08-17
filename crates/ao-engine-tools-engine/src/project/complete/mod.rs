mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{
    EngineTool, LoadPolicy, PriorVerdict, RunnerContext, ToolOutput, UserEvent, VerificationInput,
};
use ao_persistence::tasklist_store::TasklistStore;
use ao_protocol::{
    data_root::resolve_data_root,
    error::AoError,
    project::{ProjectStatus, VerificationRecord},
    tasklist::TasklistStatus,
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;

use crate::project::verify::load_tasklist_evidence;

/// Terminal action: mark the project Completed and record a final summary.
///
/// Requires a passing verification that is newer than the most recent completed
/// project-tagged tasklist. If no such verification exists, `ProjectComplete`
/// automatically runs a quick verification; on failure it returns a recoverable
/// error whose `gaps` field tells the agent what still needs to be fixed.
pub struct ProjectComplete;

#[async_trait]
impl EngineTool for ProjectComplete {
    fn name(&self) -> &str {
        "ProjectComplete"
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
        let project_id = match &ctx.project_id {
            Some(id) => id.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "ProjectComplete is only available inside a project-scoped session. \
                     This run has no project scope.",
                    false,
                ));
            }
        };

        let store = match &ctx.project_store {
            Some(s) => s,
            None => {
                return Ok(ToolOutput::error(
                    "Project store not available in this context.",
                    false,
                ));
            }
        };

        let summary = match input.get("summary").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => {
                return Ok(ToolOutput::error(
                    "missing or empty required field: summary",
                    true,
                ));
            }
        };

        let mut project = match store.get(&project_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Ok(ToolOutput::error(
                    &format!("project '{}' not found", project_id),
                    false,
                ));
            }
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to load project: {e}"),
                    false,
                ));
            }
        };

        match &project.status {
            ProjectStatus::Active => {}
            ProjectStatus::Interviewing => {
                return Ok(ToolOutput::error(
                    "Cannot complete a project that is still Interviewing. \
                     Call ProjectUpdate with activate=true first to make it Active, \
                     then complete the goal before calling ProjectComplete.",
                    true,
                ));
            }
            ProjectStatus::Completed => {
                return Ok(ToolOutput::error(
                    "The project is already Completed.",
                    true,
                ));
            }
            ProjectStatus::Archived => {
                return Ok(ToolOutput::error(
                    "Cannot complete an Archived project.",
                    false,
                ));
            }
            ProjectStatus::NeedsReview => {
                return Ok(ToolOutput::error(
                    "This project has been escalated to NeedsReview and requires human \
                     attention before it can be completed.",
                    true,
                ));
            }
        }

        // --- Verification gate ---
        // Find the timestamp of the most recently completed project-tagged tasklist.
        let last_tasklist_at =
            most_recent_tasklist_completion(&ctx.agent_id, &project_id).await;

        // A verification is valid when: it passed AND it is newer than the most
        // recently completed tasklist (so a new batch of work cannot be silently
        // skipped). Only a FULL (inspection) verification satisfies the gate —
        // a quick summary-only check can miss false claims made in tasklist
        // completion summaries. Quick verifications are useful for mid-flight
        // gap checks but must not be used as the final sign-off.
        let required_engine: Option<&str> = Some("full");
        let passing_verification =
            find_passing_verification(&project.verifications, required_engine);

        let gate_passed = match (passing_verification, last_tasklist_at) {
            (Some(v), Some(tl_ts)) => v.timestamp > tl_ts,
            (Some(_), None) => true,
            (None, _) => false,
        };

        if !gate_passed {
            // The gate requires a passing FULL (inspection) verification.
            // Only the full engine produces a record that satisfies the gate.
            // If the full engine is not available, return a clear error rather
            // than running a quick auto-verify that would not satisfy the gate
            // anyway — doing so would silently permit completion after a
            // summary-only check.
            let engine = match &ctx.full_verification_engine {
                Some(e) => e.clone(),
                None => {
                    return Ok(ToolOutput::error(
                        "ProjectComplete requires a passing full (inspection) verification, \
                         but no full engine is available. Call ProjectVerify with \
                         mode='full' to run the inspection verifier, then retry \
                         ProjectComplete.",
                        true,
                    ));
                }
            };
            let engine_label = "full";

            let evidence = load_tasklist_evidence(&ctx.agent_id, &project_id).await;
            let prior_verdicts: Vec<PriorVerdict> = project
                .verifications
                .iter()
                .map(|r| PriorVerdict {
                    round: r.round,
                    verdict: r.verdict.clone(),
                    gaps: r.gaps.clone(),
                })
                .collect();

            let vi = VerificationInput {
                project_id: project_id.clone(),
                goal: project.goal.clone(),
                spec: project.spec.clone(),
                tasklist_evidence: evidence,
                prior_verdicts,
                extra_evidence: None,
                working_dir: project.working_dir.as_deref().map(std::path::Path::new).map(std::path::PathBuf::from),
            };

            let verdict = match engine.verify(vi).await {
                Ok(v) => v,
                Err(e) => {
                    return Ok(ToolOutput::error(
                        &format!(
                            "ProjectComplete: auto-verification failed: {e}. \
                             Call ProjectVerify manually and address any gaps before retrying."
                        ),
                        true,
                    ));
                }
            };

            let round = (project.verifications.len() as u32) + 1;
            let record = VerificationRecord {
                round,
                timestamp: Utc::now(),
                verdict: verdict.verdict.clone(),
                gaps: verdict.gaps.clone(),
                confidence: verdict.confidence.clone(),
                rationale: verdict.rationale.clone(),
                engine: engine_label.to_string(),
            };
            project.verifications.push(record);
            project.updated_at = Utc::now();

            if verdict.verdict != "pass" {
                if let Err(e) = store.save(&project).await {
                    tracing::warn!("ProjectComplete: failed to persist failed verification: {e}");
                }
                let gaps_text = if verdict.gaps.is_empty() {
                    String::new()
                } else {
                    format!(
                        " Gaps:\n{}",
                        verdict
                            .gaps
                            .iter()
                            .map(|g| format!("  - {g}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                };
                return Ok(ToolOutput::error(
                    &format!(
                        "Verification did not pass (confidence: {}). The project goal is not \
                         yet met.{gaps_text} Create follow-up tasklists for each gap, complete \
                         them, then call ProjectVerify and ProjectComplete again.",
                        verdict.confidence
                    ),
                    true,
                ));
            }

            if let Err(e) = store.save(&project).await {
                return Ok(ToolOutput::error(
                    &format!("failed to persist verification: {e}"),
                    false,
                ));
            }
        }

        // Reload after any in-place saves above.
        project = match store.get(&project_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Ok(ToolOutput::error(
                    &format!("project '{}' disappeared after verification", project_id),
                    false,
                ));
            }
            Err(e) => {
                return Ok(ToolOutput::error(&format!("failed to reload project: {e}"), false));
            }
        };

        project.status = ProjectStatus::Completed;
        project.summary = Some(summary.clone());
        project.updated_at = Utc::now();

        if let Err(e) = store.save(&project).await {
            return Ok(ToolOutput::error(
                &format!("failed to save project: {e}"),
                false,
            ));
        }

        let _ = ctx
            .event_sink
            .emit(UserEvent::ProjectStateChanged {
                project_id: project.id.clone(),
                status: "completed".to_string(),
                name: project.name.clone(),
            })
            .await;

        Ok(ToolOutput::structured(serde_json::json!({
            "id": project.id,
            "name": project.name,
            "status": "completed",
            "summary": summary,
        })))
    }
}

/// Return the most recent passing verification from a list, optionally filtering
/// by engine type. `required_engine = None` accepts any engine; `Some("full")`
/// restricts to the inspection-subagent engine.
fn find_passing_verification<'a>(
    verifications: &'a [VerificationRecord],
    required_engine: Option<&str>,
) -> Option<&'a VerificationRecord> {
    verifications
        .iter()
        .rev()
        .find(|v| v.verdict == "pass" && required_engine.map_or(true, |e| v.engine == e))
}

/// Return the most recent `last_active_at` timestamp across all terminal
/// project-tagged tasklists owned by this agent.
async fn most_recent_tasklist_completion(
    agent_id: &str,
    project_id: &str,
) -> Option<chrono::DateTime<Utc>> {
    let data_root = resolve_data_root().ok()?;
    let data_root = ao_persistence::paths::DataRoot::new(&data_root);
    let tl_store = TasklistStore::new(data_root);
    let tasklists = tl_store.list_for_agent(agent_id).await.ok()?;
    let terminal = [
        TasklistStatus::Completed,
        TasklistStatus::Failed,
        TasklistStatus::Cancelled,
    ];
    tasklists
        .into_iter()
        .filter(|tl| {
            tl.project_id.as_deref() == Some(project_id) && terminal.contains(&tl.status)
        })
        .filter_map(|tl| tl.last_active_at)
        .max()
}
