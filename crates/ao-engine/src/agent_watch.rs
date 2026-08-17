//! Tier 2 of the Assignment Trigger detection ladder: the
//! agent-driven watch. Unlike `ConnectorEvent`'s deterministic single-cursor
//! poll, a watch has no one tool/field to compare — the assignment's own
//! agent decides what's out there, by whatever means its instruction implies
//! (fidelity over cost: the assignment's own agent/model, not
//! a cheap classifier).
//!
//! What must stay deterministic and code-owned is the actual
//! new-vs-already-seen judgment — dedup correctness can never be delegated
//! to the model. So [`AgentWatchDetector`] is deliberately scoped to
//! *observation only* ("here's what's out there right now"); it never sees
//! or touches the scratchpad. [`run_agent_watch_tick`] — plain code, no
//! model in this half of the loop — is what diffs those observations
//! against the durable `state_scratchpad`
//! (`ao_persistence::PersistenceLayer::assignment_scratchpads`, already
//! shipped and used today for push delivery-id dedup) and decides what
//! actually counts as new.
//!
//! Ordering is fire-then-persist: a genuine finding calls
//! `fire_assignment` first and only advances the scratchpad afterward, so a
//! crash between the two re-fires the same finding next poll instead of
//! silently losing it.
//!
//! What "advances the scratchpad" commits is deliberately weaker than
//! "delivered," though: `fire_assignment` returning `Ok` only means the
//! message reached the target agent's queue
//! (`NotificationDispatcher::submit_to_agent` is a bare channel send), not
//! that the queued turn ever ran. Recording that as unconditionally
//! delivered would open its own silent-drop path — a crash (or a queued-run
//! failure) between a successful enqueue and the turn actually executing
//! would permanently mark a finding "seen" that the agent never acted on.
//! So every to-fire item's ledger entry is written `Pending`
//! (`AssignmentScratchpad::record_pending_action`) *before* dispatch is even
//! attempted — a crash between match and dispatch still leaves a durable
//! trace instead of silently discarding the finding — and is promoted to
//! `Confirmed` only once its dispatched run (attached via
//! `AssignmentScratchpad::attach_dispatch_run` once `fire_assignment`
//! actually returns) is independently observed to reach a terminal status
//! (`reconcile_pending_deliveries`, run at the top of every contract-bound
//! tick). A dispatch call that fails outright leaves its entries `Pending`
//! with no `run_id` at all — the same "stuck" shape reconciliation already
//! has to handle for a silent post-enqueue crash — and immediately surfaces
//! an unhealthy health event naming the real failure reason, per this
//! codebase's standing rule that anything the engine detects the user must
//! see. An entry that stays `Pending` for more than
//! `PENDING_DELIVERY_RETRY_POLL_THRESHOLD` consecutive reconciliation passes
//! is deliberately retried rather than left silently stuck forever: its
//! ledger entry and item snapshot are both cleared, which makes the item
//! retry-eligible on the next poll that still observes it as matching, and a
//! second unhealthy health event discloses that the retry happened and why.
//! This accepts a narrow, disclosed risk of duplicating an action whose
//! original dispatch actually succeeded but was never confirmed, in exchange
//! for never leaving a detected finding invisible and undelivered
//! indefinitely — the trade this codebase's "no silent degradation" rule
//! calls for.
//!
//! [`LiveAgentWatchDetector`] is the production [`AgentWatchDetector`]: it
//! runs the assignment's own agent (fidelity over cost) as a
//! bounded, non-persisted child session and asks it to observe, not act.
//! Its two `runner_mode`s take genuinely different routes, because only one
//! of them can actually reach live MCP tools:
//!
//! - **`AgentRunnerMode::Api`** — driven in-process via
//!   `ao_engine_tools_runner::query_loop::run_session`, the same way
//!   `InspectionVerifier` (`verification::inspection`) drives it for
//!   `ProjectVerify`'s full mode: a filtered `RunnerContext` (the process's
//!   real tool registry, `AppState::tools_registry`, narrowed to
//!   `mcp__`-qualified tools via `Registry::filter_for` — filesystem and
//!   mutation tools are deliberately excluded since a watch only observes),
//!   a hard `max_turns` cap, and a wall-clock timeout via
//!   `tokio::time::timeout`. The provider comes from the same
//!   `provider_client_for_profile` seam `build_prompt_refine_provider`/
//!   `build_reflection_provider`/`build_quick_verification_engine` already
//!   use (`ao_engine::lib`). This path was already structurally correct and
//!   is unchanged by the CLI fix below.
//! - **`AgentRunnerMode::Cli`** — a CLI-mode agent's `ProviderClient` is
//!   `CliProviderClient` (`verification_cli_provider`), a tool-less one-shot
//!   shell-out shared with the verification/summarization/reflection
//!   passes; driving it through `run_session` like the `Api` path would
//!   give the watch's child session zero tools, so it could never actually
//!   observe anything. Instead this path dispatches through
//!   `agent_runner::RunnerDispatcher` → `CliAgentRunner::run`, the exact
//!   entry point `agent_runner::ProfileAwareChildRunner` (the Delegate/Task
//!   subagent spawn path) uses — a real CLI process spawn with
//!   `--mcp-config` wired, so the child gets the assignment's own agent's
//!   *full configured tool surface*, not a scoped registry. The run is
//!   isolated the same way a delegate child is (`isolate_history: true`, a
//!   sidechain transcript file, a dedicated event channel) so a poll never
//!   pollutes the agent's real chat, and it sets
//!   `AgentRunRequest::bypass_instance_cap` so a poll can never be
//!   blocked/queued behind — or itself block — the agent's own live turn.
//!
//! Both paths converge on the same **structured-output contract**: the
//! child is asked to reply with a JSON array; parsing tolerates a fenced
//! code block or light prose wrapping via the same "fenced, then raw, then
//! first balanced substring" strategy `ao_engine_tools_runner::reflection`/
//! `verification` already use for their own JSON replies (reimplemented
//! locally below since those helpers aren't exported for cross-crate
//! reuse).
//!
//! What must stay deterministic and code-owned is unchanged:
//! [`LiveAgentWatchDetector::observe`] only reports candidates, exactly like
//! any other [`AgentWatchDetector`] impl — it never sees or touches the
//! scratchpad, and the new-vs-seen diff always happens in
//! [`run_agent_watch_tick`] below.
//!
//! # Locked policy: a watch never fires from history
//!
//! A watch reports only items that *begin* matching after it starts
//! observing. Items already matching when the baseline was recorded are
//! seeded into the scratchpad and deliberately excluded from firing — the
//! `seed_only` branch in `run_contract_bound_tick`, pinned by the
//! `first_poll_seeds_baseline_without_firing` test.
//!
//! This is a product decision, not an implementation shortcut, and it reads
//! like a bug from the outside. Please don't "fix" it without displacing the
//! reasoning below first.
//!
//! Firing is not a read. An assignment's action has irreversible external
//! side effects — it emails a client, posts a message, files a ticket. A
//! watch bound over a thirty-row table that fired on its backlog would send
//! thirty real messages to real people on its first tick, and nothing takes
//! those back. A missed item is recoverable by hand; a sent one is not. That
//! asymmetry is resolved in favour of silence at the boundary, every time.
//!
//! The known cost is invisibility: a user who points a watch at already-
//! matching items sees a healthy watch that never fires, and can't tell that
//! apart from a broken one. **The fix for that is disclosure, not firing.**
//! `seed_baseline_disclosure_message` exists exactly so the exclusion is
//! stated out loud — it names what was skipped and tells the user to handle
//! those by hand. If this behaviour looks wrong again, make the disclosure
//! louder; do not make the seeding tick fire.
//!
//! The rule follows from the side effects, so it belongs to *this*
//! assignment type rather than being a global invariant. A future assignment
//! type whose action is genuinely side-effect-free (a pure read, an
//! idempotent refresh) could safely fire on its backlog — but that is a new
//! per-type decision needing its own justification, not licence to relax the
//! rule here.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use chrono::Utc;
use regex::Regex;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use ao_engine_tools_core::{
    output::ToolOutput, NoopDenialTracker, PermissionMode, Registry, RunnerContext, SessionKind,
};
use ao_engine_tools_runner::hooks::config::RunnerSettings;
use ao_engine_tools_runner::mcp::payload_stash;
use ao_engine_tools_runner::message::{ContentBlock, Message};
use ao_engine_tools_runner::prompt_bridge::StubBridge;
use ao_engine_tools_runner::provider::ProviderClient;
use ao_engine_tools_runner::query_loop::{run_session, RunnerConfig};
use ao_persistence::PersistenceLayer;
use ao_protocol::agent::{AgentProfile, AgentRunnerMode};
use ao_protocol::assignment::{
    Assignment, AssignmentRunStatus, AssignmentTrigger, AssignmentTriggerKind, TriggerEventContext,
};
use ao_protocol::assignment_scratchpad::{
    delivery_key, AssignmentScratchpad, DeliveryStatus, ExtractionHealth, ExtractionPath, ItemSnapshot, SNAPSHOT_CAP,
};
use ao_protocol::contract_primitives::{canonical_json, normalize_value_for_identity, sha256_hex};
use ao_protocol::event::{AgentEventPayload, SystemMessageSeverity};
use ao_protocol::extractor_contract::{self, ExtractionPlan, ExtractorKind, Tier};
use ao_protocol::watch_contract::{
    evaluate_predicate, identity_key, version_key, ChangeSpec, ContractError, FieldSpec, IdentitySpec,
    IdentityStrategy, PredicateSpec, WatchContract, WatchMode, WatchSource, IDENTITY_KEYGEN_VERSION,
};

use crate::agent_runner::{AgentRunRequest, RunScope, RunnerDispatcher};
use crate::assignment_runner::fire_assignment;
use crate::event_bus::EventBus;
use crate::queue_manager::NotificationDispatcher;

/// Function signature used to resolve a live [`ProviderClient`] for an
/// [`AgentProfile`]. Production wiring ([`LiveAgentWatchDetector::new`])
/// points this at `crate::provider_client_for_profile` — the same
/// provider-resolution seam every other one-shot model pass in this crate
/// uses (see the module doc). Tests inject a scripted resolver instead, so
/// [`LiveAgentWatchDetector::observe`] can be exercised against a
/// `MockProviderClient` without touching `providers.toml` or shelling out to
/// a real CLI binary.
type ProviderResolver = Arc<dyn Fn(&AgentProfile) -> Option<Arc<dyn ProviderClient>> + Send + Sync>;

/// Maximum number of provider turns [`LiveAgentWatchDetector`]'s child
/// session may execute. Kept low — a watch poll is meant to be a quick
/// "check and report," not an open-ended investigation, and every turn here
/// pays full model price. Mirrors the spirit of
/// `verification::inspection::INSPECTION_TURN_CAP`, scaled down since a
/// watch has a much narrower job than a full code inspection.
const AGENT_WATCH_TURN_CAP: usize = 8;

/// Wall-clock timeout for one [`LiveAgentWatchDetector::observe`] call, in
/// seconds. If the child hasn't finished by then it's cancelled and the poll
/// reports a [`AgentWatchDetectError::Failed`] — the next scheduled poll
/// retries, so there is no need to salvage a partial result here.
const AGENT_WATCH_TIMEOUT_SECS: u64 = 120;

/// Upper bound on how many item ids [`run_agent_watch_tick`] remembers per
/// assignment. Mirrors the "producer owns the cap" division of labor
/// `AssignmentScratchpad::seen_ids`'s own doc comment already describes —
/// old ids are dropped oldest-first once a poll would push the set past
/// this, so a chatty watch can't grow the scratchpad file forever.
const SEEN_IDS_CAP: usize = 200;

/// Default N for the amendment trigger: the number of
/// consecutive contract-bound polls with at least one candidate missing a
/// `required: true` field before authoring is re-run. Deliberately not 1 —
/// a single blip (a transient extraction miss) must not be enough to
/// re-litigate a working contract; only a *repeated* failure earns that.
const REQUIRED_FIELD_FAILURE_AMENDMENT_THRESHOLD: u32 = 2;

/// Cap on how many times [`run_contract_bound_tick`] will auto-clear a
/// contract for re-authoring (`AssignmentScratchpad::contract_amendment_cycle_count`)
/// before it stops and leaves the assignment visibly unhealthy instead.
/// Without this, a contract whose re-authored replacement keeps failing the
/// same required-field check amends forever: clear the contract (this
/// changes its fingerprint), which forces the next contract-bound tick's
/// "fingerprint changed — re-seed, don't fire" branch, which itself proves
/// nothing about whether the new contract actually fixed the extraction
/// problem — amend, reseed, amend, reseed, with no exit and no fire, ever.
/// Kept low (unlike [`AUTHORING_FAILURE_CEILING`]'s 5): each cycle here
/// already costs [`REQUIRED_FIELD_FAILURE_AMENDMENT_THRESHOLD`] consecutive
/// failing polls plus a full re-authoring attempt, so a small number is
/// enough to tell "legitimately needed a couple of re-authors as the
/// source's shape settled" apart from "stuck oscillating."
const CONTRACT_AMENDMENT_CYCLE_CEILING: u32 = 3;

/// Cap on authoring polls [`run_authoring_attempts`] will make within a
/// single tick: one initial attempt, plus at most one same-tick repair
/// attempt when the first attempt's rejection was specifically one
/// [`RepairContext`] knows how to describe precisely enough to hand straight
/// back to the model and expect a better answer — an unparseable
/// `predicate.expr` (a parser position, an unknown function name) or an
/// `identity.fields`/`change.material_fields` overlap the model proposed
/// directly (the exact field names that collided). Every other rejection
/// reason waits for the next scheduled poll instead of spending a second
/// model call here.
const MAX_AUTHORING_ATTEMPTS_PER_TICK: u32 = 2;

/// Consecutive authoring-mode polls (`AssignmentScratchpad::authoring_failure_streak`)
/// that may end without a bound contract before [`run_authoring_and_legacy_tick`]
/// gives up asking the model to propose one. A watch stuck re-proposing the
/// same broken shape from a static instruction will never fix itself by
/// sampling variance alone, so this bounds the cost instead of burning a
/// model call every poll interval forever — once hit, the assignment is
/// surfaced as unhealthy (see the health event in
/// [`run_authoring_and_legacy_tick`]) and later polls fall back to a plain
/// observation so the legacy `seen_ids` diff keeps working.
const AUTHORING_FAILURE_CEILING: u32 = 5;

/// Number of consecutive [`reconcile_pending_deliveries`] passes a two-phase
/// delivery ledger entry may be observed still [`DeliveryStatus::Pending`],
/// with its dispatched run not yet reached a terminal status (or with no
/// dispatched run at all, for an entry whose initial dispatch attempt failed
/// outright), before it is treated as stuck: retried and surfaced as
/// unhealthy rather than waited on silently forever. `reconcile_pending_deliveries`
/// runs once at the top of every contract-bound tick, so this is also a
/// bound on wall-clock time in terms of the assignment's own poll interval.
/// Kept small (3) rather than the many-poll ceilings this module uses
/// elsewhere (e.g. [`AUTHORING_FAILURE_CEILING`]'s 5): those bound a
/// re-*prompting* cost (a wasted model call is cheap to repeat), while this
/// bounds how long a *detected, matched* item can stay invisibly undelivered
/// — under ordinary conditions a queued turn reaches a terminal status well
/// within a single poll interval, so three full poll intervals of silence is
/// already generous room for ordinary latency (a queue backlog, a slow model
/// call) before treating it as the crash/failure this module exists to catch.
const PENDING_DELIVERY_RETRY_POLL_THRESHOLD: u32 = 3;

/// Cap on how many already-matching candidates [`run_contract_bound_tick`]
/// names by summary in the seed-baseline disclosure health event. A watch
/// seeded against a large existing backlog (a 500-row table) must not turn
/// that one message into a wall of text — this bounds the listed names,
/// with the remainder folded into an "...and N more" tail.
const SEED_DISCLOSURE_MAX_NAMED: usize = 5;

/// Length each candidate summary is truncated to (in `char`s) before being
/// listed in the seed-baseline disclosure — long summaries (a full email
/// subject, a long ticket title) would otherwise dominate the message.
const SEED_DISCLOSURE_SUMMARY_CHARS: usize = 120;

/// One item an [`AgentWatchDetector`] observed while polling. Carries enough
/// for [`run_agent_watch_tick`]'s code-owned diff to decide whether it's new,
/// and to become a [`TriggerEventContext`] if it is.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentWatchCandidate {
    /// Reference tag distinguishing candidates within one poll's reply —
    /// display/logging label ONLY, never identity. Neither dedup path
    /// trusts it: contract-bound watches hash `payload` per
    /// `contract.identity`/`contract.change`, and
    /// [`run_agent_watch_tick`]'s legacy fallback (an assignment
    /// with no bound [`WatchContract`] yet) hashes `payload`/`summary`
    /// content via [`legacy_candidate_key`] instead of diffing on this field.
    /// A detector is free to mint a different `id` for the same physical
    /// item on every single poll — that free-text drift is exactly the bug
    /// both dedup paths exist to be immune to.
    pub id: String,
    /// Token-able one-line description; becomes
    /// [`TriggerEventContext::summary`] when this candidate is new.
    pub summary: String,
    /// Raw structured data behind `summary`; becomes
    /// [`TriggerEventContext::payload`] when this candidate is new.
    pub payload: serde_json::Value,
}

/// One authoring-mode poll's raw reply: the baseline
/// candidate array — used exactly like any other first poll's baseline,
/// never to fire on its own — plus whatever contract proposal object the
/// child reported alongside it, not yet parsed into a [`WatchContract`] or
/// validated. `proposed_contract` is `None` both for a detector that has no
/// authoring-specific support (the default [`AgentWatchDetector::observe_for_authoring`]
/// impl, which every test fake gets for free) and for one that does support
/// it but whose child simply didn't include a `contract` key this poll —
/// [`run_agent_watch_tick`] treats both the same way: nothing to validate
/// this time, retry authoring next poll.
#[derive(Debug, Clone, Default)]
pub struct AuthoringReply {
    pub candidates: Vec<AgentWatchCandidate>,
    pub proposed_contract: Option<Value>,
}

/// Corrective context [`build_authoring_prompt`] folds into the next
/// authoring attempt so the model sees precisely what went wrong instead of
/// guessing blind from the same instruction again, extended by the repair
/// loop below. Note what is deliberately absent: a
/// `required`+`NotEmpty` contradiction on the same field
/// (`ContractError::RequiredFieldTargetedByTolerantPredicate`) never reaches
/// this type at all — [`auto_repair_contract`] fixes it in code before a
/// rejection is ever constructed, since the fix (drop `required`) is always
/// the same one, never a real choice for the model to make.
///
/// [`CrossPollRejection`](Self::CrossPollRejection) is the only variant that
/// crosses polls on its own; every other variant is same-tick only —
/// [`run_authoring_attempts`] builds one from a rejection and hands it to a
/// second [`AgentWatchDetector::observe_for_authoring`] call within the same
/// poll. [`Accumulated`](Self::Accumulated) is how more than one of the
/// above reaches the model at once: every distinct rejection outstanding for
/// the current authoring streak, framed as simultaneous constraints that
/// must ALL hold — not a single latest complaint that erases whatever came
/// before it. That framing is the fix for a two-constraint oscillation: a
/// proposal that satisfies the newest rejection while reintroducing an
/// earlier one is shown both at once, so it can no longer look like a valid
/// fix.
#[derive(Debug, Clone, PartialEq)]
pub enum RepairContext {
    /// `predicate.expr` failed to parse — the exact rejected expression and
    /// the parser's own error for it, verbatim.
    InvalidPredicate { rejected_expr: String, error: String },
    /// The proposal's own `identity.fields` and `change.material_fields`
    /// shared at least one field, caught by `WatchContract::validate`'s own
    /// disjointness check. Unlike the rung-drop's version of this failure
    /// (which the engine's own constructor must never produce — see
    /// [`composite_fallback_fields`]), this is the model proposing an
    /// overlapping composite or content-hash identity directly, which is
    /// repairable the same way: name the exact fields and ask for a
    /// non-overlapping set.
    IdentityMaterialFieldOverlap { fields: Vec<String> },
    /// `change.material_fields` was empty on a proposal whose `mode` wasn't
    /// `new_only` (`WatchContract::validate`'s `EmptyMaterialFields`
    /// rejection). Mechanical to fix either way: declare at least one
    /// material field, or switch `mode` to `new_only` if this watch should
    /// really fire on an item's mere existence rather than a field of it
    /// changing — precise enough to hand straight back to the model instead
    /// of waiting a whole poll.
    EmptyMaterialFields,
    /// The reply's `contract` object didn't deserialize into
    /// [`ContractProposal`] at all (`ProposalRejection::Malformed`) — a
    /// wrong type, a value outside a closed enum, or (before the `change`
    /// field was made conditionally required) an omitted key the shape
    /// still needed. `reason` is `serde`'s own error text, verbatim. Worth
    /// one same-tick retry: this is almost always a small, nameable shape
    /// mistake the model can see and fix immediately, rather than a
    /// judgment call worth waiting a whole poll to revisit.
    Malformed { reason: String },
    /// An earlier POLL's proposal for this watch was rejected — persisted on
    /// `AssignmentScratchpad::authoring_rejection_history` since the
    /// `repair` local variable in [`run_authoring_attempts`] only ever lives
    /// for one poll's attempts, and a rejected proposal is otherwise
    /// forgotten the moment that poll ends. Seeded as part of this poll's
    /// initial accumulated repair set so the model sees what an earlier
    /// poll's attempt got wrong before trying again, alongside anything this
    /// poll's own attempts add to it.
    CrossPollRejection { reason: String },
    /// More than one distinct rejection is outstanding for the current
    /// authoring streak — every entry must be satisfied AT ONCE by the next
    /// proposal. Never nested: an entry here is never itself `Accumulated`.
    /// See this type's own doc for why accumulating (rather than replacing)
    /// is the fix for a multi-constraint oscillation.
    Accumulated(Vec<RepairContext>),
}

/// Why an [`AgentWatchDetector::observe`] call didn't return candidates.
#[derive(Debug, thiserror::Error)]
pub enum AgentWatchDetectError {
    /// The detector attempted a real observation (e.g. called MCP tools or
    /// ran the assignment's agent) and that attempt itself failed — covers
    /// everything from "no provider configured for this agent" through a
    /// session error, a timeout, or a reply that couldn't be parsed as the
    /// structured-findings contract.
    #[error("agent-watch detection failed: {0}")]
    Failed(String),
}

/// Observes the current state of the world for one `AgentWatch` trigger.
/// Deliberately scoped to *observation only* — see the module doc for why
/// the new-vs-seen judgment is never delegated to an implementation of this
/// trait — that judgment must stay code-owned and deterministic.
#[async_trait]
pub trait AgentWatchDetector: Send + Sync {
    /// Returns every candidate item currently visible to the watch,
    /// regardless of whether it's been seen before — [`run_agent_watch_tick`]
    /// does the seen/unseen split. Order is a hint only (implementations
    /// SHOULD put the most-recent/most-relevant candidate first), since a
    /// tick that finds more than one new item still fires exactly once,
    /// summarizing all of them together (see [`build_event_context`]).
    async fn observe(
        &self,
        assignment: &Assignment,
        instruction: &str,
    ) -> Result<Vec<AgentWatchCandidate>, AgentWatchDetectError>;

    /// Authoring-mode poll: like [`Self::observe`], but
    /// also captures whatever contract proposal the child reported alongside
    /// its candidates in the same reply. `repair` is `Some` only on a
    /// same-tick retry after a first attempt's `predicate.expr` was rejected
    /// (see [`RepairContext`], [`run_authoring_attempts`]) — implementations
    /// that support it fold it into the prompt so the model sees what it got
    /// wrong; every other implementation is free to ignore it. The default
    /// implementation simply delegates to [`Self::observe`], ignores
    /// `repair`, and reports no proposal — correct for any implementation
    /// (including every test fake) that has no authoring-specific behavior,
    /// and it means adding this method needed no changes anywhere
    /// [`Self::observe`] was already implemented or scripted.
    /// [`LiveAgentWatchDetector`] is the only override.
    async fn observe_for_authoring(
        &self,
        assignment: &Assignment,
        instruction: &str,
        _repair: Option<&RepairContext>,
    ) -> Result<AuthoringReply, AgentWatchDetectError> {
        Ok(AuthoringReply { candidates: self.observe(assignment, instruction).await?, proposed_contract: None })
    }
}

/// System prompt for [`LiveAgentWatchDetector`]'s child session. Instructs
/// the model to observe only — the new-vs-seen decision and the
/// actual notification stay entirely in code, outside this session.
///
/// This is deliberately mode-agnostic: it describes the baseline candidate
/// shape used to seed/observe a watch, not "how to decide what counts as
/// the same item," which is a per-watch [`WatchContract`] question that
/// [`build_watch_prompt`] injects into the user turn instead. Earlier revisions of this
/// prompt also told the model its `id` "MUST" be stable across polls — that
/// was the model re-deciding identity on every single poll, which is
/// precisely the bug the contract now exists to fix, so that instruction is
/// gone rather than merely softened.
///
/// It also spells out the reply's error channel (see [`WatchObservation`]):
/// without it, a child whose tool call fails mid-turn has no way to say so
/// and can only reply `[]` — indistinguishable from a source that genuinely
/// had nothing — or invent findings. Both are worse than reporting the
/// failure honestly, so this gives the model two dedicated object shapes for
/// exactly that, spelled out with worked examples rather than left to the
/// model's own judgment about how to signal a failure.
const AGENT_WATCH_SYSTEM_PROMPT: &str = "\
You are the observation half of an agent-driven watch. A separate, \
deterministic system will compare whatever you report against what it has \
already seen and decides on its own whether anything is genuinely new — so \
report every candidate item you find, even ones you saw last time.

Do not take any action beyond observing: do not send a message, reply to \
anything, or otherwise act on what you find. Use whatever tools are \
available to you to check the current state of the thing you were asked to \
watch, then stop.

When you are done, respond with ONLY a JSON array (an empty array `[]` if \
you found nothing) — no prose before or after — in this exact shape:

[
  {\"id\": \"...\", \"summary\": \"...\", \"payload\": { }}
]

- `id`: a short reference tag for this item (e.g. a message/issue/file id), \
  used only to tell the items in this reply apart from each other. It is \
  not a dedup key and nothing downstream relies on it being the same string \
  next time you look at the same item.
- `summary`: a short, one-line human-readable description of the item.
- `payload`: whatever structured data about the item is useful downstream \
  (sender, subject, url, whatever fields are relevant) — any valid JSON.

An empty array `[]` means exactly one thing: you checked, and the source \
genuinely has nothing to report right now. It never means anything else — \
above all, never reply `[]` because a tool call failed, timed out, or \
returned something you could not use, and never invent an item to fill the \
gap instead. If something like that happens, respond with a JSON OBJECT \
instead of an array, in one of these two shapes:

- A tool call you made while checking failed: \
`{\"status\": \"tool_error\", \"tool\": \"<name of the tool call that \
failed>\", \"detail\": \"<what it returned, or its error message, \
verbatim>\"}`
- You could not complete this observation for some other reason: \
`{\"status\": \"failed\", \"reason\": \"<why, in your own words>\"}`

The instructions below may extend or override the above for this \
particular watch — follow them exactly.";

/// Plain-JSON sketch of the contract-proposal envelope [`build_watch_prompt`]
/// asks for on a watch's first (authoring) run. Kept as its own constant,
/// outside any `format!` literal, so its braces don't need escaping.
const CONTRACT_PROPOSAL_SHAPE: &str = "\
{
  \"status\": \"ok\",
  \"candidates\": [ /* same array shape described above */ ],
  \"contract\": {
    \"source\": { \"kind\": \"...\", \"ref\": \"...\" },
    \"identity\": {
      \"strategy\": \"native_id\" | \"composite_native\" | \"content_hash\",
      \"source_field\": \"...\",   // native_id only — the exact field name this source itself uses as a stable per-item key
      \"format\": \"...\",         // native_id only — a regex derived from the values you actually observed
      \"fields\": [\"...\"],       // composite_native / content_hash only — the fields combined to form identity
      \"rationale\": \"...\"       // required — plain language explaining what you chose and why
    },
    \"mode\": \"predicate_transition\" | \"new_or_changed\" | \"new_only\", // optional, defaults to predicate_transition — see below for what each means
    \"change\": { \"material_fields\": [\"...\"] }, // required unless mode is new_only
    \"predicate\": { \"natural_language\": \"...\", \"fields\": [\"...\"], \"expr\": \"...\" },
    \"fields\": { \"<field_name>\": { \"type\": \"string\", \"required\": true } },
    \"tool_used\": \"...\",        // optional — see the tool self-report instructions below
    \"arguments_used\": { }       // optional — only alongside tool_used
  }
}";

/// The closed grammar `predicate.expr` must be written in, spelled out for
/// the authoring model. `ao_protocol::watch_contract::legacy_expr` (the
/// module that converts this grammar into the typed `Predicate` every
/// contract actually stores — see its doc) accepts exactly six prefix-call
/// forms and nothing else — no infix
/// operators, no field-path syntax, no numeric/boolean literals — so without
/// this the model has no way to know it isn't free to write Python, jq, SQL,
/// or plain English into `expr`.
const PREDICATE_GRAMMAR: &str = "\
`predicate.expr` is a small, closed language — exactly six function forms, \
each written as a prefix call, nothing else is valid:\n\n\
- `not_empty(field)` — true if `field` is present and non-empty\n\
- `contains(field, 'literal')` — true if `field`'s value contains `literal` as a substring\n\
- `equals(field, 'literal')` — true if `field`'s value equals `literal` exactly\n\
- `and(pred, pred)` — true if both nested predicates are true\n\
- `or(pred, pred)` — true if either nested predicate is true\n\
- `not(pred)` — true if the nested predicate is false\n\n\
Strict rules, no exceptions:\n\
- Function names are matched exactly, lowercase, case-sensitively — `Contains` or `CONTAINS` fail.\n\
- `field` is a bare identifier: letters, digits, and underscores only. No dots, hyphens, spaces, \
or brackets — there is no field-path or JSONPath syntax, so a value one level down must still be \
named as a flat field, e.g. `first_name`, never `payload.first_name` or `payload[\"first_name\"]`.\n\
- String literals are single-quoted only, e.g. `'new'` — double-quoted strings are invalid. Inside \
a literal, `\\'` is an escaped quote and `\\\\` is an escaped backslash; nothing else is special.\n\
- There are no numeric or boolean literals anywhere in this language, and no infix or symbolic \
operators (`==`, `!=`, `&&`, `||`, `in`, `is`, etc.) — boolean combination is done with the \
`and`/`or`/`not` calls above, not operators.\n\
- Whitespace between tokens is fine; whitespace inside a name or identifier is not.\n\n\
Worked examples, for a watch over a Notion table of client rows with fields `first_name`, \
`company`, and `status`:\n\
- `not_empty(first_name)` — fires once a row has any name filled in\n\
- `equals(status, 'new')` — fires only once `status` is set to exactly \"new\"\n\
- `and(not_empty(company), or(equals(status, 'new'), contains(status, 'lead')))` — fires once a \
row has a company recorded AND its status is either exactly \"new\" or contains \"lead\"\n\n\
Every identifier you use — both inside `expr` and in the contract's `fields` list — must be one \
of these flat snake_case names. Never emit a dotted path anywhere in the contract.";

/// Build the user-turn message handed to [`LiveAgentWatchDetector`]'s child
/// session: the assignment's plain-language watch condition, framed as a
/// one-shot observation task, plus mode-specific instructions driven by
/// whether this watch already has a [`WatchContract`].
///
/// `contract: None` is authoring mode (first run only): the model observes
/// unconstrained and must propose a contract alongside its candidates.
/// `contract: Some(_)` is bind mode (every later run): the contract is
/// injected and the model is instructed to transcribe, not decide. `repair`
/// only ever applies in authoring mode — see [`build_authoring_prompt`].
fn build_watch_prompt(instruction: &str, contract: Option<&WatchContract>, repair: Option<&RepairContext>) -> String {
    match contract {
        None => build_authoring_prompt(instruction, repair),
        Some(contract) => build_bind_prompt(instruction, contract),
    }
}

/// Authoring-mode prompt: no contract exists yet, so the
/// model observes freely and must emit both the baseline candidate array
/// (unchanged shape — this run only seeds a baseline, it never fires) and a
/// proposed contract. `rationale` is called out explicitly because it is the
/// one field of the proposal a user actually reads.
/// [`PREDICATE_GRAMMAR`] is spelled out in full here — without it the model
/// has nothing to go on for `predicate.expr` but the bare `"..."` placeholder
/// in [`CONTRACT_PROPOSAL_SHAPE`], and will invent syntax the parser rejects.
///
/// `repair` is `Some` either on a same-tick retry after a rejection
/// [`RepairContext`] knows how to describe precisely, or on this poll's very
/// first attempt when an earlier poll's proposal was rejected and that
/// reason is still live on the scratchpad (see [`RepairContext`],
/// [`run_authoring_attempts`]): a short corrective section is appended
/// describing exactly what went wrong, so the model corrects the one thing
/// that actually broke instead of re-guessing the whole proposal from
/// scratch.
fn build_authoring_prompt(instruction: &str, repair: Option<&RepairContext>) -> String {
    let mut prompt = format!(
        "# Watch condition\n\n{instruction}\n\n\
         This is this watch's first run, so there is no contract yet. Check the current \
         state of the thing described above now, using whatever tools are available to \
         you, and report every candidate item you currently see, per the system prompt's \
         JSON array format.\n\n\
         Alongside that array, propose a contract for this watch: a one-time declaration \
         of what \"the same item\" means here and when this watch should fire. Once you \
         submit it, it is frozen and handed back to you unchanged on every future poll — \
         you will not get to redecide it then, so decide carefully now. Respond with one \
         JSON object, not a bare array, shaped like this:\n\n\
         {CONTRACT_PROPOSAL_SHAPE}\n\n\
         If a tool call you made while exploring failed, or you could not complete this \
         observation for some other reason, do not force a candidates array or a contract \
         proposal out of it — there is nothing yet to propose from a failed look, and a \
         guessed-at proposal frozen from bad data would be wrong for good. Respond instead with \
         a JSON object naming exactly what went wrong, in one of these two shapes, and nothing \
         else: `{{\"status\": \"tool_error\", \"tool\": \"<name of the tool call that failed>\", \
         \"detail\": \"<what it returned, or its error message, verbatim>\"}}`, or \
         `{{\"status\": \"failed\", \"reason\": \"<why, in your own words>\"}}`. There will be \
         another poll later to try authoring again.\n\n\
         {PREDICATE_GRAMMAR}\n\n\
         Prefer the strongest identity strategy that actually holds for what you observed: \
         `native_id` if the source exposes one field whose value stays the same for a given \
         item across repeat checks; `composite_native` if no single field works alone but a \
         stable combination of fields does; `content_hash` only if you found no field or \
         combination that reliably identifies an item on its own. Specifically, prefer a \
         stable native identifier the source itself already exposes (a database row's own \
         primary key, a page id, a record id) whenever you can find one in what you observed, \
         even if it sits alongside other, more descriptive fields (a name, a title) that might \
         look like the \"obvious\" choice — only fall back to a composite of semantic fields \
         when no such native id exists at all. A native id survives a person renaming or \
         re-describing the same row; a composite of name/company/etc. does not. Whichever you \
         pick, your `rationale` is shown directly to the user, so state plainly what you chose \
         and why — it is the answer to \"why should I trust this watch to tell items apart \
         correctly.\"\n\n\
         # Choosing `mode`\n\n\
         `mode` answers a different question than identity does: not \"what is this item\" but \
         \"what should make this watch fire.\" Three choices: `new_only` fires the moment an \
         item you have never seen before appears at all — existence alone is the event, so \
         `change.material_fields` may be left empty, since there is no prior version of a \
         brand-new item to diff against. `predicate_transition` (the default if you omit \
         `mode`) fires when a field on an item you have already seen changes in a way \
         `predicate` cares about — `change.material_fields` is required here, since without it \
         nothing is ever treated as a version change. `new_or_changed` fires on either. Pick \
         `new_only` only when your watch condition is genuinely \"tell me when a new X shows \
         up,\" never \"tell me when X's status changes\" — for that, use `predicate_transition` \
         and declare the fields that changing counts as material.\n\n\
         Identity width is a tradeoff, not a \"more fields is safer\" knob: a wider identity \
         resists collisions (two distinct items merging into one key) but invites splits (one \
         item's ordinary edit minting a brand-new identity_key, which reports as a phantom \
         \"new\" item instead of a change to an existing one). A phantom notification is far \
         worse than a missed one — it destroys trust in the whole watch. So prefer the \
         NARROWEST field set that plausibly identifies the item, never include a field its \
         owner is likely to edit (a name, a description, a status, anything free-text), and \
         make sure `identity.fields` never shares a field with `change.material_fields` — a \
         field that identifies an item can never also be one whose change you want reported, \
         or every edit re-keys the item's identity instead of registering as a change to it.\n\n\
         # Prefer a structured query/list tool over a single-document fetch\n\n\
         Many connectors expose more than one way to check the same source: a fetch tool that \
         returns one document's rendered content (usually prose or markup, one item at a time, \
         meant for a human or a model to read), and a query/list/search tool that returns many \
         rows in one call as structured JSON. When both exist for the thing you're watching, \
         call the query/list tool, not the single-document fetch — its array-shaped rows can be \
         extracted deterministically on every future poll without a model ever reading them \
         again, while a rendered document has to be re-read and re-interpreted by a model every \
         single time. Worked example: watching a Notion database for new or changed rows, \
         prefer a tool that queries the database's own data source directly (its rows come back \
         as one JSON object per row) over a tool that fetches a page's rendered content — the \
         query answers the whole watch in one structured call, while the page fetch would mean \
         reading and re-parsing prose for every row on every single poll.\n\n\
         # Self-report the tool you used\n\n\
         If a single tool call against the source above told you everything you needed to \
         answer this watch, report it in `tool_used`/`arguments_used` so future polls can call \
         it directly instead of running a full model turn every time. This is optional — leave \
         both fields out entirely if no single call covers it, or if you are not confident one \
         does.\n\n\
         Two hard rules, no exceptions, because a frozen call runs unattended on a schedule \
         with nobody reviewing it before it executes:\n\n\
         1. `tool_used` MUST name a READ-ONLY tool — one that only queries, searches, fetches, \
         or lists. NEVER report a tool that creates, updates, deletes, moves, archives, \
         duplicates, sends, or otherwise mutates anything. If the only way to answer this watch \
         is a tool that changes state, do not report `tool_used` at all — leave it out and this \
         watch simply keeps using the model every poll.\n\
         2. `arguments_used` MUST stay valid indefinitely — no pagination cursor, no page or \
         continuation token, no absolute date/time bound that will be stale on the next poll. If \
         answering this watch required an argument like that, do not report `tool_used` at all \
         — an unstable argument frozen today will silently return the wrong thing (or nothing) \
         on every future poll, and nobody will be watching to notice.\n\n\
         Report `tool_used` as the bare tool name only — the part after the `mcp__<connector>__` \
         prefix your tools are namespaced under (e.g. if you called `mcp__notion__notion-search`, \
         report `tool_used: \"notion-search\"`), and report `arguments_used` as the exact \
         arguments object you called it with, verbatim. Both are shown to the user exactly as \
         you write them, so they can confirm what will run unattended — do not paraphrase, \
         reformat, or invent either one."
    );
    if let Some(repair) = repair {
        match repair {
            RepairContext::Accumulated(items) => {
                let bullets: String = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| format!("{}. {}", i + 1, repair_prompt_section(item)))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                prompt.push_str(&format!(
                    "\n\n# Your previous attempts hit {} DIFFERENT problems\n\n\
                     Every one of the following must hold AT ONCE in this proposal. Fixing one while \
                     reintroducing another is not a fix — it is the same rejection under a different name, and \
                     the next attempt will be judged against everything below, not just the newest item:\n\n{bullets}",
                    items.len()
                ));
            }
            other => {
                prompt.push_str(&format!("\n\n# Your previous attempt was rejected\n\n{}", repair_prompt_section(other)));
            }
        }
    }
    prompt
}

/// Renders one [`RepairContext`] variant's instruction text for
/// [`build_authoring_prompt`] — factored out of it so
/// [`RepairContext::Accumulated`] can render each of its entries with the
/// exact same per-variant text a lone repair would get, just concatenated
/// under one combined heading instead of each getting its own.
fn repair_prompt_section(repair: &RepairContext) -> String {
    match repair {
        RepairContext::InvalidPredicate { rejected_expr, error } => format!(
            "Your previous attempt proposed `predicate.expr = {rejected_expr:?}`, which failed with this \
             error: {error} Emit a corrected `expr` that follows the grammar above exactly — re-read it \
             before answering again."
        ),
        RepairContext::IdentityMaterialFieldOverlap { fields } => format!(
            "Your previous attempt declared {fields:?} in both `identity.fields` and \
             `change.material_fields`. A field that identifies an item can never also be one whose \
             change is material — remove {fields:?} from `identity.fields` (or from \
             `change.material_fields`) so the two sets no longer overlap, and identify the item by its \
             other, stable fields instead."
        ),
        RepairContext::EmptyMaterialFields => format!(
            "Your previous attempt declared `change.material_fields` as empty while `mode` was not \
             `new_only`. Either declare at least one material field — a field whose change on an \
             already-seen item should be reported — or, if this watch should instead fire the moment a \
             brand-new item appears regardless of any field changing, set `mode` to `new_only` (which \
             does not need `change.material_fields` at all)."
        ),
        RepairContext::Malformed { reason } => format!(
            "Your previous attempt's `contract` object did not match the required shape: {reason} Re-read the \
             shape above carefully — in particular, make sure every key required for the `mode` you chose is \
             present, spelled exactly right, and holds the right type — then resubmit a complete proposal."
        ),
        RepairContext::CrossPollRejection { reason } => format!(
            "An earlier poll's proposal for this watch was rejected: {reason} Do not repeat that \
             mistake in this proposal."
        ),
        RepairContext::Accumulated(items) => {
            // Not expected to nest in practice (see the type's own doc), but
            // rendered correctly regardless rather than assumed away.
            items.iter().map(repair_prompt_section).collect::<Vec<_>>().join("\n\n")
        }
    }
}

/// Bind-mode prompt: a contract already exists, so the
/// model's job narrows from judgment to transcription. It reports exactly
/// the fields the contract declares — nothing invented, nothing renamed —
/// and relays `identity.source_field`'s value verbatim so the caller can hash
/// `payload[source_field]` in Rust instead of trusting a model-decided id.
fn build_bind_prompt(instruction: &str, contract: &WatchContract) -> String {
    let source_kind = &contract.source.kind;
    let source_ref = &contract.source.ref_;
    let identity_desc = describe_identity(contract);
    let fields_desc = describe_fields(contract);
    format!(
        "# Watch condition\n\n{instruction}\n\n\
         This watch already has a contract from a previous run, reproduced below. You are \
         bound by it: do not redesign it, do not rename anything in it, and do not decide \
         for yourself whether anything you see is new — a separate, deterministic process \
         reads the fields you report and makes that decision in code, not you.\n\n\
         Watching: {source_kind} ({source_ref})\n\n\
         {identity_desc}\n\n\
         Check the current state of the thing described above now, using whatever tools \
         are available to you. For every item you currently see — whether you reported it \
         before or not, it does not matter here, report all of them — respond with ONLY a \
         JSON array (an empty array `[]` if you found nothing) of objects, each containing \
         exactly these fields and nothing else:\n\n\
         {fields_desc}\n\n\
         Do not include an `id` field, and do not add any field that isn't listed above. \
         You are transcribing what the source already shows you, not composing a \
         description of it: for each field there is exactly one correct value, and a \
         fabricated, reworded, or approximated one will be quarantined rather than treated \
         as a new item.\n\n\
         An empty array `[]` means the source genuinely has nothing to report right now — \
         never use it to mean anything else. If a tool call you made while checking this \
         failed, do not reply `[]` and do not fabricate fields to fill the gap: respond \
         instead with a JSON OBJECT (not an array) — \
         `{{\"status\": \"tool_error\", \"tool\": \"<name of the tool call that failed>\", \
         \"detail\": \"<exactly what it returned, or its error message>\"}}`. If you could \
         not complete this observation for some other reason, respond with \
         `{{\"status\": \"failed\", \"reason\": \"<why, in your own words>\"}}` instead."
    )
}

/// Identity-strategy-specific portion of [`build_bind_prompt`]. `NativeId` is
/// the one that needs an explicit verbatim-copy instruction: the field it
/// names may not be one of `contract.fields` (it exists purely so the tick
/// can hash `payload[source_field]`), so the model must be told, separately
/// from the field list, to relay it exactly.
fn describe_identity(contract: &WatchContract) -> String {
    match contract.identity.strategy {
        IdentityStrategy::NativeId => {
            let field = contract.identity.source_field.as_deref().unwrap_or("(unnamed)");
            format!(
                "This source exposes one stable per-item key: its own `{field}` field. In \
                 addition to the fields below, your reply for each item MUST include a \
                 `{field}` key whose value is copied VERBATIM from the source — the exact \
                 characters it shows you, no reformatting, no re-casing, and never invented \
                 if it looks missing. There is exactly one correct string for `{field}` per \
                 item; you are not deciding whether it is new, only transcribing it, and a \
                 copied-wrong or made-up value will be detected and quarantined rather than \
                 treated as a new item."
            )
        }
        IdentityStrategy::CompositeNative | IdentityStrategy::ContentHash => "\
            This watch tells items apart by the combination of fields listed below, not by \
            any single id — report each of those fields exactly as observed and let the \
            combination speak for itself."
            .to_string(),
    }
}

/// Renders `contract.fields` (the extraction contract) as a sorted bullet
/// list of `name (type, required|optional)` for [`build_bind_prompt`]. Sorted
/// so the same contract always renders identical prompt text.
fn describe_fields(contract: &WatchContract) -> String {
    let mut names: Vec<&String> = contract.fields.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let spec = &contract.fields[name];
            let field_type = &spec.field_type;
            let required = if spec.required { "required" } else { "optional" };
            format!("- `{name}` ({field_type}, {required})")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Same user-turn framing as [`build_watch_prompt`], but for
/// [`LiveAgentWatchDetector::observe_via_profile_runner`]'s CLI dispatch
/// route: that path runs the assignment's agent through `AgentRunner::run`,
/// which has no separate system-prompt-override lever the way the `Api`
/// path's `RunnerConfig` does, so [`AGENT_WATCH_SYSTEM_PROMPT`]'s
/// observe-only contract has to travel as part of the one prompt string
/// instead.
fn build_full_profile_watch_prompt(
    instruction: &str,
    contract: Option<&WatchContract>,
    repair: Option<&RepairContext>,
) -> String {
    format!("{AGENT_WATCH_SYSTEM_PROMPT}\n\n{}", build_watch_prompt(instruction, contract, repair))
}

/// Tool registry [`LiveAgentWatchDetector::observe`] hands to the watch's
/// child session: every `mcp__`-qualified tool in `tools_registry`, narrowed
/// to one MCP server's `mcp__{server}__` tools when `connector_scope` names
/// one. Pulled out as a pure function of `Registry` so the filtering rule
/// itself is unit-testable without driving a full child session.
fn scoped_mcp_registry(tools_registry: &Registry, connector_scope: Option<&str>) -> Registry {
    let prefix = match connector_scope {
        Some(server) => format!("mcp__{server}__"),
        None => "mcp__".to_string(),
    };
    let mcp_tool_names: Vec<String> =
        tools_registry.list().into_iter().filter(|name| name.starts_with(&prefix)).collect();
    tools_registry.filter_for(&mcp_tool_names)
}

/// Production [`AgentWatchDetector`]: runs the assignment's own agent as a
/// bounded, non-persisted child session and parses its reply into
/// [`AgentWatchCandidate`]s. See the module doc for what infrastructure this
/// reuses, and why `Api`- and `Cli`-mode profiles take genuinely different
/// routes.
pub struct LiveAgentWatchDetector {
    persistence: Arc<PersistenceLayer>,
    /// The process's full tool registry (`AppState::tools_registry`), before
    /// this detector's own `mcp__`-only filtering. Shared, not owned — every
    /// poll filters a fresh copy rather than mutating this one. Only read on
    /// the `Api`-mode path (see [`Self::observe_via_native_session`]) — a
    /// `Cli`-mode profile runs its own full configured tool surface instead
    /// (see [`Self::observe_via_profile_runner`]).
    tools_registry: Arc<Registry>,
    /// Used only for `Api`-mode profiles.
    resolve_provider: ProviderResolver,
    /// Used only for `Cli`-mode profiles: picks the runner
    /// (`CliAgentRunner`, always, since `Cli`-mode profiles never route to
    /// the native runner — see `RunnerDispatcher::pick`) that actually spawns
    /// the agent's real CLI process with its full tool surface wired.
    dispatcher: Arc<RunnerDispatcher>,
    /// Where [`Self::observe`]/[`Self::observe_for_authoring`] surface a
    /// health event when parsing a reply drops every candidate despite the
    /// reply reporting some — see [`warn_on_total_parse_drop`]. The same
    /// `EventBus` `run_contract_bound_tick`'s own `quarantine_candidate`/
    /// `emit_health_event` calls use, so a total-drop parse shows up
    /// alongside every other watch health event rather than through a
    /// second channel.
    event_bus: Arc<EventBus>,
}

impl LiveAgentWatchDetector {
    /// Production constructor: resolves each `Api`-mode assignment's
    /// provider via `crate::provider_client_for_profile` (same seam every
    /// other one-shot model pass in this crate uses), and dispatches every
    /// `Cli`-mode assignment through `dispatcher` — the same
    /// `RunnerDispatcher` production wiring hands to
    /// `agent_runner::ProfileAwareChildRunner` for Delegate/Task subagent
    /// spawns.
    pub fn new(
        persistence: Arc<PersistenceLayer>,
        tools_registry: Arc<Registry>,
        dispatcher: Arc<RunnerDispatcher>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self::with_provider_resolver(
            persistence,
            tools_registry,
            Arc::new(crate::provider_client_for_profile),
            dispatcher,
            event_bus,
        )
    }

    /// Test-only seam: inject a scripted provider resolver (e.g. one backed
    /// by `MockProviderClient`) instead of the real `providers.toml` /
    /// CLI-binary resolution `new` wires in, so the `Api`-mode path can be
    /// exercised without a live provider. `dispatcher` still drives the
    /// `Cli`-mode path — tests targeting that path inject one built from a
    /// fake `AgentRunner` (see `RunnerDispatcher::with_runners`).
    fn with_provider_resolver(
        persistence: Arc<PersistenceLayer>,
        tools_registry: Arc<Registry>,
        resolve_provider: ProviderResolver,
        dispatcher: Arc<RunnerDispatcher>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self { persistence, tools_registry, resolve_provider, dispatcher, event_bus }
    }

    /// `Api`-mode observation path — structurally unchanged from before the
    /// CLI-tool-plumbing fix (see module doc): a filtered, MCP-only child
    /// session driven directly via `run_session` against `provider`. Returns
    /// the child's raw reply text rather than already-parsed candidates, so
    /// both [`Self::observe`] and [`Self::observe_for_authoring`] can share
    /// one poll and post-process it differently (the latter also needs to
    /// look for a `contract` key the former ignores).
    async fn observe_via_native_session(
        &self,
        profile: &AgentProfile,
        assignment: &Assignment,
        instruction: &str,
        repair: Option<&RepairContext>,
    ) -> Result<String, AgentWatchDetectError> {
        let provider = (self.resolve_provider)(profile).ok_or_else(|| {
            AgentWatchDetectError::Failed(
                "this agent has no provider configured — add an API key in Settings".to_string(),
            )
        })?;

        // Only MCP-sourced tools ("whatever MCP tools it
        // deems right") — no filesystem, bash, or mutation tools. A watch
        // only observes. `connector_scope` narrows this further to a single
        // MCP server's tools (`mcp__{server}__`) when the trigger names one;
        // `None` keeps every `mcp__`-qualified tool, exactly as before.
        let (connector_scope, contract) = match &assignment.trigger {
            AssignmentTrigger::AgentWatch { connector_scope, contract, .. } => {
                (connector_scope.as_deref(), contract.as_ref())
            }
            _ => (None, None),
        };
        let watch_registry = Arc::new(scoped_mcp_registry(&self.tools_registry, connector_scope));
        info!(
            assignment_id = %assignment.id,
            connector_scope = ?connector_scope,
            scoped_tool_count = watch_registry.list().len(),
            "agent watch: scoped tool registry for detector session (api mode)"
        );

        let cwd = assignment
            .working_directory
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));

        let session_id = uuid::Uuid::new_v4().to_string();
        let child_ctx = RunnerContext::new_with_cwd(session_id, assignment.agent_id.clone(), cwd)
            .with_registry(watch_registry)
            .with_system_prompt(AGENT_WATCH_SYSTEM_PROMPT);

        let config = RunnerConfig {
            provider,
            bridge: Arc::new(StubBridge),
            denial_tracker: Arc::new(NoopDenialTracker),
            settings: RunnerSettings::default(),
            // No human is attending a scheduled watch poll — bypass the
            // interactive permission gate the same way InspectionVerifier's
            // read-only child does. The registry is already filtered to
            // MCP-only tools and the system prompt forbids taking action.
            mode: PermissionMode::BypassPermissions,
            kind: SessionKind::Autonomous,
            auto_approve: vec![],
            system_prompt: Some(AGENT_WATCH_SYSTEM_PROMPT.to_string()),
            event_sink: None,
            thinking: None,
            max_turns: Some(AGENT_WATCH_TURN_CAP),
        };

        let initial_messages = vec![Message::User {
            content: vec![ContentBlock::Text { text: build_watch_prompt(instruction, contract, repair) }],
        }];

        let child_cancel = child_ctx.cancel.clone();
        let timeout = Duration::from_secs(AGENT_WATCH_TIMEOUT_SECS);
        let outcome = match tokio::time::timeout(timeout, run_session(initial_messages, child_ctx, config)).await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(AgentWatchDetectError::Failed(format!("agent-watch session error: {e}"))),
            Err(_elapsed) => {
                child_cancel.cancel();
                return Err(AgentWatchDetectError::Failed(format!(
                    "agent-watch observation timed out after {AGENT_WATCH_TIMEOUT_SECS}s"
                )));
            }
        };

        // Report this session's real provider-turn count into whichever
        // `MODEL_CALL_TURN_TALLY` scope (if any) is currently active — see
        // that task-local's own doc for why this is how the cost figure
        // learns a session took more than the one turn its caller's
        // pre-call floor already assumed. Reported unconditionally, before
        // the cancelled/empty-text check below, since every one of these
        // `outcome.turns` completed and cost real provider money regardless
        // of whether this call ultimately returns `Ok` or `Err` to its
        // caller.
        let _ = MODEL_CALL_TURN_TALLY
            .try_with(|tally| tally.fetch_add(outcome.turns as u32, std::sync::atomic::Ordering::Relaxed));

        if outcome.cancelled && outcome.final_assistant_text.is_empty() {
            return Err(AgentWatchDetectError::Failed(format!(
                "agent-watch child was cancelled after {} turns without reporting findings",
                outcome.turns
            )));
        }

        info!(
            assignment_id = %assignment.id,
            reply_len = outcome.final_assistant_text.len(),
            "agent watch: detector session completed (api mode)"
        );

        Ok(outcome.final_assistant_text)
    }

    /// `Cli`-mode observation path. A CLI-mode agent's `ProviderClient` is
    /// the tool-less `CliProviderClient` (see module doc), so this does NOT
    /// drive `run_session` at all — it dispatches through `self.dispatcher`,
    /// the same `RunnerDispatcher` entry point `agent_runner::
    /// ProfileAwareChildRunner` uses for Delegate/Task subagent spawns. That
    /// resolves to `CliAgentRunner::run`, which spawns the agent's real CLI
    /// process with `--mcp-config` wired — the *only* path where a CLI-mode
    /// profile gets its actual configured tools, not a scoped registry
    /// (decision: run the assignment's agent as itself, full surface).
    ///
    /// Isolated like a delegate child (`isolate_history: true`, a sidechain
    /// transcript file, a dedicated event channel) so a poll never lands in
    /// the agent's real chat history or live feed, and
    /// `bypass_instance_cap: true` so the poll can never be blocked/queued
    /// behind — or itself block — the agent's own live turn. Returns the
    /// child's raw reply text — see [`Self::observe_via_native_session`]'s
    /// doc for why.
    async fn observe_via_profile_runner(
        &self,
        profile: &AgentProfile,
        assignment: &Assignment,
        instruction: &str,
        repair: Option<&RepairContext>,
    ) -> Result<String, AgentWatchDetectError> {
        if let AssignmentTrigger::AgentWatch { connector_scope: Some(scope), .. } = &assignment.trigger {
            info!(
                assignment_id = %assignment.id,
                connector_scope = %scope,
                "agent watch: connector_scope is ignored in cli mode — the full profile tool surface is used instead"
            );
        }
        let contract = match &assignment.trigger {
            AssignmentTrigger::AgentWatch { contract, .. } => contract.as_ref(),
            _ => None,
        };

        let runner = self.dispatcher.pick(profile);

        let poll_id = uuid::Uuid::new_v4().to_string();
        let transcript_override = ao_protocol::data_root::resolve_data_root().ok().map(|root| {
            root.join("messages").join("data").join(format!("agent-watch-{poll_id}.jsonl"))
        });
        let (result_tx, _result_rx) = tokio::sync::mpsc::channel(1);
        let cancel = CancellationToken::new();

        let request = AgentRunRequest {
            agent: profile.clone(),
            prompt: build_full_profile_watch_prompt(instruction, contract, repair),
            run_complete_tx: result_tx,
            scope: RunScope::Standalone,
            session_kind: SessionKind::Autonomous,
            isolate_history: true,
            cancel: Some(cancel.clone()),
            transcript_override,
            event_channel: Some(format!("agent-watch:{poll_id}")),
            bypass_instance_cap: true,
            ..Default::default()
        };

        let timeout = Duration::from_secs(AGENT_WATCH_TIMEOUT_SECS);
        let outcome = match tokio::time::timeout(timeout, runner.run(request)).await {
            Ok(Ok(rc)) => rc,
            Ok(Err(e)) => return Err(AgentWatchDetectError::Failed(format!("agent-watch session error: {e}"))),
            Err(_elapsed) => {
                // Fires `CliAgentRunner`'s cancel bridge
                // (`cancel_token.cancelled().await -> cancel_run(...)`), the
                // same mechanism the Stop button and DelegateStop use, which
                // actually terminates the spawned OS process — not just this
                // in-process future.
                cancel.cancel();
                return Err(AgentWatchDetectError::Failed(format!(
                    "agent-watch observation timed out after {AGENT_WATCH_TIMEOUT_SECS}s"
                )));
            }
        };

        // The CLI process can end without completing normally even when
        // `runner.run` itself returns `Ok` — e.g. the agent's own
        // `no_output_timeout_ms` watchdog (default 30s, well inside our own
        // outer `AGENT_WATCH_TIMEOUT_SECS`) tripping on a hung child.
        // `parse_candidates` on a likely-empty/partial reply would fail
        // anyway, but this surfaces a clearer reason than a generic parse
        // error.
        if outcome.end_reason != ao_protocol::event::RunEndReason::Completed {
            return Err(AgentWatchDetectError::Failed(format!(
                "agent-watch cli process ended without completing normally: {:?}",
                outcome.end_reason
            )));
        }

        info!(
            assignment_id = %assignment.id,
            reply_len = outcome.output_text.len(),
            "agent watch: detector session completed (cli mode, full profile tool surface)"
        );

        Ok(outcome.output_text)
    }

    /// Shared entry point for both [`AgentWatchDetector::observe`] and
    /// [`AgentWatchDetector::observe_for_authoring`]: resolves the
    /// assignment's agent profile and runs exactly one poll (dispatched to
    /// whichever of [`Self::observe_via_native_session`]/
    /// [`Self::observe_via_profile_runner`] the profile's `runner_mode`
    /// calls for), returning the child's raw reply text unparsed. Callers
    /// decide what to extract from it.
    async fn observe_raw_text(
        &self,
        assignment: &Assignment,
        instruction: &str,
        repair: Option<&RepairContext>,
    ) -> Result<String, AgentWatchDetectError> {
        let profile = self
            .persistence
            .agents
            .get(&assignment.agent_id)
            .await
            .map_err(|e| AgentWatchDetectError::Failed(format!("failed to load agent profile: {e}")))?
            .ok_or_else(|| {
                AgentWatchDetectError::Failed(format!("agent '{}' does not exist", assignment.agent_id))
            })?;

        match profile.runner_mode {
            AgentRunnerMode::Api => self.observe_via_native_session(&profile, assignment, instruction, repair).await,
            AgentRunnerMode::Cli => self.observe_via_profile_runner(&profile, assignment, instruction, repair).await,
        }
    }
}

impl LiveAgentWatchDetector {
    /// Shared [`WatchObservation`] -> `Vec<AgentWatchCandidate>` resolution
    /// for both [`AgentWatchDetector::observe`] and
    /// [`AgentWatchDetector::observe_for_authoring`]: an `Observed` reply
    /// passes its candidates straight through, while either failure variant
    /// is turned into a health event carrying the real reason and an empty
    /// candidate list — the same "poll found nothing to fire on, but is
    /// visibly unhealthy" shape [`warn_on_total_parse_drop`] already uses,
    /// so a reported tool failure never quarantines (there is no candidate
    /// for a quarantine to blame) and never reads as an ordinary quiet tick
    /// (the health event is what tells those apart).
    async fn resolve_observation(
        &self,
        assignment: &Assignment,
        observation: WatchObservation,
    ) -> Vec<AgentWatchCandidate> {
        match observation {
            WatchObservation::Observed(candidates) => candidates,
            WatchObservation::ToolError { tool, detail } => {
                let described = match tool.as_deref() {
                    Some(tool) => format!("its `{tool}` tool call failed: {detail}"),
                    None => format!("a tool call failed: {detail}"),
                };
                warn_on_reported_observation_failure(&self.event_bus, assignment, &described).await;
                Vec::new()
            }
            WatchObservation::ObservationFailed { reason } => {
                warn_on_reported_observation_failure(&self.event_bus, assignment, &reason).await;
                Vec::new()
            }
        }
    }
}

#[async_trait]
impl AgentWatchDetector for LiveAgentWatchDetector {
    async fn observe(
        &self,
        assignment: &Assignment,
        instruction: &str,
    ) -> Result<Vec<AgentWatchCandidate>, AgentWatchDetectError> {
        let text = self.observe_raw_text(assignment, instruction, None).await?;
        let contract = assignment_watch_contract(assignment);
        let observation = parse_candidates(&text, contract).ok_or_else(|| {
            AgentWatchDetectError::Failed(format!("could not parse structured findings from reply: {text}"))
        })?;
        // Only an `Observed` reply is eligible for the total-parse-drop check
        // below — running it against a reported-failure reply's free-text
        // `detail`/`reason` risks `raw_array_len`'s own bracket-matching
        // heuristic mistaking a bracketed aside in that prose (e.g. "...
        // returned [500] Internal Server Error") for a candidate array.
        if let WatchObservation::Observed(ref candidates) = observation {
            warn_on_total_parse_drop(&self.event_bus, assignment, raw_array_len(&text), candidates.len()).await;
        }
        let candidates = self.resolve_observation(assignment, observation).await;
        info!(assignment_id = %assignment.id, candidate_count = candidates.len(), "agent watch: detector observed candidates");
        Ok(candidates)
    }

    /// Authoring-mode poll: one raw poll, parsed for
    /// *both* the baseline candidate array (identical extraction to
    /// [`Self::observe`]) and — new here — a `contract` object alongside it,
    /// via [`extract_contract_proposal`]. Never issues a second poll itself;
    /// the stability probe that needs a second, later poll
    /// is orchestrated by the caller (`author_contract`), which is free to
    /// call this or plain [`Self::observe`] again once it knows a proposal
    /// is worth verifying. `repair`, when present, is folded into the
    /// authoring prompt by [`build_authoring_prompt`] — see [`RepairContext`].
    async fn observe_for_authoring(
        &self,
        assignment: &Assignment,
        instruction: &str,
        repair: Option<&RepairContext>,
    ) -> Result<AuthoringReply, AgentWatchDetectError> {
        let text = self.observe_raw_text(assignment, instruction, repair).await?;
        // Authoring is always contract-less by construction — this is the
        // poll that PROPOSES a contract, so none is bound yet; pass `None` explicitly rather than reading
        // `assignment.trigger` so this stays correct even if a caller ever
        // invoked authoring against an assignment that already has one.
        let observation = parse_candidates(&text, None).ok_or_else(|| {
            AgentWatchDetectError::Failed(format!("could not parse structured findings from reply: {text}"))
        })?;
        // A reported failure never carries a `contract` key (see
        // `build_authoring_prompt`), so `proposed_contract` is correctly
        // `None` for those without any special-casing here. See
        // `Self::observe`'s matching guard for why this check runs before
        // `resolve_observation` consumes `observation`.
        if let WatchObservation::Observed(ref candidates) = observation {
            warn_on_total_parse_drop(&self.event_bus, assignment, raw_array_len(&text), candidates.len()).await;
        }
        let proposed_contract = extract_contract_proposal(&text);
        let candidates = self.resolve_observation(assignment, observation).await;
        info!(
            assignment_id = %assignment.id,
            candidate_count = candidates.len(),
            proposal_present = proposed_contract.is_some(),
            "agent watch: authoring-mode detector observed candidates"
        );
        Ok(AuthoringReply { candidates, proposed_contract })
    }
}

/// One [`LiveAgentWatchDetector`] child reply, parsed by [`parse_candidates`]
/// into exactly one of three outcomes — kept as real variants, never a bool
/// bolted onto a `Vec`, so a caller cannot pattern-match this and quietly
/// ignore the failure arms the way an ignorable flag would let it. This is
/// what closes the reply contract's missing error channel: before this
/// existed, a child whose tool call failed mid-turn could only reply `[]`
/// (indistinguishable from a source that genuinely had nothing) or invent
/// findings, so a broken connector rendered as a permanently quiet,
/// apparently-healthy watch.
#[derive(Debug, Clone, PartialEq)]
enum WatchObservation {
    /// The child completed its observation; `Vec` may be empty, meaning the
    /// source genuinely had nothing to report this poll — the ordinary
    /// "quiet tick," never conflated with either failure variant below.
    Observed(Vec<AgentWatchCandidate>),
    /// The child reported that a tool call it made while observing failed —
    /// [`build_bind_prompt`]/[`build_authoring_prompt`]'s `tool_error`
    /// shape. `tool` is `None` when the child didn't name one.
    ToolError { tool: Option<String>, detail: String },
    /// The child reported it could not complete the observation for some
    /// other stated reason (not a tool failure) — the `failed` shape.
    ObservationFailed { reason: String },
}

/// Parse [`LiveAgentWatchDetector`]'s child reply into a [`WatchObservation`].
/// Tolerant of a fenced ```json code block or light prose wrapping — the
/// same "fenced, then raw, then first balanced substring" strategy
/// `ao_engine_tools_runner::reflection`/`verification` already use for their
/// own JSON replies, reimplemented here since those helpers are private to
/// that crate. Returns `None` only when the reply matches none of the known
/// shapes at all — a real parse failure, as opposed to a validly-empty `[]`
/// (which returns `Some(WatchObservation::Observed(vec![]))`).
///
/// Checks for a reported failure ([`parse_reported_failure`]) first, since
/// that reply shape has no candidate array to extract at all. Everything
/// else — the current success shape, and a reply in the legacy bare-array
/// shape from a model that hasn't seen (or ignored) the newer contract —
/// falls through unchanged to the array extraction below: an object-wrapped
/// `\"candidates\": [...]` and a bare top-level `[...]` both hand
/// [`extract_array_value`]'s existing bracket-matching the same array to
/// find, so a legacy reply is parsed as an ordinary successful observation
/// rather than dropped. That fallback is deliberate, not incidental: an
/// earlier reply-shape change in this exact subsystem quarantined a reply
/// that was actually correct, just parsed in the wrong mode.
///
/// `contract` selects which of two genuinely different candidate-array
/// shapes this reply is in (see [`build_watch_prompt`]): `None` is
/// authoring/legacy mode ([`parse_authoring_candidate`]), `Some` is bind
/// mode ([`parse_bind_candidate`]). Getting this wrong silently breaks
/// parsing — bind mode's items have no `id`/`summary`/`payload` wrapper at
/// all (see [`build_bind_prompt`]) — so every caller must pass the same
/// `assignment.trigger`-derived contract [`build_watch_prompt`] used to
/// build the prompt this reply is answering (see [`assignment_watch_contract`]).
fn parse_candidates(raw: &str, contract: Option<&WatchContract>) -> Option<WatchObservation> {
    let trimmed = raw.trim();

    if let Some(observation) = parse_reported_failure(trimmed) {
        return Some(observation);
    }

    let value = extract_array_value(trimmed)?;
    let items = value.as_array()?;

    Some(WatchObservation::Observed(match contract {
        Some(contract) => items.iter().map(|item| parse_bind_candidate(contract, item)).collect(),
        None => items.iter().filter_map(parse_authoring_candidate).collect(),
    }))
}

/// Detects a reply in one of [`WatchObservation`]'s two failure shapes —
/// `{"status": "tool_error", ...}` or `{"status": "failed", ...}` — and
/// `None` for anything else, including a normal success reply (whether or
/// not it happens to carry a `\"status\": \"ok\"` key) and the legacy
/// bare-array shape, both of which [`parse_candidates`] handles itself.
///
/// Deliberately only recognizes an object at the reply's own top level (via
/// [`extract_top_level_object`]), never one found by hunting through a
/// larger reply for the first balanced `{...}` substring the way
/// [`extract_object_value`] does elsewhere in this file: a bind-mode
/// candidate is itself a JSON object, and a real contract field can
/// plausibly be named `status` with a value like `"failed"` (an order- or
/// build-status watch, say) — that substring search would find that
/// object first and misread ordinary payload data as this reply's own
/// failure report. Requiring the true top level to be an object rules that
/// out entirely, since every genuine success reply in this file's contract
/// is a bare array or an array-containing envelope, never a bare object at
/// the outer level with a matching `status` on its own.
fn parse_reported_failure(trimmed: &str) -> Option<WatchObservation> {
    let object = extract_top_level_object(trimmed)?;
    match object.get("status").and_then(Value::as_str)? {
        "tool_error" => {
            let tool = non_empty_reply_str(&object, "tool");
            let detail = non_empty_reply_str(&object, "detail")
                .or_else(|| non_empty_reply_str(&object, "error"))
                .unwrap_or_else(|| "(no detail reported)".to_string());
            Some(WatchObservation::ToolError { tool, detail })
        }
        "failed" | "error" => {
            let reason = non_empty_reply_str(&object, "reason").unwrap_or_else(|| "(no reason given)".to_string());
            Some(WatchObservation::ObservationFailed { reason })
        }
        _ => None,
    }
}

/// Parses `trimmed` as a top-level JSON object, tolerating only an optional
/// fenced ```json wrapper — unlike [`extract_object_value`], this never
/// hunts for an object embedded inside a larger reply. See
/// [`parse_reported_failure`]'s doc for why that distinction matters here.
fn extract_top_level_object(trimmed: &str) -> Option<Value> {
    if let Some(fenced) = fenced_block(trimmed) {
        if let Ok(value) = serde_json::from_str::<Value>(fenced) {
            if value.is_object() {
                return Some(value);
            }
        }
    }
    let value = serde_json::from_str::<Value>(trimmed).ok()?;
    value.is_object().then_some(value)
}

/// Trimmed, non-empty string value of `object[key]`, or `None` if the key is
/// missing, not a string, or blank — shared by [`parse_reported_failure`]'s
/// two failure shapes.
fn non_empty_reply_str(object: &Value, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// One authoring/legacy-mode array entry (the shape
/// [`AGENT_WATCH_SYSTEM_PROMPT`] documents by default): an `{id, summary,
/// payload}` envelope. Individual malformed entries (missing/blank `id`) are
/// dropped rather than failing the whole batch — `id` is display-only for
/// dedup purposes (see [`AgentWatchCandidate::id`]), but it is still the only
/// per-item label a fired notification has to show, so an entry that omits
/// it entirely is not useful to keep; a `summary` missing or blank falls back
/// to `id`, and a missing `payload` falls back to `null`,
/// since neither is dedup-critical.
fn parse_authoring_candidate(item: &Value) -> Option<AgentWatchCandidate> {
    let id = item.get("id").and_then(|v| v.as_str()).map(str::trim)?;
    if id.is_empty() {
        return None;
    }
    let summary =
        item.get("summary").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).unwrap_or(id).to_string();
    let payload = item.get("payload").cloned().unwrap_or(Value::Null);
    Some(AgentWatchCandidate { id: id.to_string(), summary, payload })
}

/// One bind-mode array entry (see [`build_bind_prompt`]): the
/// item itself IS the payload — a flat object of exactly `contract.fields`
/// (plus, for `IdentityStrategy::NativeId`, one extra `identity.source_field`
/// key) and nothing else. There is no `id` key to read here at all — the
/// prompt explicitly forbids one — so identity is derived the same way
/// [`run_contract_bound_tick`]'s own diff derives it moments later: via
/// [`identity_key`], reusing the exact ladder `contract.identity.strategy`
/// declares instead of re-deriving a parallel notion of "what identifies this
/// item" here.
///
/// Never drops an entry, even one this contract can't derive an identity for
/// ([`identity_key`] returning `Err`, e.g. a required identity field the
/// model omitted): the candidate is still handed back so the caller's diff
/// loop reaches it and quarantines it there
/// ([`run_contract_bound_tick`]'s own `identity_key`/`quarantine_candidate`
/// handling), which already logs and surfaces a health event for exactly
/// this failure. Dropping it here instead — the way the pre-fix
/// `id`-required parser dropped every bind-mode entry — would silently
/// reintroduce the same invisible 100%-drop failure this function exists to
/// fix, just gated on a different condition.
fn parse_bind_candidate(contract: &WatchContract, item: &Value) -> AgentWatchCandidate {
    let payload = item.clone();
    let id = match identity_key(contract, &payload) {
        Ok(key) => key,
        Err(_) => "(identity undetermined)".to_string(),
    };
    let summary = summarize_bind_payload(contract, &payload);
    AgentWatchCandidate { id, summary, payload }
}

/// Human-readable one-line summary for a [`parse_bind_candidate`] result,
/// built from `contract.fields`' declared values present on `payload` —
/// bind-mode replies never carry a `summary` key of their own (see
/// [`build_bind_prompt`]), so this is the only rendering
/// [`TriggerEventContext::summary`] gets when this candidate is the one that
/// ends up firing. Sorted by field name for the same reason
/// [`describe_fields`] sorts: a stable, reproducible rendering.
fn summarize_bind_payload(contract: &WatchContract, payload: &Value) -> String {
    let mut names: Vec<&String> = contract.fields.keys().collect();
    names.sort();
    let parts: Vec<String> = names
        .into_iter()
        .filter_map(|name| {
            let value = payload.get(name)?;
            if value.is_null() {
                return None;
            }
            let rendered = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Some(format!("{name}: {rendered}"))
        })
        .collect();
    if parts.is_empty() {
        "(no contract fields reported)".to_string()
    } else {
        parts.join(", ")
    }
}

/// Number of entries in the JSON array a reply parses to, independent of how
/// many of those entries [`parse_candidates`] actually turned into
/// candidates — the two length being unequal is exactly the "100% drop"
/// condition [`warn_on_total_parse_drop`] watches for. Returns `0` for a
/// reply with no locatable array at all — that case is already a hard parse
/// failure ([`AgentWatchDetectError::Failed`]) at the call site, so it never
/// reaches this check.
fn raw_array_len(raw: &str) -> usize {
    extract_array_value(raw.trim()).and_then(|v| v.as_array().map(Vec::len)).unwrap_or(0)
}

/// Surfaces the one failure mode [`parse_candidates`] itself cannot: a reply
/// whose JSON array had entries, none of which survived parsing into a
/// candidate. Standing product rule on this codebase — "if the engine
/// detects it, the user sees it" — so this must never look like an ordinary,
/// legitimately quiet poll (a reply that reports the empty array `[]`
/// because there is genuinely nothing to observe, which is `raw_item_count
/// == 0` and does NOT warn here). Bind mode ([`parse_bind_candidate`]) never
/// drops an entry, so in practice this only fires for authoring/legacy
/// replies where every entry was missing the `id` field they're required to
/// carry — but the check is mode-agnostic so a future regression in either
/// path still gets caught here rather than silently reading as "nothing
/// found."
async fn warn_on_total_parse_drop(
    event_bus: &Arc<EventBus>,
    assignment: &Assignment,
    raw_item_count: usize,
    candidate_count: usize,
) {
    if raw_item_count == 0 || candidate_count > 0 {
        return;
    }
    warn!(
        assignment_id = %assignment.id,
        assignment_name = %assignment.name,
        raw_item_count,
        "agent watch: every item in a non-empty reply was dropped while parsing; this is not a quiet tick"
    );
    emit_health_event(
        event_bus,
        assignment,
        format!(
            "Agent watch \"{}\" received a reply reporting {raw_item_count} item(s), but none of them could be \
             parsed into a usable observation — nothing was reported as new on this poll. This usually means the \
             model didn't follow the expected reply format; check the assignment's run history for the raw reply.",
            assignment.name
        ),
    )
    .await;
}

/// Surfaces the other failure mode [`parse_candidates`] itself cannot ride
/// through silently: the child explicitly reporting (via [`WatchObservation::ToolError`]
/// or [`WatchObservation::ObservationFailed`]) that it could not complete
/// this poll's observation. Mirrors [`warn_on_total_parse_drop`]'s pattern —
/// log, then raise a health event carrying `detail` verbatim — and, like
/// that function, does not change what the caller reports for this poll
/// (still zero candidates, so nothing fires): there is no real data to diff
/// against, so a missing fire is correct here. The health event is what
/// keeps that correct-but-empty result from reading as an ordinary quiet
/// tick — the standing "if the engine detects it, the user sees it" rule
/// this whole reply-shape fix exists to uphold.
async fn warn_on_reported_observation_failure(event_bus: &Arc<EventBus>, assignment: &Assignment, detail: &str) {
    warn!(
        assignment_id = %assignment.id,
        assignment_name = %assignment.name,
        detail,
        "agent watch: child reported it could not complete this poll's observation"
    );
    emit_health_event(
        event_bus,
        assignment,
        format!(
            "Agent watch \"{}\" could not complete this poll's observation: {detail} Nothing was reported as new \
             on this poll — this is not the same as the source genuinely having nothing new.",
            assignment.name
        ),
    )
    .await;
}

/// Pulls the bound [`WatchContract`], if any, off `assignment.trigger` —
/// `None` for any non-`AgentWatch` trigger or an `AgentWatch` trigger that
/// hasn't authored one yet. Shared by every [`LiveAgentWatchDetector`] call
/// site that needs to know which reply shape ([`build_watch_prompt`]) a poll
/// used.
fn assignment_watch_contract(assignment: &Assignment) -> Option<&WatchContract> {
    match &assignment.trigger {
        AssignmentTrigger::AgentWatch { contract, .. } => contract.as_ref(),
        _ => None,
    }
}

/// Locate a JSON array inside arbitrary model output, tolerating a fenced
/// code block or light prose wrapping. Mirrors
/// `ao_engine_tools_runner::reflection::extract_array_value`'s "try fenced,
/// then raw, then first balanced substring" strategy.
fn extract_array_value(trimmed: &str) -> Option<serde_json::Value> {
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(fenced) = fenced_block(trimmed) {
        candidates.push(fenced);
    }
    candidates.push(trimmed);
    if let Some(bracketed) = first_balanced_array(trimmed) {
        candidates.push(bracketed);
    }

    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
            if value.is_array() {
                return Some(value);
            }
        }
    }
    None
}

fn fenced_block(text: &str) -> Option<&str> {
    let (open_len, start) = if let Some(idx) = text.find("```json") {
        (7, idx)
    } else if let Some(idx) = text.find("```") {
        (3, idx)
    } else {
        return None;
    };
    let after = &text[start + open_len..];
    let end = after.find("```")?;
    Some(after[..end].trim())
}

/// Return the first balanced top-level `[...]` substring, honoring string
/// literals so a `]` inside a quoted value doesn't close it early.
fn first_balanced_array(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('[')?;
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extracts the `contract` proposal object from an authoring-mode reply
/// shaped `{"candidates": [...], "contract": {...}}`.
/// Mirrors [`extract_array_value`]'s "fenced, then raw, then first balanced
/// substring" tolerance, but for a top-level JSON object instead of an
/// array, then reads its `contract` key. Returns `None` for a reply that
/// isn't a JSON object at all, or is one without a `contract` key —
/// [`run_agent_watch_tick`]'s authoring path treats that as "no proposal
/// this poll," not as a validation failure (a detector's child is not
/// required to propose a contract on every authoring-mode poll, only
/// expected to eventually).
fn extract_contract_proposal(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    let object = extract_object_value(trimmed)?;
    object.get("contract").cloned()
}

fn extract_object_value(trimmed: &str) -> Option<Value> {
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(fenced) = fenced_block(trimmed) {
        candidates.push(fenced);
    }
    candidates.push(trimmed);
    if let Some(braced) = first_balanced_object(trimmed) {
        candidates.push(braced);
    }

    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
            if value.is_object() {
                return Some(value);
            }
        }
    }
    None
}

/// Return the first balanced top-level `{...}` substring, honoring string
/// literals so a `}` inside a quoted value doesn't close it early. Mirrors
/// [`first_balanced_array`] exactly, one bracket pair over.
fn first_balanced_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Runs one detect-loop evaluation for `assignment` (an `AgentWatch`
/// trigger): loads the durable scratchpad, asks `detector` what it currently
/// observes, then hands off to a code-owned (no model involved) diff —
/// [`run_contract_bound_tick`] when the assignment carries a bound
/// [`WatchContract`], or
/// [`run_legacy_seen_ids_tick`]'s `seen_ids` string-equality fallback for a
/// watch that hasn't authored one yet — and, only on a genuine finding,
/// fires the assignment exactly like any other trigger kind. Returns whether
/// a fire happened, so the caller (the schedule runner's tick loop) knows
/// whether to stamp `last_run_at` when it reschedules via `mark_polled`.
///
/// Every failure mode (scratchpad load, detection, the fire itself) is
/// caught and logged rather than propagated — mirroring
/// `ScheduleRunner::tick_connector_events`'s resilience, so one bad poll of
/// one assignment never stalls the tick loop for every other assignment.
pub async fn run_agent_watch_tick(
    persistence: &Arc<PersistenceLayer>,
    dispatcher: &Arc<dyn NotificationDispatcher>,
    event_bus: &Arc<EventBus>,
    detector: &Arc<dyn AgentWatchDetector>,
    tools_registry: &Arc<Registry>,
    assignment: &Assignment,
    instruction: &str,
    timezone: Option<&str>,
) -> bool {
    let (connector_scope, contract, extraction, extraction_tool, extraction_args, extraction_output_schema_declared) =
        match &assignment.trigger {
            AssignmentTrigger::AgentWatch {
                connector_scope,
                contract,
                extraction,
                extraction_tool,
                extraction_args,
                extraction_output_schema_declared,
                ..
            } => (
                connector_scope.as_deref(),
                contract.as_ref(),
                extraction.as_ref(),
                extraction_tool.as_deref(),
                extraction_args.as_ref(),
                *extraction_output_schema_declared,
            ),
            _ => (None, None, None, None, None, false),
        };
    info!(
        assignment_id = %assignment.id,
        assignment_name = %assignment.name,
        connector_scope = ?connector_scope,
        contract_bound = contract.is_some(),
        extraction_configured = extraction.is_some(),
        "agent watch: starting detector observation"
    );

    let previously_polled = match persistence.assignment_scratchpads.get(&assignment.id).await {
        Ok(existing) => existing,
        Err(e) => {
            warn!(
                assignment_id = %assignment.id,
                error = %e,
                "agent watch: failed to load scratchpad; treating this poll as if it were the first"
            );
            None
        }
    };
    // Seed-on-first, mirroring `tick_connector_events`: a freshly created
    // watch's first poll would otherwise report its entire pre-existing
    // backlog as "new" and fire once for all of it — record the baseline
    // instead, same as ConnectorEvent's cursor-seeding behavior.
    let is_first_poll = previously_polled.is_none();
    let mut scratchpad = previously_polled.unwrap_or_default();

    match contract {
        Some(contract) => {
            let (candidates, extraction_path, inferred_tier, force_seed_only) = match select_agent_watch_candidates(
                detector,
                tools_registry,
                event_bus,
                assignment,
                instruction,
                connector_scope,
                contract,
                &mut scratchpad,
                extraction,
                extraction_tool,
                extraction_args,
                extraction_output_schema_declared,
            )
            .await
            {
                Ok(result) => result,
                Err(()) => {
                    // `select_agent_watch_candidates` may have already recorded
                    // a model call (`observe_via_detector`) against `scratchpad`
                    // before the observation itself failed — persist that
                    // increment now, or the spawned session's cost silently
                    // never lands in `model_calls_by_day` at all.
                    if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
                        warn!(
                            assignment_id = %assignment.id,
                            error = %e,
                            "agent watch: failed to persist scratchpad after a failed observation"
                        );
                    }
                    return false;
                }
            };
            run_contract_bound_tick(
                persistence,
                dispatcher,
                event_bus,
                assignment,
                timezone,
                contract,
                scratchpad,
                is_first_poll,
                force_seed_only,
                candidates,
                extraction_path,
                inferred_tier,
            )
            .await
        }
        None => {
            // Orphaned-fingerprint reset: `contract` is `None` here, but the
            // scratchpad may still carry state computed against a contract
            // that no longer exists on the assignment record.
            // `run_contract_bound_tick`'s own amendment-cycle clear already
            // resets this inline, in the same operation as the clear (see
            // there) — this is the fallback for every OTHER door a contract
            // can be cleared or replaced through (an assignment PATCH, an
            // `AssignmentUpdate` tool call), neither of which has scratchpad
            // access of its own. Detecting it here instead, on the next
            // poll, means the reset holds regardless of which door did the
            // clearing, without plumbing scratchpad access into either of
            // those doors. `contract_amendment_cycle_count` is reset too:
            // reaching this branch with a stale fingerprint means the
            // contract was cleared through one of those other doors — most
            // plausibly a deliberate instruction/connector_scope edit —
            // which deserves a fresh amendment budget, not one inherited
            // from whatever the old contract was stuck on.
            let mut scratchpad = scratchpad;
            if scratchpad.contract_fingerprint.is_some() {
                info!(
                    assignment_id = %assignment.id,
                    assignment_name = %assignment.name,
                    "agent watch: contract was cleared since the last poll; resetting the now-orphaned scratchpad state"
                );
                scratchpad.contract_fingerprint = None;
                scratchpad.snapshots.clear();
                scratchpad.missing_required_field_streak = 0;
                scratchpad.truncation_notified = false;
                scratchpad.authoring_failure_streak = 0;
                scratchpad.contract_amendment_cycle_count = 0;
                scratchpad.all_candidates_quarantined_streak = 0;
                scratchpad.clear_extraction_plan();
                scratchpad.extraction_plan_degraded = false;
                scratchpad.extraction_plan_degraded_reason = None;
                if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
                    warn!(
                        assignment_id = %assignment.id,
                        error = %e,
                        "agent watch: failed to persist the orphaned-fingerprint scratchpad reset"
                    );
                }
            }

            run_authoring_and_legacy_tick(
                persistence,
                dispatcher,
                event_bus,
                detector,
                assignment,
                instruction,
                connector_scope,
                timezone,
                scratchpad,
                is_first_poll,
            )
            .await
        }
    }
}

/// Produces one contract-bound tick's `AgentWatchCandidate`s — this
/// function is the "skip the per-poll LLM call" seam. A plan — either `extraction` (a manually
/// configured override on the trigger itself) or, failing that, the
/// scratchpad's own authored `extraction_plan` (see [`author_extraction_plan`])
/// — whose inferred `Tier` is `Tier::Deterministic` or `Tier::Probabilistic`
/// skips the model entirely: content comes from whatever
/// `ao_engine_tools_runner::mcp::payload_stash` last captured for
/// `(connector_scope, extraction_tool)`, resolved through
/// `extractor_contract::resolve` — zero model/child-session calls. Every
/// other case — no plan available at all, a plan whose tier is
/// `Tier::ChangeDetectionOnly`, or no `connector_scope`/`extraction_tool` to
/// key the stash lookup with — falls through to `detector.observe`,
/// byte-for-byte the same call this tick made before `extraction` existed.
///
/// The fourth tuple element is `force_seed_only`: `true` exactly when this
/// poll's candidates came from the LLM fallback *because* a previously
/// working plan just failed structurally (see [`resolve_with_plan`]) —
/// the caller must feed this into [`run_contract_bound_tick`] so that tick
/// suppresses firing, the same as a genuine first poll would, rather than
/// risk mass-firing on identity keys a mismatched plan may have gotten
/// wrong.
///
/// `Err(())` means the observation itself failed and was already logged; the
/// caller's only job is to bail out of the tick, exactly like a plain
/// detector error did before this function existed.
async fn select_agent_watch_candidates(
    detector: &Arc<dyn AgentWatchDetector>,
    // The process's real tool registry (`AppState::tools_registry`) —
    // threaded down into `resolve_with_plan` so a steady-state poll with a
    // frozen `extraction_tool`/`extraction_args` can call the connector
    // directly instead of only ever reading whatever a previous model call
    // happened to leave in the payload stash.
    tools_registry: &Arc<Registry>,
    event_bus: &Arc<EventBus>,
    assignment: &Assignment,
    instruction: &str,
    connector_scope: Option<&str>,
    contract: &WatchContract,
    scratchpad: &mut AssignmentScratchpad,
    extraction: Option<&ExtractionPlan>,
    extraction_tool: Option<&str>,
    extraction_args: Option<&Value>,
    extraction_output_schema_declared: bool,
) -> Result<(Vec<AgentWatchCandidate>, ExtractionPath, Option<Tier>, bool), ()> {
    if let Some(plan) = extraction {
        return resolve_with_plan(
            detector,
            tools_registry,
            event_bus,
            assignment,
            instruction,
            plan.clone(),
            connector_scope,
            extraction_tool,
            extraction_args,
            extraction_output_schema_declared,
            scratchpad,
        )
        .await;
    }

    let live_fingerprint = contract.fingerprint();
    let current_plan = (scratchpad.extraction_plan_fingerprint.as_deref() == Some(live_fingerprint.as_str()))
        .then(|| scratchpad.extraction_plan.clone())
        .flatten();

    if let Some(plan) = current_plan {
        return resolve_with_plan(
            detector,
            tools_registry,
            event_bus,
            assignment,
            instruction,
            plan,
            connector_scope,
            extraction_tool,
            extraction_args,
            extraction_output_schema_declared,
            scratchpad,
        )
        .await;
    }

    // No usable plan yet — either never authored, or invalidated by a
    // contract amendment (fingerprint mismatch above) or a prior
    // structural resolve() failure (`resolve_with_plan` clears both
    // `extraction_plan`/`extraction_plan_fingerprint` on that path). This
    // poll always falls back to the LLM regardless of whether authoring
    // succeeds below — a freshly authored plan freezes into the scratchpad
    // and only takes effect starting the *next* poll, the same
    // freeze-once-computed treatment `author_contract` already gives
    // `WatchContract::identity`. The model runs FIRST (rather than after
    // authoring, as this used to) so a Tier 2 authoring attempt below can
    // compare its own parse of this exact payload against what the model
    // itself already extracted from it — see `author_extraction_plan`'s doc
    // for why that comparison exists. Nothing here changes what this poll
    // itself reports: every branch below returns these same `candidates`.
    let candidates = observe_via_detector(detector, assignment, instruction, scratchpad).await?;

    if let (Some(server), Some(tool)) = (connector_scope, extraction_tool) {
        if let Some(stashed) = payload_stash::global().latest_for(server, tool) {
            let attempt = author_extraction_plan(contract, &stashed, extraction_args, &candidates);
            if let Some(plan) = attempt.plan {
                info!(
                    assignment_id = %assignment.id,
                    assignment_name = %assignment.name,
                    "agent watch: authored a new extraction plan from the current payload sample; takes effect next poll"
                );
                // Baseline for `structural_mismatch`: resolve the freshly
                // authored plan against the exact sample it was derived
                // from, so the recorded expectation is the plan's own
                // resolved item shape, not a separately-reasoned walk of the
                // raw payload. Left `None` (skipping the check on every
                // later poll until a re-author populates it) on the rare
                // case this resolve doesn't succeed against its own sample.
                if let Ok(sample_resolution) =
                    extractor_contract::resolve(&plan, stashed.json_body().as_deref(), stashed.text.as_deref())
                {
                    let shape = observed_structural_shape(sample_resolution.items.iter().map(|item| &item.value));
                    scratchpad.extraction_plan_expected_item_count = Some(shape.item_count);
                    scratchpad.extraction_plan_expected_fields = Some(shape.fields);
                }
                scratchpad.extraction_plan = Some(plan);
                scratchpad.extraction_plan_fingerprint = Some(live_fingerprint);
            } else {
                if let Some(reason) = attempt.degraded_reason {
                    // Distinct from the ordinary "nothing to author from"
                    // case just below: a Tier 2 table candidate WAS found,
                    // but its own replay of this poll's payload disagreed
                    // with what the model just extracted from the same
                    // payload, so freezing was refused (see
                    // `author_extraction_plan`'s doc) — worth a diagnosable
                    // reason on the scratchpad rather than the same silent
                    // retry-next-poll every other "no plan yet" case gets.
                    // Both fields are set together, never just the reason:
                    // `extraction_plan_degraded_reason`'s own doc requires it
                    // to be `Some` only while `extraction_plan_degraded` is
                    // `true` — setting the reason alone would hide it from
                    // `derive_extraction_health` (which branches on the bool,
                    // not the reason) and silently fall back to the generic
                    // `ModelAssisted` copy instead of this specific one.
                    scratchpad.extraction_plan_degraded = true;
                    scratchpad.extraction_plan_degraded_reason = Some(reason.clone());
                    warn!(
                        assignment_id = %assignment.id,
                        assignment_name = %assignment.name,
                        reason = %reason,
                        "agent watch: a Tier 2 tabular extraction plan was found but its own replay of this poll's \
                         payload did not match what the model just extracted from it; staying on the model path"
                    );
                }
                info!(
                    assignment_id = %assignment.id,
                    assignment_name = %assignment.name,
                    extraction_tool = %tool,
                    "agent watch: could not author an extraction plan — the frozen tool's last sampled response \
                     carried no array-shaped structured content and no single recognizable table embedded in a \
                     string field, so this watch will keep using the model-extraction path every poll until a \
                     sample with a recognizable shape is observed"
                );
            }
        } else {
            info!(
                assignment_id = %assignment.id,
                assignment_name = %assignment.name,
                extraction_tool = %tool,
                "agent watch: could not author an extraction plan — no payload sample has been observed yet for \
                 the frozen tool, so this watch will use the model-extraction path this poll and retry authoring \
                 once a sample is available"
            );
        }
    }

    Ok((candidates, ExtractionPath::Llm, None, false))
}

/// Resolves `plan` against a payload for `(connector_scope, extraction_tool)`.
/// When `extraction_args`
/// is also frozen, that payload is fetched fresh on this call via
/// [`direct_invoke_payload`] — a real connector call, not a replay of
/// whatever a previous model turn happened to leave behind — and read back
/// from the stash by its exact `(server, tool, args_hash)` key. Without
/// frozen args (a row persisted before `extraction_args` existed) this falls
/// back to the old cache read, [`payload_stash::PayloadStash::latest_for`].
///
/// Distinguishes three failure shapes deliberately:
///
/// - A direct-invoke failure (the tool isn't registered, the call itself
///   errors, or the call leaves nothing usable in the stash) means this
///   poll simply has no fresh content to resolve against — the extraction
///   plan itself is not implicated, so it is left bound for the next poll.
///   Falls back to [`observe_via_detector`], reported to
///   [`run_contract_bound_tick`] as `force_seed_only` (the model's
///   candidates this poll may be keyed differently than the plan's), and
///   `extraction_plan_degraded`/`extraction_plan_degraded_reason` record the
///   specific cause — emitted as a health event too, edge-triggered so a
///   connector stuck failing across many polls warns once, not every poll.
/// - A legitimately empty [`extractor_contract::Resolution`] (the source has
///   no items right now) is not an error at all — `resolve` itself only
///   returns `Err` when the selector/identity expression doesn't apply to
///   the payload's actual shape (see its own doc), so an `Ok` with zero
///   items is just an ordinary quiet poll and is handled identically to any
///   other `Ok`.
/// - A structural [`extractor_contract::BindError`] (the plan's path no
///   longer matches what the tool returned) means the plan has drifted from
///   the payload shape — identity keys may have shifted, so this poll falls
///   back to [`observe_via_detector`] (degraded but working, never silently
///   broken), is reported to [`run_contract_bound_tick`] as `force_seed_only`
///   (never fire on a poll whose candidates might be keyed differently than
///   the plan assumed), and the scratchpad's authored plan is invalidated
///   (cleared, not merely marked) so [`select_agent_watch_candidates`]
///   re-attempts authoring on a later poll once fresher stash data is
///   available. `extraction_plan_degraded`/`extraction_plan_degraded_reason`
///   carry the real cause so a later UI task can show *why*, not just *that*
///   — emitted as a health event too, edge-triggered so a plan stuck broken
///   across many polls warns once, not every poll.
async fn resolve_with_plan(
    detector: &Arc<dyn AgentWatchDetector>,
    tools_registry: &Arc<Registry>,
    event_bus: &Arc<EventBus>,
    assignment: &Assignment,
    instruction: &str,
    plan: ExtractionPlan,
    connector_scope: Option<&str>,
    extraction_tool: Option<&str>,
    extraction_args: Option<&Value>,
    extraction_output_schema_declared: bool,
    scratchpad: &mut AssignmentScratchpad,
) -> Result<(Vec<AgentWatchCandidate>, ExtractionPath, Option<Tier>, bool), ()> {
    // `Hash`-kind selectors always infer `Tier::ChangeDetectionOnly`
    // regardless of what content is fetched (see `infer_tier`) — this poll
    // falls through to the model no matter what, so a direct-invoke here
    // would pay for a connector call whose result is thrown away. Route
    // those straight to the plain stash cache-read below (itself unused by
    // the `ChangeDetectionOnly` branch further down, but free) instead.
    let direct_invoke_eligible = extraction_args.is_some() && !matches!(plan.selector.kind, ExtractorKind::Hash);
    let stashed = match (connector_scope, extraction_tool) {
        (Some(server), Some(tool)) if direct_invoke_eligible => {
            let args = extraction_args.expect("direct_invoke_eligible checked extraction_args.is_some()");
            match direct_invoke_payload(tools_registry, assignment, server, tool, args).await {
                Ok(payload) => Some(payload),
                Err(reason) => {
                    let already_degraded = scratchpad.extraction_plan_degraded;
                    warn!(
                        assignment_id = %assignment.id,
                        assignment_name = %assignment.name,
                        error = %reason,
                        "agent watch: direct-invoke of the frozen extraction tool failed; falling back to the \
                         model for this poll and firing nothing"
                    );
                    scratchpad.extraction_plan_degraded = true;
                    if !already_degraded {
                        emit_health_event(
                            event_bus,
                            assignment,
                            format!(
                                "Agent watch \"{}\" could not fetch fresh data from its frozen tool ({reason}) — \
                                 falling back to the model for this poll (nothing will be reported as new on it).",
                                assignment.name
                            ),
                        )
                        .await;
                    }
                    scratchpad.extraction_plan_degraded_reason = Some(reason);
                    return observe_via_detector(detector, assignment, instruction, scratchpad)
                        .await
                        .map(|c| (c, ExtractionPath::Llm, None, true));
                }
            }
        }
        (Some(server), Some(tool)) => payload_stash::global().latest_for(server, tool),
        _ => None,
    };
    // `body` may be a text-parsed rescue (`StashedPayload::json_body`) rather
    // than the server's own `structuredContent` — bound to a local so the
    // `Cow` lives long enough to borrow from below. `has_structured_content`
    // deliberately stays keyed off the REAL `stashed.structured`, never
    // `body.is_some()`: a text-rescued body did not come with a server
    // promise about its shape, so it must cap at `Tier::Probabilistic`, not
    // falsely claim `Tier::Deterministic` (see `infer_tier`'s own doc).
    let body = stashed.as_ref().and_then(|s| s.json_body());
    let text = stashed.as_ref().and_then(|s| s.text.as_deref());
    let has_structured_content = stashed.as_ref().is_some_and(|s| s.structured.is_some());
    let tier = extractor_contract::infer_tier(has_structured_content, extraction_output_schema_declared, &plan.selector.kind);

    match tier {
        Tier::Deterministic | Tier::Probabilistic => match extractor_contract::resolve(&plan, body.as_deref(), text) {
            Ok(resolution) => {
                // Only `Tier::Probabilistic` gets this check: a
                // `Tier::Deterministic` plan already has a server-declared
                // schema behind its shape, the strongest guarantee this
                // module can express, so there is no comparable drift risk
                // to guard against here — see `structural_mismatch`'s own
                // doc for what a text-rescued plan needs this for.
                if tier == Tier::Probabilistic {
                    let observed = observed_structural_shape(resolution.items.iter().map(|item| &item.value));
                    if let Some(reason) = structural_mismatch(
                        scratchpad.extraction_plan_expected_item_count,
                        scratchpad.extraction_plan_expected_fields.as_ref(),
                        &observed,
                    ) {
                        let already_degraded = scratchpad.extraction_plan_degraded;
                        warn!(
                            assignment_id = %assignment.id,
                            assignment_name = %assignment.name,
                            reason = %reason,
                            "agent watch: extraction plan's structural expectation no longer matches this poll's \
                             items; falling back to the model for this poll, firing nothing, and invalidating the \
                             plan for re-authoring"
                        );
                        scratchpad.clear_extraction_plan();
                        scratchpad.extraction_plan_degraded = true;
                        scratchpad.extraction_plan_degraded_reason = Some(reason.clone());
                        if !already_degraded {
                            emit_health_event(
                                event_bus,
                                assignment,
                                format!(
                                    "Agent watch \"{}\"'s extraction plan {reason} — falling back to the model for \
                                     this poll (nothing will be reported as new on it) and will re-author a fresh \
                                     plan once fresher data is available.",
                                    assignment.name
                                ),
                            )
                            .await;
                        }
                        // One re-author model call for this poll only — the
                        // same "the model fully handled this poll" shape the
                        // `Err(e)` branch below uses, not a retry loop: this
                        // function returns immediately after, so a later
                        // poll (with `extraction_plan` now cleared) is what
                        // attempts a fresh, deterministic re-author.
                        return observe_via_detector(detector, assignment, instruction, scratchpad)
                            .await
                            .map(|c| (c, ExtractionPath::Llm, None, true));
                    }
                }

                scratchpad.extraction_plan_degraded = false;
                scratchpad.extraction_plan_degraded_reason = None;
                let candidates: Vec<AgentWatchCandidate> =
                    resolution.items.into_iter().map(resolved_item_to_candidate).collect();
                let path =
                    if tier == Tier::Deterministic { ExtractionPath::Deterministic } else { ExtractionPath::Probabilistic };
                info!(
                    assignment_id = %assignment.id,
                    assignment_name = %assignment.name,
                    ?tier,
                    candidate_count = candidates.len(),
                    "agent watch: deterministic extraction observed candidates — no model call this poll"
                );
                Ok((candidates, path, Some(tier), false))
            }
            Err(e) => {
                let already_degraded = scratchpad.extraction_plan_degraded;
                warn!(
                    assignment_id = %assignment.id,
                    assignment_name = %assignment.name,
                    error = %e,
                    "agent watch: extraction plan no longer matches the payload shape; falling back to the model \
                     for this poll, firing nothing, and invalidating the plan for re-authoring"
                );
                scratchpad.clear_extraction_plan();
                scratchpad.extraction_plan_degraded = true;
                scratchpad.extraction_plan_degraded_reason = Some(e.to_string());
                if !already_degraded {
                    emit_health_event(
                        event_bus,
                        assignment,
                        format!(
                            "Agent watch \"{}\"'s extraction plan no longer matches what its tool returned ({e}) \
                             — falling back to the model for this poll (nothing will be reported as new on it) \
                             and will re-author a fresh plan once fresher data is available.",
                            assignment.name
                        ),
                    )
                    .await;
                }
                // The model fully handled this poll — `tier` above is only
                // the tier the plan *would* have claimed had it resolved, not
                // what actually ran. Recording it here would let a poll that
                // fell back to the model still report as deterministic
                // (`None` is the same "no tier to claim" value the no-plan-
                // configured path uses, just above).
                observe_via_detector(detector, assignment, instruction, scratchpad)
                    .await
                    .map(|c| (c, ExtractionPath::Llm, None, true))
            }
        },
        Tier::ChangeDetectionOnly => observe_via_detector(detector, assignment, instruction, scratchpad)
            .await
            .map(|c| (c, ExtractionPath::Llm, Some(tier), false)),
    }
}

/// Calls `tool` on `server` directly through `tools_registry`, using the
/// frozen `args`, and reads the resulting payload back from
/// [`payload_stash`] by its exact `(server, tool, args_hash)` key — never
/// [`payload_stash::PayloadStash::latest_for`], which has no session key and
/// could hand back a payload some other assignment or chat session left
/// behind. This is what turns a bound extraction plan into a genuine
/// per-poll fetch instead of a replay of stale cached content: the tool
/// invoked here is whatever a prior authoring poll self-reported and froze
/// onto the trigger (`set_assignment_contract`), called with the exact
/// arguments frozen alongside it.
///
/// Two things this deliberately does NOT do, matching the dispatch shape
/// `ao_mcp_bridge::handler::dispatch_one` already uses for a bare
/// lookup-then-invoke: no `pre_tool_use` hook fires, and there is no
/// invoke-time re-check of `tool` against a read-only allowlist. Safety for
/// what a watch may freeze as its extraction tool is an authoring-time
/// concern (prompt constraints plus the user-visible frozen contract), not
/// something this call site re-derives.
///
/// Every failure — the tool isn't registered under `tools_registry`, the
/// call itself returns `Err`, the call completes but the server reported an
/// error, or the call left nothing behind in the stash under this exact key
/// — comes back as `Err` with a human-readable reason. The caller's only
/// job on `Err` is to fall back to the model detector for this poll.
async fn direct_invoke_payload(
    tools_registry: &Arc<Registry>,
    assignment: &Assignment,
    server: &str,
    tool: &str,
    args: &Value,
) -> Result<payload_stash::StashedPayload, String> {
    let qualified_name = format!("mcp__{server}__{tool}");
    let io_tool = tools_registry
        .lookup_io(&qualified_name)
        .ok_or_else(|| format!("tool \"{qualified_name}\" is not registered"))?;

    let ctx = RunnerContext::new(format!("agent-watch-direct-invoke-{}", assignment.id), assignment.agent_id.clone())
        .map_err(|e| format!("failed to build a call context for \"{qualified_name}\": {e}"))?;

    match io_tool.invoke(args.clone(), &ctx).await {
        Ok(ToolOutput::Error { message, .. }) => {
            Err(format!("tool \"{qualified_name}\" returned an error: {message}"))
        }
        Ok(_) => payload_stash::global().get(server, tool, &payload_stash::hash_args(args)).ok_or_else(|| {
            format!("tool \"{qualified_name}\" call succeeded but left no usable payload to extract from")
        }),
        Err(e) => Err(format!("tool \"{qualified_name}\" call failed: {e}")),
    }
}

/// [`author_extraction_plan`]'s result. `plan` is `Some` only when a plan
/// was actually derived and — for a Tier 2 table candidate — passed its
/// replay-and-diff safety check (see that function's doc). `degraded_reason`
/// is populated on exactly one outcome: a Tier 2 table WAS found but its own
/// parse of this poll's payload didn't match what the model already
/// extracted from it, so freezing was refused. Every other "nothing to
/// author from" case (no array field, no recognizable table at all, an
/// ambiguous multi-table payload) leaves `degraded_reason` `None` — that's
/// this watch's ordinary "still no plan yet" state, not a diagnosable
/// failure.
struct AuthoringAttempt {
    plan: Option<ExtractionPlan>,
    degraded_reason: Option<String>,
}

impl AuthoringAttempt {
    fn none() -> Self {
        Self { plan: None, degraded_reason: None }
    }
}

/// Best-effort, model-free synthesis of an [`ExtractionPlan`] from one
/// sample of whatever the payload stash currently holds for a watch's
/// `extraction_tool`.
/// Tries two mechanisms, in order, each one a structural read of the sample
/// rather than another model turn:
///
/// - **Tier 1** ([`author_row_array_plan`]): the sample's JSON body is
///   already array-shaped, or a structured object with an array-typed
///   top-level field that looks like the result rows.
/// - **Tier 2** ([`author_tabular_extraction_plan`]): Tier 1 found nothing,
///   but the sample contains tabular markup (an HTML `<table>` or a
///   markdown pipe table) embedded as a literal string inside one of its
///   own fields — e.g. a `{metadata, title, url, text}`-shaped tool
///   response whose `text` field is itself rendered markup, not further
///   JSON structure. Unlike Tier 1, a Tier 2 candidate is never frozen on
///   faith: `model_candidates` is this SAME poll's own model-extracted
///   candidates (the model has already read this exact payload once before
///   this function ever runs — see `select_agent_watch_candidates`), and the
///   freshly parsed rows must agree with them, field for field, on every one
///   of `contract`'s own identity and material fields before the plan is
///   trusted enough to return. Tier 1 needs no such check: an array of
///   already-JSON row objects is either the row shape or it isn't, with no
///   markup-parsing step in between that could disagree with the model.
///
/// Returns `AuthoringAttempt { plan: None, .. }` when neither tier finds
/// anything to work with — the caller keeps falling back to the LLM
/// detector and retries authoring on a later poll once the stash sample
/// looks more promising.
///
/// `extraction_args` is the frozen tool call's own arguments — the same
/// value `resolve_with_plan` uses for a direct re-invoke. It is threaded
/// through here, but not yet read: a call's arguments can carry an
/// authoritative field list the response body alone can't (a SQL
/// projection list, a GraphQL selection set), which a future authoring
/// pass can use to derive a selector/identity more precisely than either
/// tier's own structural guess. Discarding it unread here would throw that
/// information away before it ever reaches an authoring pass.
fn author_extraction_plan(
    contract: &WatchContract,
    stashed: &payload_stash::StashedPayload,
    extraction_args: Option<&Value>,
    model_candidates: &[AgentWatchCandidate],
) -> AuthoringAttempt {
    let _ = extraction_args;
    let Some(body) = stashed.json_body() else { return AuthoringAttempt::none() };
    let structured = body.as_ref();

    if let Some(plan) = author_row_array_plan(contract, structured) {
        return AuthoringAttempt { plan: Some(plan), degraded_reason: None };
    }
    author_tabular_extraction_plan(contract, structured, model_candidates)
}

/// Tier 1: `structured` is already an array (selects the whole document), or
/// a structured object with an array-typed top-level field
/// [`select_row_shaped_array_field`] judges most likely to hold the actual
/// result rows — deeper nesting is deliberately not searched, since
/// disambiguating candidates below the top level without a model call would
/// be a guess with nothing solid to lean on. `identity` is derived by
/// [`plan_identity_for_contract`]; `predicate` reuses the contract's own
/// typed predicate verbatim, the only predicate signal available here.
fn author_row_array_plan(contract: &WatchContract, structured: &Value) -> Option<ExtractionPlan> {
    let selector_expr = match structured {
        Value::Array(_) => String::new(),
        Value::Object(map) => select_row_shaped_array_field(map)?,
        _ => return None,
    };

    Some(ExtractionPlan {
        selector: extractor_contract::Selector {
            kind: ExtractorKind::JsonPath { path: selector_expr.clone() },
            expr: selector_expr,
        },
        identity: plan_identity_for_contract(contract),
        predicate: contract.predicate.predicate.clone(),
    })
}

/// The [`ExtractorKind`] an authored plan's `identity` field gets, from
/// `contract.identity.strategy`: `NativeId` maps cleanly onto a single-field
/// [`ExtractorKind::JsonPath`] (its own `source_field`); `CompositeNative`/
/// `ContentHash` combine multiple fields, which `ExtractorKind` (outside
/// `Table`, never used for `identity`) has no variant for, so both fall back
/// to whole-item hashing instead. Harmless either way: the plan's own
/// per-item `id` is never the dedup key `run_contract_bound_tick` actually
/// fires on (see [`resolved_item_to_candidate`]'s doc) — the real identity
/// key is always recomputed downstream from `contract.identity` applied to
/// the resolved item's own `payload`, which is exactly why this fallback
/// hashing the WHOLE item (not just the declared identity fields) is safe.
fn plan_identity_for_contract(contract: &WatchContract) -> ExtractorKind {
    match contract.identity.strategy {
        IdentityStrategy::NativeId => {
            ExtractorKind::JsonPath { path: contract.identity.source_field.clone().unwrap_or_default() }
        }
        IdentityStrategy::CompositeNative | IdentityStrategy::ContentHash => ExtractorKind::Hash,
    }
}

/// Tier 2: Tier 1 found no array to select over, so this looks for the rows
/// instead embedded as tabular markup inside one of the payload's own
/// string-valued fields — see [`extractor_contract::table::discover_tabular_field`].
/// Deliberately refuses to guess: that function itself returns `None` unless
/// the WHOLE payload contains exactly one recognizable table, so an
/// ambiguous or table-less payload falls through to `AuthoringAttempt::none()`
/// here exactly like Tier 1's own "no array field" case.
///
/// A found table is never frozen on faith — see [`tabular_replay_mismatch_reason`]
/// for the comparison against `model_candidates` (this same poll's own
/// model-extracted candidates) that gates it. A mismatch carries its reason
/// back in [`AuthoringAttempt::degraded_reason`] instead of silently
/// discarding it, so a watch that keeps failing this check is diagnosable
/// rather than mysteriously stuck on the model path forever.
///
/// `contract`'s own identity fields ([`contract_identity_fields`], normalized
/// the same way a table's header cells are) are threaded into
/// [`extractor_contract::table::discover_tabular_field`] so a
/// header-adjacent blank/placeholder row — real markup Notion and similar
/// tools leave behind for template rows — never counts as a data row here,
/// and are frozen onto the authored plan's own `identity_columns` so every
/// later poll's replay (`extractor_contract::resolve`) filters the exact
/// same rows, not just this one authoring pass.
fn author_tabular_extraction_plan(
    contract: &WatchContract,
    structured: &Value,
    model_candidates: &[AgentWatchCandidate],
) -> AuthoringAttempt {
    let identity_columns = contract_identity_fields(contract);
    let Some(table) =
        extractor_contract::table::discover_tabular_field(structured, TABLE_DISCOVERY_MAX_DEPTH, &identity_columns)
    else {
        return AuthoringAttempt::none();
    };

    if let Some(reason) = tabular_replay_mismatch_reason(&table.rows, model_candidates, contract) {
        return AuthoringAttempt { plan: None, degraded_reason: Some(reason) };
    }

    let plan = ExtractionPlan {
        selector: extractor_contract::Selector {
            kind: ExtractorKind::Table {
                field_path: table.field_path.clone(),
                columns: table.columns,
                identity_columns,
            },
            expr: table.field_path,
        },
        identity: plan_identity_for_contract(contract),
        predicate: contract.predicate.predicate.clone(),
    };
    AuthoringAttempt { plan: Some(plan), degraded_reason: None }
}

/// Bound on how deep [`extractor_contract::table::discover_tabular_field`]
/// walks a payload's own object/array nesting looking for a string field —
/// the same default depth [`extractor_contract::enumerate_paths`] itself
/// uses for a shape report.
const TABLE_DISCOVERY_MAX_DEPTH: usize = 6;

/// `None` when `parsed_rows` (this poll's Tier 2 parse) agrees with
/// `model_candidates` (this same poll's own model-extracted candidates) on
/// the SET of row identities — order-insensitive, so neither side's read
/// order has to match the other's — and, for every identity present on both
/// sides, on every one of `contract`'s material field values too;
/// `Some(reason)` names exactly what disagreed, for
/// [`AuthoringAttempt::degraded_reason`].
///
/// Comparing SETS of identity tuples rather than a bare row count is the
/// whole point of this check: two extractions can agree on *how many* rows
/// they found while disagreeing on *which* rows those are — wrong entities
/// entirely, not merely a miscount — and a set comparison catches that
/// (mismatched cardinality included; two multisets of different sizes are
/// never equal) without assuming either side reads rows in the same order.
/// Comparison is by field NAME after the same normalization
/// `discover_tabular_field` applies to a table's header cells (so e.g. a
/// contract field named "Company" and a parsed column key "company" are
/// treated as the same field) and by value after trimming surrounding
/// whitespace — never by raw JSON key order, which carries no meaning for
/// either side.
///
/// Once the identity sets agree, each parsed row's material fields are
/// still checked against its own identity-matched model candidate — an
/// identity-only comparison would pass a parser that names every row
/// correctly but garbles a material value, which is exactly the "wrong
/// rows" failure mode this whole gate exists to catch (see
/// `author_extraction_plan_does_not_freeze_a_tier_2_plan_on_a_replay_mismatch`).
fn tabular_replay_mismatch_reason(
    parsed_rows: &[Value],
    model_candidates: &[AgentWatchCandidate],
    contract: &WatchContract,
) -> Option<String> {
    let identity_fields = contract_identity_fields(contract);
    if identity_fields.is_empty() {
        return Some("the contract declares no identity fields to replay-check against".to_string());
    }

    let parsed_identities: Vec<Vec<String>> =
        parsed_rows.iter().map(|row| identity_tuple(row, &identity_fields)).collect();
    let model_identities: Vec<Vec<String>> =
        model_candidates.iter().map(|candidate| identity_tuple(&candidate.payload, &identity_fields)).collect();

    let mut parsed_sorted = parsed_identities.clone();
    let mut model_sorted = model_identities.clone();
    parsed_sorted.sort();
    model_sorted.sort();
    if parsed_sorted != model_sorted {
        return Some(format!(
            "the table parser's row identities disagree with the model's for the same payload: parser produced {} \
             but the model produced {}",
            format_identity_sample(&parsed_identities),
            format_identity_sample(&model_identities),
        ));
    }

    let material_fields = &contract.change.material_fields;
    let model_by_identity: HashMap<&[String], &AgentWatchCandidate> =
        model_candidates.iter().zip(model_identities.iter()).map(|(candidate, id)| (id.as_slice(), candidate)).collect();

    for (row, identity) in parsed_rows.iter().zip(parsed_identities.iter()) {
        // The identity-set check above already proved `identity` has a
        // match on the model side (both sides are the same multiset); a
        // duplicate identity on either side can make this particular lookup
        // land on a different (but, per that same check, value-identical)
        // duplicate, which is harmless for what this loop checks.
        let Some(candidate) = model_by_identity.get(identity.as_slice()) else { continue };
        for field in material_fields {
            let parsed_value = normalized_field_value(row, field);
            let model_value = normalized_field_value(&candidate.payload, field);
            if parsed_value != model_value {
                return Some(format!(
                    "row with identity {identity:?}: field \"{field}\" parsed as {parsed_value:?} but the model \
                     extracted {model_value:?} for the same row"
                ));
            }
        }
    }

    None
}

/// `contract`'s own identity field names — `identity.source_field` for
/// `NativeId`, `identity.fields` for the composite strategies — normalized
/// the same way [`extractor_contract::table::discover_tabular_field`]
/// normalizes a table's header cells, so they line up directly against a
/// parsed row's or a model candidate's own field keys. Always non-empty for
/// a bound `WatchContract` (`WatchContract::validate` rejects an empty
/// identity — `ContractError::EmptyIdentity` — before a contract is ever
/// bound); the empty-list branch callers check for is defense in depth, not
/// a case this function expects to hit in practice.
fn contract_identity_fields(contract: &WatchContract) -> Vec<String> {
    let raw: Vec<String> = match contract.identity.strategy {
        IdentityStrategy::NativeId => contract.identity.source_field.iter().cloned().collect(),
        IdentityStrategy::CompositeNative | IdentityStrategy::ContentHash => contract.identity.fields.clone(),
    };
    let mut seen = HashSet::new();
    raw.into_iter().map(|field| extractor_contract::table::normalize_column_key(&field)).filter(|field| seen.insert(field.clone())).collect()
}

/// The ordered tuple of `identity_fields`' values out of `value` (a parsed
/// Tier 2 row, or a model candidate's own `payload`) — the unit
/// [`tabular_replay_mismatch_reason`] compares as a set. A field
/// [`normalized_field_value`] can't find in `value` becomes the literal
/// string `"<missing>"` rather than shortening the tuple, so a row silently
/// missing an identity field still produces a tuple the same length as its
/// peers — visibly wrong instead of quietly absent, and never spuriously
/// "equal" to a genuinely shorter tuple.
fn identity_tuple(value: &Value, identity_fields: &[String]) -> Vec<String> {
    identity_fields
        .iter()
        .map(|field| normalized_field_value(value, field).unwrap_or_else(|| "<missing>".to_string()))
        .collect()
}

/// Bound on how many identity tuples [`format_identity_sample`] names
/// individually before folding the remainder into a "+N more" tail — a
/// mismatch reason belongs in a log line to be skimmed, not a dump of a
/// whole (potentially large) table.
const IDENTITY_SAMPLE_LIMIT: usize = 5;

/// Renders `identities` as `"<count> row(s): [a, b]; [c, d] (+N more)"` for
/// [`tabular_replay_mismatch_reason`]'s mismatch message — enough for the
/// next person to see what each side actually produced without a forensic
/// investigation, bounded by [`IDENTITY_SAMPLE_LIMIT`] so a large table's
/// mismatch doesn't turn one log line into the whole payload.
fn format_identity_sample(identities: &[Vec<String>]) -> String {
    let sample: Vec<String> =
        identities.iter().take(IDENTITY_SAMPLE_LIMIT).map(|tuple| format!("[{}]", tuple.join(", "))).collect();
    let mut text = format!("{} row(s): {}", identities.len(), sample.join("; "));
    if identities.len() > IDENTITY_SAMPLE_LIMIT {
        text.push_str(&format!(" (+{} more)", identities.len() - IDENTITY_SAMPLE_LIMIT));
    }
    text
}

/// Looks up `field` in `value` (a JSON object) after normalizing both the
/// target field name and every one of `value`'s own keys through
/// [`extractor_contract::table::normalize_column_key`] — so a Tier 2 row's
/// own normalized column key and a `WatchContract`'s declared field name are
/// compared as the same field even when their raw text formatting differs.
/// Returns the matched value as text: a string value trimmed as-is, any
/// other JSON value via its own canonical text form (numbers/bools/null all
/// stringify consistently either way). `None` when `value` isn't an object,
/// or `field` matches none of its keys.
fn normalized_field_value(value: &Value, field: &str) -> Option<String> {
    let object = value.as_object()?;
    let target = extractor_contract::table::normalize_column_key(field);
    object.iter().find(|(key, _)| extractor_contract::table::normalize_column_key(key) == target).map(|(_, v)| {
        match v {
            Value::String(s) => s.trim().to_string(),
            other => other.to_string(),
        }
    })
}

/// The structural shape a sample of resolved items presents: how many items,
/// and the union of top-level field names across whichever of them are
/// objects (a non-object item — a bare string or number a selector picked
/// out — contributes no field names, not an error). Recorded once at
/// authoring time as `scratchpad.extraction_plan_expected_item_count`/
/// `extraction_plan_expected_fields`, and recomputed from each later
/// `Tier::Probabilistic` poll's own resolution for [`structural_mismatch`]
/// to compare against — see that fn's doc for why this check exists at all.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StructuralShape {
    item_count: usize,
    fields: BTreeSet<String>,
}

fn observed_structural_shape<'a>(items: impl Iterator<Item = &'a Value>) -> StructuralShape {
    let mut fields = BTreeSet::new();
    let mut item_count = 0;
    for item in items {
        item_count += 1;
        if let Some(map) = item.as_object() {
            fields.extend(map.keys().cloned());
        }
    }
    StructuralShape { item_count, fields }
}

/// Compares one `Tier::Probabilistic` poll's freshly resolved items against
/// the structural baseline recorded when its plan was authored
/// (`scratchpad.extraction_plan_expected_item_count`/`extraction_plan_expected_fields`,
/// set in [`select_agent_watch_candidates`]'s authoring branch). `None`
/// means either the shapes agree, or `expected` has nothing recorded to
/// compare against yet (a plan authored before this check existed) — in
/// both cases there is nothing to flag. `Some(reason)` names exactly what
/// changed, for `resolve_with_plan`'s caller to both persist as
/// `extraction_plan_degraded_reason` and show the user — never a generic
/// "shape changed" placeholder.
///
/// This is the safety net a server-declared schema doesn't need but a
/// text-rescued plan does: `extractor_contract::resolve` only fails when
/// the *selector* stops matching, never when an already-selected item
/// quietly drops or renames a field the plan's `identity`/`predicate`
/// reads — so a plan can keep "succeeding," silently mis-keying every item,
/// long after its target's real shape has drifted underneath it.
///
/// Two independent triggers, deliberately narrow so a watch's ordinary job —
/// noticing new items — never trips this itself:
/// - The observed field-name set gained or lost fields relative to the
///   baseline. Item *count* changing is not checked here; a watch finding
///   more (or fewer) items than its authoring sample had is the normal,
///   expected outcome of watching something, not a structural drift signal.
/// - The baseline was authored against a non-empty sample, but this poll's
///   selector resolved to zero items — a coarser collapse signal a plain
///   field-set diff can't see (an empty set of items trivially has an
///   empty field-name union, indistinguishable from "no fields changed").
fn structural_mismatch(
    expected_item_count: Option<usize>,
    expected_fields: Option<&BTreeSet<String>>,
    observed: &StructuralShape,
) -> Option<String> {
    let expected_fields = expected_fields?;
    let expected_item_count = expected_item_count?;

    let gained: Vec<String> = observed.fields.difference(expected_fields).cloned().collect();
    let lost: Vec<String> = expected_fields.difference(&observed.fields).cloned().collect();
    if !gained.is_empty() || !lost.is_empty() {
        let mut change = Vec::new();
        if !gained.is_empty() {
            change.push(format!("gained {}", quote_and_join(&gained)));
        }
        if !lost.is_empty() {
            change.push(format!("lost {}", quote_and_join(&lost)));
        }
        return Some(format!(
            "the extraction plan's field set {} since it was authored, so its identity or predicate fields may no \
             longer mean what they did",
            change.join(" and ")
        ));
    }

    if expected_item_count > 0 && observed.item_count == 0 {
        return Some(format!(
            "the extraction plan was authored against a sample of {expected_item_count} item(s), but this poll's \
             selector resolved zero — the source may have changed shape"
        ));
    }

    None
}

fn quote_and_join(names: &[String]) -> String {
    names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", ")
}

/// Picks which top-level field of an object-shaped payload holds "the
/// items," when more than one top-level field is array-typed —
/// [`author_extraction_plan`]'s only source of ambiguity. A tool that
/// queries a collection routinely returns its result rows alongside
/// same-level metadata that also happens to be an array (which underlying
/// sources were queried, a list of warnings, a page-token stack), so picking
/// arbitrarily — e.g. whichever key an object's iteration order happens to
/// surface first — has no reason to land on the real rows over a metadata
/// neighbor, and a JSON object's own key order is not something this plan
/// can rely on in the first place. Real result rows are themselves objects;
/// an auxiliary array of ids, flags, or tokens is not — so this prefers
/// whichever top-level array's elements are objects, and only falls back to
/// iteration order (first field encountered) when no field qualifies, or
/// more than one does.
fn select_row_shaped_array_field(map: &serde_json::Map<String, Value>) -> Option<String> {
    let mut best: Option<(&String, bool)> = None;
    for (key, value) in map {
        let Some(items) = value.as_array() else { continue };
        let has_object_items = items.iter().any(Value::is_object);
        match best {
            Some((_, true)) => {}
            Some((_, false)) if !has_object_items => {}
            _ => best = Some((key, has_object_items)),
        }
    }
    best.map(|(key, _)| key.clone())
}

/// Today's UTC calendar date as `YYYY-MM-DD`, the key
/// [`AssignmentScratchpad::record_model_call`] buckets by.
fn today_utc() -> String {
    Utc::now().date_naive().to_string()
}

tokio::task_local! {
    /// Ambient per-call tally an in-flight [`AgentWatchDetector`] session
    /// reports its real provider-turn count through, scoped by
    /// [`observe_and_count_extra_model_calls`] around each of this module's
    /// detector-invoking call sites.
    ///
    /// This exists because a single `detector.observe`/`observe_for_authoring`
    /// call is not always one real model invocation: [`LiveAgentWatchDetector`]'s
    /// `Api`-mode session (`observe_via_native_session`) can spend several
    /// provider turns before its final reply — e.g. one turn to call an MCP
    /// tool, another to report findings — and every one of those turns costs
    /// real provider money. Counting "one detector call" as "one model call"
    /// (the bug this tally fixes) silently understated the true per-watch
    /// cost by however many extra turns a session actually took.
    ///
    /// Deliberately NOT instance state on [`LiveAgentWatchDetector`] itself:
    /// one detector instance is shared (`Arc<dyn AgentWatchDetector>`) across
    /// every assignment's tick, so a mutable "last call's turns" field there
    /// would race across concurrent assignments. A task-local, freshly scoped
    /// per call by [`observe_and_count_extra_model_calls`], carries the count
    /// back to exactly the caller that started this specific call — no
    /// shared state, no race, and no change needed to the
    /// [`AgentWatchDetector`] trait's signature (every test fake — there are
    /// dozens — keeps working unmodified, since a call made outside this
    /// tally's scope just leaves it untouched).
    static MODEL_CALL_TURN_TALLY: Arc<std::sync::atomic::AtomicU32>;
}

/// Runs `fut` (a `detector.observe`/`observe_for_authoring` call) with a
/// fresh [`MODEL_CALL_TURN_TALLY`] scoped to just this call, and returns
/// however many EXTRA real provider turns — beyond the first — the call
/// reported spending, alongside its own result.
///
/// Every one of this module's four detector-invoking call sites already
/// calls [`AssignmentScratchpad::record_model_call`] BEFORE attempting the
/// call, as a crash-safe floor of (at least) one spawn — see that method's
/// own doc for why recording eagerly, before the risk, matters. This
/// function's result is what those same call sites add on top of that floor
/// via [`AssignmentScratchpad::record_additional_model_calls`] once the call
/// returns and the real turn count (if any was reported) is known. `0` for
/// any call that never reports into the tally at all — every test fake, and
/// [`LiveAgentWatchDetector`]'s own `Cli`-mode path (`observe_via_profile_runner`),
/// which has no per-turn visibility into the external process it spawns —
/// so those callers correctly add nothing on top of the pre-call floor,
/// exactly today's behavior.
async fn observe_and_count_extra_model_calls<Fut, T>(fut: Fut) -> (T, u32)
where
    Fut: std::future::Future<Output = T>,
{
    let tally = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let result = MODEL_CALL_TURN_TALLY.scope(Arc::clone(&tally), fut).await;
    let turns = tally.load(std::sync::atomic::Ordering::Relaxed);
    (result, turns.saturating_sub(1))
}

/// Today's entry in `scratchpad.model_calls_by_day`, `0` if this watch has
/// spawned no model session yet today — the same [`today_utc`] key
/// [`AssignmentScratchpad::record_model_call`] writes under, so this always
/// reads back exactly what today's polls have written, never a second,
/// independently-computed date format.
pub fn model_calls_today(scratchpad: &AssignmentScratchpad) -> u32 {
    scratchpad.model_calls_by_day.get(&today_utc()).copied().unwrap_or(0)
}

/// The [`ExtractionHealth::ModelAssisted`] reason shown to the user —
/// specific about *why* every poll is paying full model price, not a generic
/// "unavailable" placeholder, per [`derive_extraction_health`]'s own doc.
fn model_assisted_health_reason(extraction_tool: Option<&str>) -> String {
    match extraction_tool {
        Some(tool) => format!(
            "The frozen tool \"{tool}\" has never returned array-shaped structured content this watch can \
             author a deterministic extraction plan from, so a model reads its output on every poll."
        ),
        None => "No deterministic extraction plan is bound for this watch yet, so a model reads its output on \
                  every poll."
            .to_string(),
    }
}

/// Derives the API-facing [`ExtractionHealth`] (and, for every state but
/// [`ExtractionHealth::Deterministic`]/[`ExtractionHealth::Pending`], a
/// human-readable reason) for one `AgentWatch` assignment. Called from the
/// assignment API response layer (`ao_server::routes::assignments::watch_health_for`)
/// — kept here, next to [`select_agent_watch_candidates`] and
/// [`resolve_with_plan`] whose branches it summarizes, rather than in that
/// crate, so this module's own tests can exercise it directly.
///
/// `scratchpad` is the watch's persisted [`AssignmentScratchpad`], if any;
/// `extraction_tool`/`extraction_configured` are read straight off the
/// trigger — the same two inputs [`select_agent_watch_candidates`] itself
/// consults, `extraction_configured` being whether the trigger's own
/// `extraction` override (checked first, before the scratchpad's authored
/// plan) is set.
///
/// - [`ExtractionHealth::Pending`]: `scratchpad` is `None` — no poll has
///   completed yet, so nothing about extraction is known yet.
/// - [`ExtractionHealth::Degraded`]: `scratchpad.extraction_plan_degraded` is
///   `true` — a plan existed and direct-invoke (or plan resolution) failed;
///   the watch fell back to the model. Checked first among the "a poll has
///   completed" states, matching [`resolve_with_plan`]'s fail-open behavior,
///   which this must never mask behind a healthier-looking state.
/// - [`ExtractionHealth::Deterministic`]: a plan is persisted (the trigger's
///   own `extraction` override, or `scratchpad.extraction_plan`) and the
///   watch is not degraded — direct-invoke resolves every poll with zero
///   model calls.
/// - [`ExtractionHealth::ModelAssisted`]: every other case once at least one
///   poll has completed — no plan is persisted, so `observe_via_detector`
///   runs a full model child session every poll. Deliberately the catch-all
///   here rather than narrowed to "frozen tool, no plan" (the scenario that
///   motivated this enum): a watch still mid-authoring, or one whose
///   authoring reply never self-reported a tool at all, pays exactly the
///   same per-poll model cost and deserves the same non-healthy signal.
pub fn derive_extraction_health(
    scratchpad: Option<&AssignmentScratchpad>,
    extraction_tool: Option<&str>,
    extraction_configured: bool,
) -> (ExtractionHealth, Option<String>) {
    let Some(scratchpad) = scratchpad else {
        return (ExtractionHealth::Pending, None);
    };
    if scratchpad.extraction_plan_degraded {
        return (ExtractionHealth::Degraded, scratchpad.extraction_plan_degraded_reason.clone());
    }
    let plan_persisted = extraction_configured || scratchpad.extraction_plan.is_some();
    if plan_persisted {
        return (ExtractionHealth::Deterministic, None);
    }
    (ExtractionHealth::ModelAssisted, Some(model_assisted_health_reason(extraction_tool)))
}

/// Relocated to [`ao_protocol::assignment_scratchpad::WatchContractStatus`]
/// so `ao_protocol::assignment::QuiescenceReason` can wrap it without
/// `ao-protocol` depending on `ao-engine` (see that type's doc for the full
/// reasoning). Re-exported from this original path so every existing
/// `ao_engine::agent_watch::WatchContractStatus` import keeps resolving
/// unchanged.
pub use ao_protocol::assignment_scratchpad::WatchContractStatus;

/// Derives [`WatchContractStatus`] for one `AgentWatch` assignment from
/// exactly two inputs: whether a `WatchContract` is currently bound, and its
/// persisted `AssignmentScratchpad`, if any. Pure and synchronous — see
/// [`WatchContractStatus`]'s own doc for why this exists and what each
/// variant means.
pub fn derive_watch_contract_status(
    contract: Option<&WatchContract>,
    scratchpad: Option<&AssignmentScratchpad>,
) -> WatchContractStatus {
    if contract.is_some() {
        return WatchContractStatus::Bound {
            bound_after_repairs: scratchpad.and_then(|s| s.contract_bound_after_failed_attempts),
        };
    }
    match scratchpad {
        Some(scratchpad) if scratchpad.authoring_failure_streak > 0 => WatchContractStatus::AuthoringRejected {
            attempts: scratchpad.authoring_failure_streak,
            ceiling_hit: scratchpad.authoring_failure_streak >= AUTHORING_FAILURE_CEILING,
            last_rejection_reason: scratchpad.last_authoring_rejection_reason.clone(),
        },
        _ => WatchContractStatus::NotYetAttempted,
    }
}

/// Shared `detector.observe` call + failure logging, used by every
/// [`select_agent_watch_candidates`] branch that falls back to the model.
/// Records the spawn on `scratchpad` before calling the detector — this is
/// the choke point every contract-bound path that actually spawns a
/// full-price model child session passes through, which is what makes it
/// the right place to count one (see `AssignmentScratchpad::model_calls_by_day`'s
/// doc for why a poll that skips the model must not increment it) — and,
/// once the call returns, tops that count up with however many EXTRA real
/// provider turns it actually spent (see [`observe_and_count_extra_model_calls`]),
/// so a session that took a tool-use round trip before replying is not
/// undercounted as a single call.
async fn observe_via_detector(
    detector: &Arc<dyn AgentWatchDetector>,
    assignment: &Assignment,
    instruction: &str,
    scratchpad: &mut AssignmentScratchpad,
) -> Result<Vec<AgentWatchCandidate>, ()> {
    scratchpad.record_model_call(&today_utc());
    let (result, extra_turns) = observe_and_count_extra_model_calls(detector.observe(assignment, instruction)).await;
    scratchpad.record_additional_model_calls(&today_utc(), extra_turns);
    result.map_err(|e| {
        warn!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            error = %e,
            "agent watch: observation FAILED; will retry next interval"
        );
    })
}

/// Maps one `extractor_contract::resolve` finding onto the shape
/// `run_contract_bound_tick` already diffs `AgentWatchDetector::observe`
/// candidates through. `matched` (whether `ExtractionPlan::predicate` held
/// for this item, evaluated with no prior-poll history) is deliberately
/// dropped here rather than used to filter — the fire/skip decision for a
/// contract-bound watch is `WatchContract::predicate`'s job exactly as it
/// was before extraction plans existed (it has real transition history to
/// evaluate `Predicate::Changed` against; `ExtractionPlan::predicate` does
/// not), so every resolved item becomes a candidate, matched or not.
fn resolved_item_to_candidate(item: extractor_contract::ResolvedItem) -> AgentWatchCandidate {
    let summary = match &item.value {
        Value::String(s) if !s.trim().is_empty() => s.clone(),
        _ => item.id.clone(),
    };
    AgentWatchCandidate { id: item.id, summary, payload: item.value }
}

/// Unit-separator join of `instruction` and `connector_scope`, so a change
/// to either (or a shift in where one ends and the other begins) can be
/// detected by simple inequality against a previously stored key — the same
/// technique `ao_protocol::assignment_scratchpad::delivery_key` uses, just
/// unhashed since this value only ever needs comparing, never displaying, and
/// staying human-readable in the persisted scratchpad JSON is worth more
/// than the hash's collision resistance would buy here.
fn authoring_input_key(instruction: &str, connector_scope: Option<&str>) -> String {
    format!("{instruction}\u{1f}{}", connector_scope.unwrap_or(""))
}

/// `contract: None` branch of [`run_agent_watch_tick`]:
/// runs the tick's authoring pass — [`run_authoring_attempts`], capped at
/// [`AUTHORING_FAILURE_CEILING`] consecutive failing polls (see below) —
/// then falls through to [`run_legacy_seen_ids_tick`] using that pass's
/// candidates, completely unchanged from before authoring existed.
///
/// This is deliberately NOT an either/or between "author" and "fire" while
/// still below the ceiling: an authoring attempt never changes whether this
/// tick fires, only `run_legacy_seen_ids_tick`'s own `seed_only`/`seen_ids`
/// diff does that. That is what keeps "nothing fires on an authoring run
/// beyond the existing first-poll baseline behaviour" true on a watch's
/// actual first poll, while avoiding a notification gap on every later poll
/// where authoring keeps failing validation and retrying — such a watch
/// degrades to (never below) its pre-contract behavior until authoring
/// finally succeeds and `run_contract_bound_tick` takes over.
///
/// `scratchpad.authoring_failure_streak` tracks consecutive polls that ended
/// without a bound contract. Below [`AUTHORING_FAILURE_CEILING`], every poll
/// still runs a fresh authoring pass; the poll that first reaches the
/// ceiling also emits a health event (extending the same
/// `AgentEventPayload::SystemMessage` channel [`reject_proposal`] already
/// uses) naming the last validation error, so a watch that can never bind
/// shows up as visibly unhealthy instead of quietly retrying forever. Once
/// at or past the ceiling, later polls stop asking the model for a proposal
/// altogether and fall back to a plain [`AgentWatchDetector::observe`] — and,
/// unlike a below-ceiling poll, are passed to [`run_legacy_seen_ids_tick`]
/// with `seed_only` forced on. A watch that can't author a contract has no
/// stable identity to diff on: its candidates still come from the model's
/// free-text reply, whose `id` field the model is explicitly told is
/// disposable (see [`AgentWatchCandidate::id`]'s own doc), so diffing it
/// once authoring has given up would just re-fire on the same underlying
/// items forever. Forcing seed-only re-baselines every frozen poll without
/// notifying — quiet, not silent: the watch is already flagged unhealthy
/// above, and resumes real diffing the moment authoring succeeds or the
/// input-change escape hatch below fires.
///
/// The poll that first pushes the streak to the ceiling takes the
/// below-ceiling branch (the streak snapshot the branch above reads is taken
/// before that poll's own outcome is folded in), yet must get the exact same
/// `seed_only` treatment as every later frozen poll — it is, after all, the
/// poll that just proved this watch has no stable identity to diff on.
/// `seed_only` below is therefore computed from `scratchpad.authoring_failure_streak`
/// AFTER this poll's outcome has been applied, not from the pre-poll
/// snapshot the branch decision above uses — the two reads are deliberately
/// different moments of the same field.
///
/// A watch stuck at the ceiling still has a way out: every poll compares
/// [`authoring_input_key`] of the assignment's *current* `instruction`/
/// `connector_scope` against `scratchpad.authoring_input_fingerprint` (the
/// pair the streak was last measured against). A never-bound watch has no
/// `contract_fingerprint` for the orphaned-fingerprint reset in
/// [`run_agent_watch_tick`] to key off of — that field is only ever set once
/// a contract binds — so this is the pre-bind equivalent: a mismatch means
/// the instruction or connector scope was edited since the ceiling was hit,
/// which both resets `authoring_failure_streak` (so this same poll makes a
/// real authoring attempt again, not just the next one) and emits a health
/// event, so an edit that lifts the freeze is as visible as the freeze
/// itself was.
async fn run_authoring_and_legacy_tick(
    persistence: &Arc<PersistenceLayer>,
    dispatcher: &Arc<dyn NotificationDispatcher>,
    event_bus: &Arc<EventBus>,
    detector: &Arc<dyn AgentWatchDetector>,
    assignment: &Assignment,
    instruction: &str,
    connector_scope: Option<&str>,
    timezone: Option<&str>,
    mut scratchpad: AssignmentScratchpad,
    is_first_poll: bool,
) -> bool {
    // Purely informational (a later UI task displays these): no `WatchContract`
    // is bound yet on this path, so there is no contract-bound dedup context
    // for an extraction plan to feed — see `ExtractionPath::Unbound`.
    scratchpad.last_extraction_path = ExtractionPath::Unbound;
    scratchpad.last_inferred_tier = None;

    let live_authoring_key = authoring_input_key(instruction, connector_scope);
    let authoring_input_changed =
        scratchpad.authoring_input_fingerprint.as_deref() != Some(live_authoring_key.as_str());
    if authoring_input_changed && scratchpad.authoring_failure_streak >= AUTHORING_FAILURE_CEILING {
        info!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            "agent watch: instruction or connector scope changed while authoring was frozen at the ceiling; \
             resuming authoring"
        );
        emit_health_event(
            event_bus,
            assignment,
            format!(
                "Agent watch \"{}\" had stopped re-prompting after repeated authoring failures — its \
                 instruction or connector scope has changed, so authoring is resuming on this poll.",
                assignment.name
            ),
        )
        .await;
        scratchpad.authoring_failure_streak = 0;
        scratchpad.last_authoring_rejection_reason = None;
        scratchpad.authoring_rejection_history.clear();
    }
    scratchpad.authoring_input_fingerprint = Some(live_authoring_key);

    // Captured once, before the branch below can further mutate the streak,
    // so it reflects exactly which branch ran this poll: frozen (plain
    // `observe`, no stable identity to diff on) or still-authoring (below the
    // ceiling, streak untouched by the input-change reset above).
    let frozen_at_ceiling = scratchpad.authoring_failure_streak >= AUTHORING_FAILURE_CEILING;

    let candidates = if frozen_at_ceiling {
        scratchpad.record_model_call(&today_utc());
        let (observed, extra_turns) = observe_and_count_extra_model_calls(detector.observe(assignment, instruction)).await;
        scratchpad.record_additional_model_calls(&today_utc(), extra_turns);
        match observed {
            Ok(candidates) => candidates,
            Err(e) => {
                warn!(
                    assignment_id = %assignment.id,
                    assignment_name = %assignment.name,
                    error = %e,
                    "agent watch: observation FAILED; will retry next interval"
                );
                // The model call recorded just above already happened (and
                // cost money) even though the observation itself failed —
                // persist that increment now rather than dropping it on the
                // floor along with the rest of this poll.
                if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
                    warn!(
                        assignment_id = %assignment.id,
                        error = %e,
                        "agent watch: failed to persist scratchpad after a failed observation"
                    );
                }
                return false;
            }
        }
    } else {
        let (candidates, outcome) =
            match run_authoring_attempts(persistence, event_bus, detector, assignment, instruction, &mut scratchpad)
                .await
            {
                Ok(result) => result,
                Err(()) => {
                    // `run_authoring_attempts` records a model call for every
                    // authoring attempt it makes, including the one whose
                    // observation just failed — persist that onto `scratchpad`
                    // now, or the attempt's cost never reaches `model_calls_by_day`.
                    if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
                        warn!(
                            assignment_id = %assignment.id,
                            error = %e,
                            "agent watch: failed to persist scratchpad after a failed authoring observation"
                        );
                    }
                    return false;
                }
            };

        match outcome {
            AuthoringPassOutcome::Bound { same_tick_repairs } => {
                // Every rejected proposal that preceded this bind: however
                // many PRIOR POLLS failed (the streak, read before it's
                // zeroed just below) plus however many same-tick repairs
                // this poll itself burned through. `0` (bound cleanly, first
                // try, first poll) reports as `None` — there is nothing to
                // tell the user was "repaired."
                let total_repairs = scratchpad.authoring_failure_streak + same_tick_repairs;
                scratchpad.contract_bound_after_failed_attempts = (total_repairs > 0).then_some(total_repairs);
                scratchpad.authoring_failure_streak = 0;
                scratchpad.last_authoring_rejection_reason = None;
                scratchpad.authoring_rejection_history.clear();
            }
            AuthoringPassOutcome::NotBound { last_error, rejection_history } => {
                // No live contract exists to report convergence for — clears
                // any note left by a now-orphaned contract this same watch
                // bound and later lost (see
                // `AssignmentScratchpad::invalidate_watch_contract_state`'s
                // doc for the other half of that lifecycle).
                scratchpad.contract_bound_after_failed_attempts = None;
                scratchpad.authoring_failure_streak += 1;
                // Only overwrite when this poll actually produced a fresh
                // reason — a poll where the child offered no proposal at all
                // (`last_error: None`) has nothing new to report, and must
                // not blank out a real reason a previous poll already
                // recorded (that reason is still the best guidance the next
                // attempt has).
                if let Some(reason) = &last_error {
                    scratchpad.last_authoring_rejection_reason = Some(reason.clone());
                }
                // `rejection_history` is `run_authoring_attempts`'s seed
                // (`scratchpad.authoring_rejection_history` as it stood
                // before this poll) plus anything this poll's own attempts
                // added — always safe to write back even when nothing new
                // was added, since it then equals what was already there.
                scratchpad.authoring_rejection_history = rejection_history;
                if scratchpad.authoring_failure_streak == AUTHORING_FAILURE_CEILING {
                    emit_health_event_with_severity(
                        event_bus,
                        assignment,
                        format!(
                            "Agent watch \"{}\" could not author a working contract after \
                             {AUTHORING_FAILURE_CEILING} consecutive polls and will stop re-prompting until its \
                             instruction or connector scope is edited — editing either resumes authoring on the \
                             next poll; this watch cannot bind and needs manual attention. Last validation error: \
                             {}",
                            assignment.name,
                            last_error.as_deref().unwrap_or("no contract was proposed on any of those polls"),
                        ),
                        Some(SystemMessageSeverity::Error),
                    )
                    .await;
                }
            }
        }

        if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
            warn!(
                assignment_id = %assignment.id,
                error = %e,
                "agent watch: failed to persist the authoring failure streak"
            );
        }

        candidates
    };

    // Post-poll, not the pre-poll `frozen_at_ceiling` the branch above chose
    // with — see this function's own doc for why the poll that just crossed
    // the ceiling needs the same `seed_only` treatment as every later frozen
    // poll, even though it ran the below-ceiling branch.
    let seed_only = is_first_poll || scratchpad.authoring_failure_streak >= AUTHORING_FAILURE_CEILING;

    run_legacy_seen_ids_tick(persistence, dispatcher, event_bus, assignment, timezone, scratchpad, seed_only, candidates)
        .await
}

/// How [`run_authoring_attempts`]'s authoring pass ended.
enum AuthoringPassOutcome {
    /// A proposal passed every check and is now bound on the assignment
    /// record — `run_agent_watch_tick`'s `contract` branch takes over from
    /// the next poll on. `same_tick_repairs` is how many proposals THIS same
    /// tick rejected and repaired before the one that bound — the loop
    /// index [`run_authoring_attempts`] was on when it returned this variant
    /// (`0` when the very first attempt bound cleanly). Combined with the
    /// pre-this-poll `AssignmentScratchpad::authoring_failure_streak` by
    /// [`run_authoring_and_legacy_tick`], this is what lets
    /// `AssignmentScratchpad::contract_bound_after_failed_attempts` count
    /// EVERY rejected proposal that preceded a bind, not just the ones that
    /// happened to land on separate polls.
    Bound { same_tick_repairs: u32 },
    /// No proposal was bound this tick — either every attempt was rejected,
    /// or the child never included a `contract` key at all. `last_error` is
    /// the last rejection's display text, when there was one, for
    /// [`run_authoring_and_legacy_tick`]'s ceiling-crossed health event to
    /// attach, and for persisting onto `AssignmentScratchpad::last_authoring_rejection_reason`
    /// so the next poll's first attempt is told what already didn't work;
    /// `None` when no proposal was ever offered to reject. `rejection_history`
    /// is the FULL accumulated set of distinct rejection reasons outstanding
    /// for this authoring streak — this poll's own, plus whatever
    /// `AssignmentScratchpad::authoring_rejection_history` already carried in
    /// from earlier polls — for persisting back onto that same field so the
    /// next poll's seed keeps growing instead of only ever holding the
    /// newest one.
    NotBound { last_error: Option<String>, rejection_history: Vec<String> },
}

/// Collapses an accumulated repair list into what
/// [`AgentWatchDetector::observe_for_authoring`] actually takes: `None` with
/// nothing to report, the lone item unwrapped when there is exactly one (so
/// a single outstanding rejection still reads as the focused, single-issue
/// prompt section it always has), and [`RepairContext::Accumulated`] only
/// once there are genuinely two or more constraints the next proposal must
/// satisfy at once.
fn combine_repairs(history: &[RepairContext]) -> Option<RepairContext> {
    match history {
        [] => None,
        [single] => Some(single.clone()),
        many => Some(RepairContext::Accumulated(many.to_vec())),
    }
}

/// Runs at most [`MAX_AUTHORING_ATTEMPTS_PER_TICK`] authoring polls within
/// one tick: an initial attempt, plus one same-tick repair attempt when (and
/// only when) the first proposal's rejection is one [`RepairContext`] knows
/// how to describe precisely — an unparseable `predicate.expr`, an
/// `identity.fields`/`change.material_fields` overlap the model proposed
/// directly, an empty `change.material_fields` on a non-`new_only` proposal,
/// or a malformed `contract` shape (most commonly a required key the chosen
/// `mode` needed and the proposal omitted) — mechanical enough that handing
/// it straight back to the model is worth the extra model call. A
/// `required`+`NotEmpty` contradiction never reaches this loop at all: see
/// [`auto_repair_contract`]. Every other rejection reason is left for the
/// next scheduled poll instead.
///
/// The initial attempt is never blind, though, even on a watch's very first
/// poll of a brand-new tick: `scratchpad.authoring_rejection_history` seeds
/// `history` with every distinct reason an EARLIER poll already hit (this
/// local `history` resets to that seed every time this function is called,
/// but the scratchpad field survives across polls — see its own doc). A
/// same-tick rejection below ADDS to that set rather than replacing it —
/// this is the fix for a multi-constraint oscillation: the model is always
/// shown every outstanding rejection at once, framed as simultaneous
/// constraints, via [`combine_repairs`]/[`RepairContext::Accumulated`] —
/// never just the newest one, which is what let fixing constraint A silently
/// reintroduce constraint B forever.
///
/// Returns the last poll's raw candidates — still needed by the legacy
/// `seen_ids` fallback [`run_authoring_and_legacy_tick`] falls through to
/// regardless of whether authoring bound anything — alongside how the pass
/// ended. `Err(())` means the detector observation itself failed (already
/// logged here); there is nothing left for the caller to do but bail out of
/// the tick, exactly as before this function existed.
async fn run_authoring_attempts(
    persistence: &Arc<PersistenceLayer>,
    event_bus: &Arc<EventBus>,
    detector: &Arc<dyn AgentWatchDetector>,
    assignment: &Assignment,
    instruction: &str,
    scratchpad: &mut AssignmentScratchpad,
) -> Result<(Vec<AgentWatchCandidate>, AuthoringPassOutcome), ()> {
    let mut reasons: Vec<String> = scratchpad.authoring_rejection_history.clone();
    let mut history: Vec<RepairContext> =
        reasons.iter().cloned().map(|reason| RepairContext::CrossPollRejection { reason }).collect();
    let mut repair = combine_repairs(&history);
    let mut candidates: Vec<AgentWatchCandidate> = Vec::new();
    let mut last_error: Option<String> = None;

    for attempt in 0..MAX_AUTHORING_ATTEMPTS_PER_TICK {
        scratchpad.record_model_call(&today_utc());
        let (observed, extra_turns) =
            observe_and_count_extra_model_calls(detector.observe_for_authoring(assignment, instruction, repair.as_ref()))
                .await;
        scratchpad.record_additional_model_calls(&today_utc(), extra_turns);
        let reply = match observed {
            Ok(reply) => reply,
            Err(e) => {
                warn!(
                    assignment_id = %assignment.id,
                    assignment_name = %assignment.name,
                    error = %e,
                    "agent watch: observation FAILED; will retry next interval"
                );
                return Err(());
            }
        };
        candidates = reply.candidates;

        let Some(proposal) = reply.proposed_contract else { break };
        let rejected_expr =
            proposal.get("predicate").and_then(|p| p.get("expr")).and_then(Value::as_str).map(str::to_string);

        match author_contract(
            persistence,
            event_bus,
            detector,
            assignment,
            instruction,
            proposal,
            &candidates,
            scratchpad,
        )
        .await
        {
            AuthorContractOutcome::Bound => {
                return Ok((candidates, AuthoringPassOutcome::Bound { same_tick_repairs: attempt }))
            }
            AuthorContractOutcome::StoreFailed => break,
            AuthorContractOutcome::Rejected(rejection) => {
                let reason_text = rejection.to_string();
                last_error = Some(reason_text.clone());
                if !reasons.contains(&reason_text) {
                    reasons.push(reason_text.clone());
                }

                // Whether a repair is precise enough to describe structurally
                // — independent of whether a same-tick retry is worth
                // spending on it (checked separately below): this reason
                // must still reach the NEXT poll's accumulated seed even
                // when this tick's own attempt budget is exhausted, so
                // whether it's "repairable" can't be gated on
                // `attempts_remain` the way the retry decision is.
                let structured = match (&rejection, rejected_expr) {
                    (ProposalRejection::Invalid(ContractError::InvalidPredicate(parser_error)), Some(expr)) => {
                        Some(RepairContext::InvalidPredicate { rejected_expr: expr, error: parser_error.clone() })
                    }
                    (ProposalRejection::Invalid(ContractError::IdentityMaterialFieldOverlap(fields)), _) => {
                        Some(RepairContext::IdentityMaterialFieldOverlap { fields: fields.clone() })
                    }
                    (ProposalRejection::Invalid(ContractError::EmptyMaterialFields), _) => {
                        Some(RepairContext::EmptyMaterialFields)
                    }
                    (ProposalRejection::Malformed(reason), _) => Some(RepairContext::Malformed { reason: reason.clone() }),
                    _ => None,
                };
                let is_repairable = structured.is_some();
                // A carried-forward flat summary of this exact same
                // rejection (seeded from an earlier poll) is superseded by
                // this attempt's own description of it — structured when one
                // is known, the same flat text otherwise — never both.
                history.retain(|r| !matches!(r, RepairContext::CrossPollRejection { reason } if *reason == reason_text));
                let item = structured.unwrap_or(RepairContext::CrossPollRejection { reason: reason_text.clone() });
                if !history.contains(&item) {
                    history.push(item);
                }

                let attempts_remain = attempt + 1 < MAX_AUTHORING_ATTEMPTS_PER_TICK;
                if !is_repairable || !attempts_remain {
                    break;
                }
                repair = combine_repairs(&history);
            }
        }
    }

    Ok((candidates, AuthoringPassOutcome::NotBound { last_error, rejection_history: reasons }))
}

/// Shape of the `contract` object an authoring-mode reply proposes —
/// everything [`WatchContract`] carries except the three
/// fields authoring itself fills in (`contract_version`, `authored_at`,
/// `authored_by_run`), which the child is never asked for (see
/// `CONTRACT_PROPOSAL_SHAPE`). Deserializing a raw proposal into this type
/// is validation gate one: an `identity.strategy` value outside the closed
/// `native_id`/`composite_native`/`content_hash` set (or any other
/// structurally wrong proposal) fails right here, before
/// [`WatchContract::validate`] or the stability probe ever run.
///
/// `predicate` is [`ProposedPredicate`], not [`PredicateSpec`], deliberately:
/// the model still emits `predicate.expr` as a string (per
/// [`PREDICATE_GRAMMAR`]), and that string is parsed into the typed
/// `Predicate` explicitly in [`author_contract`] — via the exact same
/// [`ao_protocol::watch_contract::legacy_expr::parse`] a persisted legacy
/// contract migrates through — so a parse failure surfaces as gate two
/// (`ProposalRejection::Invalid`, which the same-tick repair loop reacts to)
/// rather than gate one (`ProposalRejection::Malformed`, which it doesn't).
///
/// `Deserialize` is implemented by hand, not derived, so `change` can be
/// conditionally required: absent under `mode: new_only` (there is no prior
/// version of a brand-new item to diff, so `change.material_fields` has
/// nothing to name) but still required everywhere else, exactly as before
/// `mode` existed. A derive can't express "required unless this other field
/// has this value," and defaulting `change` unconditionally would turn a
/// `predicate_transition` proposal that forgot it into a silent
/// `EmptyMaterialFields` validation failure several steps downstream instead
/// of an immediate, precise shape error here.
#[derive(Debug, Clone)]
struct ContractProposal {
    source: WatchSource,
    identity: IdentitySpec,
    change: ChangeSpec,
    predicate: ProposedPredicate,
    mode: WatchMode,
    fields: HashMap<String, FieldSpec>,
    /// Self-reported bare name (no `mcp__{server}__` prefix) of the
    /// read-only tool the child used to answer this watch, per the
    /// authoring prompt's tool self-report instructions
    /// ([`build_authoring_prompt`]). `author_contract` freezes this
    /// verbatim onto the trigger's `extraction_tool` — never inferred from
    /// [`payload_stash`], since the stash has no session key to tell one
    /// concurrent watch's call apart from another's or from an unrelated
    /// chat turn. `None` when the child omitted it (no single stable
    /// read-only call covered this watch, or it wasn't confident one did);
    /// `author_contract` never treats that as an error, only as "stay
    /// model-driven."
    tool_used: Option<String>,
    /// The exact arguments `tool_used` was called with, verbatim. Only
    /// meaningful alongside `tool_used` — ignored when that is `None`.
    arguments_used: Option<Value>,
}

impl<'de> serde::Deserialize<'de> for ContractProposal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Wire {
            source: WatchSource,
            identity: IdentitySpec,
            #[serde(default)]
            change: Option<ChangeSpec>,
            predicate: ProposedPredicate,
            #[serde(default)]
            mode: WatchMode,
            #[serde(default)]
            fields: HashMap<String, FieldSpec>,
            #[serde(default)]
            tool_used: Option<String>,
            #[serde(default)]
            arguments_used: Option<Value>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let change = match wire.change {
            Some(change) => change,
            None if wire.mode == WatchMode::NewOnly => ChangeSpec::default(),
            None => return Err(serde::de::Error::missing_field("change")),
        };
        Ok(ContractProposal {
            source: wire.source,
            identity: wire.identity,
            change,
            predicate: wire.predicate,
            mode: wire.mode,
            fields: wire.fields,
            tool_used: wire.tool_used,
            arguments_used: wire.arguments_used,
        })
    }
}

/// The predicate shape a freshly authored proposal carries: still the
/// legacy string grammar the model is prompted with ([`PREDICATE_GRAMMAR`]).
/// See [`ContractProposal`]'s doc for why this isn't [`PredicateSpec`].
#[derive(Debug, Clone, serde::Deserialize)]
struct ProposedPredicate {
    #[serde(default)]
    natural_language: String,
    #[serde(default)]
    fields: Vec<String>,
    expr: String,
}

/// Why a proposed contract from an authoring-mode poll was rejected.
/// A rejection is never persisted: `contract` stays `None`
/// on the assignment record, and authoring is retried on the next poll
/// where `contract` is still `None`.
#[derive(Debug)]
enum ProposalRejection {
    /// The reply's `contract` object didn't deserialize into
    /// [`ContractProposal`] at all — most commonly an `identity.strategy`
    /// value outside the closed enum, since serde itself rejects that as an
    /// unrecognized variant ("unknown identity strategy"), or a key the
    /// chosen `mode` still required that the proposal omitted. Same-tick
    /// repairable (see [`RepairContext::Malformed`]): the wrapped text is
    /// `serde`'s own error, almost always specific enough for the model to
    /// see and fix immediately.
    Malformed(String),
    /// Deserialized fine, but failed [`WatchContract::validate`] (an
    /// unparseable predicate expression, an invalid `identity.format`
    /// regex, an empty identity, or empty `change.material_fields`).
    Invalid(ContractError),
    /// The stability probe's second poll itself failed (a detector error,
    /// not a disqualified field) — nothing to conclude about stability, so
    /// the whole proposal is deferred rather than guessed at.
    ProbePollFailed(String),
    /// The stability probe positively determined the proposed `native_id`
    /// field was unstable ([`ProbeOutcome::Unstable`]), so authoring tried to
    /// drop a rung to `composite_native` — but every field
    /// [`composite_fallback_fields`] had to choose from is also declared in
    /// `change.material_fields`, leaving nothing to build a composite
    /// identity from. Deliberately NOT a [`ContractError`] wrapped in
    /// `Invalid`: the model's own proposal validated cleanly against
    /// `WatchContract::validate` (that gate already ran and passed, earlier
    /// in [`author_contract`]) — this is the engine's own rung-drop
    /// construction hitting a dead end, not a defect in what was proposed,
    /// and its [`Display`](std::fmt::Display) text says so explicitly rather
    /// than reading like "the proposal failed validation."
    RungDropExhausted { dropped_field: String },
}

impl std::fmt::Display for ProposalRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposalRejection::Malformed(reason) => {
                write!(f, "proposal did not match the expected contract shape: {reason}")
            }
            ProposalRejection::Invalid(e) => write!(f, "proposal failed validation: {e}"),
            ProposalRejection::ProbePollFailed(reason) => {
                write!(f, "the stability probe's second poll failed: {reason}")
            }
            ProposalRejection::RungDropExhausted { dropped_field } => write!(
                f,
                "authoring could not verify native_id field \"{dropped_field}\" as stable, and its automatic \
                 fallback to a composite identity found no field left to use: every field available for a \
                 composite key is also declared in change.material_fields. This is an engine construction \
                 limit for this watch's current field set, not a defect in the proposal itself."
            ),
        }
    }
}

/// Delay between the stability probe's two polls: "seconds
/// apart" — long enough that a value which genuinely changes between polls
/// (an edit timestamp, a cursor, a session token) has a real chance to
/// actually move, short enough that authoring a watch doesn't visibly hang.
/// Paid exactly once per watch, only when a `native_id` proposal survives
/// structural validation — never on an ordinary contract-bound poll.
const STABILITY_PROBE_DELAY: Duration = Duration::from_secs(3);

/// Outcome of authoring's stability probe for a proposed
/// `native_id` `source_field`. Three states, kept distinct on purpose: an
/// unknown/indeterminate result must never collapse into either definite
/// one. [`Stable`](Self::Stable) is the only state that leaves the proposed
/// rung standing on its own claim; [`Unstable`](Self::Unstable) is the only
/// state that drops it. [`Inconclusive`](Self::Inconclusive) means neither
/// was actually shown — see [`probe_identity_stability`]'s doc for what the
/// caller owes each state.
enum ProbeOutcome {
    /// Every value observed for the field on poll1 was still present on
    /// poll2 (poll1's values are a subset of poll2's, including the equal
    /// case) — the strongest signal available short of watching the real
    /// source for longer than [`STABILITY_PROBE_DELAY`] allows. A watch
    /// exists because new rows show up between polls, so poll2 containing
    /// values poll1 never saw (membership growth) is expected and is not
    /// evidence of instability.
    Stable { checked: usize },
    /// The two polls' value sets shared nothing at all, and poll1 alone
    /// already contained two or more distinct values — total churn of that
    /// size is a positive finding that the field is re-minted or
    /// session-scoped, not a stable identity.
    Unstable,
    /// Neither of the above was actually shown. Carries the specific cause
    /// ([`ProbeInconclusiveCause`]) so the caller can say which situation
    /// fired instead of collapsing all of them into one generic message —
    /// this is not evidence of instability, it is the probe genuinely
    /// unable to reach a verdict, and must never be treated as if it had.
    Inconclusive(ProbeInconclusiveCause),
}

/// Why [`probe_identity_stability`] landed on [`ProbeOutcome::Inconclusive`]
/// rather than a definite verdict. Three distinct situations route here, and
/// a single generic message across all of them would erase exactly the
/// information [`author_contract`]'s scratchpad reason exists to carry.
enum ProbeInconclusiveCause {
    /// `field` was absent from every candidate in at least one of the two
    /// polls (including a poll that returned zero candidates at all) — there
    /// was nothing to compare.
    NoObservations,
    /// The two polls' value sets shared nothing, but poll1 only ever
    /// reported a single distinct value for `field`. One value carries no
    /// power to tell "this row's value was rewritten" apart from "that row
    /// was deleted and an unrelated row was added," so a lone disjoint value
    /// cannot be allowed to declare the field volatile the way two or more
    /// can.
    SingleValueDisjoint,
    /// At least one value observed on poll1 was still present on poll2, but
    /// at least one was not — partial overlap. Without a join key this can't
    /// be told apart from ordinary membership churn (a row deleted between
    /// polls). `persisted` and `vanished` partition poll1's observed values.
    PartialOverlap { persisted: usize, vanished: usize },
}

/// Compares the SET of values `field` took across `poll1` and `poll2` — not
/// which candidate reported which value. This is deliberate: with a
/// `native_id` proposal, `field`'s own value IS the claimed identity, so if
/// it is genuinely stable the same real-world row reproduces the exact same
/// value on both polls without needing any other join key to prove it.
/// Correlating candidates by the reply's free-text `id` tag instead (an
/// earlier version of this function did) doesn't work: [`AGENT_WATCH_SYSTEM_PROMPT`]
/// itself documents `id` as "not a dedup key," and each probe poll spins a
/// fresh child session, so the same row routinely gets a different tag on
/// its second observation — every proposal probed that way came back
/// inconclusive even when `field` itself was perfectly stable. Comparing the
/// field's own values sidesteps the need for a join key entirely: the model
/// already handed over both the candidate key (`field`) and, in
/// `identity.format`, a regex to validate it against (see
/// [`identity_format_matches_observed_values`], applied separately) — this
/// function only has to ask whether that key's values held steady.
///
/// Deliberately SUBSET, not set-equality: a watch exists because new rows
/// show up between polls, so `poll2` containing values `poll1` never saw is
/// ordinary membership growth, not instability. The three-way split:
/// - `values1` is a subset of `values2` (equality included) → [`Stable`](ProbeOutcome::Stable).
///   Every value this probe actually observed on poll1 was still there on
///   poll2, unchanged — that is the evidence being asked for.
/// - `values1` and `values2` are completely disjoint AND `values1` had two
///   or more distinct values → [`Unstable`](ProbeOutcome::Unstable). Total
///   churn of that size is a positive finding the key is volatile.
/// - Everything else → [`Inconclusive`](ProbeOutcome::Inconclusive): partial
///   overlap (something vanished while something else persisted), or a
///   disjoint pair where `values1` only ever had one distinct value. Without
///   a join key neither case can be told apart from ordinary row churn (a
///   rewrite looks identical to a delete-then-add), and a single value in
///   particular carries no power to prove volatility on its own.
fn probe_identity_stability(field: &str, poll1: &[AgentWatchCandidate], poll2: &[AgentWatchCandidate]) -> ProbeOutcome {
    fn observed_values<'a>(poll: &'a [AgentWatchCandidate], field: &str) -> HashSet<&'a str> {
        poll.iter().filter_map(|c| c.payload.get(field).and_then(Value::as_str)).collect()
    }

    let values1 = observed_values(poll1, field);
    let values2 = observed_values(poll2, field);

    if values1.is_empty() || values2.is_empty() {
        return ProbeOutcome::Inconclusive(ProbeInconclusiveCause::NoObservations);
    }

    if values1.is_subset(&values2) {
        return ProbeOutcome::Stable { checked: values1.len() };
    }

    if values1.is_disjoint(&values2) {
        return if values1.len() >= 2 {
            ProbeOutcome::Unstable
        } else {
            // Non-empty (checked above) and not a subset of a disjoint set
            // unless it's the empty set, so this is exactly the len == 1 case.
            ProbeOutcome::Inconclusive(ProbeInconclusiveCause::SingleValueDisjoint)
        };
    }

    // Not a subset, not disjoint: some of poll1's values survived into
    // poll2 and at least one did not.
    let vanished = values1.difference(&values2).count();
    let persisted = values1.len() - vanished;
    ProbeOutcome::Inconclusive(ProbeInconclusiveCause::PartialOverlap { persisted, vanished })
}

/// `format` is derived, not authored by us: a proposed `identity.format`
/// regex is only trustworthy if it actually matches every value observed
/// for `identity.source_field`
/// during this authoring run — both probe polls, when a probe ran. A regex
/// that compiles but doesn't match reality is the cheapest available signal
/// that it (or its target field) was confabulated rather than genuinely
/// derived from what was observed.
///
/// Unlike every other proposal defect this module checks, a bad `format`
/// does not reject the proposal — it just drops the field back to `None` and
/// lets the rest of the contract bind. `identity.format` is optional at
/// every layer that reads it (the schema, and the runtime quarantine check
/// in `ao_protocol::watch_contract`), so "the agent could not derive a
/// stable format" is a legitimate, expected outcome, not a defect worth
/// blocking the whole watch over. The alternative — rejecting the proposal
/// and forcing a from-scratch re-authoring attempt — is worse in exactly the
/// case this exists to guard against: a model that keeps re-proposing the
/// same stale, confabulated pattern (e.g. copied from an earlier version of
/// the same source) would never bind at all, leaving the watch stuck paying
/// for a full model turn every poll forever, when everything about the
/// proposal except this one optional field was correct. Only meaningful for
/// `native_id` (the only strategy with a single `format` regex to check);
/// every other strategy is left untouched.
fn drop_unverified_identity_format(
    contract: &mut WatchContract,
    poll1: &[AgentWatchCandidate],
    poll2: &[AgentWatchCandidate],
) {
    if contract.identity.strategy != IdentityStrategy::NativeId {
        return;
    }
    let (Some(field), Some(pattern)) =
        (contract.identity.source_field.clone(), contract.identity.format.clone())
    else {
        return;
    };
    // `WatchContract::validate` (already run before this is ever called)
    // already confirmed this pattern compiles.
    let Ok(re) = Regex::new(&pattern) else {
        return;
    };
    for candidate in poll1.iter().chain(poll2.iter()) {
        if let Some(value) = candidate.payload.get(&field).and_then(Value::as_str) {
            if !re.is_match(value) {
                contract.identity.format = None;
                contract.identity.rationale = format!(
                    "{} Dropped the proposed format `{pattern}`: it did not match `{field}`'s own observed \
                     value {value:?}, so it was left absent — a wrong format would otherwise quarantine every \
                     future candidate.",
                    contract.identity.rationale.trim_end_matches('.'),
                );
                return;
            }
        }
    }
}

/// Builds the composite-identity field set a rung-drop
/// falls back to when a proposed `native_id` field is disqualified: every
/// field the extraction contract marks `required` (or, if none are
/// required, every declared field), with `change.material_fields` subtracted
/// FIRST — `WatchContract::validate` rejects an identity that overlaps
/// `change.material_fields` (`ContractError::IdentityMaterialFieldOverlap`),
/// so a constructor that built the field set without subtracting first would
/// hand the validator a contract it is guaranteed to reject, blaming the
/// resulting rejection on "the proposal" when the proposal was never asked
/// about this fallback at all.
///
/// Returns `Err(())` — never an empty `Vec` — when the subtraction leaves
/// nothing: every field otherwise available for a composite key is also
/// material, so there is no field left to identify items by. The caller owns
/// turning that into a correctly-attributed [`ProposalRejection::RungDropExhausted`]
/// rather than a generic "invalid proposal" rejection.
fn composite_fallback_fields(
    fields: &HashMap<String, FieldSpec>,
    material_fields: &[String],
) -> Result<Vec<String>, ()> {
    let material: HashSet<&str> = material_fields.iter().map(String::as_str).collect();
    let mut fallback: Vec<String> = fields
        .iter()
        .filter(|(name, spec)| spec.required && !material.contains(name.as_str()))
        .map(|(name, _)| name.clone())
        .collect();
    if fallback.is_empty() {
        fallback = fields.keys().filter(|name| !material.contains(name.as_str())).cloned().collect();
    }
    fallback.sort();

    // Structural, not just a patched call site: any future caller of this
    // constructor that manages to violate the invariant it exists to
    // enforce fails right here, attributed to this function, in every debug
    // and test build — rather than surfacing three calls away as a generic
    // `WatchContract::validate` rejection with no hint which constructor was
    // at fault.
    debug_assert!(
        fallback.iter().all(|f| !material.contains(f.as_str())),
        "composite_fallback_fields must never return a field also present in change.material_fields"
    );

    if fallback.is_empty() {
        Err(())
    } else {
        Ok(fallback)
    }
}

/// Applies every DETERMINISTIC repair this module knows for a
/// [`ContractError`] `WatchContract::validate` can return, re-validating
/// after each one, until either the contract passes or an error remains that
/// has no such repair. Called instead of a bare `contract.validate()` at
/// every point [`author_contract`] validates a proposal; never counts as a
/// failed authoring attempt and never involves the model — a repair only
/// lands here when the fix is the ONLY reasonable one, so asking the model
/// to pick would be asking it to guess at something the code already knows.
///
/// Currently handles exactly one case:
/// [`ContractError::RequiredFieldTargetedByTolerantPredicate`] — a field
/// marked `required: true` that a `NotEmpty` predicate leaf also targets.
/// The two are redundant, not merely contradictory: `required` already
/// quarantines a blank value before any predicate runs, so the observable
/// behavior is identical whether `required` or the predicate leaf is the one
/// that stays. Dropping `required` is a per-field boolean with a safe
/// default (`false`) and can never leave a structural hole in the predicate
/// tree; removing the `NotEmpty` leaf instead could empty an `and`/`or`
/// branch and trade this failure for a different one. So the repair is
/// always "drop `required`," never "drop the predicate" — see
/// [`ContractError::RequiredFieldTargetedByTolerantPredicate`]'s own error
/// text, which already names both options in English; this function is what
/// stops that English from being handed to a model to choose between.
///
/// Every other [`ContractError`] `validate` can return — an empty identity,
/// an identity/material-field overlap, empty material fields, an
/// unparseable predicate, an `identity.format` that doesn't compile as a
/// regex — has more than one reasonable fix and which one is right depends
/// on what the watch is actually for, so those stay a same-tick or
/// cross-poll question for the model (see [`RepairContext`]).
fn auto_repair_contract(contract: &mut WatchContract, assignment: &Assignment) -> Result<(), ContractError> {
    loop {
        match contract.validate() {
            Ok(()) => return Ok(()),
            Err(ContractError::RequiredFieldTargetedByTolerantPredicate(field)) => {
                let spec = contract.fields.get_mut(&field).expect(
                    "WatchContract::validate only names a field this way when it already found it in \
                     self.fields with required: true",
                );
                spec.required = false;
                info!(
                    assignment_id = %assignment.id,
                    assignment_name = %assignment.name,
                    field = %field,
                    "agent watch: auto-repaired proposal in code — {field:?} was `required: true` and also \
                     targeted by a NotEmpty predicate; dropped `required` (redundant with the predicate, not \
                     merely contradictory) instead of bouncing the contradiction back to the model"
                );
            }
            Err(e) => return Err(e),
        }
    }
}

/// One authoring attempt's outcome, as returned by [`author_contract`] to
/// its caller ([`run_authoring_attempts`]).
enum AuthorContractOutcome {
    /// The proposal passed every check and is now persisted on the
    /// assignment record.
    Bound,
    /// The proposal itself failed a check — see [`ProposalRejection`]. Never
    /// persisted; carries the exact reason so the caller can decide whether
    /// a same-tick repair attempt is worth trying.
    Rejected(ProposalRejection),
    /// Every check on the proposal itself passed, but writing the resulting
    /// contract to the assignment store failed — a storage problem, not a
    /// bad proposal. Retried untouched on the next poll, same as before this
    /// feature existed: no health event, and never worth a same-tick repair.
    StoreFailed,
}

/// Attempts to author and persist a [`WatchContract`] for `assignment`:
/// parses `raw_proposal` into a [`ContractProposal`],
/// validates it structurally ([`WatchContract::validate`]), runs the
/// stability probe on a `native_id` strategy's claimed key and drops a rung
/// only on a positive [`ProbeOutcome::Unstable`] finding (an
/// [`ProbeOutcome::Inconclusive`] result keeps the proposed rung and
/// records that it was never verified instead), checks the surviving
/// `identity.format` against what was actually observed, and only once
/// every check passes, persists the resulting contract onto the assignment
/// record. Every rejection path logs at warn, emits a health event, and
/// returns without persisting anything — `contract` stays `None` so
/// [`run_agent_watch_tick`] retries authoring next poll. A bad contract is
/// never persisted, because it would silently mis-key every future poll.
async fn author_contract(
    persistence: &Arc<PersistenceLayer>,
    event_bus: &Arc<EventBus>,
    detector: &Arc<dyn AgentWatchDetector>,
    assignment: &Assignment,
    instruction: &str,
    raw_proposal: Value,
    poll1_candidates: &[AgentWatchCandidate],
    scratchpad: &mut AssignmentScratchpad,
) -> AuthorContractOutcome {
    let proposal: ContractProposal = match serde_json::from_value(raw_proposal) {
        Ok(p) => p,
        Err(e) => {
            let rejection = ProposalRejection::Malformed(e.to_string());
            reject_proposal(event_bus, assignment, &rejection, scratchpad.authoring_failure_streak).await;
            return AuthorContractOutcome::Rejected(rejection);
        }
    };

    // Captured before `proposal`'s other fields are moved into `contract`
    // below. A blank string is treated the same as an absent report — some
    // providers emit `""` rather than omitting an optional string field.
    let self_reported_tool = proposal.tool_used.clone().filter(|t| !t.trim().is_empty());
    let self_reported_args = proposal.arguments_used.clone();

    // The model still emits `predicate.expr` as a string (see
    // `ContractProposal`'s doc) — parsed into the typed `Predicate` here,
    // the same one-way conversion a persisted legacy contract goes through
    // on load. An unparseable `expr` surfaces as `ProposalRejection::Invalid`
    // (not `Malformed`, which already ran above) so the same-tick repair
    // loop below still recognizes and retries it exactly as before.
    let predicate = match ao_protocol::watch_contract::legacy_expr::parse(&proposal.predicate.expr) {
        Ok(predicate) => predicate,
        Err(e) => {
            let rejection = ProposalRejection::Invalid(e);
            reject_proposal(event_bus, assignment, &rejection, scratchpad.authoring_failure_streak).await;
            return AuthorContractOutcome::Rejected(rejection);
        }
    };

    let mut contract = WatchContract {
        contract_version: 1,
        authored_at: Utc::now().to_rfc3339(),
        authored_by_run: uuid::Uuid::new_v4().to_string(),
        source: proposal.source,
        identity: proposal.identity,
        change: proposal.change,
        predicate: PredicateSpec {
            natural_language: proposal.predicate.natural_language,
            fields: proposal.predicate.fields,
            predicate,
        },
        mode: proposal.mode,
        fields: proposal.fields,
    };

    if let Err(e) = auto_repair_contract(&mut contract, assignment) {
        let rejection = ProposalRejection::Invalid(e);
        reject_proposal(event_bus, assignment, &rejection, scratchpad.authoring_failure_streak).await;
        return AuthorContractOutcome::Rejected(rejection);
    }

    // Stability probe — `native_id` only; `composite_native`
    // and `content_hash` make no single-stable-key claim to verify. One
    // extra poll, paid exactly once here, never on an ordinary tick.
    let mut poll2_candidates: Vec<AgentWatchCandidate> = Vec::new();
    if contract.identity.strategy == IdentityStrategy::NativeId {
        tokio::time::sleep(STABILITY_PROBE_DELAY).await;
        scratchpad.record_model_call(&today_utc());
        let (observed, extra_turns) = observe_and_count_extra_model_calls(detector.observe(assignment, instruction)).await;
        scratchpad.record_additional_model_calls(&today_utc(), extra_turns);
        poll2_candidates = match observed {
            Ok(c) => c,
            Err(e) => {
                let rejection = ProposalRejection::ProbePollFailed(e.to_string());
                reject_proposal(event_bus, assignment, &rejection, scratchpad.authoring_failure_streak).await;
                return AuthorContractOutcome::Rejected(rejection);
            }
        };

        let field = contract.identity.source_field.clone().unwrap_or_default();
        match probe_identity_stability(&field, poll1_candidates, &poll2_candidates) {
            ProbeOutcome::Stable { checked } => {
                contract.identity.rationale = format!(
                    "{} Verified stable: `{field}` was unchanged for {checked} item(s) checked twice, {}s \
                     apart, during authoring.",
                    contract.identity.rationale.trim_end_matches('.'),
                    STABILITY_PROBE_DELAY.as_secs(),
                );
                scratchpad.identity_probe_inconclusive = false;
                scratchpad.identity_probe_inconclusive_reason = None;
            }
            ProbeOutcome::Inconclusive(cause) => {
                // Three states stay three: an inconclusive probe is not a
                // positive finding of instability, so the proposed rung is
                // kept, not dropped — dropping is reserved for
                // `ProbeOutcome::Unstable` below. Binding an identity that
                // was never actually verified must not be silent, though, so
                // the outcome is recorded on the scratchpad (surfaced via
                // `AssignmentWatchHealth` at the API layer) as well as folded
                // into the rationale shown to the user. `cause` distinguishes
                // which of three situations actually fired — a single
                // generic reason across all of them would be a regression in
                // explainability, the whole point of this field.
                let reason = match cause {
                    ProbeInconclusiveCause::NoObservations => format!(
                        "The stability probe could not confirm whether `{field}` stays the same for the same \
                         item across polls: this authoring run's two polls, {}s apart, left `{field}` entirely \
                         absent from every candidate in at least one of the polls, so there was nothing to \
                         compare. The proposed native_id identity was bound anyway rather than dropped to a \
                         weaker rung, since nothing was actually shown to be unstable — but it has not been \
                         verified either.",
                        STABILITY_PROBE_DELAY.as_secs(),
                    ),
                    ProbeInconclusiveCause::SingleValueDisjoint => format!(
                        "The stability probe could not confirm whether `{field}` stays the same for the same \
                         item across polls: this authoring run's first poll observed only 1 distinct value for \
                         `{field}`, and it was absent from the second poll, {}s later. A single observed value \
                         cannot be told apart from that row being deleted and a different row being added, so \
                         it carries no power to prove `{field}` volatile on its own. The proposed native_id \
                         identity was bound anyway rather than dropped to a weaker rung, since nothing was \
                         actually shown to be unstable — but it has not been verified either.",
                        STABILITY_PROBE_DELAY.as_secs(),
                    ),
                    ProbeInconclusiveCause::PartialOverlap { persisted, vanished } => format!(
                        "The stability probe could not confirm whether `{field}` stays the same for the same \
                         item across polls: `{field}` held steady on every candidate observed twice ({persisted} \
                         value(s)), but {vanished} value(s) seen on the first poll {} absent from the second, \
                         {}s later, which cannot be distinguished from {} being deleted between polls. The \
                         proposed native_id identity was bound anyway rather than dropped to a weaker rung, \
                         since nothing was actually shown to be unstable — but it has not been verified either.",
                        if vanished == 1 { "was" } else { "were" },
                        STABILITY_PROBE_DELAY.as_secs(),
                        if vanished == 1 { "that row" } else { "those rows" },
                    ),
                };
                contract.identity.rationale =
                    format!("{} Not verified: {reason}", contract.identity.rationale.trim_end_matches('.'));
                scratchpad.identity_probe_inconclusive = true;
                scratchpad.identity_probe_inconclusive_reason = Some(reason);
            }
            ProbeOutcome::Unstable => {
                // Drop a rung: native_id -> composite_native, falling back
                // to the extraction contract's own required fields (or, if
                // none are required, every declared field) as the composite
                // key — the best proxy available for "what this agent
                // considers load-bearing about the item," since a rejected
                // native_id proposal carries no `identity.fields` of its
                // own to fall back to. `change.material_fields` is
                // subtracted inside `composite_fallback_fields` itself so
                // this constructor can never hand `WatchContract::validate`
                // an identity it is guaranteed to reject.
                let dropped_field = field.clone();
                let fallback_fields = match composite_fallback_fields(&contract.fields, &contract.change.material_fields)
                {
                    Ok(fields) => fields,
                    Err(()) => {
                        let rejection = ProposalRejection::RungDropExhausted { dropped_field: dropped_field.clone() };
                        warn!(
                            assignment_id = %assignment.id,
                            assignment_name = %assignment.name,
                            reason = %rejection,
                            "agent watch: authoring's rung-drop could not construct a composite identity; \
                             leaving unbound and retrying authoring next poll"
                        );
                        emit_health_event(
                            event_bus,
                            assignment,
                            format!(
                                "Agent watch \"{}\" could not verify its proposed native_id field \"{dropped_field}\" \
                                 as stable, and authoring's automatic fallback to a composite identity could not \
                                 proceed: every field available for a composite key is also declared material for \
                                 this watch, leaving nothing to build an identity from. This is not a defect in the \
                                 proposal itself — the watch cannot bind until its instruction or connector scope \
                                 changes what fields are available.",
                                assignment.name
                            ),
                        )
                        .await;
                        return AuthorContractOutcome::Rejected(rejection);
                    }
                };

                contract.identity.strategy = IdentityStrategy::CompositeNative;
                contract.identity.source_field = None;
                contract.identity.format = None;
                contract.identity.fields = fallback_fields.clone();
                contract.identity.rationale = format!(
                    "Dropped from a single-field identity to a composite key of {fallback_fields:?}: the \
                     proposed field `{dropped_field}` was confirmed unstable — during authoring it changed \
                     between two immediate polls of the same items."
                );
                scratchpad.identity_probe_inconclusive = false;
                scratchpad.identity_probe_inconclusive_reason = None;

                if let Err(e) = auto_repair_contract(&mut contract, assignment) {
                    let rejection = ProposalRejection::Invalid(e);
                    reject_proposal(event_bus, assignment, &rejection, scratchpad.authoring_failure_streak).await;
                    return AuthorContractOutcome::Rejected(rejection);
                }
            }
        }
    }

    drop_unverified_identity_format(&mut contract, poll1_candidates, &poll2_candidates);

    match set_assignment_contract(
        persistence,
        assignment,
        Some(contract.clone()),
        self_reported_tool.clone(),
        self_reported_args,
    )
    .await
    {
        Ok(()) => {
            // Not yet reset — `run_authoring_and_legacy_tick` only zeroes
            // `authoring_failure_streak` after this `Bound` outcome bubbles
            // back up to it — so this is still the count of consecutive
            // polls that failed *before* this one, making `+ 1` the number
            // of this poll out of `AUTHORING_FAILURE_CEILING`. A watch that
            // authors cleanly on its very first poll reports "attempt 1".
            let attempt = scratchpad.authoring_failure_streak + 1;
            info!(
                assignment_id = %assignment.id,
                assignment_name = %assignment.name,
                identity_strategy = ?contract.identity.strategy,
                extraction_tool = self_reported_tool.as_deref(),
                attempt,
                "agent watch: authored and persisted a new contract"
            );
            emit_health_event_with_severity(
                event_bus,
                assignment,
                format!(
                    "Agent watch \"{}\" successfully authored its watch contract on attempt {attempt} of \
                     {AUTHORING_FAILURE_CEILING}: authoring has converged and this watch is now running on a \
                     bound contract. {} {}",
                    assignment.name,
                    contract.identity.rationale,
                    describe_bound_mode(&contract),
                ),
                Some(SystemMessageSeverity::Success),
            )
            .await;
            match &self_reported_tool {
                Some(tool) => {
                    scratchpad.extraction_plan_degraded = false;
                    scratchpad.extraction_plan_degraded_reason = None;
                    info!(
                        assignment_id = %assignment.id,
                        assignment_name = %assignment.name,
                        extraction_tool = %tool,
                        "agent watch: froze a self-reported tool for future deterministic polls"
                    );
                }
                None => {
                    scratchpad.extraction_plan_degraded = true;
                    scratchpad.extraction_plan_degraded_reason = Some(NO_SELF_REPORTED_TOOL_REASON.to_string());
                    info!(
                        assignment_id = %assignment.id,
                        assignment_name = %assignment.name,
                        "agent watch: authoring reply did not self-report a tool; this watch stays model-driven"
                    );
                }
            }
            AuthorContractOutcome::Bound
        }
        Err(e) => {
            warn!(
                assignment_id = %assignment.id,
                assignment_name = %assignment.name,
                error = %e,
                "agent watch: failed to persist the authored contract; will retry authoring next poll"
            );
            AuthorContractOutcome::StoreFailed
        }
    }
}

/// Plain-language summary of what makes the just-bound `contract` fire,
/// folded into `author_contract`'s convergence health event alongside
/// `identity.rationale` — a user who can see WHICH mode and material fields
/// were settled on can tell at a glance whether the watch matches what they
/// asked for, rather than only seeing that authoring succeeded.
fn describe_bound_mode(contract: &WatchContract) -> String {
    match contract.mode {
        WatchMode::NewOnly => "Mode: new_only — fires the moment a new item appears.".to_string(),
        WatchMode::PredicateTransition => format!(
            "Mode: predicate_transition — fires on changes to: {}.",
            contract.change.material_fields.join(", ")
        ),
        WatchMode::NewOrChanged => format!(
            "Mode: new_or_changed — fires on a new item, or on changes to: {}.",
            contract.change.material_fields.join(", ")
        ),
    }
}

/// Human-readable reason persisted to
/// [`AssignmentScratchpad::extraction_plan_degraded_reason`] (the same
/// field/channel the extraction-plan health badge already reads) whenever an
/// authoring reply binds a contract but reports no `tool_used` — the watch
/// stays fully model-driven (every poll runs a full agent turn) rather than
/// ever inferring a tool from the payload stash. Never overwritten by
/// anything else once `extraction_tool` stays `None`, so it accurately
/// explains the watch's state for as long as that remains true.
const NO_SELF_REPORTED_TOOL_REASON: &str = "\
The authoring reply did not report a single read-only tool call that answers this watch, so it \
stays fully model-driven: every poll runs the assignment's own agent instead of calling a \
connector tool directly.";

/// Emits the per-attempt rejection health event for a proposal that failed
/// validation. `current_failure_streak` is `scratchpad.authoring_failure_streak`
/// as it stood *before* this poll's outcome is recorded — every attempt
/// within the same poll (a same-tick repair included) sees the same value,
/// so `current_failure_streak + 1` is stable across them and names which
/// poll, out of [`AUTHORING_FAILURE_CEILING`], this one is.
///
/// Below the ceiling this reads as ordinary progress — a self-correcting
/// retry, not a failure — and carries no severity, same as every other
/// health event. Exactly on the poll that reaches the ceiling,
/// [`run_authoring_and_legacy_tick`] is about to freeze authoring for this
/// assignment, so this specific rejection is tagged
/// [`SystemMessageSeverity::Error`] and says so, instead of promising a retry
/// that will not happen.
async fn reject_proposal(
    event_bus: &Arc<EventBus>,
    assignment: &Assignment,
    rejection: &ProposalRejection,
    current_failure_streak: u32,
) {
    let attempt = current_failure_streak + 1;
    let at_ceiling = attempt >= AUTHORING_FAILURE_CEILING;
    if at_ceiling {
        warn!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            reason = %rejection,
            attempt,
            "agent watch: rejected a proposed contract on the poll that reaches the authoring failure ceiling"
        );
        emit_health_event_with_severity(
            event_bus,
            assignment,
            format!(
                "Agent watch \"{}\"'s contract proposal on attempt {attempt} of {AUTHORING_FAILURE_CEILING} \
                 didn't pass validation ({rejection}) — it was not saved.",
                assignment.name
            ),
            Some(SystemMessageSeverity::Error),
        )
        .await;
    } else {
        info!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            reason = %rejection,
            attempt,
            "agent watch: adjusting a proposed contract; retrying authoring next poll"
        );
        emit_health_event(
            event_bus,
            assignment,
            format!(
                "Agent watch \"{}\" is adjusting its watch contract — attempt {attempt} of \
                 {AUTHORING_FAILURE_CEILING}: its last proposal needed a correction ({rejection}). This is a \
                 normal self-correcting retry; authoring will try again on the next poll.",
                assignment.name
            ),
        )
        .await;
    }
}

/// Fetches the current on-disk assignment record — not the possibly-stale
/// `assignment` a tick started with, since a poll (especially one that ran
/// the stability probe) can take a while, and this avoids clobbering a
/// concurrent edit to any other field — and writes `contract`,
/// `extraction_tool`, and `extraction_args` onto its `AgentWatch` trigger in
/// one read-modify-write. `extraction_tool`/`extraction_args` freeze from
/// the authoring reply's self-report (see [`author_contract`]) — callers
/// clearing the contract (no self-report to freeze) pass `None` for both,
/// which also clears any tool a previous contract had frozen, since a
/// cleared contract is about to be re-authored from scratch anyway.
async fn set_assignment_contract(
    persistence: &Arc<PersistenceLayer>,
    assignment: &Assignment,
    contract: Option<WatchContract>,
    extraction_tool: Option<String>,
    extraction_args: Option<Value>,
) -> Result<(), ao_protocol::error::AoError> {
    let mut fresh = persistence.assignments.get(&assignment.id).await.unwrap_or_else(|| assignment.clone());
    if let AssignmentTrigger::AgentWatch {
        contract: slot,
        extraction_tool: tool_slot,
        extraction_args: args_slot,
        ..
    } = &mut fresh.trigger
    {
        *slot = contract;
        *tool_slot = extraction_tool;
        *args_slot = extraction_args;
    }
    fresh.updated_ts = Utc::now();
    persistence.assignments.update(fresh).await
}

/// Contract-driven diff.
/// Per candidate: hash identity/version, evaluate the predicate, diff against
/// [`AssignmentScratchpad::snapshots`], decide fire-or-skip per
/// `contract.mode`, then *always* upsert the snapshot — a quiet tick still
/// carries information a future transition must diff against. Only a failed
/// [`fire_assignment`] discards the tick's state; every other completed path
/// (nothing fired, or the fire succeeded) persists, so a quiet poll is never
/// lost to the old "return early without persisting" shortcut.
///
/// `force_seed_only`, when `true`, suppresses firing on this tick exactly
/// like `is_first_poll`/a contract-fingerprint mismatch already do —
/// [`select_agent_watch_candidates`] sets it when this poll's candidates came
/// from the LLM fallback because a previously working extraction plan just
/// failed structurally, so identity keys for this poll may not mean what
/// they used to. Deliberately narrower than the full `seed_only` treatment
/// below, though: it does NOT reset `edge_counter` or feed
/// `seed_matches`/the baseline-disclosure message, because — unlike a real
/// reseed — the contract's own identity/version meaning has not changed here,
/// only the extraction mechanism hiccuped, so existing snapshots stay valid
/// history to diff future polls against once the plan is re-authored.
async fn run_contract_bound_tick(
    persistence: &Arc<PersistenceLayer>,
    dispatcher: &Arc<dyn NotificationDispatcher>,
    event_bus: &Arc<EventBus>,
    assignment: &Assignment,
    timezone: Option<&str>,
    contract: &WatchContract,
    mut scratchpad: AssignmentScratchpad,
    is_first_poll: bool,
    force_seed_only: bool,
    candidates: Vec<AgentWatchCandidate>,
    extraction_path: ExtractionPath,
    inferred_tier: Option<Tier>,
) -> bool {
    // Two-phase ledger reconciliation runs before anything else in this
    // tick (see the fire-then-persist note above): a `Pending`
    // entry from a prior tick's fire needs its dispatched run's outcome
    // checked on every poll regardless of whether *this* poll finds
    // anything new, and both persist points below (quiet-tick and
    // post-fire) must carry the result either way.
    reconcile_pending_deliveries(persistence, event_bus, assignment, &mut scratchpad).await;

    // Purely informational (a later UI task displays these) — set once, up
    // front, so both persist points below (the quiet-tick branch and the
    // post-fire branch) carry it regardless of which one this tick takes.
    scratchpad.last_extraction_path = extraction_path;
    scratchpad.last_inferred_tier = inferred_tier;

    let live_fingerprint = contract.fingerprint();
    // `!= Some(live_fingerprint)` rather than "was Some and differed" so this
    // also catches an assignment that ran on the legacy `seen_ids` path
    // before ever binding a contract: its scratchpad already exists (so
    // `is_first_poll` is false) but `contract_fingerprint` is still `None`.
    // Without this, that one-time upgrade tick would diff every candidate
    // against empty `snapshots` and, under `predicate_transition`, fire on
    // every already-matching item at once — exactly the flood this gate
    // exists to prevent, just triggered by "gained a contract" instead of
    // "contract changed."
    let fingerprint_changed =
        !is_first_poll && scratchpad.contract_fingerprint.as_deref() != Some(live_fingerprint.as_str());

    // A keygen version bump changes `identity_key`'s output for the same
    // payload (e.g. the identity-text normalization `IDENTITY_KEYGEN_VERSION`
    // 2 was introduced alongside): every key in `snapshots` was hashed under
    // the old rules, so diffing them against freshly-hashed keys would read
    // the entire existing backlog as unrecognized and mass-fire on it — the
    // same flood `fingerprint_changed` guards against, just triggered by
    // "the engine's hashing changed" instead of "the contract changed."
    let keygen_changed =
        !is_first_poll && scratchpad.identity_keygen_version != Some(IDENTITY_KEYGEN_VERSION);

    // A brand-new watch's first-ever poll and an amended (or newly bound)
    // contract both need the same treatment: record where every currently
    // observed item stands right now without treating any of it as a
    // transition worth firing on. Applying the decision table unmodified
    // here would flood the user — either on a pre-existing backlog (first
    // poll) or on snapshot keys that may no longer mean the same thing under
    // the new contract (amendment). Re-keying an amendment instead of
    // re-seeding is explicitly out of scope for v1 — a
    // wrong migration would ship a worse bug than the one this feature
    // fixes.
    //
    // This line is the enforcement point for the locked never-fire-from-
    // history policy — read the "Locked policy" section in this module's
    // header before changing it. Short version: firing has irreversible
    // external side effects, so a backlog must never be fired on, and the
    // remedy for the resulting invisibility is louder disclosure (see
    // `seed_baseline_disclosure_message`), never a firing seed tick.
    let seed_only = is_first_poll || fingerprint_changed || keygen_changed;
    // Suppresses firing for `force_seed_only` too (see this function's own
    // doc) without pulling it into `seed_only` itself — `seed_only` also
    // drives `edge_counter` reset and the baseline-disclosure message below,
    // neither of which a mid-life extraction hiccup should trigger.
    let suppress_fire = seed_only || force_seed_only;

    if fingerprint_changed {
        scratchpad.snapshots.clear();
        warn!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            "agent watch: contract fingerprint changed since the last poll; re-seeding snapshots, no fire this tick"
        );
        emit_health_event(
            event_bus,
            assignment,
            format!(
                "Agent watch \"{}\"'s contract was amended since its last poll — re-seeding its \
                 observation baseline under the new contract. Nothing was reported as new on this poll.",
                assignment.name
            ),
        )
        .await;
    }
    scratchpad.contract_fingerprint = Some(live_fingerprint);
    scratchpad.identity_keygen_version = Some(IDENTITY_KEYGEN_VERSION);

    let now = Utc::now().to_rfc3339();
    // Third element is the candidate's `identity_key` at fire time, carried
    // alongside its `delivery_key` purely so a successful fire can record a
    // legible `Pending` ledger entry (`AssignmentScratchpad::record_pending_action`)
    // — `identity_key` itself is moved into this candidate's `ItemSnapshot`
    // a few lines below, so it has to be cloned here or it wouldn't survive
    // that far.
    let mut to_fire: Vec<(&AgentWatchCandidate, String, String)> = Vec::new();
    // Amendment-trigger tracking: true if
    // any candidate this poll was missing a `required: true` field from
    // `contract.fields` — the ONLY valid amendment trigger. Evaluated once
    // per poll (not per candidate) below the loop.
    let mut poll_had_missing_required_field = false;
    // "Bound and matching nothing" tracking (this feature): every candidate
    // that makes it past all three quarantine checks below counts as a
    // survivor, regardless of whether it goes on to match the predicate or
    // fire — surviving quarantine, not matching, is what this signal is
    // about. Paired with `candidates.len()` after the loop to populate
    // `AssignmentScratchpad::last_poll_observed_candidates`/
    // `last_poll_surviving_candidates`.
    let mut surviving_candidate_count: u32 = 0;
    // Counts each distinct `ContractError` display string a candidate was
    // quarantined for this poll — not per-candidate-id, since the aggregated
    // health event below names the *dominant* reason, not every individual
    // one (that is already covered by the per-candidate `quarantine_candidate`
    // events).
    let mut quarantine_reason_counts: HashMap<String, u32> = HashMap::new();
    // Aggregate across the whole tick: `record_snapshot`
    // evicts one oldest entry per push once over cap, so summing per-candidate
    // would flood a health event per candidate instead of one per tick. See the
    // aggregate emission below the loop.
    let mut total_dropped_count: usize = 0;
    // Candidates this seeding tick excluded from firing solely because they
    // were already matching when the baseline was recorded — the
    // never-fire-from-history policy stays in force, but the user gets told
    // what it silently excluded instead of seeing a healthy-looking watch
    // that never fires. Collected here (not re-derived after the loop) since
    // `prev` is never seeded yet on a `seed_only` tick, so `is_matching`
    // alone is exactly "already matching." Reported once, below the loop.
    let mut seed_matches: Vec<&AgentWatchCandidate> = Vec::new();

    for candidate in &candidates {
        let payload = &candidate.payload;

        // A `required: true` extraction field missing from this candidate
        // is its own fail-closed quarantine, independent
        // of and checked before identity/version/predicate — none of those
        // depend on it, but a poll that can't extract what the contract
        // asked for cannot be trusted either.
        let missing_required = missing_required_fields(contract, payload);
        if !missing_required.is_empty() {
            poll_had_missing_required_field = true;
            let error = ContractError::MissingFields(missing_required);
            *quarantine_reason_counts.entry(error.to_string()).or_insert(0) += 1;
            quarantine_candidate(event_bus, assignment, candidate, &error).await;
            continue;
        }

        // Fail closed: identity/version evaluation below
        // can fail on a malformed observation (a missing required field, a
        // relayed id that doesn't match the declared format). Neither may
        // ever resolve to "treat it as new" — the candidate is quarantined
        // (logged, surfaced as a health event, skipped) instead. Predicate
        // evaluation itself can no longer fail this way: `contract.predicate`
        // is a typed `Predicate`, well-formed by construction (see
        // `ao_protocol::watch_contract::WatchContract::validate`'s doc) —
        // the "can't parse" failure mode retired along with the old
        // string-`expr` evaluator.
        let identity_key = match identity_key(contract, payload) {
            Ok(k) => k,
            Err(e) => {
                *quarantine_reason_counts.entry(e.to_string()).or_insert(0) += 1;
                quarantine_candidate(event_bus, assignment, candidate, &e).await;
                continue;
            }
        };
        let version_key = match version_key(contract, payload) {
            Ok(k) => k,
            Err(e) => {
                *quarantine_reason_counts.entry(e.to_string()).or_insert(0) += 1;
                quarantine_candidate(event_bus, assignment, candidate, &e).await;
                continue;
            }
        };

        // Survived every quarantine check above (this feature) — counted
        // regardless of whether it goes on to match the predicate or fire;
        // see `surviving_candidate_count`'s own doc above the loop.
        surviving_candidate_count += 1;

        // Looked up before evaluating the predicate (rather than after, as
        // this used to run) so `Predicate::Changed` can compare against the
        // same item's last observed payload when there is one — `None` only
        // for an item with no prior snapshot under this `identity_key` (a
        // first-ever observation, which `Predicate::Changed` itself treats
        // as a change; see `ao_protocol::predicate::Predicate::Changed`'s
        // doc).
        let prev = scratchpad.snapshots.iter().find(|s| s.identity_key == identity_key).cloned();
        let is_matching = evaluate_predicate(contract, payload, prev.as_ref().map(|p| &p.payload));

        if seed_only && is_matching {
            seed_matches.push(candidate);
        }

        let was_matching = prev.as_ref().map(|p| p.predicate_value).unwrap_or(false);
        // `predicate(None) == false`, so a brand-new already-matching item
        // and an existing item that just started matching both land here —
        // that is the whole point of transition semantics.
        let transitioned_into_matching = !was_matching && is_matching;

        let edge_counter = if seed_only {
            0
        } else {
            prev.as_ref().map(|p| p.edge_counter).unwrap_or(0) + if transitioned_into_matching { 1 } else { 0 }
        };

        let should_fire = !suppress_fire
            && match contract.mode {
                WatchMode::PredicateTransition => transitioned_into_matching,
                WatchMode::NewOrChanged => prev.as_ref().map(|p| p.version_key != version_key).unwrap_or(true),
                WatchMode::NewOnly => prev.is_none(),
            };

        // Computed (borrowing identity_key/version_key) before the upsert
        // below moves them into the snapshot. `identity_key` is cloned here
        // too — `to_fire` carries it through to the pending-ledger record
        // below, since the original binding won't survive the move into
        // `ItemSnapshot` a few lines down.
        let delivery = should_fire
            .then(|| (delivery_key(&assignment.id, &identity_key, &version_key, edge_counter), identity_key.clone()));

        // ALWAYS upsert, fired or not — a quiet tick's
        // snapshot is exactly the prior state a future transition diffs
        // against.
        if let Some(truncation) = scratchpad.record_snapshot(ItemSnapshot {
            identity_key,
            version_key,
            predicate_value: is_matching,
            edge_counter,
            last_seen_at: now.clone(),
            payload: payload.clone(),
        }) {
            // A watch over a source larger than `SNAPSHOT_CAP` evicts its
            // oldest snapshot here — silent eviction would read as "new
            // item" and could re-fire on something already seen.
            // Accumulated and reported once, below the loop,
            // rather than per-candidate — `record_snapshot` evicts exactly
            // one oldest entry per push once over cap, so emitting here
            // would fire one health event per candidate instead of one per
            // tick.
            total_dropped_count += truncation.dropped_count;
        }

        if let Some((key, item_identity_key)) = delivery {
            // Idempotency check at the send boundary: the
            // decision-table transition above already implies this should
            // fire, but the action ledger is the generic primitive that
            // makes even a non-email action dedupable, independent of that
            // decision.
            if scratchpad.has_seen_delivery(&key, SystemTime::now()) {
                info!(
                    assignment_id = %assignment.id,
                    candidate_id = %candidate.id,
                    "agent watch: transition already recorded in the action ledger; skipping duplicate fire"
                );
            } else {
                to_fire.push((candidate, key, item_identity_key));
            }
        }
    }

    // Kill the silence, keep the exclusion: a `seed_only` tick with at least
    // one already-matching candidate stays fully quiet toward the agent (the
    // never-fire-from-history policy above is unmodified by this), but the
    // user still needs to know their new watch found a backlog it will not
    // act on. One aggregated message per seeding tick, never one per
    // candidate — same shape as the truncation-latch aggregation below.
    // Held rather than emitted immediately: a seeding tick over a source
    // larger than `SNAPSHOT_CAP` (first poll on an oversized source) also
    // trips the truncation latch below in the same tick, and the two must
    // land as one health event, not two independent ones.
    let mut seed_disclosure = if !seed_matches.is_empty() {
        Some(seed_baseline_disclosure_message(&assignment.name, &seed_matches))
    } else {
        None
    };

    // Edge-triggered latch: a source that stays over
    // `SNAPSHOT_CAP` across many ticks must warn exactly once for the life of
    // that condition, not re-fire on every poll — otherwise a watch degraded
    // by an oversized source becomes noisier than one that just stayed
    // silent. Fire only on the false -> true transition of "this tick
    // dropped anything." The latch clears only when `snapshots.len()` is
    // genuinely back under `SNAPSHOT_CAP` — being AT cap is itself the
    // degraded condition, since the very next never-before-seen identity
    // will be evicted, and `record_snapshot` drains back to exactly
    // `SNAPSHOT_CAP` (never below) once it's been hit. This tick's drop
    // count is a bad proxy for "no longer degraded": an empty poll, a short
    // poll, or a poll that only re-observes already-tracked identities all
    // drop zero while the set stays pinned at cap, so gating the clear on
    // drop count made the latch flap clear-and-rearm on the very next quiet
    // tick instead of holding for the life of the condition. Checking
    // `snapshots.len()` directly instead means the latch only re-arms once
    // something actually prunes the set back below cap (a contract
    // amendment re-seeding from empty, today's only such path).
    if total_dropped_count > 0 && !scratchpad.truncation_notified {
        warn!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            dropped_count = total_dropped_count,
            retained_count = SNAPSHOT_CAP,
            "agent watch: snapshot store exceeded its cap; oldest entries evicted"
        );
        let truncation_message = format!(
            "Agent watch \"{}\" is tracking more than {SNAPSHOT_CAP} items, so it dropped its {} \
             oldest observation(s) to stay within that limit. If a dropped item shows up again on a \
             later poll, it will look brand new and may be reported to you a second time.",
            assignment.name, total_dropped_count
        );
        let message = match seed_disclosure.take() {
            Some(seed_text) => format!("{truncation_message}\n\n{seed_text}"),
            None => truncation_message,
        };
        emit_health_event(event_bus, assignment, message).await;
        scratchpad.truncation_notified = true;
    }
    if scratchpad.snapshots.len() < SNAPSHOT_CAP {
        scratchpad.truncation_notified = false;
    }
    if let Some(seed_text) = seed_disclosure {
        emit_health_event(event_bus, assignment, seed_text).await;
    }

    // Amendment trigger: the ONLY valid trigger is N
    // consecutive polls with at least one missing-required-field candidate
    // — never a mid-poll decision, and never based on a single blip. A poll
    // that observed zero candidates at all leaves the streak untouched:
    // there is nothing to judge extraction quality from.
    if !candidates.is_empty() {
        scratchpad.missing_required_field_streak = if poll_had_missing_required_field {
            scratchpad.missing_required_field_streak.saturating_add(1)
        } else {
            0
        };
    }

    // "Bound and matching nothing" (this feature — see
    // `AssignmentScratchpad::all_candidates_quarantined_streak`'s own doc):
    // distinct from the missing-required-field amendment trigger above,
    // which only tracks one narrow quarantine reason and only matters
    // once it crosses `REQUIRED_FIELD_FAILURE_AMENDMENT_THRESHOLD` consecutive
    // polls. This tracks EVERY quarantine reason (missing field, format
    // mismatch, invalid identity strategy, ...) and must be visible the very
    // poll it happens on — a watch that is bound, receiving candidates, and
    // rejecting all of them must never look the same as a watch that simply
    // has nothing new (design rule: "if the engine detects it, the user sees
    // it"). A poll that observed zero candidates leaves the streak untouched,
    // mirroring the missing-required-field streak immediately above: there is
    // nothing to judge "matching nothing" from when nothing was observed at
    // all.
    scratchpad.last_poll_observed_candidates = candidates.len() as u32;
    scratchpad.last_poll_surviving_candidates = surviving_candidate_count;
    if !candidates.is_empty() {
        scratchpad.all_candidates_quarantined_streak = if surviving_candidate_count == 0 {
            scratchpad.all_candidates_quarantined_streak.saturating_add(1)
        } else {
            0
        };
    }
    if !candidates.is_empty() && surviving_candidate_count == 0 {
        let dominant_reason = dominant_quarantine_reason(&quarantine_reason_counts);
        warn!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            observed = scratchpad.last_poll_observed_candidates,
            streak = scratchpad.all_candidates_quarantined_streak,
            reason = %dominant_reason,
            "agent watch: every candidate this poll was quarantined; bound but matching nothing"
        );
        emit_health_event(
            event_bus,
            assignment,
            format!(
                "Agent watch \"{}\" is bound and observed {} candidate(s) on this poll, but quarantined every \
                 one of them — none were recorded as new or eligible to fire. This is not the same as a quiet \
                 poll: the watch is receiving candidates, but none currently pass its contract. Dominant reason: \
                 {dominant_reason}.",
                assignment.name,
                scratchpad.last_poll_observed_candidates,
            ),
        )
        .await;
    }

    if scratchpad.missing_required_field_streak >= REQUIRED_FIELD_FAILURE_AMENDMENT_THRESHOLD {
        scratchpad.missing_required_field_streak = 0;
        // Unconditional: this is what lets the `== CEILING + 1` check below
        // land on exactly one poll, the same "increment, then compare"
        // shape `run_authoring_and_legacy_tick` uses for
        // `authoring_failure_streak`/`AUTHORING_FAILURE_CEILING`.
        scratchpad.contract_amendment_cycle_count = scratchpad.contract_amendment_cycle_count.saturating_add(1);
        let cycle = scratchpad.contract_amendment_cycle_count;

        if cycle > CONTRACT_AMENDMENT_CYCLE_CEILING {
            // Bounded amendment cycle: a repeated amendment has already
            // proven it does not change the outcome, so stop clearing and
            // leave the (still bad) contract bound rather than
            // amend-reseed forever. Per-candidate quarantine events (see
            // `quarantine_candidate`) keep the underlying problem visible
            // on every later poll, so this is a deliberate stop, not a
            // silent one — and the transition into it, below, is its own
            // loud health event, emitted exactly once.
            warn!(
                assignment_id = %assignment.id,
                assignment_name = %assignment.name,
                cycle,
                "agent watch: missing-required-field threshold hit again past the amendment-cycle ceiling; \
                 leaving the contract bound rather than amending again"
            );
            if cycle == CONTRACT_AMENDMENT_CYCLE_CEILING + 1 {
                emit_health_event(
                    event_bus,
                    assignment,
                    format!(
                        "Agent watch \"{}\" is now unhealthy: it has amended its contract \
                         {CONTRACT_AMENDMENT_CYCLE_CEILING} times and the newly authored contract still hits \
                         the same missing-required-field problem. Automatic re-authoring has stopped — this \
                         watch needs manual review: check that the source actually provides the field(s) the \
                         contract requires, or edit the watch's instruction.",
                        assignment.name
                    ),
                )
                .await;
            }
        } else {
            warn!(
                assignment_id = %assignment.id,
                assignment_name = %assignment.name,
                threshold = REQUIRED_FIELD_FAILURE_AMENDMENT_THRESHOLD,
                cycle,
                ceiling = CONTRACT_AMENDMENT_CYCLE_CEILING,
                "agent watch: consecutive missing-required-field polls hit the amendment threshold; clearing the \
                 contract so authoring re-runs next poll"
            );
            let message = if cycle == CONTRACT_AMENDMENT_CYCLE_CEILING {
                format!(
                    "Agent watch \"{}\" has now amended its contract {CONTRACT_AMENDMENT_CYCLE_CEILING} times \
                     without resolving a missing-required-field problem — re-authoring one last time. If the \
                     newly authored contract hits the same problem again, this watch will stop re-authoring \
                     automatically and will need manual review.",
                    assignment.name
                )
            } else {
                format!(
                    "Agent watch \"{}\" has been missing a required field for {REQUIRED_FIELD_FAILURE_AMENDMENT_THRESHOLD} \
                     polls in a row — re-authoring its contract on the next poll.",
                    assignment.name
                )
            };
            emit_health_event(event_bus, assignment, message).await;

            // Problem-A reset: this contract is being cleared right here,
            // so the state computed against it stops meaning anything.
            // `contract_amendment_cycle_count` is deliberately left alone —
            // it is what bounds this very loop across contracts.
            scratchpad.contract_fingerprint = None;
            scratchpad.snapshots.clear();
            scratchpad.truncation_notified = false;
            scratchpad.authoring_failure_streak = 0;
            scratchpad.all_candidates_quarantined_streak = 0;
            scratchpad.clear_extraction_plan();
            scratchpad.extraction_plan_degraded = false;
            scratchpad.extraction_plan_degraded_reason = None;

            if let Err(e) = set_assignment_contract(persistence, assignment, None, None, None).await {
                warn!(
                    assignment_id = %assignment.id,
                    error = %e,
                    "agent watch: failed to clear the contract to trigger re-authoring"
                );
            }
        }
    }

    if to_fire.is_empty() {
        scratchpad.record_poll_outcome(false, &now);
        if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
            warn!(assignment_id = %assignment.id, error = %e, "agent watch: failed to persist scratchpad after a quiet tick");
        }
        info!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            candidate_count = candidates.len(),
            seed_only,
            force_seed_only,
            "agent watch: result — no transition to fire on; scratchpad persisted"
        );
        return false;
    }

    let fired: Vec<&AgentWatchCandidate> = to_fire.iter().map(|(c, _, _)| *c).collect();
    let event_context = build_event_context(&fired);
    let fired_summary = event_context.summary.clone();

    // Two-phase ledger, match-time write (module doc, above): every to-fire
    // item is recorded `Pending` — and that, plus this tick's snapshot
    // upserts above, persisted — *before* dispatch is even attempted. A
    // crash between here and the dispatch call below still leaves a durable
    // trace rather than silently discarding the finding; a dispatch call
    // that then fails outright leaves these same entries `Pending` with no
    // `run_id`, which `reconcile_pending_deliveries` already treats as
    // stuck the same as a silent post-enqueue crash.
    let recorded_at = SystemTime::now();
    for (_, key, item_identity_key) in &to_fire {
        scratchpad.record_pending_action(key, item_identity_key, recorded_at);
    }
    if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
        warn!(assignment_id = %assignment.id, error = %e, "agent watch: failed to persist scratchpad before dispatch");
    }

    match fire_assignment(
        persistence,
        dispatcher,
        event_bus,
        assignment,
        AssignmentTriggerKind::AgentWatch,
        Some(fired_summary.clone()),
        timezone,
        Some(event_context),
    )
    .await
    {
        Ok(run) => {
            // Still `Pending`, not promoted to `Confirmed` (module doc,
            // above): `fire_assignment` returning `Ok` only proves the
            // message reached the target agent's queue, not that the queued
            // turn ran. `run.id` is the correlator `reconcile_pending_deliveries`
            // looks up on a later tick to find out.
            for (_, key, _) in &to_fire {
                scratchpad.attach_dispatch_run(key, run.id.clone());
            }
            scratchpad.record_poll_outcome(true, &now);
            if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
                warn!(assignment_id = %assignment.id, error = %e, "agent watch: failed to persist scratchpad after firing");
            }
            info!(
                assignment_id = %assignment.id,
                assignment_name = %assignment.name,
                summary = %fired_summary,
                "agent watch: FIRED — dispatched to agent"
            );
            true
        }
        Err(e) => {
            // Nothing was actually sent — the entries recorded above stay
            // `Pending` with no `run_id` at all, so `reconcile_pending_deliveries`
            // will retry them once they've been stuck long enough (see
            // `PENDING_DELIVERY_RETRY_POLL_THRESHOLD`). Surfaced immediately
            // rather than waiting for that reconciliation pass, since the
            // real failure reason is already known right here — "if the
            // engine detects it, the user sees it" applies the moment the
            // engine knows, not once a threshold happens to elapse.
            warn!(
                assignment_id = %assignment.id,
                assignment_name = %assignment.name,
                error = %e,
                "agent watch: FAILED to dispatch a fire; entries stay Pending with no run_id and will be retried"
            );
            let identity_keys: Vec<&str> = to_fire.iter().map(|(_, _, k)| k.as_str()).collect();
            emit_health_event(
                event_bus,
                assignment,
                format!(
                    "Agent watch \"{}\" matched {} new item(s) ({}) but failed to dispatch them to the agent: \
                     {e}. They remain recorded and will be retried automatically if the failure persists.",
                    assignment.name,
                    to_fire.len(),
                    identity_keys.join(", "),
                ),
            )
            .await;
            scratchpad.record_poll_outcome(false, &now);
            if let Err(persist_err) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
                warn!(assignment_id = %assignment.id, error = %persist_err, "agent watch: failed to persist scratchpad after a dispatch failure");
            }
            false
        }
    }
}

/// Builds the one seed-baseline disclosure message [`run_contract_bound_tick`]
/// emits when a `seed_only` tick observes `seed_matches` (already-guaranteed
/// non-empty by the caller). Names up to [`SEED_DISCLOSURE_MAX_NAMED`]
/// candidates by [`AgentWatchCandidate::summary`] — each truncated to
/// [`SEED_DISCLOSURE_SUMMARY_CHARS`] characters so one long summary can't
/// dominate the message — with any remainder folded into a single
/// "...and N more" tail instead of listing a whole backlog.
fn seed_baseline_disclosure_message(assignment_name: &str, seed_matches: &[&AgentWatchCandidate]) -> String {
    let total = seed_matches.len();
    let named = seed_matches
        .iter()
        .take(SEED_DISCLOSURE_MAX_NAMED)
        .map(|c| {
            if c.summary.chars().count() > SEED_DISCLOSURE_SUMMARY_CHARS {
                let head: String = c.summary.chars().take(SEED_DISCLOSURE_SUMMARY_CHARS).collect();
                format!("- {head}...")
            } else {
                format!("- {}", c.summary)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tail = if total > SEED_DISCLOSURE_MAX_NAMED {
        format!("\n...and {} more", total - SEED_DISCLOSURE_MAX_NAMED)
    } else {
        String::new()
    };
    format!(
        "Agent watch \"{assignment_name}\" found {total} item(s) that already match its predicate while \
         recording its observation baseline. They will NOT be acted on — the watch never fires from \
         pre-existing history, only on items that begin matching from now on. If any of these still need \
         handling, take care of them by hand:\n{named}{tail}"
    )
}

/// Emits a health-panel-visible system event naming `assignment` and a
/// specific failure — the same `assignment:{id}` SSE
/// channel / `AgentEventPayload::SystemMessage` shape
/// `assignment_runner::fire_assignment` already uses for its own
/// user-visible "run started" notice, so this surfaces the same way without
/// inventing a second event shape.
async fn emit_health_event(event_bus: &Arc<EventBus>, assignment: &Assignment, text: String) {
    emit_health_event_with_severity(event_bus, assignment, text, None).await;
}

/// Same channel as [`emit_health_event`], but lets a caller tag the bubble
/// with a [`SystemMessageSeverity`] — presently only the authoring
/// convergence/retry/freeze messages in [`reject_proposal`] and
/// [`author_contract`] have an opinion about tone; every other health event
/// keeps calling the plain [`emit_health_event`] above and renders exactly as
/// it always has.
async fn emit_health_event_with_severity(
    event_bus: &Arc<EventBus>,
    assignment: &Assignment,
    text: String,
    severity: Option<SystemMessageSeverity>,
) {
    event_bus
        .emit(
            &format!("assignment:{}", assignment.id),
            &assignment.agent_id,
            None,
            AgentEventPayload::SystemMessage { text, severity },
        )
        .await;
}

/// Reconciles every [`DeliveryStatus::Pending`] entry in
/// `scratchpad.seen_deliveries` against the `AssignmentRun` its dispatch
/// created (if any) — the promotion half of the two-phase ledger the module
/// doc describes. A `Pending` entry with a `run_id` means `fire_assignment`
/// successfully handed the message to the target agent's queue, but nothing
/// has since confirmed the queued turn actually ran: the production pump
/// (`queue_manager::mark_assignment_run_succeeded`/`_failed`) is the only
/// thing that ever writes a terminal `AssignmentRunStatus` onto that row,
/// driven by the runner's own completion signal or (if the runner panics or
/// errors without ever emitting one) the outer runner-failure watcher — so
/// observing a terminal status here means the turn genuinely reached an end,
/// not that this function is guessing. A `Pending` entry with no `run_id` at
/// all means the dispatch attempt itself failed outright (see the `Err`
/// branch above) — there is no run to look up, so it is treated as stuck
/// from the first reconciliation pass that sees it.
///
/// An entry whose run has reached `Succeeded` or `Failed` promotes to
/// `Confirmed` — either way the turn ran, so there is nothing left to wait
/// on or retry. `Failed` promotes exactly like `Succeeded`: the *ledger's*
/// job is only "do we know what happened," not "did it happen cleanly," and
/// a genuine failure already gets its own visibility through the
/// `AssignmentRun` row and `mark_assignment_run_failed`'s own health event —
/// duplicating that here would be noise, and a failed turn must not be
/// retried either, since it may have partially completed an irreversible
/// action before erroring.
///
/// An entry that hasn't reached a terminal status accrues
/// [`SeenDelivery::pending_poll_count`] by one every pass. While that count
/// stays at or under [`PENDING_DELIVERY_RETRY_POLL_THRESHOLD`] the entry is
/// simply left `Pending` — still counted "seen" by
/// [`AssignmentScratchpad::has_seen_delivery`], so the item is not
/// re-dispatched out from under an ordinary in-flight turn. Once the count
/// exceeds the threshold the entry is treated as stuck: its ledger entry and
/// its `ItemSnapshot` are both cleared (see
/// [`AssignmentScratchpad::clear_pending_delivery`]), which makes the item
/// retry-eligible — the diff loop above (which runs before this function
/// returns control to its caller for the rest of the tick) will fire on it
/// again as a fresh transition if it's still observed matching, recording a
/// brand-new `Pending` entry with its own poll count starting over at `0` —
/// and an unhealthy health event discloses that the retry happened and why,
/// naming the item.
async fn reconcile_pending_deliveries(
    persistence: &Arc<PersistenceLayer>,
    event_bus: &Arc<EventBus>,
    assignment: &Assignment,
    scratchpad: &mut AssignmentScratchpad,
) {
    let pending_indices: Vec<usize> = scratchpad
        .seen_deliveries
        .iter()
        .enumerate()
        .filter(|(_, d)| d.status == DeliveryStatus::Pending)
        .map(|(i, _)| i)
        .collect();

    for i in pending_indices {
        let run_id = scratchpad.seen_deliveries[i].run_id.clone();

        let reached_terminal_status = match &run_id {
            Some(run_id) => match persistence.assignment_runs.get(&assignment.id, run_id).await {
                Ok(Some(run)) => matches!(run.status, AssignmentRunStatus::Succeeded | AssignmentRunStatus::Failed),
                Ok(None) => false,
                Err(e) => {
                    warn!(
                        assignment_id = %assignment.id,
                        run_id = %run_id,
                        error = %e,
                        "agent watch: failed to look up a dispatched run while reconciling a pending delivery; leaving it pending"
                    );
                    false
                }
            },
            // No run was ever dispatched — the fire attempt itself failed —
            // so there is nothing to look up and this entry is stuck by
            // definition.
            None => false,
        };

        if reached_terminal_status {
            let key = scratchpad.seen_deliveries[i].id.clone();
            scratchpad.confirm_pending_delivery(&key);
            continue;
        }

        scratchpad.seen_deliveries[i].pending_poll_count =
            scratchpad.seen_deliveries[i].pending_poll_count.saturating_add(1);
        if scratchpad.seen_deliveries[i].pending_poll_count <= PENDING_DELIVERY_RETRY_POLL_THRESHOLD {
            continue;
        }

        let entry = &scratchpad.seen_deliveries[i];
        let key = entry.id.clone();
        let identity_key = entry.identity_key.clone().unwrap_or_else(|| "<unknown item>".to_string());
        let poll_count = entry.pending_poll_count;

        warn!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            identity_key = %identity_key,
            run_id = run_id.as_deref().unwrap_or("<none — the dispatch attempt itself failed>"),
            poll_count,
            "agent watch: a pending delivery has stayed unconfirmed past the retry threshold; clearing it to retry"
        );

        scratchpad.clear_pending_delivery(&key);
        scratchpad.snapshots.retain(|s| s.identity_key != identity_key);

        emit_health_event(
            event_bus,
            assignment,
            format!(
                "Agent watch \"{}\" dispatched an action for \"{identity_key}\" but never confirmed that the \
                 agent turn carrying it out actually completed, across {poll_count} polls — the server may have \
                 restarted before that turn ran, or it is still stuck in the queue. This item is being retried \
                 now if it still matches; if the original action actually did go through, this retry may \
                 duplicate it, so check the assignment's run history if that matters here.",
                assignment.name,
            ),
        )
        .await;
    }
}

/// Fail-closed handling for a candidate whose identity/version/predicate
/// evaluation returned a [`ContractError`]: logged at warn,
/// surfaced as a health event, and — since the caller `continue`s right
/// after — never stored in `snapshots` and never fired on. "I don't know
/// whether this is new" must never resolve to "send the notification."
async fn quarantine_candidate(
    event_bus: &Arc<EventBus>,
    assignment: &Assignment,
    candidate: &AgentWatchCandidate,
    error: &ContractError,
) {
    warn!(
        assignment_id = %assignment.id,
        assignment_name = %assignment.name,
        candidate_id = %candidate.id,
        error = %error,
        "agent watch: quarantining a candidate that failed its contract — not treated as new, will not fire"
    );
    emit_health_event(
        event_bus,
        assignment,
        format!("Agent watch \"{}\" quarantined an observation ({}): {error}", assignment.name, candidate.id),
    )
    .await;
}

/// Picks the most-frequent [`ContractError`] display string out of one
/// poll's `quarantine_reason_counts` (`run_contract_bound_tick`'s per-tick
/// tally, keyed by `ContractError::to_string()`) — the single reason named
/// in the "bound and matching nothing" aggregated health event, so that
/// event says *why* a poll rejected everything instead of just *that* it
/// did. Ties are broken by picking the lexicographically smallest reason
/// string, purely for deterministic output — no significance is attached to
/// the ordering itself. Never called with an empty map by its only caller
/// (a poll that reaches it has, by construction, quarantined at least one
/// candidate), but falls back to a placeholder rather than panicking if that
/// invariant is ever violated.
fn dominant_quarantine_reason(counts: &HashMap<String, u32>) -> String {
    counts
        .iter()
        .max_by(|(reason_a, count_a), (reason_b, count_b)| count_a.cmp(count_b).then_with(|| reason_b.cmp(reason_a)))
        .map(|(reason, _)| reason.clone())
        .unwrap_or_else(|| "unknown reason".to_string())
}

/// Names of `contract.fields` entries marked `required: true` that are
/// missing, null, or blank in `payload` — the signal
/// [`run_contract_bound_tick`] tracks in
/// [`AssignmentScratchpad::missing_required_field_streak`] to drive the
/// amendment trigger. Deliberately independent of
/// `identity_key`/`version_key`'s own [`ContractError::MissingField`]
/// checks, which are about `identity.*`, not the generic extraction
/// contract.
fn missing_required_fields(contract: &WatchContract, payload: &Value) -> Vec<String> {
    contract
        .fields
        .iter()
        .filter(|(_, spec)| spec.required)
        .filter(|(name, _)| match payload.get(name.as_str()) {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => s.trim().is_empty(),
            Some(_) => false,
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Deterministic, content-derived dedupe key for one candidate on the legacy
/// (pre-`WatchContract`) fallback — [`run_legacy_seen_ids_tick`] persists
/// and diffs on this, never on [`AgentWatchCandidate::id`]. A detector is
/// free to mint a different `id` for the exact same physical item on every
/// poll (see that field's own doc); a key derived purely from the item's own
/// observed content mints the SAME key every time by construction, so it is
/// the only thing here safe to treat as identity.
///
/// Mirrors `ao_protocol::watch_contract`'s `ContentHash` identity strategy
/// (same normalize/canonicalize/hash recipe, via the same
/// `ao_protocol::contract_primitives` this shares with it): fold away
/// cosmetic variance with [`normalize_value_for_identity`] (trims, collapses
/// internal whitespace, lowercases, folds confusables — recursively over
/// every string leaf), serialize in a stable field order with
/// [`canonical_json`] (object keys sorted), then [`sha256_hex`]. Falls back
/// to `summary` when `payload` is empty or not an object — some detectors
/// never populate it — since hashing an empty object would collide every
/// distinct item onto the same key and defeat dedup entirely.
///
/// If the SAME physical item's content is genuinely unstable across polls
/// (not just its `id`), this mints a different key every time and the item
/// looks new every poll — deliberately: that is a real data-quality problem
/// with the source or the detector, and surfacing it as repeated (accurate)
/// fires is more honest than silently absorbing it into a fuzzy match.
fn legacy_candidate_key(candidate: &AgentWatchCandidate) -> String {
    let content = match &candidate.payload {
        Value::Object(map) if !map.is_empty() => candidate.payload.clone(),
        _ => Value::String(candidate.summary.clone()),
    };
    sha256_hex(&canonical_json(&normalize_value_for_identity(&content)))
}

/// Trims and lowercases a legacy dedupe key before it's compared or
/// persisted — belt-and-suspenders on top of [`legacy_candidate_key`]
/// already emitting lowercase hex: `scratchpad.seen_ids` is a plain
/// `Vec<String>` with no format enforced at the type level, so nothing at
/// the type system stops a hand-edited or differently-cased entry from
/// silently defeating an exact-string comparison.
fn normalize_seen_key(key: &str) -> String {
    key.trim().to_lowercase()
}

/// A [`legacy_candidate_key`] output: exactly 64 hex digits (a sha256 digest
/// in hex). Used to tell a post-upgrade `seen_ids` entry (a content-derived
/// key) apart from a pre-upgrade one (an arbitrary model-minted `id` string)
/// — see [`run_legacy_seen_ids_tick`]'s upgrade branch. Deliberately just a
/// shape check, not a cryptographic one: nothing here needs to verify the
/// digest is genuine, only to notice a string that could not possibly be one.
fn is_content_hash_key(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Legacy diff (pre-`WatchContract`): exact-match against
/// `scratchpad.seen_ids`, keyed by [`legacy_candidate_key`] rather than
/// [`AgentWatchCandidate::id`] — kept otherwise unchanged for any assignment
/// that hasn't authored a contract yet — a live watch must not reset or
/// re-fire just because a sibling watch has upgraded.
///
/// `seed_only` is `true` on two distinct occasions, both meaning "there is
/// nothing trustworthy to diff `candidates` against yet, so just record them
/// and don't fire": a watch's actual first poll (no `seen_ids` baseline
/// exists at all), and every poll of a watch [`run_authoring_and_legacy_tick`]
/// has frozen at [`AUTHORING_FAILURE_CEILING`] (no bound contract was ever
/// authored, so this watch is deliberately never notified again until
/// authoring succeeds or its input changes — see that function's own doc).
///
/// A THIRD, internally-detected occasion sits in front of both of those: a
/// `scratchpad.seen_ids` still carrying pre-upgrade model-minted ids (this
/// function shipped before content-derived keys existed) has nothing in the
/// new key space to diff against — every already-seen item would otherwise
/// look brand new the moment this ships, firing on the whole backlog at
/// once. That scratchpad is re-baselined onto the new key space for exactly
/// one poll (replacing the stale ids outright, since they can never match a
/// content key again and leaving them mixed in would keep tripping this same
/// branch forever) and does not fire; every poll after it sees a pure
/// hex-keyed `seen_ids` and takes the normal path below.
async fn run_legacy_seen_ids_tick(
    persistence: &Arc<PersistenceLayer>,
    dispatcher: &Arc<dyn NotificationDispatcher>,
    event_bus: &Arc<EventBus>,
    assignment: &Assignment,
    timezone: Option<&str>,
    mut scratchpad: AssignmentScratchpad,
    seed_only: bool,
    candidates: Vec<AgentWatchCandidate>,
) -> bool {
    let now = Utc::now().to_rfc3339();
    let keyed_candidates: Vec<(&AgentWatchCandidate, String)> =
        candidates.iter().map(|c| (c, normalize_seen_key(&legacy_candidate_key(c)))).collect();

    let upgrading_from_legacy_ids =
        !scratchpad.seen_ids.is_empty() && scratchpad.seen_ids.iter().any(|id| !is_content_hash_key(id));
    if upgrading_from_legacy_ids {
        scratchpad.seen_ids = keyed_candidates.iter().map(|(_, key)| key.clone()).collect();
        if scratchpad.seen_ids.len() > SEEN_IDS_CAP {
            let excess = scratchpad.seen_ids.len() - SEEN_IDS_CAP;
            scratchpad.seen_ids.drain(0..excess);
        }
        scratchpad.record_poll_outcome(false, &now);
        if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
            warn!(assignment_id = %assignment.id, error = %e, "agent watch: failed to persist scratchpad while upgrading legacy seen_ids");
        }
        info!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            seeded_count = scratchpad.seen_ids.len(),
            "agent watch: upgrading pre-content-hash seen_ids to content-derived keys; no fire"
        );
        return false;
    }

    let seen: HashSet<String> = scratchpad.seen_ids.iter().map(|id| normalize_seen_key(id)).collect();
    let new_keyed_candidates: Vec<(&AgentWatchCandidate, String)> =
        keyed_candidates.iter().filter(|(_, key)| !seen.contains(key)).map(|(c, key)| (*c, key.clone())).collect();
    let new_candidates: Vec<&AgentWatchCandidate> = new_keyed_candidates.iter().map(|(c, _)| *c).collect();

    if seed_only {
        record_seen(&mut scratchpad, keyed_candidates.into_iter().map(|(_, key)| key));
        scratchpad.record_poll_outcome(false, &now);
        if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
            warn!(assignment_id = %assignment.id, error = %e, "agent watch: failed to seed scratchpad baseline");
        }
        info!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            seeded_count = candidates.len(),
            "agent watch: seeding scratchpad baseline (first poll, or authoring frozen at the ceiling); no fire"
        );
        return false;
    }

    if new_candidates.is_empty() {
        info!(
            assignment_id = %assignment.id,
            assignment_name = %assignment.name,
            observed_count = candidates.len(),
            seen_count = seen.len(),
            "agent watch: result — no change (all observed candidates already seen)"
        );
        scratchpad.record_poll_outcome(false, &now);
        if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
            warn!(assignment_id = %assignment.id, error = %e, "agent watch: failed to persist scratchpad after a quiet tick");
        }
        return false;
    }

    info!(
        assignment_id = %assignment.id,
        assignment_name = %assignment.name,
        new_item_count = new_candidates.len(),
        "agent watch: result — MATCHED new item(s); firing assignment"
    );

    let event_context = build_event_context(&new_candidates);
    let fired_summary = event_context.summary.clone();
    record_seen(&mut scratchpad, new_keyed_candidates.into_iter().map(|(_, key)| key));

    // Fire-then-persist: only commit the advanced scratchpad
    // once the fire itself has actually gone out — a fire failure leaves
    // the previous scratchpad in place so the same finding is retried next
    // poll instead of being silently marked seen and dropped.
    match fire_assignment(
        persistence,
        dispatcher,
        event_bus,
        assignment,
        AssignmentTriggerKind::AgentWatch,
        Some(fired_summary.clone()),
        timezone,
        Some(event_context),
    )
    .await
    {
        Ok(_) => {
            scratchpad.record_poll_outcome(true, &now);
            if let Err(e) = persistence.assignment_scratchpads.set(&assignment.id, &scratchpad).await {
                warn!(assignment_id = %assignment.id, error = %e, "agent watch: failed to persist scratchpad after firing");
            }
            info!(
                assignment_id = %assignment.id,
                assignment_name = %assignment.name,
                summary = %fired_summary,
                "agent watch: FIRED — dispatched to agent"
            );
            true
        }
        Err(e) => {
            // This path predates the two-phase ledger (it never touches
            // `seen_deliveries`) — leaving the scratchpad unpersisted already
            // means this finding is retried every poll rather than
            // permanently dropped. What it was missing is disclosure: a
            // failure here was previously visible only in the server log,
            // which violates this codebase's "if the engine detects it, the
            // user sees it" rule just as much as a silently swallowed item
            // would.
            warn!(
                assignment_id = %assignment.id,
                assignment_name = %assignment.name,
                error = %e,
                "agent watch: FAILED to fire assignment; scratchpad left unchanged so this finding is retried next poll"
            );
            emit_health_event(
                event_bus,
                assignment,
                format!(
                    "Agent watch \"{}\" matched {} new item(s) but failed to dispatch them to the agent: {e}. \
                     They will be retried on the next poll.",
                    assignment.name,
                    new_candidates.len(),
                ),
            )
            .await;
            false
        }
    }
}

/// Builds the single [`TriggerEventContext`] for a poll with one or more
/// candidates to fire on — a burst of new items (legacy diff) or transitions
/// (contract-bound diff) alike. Bundled into one fire rather than one fire
/// per candidate — tier 2's whole framing is "one instruction, one
/// full-price model call per poll", so several at once still
/// costs, and surfaces as, exactly one run rather than several.
fn build_event_context(new_candidates: &[&AgentWatchCandidate]) -> TriggerEventContext {
    let summary = match new_candidates {
        [only] => only.summary.clone(),
        many => format!(
            "Agent watch found {} new items: {}",
            many.len(),
            many.iter().map(|c| c.summary.as_str()).collect::<Vec<_>>().join("; ")
        ),
    };
    let payload = serde_json::json!({
        "items": new_candidates
            .iter()
            .map(|c| serde_json::json!({ "id": c.id, "summary": c.summary, "payload": c.payload }))
            .collect::<Vec<_>>(),
    });
    TriggerEventContext { summary, payload }
}

/// Appends `new_ids` to `scratchpad.seen_ids`, then drops the oldest entries
/// past [`SEEN_IDS_CAP`] — the same oldest-first eviction
/// `AssignmentScratchpad::seen_ids`'s own doc comment assigns to the
/// producer. Legacy-diff-only; contract-bound watches cap via
/// `AssignmentScratchpad::record_snapshot`'s own `SNAPSHOT_CAP` instead.
fn record_seen(scratchpad: &mut AssignmentScratchpad, new_ids: impl IntoIterator<Item = String>) {
    scratchpad.seen_ids.extend(new_ids);
    if scratchpad.seen_ids.len() > SEEN_IDS_CAP {
        let excess = scratchpad.seen_ids.len() - SEEN_IDS_CAP;
        scratchpad.seen_ids.drain(0..excess);
    }
}

/// Test-only scripted [`AgentWatchDetector`]: returns one queued response
/// per call, in order, panicking if polled more times than scripted. Shared
/// (not nested inside `mod tests`) so both this module's own tests and
/// `schedule_runner`'s integration tests can exercise the same fake without
/// duplicating it.
#[cfg(test)]
pub(crate) struct ScriptedDetector {
    responses: std::sync::Mutex<std::collections::VecDeque<Result<Vec<AgentWatchCandidate>, AgentWatchDetectError>>>,
}

#[cfg(test)]
impl ScriptedDetector {
    pub(crate) fn new(responses: Vec<Result<Vec<AgentWatchCandidate>, AgentWatchDetectError>>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into_iter().collect()),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl AgentWatchDetector for ScriptedDetector {
    async fn observe(
        &self,
        _assignment: &Assignment,
        _instruction: &str,
    ) -> Result<Vec<AgentWatchCandidate>, AgentWatchDetectError> {
        self.responses
            .lock()
            .expect("ScriptedDetector mutex poisoned")
            .pop_front()
            .expect("ScriptedDetector polled more times than scripted")
    }
}

/// Test-only scripted [`AgentWatchDetector`] with independent response
/// queues for [`AgentWatchDetector::observe`] (used by the stability
/// probe's second poll) and [`AgentWatchDetector::observe_for_authoring`]
/// (the authoring poll itself) — the authoring tests below need to script
/// both a proposal AND the probe's follow-up observation, which
/// [`ScriptedDetector`]'s single queue over the default trait method can't
/// express.
#[cfg(test)]
pub(crate) struct ScriptedAuthoringDetector {
    authoring_responses: std::sync::Mutex<std::collections::VecDeque<Result<AuthoringReply, AgentWatchDetectError>>>,
    observe_responses: std::sync::Mutex<std::collections::VecDeque<Result<Vec<AgentWatchCandidate>, AgentWatchDetectError>>>,
    /// One entry per [`AgentWatchDetector::observe_for_authoring`] call, in
    /// order — the `repair` argument it was actually invoked with, cloned.
    /// Lets the repair-loop tests assert not just that a second attempt ran,
    /// but that it carried the first attempt's rejected `expr`/error.
    observed_repairs: std::sync::Mutex<Vec<Option<RepairContext>>>,
}

#[cfg(test)]
impl ScriptedAuthoringDetector {
    pub(crate) fn new(
        authoring_responses: Vec<Result<AuthoringReply, AgentWatchDetectError>>,
        observe_responses: Vec<Result<Vec<AgentWatchCandidate>, AgentWatchDetectError>>,
    ) -> Self {
        Self {
            authoring_responses: std::sync::Mutex::new(authoring_responses.into_iter().collect()),
            observe_responses: std::sync::Mutex::new(observe_responses.into_iter().collect()),
            observed_repairs: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn observed_repairs(&self) -> Vec<Option<RepairContext>> {
        self.observed_repairs.lock().expect("ScriptedAuthoringDetector mutex poisoned").clone()
    }
}

#[cfg(test)]
#[async_trait]
impl AgentWatchDetector for ScriptedAuthoringDetector {
    async fn observe(
        &self,
        _assignment: &Assignment,
        _instruction: &str,
    ) -> Result<Vec<AgentWatchCandidate>, AgentWatchDetectError> {
        self.observe_responses
            .lock()
            .expect("ScriptedAuthoringDetector mutex poisoned")
            .pop_front()
            .expect("ScriptedAuthoringDetector.observe polled more times than scripted")
    }

    async fn observe_for_authoring(
        &self,
        _assignment: &Assignment,
        _instruction: &str,
        repair: Option<&RepairContext>,
    ) -> Result<AuthoringReply, AgentWatchDetectError> {
        self.observed_repairs.lock().expect("ScriptedAuthoringDetector mutex poisoned").push(repair.cloned());
        self.authoring_responses
            .lock()
            .expect("ScriptedAuthoringDetector mutex poisoned")
            .pop_front()
            .expect("ScriptedAuthoringDetector.observe_for_authoring polled more times than scripted")
    }
}

#[cfg(test)]
mod tests;
