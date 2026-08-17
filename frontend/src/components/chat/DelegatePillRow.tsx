import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Loader2, X, XCircle } from "lucide-react";
import { useChatStore, inFlightKey, type RunningDelegateInfo } from "../../stores/chatStore";
import { cancelDelegate } from "../../lib/api";

/** Visible-pill cap — see `DelegatePillRow`'s doc comment. Deliberately not a
 *  scroller: once more delegates are running than fit, the rest collapse
 *  into a "+N more" indicator next to the kill-all button instead of paging
 *  or scrolling. */
const MAX_VISIBLE_PILLS = 3;

/** How long a pill stays in "Stopping…" before it's treated as stuck.
 *
 *  This is a REGRESSION DETECTOR, not a UX tuning knob. In normal operation
 *  the backend's `delegate.complete` SSE event removes this pill from
 *  `runningDelegatesByThread` (via `endDelegateRun`) well under this window,
 *  the instant the cancellation actually finishes server-side. If this
 *  timeout ever fires during real use, the cancel request was accepted but
 *  no completion event ever arrived — that means the backend's completion
 *  emit is broken (a stuck subprocess, a dropped event), not that the
 *  network or the cancel itself is merely slow. Do not raise this value to
 *  paper over a slow op, and do not delete it as "dead code that never
 *  fires" — it firing IS the signal that something upstream broke. */
const STOP_TIMEOUT_MS = 10_000;

function formatElapsed(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const m = Math.floor(totalSeconds / 60);
  const s = totalSeconds % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

type KillState = "idle" | "stopping" | "stuck";

function DelegatePill({
  delegationId,
  info,
  now,
}: {
  delegationId: string;
  info: RunningDelegateInfo;
  now: number;
}) {
  const [killState, setKillState] = useState<KillState>("idle");

  // Regression-detector timer (see STOP_TIMEOUT_MS above) — armed the
  // instant this pill enters "stopping", cleared on unmount (which also
  // covers the normal-completion path: `delegate.complete` removes this
  // delegation from the store, DelegatePillRow stops rendering this pill,
  // and this component — timer included — unmounts).
  useEffect(() => {
    if (killState !== "stopping") return;
    const timeoutId = setTimeout(() => setKillState("stuck"), STOP_TIMEOUT_MS);
    return () => clearTimeout(timeoutId);
  }, [killState]);

  const handleKill = useCallback(() => {
    // The button is disabled for every state but "idle" (see below), so a
    // second click can't re-enter this — no need to guard against
    // re-arming the timer or double-firing the cancel request.
    setKillState("stopping");
    cancelDelegate(delegationId).catch(() => {
      // The cancel REQUEST itself failed (network/HTTP error) — a distinct,
      // faster failure than the completion-timeout above, so surface it
      // immediately instead of waiting out the full window.
      setKillState("stuck");
    });
  }, [delegationId]);

  const isStopping = killState === "stopping";
  const isStuck = killState === "stuck";

  return (
    <div
      data-testid={`delegate-pill-${delegationId}`}
      data-kill-state={killState}
      className={`inline-flex items-center gap-[6px] px-[8px] py-[4px] rounded-full border text-[12px] ${
        isStuck
          ? "bg-[var(--error-bg)] border-[var(--error-border)]"
          : "bg-[var(--bg-secondary)] border-[var(--border-primary)]"
      }`}
    >
      <span className="truncate max-w-[110px] text-[var(--text-primary)]" title={info.delegateName}>
        {info.delegateName}
      </span>
      <span className={`tabular-nums ${isStuck ? "text-[var(--error)]" : "text-[var(--text-tertiary)]"}`}>
        {isStuck ? "Stop failed" : isStopping ? "Stopping…" : formatElapsed(now - info.startedAt)}
      </span>
      <button
        type="button"
        onClick={handleKill}
        disabled={killState !== "idle"}
        aria-label={isStuck ? "Stop failed" : isStopping ? "Stopping…" : `Stop ${info.delegateName}`}
        data-testid={`delegate-pill-kill-${delegationId}`}
        className={`flex-shrink-0 w-[16px] h-[16px] flex items-center justify-center rounded-full transition-colors ${
          killState === "idle"
            ? "hover:bg-red-500/15 cursor-pointer text-[var(--text-tertiary)] hover:text-red-500"
            : "cursor-not-allowed text-[var(--text-tertiary)]"
        }`}
      >
        {isStuck ? (
          <AlertTriangle size={10} />
        ) : isStopping ? (
          <Loader2 size={10} className="animate-spin" />
        ) : (
          <X size={10} />
        )}
      </button>
    </div>
  );
}

/** Live-activity row for async `Delegate` runs — one pill per delegation
 *  currently in flight in `threadId` of `agentId`, sourced from
 *  `runningDelegatesByThread` (subscribed narrowly by composite key, see
 *  `inFlightKey`, so a delegate ticking on another thread never re-renders
 *  this one). Only ASYNC delegations get a pill: a sync `Delegate` call
 *  blocks the parent's own turn, so the existing typing indicator already
 *  covers it and it never appears in `runningDelegatesByThread` at all (see
 *  that map's own doc comment) — a pill here specifically means "running
 *  behind your back while you can keep chatting".
 *
 *  Each pill shows the delegate's name, a live-ticking elapsed time (one
 *  shared `setInterval` owned by this row, not one per pill — see the `now`
 *  state below), and a kill button. Killing is optimistic-but-honest: the
 *  clicked pill flips to "Stopping…" immediately, but is only ever REMOVED
 *  by the backend's `delegate.complete` SSE event actually arriving and
 *  clearing it from the store (`endDelegateRun`) — never removed locally on
 *  click. See `STOP_TIMEOUT_MS` for what happens if that event never shows
 *  up.
 *
 *  Caps at `MAX_VISIBLE_PILLS` visible pills (no scroller); anything beyond
 *  that collapses into a "+N more" counter. The kill-all button sits next to
 *  that counter and ONLY appears alongside it — with 3 or fewer delegates
 *  every pill already has its own reachable kill button, so kill-all only
 *  earns its place once overflow hides some delegates behind "+N more" and
 *  becomes the sole way to reach them. It is thread-scoped only: it
 *  enumerates this thread's own delegation ids and calls the single-
 *  delegation cancel endpoint once per id — there is no session-wide
 *  cancel-all endpoint to call instead, by deliberate backend design (that
 *  primitive is reserved for teardown, not exposed as a user-facing
 *  button). */
export function DelegatePillRow({ agentId, threadId }: { agentId: string; threadId?: string }) {
  const key = useMemo(() => inFlightKey(agentId, threadId), [agentId, threadId]);
  const delegates = useChatStore((s) => s.runningDelegatesByThread.get(key));

  const hasDelegates = !!delegates && delegates.size > 0;
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!hasDelegates) return;
    const intervalId = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(intervalId);
  }, [hasDelegates]);

  if (!delegates || delegates.size === 0) return null;

  const entries = Array.from(delegates.entries()).sort((a, b) => a[1].startedAt - b[1].startedAt);
  const visible = entries.slice(0, MAX_VISIBLE_PILLS);
  const overflowCount = entries.length - visible.length;

  const handleKillAll = () => {
    for (const [delegationId] of entries) {
      cancelDelegate(delegationId).catch(() => {});
    }
  };

  return (
    <div
      className="flex items-center gap-[6px] flex-wrap px-[10px] @sm:px-[16px] @lg:px-[20px] pb-[8px]"
      data-testid="delegate-pill-row"
    >
      {visible.map(([delegationId, info]) => (
        <DelegatePill key={delegationId} delegationId={delegationId} info={info} now={now} />
      ))}
      {overflowCount > 0 && (
        <div className="inline-flex items-center gap-[8px]">
          <span className="text-[11px] text-[var(--text-tertiary)]" data-testid="delegate-pill-overflow">
            +{overflowCount} more
          </span>
          <button
            type="button"
            onClick={handleKillAll}
            data-testid="delegate-pill-kill-all"
            className="inline-flex items-center gap-[3px] text-[11px] text-[var(--text-tertiary)] hover:text-red-500 transition-colors cursor-pointer"
          >
            <XCircle size={11} />
            Stop all
          </button>
        </div>
      )}
    </div>
  );
}
