pub mod consolidation;
mod prompt;
pub mod retirement;
pub mod review;
#[cfg(test)]
mod tests;

use ao_engine_tools_core::{
    background_agents::{ModelId, SubagentDefinition, SubagentSpawner},
    skill_registry::{
        dispatch::{
            skill_dir_for_record, substitute_skill_vars, validate_skill_description,
            validate_skill_name, write_skill_to_user_pool, SkillWriteError,
        },
        set_disable_model_invocation, set_version, ContextMode, SkillEntry, SkillRegistry,
        SkillSource,
    },
    trust_gate::{stage_candidate, ArtifactType, CandidateOrigin, CandidateScope, StagingRequest},
    EngineTool, PermissionContext, PermissionDecision, RunnerContext, ToolOutput,
};
use std::collections::HashSet;
use std::sync::Arc;
use ao_protocol::{data_root::resolve_data_root, error::AoError};
use async_trait::async_trait;
use serde_json::Value;
use tracing;

/// RunSkill — invokes a Studio-registered skill from `<data_dir>/skills/`
/// plus per-agent plugin pools. Distinct from any skill system owned by the
/// host CLI process that embeds Studio, which exposes its own separate tool
/// for that purpose. The historical name "Skill" collided with a host CLI's
/// tool of the same name, shadowing dispatch in CLI mode. The rename keeps
/// both systems usable side by side.
pub struct RunSkill {
    spawner: Option<Arc<SubagentSpawner>>,
}

impl RunSkill {
    pub fn new() -> Self {
        Self { spawner: None }
    }

    pub fn with_spawner(spawner: Arc<SubagentSpawner>) -> Self {
        Self { spawner: Some(spawner) }
    }
}

impl Default for RunSkill {
    fn default() -> Self {
        Self::new()
    }
}

fn skill_record_to_subagent_definition(
    record: &ao_engine_tools_core::skill_registry::SkillRecord,
    args: &str,
    parent_agent_id: &str,
    skill_dir: &std::path::Path,
    data_dir: &std::path::Path,
    session_id: &str,
    agent_id: &str,
) -> SubagentDefinition {
    let substituted =
        substitute_skill_vars(&record.body, args, data_dir, skill_dir, session_id, agent_id);

    SubagentDefinition {
        id: record.agent.as_deref().unwrap_or(parent_agent_id).to_string(),
        description: record.description.clone(),
        allowed_tools: record.allowed_tools.clone(),
        system_prompt_fragment: substituted,
        model_override: record.model.as_deref().map(ModelId::new),
    }
}

#[async_trait]
impl EngineTool for RunSkill {
    fn name(&self) -> &str {
        "RunSkill"
    }

    fn description(&self) -> &str {
        prompt::DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &Value,
        _ctx: &PermissionContext,
    ) -> PermissionDecision {
        // Loading a skill only injects instructions into the agent's context;
        // it performs no side effects of its own. Every tool the skill then
        // calls (Bash, Edit, Write, …) is gated individually at its own
        // permission check, so allowing the load loses nothing on safety.
        // Visibility is preserved: a "Loaded skill: X" chip is surfaced after
        // dispatch. A confirmable posture can be reintroduced here once a live
        // permission-prompt bridge exists (see the native runner gap note).
        PermissionDecision::Allow
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let raw_skill = match input.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("missing required field: skill", true)),
        };
        // Strip a single leading `/` so `skill: "/review-pr"` resolves the same as `"review-pr"`.
        let skill_name = raw_skill.strip_prefix('/').unwrap_or(&raw_skill).to_string();
        let args = input.get("args").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let entry = {
            let registry = ctx.skill_registry.read().unwrap();
            // Resolution order: exact name → qualified `plugin:skill` → qualified `mcp_server:prompt`.
            let found = registry.get(&skill_name).cloned().or_else(|| {
                let Some((prefix, plain_name)) = skill_name.split_once(':') else {
                    return None;
                };
                // Plugin-sourced qualified form.
                let plugin_match = registry.all_visible().find(|(_, e)| {
                    if let SkillEntry::Ok(r) = e {
                        r.name == plain_name
                            && matches!(
                                &r.source,
                                SkillSource::Plugin { plugin_name }
                                if plugin_name == prefix
                            )
                    } else {
                        false
                    }
                });
                if plugin_match.is_some() {
                    return plugin_match.map(|(_, e)| e.clone());
                }
                // MCP-sourced qualified form: `server_name:prompt_name`.
                registry
                    .all_visible()
                    .find(|(_, e)| {
                        if let SkillEntry::Ok(r) = e {
                            r.name == plain_name
                                && matches!(
                                    &r.source,
                                    SkillSource::Mcp { server_name }
                                    if server_name == prefix
                                )
                        } else {
                            false
                        }
                    })
                    .map(|(_, e)| e.clone())
            });
            found
        };

        let entry = match entry {
            Some(e) => e,
            None => {
                return Ok(ToolOutput::error(
                    format!("skill '{}' not found", skill_name),
                    true,
                ))
            }
        };

        match entry {
            SkillEntry::Err(reason) => Ok(ToolOutput::error(
                format!("skill '{}' failed to load: {}", skill_name, reason),
                false,
            )),
            SkillEntry::Ok(record) => {
                if record.disable_model_invocation {
                    return Ok(ToolOutput::error(
                        format!("skill '{}' is not available for model invocation", skill_name),
                        true,
                    ));
                }

                let data_dir = resolve_data_root()
                    .map_err(|e| AoError::Internal(format!("data root error: {e}")))?;

                let skill_dir = skill_dir_for_record(&record, &skill_name, &data_dir);

                let agent_skills_usage_dir = data_dir
                    .join("agent_homes")
                    .join(&ctx.agent_id)
                    .join("skills");
                if let Err(e) = ao_engine_tools_core::skill_registry::usage::increment(
                    &agent_skills_usage_dir,
                    &skill_name,
                )
                .await
                {
                    tracing::warn!(
                        "Failed to update skill usage counter for '{}': {}",
                        skill_name,
                        e
                    );
                }

                match record.context {
                    ContextMode::Fork => {
                        let definition = skill_record_to_subagent_definition(
                            &record,
                            &args,
                            &ctx.agent_id,
                            &skill_dir,
                            &data_dir,
                            &ctx.session_id,
                            &ctx.agent_id,
                        );

                        match &self.spawner {
                            Some(spawner) => {
                                let raw = spawner.spawn_sync(ctx, definition, args).await;
                                // Prepend a contextual header so the model gets a
                                // recognizable completion signal with the skill name.
                                Ok(match raw {
                                    ToolOutput::Text(text) => ToolOutput::text(format!(
                                        "Skill \"{}\" completed.\n\nResult:\n{}",
                                        skill_name, text
                                    )),
                                    other => other,
                                })
                            }
                            None => Ok(ToolOutput::error(
                                "fork dispatch requires a spawner (none configured in this context)",
                                false,
                            )),
                        }
                    }
                    ContextMode::Inline => {
                        let substituted = substitute_skill_vars(
                            &record.body,
                            &args,
                            &data_dir,
                            &skill_dir,
                            &ctx.session_id,
                            &ctx.agent_id,
                        );

                        if !record.allowed_tools.is_empty() {
                            ctx.set_skill_tool_filter(
                                record.allowed_tools.iter().cloned().collect::<HashSet<String>>(),
                            );
                        }

                        // Single-call dispatch (MCP HTTP route): no turn loop
                        // will drain `pending_user_messages`, so an enqueued
                        // body would be silently dropped. Return the substituted
                        // body as the tool result instead — the externally
                        // driven agent reads it as this call's output and acts
                        // on the skill instructions on its next step.
                        if ctx.inline_skill_via_tool_result {
                            return Ok(ToolOutput::text(substituted));
                        }

                        // In-process runner: wrap in `[skill "<name>" loaded]\n<body>`
                        // so the runner-side drain can persist this as a hidden
                        // transcript entry that the frontend coalesces into a
                        // "Loaded skill: X" chip on the surrounding agent bubble.
                        // The prefix also reads as a useful contextualization
                        // hint to the model — it sees the skill name as the
                        // first line of the synthesized user turn.
                        let wrapped = format!("[skill \"{}\" loaded]\n{}", skill_name, substituted);
                        ctx.enqueue_user_message(wrapped);

                        Ok(ToolOutput::text(format!("Launching skill: {}", skill_name)))
                    }
                }
            }
        }
    }
}

/// SkillRegister — creates/updates a Studio-registered skill in
/// `<data_dir>/skills/`. Companion to [`RunSkill`]; same rename motivation.
/// The historical name was `SkillWrite`.
pub struct SkillRegister;

/// What the trust gate managed to do to the submitted body. The file is
/// written in all three cases; they differ in whether the skill can ever be
/// invoked, so each gets its own message rather than being collapsed into a
/// quarantined/not-quarantined boolean — under that shape both failure arms
/// report as an ordinary success, which is the one thing the caller most
/// needs to be able to tell apart.
enum GateResult {
    /// Frontmatter parsed and the gate stamped `disable-model-invocation:
    /// true`. The skill is on disk, awaiting human approval in the review
    /// queue. This is the normal outcome — the gate never auto-enables.
    Parked,
    /// Frontmatter parsed but the gate could not rewrite it, so the skill is
    /// on disk carrying whatever invocability the model asked for. This is a
    /// gate bypass, not a success.
    StampFailed,
    /// Frontmatter did not parse. Nothing to stamp, and nothing to
    /// quarantine: the skill cannot load into a `SkillEntry::Ok` at all (see
    /// `SkillRegistry::load`), so `RunSkill` will refuse it and the review
    /// queue — which reads parsed records — will not list it either.
    Unloadable,
}

#[async_trait]
impl EngineTool for SkillRegister {
    fn name(&self) -> &str {
        "SkillRegister"
    }
    fn description(&self) -> &str {
        prompt::DESCRIPTION_SKILL_WRITE
    }
    fn input_schema(&self) -> Value {
        prompt::skill_write_input_schema()
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let name = match input.get("name").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("missing required field: name", true)),
        };
        let description = match input.get("description").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("missing required field: description", true)),
        };
        let body = match input.get("body").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("missing required field: body", true)),
        };
        let override_existing =
            input.get("override").and_then(|v| v.as_bool()).unwrap_or(false);

        if let Err(msg) = validate_skill_name(&name) {
            return Ok(ToolOutput::error(msg, true));
        }
        if let Err(msg) = validate_skill_description(&description) {
            return Ok(ToolOutput::error(msg, true));
        }

        // Warn about unknown allowed-tools in body frontmatter.
        let parsed_frontmatter = ao_engine_tools_core::skill_registry::parse_frontmatter(&body);
        if let Ok(parsed) = &parsed_frontmatter {
            for tool_name in &parsed.allowed_tools {
                if ctx.registry.lookup_engine(tool_name).is_none() {
                    tracing::warn!(
                        tool = %tool_name,
                        "SkillRegister: '{}' is not a known registered engine tool",
                        tool_name
                    );
                }
            }
        }

        let data_dir = resolve_data_root()
            .map_err(|e| AoError::Internal(format!("data root error: {e}")))?;

        let registry_snapshot = ctx.skill_registry.read().unwrap().clone();

        // Trust gate: re-establish the review
        // boundary the old `.pending/` staging area used to provide. Every
        // `SkillRegister` call is itself a model tool call — there is no
        // path through this tool for a human to have typed the body
        // directly — so every candidate here originates as
        // `CandidateOrigin::SelfAuthored`. Per the accepted boundary a
        // model-invocable skill ALWAYS stages for review — there is no
        // low-stakes/narrow-tool-allowlist carve-out (that classifier,
        // `classify_skill_stakes`, predates the trust gate and has been
        // removed: the accepted table draws no distinction by tool
        // allowlist, only by artifact type). The gate's verdict is forced
        // into the frontmatter regardless of what `disable-model-invocation`
        // value the body itself claims, because the model that authored the
        // body cannot be trusted to gate its own invocability.
        //
        // Skills whose frontmatter fails to parse are left untouched here:
        // they already fail to load into a `SkillEntry::Ok` (see
        // `SkillRegistry::load`), so `RunSkill` already can't invoke them —
        // there is nothing for the gate to quarantine.
        let (gated_body, gate_result) = match &parsed_frontmatter {
            Ok(_parsed) => {
                let decision = stage_candidate(StagingRequest {
                    artifact_type: ArtifactType::Skill,
                    origin: CandidateOrigin::SelfAuthored,
                    scope: CandidateScope::Agent,
                    contradicts_existing: false,
                    overwrites_manual: false,
                });
                debug_assert!(
                    !decision.auto_enable(),
                    "a self-authored Skill candidate must never auto-enable"
                );
                match set_disable_model_invocation(&body, !decision.auto_enable()) {
                    Ok(rewritten) => (rewritten, GateResult::Parked),
                    Err(e) => {
                        tracing::warn!(
                            "SkillRegister: trust gate could not rewrite frontmatter for '{}': {}",
                            name,
                            e
                        );
                        (body.clone(), GateResult::StampFailed)
                    }
                }
            }
            Err(_) => (body.clone(), GateResult::Unloadable),
        };

        // Versioning: a skill starts at version 1;
        // re-registering over an existing name — whether a live skill or a
        // stale load-error entry — bumps it by 1. Skipped alongside the gate
        // stamp above when the submitted frontmatter itself fails to parse,
        // since there is nothing valid to stamp a version onto.
        let versioned_body = if parsed_frontmatter.is_ok() {
            let next_version = match registry_snapshot.get(&name) {
                Some(SkillEntry::Ok(existing)) => existing.version.saturating_add(1),
                _ => 1,
            };
            match set_version(&gated_body, next_version) {
                Ok(rewritten) => rewritten,
                Err(e) => {
                    tracing::warn!(
                        "SkillRegister: could not stamp version for '{}': {}",
                        name,
                        e
                    );
                    gated_body.clone()
                }
            }
        } else {
            gated_body.clone()
        };

        match write_skill_to_user_pool(
            &data_dir,
            &ctx.agent_id,
            &name,
            &versioned_body,
            override_existing,
            &registry_snapshot,
        )
        .await
        {
            Ok((_, updated_profile)) => {
                // Reload registry and replace it in context.
                let new_registry =
                    Arc::new(SkillRegistry::load(&data_dir, &updated_profile));
                ctx.replace_skill_registry(new_registry);
                let message = match gate_result {
                    GateResult::Parked => format!(
                        "Skill '{}' written and staged for review — not yet model-invocable \
                         (disable-model-invocation is set). Approve it in Studio under \
                         Memories → Skills held for review; RunSkill will not dispatch it \
                         until then, and no tool call can approve it.",
                        name
                    ),
                    GateResult::StampFailed => format!(
                        "Skill '{}' written, but the review gate could not stamp its \
                         frontmatter, so it was NOT staged for review and its \
                         disable-model-invocation value is whatever the body specified. \
                         Re-register it with valid frontmatter.",
                        name
                    ),
                    GateResult::Unloadable => format!(
                        "Skill '{}' written, but its frontmatter does not parse, so it \
                         cannot load: RunSkill will refuse it and it will not appear in the \
                         review queue. Re-register it with valid YAML frontmatter \
                         (`name` and `description` are required).",
                        name
                    ),
                };
                Ok(ToolOutput::text(message))
            }
            Err(SkillWriteError::SkillExists) => Ok(ToolOutput::error(
                format!("skill '{}' already exists (SkillExists)", name),
                true,
            )),
            Err(SkillWriteError::SkillCollidesWithPlugin) => Ok(ToolOutput::error(
                format!(
                    "skill '{}' collides with a plugin-pool skill (SkillCollidesWithPlugin)",
                    name
                ),
                true,
            )),
            Err(SkillWriteError::ProfileNotFound) => Ok(ToolOutput::error(
                format!("agent profile for '{}' not found", ctx.agent_id),
                true,
            )),
            Err(e) => Err(AoError::Internal(format!("SkillRegister error: {}", e))),
        }
    }
}
