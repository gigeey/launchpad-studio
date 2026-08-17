import { useCallback, useEffect, useRef } from "react";
import type { Attachment } from "../types/api";
import { useQueuedSendStore } from "../stores/queuedSendStore";

type SendFn = (content: string, attachmentIds?: string[], attachments?: Attachment[]) => void;

export interface QueuedPart {
  content: string;
  attachmentIds?: string[];
  attachments?: Attachment[];
}

export interface QueueState {
  /** Conversation identity (e.g. `${agentId}:${threadId}`) this queue belongs to. */
  key: string;
  /** Whether that conversation's agent currently has an active run. */
  busy: boolean;
  queue: QueuedPart[];
}

export interface QueueTransition {
  action: "none" | "flush" | "abandon";
  content?: string;
  attachmentIds?: string[];
  attachments?: Attachment[];
  /** Only set for `"abandon"` — the identity the queued content belonged to. */
  prevKey?: string;
  nextQueue: QueuedPart[];
}

/** Joins queued parts into a single send: content on its own paragraph per
 *  part, attachments concatenated in submission order. Empty arrays collapse
 *  to `undefined` so callers can keep treating "no attachments" uniformly. */
export function combineQueuedParts(pending: QueuedPart[]): [string, string[] | undefined, Attachment[] | undefined] {
  const content = pending.map((p) => p.content).join("\n\n");
  const attachmentIds = pending.flatMap((p) => p.attachmentIds ?? []);
  const attachments = pending.flatMap((p) => p.attachments ?? []);
  return [content, attachmentIds.length > 0 ? attachmentIds : undefined, attachments.length > 0 ? attachments : undefined];
}

/**
 * Pure resolver behind `useQueuedMessageSend` (split out so it's testable
 * without a hook-rendering harness — mirrors `resolveStreamingThreadId` in
 * `stores/chatStore.ts`).
 *
 * Given the previously-recorded queue state and the newly-observed
 * `(key, busy)` pair, decides what — if anything — should happen to whatever
 * is currently queued:
 *  - same conversation, run just finished (`busy` true → false): `"flush"`
 *    the queue to that same conversation.
 *  - conversation identity changed out from under the queue: `"abandon"`
 *    whatever was queued for the outgoing `prevKey` (the caller decides how
 *    — resend, or hand back as an unsent draft) rather than silently
 *    attributing it to the new conversation or dropping it.
 *  - anything else (still busy, or nothing was queued): `"none"`.
 */
export function resolveQueueTransition(prev: QueueState, next: { key: string; busy: boolean }): QueueTransition {
  if (prev.key === next.key) {
    if (prev.busy && !next.busy && prev.queue.length > 0) {
      const [content, attachmentIds, attachments] = combineQueuedParts(prev.queue);
      return { action: "flush", content, attachmentIds, attachments, nextQueue: [] };
    }
    return { action: "none", nextQueue: prev.queue };
  }
  if (prev.queue.length > 0) {
    const [content, attachmentIds, attachments] = combineQueuedParts(prev.queue);
    return { action: "abandon", content, attachmentIds, attachments, prevKey: prev.key, nextQueue: [] };
  }
  return { action: "none", nextQueue: [] };
}

const EMPTY_QUEUE: QueuedPart[] = [];

/**
 * Bootstraps the `{ key, busy, queue }` a fresh (or freshly-remounted)
 * `useQueuedMessageSend` should start reconciling from, given whatever queue
 * survived in `queuedSendStore` from a prior mount.
 *
 * A persisted queue can only exist because the run for `key` was still
 * active when its contents were queued — the store is only ever written to
 * while `busy` is true (see `useQueuedMessageSend`). So a non-empty
 * persisted queue is treated as if `busy: true` had last been observed,
 * regardless of the live `liveBusy` passed in: that lets the very first
 * `resolveQueueTransition` call after mount see a true->false edge (and
 * flush) if the run finished while this hook's owner was unmounted, or stay
 * queued (via the "still busy" no-op branch) if it's still running — instead
 * of unconditionally force-sending on mount/unmount the way the hook used to.
 */
export function bootstrapQueueState(key: string, persistedQueue: QueuedPart[], liveBusy: boolean): QueueState {
  return { key, busy: persistedQueue.length > 0 ? true : liveBusy, queue: persistedQueue };
}

/**
 * Wraps a `ChatInput`-style `onSend` so a message submitted while the agent
 * already has a run in flight is held locally instead of being sent right
 * away.
 *
 * Why this matters: the server appends a user message to the transcript the
 * instant it's received, regardless of whether a run is currently active —
 * only the *response* to an in-flight run is appended once that run
 * finishes. So sending a follow-up mid-stream lands it in the transcript
 * BEFORE the still-streaming reply it was actually sent after, which reads
 * as two user messages and two agent messages in the wrong order. Holding
 * the message client-side and only calling `send` once the run ends keeps
 * transcript order honest. Several messages queued during the same run are
 * combined into a single send (joined with a blank line, attachments
 * concatenated in order) rather than firing as separate follow-up runs.
 *
 * `key` identifies the conversation whose `busy` flag is being watched (e.g.
 * `${agentId}:${threadId}`). Only pass a `key` that can change *without*
 * this hook's owning component unmounting (mirrors `useReadyLatch`'s
 * `resetKey`) — e.g. ChatView, which deliberately stays mounted while the
 * selected agent/thread changes. If your surface instead unmounts/remounts
 * per conversation (give the JSX a `key` prop for that), a constant `key`
 * here is fine and `onAbandon` is unnecessary.
 *
 * When `key` changes while something is still queued, `send` can no longer
 * be trusted to land in the right place (it may resolve "which agent" from
 * ambient store state that's already moved on to the new conversation by the
 * time the flush runs) — so `onAbandon`, if provided, receives the outgoing
 * key's queued content instead of it being silently sent or dropped.
 *
 * Returns the wrapped send function plus the live list of currently-queued
 * parts (`queue`) and a way to drop one before it's sent (`removeQueued`), so
 * callers can surface more than a bare "N message(s) queued" count — e.g. a
 * click-to-expand preview of what's actually waiting, so the queued marker
 * doesn't force you to remember what you typed.
 */
export function useQueuedMessageSend(
  key: string,
  busy: boolean,
  send: SendFn,
  onAbandon?: (prevKey: string, content: string, attachmentIds?: string[], attachments?: Attachment[]) => void,
): { send: SendFn; queue: QueuedPart[]; removeQueued: (index: number) => void } {
  // Sourced from `queuedSendStore` (not local state) so it survives this
  // hook's owner unmounting and remounting — see the store's docstring for
  // why a plain `useState`/`useRef` isn't enough.
  const queue = useQueuedSendStore((s) => s.queues[key] ?? EMPTY_QUEUE);
  const setStoreQueue = useQueuedSendStore((s) => s.setQueue);

  const stateRef = useRef<QueueState>(bootstrapQueueState(key, queue, busy));

  const sendRef = useRef(send);
  sendRef.current = send;
  const abandonRef = useRef(onAbandon);
  abandonRef.current = onAbandon;

  // Reactive path: flush once the watched conversation's run finishes, or
  // hand off (or best-effort send) whatever was queued when the identity
  // being watched changes out from under us. Also the reconciliation path on
  // mount/remount — `stateRef`'s initial `busy` came from `bootstrapQueueState`,
  // which treats a restored non-empty queue as "was busy", so this first run
  // naturally flushes it if the run has since finished, or leaves it queued
  // (via the "still busy" no-op branch) if it's still going.
  useEffect(() => {
    const prev = stateRef.current;
    const transition = resolveQueueTransition(prev, { key, busy });
    if (transition.action === "flush") {
      sendRef.current(transition.content!, transition.attachmentIds, transition.attachments);
    } else if (transition.action === "abandon") {
      if (abandonRef.current) {
        abandonRef.current(transition.prevKey!, transition.content!, transition.attachmentIds, transition.attachments);
      } else {
        sendRef.current(transition.content!, transition.attachmentIds, transition.attachments);
      }
    }
    if (prev.key !== key) setStoreQueue(prev.key, []);
    // `resolveQueueTransition` hands back `prev.queue` itself (same
    // reference) for a "none" transition on an unchanged key, so this skips
    // a no-op store write on every busy-flag flip that has nothing queued —
    // the common case (most runs finish with nothing queued behind them).
    if (transition.nextQueue !== prev.queue || prev.key !== key) setStoreQueue(key, transition.nextQueue);
    stateRef.current = { key, busy, queue: transition.nextQueue };
  }, [key, busy, setStoreQueue]);

  const queuedSend = useCallback(
    (content: string, attachmentIds?: string[], attachments?: Attachment[]) => {
      if (stateRef.current.busy) {
        const next = [...stateRef.current.queue, { content, attachmentIds, attachments }];
        stateRef.current.queue = next;
        setStoreQueue(stateRef.current.key, next);
        return;
      }
      sendRef.current(content, attachmentIds, attachments);
    },
    [setStoreQueue],
  );

  // Lets the queued-messages preview drop a mistaken entry before it ever
  // gets combined and sent — e.g. the click-to-view popover's per-row "X".
  const removeQueued = useCallback(
    (index: number) => {
      const next = stateRef.current.queue.filter((_, i) => i !== index);
      stateRef.current.queue = next;
      setStoreQueue(stateRef.current.key, next);
    },
    [setStoreQueue],
  );

  return { send: queuedSend, queue, removeQueued };
}
