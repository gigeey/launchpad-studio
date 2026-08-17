mod prompt;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{
    context::{TodoItem, TodoStatus},
    EngineTool, LoadPolicy, RunnerContext, ToolOutput, UserEvent,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

pub struct TodoWrite;

#[async_trait]
impl EngineTool for TodoWrite {
    fn name(&self) -> &str {
        "TodoWrite"
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
        false
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let todos_val = match input.get("todos") {
            Some(v) => v,
            None => {
                return Ok(ToolOutput::error("missing required field: todos", true));
            }
        };

        let arr = match todos_val.as_array() {
            Some(a) => a,
            None => {
                return Ok(ToolOutput::error("\"todos\" must be an array", true));
            }
        };

        let mut items: Vec<TodoItem> = Vec::with_capacity(arr.len());
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (i, entry) in arr.iter().enumerate() {
            let id = match entry.get("id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => {
                    return Ok(ToolOutput::error(
                        &format!("todos[{i}]: missing or empty \"id\""),
                        true,
                    ));
                }
            };

            if !seen_ids.insert(id.clone()) {
                return Ok(ToolOutput::error(
                    &format!("duplicate todo id: \"{id}\""),
                    true,
                ));
            }

            let content = match entry.get("content").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return Ok(ToolOutput::error(
                        &format!("todos[{i}]: missing required field \"content\""),
                        true,
                    ));
                }
            };

            let status = match entry.get("status").and_then(|v| v.as_str()) {
                Some("pending") => TodoStatus::Pending,
                Some("in_progress") => TodoStatus::InProgress,
                Some("completed") => TodoStatus::Completed,
                Some(other) => {
                    return Ok(ToolOutput::error(
                        &format!(
                            "todos[{i}]: unknown status \"{other}\"; \
                             must be \"pending\", \"in_progress\", or \"completed\""
                        ),
                        true,
                    ));
                }
                None => {
                    return Ok(ToolOutput::error(
                        &format!("todos[{i}]: missing required field \"status\""),
                        true,
                    ));
                }
            };

            let active_form = entry
                .get("active_form")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| content.clone());

            items.push(TodoItem {
                id,
                content,
                status,
                active_form,
            });
        }

        ctx.todos.replace(&ctx.agent_id, items.clone());

        let total = items.len();
        let in_progress = items.iter().filter(|t| t.status == TodoStatus::InProgress).count();
        let pending = items.iter().filter(|t| t.status == TodoStatus::Pending).count();
        let completed = items.iter().filter(|t| t.status == TodoStatus::Completed).count();

        ctx.event_sink
            .emit(UserEvent::TodosUpdated {
                count: total,
                in_progress,
                pending,
                completed,
            })
            .await
            .map_err(|e| AoError::Internal(format!("event sink error: {e}")))?;

        let summary = format!("{total} todos: {in_progress} in_progress, {pending} pending, {completed} completed");
        Ok(ToolOutput::text(summary))
    }
}
