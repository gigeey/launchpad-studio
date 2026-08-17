//! Full-mode (inspection) verification engine for CLI-backed coordinators.
//!
//! When the coordinator agent runs through a local CLI binary rather than a
//! metered API, `InspectionVerifier` cannot be used — it spawns an isolated
//! native query-loop child session that requires a tool-calling API provider.
//! A CLI binary performs its own internal tool use; it does not round-trip
//! tool calls through our `ProviderClient` seam.
//!
//! This module provides `CliInspectionVerifier`, which instead invokes the
//! CLI binary once (non-interactively) with the inspection prompt and the
//! project's working directory, collects the binary's final text, and parses
//! a structured verdict from it. The binary is responsible for reading the
//! relevant files and running tests — we only need to supply the prompt and CWD.
//!
//! Shared with `InspectionVerifier`:
//! - `INSPECTOR_SYSTEM_PROMPT` — the impartial-inspector instruction
//! - `build_inspection_prompt` — goal + spec + tasklist evidence + prior verdicts
//! - `parse_verdict` — JSON verdict extractor (in `verification/mod.rs`)
//! - `inconclusive_fail` — standardized fallback verdict
//! - `VERDICT_RETRY_NUDGE` — one-shot retry prompt fragment
//! - `INSPECTION_TIMEOUT_SECS` — wall-clock budget for the run

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use ao_engine_tools_core::{VerificationEngine, VerificationInput, VerificationVerdict};
use ao_engine_tools_runner::message::{ContentBlock, Message};
use ao_engine_tools_runner::provider::{
    CompletionEvent, CompletionRequest, ProviderClient,
};
use ao_engine_tools_runner::verification::inspection::{
    build_inspection_prompt, inconclusive_fail, INSPECTOR_SYSTEM_PROMPT,
    INSPECTION_TIMEOUT_SECS, VERDICT_RETRY_NUDGE,
};
use ao_engine_tools_runner::verification::parse_verdict;
use ao_normalizer::registry::NormalizerRegistry;
use ao_process::default_supervisor::DefaultProcessSupervisor;
use ao_process::supervisor::ProcessSupervisor;
use ao_protocol::agent::{AgentProfile, CliProviderConfig, ProviderConfig};

use crate::verification_cli_provider::CliProviderClient;

/// Full-mode verification engine for CLI-backed coordinators.
///
/// Invokes the coordinator's CLI binary once with the inspection prompt (goal +
/// spec + tasklist evidence + prior verdicts) and the project working directory
/// as the CWD. The binary performs its own file reading and testing; we collect
/// its text output and parse the structured JSON verdict from it.
///
/// For the Claude CLI, `--dangerously-skip-permissions` is automatically appended
/// to the configured base args (if not already present) so the binary can read
/// files and run commands without interactive approval prompts.
pub struct CliInspectionVerifier {
    profile: AgentProfile,
    supervisor: Arc<dyn ProcessSupervisor>,
    normalizers: Arc<NormalizerRegistry>,
}

impl CliInspectionVerifier {
    /// Build a verifier from a coordinator profile, using the real process
    /// supervisor and the default normalizer registry.
    pub fn new(profile: AgentProfile) -> Self {
        Self {
            profile,
            supervisor: Arc::new(DefaultProcessSupervisor::new()),
            normalizers: Arc::new(NormalizerRegistry::new()),
        }
    }

    /// Build a verifier with explicit supervisor and normalizer registry.
    /// Allows tests to script process output without a real CLI binary.
    pub fn with_components(
        profile: AgentProfile,
        supervisor: Arc<dyn ProcessSupervisor>,
        normalizers: Arc<NormalizerRegistry>,
    ) -> Self {
        Self { profile, supervisor, normalizers }
    }
}

/// Apply the argv overrides an inspection run needs on top of the coordinator's
/// configured CLI provider.
///
/// For the Claude CLI, inject `--dangerously-skip-permissions` so the binary can
/// read files and run commands without stopping for interactive approval — an
/// inspection run is unattended, so a permission prompt would hang it until the
/// supervisor's timeout. Matching is on the command's file name, so an absolute
/// path such as `/usr/local/bin/claude` is recognized while `/bin/echo` is not.
/// Idempotent, and a no-op for every other command.
fn apply_inspection_arg_overrides(cli: &mut CliProviderConfig) {
    let is_claude = std::path::Path::new(&cli.command)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|b| b == "claude")
        .unwrap_or(false);
    if is_claude && !cli.args.iter().any(|a| a == "--dangerously-skip-permissions") {
        cli.args.push("--dangerously-skip-permissions".to_string());
    }
}

#[async_trait]
impl VerificationEngine for CliInspectionVerifier {
    async fn verify(&self, input: VerificationInput) -> Result<VerificationVerdict, String> {
        let cwd = match input.working_dir.clone() {
            Some(p) => p,
            None => {
                return Ok(VerificationVerdict {
                    verdict: "fail".to_string(),
                    confidence: "low".to_string(),
                    gaps: vec![
                        "working_dir is not set on this project; file-level inspection \
                         is unavailable. Set the project working_dir via ProjectUpdate \
                         and re-run mode='full'."
                            .to_string(),
                    ],
                    rationale: "The inspection engine requires a working directory to open \
                                source files, run git commands, and execute tests. This project \
                                has no working_dir configured, so no artifact evidence could be \
                                gathered. The verdict is inconclusive (fail/low confidence)."
                        .to_string(),
                });
            }
        };

        // Clone the coordinator profile and override fields for the inspection run:
        // - working_dir → the project directory to inspect (PathBuf → String)
        // - system_prompt → cleared here; set per-request in CompletionRequest
        // - argv overrides → see `apply_inspection_arg_overrides`
        let mut inspect_profile = self.profile.clone();
        inspect_profile.working_dir = Some(cwd.to_string_lossy().into_owned());
        inspect_profile.system_prompt = None;

        let ProviderConfig::Cli(ref mut cli) = inspect_profile.provider;
        apply_inspection_arg_overrides(cli);

        let provider: Arc<dyn ProviderClient> = Arc::new(CliProviderClient::with_components(
            inspect_profile,
            Arc::clone(&self.supervisor),
            Arc::clone(&self.normalizers),
        ));

        let user_message = build_inspection_prompt(&input);
        let inspection_request = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: user_message }],
            }],
            system_prompt: Some(INSPECTOR_SYSTEM_PROMPT.to_string()),
            tools: vec![],
            ..Default::default()
        };

        let timeout = Duration::from_secs(INSPECTION_TIMEOUT_SECS);
        let raw_text = match tokio::time::timeout(
            timeout,
            single_shot(&provider, inspection_request),
        )
        .await
        {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                return Ok(inconclusive_fail(format!("inspection CLI error: {e}")));
            }
            Err(_elapsed) => {
                return Ok(inconclusive_fail(format!(
                    "inspection timed out after {}s",
                    INSPECTION_TIMEOUT_SECS
                )));
            }
        };

        // First parse attempt.
        match parse_verdict(&raw_text) {
            Ok(v) => return Ok(v),
            Err(_) => {}
        }

        // Retry: send the full original context + failed response + nudge in
        // one combined message. A CLI binary is stateless across invocations,
        // so the retry must include enough context for it to re-emit the verdict.
        let retry_user = format!(
            "{}\n\n## Previous attempt (could not parse as JSON)\n\n{}\n\n{}",
            build_inspection_prompt(&input),
            raw_text,
            VERDICT_RETRY_NUDGE
        );
        let retry_request = CompletionRequest {
            messages: vec![Message::User {
                content: vec![ContentBlock::Text { text: retry_user }],
            }],
            system_prompt: Some(INSPECTOR_SYSTEM_PROMPT.to_string()),
            tools: vec![],
            ..Default::default()
        };

        let retry_result = tokio::time::timeout(
            timeout,
            single_shot(&provider, retry_request),
        )
        .await;

        let retry_text = match retry_result {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Ok(inconclusive_fail(format!("inspection retry error: {e}"))),
            Err(_) => {
                return Ok(inconclusive_fail(format!(
                    "inspection retry timed out after {}s",
                    INSPECTION_TIMEOUT_SECS
                )));
            }
        };

        match parse_verdict(&retry_text) {
            Ok(v) => Ok(v),
            Err(e) => Ok(inconclusive_fail(format!("verdict parse failed after retry: {e}"))),
        }
    }
}

/// Drive one provider completion to a text string. Returns `Err` when the
/// provider fails or the binary produces no output.
async fn single_shot(
    provider: &Arc<dyn ProviderClient>,
    request: CompletionRequest,
) -> Result<String, String> {
    let cancel = CancellationToken::new();
    let mut stream = provider
        .complete(request, cancel)
        .await
        .map_err(|e| format!("provider error: {e}"))?;

    let mut text = String::new();
    loop {
        match stream.recv().await {
            None => break,
            Some(Ok(CompletionEvent::AssistantText(chunk))) => text.push_str(&chunk),
            Some(Ok(CompletionEvent::TurnComplete { .. })) => break,
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(format!("stream error: {e}")),
        }
    }

    if text.trim().is_empty() {
        return Err("inspection CLI produced no output".to_string());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use ao_process::mock::{MockProcessSupervisor, MockScenario};
    use ao_protocol::agent::{
        AgentRunnerMode, CliProviderConfig, InputMode, OutputFormat,
    };

    fn cli_profile(command: &str) -> AgentProfile {
        AgentProfile {
            id: "inspect-agent".to_string(),
            name: "Inspect Agent".to_string(),
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

    fn test_input(working_dir: Option<PathBuf>) -> VerificationInput {
        VerificationInput {
            project_id: "proj-1".to_string(),
            goal: "Build a widget".to_string(),
            spec: None,
            tasklist_evidence: vec![],
            prior_verdicts: vec![],
            extra_evidence: None,
            working_dir,
        }
    }

    /// Missing `working_dir` returns a graceful fail/low verdict without
    /// attempting to spawn any process.
    #[tokio::test]
    async fn missing_working_dir_returns_fail_low() {
        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![]));
        let verifier = CliInspectionVerifier::with_components(
            cli_profile("echo"),
            supervisor,
            Arc::new(NormalizerRegistry::new()),
        );

        let verdict = verifier.verify(test_input(None)).await.unwrap();

        assert_eq!(verdict.verdict, "fail");
        assert_eq!(verdict.confidence, "low");
        assert!(verdict.gaps.iter().any(|g| g.contains("working_dir")));
    }

    /// A binary that outputs a valid JSON verdict returns that verdict.
    #[tokio::test]
    async fn valid_verdict_returned_from_cli_output() {
        let verdict_json = r#"{"verdict":"pass","confidence":"high","gaps":[],"rationale":"All checks passed."}"#;
        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![MockScenario {
                stdout_lines: vec![verdict_json.to_string()],
                stderr_lines: vec![],
                exit_code: 0,
                delay_per_line_ms: 0,
            }]));
        let verifier = CliInspectionVerifier::with_components(
            cli_profile("stub"),
            supervisor,
            Arc::new(NormalizerRegistry::new()),
        );

        let verdict = verifier
            .verify(test_input(Some(PathBuf::from("/tmp"))))
            .await
            .unwrap();

        assert_eq!(verdict.verdict, "pass");
        assert_eq!(verdict.confidence, "high");
        assert!(verdict.gaps.is_empty());
    }

    /// Garbage output on first attempt → retry → retry returns valid JSON.
    #[tokio::test]
    async fn garbage_first_attempt_triggers_retry() {
        let valid_json = r#"{"verdict":"fail","confidence":"low","gaps":["tests missing"],"rationale":"No test suite."}"#;
        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![
                // First call: garbage output
                MockScenario {
                    stdout_lines: vec!["I inspected things but the verdict is unclear.".to_string()],
                    stderr_lines: vec![],
                    exit_code: 0,
                    delay_per_line_ms: 0,
                },
                // Second call (retry): valid JSON
                MockScenario {
                    stdout_lines: vec![valid_json.to_string()],
                    stderr_lines: vec![],
                    exit_code: 0,
                    delay_per_line_ms: 0,
                },
            ]));
        let verifier = CliInspectionVerifier::with_components(
            cli_profile("stub"),
            supervisor,
            Arc::new(NormalizerRegistry::new()),
        );

        let verdict = verifier
            .verify(test_input(Some(PathBuf::from("/tmp"))))
            .await
            .unwrap();

        assert_eq!(verdict.verdict, "fail");
        assert_eq!(verdict.gaps, vec!["tests missing"]);
    }

    /// When both attempts fail to parse, the verifier returns an inconclusive
    /// fail rather than propagating an error.
    #[tokio::test]
    async fn both_attempts_fail_returns_inconclusive() {
        let supervisor: Arc<dyn ProcessSupervisor> =
            Arc::new(MockProcessSupervisor::new(vec![
                MockScenario {
                    stdout_lines: vec!["not json at all".to_string()],
                    stderr_lines: vec![],
                    exit_code: 0,
                    delay_per_line_ms: 0,
                },
                MockScenario {
                    stdout_lines: vec!["still not json".to_string()],
                    stderr_lines: vec![],
                    exit_code: 0,
                    delay_per_line_ms: 0,
                },
            ]));
        let verifier = CliInspectionVerifier::with_components(
            cli_profile("stub"),
            supervisor,
            Arc::new(NormalizerRegistry::new()),
        );

        let verdict = verifier
            .verify(test_input(Some(PathBuf::from("/tmp"))))
            .await
            .unwrap();

        assert_eq!(verdict.verdict, "fail");
        assert_eq!(verdict.confidence, "low");
        assert!(verdict.gaps.iter().any(|g| g.contains("inconclusive")));
    }

    /// `parse_verdict` reuse: the public function parses a fenced JSON block
    /// correctly. This verifies the function is reachable from `ao-engine`
    /// after being made pub in `ao-engine-tools-runner`.
    #[test]
    fn parse_verdict_reachable_and_correct() {
        let text = concat!(
            "```json\n",
            r#"{"verdict":"pass","confidence":"medium","gaps":[],"rationale":"ok"}"#,
            "\n```"
        );
        let v = parse_verdict(text).unwrap();
        assert_eq!(v.verdict, "pass");
        assert_eq!(v.confidence, "medium");
    }

    /// For a `claude` command, `--dangerously-skip-permissions` is injected
    /// into the inspection profile's args automatically.
    #[test]
    fn claude_gets_skip_permissions_injected() {
        let profile = cli_profile("claude");

        let mut inspect_profile = profile.clone();
        let ProviderConfig::Cli(ref mut cli) = inspect_profile.provider;
        apply_inspection_arg_overrides(cli);

        let ProviderConfig::Cli(ref cli) = inspect_profile.provider;
        assert!(
            cli.args.contains(&"--dangerously-skip-permissions".to_string()),
            "claude CLI must get --dangerously-skip-permissions for inspection"
        );
    }

    /// For a non-claude command, no skip-permissions flag is added.
    #[test]
    fn non_claude_does_not_get_skip_permissions() {
        let profile = cli_profile("gemini");
        let mut inspect_profile = profile.clone();
        let ProviderConfig::Cli(ref mut cli) = inspect_profile.provider;
        apply_inspection_arg_overrides(cli);

        let ProviderConfig::Cli(ref cli) = inspect_profile.provider;
        assert!(
            !cli.args.contains(&"--dangerously-skip-permissions".to_string()),
            "non-claude CLI must NOT get --dangerously-skip-permissions"
        );
    }
}
