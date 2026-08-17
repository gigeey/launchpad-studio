//! A [`ProviderClient`] that satisfies a single completion by invoking a
//! command-line agent binary once, non-interactively, and returning the text
//! the binary prints.
//!
//! # Why this exists
//!
//! The verification engines (`ProjectVerify`'s quick judge and full inspector)
//! talk to a model exclusively through the [`ProviderClient`] seam. Every
//! existing implementor of that seam is an HTTP API client (Anthropic, OpenAI,
//! Gemini). When the coordinating agent is configured to run through a local
//! CLI binary instead of a metered API, verification should follow the same
//! route — otherwise a CLI-configured agent silently bills an API on every
//! gap-check.
//!
//! This client bridges that gap: it takes the coordinator's CLI configuration
//! verbatim, builds a one-shot print invocation that carries the verifier's
//! system prompt and the single user message, spawns the binary through the
//! shared process supervisor, normalizes its stdout into assistant text, and
//! emits exactly one [`CompletionEvent::AssistantText`] followed by a
//! [`CompletionEvent::TurnComplete`]. There is no tool-calling round-trip — the
//! quick judge sends an empty tool catalogue, so a single turn is all that is
//! ever required.
//!
//! Only the system prompt and the user message differ from a normal coordinator
//! run; the binary, flags, model, environment, and working directory are reused
//! unchanged so the verifier and the coordinator share one configuration.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use ao_normalizer::registry::NormalizerRegistry;
use ao_process::default_supervisor::DefaultProcessSupervisor;
use ao_process::supervisor::{ManagedRun, ProcessSupervisor, SpawnInput, TerminationReason};
use ao_protocol::agent::{AgentProfile, InputMode, ProviderConfig};
use ao_protocol::event::AgentEventPayload;

use ao_engine_tools_runner::message::{ContentBlock, Message, MessageNormalizer, NormalizerError};
use ao_engine_tools_runner::provider::{
    CompletionEvent, CompletionRequest, CompletionStream, ProviderClient, ProviderError, StopReason,
};

use crate::agent_runner::CliAgentRunner;

/// Buffer size for the channel backing a single completion stream. Two events
/// (one assistant-text block, one turn boundary) are all a successful call
/// produces, so a small buffer drains in one pass without backpressure.
const STREAM_BUFFER: usize = 8;

/// Identity message normalizer.
///
/// The [`ProviderClient`] trait requires every implementor to expose a
/// [`MessageNormalizer`], but this client never converts canonical messages
/// through it — it extracts the user text directly and hands the binary its own
/// flag-mapped argv. This passthrough exists only to satisfy the trait; it
/// serialises and deserialises canonical messages without reshaping them.
struct PassthroughMessageNormalizer;

impl MessageNormalizer for PassthroughMessageNormalizer {
    fn to_provider(&self, messages: &[Message]) -> Result<Value, NormalizerError> {
        serde_json::to_value(messages).map_err(|e| NormalizerError::Unrepresentable(e.to_string()))
    }

    fn from_provider(&self, value: Value) -> Result<Vec<Message>, NormalizerError> {
        serde_json::from_value(value).map_err(|e| NormalizerError::Shape(e.to_string()))
    }
}

/// Completion client that drives a single CLI invocation per request.
pub struct CliProviderClient {
    /// The coordinator profile whose CLI configuration is reused for the
    /// verification call. Cloned into each spawned completion task.
    profile: AgentProfile,
    /// Process supervisor used to spawn the binary. Defaults to the real
    /// supervisor; tests inject a scripted one.
    supervisor: Arc<dyn ProcessSupervisor>,
    /// Registry that maps a CLI command to the normalizer that decodes its
    /// stdout into assistant text.
    normalizers: Arc<NormalizerRegistry>,
    /// Trait-required normalizer; unused on the active completion path.
    message_normalizer: PassthroughMessageNormalizer,
}

impl CliProviderClient {
    /// Build a client from a coordinator profile, wiring the real process
    /// supervisor and the default normalizer registry.
    pub fn new(profile: AgentProfile) -> Self {
        Self {
            profile,
            supervisor: Arc::new(DefaultProcessSupervisor::new()),
            normalizers: Arc::new(NormalizerRegistry::new()),
            message_normalizer: PassthroughMessageNormalizer,
        }
    }

    /// Build a client with an explicit supervisor and normalizer registry.
    pub fn with_components(
        profile: AgentProfile,
        supervisor: Arc<dyn ProcessSupervisor>,
        normalizers: Arc<NormalizerRegistry>,
    ) -> Self {
        Self {
            profile,
            supervisor,
            normalizers,
            message_normalizer: PassthroughMessageNormalizer,
        }
    }
}

/// Concatenate the text content of every user message in the request. The
/// verification engines send a single user message built from `Text` blocks;
/// non-text blocks carry no judge-relevant content and are skipped.
fn collect_user_text(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        if let Message::User { content } = message {
            for block in content {
                if let ContentBlock::Text { text } = block {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

#[async_trait]
impl ProviderClient for CliProviderClient {
    async fn complete(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        let profile = self.profile.clone();
        let supervisor = Arc::clone(&self.supervisor);
        let normalizers = Arc::clone(&self.normalizers);

        tokio::spawn(async move {
            run_cli_completion(profile, supervisor, normalizers, request, cancel, tx).await;
        });

        Ok(CompletionStream::new(rx))
    }

    fn message_normalizer(&self) -> &dyn MessageNormalizer {
        &self.message_normalizer
    }
}

/// Drive one CLI invocation to completion and push the resulting events onto
/// the stream channel. Any hard failure is surfaced as an `Err` item so the
/// engine's drain loop reports it rather than silently parsing empty output.
async fn run_cli_completion(
    profile: AgentProfile,
    supervisor: Arc<dyn ProcessSupervisor>,
    normalizers: Arc<NormalizerRegistry>,
    request: CompletionRequest,
    cancel: CancellationToken,
    tx: mpsc::Sender<Result<CompletionEvent, ProviderError>>,
) {
    let user_text = collect_user_text(&request.messages);

    // Reuse the coordinator's flag mapping verbatim, but swap in the verifier's
    // system prompt. Cloning the profile keeps the binary, args, model, and
    // thinking flags identical to a normal coordinator run — only the system
    // prompt and the user message change.
    let mut synthetic = profile;
    synthetic.system_prompt = request.system_prompt.clone();

    let ProviderConfig::Cli(ref cli) = synthetic.provider;

    // No MCP config path: this is a one-shot, non-interactive call with no tool
    // surface, matching the empty tool catalogue the quick judge sends.
    let argv = CliAgentRunner::build_argv(&synthetic, &user_text, None, None);

    let stdin_data = if cli.input_mode == InputMode::Stdin {
        Some(user_text.clone())
    } else {
        None
    };

    let command = cli.command.clone();
    let no_output_timeout_ms = cli.no_output_timeout_ms;
    let env = if synthetic.env.is_empty() {
        None
    } else {
        Some(synthetic.env.clone())
    };
    let cwd = synthetic.working_dir.clone();
    let timeout_ms = synthetic.timeout_seconds.saturating_mul(1000);

    let spawn_input = SpawnInput {
        run_id: None,
        backend_id: format!("verify-{}", synthetic.id),
        scope_key: None,
        argv,
        cwd,
        env,
        stdin_data,
        timeout_ms: Some(timeout_ms),
        no_output_timeout_ms: Some(no_output_timeout_ms),
        tools_in_flight: None,
        form_suspended: None,
    };

    let managed = match supervisor.spawn(spawn_input).await {
        Ok(run) => run,
        Err(e) => {
            let _ = tx
                .send(Err(ProviderError::Transport(format!(
                    "verification CLI spawn failed: {e}"
                ))))
                .await;
            return;
        }
    };

    let ManagedRun {
        mut stdout_rx,
        mut stderr_rx,
        wait_handle,
        cancel_tx,
        ..
    } = managed;

    // Translate a cancelled token into a child kill. The supervisor closes
    // stdout once the process dies, which unblocks the drain loop below.
    let cancel_watch = cancel.clone();
    tokio::spawn(async move {
        cancel_watch.cancelled().await;
        let _ = cancel_tx.send(TerminationReason::Cancelled);
    });

    // Drain stderr off-thread so a chatty error channel can't deadlock stdout.
    let stderr_handle = tokio::spawn(async move {
        let mut collected = String::new();
        while let Some(line) = stderr_rx.recv().await {
            collected.push_str(&line);
            collected.push('\n');
        }
        collected
    });

    let mut normalizer = normalizers.create(&command, cli);

    // Streamed deltas and finalized blocks are accumulated separately: the
    // finalized `TextComplete` block (when the normalizer emits one) is the
    // authoritative full text, so it is preferred over the concatenated deltas
    // to avoid double-counting.
    let mut delta_text = String::new();
    let mut complete_blocks: Vec<String> = Vec::new();

    while let Some(chunk) = stdout_rx.recv().await {
        for payload in normalizer.process_chunk(&chunk) {
            absorb_text(payload, &mut delta_text, &mut complete_blocks);
        }
    }

    let run_exit = wait_handle.await.ok();
    let exit_code = run_exit.as_ref().and_then(|exit| exit.exit_code);
    let stderr_text = stderr_handle.await.unwrap_or_default();

    for payload in normalizer.finalize(exit_code, &stderr_text) {
        absorb_text(payload, &mut delta_text, &mut complete_blocks);
    }

    if cancel.is_cancelled() {
        let _ = tx.send(Err(ProviderError::Cancelled)).await;
        return;
    }

    let full_text = if complete_blocks.is_empty() {
        delta_text
    } else {
        complete_blocks.join("")
    };

    if full_text.trim().is_empty() {
        let _ = tx
            .send(Err(ProviderError::Transport(format!(
                "verification CLI produced no output (exit_code={:?}, stderr={:?})",
                exit_code,
                stderr_text.trim()
            ))))
            .await;
        return;
    }

    let _ = tx
        .send(Ok(CompletionEvent::AssistantText(full_text)))
        .await;
    let _ = tx
        .send(Ok(CompletionEvent::TurnComplete {
            stop_reason: StopReason::Natural,
        }))
        .await;
}

/// Route a normalizer payload into the delta buffer or the finalized-block
/// list. Every other payload kind (errors, lifecycle markers) is irrelevant to
/// a single-turn text verdict and is dropped.
fn absorb_text(
    payload: AgentEventPayload,
    delta_text: &mut String,
    complete_blocks: &mut Vec<String>,
) {
    match payload {
        AgentEventPayload::TextDelta { text } => delta_text.push_str(&text),
        AgentEventPayload::TextComplete { text } => complete_blocks.push(text),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use ao_process::mock::{MockProcessSupervisor, MockScenario};
    use ao_protocol::agent::{
        AgentRunnerMode, CliProviderConfig, OutputFormat, ProviderConfig,
    };

    fn cli_profile(command: &str) -> AgentProfile {
        AgentProfile {
            id: "verify-agent".to_string(),
            name: "Verify Agent".to_string(),
            description: String::new(),
            emoji: None,
            provider: ProviderConfig::Cli(CliProviderConfig {
                command: command.to_string(),
                args: vec![],
                normalizer: None,
                output_format: OutputFormat::Text,
                input_mode: InputMode::Arg,
                model_arg: None,
                model_aliases: HashMap::new(),
                system_prompt_arg: None,
                session_arg: None,
                resume_args: vec![],
                session_id_fields: vec![],
                clear_env: false,
                no_output_timeout_ms: 30_000,
                file_capabilities: None,
            }),
            model: None,
            skills: vec![],
            system_prompt: None,
            tools: None,
            env: HashMap::new(),
            max_instances: 1,
            timeout_seconds: 60,
            working_dir: None,
            home_dir: None,
            serialize: true,
            workflows: None,
            template: None,
            runner_mode: AgentRunnerMode::Cli,
            enabled_plugins: HashMap::new(),
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

    fn judge_request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text {
                    text: "verify this".to_string(),
                }],
            }],
            system_prompt: Some("You are a verifier.".to_string()),
            tools: vec![],
            ..Default::default()
        }
    }

    /// A scripted process whose stdout is a JSON verdict is decoded into a
    /// single `AssistantText` event carrying that verdict, followed by a
    /// `TurnComplete`. This exercises the full spawn → normalize → emit path
    /// against a stub binary without touching a real CLI.
    #[tokio::test]
    async fn emits_assistant_text_then_turn_complete_from_stdout() {
        let verdict = r#"{"verdict":"pass","confidence":"high","gaps":[],"rationale":"ok"}"#;
        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![MockScenario {
                stdout_lines: vec![verdict.to_string()],
                stderr_lines: vec![],
                exit_code: 0,
                delay_per_line_ms: 0,
            }]));
        let normalizers = Arc::new(NormalizerRegistry::new());
        let client = CliProviderClient::with_components(
            cli_profile("stub-binary"),
            supervisor,
            normalizers,
        );

        let mut stream = client
            .complete(judge_request(), CancellationToken::new())
            .await
            .expect("stream should construct");

        let mut text = String::new();
        let mut saw_turn_complete = false;
        while let Some(item) = stream.recv().await {
            match item.expect("no stream error") {
                CompletionEvent::AssistantText(chunk) => text.push_str(&chunk),
                CompletionEvent::TurnComplete { .. } => {
                    saw_turn_complete = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(text.contains("\"verdict\":\"pass\""), "got text: {text}");
        assert!(saw_turn_complete, "stream must end with TurnComplete");
    }

    /// A process that prints nothing yields a transport error rather than an
    /// empty assistant turn, so the engine reports a real failure instead of
    /// trying to parse an empty verdict.
    #[tokio::test]
    async fn empty_output_surfaces_provider_error() {
        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![MockScenario {
                stdout_lines: vec![],
                stderr_lines: vec!["boom".to_string()],
                exit_code: 1,
                delay_per_line_ms: 0,
            }]));
        let normalizers = Arc::new(NormalizerRegistry::new());
        let client = CliProviderClient::with_components(
            cli_profile("stub-binary"),
            supervisor,
            normalizers,
        );

        let mut stream = client
            .complete(judge_request(), CancellationToken::new())
            .await
            .expect("stream should construct");

        let first = stream.recv().await.expect("one item expected");
        assert!(
            matches!(first, Err(ProviderError::Transport(_))),
            "empty output should surface a transport error, got {first:?}"
        );
    }
}
