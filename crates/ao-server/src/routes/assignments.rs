use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use croner::Cron;
use serde::Deserialize;
use uuid::Uuid;

use ao_engine::agent_watch::{derive_extraction_health, derive_watch_contract_status, model_calls_today, WatchContractStatus};
use ao_engine::assignment_runner::fire_assignment;
use ao_engine::queue_manager::NotificationDispatcher;
use ao_engine::AppState;
use ao_persistence::cron_util::compute_next_fire_at;
use ao_protocol::assignment::{
    carry_forward_watch_contract, Assignment, AssignmentBinding, AssignmentRun,
    AssignmentThreadPolicy, AssignmentTrigger, AssignmentTriggerKind, OutputMode,
    QuiescenceReason, TriggerEventContext,
};
use ao_protocol::assignment_scratchpad::{ExtractionHealth, ExtractionPath};
use ao_protocol::error::AoError;
use ao_protocol::extractor_contract::{ExtractorKind, Tier};
use ao_protocol::webhook_template::render_prompt_template;
use serde::Serialize;
use tracing::{info, warn};

use crate::error::AppError;

/// Health payload joined onto the API response for EVERY assignment,
/// regardless of trigger kind — the one liveness contract that answers, for
/// any `Cron`/`ConnectorEvent`/`AgentWatch` row, the four questions "am I
/// bound?", "when did I last evaluate?", "what did I see?", and "why have I
/// not fired?".
///
/// Originally `AgentWatch`-only (hence the name, kept for backward
/// compatibility with the frontend consumer already reading it): the
/// extraction-tier fields below this doc comment are still populated purely
/// from the watch's persisted `AssignmentScratchpad`
/// (`ao_protocol::assignment_scratchpad`) and stay meaningful only for an
/// `AgentWatch` trigger — see each field's own doc. `Cron`/`ConnectorEvent`
/// rows get this same struct with those fields at their neutral "nothing to
/// report" values (no scratchpad concept exists for them), and the fields
/// added below (`last_evaluated_at`/`fire_count`/`quiescence_reason`/
/// `quiescence_explanation`), which are populated the same way for every
/// trigger kind straight off `Assignment.liveness` — these are what make
/// this struct one contract instead of an `AgentWatch` special case.
///
/// This is what makes the "if the engine detects it, the user sees it" rule
/// (no silent degradation across `extractor_contract::Tier`) hold at the API
/// boundary: a watch that has never polled (`has_evaluated: false`) must stay
/// distinguishable from one that polled and simply has no tier to report
/// (`has_evaluated: true`, `tier: None` — e.g. a contract not yet bound, or a
/// bound contract with no `ExtractionPlan` configured), which in turn must
/// stay distinguishable from every one of the three `Tier` values. The same
/// "never conflate unknown with healthy-quiet" rule is why
/// `last_evaluated_at: None` (never evaluated) must render distinctly from
/// `last_evaluated_at: Some(_)` with `quiescence_reason: None` (evaluated,
/// and its most recent tick fired) — collapsing those two readings is the
/// exact bug class this struct's generic fields exist to eliminate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssignmentWatchHealth {
    /// `false` until this watch's first poll has produced a scratchpad.
    /// Meaningful only for an `AgentWatch` trigger — always `false` for
    /// `Cron`/`ConnectorEvent` rows (neither has a scratchpad concept);
    /// those trigger kinds answer "have I ever been evaluated" via
    /// `last_evaluated_at` below instead.
    pub has_evaluated: bool,
    /// `None` on an evaluated watch with no `ExtractionPlan` bound at the
    /// time of its last poll (contract still being authored, or a bound
    /// contract with no extraction plan — every poll runs the full model
    /// detector instead). Otherwise the tier that poll's plan resolved to.
    pub tier: Option<Tier>,
    /// Which mechanism actually produced the last poll's candidates.
    pub extraction_path: Option<ExtractionPath>,
    /// The concrete, engine-derived cause the extraction plan could only
    /// hash the whole response instead of extracting individual items.
    /// `Some` exactly when `tier` is `Some(Tier::ChangeDetectionOnly)` — this
    /// is the reason a `ChangeDetectionOnly` badge must display instead of a
    /// generic "unavailable" string.
    pub degraded_reason: Option<String>,
    /// `true` once a poll's `extractor_contract::resolve` call has failed
    /// structurally (the plan's selector/identity path no longer matches
    /// what its tool actually returned) and this watch fell back to the
    /// model for that poll — distinct from `degraded_reason` above, which
    /// only ever explains a `ChangeDetectionOnly` tier, itself a normal,
    /// expected state. A watch running on this fallback is never healthy:
    /// "if the engine detects it, the user sees it."
    pub extraction_plan_degraded: bool,
    /// The structured `extractor_contract::BindError`'s own detail (the
    /// available-paths list or the excerpt that failed to match) as of the
    /// poll that set `extraction_plan_degraded` — `Some` exactly when
    /// `extraction_plan_degraded` is `true`, so the eventual UI surface can
    /// show *why* the plan broke, not just *that* it did.
    pub extraction_plan_degraded_reason: Option<String>,
    /// The bound contract's fire condition rendered back into the readable
    /// expression grammar, for the contract explainer's "show raw expression"
    /// disclosure.
    ///
    /// Derived here, at the response layer, rather than stored on
    /// `WatchContract` — deliberately. `WatchContract::fingerprint()`
    /// serializes the entire contract, so adding a rendered-expression field
    /// inside it would change the fingerprint of every already-persisted
    /// assignment and force a snapshot reseed (each watch would drop its
    /// per-item baseline). This field is response-only and never persisted,
    /// so the fingerprint is untouched.
    ///
    /// It is re-rendered from the typed predicate that actually executes,
    /// never a replay of the string a contract was originally authored from —
    /// so it cannot drift into showing a condition the watch is not running.
    ///
    /// `None` when no contract is bound yet, or when the bound predicate uses
    /// comparisons the display grammar cannot express (see
    /// `Predicate::to_expr`). Both are rendered as an explicit message by the
    /// UI, never as a blank box.
    pub predicate_expr: Option<String>,
    /// Per-day count of LLM child sessions this watch's detector has
    /// spawned, keyed by UTC calendar date (`YYYY-MM-DD`) — the only
    /// usage/cost telemetry this system tracks, straight off
    /// `AssignmentScratchpad::model_calls_by_day`. Empty on a watch that has
    /// never spawned a model session (either it has never polled, or every
    /// poll so far resolved deterministically).
    pub model_calls_by_day: std::collections::BTreeMap<String, u32>,
    /// Count of consecutive completed polls that produced zero newly-fired
    /// items — informational only, straight off
    /// `AssignmentScratchpad::consecutive_polls_without_new_items`. A high
    /// value does not mean this watch is unhealthy; it may simply be quiet.
    pub consecutive_polls_without_new_items: u32,
    /// RFC3339 timestamp of the last poll that fired at least one item —
    /// straight off `AssignmentScratchpad::last_new_item_at`. `None` until
    /// this watch's first-ever fire.
    pub last_new_item_at: Option<String>,
    /// Whether this watch's steady-state poll can skip the model entirely —
    /// `ao_engine::agent_watch::derive_extraction_health`'s explicit answer
    /// to "if the engine detects it, the user sees it," specifically for the
    /// gap `extraction_path`/`extraction_plan_degraded` alone leave open: a
    /// watch with a frozen `extraction_tool` but no extraction plan ever
    /// authored for it runs the model on every single poll, the same as a
    /// watch that's still mid-authoring — `extraction_path` reads `Llm` for
    /// both, indistinguishable from a healthy `Unbound`/no-plan-configured
    /// watch without this field.
    pub extraction_health: ExtractionHealth,
    /// Human-readable reason for `extraction_health` — verbatim UI copy, not
    /// a placeholder. `None` for `Pending`/`Deterministic` (nothing to
    /// explain); `Some` for `ModelAssisted` (why no plan could be authored)
    /// and `Degraded` (reuses `extraction_plan_degraded_reason`).
    pub extraction_health_reason: Option<String>,
    /// Today's entry in `AssignmentScratchpad::model_calls_by_day` (UTC
    /// calendar date), `0` if this watch has spawned no model session yet
    /// today — the immediate "is this costing me right now" number
    /// `model_calls_by_day`'s full history doesn't answer at a glance.
    pub model_calls_today: u32,
    /// Which mechanism produced the last poll's candidates, as a plain
    /// string — the same value as `extraction_path`, duplicated under this
    /// name because the extraction-health UI reads it directly rather than
    /// through the typed `WatchExtractionPath` union. `None` exactly when
    /// `extraction_path` is `None`.
    pub last_extraction_path: Option<String>,
    /// `true` once a bound `native_id` contract's stability probe came back
    /// inconclusive rather than confirmed stable — straight off
    /// `AssignmentScratchpad::identity_probe_inconclusive`. The identity is
    /// bound and the watch runs normally either way (an inconclusive probe
    /// never drops a rung — only a positive instability finding does), but
    /// this is what makes "this watch's identity was never actually
    /// verified across polls" visible instead of indistinguishable from one
    /// that was.
    pub identity_probe_inconclusive: bool,
    /// Human-readable reason for `identity_probe_inconclusive` — straight
    /// off `AssignmentScratchpad::identity_probe_inconclusive_reason`.
    /// `Some` exactly when that flag is `true`.
    pub identity_probe_inconclusive_reason: Option<String>,
    /// Whether the last poll's zero-model-call extraction is backed by a
    /// server-declared schema (`"declared_schema"`) or was reconstructed by
    /// parsing text out of a response the server never declared a schema
    /// for (`"parsed_from_text"`) — see [`extraction_provenance_wire_str`].
    ///
    /// This exists because `extraction_health: Deterministic` alone
    /// conflates the two: the cost claim its frozen-contract disclosure
    /// makes ("no model reviews this before it runs") is true in both
    /// cases, but what it says nothing about is DRIFT RISK, which
    /// provenance predicts and cost does not — a declared-schema plan has a
    /// server-side contract behind its shape, a text-rescued one does not.
    /// `None` whenever the last poll didn't actually run through a
    /// resolved plan (still model-assisted, unbound, or a response captured
    /// before this field existed) — that absence must never be read as a
    /// "declared schema" guarantee it didn't earn.
    pub extraction_provenance: Option<String>,
    /// `true` when the most recently completed poll observed at least one
    /// candidate and quarantined every one of them — "bound and matching
    /// nothing." This is the condition `consecutive_polls_without_new_items`
    /// alone cannot distinguish from a genuinely quiet watch: that counter
    /// climbs identically whether zero candidates arrived or every one of
    /// them was rejected, and "if the engine detects it, the user sees it"
    /// requires those two to look different. Derived here, server-side, from
    /// `AssignmentScratchpad::last_poll_observed_candidates`/
    /// `last_poll_surviving_candidates` — never left for a client to
    /// recompute from those two counts itself. `false` on a watch that has
    /// never polled, or whose last poll was quiet or had at least one
    /// surviving candidate.
    pub bound_matching_nothing: bool,
    /// Count of consecutive polls `bound_matching_nothing` has been `true`,
    /// straight off `AssignmentScratchpad::all_candidates_quarantined_streak`
    /// — lets a UI distinguish "just happened once" from "every poll for a
    /// while now." `0` on a watch that has never polled.
    pub bound_matching_nothing_streak: u32,
    /// The single source of truth for which of three MUTUALLY EXCLUSIVE
    /// contract-authoring states this watch is in — see
    /// `ao_engine::agent_watch::WatchContractStatus`'s own doc. Replaces
    /// what the frontend used to reconcile itself from two independent
    /// signals (`Assignment::trigger`'s own `contract` presence, and this
    /// same response's `extraction_health == ModelAssisted`), which could
    /// both read true at once and render two contradictory statements on the
    /// same panel. A client MUST branch on this field alone for "is a
    /// contract bound yet" — never re-derive it from `tier`/`extraction_health`
    /// or from whether `Assignment::trigger`'s `contract` is present.
    pub contract_status: WatchContractStatus,

    // -- Generic liveness fields below: populated identically for every
    // -- trigger kind, straight off `Assignment.liveness`. These, not the
    // -- extraction-tier fields above, are what answer the four liveness
    // -- questions ("am I bound?", "when did I last evaluate?", "what did I
    // -- see?", "why have I not fired?") for `Cron`/`ConnectorEvent` rows.
    /// When the tick loop most recently evaluated this assignment at all,
    /// regardless of trigger kind or whether that tick fired — straight off
    /// `Assignment.liveness.last_evaluated_at`. `None` until the very first
    /// tick ever looks at this assignment. This is the field that answers
    /// "when did I last evaluate?", and — together with `quiescence_reason`
    /// below — is what tells a never-evaluated assignment apart from one
    /// that was evaluated and correctly chose not to fire: the former has
    /// `last_evaluated_at: None`, the latter has `last_evaluated_at:
    /// Some(_)` with `quiescence_reason: Some(_)`.
    pub last_evaluated_at: Option<DateTime<Utc>>,
    /// Total number of times this assignment has fired, over its whole
    /// lifetime — straight off `Assignment.liveness.fire_count`. `0` for an
    /// assignment that has never fired, whether or not it has ever been
    /// evaluated.
    pub fire_count: u64,
    /// Why the most recent tick ended without firing, machine-readable —
    /// straight off `Assignment.liveness.last_quiescence`. `None` when
    /// either this assignment has never been evaluated, or its most recent
    /// tick fired (`LivenessState::last_quiescence` is cleared on every
    /// fire, per that field's own doc). Present for every trigger kind, not
    /// just `AgentWatch` — this is the answer to "what did I see?"/"why have
    /// I not fired?" in machine-readable form; render `quiescence_explanation`
    /// below for the human-readable one, never re-derive prose from this tag
    /// client-side.
    pub quiescence_reason: Option<QuiescenceReason>,
    /// One plain-English sentence rendered server-side from
    /// `quiescence_reason`, naming the specific cause (e.g. which MCP server
    /// is disconnected, or what a failed fire attempt's error said) rather
    /// than a generic "unavailable" string — see
    /// `render_quiescence_explanation`. `Some` exactly when
    /// `quiescence_reason` is `Some`; never a placeholder. This is the
    /// actual product surface for "why have I not fired?" — a client should
    /// render this string directly rather than switching on
    /// `quiescence_reason`'s tag itself.
    pub quiescence_explanation: Option<String>,
}

/// Wire response shape for every assignment-returning route below: the
/// persisted [`Assignment`] row flattened alongside its derived, non-persisted
/// [`AssignmentWatchHealth`].
#[derive(Debug, Serialize)]
pub struct AssignmentResponse {
    #[serde(flatten)]
    pub assignment: Assignment,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch_health: Option<AssignmentWatchHealth>,
}

/// Explains, in terms a user can act on, why an `AgentWatch`'s extraction
/// plan could only reach [`Tier::ChangeDetectionOnly`] — derived purely from
/// what the trigger itself declares ([`ExtractorKind`], whether the connector
/// declared an output schema), never a generic "unavailable" placeholder.
fn change_detection_reason(trigger: &AssignmentTrigger) -> String {
    match trigger {
        AssignmentTrigger::AgentWatch { extraction: Some(plan), extraction_output_schema_declared, .. } => {
            match &plan.selector.kind {
                ExtractorKind::Hash if *extraction_output_schema_declared => {
                    "The extraction plan hashes the whole tool response as one unit — it declares no selector or identity field for individual items, so it can tell you the response changed but not what changed.".to_string()
                }
                ExtractorKind::Hash => {
                    "The connector has not declared an output schema for this tool, and the extraction plan has no selector for individual items, so it can only hash the whole response — it can tell you something changed, not what.".to_string()
                }
                _ => {
                    "The extraction plan did not qualify for item-level extraction on the last poll, so this watch fell back to hashing the whole response — it can tell you something changed, not what.".to_string()
                }
            }
        }
        _ => {
            "No extraction plan is bound to this watch — it has no declared output schema, no structured content to select from, and no stable identity field, so it can only detect that the response changed, not what changed.".to_string()
        }
    }
}

/// `%Y-%m-%d %H:%M UTC` — the one timestamp rendering
/// `render_quiescence_explanation` uses, so every sentence it produces
/// states a date alongside the time instead of a bare "since 14:02" that
/// would read as "today" no matter how stale the reading actually is.
fn format_utc(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Renders ONE plain-English sentence naming the specific cause behind a
/// [`QuiescenceReason`] — this string, not the machine-readable tag it's
/// derived from, is the actual product surface a user reads. Every arm names
/// the concrete cause (which MCP server, what a failed fire's error said,
/// which timestamp) rather than falling back to a generic "unavailable".
///
/// `last_evaluated_at` is the SAME timestamp `AssignmentWatchHealth::last_evaluated_at`
/// carries alongside this string on the response — `AssignmentStore::mark_evaluated`
/// stamps both together in one call, so citing it here as "as of the last
/// check at ..." is never a stale or mismatched read.
fn render_quiescence_explanation(reason: &QuiescenceReason, last_evaluated_at: Option<DateTime<Utc>>) -> String {
    let checked_at = last_evaluated_at.map(format_utc);
    match reason {
        QuiescenceReason::Expired { expires_at } => format!(
            "Not fired: this assignment expired at {} and was disabled instead of evaluated.",
            format_utc(*expires_at)
        ),
        QuiescenceReason::NotDue { next_fire_at } => match next_fire_at {
            Some(next) => format!("Not fired: not due to run again until {}.", format_utc(*next)),
            None => "Not fired: no next run time has been scheduled yet.".to_string(),
        },
        QuiescenceReason::FireFailed { reason: error } => {
            format!("Not fired: the last attempt to fire failed — {error}.")
        }
        QuiescenceReason::ServerNotConnected { server, state } => match (state, checked_at) {
            (Some(state), Some(checked_at)) => format!(
                "Not fired: the {server} MCP server was not connected (state: {state}) as of the last check at {checked_at}."
            ),
            (Some(state), None) => {
                format!("Not fired: the {server} MCP server is not connected (state: {state}).")
            }
            (None, Some(checked_at)) => format!(
                "Not fired: the {server} MCP server has not been connected, as of the last check at {checked_at}."
            ),
            (None, None) => format!("Not fired: the {server} MCP server has not been connected."),
        },
        QuiescenceReason::NoLiveHandle { server } => format!(
            "Not fired: the {server} MCP server reports connected, but no live connection handle is available to poll it right now."
        ),
        QuiescenceReason::PollFailed { server, reason: error } => {
            format!("Not fired: the last poll of the {server} MCP server failed — {error}.")
        }
        QuiescenceReason::CursorUnresolved { server } => format!(
            "Not fired: the {server} MCP server's last response didn't contain the field this assignment tracks for changes, so nothing could be compared."
        ),
        QuiescenceReason::AgentWatchContractNotBound(status) => match status {
            WatchContractStatus::NotYetAttempted => {
                "Not fired: no watch contract has been authored yet for this assignment.".to_string()
            }
            WatchContractStatus::AuthoringRejected { attempts, ceiling_hit, last_rejection_reason } => {
                let base = if *ceiling_hit {
                    format!(
                        "Not fired: contract authoring was rejected {attempts} time{} in a row and has stopped retrying — edit the instruction or connector scope to try again.",
                        if *attempts == 1 { "" } else { "s" }
                    )
                } else {
                    format!(
                        "Not fired: the most recent contract proposal (attempt {attempts}) was rejected — authoring will retry automatically on the next poll."
                    )
                };
                match last_rejection_reason {
                    Some(r) => format!("{base} Last rejection: {r}"),
                    None => base,
                }
            }
            WatchContractStatus::Bound { .. } => {
                "Not fired: this watch's contract is bound, but the last poll observed nothing new to act on.".to_string()
            }
        },
    }
}

/// Looks up `assignment`'s persisted [`AssignmentScratchpad`](ao_protocol::assignment_scratchpad::AssignmentScratchpad)
/// and derives its [`AssignmentWatchHealth`] — the one liveness payload
/// builder for EVERY trigger kind, not just `AgentWatch`. The
/// extraction-tier fields (`tier`/`extraction_path`/`contract_status`/etc.)
/// only ever carry real data for an `AgentWatch` trigger, since no other
/// trigger kind has a scratchpad or extraction-tier concept — for
/// `Cron`/`ConnectorEvent` they fall out at their neutral "nothing to
/// report" values below (`scratchpad` is always `None` for those, and the
/// trigger match two lines down already reads as `AgentWatch`'s own "not yet
/// evaluated" case). The generic liveness fields
/// (`last_evaluated_at`/`fire_count`/`quiescence_reason`/
/// `quiescence_explanation`) are populated from `assignment.liveness` the
/// same way for every trigger kind — see this function's callers, all of
/// which now get a non-`None` payload regardless of `assignment.trigger`.
async fn watch_health_for(state: &Arc<AppState>, assignment: &Assignment) -> AssignmentWatchHealth {
    let scratchpad = if matches!(assignment.trigger, AssignmentTrigger::AgentWatch { .. }) {
        state
            .persistence
            .assignment_scratchpads
            .get(&assignment.id)
            .await
            .ok()
            .flatten()
    } else {
        // No other trigger kind ever writes a scratchpad row under this id —
        // skip the lookup rather than issue a store read that can only ever
        // come back empty.
        None
    };

    // Read off the trigger, not the scratchpad: a contract is bound at
    // authoring time, so a watch that has never completed a poll can still
    // have an expression worth showing. `extraction_tool`/`extraction_configured`
    // are the same two trigger fields `derive_extraction_health` consults —
    // read here rather than passed through the scratchpad, since a manually
    // configured `extraction` override lives on the trigger, not the
    // scratchpad, and `extraction_tool` freezes there too (see `AssignmentTrigger::AgentWatch`).
    let (contract, predicate_expr, extraction_tool, extraction_configured) = match &assignment.trigger {
        AssignmentTrigger::AgentWatch { contract, extraction, extraction_tool, .. } => (
            contract.as_ref(),
            contract.as_ref().and_then(|c| c.predicate.predicate.to_expr()),
            extraction_tool.as_deref(),
            extraction.is_some(),
        ),
        _ => (None, None, None, false),
    };

    let (extraction_health, extraction_health_reason) =
        derive_extraction_health(scratchpad.as_ref(), extraction_tool, extraction_configured);
    let contract_status = derive_watch_contract_status(contract, scratchpad.as_ref());

    // Generic liveness, read straight off `Assignment.liveness` — the same
    // three values regardless of trigger kind. `quiescence_explanation` is
    // rendered here, once, server-side, so every consumer (frontend badge,
    // any future integration) reads identical prose rather than each
    // re-deriving its own sentence from the machine-readable tag.
    let last_evaluated_at = assignment.liveness.last_evaluated_at;
    let fire_count = assignment.liveness.fire_count;
    let quiescence_reason = assignment.liveness.last_quiescence.clone();
    let quiescence_explanation = quiescence_reason
        .as_ref()
        .map(|reason| render_quiescence_explanation(reason, last_evaluated_at));

    match scratchpad {
        None => AssignmentWatchHealth {
            has_evaluated: false,
            tier: None,
            extraction_path: None,
            degraded_reason: None,
            extraction_plan_degraded: false,
            extraction_plan_degraded_reason: None,
            predicate_expr,
            model_calls_by_day: std::collections::BTreeMap::new(),
            consecutive_polls_without_new_items: 0,
            last_new_item_at: None,
            extraction_health,
            extraction_health_reason,
            model_calls_today: 0,
            last_extraction_path: None,
            identity_probe_inconclusive: false,
            identity_probe_inconclusive_reason: None,
            extraction_provenance: None,
            bound_matching_nothing: false,
            bound_matching_nothing_streak: 0,
            contract_status,
            last_evaluated_at,
            fire_count,
            quiescence_reason,
            quiescence_explanation,
        },
        Some(scratchpad) => {
            let degraded_reason = (scratchpad.last_inferred_tier == Some(Tier::ChangeDetectionOnly))
                .then(|| change_detection_reason(&assignment.trigger));
            AssignmentWatchHealth {
                has_evaluated: true,
                tier: scratchpad.last_inferred_tier,
                extraction_path: Some(scratchpad.last_extraction_path),
                degraded_reason,
                extraction_plan_degraded: scratchpad.extraction_plan_degraded,
                extraction_plan_degraded_reason: scratchpad.extraction_plan_degraded_reason.clone(),
                predicate_expr,
                model_calls_by_day: scratchpad.model_calls_by_day.clone(),
                consecutive_polls_without_new_items: scratchpad.consecutive_polls_without_new_items,
                last_new_item_at: scratchpad.last_new_item_at.clone(),
                extraction_health,
                extraction_health_reason,
                model_calls_today: model_calls_today(&scratchpad),
                last_extraction_path: Some(extraction_path_wire_str(scratchpad.last_extraction_path).to_string()),
                identity_probe_inconclusive: scratchpad.identity_probe_inconclusive,
                identity_probe_inconclusive_reason: scratchpad.identity_probe_inconclusive_reason.clone(),
                extraction_provenance: extraction_provenance_wire_str(scratchpad.last_extraction_path)
                    .map(str::to_string),
                bound_matching_nothing: scratchpad.last_poll_observed_candidates > 0
                    && scratchpad.last_poll_surviving_candidates == 0,
                bound_matching_nothing_streak: scratchpad.all_candidates_quarantined_streak,
                contract_status,
                last_evaluated_at,
                fire_count,
                quiescence_reason,
                quiescence_explanation,
            }
        }
    }
}

/// `scratchpad.last_extraction_path`'s serde wire name, for
/// `AssignmentWatchHealth::last_extraction_path` — kept as a plain `String`
/// on that field (rather than the typed `ExtractionPath` `extraction_path`
/// already carries) because the extraction-health UI reads it directly, not
/// through the `WatchExtractionPath` TS union. Matched by hand instead of a
/// `serde_json::to_value` round-trip since there are only four variants and
/// this stays a `&'static str`, not an allocation.
fn extraction_path_wire_str(path: ExtractionPath) -> &'static str {
    match path {
        ExtractionPath::Unbound => "unbound",
        ExtractionPath::Llm => "llm",
        ExtractionPath::Deterministic => "deterministic",
        ExtractionPath::Probabilistic => "probabilistic",
    }
}

/// `AssignmentWatchHealth::extraction_provenance`'s wire value — `Some` only
/// for the two paths that actually resolved a plan with zero model calls
/// (see [`ExtractionPath`]'s own variants): a schema the server declared for
/// this tool ("declared_schema"), or JSON reconstructed by parsing a text
/// response the server never declared a schema for ("parsed_from_text").
///
/// `ExtractionPath::Llm`/`ExtractionPath::Unbound` both map to `None` — a
/// poll that actually ran the model, or one from a watch with no bound
/// contract yet, attests to neither shape. So does an older persisted
/// scratchpad from before `last_extraction_path` existed, which
/// `#[serde(default)]` deserializes to `ExtractionPath::Unbound`: this is
/// exactly what keeps that legacy row from claiming a "declared schema"
/// guarantee it never earned.
fn extraction_provenance_wire_str(path: ExtractionPath) -> Option<&'static str> {
    match path {
        ExtractionPath::Deterministic => Some("declared_schema"),
        ExtractionPath::Probabilistic => Some("parsed_from_text"),
        ExtractionPath::Unbound | ExtractionPath::Llm => None,
    }
}

/// Joins `assignment` with its derived [`AssignmentWatchHealth`] for every
/// route below that returns an assignment over HTTP.
async fn with_watch_health(state: &Arc<AppState>, assignment: Assignment) -> AssignmentResponse {
    let watch_health = Some(watch_health_for(state, &assignment).await);
    AssignmentResponse { assignment, watch_health }
}

#[derive(Debug, Deserialize)]
pub struct CreateAssignmentRequest {
    pub name: String,
    pub instruction: String,
    #[serde(default)]
    pub working_directory: Option<String>,
    pub trigger: AssignmentTrigger,
    #[serde(default)]
    pub bindings: Vec<AssignmentBinding>,
    #[serde(default)]
    pub output_mode: OutputMode,
    /// Where this assignment's runs land. When omitted, the default is
    /// resolved from `trigger` (see [`default_thread_policy_for_trigger`]):
    /// `Cron` → `Main` (matches the pre-Assignment scheduled-task feel),
    /// `Webhook` → `Fresh` (safe for autonomous runs). `dedicated_thread_id`
    /// is never client-settable: it's server-managed, claimed automatically
    /// on first fire when `thread_policy` is `Dedicated`.
    #[serde(default)]
    pub thread_policy: Option<AssignmentThreadPolicy>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct PatchAssignmentRequest {
    pub name: Option<String>,
    pub instruction: Option<String>,
    pub working_directory: Option<String>,
    /// Full trigger replacement when present. Absent fields are unchanged.
    pub trigger: Option<AssignmentTrigger>,
    pub bindings: Option<Vec<AssignmentBinding>>,
    pub output_mode: Option<OutputMode>,
    pub thread_policy: Option<AssignmentThreadPolicy>,
    pub enabled: Option<bool>,
    pub expires_at: Option<chrono::DateTime<Utc>>,
}

fn default_true() -> bool {
    true
}

/// Trigger-dependent default for `thread_policy` when the create request
/// omits it (decision #1 in the Assignments Convergence plan): `Cron`
/// matches the reminder feel scheduled tasks had (posts into the main
/// thread), while `Webhook` defaults to a disposable thread since inbound
/// callers are untrusted and a fire should never interrupt live chat.
fn default_thread_policy_for_trigger(trigger: &AssignmentTrigger) -> AssignmentThreadPolicy {
    match trigger {
        AssignmentTrigger::Cron { .. } => AssignmentThreadPolicy::Main,
        AssignmentTrigger::Webhook { .. } => AssignmentThreadPolicy::Fresh,
        AssignmentTrigger::ConnectorEvent { .. } => AssignmentThreadPolicy::Fresh,
        AssignmentTrigger::AgentWatch { .. } => AssignmentThreadPolicy::Fresh,
    }
}

/// Read the user's IANA timezone from preferences, used for cron next-fire computation.
pub(crate) async fn user_timezone(state: &Arc<AppState>) -> Option<String> {
    state
        .persistence
        .preferences
        .get()
        .await
        .ok()
        .flatten()
        .and_then(|p| p.timezone)
}

/// Validate a cron expression and return `ValidationError` on failure.
fn validate_cron(expr: &str) -> Result<(), AppError> {
    Cron::from_str(expr)
        .map(|_| ())
        .map_err(|e| AppError(AoError::ValidationError(format!("Invalid cron expression: {e}"))))
}

/// `GET /agents/{agent_id}/assignments` — list all assignments for an agent.
pub async fn list_agent_assignments(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<AssignmentResponse>>, AppError> {
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let assignments = state.persistence.assignments.list_for_agent(&agent_id).await;
    let mut responses = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        responses.push(with_watch_health(&state, assignment).await);
    }
    Ok(Json(responses))
}

/// `POST /agents/{agent_id}/assignments` — create a new assignment.
pub async fn create_assignment(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<CreateAssignmentRequest>,
) -> Result<Json<AssignmentResponse>, AppError> {
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    req.trigger
        .validate()
        .map_err(|e| AppError(AoError::ValidationError(e)))?;

    state
        .persistence
        .assignments
        .enforce_agent_watch_cap(&agent_id, &req.trigger, req.enabled, false)
        .await?;

    let tz = user_timezone(&state).await;

    let next_fire_at = match &req.trigger {
        AssignmentTrigger::Cron { cron_expr, .. } => {
            validate_cron(cron_expr)?;
            compute_next_fire_at(Some(cron_expr), tz.as_deref())
        }
        AssignmentTrigger::Webhook { .. } => None,
        // Poll ASAP after creation so the first tick seeds the dedup
        // baseline instead of waiting a full `poll_interval_secs` (mirrors
        // the `AssignmentCreate` tool path).
        AssignmentTrigger::ConnectorEvent { .. } => Some(Utc::now()),
        // Poll ASAP so the first tick seeds the dedup scratchpad's baseline
        // instead of waiting a full `poll_interval_secs`, matching
        // `ConnectorEvent`'s seed-on-first convention.
        AssignmentTrigger::AgentWatch { .. } => Some(Utc::now()),
    };

    let thread_policy = req
        .thread_policy
        .unwrap_or_else(|| default_thread_policy_for_trigger(&req.trigger));

    let now = Utc::now();
    let assignment = Assignment {
        id: Uuid::new_v4().to_string(),
        agent_id: agent_id.clone(),
        name: req.name,
        instruction: req.instruction,
        working_directory: req.working_directory,
        trigger: req.trigger,
        bindings: req.bindings,
        output_mode: req.output_mode,
        thread_policy,
        dedicated_thread_id: None,
        enabled: req.enabled,
        expires_at: req.expires_at,
        next_fire_at,
        last_run_at: None,
        last_event_cursor: None,
        liveness: ao_protocol::assignment::LivenessState::default(),
        created_ts: now,
        updated_ts: now,
    };

    state.persistence.assignments.add(assignment.clone()).await?;
    Ok(Json(with_watch_health(&state, assignment).await))
}

/// `GET /assignments/{assignment_id}` — fetch one assignment by id.
pub async fn get_assignment(
    State(state): State<Arc<AppState>>,
    Path(assignment_id): Path<String>,
) -> Result<Json<AssignmentResponse>, AppError> {
    let assignment = state
        .persistence
        .assignments
        .get(&assignment_id)
        .await
        .ok_or_else(|| AoError::AssignmentNotFound(assignment_id.clone()))?;
    Ok(Json(with_watch_health(&state, assignment).await))
}

/// `PATCH /assignments/{assignment_id}` — update mutable fields.
///
/// Absent fields in the request body are left unchanged. When the trigger
/// changes, the cron expression is re-validated and `next_fire_at` is
/// recomputed (or cleared for Webhook triggers). `updated_ts` is always bumped.
pub async fn patch_assignment(
    State(state): State<Arc<AppState>>,
    Path(assignment_id): Path<String>,
    Json(req): Json<PatchAssignmentRequest>,
) -> Result<Json<AssignmentResponse>, AppError> {
    let mut assignment = state
        .persistence
        .assignments
        .get(&assignment_id)
        .await
        .ok_or_else(|| AoError::AssignmentNotFound(assignment_id.clone()))?;

    let was_already_active_agent_watch =
        assignment.enabled && matches!(assignment.trigger, AssignmentTrigger::AgentWatch { .. });

    if let Some(name) = req.name {
        assignment.name = name;
    }
    if let Some(instruction) = req.instruction {
        assignment.instruction = instruction;
    }
    if let Some(working_directory) = req.working_directory {
        assignment.working_directory = Some(working_directory);
    }
    if let Some(trigger) = req.trigger {
        trigger
            .validate()
            .map_err(|e| AppError(AoError::ValidationError(e)))?;
        let tz = user_timezone(&state).await;
        match &trigger {
            AssignmentTrigger::Cron { cron_expr, .. } => {
                validate_cron(cron_expr)?;
                assignment.next_fire_at = compute_next_fire_at(Some(cron_expr), tz.as_deref());
            }
            AssignmentTrigger::Webhook { .. } => {
                assignment.next_fire_at = None;
            }
            // Poll ASAP after the trigger changes so the first tick seeds
            // the dedup baseline instead of waiting a full
            // `poll_interval_secs` (mirrors the create path).
            AssignmentTrigger::ConnectorEvent { .. } => {
                assignment.next_fire_at = Some(Utc::now());
            }
            AssignmentTrigger::AgentWatch { .. } => {
                assignment.next_fire_at = Some(Utc::now());
            }
        }
        let (trigger, cleared_reason) = carry_forward_watch_contract(&assignment.trigger, trigger);
        if let Some(reason) = cleared_reason {
            info!(
                assignment_id = %assignment_id,
                reason,
                "agent watch: clearing watch contract on update"
            );
            // The other half of `carry_forward_watch_contract`'s clear: that
            // function only decides the NEW trigger's `contract` slot, since
            // it has no persistence access of its own — without this, the
            // OLD contract's scratchpad (snapshots, authoring streak,
            // extraction plan, convergence note) keeps answering
            // `AssignmentWatchHealth` queries for the now contract-less
            // trigger until the next poll happens to overwrite it. See
            // `AssignmentScratchpad::invalidate_watch_contract_state`'s doc.
            if let Some(mut scratchpad) =
                state.persistence.assignment_scratchpads.get(&assignment_id).await.ok().flatten()
            {
                scratchpad.invalidate_watch_contract_state();
                if let Err(e) = state.persistence.assignment_scratchpads.set(&assignment_id, &scratchpad).await {
                    warn!(
                        assignment_id = %assignment_id,
                        error = %e,
                        "agent watch: failed to invalidate the watch contract scratchpad state after a clearing edit"
                    );
                }
            }
        }
        assignment.trigger = trigger;
    }
    if let Some(bindings) = req.bindings {
        assignment.bindings = bindings;
    }
    if let Some(output_mode) = req.output_mode {
        assignment.output_mode = output_mode;
    }
    if let Some(thread_policy) = req.thread_policy {
        // `dedicated_thread_id` is deliberately left untouched here even
        // when switching away from `Dedicated` — it's inert unless
        // `thread_policy` is `Dedicated`, and preserving it means switching
        // back later reuses the same thread instead of silently detaching
        // from it.
        assignment.thread_policy = thread_policy;
    }
    if let Some(enabled) = req.enabled {
        assignment.enabled = enabled;
    }
    if let Some(expires_at) = req.expires_at {
        assignment.expires_at = Some(expires_at);
    }

    state
        .persistence
        .assignments
        .enforce_agent_watch_cap(
            &assignment.agent_id,
            &assignment.trigger,
            assignment.enabled,
            was_already_active_agent_watch,
        )
        .await?;

    assignment.updated_ts = Utc::now();
    state.persistence.assignments.update(assignment.clone()).await?;
    Ok(Json(with_watch_health(&state, assignment).await))
}

/// `DELETE /assignments/{assignment_id}` — drop the assignment row.
///
/// Run history (the JSONL file) is intentionally preserved. Returns 204 on
/// success, 404 when no row with that id exists.
pub async fn delete_assignment(
    State(state): State<Arc<AppState>>,
    Path(assignment_id): Path<String>,
) -> Result<StatusCode, AppError> {
    state
        .persistence
        .assignments
        .get(&assignment_id)
        .await
        .ok_or_else(|| AoError::AssignmentNotFound(assignment_id.clone()))?;

    state.persistence.assignments.remove(&assignment_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /assignments/{assignment_id}/runs` — run history, newest first.
pub async fn list_assignment_runs(
    State(state): State<Arc<AppState>>,
    Path(assignment_id): Path<String>,
) -> Result<Json<Vec<AssignmentRun>>, AppError> {
    state
        .persistence
        .assignments
        .get(&assignment_id)
        .await
        .ok_or_else(|| AoError::AssignmentNotFound(assignment_id.clone()))?;

    let mut runs = state
        .persistence
        .assignment_runs
        .list_for_assignment(&assignment_id)
        .await?;

    // Newest first per the frontend contract.
    runs.sort_by(|a, b| b.queued_at.cmp(&a.queued_at));
    Ok(Json(runs))
}

#[derive(Debug, Deserialize, Default)]
pub struct TriggerAssignmentRequest {
    pub token: Option<String>,
    pub payload_summary: Option<String>,
    /// Structured event data for this manual fire, when the caller has it
    /// (e.g. a captured sample payload replayed for testing). When present,
    /// rendered against the trigger's `prompt_template` (if set) and carried
    /// through as a real [`TriggerEventContext`] — this is the same event
    /// payload plumbing the named-route gateway uses, so a manual trigger
    /// with a payload behaves identically to an inbound POST carrying it.
    /// `None` (the common case for a plain "fire now" click) still yields
    /// the bare static-instruction fire this endpoint has always produced.
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

/// `POST /assignments/{assignment_id}/trigger` — fire an assignment immediately.
///
/// Validates the assignment, checks the webhook token when configured, then
/// delegates to the shared `fire_assignment` helper which creates a thread,
/// persists a run row, and enqueues a non-interactive message on the agent's
/// queue manager. Returns 202 with the queued [`AssignmentRun`] row.
pub async fn trigger_assignment(
    State(state): State<Arc<AppState>>,
    Path(assignment_id): Path<String>,
    body: Option<Json<TriggerAssignmentRequest>>,
) -> Result<(StatusCode, Json<AssignmentRun>), AppError> {
    let assignment = state
        .persistence
        .assignments
        .get(&assignment_id)
        .await
        .ok_or_else(|| AoError::AssignmentNotFound(assignment_id.clone()))?;

    if !assignment.enabled {
        return Err(AppError(AoError::Conflict(
            "assignment is disabled".to_string(),
        )));
    }

    let req = body.map(|Json(r)| r).unwrap_or_default();

    // Token check — only applies to Webhook triggers with a stored token.
    if let AssignmentTrigger::Webhook { token: Some(stored_token), .. } = &assignment.trigger {
        let provided = req.token.as_deref().unwrap_or("");
        if provided != stored_token {
            return Err(AppError(AoError::Unauthorized(
                "invalid assignment token".to_string(),
            )));
        }
    }

    let tz = user_timezone(&state).await;
    let dispatcher = Arc::clone(&state.queue_managers) as Arc<dyn NotificationDispatcher>;

    // When the caller supplies a real event payload, render it through the
    // trigger's prompt_template (falling back to the static instruction when
    // unset, same as the gateway) and carry the payload itself through
    // TriggerEventContext instead of the bare `event_context: None` this
    // endpoint used to hardcode.
    let mut fire_target = assignment.clone();
    if let Some(payload) = &req.payload {
        if let AssignmentTrigger::Webhook { prompt_template: Some(tpl), .. } = &assignment.trigger {
            fire_target.instruction = render_prompt_template(tpl, payload);
        }
    }
    let event_context = req.payload.as_ref().map(|payload| TriggerEventContext {
        summary: format!("Manually triggered event payload for assignment `{}`", assignment.id),
        payload: payload.clone(),
    });

    let run = fire_assignment(
        &state.persistence,
        &dispatcher,
        &state.event_bus,
        &fire_target,
        AssignmentTriggerKind::Webhook,
        req.payload_summary
            .map(|s| s.chars().take(500).collect::<String>()),
        tz.as_deref(),
        event_context,
    )
    .await?;

    Ok((StatusCode::ACCEPTED, Json(run)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ao_persistence::PersistenceLayer;
    use ao_process::mock::MockProcessSupervisor;
    use ao_protocol::assignment::{ConnectorPollSpec, LivenessState, OutputMode};
    use ao_protocol::watch_contract::{
        ChangeSpec, IdentitySpec, IdentityStrategy, PredicateSpec, WatchContract, WatchMode,
        WatchSource,
    };
    use std::collections::HashMap;

    async fn setup_state() -> (Arc<AppState>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("Failed to create temp dir");
        let state = {
            let _guard = crate::routes::env_lock::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var("LAUNCHPAD_STUDIO_DATA_DIR", tmp.path());
            let _ = PersistenceLayer::init();
            let mock = MockProcessSupervisor::new(vec![]);
            AppState::new_with_mock(mock)
                .await
                .expect("AppState init")
        };
        (Arc::new(state), tmp)
    }

    fn sample_watch_contract() -> WatchContract {
        WatchContract {
            contract_version: 1,
            authored_at: "2026-07-27T09:00:00Z".to_string(),
            authored_by_run: "run-1".to_string(),
            source: WatchSource {
                kind: "example".to_string(),
                ref_: "abc-123".to_string(),
            },
            identity: IdentitySpec {
                strategy: IdentityStrategy::NativeId,
                source_field: Some("unique_identifier".to_string()),
                format: None,
                fields: vec![],
                rationale: "test fixture".to_string(),
            },
            change: ChangeSpec {
                material_fields: vec!["status".to_string()],
                version_hint_field: None,
            },
            predicate: PredicateSpec {
                natural_language: "always fires".to_string(),
                fields: vec![],
                // Vacuously true (an empty `And`) — the legacy grammar had
                // no bare boolean literal, so this is the typed equivalent
                // of what this fixture always meant.
                predicate: ao_protocol::predicate::Predicate::And(vec![]),
            },
            mode: WatchMode::PredicateTransition,
            fields: HashMap::new(),
        }
    }

    fn sample_agent_watch_assignment(id: &str) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: "agent-1".to_string(),
            name: "Watch for finance emails".to_string(),
            instruction: "Summarize the new email from finance.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::AgentWatch {
                instruction: "Check my inbox for a new email from finance".to_string(),
                poll_interval_secs: 900,
                connector_scope: Some("gmail".to_string()),
                contract: Some(sample_watch_contract()),
                extraction: None,
                extraction_tool: None,
                extraction_args: None,
                extraction_output_schema_declared: false,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: Some(now),
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    /// Same shape as an assignment `AssignmentTrigger::Cron` create request
    /// would produce — for the liveness-payload tests below, which need a
    /// non-`AgentWatch` trigger to prove `watch_health_for` now covers every
    /// trigger kind, not just `AgentWatch`.
    fn sample_cron_assignment(id: &str) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: "agent-1".to_string(),
            name: "Morning digest".to_string(),
            instruction: "Summarize overnight activity.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Cron { cron_expr: "0 9 * * *".to_string(), is_recurring: true },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: Some(now),
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    /// Same shape as an assignment `AssignmentTrigger::ConnectorEvent` create
    /// request would produce — for the liveness-payload tests below.
    fn sample_connector_event_assignment(id: &str) -> Assignment {
        let now = Utc::now();
        Assignment {
            id: id.to_string(),
            agent_id: "agent-1".to_string(),
            name: "New Notion pages".to_string(),
            instruction: "Summarize any new page.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::ConnectorEvent {
                server_name: "notion".to_string(),
                poll: ConnectorPollSpec {
                    tool_name: "list_pages".to_string(),
                    arguments: serde_json::Value::Object(serde_json::Map::new()),
                    cursor_path: Some("content.0.text".to_string()),
                },
                poll_interval_secs: 300,
            },
            bindings: vec![],
            output_mode: OutputMode::Background,
            thread_policy: AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: Some(now),
            last_run_at: None,
            last_event_cursor: None,
            liveness: ao_protocol::assignment::LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn unwrap_ok<T>(r: Result<T, AppError>) -> T {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok, got error: {:?}", e.0),
        }
    }

    fn empty_patch() -> PatchAssignmentRequest {
        PatchAssignmentRequest {
            name: None,
            instruction: None,
            working_directory: None,
            trigger: None,
            bindings: None,
            output_mode: None,
            thread_policy: None,
            enabled: None,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn patch_only_poll_interval_preserves_watch_contract() {
        let (state, _tmp) = setup_state().await;
        let assignment = sample_agent_watch_assignment("watch-1");
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut patch = empty_patch();
        patch.trigger = Some(AssignmentTrigger::AgentWatch {
            instruction: "Check my inbox for a new email from finance".to_string(),
            poll_interval_secs: 1800,
            connector_scope: Some("gmail".to_string()),
            contract: None,
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        });

        let Json(updated) = unwrap_ok(
            patch_assignment(
                State(Arc::clone(&state)),
                Path("watch-1".to_string()),
                Json(patch),
            )
            .await,
        );

        match updated.assignment.trigger {
            AssignmentTrigger::AgentWatch {
                poll_interval_secs,
                contract,
                ..
            } => {
                assert_eq!(poll_interval_secs, 1800);
                assert!(contract.is_some(), "contract must be preserved when instruction/connector_scope are unchanged");
            }
            other => panic!("expected AgentWatch trigger, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn patch_changing_instruction_clears_watch_contract() {
        let (state, _tmp) = setup_state().await;
        let assignment = sample_agent_watch_assignment("watch-2");
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut patch = empty_patch();
        patch.trigger = Some(AssignmentTrigger::AgentWatch {
            instruction: "Check my inbox for a new email from legal".to_string(),
            poll_interval_secs: 900,
            connector_scope: Some("gmail".to_string()),
            contract: None,
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        });

        let Json(updated) = unwrap_ok(
            patch_assignment(
                State(Arc::clone(&state)),
                Path("watch-2".to_string()),
                Json(patch),
            )
            .await,
        );

        match updated.assignment.trigger {
            AssignmentTrigger::AgentWatch { contract, .. } => {
                assert!(contract.is_none(), "contract must be cleared when instruction changes");
            }
            other => panic!("expected AgentWatch trigger, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn patch_changing_connector_scope_clears_watch_contract() {
        let (state, _tmp) = setup_state().await;
        let assignment = sample_agent_watch_assignment("watch-3");
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let mut patch = empty_patch();
        patch.trigger = Some(AssignmentTrigger::AgentWatch {
            instruction: "Check my inbox for a new email from finance".to_string(),
            poll_interval_secs: 900,
            connector_scope: Some("outlook".to_string()),
            contract: None,
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        });

        let Json(updated) = unwrap_ok(
            patch_assignment(
                State(Arc::clone(&state)),
                Path("watch-3".to_string()),
                Json(patch),
            )
            .await,
        );

        match updated.assignment.trigger {
            AssignmentTrigger::AgentWatch { contract, .. } => {
                assert!(contract.is_none(), "contract must be cleared when connector_scope changes");
            }
            other => panic!("expected AgentWatch trigger, got {:?}", other),
        }
    }

    /// FIX 1c regression: a native_id contract bound off an inconclusive
    /// stability probe (`ao_engine::agent_watch::probe_identity_stability`)
    /// must surface as such through the same derived-health path the UI
    /// already reads `extraction_plan_degraded`/`extraction_plan_degraded_reason`
    /// through — never silently indistinguishable from a probe that actually
    /// confirmed stability.
    #[tokio::test]
    async fn watch_health_surfaces_an_inconclusive_identity_probe() {
        use ao_protocol::assignment_scratchpad::AssignmentScratchpad;

        let (state, _tmp) = setup_state().await;
        let assignment = sample_agent_watch_assignment("watch-inconclusive-1");
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let scratchpad = AssignmentScratchpad {
            identity_probe_inconclusive: true,
            identity_probe_inconclusive_reason: Some(
                "the probe's two polls shared no candidate that reported a value for `url`".to_string(),
            ),
            ..Default::default()
        };
        state.persistence.assignment_scratchpads.set("watch-inconclusive-1", &scratchpad).await.unwrap();

        let Json(response) = unwrap_ok(
            get_assignment(State(Arc::clone(&state)), Path("watch-inconclusive-1".to_string())).await,
        );

        let watch_health = response.watch_health.expect("an AgentWatch trigger must report watch_health");
        assert!(watch_health.identity_probe_inconclusive, "the inconclusive probe outcome must be surfaced");
        assert_eq!(
            watch_health.identity_probe_inconclusive_reason.as_deref(),
            Some("the probe's two polls shared no candidate that reported a value for `url`"),
            "the reason must be relayed verbatim, not replaced with a placeholder"
        );
    }

    /// Sibling to the above: a watch whose probe DID confirm stability (the
    /// default, unset scratchpad state) must read as verified, not merely
    /// "not yet flagged" — the two states must be visibly different, not
    /// just internally different.
    #[tokio::test]
    async fn watch_health_does_not_flag_a_watch_with_no_recorded_probe_outcome() {
        use ao_protocol::assignment_scratchpad::AssignmentScratchpad;

        let (state, _tmp) = setup_state().await;
        let assignment = sample_agent_watch_assignment("watch-verified-1");
        state.persistence.assignments.add(assignment.clone()).await.unwrap();
        state
            .persistence
            .assignment_scratchpads
            .set("watch-verified-1", &AssignmentScratchpad::default())
            .await
            .unwrap();

        let Json(response) =
            unwrap_ok(get_assignment(State(Arc::clone(&state)), Path("watch-verified-1".to_string())).await);

        let watch_health = response.watch_health.expect("an AgentWatch trigger must report watch_health");
        assert!(!watch_health.identity_probe_inconclusive);
        assert_eq!(watch_health.identity_probe_inconclusive_reason, None);
    }

    fn sample_extraction_plan() -> ao_protocol::extractor_contract::ExtractionPlan {
        ao_protocol::extractor_contract::ExtractionPlan {
            selector: ao_protocol::extractor_contract::Selector {
                kind: ExtractorKind::JsonPath { path: "items".to_string() },
                expr: "items".to_string(),
            },
            identity: ExtractorKind::JsonPath { path: "id".to_string() },
            predicate: ao_protocol::predicate::Predicate::And(vec![]),
        }
    }

    /// A plan resolved against content the server both returned as
    /// structured data and declared a schema for must surface as
    /// "declared_schema" — the strongest provenance claim this field can
    /// make, and the only one a `FrozenContractDisclosure`'s "no model
    /// reviews this before it runs" claim should be read as implying.
    #[tokio::test]
    async fn watch_health_reports_declared_schema_provenance_for_a_deterministic_extraction_path() {
        use ao_protocol::assignment_scratchpad::AssignmentScratchpad;

        let (state, _tmp) = setup_state().await;
        let assignment = sample_agent_watch_assignment("watch-provenance-declared");
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let scratchpad = AssignmentScratchpad {
            extraction_plan: Some(sample_extraction_plan()),
            last_extraction_path: ExtractionPath::Deterministic,
            ..Default::default()
        };
        state.persistence.assignment_scratchpads.set("watch-provenance-declared", &scratchpad).await.unwrap();

        let Json(response) = unwrap_ok(
            get_assignment(State(Arc::clone(&state)), Path("watch-provenance-declared".to_string())).await,
        );

        let watch_health = response.watch_health.expect("an AgentWatch trigger must report watch_health");
        assert_eq!(watch_health.extraction_health, ExtractionHealth::Deterministic);
        assert_eq!(watch_health.extraction_provenance.as_deref(), Some("declared_schema"));
    }

    /// A plan resolved by parsing JSON out of a text block — no server
    /// schema promise behind it — must surface as "parsed_from_text", never
    /// the same "declared_schema" value a real schema-backed plan reports,
    /// even though both share the same `extraction_health: Deterministic`
    /// (zero model calls) and the same frozen-contract disclosure.
    #[tokio::test]
    async fn watch_health_reports_parsed_from_text_provenance_for_a_probabilistic_extraction_path() {
        use ao_protocol::assignment_scratchpad::AssignmentScratchpad;

        let (state, _tmp) = setup_state().await;
        let assignment = sample_agent_watch_assignment("watch-provenance-parsed");
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let scratchpad = AssignmentScratchpad {
            extraction_plan: Some(sample_extraction_plan()),
            last_extraction_path: ExtractionPath::Probabilistic,
            ..Default::default()
        };
        state.persistence.assignment_scratchpads.set("watch-provenance-parsed", &scratchpad).await.unwrap();

        let Json(response) = unwrap_ok(
            get_assignment(State(Arc::clone(&state)), Path("watch-provenance-parsed".to_string())).await,
        );

        let watch_health = response.watch_health.expect("an AgentWatch trigger must report watch_health");
        assert_eq!(watch_health.extraction_health, ExtractionHealth::Deterministic);
        assert_eq!(watch_health.extraction_provenance.as_deref(), Some("parsed_from_text"));
        assert_ne!(
            watch_health.extraction_provenance.as_deref(),
            Some("declared_schema"),
            "a parsed-text plan must never claim the same guarantee a declared-schema plan does"
        );
    }

    /// Regression guard for a scratchpad persisted before
    /// `last_extraction_path` existed: it deserializes to
    /// `ExtractionPath::Unbound` via `#[serde(default)]`, even though a
    /// plan is bound and `extraction_health` still reads `Deterministic`.
    /// `extraction_provenance` must fall back to a neutral `None` here,
    /// never `Some("declared_schema")` — that guarantee was never actually
    /// recorded for this row.
    #[tokio::test]
    async fn watch_health_falls_back_to_neutral_provenance_when_last_extraction_path_was_never_recorded() {
        use ao_protocol::assignment_scratchpad::AssignmentScratchpad;

        let (state, _tmp) = setup_state().await;
        let assignment = sample_agent_watch_assignment("watch-provenance-legacy");
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let scratchpad = AssignmentScratchpad {
            extraction_plan: Some(sample_extraction_plan()),
            // `last_extraction_path` deliberately left at its `Default`
            // (`ExtractionPath::Unbound`) — simulating a row persisted
            // before the field existed.
            ..Default::default()
        };
        state.persistence.assignment_scratchpads.set("watch-provenance-legacy", &scratchpad).await.unwrap();

        let Json(response) = unwrap_ok(
            get_assignment(State(Arc::clone(&state)), Path("watch-provenance-legacy".to_string())).await,
        );

        let watch_health = response.watch_health.expect("an AgentWatch trigger must report watch_health");
        assert_eq!(
            watch_health.extraction_health,
            ExtractionHealth::Deterministic,
            "a bound plan alone is still enough for extraction_health, independent of provenance"
        );
        assert_eq!(
            watch_health.extraction_provenance, None,
            "no recorded extraction path must never be read as a declared-schema guarantee"
        );
    }

    // -----------------------------------------------------------------
    // Liveness contract: `watch_health_for`/`AssignmentWatchHealth` now
    // cover every trigger kind, not just `AgentWatch` — these tests exercise
    // the full `get_assignment` route (same as the extraction-tier tests
    // above) for one `Cron`, one `ConnectorEvent`, and one `AgentWatch`
    // assignment, plus a dedicated never-evaluated case and a direct
    // per-variant check of `render_quiescence_explanation`'s wording.
    // -----------------------------------------------------------------

    /// A brand-new `Cron` assignment that has never been ticked must report
    /// `last_evaluated_at: None` — never a `Some` default it doesn't own —
    /// and no `quiescence_reason`/`quiescence_explanation`. This is the
    /// "never evaluated" state that must render distinctly from "evaluated,
    /// correctly chose not to fire" (below): collapsing the two is the exact
    /// bug class this field exists to eliminate.
    #[tokio::test]
    async fn watch_health_for_never_evaluated_cron_assignment_reports_no_liveness_data() {
        let (state, _tmp) = setup_state().await;
        let assignment = sample_cron_assignment("cron-never-evaluated");
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let Json(response) =
            unwrap_ok(get_assignment(State(Arc::clone(&state)), Path("cron-never-evaluated".to_string())).await);

        let health = response.watch_health.expect("every trigger kind must now report a liveness payload");
        assert_eq!(health.last_evaluated_at, None);
        assert_eq!(health.fire_count, 0);
        assert_eq!(health.quiescence_reason, None);
        assert_eq!(
            health.quiescence_explanation, None,
            "a never-evaluated assignment must never carry an explanation string — there is nothing to explain yet"
        );
        // AgentWatch-specific fields still fall out at their neutral
        // "nothing to report" defaults for a non-AgentWatch trigger.
        assert!(!health.has_evaluated);
        assert_eq!(health.tier, None);
    }

    /// A `Cron` assignment that HAS been evaluated but is not yet due must
    /// report `last_evaluated_at: Some(_)` alongside a populated
    /// `quiescence_reason`/`quiescence_explanation` — the "evaluated,
    /// correctly chose not to fire" state, visually and structurally
    /// distinct from the never-evaluated case above (`last_evaluated_at`
    /// alone tells them apart; a client must never conflate the two).
    #[tokio::test]
    async fn watch_health_for_cron_assignment_reports_quiescence_reason_and_explanation() {
        let (state, _tmp) = setup_state().await;
        let mut assignment = sample_cron_assignment("cron-not-due");
        let evaluated_at = Utc::now();
        let next_fire_at = evaluated_at + chrono::Duration::hours(2);
        assignment.liveness = LivenessState {
            last_evaluated_at: Some(evaluated_at),
            fire_count: 1,
            last_quiescence: Some(QuiescenceReason::NotDue { next_fire_at: Some(next_fire_at) }),
        };
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let Json(response) =
            unwrap_ok(get_assignment(State(Arc::clone(&state)), Path("cron-not-due".to_string())).await);

        let health = response.watch_health.expect("every trigger kind must now report a liveness payload");
        assert_eq!(health.last_evaluated_at, Some(evaluated_at));
        assert_eq!(health.fire_count, 1);
        assert_eq!(health.quiescence_reason, Some(QuiescenceReason::NotDue { next_fire_at: Some(next_fire_at) }));
        assert_eq!(
            health.quiescence_explanation.as_deref(),
            Some(format!("Not fired: not due to run again until {}.", format_utc(next_fire_at)).as_str())
        );
    }

    /// A `ConnectorEvent` assignment whose backing MCP server is not
    /// connected must surface `ServerNotConnected` verbatim, with a
    /// human-readable sentence naming the specific server rather than a
    /// generic "unavailable" string — matches this task's own worked
    /// example wording.
    #[tokio::test]
    async fn watch_health_for_connector_event_assignment_reports_server_not_connected() {
        let (state, _tmp) = setup_state().await;
        let mut assignment = sample_connector_event_assignment("connector-not-connected");
        let evaluated_at = Utc::now();
        assignment.liveness = LivenessState {
            last_evaluated_at: Some(evaluated_at),
            fire_count: 0,
            last_quiescence: Some(QuiescenceReason::ServerNotConnected {
                server: "notion".to_string(),
                state: Some("Disconnected".to_string()),
            }),
        };
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let Json(response) = unwrap_ok(
            get_assignment(State(Arc::clone(&state)), Path("connector-not-connected".to_string())).await,
        );

        let health = response.watch_health.expect("every trigger kind must now report a liveness payload");
        assert_eq!(
            health.quiescence_reason,
            Some(QuiescenceReason::ServerNotConnected { server: "notion".to_string(), state: Some("Disconnected".to_string()) })
        );
        let explanation = health.quiescence_explanation.expect("a quiescent tick must always carry an explanation");
        assert!(explanation.contains("notion"), "explanation must name the specific server: {explanation}");
        assert!(
            !explanation.to_lowercase().contains("unavailable"),
            "explanation must name the specific cause, not a generic 'unavailable': {explanation}"
        );
    }

    /// A `ConnectorEvent` assignment whose most recent tick FIRED must
    /// report the incremented `fire_count` and a cleared `quiescence_reason`
    /// — the "fired recently" state, distinct from both the never-evaluated
    /// and evaluated-not-fired cases above.
    #[tokio::test]
    async fn watch_health_for_connector_event_assignment_reports_fire_count_and_clears_quiescence() {
        let (state, _tmp) = setup_state().await;
        let mut assignment = sample_connector_event_assignment("connector-fired");
        let evaluated_at = Utc::now();
        assignment.liveness = LivenessState {
            last_evaluated_at: Some(evaluated_at),
            fire_count: 3,
            last_quiescence: None,
        };
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let Json(response) =
            unwrap_ok(get_assignment(State(Arc::clone(&state)), Path("connector-fired".to_string())).await);

        let health = response.watch_health.expect("every trigger kind must now report a liveness payload");
        assert_eq!(health.last_evaluated_at, Some(evaluated_at));
        assert_eq!(health.fire_count, 3);
        assert_eq!(health.quiescence_reason, None);
        assert_eq!(
            health.quiescence_explanation, None,
            "a most-recent fire must clear the explanation alongside the reason — a stale 'why not fired' sentence must never survive a fire"
        );
    }

    /// An `AgentWatch` assignment must keep reporting its existing
    /// extraction-tier fields exactly as before (regression guard for "do
    /// not rename or remove"), while ALSO now carrying the generic liveness
    /// fields, wrapping `WatchContractStatus` inside `QuiescenceReason` for
    /// its human-readable sentence.
    #[tokio::test]
    async fn watch_health_for_agent_watch_assignment_preserves_existing_fields_and_adds_liveness() {
        let (state, _tmp) = setup_state().await;
        let mut assignment = sample_agent_watch_assignment("watch-liveness-1");
        // No contract authored yet, so this fixture's own derived
        // `contract_status` (`NotYetAttempted`) is consistent with the
        // `AgentWatchContractNotBound(AuthoringRejected { .. })` liveness
        // reason set below — a watch whose authoring is actively being
        // rejected, not one that already has a bound contract.
        if let AssignmentTrigger::AgentWatch { contract, .. } = &mut assignment.trigger {
            *contract = None;
        }
        let evaluated_at = Utc::now();
        assignment.liveness = LivenessState {
            last_evaluated_at: Some(evaluated_at),
            fire_count: 0,
            last_quiescence: Some(QuiescenceReason::AgentWatchContractNotBound(WatchContractStatus::AuthoringRejected {
                attempts: 2,
                ceiling_hit: false,
                last_rejection_reason: Some("no stable identity field found".to_string()),
            })),
        };
        state.persistence.assignments.add(assignment.clone()).await.unwrap();

        let Json(response) =
            unwrap_ok(get_assignment(State(Arc::clone(&state)), Path("watch-liveness-1".to_string())).await);

        let health = response.watch_health.expect("an AgentWatch trigger must report watch_health");
        // Pre-existing AgentWatch-specific fields: unchanged behavior — no
        // scratchpad was ever written for this assignment, so `has_evaluated`/
        // `tier`/`contract_status` still read the same "never polled, no
        // contract" branch as before this task.
        assert!(!health.has_evaluated);
        assert_eq!(health.tier, None);
        assert_eq!(health.contract_status, WatchContractStatus::NotYetAttempted);
        // New generic liveness fields, populated alongside the above.
        assert_eq!(health.last_evaluated_at, Some(evaluated_at));
        assert_eq!(health.fire_count, 0);
        assert!(matches!(
            health.quiescence_reason,
            Some(QuiescenceReason::AgentWatchContractNotBound(WatchContractStatus::AuthoringRejected { attempts: 2, .. }))
        ));
        let explanation = health.quiescence_explanation.expect("a quiescent tick must always carry an explanation");
        assert!(explanation.contains("attempt 2"), "explanation must name the concrete attempt count: {explanation}");
        assert!(
            explanation.contains("no stable identity field found"),
            "explanation must surface the last rejection reason verbatim: {explanation}"
        );
    }

    /// Direct, non-async lock-down of `render_quiescence_explanation`'s
    /// exact wording for every `QuiescenceReason` variant — the sentences
    /// are the actual product surface, so this pins them independently of
    /// any route/state plumbing.
    #[test]
    fn render_quiescence_explanation_names_the_specific_cause_for_every_variant() {
        let ts = "2026-08-05T14:02:00Z".parse::<DateTime<Utc>>().unwrap();

        assert_eq!(
            render_quiescence_explanation(&QuiescenceReason::Expired { expires_at: ts }, None),
            format!("Not fired: this assignment expired at {} and was disabled instead of evaluated.", format_utc(ts))
        );

        assert_eq!(
            render_quiescence_explanation(&QuiescenceReason::NotDue { next_fire_at: Some(ts) }, None),
            format!("Not fired: not due to run again until {}.", format_utc(ts))
        );
        assert_eq!(
            render_quiescence_explanation(&QuiescenceReason::NotDue { next_fire_at: None }, None),
            "Not fired: no next run time has been scheduled yet."
        );

        assert_eq!(
            render_quiescence_explanation(&QuiescenceReason::FireFailed { reason: "connection reset".to_string() }, None),
            "Not fired: the last attempt to fire failed — connection reset."
        );

        assert_eq!(
            render_quiescence_explanation(
                &QuiescenceReason::ServerNotConnected { server: "notion".to_string(), state: None },
                Some(ts)
            ),
            format!(
                "Not fired: the notion MCP server has not been connected, as of the last check at {}.",
                format_utc(ts)
            )
        );
        assert_eq!(
            render_quiescence_explanation(
                &QuiescenceReason::ServerNotConnected { server: "notion".to_string(), state: Some("Disconnected".to_string()) },
                None
            ),
            "Not fired: the notion MCP server is not connected (state: Disconnected)."
        );

        assert_eq!(
            render_quiescence_explanation(&QuiescenceReason::NoLiveHandle { server: "gmail".to_string() }, None),
            "Not fired: the gmail MCP server reports connected, but no live connection handle is available to poll it right now."
        );

        assert_eq!(
            render_quiescence_explanation(
                &QuiescenceReason::PollFailed { server: "linear".to_string(), reason: "timed out after 30s".to_string() },
                None
            ),
            "Not fired: the last poll of the linear MCP server failed — timed out after 30s."
        );

        assert_eq!(
            render_quiescence_explanation(&QuiescenceReason::CursorUnresolved { server: "slack".to_string() }, None),
            "Not fired: the slack MCP server's last response didn't contain the field this assignment tracks for changes, so nothing could be compared."
        );

        assert_eq!(
            render_quiescence_explanation(
                &QuiescenceReason::AgentWatchContractNotBound(WatchContractStatus::NotYetAttempted),
                None
            ),
            "Not fired: no watch contract has been authored yet for this assignment."
        );
        assert_eq!(
            render_quiescence_explanation(
                &QuiescenceReason::AgentWatchContractNotBound(WatchContractStatus::AuthoringRejected {
                    attempts: 1,
                    ceiling_hit: false,
                    last_rejection_reason: None,
                }),
                None
            ),
            "Not fired: the most recent contract proposal (attempt 1) was rejected — authoring will retry automatically on the next poll."
        );
        assert_eq!(
            render_quiescence_explanation(
                &QuiescenceReason::AgentWatchContractNotBound(WatchContractStatus::AuthoringRejected {
                    attempts: 5,
                    ceiling_hit: true,
                    last_rejection_reason: Some("no proposal offered".to_string()),
                }),
                None
            ),
            "Not fired: contract authoring was rejected 5 times in a row and has stopped retrying — edit the instruction or connector scope to try again. Last rejection: no proposal offered"
        );
        assert_eq!(
            render_quiescence_explanation(
                &QuiescenceReason::AgentWatchContractNotBound(WatchContractStatus::Bound { bound_after_repairs: None }),
                None
            ),
            "Not fired: this watch's contract is bound, but the last poll observed nothing new to act on."
        );
    }
}
