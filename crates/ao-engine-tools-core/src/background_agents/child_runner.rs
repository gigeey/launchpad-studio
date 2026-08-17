use ao_protocol::agent::AgentProfile;
use ao_protocol::error::AoError;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::context::RunnerContext;

use super::handle::{BackgroundAgentId, RunnerEvent, TaskFinalReport};

/// Launches a child agent session asynchronously.
///
/// Implementors own the execution strategy. The `event_tx` sender is moved
/// into the spawned task so the child can emit [`RunnerEvent`]s as it runs.
/// The [`JoinHandle`] resolves to a [`TaskFinalReport`] when the session
/// ends (normally, cancelled, or failed).
///
/// The production implementation in `ao-engine` wraps the full runner
/// dispatch path for named-profile targets, or drives `run_session`
/// in-process for built-in catalog subagents.
///
/// # Contract
///
/// Before the join handle resolves, the implementation must emit at least a
/// terminal [`RunnerEvent::Completed`] or [`RunnerEvent::Cancelled`] on
/// `event_tx` so observers can detect session end without polling the handle.
pub trait ChildRunner: Send + Sync {
    /// Spawn a child session and return its [`JoinHandle`].
    ///
    /// `target_profile` carries the resolved [`AgentProfile`] for named-delegate
    /// launches so the runner can select the right provider, model, runner mode,
    /// skills, and workflows. `None` means the child is a built-in catalog
    /// subagent (e.g. Explore, general-purpose) — the runner drives it
    /// in-process against the default API path.
    fn launch(
        &self,
        child_ctx: RunnerContext,
        initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        target_profile: Option<AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>>;
}
