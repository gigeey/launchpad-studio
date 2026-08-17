use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;
use crate::assignment_scratchpad::WatchContractStatus;
use crate::extractor_contract::ExtractionPlan;
use crate::thread::ThreadId;
use crate::watch_contract::WatchContract;
use crate::webhook_filter::WebhookFilter;

pub type AssignmentId = String;

/// What a `ConnectorEvent` trigger polls on each tick, and how the poll loop
/// extracts a comparable value for cursor dedup from the result.
///
/// `tool_name` is the raw MCP tool name (as advertised by the connector's
/// `tools/list`, not the runner's `mcp__{server}__{tool}` qualified form —
/// the poller calls the server directly, bypassing the tool registry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorPollSpec {
    /// MCP tool to invoke each poll (e.g. `"list_emails"`, `"search_issues"`).
    pub tool_name: String,
    /// Arguments passed as the tool call's `arguments` object.
    #[serde(default = "default_poll_arguments")]
    pub arguments: serde_json::Value,
    /// Dot-path into the raw `tools/call` result identifying the value to
    /// compare against `Assignment::last_event_cursor` (e.g. `"content.0.text"`
    /// or a structured-content field). Effectively required: if `None`, or if
    /// the path doesn't resolve against a given poll's result (or resolves to
    /// `null`), that poll extracts no cursor at all, so the tick loop
    /// (`schedule_runner::tick_connector_events`) neither fires nor advances
    /// `last_event_cursor` — it just reschedules the next poll as a no-op.
    /// There is no "compare the whole result" fallback for a blank path.
    ///
    /// Known limitation: nothing validates `cursor_path` at save time, so a
    /// typo'd path produces a watch that polls forever and never fires,
    /// indistinguishably from one whose source genuinely never changes.
    /// Validating the path against a sample poll result is not implemented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_path: Option<String>,
}

fn default_poll_arguments() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Structured event data captured when a trigger fires, threaded through to
/// the dispatched instruction so the fired agent receives what actually
/// triggered it instead of only its own static instruction.
///
/// Populated for `ConnectorEvent` fires (the poll result that changed); other
/// trigger kinds (`Cron`, `Webhook`, `Manual`) have no natural poll result and
/// leave this `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerEventContext {
    /// Token-able one-line description of what changed, safe to inline
    /// directly into the dispatched message (e.g. "New result from
    /// `list_emails` on `gmail` — cursor changed to 182").
    pub summary: String,
    /// The raw poll result backing `summary`, kept structured so an
    /// instruction can eventually reference specific fields (sender,
    /// subject, filename, …) rather than re-parsing free text.
    pub payload: serde_json::Value,
}

/// How an Assignment is fired.
///
/// `Cron` drives recurring or one-shot schedule-based execution via the
/// existing schedule-runner infrastructure (same cron expression format and
/// timezone-aware next-fire computation already used by scheduled tasks).
///
/// `Webhook` accepts an inbound HTTP POST identifying a named route
/// (`route_name`, served at `/webhooks/<route_name>`) and validates it
/// against `secret_ref` — a route with no secret is rejected rather than
/// treated as public. Connector platforms with genuine outbound-push
/// support (Zapier, GitHub, Stripe, Make, …) target this variant directly.
///
/// `ConnectorEvent` covers connectors with no push capability (the common
/// case today — MCP is pure request/response): the schedule-runner tick
/// polls `poll` on `server_name` every `poll_interval_secs` and fires when
/// the extracted cursor differs from `Assignment::last_event_cursor`.
///
/// `AgentWatch` is Tier 2 of the detection ladder: the fallback for any MCP
/// connector with no curated `ConnectorEvent`
/// entry. There is no single tool/cursor to poll — `instruction` is handed
/// to the assignment's own agent each due poll, which decides for itself
/// what to check and how. The new-vs-already-seen judgment is deliberately
/// kept out of the model and owned by code —
/// see `ao_engine::agent_watch::run_agent_watch_tick`, which diffs the
/// detector's observations against the same `state_scratchpad`
/// (`PersistenceLayer::assignment_scratchpads`) the push side already uses
/// for delivery-id dedup. `AgentWatch` never touches `last_event_cursor`
/// (that field stays `None`, exactly like `Cron`/`Webhook`) — its dedup
/// state lives entirely in the scratchpad instead.
///
/// `Eq` (not just `PartialEq`) intentionally dropped: `AgentWatch::contract`
/// holds a `WatchContract`, whose `predicate` field is the shared `Predicate`
/// enum — `GreaterThan`/`LessThan` carry an `f64`, which has no `Eq` impl.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AssignmentTrigger {
    Cron {
        /// Standard 5-field cron expression (e.g. `"0 9 * * *"`). Evaluated
        /// in the user's IANA timezone via the same next-fire helper used by
        /// the scheduled-task store.
        cron_expr: String,
        /// When `true` the assignment fires repeatedly on the cron schedule;
        /// when `false` it fires once and disables itself.
        is_recurring: bool,
    },
    Webhook {
        /// Deprecated legacy shared-secret check: if set, the inbound POST
        /// must supply `X-Assignment-Token: <token>` or `?token=<token>`.
        /// Superseded by `secret_ref`-based HMAC signing (`route_name` +
        /// `secret_ref`); kept only so rows created before the route gateway
        /// existed keep deserializing and keep working unmodified.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        /// Named route this trigger is served under: inbound POSTs land at
        /// `/webhooks/<route_name>` on the shared gateway. `None` for rows
        /// that predate the named-route gateway (they keep resolving via
        /// the legacy per-assignment `/assignments/{id}/trigger` URL).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        route_name: Option<String>,
        /// Reference to this route's HMAC signing secret (resolved through
        /// the secret store, never the plaintext secret itself). A route
        /// gateway MUST refuse to serve a route with no secret configured —
        /// `None` here only describes a legacy row that predates the
        /// requirement and has not yet been migrated to a named route.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_ref: Option<String>,
        /// Event-type allowlist matched against the inbound event's type
        /// (e.g. a GitHub `X-GitHub-Event` value like `"pull_request"`).
        /// Empty means allow every event type through to `filters`.
        #[serde(default)]
        events: Vec<String>,
        /// Declarative, pre-agent relevance filter evaluated against the
        /// raw JSON payload (see [`WebhookFilter`]). `None` means no
        /// filtering beyond the `events` allowlist.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filters: Option<WebhookFilter>,
        /// Prompt template rendered with dot-path payload field references
        /// (e.g. `{pull_request.title}`) and handed to the agent as its
        /// instruction. `None` falls back to the assignment's static
        /// `instruction` field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_template: Option<String>,
        /// Where a payload that passes `events`/`filters` is routed.
        #[serde(default)]
        deliver: WebhookDeliverTarget,
    },
    ConnectorEvent {
        /// MCP server / connector id, matching an entry the connector was
        /// added under (e.g. an `mcp_servers.toml` name).
        server_name: String,
        /// What to poll and how to extract the dedup cursor.
        poll: ConnectorPollSpec,
        /// Minimum seconds between polls. The tick only re-polls once this
        /// many seconds have elapsed since the previous poll attempt.
        poll_interval_secs: u64,
    },
    AgentWatch {
        /// The plain-language watch condition, e.g. "check my inbox for a
        /// new email from finance and summarize it." Handed to the
        /// assignment's own agent on every due poll — there is
        /// no `tool_name`/`arguments`/`cursor_path` to configure, which is
        /// the entire point of this tier: any MCP connector works day one,
        /// with the agent itself deciding which tools to call.
        instruction: String,
        /// Minimum seconds between detect-loop evaluations. Same
        /// floor-clamp convention as `ConnectorEvent::poll_interval_secs`
        /// (`MIN_POLL_INTERVAL_SECS` in `ao_engine::schedule_runner`) — a
        /// short interval here is a real cost risk since every poll pays
        /// full model price, not just a rate-limit courtesy.
        poll_interval_secs: u64,
        /// Restricts which MCP tools the watch agent may call each poll.
        /// `None` — the default — leaves the watch agent free to use every
        /// configured MCP tool, exactly like the original AgentWatch
        /// behavior. `Some(server_name)` restricts it to only the tools
        /// registered under that one MCP server's `mcp__{server_name}__`
        /// prefix, so a watch aimed at one connector can't wander into an
        /// unrelated one. `server_name` is the lowercase MCP server `name`
        /// (matching `^[a-z][a-z0-9_]*$`, the same rule `validate()` below
        /// enforces). `#[serde(default)]` so assignments persisted before
        /// this field existed deserialize with `None` — serde applies a
        /// missing field's `Default` per-field within a matched enum
        /// variant, the same as it would for a plain struct.
        #[serde(default)]
        connector_scope: Option<String>,
        /// The frozen declaration of what this watch observes and what
        /// identity, change, and firing mean for it — see
        /// [`crate::watch_contract`]. `None` until the watch's
        /// first run authors one; every poll after that binds the model to
        /// it instead of letting it re-decide identity from scratch.
        /// `#[serde(default)]` so rows persisted before this field existed
        /// deserialize with `None`, the same additive pattern
        /// `connector_scope` above already established for this variant.
        #[serde(default)]
        contract: Option<WatchContract>,
        /// A code-owned alternative to asking the model to observe every
        /// poll (`ao_engine::agent_watch::run_agent_watch_tick`'s
        /// extraction-path branch): when this resolves to
        /// `extractor_contract::Tier::Deterministic` or `::Probabilistic`,
        /// candidates come from `extractor_contract::resolve` against the
        /// poll's own tool output instead of a full-price model child
        /// session. `None` — the default — means every contract-bound poll
        /// still asks the model, exactly as before this field existed.
        /// `#[serde(default)]`, the same additive pattern `contract` and
        /// `connector_scope` above already use for this variant.
        #[serde(default)]
        extraction: Option<ExtractionPlan>,
        /// The bare MCP `tools/call` name (no `mcp__{server}__` registry
        /// prefix) `extraction`'s selector reads its content from, via
        /// `ao_engine_tools_runner::mcp::payload_stash`, keyed together with
        /// `connector_scope` as the stash's `(server, tool)` pair. Only
        /// consulted when `extraction` is `Some`; a plan with no tool named
        /// here, or no `connector_scope`, cannot be resolved deterministically
        /// and falls back to the model detector. `#[serde(default)]`, same
        /// additive reasoning as `extraction`.
        #[serde(default)]
        extraction_tool: Option<String>,
        /// The arguments `extraction_tool` is called with. Only meaningful
        /// alongside `extraction_tool` — a tool name with no arguments isn't
        /// callable, so this is carried and cleared in lockstep with it (see
        /// `carry_forward_watch_contract` below). `None` — the default —
        /// means no call has been recorded yet. `#[serde(default)]`, same
        /// additive reasoning as `extraction_tool`.
        #[serde(default)]
        extraction_args: Option<serde_json::Value>,
        /// Whether the MCP server has declared an `outputSchema` for
        /// `extraction_tool` — the second input to
        /// `extractor_contract::infer_tier`, alongside whatever structured
        /// content the payload stash returns for `extraction_tool` on a given
        /// poll. Defaults to `false`, the conservative assumption: a
        /// `JsonPath` extraction plan reads as `Tier::Probabilistic`, not
        /// `Tier::Deterministic`, until this is confirmed `true`. A live
        /// per-poll lookup against the server's actual declared schema
        /// (`ao_engine_tools_runner::mcp::schema_fetch::McpToolDescriptor::output_schema`)
        /// is out of scope here — this field is a static stand-in for that
        /// lookup until a later phase wires it up.
        #[serde(default)]
        extraction_output_schema_declared: bool,
    },
}

/// Floor on `AgentWatch::poll_interval_secs` enforced at registration time by
/// [`AssignmentTrigger::validate`]. Deliberately much higher than
/// `ConnectorEvent`'s runtime clamp (`MIN_POLL_INTERVAL_SECS` in
/// `ao_engine::schedule_runner`, 60s) because every AgentWatch poll runs a
/// full agent turn against a live model (fidelity over cost)
/// rather than a cheap deterministic tool call — a 60s floor here would let
/// a misconfigured watch burn a full model call every minute indefinitely.
/// A still-authoring watch costs roughly 96 model sessions/day at this
/// default vs. roughly 2880/day at a 60s floor, so this must never be
/// lowered as a shipped default — see
/// [`MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR`] for the sanctioned way
/// to lower it for a demo or local test.
pub const MIN_AGENT_WATCH_POLL_INTERVAL_SECS: u64 = 900;

/// Env var that overrides [`MIN_AGENT_WATCH_POLL_INTERVAL_SECS`] for demos
/// and local testing. Unset in any shipped or normal-operation environment —
/// setting it lowers the poll floor and multiplies background model-session
/// cost for every enabled `AgentWatch`. Read fresh on every call to
/// [`effective_min_agent_watch_poll_interval_secs`] rather than cached, so
/// it can be flipped without a process restart; `ao_server::main` reads it
/// once at startup purely to log a WARN when it lowers the effective floor,
/// which is where callers should look to confirm it is (or isn't) active.
pub const MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR: &str =
    "LAUNCHPAD_AGENT_WATCH_MIN_POLL_INTERVAL_SECS_OVERRIDE";

/// Resolves the effective `AgentWatch` poll-interval floor: the value of
/// [`MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR`] if it is set and parses
/// to a non-zero `u64`, otherwise [`MIN_AGENT_WATCH_POLL_INTERVAL_SECS`].
/// Absent, unparseable, or zero input silently falls back to the default —
/// this function never panics and never returns 0.
pub fn effective_min_agent_watch_poll_interval_secs() -> u64 {
    std::env::var(MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(MIN_AGENT_WATCH_POLL_INTERVAL_SECS)
}

/// Cap on how many `AgentWatch` assignments a single agent may have enabled
/// at once. Enforced only at the moment a create or patch would newly turn
/// an assignment into an enabled `AgentWatch` (see
/// `AssignmentStore::enforce_agent_watch_cap` in `ao-persistence`, called
/// from both the `/assignments` HTTP routes and the `AssignmentCreate`/
/// `AssignmentUpdate` engine tools) — every enabled `AgentWatch` runs a full
/// agent turn on its own poll schedule (fidelity over cost),
/// so an unbounded per-agent count is an unbounded background model cost.
/// Lowering this constant, or anything else that pushes an agent over the
/// cap after the fact, never retroactively disables existing rows.
pub const MAX_ACTIVE_AGENT_WATCHES_PER_AGENT: usize = 10;

impl AssignmentTrigger {
    /// Structural validation applied when a trigger is registered
    /// (assignment create/update). Currently the only rule: a `Webhook`
    /// route whose `deliver` target skips the agent run (`DeliverOnly` or
    /// `GithubComment`) is meaningless without a `route_name` — `deliver` is
    /// only ever evaluated by the named-route gateway (`POST
    /// /webhooks/{route_name}`); the legacy per-assignment trigger endpoint
    /// applies no `events`/`filters`/`deliver` semantics at all. A route
    /// with no name can never receive a POST, so a `DeliverOnly`/
    /// `GithubComment` deliver target on it could never actually deliver
    /// anything — reject it at registration rather than silently accepting
    /// a route that can never fire.
    pub fn validate(&self) -> Result<(), String> {
        if let AssignmentTrigger::Webhook {
            deliver,
            route_name,
            ..
        } = self
        {
            let skips_agent = *deliver != WebhookDeliverTarget::Agent;
            let has_route_name = route_name
                .as_deref()
                .map(|r| !r.trim().is_empty())
                .unwrap_or(false);
            if skips_agent && !has_route_name {
                return Err(format!(
                    "a webhook trigger with deliver = {deliver:?} skips the agent run and requires a route_name so the named-route gateway can actually receive and deliver it"
                ));
            }
        }
        if let AssignmentTrigger::AgentWatch {
            instruction,
            poll_interval_secs,
            connector_scope,
            ..
        } = self
        {
            if instruction.trim().is_empty() {
                return Err(
                    "an agent-driven watch trigger requires a non-blank instruction — it is the entire watch condition the detect loop evaluates"
                        .to_string(),
                );
            }
            let min_poll_interval_secs = effective_min_agent_watch_poll_interval_secs();
            if *poll_interval_secs < min_poll_interval_secs {
                return Err(format!(
                    "AgentWatch poll interval must be at least {min_poll_interval_secs} seconds"
                ));
            }
            if let Some(server_name) = connector_scope {
                if !is_valid_connector_scope(server_name) {
                    return Err(format!(
                        "invalid connector_scope {server_name:?} (must be a lowercase MCP server name matching ^[a-z][a-z0-9_]*$)"
                    ));
                }
            }
            // `ao_engine::agent_watch::LiveAgentWatchDetector` is now the
            // detector `run_agent_watch_tick` evaluates every due poll
            // against — a well-formed AgentWatch trigger is safe to accept.
        }
        Ok(())
    }
}

/// Same MCP server name rule `McpServerEntry` enforces in
/// `ao-engine-tools-provider-config` (`^[a-z][a-z0-9_]*$`), inlined here
/// rather than depending on that crate — `AgentWatch::connector_scope`
/// stores exactly that server `name`, so a scope this rejects could never
/// match a real connector anyway.
fn is_valid_connector_scope(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        }
        _ => false,
    }
}

/// Applied whenever an assignment's `trigger` is replaced during an update:
/// decides whether a previously authored [`WatchContract`] (and, alongside
/// it, a previously bound `extraction`/`extraction_tool`/`extraction_args`/
/// `extraction_output_schema_declared`) carries forward onto the new trigger
/// or is discarded.
///
/// Both the contract and the extraction plan are derived from `instruction`
/// and `connector_scope` (they describe what those declare should be
/// watched), so it's only ever safe to keep either when both are unchanged —
/// anything else (a poll interval tweak, an assignment rename, `enabled`
/// toggling) leaves them untouched. A trigger pair that isn't `AgentWatch` on
/// both sides has nothing to carry, and is left as-is. Callers should log the
/// returned reason at INFO alongside the assignment id — this function only
/// decides, it doesn't log, since it has no assignment id and callers
/// already have their own logging conventions.
pub fn carry_forward_watch_contract(
    previous: &AssignmentTrigger,
    mut next: AssignmentTrigger,
) -> (AssignmentTrigger, Option<&'static str>) {
    let AssignmentTrigger::AgentWatch {
        instruction: prev_instruction,
        connector_scope: prev_scope,
        contract: prev_contract,
        extraction: prev_extraction,
        extraction_tool: prev_extraction_tool,
        extraction_args: prev_extraction_args,
        extraction_output_schema_declared: prev_extraction_schema_declared,
        ..
    } = previous
    else {
        return (next, None);
    };
    let AssignmentTrigger::AgentWatch {
        instruction: next_instruction,
        connector_scope: next_scope,
        contract: next_contract,
        extraction: next_extraction,
        extraction_tool: next_extraction_tool,
        extraction_args: next_extraction_args,
        extraction_output_schema_declared: next_extraction_schema_declared,
        ..
    } = &mut next
    else {
        return (next, None);
    };

    let instruction_changed = prev_instruction != next_instruction;
    let connector_scope_changed = prev_scope != next_scope;

    let cleared_reason = match (instruction_changed, connector_scope_changed) {
        (false, false) => {
            *next_contract = prev_contract.clone();
            *next_extraction = prev_extraction.clone();
            *next_extraction_tool = prev_extraction_tool.clone();
            *next_extraction_args = prev_extraction_args.clone();
            *next_extraction_schema_declared = *prev_extraction_schema_declared;
            None
        }
        (true, false) => Some("instruction changed"),
        (false, true) => Some("connector_scope changed"),
        (true, true) => Some("instruction changed, connector_scope changed"),
    };
    (next, cleared_reason)
}

/// Where a `Webhook` delivery that passes `events`/`filters` is routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebhookDeliverTarget {
    /// Start the assignment's agent, handing it the rendered
    /// `prompt_template` (or the static `instruction` if unset) as its
    /// message. The default — matches every other trigger kind's behavior.
    #[default]
    Agent,
    /// Notify only: no agent run starts. Zero LLM tokens, sub-second
    /// delivery — for routes that exist purely to record or relay an event.
    DeliverOnly,
    /// Post the rendered `prompt_template` as a comment on the pull request
    /// identified by the payload (`gh pr comment <pr> --repo <repo> --body
    /// <content>`), without starting a full agent run.
    GithubComment,
}

/// How the assignment run's output is handled after completion.
///
/// v1 only supports `Background`: output is captured in the [`AssignmentRun`]
/// row and surfaced in the frontend Assignments tab. Future variants might
/// add delivery mechanisms (email, outbound webhook).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    #[default]
    Background,
}

/// Which thread an assignment's runs land in.
///
/// `Fresh` (the default) preserves the original behavior: every fire creates
/// a brand-new, disposable thread, so a proactive run never interrupts
/// whatever thread the user is actively chatting in, and each run starts
/// with a cold history. `Main` and `Dedicated` trade that isolation for
/// continuity — pick them when a run should read like part of an ongoing
/// conversation (e.g. a coaching check-in that should land in the main
/// thread) or accumulate its own running history across fires (e.g. a daily
/// brief that builds on yesterday's), respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentThreadPolicy {
    /// Every fire creates a new throwaway thread. Matches pre-policy
    /// behavior exactly, which is why it's the default for both new rows
    /// (no explicit choice) and legacy rows (missing the field entirely).
    #[default]
    Fresh,
    /// Every fire posts into the agent's main/default thread — the same
    /// thread interactive chat uses. This deliberately gives up the
    /// never-interrupt-live-chat guarantee `Fresh` provides.
    Main,
    /// Every fire reuses one thread dedicated to this assignment. Created
    /// lazily on first fire and recorded in `Assignment::dedicated_thread_id`
    /// so later fires reuse the same thread instead of creating a new one.
    Dedicated,
}

/// Placeholder for MCP server or external connector bindings.
///
/// v1: the field is present for forward compatibility but carries no enforced
/// semantics. Callers may omit it entirely. Future: a `kind = "mcp_server"`
/// entry would tell the assignment runner to inject the named server's context
/// or enable specific MCP tools for the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentBinding {
    pub kind: String,
    pub ref_id: String,
}

/// A persistent rule that drives proactive agent runs on a trigger.
///
/// Stored as a row in the assignment store (`{data_root}/assignments.json`).
/// One `Assignment` may produce many [`AssignmentRun`] executions over time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub id: AssignmentId,
    /// Owning agent.
    pub agent_id: AgentId,
    /// Human-readable label shown in the Assignments tab.
    pub name: String,
    /// The prompt injected into the agent on each triggered run. Treated
    /// identically to a user-typed message but marked as autonomous so it
    /// bypasses the interactive-serialization gate.
    pub instruction: String,
    /// Focus path for the fired run, passed through as the dispatched
    /// message's `focus_path`. `None` runs with no focus override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    pub trigger: AssignmentTrigger,
    /// v1: empty in practice; validated but not acted on.
    #[serde(default)]
    pub bindings: Vec<AssignmentBinding>,
    #[serde(default)]
    pub output_mode: OutputMode,
    /// Where this assignment's runs land. See [`AssignmentThreadPolicy`].
    #[serde(default)]
    pub thread_policy: AssignmentThreadPolicy,
    /// The thread claimed for `Dedicated`-policy runs, set the first time
    /// such an assignment fires and reused on every subsequent fire. Ignored
    /// (but preserved across policy changes) when `thread_policy` is not
    /// `Dedicated`. Always `None` for `Fresh`/`Main` assignments and for a
    /// `Dedicated` assignment that has never fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedicated_thread_id: Option<ThreadId>,
    /// When `false` the assignment is visible but will not fire.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Optional cutoff after which the assignment stops firing. The
    /// schedule-runner tick disables the assignment once `expires_at` is in
    /// the past, mirroring the legacy scheduled-task expiry check. `None`
    /// means the assignment never expires on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Scheduling state. For `Cron`, the next time the cron expression is
    /// due; updated by the store's `mark_fired`. For `ConnectorEvent` and
    /// `AgentWatch`, the next time the poll loop / detect loop should
    /// re-evaluate; updated by the store's `mark_polled`. Always `None` for
    /// `Webhook` assignments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<DateTime<Utc>>,
    /// Last cursor value observed by a `ConnectorEvent` poll, used to dedup
    /// consecutive polls that see the same underlying state. `None` until the
    /// first poll ever runs (that first poll seeds this baseline without
    /// firing — see `ScheduleRunner::tick_connector_events`). Always `None`
    /// for `Cron`/`Webhook`/`AgentWatch` assignments — `AgentWatch`'s dedup
    /// state lives entirely in the `state_scratchpad` store instead (see
    /// [`AssignmentTrigger::AgentWatch`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_cursor: Option<String>,
    /// Liveness bookkeeping: when the tick loop last looked at this
    /// assignment at all, how many times it has fired, and — when the most
    /// recent tick ended without firing — why. `#[serde(default)]` so an
    /// `assignments.json` written before this field existed keeps
    /// deserializing instead of failing to load (see
    /// `assignment_liveness_field_defaults_on_missing_json` in this module's
    /// tests).
    #[serde(default)]
    pub liveness: LivenessState,
    pub created_ts: DateTime<Utc>,
    pub updated_ts: DateTime<Utc>,
}

fn default_enabled() -> bool {
    true
}

/// The complete, closed set of reasons an assignment's tick can end WITHOUT
/// firing. Every variant traces to one specific branch in
/// `ao_engine::schedule_runner::ScheduleRunner`'s tick loop; see each
/// variant's doc for the exact source. Two branches (the `Cron` and
/// `ConnectorEvent` expiry checks, and their respective not-due checks) share
/// a variant here because they are the same check duplicated across two
/// trigger kinds, not two distinct reasons.
///
/// Deliberately does NOT cover `ConnectorEvent`'s "seeded baseline" or
/// "cursor unchanged" outcomes — both are already fully reconstructable from
/// `Assignment::last_event_cursor` alone (`None` vs. an unchanged value)
/// without a new reason needing to duplicate that state, unlike every other
/// variant here, none of which corresponds to any other field already on
/// `Assignment`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QuiescenceReason {
    /// The assignment's `expires_at` had already passed at tick time, so the
    /// trigger was disabled instead of evaluated this tick. Shared by the
    /// `Cron` tick (`schedule_runner.rs:152-159`) and the `ConnectorEvent`
    /// poll loop (`schedule_runner.rs:243-250`) — both expiry checks are
    /// identical modulo trigger kind.
    Expired { expires_at: DateTime<Utc> },
    /// `next_fire_at` was still in the future at tick time. Shared by the
    /// `Cron` tick (`schedule_runner.rs:162-164`, where a missing
    /// `next_fire_at` also reads as not-due) and the `ConnectorEvent` poll
    /// loop (`schedule_runner.rs:255-258`, where a missing `next_fire_at`
    /// instead reads as due-now for the assignment's first-ever poll — so in
    /// practice this variant is only ever recorded there once a poll has run
    /// at least once before).
    NotDue { next_fire_at: Option<DateTime<Utc>> },
    /// A due assignment's `fire_assignment` call returned `Err`. Shared by
    /// the `Cron` tick (`schedule_runner.rs:183-187`) and the
    /// `ConnectorEvent` poll loop's fire-on-change arm — the latter isn't
    /// one of this module's originally-cited line ranges, but a fire
    /// attempt that fails is structurally the same "declined to fire, here's
    /// why" outcome for either trigger kind, and the restructure that wires
    /// this variant up makes it impossible for that arm to return anything
    /// else without a reason. `reason` is the error's `Display`.
    FireFailed { reason: String },
    /// A due `ConnectorEvent` assignment's backing MCP server was not in
    /// `McpServerState::Connected` this tick — `schedule_runner.rs:263-273`.
    /// `state` is the server's actual current state (its `Debug` rendering)
    /// where the MCP manager has one on record, `None` if the server isn't
    /// registered with it at all. Rendered as a string rather than the
    /// engine-crate `McpServerState` type itself, since `ao-protocol` cannot
    /// depend on the crate that defines it without creating a dependency
    /// cycle (`ao-engine-tools-runner` already depends on `ao-protocol`).
    ServerNotConnected {
        server: String,
        state: Option<String>,
    },
    /// A due `ConnectorEvent` assignment's backing server reported
    /// `Connected`, but the MCP manager had no live client handle for it —
    /// `schedule_runner.rs:275-282`. Distinct from `ServerNotConnected`: this
    /// is the manager's status cache and its handle table disagreeing with
    /// each other, not a straightforwardly offline/unauthorized connector.
    NoLiveHandle { server: String },
    /// A due `ConnectorEvent` assignment's `tools/call` poll returned `Err`
    /// — `schedule_runner.rs:288-307`. `reason` is the error's `Display`.
    PollFailed { server: String, reason: String },
    /// A due `ConnectorEvent` assignment's poll succeeded, but
    /// `ConnectorPollSpec::cursor_path` either wasn't configured or didn't
    /// resolve against the result, so no cursor could be extracted —
    /// `schedule_runner.rs:315-331`. The poll is still recorded (via
    /// `mark_polled`), but it neither fires nor advances
    /// `Assignment::last_event_cursor`.
    CursorUnresolved { server: String },
    /// A due `AgentWatch` assignment's contract-authoring lifecycle hasn't
    /// produced a bound `WatchContract` yet, wrapping
    /// [`crate::assignment_scratchpad::WatchContractStatus`] (re-exported as
    /// `ao_engine::agent_watch::WatchContractStatus`) rather than duplicating
    /// its states — see that type's own doc for the authoring-lifecycle
    /// detail. Expected to only ever carry its `NotYetAttempted` or
    /// `AuthoringRejected` variants here: a `Bound` status means the
    /// assignment already has a live contract, so any given tick's silence
    /// then is a "polled, nothing new observed" outcome, which is outside
    /// this reason's scope (there is no dedicated variant for it — see this
    /// enum's own doc for why the analogous `ConnectorEvent` "unchanged"
    /// case is likewise omitted).
    AgentWatchContractNotBound(WatchContractStatus),
}

/// Liveness bookkeeping for one [`Assignment`]: when the tick loop last
/// looked at it (regardless of outcome), how many times it has actually
/// fired, and — when the most recent tick ended without firing — why. Always
/// present (never `Option`) so a never-evaluated assignment reads as
/// `LivenessState::default()` rather than as a wholly absent field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LivenessState {
    /// When the tick loop last evaluated this assignment at all, whether or
    /// not that tick fired. `None` until the very first tick ever looks at
    /// it. Set unconditionally by `AssignmentStore::mark_evaluated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_evaluated_at: Option<DateTime<Utc>>,
    /// Total number of times this assignment has fired, over its whole
    /// lifetime. Incremented only on a fire outcome.
    #[serde(default)]
    pub fire_count: u64,
    /// Why the most recent tick ended without firing. Cleared (`None`) the
    /// moment the assignment next fires; set to `Some` on every no-fire
    /// outcome, overwriting whatever reason was recorded previously.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_quiescence: Option<QuiescenceReason>,
}

impl Default for LivenessState {
    fn default() -> Self {
        LivenessState {
            last_evaluated_at: None,
            fire_count: 0,
            last_quiescence: None,
        }
    }
}

pub type AssignmentRunId = String;

/// What class of trigger fired this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentTriggerKind {
    Cron,
    Webhook,
    Manual,
    ConnectorEvent,
    AgentWatch,
}

impl AssignmentTriggerKind {
    /// Lowercase wire string used inside the assignment-run message envelope
    /// and the [`crate::scheduled_task::MessageSource::Assignment`] payload.
    pub fn as_str(self) -> &'static str {
        match self {
            AssignmentTriggerKind::Cron => "cron",
            AssignmentTriggerKind::Webhook => "webhook",
            AssignmentTriggerKind::Manual => "manual",
            AssignmentTriggerKind::ConnectorEvent => "connector_event",
            AssignmentTriggerKind::AgentWatch => "agent_watch",
        }
    }
}

/// Lifecycle state of a single proactive execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentRunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
}

/// One proactive execution of an [`Assignment`].
///
/// Persisted as a line in
/// `{data_root}/assignment_runs/{assignment_id}.jsonl`
/// (one JSONL file per assignment so list-by-assignment is a single file read
/// rather than a full scan).
///
/// Lifecycle: the trigger code inserts a `Queued` row, the queue-manager
/// callback updates it to `Running` when dispatch begins, and the run-complete
/// path updates it to `Succeeded` or `Failed` with an `output_summary` /
/// `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssignmentRun {
    pub id: AssignmentRunId,
    pub assignment_id: AssignmentId,
    pub agent_id: AgentId,
    pub trigger_kind: AssignmentTriggerKind,
    /// Free-form context captured at trigger time. For `Cron`: the cron
    /// expression string. For `Webhook`: a truncated excerpt of the POST body
    /// (max 500 chars). For `Manual`: absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_payload: Option<String>,
    pub status: AssignmentRunStatus,
    /// First ~500 chars of the agent's final assistant text, captured at run
    /// completion. `None` until the run finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
    /// The thread that holds this run's conversation, resolved from the
    /// assignment's `thread_policy` immediately before enqueueing: a brand
    /// new thread (`Fresh`), the agent's main thread (`Main`), or the
    /// assignment's reused `dedicated_thread_id` (`Dedicated`). Set from
    /// `Queued` onwards and never changes for a given run. `None` only in
    /// error paths where thread resolution failed before the run was
    /// enqueued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    pub queued_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ts: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR` is process-global, and
    // this crate's unit tests run in parallel by default, so any test that
    // sets/clears it must not run concurrently with another test that relies
    // on the poll-interval floor being the compiled-in default. Serialize
    // just that handful of tests rather than pulling in a `#[serial]` crate.
    static POLL_INTERVAL_OVERRIDE_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn sample_cron_assignment() -> Assignment {
        let now = Utc::now();
        Assignment {
            id: "assign-1".to_string(),
            agent_id: "agent-1".to_string(),
            name: "Daily standup summary".to_string(),
            instruction: "Write a bullet-point standup draft.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Cron {
                cron_expr: "0 8 * * 1-5".to_string(),
                is_recurring: true,
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
            liveness: LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn sample_connector_event_assignment() -> Assignment {
        let now = Utc::now();
        Assignment {
            id: "assign-connector".to_string(),
            agent_id: "agent-1".to_string(),
            name: "New starred email".to_string(),
            instruction: "Summarize the new starred email.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::ConnectorEvent {
                server_name: "gmail".to_string(),
                poll: ConnectorPollSpec {
                    tool_name: "list_starred".to_string(),
                    arguments: serde_json::json!({ "max_results": 5 }),
                    cursor_path: Some("structuredContent.latest_id".to_string()),
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
            liveness: LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    fn sample_run() -> AssignmentRun {
        let now = Utc::now();
        AssignmentRun {
            id: "run-1".to_string(),
            assignment_id: "assign-1".to_string(),
            agent_id: "agent-1".to_string(),
            trigger_kind: AssignmentTriggerKind::Cron,
            trigger_payload: Some("0 8 * * 1-5".to_string()),
            status: AssignmentRunStatus::Queued,
            output_summary: None,
            thread_id: Some("thread-1".to_string()),
            queued_at: now,
            started_ts: None,
            finished_ts: None,
            error: None,
        }
    }

    #[test]
    fn assignment_cron_json_round_trip() {
        let assignment = sample_cron_assignment();
        let json = serde_json::to_string(&assignment).expect("serialize");
        let back: Assignment = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, assignment.id);
        assert_eq!(back.name, assignment.name);
        assert_eq!(back.trigger, assignment.trigger);
        assert_eq!(back.output_mode, OutputMode::Background);
        assert!(back.enabled);
    }

    #[test]
    fn assignment_webhook_json_round_trip() {
        let now = Utc::now();
        let assignment = Assignment {
            id: "assign-wh".to_string(),
            agent_id: "agent-1".to_string(),
            name: "Inbound hook".to_string(),
            instruction: "Process the inbound event.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::Webhook {
                token: Some("secret-token".to_string()),
                route_name: Some("github-prs".to_string()),
                secret_ref: Some("vault:webhook/github-prs".to_string()),
                events: vec!["pull_request".to_string()],
                filters: None,
                prompt_template: Some(
                    "Review PR #{pull_request.number}: {pull_request.title}".to_string(),
                ),
                deliver: WebhookDeliverTarget::GithubComment,
            },
            bindings: vec![],
            output_mode: OutputMode::default(),
            thread_policy: AssignmentThreadPolicy::default(),
            dedicated_thread_id: None,
            enabled: true,
            expires_at: None,
            next_fire_at: None,
            last_run_at: None,
            last_event_cursor: None,
            liveness: LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        };
        let json = serde_json::to_string(&assignment).expect("serialize");
        // Webhook trigger has no next_fire_at; it is skipped on serialize.
        assert!(!json.contains("next_fire_at"));
        let back: Assignment = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back.trigger,
            AssignmentTrigger::Webhook {
                token: Some("secret-token".to_string()),
                route_name: Some("github-prs".to_string()),
                secret_ref: Some("vault:webhook/github-prs".to_string()),
                events: vec!["pull_request".to_string()],
                filters: None,
                prompt_template: Some(
                    "Review PR #{pull_request.number}: {pull_request.title}".to_string()
                ),
                deliver: WebhookDeliverTarget::GithubComment,
            }
        );
        assert!(back.next_fire_at.is_none());
    }

    #[test]
    fn assignment_trigger_tag_discriminants() {
        let cron = AssignmentTrigger::Cron {
            cron_expr: "* * * * *".to_string(),
            is_recurring: false,
        };
        let json = serde_json::to_string(&cron).expect("serialize");
        assert!(json.contains("\"type\":\"Cron\""), "got: {json}");

        let webhook = AssignmentTrigger::Webhook {
            token: None,
            route_name: None,
            secret_ref: None,
            events: vec![],
            filters: None,
            prompt_template: None,
            deliver: WebhookDeliverTarget::default(),
        };
        let json = serde_json::to_string(&webhook).expect("serialize");
        assert!(json.contains("\"type\":\"Webhook\""), "got: {json}");

        let connector_event = AssignmentTrigger::ConnectorEvent {
            server_name: "gmail".to_string(),
            poll: ConnectorPollSpec {
                tool_name: "list_starred".to_string(),
                arguments: serde_json::json!({}),
                cursor_path: None,
            },
            poll_interval_secs: 60,
        };
        let json = serde_json::to_string(&connector_event).expect("serialize");
        assert!(json.contains("\"type\":\"ConnectorEvent\""), "got: {json}");
    }

    #[test]
    fn assignment_connector_event_json_round_trip() {
        let assignment = sample_connector_event_assignment();
        let json = serde_json::to_string(&assignment).expect("serialize");
        let back: Assignment = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.trigger, assignment.trigger);
        assert!(back.last_event_cursor.is_none());
    }

    #[test]
    fn last_event_cursor_round_trips_and_is_omitted_when_absent() {
        let mut assignment = sample_connector_event_assignment();
        assignment.last_event_cursor = Some("msg-42".to_string());

        let json = serde_json::to_string(&assignment).expect("serialize");
        let back: Assignment = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.last_event_cursor.as_deref(), Some("msg-42"));

        let fresh = sample_connector_event_assignment();
        let fresh_json = serde_json::to_string(&fresh).expect("serialize");
        assert!(!fresh_json.contains("last_event_cursor"));
    }

    #[test]
    fn connector_poll_spec_arguments_default_to_empty_object_when_absent() {
        let json = r#"{"tool_name": "list_starred"}"#;
        let spec: ConnectorPollSpec = serde_json::from_str(json).expect("minimal poll spec");
        assert_eq!(spec.arguments, serde_json::json!({}));
        assert!(spec.cursor_path.is_none());
    }

    #[test]
    fn output_mode_serializes_snake_case_and_defaults() {
        let json = serde_json::to_string(&OutputMode::Background).expect("serialize");
        assert_eq!(json, "\"background\"");
        assert_eq!(OutputMode::default(), OutputMode::Background);
    }

    #[test]
    fn webhook_deliver_target_serializes_snake_case_and_defaults() {
        assert_eq!(
            serde_json::to_string(&WebhookDeliverTarget::Agent).unwrap(),
            "{\"type\":\"agent\"}"
        );
        assert_eq!(
            serde_json::to_string(&WebhookDeliverTarget::DeliverOnly).unwrap(),
            "{\"type\":\"deliver_only\"}"
        );
        assert_eq!(
            serde_json::to_string(&WebhookDeliverTarget::GithubComment).unwrap(),
            "{\"type\":\"github_comment\"}"
        );
        assert_eq!(WebhookDeliverTarget::default(), WebhookDeliverTarget::Agent);
    }

    #[test]
    fn webhook_trigger_carries_declarative_filters() {
        use crate::webhook_filter::{WebhookFieldFilter, WebhookFilter, WebhookFilterOp};

        let trigger = AssignmentTrigger::Webhook {
            token: None,
            route_name: Some("github-prs".to_string()),
            secret_ref: Some("vault:webhook/github-prs".to_string()),
            events: vec!["pull_request".to_string()],
            filters: Some(WebhookFilter::Field(WebhookFieldFilter {
                field: "action".to_string(),
                op: WebhookFilterOp::Equals {
                    value: serde_json::json!("opened"),
                },
            })),
            prompt_template: Some("{__raw__}".to_string()),
            deliver: WebhookDeliverTarget::DeliverOnly,
        };
        let json = serde_json::to_string(&trigger).expect("serialize");
        let back: AssignmentTrigger = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, trigger);
    }

    #[test]
    fn validate_rejects_deliver_only_without_route_name() {
        let trigger = AssignmentTrigger::Webhook {
            token: None,
            route_name: None,
            secret_ref: None,
            events: vec![],
            filters: None,
            prompt_template: None,
            deliver: WebhookDeliverTarget::DeliverOnly,
        };
        assert!(trigger.validate().is_err());
    }

    #[test]
    fn validate_accepts_deliver_only_with_route_name() {
        let trigger = AssignmentTrigger::Webhook {
            token: None,
            route_name: Some("my-route".to_string()),
            secret_ref: Some("vault:webhook/my-route".to_string()),
            events: vec![],
            filters: None,
            prompt_template: None,
            deliver: WebhookDeliverTarget::DeliverOnly,
        };
        assert!(trigger.validate().is_ok());
    }

    #[test]
    fn validate_rejects_github_comment_without_route_name() {
        let trigger = AssignmentTrigger::Webhook {
            token: None,
            route_name: None,
            secret_ref: None,
            events: vec![],
            filters: None,
            prompt_template: None,
            deliver: WebhookDeliverTarget::GithubComment,
        };
        assert!(trigger.validate().is_err());
    }

    #[test]
    fn validate_accepts_agent_deliver_target_without_route_name() {
        // The legacy per-assignment trigger path never sets route_name, and
        // the default Agent deliver target must keep working unmodified.
        let trigger = AssignmentTrigger::Webhook {
            token: Some("legacy-token".to_string()),
            route_name: None,
            secret_ref: None,
            events: vec![],
            filters: None,
            prompt_template: None,
            deliver: WebhookDeliverTarget::Agent,
        };
        assert!(trigger.validate().is_ok());
    }

    #[test]
    fn validate_accepts_non_webhook_triggers_unconditionally() {
        let trigger = AssignmentTrigger::Cron {
            cron_expr: "* * * * *".to_string(),
            is_recurring: true,
        };
        assert!(trigger.validate().is_ok());
    }

    #[test]
    fn validate_rejects_agent_watch_with_blank_instruction() {
        let trigger = AssignmentTrigger::AgentWatch {
            instruction: "   ".to_string(),
            poll_interval_secs: 900,
            connector_scope: None,
            contract: None,
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        };
        let err = trigger.validate().unwrap_err();
        assert!(err.contains("non-blank instruction"), "got: {err}");
    }

    #[test]
    fn validate_accepts_well_formed_agent_watch_trigger() {
        // The production detector (`ao_engine::agent_watch::LiveAgentWatchDetector`)
        // is wired into the scheduler's detect loop now, so a well-formed
        // AgentWatch trigger (non-blank instruction + cadence) is safe to
        // register over the HTTP create/update routes.
        let trigger = AssignmentTrigger::AgentWatch {
            instruction: "Check my inbox for a new email from finance".to_string(),
            poll_interval_secs: 900,
            connector_scope: None,
            contract: None,
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        };
        assert!(trigger.validate().is_ok());
    }

    #[test]
    fn validate_accepts_well_formed_connector_scope() {
        let trigger = AssignmentTrigger::AgentWatch {
            instruction: "Check notion for a new page".to_string(),
            poll_interval_secs: 900,
            connector_scope: Some("notion".to_string()),
            contract: None,
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        };
        assert!(trigger.validate().is_ok());
    }

    /// Pins `MIN_AGENT_WATCH_POLL_INTERVAL_SECS` to its shipped value. This
    /// constant is a cost control, not a tuning knob: every `AgentWatch`
    /// poll runs a full agent turn against a live model, so a still-
    /// authoring watch costs roughly 96 model sessions/day at a 900s floor
    /// versus roughly 2880/day at a 60s floor — a 30x jump in background
    /// model cost per watch for a change that looks like an innocuous
    /// integer edit. If you are here to change this number, you almost
    /// certainly want `MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR`
    /// instead: it lowers the effective floor for a demo or local run
    /// without lowering the shipped default for every user.
    #[test]
    fn poll_interval_floor_is_never_lowered() {
        assert_eq!(MIN_AGENT_WATCH_POLL_INTERVAL_SECS, 900);
    }

    #[test]
    fn validate_rejects_agent_watch_poll_interval_below_floor() {
        // Guards against `MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR`
        // tests below concurrently lowering the effective floor out from
        // under this assertion — see `POLL_INTERVAL_OVERRIDE_TEST_LOCK`.
        let _serial = POLL_INTERVAL_OVERRIDE_TEST_LOCK.lock().unwrap();
        let trigger = AssignmentTrigger::AgentWatch {
            instruction: "Check my inbox for a new email from finance".to_string(),
            poll_interval_secs: MIN_AGENT_WATCH_POLL_INTERVAL_SECS - 1,
            connector_scope: None,
            contract: None,
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        };
        let err = trigger.validate().unwrap_err();
        assert!(
            err.contains(&MIN_AGENT_WATCH_POLL_INTERVAL_SECS.to_string()),
            "got: {err}"
        );
    }

    #[test]
    fn effective_min_agent_watch_poll_interval_secs_defaults_when_unset() {
        let _serial = POLL_INTERVAL_OVERRIDE_TEST_LOCK.lock().unwrap();
        let key = MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR;
        let prev = std::env::var(key).ok();
        std::env::remove_var(key);

        assert_eq!(
            effective_min_agent_watch_poll_interval_secs(),
            MIN_AGENT_WATCH_POLL_INTERVAL_SECS
        );

        if let Some(v) = prev {
            std::env::set_var(key, v);
        }
    }

    #[test]
    fn effective_min_agent_watch_poll_interval_secs_honors_valid_override() {
        let _serial = POLL_INTERVAL_OVERRIDE_TEST_LOCK.lock().unwrap();
        let key = MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR;
        let prev = std::env::var(key).ok();
        std::env::set_var(key, "30");

        assert_eq!(effective_min_agent_watch_poll_interval_secs(), 30);

        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn effective_min_agent_watch_poll_interval_secs_falls_back_on_garbage() {
        let _serial = POLL_INTERVAL_OVERRIDE_TEST_LOCK.lock().unwrap();
        let key = MIN_AGENT_WATCH_POLL_INTERVAL_OVERRIDE_ENV_VAR;
        let prev = std::env::var(key).ok();
        std::env::set_var(key, "not-a-number");

        assert_eq!(
            effective_min_agent_watch_poll_interval_secs(),
            MIN_AGENT_WATCH_POLL_INTERVAL_SECS
        );

        std::env::set_var(key, "0");
        assert_eq!(
            effective_min_agent_watch_poll_interval_secs(),
            MIN_AGENT_WATCH_POLL_INTERVAL_SECS
        );

        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn validate_rejects_malformed_connector_scope() {
        let trigger = AssignmentTrigger::AgentWatch {
            instruction: "Check notion for a new page".to_string(),
            poll_interval_secs: 900,
            connector_scope: Some("Notion Docs!".to_string()),
            contract: None,
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        };
        let err = trigger.validate().unwrap_err();
        assert!(err.contains("connector_scope"), "got: {err}");
    }

    fn sample_agent_watch_assignment() -> Assignment {
        let now = Utc::now();
        Assignment {
            id: "assign-watch".to_string(),
            agent_id: "agent-1".to_string(),
            name: "Watch for finance emails".to_string(),
            instruction: "Summarize the new email from finance.".to_string(),
            working_directory: None,
            trigger: AssignmentTrigger::AgentWatch {
                instruction: "Check my inbox for a new email from finance".to_string(),
                poll_interval_secs: 300,
                connector_scope: None,
                contract: None,
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
            liveness: LivenessState::default(),
            created_ts: now,
            updated_ts: now,
        }
    }

    #[test]
    fn assignment_agent_watch_json_round_trip() {
        let assignment = sample_agent_watch_assignment();
        let json = serde_json::to_string(&assignment).expect("serialize");
        let back: Assignment = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.trigger, assignment.trigger);
        assert!(back.last_event_cursor.is_none());
    }

    #[test]
    fn agent_watch_trigger_back_compat_row_with_no_contract_key_still_deserializes() {
        // A row persisted before `contract` existed — the same additive
        // pattern `connector_scope` already established for this variant
        // (see the field's doc comment): a legacy AgentWatch trigger must
        // keep deserializing with `contract: None`, not fail.
        let json = r#"{
            "type": "AgentWatch",
            "instruction": "Check my inbox for a new email from finance",
            "poll_interval_secs": 900
        }"#;
        let trigger: AssignmentTrigger =
            serde_json::from_str(json).expect("legacy agent-watch trigger");
        assert_eq!(
            trigger,
            AssignmentTrigger::AgentWatch {
                instruction: "Check my inbox for a new email from finance".to_string(),
                poll_interval_secs: 900,
                connector_scope: None,
                contract: None,
                extraction: None,
                extraction_tool: None,
                extraction_args: None,
                extraction_output_schema_declared: false,
            }
        );
    }

    #[test]
    fn agent_watch_trigger_back_compat_row_with_no_extraction_args_key_still_deserializes() {
        // A row persisted before `extraction_args` existed — same additive
        // pattern as `contract` and `extraction_tool` above: a legacy row
        // that names an `extraction_tool` but predates `extraction_args`
        // must still deserialize, with `extraction_args: None`, not fail.
        let json = r#"{
            "type": "AgentWatch",
            "instruction": "Check my inbox for a new email from finance",
            "poll_interval_secs": 900,
            "connector_scope": "gmail",
            "extraction_tool": "list_messages"
        }"#;
        let trigger: AssignmentTrigger =
            serde_json::from_str(json).expect("legacy agent-watch trigger");
        match trigger {
            AssignmentTrigger::AgentWatch {
                extraction_tool,
                extraction_args,
                ..
            } => {
                assert_eq!(extraction_tool.as_deref(), Some("list_messages"));
                assert!(extraction_args.is_none());
            }
            other => panic!("expected AgentWatch trigger, got {:?}", other),
        }
    }

    #[test]
    fn carry_forward_watch_contract_keeps_extraction_args_in_lockstep_with_extraction_tool() {
        fn agent_watch(
            instruction: &str,
            connector_scope: Option<&str>,
            extraction_tool: Option<&str>,
            extraction_args: Option<serde_json::Value>,
        ) -> AssignmentTrigger {
            AssignmentTrigger::AgentWatch {
                instruction: instruction.to_string(),
                poll_interval_secs: 900,
                connector_scope: connector_scope.map(str::to_string),
                contract: None,
                extraction: None,
                extraction_tool: extraction_tool.map(str::to_string),
                extraction_args,
                extraction_output_schema_declared: false,
            }
        }

        let bound_tool = Some("list_messages");
        let bound_args = Some(serde_json::json!({"query": "from:finance"}));
        let prev = agent_watch(
            "Check my inbox for a new email from finance",
            Some("gmail"),
            bound_tool,
            bound_args.clone(),
        );

        // Neither instruction nor connector_scope changed: extraction_tool
        // and extraction_args both carry forward together.
        let (carried, reason) = carry_forward_watch_contract(
            &prev,
            agent_watch(
                "Check my inbox for a new email from finance",
                Some("gmail"),
                None,
                None,
            ),
        );
        assert!(reason.is_none(), "got: {reason:?}");
        match carried {
            AssignmentTrigger::AgentWatch {
                extraction_tool,
                extraction_args,
                ..
            } => {
                assert_eq!(extraction_tool.as_deref(), bound_tool);
                assert_eq!(extraction_args, bound_args);
            }
            other => panic!("expected AgentWatch trigger, got {:?}", other),
        }

        // Every branch that clears the contract must clear extraction_tool
        // and extraction_args in lockstep too — a carried tool name with a
        // dropped argument blob would be silently uncallable.
        let clearing_cases = [
            (
                "instruction changed",
                agent_watch(
                    "Check my inbox for a new email from legal",
                    Some("gmail"),
                    None,
                    None,
                ),
            ),
            (
                "connector_scope changed",
                agent_watch(
                    "Check my inbox for a new email from finance",
                    Some("outlook"),
                    None,
                    None,
                ),
            ),
            (
                "instruction changed, connector_scope changed",
                agent_watch(
                    "Check my inbox for a new email from legal",
                    Some("outlook"),
                    None,
                    None,
                ),
            ),
        ];
        for (expected_reason, next) in clearing_cases {
            let (cleared, reason) = carry_forward_watch_contract(&prev, next);
            assert_eq!(reason, Some(expected_reason));
            match cleared {
                AssignmentTrigger::AgentWatch {
                    extraction_tool,
                    extraction_args,
                    ..
                } => {
                    assert!(
                        extraction_tool.is_none(),
                        "extraction_tool must clear when {expected_reason}"
                    );
                    assert!(
                        extraction_args.is_none(),
                        "extraction_args must clear when {expected_reason}"
                    );
                }
                other => panic!("expected AgentWatch trigger, got {:?}", other),
            }
        }
    }

    #[test]
    fn agent_watch_trigger_tag_discriminant() {
        let trigger = AssignmentTrigger::AgentWatch {
            instruction: "watch for something".to_string(),
            poll_interval_secs: 120,
            connector_scope: None,
            contract: None,
            extraction: None,
            extraction_tool: None,
            extraction_args: None,
            extraction_output_schema_declared: false,
        };
        let json = serde_json::to_string(&trigger).expect("serialize");
        assert!(json.contains("\"type\":\"AgentWatch\""), "got: {json}");
        assert!(
            json.contains("\"instruction\":\"watch for something\""),
            "got: {json}"
        );
        assert!(json.contains("\"poll_interval_secs\":120"), "got: {json}");

        let back: AssignmentTrigger = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, trigger);
    }

    #[test]
    fn assignment_run_status_and_trigger_kind_snake_case() {
        assert_eq!(
            serde_json::to_string(&AssignmentRunStatus::Succeeded).unwrap(),
            "\"succeeded\""
        );
        assert_eq!(
            serde_json::to_string(&AssignmentTriggerKind::Webhook).unwrap(),
            "\"webhook\""
        );
        assert_eq!(AssignmentTriggerKind::Cron.as_str(), "cron");
        assert_eq!(AssignmentTriggerKind::Webhook.as_str(), "webhook");
        assert_eq!(AssignmentTriggerKind::Manual.as_str(), "manual");
        assert_eq!(
            AssignmentTriggerKind::ConnectorEvent.as_str(),
            "connector_event"
        );
    }

    #[test]
    fn assignment_run_json_round_trip() {
        let run = sample_run();
        let json = serde_json::to_string(&run).expect("serialize");
        let back: AssignmentRun = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, run.id);
        assert_eq!(back.assignment_id, run.assignment_id);
        assert_eq!(back.trigger_kind, AssignmentTriggerKind::Cron);
        assert_eq!(back.status, AssignmentRunStatus::Queued);
        assert_eq!(back.thread_id.as_deref(), Some("thread-1"));
    }

    #[test]
    fn assignment_back_compat_minimal_payload() {
        // A minimal serialized shape missing every defaulted field must
        // round-trip via serde defaults: no bindings, output_mode, enabled,
        // next_fire_at, last_run_at, thread_policy, or dedicated_thread_id.
        let json = r#"{
            "id": "a-legacy",
            "agent_id": "agent-1",
            "name": "Legacy",
            "instruction": "do the thing",
            "trigger": { "type": "Webhook", "token": null },
            "created_ts": "2026-01-01T00:00:00Z",
            "updated_ts": "2026-01-01T00:00:00Z"
        }"#;
        let a: Assignment = serde_json::from_str(json).expect("legacy payload");
        assert!(a.bindings.is_empty());
        assert_eq!(a.output_mode, OutputMode::Background);
        assert!(a.enabled, "enabled defaults to true");
        assert!(a.next_fire_at.is_none());
        assert!(a.last_run_at.is_none());
        assert_eq!(
            a.thread_policy,
            AssignmentThreadPolicy::Fresh,
            "a pre-policy row must behave exactly as it did before: fresh thread every fire"
        );
        assert!(a.dedicated_thread_id.is_none());
        assert!(a.working_directory.is_none());
        assert!(a.expires_at.is_none());
        assert!(a.last_event_cursor.is_none());
        assert_eq!(
            a.trigger,
            AssignmentTrigger::Webhook {
                token: None,
                route_name: None,
                secret_ref: None,
                events: vec![],
                filters: None,
                prompt_template: None,
                deliver: WebhookDeliverTarget::Agent,
            },
            "a pre-route-gateway Webhook row must fill in every new field with its default"
        );
    }

    #[test]
    fn webhook_trigger_back_compat_row_with_only_a_token_still_deserializes() {
        // A row stored before route_name/secret_ref/events/filters/
        // prompt_template/deliver existed at all — the entire additive
        // field set must default rather than error.
        let json = r#"{ "type": "Webhook", "token": "legacy-secret" }"#;
        let trigger: AssignmentTrigger =
            serde_json::from_str(json).expect("legacy webhook trigger");
        assert_eq!(
            trigger,
            AssignmentTrigger::Webhook {
                token: Some("legacy-secret".to_string()),
                route_name: None,
                secret_ref: None,
                events: vec![],
                filters: None,
                prompt_template: None,
                deliver: WebhookDeliverTarget::Agent,
            }
        );
    }

    #[test]
    fn working_directory_and_expires_at_round_trip_and_are_omitted_when_absent() {
        let mut assignment = sample_cron_assignment();
        let expiry = Utc::now() + chrono::Duration::days(1);
        assignment.working_directory = Some("/tmp/project".to_string());
        assignment.expires_at = Some(expiry);

        let json = serde_json::to_string(&assignment).expect("serialize");
        let back: Assignment = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.working_directory.as_deref(), Some("/tmp/project"));
        assert_eq!(back.expires_at, Some(expiry));

        // Absent values are skipped on serialize, not emitted as null.
        let fresh = sample_cron_assignment();
        let fresh_json = serde_json::to_string(&fresh).expect("serialize");
        assert!(!fresh_json.contains("working_directory"));
        assert!(!fresh_json.contains("expires_at"));
    }

    #[test]
    fn thread_policy_serializes_snake_case_and_defaults() {
        assert_eq!(
            serde_json::to_string(&AssignmentThreadPolicy::Fresh).unwrap(),
            "\"fresh\""
        );
        assert_eq!(
            serde_json::to_string(&AssignmentThreadPolicy::Main).unwrap(),
            "\"main\""
        );
        assert_eq!(
            serde_json::to_string(&AssignmentThreadPolicy::Dedicated).unwrap(),
            "\"dedicated\""
        );
        assert_eq!(
            AssignmentThreadPolicy::default(),
            AssignmentThreadPolicy::Fresh
        );
    }

    #[test]
    fn dedicated_thread_id_round_trips_and_is_omitted_when_absent() {
        let mut assignment = sample_cron_assignment();
        assignment.thread_policy = AssignmentThreadPolicy::Dedicated;
        assignment.dedicated_thread_id = Some("thread-dedicated-1".to_string());

        let json = serde_json::to_string(&assignment).expect("serialize");
        let back: Assignment = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.thread_policy, AssignmentThreadPolicy::Dedicated);
        assert_eq!(
            back.dedicated_thread_id.as_deref(),
            Some("thread-dedicated-1")
        );

        // Absent dedicated_thread_id is skipped on serialize, not emitted as null.
        let fresh = sample_cron_assignment();
        let fresh_json = serde_json::to_string(&fresh).expect("serialize");
        assert!(!fresh_json.contains("dedicated_thread_id"));
    }

    #[test]
    fn assignment_run_back_compat_minimal_payload() {
        // Only the always-present fields; every optional field absent.
        let json = r#"{
            "id": "r-legacy",
            "assignment_id": "a-legacy",
            "agent_id": "agent-1",
            "trigger_kind": "manual",
            "status": "running",
            "queued_at": "2026-01-01T00:00:00Z"
        }"#;
        let r: AssignmentRun = serde_json::from_str(json).expect("legacy run payload");
        assert_eq!(r.trigger_kind, AssignmentTriggerKind::Manual);
        assert_eq!(r.status, AssignmentRunStatus::Running);
        assert!(r.trigger_payload.is_none());
        assert!(r.output_summary.is_none());
        assert!(r.thread_id.is_none());
        assert!(r.started_ts.is_none());
        assert!(r.finished_ts.is_none());
        assert!(r.error.is_none());
    }

    #[test]
    fn assignment_liveness_field_defaults_on_missing_json() {
        // Simulates an `assignments.json` row written before `liveness`
        // existed — no `liveness` key at all. It must still deserialize
        // (not error out and wipe the user's existing assignments), with
        // `liveness` filled in as `LivenessState::default()`.
        let json = r#"{
            "id": "a-legacy",
            "agent_id": "agent-1",
            "name": "Legacy assignment",
            "instruction": "Do the thing.",
            "trigger": {"type": "Cron", "cron_expr": "0 8 * * 1-5", "is_recurring": true},
            "created_ts": "2026-01-01T00:00:00Z",
            "updated_ts": "2026-01-01T00:00:00Z"
        }"#;
        assert!(
            !json.contains("liveness"),
            "fixture must omit `liveness` to actually exercise the back-compat path"
        );
        let a: Assignment = serde_json::from_str(json).expect("legacy assignment payload without liveness");
        assert_eq!(a.liveness, LivenessState::default());
        assert!(a.liveness.last_evaluated_at.is_none());
        assert_eq!(a.liveness.fire_count, 0);
        assert!(a.liveness.last_quiescence.is_none());
        // And other pre-existing defaults still work as before — this isn't
        // a new gate, just confirming `liveness` didn't disturb them.
        assert!(a.enabled);
        assert!(a.bindings.is_empty());
    }
}
