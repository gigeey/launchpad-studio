pub mod prompt;
#[cfg(test)]
mod tests;

use std::collections::HashSet;

use ao_engine_tools_core::{
    EngineTool, EventKind, LoadPolicy, RunnerContext, ToolOutput, ToolRef, ToolUsageEvent,
};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};

/// Search the deferred tool catalog by keyword, or activate tools with select:.
pub struct ToolSearch;

fn score_entry(search_text: &str, name_lower: &str, tokens: &[&str]) -> f64 {
    let mut score = 0.0_f64;
    for token in tokens {
        if search_text.contains(*token) {
            score += 1.0;
            if name_lower.contains(*token) {
                // 2x bonus for tokens that match the tool name itself
                score += 2.0;
            }
        }
    }
    score
}

#[async_trait]
impl EngineTool for ToolSearch {
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of a specific deferred tool to resolve. Returns the tool schema and marks it for inclusion in subsequent requests. Use this instead of query when you know the exact tool name."
                },
                "query": {
                    "type": "string",
                    "description": "Keyword query, or select:Name1,Name2 to activate tools by name."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return (keyword search only).",
                    "default": 5
                }
            },
            "required": []
        })
    }

    fn load_policy(&self) -> LoadPolicy {
        LoadPolicy::AlwaysLoad
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        // --- name: single-tool resolution path ---
        if let Some(name) = input.get("name").and_then(|v| v.as_str()) {
            return match ctx.registry.lookup(name) {
                None => Ok(ToolOutput::error(format!("unknown tool: {name}"), true)),
                Some(tool_ref) => {
                    if tool_ref.load_policy() == LoadPolicy::Deferred {
                        {
                            let mut guard = ctx.loaded_deferred_tools.write().unwrap();
                            guard.insert(name.to_string());
                        }
                        let description = tool_ref.description().to_string();
                        let schema = tool_ref.input_schema();
                        Ok(ToolOutput::text(format!(
                            "Tool resolved: {name}\nDescription: {description}\nSchema: {}",
                            serde_json::to_string_pretty(&schema).unwrap_or_default()
                        )))
                    } else {
                        Ok(ToolOutput::text(format!("Tool '{name}' is already available.")))
                    }
                }
            };
        }

        let query = match input.get("query").and_then(|v| v.as_str()) {
            Some(q) => q.to_string(),
            None => return Ok(ToolOutput::error("missing required field: query", true)),
        };

        // --- select: activation path ---
        if query.trim_start().to_ascii_lowercase().starts_with("select:") {
            let after_prefix = query
                .trim_start()
                .splitn(2, ':')
                .nth(1)
                .unwrap_or("")
                .trim();

            let names: Vec<&str> = after_prefix
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();

            let mut activated: Vec<Value> = Vec::new();
            let mut unresolved: Vec<String> = Vec::new();

            for name in names {
                match ctx.registry.lookup(name) {
                    Some(tool_ref) => {
                        let schema = match &tool_ref {
                            ToolRef::Io(t) => t.input_schema(),
                            ToolRef::Engine(t) => t.input_schema(),
                        };

                        // Add to activated_tools (idempotent — already present is fine)
                        {
                            let mut guard = ctx.activated_tools.lock().unwrap();
                            guard.insert(name.to_string());
                        }

                        // If deferred, mark as loaded so providers include full schema next turn
                        if tool_ref.load_policy() == LoadPolicy::Deferred {
                            let mut guard = ctx.loaded_deferred_tools.write().unwrap();
                            guard.insert(name.to_string());
                        }

                        // Emit Selected telemetry event
                        ctx.telemetry.emit(ToolUsageEvent {
                            agent_id: ctx.agent_id.clone(),
                            session_id: ctx.session_id.clone(),
                            tool_name: name.to_string(),
                            kind: EventKind::Selected,
                            ts: Utc::now(),
                            metadata: Value::Object(Default::default()),
                        });

                        activated.push(json!({
                            "name": name,
                            "schema": schema,
                        }));
                    }
                    None => {
                        unresolved.push(name.to_string());
                    }
                }
            }

            return Ok(ToolOutput::Structured(json!({
                "activated": activated,
                "unresolved": unresolved,
            })));
        }

        // --- keyword search path ---
        let max_results = input
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;

        let deferred_index = ctx.registry.deferred_index();

        // loaded_set = always_load_tools union activated_tools
        let mut loaded_set: HashSet<String> = ctx.always_load_tools.iter().cloned().collect();
        {
            let activated = ctx.activated_tools.lock().unwrap();
            for name in activated.iter() {
                loaded_set.insert(name.clone());
            }
        }

        let candidates: Vec<_> = deferred_index
            .entries()
            .iter()
            .filter(|e| !loaded_set.contains(&e.name))
            .collect();

        let query_trimmed = query.trim();

        let mut scored: Vec<(String, String, f64)> = if query_trimmed.is_empty() {
            let mut entries: Vec<_> = candidates
                .iter()
                .map(|e| (e.name.clone(), e.short_description.clone(), 0.0_f64))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
        } else {
            let query_lower = query_trimmed.to_lowercase();
            let tokens: Vec<&str> = query_lower.split_whitespace().collect();
            let mut entries: Vec<_> = candidates
                .iter()
                .map(|e| {
                    let name_lower = e.name.to_lowercase();
                    let score = score_entry(&e.search_text, &name_lower, &tokens);
                    (e.name.clone(), e.short_description.clone(), score)
                })
                .collect();
            entries.sort_by(|a, b| {
                b.2.partial_cmp(&a.2)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.cmp(&b.0))
            });
            entries
        };

        scored.truncate(max_results);

        let results: Vec<Value> = scored
            .iter()
            .map(|(name, short_description, score)| {
                json!({
                    "name": name,
                    "short_description": short_description,
                    "score": score
                })
            })
            .collect();

        Ok(ToolOutput::Structured(json!({ "results": results })))
    }
}
