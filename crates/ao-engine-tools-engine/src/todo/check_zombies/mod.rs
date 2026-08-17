mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::{EngineTool, LoadPolicy, RunnerContext, ToolOutput, ZombieReport};
use ao_protocol::error::AoError;
use async_trait::async_trait;
use serde_json::Value;

const DEFAULT_GRACE_SECS: u64 = 60;

pub struct TodoCheckZombies;

#[async_trait]
impl EngineTool for TodoCheckZombies {
    fn name(&self) -> &str {
        "TodoCheckZombies"
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

        let grace_secs = input
            .get("grace_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_GRACE_SECS);

        let auto_requeue = input
            .get("auto_requeue")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let zombies = match svc.check_zombies_for_agent(&ctx.agent_id, grace_secs).await {
            Ok(z) => z,
            Err(e) => {
                return Ok(ToolOutput::error(
                    &format!("failed to scan for zombie tasks: {e}"),
                    false,
                ));
            }
        };

        if zombies.is_empty() {
            return Ok(ToolOutput::text(format!(
                "No zombie tasks detected (all InProgress tasks have live runners, or none exist after {}s grace).",
                grace_secs
            )));
        }

        if auto_requeue {
            requeue_zombies(&svc, &ctx.agent_id, &zombies).await
        } else {
            Ok(ToolOutput::text(format_zombie_report(&zombies, grace_secs)))
        }
    }
}

async fn requeue_zombies(
    svc: &Arc<dyn ao_engine_tools_core::TasklistServiceHandle + Send + Sync>,
    agent_id: &str,
    zombies: &[ZombieReport],
) -> Result<ToolOutput, AoError> {
    let mut lines = Vec::with_capacity(zombies.len() + 2);
    lines.push(format!(
        "{} zombie task(s) detected — resetting each to Pending:\n",
        zombies.len()
    ));

    for z in zombies {
        match svc
            .requeue_task_for_agent(agent_id, &z.tasklist_id, &z.task_id)
            .await
        {
            Ok(()) => lines.push(format!("  • {} → reset to Pending", z.task_id)),
            Err(e) => lines.push(format!("  • {} → requeue failed: {}", z.task_id, e)),
        }
    }

    lines.push("\nEach task will be re-dispatched on the next feeder advance.".to_string());
    Ok(ToolOutput::text(lines.join("\n")))
}

fn format_zombie_report(zombies: &[ZombieReport], grace_secs: u64) -> String {
    let mut lines = Vec::with_capacity(zombies.len() * 5 + 3);
    lines.push(format!(
        "{} zombie task(s) detected (InProgress, no live runner, grace={}s):\n",
        zombies.len(),
        grace_secs
    ));

    for z in zombies {
        lines.push(format!("  • task_id:  {}", z.task_id));
        lines.push(format!("    title:    {:?}", z.task_title));
        lines.push(format!("    agent:    {}", z.agent_id));
        match z.secs_since_dispatch {
            Some(s) => lines.push(format!("    dispatch: {}s ago", s)),
            None => lines.push(
                "    dispatch: unknown (no timestamp — possible server restart)".to_string(),
            ),
        }
        lines.push(String::new());
    }

    lines.push(
        "Re-run with auto_requeue: true to reset each zombie to Pending for re-dispatch."
            .to_string(),
    );
    lines.join("\n")
}
