import { create } from "zustand";
import type { QueuedPart } from "../hooks/useQueuedMessageSend";

/**
 * Backs `useQueuedMessageSend`'s per-conversation queue of not-yet-sent
 * messages held while that conversation's agent run is in flight.
 *
 * Why this needs to live outside the hook's own component state: `ChatView`
 * unmounts on every navigation away from Chat/Home (it's swapped out by the
 * router, not just re-keyed — see `App.tsx`'s route tree), so a `useRef`
 * scoped to that component instance loses its queue the moment the user
 * leaves the page. The hook used to paper over this with an unmount effect
 * that force-sent whatever was queued — but that ignored the run's `busy`
 * state entirely, so navigating away mid-run submitted the queued message
 * immediately instead of waiting for the run to finish, defeating the whole
 * point of queuing (see the docstring on `useQueuedMessageSend`). Keeping
 * the queue here instead lets the hook rehydrate it on remount and decide
 * correctly whether to flush (the run has since finished) or keep waiting
 * (still busy) — see `bootstrapQueueState`.
 *
 * Deliberately NOT persisted to localStorage (unlike `draftStore`): whether
 * a conversation is still "busy" is learned purely from live SSE events
 * (`chatStore`'s `inFlightByAgent`, populated by `run_started`/`run_ended`),
 * which starts empty on every fresh load — a full app reload has no reliable
 * way to know a queued message's run is still actually in progress, so
 * rehydrating a persisted queue post-reload could flush it immediately
 * instead of waiting. In-memory is enough to fix the reported bug (queue
 * surviving same-session navigation away from and back to the chat).
 */
interface QueuedSendState {
  queues: Record<string, QueuedPart[]>;
  setQueue: (key: string, queue: QueuedPart[]) => void;
}

export const useQueuedSendStore = create<QueuedSendState>((set) => ({
  queues: {},
  setQueue: (key, queue) =>
    set((state) => {
      if (queue.length === 0) {
        const { [key]: _, ...rest } = state.queues;
        return { queues: rest };
      }
      return { queues: { ...state.queues, [key]: queue } };
    }),
}));
