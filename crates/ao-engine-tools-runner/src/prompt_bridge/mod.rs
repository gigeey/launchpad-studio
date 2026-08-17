//! User-prompt bridge — trait surface the runner uses to ask the
//! operator about ambiguous tool calls, plus an in-memory denial counter
//! that fences repeated `Ask` outcomes from runaway subagents.
//!
//! # Trait surface
//!
//! [`UserPromptBridge`] is an async trait that extends [`QuestionBridge`]
//! (from `ao-engine-tools-core`). The runner's permission gate hands it an
//! [`AskRequest`] (tool name, input, reason, plus the calling agent / session
//! identifiers) and awaits an [`AskOutcome`] back. Three reference
//! implementations ship with the crate:
//!
//! - [`StubBridge`] — denies every prompt and returns `NoOperator` for
//!   questions. The right choice for SDK embeddings or any non-interactive
//!   session where there is nobody to answer.
//! - [`ScriptedBridge`] — pops a pre-recorded outcome per call. Used by
//!   the runner's own tests and the crate-level integration tests.
//! - [`LiveBridge`] — the real bridge backed by per-question oneshot
//!   channels. `ask_question` suspends until `deliver_answer` is called
//!   from outside (typically a WebSocket handler or a test peer task).
//!   `cancel_pending` drains the channel map so all pending futures resolve
//!   with `Err(Cancelled)` when the session is cancelled.
//! - [`StdinBridge`] — terminal-shaped bridge that prints the request to
//!   stdout and reads a one-character answer (y/s/n) from stdin. The
//!   intended pick for the CLI dogfood loop where the human at the
//!   terminal is the operator. A `tokio::sync::Mutex` serializes
//!   concurrent prompts so a batched tool group prompts the operator one
//!   at a time.
//!
//! The real WebSocket-backed bridge that talks to the desktop frontend
//! lives in a separate downstream crate; this module deliberately stays
//! free of UI / transport concerns.
//!
//! # Question-bridge re-exports
//!
//! [`QuestionRequest`], [`Choice`], [`ChoiceId`], [`AskQuestionError`], and
//! [`QuestionBridge`] are defined in `ao-engine-tools-core` and re-exported
//! here for convenience so callers can import everything from this module.
//!
//! # Denial fencing
//!
//! [`InMemoryDenialTracker`] implements
//! [`DenialTracker`](ao_engine_tools_core::DenialTracker) with a
//! `Mutex<HashMap>` keyed by `(agent_id, tool_name)`. The trait method
//! `record_denial` increments the counter for the supplied pair; the
//! gate consults `count` BEFORE prompting the user so a counter that has
//! reached the configured threshold short-circuits to `Deny` without
//! disturbing the user. `reset_session` drops every counter recorded for
//! the named session — call it when a session terminates so repeated
//! agent ids in a fresh session start with a clean slate.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ao_engine_tools_core::{DenialTracker, EventSink, UserEvent};
use ao_persistence::snapshot::SnapshotStore;
use ao_persistence::transcript::TranscriptStore;
use async_trait::async_trait;
use serde_json::{json, Value};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

// Re-export core question-bridge and form-bridge types so callers can import
// everything from this module without reaching into ao-engine-tools-core directly.
pub use ao_engine_tools_core::{
    form_request_entry, form_withdrawn_entry, AskQuestionError, Choice, ChoiceId, FormAnswer,
    FormBridge, FormField, FormFieldKind, FormFieldPayload, FormOption, FormOptionPayload,
    FormRequest, FormRequestMeta, FormResponse, NoopFormBridge, QuestionBridge, QuestionRequest,
};

/// Per-agent registry of live [`LiveFormBridge`] instances.
///
/// Holds multiple bridges per agent so that parallel tool calls originating from
/// the same agent can each own an independent bridge without clobbering one
/// another. Bridges are identified by pointer identity so a request that
/// deregisters its own bridge cannot accidentally cancel a sibling's pending
/// form. Form answers are delivered by `form_id` — the UUID minted inside
/// [`LiveFormBridge::ask_form`] — and routed to the first bridge that owns that
/// form.
pub struct FormBridgeRegistry {
    inner: std::sync::Mutex<HashMap<String, Vec<Arc<LiveFormBridge>>>>,
}

impl FormBridgeRegistry {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Push `bridge` onto the set of bridges tracked for `agent_id`.
    pub fn register(&self, agent_id: &str, bridge: Arc<LiveFormBridge>) {
        self.inner
            .lock()
            .unwrap()
            .entry(agent_id.to_string())
            .or_default()
            .push(bridge);
    }

    /// Remove exactly `bridge` (by pointer identity) from the set tracked for
    /// `agent_id`. If no other bridges remain for the agent the key is dropped.
    pub fn deregister(&self, agent_id: &str, bridge: &Arc<LiveFormBridge>) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(vec) = guard.get_mut(agent_id) {
            vec.retain(|b| !Arc::ptr_eq(b, bridge));
            if vec.is_empty() {
                guard.remove(agent_id);
            }
        }
    }

    /// Number of bridges currently tracked for `agent_id`. Non-mutating,
    /// unlike [`Self::deliver`] (which consumes a matching pending answer as
    /// a side effect) — the right check for tests and observability code
    /// that just want to know whether anything is still registered.
    pub fn bridge_count(&self, agent_id: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .get(agent_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Deliver `response` to the first bridge under `agent_id` that owns
    /// `form_id`. Returns `Ok(())` on the first successful delivery.
    /// Returns `Err(`[`DeliverAnswerError::Unknown`]`)` if no bridge owns that
    /// form (session ended, already answered, or form_id never registered).
    ///
    /// The bridge Vec is cloned out before iterating so the mutex is not held
    /// across the delivery call, which wakes async tasks.
    pub fn deliver(
        &self,
        agent_id: &str,
        form_id: &str,
        response: FormResponse,
    ) -> Result<(), DeliverAnswerError> {
        let bridges = {
            let guard = self.inner.lock().unwrap();
            guard.get(agent_id).cloned().unwrap_or_default()
        };
        for bridge in &bridges {
            if bridge.deliver_form_answer(form_id, response.clone()).is_ok() {
                return Ok(());
            }
        }
        Err(DeliverAnswerError::Unknown)
    }
}

impl Default for FormBridgeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Payload the runner hands to the prompt bridge when a tool call needs
/// user confirmation.
///
/// The bridge typically renders `tool_name` plus a summary of `input`
/// and `reason` to the operator and waits for an answer. Identifiers
/// (`agent_id`, `session_id`) are forwarded so the UI can attribute the
/// prompt to a specific subagent / session and so the bridge can route
/// the answer back through whatever transport the surface uses.
#[derive(Debug, Clone)]
pub struct AskRequest {
    pub tool_name: String,
    pub input: Value,
    pub reason: String,
    pub agent_id: String,
    pub session_id: String,
}

/// User's answer to an [`AskRequest`].
///
/// Mirrors the allow-shaped variants of
/// [`PermissionDecision`](ao_engine_tools_core::PermissionDecision) but
/// drops `Mutate` (the user picks an action; mutating the tool input is
/// a hook responsibility) and the `reason` payload on `Deny` (the
/// runner attaches its own message when forming the
/// [`PermissionVerdict`](crate::permissions) — the bridge's job is the
/// verdict, not the wording).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskOutcome {
    Allow,
    AllowOnce,
    AllowSession,
    Deny,
}

/// Trait the runner uses to ask the operator about ambiguous tool calls.
///
/// Extends [`QuestionBridge`] so that the same bridge impl can handle both
/// permission prompts (`ask`) and structured multiple-choice questions
/// (`ask_question`, inherited from the supertrait). Implementations must be
/// `Send + Sync` so the gate can hold them behind `Arc<dyn UserPromptBridge>`
/// and dispatch from any task.
#[async_trait]
pub trait UserPromptBridge: QuestionBridge {
    /// Ask the operator whether a tool call should be allowed.
    async fn ask(&self, request: AskRequest) -> AskOutcome;
    // ask_question is inherited from QuestionBridge supertrait.
}

/// Bridge that always denies and returns `NoOperator` for questions.
/// The right pick for SDK / headless sessions where there is no operator.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubBridge;

#[async_trait]
impl QuestionBridge for StubBridge {
    async fn ask_question(
        &self,
        _request: QuestionRequest,
    ) -> Result<ChoiceId, AskQuestionError> {
        Err(AskQuestionError::NoOperator)
    }
}

#[async_trait]
impl UserPromptBridge for StubBridge {
    async fn ask(&self, _request: AskRequest) -> AskOutcome {
        AskOutcome::Deny
    }
}

#[async_trait]
impl FormBridge for StubBridge {
    async fn ask_form(&self, _: FormRequest) -> Result<FormResponse, AskQuestionError> {
        Err(AskQuestionError::NoOperator)
    }
}

/// Test bridge that replays a pre-recorded sequence of outcomes.
///
/// Each call to [`UserPromptBridge::ask`] pops the front of the script
/// and returns it. When the script is exhausted the bridge returns
/// [`AskOutcome::Deny`] (matching [`StubBridge`]'s behavior) so a test
/// that under-scripts fails closed instead of panicking inside the
/// gate. Tests that want to assert on exhaustion can read the
/// remaining-script length via [`ScriptedBridge::remaining`].
///
/// [`QuestionBridge::ask_question`] pops from a separate
/// `question_script`; exhaustion returns
/// `Err(`[`AskQuestionError::Cancelled`]`)`. Use
/// [`ScriptedBridge::with_question_script`] to seed the question script.
#[derive(Debug)]
pub struct ScriptedBridge {
    script: Mutex<VecDeque<AskOutcome>>,
    question_script: Mutex<VecDeque<Result<ChoiceId, AskQuestionError>>>,
}

impl ScriptedBridge {
    pub fn new(script: impl IntoIterator<Item = AskOutcome>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            question_script: Mutex::new(VecDeque::new()),
        }
    }

    /// Seed both the `ask` script and the `ask_question` script.
    ///
    /// The `question_script` items are returned in order by
    /// [`QuestionBridge::ask_question`]; exhaustion yields
    /// `Err(AskQuestionError::Cancelled)`. The `ask` script behaves
    /// identically to `ScriptedBridge::new`; it is required so the
    /// caller can share a single bridge across both call types if needed.
    pub fn with_question_script(
        question_script: Vec<Result<ChoiceId, AskQuestionError>>,
    ) -> Self {
        Self {
            script: Mutex::new(VecDeque::new()),
            question_script: Mutex::new(question_script.into_iter().collect()),
        }
    }

    /// Number of scripted `ask` answers still queued.
    pub fn remaining(&self) -> usize {
        self.script
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// Number of scripted `ask_question` answers still queued.
    pub fn question_remaining(&self) -> usize {
        self.question_script
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }
}

#[async_trait]
impl QuestionBridge for ScriptedBridge {
    async fn ask_question(
        &self,
        _request: QuestionRequest,
    ) -> Result<ChoiceId, AskQuestionError> {
        self.question_script
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .unwrap_or(Err(AskQuestionError::Cancelled))
    }
}

#[async_trait]
impl UserPromptBridge for ScriptedBridge {
    async fn ask(&self, _request: AskRequest) -> AskOutcome {
        self.script
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .unwrap_or(AskOutcome::Deny)
    }
}

#[async_trait]
impl FormBridge for ScriptedBridge {
    async fn ask_form(&self, _: FormRequest) -> Result<FormResponse, AskQuestionError> {
        Err(AskQuestionError::NoOperator)
    }
}

/// Error returned by [`LiveBridge::deliver_answer`].
#[derive(Debug, Error)]
pub enum DeliverAnswerError {
    #[error("unknown choice id — not registered or already answered")]
    Unknown,
}

/// Live bridge backed by per-question oneshot channels.
///
/// NOTE: with the retirement of the `AskUserQuestion` engine tool (replaced by
/// `AskUserQuestionWithForm`, which suspends through [`LiveFormBridge`]), this
/// bridge has no production wiring — it is exercised only by tests and is
/// scheduled for teardown together with the rest of the `ask_question`
/// surface.
///
/// `ask_question` mints a fresh [`ChoiceId`], registers a oneshot sender in
/// the channel map, emits a [`UserEvent::Question`] through the event sink,
/// then suspends until the paired receiver delivers an answer. The caller
/// (a WebSocket handler or a test peer task) resolves the future by calling
/// `deliver_answer`.
///
/// `cancel_pending` (from [`QuestionBridge`]) drains the channel map — each
/// dropped sender causes the corresponding `rx.await` to return
/// `Err(Cancelled)` immediately. Call this from the session-end cleanup hook
/// to avoid leaked tasks when the session is cancelled.
///
/// `ask` (from [`UserPromptBridge`]) always returns `AskOutcome::Deny`
/// because `LiveBridge` is question-only — permission prompts flow through
/// a separate channel in production and are not handled here.
pub struct LiveBridge {
    channels: Mutex<HashMap<ChoiceId, tokio::sync::oneshot::Sender<ChoiceId>>>,
    event_sink: Arc<dyn EventSink + Send + Sync>,
}

impl LiveBridge {
    pub fn new(event_sink: Arc<dyn EventSink + Send + Sync>) -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            event_sink,
        }
    }

    /// Deliver the operator's selected answer for a pending question.
    ///
    /// Removes the sender for `id` from the channel map and sends `answer`,
    /// resolving the awaiting `ask_question` future with `Ok(answer)`. Returns
    /// `Err(DeliverAnswerError::Unknown)` when `id` is absent (never
    /// registered, already answered, or already cancelled).
    pub fn deliver_answer(
        &self,
        id: &ChoiceId,
        answer: ChoiceId,
    ) -> Result<(), DeliverAnswerError> {
        match self.channels.lock().unwrap().remove(id) {
            Some(tx) => {
                let _ = tx.send(answer);
                Ok(())
            }
            None => Err(DeliverAnswerError::Unknown),
        }
    }

    /// Number of questions currently awaiting an answer.
    pub fn pending_count(&self) -> usize {
        self.channels.lock().unwrap().len()
    }
}

#[async_trait]
impl QuestionBridge for LiveBridge {
    async fn ask_question(
        &self,
        request: QuestionRequest,
    ) -> Result<ChoiceId, AskQuestionError> {
        let id = ChoiceId(uuid::Uuid::new_v4().to_string());
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.channels.lock().unwrap().insert(id.clone(), tx);
        self.event_sink
            .emit(UserEvent::Question {
                id: id.0.clone(),
                prompt: request.question,
                choices: request.choices.iter().map(|c| c.label.clone()).collect(),
            })
            .await
            .map_err(|_| AskQuestionError::NoOperator)?;
        rx.await.map_err(|_| AskQuestionError::Cancelled)
    }

    fn cancel_pending(&self) {
        // Drop all senders; each paired rx.await resolves to Err(Cancelled).
        self.channels.lock().unwrap().clear();
    }
}

#[async_trait]
impl UserPromptBridge for LiveBridge {
    /// `LiveBridge` is question-only — permission prompts always deny.
    async fn ask(&self, _request: AskRequest) -> AskOutcome {
        AskOutcome::Deny
    }
}

/// Terminal-shaped bridge for interactive CLIs. Prints the request to
/// stdout and reads a single line from stdin. Accepted answers (case-
/// insensitive, leading/trailing whitespace ignored):
///
/// - `y` / `yes` / `1` → [`AskOutcome::AllowOnce`]
/// - `s` / `session` / `2` → [`AskOutcome::AllowSession`]
/// - any other input → [`AskOutcome::Deny`]
///
/// Stdin is read inside `tokio::task::spawn_blocking` so a slow operator
/// doesn't park the runtime. A [`tokio::sync::Mutex`] serializes
/// concurrent prompts: when the executor batches several tool calls and
/// each one needs approval, the operator is prompted one at a time
/// instead of seeing interleaved questions on a single TTY.
///
/// `ask_question` ([`QuestionBridge::ask_question`]) renders the
/// numbered choice list to stdout and reads the operator's selection;
/// invalid input or EOF returns `Err(`[`AskQuestionError::Cancelled`]`)`.
#[derive(Debug, Default)]
pub struct StdinBridge {
    lock: tokio::sync::Mutex<()>,
}

impl StdinBridge {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Format a permission prompt for the operator. Pulled out so unit tests
/// can assert on the rendered shape without touching real stdio.
fn format_ask_prompt(request: &AskRequest) -> String {
    let input_pretty = serde_json::to_string_pretty(&request.input)
        .unwrap_or_else(|_| request.input.to_string());
    format!(
        "\n\
         [permission requested]\n  \
         tool:   {tool}\n  \
         reason: {reason}\n  \
         input:  {input}\n\
         [y]es / [s]ession / [n]o (default no): ",
        tool = request.tool_name,
        reason = request.reason,
        input = indent_after_first(&input_pretty, "          "),
    )
}

/// Prepend `indent` to every line of `text` after the first. Keeps the
/// "input:" header on its own line and aligns the JSON block under it.
fn indent_after_first(text: &str, indent: &str) -> String {
    let mut iter = text.lines();
    let first = iter.next().unwrap_or("").to_string();
    let rest: Vec<String> = iter.map(|l| format!("{indent}{l}")).collect();
    if rest.is_empty() {
        first
    } else {
        format!("{first}\n{}", rest.join("\n"))
    }
}

/// Map a single line of operator input to an `AskOutcome`. Invalid
/// answers (and empty lines) deny the request — interactive callers
/// should retry by re-prompting if they want lenient parsing, but the
/// runner's permission gate treats every `Deny` as terminal.
fn parse_ask_answer(line: &str) -> AskOutcome {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "1" => AskOutcome::AllowOnce,
        "s" | "session" | "2" => AskOutcome::AllowSession,
        _ => AskOutcome::Deny,
    }
}

#[async_trait]
impl QuestionBridge for StdinBridge {
    async fn ask_question(
        &self,
        request: QuestionRequest,
    ) -> Result<ChoiceId, AskQuestionError> {
        let _guard = self.lock.lock().await;
        let choices = request.choices.clone();
        let prompt = {
            let mut s = format!("\n[question] {}\n", request.question);
            for (i, choice) in choices.iter().enumerate() {
                s.push_str(&format!("  {}) {}\n", i + 1, choice.label));
            }
            s.push_str("select [1-");
            s.push_str(&choices.len().to_string());
            s.push_str("]: ");
            s
        };
        let line = tokio::task::spawn_blocking(move || -> Option<String> {
            use std::io::{self, BufRead, Write};
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(prompt.as_bytes()).ok()?;
            out.flush().ok()?;
            let stdin = io::stdin();
            let mut buf = String::new();
            match stdin.lock().read_line(&mut buf) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(buf),
            }
        })
        .await
        .ok()
        .flatten()
        .ok_or(AskQuestionError::Cancelled)?;

        let idx: usize = line
            .trim()
            .parse()
            .map_err(|_| AskQuestionError::Cancelled)?;
        if idx == 0 || idx > choices.len() {
            return Err(AskQuestionError::Cancelled);
        }
        Ok(choices[idx - 1].id.clone())
    }
}

#[async_trait]
impl UserPromptBridge for StdinBridge {
    async fn ask(&self, request: AskRequest) -> AskOutcome {
        let _guard = self.lock.lock().await;
        let prompt = format_ask_prompt(&request);
        let line = tokio::task::spawn_blocking(move || -> Option<String> {
            use std::io::{self, BufRead, Write};
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(prompt.as_bytes()).ok()?;
            out.flush().ok()?;
            let stdin = io::stdin();
            let mut buf = String::new();
            match stdin.lock().read_line(&mut buf) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(buf),
            }
        })
        .await
        .ok()
        .flatten();
        match line {
            Some(s) => parse_ask_answer(&s),
            None => AskOutcome::Deny,
        }
    }
}

#[async_trait]
impl FormBridge for StdinBridge {
    async fn ask_form(&self, _: FormRequest) -> Result<FormResponse, AskQuestionError> {
        Err(AskQuestionError::NoOperator)
    }
}

/// Snapshot/transcript context wired into a [`LiveFormBridge`] so `ask_form`
/// can persist a pending sync form into the same `pending_forms` structure the
/// async (`AskUserQuestionWithForm` mode="async") path uses — see
/// `ao_engine_tools_core::form_events::wire_posted_async_form` — tagged
/// `mode: "sync"` instead of `"async"`. Optional: a bridge built without
/// [`LiveFormBridge::with_persistence`] (every pre-existing call site) leaves
/// `ask_form` behaving exactly as it did before this feature.
#[derive(Clone)]
struct SyncFormPersistence {
    snapshot_store: Arc<SnapshotStore>,
    transcript_store: Arc<TranscriptStore>,
    /// Agent id, or `project_{id}` for a project-scoped run — same scoping
    /// convention the async path uses.
    scope_key: String,
    thread_id: Option<String>,
}

/// Guard that removes a sync form's `pending_forms` snapshot pointer exactly
/// once, covering every way [`LiveFormBridge::ask_form`] can end:
///
///   1. the answer arrives (`submit_form_answer` resolves the oneshot,
///      `rx.await` returns `Ok`) — `ask_form` calls [`Self::clear_now`] and
///      awaits it directly, so the pointer is gone before `ask_form` returns;
///   2. `cancel_pending` fires (drops the sender, `rx.await` returns `Err`) —
///      same explicit `clear_now` call, same guarantee;
///   3. the caller races `ctx.cancel` — or its own configured sync-form
///      deadline — against this call in a `tokio::select!` (see
///      `AskUserQuestionWithForm::invoke`'s `resolve_sync_form`) and drops
///      the whole `ask_form` future without ever polling it to completion,
///      so neither of the above lines ever runs — this guard is a local
///      held across the `rx.await` suspension point, so Rust drops it as
///      part of unwinding the future's state, and [`Drop::drop`] below
///      performs the same clear as a fallback.
///
/// Never touches the oneshot or the channel map — delivery stays exactly the
/// process-local oneshot round-trip it always was; this only cleans up the
/// UI-reconstruction pointer in the snapshot.
struct PendingFormClearGuard {
    snapshot_store: Arc<SnapshotStore>,
    scope_key: String,
    form_id: String,
    /// Set by [`Self::clear_now`] right before it awaits the clear, so
    /// `Drop` — which still runs afterward, since `clear_now` only borrows
    /// `self` for the duration of its own body — sees the work is already
    /// spoken for and skips its fallback spawn.
    resolved: bool,
}

impl PendingFormClearGuard {
    /// Await the clear directly instead of leaving it to the fallback spawn
    /// in `Drop`. Call this whenever `ask_form` reaches a normal return (the
    /// answer arrived or `cancel_pending` fired) so the pointer is gone
    /// before the caller ever sees the result.
    async fn clear_now(mut self) {
        self.resolved = true;
        if let Err(e) = self
            .snapshot_store
            .clear_pending_form(&self.scope_key, &self.form_id)
            .await
        {
            tracing::warn!(
                scope_key = %self.scope_key,
                form_id = %self.form_id,
                error = %e,
                "failed to clear pending sync form from snapshot"
            );
        }
    }
}

impl Drop for PendingFormClearGuard {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        // Fallback for the future-dropped-mid-await case (case 3 above):
        // `Drop::drop` cannot be async, so the clear runs on a spawned task;
        // failure just logs, matching the write side's best-effort policy.
        let snapshot_store = Arc::clone(&self.snapshot_store);
        let scope_key = std::mem::take(&mut self.scope_key);
        let form_id = std::mem::take(&mut self.form_id);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(e) = snapshot_store.clear_pending_form(&scope_key, &form_id).await {
                        tracing::warn!(
                            scope_key = %scope_key,
                            form_id = %form_id,
                            error = %e,
                            "failed to clear pending sync form from snapshot (fallback)"
                        );
                    }
                });
            }
            Err(_) => {
                tracing::warn!(
                    scope_key = %scope_key,
                    form_id = %form_id,
                    "no tokio runtime available to clear pending sync form from snapshot"
                );
            }
        }
    }
}

/// RAII guard marking a run as "suspended on a synchronous form" for its
/// lifetime. Increments the shared counter on construction and decrements it
/// exactly once on drop — covering every way [`LiveFormBridge::ask_form`] can
/// end: the answer arrives, `cancel_pending` drains the sender, or the caller
/// drops the whole `ask_form` future outright (e.g.
/// `AskUserQuestionWithForm::invoke`'s `resolve_sync_form` picks the
/// cancellation branch OR its own configured sync-form deadline branch
/// instead, and never polls this future to completion —
/// Rust still runs the destructors of everything held across that dropped
/// future's suspension point, including this guard). A no-op when the bridge
/// was built without [`LiveFormBridge::with_suspension_counter`]. Mirrors
/// [`PendingFormClearGuard`]'s single-cleanup shape above, minus the async
/// step — decrementing an atomic needs no `Drop`-can't-be-async workaround.
struct FormSuspensionGuard(Option<Arc<AtomicUsize>>);

impl FormSuspensionGuard {
    fn enter(counter: Option<&Arc<AtomicUsize>>) -> Self {
        if let Some(c) = counter {
            c.fetch_add(1, Ordering::Relaxed);
        }
        Self(counter.cloned())
    }
}

impl Drop for FormSuspensionGuard {
    fn drop(&mut self) {
        if let Some(c) = &self.0 {
            // Guard against underflow if enter/drop ever get out of sync.
            let _ = c.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                if v == 0 {
                    None
                } else {
                    Some(v - 1)
                }
            });
        }
    }
}

/// Live form bridge backed by per-form oneshot channels.
///
/// `ask_form` mints a UUID, registers a oneshot sender keyed by that id,
/// emits `UserEvent::FormRequest` through the event sink, then — when
/// [`Self::with_persistence`] has been called — persists the pending form
/// into the snapshot/transcript (see [`SyncFormPersistence`]) before
/// suspending until `deliver_form_answer` is called (typically from an HTTP
/// route handler). The persisted copy is for UI reconstruction only (a page
/// reload can rehydrate an answerable form); the broadcast event stays the
/// fast path, and delivery is always the oneshot, never the snapshot.
///
/// `cancel_pending` drains the channel map — each dropped sender causes the
/// corresponding `rx.await` to return `Err(Cancelled)`. Call this from the
/// session-end cleanup path to prevent leaked tasks when a run is cancelled
/// while a form is displayed.
pub struct LiveFormBridge {
    channels: Mutex<HashMap<String, tokio::sync::oneshot::Sender<FormResponse>>>,
    event_sink: Arc<dyn EventSink + Send + Sync>,
    /// `false` for a session with no interactive surface to render a form on
    /// (a channel-bridge thread — Telegram, Discord, Slack, ...). `ask_form`
    /// checks this before emitting a `FormRequest` or registering a oneshot,
    /// so such a session fails fast with `NoOperator` instead of suspending
    /// on an answer that can never be delivered.
    interactive: bool,
    /// See [`SyncFormPersistence`]. `None` until [`Self::with_persistence`] is
    /// called.
    persistence: Option<SyncFormPersistence>,
    /// Shared counter incremented for the duration of every outstanding
    /// `ask_form` call (see [`FormSuspensionGuard`]), so the process
    /// supervisor's overall wall-clock deadline can exclude time spent
    /// genuinely blocked on a human answer
    /// (`ao_process::supervisor::SpawnInput::form_suspended`). `None` for
    /// every pre-existing call site (native runner, tests) — `ask_form`
    /// behaves exactly as before when unset. Wired via
    /// [`Self::with_suspension_counter`].
    suspended: Option<Arc<AtomicUsize>>,
}

impl LiveFormBridge {
    pub fn new(event_sink: Arc<dyn EventSink + Send + Sync>) -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            event_sink,
            interactive: true,
            persistence: None,
            suspended: None,
        }
    }

    /// Construct a bridge for a session with no interactive UI — `ask_form`
    /// short-circuits to `Err(AskQuestionError::NoOperator)` immediately
    /// rather than emitting a `FormRequest` and awaiting an answer.
    pub fn new_non_interactive(event_sink: Arc<dyn EventSink + Send + Sync>) -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            event_sink,
            interactive: false,
            persistence: None,
            suspended: None,
        }
    }

    /// Wire snapshot/transcript persistence so `ask_form` records its pending
    /// form for UI reconstruction after a frontend reload (see
    /// [`SyncFormPersistence`]). Call before wrapping the bridge in `Arc`.
    /// Every constructor above defaults to `persistence: None`, so a caller
    /// that never invokes this keeps `ask_form`'s pre-existing behavior
    /// (event-only, no persistence) unchanged.
    pub fn with_persistence(
        mut self,
        snapshot_store: Arc<SnapshotStore>,
        transcript_store: Arc<TranscriptStore>,
        scope_key: String,
        thread_id: Option<String>,
    ) -> Self {
        self.persistence = Some(SyncFormPersistence {
            snapshot_store,
            transcript_store,
            scope_key,
            thread_id,
        });
        self
    }

    /// Wire a shared suspension counter so `ask_form` marks this bridge's
    /// outstanding calls as "suspended on a human form" for the process
    /// supervisor's overall wall-clock deadline — see
    /// `ao_process::supervisor::SpawnInput::form_suspended`. Call before
    /// wrapping the bridge in `Arc`. Every constructor above defaults to
    /// `suspended: None`, so a caller that never invokes this keeps
    /// `ask_form`'s pre-existing behavior unchanged.
    pub fn with_suspension_counter(mut self, counter: Arc<AtomicUsize>) -> Self {
        self.suspended = Some(counter);
        self
    }

    /// Deliver the operator's form answer to the waiting `ask_form` future.
    ///
    /// Removes the sender for `form_id` and sends `response`. Returns
    /// `Err(DeliverAnswerError::Unknown)` when `form_id` is absent (not registered,
    /// already answered, or session was cancelled).
    pub fn deliver_form_answer(
        &self,
        form_id: &str,
        response: FormResponse,
    ) -> Result<(), DeliverAnswerError> {
        match self.channels.lock().unwrap().remove(form_id) {
            Some(tx) => {
                let _ = tx.send(response);
                Ok(())
            }
            None => Err(DeliverAnswerError::Unknown),
        }
    }

    /// Number of forms currently awaiting an answer.
    pub fn pending_count(&self) -> usize {
        self.channels.lock().unwrap().len()
    }

    /// Best-effort write of the sync form's `pending_forms` pointer + its
    /// `form_request` transcript entry, mirroring the async path's
    /// persistence but tagged `mode: "sync"`. Returns `None` when no
    /// [`SyncFormPersistence`] was wired via [`Self::with_persistence`] —
    /// `ask_form` then has nothing to clean up either. Returns
    /// `Some(PendingFormClearGuard)` otherwise, which removes the pointer
    /// again on drop (see that type's doc comment for the exit paths it
    /// covers).
    async fn persist_pending(
        &self,
        id: &str,
        title: &str,
        intro: &Option<String>,
        fields: &[FormFieldPayload],
    ) -> Option<PendingFormClearGuard> {
        let p = self.persistence.as_ref()?;

        // Same wrapper shape as `FormRequestMeta.spec` on the async path:
        // the flat form definition (form_id/title/intro/fields), not the
        // outer `{form_id, spec, mode}` envelope — that envelope is
        // `FormRequestMeta` itself, built just below.
        let inner_spec = json!({
            "form_id": id,
            "title": title,
            "intro": intro,
            "fields": fields,
        });
        let meta = FormRequestMeta {
            form_id: id.to_string(),
            spec: inner_spec,
            mode: "sync".to_string(),
        };

        // `hidden_from_user: true` — sync forms have never rendered as their
        // own timeline entry (the composer overlay is their only UI); this
        // entry exists purely for `ask_user_question_form`-adjacent readers
        // to reconstruct pending state, not to be shown. It also keeps this
        // entry out of `is_pending_form_latest_in_thread`'s "last visible
        // entry" scan on `ao-server`, so it can never be mistaken for the
        // thing that superseded an unrelated async form on the same thread.
        let entry = form_request_entry(&p.scope_key, meta.clone(), true);
        if let Err(e) = p.transcript_store.append(&p.scope_key, &entry).await {
            tracing::warn!(
                scope_key = %p.scope_key,
                form_id = %id,
                error = %e,
                "failed to persist form_request transcript entry for sync form"
            );
        }

        // Upsert the pending-form pointer, keyed by thread. If this displaces
        // a still-pending form on the same thread — sync or async, either
        // can occupy the slot this posts into — leave a visible
        // `form_withdrawn` trace for it instead of letting it vanish
        // silently, mirroring the async path's own supersede handling (see
        // `ao_engine_tools_core::form_events::persist_posted_form`'s
        // `Ok(Some(replaced))` branch). The displaced form's own suspended
        // `ask_form` call (if it was sync) is NOT woken by this — its
        // `PendingFormClearGuard`/oneshot are untouched, so it keeps waiting
        // until answered, cancelled, or its own timeout elapses; this only
        // stops it from silently losing its UI trace the moment it's
        // pushed off the slot.
        let spec_value = serde_json::to_value(&meta).unwrap_or(Value::Null);
        match p
            .snapshot_store
            .set_pending_form(&p.scope_key, p.thread_id.clone(), id.to_string(), spec_value)
            .await
        {
            Ok(Some(replaced)) => {
                let withdrawn =
                    form_withdrawn_entry(&p.scope_key, &replaced.form_id, &replaced.spec);
                if let Err(e) = p.transcript_store.append(&p.scope_key, &withdrawn).await {
                    tracing::warn!(
                        scope_key = %p.scope_key,
                        form_id = %replaced.form_id,
                        error = %e,
                        "failed to persist form_withdrawn transcript entry for superseded sync form"
                    );
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    scope_key = %p.scope_key,
                    form_id = %id,
                    error = %e,
                    "failed to set pending_form on snapshot for sync form"
                );
            }
        }

        Some(PendingFormClearGuard {
            snapshot_store: Arc::clone(&p.snapshot_store),
            scope_key: p.scope_key.clone(),
            form_id: id.to_string(),
            resolved: false,
        })
    }
}

#[async_trait]
impl FormBridge for LiveFormBridge {
    async fn ask_form(&self, request: FormRequest) -> Result<FormResponse, AskQuestionError> {
        if !self.interactive {
            return Err(AskQuestionError::NoOperator);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.channels.lock().unwrap().insert(id.clone(), tx);
        // Suspension begins the instant the oneshot sender is registered —
        // from here until this function returns by ANY path (answered,
        // `cancel_pending`, or this whole future dropped by an outer
        // `tokio::select!`), a genuine human answer is outstanding. See
        // `FormSuspensionGuard` for the exit paths it covers.
        let _suspension_guard = FormSuspensionGuard::enter(self.suspended.as_ref());
        let fields = fields_to_payload(&request.fields);
        self.event_sink
            .emit(ao_engine_tools_core::UserEvent::FormRequest {
                id: id.clone(),
                agent_id: request.agent_id.clone(),
                session_id: request.session_id.clone(),
                title: request.title.clone(),
                intro: request.intro.clone(),
                fields: fields.clone(),
            })
            .await
            .map_err(|_| AskQuestionError::Cancelled)?;

        // Snapshot-only bookkeeping for UI reconstruction — never on the
        // delivery path. See `PendingFormClearGuard` for the exit paths it
        // covers; `clear_now` below handles the two paths reachable from
        // this function's own return, and its `Drop` fallback catches the
        // third (the caller dropping this whole future via `tokio::select!`).
        let clear_guard = self
            .persist_pending(&id, &request.title, &request.intro, &fields)
            .await;

        let result = rx.await.map_err(|_| AskQuestionError::Cancelled);
        if let Some(guard) = clear_guard {
            guard.clear_now().await;
        }
        result
    }

    fn cancel_pending(&self) {
        // Drop all senders; each paired rx.await resolves to Err(Cancelled).
        self.channels.lock().unwrap().clear();
    }
}

/// Convert the typed [`FormField`] list into wire-safe [`FormFieldPayload`] structs
/// for inclusion in the `UserEvent::FormRequest` payload. Delegates to the shared
/// `FormFieldPayload::from` conversion so the sync and async form paths stay in
/// lockstep.
fn fields_to_payload(fields: &[FormField]) -> Vec<FormFieldPayload> {
    fields.iter().map(FormFieldPayload::from).collect()
}

// ─── Live permission bridge ───────────────────────────────────────────────────

/// Form field id for the permission decision radio group.
const PERM_FIELD_DECISION: &str = "decision";
/// Option id for "allow this call".
const PERM_OPT_ALLOW: &str = "allow";
/// Option id for "allow all calls to this tool for the rest of the session".
const PERM_OPT_ALLOW_SESSION: &str = "allow_session";
/// Option id for "deny this call".
const PERM_OPT_DENY: &str = "deny";

/// Permission bridge that presents tool-approval prompts as interactive forms.
///
/// When a tool returns `Ask`, [`UserPromptBridge::ask`] emits a three-option
/// radio form through the existing form channel (the same path
/// `AskUserQuestionWithForm` uses). The operator's selection is mapped:
///
/// - "Allow" → [`AskOutcome::Allow`]
/// - "Allow for this session" → [`AskOutcome::AllowSession`]; the tool name is
///   added to an internal set so all subsequent `ask()` calls for that tool
///   return [`AskOutcome::Allow`] without showing another form.
/// - "Deny" → [`AskOutcome::Deny`]
///
/// `cancel` is raced against the form await so a cancelled session resolves
/// immediately with [`AskOutcome::Deny`] instead of hanging.
///
/// Wraps a [`LiveFormBridge`] so answers arrive through the same HTTP delivery
/// route used for `AskUserQuestionWithForm` — no additional infrastructure needed.
pub struct LivePermissionBridge {
    form_bridge: Arc<LiveFormBridge>,
    session_approved: Arc<Mutex<HashSet<String>>>,
    cancel: CancellationToken,
}

impl LivePermissionBridge {
    pub fn new(form_bridge: Arc<LiveFormBridge>, cancel: CancellationToken) -> Self {
        Self {
            form_bridge,
            session_approved: Arc::new(Mutex::new(HashSet::new())),
            cancel,
        }
    }

    fn map_response(&self, response: FormResponse, tool_name: &str) -> AskOutcome {
        let Some(answer) = response.answers.get(PERM_FIELD_DECISION) else {
            return AskOutcome::Deny;
        };
        match answer {
            FormAnswer::Selections(ids) => match ids.first().map(String::as_str) {
                Some(PERM_OPT_ALLOW) => AskOutcome::Allow,
                Some(PERM_OPT_ALLOW_SESSION) => {
                    self.session_approved
                        .lock()
                        .unwrap()
                        .insert(tool_name.to_string());
                    AskOutcome::AllowSession
                }
                _ => AskOutcome::Deny,
            },
            _ => AskOutcome::Deny,
        }
    }
}

#[async_trait]
impl QuestionBridge for LivePermissionBridge {
    async fn ask_question(
        &self,
        _request: QuestionRequest,
    ) -> Result<ChoiceId, AskQuestionError> {
        Err(AskQuestionError::NoOperator)
    }
}

#[async_trait]
impl UserPromptBridge for LivePermissionBridge {
    async fn ask(&self, request: AskRequest) -> AskOutcome {
        // Skip the form for tools the operator already approved this session.
        if self
            .session_approved
            .lock()
            .unwrap()
            .contains(&request.tool_name)
        {
            return AskOutcome::Allow;
        }

        let form_request = FormRequest {
            id: String::new(),
            agent_id: request.agent_id.clone(),
            session_id: request.session_id.clone(),
            title: format!("Approval required: {}", request.tool_name),
            intro: Some(request.reason.clone()),
            fields: vec![FormField {
                id: PERM_FIELD_DECISION.to_string(),
                kind: FormFieldKind::Radio {
                    options: vec![
                        FormOption {
                            id: PERM_OPT_ALLOW.to_string(),
                            label: "Allow".to_string(),
                            description: Some("Allow this tool call".to_string()),
                        },
                        FormOption {
                            id: PERM_OPT_ALLOW_SESSION.to_string(),
                            label: "Allow for this session".to_string(),
                            description: Some(
                                "Allow all calls to this tool for the rest of this session"
                                    .to_string(),
                            ),
                        },
                        FormOption {
                            id: PERM_OPT_DENY.to_string(),
                            label: "Deny".to_string(),
                            description: Some("Deny this tool call".to_string()),
                        },
                    ],
                },
                label: "Select an action".to_string(),
                description: None,
                required: true,
            }],
        };

        let result = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(AskQuestionError::Cancelled),
            r = self.form_bridge.ask_form(form_request) => r,
        };

        match result {
            Ok(response) => self.map_response(response, &request.tool_name),
            Err(_) => AskOutcome::Deny,
        }
    }
}

/// In-memory implementation of
/// [`DenialTracker`](ao_engine_tools_core::DenialTracker).
///
/// Counters are keyed by `(agent_id, tool_name)`; storage is a plain
/// `Mutex<HashMap>` so the type is `Send + Sync` without dragging in a
/// concurrent-map dependency. Each entry remembers which session
/// recorded the first denial so [`reset_session`] can drop only the
/// matching session's counters when a session ends.
///
/// [`reset_session`]: DenialTracker::reset_session
#[derive(Debug, Default)]
pub struct InMemoryDenialTracker {
    inner: Mutex<HashMap<(String, String), DenialEntry>>,
}

#[derive(Debug, Clone)]
struct DenialEntry {
    count: u32,
    session_id: String,
}

impl InMemoryDenialTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a denial that should be associated with `session_id`.
    ///
    /// The trait method [`DenialTracker::record_denial`] cannot carry
    /// session context (the signature is fixed on the foundation crate);
    /// the runner's permission gate calls this method instead so
    /// [`reset_session`](DenialTracker::reset_session) can target a
    /// specific session.
    pub fn record_in_session(&self, session_id: &str, agent_id: &str, tool_name: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let entry = guard
            .entry((agent_id.to_string(), tool_name.to_string()))
            .or_insert_with(|| DenialEntry {
                count: 0,
                session_id: session_id.to_string(),
            });
        entry.count = entry.count.saturating_add(1);
    }
}

impl DenialTracker for InMemoryDenialTracker {
    fn record_denial(&self, agent_id: &str, tool_name: &str) {
        self.record_in_session("", agent_id, tool_name);
    }

    fn count(&self, agent_id: &str, tool_name: &str) -> u32 {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&(agent_id.to_string(), tool_name.to_string()))
            .map(|e| e.count)
            .unwrap_or(0)
    }

    fn reset_session(&self, session_id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, entry| entry.session_id != session_id);
    }
}

#[cfg(test)]
mod tests;
