import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "../../lib/api";

export type ArtifactRegenStatus = "idle" | "working" | "done" | "error";

const POLL_INTERVAL_MS = 1500;
const TIMEOUT_MS = 600_000;
const SLOW_MESSAGE = "Still working — this is taking longer than usual…";

export interface UseArtifactRegenResult {
  status: ArtifactRegenStatus;
  error: string | null;
  /** Soft, non-error status text. There's no completion signal besides the
   *  `updated_at` poll (see hook doc), so a run that's still going after a
   *  long stretch can't be told apart from a dead one — this surfaces that
   *  as an informational message rather than promoting it to `error`.
   *  `status` stays `"working"` the whole time. Null except during that
   *  long tail; cleared on every `start()` call and identity change. */
  message: string | null;
  /** Kicks off a run and polls until it lands. Defaults to whole-artifact
   *  regenerate; pass `trigger` to drive the same "working…" / re-render
   *  lifecycle off a different POST (e.g. the chat-to-adjust endpoint) while
   *  reusing the exact same baseline-snapshot-then-poll mechanics. */
  start: (trigger?: () => Promise<unknown>) => Promise<void>;
  /** Re-attaches to a task that was already running before this hook
   *  instance mounted, restoring `status: 'working'` and resuming the same
   *  poll loop `start()` uses — no new POST, since the run is already in
   *  flight. See the doc comment above `resume` itself for the full
   *  rationale and the baseline-snapshot contract callers must uphold. */
  resume: (taskId: string, baselineUpdatedAt: string, baselineChecksum: string) => void;
}

/**
 * Trigger-and-poll hook for whole-artifact regeneration
 * (`POST /agents/{agent}/artifacts/{id}/regenerate`). `start()` kicks off a
 * background agent that replays the artifact's origin prompt, then polls
 * `getArtifact` until `updated_at`/`checksum_sha256` advance past the
 * snapshot taken right before the request. The regenerate endpoint has no
 * paired completion event (it spawns a plain background agent, not a
 * tracked tasklist task), so diffing the artifact record on a timer is the
 * only completion signal available. That means a slow run and a dead run
 * look identical, so this deliberately does not fail fast: it polls for up
 * to 10 minutes, and a completion that lands at any point before that bound
 * still resolves to `'done'`. Only once the bound is hit with no change
 * does it stop polling — and even then it stays out of `error`, instead
 * leaving `status: 'working'` and populating `message` with an
 * informational "still working" note, since a hard error would be a false
 * claim we can't actually back up.
 *
 * Deliberately generic — keyed only by `agentId`/`artifactId`, with no
 * ArtifactRenderer-specific state — so any other surface (e.g. the
 * chat-to-adjust panel, `ArtifactChatPanel`) can drive the same "working…" /
 * re-render lifecycle against the same artifact id. `start()`'s optional
 * `trigger` argument is the seam that makes that reuse possible: it swaps
 * out only the POST that kicks off the background agent, while the
 * baseline-snapshot-then-poll-until-changed mechanics stay identical.
 *
 * `status`/`error`/`message` all live in this hook's own local state, reset
 * on every mount (see the identity effect below) — so unmounting mid-run
 * (e.g. the user navigates away from the artifact) drops the in-progress
 * spinner even though the background task keeps running server-side.
 * `resume()` is the way back in: a caller that re-fetches the artifact and
 * finds a non-null `running_task_id` can hand it to `resume()` to restore
 * `status: 'working'` and pick the same poll loop back up, instead of the
 * task's completion silently going unreported.
 */
export function useArtifactRegen(
  agentId: string | null,
  artifactId: string | null,
): UseArtifactRegenResult {
  const [status, setStatus] = useState<ArtifactRegenStatus>("idle");
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  // Bumped on every start() call and whenever the artifact identity changes,
  // so a stale poll loop from a superseded run never writes state after the
  // fact (a race a plain "am I still mounted" flag wouldn't catch, since
  // start() can be called again on the same mounted instance).
  const generationRef = useRef(0);
  const timeoutRef = useRef<number | null>(null);

  const clearPending = useCallback(() => {
    if (timeoutRef.current !== null) {
      window.clearTimeout(timeoutRef.current);
      timeoutRef.current = null;
    }
  }, []);

  useEffect(() => {
    setStatus("idle");
    setError(null);
    setMessage(null);
    return () => {
      generationRef.current += 1;
      clearPending();
    };
  }, [agentId, artifactId, clearPending]);

  // The poll loop itself, factored out so `start()` and `resume()` schedule
  // the exact same mechanics instead of each carrying their own copy. Takes
  // `agent`/`artifact` as plain string args (rather than closing over the
  // hook's possibly-null `agentId`/`artifactId` params) so both callers —
  // each of which has already null-checked before calling in — can hand
  // them over narrowed, with no re-check needed here.
  const schedulePoll = useCallback(
    (
      generation: number,
      agent: string,
      artifact: string,
      taskId: string | undefined,
      baselineUpdatedAt: string,
      baselineChecksum: string,
      deadline: number,
    ) => {
      const poll = () => {
        if (generationRef.current !== generation) return;
        // The updated_at/checksum diff below remains the sole authority for
        // a 'done' transition — task status only ever short-circuits
        // polling early on a 'failed' run. A lone 'completed' status is not
        // itself a 'done' signal, since the artifact write is what actually
        // matters. Routed through `Promise.resolve().then(...)` (not a
        // direct call) so a synchronous throw from `getArtifactTaskStatus`
        // — not just an async rejection — lands in the `.catch` below
        // instead of escaping the poll tick uncaught.
        const statusCheck = taskId
          ? Promise.resolve()
              .then(() => api.getArtifactTaskStatus(agent, artifact, taskId))
              .catch(() => null)
          : Promise.resolve(null);

        Promise.all([api.getArtifact(agent, artifact), statusCheck])
          .then(([current, taskStatus]) => {
            if (generationRef.current !== generation) return;
            // Checked before the deadline below so a completion that lands
            // on the same tick the bound is crossed still resolves to
            // 'done' — success always wins over the soft timeout, never the
            // reverse.
            if (current.updated_at !== baselineUpdatedAt || current.checksum_sha256 !== baselineChecksum) {
              setStatus("done");
              return;
            }
            if (taskStatus?.status === "failed") {
              // Fail fast instead of waiting out the 10-minute soft
              // timeout — the server already knows the run died.
              // "running"/"unknown" (bogus id, or queried before
              // mark_running lands) are treated identically to a normal
              // in-flight poll, never as an error.
              setStatus("error");
              setError(taskStatus.error ?? "The regeneration run failed.");
              return;
            }
            if (Date.now() >= deadline) {
              // No completion signal after 10 minutes. We can't tell a slow
              // run from a dead one, so stop polling but stay out of
              // 'error' — leave status at 'working' and only add an
              // informational note.
              setMessage(SLOW_MESSAGE);
              return;
            }
            timeoutRef.current = window.setTimeout(poll, POLL_INTERVAL_MS);
          })
          .catch(() => {
            if (generationRef.current !== generation) return;
            if (Date.now() >= deadline) {
              setMessage(SLOW_MESSAGE);
              return;
            }
            // Transient poll failure — keep trying until the deadline
            // instead of surfacing a single flaky fetch as a hard error.
            timeoutRef.current = window.setTimeout(poll, POLL_INTERVAL_MS);
          });
      };

      timeoutRef.current = window.setTimeout(poll, POLL_INTERVAL_MS);
    },
    [],
  );

  const start = useCallback(async (trigger?: () => Promise<unknown>) => {
    if (!agentId || !artifactId) return;
    clearPending();
    const generation = ++generationRef.current;
    setStatus("working");
    setError(null);
    setMessage(null);

    let baselineUpdatedAt: string;
    let baselineChecksum: string;
    let taskId: string | undefined;
    try {
      const before = await api.getArtifact(agentId, artifactId);
      baselineUpdatedAt = before.updated_at;
      baselineChecksum = before.checksum_sha256;
      const triggered = await (trigger ? trigger() : api.regenerateArtifact(agentId, artifactId));
      // Both `regenerateArtifact` and `chatArtifact` (the only two `trigger`
      // callers today) return `{ task_id }` — narrow defensively so a future
      // `trigger` that returns something else just skips the fail-fast path
      // below instead of throwing.
      taskId = (triggered as { task_id?: string } | undefined)?.task_id;
    } catch (err) {
      if (generationRef.current === generation) {
        setStatus("error");
        setError((err as Error).message);
      }
      return;
    }
    if (generationRef.current !== generation) return;

    const deadline = Date.now() + TIMEOUT_MS;
    schedulePoll(generation, agentId, artifactId, taskId, baselineUpdatedAt, baselineChecksum, deadline);
  }, [agentId, artifactId, clearPending, schedulePoll]);

  /**
   * Re-attaches to a task that was already running before this hook
   * instance mounted. `start()`'s baseline-then-poll flow assumes it is the
   * one making the trigger POST, so it snapshots the artifact itself right
   * before firing that request. `resume()` has no POST to make — the run is
   * already in flight, kicked off by a prior mount (or a prior hook
   * instance) that has since unmounted and, with it, canceled the original
   * poll loop via `clearPending()`. So the caller supplies the baseline
   * instead: `updated_at`/`checksum_sha256` from the same `getArtifact`
   * response that surfaced a non-null `running_task_id` in the first place
   * (see `ArtifactPreview`'s mount effect). That keeps this resume free of
   * its own snapshot fetch — polling picks up from data the caller already
   * had on hand — while still diffing against the correct pre-completion
   * state. Once scheduled, the poll loop is identical to `start()`'s; only
   * the seam that gets it running differs.
   */
  const resume = useCallback(
    (taskId: string, baselineUpdatedAt: string, baselineChecksum: string) => {
      if (!agentId || !artifactId) return;
      // Defensive guard mirroring the caller-side one in ArtifactPreview:
      // never stomp an already-running poll loop (whether from `start()` or
      // an earlier `resume()`) with a fresh generation.
      if (status === "working") return;
      clearPending();
      const generation = ++generationRef.current;
      setStatus("working");
      setError(null);
      setMessage(null);
      const deadline = Date.now() + TIMEOUT_MS;
      schedulePoll(generation, agentId, artifactId, taskId, baselineUpdatedAt, baselineChecksum, deadline);
    },
    [agentId, artifactId, status, clearPending, schedulePoll],
  );

  return { status, error, message, start, resume };
}
