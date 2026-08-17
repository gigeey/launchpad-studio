pub mod envelope;
mod prompt;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use ao_engine_tools_core::background_agents::{
    RunnerEvent, SubagentRegistry, SubagentSpawner, TaskFinalStatus,
};
use ao_engine_tools_core::{IoTool, Registry, RunnerContext, ToolOutput};
use ao_persistence::profiles::AgentProfileStore;
use ao_protocol::{
    agent::{AgentProfile, DelegateTarget},
    data_root::resolve_data_root,
    error::AoError,
};
use async_trait::async_trait;
use serde_json::Value;

pub use envelope::{build_envelope, format_history_block, name_or_prefix, EnvelopeMode};

/// How many of the parent's most-recent transcript entries to include when
/// `share_context: true`. Sized to cover a few active turns of multi-tool
/// dialogue without blowing the child's prompt budget — the byte-budget cap
/// inside [`format_history_block`] is the load-bearing limit, this just
/// bounds the read.
const FORK_TRANSCRIPT_ENTRY_LIMIT: usize = 60;

/// Delegate IoTool — the single front door for handing a task to another
/// agent.
///
/// It resolves `target` over two namespaces, in priority order:
/// 1. **Address book** — a `delegates_to` entry on the calling agent's
///    profile. These are user-configured delegates that may carry their own
///    tools, skills, and instructions; this path forks via [`AgentProfile`]
///    and fires the `kind="delegate"` usage counter.
/// 2. **Catalog subagent type** — a [`SubagentRegistry`] entry. No built-in
///    catalog ships with the engine (entries are populated by feature code,
///    e.g. skill fork-mode); omitting `target` instead clones the calling
///    agent's own profile, or — when the caller has no profile — returns a
///    recoverable error naming the fix. This path fires the `kind="agent"`
///    counter on a successful spawn.
///
/// `spawner`, `agent_store`, and `subagent_registry` are optional so the tool
/// can be registered in `register_all` without runtime wiring; state.rs
/// replaces the entry with a fully-wired instance at AppState construction
/// time. `description` is precomputed once at wiring time rather than
/// rebuilt on every call.
pub struct Delegate {
    spawner: Option<Arc<SubagentSpawner>>,
    agent_store: Option<Arc<AgentProfileStore>>,
    subagent_registry: Option<Arc<SubagentRegistry>>,
    description: String,
}

impl Delegate {
    pub fn new() -> Self {
        Self {
            spawner: None,
            agent_store: None,
            subagent_registry: None,
            description: prompt::DESCRIPTION.to_string(),
        }
    }

    pub fn with_spawner_and_store(
        spawner: Arc<SubagentSpawner>,
        agent_store: Arc<AgentProfileStore>,
    ) -> Self {
        let subagent_registry = spawner.subagent_registry();
        let description = prompt::build_description(&subagent_registry);
        Self {
            spawner: Some(spawner),
            agent_store: Some(agent_store),
            subagent_registry: Some(subagent_registry),
            description,
        }
    }
}

impl Default for Delegate {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl IoTool for Delegate {
    fn name(&self) -> &str {
        "Delegate"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        prompt::input_schema()
    }

    /// Spawning a child agent is permission-light from the parent's perspective;
    /// the child's individual tool calls are permission-gated server-side by
    /// our own system. Marking read-only allows MCP clients to issue multiple
    /// sync Delegate calls concurrently (one per tool_use block in the same
    /// assistant message) without serialising them. Transcript isolation ensures
    /// children do not share state with the parent's conversation.
    fn is_concurrency_safe(&self) -> bool {
        true
    }

    /// Delegate interacts with unpredictable external systems (it runs an
    /// arbitrary child agent that may make network calls, write files, etc.).
    /// The open-world hint lets MCP clients apply looser permission-caching for
    /// the spawn call itself, consistent with the child's own tool calls
    /// carrying the real permission checks.
    fn mcp_open_world_hint(&self) -> bool {
        true
    }

    fn cli_compatible(&self) -> bool {
        true
    }

    async fn invoke(&self, input: Value, ctx: &RunnerContext) -> Result<ToolOutput, AoError> {
        let (spawner, agent_store) = match (&self.spawner, &self.agent_store) {
            (Some(s), Some(a)) => (s, a),
            _ => {
                return Ok(ToolOutput::error(
                    "Delegate requires a spawner and agent store (none configured in this context)",
                    false,
                ))
            }
        };

        let directive = match input.get("directive").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return Ok(ToolOutput::error("missing required field: directive", true)),
        };
        // mode defaults to sync; target is optional (omitted => clone the caller's own profile).
        let mode = input
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("sync")
            .to_string();
        if mode != "sync" && mode != "async" {
            return Ok(ToolOutput::error(
                format!("unknown mode '{}'; expected 'sync' or 'async'", mode),
                true,
            ));
        }
        let target_opt = input
            .get("target")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let share_context = input
            .get("share_context")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Load the calling agent's profile for its delegates_to address book.
        // A missing profile is NOT an error — the agent simply has no address
        // book, so we fall through to the generic subagent path.
        let parent_profile = match agent_store.get(&ctx.agent_id).await {
            Ok(opt) => opt,
            Err(e) => {
                return Ok(ToolOutput::error(
                    format!("failed to load agent profile: {e}"),
                    false,
                ))
            }
        };

        // Resolve an address-book entry only when `target` names one.
        let addr_entry = match (&target_opt, &parent_profile) {
            (Some(name), Some(profile)) => {
                profile.delegates_to.iter().find(|e| &e.name == name).cloned()
            }
            _ => None,
        };

        if let Some(entry) = addr_entry {
            // SAFETY: addr_entry is only Some when parent_profile is Some.
            let parent_profile = parent_profile.as_ref().expect("address-book entry implies a parent profile");
            self.run_address_book_delegation(
                spawner,
                agent_store,
                ctx,
                parent_profile,
                entry,
                directive,
                &mode,
                share_context,
            )
            .await
        } else {
            self.run_subagent_delegation(
                spawner,
                ctx,
                parent_profile.as_ref(),
                target_opt,
                directive,
                &mode,
                share_context,
            )
            .await
        }
    }
}

impl Delegate {
    /// Hand a directive to a user-configured address-book agent (the
    /// `delegates_to` path). Preserves the share_context fork semantics and
    /// fires the `kind="delegate"` usage counter.
    #[allow(clippy::too_many_arguments)]
    async fn run_address_book_delegation(
        &self,
        spawner: &Arc<SubagentSpawner>,
        agent_store: &Arc<AgentProfileStore>,
        ctx: &RunnerContext,
        parent_profile: &AgentProfile,
        entry: DelegateTarget,
        directive: String,
        mode: &str,
        share_context: bool,
    ) -> Result<ToolOutput, AoError> {
        let target_name = entry.name.clone();

        // Gate on share_context_allowed.
        if share_context && !entry.share_context_allowed {
            return Ok(ToolOutput::error(
                format!(
                    "target '{}' does not allow context sharing (share_context_allowed: false)",
                    target_name
                ),
                true,
            ));
        }

        // Look up the target agent profile from persistence.
        let target_profile = match agent_store.get(&entry.target_agent_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Ok(ToolOutput::error(
                    format!(
                        "target agent '{}' (id: '{}') not found; address book entry may be stale",
                        target_name, entry.target_agent_id
                    ),
                    true,
                ))
            }
            Err(e) => {
                return Ok(ToolOutput::error(
                    format!("failed to load target agent profile: {e}"),
                    false,
                ))
            }
        };

        // Cycle detection — telemetry only, not a hard reject (mutual delegation is legitimate).
        if ctx.delegate_chain.contains(&entry.target_agent_id) {
            tracing::debug!(
                agent_id = %ctx.agent_id,
                target_id = %entry.target_agent_id,
                chain = ?ctx.delegate_chain,
                "delegate cycle detected in chain; mutual delegation is allowed"
            );
        }

        // Resolve display names with ID prefix fallback.
        let parent_display = name_or_prefix(&parent_profile.name, &parent_profile.id);
        let child_display = name_or_prefix(&target_profile.name, &target_profile.id);

        let envelope_mode = if share_context {
            EnvelopeMode::ForkShared
        } else {
            EnvelopeMode::Fresh
        };
        let envelope_text = build_envelope(parent_display, child_display, envelope_mode, &directive);

        // share_context: true is a no-op unless the parent's actual conversation
        // gets forwarded to the child. The runner reads history keyed by the
        // child's own agent_id (= target_profile.id), so without an explicit
        // injection here, the child starts clean-room despite the fork envelope
        // promising otherwise. Pull the parent's most-recent transcript entries
        // off the shared TranscriptStore and prepend them as a
        // `[Conversation history]` block to the directive. When no store is
        // configured (test contexts, headless runs) we silently skip injection
        // — the envelope text still tells the child it's in fork mode, which
        // beats erroring out and breaking otherwise-working delegations.
        let wrapped = if share_context {
            match &ctx.transcript_store {
                Some(store) => {
                    let entries = store
                        .read_recent(&ctx.agent_id, FORK_TRANSCRIPT_ENTRY_LIMIT)
                        .await
                        .unwrap_or_default();
                    let history_block = format_history_block(&entries);
                    if history_block.is_empty() {
                        envelope_text
                    } else {
                        format!("{}\n\n{}", history_block, envelope_text)
                    }
                }
                None => envelope_text,
            }
        } else {
            envelope_text
        };

        // Clone before target_name is potentially moved into spawn_named_async.
        let target_for_log = target_name.clone();

        let result = match mode {
            "sync" => {
                spawner
                    .spawn_named(ctx, &target_profile, wrapped, share_context)
                    .await
            }
            "async" => {
                spawner
                    .spawn_named_async(ctx, &target_profile, wrapped, share_context, target_name)
                    .await
            }
            other => {
                return Ok(ToolOutput::error(
                    format!("unknown mode '{}'; expected 'sync' or 'async'", other),
                    true,
                ))
            }
        };

        fire_delegate_usage_counter(&ctx.agent_id, target_for_log);

        Ok(result)
    }

    /// Resolve and execute a subagent delegation when the address-book lookup
    /// did not match. Implements four-step resolution:
    ///
    /// 1. `target` names a catalog subagent type registered in the
    ///    [`SubagentRegistry`] → catalog spawn.
    /// 2. `target` unknown in both namespaces → error (enumerates both).
    /// 3. No `target` AND parent profile → clone parent (`spawn_named` with the
    ///    caller's own profile, honoring `share_context` transcript-only).
    /// 4. No `target` AND no parent profile → recoverable error; there is no
    ///    default stranger agent to fall back to.
    ///
    /// Fires the `kind="agent"` usage counter on a successful spawn (cases 1
    /// and 3).
    async fn run_subagent_delegation(
        &self,
        spawner: &Arc<SubagentSpawner>,
        ctx: &RunnerContext,
        parent_profile: Option<&AgentProfile>,
        target_opt: Option<String>,
        directive: String,
        mode: &str,
        share_context: bool,
    ) -> Result<ToolOutput, AoError> {
        let registry = match &self.subagent_registry {
            Some(r) => r,
            None => {
                return Ok(ToolOutput::error(
                    "Delegate subagent catalog unavailable (no registry configured in this context)",
                    false,
                ))
            }
        };

        // Step 3: no `target` AND parent profile → clone parent profile.
        // The child runs as a fresh instance of the caller: same provider,
        // runner_mode, skills, workflows, and composed prompt. share_context
        // is transcript-only (prepends history; profile drives everything else).
        if target_opt.is_none() {
            if let Some(profile) = parent_profile {
                let display = name_or_prefix(&profile.name, &profile.id);
                let envelope_mode = if share_context {
                    EnvelopeMode::ForkShared
                } else {
                    EnvelopeMode::Fresh
                };
                let envelope_text = build_envelope(display, display, envelope_mode, &directive);

                let wrapped = if share_context {
                    match &ctx.transcript_store {
                        Some(store) => {
                            let entries = store
                                .read_recent(&ctx.agent_id, FORK_TRANSCRIPT_ENTRY_LIMIT)
                                .await
                                .unwrap_or_default();
                            let history_block = format_history_block(&entries);
                            if history_block.is_empty() {
                                envelope_text
                            } else {
                                format!("{}\n\n{}", history_block, envelope_text)
                            }
                        }
                        None => envelope_text,
                    }
                } else {
                    envelope_text
                };

                let profile_id = profile.id.clone();
                let profile_name = profile.name.clone();
                let result = match mode {
                    "sync" => spawner.spawn_named(ctx, profile, wrapped, share_context).await,
                    "async" => {
                        spawner
                            .spawn_named_async(ctx, profile, wrapped, share_context, profile_name)
                            .await
                    }
                    other => {
                        return Ok(ToolOutput::error(
                            format!("unknown mode '{}'; expected 'sync' or 'async'", other),
                            true,
                        ))
                    }
                };
                fire_agent_usage_counter(&ctx.agent_id, &profile_id);
                return Ok(result);
            }
        }

        // Step 2: `target` explicitly names a catalog subagent type.
        // Step 4: no `target` AND no parent profile → there is no default
        // stranger agent to spawn. Surface a recoverable error naming the fix
        // instead of guessing.
        let subagent_type = match target_opt {
            Some(t) => t,
            None => {
                return Ok(ToolOutput::error(
                    "no target was given, and the calling agent's own profile could not be \
                     resolved, so there is no agent to clone. Retry with an explicit target \
                     naming an address-book agent.",
                    true,
                ));
            }
        };

        // Validate — enumerates BOTH namespaces so the model can self-correct.
        if registry.lookup_by_id(&subagent_type).is_err() {
            let addr_names: Vec<String> = parent_profile
                .map(|p| p.delegates_to.iter().map(|e| e.name.clone()).collect())
                .unwrap_or_default();
            let builtin_ids: Vec<String> = registry.list().iter().map(|d| d.id.clone()).collect();
            return Ok(ToolOutput::error(
                format!(
                    "unknown target '{}'. Available address-book targets: {:?}. \
                     Available subagent types: {:?}.",
                    subagent_type, addr_names, builtin_ids
                ),
                true,
            ));
        }

        // Spawn the child. The same call backs both modes; the difference is
        // whether we keep the handle live (async) or await it (sync).
        let (bg_id, _rx) = match spawner.spawn(ctx, &subagent_type, directive).await {
            Ok(pair) => pair,
            Err(e) => return Ok(e.to_tool_output()),
        };

        fire_agent_usage_counter(&ctx.agent_id, &subagent_type);

        if mode == "async" {
            // Leave the handle in the registry so DelegateOutput can poll it.
            let spawned_at = ctx
                .background_agents
                .get(&bg_id)
                .await
                .expect("handle just inserted by spawn")
                .spawned_at;

            let _ = ctx.runner_events.send(RunnerEvent::AsyncLaunched {
                background_agent_id: bg_id.clone(),
                subagent_type: subagent_type.clone(),
                parent_agent_id: ctx.agent_id.clone(),
                spawned_at,
            });

            let transcript_path = resolve_data_root()
                .map(|r| {
                    r.join("messages")
                        .join("data")
                        .join(format!("{}.jsonl", bg_id))
                        .display()
                        .to_string()
                })
                .unwrap_or_default();

            return Ok(ToolOutput::text(if transcript_path.is_empty() {
                format!(
                    "Delegated to {} in background (delegation_id={})\nPoll with DelegateOutput (supports wait_seconds; results survive restarts).",
                    subagent_type, bg_id
                )
            } else {
                format!(
                    "Delegated to {} in background (delegation_id={})\ntranscript_path={}\nPoll with DelegateOutput (supports wait_seconds; results survive restarts).",
                    subagent_type, bg_id, transcript_path
                )
            }));
        }

        // --- Sync mode: take ownership of the handle and await it. ---
        let mut handle = match ctx.background_agents.remove(&bg_id).await {
            Some(h) => h,
            None => {
                return Ok(ToolOutput::error(
                    "internal: background agent handle not found after spawn",
                    false,
                ))
            }
        };

        let report = tokio::select! {
            result = &mut handle.join => {
                match result {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        return Ok(ToolOutput::error(format!("subagent runner error: {e}"), false))
                    }
                    Err(e) => {
                        return Ok(ToolOutput::error(format!("subagent task panicked: {e}"), false))
                    }
                }
            }
            _ = ctx.cancel.cancelled() => {
                handle.cancel.cancel();
                return Ok(ToolOutput::error("delegation cancelled by parent", true));
            }
        };

        match report.status {
            TaskFinalStatus::Completed | TaskFinalStatus::Failed => {
                match report.final_assistant_text {
                    Some(text) => Ok(ToolOutput::text(format_with_stats(
                        &text,
                        report.duration_ms,
                        report.num_turns,
                    ))),
                    None => Ok(ToolOutput::error(
                        "subagent completed without producing any output",
                        false,
                    )),
                }
            }
            TaskFinalStatus::Cancelled => Ok(ToolOutput::error(
                "subagent was cancelled before completing",
                true,
            )),
        }
    }
}

/// Append a compact stats line to `text` when duration or turn count is known.
///
/// Returns the text unchanged when neither stat is available (e.g. test mocks
/// that do not record timing). When at least one stat is present the line is
/// appended as `\n\n[stats: duration=Xms, turns=N]` so it reads as a clearly
/// separate annotation rather than content.
fn format_with_stats(text: &str, duration_ms: Option<u64>, num_turns: Option<u32>) -> String {
    match (duration_ms, num_turns) {
        (Some(d), Some(t)) => format!("{}\n\n[stats: duration={}ms, turns={}]", text, d, t),
        (Some(d), None) => format!("{}\n\n[stats: duration={}ms]", text, d),
        (None, Some(t)) => format!("{}\n\n[stats: turns={}]", text, t),
        (None, None) => text.to_string(),
    }
}

/// Fire-and-forget `kind="delegate"` usage counter for an address-book
/// delegation. Failure must NOT fail the tool call.
fn fire_delegate_usage_counter(agent_id: &str, target: String) {
    let agent_id = agent_id.to_string();
    tokio::spawn(async move {
        match resolve_data_root() {
            Ok(data_root) => {
                let agent_data_dir = data_root.join("agents").join(&agent_id);
                match ao_engine_tools_core::delegation_usage::increment_delegate(&agent_data_dir)
                    .await
                {
                    Ok(entry) => {
                        let total = entry.delegate_count + entry.agent_count;
                        let ratio = if total == 0 {
                            0.0_f64
                        } else {
                            entry.delegate_count as f64 / total as f64
                        };
                        tracing::info!(
                            event = "delegation_usage",
                            agent_id = %agent_id,
                            kind = "delegate",
                            target = %target,
                            delegate_count = entry.delegate_count,
                            agent_count = entry.agent_count,
                            ratio = ratio,
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Failed to update delegation usage counter (delegate): {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to resolve data root for delegation usage counter: {e}");
            }
        }
    });
}

/// Fire-and-forget `kind="agent"` usage counter for a generic subagent
/// delegation. Failure must NOT fail the tool call.
fn fire_agent_usage_counter(agent_id: &str, subagent_type: &str) {
    let agent_id = agent_id.to_string();
    let subagent_type = subagent_type.to_string();
    tokio::spawn(async move {
        match resolve_data_root() {
            Ok(data_root) => {
                let agent_data_dir = data_root.join("agents").join(&agent_id);
                match ao_engine_tools_core::delegation_usage::increment_agent(&agent_data_dir).await
                {
                    Ok(entry) => {
                        let total = entry.delegate_count + entry.agent_count;
                        let ratio = if total == 0 {
                            0.0_f64
                        } else {
                            entry.delegate_count as f64 / total as f64
                        };
                        tracing::info!(
                            event = "delegation_usage",
                            agent_id = %agent_id,
                            kind = "agent",
                            target = %subagent_type,
                            delegate_count = entry.delegate_count,
                            agent_count = entry.agent_count,
                            ratio = ratio,
                        );
                    }
                    Err(e) => {
                        tracing::warn!("Failed to update delegation usage counter (agent): {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to resolve data root for delegation usage counter: {e}");
            }
        }
    });
}

/// Register the Delegate tool into `registry` with no-op spawner/store.
///
/// State-wired callers (e.g. AppState) should replace this entry by calling
/// `registry.register_io(Arc::new(Delegate::with_spawner_and_store(s, a)))`
/// after this call.
pub fn register(registry: &mut Registry) {
    registry.register_io(Arc::new(Delegate::new()));
}
