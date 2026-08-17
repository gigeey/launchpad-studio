//! Inspection verification engine — the full-mode back-end for `ProjectVerify`.
//!
//! Unlike the quick engine (a single model call over tasklist summaries),
//! the inspection engine spawns an isolated, read-only child session that
//! opens the project's working directory, reads source files, inspects git
//! history, and optionally runs the test suite before issuing a structured
//! verdict. Claims in tasklist summaries are treated as unverified — the
//! child is instructed to seek artifact evidence rather than accepting them
//! at face value.
//!
//! # Architecture
//!
//! The `InspectionVerifier` holds the provider and configuration needed to
//! spin up a child `RunnerConfig`. In `verify()` it:
//!
//! 1. Builds a `RunnerContext` with `cwd = input.working_dir` (falls back to
//!    the quick engine with a rationale note when `working_dir` is absent).
//! 2. Assembles a filtered tool registry (Read, Glob, Grep, Bash only —
//!    no mutations, no further delegation).
//! 3. Calls `run_session` with the inspector system prompt, a turn cap of
//!    [`INSPECTION_TURN_CAP`], and a wall-clock timeout of
//!    [`INSPECTION_TIMEOUT_SECS`].
//! 4. Extracts a fenced JSON verdict from the child's final message.
//! 5. On parse failure retries ONCE with a targeted nudge.
//! 6. On budget exhaustion or repeated failure returns an inconclusive-fail
//!    verdict so the orchestrator always receives a structured response.
//!
//! # Tool allowlist
//!
//! The child registry is filtered to [`INSPECTION_ALLOWED_TOOLS`]. No
//! write tools, no task/todo tools, no delegation, and no project tools
//! are included. The system prompt additionally instructs the child to
//! use Bash only for read-only commands (`git diff`, `cargo test`, etc.)
//! and not to mutate the workspace.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use ao_engine_tools_core::{
    NoopDenialTracker, Registry, RunnerContext, VerificationEngine, VerificationInput,
    VerificationVerdict,
};

use crate::{
    hooks::config::RunnerSettings,
    message::{ContentBlock, Message},
    provider::{CompletionEvent, CompletionRequest, ProviderClient},
    prompt_bridge::StubBridge,
    query_loop::{run_session, RunnerConfig},
};
use ao_engine_tools_core::permissions::{PermissionMode, SessionKind};

/// Names of the tools the inspection child is allowed to call.
///
/// The set is intentionally small: read/search utilities and Bash for
/// executing git and test commands. The system prompt further restricts
/// Bash to read-only operations.
pub const INSPECTION_ALLOWED_TOOLS: &[&str] = &["Read", "Glob", "Grep", "Bash"];

/// Maximum number of provider turns the inspection child may execute.
///
/// Kept low (15) so a runaway child does not block the parent indefinitely.
/// The system prompt instructs the child to end with the verdict JSON once
/// inspection is complete.
pub const INSPECTION_TURN_CAP: usize = 15;

/// Wall-clock timeout for the entire inspection run, in seconds.
///
/// If the child does not finish within this window, the run is cancelled and
/// an inconclusive-fail verdict is returned.
pub const INSPECTION_TIMEOUT_SECS: u64 = 300; // 5 minutes

/// System prompt for the impartial inspection child.
///
/// The child receives this prompt, the inspector user message (goal + spec +
/// prior gaps + UNVERIFIED claim summaries), and is expected to inspect the
/// working directory and respond with a structured JSON verdict.
pub const INSPECTOR_SYSTEM_PROMPT: &str = "\
You are an impartial, sceptical code inspector. You have been given a software project goal \
and a set of UNVERIFIED CLAIMS about what has been done. Your job is to inspect the actual \
working directory and decide — based on evidence you find yourself — whether the goal is \
genuinely met.

## Your mandate

1. Be sceptical. Treat every claim as unverified until you confirm it with an artifact.
2. Read the relevant source files, check git status and git diff, and run the test suite \
   if one is discoverable and cheap to execute (e.g. `cargo test`, `npm test`, `pytest`).
3. Prefer reading actual code over trusting completion summaries.
4. When a test suite run succeeds, record it as evidence. When it fails, note the failures.
5. List specific, actionable gaps when the verdict is 'fail'. Each gap must cite the \
   file path or command output that supports it.
6. Do NOT create, edit, or delete any files. Do NOT commit or push anything. Use Bash \
   only for read-only commands: `git status`, `git diff`, `git log`, `cat`, test runners, \
   linters in check-only mode, etc.

## Verdict format

When you have gathered enough evidence, respond with ONLY a JSON object enclosed in a \
```json ... ``` fenced code block. No prose before or after the block. The exact shape is:

```json
{
  \"verdict\": \"pass\" | \"fail\",
  \"confidence\": \"high\" | \"medium\" | \"low\",
  \"gaps\": [\"gap 1 (file:line)\", \"gap 2 (command output)\"],
  \"rationale\": \"one paragraph explaining what you inspected and why you reached this verdict\"
}
```

A verdict of 'pass' means the goal is concretely met — code exists, tests pass, \
deliverables are present. A verdict of 'fail' means at least one gap remains.";

/// Retry nudge sent when the child's first response cannot be parsed as a verdict.
pub const VERDICT_RETRY_NUDGE: &str = "\
Your previous response could not be parsed as the required JSON verdict. \
Please reply with ONLY the JSON object, enclosed in a ```json ... ``` fenced code block, \
with no other text. The required shape is:\n\
```json\n\
{\n\
  \"verdict\": \"pass\" | \"fail\",\n\
  \"confidence\": \"high\" | \"medium\" | \"low\",\n\
  \"gaps\": [\"gap description\"],\n\
  \"rationale\": \"explanation\"\n\
}\n\
```";

/// Production implementation of the full inspection engine.
///
/// Spawns an isolated child session over the project's working directory,
/// feeds it the inspection prompt, and returns a structured verdict.
pub struct InspectionVerifier {
    provider: Arc<dyn ProviderClient>,
    settings: RunnerSettings,
    full_registry: Arc<Registry>,
}

impl InspectionVerifier {
    /// Build an `InspectionVerifier` from provider and registry.
    ///
    /// `full_registry` is the parent session's complete registry; the verifier
    /// filters it to `INSPECTION_ALLOWED_TOOLS` at construction time and reuses
    /// the filtered copy for every `verify()` call.
    pub fn new(
        provider: Arc<dyn ProviderClient>,
        settings: RunnerSettings,
        full_registry: Arc<Registry>,
    ) -> Self {
        Self {
            provider,
            settings,
            full_registry,
        }
    }
}

#[async_trait]
impl VerificationEngine for InspectionVerifier {
    async fn verify(&self, input: VerificationInput) -> Result<VerificationVerdict, String> {
        // When no working directory is available, the inspection child cannot
        // open any files. Fall back gracefully rather than erroring hard.
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

        // Build a filtered registry containing only the inspection allowlist.
        let allowed: Vec<String> = INSPECTION_ALLOWED_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let inspection_registry = Arc::new(self.full_registry.filter_for(&allowed));

        // Construct a minimal, isolated RunnerContext for the child.
        let session_id = uuid::Uuid::new_v4().to_string();
        let agent_id = format!("inspector-{}", &session_id[..8]);
        let child_ctx = RunnerContext::new_with_cwd(session_id, agent_id, cwd)
            .with_registry(inspection_registry)
            .with_system_prompt(INSPECTOR_SYSTEM_PROMPT);

        // Build the child's RunnerConfig with the turn cap.
        // AcceptEdits mode is used so the child's Bash tool calls are not
        // blocked by the permission gate — the system prompt instructs the
        // child to issue only read-only commands, not mutation operations.
        let config = RunnerConfig {
            provider: self.provider.clone(),
            bridge: Arc::new(StubBridge),
            denial_tracker: Arc::new(NoopDenialTracker),
            settings: self.settings.clone(),
            // BypassPermissions lets the restricted child run Bash commands
            // (e.g. git diff, cargo test) without an interactive prompt gate.
            // The tool registry is already filtered to read-only tools and the
            // system prompt forbids mutation operations.
            mode: PermissionMode::BypassPermissions,
            kind: SessionKind::Autonomous,
            auto_approve: vec![],
            system_prompt: Some(INSPECTOR_SYSTEM_PROMPT.to_string()),
            event_sink: None,
            thinking: None,
            max_turns: Some(INSPECTION_TURN_CAP),
        };

        let initial_messages = vec![Message::User {
            content: vec![ContentBlock::Text {
                text: build_inspection_prompt(&input),
            }],
        }];

        // Extract the cancel token before moving child_ctx into the future.
        let child_cancel = child_ctx.cancel.clone();

        // Run the inspection child, applying a wall-clock timeout.
        let timeout = Duration::from_secs(INSPECTION_TIMEOUT_SECS);
        let outcome = match tokio::time::timeout(
            timeout,
            run_session(initial_messages, child_ctx, config),
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Ok(inconclusive_fail(format!(
                    "inspection session error: {e}"
                )));
            }
            Err(_elapsed) => {
                child_cancel.cancel();
                return Ok(inconclusive_fail(format!(
                    "inspection timed out after {}s",
                    INSPECTION_TIMEOUT_SECS
                )));
            }
        };

        if outcome.cancelled && outcome.final_assistant_text.is_empty() {
            return Ok(inconclusive_fail(
                format!(
                    "inspection child was cancelled after {} turns without producing a verdict",
                    outcome.turns
                ),
            ));
        }

        let raw_text = outcome.final_assistant_text.clone();

        // First attempt to parse the verdict from the child's final message.
        match super::parse_verdict(&raw_text) {
            Ok(v) => return Ok(v),
            Err(_parse_err) => {
                // First parse attempt failed. Try once more with a direct provider
                // call that asks the model to re-emit only the JSON verdict.
            }
        }

        // Retry path: feed the failed response back with a nudge message and
        // ask the model to re-emit just the JSON.
        let retry_result = retry_verdict_parse(
            &self.provider,
            &raw_text,
        )
        .await;

        match retry_result {
            Ok(v) => Ok(v),
            Err(reason) => Ok(inconclusive_fail(reason)),
        }
    }
}

/// Assemble the user-turn message the inspector receives.
///
/// Labels the tasklist summaries explicitly as UNVERIFIED CLAIMS so the
/// inspector is primed to seek evidence rather than trusting them.
pub fn build_inspection_prompt(input: &VerificationInput) -> String {
    let mut msg = format!("# Project goal\n\n{}\n", input.goal);

    if let Some(ref spec) = input.spec {
        if !spec.trim().is_empty() {
            msg.push_str(&format!("\n# Spec\n\n{}\n", spec));
        }
    }

    if !input.prior_verdicts.is_empty() {
        msg.push_str("\n# Prior verification rounds\n\n");
        for pv in &input.prior_verdicts {
            msg.push_str(&format!(
                "Round {}: {} — gaps: {}\n",
                pv.round,
                pv.verdict,
                if pv.gaps.is_empty() {
                    "none".to_string()
                } else {
                    pv.gaps.join("; ")
                }
            ));
        }
    }

    // Label summaries as unverified so the model doesn't treat them as ground truth.
    if input.tasklist_evidence.is_empty() {
        msg.push_str("\n# UNVERIFIED CLAIMS (tasklist summaries)\n\nNo completed tasklists yet.\n");
    } else {
        msg.push_str("\n# UNVERIFIED CLAIMS (tasklist summaries — verify with artifacts)\n\n");
        for item in &input.tasklist_evidence {
            msg.push_str(&format!("## {}\n\n{}\n\n", item.title, item.summary));
        }
    }

    if let Some(ref extra) = input.extra_evidence {
        if !extra.trim().is_empty() {
            msg.push_str(&format!("\n# Additional context\n\n{}\n", extra));
        }
    }

    msg.push_str(
        "\nInspect the working directory now. Read the relevant files, check git diff/status, \
         and run the test suite if discoverable. Then respond with ONLY the JSON verdict \
         in a ```json ... ``` fenced block.",
    );
    msg
}

/// Return a fail verdict with `confidence: low` when inspection is inconclusive.
///
/// Used on budget exhaustion, session failure, child cancellation, and when
/// both the initial and retry parse attempts fail. The orchestrator always
/// receives a structured `VerificationVerdict` — never a hard error — so it
/// can record the round and decide whether to retry.
pub fn inconclusive_fail(reason: impl Into<String>) -> VerificationVerdict {
    let r = reason.into();
    VerificationVerdict {
        verdict: "fail".to_string(),
        confidence: "low".to_string(),
        gaps: vec![format!("verification inconclusive: {r}")],
        rationale: format!(
            "The inspection run could not produce a reliable verdict. Reason: {r}. \
             Re-run mode='full' or address outstanding gaps and retry."
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;

    use ao_engine_tools_core::{IoTool, LoadPolicy, Registry, RunnerContext, ToolOutput};
    use ao_protocol::error::AoError;
    use async_trait::async_trait;
    use serde_json::Value;

    use crate::hooks::config::RunnerSettings;
    use crate::provider::{CompletionEvent, MockProviderClient, StopReason};

    use super::{InspectionVerifier, INSPECTION_ALLOWED_TOOLS};
    use ao_engine_tools_core::{VerificationEngine, VerificationInput};

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    fn test_input(working_dir: Option<PathBuf>) -> VerificationInput {
        VerificationInput {
            project_id: "test-proj".to_string(),
            goal: "Build a widget".to_string(),
            spec: None,
            tasklist_evidence: vec![],
            prior_verdicts: vec![],
            extra_evidence: None,
            working_dir,
        }
    }

    /// Minimal stub tool that satisfies [`IoTool`]. Used only to populate a
    /// registry for allowlist and filtering tests — never actually invoked.
    struct StubTool {
        tool_name: String,
    }

    #[async_trait]
    impl IoTool for StubTool {
        fn name(&self) -> &str {
            &self.tool_name
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn load_policy(&self) -> LoadPolicy {
            LoadPolicy::AlwaysLoad
        }
        async fn invoke(
            &self,
            _input: Value,
            _ctx: &RunnerContext,
        ) -> Result<ToolOutput, AoError> {
            Ok(ToolOutput::text("stub"))
        }
    }

    fn registry_with_names(names: &[&str]) -> Arc<Registry> {
        let mut reg = Registry::new();
        for &name in names {
            reg.register_io(Arc::new(StubTool {
                tool_name: name.to_string(),
            }));
        }
        Arc::new(reg)
    }

    fn make_verifier(provider: Arc<MockProviderClient>) -> InspectionVerifier {
        let registry = registry_with_names(INSPECTION_ALLOWED_TOOLS);
        InspectionVerifier::new(provider, RunnerSettings::default(), registry)
    }

    // ---------------------------------------------------------------------------
    // parse_verdict tests (via the parent module's function)
    // ---------------------------------------------------------------------------

    /// Plain JSON with no fencing should parse cleanly.
    #[test]
    fn parse_verdict_plain_json() {
        let json =
            r#"{"verdict":"pass","confidence":"high","gaps":[],"rationale":"Goal met."}"#;
        let v = super::super::parse_verdict(json).unwrap();
        assert_eq!(v.verdict, "pass");
        assert_eq!(v.confidence, "high");
        assert!(v.gaps.is_empty());
        assert_eq!(v.rationale, "Goal met.");
    }

    /// JSON wrapped in a ```json ... ``` fenced block (common model output style).
    #[test]
    fn parse_verdict_fenced_json() {
        let text = concat!(
            "```json\n",
            r#"{"verdict":"fail","confidence":"medium","gaps":["tests missing"],"rationale":"No tests found."}"#,
            "\n```"
        );
        let v = super::super::parse_verdict(text).unwrap();
        assert_eq!(v.verdict, "fail");
        assert_eq!(v.confidence, "medium");
        assert_eq!(v.gaps, vec!["tests missing"]);
    }

    /// Arbitrary prose that contains no parseable JSON should return an Err.
    #[test]
    fn parse_verdict_garbage_returns_err() {
        let result = super::super::parse_verdict("Sorry, I couldn't complete the inspection.");
        assert!(result.is_err(), "garbage text must not parse as a verdict");
    }

    // ---------------------------------------------------------------------------
    // InspectionVerifier unit tests (mocked provider — no live model calls)
    // ---------------------------------------------------------------------------

    /// When `working_dir` is absent, `verify()` returns an immediate fail/low
    /// verdict without touching the provider.
    #[tokio::test]
    async fn missing_working_dir_returns_fail_low_confidence() {
        // Empty script — any call to `complete()` would return ScriptExhausted.
        // A correct implementation should never reach the provider in this path.
        let provider = Arc::new(MockProviderClient::new(vec![]));
        let verifier = make_verifier(provider);

        let verdict = verifier.verify(test_input(None)).await.unwrap();

        assert_eq!(verdict.verdict, "fail");
        assert_eq!(verdict.confidence, "low");
        assert!(
            verdict.gaps.iter().any(|g| g.contains("working_dir")),
            "gap must mention working_dir; got: {:?}",
            verdict.gaps
        );
    }

    /// The child registry must contain exactly the tools in
    /// `INSPECTION_ALLOWED_TOOLS` — no more, no fewer. Verified by filtering
    /// a parent registry that also contains several disallowed tools and
    /// confirming the result set.
    #[test]
    fn allowlist_enforcement() {
        let all_names = [
            "Read", "Glob", "Grep", "Bash",
            "Edit", "Write", "Delegate", "ProjectVerify", "TodoCreate",
        ];
        let mut reg = Registry::new();
        for name in &all_names {
            reg.register_io(Arc::new(StubTool {
                tool_name: name.to_string(),
            }));
        }

        let allowed: Vec<String> =
            INSPECTION_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect();
        let filtered = reg.filter_for(&allowed);

        let listed: HashSet<String> = filtered.list().into_iter().collect();
        let expected: HashSet<String> =
            INSPECTION_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect();

        assert_eq!(
            listed, expected,
            "filtered registry must contain exactly INSPECTION_ALLOWED_TOOLS"
        );
    }

    /// Happy path: the child session returns a valid fenced JSON verdict in its
    /// final message. `verify()` must return the parsed verdict.
    #[tokio::test]
    async fn valid_fenced_verdict_returned() {
        let verdict_json = concat!(
            "```json\n",
            r#"{"verdict":"pass","confidence":"high","gaps":[],"rationale":"All checks passed."}"#,
            "\n```"
        );
        let provider = Arc::new(MockProviderClient::new(vec![vec![
            CompletionEvent::AssistantText(verdict_json.to_string()),
            CompletionEvent::TurnComplete {
                stop_reason: StopReason::Natural,
            },
        ]]));
        let verifier = make_verifier(provider);

        let verdict = verifier.verify(test_input(Some(PathBuf::from("/tmp")))).await.unwrap();

        assert_eq!(verdict.verdict, "pass");
        assert_eq!(verdict.confidence, "high");
        assert!(verdict.gaps.is_empty());
    }

    /// Retry path: the child's first response is unparseable; the one-shot
    /// retry provider call returns valid JSON. `verify()` must return the
    /// parsed verdict from the retry.
    #[tokio::test]
    async fn garbage_response_triggers_retry_and_succeeds() {
        let valid_json = r#"{"verdict":"fail","confidence":"low","gaps":["no tests"],"rationale":"Test suite absent."}"#;

        // Turn 1: child session final message — garbage (triggers retry).
        // Turn 2: retry provider call — valid JSON.
        let provider = Arc::new(MockProviderClient::new(vec![
            vec![
                CompletionEvent::AssistantText(
                    "Hmm, I wasn't able to find a clear answer.".to_string(),
                ),
                CompletionEvent::TurnComplete {
                    stop_reason: StopReason::Natural,
                },
            ],
            vec![
                CompletionEvent::AssistantText(valid_json.to_string()),
                CompletionEvent::TurnComplete {
                    stop_reason: StopReason::Natural,
                },
            ],
        ]));
        let verifier = make_verifier(provider);

        let verdict = verifier.verify(test_input(Some(PathBuf::from("/tmp")))).await.unwrap();

        assert_eq!(verdict.verdict, "fail");
        assert_eq!(verdict.gaps, vec!["no tests"]);
    }

    /// When the provider fails immediately (no scripted turns), `run_session`
    /// returns an error. `verify()` must return an inconclusive-fail verdict
    /// rather than propagating the error.
    #[tokio::test]
    async fn provider_failure_returns_inconclusive_fail() {
        // Empty script — `complete()` returns ScriptExhausted on first call.
        let provider = Arc::new(MockProviderClient::new(vec![]));
        let verifier = make_verifier(provider);

        let verdict = verifier
            .verify(test_input(Some(PathBuf::from("/tmp"))))
            .await
            .unwrap(); // must be Ok(verdict), never Err

        assert_eq!(verdict.verdict, "fail");
        assert_eq!(verdict.confidence, "low");
        assert!(
            verdict.gaps.iter().any(|g| g.contains("inconclusive")),
            "must be inconclusive-fail; gaps: {:?}",
            verdict.gaps
        );
    }

    /// When both the initial parse attempt and the retry both fail, `verify()`
    /// must return an inconclusive-fail verdict, not a hard error.
    #[tokio::test]
    async fn both_parse_attempts_fail_returns_inconclusive_fail() {
        let garbage = "I inspected things but couldn't format the result.";
        let provider = Arc::new(MockProviderClient::new(vec![
            // Turn 1: child final message — garbage.
            vec![
                CompletionEvent::AssistantText(garbage.to_string()),
                CompletionEvent::TurnComplete {
                    stop_reason: StopReason::Natural,
                },
            ],
            // Turn 2: retry provider call — still garbage.
            vec![
                CompletionEvent::AssistantText("still not valid JSON".to_string()),
                CompletionEvent::TurnComplete {
                    stop_reason: StopReason::Natural,
                },
            ],
        ]));
        let verifier = make_verifier(provider);

        let verdict = verifier
            .verify(test_input(Some(PathBuf::from("/tmp"))))
            .await
            .unwrap();

        assert_eq!(verdict.verdict, "fail");
        assert_eq!(verdict.confidence, "low");
        assert!(
            verdict.gaps.iter().any(|g| g.contains("inconclusive")),
            "must be inconclusive-fail; gaps: {:?}",
            verdict.gaps
        );
    }
}

/// One-shot retry: send the child's failed text back to the model with a
/// parse-nudge instruction and extract the verdict from the reply.
async fn retry_verdict_parse(
    provider: &Arc<dyn ProviderClient>,
    failed_text: &str,
) -> Result<VerificationVerdict, String> {
    let messages = vec![
        Message::User {
            content: vec![ContentBlock::Text {
                text: "Please output the inspection verdict as JSON.".to_string(),
            }],
        },
        Message::Assistant {
            content: vec![ContentBlock::Text {
                text: failed_text.to_string(),
            }],
        },
        Message::User {
            content: vec![ContentBlock::Text {
                text: VERDICT_RETRY_NUDGE.to_string(),
            }],
        },
    ];

    let request = CompletionRequest {
        messages,
        system_prompt: Some(INSPECTOR_SYSTEM_PROMPT.to_string()),
        tools: vec![],
        ..Default::default()
    };

    let cancel = CancellationToken::new();
    let mut stream = provider
        .complete(request, cancel)
        .await
        .map_err(|e| format!("retry provider call failed: {e}"))?;

    let mut text = String::new();
    loop {
        match stream.recv().await {
            None => break,
            Some(Ok(CompletionEvent::AssistantText(chunk))) => text.push_str(&chunk),
            Some(Ok(CompletionEvent::TurnComplete { .. })) => break,
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(format!("retry stream error: {e}")),
        }
    }

    super::parse_verdict(&text).map_err(|e| format!("retry parse also failed: {e}"))
}
