use ao_persistence::projects::ProjectStore;
use ao_protocol::project::{Project, ProjectStatus};

/// Build the project context block appended to the system prompt of every
/// project-scoped run.
///
/// Contains sections describing the project goal, spec, working directory,
/// and attachments, plus a status-dependent role block that teaches the agent
/// what its current job is:
///
/// - `Interviewing` → gather information from the user to produce a spec,
///   then activate the project once the spec is complete.
/// - `Active` → drive the goal autonomously via dispatched tasklists; mark
///   the project complete only when the goal is genuinely met.
/// - `NeedsReview` → summarise and wait for the project owner.
/// - Other statuses → no role block is appended; the agent behaves normally.
///
/// PLACEMENT CONTRACT: the agent runners append this block AFTER the
/// composed system prompt (mirroring the tasklist preamble) for runs with
/// `RunScope::Project`. Do NOT inject project context by mutating the
/// profile's legacy `system_prompt` field before a run — the system-prompt
/// composer rebuilds the prompt from the profile's `persona` /
/// `special_instructions` fields and discards the legacy field, so anything
/// stuffed there is silently dropped.
pub fn build_project_context_block(project: &Project) -> String {
    let mut sections: Vec<String> = Vec::new();

    let mut project_block = format!(
        "## Project Context\n\n\
         You are operating within a long-running project workspace.\n\n\
         - **Project ID**: `{}`\n\
         - **Name**: {}\n\
         - **Goal**: {}\n",
        project.id, project.name, project.goal
    );

    if let Some(ref spec) = project.spec {
        if !spec.trim().is_empty() {
            project_block.push_str(&format!("- **Spec**:\n\n{}\n", spec));
        }
    }

    if let Some(ref working_dir) = project.working_dir {
        if !working_dir.trim().is_empty() {
            project_block.push_str(&format!("- **Working directory**: `{}`\n", working_dir));
        }
    }

    if !project.attachments.is_empty() {
        project_block.push_str("- **Attachments**:\n");
        for path in &project.attachments {
            project_block.push_str(&format!("  - `{}`\n", path));
        }
    }

    sections.push(project_block);

    match project.status {
        ProjectStatus::Interviewing => {
            sections.push(
                "## Your Current Role: Interview\n\n\
                 The project has not yet been fully specified. Your job right now is to \
                 interview the user to gather what is needed for a complete, actionable spec.\n\n\
                 Guidelines:\n\
                 - Be efficient — ask only what materially affects how the goal will be executed.\n\
                 - Do not ask for information you can reasonably infer from the goal.\n\
                 - When you have gathered sufficient information to produce a complete spec, \
                   record the spec and activate the project. A dedicated tool for this will be \
                   available to you; use it as soon as the spec is ready.\n\n\
                 **Do not do the work yourself.** Interviewing is a planning phase: do not \
                 start executing the goal — no code edits, no file writes, no implementation \
                 commands. Light read-only exploration to ask sharper questions is fine. Your \
                 outputs in this phase are questions, the spec, the project name update, and \
                 project activation. All execution happens after activation, and exclusively \
                 through dispatched tasklists (`TodoCreate`) — never performed directly by you.\n\n\
                 **Project naming:** In your first reply, check the project name shown above. \
                 If it looks like a rough snippet of the goal text rather than an intentional \
                 title, choose a concise name of five words or fewer that captures the essence \
                 of the work, pick a fitting emoji, and call the `ProjectUpdate` tool with the \
                 `name` and `emoji` fields before or alongside your opening message."
                    .to_string(),
            );
        }
        ProjectStatus::Active => {
            sections.push(
                "## Your Current Role: Autonomous Orchestration\n\n\
                 The project spec is set and you are the autonomous driver. Your job is to \
                 complete the goal without human intervention.\n\n\
                 Plan in phases up front. Before creating anything, think through the whole \
                 arc of the work and decide the phase boundaries — for example: set up, then \
                 validate the setup, then implement. You can only have ONE active tasklist at \
                 a time, so you do not create every phase at once: you start the first phase, \
                 and when its completion summary returns you validate it and only then author \
                 the next phase's tasklist. Deferring the later phases costs you nothing — you \
                 keep your full transcript across each completion, so all of this planning and \
                 research is still in context when you create them. Hold the multi-phase plan \
                 in your reasoning so a later re-invocation can pick up the next phase without \
                 re-deriving it.\n\n\
                 Orchestration loop:\n\
                 1. Break the goal into phases, then create a dispatched tasklist for the \
                    FIRST phase using `TodoCreate` (sequential `SEQ` or parallel `PAR` groups \
                    as appropriate). Do not try to create every phase at once — only one \
                    tasklist is active at a time.\n\
                 2. When a completion summary returns, call `ProjectVerify mode='quick'` for a \
                    fast mid-flight check. This judges the goal against tasklist summaries — cheap \
                    and useful for tracking gaps, but it cannot confirm that code actually works.\n\
                 3. If the quick verdict is `fail`, examine the `gaps` list and create follow-up \
                    tasklists to address each gap. Do NOT attempt `ProjectComplete` while any \
                    gaps remain.\n\
                 4. Once the quick verdict is `pass` AND all tasklists are done, call \
                    `ProjectVerify mode='full'`. This spawns an isolated read-only inspector \
                    that opens source files, reads git diffs, and runs the test suite. It \
                    verifies the claim against actual artifacts — required before completing.\n\
                 5. If the full verdict is `fail`, address the gaps and re-verify.\n\
                 6. Once `ProjectVerify mode='full'` returns `verdict: pass`, call \
                    `ProjectComplete`. The gate requires a recent full-verification pass — \
                    summary-only verdicts are rejected.\n\n\
                 Important:\n\
                 - You are the orchestrator, not the executor. Never edit files, write code, \
                   or run implementation commands yourself — every piece of execution must be \
                   dispatched through `TodoCreate` tasklists, even when a task looks trivial.\n\
                 - Use `mode='quick'` for cheap mid-flight checks between work batches.\n\
                 - Use `mode='full'` before calling `ProjectComplete`. Do NOT skip it.\n\
                 - Feed returned `gaps` lists directly into follow-up `TodoCreate` calls.\n\
                 - Never skip both verifications. Tasks completing does not mean the goal is met.\n\
                 - Do not wait for the user to prompt each step. Drive autonomously."
                    .to_string(),
            );
        }
        ProjectStatus::NeedsReview => {
            sections.push(
                "## Your Current Role: Escalated — Needs Human Review\n\n\
                 This project reached the maximum number of verification rounds without a \
                 passing verdict. Automated work has been paused. Summarise what was attempted \
                 and what gaps remain, then wait for the project owner to decide how to proceed."
                    .to_string(),
            );
        }
        _ => {}
    }

    sections.join("\n\n")
}

/// Load the project record for a project-scoped run and append its context
/// block to an already-composed system prompt.
///
/// Shared by both agent runners so the injection semantics cannot drift.
/// Fail-soft by design: a missing or unreadable project record logs a warning
/// and returns the prompt unchanged rather than failing the run — the record
/// was present moments earlier when the run was queued, so absence here is a
/// freak race (e.g. project deleted mid-flight), not a normal condition.
pub async fn append_project_context(
    projects: &ProjectStore,
    project_id: &str,
    composed_prompt: Option<String>,
) -> Option<String> {
    match projects.get(project_id).await {
        Ok(Some(project)) => {
            let block = build_project_context_block(&project);
            Some(match composed_prompt {
                Some(prompt) => format!("{}\n\n{}", prompt, block),
                None => block,
            })
        }
        Ok(None) => {
            tracing::warn!(
                project_id = %project_id,
                "project record missing at run time; project context block omitted"
            );
            composed_prompt
        }
        Err(e) => {
            tracing::warn!(
                project_id = %project_id,
                "failed to load project for context block: {}; block omitted",
                e
            );
            composed_prompt
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_project(status: ProjectStatus) -> Project {
        Project {
            id: "proj-1".to_string(),
            name: "Test Project".to_string(),
            emoji: None,
            goal: "Build a search feature".to_string(),
            spec: None,
            agent_id: "agent-1".to_string(),
            working_dir: None,
            attachments: vec![],
            status,
            summary: None,
            verifications: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn includes_project_id_goal_and_name() {
        let project = make_project(ProjectStatus::Active);
        let result = build_project_context_block(&project);
        assert!(result.contains("proj-1"));
        assert!(result.contains("Build a search feature"));
        assert!(result.contains("Test Project"), "Name must appear in project context block");
        assert!(result.contains("**Name**"), "Name label must appear in project context block");
    }

    async fn make_store() -> (ProjectStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("temp dir");
        let data_root = ao_persistence::paths::DataRoot::new(tmp.path());
        tokio::fs::create_dir_all(data_root.projects_dir())
            .await
            .expect("create projects dir");
        (ProjectStore::new(data_root), tmp)
    }

    /// The composed prompt must come first, with the project block appended —
    /// the runner-side injection order that puts role instructions nearest
    /// the end of the system prompt.
    #[tokio::test]
    async fn append_loads_project_and_appends_block_after_composed_prompt() {
        let (store, _tmp) = make_store().await;
        let project = make_project(ProjectStatus::Interviewing);
        store.create(&project).await.expect("seed project");

        let result =
            append_project_context(&store, &project.id, Some("COMPOSED_PROMPT".to_string()))
                .await
                .expect("prompt must be present");
        assert!(result.starts_with("COMPOSED_PROMPT"));
        assert!(result.contains("## Project Context"));
        assert!(
            result.contains("Do not do the work yourself"),
            "interviewing role block must ride along"
        );
    }

    /// Missing project record must not fail the run — prompt passes through
    /// unchanged (fail-soft contract).
    #[tokio::test]
    async fn append_passes_prompt_through_when_project_missing() {
        let (store, _tmp) = make_store().await;

        let result = append_project_context(
            &store,
            "no-such-project",
            Some("COMPOSED_PROMPT".to_string()),
        )
        .await;
        assert_eq!(result.as_deref(), Some("COMPOSED_PROMPT"));
    }

    #[test]
    fn interviewing_status_has_interview_section() {
        let project = make_project(ProjectStatus::Interviewing);
        let result = build_project_context_block(&project);
        assert!(result.contains("## Your Current Role: Interview"));
        assert!(result.contains("interview the user"));
        assert!(!result.contains("Autonomous Orchestration"));
    }

    #[test]
    fn interviewing_section_includes_naming_guidance() {
        let project = make_project(ProjectStatus::Interviewing);
        let result = build_project_context_block(&project);
        assert!(
            result.contains("ProjectUpdate"),
            "Interviewing section must mention ProjectUpdate tool for naming"
        );
        assert!(
            result.contains("name"),
            "Interviewing section must mention name field"
        );
        assert!(
            result.contains("emoji"),
            "Interviewing section must mention emoji field"
        );
    }

    #[test]
    fn interviewing_section_forbids_direct_execution() {
        let project = make_project(ProjectStatus::Interviewing);
        let result = build_project_context_block(&project);
        assert!(
            result.contains("Do not do the work yourself"),
            "Interviewing section must forbid executing the goal during the interview"
        );
        assert!(
            result.contains("TodoCreate"),
            "Interviewing section must point execution at dispatched tasklists"
        );
    }

    #[test]
    fn active_section_forbids_direct_execution() {
        let project = make_project(ProjectStatus::Active);
        let result = build_project_context_block(&project);
        assert!(
            result.contains("orchestrator, not the executor"),
            "Active section must forbid hands-on execution by the coordinator"
        );
    }

    #[test]
    fn active_status_has_orchestration_section() {
        let project = make_project(ProjectStatus::Active);
        let result = build_project_context_block(&project);
        assert!(result.contains("## Your Current Role: Autonomous Orchestration"));
        assert!(result.contains("TodoCreate"));
        assert!(!result.contains("interview the user"));
    }

    #[test]
    fn spec_included_when_present() {
        let mut project = make_project(ProjectStatus::Active);
        project.spec = Some("Implement full-text search using tantivy.".to_string());
        let result = build_project_context_block(&project);
        assert!(result.contains("tantivy"));
    }

    #[test]
    fn working_dir_included_when_present() {
        let mut project = make_project(ProjectStatus::Active);
        project.working_dir = Some("/workspace/myrepo".to_string());
        let result = build_project_context_block(&project);
        assert!(result.contains("/workspace/myrepo"));
    }

    #[test]
    fn attachments_included_when_present() {
        let mut project = make_project(ProjectStatus::Active);
        project.attachments = vec!["/tmp/design.pdf".to_string()];
        let result = build_project_context_block(&project);
        assert!(result.contains("/tmp/design.pdf"));
    }

    #[test]
    fn completed_status_has_no_role_section() {
        let project = make_project(ProjectStatus::Completed);
        let result = build_project_context_block(&project);
        assert!(!result.contains("## Your Current Role"));
    }
}
