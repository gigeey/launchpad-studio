mod prompt;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::process::Command;

use ao_engine_tools_core::{
    AskQuestionError, EngineTool, FormAnswer, FormField, FormFieldKind, FormOption, FormRequest,
    FormResponse, LoadPolicy, RunnerContext, ToolOutput, UserEvent, WorktreeEntry,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

pub struct EnterWorktree;

#[async_trait]
impl EngineTool for EnterWorktree {
    fn name(&self) -> &str {
        "EnterWorktree"
    }

    fn description(&self) -> &str {
        prompt::ENTER_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::enter_input_schema()
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn mutates_filesystem(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        // Single-level nesting only.
        if !ctx.worktree_stack.lock().unwrap().is_empty() {
            return Ok(ToolOutput::error(
                "already inside a worktree — exit the current one before entering another",
                true,
            ));
        }

        let slug = derive_slug(input.get("name").and_then(|v| v.as_str()));

        let current_cwd = ctx.cwd.read().unwrap().clone();

        // Resolve the canonical git root from the current directory.
        let git_root = match resolve_git_root(&current_cwd) {
            Ok(r) => r,
            Err(msg) => return Ok(ToolOutput::error(&msg, true)),
        };

        let worktree_path = git_root
            .join(".launchpad_studio")
            .join("worktrees")
            .join(&slug);
        let branch = format!("worktree/{slug}");

        // Write a .gitignore inside .launchpad_studio/ so the worktree
        // directories never appear as untracked files in the main tree.
        ensure_launchpad_gitignore(&git_root)?;

        // Capture HEAD before creating the worktree so we have a stable base.
        let base_commit = match git_rev_parse_head(&current_cwd) {
            Ok(c) => c,
            Err(msg) => return Ok(ToolOutput::error(&msg, true)),
        };

        // Create the worktree and its branch from the current HEAD.
        if let Err(msg) = git_worktree_add(&worktree_path, &branch, &current_cwd) {
            return Ok(ToolOutput::error(&msg, true));
        }

        let entry = WorktreeEntry {
            restore_cwd: current_cwd.clone(),
            worktree_path: worktree_path.clone(),
            branch: branch.clone(),
            base_commit: base_commit.clone(),
        };

        ctx.worktree_stack.lock().unwrap().push(entry);
        *ctx.cwd.write().unwrap() = worktree_path.clone();

        ctx.event_sink
            .emit(UserEvent::CwdChanged {
                from: current_cwd.clone(),
                to: worktree_path.clone(),
            })
            .await
            .map_err(|e| AoError::Internal(format!("event sink error: {e}")))?;

        let msg = format!(
            "Worktree ready.\n\
             Path:        {}\n\
             Branch:      {}\n\
             Base commit: {}\n\
             \n\
             The session is now working inside the worktree. Use ExitWorktree \
             with action=keep to leave while preserving the branch, or \
             action=remove to leave and delete the branch and directory.",
            worktree_path.display(),
            branch,
            base_commit,
        );
        Ok(ToolOutput::text(msg))
    }
}

pub struct ExitWorktree;

#[async_trait]
impl EngineTool for ExitWorktree {
    fn name(&self) -> &str {
        "ExitWorktree"
    }

    fn description(&self) -> &str {
        prompt::EXIT_DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::exit_input_schema()
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::Deferred
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn mutates_filesystem(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let action = match input.get("action").and_then(|v| v.as_str()) {
            Some("keep") => ExitAction::Keep,
            Some("remove") => ExitAction::Remove,
            Some(other) => {
                return Ok(ToolOutput::error(
                    &format!("unknown action '{other}': must be 'keep' or 'remove'"),
                    true,
                ));
            }
            None => {
                return Ok(ToolOutput::error(
                    "action is required: 'keep' or 'remove'",
                    true,
                ));
            }
        };

        let entry = match ctx.worktree_stack.lock().unwrap().pop() {
            Some(e) => e,
            None => {
                return Ok(ToolOutput::error(
                    "no active worktree session to exit",
                    true,
                ));
            }
        };

        let prior_cwd = ctx.cwd.read().unwrap().clone();
        *ctx.cwd.write().unwrap() = entry.restore_cwd.clone();

        ctx.event_sink
            .emit(UserEvent::CwdChanged {
                from: prior_cwd,
                to: entry.restore_cwd.clone(),
            })
            .await
            .map_err(|e| AoError::Internal(format!("event sink error: {e}")))?;

        match action {
            ExitAction::Keep => {
                let msg = format!(
                    "Exited worktree (kept).\n\
                     Branch {} and directory {} are preserved on disk.\n\
                     Session working directory restored to: {}",
                    entry.branch,
                    entry.worktree_path.display(),
                    entry.restore_cwd.display(),
                );
                Ok(ToolOutput::text(msg))
            }
            ExitAction::Remove => {
                if let Err(what_is_lost) =
                    check_worktree_clean(&entry.worktree_path, &entry.base_commit)
                {
                    // Dirty worktree — ask the operator before discarding work.
                    // Routed through the form bridge (the same channel that
                    // powers AskUserQuestionWithForm) because that is the
                    // interactive prompt path wired end-to-end in native mode;
                    // the plain question channel has no live delivery route.
                    let form = FormRequest {
                        id: String::new(),
                        agent_id: ctx.agent_id.clone(),
                        session_id: ctx.session_id.clone(),
                        title: format!("Remove worktree branch '{}'?", entry.branch),
                        intro: Some(format!(
                            "This worktree has unsaved work: {what_is_lost}. \
                             Removing it permanently discards that work."
                        )),
                        fields: vec![FormField {
                            id: REMOVE_FIELD_DECISION.to_string(),
                            kind: FormFieldKind::Radio {
                                options: vec![
                                    FormOption {
                                        id: REMOVE_OPT_REMOVE.to_string(),
                                        label: "Remove and discard".to_string(),
                                        description: Some(
                                            "Permanently delete the worktree directory and branch."
                                                .to_string(),
                                        ),
                                    },
                                    FormOption {
                                        id: REMOVE_OPT_KEEP.to_string(),
                                        label: "Keep".to_string(),
                                        description: Some(
                                            "Leave the worktree and branch on disk.".to_string(),
                                        ),
                                    },
                                ],
                            },
                            label: "Select an action".to_string(),
                            description: None,
                            required: true,
                        }],
                    };

                    let answer = tokio::select! {
                        biased;
                        _ = ctx.cancel.cancelled() => Err(AskQuestionError::Cancelled),
                        r = ctx.form_bridge.ask_form(form) => r,
                    };

                    match answer {
                        Ok(response) => {
                            if !form_selected_remove(&response) {
                                return Ok(ToolOutput::text(format!(
                                    "Worktree removal declined.\n\
                                     Branch {} and directory {} are preserved on disk.\n\
                                     Session working directory restored to: {}",
                                    entry.branch,
                                    entry.worktree_path.display(),
                                    entry.restore_cwd.display(),
                                )));
                            }
                            // Operator approved — fall through to removal below.
                        }
                        Err(AskQuestionError::Cancelled) => {
                            return Ok(ToolOutput::error("removal cancelled", false));
                        }
                        Err(AskQuestionError::NoOperator) => {
                            return Ok(ToolOutput::error(
                                &format!(
                                    "no operator available to confirm removal — the worktree \
                                     has unsaved work ({what_is_lost}). Use action=keep to \
                                     preserve the branch, or commit/merge the changes first."
                                ),
                                true,
                            ));
                        }
                    }
                }

                // Removal approved (clean tree or operator-confirmed dirty tree).
                // --force is required because git worktree remove refuses dirty trees.
                if let Err(msg) =
                    git_worktree_remove(&entry.worktree_path, &entry.branch, &entry.restore_cwd)
                {
                    return Ok(ToolOutput::error(&msg, true));
                }

                let msg = format!(
                    "Exited worktree (removed).\n\
                     Branch {} and directory {} have been deleted.\n\
                     Session working directory restored to: {}",
                    entry.branch,
                    entry.worktree_path.display(),
                    entry.restore_cwd.display(),
                );
                Ok(ToolOutput::text(msg))
            }
        }
    }
}

enum ExitAction {
    Keep,
    Remove,
}

/// Field id for the dirty-removal confirmation radio group.
const REMOVE_FIELD_DECISION: &str = "decision";
/// Option id for "remove and discard the worktree".
const REMOVE_OPT_REMOVE: &str = "remove";
/// Option id for "keep the worktree on disk".
const REMOVE_OPT_KEEP: &str = "keep";

/// Returns `true` only when the operator explicitly selected
/// "remove and discard" in the dirty-removal confirmation form. Any other
/// shape (the keep option, an empty selection, a non-selection answer, or a
/// missing field) is treated as a decline so removal fails safe.
fn form_selected_remove(response: &FormResponse) -> bool {
    matches!(
        response.answers.get(REMOVE_FIELD_DECISION),
        Some(FormAnswer::Selections(ids)) if ids.iter().any(|id| id == REMOVE_OPT_REMOVE)
    )
}

// ── Git helpers ──────────────────────────────────────────────────────────────

fn resolve_git_root(cwd: &Path) -> Result<PathBuf, String> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;

    if !out.status.success() {
        return Err(
            "the current directory is not inside a git repository".to_string(),
        );
    }

    let raw = String::from_utf8_lossy(&out.stdout);
    Ok(PathBuf::from(raw.trim()))
}

fn git_rev_parse_head(cwd: &Path) -> Result<String, String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run git rev-parse: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git rev-parse HEAD failed: {stderr}"));
    }

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_worktree_add(worktree_path: &Path, branch: &str, cwd: &Path) -> Result<(), String> {
    let out = Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            branch,
            worktree_path.to_str().unwrap_or(""),
            "HEAD",
        ])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run git worktree add: {e}"))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git worktree add failed: {stderr}"));
    }

    Ok(())
}

fn git_worktree_remove(worktree_path: &Path, branch: &str, cwd: &Path) -> Result<(), String> {
    // Unregister and delete the worktree directory.
    let rm = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            worktree_path.to_str().unwrap_or(""),
        ])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run git worktree remove: {e}"))?;

    if !rm.status.success() {
        let stderr = String::from_utf8_lossy(&rm.stderr);
        return Err(format!("git worktree remove failed: {stderr}"));
    }

    // Delete the branch.
    let del = Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("failed to run git branch -D: {e}"))?;

    if !del.status.success() {
        let stderr = String::from_utf8_lossy(&del.stderr);
        return Err(format!("git branch -D {branch} failed: {stderr}"));
    }

    Ok(())
}

/// Returns `Ok(())` if the worktree is clean (no uncommitted changes and no
/// commits ahead of `base_commit`). Returns `Err(description)` otherwise.
fn check_worktree_clean(worktree_path: &Path, base_commit: &str) -> Result<(), String> {
    // Check for uncommitted changes.
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .output()
        .map_err(|e| format!("git status failed: {e}"))?;

    if !status.status.success() {
        return Err(format!(
            "git status exited with error: {}",
            String::from_utf8_lossy(&status.stderr)
        ));
    }

    let status_out = String::from_utf8_lossy(&status.stdout);
    let changed_files = status_out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();

    // Check for commits not in the base.
    let ahead = Command::new("git")
        .args(["rev-list", "--count", &format!("{base_commit}..HEAD")])
        .current_dir(worktree_path)
        .output()
        .map_err(|e| format!("git rev-list failed: {e}"))?;

    if !ahead.status.success() {
        return Err(format!(
            "git rev-list exited with error: {}",
            String::from_utf8_lossy(&ahead.stderr)
        ));
    }

    let ahead_count: u64 = String::from_utf8_lossy(&ahead.stdout)
        .trim()
        .parse()
        .unwrap_or(0);

    if changed_files > 0 || ahead_count > 0 {
        let mut parts = Vec::new();
        if changed_files > 0 {
            parts.push(format!("{changed_files} uncommitted file(s)"));
        }
        if ahead_count > 0 {
            parts.push(format!("{ahead_count} unmerged commit(s) ahead of base"));
        }
        return Err(parts.join(", "));
    }

    Ok(())
}

/// Write a `*` gitignore inside `<git_root>/.launchpad_studio/` so the
/// worktree directories are never shown as untracked in the main repository.
/// Only writes if the file does not already contain a wildcard line.
fn ensure_launchpad_gitignore(git_root: &Path) -> Result<(), AoError> {
    let dir = git_root.join(".launchpad_studio");
    std::fs::create_dir_all(&dir)
        .map_err(|e| AoError::Internal(format!("failed to create .launchpad_studio: {e}")))?;

    let ignore_path = dir.join(".gitignore");
    let existing = std::fs::read_to_string(&ignore_path).unwrap_or_default();
    if !existing.lines().any(|l| l.trim() == "*") {
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("*\n");
        std::fs::write(&ignore_path, content)
            .map_err(|e| AoError::Internal(format!("failed to write .gitignore: {e}")))?;
    }

    Ok(())
}

/// Produce a URL-safe slug from a user-supplied name, or generate a random
/// one when no name is given. The slug is used for both the worktree directory
/// name and the branch name suffix.
fn derive_slug(name: Option<&str>) -> String {
    match name.filter(|n| !n.trim().is_empty()) {
        Some(n) => slugify(n),
        None => {
            // Use the first 8 hex chars of a random UUID as a compact unique id.
            let id = Uuid::new_v4().to_string();
            format!("wt-{}", &id[..8])
        }
    }
}

/// Convert an arbitrary string into a slug suitable for use in a branch name
/// and directory path: lowercase, spaces and non-alphanumeric characters
/// (except `-` and `.`) replaced with `-`, leading/trailing `-` stripped.
fn slugify(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    // Collapse consecutive `-` and strip leading/trailing `-`.
    let mut out = String::with_capacity(slug.len());
    let mut prev_dash = true; // treat start as after a dash to strip leading `-`
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash {
                out.push(c);
                prev_dash = true;
            }
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        // Fallback: if the entire string reduced to nothing, use a UUID prefix.
        let id = Uuid::new_v4().to_string();
        return format!("wt-{}", &id[..8]);
    }
    out
}
