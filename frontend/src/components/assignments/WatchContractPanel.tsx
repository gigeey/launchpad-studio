import { AlertTriangle, CheckCircle2, CircleDashed, Sparkles } from "lucide-react";
import type {
  AssignmentWatchHealth,
  WatchContract,
  WatchContractStatus,
  WatchIdentityStrategy,
  WatchMode,
} from "../../types/api";

function joinList(items: string[]): string {
  if (items.length === 0) return "";
  if (items.length === 1) return items[0];
  if (items.length === 2) return `${items[0]} and ${items[1]}`;
  return `${items.slice(0, -1).join(", ")}, and ${items[items.length - 1]}`;
}

function quoteField(field: string): string {
  return `"${field}"`;
}

/** One sentence per rung of the identity fallback ladder
 *  written for this panel's "who is
 *  this?" section — explains what "the same item" means under that rung
 *  rather than naming the rung itself. */
const IDENTITY_STRATEGY_COPY: Record<WatchIdentityStrategy, (contract: WatchContract) => string> = {
  native_id: (contract) =>
    contract.identity.source_field
      ? `Two observations count as the same item when they share the same "${contract.identity.source_field}" value.`
      : "Two observations count as the same item when they share the same source id.",
  composite_native: (contract) =>
    `No single field is a stable id here, so two observations count as the same item when ${joinList(
      contract.identity.fields.map(quoteField),
    )} all match.`,
  content_hash: (contract) =>
    `Nothing on this source is a stable id, so two observations count as the same item when everything in ${joinList(
      contract.identity.fields.map(quoteField),
    )} matches exactly.`,
};

/** One sentence per `WatchMode` explaining the firing behavior it produces,
 *  written for a human rather than as the enum value itself. */
const MODE_COPY: Record<WatchMode, string> = {
  predicate_transition:
    "It fires once when an item starts matching — whether that's a brand-new item that already matches, or an existing item that just changed into matching. It stays quiet the whole time an item keeps matching, and won't fire again unless it leaves and re-enters that state.",
  new_or_changed:
    "It fires whenever an item is new, or any of its material fields change — regardless of whether the item matches the condition right now.",
  new_only: "It fires the first time an item is ever seen, and never again after that — later changes to it are ignored.",
};

const sectionLabelClass = "text-[13px] font-bold text-[var(--modal-text-primary)]";
const bodyTextClass = "text-[14px] text-[var(--modal-text-primary)] leading-relaxed";
const hintTextClass = "text-[13px] text-[var(--modal-text-secondary)] leading-relaxed";
const cardClass =
  "flex flex-col gap-[14px] p-[14px] rounded-[10px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)]";

/** One tier's badge presentation: label, plain-language detail, and a tone
 *  built entirely from theme custom properties (`--success`/`--error`/
 *  `--accent`/`--text-secondary`) so it re-derives correctly under a runtime
 *  custom theme — never a hardcoded hex.
 *
 *  Tone mapping is deliberate, not decorative: `--success` (green) only for
 *  `deterministic` (truly healthy, zero model involvement); `--accent` (the
 *  app's own interactive color — neither success nor error) for
 *  `probabilistic`, since it's degraded-but-usable, not broken;
 *  `--error` (red) for `change_detection_only` and for a poll that fell back
 *  to the model after its extraction plan broke — both honest-failure
 *  states; and a neutral `--text-secondary`/`--border-secondary` tone for "no
 *  tier to report," which must never read as any of the above. */
interface WatchHealthPresentation {
  label: string;
  detail: string;
  toneClass: string;
  Icon: typeof CheckCircle2;
}

// Exported (not just module-local) so `AssignmentLivenessBadge` — the
// generic, all-trigger-kinds liveness badge — can render in the same tone
// language as this panel's own AgentWatch-specific badges below, rather than
// re-deriving equivalent-but-drifting hex/theme-var strings of its own.
export const ERROR_TONE_CLASS = "text-[var(--error)] bg-[var(--error)]/10 border-[var(--error)]/30";
/** Same tone the health badge above uses for `probabilistic` — "degraded but
 *  usable," not an error. Reused by the `model_assisted` extraction
 *  disclosure below so a metered, working watch never reads as broken. */
export const ACCENT_TONE_CLASS = "text-[var(--accent)] bg-[var(--accent)]/10 border-[var(--accent)]/30";
/** Same tone the health badge below uses for `deterministic` — genuinely
 *  good news, not a caveat. Reused by [[ContractRepairedDisclosure]] so a
 *  contract that converged after repair reads as resolved, not merely
 *  "different from before." */
export const SUCCESS_TONE_CLASS = "text-[var(--success)] bg-[var(--success)]/10 border-[var(--success)]/30";
/** Neutral "nothing to report yet" tone — same visual language as
 *  `NEUTRAL_PRESENTATION` below, exported for `AssignmentLivenessBadge`'s
 *  never-evaluated state. */
export const NEUTRAL_TONE_CLASS = "text-[var(--text-secondary)] bg-[var(--bg-tertiary)] border-[var(--border-secondary)]";

/** "Never evaluated" (no scratchpad yet) and "evaluated with no tier"
 *  (contract not yet bound, or bound with no extraction plan) both fall back
 *  to this neutral, non-`Icon`-alarming presentation — deliberately styled
 *  identically to each other but distinctly from all three tiers below, so
 *  neither can be mistaken for "healthy." */
const NEUTRAL_PRESENTATION: Omit<WatchHealthPresentation, "label" | "detail"> = {
  toneClass: NEUTRAL_TONE_CLASS,
  Icon: CircleDashed,
};

/** attempts/ceiling_hit/last_rejection_reason -> one line of copy for both
 *  the health badge (this function) and the panel's own detail paragraph
 *  (`authoringRejectedDetail` below) — kept as the one place that decides
 *  what "rejected N times" reads like, so the two never drift apart. */
function authoringRejectedSummary(status: Extract<WatchContractStatus, { state: "authoring_rejected" }>): string {
  if (status.ceiling_hit) {
    return `Rejected ${status.attempts} time${status.attempts === 1 ? "" : "s"} in a row — authoring has stopped retrying until the instruction or connector scope is edited.`;
  }
  return `Attempt ${status.attempts} was rejected — retrying automatically on the next poll.`;
}

function watchHealthPresentation(health: AssignmentWatchHealth | null | undefined): WatchHealthPresentation {
  if (!health || !health.has_evaluated) {
    return {
      ...NEUTRAL_PRESENTATION,
      label: "Not yet evaluated",
      detail: "This watch hasn't completed a poll yet — its health will appear once it has.",
    };
  }
  // Checked ahead of everything below, and the single source of truth for
  // whether a contract is bound at all — see [[WatchContractStatus]]'s own
  // doc for why this must gate the tier switch below rather than being
  // inferred from `tier`/`extraction_health`, which alone cannot tell "no
  // contract is bound yet" apart from "a contract IS bound but its
  // extraction target can't be frozen." Both used to read as the same
  // "model-assisted, no fixed tier" copy, which is only true of the second.
  const status = health.contract_status;
  if (status?.state === "not_yet_attempted") {
    return {
      ...NEUTRAL_PRESENTATION,
      label: "Not yet evaluated",
      detail: "This watch hasn't completed a poll yet — its health will appear once it has.",
    };
  }
  if (status?.state === "authoring_rejected") {
    return {
      label: status.ceiling_hit ? "Contract proposal rejected — stopped retrying" : "Adjusting contract",
      detail: status.last_rejection_reason
        ? `${authoringRejectedSummary(status)} Last rejection: ${status.last_rejection_reason}`
        : authoringRejectedSummary(status),
      toneClass: status.ceiling_hit ? ERROR_TONE_CLASS : ACCENT_TONE_CLASS,
      Icon: status.ceiling_hit ? AlertTriangle : CircleDashed,
    };
  }
  // Checked ahead of the tier switch below, and independent of it: a poll
  // that takes this branch reports `tier: null` (the engine never claims a
  // tier it didn't actually run), so this is the only way the badge can
  // still tell "fell back to the model this poll" apart from "no tier to
  // report yet." Never fold this into the `default` arm below — that arm's
  // copy is neutral ("nothing to report"), and this state is the opposite:
  // a poll that actively degraded and must read as unhealthy.
  if (health.extraction_plan_degraded) {
    return {
      label: "Degraded — fell back to the model",
      detail:
        health.extraction_plan_degraded_reason ||
        "This watch's extraction plan failed on the last poll and fell back to the model.",
      toneClass: ERROR_TONE_CLASS,
      Icon: AlertTriangle,
    };
  }
  switch (health.tier) {
    case "deterministic":
      return {
        label: "Deterministic",
        detail: "Extracting exact items with no model involved. Healthy.",
        toneClass: SUCCESS_TONE_CLASS,
        Icon: CheckCircle2,
      };
    case "probabilistic":
      return {
        label: "Probabilistic",
        detail: "Extraction worked, but inference was involved — it may be wrong. Degraded, still usable.",
        toneClass: "text-[var(--accent)] bg-[var(--accent)]/10 border-[var(--accent)]/30",
        Icon: Sparkles,
      };
    case "change_detection_only":
      return {
        label: "Change detection only",
        detail:
          health.degraded_reason ||
          "This watch can only tell you something changed, not what changed.",
        toneClass: ERROR_TONE_CLASS,
        Icon: AlertTriangle,
      };
    default:
      // `tier` is architecturally always `null` for a model-assisted watch
      // (its detector never computes an `ExtractionPlan`, so there is no
      // tier to infer) — that is a known, named, expected state, not an
      // unknown one, and `extraction_health` (already shipped on the same
      // `health` object) is what tells the two apart. Reaching this arm at
      // all already means `status?.state === "bound"` (the two branches
      // above return early for `not_yet_attempted`/`authoring_rejected`), so
      // this is never the "no contract yet" case the model-assisted copy
      // used to be misapplied to — only "a contract IS bound, but its
      // extraction target can't be frozen." Rendering the generic "no tier
      // to report yet" copy here contradicts the model-assisted disclosure
      // rendered directly below this badge on the same panel, so this state
      // gets its own copy in the same accent tone as that disclosure
      // instead. The genuinely-unknown case (extraction_health
      // `pending`/absent — e.g. an older persisted assignment, or a poll
      // that hasn't run) still falls through to the neutral copy below.
      if (health.extraction_health === "model_assisted") {
        return {
          label: "Model-assisted — no fixed tier",
          detail:
            "This watch has no extraction tier because its target can't be frozen — a model reads the tool's response and extracts items on every poll instead. See the extraction detail below.",
          toneClass: ACCENT_TONE_CLASS,
          Icon: Sparkles,
        };
      }
      return {
        ...NEUTRAL_PRESENTATION,
        label: "No tier recorded",
        detail: "This watch has evaluated, but has no extraction tier to report yet.",
      };
  }
}

/** Health badge for one `AgentWatch` assignment's extraction tier — the UI
 *  half of the "if the engine detects it, the user sees it" rule
 *  every distinct engine-reported state
 *  (never evaluated, each of the three `Tier`s, evaluated-with-no-tier, and a
 *  poll that fell back to the model after its extraction plan broke) gets
 *  its own label, tone, and detail text, so none can be silently confused
 *  for another — most importantly, "unknown" never reads as "healthy," a
 *  degraded fallback poll never reads as "deterministic," and both
 *  `change_detection_only` and a degraded fallback always carry their
 *  concrete cause instead of a generic "unavailable" string.
 *
 *  Usable standalone (the assignment list row) or inside `WatchContractPanel`
 *  below — every color comes from a theme custom property, never a
 *  hardcoded hex, so a runtime-generated custom theme re-derives it
 *  correctly. */
export function AssignmentWatchHealthBadge({ health }: { health: AssignmentWatchHealth | null | undefined }) {
  const { label, detail, toneClass, Icon } = watchHealthPresentation(health);
  const tierAttr = !health?.has_evaluated
    ? "unevaluated"
    : health.extraction_plan_degraded
      ? "degraded"
      : health.tier ?? "none";

  return (
    <div
      data-testid="watch-health-badge"
      data-watch-health-tier={tierAttr}
      className={`flex flex-col gap-[4px] p-[10px] rounded-[8px] border ${toneClass}`}
    >
      <div className="flex items-center gap-[6px] text-[13px] font-bold">
        <Icon size={14} className="flex-shrink-0" />
        <span data-testid="watch-health-label">{label}</span>
      </div>
      <p className="text-[12px] leading-relaxed" data-testid="watch-health-detail">
        {detail}
      </p>
    </div>
  );
}

/** Sub-line shown inside "Who counts as the same item" when
 *  `watchHealth.identity_probe_inconclusive` is `true`: the bound
 *  `native_id` contract's two-poll stability probe never actually confirmed
 *  the key is stable, only that it was assumed. This is NOT an error state —
 *  the watch is bound and running normally — so it's styled with
 *  `ACCENT_TONE_CLASS`, the same "degraded but usable, not broken" tone
 *  `ModelAssistedExtractionDisclosure` and the `probabilistic` health tier
 *  use, never the error tone.
 *
 *  Rendered directly beneath `identity.rationale` — the authored-time
 *  sentence that may itself claim the key was "verified" — so this caveat
 *  sits alongside that claim rather than being swallowed by it: the
 *  authored rationale is a point-in-time assertion by the authoring run,
 *  while this reflects the actual, current per-poll probe result. Neither
 *  gates the other's visibility. */
function IdentityProbeInconclusiveDisclosure({ reason }: { reason: string | null | undefined }) {
  return (
    <div className="flex flex-col gap-[6px]" data-testid="watch-identity-probe-inconclusive">
      <span
        className={`inline-flex w-fit items-center gap-[6px] px-[8px] py-[3px] rounded-full text-[12px] font-bold border ${ACCENT_TONE_CLASS}`}
      >
        <CircleDashed size={12} className="flex-shrink-0" />
        Identity not yet confirmed
      </span>
      <p className={bodyTextClass}>
        This watch's item-identity key was assumed rather than verified across polls — if the key turns out not to
        be stable, duplicate or missed items are possible.
      </p>
      <p className={hintTextClass} data-testid="watch-identity-probe-inconclusive-reason">
        {reason || "This watch's identity probe came back inconclusive and no further detail was recorded."}
      </p>
    </div>
  );
}

/** Copy for [`ExtractionProvenanceDisclosure`]'s sub-line, keyed by the
 *  server-derived `watchHealth.extraction_provenance` value — never
 *  re-derived from `tier`/`extraction_path` client-side. */
const EXTRACTION_PROVENANCE_COPY: Record<"declared_schema" | "parsed_from_text", string> = {
  declared_schema:
    "This plan is backed by a schema the server declared for this tool — the shape is a contractual promise, not a one-time guess.",
  parsed_from_text:
    "This plan was reconstructed by parsing JSON out of a response the server never declared a schema for — still zero model calls, but the shape isn't guaranteed and could drift without notice.",
};

/** Provenance sub-line for [`FrozenContractDisclosure`]: pairs with that
 *  disclosure's "no model reviews this before it runs" claim, which is TRUE
 *  regardless of provenance (extraction really is zero-model-call either
 *  way) but says nothing about DRIFT RISK, which provenance predicts and
 *  cost does not — a plan backed by a server-declared schema is a
 *  contractual promise about shape; one reconstructed by parsing text out of
 *  an undeclared response is a parsed guess that can drift without the
 *  server ever signaling it.
 *
 *  Styled with the same accent tone [`IdentityProbeInconclusiveDisclosure`]
 *  uses — a confidence caveat on a working watch, never the error tone.
 *
 *  Renders a neutral, no-guarantee sentence (never silence) when
 *  `provenance` is absent — a response captured before this field existed,
 *  or a poll whose last extraction path attests to neither shape — since
 *  silence here would read as "nothing to disclose" rather than "unknown." */
function ExtractionProvenanceDisclosure({ provenance }: { provenance: string | null | undefined }) {
  const copy =
    provenance === "declared_schema" || provenance === "parsed_from_text"
      ? EXTRACTION_PROVENANCE_COPY[provenance]
      : "This watch's plan provenance was not recorded, so whether its shape is a declared guarantee or a parsed guess is unknown.";

  return (
    <p className="text-[12px] leading-relaxed text-[var(--accent)]" data-testid="watch-extraction-provenance">
      {copy}
    </p>
  );
}

/** "Frozen contract" disclosure: shown only once an authoring run has
 *  self-reported a
 *  read-only tool to call directly on every future poll instead of running
 *  the model. This is the human half of that feature's safety story — the
 *  engine's only defense against a frozen write tool is the authoring
 *  prompt's instruction not to self-report one, so a person must actually be
 *  able to read what got frozen and confirm it only reads data.
 *
 *  Renders nothing when `tool` is absent — a watch that never froze a tool
 *  (or explicitly declined to) shows no empty box here; its reason, if any,
 *  belongs to the health badge above, not this disclosure.
 *
 *  `provenance` renders [`ExtractionProvenanceDisclosure`] right below the
 *  "no model reviews this" claim — that claim is a cost guarantee true for
 *  every provenance, so the sub-line is what tells a declared-schema plan
 *  apart from a parsed-text one instead of both reading identically here. */
function FrozenContractDisclosure({
  tool,
  args,
  provenance,
}: {
  tool: string;
  args: unknown;
  provenance?: string | null;
}) {
  return (
    <div className="flex flex-col gap-[6px]" data-testid="watch-frozen-contract">
      <span className={sectionLabelClass}>Frozen tool call</span>
      <p className={bodyTextClass}>
        Every poll calls <code className="font-mono text-[var(--modal-text-primary)]" data-testid="watch-frozen-contract-tool">{tool}</code>{" "}
        directly with the arguments below — no model reviews this before it runs.
      </p>
      <pre
        className="font-mono text-[12px] text-[var(--modal-text-primary)] whitespace-pre-wrap bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[8px] p-[10px]"
        data-testid="watch-frozen-contract-args"
      >
        {JSON.stringify(args ?? null, null, 2)}
      </pre>
      <ExtractionProvenanceDisclosure provenance={provenance} />
      <p className={hintTextClass}>
        This runs on a fixed schedule with nobody reviewing it each time — confirm it only reads data before trusting
        this watch to run unattended.
      </p>
    </div>
  );
}

/** `model_assisted` extraction disclosure — the row that doesn't exist today.
 *  May show the same frozen tool + args as [`FrozenContractDisclosure`], but
 *  must never claim "no model reviews this before it runs": a model reads
 *  the tool's response and extracts the items on every single poll, because
 *  no extraction plan could ever be authored for this source. Styled with
 *  `ACCENT_TONE_CLASS` — the same "degraded but usable" tone the health
 *  badge above uses for `probabilistic` — deliberately not the error tone,
 *  since a watch spending metered model calls per poll is working as
 *  designed, not broken. */
function ModelAssistedExtractionDisclosure({
  tool,
  args,
  modelCallsToday,
  reason,
}: {
  tool: string | null | undefined;
  args: unknown;
  modelCallsToday: number;
  reason: string | null | undefined;
}) {
  return (
    <div className="flex flex-col gap-[6px]" data-testid="watch-model-assisted-disclosure">
      <span
        className={`inline-flex w-fit items-center gap-[6px] px-[8px] py-[3px] rounded-full text-[12px] font-bold border ${ACCENT_TONE_CLASS}`}
        data-testid="watch-model-assisted-badge"
      >
        <Sparkles size={12} className="flex-shrink-0" />
        {tool ? "Frozen target · model-extracted" : "Model-extracted"} — {modelCallsToday} model calls today
      </span>
      <p className={bodyTextClass}>
        {tool ? (
          <>
            Every poll calls{" "}
            <code className="font-mono text-[var(--modal-text-primary)]" data-testid="watch-model-assisted-tool">
              {tool}
            </code>{" "}
            with the arguments below, but a model reads what it returns and extracts the items — every poll, not just
            the first.
          </>
        ) : (
          "A model reads this source and extracts the items on every poll — no frozen extraction target exists for it."
        )}
      </p>
      {tool && (
        <pre
          className="font-mono text-[12px] text-[var(--modal-text-primary)] whitespace-pre-wrap bg-[var(--modal-bg)] border border-[var(--modal-border-primary)] rounded-[8px] p-[10px]"
          data-testid="watch-model-assisted-args"
        >
          {JSON.stringify(args ?? null, null, 2)}
        </pre>
      )}
      {reason && (
        <p className={hintTextClass} data-testid="watch-extraction-health-reason">
          {reason}
        </p>
      )}
    </div>
  );
}

/** `degraded` extraction disclosure: an extraction plan existed but
 *  direct-invoke failed on the last poll, so this watch fell back to the
 *  model. Distinct from the health badge's own degraded-tier rendering above
 *  (which reports *tier* health) — this occupies the same slot the frozen-
 *  tool disclosure would otherwise take, so a reader scanning this section
 *  never sees a stale "frozen tool, no model" claim once a watch has
 *  actually degraded. */
function DegradedExtractionDisclosure({ reason }: { reason: string | null | undefined }) {
  return (
    <div className="flex flex-col gap-[6px]" data-testid="watch-extraction-degraded">
      <span
        className={`inline-flex w-fit items-center gap-[6px] px-[8px] py-[3px] rounded-full text-[12px] font-bold border ${ERROR_TONE_CLASS}`}
      >
        <AlertTriangle size={12} className="flex-shrink-0" />
        Degraded — fell back to the model
      </span>
      <p className={bodyTextClass} data-testid="watch-extraction-health-reason">
        {reason || "This watch's extraction fell back to the model and no further detail was recorded."}
      </p>
    </div>
  );
}

/** `pending` extraction disclosure: no poll has completed yet, so nothing
 *  true can be claimed about model involvement in either direction — neither
 *  "no model reviews this" nor "a model reviews this every poll." */
function PendingExtractionDisclosure() {
  return (
    <div className="flex flex-col gap-[6px]" data-testid="watch-extraction-pending">
      <span className={sectionLabelClass}>Extraction not yet evaluated</span>
      <p className={hintTextClass}>
        This watch hasn't completed a poll yet, so there's nothing to disclose yet about whether a model is involved
        in extracting its items.
      </p>
    </div>
  );
}

/** Dispatches the bottom-of-panel extraction disclosure by `extraction_health`
 *  alone — never by tool-name truthiness. This is the fix for the bug where
 *  a merely-frozen tool name (which says nothing about whether a model still
 *  runs every poll) produced the same "no model reviews this" claim
 *  regardless of health. `deterministic` is the ONLY state permitted to make
 *  that claim; every other state gets its own honest copy instead. Absent
 *  `watchHealth`/`extraction_health` (a response captured before the field
 *  existed) falls back to the `pending` rendering — the only safe default,
 *  since it asserts nothing either way. */
function ExtractionHealthDisclosure({
  health,
  tool,
  args,
}: {
  health: AssignmentWatchHealth | null | undefined;
  tool: string | null | undefined;
  args: unknown;
}) {
  const extractionHealth = health?.extraction_health ?? "pending";
  switch (extractionHealth) {
    case "deterministic":
      return tool ? (
        <FrozenContractDisclosure tool={tool} args={args} provenance={health?.extraction_provenance} />
      ) : null;
    case "model_assisted":
      return (
        <ModelAssistedExtractionDisclosure
          tool={tool}
          args={args}
          modelCallsToday={health?.model_calls_today ?? 0}
          reason={health?.extraction_health_reason}
        />
      );
    case "degraded":
      return <DegradedExtractionDisclosure reason={health?.extraction_health_reason} />;
    case "pending":
    default:
      return <PendingExtractionDisclosure />;
  }
}

/** Convergence banner shown at the top of a bound contract's body when
 *  `WatchContractStatus.Bound.bound_after_repairs` is non-null: THIS SAME
 *  contract only bound after `repairCount` earlier proposals were rejected.
 *
 *  Exists because authoring can now self-correct within a single poll (or
 *  across a later one) after a loud rejection — without this, a user who
 *  saw that rejection gets no follow-up once it quietly converges, and has
 *  no way to tell "this rejection is still outstanding" apart from "this
 *  was already fixed," which has previously cost a false investigation.
 *  Styled in the success tone (never accent/error) — this is unambiguously
 *  good news, not a lingering caveat, so it must never read the same as an
 *  outstanding [[authoringRejectedSummary]] disclosure. */
function ContractRepairedDisclosure({ repairCount }: { repairCount: number }) {
  return (
    <div
      className={`flex items-center gap-[8px] px-[10px] py-[8px] rounded-[8px] border ${SUCCESS_TONE_CLASS}`}
      data-testid="watch-contract-repaired"
    >
      <CheckCircle2 size={14} className="flex-shrink-0" />
      <p className="text-[13px] leading-relaxed">
        This contract bound after repairing {repairCount} rejected proposal{repairCount === 1 ? "" : "s"} — any
        earlier rejection notice for this watch no longer applies.
      </p>
    </div>
  );
}

/** Read-only rendering of a watch's `WatchContract`
 *  organized around the three plain-
 *  language questions the contract exists to answer: who counts as the same
 *  item, what counts as a real change, and what makes it fire.
 *
 *  Deliberately display-only — editing, amendment, and re-keying are out of
 *  scope for v1 — so this component renders no `input`,
 *  `textarea`, `button`, or other editable control anywhere in its tree,
 *  only text and a native `<details>` disclosure for the curious to read the
 *  raw predicate expression.
 *
 *  `contract` is `null`/`undefined` for a watch that hasn't completed its
 *  first poll yet, or one whose authoring is still retrying — the legitimate
 *  pre-authoring state, not an error — so that case renders an explanatory
 *  placeholder instead of an empty box. Which placeholder is entirely driven
 *  by `watchHealth.contract_status` (see [`WatchContractStatus`]'s own doc):
 *  `"not_yet_attempted"` and `"authoring_rejected"` render DIFFERENT, never
 *  both-at-once, copy — the bug this component used to have was rendering a
 *  generic "learning this source" placeholder unconditionally alongside a
 *  health badge that could independently say "model-assisted, no fixed
 *  tier" for the exact same rejected-but-not-yet-bound poll, two statements
 *  that contradict each other (one says "nothing tried yet," the other says
 *  "tried and permanently degraded"). `contract_status` is now the ONLY
 *  thing this component's top-level branch reads, so that pair can no
 *  longer both render.
 *
 *  `watchHealth` renders as an [`AssignmentWatchHealthBadge`] above the
 *  contract body (or the empty-state placeholder) whenever it's supplied —
 *  independent of whether `contract` itself is bound yet, since a watch can
 *  be evaluated with no tier to report (still authoring) just as easily as
 *  it can have a bound contract with no extraction plan configured.
 *
 *  `watchHealth.contract_status.state === "bound"` with a non-null
 *  `bound_after_repairs` renders [`ContractRepairedDisclosure`] at the top
 *  of the bound body — the CONVERGENCE half of the fix: a contract that only
 *  bound after some rejected proposals says so explicitly, so an earlier
 *  rejection a user saw doesn't read as still-outstanding once it resolves.
 *
 *  `watchHealth.identity_probe_inconclusive` renders
 *  [`IdentityProbeInconclusiveDisclosure`] inside "Who counts as the same
 *  item," right below the identity rationale — `true` exactly means this
 *  watch's stability probe never confirmed its key, only assumed it.
 *  `false`/absent (including responses captured before the field existed)
 *  renders nothing here; never inferred from any other field.
 *
 *  `extractionTool`/`extractionArgs` feed [`ExtractionHealthDisclosure`],
 *  which renders one of four mutually-exclusive states keyed on
 *  `watchHealth.extraction_health` — `deterministic` (the frozen tool + args
 *  run with zero model calls, the only state allowed to say so),
 *  `model_assisted` (the same frozen target, but a model extracts its items
 *  every poll), `degraded` (fell back to the model after the extraction plan
 *  broke), or `pending` (nothing to claim yet). `extractionTool` truthiness
 *  alone never gates the "no model reviews this" claim. */
export function WatchContractPanel({
  contract,
  watchHealth,
  extractionTool,
  extractionArgs,
}: {
  contract: WatchContract | null | undefined;
  watchHealth?: AssignmentWatchHealth | null;
  /** Bare tool name self-reported at authoring time, or `null`/undefined
   *  when this watch has no frozen target — see [`ExtractionHealthDisclosure`]. */
  extractionTool?: string | null;
  /** The arguments `extractionTool` was self-reported as having been called
   *  with — rendered as-is, whatever shape it is. */
  extractionArgs?: unknown;
}) {
  const status = watchHealth?.contract_status;

  // `status` is the single source of truth for these two placeholders —
  // never `!contract` alone, and never re-derived from `watchHealth`'s tier/
  // extraction fields. Falls through to the `!contract` check below only
  // when `watchHealth` (or its `contract_status`) wasn't supplied at all —
  // e.g. a caller that hasn't migrated yet — in which case the pre-existing
  // "learning this source" copy is still the safest default.
  if (status?.state === "authoring_rejected") {
    return (
      <div className={cardClass} data-testid="watch-contract-authoring-rejected">
        {watchHealth !== undefined && <AssignmentWatchHealthBadge health={watchHealth} />}
        <p className={bodyTextClass} data-testid="watch-contract-authoring-rejected-detail">
          {authoringRejectedSummary(status)}
        </p>
        {status.last_rejection_reason && (
          <p className={hintTextClass} data-testid="watch-contract-authoring-rejected-reason">
            Last rejection: {status.last_rejection_reason}
          </p>
        )}
      </div>
    );
  }

  if (!contract) {
    return (
      <div className={cardClass} data-testid="watch-contract-empty">
        {watchHealth !== undefined && <AssignmentWatchHealthBadge health={watchHealth} />}
        <p className={hintTextClass}>Learning this source — the contract will appear after the first poll.</p>
      </div>
    );
  }

  const materialFields = contract.change.material_fields;
  const trackedFields = Object.keys(contract.fields);
  const ignoredFields = trackedFields.filter((field) => !materialFields.includes(field));
  // Rendered server-side from the typed predicate that actually executes, and
  // carried on the response-only health payload rather than on the contract
  // itself (the contract is fingerprinted — see `predicate_expr`'s doc).
  const predicateExpr = watchHealth?.predicate_expr;
  const boundAfterRepairs = status?.state === "bound" ? status.bound_after_repairs : null;

  return (
    <div className={cardClass} data-testid="watch-contract-panel">
      {watchHealth !== undefined && <AssignmentWatchHealthBadge health={watchHealth} />}
      {boundAfterRepairs != null && <ContractRepairedDisclosure repairCount={boundAfterRepairs} />}
      <p className={hintTextClass}>
        Watching <code className="font-mono text-[var(--modal-text-primary)]">{contract.source.kind}</code>
        {contract.source.ref && (
          <>
            {" "}
            (ref <code className="font-mono text-[var(--modal-text-primary)]">{contract.source.ref}</code>)
          </>
        )}
      </p>

      <div className="flex flex-col gap-[6px]">
        <span className={sectionLabelClass}>Who counts as the same item</span>
        <p className={bodyTextClass}>{IDENTITY_STRATEGY_COPY[contract.identity.strategy](contract)}</p>
        <p className={bodyTextClass} data-testid="watch-contract-rationale">
          {contract.identity.rationale || "(no rationale recorded)"}
        </p>
        {contract.identity.format && (
          <p className={hintTextClass}>
            Values are also checked against{" "}
            <code className="font-mono text-[var(--modal-text-primary)]">{contract.identity.format}</code> — anything
            that doesn't match is quarantined instead of treated as new.
          </p>
        )}
        {watchHealth?.identity_probe_inconclusive === true && (
          <IdentityProbeInconclusiveDisclosure reason={watchHealth.identity_probe_inconclusive_reason} />
        )}
      </div>

      <div className="flex flex-col gap-[6px]">
        <span className={sectionLabelClass}>What counts as a change</span>
        {materialFields.length > 0 ? (
          <p className={bodyTextClass}>
            Only a change to {joinList(materialFields.map(quoteField))} counts as something new.
          </p>
        ) : (
          <p className={hintTextClass}>No fields are declared material yet.</p>
        )}
        {ignoredFields.length > 0 && (
          <p className={hintTextClass}>
            Everything else being tracked ({joinList(ignoredFields.map(quoteField))}) can change freely without
            triggering anything.
          </p>
        )}
        {contract.change.version_hint_field && (
          <p className={hintTextClass}>
            <code className="font-mono text-[var(--modal-text-primary)]">{contract.change.version_hint_field}</code>{" "}
            is used as a cheap signal for when to bother re-checking an item — it never decides whether this fires.
          </p>
        )}
      </div>

      <div className="flex flex-col gap-[6px]">
        <span className={sectionLabelClass}>When it fires</span>
        <p className={bodyTextClass} data-testid="watch-contract-predicate">
          {contract.predicate.natural_language || "(no condition recorded)"}
        </p>
        <p className={hintTextClass}>{MODE_COPY[contract.mode]}</p>
        <details className="mt-[2px]">
          <summary className="text-[12px] text-[var(--modal-text-secondary)] cursor-pointer select-none">
            Show raw expression
          </summary>
          {predicateExpr ? (
            <code
              className="block mt-[6px] font-mono text-[12px] text-[var(--modal-text-primary)] whitespace-pre-wrap"
              data-testid="watch-contract-expr"
            >
              {predicateExpr}
            </code>
          ) : (
            <p className={`mt-[6px] ${hintTextClass}`} data-testid="watch-contract-expr-unavailable">
              No expression available — this watch's condition uses comparisons the readable expression syntax can't
              represent. The plain-language description above is authoritative.
            </p>
          )}
        </details>
      </div>

      <ExtractionHealthDisclosure health={watchHealth} tool={extractionTool} args={extractionArgs} />
    </div>
  );
}
