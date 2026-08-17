import { useRef } from "react";

/**
 * Monotonic "is this content actually ready to show" latch, as opposed to a
 * raw `loading` flag (which flaps true/false on every request, including
 * unrelated background mutations that share the same store field — e.g.
 * starting a task re-triggers the same `loading` boolean the initial task
 * list fetch used).
 *
 * Latches `true` the first time either (a) data is already present, or (b) a
 * loading cycle has been observed to start and finish. Once true, it stays
 * true regardless of any later `loading` flap — so a background refetch on
 * an already-visible view never re-hides its content.
 *
 * Pass `resetKey` when the same component instance can pivot to a different
 * data identity without unmounting (e.g. ChatView staying mounted while the
 * selected agent/thread changes) — the latch re-arms so the previous
 * identity's content isn't shown as a stand-in for the new one while it
 * loads.
 */
export function useReadyLatch(
  dataPresent: boolean,
  loading: boolean,
  resetKey?: string | number,
): boolean {
  const ready = useRef(false);
  const sawLoading = useRef(false);
  const key = useRef(resetKey);

  if (key.current !== resetKey) {
    key.current = resetKey;
    ready.current = false;
    sawLoading.current = false;
  }

  if (loading) sawLoading.current = true;
  if (!ready.current && (dataPresent || (sawLoading.current && !loading))) {
    ready.current = true;
  }

  return ready.current;
}
