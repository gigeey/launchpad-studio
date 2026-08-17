import { AlertTriangle, CheckCircle2, CircleDashed, Clock } from "lucide-react";
import type { AssignmentTrigger, AssignmentWatchHealth, QuiescenceReason } from "../../types/api";
import { AssignmentWatchHealthBadge, ERROR_TONE_CLASS, NEUTRAL_TONE_CLASS, SUCCESS_TONE_CLASS } from "./WatchContractPanel";

// ---------------------------------------------------------------------------
// One liveness badge for EVERY assignment trigger kind — the UI half of the
// "one health contract across all three trigger types" rule
// Answers, for any row, the
// four liveness questions: am I bound? when did I last evaluate? what did I
// see? why have I not fired? "Am I bound?" and "what did I see?" are answered
// by the AgentWatch-specific `AssignmentWatchHealthBadge` (kept fully
// unchanged below, for `AgentWatch` rows only — see its own doc); "when did
// I last evaluate?" and "why have I not fired?" are answered here, for every
// trigger kind, off `AssignmentWatchHealth`'s generic liveness fields
// (`last_evaluated_at`/`quiescence_reason`/`quiescence_explanation`).
//
// Deliberately does NOT render for a `Webhook` trigger: the backend's
// `Assignment.liveness` is only ever advanced by the schedule-runner tick
// loop (`Cron`/`ConnectorEvent`/`AgentWatch`) — `Webhook` fires are pushed
// in over HTTP with no tick, so `watch_health_for` reports it as perpetually
// "never evaluated" even after it has fired many times. Showing that would
// be actively misleading, not merely incomplete, so `AssignmentList` keeps
// `Webhook` rows on the plain active/paused/expired pill instead.
// ---------------------------------------------------------------------------

/** Same "just now / Xm ago / Xh ago / Xd ago / calendar date" convention
 *  `WorkflowTile`/`SearchBar` already use elsewhere in this codebase — kept
 *  as a local copy rather than a new shared utility, matching how those two
 *  do it. */
function formatRelativeTime(iso: string | null | undefined): string {
  if (!iso) return "never";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "never";
  const diffMs = Date.now() - date.getTime();
  const diffSec = Math.floor(diffMs / 1000);
  if (diffSec < 60) return "just now";
  const diffMin = Math.floor(diffSec / 60);
  if (diffMin < 60) return `${diffMin}m ago`;
  const diffHr = Math.floor(diffMin / 60);
  if (diffHr < 24) return `${diffHr}h ago`;
  const diffDay = Math.floor(diffHr / 24);
  if (diffDay < 30) return `${diffDay}d ago`;
  return date.toLocaleDateString([], { month: "short", day: "numeric" });
}

/** Short, at-a-glance label per `QuiescenceReason` kind — distinct from
 *  `quiescence_explanation`'s full sentence, which is rendered as this
 *  badge's detail line right below the label. */
const QUIESCENCE_LABEL: Record<QuiescenceReason["kind"], string> = {
  expired: "Expired",
  not_due: "Waiting",
  fire_failed: "Fire failed",
  server_not_connected: "Server not connected",
  no_live_handle: "No live connection",
  poll_failed: "Poll failed",
  cursor_unresolved: "Nothing to compare",
  agent_watch_contract_not_bound: "Contract not bound",
};

/** `not_due` is the one quiescence reason that is a completely normal,
 *  expected state (the assignment just hasn't reached its own schedule
 *  yet) — every other reason is something actually blocking a fire and
 *  deserves the same attention-grabbing tone the AgentWatch badge above
 *  uses for its own honest-failure states. */
function isExpectedWait(reason: QuiescenceReason): boolean {
  return reason.kind === "not_due";
}

type LivenessRowState = "never_evaluated" | "quiescent" | "fired";

interface LivenessPresentation {
  state: LivenessRowState;
  label: string;
  toneClass: string;
  Icon: typeof CheckCircle2;
}

function livenessPresentation(health: AssignmentWatchHealth | null | undefined): LivenessPresentation {
  if (!health || !health.last_evaluated_at) {
    return { state: "never_evaluated", label: "Never evaluated", toneClass: NEUTRAL_TONE_CLASS, Icon: CircleDashed };
  }
  const reason = health.quiescence_reason;
  if (reason) {
    return {
      state: "quiescent",
      label: QUIESCENCE_LABEL[reason.kind] ?? "Not fired",
      toneClass: isExpectedWait(reason) ? NEUTRAL_TONE_CLASS : ERROR_TONE_CLASS,
      Icon: isExpectedWait(reason) ? Clock : AlertTriangle,
    };
  }
  return { state: "fired", label: "Fired", toneClass: SUCCESS_TONE_CLASS, Icon: CheckCircle2 };
}

/** Generic liveness badge for a `Cron`/`ConnectorEvent` row — never rendered
 *  for `AgentWatch` (that trigger kind keeps its own richer
 *  `AssignmentWatchHealthBadge` tier presentation instead; see
 *  `AssignmentLivenessBadge` below). Visually and structurally distinguishes
 *  all three row states this task exists to keep apart: a never-evaluated
 *  assignment (`toneClass`/`Icon` neutral, no explanation line) reads
 *  nothing like an evaluated one that correctly chose not to fire (a
 *  `quiescence_explanation` line always accompanies that state), which in
 *  turn reads nothing like a recent fire (success tone). */
function GenericLivenessBadge({ health }: { health: AssignmentWatchHealth | null | undefined }) {
  const { state, label, toneClass, Icon } = livenessPresentation(health);
  return (
    <div
      data-testid="assignment-liveness-badge"
      data-liveness-state={state}
      className={`flex flex-col gap-[4px] p-[10px] rounded-[8px] border ${toneClass}`}
    >
      <div className="flex items-center gap-[6px] text-[13px] font-bold">
        <Icon size={14} className="flex-shrink-0" />
        <span data-testid="assignment-liveness-label">{label}</span>
      </div>
      {health?.quiescence_explanation && (
        <p className="text-[12px] leading-relaxed" data-testid="assignment-liveness-explanation">
          {health.quiescence_explanation}
        </p>
      )}
      <p className="text-[11px] text-[var(--text-tertiary)]" data-testid="assignment-liveness-last-checked">
        Last checked: {formatRelativeTime(health?.last_evaluated_at)}
      </p>
    </div>
  );
}

/** Just the "Last checked: …" line, appended below the unmodified
 *  `AssignmentWatchHealthBadge` for an `AgentWatch` row — that badge already
 *  carries its own tier-derived explanation text, so only the relative
 *  timestamp is missing to satisfy "every assignment row" showing one. */
function LastCheckedLine({ health }: { health: AssignmentWatchHealth | null | undefined }) {
  return (
    <p
      className="text-[11px] text-[var(--text-tertiary)] px-[2px]"
      data-testid="assignment-liveness-last-checked"
    >
      Last checked: {formatRelativeTime(health?.last_evaluated_at)}
    </p>
  );
}

interface AssignmentLivenessBadgeProps {
  health: AssignmentWatchHealth | null | undefined;
  triggerType: AssignmentTrigger["type"];
}

/** The one liveness badge `AssignmentList` renders for every `Cron`/
 *  `ConnectorEvent`/`AgentWatch` row (never `Webhook` — see this module's
 *  own doc). For `AgentWatch` it renders the existing, UNCHANGED
 *  `AssignmentWatchHealthBadge` plus a last-checked line; for `Cron`/
 *  `ConnectorEvent` it renders `GenericLivenessBadge`, built entirely from
 *  the generic liveness fields every trigger kind now carries. */
export function AssignmentLivenessBadge({ health, triggerType }: AssignmentLivenessBadgeProps) {
  if (triggerType === "AgentWatch") {
    return (
      <div className="flex flex-col gap-[4px]" data-testid="assignment-liveness-badge-agent-watch">
        <AssignmentWatchHealthBadge health={health} />
        <LastCheckedLine health={health} />
      </div>
    );
  }
  return <GenericLivenessBadge health={health} />;
}
