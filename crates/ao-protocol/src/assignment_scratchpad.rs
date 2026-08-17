//! Durable per-assignment dedup cursor for the agent-driven watch detection
//! tier. See [`crate::watch_contract`] for the `WatchContract` type the
//! snapshot fields below serve.
//!
//! An agent-driven watch has no deterministic event source to key a cursor
//! off of — it re-polls its own agent on an instruction and must decide for
//! itself what's new. That decision has to be code-owned and deterministic,
//! not left to the model: thread memory is model-mediated (a forgotten or
//! malformed write silently loses the cursor), FIFO-cap-evicted (the exact
//! ids still needed to suppress a repeat can fall off the cap), and would
//! otherwise inflate every future poll's prompt for no benefit. This type is
//! the blob `ao_persistence::assignment_scratchpad_store::AssignmentScratchpadStore`
//! persists instead, read and written by code only, keyed by assignment id.
//!
//! `last_seen_id`/`seen_ids` are the pre-contract cursor shape and stay
//! present and populated for any assignment upgraded from that format — a
//! live watch must not reset or re-fire just because the schema grew.
//! `snapshots`/`contract_fingerprint` are the `WatchContract`-bound
//! equivalent: full per-item state (not just an id) keyed by `identity_key`,
//! plus the fingerprint of the contract that state was computed against, so
//! a tick can detect a contract amendment and re-key before diffing.
//! `seen_deliveries` is additive to both — inbound push-delivery dedup and,
//! via [`delivery_key`]/[`AssignmentScratchpad::record_action`], the
//! generic outbound action-dedup ledger.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::extractor_contract::{ExtractionPlan, Tier};

/// How long a push delivery-id is remembered for dedup before it's evicted.
/// Matches the window inbound platforms (e.g. GitHub) keep retrying a
/// delivery for — once a retry stops arriving, there is nothing left to
/// dedup against, so there's no reason to keep the id around. Does not apply
/// to action-ledger entries (see [`SeenDelivery::permanent`]).
pub const DELIVERY_ID_TTL: Duration = Duration::from_secs(3600);

/// Upper bound on how many [`ItemSnapshot`]s [`AssignmentScratchpad::record_snapshot`]
/// keeps per assignment, oldest-first evicted once a poll would push the set
/// past this — same "producer caps, oldest-first" shape `seen_ids` already
/// uses (see `ao_engine::agent_watch::SEEN_IDS_CAP`), just ten times larger
/// because a snapshot carries the previous predicate answer a future
/// transition must diff against, so evicting one is a real loss, not just a
/// memory saving. A watch over a source larger than this cap will still
/// silently look "new" once its oldest snapshot falls off the back — that is
/// why `record_snapshot` returns [`SnapshotTruncation`] instead of evicting
/// quietly.
pub const SNAPSHOT_CAP: usize = 2000;

/// Upper bound on how many distinct UTC calendar days
/// [`AssignmentScratchpad::model_calls_by_day`] keeps before the oldest is
/// evicted. Answering "calls per assignment per day" only ever needs a
/// recent window, not forever-retention, and an unbounded map keyed by
/// every day a long-lived watch has ever polled would otherwise grow
/// without limit.
pub const MODEL_CALL_DAY_BUCKET_CAP: usize = 30;

/// One assignment's durable dedup state, keyed externally by assignment id —
/// see `AssignmentScratchpadStore`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AssignmentScratchpad {
    /// The most recently observed item's identifier, for watches where "new"
    /// means "came after this one" (e.g. a monotonically increasing id).
    #[serde(default)]
    pub last_seen_id: Option<String>,

    /// Oldest-first record of recently observed item identifiers, for
    /// watches where "new" means "not in this set" rather than "after a
    /// cursor" (ids that don't arrive in a stable order). Bounding this to a
    /// sane cap is the producer's responsibility, the same division of
    /// labor `ao_engine`'s `SeenMessageIds` uses for
    /// `ChannelCursor::Discord::seen_message_ids` — this type stores
    /// whatever snapshot it's given rather than enforcing a cap itself.
    ///
    /// Superseded by `snapshots` for watches bound by a `WatchContract`, but
    /// kept present and populated exactly as before for any assignment
    /// upgraded from the pre-contract format — a live watch must not reset
    /// or re-fire just because the on-disk schema grew.
    #[serde(default)]
    pub seen_ids: Vec<String>,

    /// Push delivery-ids seen recently (e.g. `X-GitHub-Delivery`, `svix-id`,
    /// `X-Request-ID`), each tagged with the time it was recorded. Unlike
    /// `last_seen_id`/`seen_ids` (poll cursors, kept indefinitely) these
    /// expire after [`DELIVERY_ID_TTL`] — a retry stops arriving well before
    /// then, so there is no reason to grow this list forever. Additive to
    /// the poll-cursor fields above; a legacy row with neither key present
    /// still deserializes with this empty.
    ///
    /// Also doubles as the action ledger for `WatchContract`-bound watches:
    /// entries recorded via [`AssignmentScratchpad::record_action`]
    /// live in this same list, keyed by [`delivery_key`] rather than a
    /// push-transport header, and marked [`SeenDelivery::permanent`] so they
    /// are never TTL-evicted.
    #[serde(default)]
    pub seen_deliveries: Vec<SeenDelivery>,

    /// Per-item state as of the most recent poll, keyed by `identity_key`.
    /// This is what makes transition detection, re-keying
    /// on contract amendment, and post-incident debugging possible — see
    /// [`ItemSnapshot`].
    #[serde(default)]
    pub snapshots: Vec<ItemSnapshot>,

    /// [`WatchContract::fingerprint`](crate::watch_contract::WatchContract::fingerprint)
    /// of the contract `snapshots` was last computed against. A tick that
    /// finds this doesn't match the live contract's fingerprint knows the
    /// contract was amended and a re-key is owed before
    /// any diff runs. `None` for a watch that has never bound a contract.
    #[serde(default)]
    pub contract_fingerprint: Option<String>,

    /// [`IDENTITY_KEYGEN_VERSION`](crate::watch_contract::IDENTITY_KEYGEN_VERSION)
    /// that `snapshots`' `identity_key`s were last computed under. A tick
    /// that finds this doesn't match the engine's current keygen version
    /// knows every existing key was hashed by different rules and must not
    /// be diffed against fresh keys — that mismatch alone must force a
    /// re-seed, the same as a `contract_fingerprint` mismatch. `None` for a
    /// watch that predates this field.
    #[serde(default)]
    pub identity_keygen_version: Option<u32>,

    /// Count of consecutive contract-bound polls in which at least one
    /// observed candidate was missing a `required: true` field from
    /// `contract.fields`. This is the only amendment trigger: a whim-free,
    /// repeated-failure signal, not a re-litigation of the contract on every
    /// poll. Reset to `0` the moment
    /// a poll's candidates all extract cleanly, and again once a new
    /// contract is authored and bound. A later piece of this feature reads
    /// this to decide when to re-run authoring.
    #[serde(default)]
    pub missing_required_field_streak: u32,

    /// Edge-triggered latch for the `SNAPSHOT_CAP` overflow health event:
    /// set once a tick has warned that this
    /// watch's snapshot store is over cap, so a source that stays over cap
    /// across many ticks warns exactly once for the life of that condition
    /// instead of re-firing on every poll. Cleared only once `snapshots.len()`
    /// is genuinely back under `SNAPSHOT_CAP` — not merely once a tick drops
    /// nothing, since `record_snapshot` never shrinks the set below cap on
    /// its own — so the condition clearing and later recurring is treated as
    /// a new episode and warns again.
    #[serde(default)]
    pub truncation_notified: bool,

    /// Count of consecutive authoring-mode polls that ended without a bound
    /// `WatchContract` — a rejected or absent proposal, poll after poll,
    /// with a static instruction that cannot fix itself on its own. Reset to
    /// `0` the moment a poll binds a contract; incremented on every poll
    /// that doesn't. `ao_engine::agent_watch::run_authoring_and_legacy_tick`
    /// reads this to decide when to stop re-prompting for a proposal and
    /// surface the watch as unhealthy instead of retrying forever.
    #[serde(default)]
    pub authoring_failure_streak: u32,

    /// Display text of the most recent authoring rejection (either
    /// [`ContractError`](crate::watch_contract::ContractError) validation
    /// failure or the engine's own rung-drop construction failure), kept
    /// across polls so the next authoring attempt can be told what already
    /// didn't work instead of repeating it blind. `ao_engine::agent_watch`
    /// writes this whenever a poll ends `NotBound` with a rejection reason,
    /// leaving it untouched when a poll offers no proposal at all (nothing
    /// new to report, so the last real reason still stands). Reset to `None`
    /// in exactly the same two places `authoring_failure_streak` resets: a
    /// successful bind (the reason no longer applies to anything live) and
    /// an instruction/connector-scope edit (a fresh input deserves a fresh
    /// attempt, not a stale complaint about the old one).
    #[serde(default)]
    pub last_authoring_rejection_reason: Option<String>,

    /// Every DISTINCT rejection reason (`Display` text of a
    /// [`ContractError`](crate::watch_contract::ContractError) validation
    /// failure or a proposal-shape error) seen so far during the CURRENT
    /// authoring streak — same-tick repair attempts and earlier polls alike
    /// — in the order first encountered. Unlike `last_authoring_rejection_reason`
    /// (always just the newest one, for display), this is what lets
    /// `ao_engine::agent_watch::run_authoring_attempts` hand the model every
    /// outstanding constraint at once instead of only the latest complaint:
    /// two constraints shown one at a time, each fix regenerated from
    /// scratch, is a guaranteed oscillation — satisfy the newest and forget
    /// the one before it, forever. Reset in exactly the same two places as
    /// `authoring_failure_streak`: a successful bind, and an
    /// instruction/connector-scope edit.
    #[serde(default)]
    pub authoring_rejection_history: Vec<String>,

    /// `Some(n)` when the poll that most recently bound this watch's live
    /// `WatchContract` only succeeded after `n` rejected proposals — the
    /// pre-bind value of `authoring_failure_streak` (consecutive failed
    /// polls) plus however many same-tick repair attempts
    /// (`ao_engine::agent_watch::RepairContext`) that same poll burned
    /// through before binding. `None` when the live contract bound cleanly
    /// on its very first attempt, or when no contract is bound at all.
    ///
    /// This exists so a UI that showed a loud rejection can later show an
    /// equally explicit "bound after repairing N proposal(s)" — silence
    /// alone, once the contract binds, reads as "nothing happened here,"
    /// which is indistinguishable from a rejection nobody ever revisited.
    /// Cleared alongside every other piece of contract-derived state by
    /// [`Self::invalidate_watch_contract_state`] (an edited instruction or
    /// connector scope deserves a fresh convergence story, not a stale one
    /// from a now-discarded contract) and overwritten (to `None`, if this
    /// poll bound cleanly) on every later successful bind.
    #[serde(default)]
    pub contract_bound_after_failed_attempts: Option<u32>,

    /// `true` once an authoring pass has bound a `native_id` contract whose
    /// stability probe (`ao_engine::agent_watch::probe_identity_stability`)
    /// came back inconclusive — neither confirmed stable nor caught changing
    /// between the probe's two polls, most often because the two polls
    /// shared no candidate that reported a value for the proposed key at
    /// all. The identity is bound anyway (dropping a rung is reserved for a
    /// positive "this value changed" finding, never for "couldn't tell") —
    /// this flag is what keeps that unverified state visible instead of
    /// silently indistinguishable from a probe that actually confirmed
    /// stability. Cleared the moment a later authoring pass binds a contract
    /// whose probe came back stable (or whose identity strategy never runs
    /// the probe at all).
    #[serde(default)]
    pub identity_probe_inconclusive: bool,

    /// Human-readable reason for `identity_probe_inconclusive` — `Some`
    /// exactly when that flag is `true`. `None` otherwise.
    #[serde(default)]
    pub identity_probe_inconclusive_reason: Option<String>,

    /// `instruction`/`connector_scope` this assignment's authoring pass was
    /// last measured against, as computed by
    /// `ao_engine::agent_watch::authoring_input_key`. `None` until the first
    /// authoring-mode poll runs. A watch that has climbed
    /// `authoring_failure_streak` to `AUTHORING_FAILURE_CEILING` stops
    /// re-prompting the model — but a *never-bound* watch has no
    /// `contract_fingerprint` to key an "external edit cleared this" reset
    /// off of (that field only ever gets set once a contract binds), so this
    /// is the equivalent signal for the pre-bind case: a live key that no
    /// longer matches this one means the instruction or connector scope was
    /// edited since the streak was last measured, and the ceiling should not
    /// apply to the new input. Reset alongside `authoring_failure_streak`
    /// whenever that edit is detected.
    #[serde(default)]
    pub authoring_input_fingerprint: Option<String>,

    /// Count of times `missing_required_field_streak` has hit the
    /// amendment trigger and the contract was auto-cleared for
    /// re-authoring. Unlike `missing_required_field_streak` (reset on every
    /// clean poll) this is deliberately monotonic across distinct
    /// contracts — it is what lets `ao_engine::agent_watch::run_contract_bound_tick`
    /// tell "this watch legitimately needed one re-author as its source
    /// evolved" apart from "this watch is oscillating between amend and
    /// re-seed forever." Reset to `0` only when a contract is cleared or
    /// replaced through a door that has no scratchpad access of its own
    /// (see the orphaned-fingerprint reset in `run_agent_watch_tick`) — a
    /// deliberate instruction/connector_scope edit is a fresh start and
    /// should not inherit the old contract's amendment history. Never reset
    /// by the amendment clear itself, since that is exactly the loop this
    /// counter bounds.
    #[serde(default)]
    pub contract_amendment_cycle_count: u32,

    /// Which mechanism produced this watch's most recent poll's candidates —
    /// purely informational (a later UI task surfaces it), never read by any
    /// dedup/fire decision in this module. See
    /// [`ExtractionPath`]'s own variants for what each value means
    /// operationally.
    #[serde(default)]
    pub last_extraction_path: ExtractionPath,

    /// The `extractor_contract::Tier` inferred while deciding
    /// `last_extraction_path`. `None` whenever no `ExtractionPlan` was
    /// configured for this poll (i.e. whenever `last_extraction_path` is
    /// `Unbound`, or `Llm` with no plan at all) — `Some` whenever a plan was
    /// present and its tier was computed, even if that tier itself routed
    /// the poll through the model (`Tier::ChangeDetectionOnly`).
    #[serde(default)]
    pub last_inferred_tier: Option<Tier>,

    /// The `extractor_contract::ExtractionPlan` authored once for this watch
    /// from a sample of its own tool output — see
    /// `ao_engine::agent_watch::author_extraction_plan`. Lives here, not on
    /// `AssignmentTrigger::AgentWatch` alongside `contract`, because it is
    /// derived/re-computable state exactly like `snapshots`/
    /// `contract_fingerprint`, not part of the watch's own declaration: a
    /// contract amendment should invalidate it for free (see
    /// `extraction_plan_fingerprint`) the same way it already invalidates
    /// `snapshots`, rather than needing its own bespoke reset wired through
    /// every door a contract can change through. `None` until authored, or
    /// once a structural `extractor_contract::resolve` failure invalidates it
    /// for re-authoring (see `extraction_plan_degraded`).
    #[serde(default)]
    pub extraction_plan: Option<ExtractionPlan>,

    /// [`WatchContract::fingerprint`](crate::watch_contract::WatchContract::fingerprint)
    /// of the contract `extraction_plan` was authored against — the same
    /// mismatch-detects-staleness shape `contract_fingerprint` already uses
    /// for `snapshots`, applied to the extraction plan instead. A tick that
    /// finds this doesn't match the live contract's fingerprint treats
    /// `extraction_plan` as absent and re-attempts authoring. `None`
    /// whenever `extraction_plan` is `None`.
    #[serde(default)]
    pub extraction_plan_fingerprint: Option<String>,

    /// Edge-triggered latch (same shape as `truncation_notified`): set the
    /// moment a poll's `extractor_contract::resolve` call fails structurally
    /// (the plan's selector/identity path no longer matches the tool's
    /// payload shape — never set for a legitimate zero-item result, which is
    /// not an error). Cleared the moment a later poll resolves cleanly again.
    /// Read by nothing in this crate; a later UI task surfaces it alongside
    /// `last_extraction_path`/`last_inferred_tier`.
    #[serde(default)]
    pub extraction_plan_degraded: bool,

    /// The structured `extractor_contract::BindError`'s display text as of
    /// the poll that set `extraction_plan_degraded` — the available-paths
    /// list or the excerpt that failed to match, so the eventual UI surface
    /// can show *why* the watch degraded, not just *that* it did. `Some`
    /// only while `extraction_plan_degraded` is `true`.
    #[serde(default)]
    pub extraction_plan_degraded_reason: Option<String>,

    /// Item count observed in the sample `extraction_plan` was authored
    /// from — the structural baseline `ao_engine::agent_watch::resolve_with_plan`'s
    /// `Tier::Probabilistic` polls compare each later poll's resolved items
    /// against (paired with `extraction_plan_expected_fields`). This exists
    /// because a text-rescued plan has no server-declared schema behind it:
    /// `extractor_contract::resolve` only fails when the *selector* stops
    /// matching, never when an already-selected item quietly drops or
    /// renames a field the plan's `identity`/`predicate` reads — so a plan
    /// can keep "succeeding" long after its target's shape has drifted
    /// underneath it. `None` whenever `extraction_plan` is `None`, or for a
    /// plan authored before this field existed — there being nothing
    /// recorded means there is nothing to compare against, not that the
    /// baseline was zero.
    #[serde(default)]
    pub extraction_plan_expected_item_count: Option<usize>,

    /// Union of top-level field names observed across the sample
    /// `extraction_plan` was authored from — see
    /// `extraction_plan_expected_item_count`'s doc for the full rationale.
    /// `None` under the same conditions as that field, and always cleared
    /// alongside it (see `AssignmentScratchpad::clear_extraction_plan`).
    #[serde(default)]
    pub extraction_plan_expected_fields: Option<BTreeSet<String>>,

    /// Per-day count of real provider/model invocations this assignment's
    /// agent-watch detector has spent, keyed by UTC calendar date
    /// (`YYYY-MM-DD`, e.g. `2026-07-28`). The only usage/cost telemetry this
    /// system persists — deliberately just an invocation count, never
    /// tokens or dollars (see [`AssignmentScratchpad::record_model_call`]/
    /// [`AssignmentScratchpad::record_additional_model_calls`]). This counts
    /// every completed provider turn, not just every detector session
    /// spawned — a single session that takes a tool-use round trip before
    /// its final reply spends two real invocations, and this must show
    /// that, not one, or the figure understates true cost. Bounded to
    /// [`MODEL_CALL_DAY_BUCKET_CAP`] most recent days, oldest evicted first,
    /// so a long-lived watch's scratchpad cannot grow forever from this
    /// field alone.
    #[serde(default)]
    pub model_calls_by_day: BTreeMap<String, u32>,

    /// Count of consecutive completed polls that produced zero newly-fired
    /// items (see [`AssignmentScratchpad::record_poll_outcome`]). Purely
    /// informational — a legitimately quiet watch runs this up exactly as
    /// much as one whose source has silently gone stale, and nothing in
    /// this crate reads it back to make a dedup/fire/health decision. It
    /// exists so a human can look at "247 polls, 0 items, last fired 9 days
    /// ago" and judge for themselves, not so an automated check can flag it.
    #[serde(default)]
    pub consecutive_polls_without_new_items: u32,

    /// RFC3339 timestamp of the last poll that fired at least one item.
    /// Distinct from [`ItemSnapshot::last_seen_at`], which is per-item and
    /// advances on every poll that merely observes an item, fired or not —
    /// this field only moves on an actual fire. `None` until this
    /// assignment's first-ever fire.
    #[serde(default)]
    pub last_new_item_at: Option<String>,

    /// Count of candidates `ao_engine::agent_watch::run_contract_bound_tick`
    /// observed on the most recent poll — every `AgentWatchCandidate` this
    /// watch's detector or extraction plan returned that poll, before any
    /// quarantine check ran. `0` for a poll that observed nothing, same as a
    /// watch that has never polled.
    ///
    /// Exists alongside [`Self::last_poll_surviving_candidates`] because
    /// [`Self::consecutive_polls_without_new_items`] cannot tell "this poll
    /// was quiet" apart from "this poll observed candidates but rejected
    /// every one of them" — both climb that counter identically. This pair
    /// is what makes the two conditions visible as distinct (see
    /// [`Self::all_candidates_quarantined_streak`]).
    #[serde(default)]
    pub last_poll_observed_candidates: u32,

    /// Count of `last_poll_observed_candidates` that were NOT quarantined —
    /// i.e. survived `missing_required_fields`/`identity_key`/`version_key`
    /// evaluation and were recorded as an [`ItemSnapshot`]. A positive
    /// `last_poll_observed_candidates` paired with `0` here is the "bound
    /// and matching nothing" condition: the watch is bound and receiving
    /// candidates, but none of them pass its contract — a materially
    /// different, and more urgent, condition than a poll that observed
    /// nothing at all.
    #[serde(default)]
    pub last_poll_surviving_candidates: u32,

    /// Count of consecutive polls where `last_poll_observed_candidates > 0`
    /// and `last_poll_surviving_candidates == 0` — every observed candidate
    /// that poll was quarantined. Reset to `0` the moment a poll has at
    /// least one surviving candidate, and on the same two contract-cleared/
    /// re-seeded doors [`Self::missing_required_field_streak`] resets
    /// through (`ao_engine::agent_watch::run_agent_watch_tick`'s
    /// orphaned-fingerprint reset, and `run_contract_bound_tick`'s own
    /// amendment-cycle clear). Left untouched (neither incremented nor
    /// reset) on a poll that observed zero candidates — mirroring
    /// `missing_required_field_streak`'s own doc: there is nothing to judge
    /// "matching nothing" from when nothing was even observed.
    ///
    /// Unlike `missing_required_field_streak` this drives a health event on
    /// every poll it is nonzero on, not just once it crosses a threshold —
    /// "bound and matching nothing" is reportable from the first poll it
    /// happens on, not after several.
    #[serde(default)]
    pub all_candidates_quarantined_streak: u32,
}

/// Which mechanism produced a contract-bound watch's most recent poll's
/// candidates. Purely informational (a later UI task displays it); nothing
/// in this crate or `ao_engine::agent_watch` reads it back to make a
/// dedup/fire decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionPath {
    /// No `WatchContract` is bound yet — `ao_engine::agent_watch::run_authoring_and_legacy_tick`
    /// ran instead of `run_contract_bound_tick`, so there is no
    /// contract-bound dedup context for an extraction plan to feed yet.
    #[default]
    Unbound,
    /// Candidates came from `AgentWatchDetector::observe` — a full-price
    /// model child session, either because no `ExtractionPlan` was
    /// configured or because the configured plan's inferred `Tier` was
    /// `ChangeDetectionOnly`.
    Llm,
    /// Candidates came from `extractor_contract::resolve` against content
    /// the server contractually promised the shape of — zero model calls.
    Deterministic,
    /// Candidates came from `extractor_contract::resolve` against content
    /// whose shape is not a guarantee (unstructured prose, or structured
    /// content with no declared schema) — still zero model calls.
    Probabilistic,
}

/// Whether an `AgentWatch`'s steady-state poll can skip the model entirely,
/// derived from the same scratchpad state [`ExtractionPath`] reports
/// (`ao_engine::agent_watch::derive_extraction_health`) — never itself
/// persisted, since it is fully recomputable from `extraction_plan`/
/// `extraction_plan_degraded` plus the trigger's frozen `extraction_tool`.
///
/// This exists because [`ExtractionPath`] alone cannot tell "this watch runs
/// the model every poll because no extraction plan could ever be authored
/// for it" apart from "this watch runs the model every poll because no
/// contract is bound yet" apart from "this watch has never polled" — all
/// three read as `Llm`/`Unbound`/absent to that enum, and the middle one is a
/// silent, un-fixable-by-the-user cost sink that the "if the engine detects
/// it, the user sees it" rule requires to be visibly distinct from healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionHealth {
    /// No poll has completed yet — nothing is known.
    Pending,
    /// A frozen extraction plan exists and direct-invoke is being used. No model call per poll.
    Deterministic,
    /// The tool + args are frozen, but no extraction plan was ever authored
    /// (source returns no structured content). A model extracts on every poll.
    ModelAssisted,
    /// A plan existed and direct-invoke failed; the watch fell back to model extraction.
    Degraded,
}

/// One `AgentWatch` assignment's contract-authoring lifecycle, as exactly
/// one of three MUTUALLY EXCLUSIVE states — the single source of truth
/// `ao_server::routes::assignments::watch_health_for` derives
/// `AssignmentWatchHealth::contract_status` from, and the only thing
/// `WatchContractPanel` (frontend) now branches its top-level rendering on.
///
/// This replaces what used to be two INDEPENDENTLY computed signals that a
/// UI had to reconcile itself: whether `trigger.contract` was `Some`, and
/// `derive_extraction_health`'s `ExtractionHealth::ModelAssisted`, which
/// never looked at contract-bound-ness at all — it reads "no plan persisted"
/// as its trigger, and that condition is equally true whether the actual
/// cause is "authoring hasn't bound a contract yet" or "a contract IS bound
/// but its extraction target can't be frozen." Those two causes need
/// completely different copy, and rendering them from two separately-checked
/// booleans let a single poll produce both "no contract yet" and
/// "model-assisted, no fixed tier" on screen at once — a poll that recorded
/// a scratchpad (so `derive_extraction_health` sees `Some`) but whose
/// authoring attempt was rejected (so `contract` stayed `None`) satisfies
/// both independently. A `match` on this enum can only ever take one arm, so
/// that contradiction is now structurally unrepresentable rather than merely
/// avoided by careful `if` ordering.
///
/// Lives here (rather than in `ao_engine::agent_watch`, where the deriving
/// logic itself still lives via `derive_watch_contract_status`) so that
/// `QuiescenceReason` (`crate::assignment::QuiescenceReason`) can wrap it
/// without `ao-protocol` depending on `ao-engine` — `ao-engine` already
/// depends on `ao-protocol`, so the reverse edge would be a cycle.
/// `ao_engine::agent_watch` re-exports this type from its original path so
/// no existing `use ao_engine::agent_watch::WatchContractStatus` import
/// breaks. Unlike the rest of this file's types, it derives `Deserialize`
/// as well as `Serialize` purely so it satisfies `QuiescenceReason`'s own
/// derive — this enum is not itself persisted anywhere directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WatchContractStatus {
    /// No poll has completed for the assignment's CURRENT instruction/
    /// connector_scope yet — either `scratchpad` is `None` outright (a
    /// brand-new watch), or one exists but
    /// `scratchpad.authoring_failure_streak` is `0` while no contract is
    /// bound, which only happens in the window right after
    /// `AssignmentScratchpad::invalidate_watch_contract_state` has reset a
    /// previous contract's derived state (an edited instruction or
    /// connector_scope) and the next poll hasn't run yet. Both read the same
    /// to a user: nothing is known about authoring for what's live right
    /// now.
    NotYetAttempted,
    /// At least one poll has run authoring for the current input and every
    /// attempt so far was rejected, or offered no proposal at all — no
    /// `WatchContract` is bound yet.
    AuthoringRejected {
        /// `AssignmentScratchpad::authoring_failure_streak` verbatim —
        /// consecutive polls that ended without a bound contract.
        attempts: u32,
        /// `true` once `attempts` has reached `AUTHORING_FAILURE_CEILING`:
        /// authoring has stopped re-prompting until the instruction or
        /// connector scope is edited.
        ceiling_hit: bool,
        /// `AssignmentScratchpad::last_authoring_rejection_reason` verbatim
        /// — the most recent concrete validation failure, when one was ever
        /// recorded (a poll that offered no proposal at all has nothing to
        /// report here, but doesn't blank out a real prior reason either).
        last_rejection_reason: Option<String>,
    },
    /// A `WatchContract` is bound — the product spec's "frozen" state.
    /// Extraction-tier detail (deterministic/probabilistic/model-assisted
    /// extraction/degraded) is unrelated to this enum and still lives on
    /// `AssignmentWatchHealth`'s own `extraction_health`/`tier` fields,
    /// exactly as before — this variant only ever answers "is there a live
    /// contract to show," not "how healthy is its extraction."
    Bound {
        /// `AssignmentScratchpad::contract_bound_after_failed_attempts`
        /// verbatim: `Some(n)` when the live contract only bound after `n`
        /// rejected proposals — the signal a panel uses to say so
        /// explicitly instead of leaving an earlier rejection looking
        /// unresolved. `None` when it bound cleanly on the first attempt.
        bound_after_repairs: Option<u32>,
    },
}

/// Two-phase confirmation status for an action-ledger [`SeenDelivery`] (see
/// [`AssignmentScratchpad::record_pending_action`]). Firing only ever proves
/// "the message reached the target agent's queue," not "the queued turn
/// ran" — so a ledger entry starts `Pending` and is promoted to `Confirmed`
/// only once its dispatched run is independently confirmed to have reached
/// a terminal state. Either status dedupes a poll the same way (see
/// [`AssignmentScratchpad::has_seen_delivery`]) — a `Pending` entry that has
/// not yet stayed pending long enough to look stuck is left alone, not
/// retried, so an ordinary in-flight turn is never duplicated. Only once an
/// entry has stayed `Pending` past a poll-count threshold (see
/// `ao_engine::agent_watch::PENDING_DELIVERY_RETRY_POLL_THRESHOLD`) does the
/// engine treat it as retry-eligible and surface it as unhealthy — see
/// `ao_engine::agent_watch::reconcile_pending_deliveries`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// Successfully enqueued, but the queued turn's outcome is not yet
    /// known — e.g. the process may have restarted before that turn ran, or
    /// it is still in flight.
    Pending,
    /// The dispatched turn reached a terminal state (succeeded or failed),
    /// or — via the `#[default]` below — this entry predates the two-phase
    /// ledger entirely. Defaulting a missing `status` key to `Confirmed` is
    /// load-bearing: every entry written before this field existed was
    /// recorded through the old unconditional [`AssignmentScratchpad::record_action`]
    /// path, which only ever ran after a fire genuinely went out, so it must
    /// load back as already-resolved rather than newly suspicious the
    /// moment this field ships.
    #[default]
    Confirmed,
}

/// One push-delivery or action-ledger dedup record. See
/// [`AssignmentScratchpad::seen_deliveries`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SeenDelivery {
    pub id: String,
    pub seen_at: SystemTime,
    /// True for action-ledger entries recorded via
    /// [`AssignmentScratchpad::record_action`] (keyed by [`delivery_key`]),
    /// which must never expire — an action taken once has to stay deduped
    /// for the life of the assignment, not just for the hour
    /// [`DELIVERY_ID_TTL`] gives an inbound webhook's retry window to close.
    /// `false` (the serde default, matching every row written before the
    /// action ledger existed) for ordinary push-delivery dedup via
    /// [`AssignmentScratchpad::record_delivery`], which keeps today's
    /// TTL-eviction behavior unchanged.
    #[serde(default)]
    pub permanent: bool,
    /// Two-phase confirmation status — see [`DeliveryStatus`].
    #[serde(default)]
    pub status: DeliveryStatus,
    /// The dispatched `AssignmentRun::id` a `Pending` entry's outcome can be
    /// looked up by. `Some` only while `status == Pending`; cleared back to
    /// `None` on promotion (nothing left to reconcile) and always `None` for
    /// entries that predate the two-phase ledger.
    #[serde(default)]
    pub run_id: Option<String>,
    /// The item's `identity_key` as of the poll that recorded this entry,
    /// kept only so a `Pending` entry that never resolves can be reported to
    /// the user by something more legible than its opaque [`delivery_key`]
    /// hash. `Some` only while `status == Pending`.
    #[serde(default)]
    pub identity_key: Option<String>,
    /// Set once a still-`Pending` entry has been reported as unhealthy, so a
    /// watch that stays stuck doesn't re-emit the same health event on every
    /// later poll — the same edge-triggered-latch shape
    /// [`AssignmentScratchpad::truncation_notified`] uses.
    #[serde(default)]
    pub stale_notified: bool,
    /// Number of consecutive reconciliation passes this entry has been
    /// observed still `Pending` without its dispatched run reaching a
    /// terminal status (an entry whose initial dispatch attempt failed
    /// outright starts accruing this the same way, since it has no `run_id`
    /// to ever resolve on its own). Reset to `0` by
    /// [`AssignmentScratchpad::record_pending_action`] and read by
    /// `ao_engine::agent_watch::reconcile_pending_deliveries` to decide when
    /// an entry has been stuck long enough to retry rather than keep waiting.
    /// `0` for any entry that predates this counter.
    #[serde(default)]
    pub pending_poll_count: u32,
}

/// One item's state as of the poll that produced it.
/// Persisting the full field map — not just the keys — is what makes change
/// detection, re-keying on contract amendment, and post-incident debugging
/// possible; hashes alone can't be diffed after the fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemSnapshot {
    /// "Who is this?" — see `identity_key` in `watch_contract`.
    pub identity_key: String,
    /// "Has it changed?" — see `version_key` in `watch_contract`.
    pub version_key: String,
    /// The previous poll's answer to `contract.predicate` for this
    /// item. Without this, a `false -> true` edge — the entire basis of
    /// `WatchMode::PredicateTransition` — can't be told apart from
    /// `true -> true`; both would otherwise look like "currently matching."
    pub predicate_value: bool,
    /// Incremented every time this item enters the matching state (a
    /// `false -> true` edge on `predicate_value`). Folded into
    /// [`delivery_key`] so a client that goes matching -> not-matching ->
    /// matching again with byte-identical data produces a *different*
    /// ledger key on the second entry — that is a real re-entry the user
    /// asked to hear about, not a duplicate to swallow.
    pub edge_counter: u32,
    /// RFC3339 timestamp of the poll that produced this snapshot.
    pub last_seen_at: String,
    /// The full extracted field map observed for this item on that poll.
    pub payload: Value,
}

/// Returned by [`AssignmentScratchpad::record_snapshot`] when adding a
/// snapshot pushed `snapshots` past [`SNAPSHOT_CAP`], forcing an
/// oldest-first eviction. Deliberately carries no assignment id — this type
/// stays pure and synchronous like the rest of this module; a caller with
/// assignment context attaches it before logging or surfacing it to the
/// user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotTruncation {
    pub dropped_count: usize,
    pub retained_count: usize,
}

/// Unit separator joining [`delivery_key`]'s components — vanishingly
/// unlikely to appear in a real id/hash, and its only purpose is stopping
/// two different field splits (e.g. an `identity_key` that happens to end
/// where a `version_key` begins) from concatenating to the same string, not
/// defending against adversarial input. Same technique as `watch_contract`'s
/// `COMPOSITE_KEY_JOIN`, reimplemented here since that constant is private
/// to that module.
const DELIVERY_KEY_JOIN: &str = "\u{1f}";

/// Computes the generic action-ledger dedup key `AssignmentScratchpad::record_action`
/// records into `seen_deliveries`: a sha256 hex digest of
/// `assignment_id`, `identity_key`, `version_key`, and `edge_counter`,
/// unit-separator-joined so no field-boundary ambiguity can collide two
/// different tuples into the same key.
///
/// This exists because most actions have no natural unique key of their
/// own — "draft a reply," "post to a channel," "create an issue" don't
/// produce a Message-ID the way sending an email does — so the ledger
/// derives one from the same identity/version/edge state the watch already
/// computes for every item on every poll, rather than depending on
/// action-specific fields (this must never be keyed off an email address or
/// any other action-specific value). `edge_counter` is load-bearing: see its
/// doc on [`ItemSnapshot::edge_counter`].
pub fn delivery_key(assignment_id: &str, identity_key: &str, version_key: &str, edge_counter: u32) -> String {
    let joined =
        [assignment_id, identity_key, version_key, &edge_counter.to_string()].join(DELIVERY_KEY_JOIN);
    let mut hasher = Sha256::new();
    hasher.update(joined.as_bytes());
    format!("{:x}", hasher.finalize())
}

impl AssignmentScratchpad {
    /// True if `delivery_id` was recorded within the last [`DELIVERY_ID_TTL`]
    /// relative to `now` — or was recorded [`SeenDelivery::permanent`],
    /// which never expires. Pure and synchronous — callers own persistence.
    pub fn has_seen_delivery(&self, delivery_id: &str, now: SystemTime) -> bool {
        self.seen_deliveries
            .iter()
            .any(|d| d.id == delivery_id && (d.permanent || !is_expired(d.seen_at, now)))
    }

    /// Records `delivery_id` as seen at `now`, evicting any non-permanent
    /// entries older than [`DELIVERY_ID_TTL`] first so this list never grows
    /// unbounded. Pure and synchronous — callers own persistence.
    pub fn record_delivery(&mut self, delivery_id: &str, now: SystemTime) {
        self.seen_deliveries.retain(|d| d.permanent || !is_expired(d.seen_at, now));
        self.seen_deliveries.push(SeenDelivery {
            id: delivery_id.to_string(),
            seen_at: now,
            permanent: false,
            status: DeliveryStatus::Confirmed,
            run_id: None,
            identity_key: None,
            stale_notified: false,
            pending_poll_count: 0,
        });
    }

    /// Records `action_key` (typically [`delivery_key`]'s output) in the
    /// same ledger [`Self::record_delivery`] uses, but marked
    /// [`SeenDelivery::permanent`] so [`DELIVERY_ID_TTL`] never evicts it —
    /// an action taken once must stay deduped for the life of the
    /// assignment, unlike a push-delivery id whose retry window closes
    /// within the hour. Records `status: Confirmed` directly — callers use
    /// this only once the action is already known to have completed;
    /// [`Self::record_pending_action`] is for the two-phase case where only
    /// successful enqueue is known so far. Pure and synchronous — callers
    /// own persistence.
    pub fn record_action(&mut self, action_key: &str, now: SystemTime) {
        self.seen_deliveries.retain(|d| d.permanent || !is_expired(d.seen_at, now));
        self.seen_deliveries.push(SeenDelivery {
            id: action_key.to_string(),
            seen_at: now,
            permanent: true,
            status: DeliveryStatus::Confirmed,
            run_id: None,
            identity_key: None,
            stale_notified: false,
            pending_poll_count: 0,
        });
    }

    /// Two-phase counterpart of [`Self::record_action`]: records
    /// `action_key` as [`DeliveryStatus::Pending`] *before* dispatch is even
    /// attempted — this is the match-time write that closes the crash window
    /// between deciding an item is new and actually firing it. `run_id` is
    /// unknown yet (attach it once dispatch is attempted via
    /// [`Self::attach_dispatch_run`]); `identity_key` is kept purely so a
    /// health event can name the item legibly if it never resolves.
    /// Overwrites any prior entry for the same `action_key`, resetting
    /// [`SeenDelivery::pending_poll_count`] to `0` — the caller re-recording
    /// under this key is exactly what happens when
    /// `ao_engine::agent_watch::reconcile_pending_deliveries` retries a
    /// stuck entry, and that retry deserves a fresh count, not one that
    /// starts already past the threshold. Pure and synchronous — callers own
    /// persistence and the reconciliation pass that promotes this entry via
    /// [`Self::confirm_pending_delivery`].
    pub fn record_pending_action(&mut self, action_key: &str, identity_key: &str, now: SystemTime) {
        self.seen_deliveries.retain(|d| d.id != action_key && (d.permanent || !is_expired(d.seen_at, now)));
        self.seen_deliveries.push(SeenDelivery {
            id: action_key.to_string(),
            seen_at: now,
            permanent: true,
            status: DeliveryStatus::Pending,
            run_id: None,
            identity_key: Some(identity_key.to_string()),
            stale_notified: false,
            pending_poll_count: 0,
        });
    }

    /// Attaches the dispatched `AssignmentRun::id` to the `Pending` entry
    /// keyed by `action_key`, once `fire_assignment` has actually been
    /// attempted for it. Separate from [`Self::record_pending_action`]
    /// because the match-time write happens *before* dispatch, when no run
    /// id exists yet — a dispatch call that fails outright leaves the entry
    /// `Pending` with no `run_id` at all, which
    /// `ao_engine::agent_watch::reconcile_pending_deliveries` also treats as
    /// stuck. Returns `false` (a no-op) if no `Pending` entry with that key
    /// exists — a caller racing a promotion, or attaching to an already-
    /// resolved entry, is not an error. Pure and synchronous — callers own
    /// persistence.
    pub fn attach_dispatch_run(&mut self, action_key: &str, run_id: String) -> bool {
        let Some(entry) = self.seen_deliveries.iter_mut().find(|d| d.id == action_key) else {
            return false;
        };
        if entry.status != DeliveryStatus::Pending {
            return false;
        }
        entry.run_id = Some(run_id);
        true
    }

    /// Promotes the `Pending` entry keyed by `action_key` to `Confirmed`,
    /// clearing `run_id`/`identity_key` since there is nothing left to
    /// reconcile or report. Returns `false` (a no-op) if no entry with that
    /// key exists or it was not `Pending` — a caller reconciling the same
    /// entry twice, or racing another promotion, is not an error. Pure and
    /// synchronous — callers own persistence.
    pub fn confirm_pending_delivery(&mut self, action_key: &str) -> bool {
        let Some(entry) = self.seen_deliveries.iter_mut().find(|d| d.id == action_key) else {
            return false;
        };
        if entry.status != DeliveryStatus::Pending {
            return false;
        }
        entry.status = DeliveryStatus::Confirmed;
        entry.run_id = None;
        entry.identity_key = None;
        true
    }

    /// Removes the `Pending` entry keyed by `action_key` outright, making the
    /// item it names retry-eligible: with no ledger entry left,
    /// [`Self::has_seen_delivery`] no longer suppresses it, so the next poll
    /// that observes the same item as a fresh transition will fire on it
    /// again. Called only by `ao_engine::agent_watch::reconcile_pending_deliveries`
    /// once an entry has been stuck past the retry-poll threshold — the
    /// snapshot that made this item "already seen" is cleared alongside this
    /// by that same caller. Pure and synchronous — callers own persistence.
    pub fn clear_pending_delivery(&mut self, action_key: &str) {
        self.seen_deliveries.retain(|d| d.id != action_key);
    }

    /// Upserts `snapshot` by `identity_key` (replacing any prior snapshot
    /// for the same item so a watch's own re-observation never duplicates
    /// it), then evicts oldest-first once `snapshots` exceeds
    /// [`SNAPSHOT_CAP`]. Pure and synchronous — callers own persistence and,
    /// if this returns `Some`, own attaching assignment context and
    /// reporting it to the user.
    pub fn record_snapshot(&mut self, snapshot: ItemSnapshot) -> Option<SnapshotTruncation> {
        self.snapshots.retain(|s| s.identity_key != snapshot.identity_key);
        self.snapshots.push(snapshot);
        if self.snapshots.len() > SNAPSHOT_CAP {
            let dropped_count = self.snapshots.len() - SNAPSHOT_CAP;
            self.snapshots.drain(0..dropped_count);
            Some(SnapshotTruncation { dropped_count, retained_count: self.snapshots.len() })
        } else {
            None
        }
    }

    /// Clears `extraction_plan` and every piece of state derived from it —
    /// `extraction_plan_fingerprint` and the structural-expectation baseline
    /// (`extraction_plan_expected_item_count`/`extraction_plan_expected_fields`)
    /// — so a plan invalidated for any reason never leaves a stale baseline
    /// behind for whatever plan is authored next. Deliberately leaves
    /// `extraction_plan_degraded`/`extraction_plan_degraded_reason`
    /// untouched: callers disagree on whether clearing here means "a fresh
    /// start" (a contract amendment resets degraded too) or "this plan just
    /// broke" (a structural/structural-expectation failure sets degraded
    /// right after calling this) — that decision stays with the caller.
    pub fn clear_extraction_plan(&mut self) {
        self.extraction_plan = None;
        self.extraction_plan_fingerprint = None;
        self.extraction_plan_expected_item_count = None;
        self.extraction_plan_expected_fields = None;
    }

    /// Resets every piece of state a `WatchContract` binding or an authoring
    /// pass derives, back to the same defaults a brand-new scratchpad starts
    /// with — called by `ao_server::routes::assignments::patch_assignment`
    /// whenever `ao_protocol::assignment::carry_forward_watch_contract`
    /// decides an edit invalidates the previous contract.
    ///
    /// Without this, a scratchpad written by the OLD (now-discarded)
    /// contract's authoring/extraction/snapshot machinery keeps answering
    /// `AssignmentWatchHealth` queries for the NEW, contract-less trigger
    /// until the next poll happens to overwrite it — producing a panel that
    /// shows a stale tier, a stale rejection reason, or a stale convergence
    /// note alongside "no contract bound yet." Making this state follow the
    /// contract's own lifecycle instead is what
    /// `ao_engine::agent_watch::derive_watch_contract_status` relies on to
    /// report `NotYetAttempted` immediately after such an edit, rather than
    /// replaying the previous contract's fate.
    ///
    /// Deliberately leaves untouched every field that describes the
    /// assignment's identity-independent history rather than any one
    /// contract's fate: `last_seen_id`/`seen_ids` (the pre-contract dedup
    /// cursor), `seen_deliveries` (the action ledger — an action already
    /// taken must stay deduped even if the contract that triggered it is
    /// gone), and `model_calls_by_day`/`consecutive_polls_without_new_items`/
    /// `last_new_item_at` (usage/quiet-watch telemetry an editor's whim
    /// should not reset for free).
    pub fn invalidate_watch_contract_state(&mut self) {
        self.snapshots.clear();
        self.contract_fingerprint = None;
        self.identity_keygen_version = None;
        self.missing_required_field_streak = 0;
        self.truncation_notified = false;
        self.authoring_failure_streak = 0;
        self.last_authoring_rejection_reason = None;
        self.authoring_rejection_history.clear();
        self.contract_bound_after_failed_attempts = None;
        self.identity_probe_inconclusive = false;
        self.identity_probe_inconclusive_reason = None;
        self.authoring_input_fingerprint = None;
        self.contract_amendment_cycle_count = 0;
        self.last_extraction_path = ExtractionPath::Unbound;
        self.last_inferred_tier = None;
        self.clear_extraction_plan();
        self.extraction_plan_degraded = false;
        self.extraction_plan_degraded_reason = None;
        self.last_poll_observed_candidates = 0;
        self.last_poll_surviving_candidates = 0;
        self.all_candidates_quarantined_streak = 0;
    }

    /// Records one LLM child session spawned for `date` (a UTC calendar
    /// date, `YYYY-MM-DD` — callers pass `Utc::now().date_naive()`),
    /// creating or incrementing that day's bucket in
    /// [`Self::model_calls_by_day`]. Once recording pushes the map past
    /// [`MODEL_CALL_DAY_BUCKET_CAP`] distinct days, the oldest is evicted —
    /// lexicographically smallest is also chronologically oldest for ISO
    /// dates, so a plain key comparison is enough, no parsing required.
    /// Pure and synchronous — callers own persistence.
    ///
    /// Recorded BEFORE the session it counts is even attempted (see every
    /// call site in `ao_engine::agent_watch`) so a crash mid-session still
    /// leaves this one spawn on the books — a caller that later learns the
    /// session actually spent more than one real provider turn tops the
    /// count up with [`Self::record_additional_model_calls`] rather than
    /// calling this again, which would double-count the first turn.
    pub fn record_model_call(&mut self, date: &str) {
        self.record_additional_model_calls(date, 1);
    }

    /// Tops up `date`'s bucket by `extra` on top of whatever
    /// [`Self::record_model_call`] already recorded for it — for a caller
    /// that only learns AFTER a session completes that it spent more than
    /// one real provider turn (e.g. a tool-use round trip before the
    /// child's final reply): `record_model_call`'s pre-call floor already
    /// booked one of those turns, so this adds just the rest. A no-op when
    /// `extra` is `0`, so callers can call this unconditionally without a
    /// branch.
    pub fn record_additional_model_calls(&mut self, date: &str, extra: u32) {
        if extra == 0 {
            return;
        }
        let count = self.model_calls_by_day.entry(date.to_string()).or_insert(0);
        *count = count.saturating_add(extra);
        while self.model_calls_by_day.len() > MODEL_CALL_DAY_BUCKET_CAP {
            let Some(oldest) = self.model_calls_by_day.keys().next().cloned() else { break };
            self.model_calls_by_day.remove(&oldest);
        }
    }

    /// Updates the quiet-watch drift counters (see their own doc comments)
    /// for one completed poll whose fire outcome is now known: `true` resets
    /// [`Self::consecutive_polls_without_new_items`] to `0` and stamps
    /// [`Self::last_new_item_at`] at `now` (an RFC3339 timestamp); `false`
    /// increments the streak (saturating) and leaves `last_new_item_at`
    /// untouched. Pure and synchronous — callers own persistence.
    pub fn record_poll_outcome(&mut self, fired_new_item: bool, now: &str) {
        if fired_new_item {
            self.consecutive_polls_without_new_items = 0;
            self.last_new_item_at = Some(now.to_string());
        } else {
            self.consecutive_polls_without_new_items = self.consecutive_polls_without_new_items.saturating_add(1);
        }
    }
}

/// A timestamp is expired once `now` is at least [`DELIVERY_ID_TTL`] past it.
/// A `seen_at` that is ahead of `now` (clock skew) is treated as fresh
/// rather than erroring.
fn is_expired(seen_at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(seen_at).map(|age| age >= DELIVERY_ID_TTL).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let scratchpad = AssignmentScratchpad {
            last_seen_id: Some("item-42".to_string()),
            seen_ids: vec!["item-40".to_string(), "item-41".to_string(), "item-42".to_string()],
            seen_deliveries: vec![SeenDelivery {
                id: "delivery-1".to_string(),
                seen_at: SystemTime::now(),
                permanent: false,
                status: DeliveryStatus::Confirmed,
                run_id: None,
                identity_key: None,
                stale_notified: false,
                pending_poll_count: 0,
            }],
            snapshots: vec![ItemSnapshot {
                identity_key: "id-key-1".to_string(),
                version_key: "ver-key-1".to_string(),
                predicate_value: true,
                edge_counter: 1,
                last_seen_at: "2026-07-27T09:00:00Z".to_string(),
                payload: serde_json::json!({ "tag": "Very Important" }),
            }],
            contract_fingerprint: Some("fingerprint-abc".to_string()),
            identity_keygen_version: Some(2),
            missing_required_field_streak: 1,
            truncation_notified: true,
            authoring_failure_streak: 3,
            last_authoring_rejection_reason: Some("proposal failed validation: no material fields declared".to_string()),
            authoring_rejection_history: vec![
                "proposal failed validation: no material fields declared".to_string(),
                "proposal did not match the expected contract shape: missing field change".to_string(),
            ],
            contract_bound_after_failed_attempts: Some(2),
            identity_probe_inconclusive: true,
            identity_probe_inconclusive_reason: Some("the probe's two polls shared no comparable candidate".to_string()),
            authoring_input_fingerprint: Some("authoring-key-abc".to_string()),
            contract_amendment_cycle_count: 2,
            last_extraction_path: ExtractionPath::Deterministic,
            last_inferred_tier: Some(Tier::Deterministic),
            extraction_plan: Some(ExtractionPlan {
                selector: crate::extractor_contract::Selector {
                    kind: crate::extractor_contract::ExtractorKind::JsonPath { path: "items".to_string() },
                    expr: "items".to_string(),
                },
                identity: crate::extractor_contract::ExtractorKind::JsonPath { path: "id".to_string() },
                predicate: crate::extractor_contract::Predicate::NotEmpty { path: "id".to_string() },
            }),
            extraction_plan_fingerprint: Some("extraction-fingerprint-abc".to_string()),
            extraction_plan_expected_item_count: Some(2),
            extraction_plan_expected_fields: Some(BTreeSet::from(["id".to_string(), "tag".to_string()])),
            extraction_plan_degraded: true,
            extraction_plan_degraded_reason: Some("path \"items\" did not resolve".to_string()),
            model_calls_by_day: BTreeMap::from([("2026-07-27".to_string(), 3), ("2026-07-28".to_string(), 1)]),
            consecutive_polls_without_new_items: 247,
            last_new_item_at: Some("2026-07-19T09:00:00Z".to_string()),
            last_poll_observed_candidates: 2,
            last_poll_surviving_candidates: 0,
            all_candidates_quarantined_streak: 1,
        };
        let json = serde_json::to_string(&scratchpad).unwrap();
        let back: AssignmentScratchpad = serde_json::from_str(&json).unwrap();
        assert_eq!(scratchpad, back);
    }

    #[test]
    fn default_is_empty() {
        let scratchpad = AssignmentScratchpad::default();
        assert_eq!(scratchpad.last_seen_id, None);
        assert!(scratchpad.seen_ids.is_empty());
        assert!(scratchpad.seen_deliveries.is_empty());
        assert!(scratchpad.snapshots.is_empty());
        assert_eq!(scratchpad.contract_fingerprint, None);
        assert_eq!(scratchpad.missing_required_field_streak, 0);
        assert!(!scratchpad.truncation_notified);
        assert_eq!(scratchpad.authoring_failure_streak, 0);
        assert_eq!(scratchpad.last_authoring_rejection_reason, None);
        assert_eq!(scratchpad.contract_bound_after_failed_attempts, None);
        assert!(!scratchpad.identity_probe_inconclusive);
        assert_eq!(scratchpad.identity_probe_inconclusive_reason, None);
        assert_eq!(scratchpad.authoring_input_fingerprint, None);
        assert_eq!(scratchpad.contract_amendment_cycle_count, 0);
        assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Unbound);
        assert_eq!(scratchpad.last_inferred_tier, None);
        assert_eq!(scratchpad.extraction_plan, None);
        assert_eq!(scratchpad.extraction_plan_fingerprint, None);
        assert!(!scratchpad.extraction_plan_degraded);
        assert_eq!(scratchpad.extraction_plan_degraded_reason, None);
        assert!(scratchpad.model_calls_by_day.is_empty());
        assert_eq!(scratchpad.consecutive_polls_without_new_items, 0);
        assert_eq!(scratchpad.last_new_item_at, None);
        assert_eq!(scratchpad.last_poll_observed_candidates, 0);
        assert_eq!(scratchpad.last_poll_surviving_candidates, 0);
        assert_eq!(scratchpad.all_candidates_quarantined_streak, 0);
    }

    #[test]
    fn legacy_scratchpad_missing_extraction_path_fields_still_deserializes() {
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "contract_fingerprint": "abc" }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Unbound);
        assert_eq!(scratchpad.last_inferred_tier, None);
    }

    #[test]
    fn legacy_scratchpad_missing_extraction_plan_fields_still_deserializes() {
        // Exact shape a scratchpad was persisted in before this phase added
        // `extraction_plan`/`extraction_plan_fingerprint`/
        // `extraction_plan_degraded`/`extraction_plan_degraded_reason` — a
        // deploy that adds them must never fail to load an already-live
        // watch's scratchpad.
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "contract_fingerprint": "abc" }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert_eq!(scratchpad.extraction_plan, None);
        assert_eq!(scratchpad.extraction_plan_fingerprint, None);
        assert!(!scratchpad.extraction_plan_degraded);
        assert_eq!(scratchpad.extraction_plan_degraded_reason, None);
    }

    #[test]
    fn legacy_scratchpad_missing_authoring_input_fingerprint_key_still_deserializes() {
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "authoring_failure_streak": 5 }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert_eq!(scratchpad.authoring_input_fingerprint, None);
    }

    #[test]
    fn legacy_scratchpad_missing_contract_amendment_cycle_count_key_still_deserializes() {
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "contract_fingerprint": "abc" }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert_eq!(scratchpad.contract_amendment_cycle_count, 0);
    }

    #[test]
    fn legacy_scratchpad_missing_authoring_failure_streak_key_still_deserializes() {
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "contract_fingerprint": "abc" }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert_eq!(scratchpad.authoring_failure_streak, 0);
    }

    #[test]
    fn legacy_scratchpad_missing_last_authoring_rejection_reason_and_identity_probe_fields_still_deserializes() {
        // Exact shape a scratchpad was persisted in before this phase added
        // `last_authoring_rejection_reason`/`identity_probe_inconclusive`/
        // `identity_probe_inconclusive_reason` — a deploy that adds them must
        // never fail to load an already-live watch's scratchpad.
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "authoring_failure_streak": 5 }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert_eq!(scratchpad.last_authoring_rejection_reason, None);
        assert!(!scratchpad.identity_probe_inconclusive);
        assert_eq!(scratchpad.identity_probe_inconclusive_reason, None);
    }

    #[test]
    fn legacy_scratchpad_missing_missing_required_field_streak_key_still_deserializes() {
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "contract_fingerprint": "abc" }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert_eq!(scratchpad.missing_required_field_streak, 0);
    }

    #[test]
    fn legacy_scratchpad_missing_truncation_notified_key_still_deserializes() {
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "contract_fingerprint": "abc" }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert!(!scratchpad.truncation_notified);
    }

    #[test]
    fn legacy_scratchpad_missing_seen_deliveries_key_still_deserializes() {
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [] }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert!(scratchpad.seen_deliveries.is_empty());
    }

    #[test]
    fn legacy_scratchpad_missing_model_call_and_drift_fields_still_deserializes() {
        // Exact shape a scratchpad was persisted in before this phase added
        // model-call and quiet-watch-drift telemetry — a deploy that adds
        // these fields must never fail to load an already-live watch's
        // scratchpad, and must read back as zero/empty, not an error.
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "contract_fingerprint": "abc" }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert!(scratchpad.model_calls_by_day.is_empty());
        assert_eq!(scratchpad.consecutive_polls_without_new_items, 0);
        assert_eq!(scratchpad.last_new_item_at, None);
    }

    #[test]
    fn legacy_scratchpad_missing_all_quarantined_fields_still_deserializes() {
        // Exact shape a scratchpad was persisted in before this phase added
        // the "bound and matching nothing" telemetry — a deploy that adds
        // these fields must never fail to load an already-live watch's
        // scratchpad, and must read back as zero, not an error.
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "contract_fingerprint": "abc" }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert_eq!(scratchpad.last_poll_observed_candidates, 0);
        assert_eq!(scratchpad.last_poll_surviving_candidates, 0);
        assert_eq!(scratchpad.all_candidates_quarantined_streak, 0);
    }

    #[test]
    fn legacy_scratchpad_missing_snapshots_and_fingerprint_keys_still_deserializes_with_seen_ids_intact() {
        let json = r#"{
            "last_seen_id": "item-1",
            "seen_ids": ["item-0", "item-1"],
            "seen_deliveries": [{ "id": "delivery-1", "seen_at": { "secs_since_epoch": 1, "nanos_since_epoch": 0 } }]
        }"#;
        let scratchpad: AssignmentScratchpad =
            serde_json::from_str(json).expect("pre-v2 scratchpad missing snapshots/contract_fingerprint");

        assert_eq!(scratchpad.last_seen_id.as_deref(), Some("item-1"));
        assert_eq!(scratchpad.seen_ids, vec!["item-0".to_string(), "item-1".to_string()]);
        assert_eq!(scratchpad.seen_deliveries.len(), 1, "pre-existing action/delivery ledger must survive the upgrade");
        assert!(!scratchpad.seen_deliveries[0].permanent, "a legacy delivery record must default to non-permanent");
        assert!(scratchpad.snapshots.is_empty());
        assert_eq!(scratchpad.contract_fingerprint, None);
    }

    #[test]
    fn has_seen_delivery_is_false_until_recorded() {
        let scratchpad = AssignmentScratchpad::default();
        assert!(!scratchpad.has_seen_delivery("delivery-1", SystemTime::now()));
    }

    #[test]
    fn record_then_has_seen_delivery_is_true() {
        let mut scratchpad = AssignmentScratchpad::default();
        let now = SystemTime::now();
        scratchpad.record_delivery("delivery-1", now);
        assert!(scratchpad.has_seen_delivery("delivery-1", now));
        assert!(!scratchpad.has_seen_delivery("delivery-2", now));
    }

    #[test]
    fn delivery_expires_after_ttl() {
        let mut scratchpad = AssignmentScratchpad::default();
        let recorded_at = SystemTime::now();
        scratchpad.record_delivery("delivery-1", recorded_at);

        let just_before_expiry = recorded_at + DELIVERY_ID_TTL - Duration::from_secs(1);
        assert!(scratchpad.has_seen_delivery("delivery-1", just_before_expiry));

        let at_expiry = recorded_at + DELIVERY_ID_TTL;
        assert!(
            !scratchpad.has_seen_delivery("delivery-1", at_expiry),
            "an entry exactly at the TTL boundary must be treated as expired"
        );

        let well_past_expiry = recorded_at + DELIVERY_ID_TTL + Duration::from_secs(60);
        assert!(!scratchpad.has_seen_delivery("delivery-1", well_past_expiry));
    }

    #[test]
    fn record_delivery_evicts_expired_entries() {
        let mut scratchpad = AssignmentScratchpad::default();
        let recorded_at = SystemTime::now();
        scratchpad.record_delivery("old-delivery", recorded_at);

        let after_ttl = recorded_at + DELIVERY_ID_TTL + Duration::from_secs(1);
        scratchpad.record_delivery("new-delivery", after_ttl);

        assert_eq!(scratchpad.seen_deliveries.len(), 1, "the expired entry must be evicted, not just skipped");
        assert!(scratchpad.has_seen_delivery("new-delivery", after_ttl));
        assert!(!scratchpad.has_seen_delivery("old-delivery", after_ttl));
    }

    #[test]
    fn future_dated_seen_at_is_treated_as_fresh_not_expired() {
        let mut scratchpad = AssignmentScratchpad::default();
        let now = SystemTime::now();
        let clock_skewed_future = now + Duration::from_secs(30);
        scratchpad.record_delivery("delivery-1", clock_skewed_future);

        assert!(scratchpad.has_seen_delivery("delivery-1", now));
    }

    #[test]
    fn recording_a_delivery_does_not_disturb_poll_cursor_fields() {
        let mut scratchpad = AssignmentScratchpad {
            last_seen_id: Some("cursor-42".to_string()),
            seen_ids: vec!["a".to_string(), "b".to_string()],
            ..Default::default()
        };
        scratchpad.record_delivery("delivery-1", SystemTime::now());

        assert_eq!(scratchpad.last_seen_id.as_deref(), Some("cursor-42"));
        assert_eq!(scratchpad.seen_ids, vec!["a".to_string(), "b".to_string()]);
    }

    // -- delivery_key -----------------------------------------------------

    #[test]
    fn delivery_key_is_stable_across_repeated_calls_with_the_same_inputs() {
        let a = delivery_key("assignment-1", "identity-a", "version-a", 1);
        let b = delivery_key("assignment-1", "identity-a", "version-a", 1);
        assert_eq!(a, b);
    }

    #[test]
    fn delivery_key_is_sensitive_to_edge_counter() {
        let first_entry = delivery_key("assignment-1", "identity-a", "version-a", 1);
        let re_entry = delivery_key("assignment-1", "identity-a", "version-a", 2);
        assert_ne!(
            first_entry, re_entry,
            "a client re-entering the matching state must produce a different ledger key, not be swallowed as a duplicate"
        );
    }

    #[test]
    fn delivery_key_is_sensitive_to_every_component() {
        let base = delivery_key("assignment-1", "identity-a", "version-a", 1);
        assert_ne!(base, delivery_key("assignment-2", "identity-a", "version-a", 1));
        assert_ne!(base, delivery_key("assignment-1", "identity-b", "version-a", 1));
        assert_ne!(base, delivery_key("assignment-1", "identity-a", "version-b", 1));
    }

    #[test]
    fn delivery_key_does_not_let_field_boundaries_collide() {
        // ("ab", "c") and ("a", "bc") must not hash to the same key just
        // because a naive join would concatenate them identically.
        let split_early = delivery_key("assignment-1", "ab", "c", 1);
        let split_late = delivery_key("assignment-1", "a", "bc", 1);
        assert_ne!(split_early, split_late);
    }

    // -- action ledger (record_action / permanent) -------------------------

    #[test]
    fn record_action_is_deduped_and_never_expires() {
        let mut scratchpad = AssignmentScratchpad::default();
        let key = delivery_key("assignment-1", "identity-a", "version-a", 1);
        let recorded_at = SystemTime::now();
        scratchpad.record_action(&key, recorded_at);

        assert!(scratchpad.has_seen_delivery(&key, recorded_at));

        let long_after_delivery_ttl = recorded_at + DELIVERY_ID_TTL * 100;
        assert!(
            scratchpad.has_seen_delivery(&key, long_after_delivery_ttl),
            "an action-ledger entry must never expire, unlike a push-delivery id"
        );
    }

    #[test]
    fn record_action_does_not_evict_permanent_entries_when_pruning_expired_deliveries() {
        let mut scratchpad = AssignmentScratchpad::default();
        let action_key = delivery_key("assignment-1", "identity-a", "version-a", 1);
        let recorded_at = SystemTime::now();
        scratchpad.record_action(&action_key, recorded_at);

        let long_after_delivery_ttl = recorded_at + DELIVERY_ID_TTL * 100;
        scratchpad.record_delivery("push-delivery-1", long_after_delivery_ttl);

        assert!(scratchpad.has_seen_delivery(&action_key, long_after_delivery_ttl));
        assert!(scratchpad.has_seen_delivery("push-delivery-1", long_after_delivery_ttl));
    }

    // -- two-phase delivery ledger (record_pending_action / attach_dispatch_run / confirm_pending_delivery) --

    #[test]
    fn record_pending_action_records_pending_with_no_run_id_yet() {
        // The match-time write happens *before* dispatch is attempted, so
        // there is no run to correlate against yet — that arrives later via
        // `attach_dispatch_run`, once `fire_assignment` has actually run.
        let mut scratchpad = AssignmentScratchpad::default();
        let key = delivery_key("assignment-1", "identity-a", "version-a", 1);
        let recorded_at = SystemTime::now();
        scratchpad.record_pending_action(&key, "identity-a", recorded_at);

        assert_eq!(scratchpad.seen_deliveries.len(), 1);
        let entry = &scratchpad.seen_deliveries[0];
        assert_eq!(entry.status, DeliveryStatus::Pending);
        assert_eq!(entry.run_id, None);
        assert_eq!(entry.identity_key.as_deref(), Some("identity-a"));
        assert!(!entry.stale_notified);
        assert_eq!(entry.pending_poll_count, 0);
        // Dedup semantics are unaffected by status: a Pending entry already
        // counts as "seen" so a duplicate fire attempt is still suppressed.
        assert!(scratchpad.has_seen_delivery(&key, recorded_at));
    }

    #[test]
    fn record_pending_action_overwrites_a_prior_entry_for_the_same_key_and_resets_poll_count() {
        let mut scratchpad = AssignmentScratchpad::default();
        let key = delivery_key("assignment-1", "identity-a", "version-a", 1);
        scratchpad.record_pending_action(&key, "identity-a", SystemTime::now());
        scratchpad.seen_deliveries[0].pending_poll_count = 5;

        scratchpad.record_pending_action(&key, "identity-a", SystemTime::now());

        assert_eq!(scratchpad.seen_deliveries.len(), 1, "re-recording under the same key must not duplicate");
        assert_eq!(scratchpad.seen_deliveries[0].pending_poll_count, 0, "a fresh retry deserves a fresh count");
    }

    #[test]
    fn attach_dispatch_run_sets_run_id_on_a_pending_entry() {
        let mut scratchpad = AssignmentScratchpad::default();
        let key = delivery_key("assignment-1", "identity-a", "version-a", 1);
        scratchpad.record_pending_action(&key, "identity-a", SystemTime::now());

        assert!(scratchpad.attach_dispatch_run(&key, "run-1".to_string()));
        assert_eq!(scratchpad.seen_deliveries[0].run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn attach_dispatch_run_is_a_noop_for_an_unknown_key() {
        let mut scratchpad = AssignmentScratchpad::default();
        assert!(!scratchpad.attach_dispatch_run("no-such-key", "run-1".to_string()));
    }

    #[test]
    fn attach_dispatch_run_is_a_noop_once_already_confirmed() {
        let mut scratchpad = AssignmentScratchpad::default();
        let key = delivery_key("assignment-1", "identity-a", "version-a", 1);
        scratchpad.record_pending_action(&key, "identity-a", SystemTime::now());
        assert!(scratchpad.confirm_pending_delivery(&key));

        assert!(!scratchpad.attach_dispatch_run(&key, "run-1".to_string()));
        assert_eq!(scratchpad.seen_deliveries[0].run_id, None);
    }

    #[test]
    fn confirm_pending_delivery_promotes_and_clears_correlators() {
        let mut scratchpad = AssignmentScratchpad::default();
        let key = delivery_key("assignment-1", "identity-a", "version-a", 1);
        scratchpad.record_pending_action(&key, "identity-a", SystemTime::now());
        scratchpad.attach_dispatch_run(&key, "run-1".to_string());

        assert!(scratchpad.confirm_pending_delivery(&key));

        let entry = &scratchpad.seen_deliveries[0];
        assert_eq!(entry.status, DeliveryStatus::Confirmed);
        assert_eq!(entry.run_id, None, "nothing left to reconcile once confirmed");
        assert_eq!(entry.identity_key, None);
    }

    #[test]
    fn confirm_pending_delivery_is_a_noop_for_an_unknown_key() {
        let mut scratchpad = AssignmentScratchpad::default();
        assert!(!scratchpad.confirm_pending_delivery("no-such-key"));
    }

    #[test]
    fn clear_pending_delivery_removes_the_entry_making_it_retry_eligible() {
        let mut scratchpad = AssignmentScratchpad::default();
        let key = delivery_key("assignment-1", "identity-a", "version-a", 1);
        scratchpad.record_pending_action(&key, "identity-a", SystemTime::now());
        assert!(scratchpad.has_seen_delivery(&key, SystemTime::now()));

        scratchpad.clear_pending_delivery(&key);

        assert!(scratchpad.seen_deliveries.is_empty());
        assert!(
            !scratchpad.has_seen_delivery(&key, SystemTime::now()),
            "clearing the entry must free the key up for a fresh fire"
        );
    }

    #[test]
    fn clear_pending_delivery_is_a_noop_for_an_unknown_key() {
        let mut scratchpad = AssignmentScratchpad::default();
        scratchpad.clear_pending_delivery("no-such-key");
        assert!(scratchpad.seen_deliveries.is_empty());
    }

    #[test]
    fn confirm_pending_delivery_is_a_noop_for_an_already_confirmed_entry() {
        let mut scratchpad = AssignmentScratchpad::default();
        let key = delivery_key("assignment-1", "identity-a", "version-a", 1);
        scratchpad.record_action(&key, SystemTime::now());

        assert!(
            !scratchpad.confirm_pending_delivery(&key),
            "confirming an entry that was never Pending must be a no-op, not an error"
        );
        assert_eq!(scratchpad.seen_deliveries[0].status, DeliveryStatus::Confirmed);
    }

    #[test]
    fn record_action_still_records_directly_as_confirmed() {
        let mut scratchpad = AssignmentScratchpad::default();
        let key = delivery_key("assignment-1", "identity-a", "version-a", 1);
        scratchpad.record_action(&key, SystemTime::now());

        let entry = &scratchpad.seen_deliveries[0];
        assert_eq!(entry.status, DeliveryStatus::Confirmed);
        assert_eq!(entry.run_id, None);
        assert_eq!(entry.identity_key, None);
    }

    #[test]
    fn legacy_on_disk_seen_delivery_missing_status_field_defaults_to_confirmed() {
        // Exact shape a `SeenDelivery` was persisted in before `status` /
        // `run_id` / `identity_key` / `stale_notified` existed (pre-dates
        // this two-phase ledger entirely). A deploy that adds these fields
        // must never cause a previously-delivered item to read back as
        // newly `Pending` — that would turn every already-completed
        // delivery into a spurious "never confirmed" health warning on
        // the next poll.
        let json = r#"{
            "last_seen_id": null,
            "seen_ids": [],
            "seen_deliveries": [
                { "id": "delivery-abc", "seen_at": { "secs_since_epoch": 1700000000, "nanos_since_epoch": 0 }, "permanent": true }
            ]
        }"#;
        let scratchpad: AssignmentScratchpad =
            serde_json::from_str(json).expect("pre-two-phase seen_deliveries entry must still deserialize");

        assert_eq!(scratchpad.seen_deliveries.len(), 1);
        let entry = &scratchpad.seen_deliveries[0];
        assert_eq!(entry.id, "delivery-abc");
        assert!(entry.permanent);
        assert_eq!(
            entry.status,
            DeliveryStatus::Confirmed,
            "a legacy entry with no status field must default to Confirmed, never Pending"
        );
        assert_eq!(entry.run_id, None);
        assert_eq!(entry.identity_key, None);
        assert!(!entry.stale_notified);
        assert_eq!(entry.pending_poll_count, 0);
    }

    // -- record_snapshot / SNAPSHOT_CAP -------------------------------------

    fn snapshot(identity_key: &str) -> ItemSnapshot {
        ItemSnapshot {
            identity_key: identity_key.to_string(),
            version_key: "version-1".to_string(),
            predicate_value: false,
            edge_counter: 0,
            last_seen_at: "2026-07-27T09:00:00Z".to_string(),
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn record_snapshot_upserts_by_identity_key_instead_of_duplicating() {
        let mut scratchpad = AssignmentScratchpad::default();
        assert!(scratchpad.record_snapshot(snapshot("item-1")).is_none());
        let mut updated = snapshot("item-1");
        updated.predicate_value = true;
        assert!(scratchpad.record_snapshot(updated).is_none());

        assert_eq!(scratchpad.snapshots.len(), 1);
        assert!(scratchpad.snapshots[0].predicate_value);
    }

    #[test]
    fn record_snapshot_under_cap_reports_no_truncation() {
        let mut scratchpad = AssignmentScratchpad::default();
        for i in 0..10 {
            assert!(scratchpad.record_snapshot(snapshot(&format!("item-{i}"))).is_none());
        }
        assert_eq!(scratchpad.snapshots.len(), 10);
    }

    #[test]
    fn record_snapshot_over_cap_evicts_oldest_first_and_reports_truncation() {
        let mut scratchpad = AssignmentScratchpad::default();
        let mut last_truncation = None;
        for i in 0..(SNAPSHOT_CAP + 10) {
            last_truncation = scratchpad.record_snapshot(snapshot(&format!("item-{i}")));
        }

        assert_eq!(scratchpad.snapshots.len(), SNAPSHOT_CAP, "snapshots must be capped at SNAPSHOT_CAP");
        let truncation = last_truncation.expect("pushing past the cap must report a truncation");
        assert_eq!(truncation.dropped_count, 1, "each push past the cap evicts exactly one oldest entry");
        assert_eq!(truncation.retained_count, SNAPSHOT_CAP);

        // The oldest 10 were dropped; the most recent SNAPSHOT_CAP remain.
        assert!(!scratchpad.snapshots.iter().any(|s| s.identity_key == "item-0"));
        assert!(scratchpad.snapshots.iter().any(|s| s.identity_key == format!("item-{}", SNAPSHOT_CAP + 9)));
    }

    // -- record_model_call / MODEL_CALL_DAY_BUCKET_CAP ---------------------

    #[test]
    fn record_model_call_creates_and_increments_a_day_bucket() {
        let mut scratchpad = AssignmentScratchpad::default();
        scratchpad.record_model_call("2026-07-28");
        scratchpad.record_model_call("2026-07-28");
        scratchpad.record_model_call("2026-07-27");

        assert_eq!(scratchpad.model_calls_by_day.get("2026-07-28"), Some(&2));
        assert_eq!(scratchpad.model_calls_by_day.get("2026-07-27"), Some(&1));
        assert_eq!(scratchpad.model_calls_by_day.len(), 2);
    }

    #[test]
    fn record_model_call_day_bucket_map_is_bounded() {
        let mut scratchpad = AssignmentScratchpad::default();
        for day in 0..(MODEL_CALL_DAY_BUCKET_CAP + 10) {
            scratchpad.record_model_call(&format!("2026-01-{day:03}"));
        }

        assert_eq!(
            scratchpad.model_calls_by_day.len(),
            MODEL_CALL_DAY_BUCKET_CAP,
            "feeding many distinct days must evict old ones rather than growing forever"
        );
        assert!(
            !scratchpad.model_calls_by_day.contains_key("2026-01-000"),
            "the oldest day must have been evicted"
        );
        assert!(scratchpad.model_calls_by_day.contains_key(&format!("2026-01-{:03}", MODEL_CALL_DAY_BUCKET_CAP + 9)));
    }

    // -- record_poll_outcome / consecutive_polls_without_new_items ---------

    #[test]
    fn record_poll_outcome_increments_streak_across_successive_quiet_polls() {
        let mut scratchpad = AssignmentScratchpad::default();
        scratchpad.record_poll_outcome(false, "2026-07-26T09:00:00Z");
        scratchpad.record_poll_outcome(false, "2026-07-27T09:00:00Z");
        scratchpad.record_poll_outcome(false, "2026-07-28T09:00:00Z");

        assert_eq!(scratchpad.consecutive_polls_without_new_items, 3);
        assert_eq!(scratchpad.last_new_item_at, None, "no poll has fired yet");
    }

    #[test]
    fn record_poll_outcome_resets_streak_and_stamps_last_new_item_at_on_a_fire() {
        let mut scratchpad = AssignmentScratchpad::default();
        scratchpad.record_poll_outcome(false, "2026-07-26T09:00:00Z");
        scratchpad.record_poll_outcome(false, "2026-07-27T09:00:00Z");
        scratchpad.record_poll_outcome(true, "2026-07-28T09:00:00Z");

        assert_eq!(scratchpad.consecutive_polls_without_new_items, 0);
        assert_eq!(scratchpad.last_new_item_at.as_deref(), Some("2026-07-28T09:00:00Z"));
    }

    #[test]
    fn a_large_consecutive_empty_count_does_not_touch_any_other_health_field() {
        // Informational only, by design: this counter must never couple
        // into anything this crate treats as a degraded/unhealthy signal.
        let mut scratchpad = AssignmentScratchpad::default();
        for _ in 0..10_000 {
            scratchpad.record_poll_outcome(false, "2026-07-28T09:00:00Z");
        }

        assert_eq!(scratchpad.consecutive_polls_without_new_items, 10_000);
        assert!(!scratchpad.truncation_notified);
        assert!(!scratchpad.extraction_plan_degraded);
        assert_eq!(scratchpad.authoring_failure_streak, 0);
        assert_eq!(scratchpad.missing_required_field_streak, 0);
    }

    // -- ExtractionHealth ---------------------------------------------------

    #[test]
    fn extraction_health_serializes_to_the_wire_names_the_frontend_type_expects() {
        assert_eq!(serde_json::to_string(&ExtractionHealth::Pending).unwrap(), "\"pending\"");
        assert_eq!(serde_json::to_string(&ExtractionHealth::Deterministic).unwrap(), "\"deterministic\"");
        assert_eq!(serde_json::to_string(&ExtractionHealth::ModelAssisted).unwrap(), "\"model_assisted\"");
        assert_eq!(serde_json::to_string(&ExtractionHealth::Degraded).unwrap(), "\"degraded\"");
    }

    #[test]
    fn legacy_scratchpad_missing_contract_bound_after_failed_attempts_key_still_deserializes() {
        let json = r#"{ "last_seen_id": "item-1", "seen_ids": [], "authoring_failure_streak": 5 }"#;
        let scratchpad: AssignmentScratchpad = serde_json::from_str(json).expect("legacy scratchpad");
        assert_eq!(scratchpad.contract_bound_after_failed_attempts, None);
    }

    // -- invalidate_watch_contract_state -------------------------------------

    /// A scratchpad with every contract-derived field populated, as if a
    /// `WatchContract` had bound after some repair and run for a while.
    fn scratchpad_with_bound_contract_state() -> AssignmentScratchpad {
        AssignmentScratchpad {
            last_seen_id: Some("cursor-1".to_string()),
            seen_ids: vec!["a".to_string()],
            seen_deliveries: vec![SeenDelivery {
                id: "action-1".to_string(),
                seen_at: SystemTime::now(),
                permanent: true,
                status: DeliveryStatus::Confirmed,
                run_id: None,
                identity_key: None,
                stale_notified: false,
                pending_poll_count: 0,
            }],
            snapshots: vec![ItemSnapshot {
                identity_key: "id-key-1".to_string(),
                version_key: "ver-key-1".to_string(),
                predicate_value: true,
                edge_counter: 1,
                last_seen_at: "2026-07-27T09:00:00Z".to_string(),
                payload: serde_json::json!({ "tag": "x" }),
            }],
            contract_fingerprint: Some("fingerprint-abc".to_string()),
            identity_keygen_version: Some(2),
            missing_required_field_streak: 2,
            truncation_notified: true,
            authoring_failure_streak: 3,
            last_authoring_rejection_reason: Some("proposal failed validation".to_string()),
            authoring_rejection_history: vec!["proposal failed validation".to_string(), "still broken".to_string()],
            contract_bound_after_failed_attempts: Some(2),
            identity_probe_inconclusive: true,
            identity_probe_inconclusive_reason: Some("inconclusive".to_string()),
            authoring_input_fingerprint: Some("authoring-key-abc".to_string()),
            contract_amendment_cycle_count: 4,
            last_extraction_path: ExtractionPath::Deterministic,
            last_inferred_tier: Some(Tier::Deterministic),
            extraction_plan: Some(ExtractionPlan {
                selector: crate::extractor_contract::Selector {
                    kind: crate::extractor_contract::ExtractorKind::JsonPath { path: "items".to_string() },
                    expr: "items".to_string(),
                },
                identity: crate::extractor_contract::ExtractorKind::JsonPath { path: "id".to_string() },
                predicate: crate::extractor_contract::Predicate::NotEmpty { path: "id".to_string() },
            }),
            extraction_plan_fingerprint: Some("extraction-fingerprint-abc".to_string()),
            extraction_plan_expected_item_count: Some(2),
            extraction_plan_expected_fields: Some(BTreeSet::from(["id".to_string()])),
            extraction_plan_degraded: true,
            extraction_plan_degraded_reason: Some("path did not resolve".to_string()),
            model_calls_by_day: BTreeMap::from([("2026-07-27".to_string(), 3)]),
            consecutive_polls_without_new_items: 12,
            last_new_item_at: Some("2026-07-19T09:00:00Z".to_string()),
            last_poll_observed_candidates: 4,
            last_poll_surviving_candidates: 1,
            all_candidates_quarantined_streak: 2,
        }
    }

    #[test]
    fn invalidate_watch_contract_state_clears_every_contract_derived_field() {
        let mut scratchpad = scratchpad_with_bound_contract_state();
        scratchpad.invalidate_watch_contract_state();

        assert!(scratchpad.snapshots.is_empty());
        assert_eq!(scratchpad.contract_fingerprint, None);
        assert_eq!(scratchpad.identity_keygen_version, None);
        assert_eq!(scratchpad.missing_required_field_streak, 0);
        assert!(!scratchpad.truncation_notified);
        assert_eq!(scratchpad.authoring_failure_streak, 0);
        assert_eq!(scratchpad.last_authoring_rejection_reason, None);
        assert!(
            scratchpad.authoring_rejection_history.is_empty(),
            "a discarded contract's accumulated rejection history must not seed the next authoring streak"
        );
        assert_eq!(
            scratchpad.contract_bound_after_failed_attempts, None,
            "a discarded contract's convergence note must not survive onto whatever contract binds next"
        );
        assert!(!scratchpad.identity_probe_inconclusive);
        assert_eq!(scratchpad.identity_probe_inconclusive_reason, None);
        assert_eq!(scratchpad.authoring_input_fingerprint, None);
        assert_eq!(scratchpad.contract_amendment_cycle_count, 0);
        assert_eq!(scratchpad.last_extraction_path, ExtractionPath::Unbound);
        assert_eq!(scratchpad.last_inferred_tier, None);
        assert_eq!(scratchpad.extraction_plan, None);
        assert_eq!(scratchpad.extraction_plan_fingerprint, None);
        assert_eq!(scratchpad.extraction_plan_expected_item_count, None);
        assert_eq!(scratchpad.extraction_plan_expected_fields, None);
        assert!(!scratchpad.extraction_plan_degraded);
        assert_eq!(scratchpad.extraction_plan_degraded_reason, None);
        assert_eq!(scratchpad.last_poll_observed_candidates, 0);
        assert_eq!(scratchpad.last_poll_surviving_candidates, 0);
        assert_eq!(scratchpad.all_candidates_quarantined_streak, 0);
    }

    #[test]
    fn invalidate_watch_contract_state_preserves_identity_independent_telemetry() {
        let mut scratchpad = scratchpad_with_bound_contract_state();
        scratchpad.invalidate_watch_contract_state();

        assert_eq!(scratchpad.last_seen_id.as_deref(), Some("cursor-1"));
        assert_eq!(scratchpad.seen_ids, vec!["a".to_string()]);
        assert_eq!(scratchpad.seen_deliveries.len(), 1, "the action ledger must survive a contract edit");
        assert_eq!(
            scratchpad.model_calls_by_day.get("2026-07-27"),
            Some(&3),
            "model-call telemetry must never be reset by a contract edit"
        );
        assert_eq!(scratchpad.consecutive_polls_without_new_items, 12);
        assert_eq!(scratchpad.last_new_item_at.as_deref(), Some("2026-07-19T09:00:00Z"));
    }
}
