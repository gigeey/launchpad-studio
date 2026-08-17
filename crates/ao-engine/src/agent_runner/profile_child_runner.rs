use std::sync::{Arc, OnceLock};

use ao_engine_tools_core::background_agents::{
    BackgroundAgentId, ChildRunner, RunnerEvent, TaskFinalReport,
};
use ao_engine_tools_core::context::RunnerContext;
use ao_engine_tools_core::SessionKind;
use ao_protocol::agent::AgentProfile;
use ao_protocol::error::AoError;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::agent_runner::{AgentRunRequest, RunScope, RunnerDispatcher};
use crate::mcp_session::McpSessionStore;
use super::native::{NativeChildRunner, ProviderFactory};

/// A [`ChildRunner`] that routes named-profile delegates through the full
/// [`RunnerDispatcher`] path (CLI or API, based on the profile's `runner_mode`),
/// while keeping built-in catalog subagents on the existing in-process API path.
///
/// The dispatcher is late-bound via [`set_dispatcher`] because it is built
/// after the spawner in `AppState::new`. An [`OnceLock`] breaks the ordering
/// cycle — the same pattern used for other late-bound handles in this codebase.
pub struct ProfileAwareChildRunner {
    dispatcher: Arc<OnceLock<Arc<RunnerDispatcher>>>,
    native: NativeChildRunner,
}

impl ProfileAwareChildRunner {
    /// `provider_factory` is shared with the main-loop `NativeAgentRunner` —
    /// pass the same `Arc` at construction so both paths resolve provider and
    /// model through identical logic (see `NativeChildRunner`).
    pub fn new(
        mcp_sessions: Option<Arc<McpSessionStore>>,
        provider_factory: Arc<dyn ProviderFactory>,
    ) -> Self {
        Self {
            dispatcher: Arc::new(OnceLock::new()),
            native: NativeChildRunner::new(mcp_sessions, provider_factory),
        }
    }

    /// Late-bind the dispatcher. Must be called once after `RunnerDispatcher::new`.
    pub fn set_dispatcher(&self, dispatcher: Arc<RunnerDispatcher>) {
        let _ = self.dispatcher.set(dispatcher);
    }
}

impl ChildRunner for ProfileAwareChildRunner {
    fn launch(
        &self,
        child_ctx: RunnerContext,
        initial_prompt: String,
        background_agent_id: BackgroundAgentId,
        event_tx: broadcast::Sender<RunnerEvent>,
        target_profile: Option<AgentProfile>,
    ) -> JoinHandle<Result<TaskFinalReport, AoError>> {
        match target_profile {
            Some(profile) => {
                let dispatcher = match self.dispatcher.get() {
                    Some(d) => Arc::clone(d),
                    None => {
                        let bg_id = background_agent_id;
                        let msg = "delegate runner: dispatcher not yet bound".to_string();
                        tracing::error!(background_agent_id = %bg_id, "{}", msg);
                        return tokio::spawn(async move {
                            let _ = event_tx.send(RunnerEvent::Failed {
                                background_agent_id: bg_id,
                                error: msg.clone(),
                            });
                            Ok(TaskFinalReport::failed(msg))
                        });
                    }
                };

                let bg_id = background_agent_id;
                let cancel = child_ctx.cancel.clone();

                // Extract delegation metadata from the child context built by
                // build_delegate_context. The runner will use these to propagate
                // depth/chain caps and parent-session info correctly.
                let depth = child_ctx.depth;
                let delegate_chain = child_ctx.delegate_chain.clone();
                let spawn_chain = child_ctx.spawn_chain.clone();
                let parent_session_id = child_ctx.parent_session_id.clone();
                let parent_agent_id = child_ctx.parent_agent_id.clone();
                let parent_current_cwd = child_ctx
                    .parent_current_cwd
                    .clone()
                    .map(|p| p.to_string_lossy().into_owned());

                // Use a dummy completion channel — the result comes from run()'s
                // return value directly; the queue manager notification path is
                // not needed for child delegate runs.
                let (result_tx, _result_rx) = tokio::sync::mpsc::channel(1);

                // Give the child its own sidechain transcript and a
                // delegate-scoped live-event channel. The transcript path is
                // the same file the spawner's sidechain persister appends
                // terminal events to, so the full child run and its outcome
                // land in one place. Without these overrides a clone-parent
                // delegate (same agent_id as the caller) would stream and
                // persist its turns straight into the parent's chat history.
                let transcript_override = ao_protocol::data_root::resolve_data_root()
                    .ok()
                    .map(|root| {
                        root.join("messages")
                            .join("data")
                            .join(format!("{}.jsonl", bg_id.as_str()))
                    });
                let event_channel = Some(format!("delegate:{}", bg_id.as_str()));

                let request = AgentRunRequest {
                    agent: profile.clone(),
                    prompt: initial_prompt,
                    run_complete_tx: result_tx,
                    scope: RunScope::Standalone,
                    session_kind: SessionKind::Autonomous,
                    isolate_history: true,
                    depth,
                    delegate_chain,
                    spawn_chain,
                    parent_session_id,
                    parent_agent_id,
                    parent_current_cwd,
                    cancel: Some(cancel.clone()),
                    transcript_override,
                    event_channel,
                    ..Default::default()
                };

                let runner = dispatcher.pick(&profile);

                tokio::spawn(async move {
                    match runner.run(request).await {
                        Ok(rc) => {
                            let report = if cancel.is_cancelled() {
                                let _ = event_tx.send(RunnerEvent::Cancelled {
                                    background_agent_id: bg_id,
                                });
                                TaskFinalReport::cancelled()
                            } else {
                                let text = if rc.output_text.is_empty() {
                                    None
                                } else {
                                    Some(rc.output_text)
                                };
                                let _ = event_tx.send(RunnerEvent::Completed {
                                    background_agent_id: bg_id,
                                });
                                TaskFinalReport::completed(text)
                            };
                            Ok(report)
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            let _ = event_tx.send(RunnerEvent::Failed {
                                background_agent_id: bg_id,
                                error: msg.clone(),
                            });
                            Ok(TaskFinalReport::failed(msg))
                        }
                    }
                })
            }
            None => {
                // Built-in catalog subagents (Explore, general-purpose) run
                // in-process via the existing API path — byte-equivalent to
                // the previous NativeChildRunner behavior.
                self.native.launch(
                    child_ctx,
                    initial_prompt,
                    background_agent_id,
                    event_tx,
                    None,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    use ao_engine_tools_core::background_agents::{BackgroundAgentId, ChildRunner, RunnerEvent, TaskFinalReport, TaskFinalStatus};
    use ao_engine_tools_core::context::RunnerContext;
    use ao_protocol::agent::{AgentProfile, AgentRunnerMode};
    use ao_protocol::error::AoError;
    use ao_protocol::event::RunEndReason;

    use crate::agent_runner::{
        AgentRunRequest, AgentRunner, DefaultProviderFactory, RunComplete, RunnerDispatcher,
    };

    // ─── helpers ─────────────────────────────────────────────────────────────

    /// A mock AgentRunner that captures the last AgentRunRequest it receives.
    struct RequestCapturingRunner {
        captured: Arc<Mutex<Option<AgentRunRequest>>>,
        mode: AgentRunnerMode,
        result: Result<String, String>,
    }

    #[async_trait]
    impl AgentRunner for RequestCapturingRunner {
        async fn run(&self, req: AgentRunRequest) -> Result<RunComplete, AoError> {
            *self.captured.lock().unwrap() = Some(req);
            match &self.result {
                Ok(text) => Ok(RunComplete {
                    run_id: "test-run".to_string(),
                    output_text: text.clone(),
                    workflow_followups: vec![],
                    end_reason: RunEndReason::Completed,
                }),
                Err(msg) => Err(AoError::Internal(msg.clone())),
            }
        }
        fn mode(&self) -> AgentRunnerMode {
            self.mode
        }
    }

    fn make_cli_capturing_runner(
        captured: Arc<Mutex<Option<AgentRunRequest>>>,
        result: Result<String, String>,
    ) -> Arc<dyn AgentRunner> {
        Arc::new(RequestCapturingRunner { captured, mode: AgentRunnerMode::Cli, result })
    }

    fn make_api_capturing_runner(
        captured: Arc<Mutex<Option<AgentRunRequest>>>,
        result: Result<String, String>,
    ) -> Arc<dyn AgentRunner> {
        Arc::new(RequestCapturingRunner { captured, mode: AgentRunnerMode::Api, result })
    }

    fn make_test_profile(runner_mode: AgentRunnerMode) -> AgentProfile {
        use ao_protocol::agent::{CliProviderConfig, InputMode, OutputFormat, ProviderConfig};
        AgentProfile {
            id: "test-agent".to_string(),
            name: "Test".to_string(),
            description: "".to_string(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: "echo".to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: Default::default(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: Default::default(),
            max_instances: 1,
            timeout_seconds: 60,
            working_dir: None,
            home_dir: None,
            serialize: false,
            workflows: None,
            template: None,
            runner_mode,
            enabled_plugins: Default::default(),
            enabled_launchpad_global_skills: None,
            enabled_launchpad_project_skills: std::collections::BTreeMap::new(),
            owning_team_id: None,
            native_provider: None,
            thinking: None,
            max_output_tokens: None,
            max_context_tokens: None,
            reasoning_effort: None,
            delegates_to: vec![],
            persona: None,
            special_instructions: None,
            legacy_system_prompt: None,
            max_delegation_depth: None,
            channels: vec![],
            max_turns: None,
        }
    }

    fn make_child_ctx() -> RunnerContext {
        RunnerContext::new_with_cwd("sess-1", "child-agent", PathBuf::from("/tmp"))
    }

    async fn launch_and_join(
        runner: &super::ProfileAwareChildRunner,
        child_ctx: RunnerContext,
        profile: AgentProfile,
    ) -> TaskFinalReport {
        let (event_tx, _) = broadcast::channel::<RunnerEvent>(16);
        let bg_id = BackgroundAgentId::new();
        let handle = runner.launch(child_ctx, "do the thing".to_string(), bg_id, event_tx, Some(profile));
        handle.await.expect("launch task panicked").expect("launch returned Err")
    }

    // ─── tests ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn propagates_depth_and_delegate_chain_from_child_ctx() {
        // Verifies that depth and delegate_chain from the child context are
        // threaded into the AgentRunRequest so a grandchild delegation built
        // on top of this run will see the full ancestry chain.
        let captured = Arc::new(Mutex::new(None::<AgentRunRequest>));
        let cli = make_cli_capturing_runner(Arc::clone(&captured), Ok("ok".to_string()));
        let api = make_api_capturing_runner(Arc::new(Mutex::new(None)), Ok("ok".to_string()));
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(cli, api));

        let runner = super::ProfileAwareChildRunner::new(None, Arc::new(DefaultProviderFactory));
        runner.set_dispatcher(dispatcher);

        let mut ctx = make_child_ctx();
        ctx.depth = 3;
        ctx.delegate_chain = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        let profile = make_test_profile(AgentRunnerMode::Cli);
        launch_and_join(&runner, ctx, profile).await;

        let req = captured.lock().unwrap().take().expect("runner must have been called");
        assert_eq!(req.depth, 3, "depth must propagate from child_ctx");
        assert_eq!(req.delegate_chain, vec!["a", "b", "c"], "delegate_chain must propagate from child_ctx");
    }

    #[tokio::test]
    async fn sets_isolate_history_true_for_profile_delegates() {
        // Delegated children must never resume the target's personal history —
        // isolate_history: true ensures the runner starts with a clean slate.
        let captured = Arc::new(Mutex::new(None::<AgentRunRequest>));
        let cli = make_cli_capturing_runner(Arc::clone(&captured), Ok("ok".to_string()));
        let api = make_api_capturing_runner(Arc::new(Mutex::new(None)), Ok("ok".to_string()));
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(cli, api));

        let runner = super::ProfileAwareChildRunner::new(None, Arc::new(DefaultProviderFactory));
        runner.set_dispatcher(dispatcher);

        let ctx = make_child_ctx();
        let profile = make_test_profile(AgentRunnerMode::Cli);
        launch_and_join(&runner, ctx, profile).await;

        let req = captured.lock().unwrap().take().expect("runner must have been called");
        assert!(req.isolate_history, "profile delegates must always set isolate_history: true");
    }

    #[tokio::test]
    async fn routes_transcript_and_events_away_from_personal_channels() {
        // A delegated child must carry a sidechain transcript path
        // (messages/data/<delegation_id>.jsonl) and a delegate-scoped event
        // channel so its output never lands in the profile owner's chat
        // history — the clone-parent default delegate shares the caller's
        // agent_id, making this the only thing standing between the child's
        // turns and the parent's transcript.
        let captured = Arc::new(Mutex::new(None::<AgentRunRequest>));
        let cli = make_cli_capturing_runner(Arc::clone(&captured), Ok("ok".to_string()));
        let api = make_api_capturing_runner(Arc::new(Mutex::new(None)), Ok("ok".to_string()));
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(cli, api));

        let runner = super::ProfileAwareChildRunner::new(None, Arc::new(DefaultProviderFactory));
        runner.set_dispatcher(dispatcher);

        let (event_tx, _) = broadcast::channel::<RunnerEvent>(16);
        let bg_id = BackgroundAgentId::new();
        let bg_id_str = bg_id.as_str().to_string();
        let handle = runner.launch(
            make_child_ctx(),
            "do the thing".to_string(),
            bg_id,
            event_tx,
            Some(make_test_profile(AgentRunnerMode::Cli)),
        );
        handle.await.expect("launch task panicked").expect("launch returned Err");

        let req = captured.lock().unwrap().take().expect("runner must have been called");
        assert_eq!(
            req.event_channel.as_deref(),
            Some(format!("delegate:{}", bg_id_str).as_str()),
            "delegate children must emit on a delegate-scoped event channel"
        );
        let override_path = req
            .transcript_override
            .expect("transcript_override must be set for delegate children");
        let expected_suffix = PathBuf::from("messages")
            .join("data")
            .join(format!("{}.jsonl", bg_id_str));
        assert!(
            override_path.ends_with(&expected_suffix),
            "transcript override must point at the child's sidechain file; got {:?}",
            override_path
        );
    }

    #[tokio::test]
    async fn completed_run_maps_to_completed_report() {
        // RunComplete with non-empty output_text → TaskFinalReport::completed(Some(text)).
        let cli = make_cli_capturing_runner(Arc::new(Mutex::new(None)), Ok("result text".to_string()));
        let api = make_api_capturing_runner(Arc::new(Mutex::new(None)), Ok("result text".to_string()));
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(cli, api));

        let runner = super::ProfileAwareChildRunner::new(None, Arc::new(DefaultProviderFactory));
        runner.set_dispatcher(dispatcher);

        let report = launch_and_join(&runner, make_child_ctx(), make_test_profile(AgentRunnerMode::Cli)).await;

        assert_eq!(report.status, TaskFinalStatus::Completed, "run Ok must produce Completed");
        assert_eq!(
            report.final_assistant_text.as_deref(),
            Some("result text"),
            "output text must be carried into the report"
        );
    }

    #[tokio::test]
    async fn error_maps_to_failed_report() {
        // Err(e) from the runner → TaskFinalReport::failed(e.to_string()).
        let cli = make_cli_capturing_runner(Arc::new(Mutex::new(None)), Err("runner blew up".to_string()));
        let api = make_api_capturing_runner(Arc::new(Mutex::new(None)), Err("runner blew up".to_string()));
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(cli, api));

        let runner = super::ProfileAwareChildRunner::new(None, Arc::new(DefaultProviderFactory));
        runner.set_dispatcher(dispatcher);

        let report = launch_and_join(&runner, make_child_ctx(), make_test_profile(AgentRunnerMode::Cli)).await;

        assert_eq!(report.status, TaskFinalStatus::Failed, "Err from runner must produce Failed");
        assert!(
            report.error_message.as_deref().unwrap_or("").contains("runner blew up"),
            "error message must carry the runner error; got: {:?}", report.error_message
        );
    }

    #[tokio::test]
    async fn cancelled_token_maps_to_cancelled_report() {
        // When the cancel token fires before run() returns, the report is Cancelled.
        let captured = Arc::new(Mutex::new(None::<AgentRunRequest>));
        let cli = make_cli_capturing_runner(Arc::clone(&captured), Ok("ignored".to_string()));
        let api = make_api_capturing_runner(Arc::new(Mutex::new(None)), Ok("ignored".to_string()));
        let dispatcher = Arc::new(RunnerDispatcher::with_runners(cli, api));

        let runner = super::ProfileAwareChildRunner::new(None, Arc::new(DefaultProviderFactory));
        runner.set_dispatcher(dispatcher);

        // Pre-cancel the context token so the runner observes it was already fired.
        let mut ctx = make_child_ctx();
        ctx.cancel = CancellationToken::new();
        ctx.cancel.cancel();

        let report = launch_and_join(&runner, ctx, make_test_profile(AgentRunnerMode::Cli)).await;

        assert_eq!(
            report.status, TaskFinalStatus::Cancelled,
            "pre-cancelled token must produce Cancelled report"
        );
    }
}
