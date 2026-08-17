mod prompt;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::Arc;

use ao_engine_tools_core::{IoTool, Registry, RunnerContext, ToolOutput};
use ao_protocol::error::AoError;
use ao_protocol::memory::MemoryScope;
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::memory::{
    resolve_scope_context, resolve_working_dir, ScopeContext, PROJECT_HARD_CAP, PROJECT_SOFT_CAP,
};

/// Default injection budget, in characters, when the caller omits `budget_chars`.
pub const DEFAULT_BUDGET_CHARS: usize = 4000;
/// Floor on the requested budget — below this, a single average-sized entry
/// would not fit and every call would come back empty.
pub const MIN_BUDGET_CHARS: usize = 500;
/// Ceiling on the requested budget — above this the caller has effectively
/// asked to disable budgeting, which defeats the point of a bounded read tool.
pub const MAX_BUDGET_CHARS: usize = 20_000;

/// Shortest token length considered for the keyword filter. Filters out glue
/// words ("to", "the", "is") without needing a maintained stopword list.
const MIN_TOKEN_LEN: usize = 3;

/// Register `LoadMemory` into the supplied [`Registry`].
pub fn register(registry: &mut Registry) {
    registry.register_io(Arc::new(LoadMemory));
}

/// Read tool that injects a target repo's project-scope memory in one call.
///
/// Wraps the same `working_dir`-based scope resolution that
/// `MemoryList`/`MemoryWrite` already use to reach a sibling repo, but
/// returns full entry content (bounded by a character budget) instead of a
/// paginated preview list — see [`prompt::DESCRIPTION`] for the model-facing
/// contract.
pub struct LoadMemory;

#[async_trait]
impl IoTool for LoadMemory {
    fn name(&self) -> &str {
        "LoadMemory"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let repo = match input.get("repo").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s,
            _ => return Ok(ToolOutput::error("Missing required field: repo", false)),
        };

        let budget_chars = input
            .get("budget_chars")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_BUDGET_CHARS)
            .clamp(MIN_BUDGET_CHARS, MAX_BUDGET_CHARS);

        let task = input.get("task").and_then(Value::as_str);

        let store = match &ctx.memory_store {
            Some(s) => s.clone(),
            None => {
                return Ok(ToolOutput::error(
                    "Memory store not available in this context.",
                    false,
                ));
            }
        };

        let cwd = ctx.cwd.read().unwrap().clone();
        let repo_path = resolve_working_dir(Some(repo), &cwd);

        if !tokio::fs::try_exists(&repo_path).await.unwrap_or(false) {
            return Ok(ToolOutput::error(
                format!("repo path does not exist: {}", repo_path.display()),
                true,
            ));
        }

        // Force Project scope resolution against the target repo, ignoring
        // any parent-delegation cwd — `repo` is an explicit override, same
        // precedence rule `working_dir` already gets on MemoryList/MemoryWrite.
        let scope_ctx = match resolve_scope_context(
            &MemoryScope::Project,
            &ctx.agent_id,
            Some(repo_path.as_path()),
            None,
            &cwd,
            None,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolOutput::error(
                    format!("Failed to resolve project scope for {}: {e}", repo_path.display()),
                    false,
                ));
            }
        };

        let (hash, canonical_key) = match &scope_ctx {
            ScopeContext::Project { hash, canonical_key } => (hash.clone(), canonical_key.clone()),
            other => unreachable!(
                "resolve_scope_context(MemoryScope::Project, ..) always returns ScopeContext::Project, got {other:?}"
            ),
        };

        let mut entries = store.list_project(&hash).await?;
        let entry_count = entries.len();
        let total_chars: usize = entries.iter().map(|e| e.content.chars().count()).sum();

        // Baseline order is recency; only overridden below when task-keyword
        // ranking actually applies.
        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        let mut filtered_by_task = false;
        if total_chars > budget_chars {
            if let Some(t) = task {
                let tokens = tokenize(t);
                if !tokens.is_empty() {
                    filtered_by_task = true;
                    entries.sort_by(|a, b| {
                        let score_a = keyword_score(&a.content, &tokens);
                        let score_b = keyword_score(&b.content, &tokens);
                        score_b
                            .cmp(&score_a)
                            .then_with(|| b.updated_at.cmp(&a.updated_at))
                    });
                }
            }
        }

        let (selected, truncated) = if total_chars <= budget_chars {
            (entries, false)
        } else {
            // Budget pressure: walk the ranked order, keeping everything that
            // still fits. The top-ranked entry is always kept even if it alone
            // exceeds the budget, so a too-small budget_chars never silently
            // produces zero entries from a non-empty scope.
            let mut picked = Vec::new();
            let mut used = 0usize;
            for (i, entry) in entries.into_iter().enumerate() {
                let len = entry.content.chars().count();
                if i == 0 || used + len <= budget_chars {
                    used += len;
                    picked.push(entry);
                } else {
                    break;
                }
            }
            let truncated = picked.len() < entry_count;
            (picked, truncated)
        };

        let chars_returned: usize = selected.iter().map(|e| e.content.chars().count()).sum();
        let entries_json: Vec<Value> = selected
            .into_iter()
            .map(|e| {
                json!({
                    "id": e.id,
                    "content": e.content,
                    "created_at": e.created_at.to_rfc3339(),
                    "updated_at": e.updated_at.to_rfc3339(),
                })
            })
            .collect();
        let returned_count = entries_json.len();

        Ok(ToolOutput::structured(json!({
            "scope": "project",
            "repo_input": repo,
            "project_root": canonical_key,
            "entries": entries_json,
            "entry_count": entry_count,
            "returned_count": returned_count,
            "chars_returned": chars_returned,
            "budget_chars": budget_chars,
            "truncated": truncated,
            "filtered_by_task": filtered_by_task,
            "scope_summary": {
                "count": entry_count,
                "soft_cap": PROJECT_SOFT_CAP,
                "hard_cap": PROJECT_HARD_CAP,
            }
        })))
    }
}

/// Split `task` into lowercase alphanumeric tokens of at least
/// [`MIN_TOKEN_LEN`] chars, deduplicated. Standing in for a real tokenizer:
/// good enough to rank memory entries by topical overlap without pulling in
/// a search library for what is, today, at most ~150 short entries.
fn tokenize(task: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    task.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() >= MIN_TOKEN_LEN)
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

/// Count how many distinct `tokens` appear as a case-insensitive substring of
/// `content`. A poor-man's relevance score standing in for a real FTS5 index
/// — see workstream F's files-vs-index tripwire: this is the mechanism that
/// is expected to strain first if per-repo memory outgrows the injection
/// budget, which is the intended signal to reach for FTS5 instead of growing
/// this function further.
fn keyword_score(content: &str, tokens: &[String]) -> usize {
    let lower = content.to_lowercase();
    tokens.iter().filter(|t| lower.contains(t.as_str())).count()
}
