use std::sync::Arc;

use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, ChildRunner, RunnerEvent, TaskFinalReport,
};
use ao_engine_tools_core::{RunnerContext, SessionKind};
use ao_protocol::error::AoError;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::message::{ContentBlock, Message};
use crate::query_loop::{run_session, RunnerConfig};

/// A [`ChildRunner`] that launches child sessions via [`run_session`].
///
/// Each call to [`launch`](ChildRunner::launch) spawns a tokio task that:
/// 1. Wraps `initial_prompt` in a single `User` turn.
/// 2. Drives [`run_session`] to completion.
/// 3. Emits a terminal [`RunnerEvent::Completed`] or [`RunnerEvent::Cancelled`]
///    on `event_tx` before resolving.
/// 4. Maps [`SessionOutcome`](crate::query_loop::SessionOutcome) to
///    [`TaskFinalReport`].
pub struct SessionChildRunner {
    provider: Arc<dyn crate::provider::ProviderClient>,
    bridge: Arc<dyn crate::prompt_bridge::UserPromptBridge>,
    denial_tracker: Arc<dyn ao_engine_tools_core::DenialTracker>,
    settings: crate::hooks::config::RunnerSettings,
    mode: ao_engine_tools_core::PermissionMode,
    system_prompt: Option<String>,
    /// Propagated from the parent `RunnerConfig` so bounded child sessions
    /// (e.g. inspection verifiers) have their turn cap enforced inside the
    /// child's own `run_session` call.
    max_turns: Option<usize>,
}

impl SessionChildRunner {
    pub fn new(config: &RunnerConfig) -> Arc<Self> {
        Arc::new(Self {
            provider: config.provider.clone(),
            bridge: config.bridge.clone(),
            denial_tracker: config.denial_tracker.clone(),
            settings: config.settings.clone(),
            mode: config.mode,
            system_prompt: config.system_prompt.clone(),
            max_turns: config.max_turns,
        })
    }
}

impl ChildRunner for SessionChildRunner {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        _target_profile: Option<ao_protocol::agent::AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        // Background subagents report through TaskFinalReport, not through
        // the parent session's live event stream — propagating the parent's
        // sink would interleave child chunks into the operator's terminal.
        let config = RunnerConfig {
            provider: self.provider.clone(),
            bridge: self.bridge.clone(),
            denial_tracker: self.denial_tracker.clone(),
            settings: self.settings.clone(),
            mode: self.mode,
            kind: SessionKind::Autonomous,
            auto_approve: vec![],
            system_prompt: self.system_prompt.clone(),
            event_sink: None,
            // Background subagent — no profile-level thinking config plumbed
            // here; defaults to "no extended thinking" on the API path.
            thinking: None,
            max_turns: self.max_turns,
        };
        let bg_id = background_agent_id;
        tokio::spawn(async move {
            // Held for the lifetime of this detached child run so the process
            // knows background agent work is in flight even though this task
            // is invisible to `InstanceRegistry` (see `background_activity`
            // module docs). Drops — and releases — on every exit path,
            // including a panic inside `run_session`.
            let _activity_guard = ao_protocol::background_activity::background_activity_guard();
            let initial_messages = vec![Message::User {
                content: vec![ContentBlock::Text { text: initial_prompt }],
            }];
            let start = std::time::Instant::now();
            match run_session(initial_messages, child_ctx, config).await {
                Ok(outcome) => {
                    let elapsed_ms = start.elapsed().as_millis() as u64;
                    let turns = outcome.turns as u32;
                    let report = if outcome.cancelled {
                        let _ = event_tx.send(RunnerEvent::Cancelled {
                            background_agent_id: bg_id,
                        });
                        TaskFinalReport::cancelled().with_stats(elapsed_ms, turns)
                    } else {
                        let text = (!outcome.final_assistant_text.is_empty())
                            .then_some(outcome.final_assistant_text);
                        let _ = event_tx.send(RunnerEvent::Completed {
                            background_agent_id: bg_id,
                        });
                        TaskFinalReport::completed(text).with_stats(elapsed_ms, turns)
                    };
                    Ok(report)
                }
                Err(e) => {
                    let _ = event_tx.send(RunnerEvent::Cancelled {
                        background_agent_id: bg_id,
                    });
                    Ok(TaskFinalReport::failed(e.to_string()))
                }
            }
        })
    }
}
