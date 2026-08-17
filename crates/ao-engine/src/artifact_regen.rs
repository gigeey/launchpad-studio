//! Reusable core for "spawn a background subagent to rewrite one artifact in
//! place." Backs both the regenerate endpoint (`POST
//! .../artifacts/{id}/regenerate`) and the chat-to-adjust endpoint (`POST
//! .../artifacts/{id}/chat`) — the two are just a different `seed_prompt` /
//! [`ArtifactAgentMode`] fed into the same [`spawn_artifact_agent`].
//!
//! # Architecture
//!
//! No model/provider/HTTP client is constructed here. The subagent runs as
//! the artifact's OWNING agent's own [`AgentProfile`] — same model, same
//! tools (so a websearch-backed refresh can actually rerun the search), same
//! runner_mode — driven through [`AppState::spawner`], the identical
//! [`SubagentSpawner`] instance the `Delegate` tool already uses for every
//! agent-initiated background delegation. This module only supplies the
//! synthetic "parent" side of that call: an HTTP route has no live
//! [`RunnerContext`] of its own to delegate from, so one is built fresh here,
//! mirroring how `handle_mcp_request` builds a per-request context for the
//! MCP HTTP path (`crates/ao-server/src/routes/mcp.rs`).
//!
//! Note that almost none of that synthetic context's fields end up mattering:
//! `SubagentSpawner::spawn_named_async_id` resolves a *profile*-based child,
//! and `ProfileAwareChildRunner` (the production `ChildRunner`) rebuilds the
//! child's registry/system-prompt/cwd from `target_profile` itself rather
//! than inheriting them from the parent context — see that type's doc
//! comment. Only `agent_id`/`session_id`/`cwd` (recorded as parent lineage)
//! and `delegate_chain`/`background_agents` (guard bookkeeping) carry through.

use std::path::PathBuf;
use std::sync::Arc;

use ao_engine_tools_core::background_agents::BackgroundAgentId;
use ao_engine_tools_core::context::RunnerContext;
use ao_protocol::artifact::IntentSource;
use ao_protocol::error::AoError;

use crate::artifact_task_status::ArtifactTaskCompletionSink;
use crate::state::AppState;

/// Distinguishes the two seed sources that call [`spawn_artifact_agent`]:
/// whole-artifact regenerate (`origin_intent.refresh_prompt` replayed
/// verbatim) and chat-to-adjust (an arbitrary user message about one
/// specific artifact, assembled by `ao_server::routes::artifacts::chat_artifact`
/// from the message plus recent intent-ledger/thread context). The seed
/// instruction's wording differs slightly per mode so a "regenerate from
/// scratch" phrasing doesn't leak into what is actually meant as a targeted
/// edit request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactAgentMode {
    /// Replay `origin_intent.refresh_prompt` — redo the whole artifact from
    /// scratch (e.g. rerun the websearch that seeded it).
    Regenerate,
    /// Apply one targeted adjustment described in a chat message, preserving
    /// the rest of the artifact where the request doesn't ask otherwise.
    ChatAdjust,
}

impl ArtifactAgentMode {
    fn instruction_verb(self) -> &'static str {
        match self {
            ArtifactAgentMode::Regenerate => {
                "regenerate it from scratch, following the original request below exactly"
            }
            ArtifactAgentMode::ChatAdjust => {
                "apply the following adjustment, preserving everything else about the artifact \
                 that the request doesn't ask you to change"
            }
        }
    }

    /// The [`IntentSource`] the spawned subagent's `ArtifactWrite` call
    /// should be tagged with in the artifact's intent ledger.
    fn intent_source(self) -> IntentSource {
        match self {
            ArtifactAgentMode::Regenerate => IntentSource::Regenerate,
            ArtifactAgentMode::ChatAdjust => IntentSource::Chat,
        }
    }
}

/// Resolve `agent_id`'s [`AgentProfile`](ao_protocol::agent::AgentProfile),
/// load `artifact_id`'s current payload for context, and spawn a background
/// subagent — running AS that same agent profile — to rewrite it in place via
/// `ArtifactWrite(id=artifact_id)`.
///
/// Returns the spawned [`BackgroundAgentId`] immediately; this function does
/// not wait for the subagent to finish. There is no separate completion
/// surface — callers observe completion by polling the artifact and watching
/// `updated_at` (bumped by a successful in-place `ArtifactWrite`).
///
/// Callers are responsible for deciding *whether* it's valid to call this for
/// a given artifact (e.g. the regenerate route gates on
/// `refresh_intent == WholeArtifact` and a non-empty
/// `origin_intent.refresh_prompt`) — this function itself is mode-agnostic
/// and will happily rewrite any artifact it's pointed at.
pub async fn spawn_artifact_agent(
    state: &AppState,
    agent_id: &str,
    artifact_id: &str,
    seed_prompt: String,
    mode: ArtifactAgentMode,
) -> Result<BackgroundAgentId, AoError> {
    let profile = state
        .persistence
        .agents
        .get(agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.to_string()))?;

    // Current payload, for context. Every stored blob is either raw HTML
    // markup or `serde_json`-serialized text, both of which are valid UTF-8,
    // so a lossy decode never actually loses anything in practice.
    let (record, payload_bytes) = state
        .persistence
        .artifacts
        .get_payload(agent_id, artifact_id)
        .await?;
    let current_content = String::from_utf8_lossy(&payload_bytes);

    let directive = format!(
        "You are updating artifact {artifact_id} IN PLACE. Call ArtifactWrite with \
         id=\"{artifact_id}\" to overwrite it — do not create a new artifact. \
         The artifact's title is \"{title}\" ({kind:?}, {format:?} format).\n\n\
         Request: {verb}: {seed_prompt}\n\n\
         ## Current artifact content\n\n{current_content}",
        artifact_id = artifact_id,
        title = record.title,
        kind = record.kind,
        format = record.format,
        verb = mode.instruction_verb(),
        seed_prompt = seed_prompt,
        current_content = current_content,
    );

    // Synthetic "parent" for this self-delegation — the owning agent
    // delegating to a background instance of itself. See the module doc for
    // why the heavier RunnerContext fields (registry, cwd, memory_loader...)
    // don't need real values here: the profile-based child runner ignores
    // them and resolves everything from `profile` instead.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let completion_sink = Arc::new(ArtifactTaskCompletionSink {
        status: Arc::clone(&state.artifact_task_status),
        persistence: Arc::clone(&state.persistence),
        agent_id: agent_id.to_string(),
        artifact_id: artifact_id.to_string(),
    });
    let parent_ctx = RunnerContext::new_with_cwd(uuid::Uuid::new_v4().to_string(), agent_id.to_string(), cwd)
        .with_artifact_intent_source(mode.intent_source())
        .with_delegate_completion_sink(completion_sink);

    let target_name = profile.name.clone();
    let bg_id = state
        .spawner
        .spawn_named_async_id(&parent_ctx, &profile, directive, false, target_name)
        .await
        .map_err(|tool_output| {
            AoError::Internal(format!(
                "failed to spawn artifact agent for artifact '{artifact_id}': {}",
                tool_output.as_text()
            ))
        })?;

    state
        .artifact_task_status
        .mark_running(bg_id.to_string(), artifact_id.to_string());

    Ok(bg_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regenerate_verb_mentions_from_scratch() {
        assert!(ArtifactAgentMode::Regenerate
            .instruction_verb()
            .contains("from scratch"));
    }

    #[test]
    fn chat_adjust_verb_mentions_preserving_the_rest() {
        assert!(ArtifactAgentMode::ChatAdjust
            .instruction_verb()
            .contains("preserving"));
    }
}
