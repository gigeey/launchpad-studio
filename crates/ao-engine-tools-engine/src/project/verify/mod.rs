mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{
    EngineTool, LoadPolicy, PriorVerdict, RunnerContext, TasklistEvidence, ToolOutput, UserEvent,
    VerificationInput,
};
use ao_persistence::{changelog::ChangelogStore, tasklist_store::TasklistStore};
use ao_protocol::{
    data_root::resolve_data_root,
    error::AoError,
    project::{ProjectStatus, VerificationRecord, MAX_VERIFICATION_ROUNDS},
    tasklist::TasklistStatus,
};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;

/// Ask an independent verifier whether the project goal has been met.
///
/// Unlike the agent's own self-assessment, this tool issues a fresh model call
/// with no access to the conversation transcript — only the goal, spec, and the
/// objective completion summaries of every project-tagged tasklist. The verdict
/// is persisted so follow-up calls can build on settled history.
pub struct ProjectVerify;

#[async_trait]
impl EngineTool for ProjectVerify {
    fn name(&self) -> &str {
        "ProjectVerify"
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
                    "ProjectVerify is only available inside a project-scoped session.",
                    false,
                ));
            }
        };

        let store = match &ctx.project_store {
            Some(s) => s.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "Project store not available in this context.",
                    false,
                ));
            }
        };

        let mode = input
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("quick");

        // Select which engine to use:
        // - "quick" → single model call against tasklist summaries (fast, no file access).
        // - "full"  → isolated read-only inspection child that opens the working directory,
        //             reads diffs, and runs the test suite (required for ProjectComplete gate).
        let (engine, engine_label) = match mode {
            "full" => {
                let e = match &ctx.full_verification_engine {
                    Some(e) => e.clone(),
                    None => {
                        return Ok(ToolOutput::error(
                            "Full inspection verification is not available in this session. \
                             Ensure the project has a working_dir set and a provider is \
                             configured, then retry.",
                            true,
                        ));
                    }
                };
                (e, "full")
            }
            _ => {
                // mode = "quick" (default) or any unrecognised value falls through to quick.
                let e = match &ctx.verification_engine {
                    Some(e) => e.clone(),
                    None => {
                        return Ok(ToolOutput::error(
                            "Verification engine not available — no provider is configured for \
                             this session. Ensure a provider API key is set up and try again.",
                            false,
                        ));
                    }
                };
                (e, "quick")
            }
        };

        let extra_evidence = input
            .get("extra_evidence")
            .and_then(|v| v.as_str())
            .map(str::to_string);

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

        // Refuse to verify a project that is not in an actionable state.
        match &project.status {
            ProjectStatus::Active => {}
            ProjectStatus::NeedsReview => {
                return Ok(ToolOutput::error(
                    "This project has been escalated to NeedsReview and requires human \
                     attention before further verification. Contact the project owner.",
                    true,
                ));
            }
            ProjectStatus::Completed => {
                return Ok(ToolOutput::error(
                    "The project is already Completed.",
                    true,
                ));
            }
            ProjectStatus::Interviewing => {
                return Ok(ToolOutput::error(
                    "Cannot verify a project that is still Interviewing. \
                     Activate the project first via ProjectUpdate.",
                    true,
                ));
            }
            ProjectStatus::Archived => {
                return Ok(ToolOutput::error(
                    "Cannot verify an Archived project.",
                    false,
                ));
            }
        }

        // Only FULL verifications count toward the round cap. Quick checks are
        // cheap mid-flight compasses and do not burn formal rounds — the cap
        // exists to prevent runaway inspection costs, not to throttle summary
        // reads. This also means a quick verdict cannot artificially exhaust the
        // budget and prevent a legitimate full inspection from running.
        let rounds_used = project
            .verifications
            .iter()
            .filter(|v| v.engine == "full")
            .count();
        if rounds_used >= MAX_VERIFICATION_ROUNDS {
            project.status = ProjectStatus::NeedsReview;
            project.updated_at = Utc::now();
            if let Err(e) = store.save(&project).await {
                tracing::warn!("ProjectVerify: failed to persist NeedsReview status: {e}");
            }
            let _ = ctx
                .event_sink
                .emit(UserEvent::ProjectStateChanged {
                    project_id: project.id.clone(),
                    status: "needs_review".to_string(),
                    name: project.name.clone(),
                })
                .await;
            return Ok(ToolOutput::error(
                &format!(
                    "Verification round cap ({MAX_VERIFICATION_ROUNDS}) reached without a passing \
                     verdict. The project status has been set to 'needs_review'. Escalate to the \
                     project owner for manual review instead of retrying automatically."
                ),
                false,
            ));
        }

        // Gather tasklist evidence from disk.
        let evidence = load_tasklist_evidence(&ctx.agent_id, &project_id).await;

        // Build prior-verdict context.
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
            extra_evidence,
            working_dir: project.working_dir.as_deref().map(std::path::Path::new).map(std::path::PathBuf::from),
        };

        let verdict = match engine.verify(vi).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("verification call failed: {e}"),
                    true,
                ));
            }
        };

        // Round number counts across both quick and full verifications for the
        // display field; the round cap above only counts full runs.
        let total_rounds = project.verifications.len();
        let round = (total_rounds as u32) + 1;
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

        if let Err(e) = store.save(&project).await {
            tracing::warn!("ProjectVerify: failed to persist verification record: {e}");
        }

        let _ = ctx
            .event_sink
            .emit(UserEvent::ProjectStateChanged {
                project_id: project.id.clone(),
                status: serde_json::to_value(&project.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "active".to_string()),
                name: project.name.clone(),
            })
            .await;

        // `full_rounds_remaining` reflects how many inspection (full) rounds are
        // left before the project is escalated to NeedsReview. Quick runs are
        // not subtracted from this budget.
        let full_rounds_used = project
            .verifications
            .iter()
            .filter(|v| v.engine == "full")
            .count();
        Ok(ToolOutput::structured(serde_json::json!({
            "round": round,
            "engine": engine_label,
            "verdict": verdict.verdict,
            "confidence": verdict.confidence,
            "gaps": verdict.gaps,
            "rationale": verdict.rationale,
            "full_rounds_remaining": MAX_VERIFICATION_ROUNDS.saturating_sub(full_rounds_used),
        })))
    }
}

/// Load completion summaries for all terminal, project-tagged tasklists owned
/// by this agent. Falls back gracefully to an empty list if the data root or
/// tasklist store is unavailable.
pub(crate) async fn load_tasklist_evidence(
    agent_id: &str,
    project_id: &str,
) -> Vec<TasklistEvidence> {
    let data_root = match resolve_data_root() {
        Ok(p) => ao_persistence::paths::DataRoot::new(&p),
        Err(e) => {
            tracing::warn!("ProjectVerify: cannot resolve data root for evidence: {e}");
            return vec![];
        }
    };

    let tl_store = TasklistStore::new(data_root.clone());
    let tasklists = match tl_store.list_for_agent(agent_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("ProjectVerify: failed to list agent tasklists: {e}");
            return vec![];
        }
    };

    let terminal_statuses = [
        TasklistStatus::Completed,
        TasklistStatus::Failed,
        TasklistStatus::Cancelled,
    ];

    let project_lists: Vec<_> = tasklists
        .into_iter()
        .filter(|tl| {
            tl.project_id.as_deref() == Some(project_id)
                && terminal_statuses.contains(&tl.status)
        })
        .collect();

    let changelog = ChangelogStore::new(data_root);
    let mut evidence = Vec::with_capacity(project_lists.len());

    for tl in project_lists {
        let entries = changelog
            .read_recent(&tl.owner, &tl.id, usize::MAX)
            .await
            .unwrap_or_default();

        let all_tasks: Vec<_> = tl.groups.iter().flat_map(|g| g.tasks.iter()).collect();
        let summary = if entries.is_empty() && all_tasks.is_empty() {
            format!(
                "Tasklist '{}' finished with status {:?} — no task details available.",
                tl.title, tl.status
            )
        } else {
            let succeeded = all_tasks
                .iter()
                .filter(|t| t.status == ao_protocol::tasklist::TaskStatus::Completed)
                .count();
            let failed = all_tasks
                .iter()
                .filter(|t| t.status == ao_protocol::tasklist::TaskStatus::Failed)
                .count();
            let skipped = all_tasks
                .iter()
                .filter(|t| t.status == ao_protocol::tasklist::TaskStatus::Skipped)
                .count();

            let mut lines = vec![format!(
                "Tasklist '{}' finished: {} succeeded, {} failed, {} skipped.",
                tl.title, succeeded, failed, skipped
            )];

            for task in &all_tasks {
                let title = task.prompt.lines().next().unwrap_or(task.prompt.as_str());
                let status_str = format!("{:?}", task.status).to_lowercase();
                let task_summary = entries
                    .iter()
                    .find(|e| e.task_id == task.id)
                    .map(|e| e.summary.as_str())
                    .unwrap_or("(no summary)");
                lines.push(format!("  - {title} [{status_str}]: {task_summary}"));
            }

            lines.join("\n")
        };

        evidence.push(TasklistEvidence {
            title: tl.title.clone(),
            summary,
        });
    }

    evidence
}
