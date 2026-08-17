//! Query loop — top-level entry point that drives the runner pipeline
//! end-to-end.
//!
//! [`run_session`] takes an initial transcript, a [`RunnerContext`] (the
//! per-session identity / cancel / registry handle from
//! `ao-engine-tools-core`), and a [`RunnerConfig`] carrying the provider
//! seam, the prompt bridge, the merged `settings.json`, and the denial
//! tracker. It then loops:
//!
//! 1. Ask the provider for a turn (`provider.complete(...)`).
//! 2. Drain the [`CompletionStream`](crate::provider::CompletionStream),
//!    collecting [`CompletionEvent::AssistantText`](crate::provider::CompletionEvent)
//!    chunks and [`CompletionEvent::ToolUse`](crate::provider::CompletionEvent)
//!    blocks until [`CompletionEvent::TurnComplete`](crate::provider::CompletionEvent)
//!    or end-of-stream.
//! 3. If the turn carried no tool-use blocks, append the assistant turn
//!    to the transcript and return a [`SessionOutcome`].
//! 4. Otherwise, partition the tool-use blocks into batches respecting
//!    the concurrency-safe contract, hand each batch to
//!    [`run_batch`](crate::executor::run_batch) with a per-invocation
//!    pipeline closure (validate → pre-hook → permission → tool.invoke
//!    → post-hook → encode), then append the resulting `tool_result`
//!    blocks to the transcript in original order and loop.
//!
//! Cancellation is graceful: when [`RunnerContext::cancel`] fires, the
//! in-flight batch produces `cancelled` tool-result placeholders for any
//! invocations that did not complete, the loop appends those
//! placeholders to the transcript, and the next loop iteration returns
//! a [`SessionOutcome`] flagged with [`SessionOutcome::cancelled`]. The
//! one-result-per-`tool_use` invariant the dispatcher contract relies
//! on therefore always holds.
//!
//! The canonical [`crate::message::Message`] type carries the transcript
//! end-to-end: callers supply typed initial messages, the loop appends
//! typed assistant turns and `ToolResult` entries, and the final
//! transcript is returned as [`SessionOutcome::messages`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ao_engine_tools_core::{
    DenialTracker, EventKind, PermissionContext, PermissionDecision, PermissionMode, Registry,
    RunnerContext, SessionKind, TelemetryWriter, ToolAdmission, ToolBlock, ToolOutput, ToolRef,
    ToolUsageEvent,
};
use ao_protocol::data_root::resolve_data_root;
use ao_protocol::outcome::{ArtifactRef, OutcomeRecord, OutcomeSignal};
use chrono::Utc;
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::executor::{run_batch, InvocationResult};
use crate::message::{ContentBlock, Message};
use crate::tool_usage_log::JsonlTelemetryWriter;
use crate::hooks::{
    config::{HookEntry, RunnerSettings},
    run_post_hooks, run_pre_hooks, HookOutcome, HookRequest,
};
use crate::partition::{partition_invocations, ToolInvocation};
use crate::permissions::{
    evaluate_permission,
    rule::{parse_rule, rule_matches, PermissionRule},
    PermissionVerdict,
};
use crate::prompt_bridge::UserPromptBridge;
use crate::provider::{CompletionEvent, CompletionRequest, ProviderClient, ProviderError, ToolSpec, Usage};
use crate::validation::{validate_invocation, ValidationOutcome};

/// Wires [`run_session`] to its collaborators.
///
/// All trait-object handles are `Arc<dyn ... + Send + Sync>` so the
/// config can be cloned freely into spawned tasks. `settings` carries
/// the merged `settings.json` (project-local layered over user-global)
/// loaded by [`crate::hooks::config::load_runner_settings`].
///
/// `event_sink` is an optional live-event channel: when present, the
/// loop fans out per-chunk assistant text, every `tool_use` block, and
/// every `tool_result` payload as soon as the provider stream produces
/// them — useful for terminal REPLs that need real-time output instead
/// of the end-of-turn `SessionOutcome::messages` snapshot. Library
/// consumers (Tauri command handlers, tests) typically leave it `None`
/// and read the final transcript instead.
pub struct RunnerConfig {
    pub provider: Arc<dyn ProviderClient>,
    pub bridge: Arc<dyn UserPromptBridge>,
    pub denial_tracker: Arc<dyn DenialTracker>,
    pub settings: RunnerSettings,
    pub mode: PermissionMode,
    /// Whether a human is attending this session.
    ///
    /// Defaults to `Interactive`. Set to `Autonomous` for tasklist workers,
    /// background subagents, and scheduled-task runs where no operator is
    /// present. Controls tool registration (Sleep becomes available), the
    /// autonomous-pacing system-prompt section, drain priority, and
    /// permission-ask resolution.
    pub kind: SessionKind,
    /// Rules that auto-approve `Ask` decisions in Autonomous sessions without
    /// raising a dialog. Populated per-launch by the caller; ignored when
    /// `kind == Interactive`.
    ///
    /// Each entry is a parsed permission rule whose match predicate is applied
    /// to the tool call. When any entry matches, the decision resolves to
    /// `Allow` instead of auto-deny. The rule's `decision` field is ignored —
    /// all entries in this list are treated as `Allow`.
    pub auto_approve: Vec<PermissionRule>,
    pub system_prompt: Option<String>,
    pub event_sink: Option<Arc<dyn SessionEventSink>>,
    /// Reasoning channel configuration for this session. `None` means
    /// "use the provider's default" — for Anthropic that's no extended
    /// thinking on the API path; for the CLI path that's adaptive
    /// thinking with `display = "omitted"`. The query loop attaches
    /// this to every [`crate::provider::CompletionRequest`] it builds,
    /// mirroring how `system_prompt` is plumbed.
    pub thinking: Option<ao_protocol::agent::ThinkingConfig>,
    /// Optional cap on the number of provider turns this session may
    /// execute. When the counter reaches this value the session exits
    /// with `cancelled: true` and the last assistant text seen so far.
    ///
    /// `None` (the default) applies no cap — the session runs until the
    /// model naturally stops issuing tool calls or the `CancellationToken`
    /// fires.
    ///
    /// Set this for bounded runs such as inspection verifiers that should
    /// never consume unbounded turns regardless of how complex the task is.
    pub max_turns: Option<usize>,
}

/// Live events emitted by [`run_session`] when a [`SessionEventSink`] is
/// installed. Mirrors the slice of [`crate::provider::CompletionEvent`]
/// the CLI cares about, plus `ToolResult` (which the provider stream
/// never sees — it's produced by the runner after batch execution).
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// One chunk of assistant text. Concatenating all `AssistantText`
    /// payloads emitted between `ToolUse`/`ToolResult` events yields the
    /// full text for that assistant block.
    AssistantText(String),
    /// A `tool_use` block the model emitted in the current turn. Fired
    /// before the runner has dispatched the corresponding tool.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// A `tool_result` block the runner produced for a previously-fired
    /// `ToolUse` event with the same `id`. Fired after the per-call
    /// pipeline (validate → hooks → permission → invoke → encode) has
    /// completed, but before the next provider turn starts.
    ToolResult {
        tool_use_id: String,
        output: ToolOutput,
    },
    /// Token-usage accounting for the current turn, forwarded from the
    /// provider stream. Emit-only in v1 — loop control still ignores it.
    Usage(Usage),
    /// The provider opened a dedicated reasoning channel. Forwarded
    /// verbatim from [`crate::provider::CompletionEvent::ThinkingStart`].
    /// Surface this as the cue to mount a "Thinking…" indicator —
    /// providers may emit it without any subsequent deltas (the
    /// "thinking happened but text was suppressed" case), so the absence
    /// of deltas between start and end is itself a valid stream shape.
    ThinkingStart,
    /// One chunk of reasoning text within an in-progress thinking block.
    /// Forwarded verbatim from
    /// [`crate::provider::CompletionEvent::ThinkingDelta`]; concatenating
    /// all deltas between a `ThinkingStart` and its matching `ThinkingEnd`
    /// yields the full reasoning trace.
    ThinkingDelta { text: String },
    /// The provider closed the current reasoning channel. `elapsed_ms` is
    /// the wall-clock duration the provider measured from the matching
    /// `ThinkingStart`; downstream UIs use it to render a "Thought for Ns"
    /// footer when the bubble collapses.
    ThinkingEnd { elapsed_ms: u64 },
    /// A complete signed reasoning block captured from the provider after
    /// its `content_block_stop`. Emitted once per block, AFTER the
    /// matching `ThinkingEnd`. The sink MUST persist this to the transcript
    /// so the thinking block survives the persist→reload→replay round-trip:
    /// Anthropic rejects any follow-up request whose latest assistant turn
    /// is missing or has modified the thinking blocks that were present in
    /// the original response.
    ThinkingBlock {
        text: Option<String>,
        signature: Option<String>,
    },
    /// A complete redacted reasoning block. Same persistence contract as
    /// `ThinkingBlock` — the opaque `data` blob must be echoed back
    /// verbatim on the next turn.
    RedactedThinkingBlock { data: String },
    /// A synthesized user-role message that the runner is about to inject
    /// into the next turn — currently emitted by [`run_session`] after it
    /// drains [`RunnerContext::pending_user_messages`] (e.g. an inline
    /// skill body queued by the `RunSkill` tool). Sinks should persist the
    /// content as a hidden transcript entry (`hidden_from_user: true`) so
    /// the user-visible chat stream stays clean while the on-disk
    /// transcript remains faithful for reload and recall.
    HiddenUserMessage { content: String },
    /// Emitted after an async `AskUserQuestionWithForm` call has written the
    /// `form_request` transcript entry and recorded a `pending_forms` entry on
    /// the agent snapshot. Sinks should forward this so connected clients can
    /// surface the waiting form without requiring a manual reload.
    ///
    /// `spec` is the complete form the async tool call produced, parsed via
    /// `ao_engine_tools_core::form_events::parse_form_spec_payload` — same
    /// shape a sync `FormRequest` carries — so a live client can render the
    /// card directly from this event.
    FormPosted {
        form_id: String,
        spec: ao_engine_tools_core::FormSpecPayload,
    },
}

/// Optional fan-out channel for live session events. Implementations
/// must be cheap and non-blocking — the loop calls `emit` on the hot
/// path of every assistant text chunk.
pub trait SessionEventSink: Send + Sync {
    fn emit(&self, event: SessionEvent);

    /// The stable turn id the sink is currently persisting this run's entries
    /// under, if it tracks one. `run_session` reads this just before executing
    /// a round's tool calls and stamps it onto
    /// [`RunnerContext::current_message_id`], so an `ArtifactWrite` performed
    /// during those calls anchors its `source_message_id` to the same turn the
    /// assistant bubble is persisted under. That lets the produced artifact
    /// resolve inline in the thread bubble instead of only in the Assets panel.
    ///
    /// Default `None` for sinks that don't persist turn-scoped transcript
    /// entries (an artifact written under that state simply has no inline
    /// linkage — a supported, non-error condition).
    fn current_turn_id(&self) -> Option<String> {
        None
    }
}

/// Result of a completed (or cancelled) session.
///
/// `messages` is the full transcript, including the initial messages
/// the caller supplied, every assistant turn the provider produced, and
/// every `tool_result` block the runner emitted. `final_assistant_text`
/// is the concatenated text from the last assistant turn — a
/// convenience for callers that only care about the final answer.
/// `turns` counts how many provider calls completed; useful for tests
/// asserting that the loop exited at the expected boundary.
/// `cancelled` is `true` when the loop exited because the context's
/// cancel token fired; in that case the transcript still carries one
/// `tool_result` block per `tool_use` the model emitted in the final
/// in-flight turn.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionOutcome {
    pub messages: Vec<Message>,
    pub final_assistant_text: String,
    pub turns: usize,
    pub cancelled: bool,
}

/// Hard runner failures that terminate the session without a
/// [`SessionOutcome`].
#[derive(Debug, Error)]
pub enum RunnerError {
    /// The provider's `complete` call returned an error other than
    /// [`ProviderError::Cancelled`].
    #[error("provider error: {0}")]
    Provider(String),
}

impl From<ProviderError> for RunnerError {
    fn from(err: ProviderError) -> Self {
        Self::Provider(err.to_string())
    }
}

/// Drive the runner loop to completion against the supplied provider,
/// bridge, settings, and registry (held via `runner_ctx`).
///
/// Returns once the provider produces a turn with no `tool_use` blocks
/// (the natural exit), or once the supplied [`CancellationToken`] fires
/// (graceful exit; [`SessionOutcome::cancelled`] is `true`). Hard
/// provider failures bubble up as [`RunnerError::Provider`].
pub async fn run_session(
    initial_messages: Vec<Message>,
    runner_ctx: RunnerContext,
    config: RunnerConfig,
) -> Result<SessionOutcome, RunnerError> {
    let mut runner_ctx = init_session_context(runner_ctx, &config);
    let mut messages = initial_messages;
    let mut final_assistant_text = String::new();
    let mut turns: usize = 0;
    // Identifies this `run_session` call's `OutcomeRecord` — one call
    // answers one user turn (see the module doc), so a single id generated
    // here covers the whole loop below.
    let turn_id = uuid::Uuid::new_v4().to_string();

    loop {
        if runner_ctx.cancel.is_cancelled() {
            on_session_end(&runner_ctx).await;
            return Ok(SessionOutcome {
                messages,
                final_assistant_text,
                turns,
                cancelled: true,
            });
        }

        let loaded_deferred: HashSet<String> =
            runner_ctx.loaded_deferred_tools.read().unwrap().clone();

        let (tools, deferred_tools) = {
            let force_loaded: HashSet<String> = {
                let activated = runner_ctx.activated_tools.lock().unwrap();
                runner_ctx.always_load_tools.iter().cloned()
                    .chain(activated.iter().cloned())
                    .chain(loaded_deferred.iter().cloned())
                    .collect()
            };
            build_tool_specs(
                &runner_ctx.registry,
                &force_loaded,
                runner_ctx.tool_admission.as_ref(),
            )
        };

        // Visibility on the API turn boundary — names of the tools the
        // model can call this turn, plus the rolling counters. Keeps the
        // log readable when the model fans out into a few tools per turn
        // without dumping the full request body.
        tracing::info!(
            agent_id = %runner_ctx.agent_id,
            session_id = %runner_ctx.session_id,
            turn = turns + 1,
            message_count = messages.len(),
            tool_count = tools.len(),
            deferred_count = deferred_tools.len(),
            tools = %tools.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(","),
            "api turn → request"
        );

        let request = CompletionRequest {
            messages: messages.clone(),
            system_prompt: config.system_prompt.clone(),
            tools,
            mode: config.mode,
            deferred_tools,
            loaded_deferred_tools: loaded_deferred,
            thinking: config.thinking.clone(),
        };

        let mut stream = config
            .provider
            .complete(request, runner_ctx.cancel.clone())
            .await?;

        let mut assistant_text = String::new();
        let mut tool_uses: Vec<ToolInvocation> = Vec::new();
        // Reasoning blocks emitted by the provider this turn, in stream
        // order. Signed `thinking` blocks are captured after their
        // streaming `Thinking{Start,Delta,End}` triplet; redacted blocks
        // are captured from their single replay event. The assistant
        // message we append at end-of-turn echoes them back in this same
        // order — Anthropic rejects a follow-up turn whose transcript
        // carries any `tool_use` without the prior turn's matching
        // reasoning blocks (signatures and redacted payloads included). For
        // turns without reasoning (provider didn't open the channel) this
        // stays empty and the assistant message is built as before.
        let mut reasoning_blocks: Vec<ReplayReasoning> = Vec::new();
        loop {
            match stream.recv().await {
                None => break,
                Some(Ok(event)) => match event {
                    CompletionEvent::AssistantText(s) => {
                        if let Some(sink) = config.event_sink.as_ref() {
                            sink.emit(SessionEvent::AssistantText(s.clone()));
                        }
                        assistant_text.push_str(&s);
                    }
                    CompletionEvent::ToolUse { id, name, input } => {
                        // Compact preview of the tool input so logs stay
                        // greppable but skim long file paths / query bodies.
                        let input_preview = preview_value(&input, 200);
                        tracing::info!(
                            agent_id = %runner_ctx.agent_id,
                            session_id = %runner_ctx.session_id,
                            tool_use_id = %id,
                            tool = %name,
                            input = %input_preview,
                            "api turn → tool_use"
                        );
                        if let Some(sink) = config.event_sink.as_ref() {
                            sink.emit(SessionEvent::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            });
                        }
                        tool_uses.push(ToolInvocation { id, name, input });
                    }
                    CompletionEvent::Usage(u) => {
                        tracing::debug!(
                            agent_id = %runner_ctx.agent_id,
                            session_id = %runner_ctx.session_id,
                            input_tokens = u.input_tokens,
                            output_tokens = u.output_tokens,
                            cache_read = ?u.cache_read,
                            "api turn → usage"
                        );
                        if let Some(sink) = config.event_sink.as_ref() {
                            sink.emit(SessionEvent::Usage(u.clone()));
                        }
                        // v1 still ignores usage for loop control; emit-only.
                    }
                    CompletionEvent::ThinkingStart => {
                        tracing::debug!(
                            agent_id = %runner_ctx.agent_id,
                            session_id = %runner_ctx.session_id,
                            "api turn → thinking_start"
                        );
                        if let Some(sink) = config.event_sink.as_ref() {
                            sink.emit(SessionEvent::ThinkingStart);
                        }
                    }
                    CompletionEvent::ThinkingDelta { text } => {
                        tracing::trace!(
                            agent_id = %runner_ctx.agent_id,
                            session_id = %runner_ctx.session_id,
                            chars = text.len(),
                            "api turn → thinking_delta"
                        );
                        if let Some(sink) = config.event_sink.as_ref() {
                            sink.emit(SessionEvent::ThinkingDelta { text });
                        }
                    }
                    CompletionEvent::ThinkingEnd { elapsed_ms } => {
                        tracing::debug!(
                            agent_id = %runner_ctx.agent_id,
                            session_id = %runner_ctx.session_id,
                            elapsed_ms,
                            "api turn → thinking_end"
                        );
                        if let Some(sink) = config.event_sink.as_ref() {
                            sink.emit(SessionEvent::ThinkingEnd { elapsed_ms });
                        }
                    }
                    CompletionEvent::ThinkingBlock { text, signature } => {
                        // Capture for replay on the next turn. Forward to
                        // the event sink so `TimelineAdapter` can persist
                        // the block to the transcript — without this,
                        // thinking blocks are absent when history is
                        // reloaded on the next session and the API rejects
                        // the follow-up with a 400 "thinking blocks cannot
                        // be modified" error.
                        tracing::trace!(
                            agent_id = %runner_ctx.agent_id,
                            session_id = %runner_ctx.session_id,
                            text_len = text.as_ref().map(|s| s.len()).unwrap_or(0),
                            has_signature = signature.is_some(),
                            "api turn → thinking_block (replay)"
                        );
                        reasoning_blocks.push(ReplayReasoning::Thinking { text: text.clone(), signature: signature.clone() });
                        if let Some(sink) = config.event_sink.as_ref() {
                            sink.emit(SessionEvent::ThinkingBlock { text, signature });
                        }
                    }
                    CompletionEvent::RedactedThinkingBlock { data } => {
                        // Same persistence contract as ThinkingBlock above.
                        tracing::trace!(
                            agent_id = %runner_ctx.agent_id,
                            session_id = %runner_ctx.session_id,
                            data_len = data.len(),
                            "api turn → redacted_thinking_block (replay)"
                        );
                        reasoning_blocks.push(ReplayReasoning::Redacted { data: data.clone() });
                        if let Some(sink) = config.event_sink.as_ref() {
                            sink.emit(SessionEvent::RedactedThinkingBlock { data });
                        }
                    }
                    CompletionEvent::TurnComplete { stop_reason } => {
                        tracing::info!(
                            agent_id = %runner_ctx.agent_id,
                            session_id = %runner_ctx.session_id,
                            ?stop_reason,
                            text_len = assistant_text.len(),
                            tool_uses = tool_uses.len(),
                            "api turn → complete"
                        );
                        break;
                    }
                    CompletionEvent::Error(msg) => {
                        tracing::warn!(
                            agent_id = %runner_ctx.agent_id,
                            session_id = %runner_ctx.session_id,
                            error = %msg,
                            "api turn → soft provider error (continuing)"
                        );
                        // Soft mid-stream error: the provider chose to
                        // keep the turn open so we record it on the
                        // assistant turn and continue draining.
                        assistant_text.push_str(&format!("[provider error: {msg}]"));
                    }
                },
                Some(Err(err)) => {
                    if matches!(err, ProviderError::Cancelled) {
                        tracing::info!(
                            agent_id = %runner_ctx.agent_id,
                            session_id = %runner_ctx.session_id,
                            "api turn → cancelled mid-stream"
                        );
                        on_session_end(&runner_ctx).await;
                        return Ok(SessionOutcome {
                            messages,
                            final_assistant_text,
                            turns,
                            cancelled: true,
                        });
                    }
                    tracing::error!(
                        agent_id = %runner_ctx.agent_id,
                        session_id = %runner_ctx.session_id,
                        error = %err,
                        "api turn → hard provider error"
                    );
                    return Err(err.into());
                }
            }
        }

        turns = turns.saturating_add(1);

        // Turn cap: exit with cancelled=true when the session has exhausted its
        // allotted provider calls. Inspection verifiers set this to a small value
        // (~15) so a runaway child cannot consume unbounded turns.
        if let Some(cap) = config.max_turns {
            if turns >= cap {
                on_session_end(&runner_ctx).await;
                return Ok(SessionOutcome {
                    messages,
                    final_assistant_text: assistant_text,
                    turns,
                    cancelled: true,
                });
            }
        }

        if !assistant_text.is_empty()
            || !tool_uses.is_empty()
            || !reasoning_blocks.is_empty()
        {
            messages.push(assistant_turn(
                &assistant_text,
                &tool_uses,
                &reasoning_blocks,
            ));
        }

        if tool_uses.is_empty() {
            final_assistant_text = assistant_text;
            on_session_end(&runner_ctx).await;
            finalize_turn_outcome(&runner_ctx, &turn_id).await;
            return Ok(SessionOutcome {
                messages,
                final_assistant_text,
                turns,
                cancelled: false,
            });
        }

        // Anchor any artifact this round's tool calls produce to the assistant
        // turn currently being persisted: `ArtifactWrite` reads
        // `current_message_id` to stamp `source_message_id`, and the sink's
        // turn id is exactly what the response bubble carries in its persisted
        // metadata — so the artifact resolves inline in the thread rather than
        // only in the Assets panel. `None`-tolerant: a sink that tracks no turn
        // simply leaves the artifact without inline linkage.
        runner_ctx.current_message_id = config
            .event_sink
            .as_ref()
            .and_then(|sink| sink.current_turn_id());

        let batches = partition_invocations(&tool_uses, &runner_ctx.registry);

        let mut id_to_position: HashMap<String, usize> = HashMap::with_capacity(tool_uses.len());
        for (i, inv) in tool_uses.iter().enumerate() {
            id_to_position.insert(inv.id.clone(), i);
        }
        let mut ordered_results: Vec<Option<InvocationResult>> =
            (0..tool_uses.len()).map(|_| None).collect();

        let cap = config.settings.permissions.concurrent_tool_cap.max(1);
        for batch in &batches {
            let runner_ctx_outer = runner_ctx.clone();
            let config_ref = &config;
            let results = run_batch(
                batch,
                cap,
                runner_ctx.cancel.clone(),
                |inv: ToolInvocation, cancel: CancellationToken| {
                    let runner_ctx_inner = runner_ctx_outer.clone();
                    async move { run_one_invocation(inv, runner_ctx_inner, config_ref, cancel).await }
                },
            )
            .await;

            for r in results {
                if let Some(pos) = id_to_position.get(&r.id) {
                    ordered_results[*pos] = Some(r);
                }
            }
        }

        for slot in ordered_results.into_iter() {
            let r = slot.expect("every tool_use produces exactly one result");
            // Lookup the tool name + log the outcome before consuming the
            // payload. `as_text()` already truncates structured outputs to a
            // textual representation; cap to 200 chars for log brevity.
            let inv_name = tool_uses
                .iter()
                .find(|inv| inv.id == r.id)
                .map(|inv| inv.name.as_str())
                .unwrap_or("?");
            let is_error = matches!(&r.payload, ao_engine_tools_core::ToolOutput::Error { .. })
                || matches!(&r.payload, ao_engine_tools_core::ToolOutput::Structured(v)
                    if v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false));
            let output_preview = preview_text(&r.payload.as_text(), 200);
            if is_error {
                tracing::warn!(
                    agent_id = %runner_ctx.agent_id,
                    session_id = %runner_ctx.session_id,
                    tool_use_id = %r.id,
                    tool = %inv_name,
                    output = %output_preview,
                    "api turn → tool_result (error)"
                );
            } else {
                tracing::info!(
                    agent_id = %runner_ctx.agent_id,
                    session_id = %runner_ctx.session_id,
                    tool_use_id = %r.id,
                    tool = %inv_name,
                    output = %output_preview,
                    "api turn → tool_result"
                );
            }
            if let Some(sink) = config.event_sink.as_ref() {
                sink.emit(SessionEvent::ToolResult {
                    tool_use_id: r.id.clone(),
                    output: r.payload.clone(),
                });
            }
            if inv_name == "AskUserQuestionWithForm" {
                if let ToolOutput::Structured(ref v) = r.payload {
                    if v.get("posted").and_then(|p| p.as_bool()) == Some(true) {
                        if let (Some(form_id), Some(spec)) = (
                            v.get("form_id").and_then(|f| f.as_str()).map(str::to_owned),
                            v.get("spec").cloned(),
                        ) {
                            // Project-scoped runs store form state under the project key.
                            let scope_key: String = runner_ctx
                                .project_id
                                .as_deref()
                                .map(|pid| format!("project_{}", pid))
                                .unwrap_or_else(|| runner_ctx.agent_id.clone());

                            // Shared with the out-of-band `wire_posted_async_form`
                            // path (MCP bridge) — persists the form_request
                            // transcript entry and upserts the thread-scoped
                            // pending-form pointer on the snapshot.
                            ao_engine_tools_core::form_events::persist_posted_form(
                                &runner_ctx,
                                &scope_key,
                                &form_id,
                                &spec,
                            )
                            .await;
                            if let Some(sink) = config.event_sink.as_ref() {
                                sink.emit(SessionEvent::FormPosted {
                                    form_id: form_id.clone(),
                                    spec: ao_engine_tools_core::form_events::parse_form_spec_payload(
                                        &form_id, &spec,
                                    ),
                                });
                            }
                        }
                    }
                }
            }
            messages.push(tool_result_message(&r.id, &r.payload));
        }

        // Drain user-role messages enqueued during this turn. Normal-priority
        // messages always drain; low-priority messages (e.g. background delegate
        // completion notices) are held in Autonomous sessions until the first turn
        // boundary where Sleep did not run, then released at that boundary.
        // Drain happens AFTER the tool_result push so messages land immediately
        // after the triggering tool_result — the ordering the model needs.
        let drained: Vec<String> = {
            let sleep_ran = runner_ctx.sleep_ran();
            runner_ctx.reset_sleep_ran();
            let mut q = runner_ctx.pending_user_messages.lock().unwrap();
            q.drain_for(config.kind, sleep_ran)
        };
        for content in drained {
            if let Some(sink) = config.event_sink.as_ref() {
                sink.emit(SessionEvent::HiddenUserMessage {
                    content: content.clone(),
                });
            }
            messages.push(Message::User {
                content: vec![ContentBlock::Text { text: content }],
            });
        }

        // Clear the skill tool filter at the turn boundary so each new
        // assistant turn starts without a stale per-skill allowlist.
        runner_ctx.clear_skill_tool_filter();
    }
}

/// Compute the session's always-loaded tool set and install a real telemetry
/// writer, producing an updated [`RunnerContext`] ready for the query loop.
///
/// - `always_load_tools` is recomputed from the registry and the user's
///   `tool_load_overrides` settings every session so per-session overrides
///   take effect immediately.
/// - `activated_tools` is left untouched: top-level sessions start with the
///   empty set from [`RunnerContext::new`]; child sessions inherit the parent's
///   `Arc` via `.child()` so activations are visible across the tree.
/// - Telemetry is replaced with [`JsonlTelemetryWriter`] only when the context
///   carries the default [`NoopTelemetryWriter`]. Caller-supplied writers (e.g.
///   test spies) are preserved as-is.
pub(crate) fn init_session_context(runner_ctx: RunnerContext, config: &RunnerConfig) -> RunnerContext {
    // Stamp the session kind so tools and the drain loop can read it.
    let runner_ctx = runner_ctx.with_kind(config.kind);

    // Extend the registry with autonomous-only tools (e.g. Sleep) when this
    // is an Autonomous session. The base registry never includes these tools;
    // cloning it here produces a lightweight per-session extension without
    // mutating the process-wide catalog.
    let runner_ctx = if config.kind == SessionKind::Autonomous {
        let mut extended = (*runner_ctx.registry).clone();
        ao_engine_tools_engine::register_autonomous_tools(&mut extended);
        runner_ctx.with_registry(Arc::new(extended))
    } else {
        runner_ctx
    };

    // `always_load_tools` is purely load-policy driven (which tools are eager
    // vs. deferred). The agent-level admission gate (`tool_admission`) is applied
    // separately when each turn's tool array is built, so the two concerns stay
    // independent: an agent's deny list never silently promotes a deferred tool,
    // and an allow list never disturbs deferred flagging.
    let always_load_tools =
        Arc::new(runner_ctx.registry.resolved_loaded_set(&config.settings.tool_load_overrides));
    let ctx = runner_ctx.with_always_load_tools(always_load_tools);
    if ctx.telemetry.is_noop() {
        match resolve_data_root() {
            Ok(data_root) => {
                let log_path = data_root
                    .join("agent_homes")
                    .join(&ctx.agent_id)
                    .join("tool_usage.jsonl");
                ctx.with_telemetry(Arc::new(JsonlTelemetryWriter::new(log_path))
                    as Arc<dyn TelemetryWriter + Send + Sync>)
            }
            Err(err) => {
                tracing::warn!("session init: cannot resolve data root for telemetry: {err}");
                ctx
            }
        }
    } else {
        ctx
    }
}

/// Grace period given to each live background agent to finish cleanly
/// before its handle is dropped during session teardown.
const DEFAULT_TEARDOWN_GRACE_PERIOD: Duration = Duration::from_millis(500);

/// Cleanup executed on every session exit path (completion or cancellation).
///
/// Cancels all live background agents via `background_agents.cancel_all()`,
/// cancels pending questions via `prompt_bridge.cancel_pending()`, cancels
/// pending forms via `form_bridge.cancel_pending()` (a session cancelled while
/// `AskUserQuestionWithForm` is suspended would otherwise leak the form's
/// oneshot sender in the bridge's channel map), then drains the
/// `worktree_stack` and restores `cwd` to the bottom-of-stack value (the
/// session-start cwd pushed first). If the stack is empty the cwd is already
/// at the session-start path and this is a no-op. Emits no events — cleanup
/// is silent; events fire only from happy-path tool invocations.
async fn on_session_end(ctx: &RunnerContext) {
    ctx.background_agents
        .cancel_all(DEFAULT_TEARDOWN_GRACE_PERIOD)
        .await;
    ctx.prompt_bridge.cancel_pending();
    ctx.form_bridge.cancel_pending();
    let mut stack = ctx.worktree_stack.lock().unwrap();
    if stack.is_empty() {
        return;
    }
    let original: PathBuf = stack[0].restore_cwd.clone();
    stack.clear();
    drop(stack);
    *ctx.cwd.write().unwrap() = original;
}

/// Build and persist this turn's [`OutcomeRecord`], capturing whatever
/// artifacts `ctx.artifacts_used` accumulated during the turn (memory
/// entries surfaced at turn start, skills invoked along the way). A no-op
/// when the context carries no `outcome_store` — test fixtures and contexts
/// that don't wire outcome persistence skip the write silently rather than
/// erroring.
///
/// Only called from the natural "no more tool calls" exit — a cancelled or
/// turn-capped run didn't complete, so there is nothing meaningful yet to
/// score.
///
/// TODO(outcome-signal): `signal` is a first-cut heuristic — every persisted
/// record is `Implicit`. Detecting explicit reactions and correlating a
/// specific artifact with a *following* correction (rather than judging the
/// whole turn) is richer derivation that belongs to memory decay/boost and
/// skill retirement, the actual consumers of this record.
async fn finalize_turn_outcome(ctx: &RunnerContext, turn_id: &str) {
    let Some(store) = ctx.outcome_store.as_ref() else {
        return;
    };
    let record = OutcomeRecord {
        turn_id: turn_id.to_string(),
        session_id: ctx.session_id.clone(),
        artifacts_used: ctx.artifacts_used_snapshot(),
        signal: OutcomeSignal::Implicit,
        timestamp: Utc::now(),
    };
    if let Err(err) = store.append(&ctx.agent_id, &record).await {
        tracing::warn!(
            agent_id = %ctx.agent_id,
            session_id = %ctx.session_id,
            turn_id = %turn_id,
            error = %err,
            "failed to persist turn outcome record"
        );
    }
}

/// Run a single tool invocation through the full per-call pipeline:
/// validate → pre-hook → permission → tool.invoke → post-hook → encode.
///
/// Any non-`Ok` outcome along the validation, hook, or permission
/// stages short-circuits to a `tool_result` error block (the post-hook
/// stage is also skipped — post-hooks observe successful invocations
/// only) so the dispatcher always emits exactly one result per
/// `tool_use`.
async fn run_one_invocation(
    inv: ToolInvocation,
    runner_ctx: RunnerContext,
    config: &RunnerConfig,
    cancel: CancellationToken,
) -> InvocationResult {
    let id = inv.id.clone();
    let name = inv.name.clone();
    let input = inv.input.clone();

    let tool_ref = match runner_ctx.registry.lookup(&name) {
        Some(t) => t,
        None => {
            // Fuzzy-suggest from the registry so the model gets a corrective
            // hint instead of a bare "unknown tool" (typos, case-folded
            // variants). Identical mechanism to the CLI XML dispatcher in
            // `crates/ao-engine/src/agent_runner/cli.rs::dispatch_xml_tool_call`;
            // the API path has no wire-format wrapper concern (the request body
            // is structured JSON, not a system-prompt-injected XML catalog),
            // so the message stays short.
            let message = match runner_ctx.registry.nearest_name(&name) {
                Some(suggested) => format!(
                    "unknown tool '{name}'. Did you mean '{suggested}'?"
                ),
                None => format!("unknown tool '{name}'"),
            };
            return error_result(
                id,
                ToolOutput::Error {
                    message,
                    recoverable: false,
                },
            );
        }
    };

    if !runner_ctx.check_skill_tool_filter(&name) {
        return error_result(
            id,
            ToolOutput::Error {
                message: format!(
                    "tool '{name}' is not permitted within this skill's allowed-tools scope"
                ),
                recoverable: true,
            },
        );
    }

    let validation = validate_invocation(&tool_ref, &input).await;
    let parsed_input = match validation {
        ValidationOutcome::Ok(v) => v,
        ValidationOutcome::SchemaError(msg) | ValidationOutcome::ToolError(msg) => {
            return error_result(
                id,
                ToolOutput::Error {
                    message: msg,
                    recoverable: false,
                },
            );
        }
    };

    let matched_pre: Vec<&HookEntry> = config
        .settings
        .hooks
        .pre_tool_use
        .iter()
        .filter(|h| hook_match_string(&h.r#match, &name, &parsed_input))
        .collect();

    let pre_request = HookRequest {
        tool_name: name.clone(),
        input: parsed_input.clone(),
        agent_id: runner_ctx.agent_id.clone(),
        session_id: runner_ctx.session_id.clone(),
    };
    let pre_outcome = if matched_pre.is_empty() {
        HookOutcome::Continue
    } else {
        run_pre_hooks(&matched_pre, &pre_request, cancel.clone()).await
    };

    let perm_ctx = PermissionContext::new(
        runner_ctx.permissions.mode(),
        runner_ctx.agent_id.clone(),
        runner_ctx.session_id.clone(),
    )
    .with_tracker(config.denial_tracker.clone());

    let tool_decision = match &tool_ref {
        ToolRef::Io(t) => t.check_permissions(&parsed_input, &perm_ctx).await,
        ToolRef::Engine(t) => t.check_permissions(&parsed_input, &perm_ctx).await,
    };

    let mutates = match &tool_ref {
        ToolRef::Io(t) => t.mutates_for_input(&parsed_input),
        ToolRef::Engine(t) => t.mutates_for_input(&parsed_input),
    };

    let verdict = evaluate_permission(
        tool_decision,
        pre_outcome,
        &config.settings.permissions,
        &perm_ctx,
        config.bridge.as_ref(),
        &name,
        &parsed_input,
        mutates,
        config.kind,
        &config.auto_approve,
    )
    .await;

    let invocation_input = match verdict {
        PermissionVerdict::Allow => parsed_input.clone(),
        PermissionVerdict::AllowMutated(v) => v,
        PermissionVerdict::Deny(reason) => {
            return error_result(
                id,
                ToolOutput::Error {
                    message: reason,
                    recoverable: false,
                },
            );
        }
        PermissionVerdict::AutoDeny(reason) => {
            return error_result(
                id,
                ToolOutput::Error {
                    message: reason,
                    recoverable: true,
                },
            );
        }
    };

    if cancel.is_cancelled() {
        return error_result(
            id,
            ToolOutput::Error {
                message: "cancelled".to_string(),
                recoverable: false,
            },
        );
    }

    let invoke_result = match &tool_ref {
        ToolRef::Io(t) => t.invoke(invocation_input.clone(), &runner_ctx).await,
        ToolRef::Engine(t) => t.invoke(invocation_input.clone(), &runner_ctx).await,
    };

    let payload = match invoke_result {
        Ok(out) => out,
        Err(err) => ToolOutput::Error {
            message: err.to_string(),
            recoverable: false,
        },
    };

    if !matches!(payload, ToolOutput::Error { .. }) {
        let is_activated = runner_ctx.activated_tools.lock().unwrap().contains(&name);
        if is_activated {
            runner_ctx.telemetry.emit(ToolUsageEvent {
                agent_id: runner_ctx.agent_id.clone(),
                session_id: runner_ctx.session_id.clone(),
                tool_name: name.clone(),
                kind: EventKind::Invoked,
                ts: Utc::now(),
                metadata: serde_json::Value::Object(Default::default()),
            });
        }
        // Skill reuse is one of the three implicit-signal shapes — record
        // it as an artifact this turn drew on so `finalize_turn_outcome` can
        // include it in the turn's `OutcomeRecord`.
        if name == "RunSkill" {
            if let Some(skill_name) = invocation_input.get("skill").and_then(|v| v.as_str()) {
                let skill_name = skill_name.strip_prefix('/').unwrap_or(skill_name);
                runner_ctx.record_artifact_used(ArtifactRef::skill(skill_name));
            }
        }
    }

    let matched_post: Vec<&HookEntry> = config
        .settings
        .hooks
        .post_tool_use
        .iter()
        .filter(|h| hook_match_string(&h.r#match, &name, &parsed_input))
        .collect();
    if !matched_post.is_empty() {
        let post_request = HookRequest {
            tool_name: name.clone(),
            input: invocation_input.clone(),
            agent_id: runner_ctx.agent_id.clone(),
            session_id: runner_ctx.session_id.clone(),
        };
        run_post_hooks(&matched_post, &post_request).await;
    }

    InvocationResult {
        id,
        index: 0,
        payload,
    }
}

fn error_result(id: String, payload: ToolOutput) -> InvocationResult {
    InvocationResult {
        id,
        index: 0,
        payload,
    }
}

/// Decide whether a hook entry's `match` string applies to a given tool
/// invocation. Reuses the rule grammar parser so hook matches and
/// permission rules share the same `Tool(arg-glob)` semantics. A
/// malformed match string disables the hook silently — the loader
/// already validated rule decisions, so the only way this branch fires
/// is on a hand-edited config where the match expression itself is
/// invalid.
fn hook_match_string(rule_str: &str, tool_name: &str, input: &Value) -> bool {
    parse_rule(rule_str, PermissionDecision::Allow)
        .ok()
        .map(|r| rule_matches(&r, tool_name, input))
        .unwrap_or(false)
}

/// Build tool specs for a provider request.
///
/// Returns `(specs, deferred_names)` where `specs` contains every admitted
/// registered tool (always-loaded and deferred) and `deferred_names` is the set
/// of admitted tool names that are NOT in `force_loaded` (i.e. their policy is
/// Deferred and they have not yet been resolved via ToolSearch).
///
/// `force_loaded` = always_load_tools ∪ activated_tools ∪ loaded_deferred_tools.
/// Tools in `force_loaded` are included in `specs` but NOT in `deferred_names`
/// (they are treated as fully loaded regardless of their registered policy).
///
/// `admission` is the agent-level gate. When `Some`, any tool the gate does not
/// permit is dropped entirely — it appears in neither `specs` nor
/// `deferred_names`, so a denied tool can never reach the model (not even as a
/// deferred entry, and not via a later `ToolSearch` activation).
fn build_tool_specs(
    registry: &Registry,
    force_loaded: &HashSet<String>,
    admission: Option<&ToolAdmission>,
) -> (Vec<ToolSpec>, HashSet<String>) {
    let names = registry.list();
    let mut specs = Vec::with_capacity(names.len());
    let mut deferred_names = HashSet::new();
    for name in names {
        // Agent-level admission gate is the single chokepoint: a tool the gate
        // excludes is skipped before it can be emitted or flagged.
        if let Some(gate) = admission {
            if !gate.permits(&name) {
                continue;
            }
        }
        let Some(t) = registry.lookup(&name) else {
            continue;
        };
        let (description, schema) = match &t {
            ToolRef::Io(io) => (io.description().to_string(), io.input_schema()),
            ToolRef::Engine(eg) => (eg.description().to_string(), eg.input_schema()),
        };
        // A tool is advertised as deferred whenever it is not in the loaded set
        // for this turn. `force_loaded` already accounts for load policy and its
        // overrides (always_load_tools = resolved load set) plus runtime
        // activation, so membership — not the tool's intrinsic policy — is the
        // correct signal. This keeps a ForceDeferred-overridden always-load tool
        // flagged (it was demoted out of always_load_tools) instead of leaking
        // into the eager set.
        if !force_loaded.contains(&name) {
            deferred_names.insert(name.clone());
        }
        specs.push(ToolSpec {
            name,
            description,
            input_schema: schema,
        });
    }
    (specs, deferred_names)
}

/// Compact, single-line preview of a JSON-shaped tool input. Long values
/// truncate; sensitive keys (e.g. `api_key`) are not specially redacted —
/// callers must avoid feeding secrets through tool calls.
fn preview_value(v: &Value, max: usize) -> String {
    let raw = match serde_json::to_string(v) {
        Ok(s) => s,
        Err(_) => format!("{:?}", v),
    };
    preview_text(&raw, max)
}

/// Truncate a textual blob to one line, replacing newlines with `\n`
/// escapes so the log entry stays single-line greppable.
fn preview_text(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', "\\n");
    if one_line.len() <= max {
        one_line
    } else {
        let boundary = clamp_to_char_boundary(&one_line, max);
        format!("{}…<{} bytes>", &one_line[..boundary], one_line.len())
    }
}

/// Largest byte index `<= max` that lands on a valid UTF-8 char boundary in
/// `text`. `preview_text` caps by byte length; without this, a cap that
/// falls inside a multi-byte character (tool input/output routinely carries
/// em-dashes, emoji, CJK, etc.) would panic on the raw slice.
fn clamp_to_char_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    (0..=max).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0)
}

/// A reasoning block captured during a turn for verbatim replay on the
/// next request. Anthropic requires every reasoning block the model
/// emitted before a `tool_use` to be echoed back in its original
/// position: signed `thinking` blocks with their text and signature, and
/// `redacted_thinking` blocks with their opaque payload. Keeping both in
/// one ordered list preserves the relative order the two kinds can
/// interleave in within a single turn.
enum ReplayReasoning {
    Thinking {
        text: Option<String>,
        signature: Option<String>,
    },
    Redacted {
        data: String,
    },
}

/// Build the assistant message that closes the current turn.
///
/// Block ordering matters for multi-turn replay against Anthropic's
/// API: reasoning blocks must precede the text and tool_use blocks they
/// preceded on the wire, and signed vs. redacted reasoning blocks must
/// keep their original relative order among themselves. Recreating that
/// order is what keeps the next turn's request legal when extended
/// thinking was enabled and any tool_use was emitted. Providers without a
/// reasoning channel pass an empty `reasoning_blocks` slice and the shape
/// collapses to the legacy text+tool_use layout.
fn assistant_turn(
    text: &str,
    tool_uses: &[ToolInvocation],
    reasoning_blocks: &[ReplayReasoning],
) -> Message {
    let mut content = Vec::with_capacity(reasoning_blocks.len() + 1 + tool_uses.len());
    for block in reasoning_blocks {
        match block {
            ReplayReasoning::Thinking { text, signature } => {
                content.push(ContentBlock::Thinking {
                    text: text.clone(),
                    signature: signature.clone(),
                });
            }
            ReplayReasoning::Redacted { data } => {
                content.push(ContentBlock::RedactedThinking { data: data.clone() });
            }
        }
    }
    if !text.is_empty() {
        content.push(ContentBlock::Text { text: text.to_string() });
    }
    for inv in tool_uses {
        content.push(ContentBlock::ToolUse {
            id: inv.id.clone(),
            name: inv.name.clone(),
            input: inv.input.clone(),
        });
    }
    Message::Assistant { content }
}

fn tool_result_message(id: &str, payload: &ToolOutput) -> Message {
    // The multimodal `Blocks` payload maps each block to a canonical content
    // block; every other variant collapses to a single text block.
    if let ToolOutput::Blocks(blocks) = payload {
        let content = blocks
            .iter()
            .map(|b| match b {
                ToolBlock::Text { text } => ContentBlock::Text { text: text.clone() },
                ToolBlock::Image { media_type, data } => ContentBlock::Image {
                    media_type: media_type.clone(),
                    data: data.clone(),
                },
                ToolBlock::Document {
                    media_type,
                    data,
                    title,
                } => ContentBlock::Document {
                    media_type: media_type.clone(),
                    data: data.clone(),
                    title: title.clone(),
                },
            })
            .collect();
        return Message::ToolResult {
            tool_use_id: id.to_string(),
            content,
            is_error: false,
        };
    }

    let (content_text, is_error) = match payload {
        ToolOutput::Text(s) => (s.clone(), false),
        ToolOutput::Structured(v) => {
            let is_err = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
            (ToolOutput::structured_to_text(v), is_err)
        }
        ToolOutput::Error { message, .. } => (message.clone(), true),
        ToolOutput::Blocks(_) => unreachable!("handled above"),
    };
    Message::ToolResult {
        tool_use_id: id.to_string(),
        content: vec![ContentBlock::Text { text: content_text }],
        is_error,
    }
}

#[cfg(test)]
mod tests;
