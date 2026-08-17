/**
 * Focus-path store integration tests for per-thread focus mode in ChatView.
 *
 * ChatView computes a `draftKey` via `threadDraftKey` (see lib/threadNavigation.ts)
 * and now reuses it as the focus-path store key — both the read that feeds the
 * outgoing message's `focus_path` and the `focusStoreKey` prop handed to
 * ChatInput's picker. That makes focus mode per-thread: the same agent can point
 * different threads at different projects at once. These tests verify the store
 * side of that contract: each thread's focus path is independent, the default
 * thread keeps using the pre-threads bare-agentId key so old focus paths aren't
 * orphaned, switching threads doesn't clobber it, and deleting a thread drops
 * only its own entry.
 */

import { beforeEach, describe, it, expect } from "vitest";
import { useFocusPathStore } from "../stores/focusPathStore";
import { threadDraftKey } from "../lib/threadNavigation";

function resetFocusStore() {
  localStorage.clear();
  useFocusPathStore.setState({ focusPaths: {} });
}

describe("ChatView per-thread focus paths", () => {
  beforeEach(resetFocusStore);

  it("keeps a legacy bare-agentId focus path intact and readable for the default thread", () => {
    const agentId = "agent-legacy";
    const defaultThreadId = "thread-default-1";
    const { setFocusPath } = useFocusPathStore.getState();

    // Simulate a focus path saved before per-thread scoping existed, when the
    // store was keyed by bare agent id.
    setFocusPath(agentId, "/projects/legacy");

    const key = threadDraftKey(agentId, defaultThreadId, defaultThreadId);
    expect(key).toBe(agentId);
    expect(useFocusPathStore.getState().focusPaths[key]).toBe("/projects/legacy");
  });

  it("gives a non-default thread its own focus path, independent of the default thread's", () => {
    const agentId = "agent-multi";
    const defaultThreadId = "thread-default-1";
    const otherThreadId = "thread-other-2";
    const { setFocusPath } = useFocusPathStore.getState();

    const defaultKey = threadDraftKey(agentId, defaultThreadId, defaultThreadId);
    const otherKey = threadDraftKey(agentId, otherThreadId, defaultThreadId);

    // The whole point of the feature: one agent, two threads, two projects.
    setFocusPath(defaultKey, "/projects/alpha");
    setFocusPath(otherKey, "/projects/beta");

    const state = useFocusPathStore.getState();
    expect(state.focusPaths[defaultKey]).toBe("/projects/alpha");
    expect(state.focusPaths[otherKey]).toBe("/projects/beta");
  });

  it("switching threads never clobbers the other thread's focus path", () => {
    const agentId = "agent-switch";
    const defaultThreadId = "thread-default-1";
    const otherThreadId = "thread-other-2";
    const { setFocusPath, clearFocusPath } = useFocusPathStore.getState();
    const defaultKey = threadDraftKey(agentId, defaultThreadId, defaultThreadId);
    const otherKey = threadDraftKey(agentId, otherThreadId, defaultThreadId);

    setFocusPath(defaultKey, "/projects/alpha");
    setFocusPath(otherKey, "/projects/beta");

    // Clearing one thread's focus (the picker's "×") leaves the other alone.
    clearFocusPath(otherKey);
    const state = useFocusPathStore.getState();
    expect(state.focusPaths[defaultKey]).toBe("/projects/alpha");
    expect(state.focusPaths[otherKey]).toBeUndefined();
  });

  it("deleting a non-default thread drops only its focus entry, never the default thread's bare-agentId one", () => {
    const agentId = "agent-delete";
    const defaultThreadId = "thread-default-1";
    const deletedThreadId = "thread-deleted-2";
    const { setFocusPath, clearFocusPath } = useFocusPathStore.getState();

    setFocusPath(agentId, "/projects/main-survives");
    setFocusPath(threadDraftKey(agentId, deletedThreadId, defaultThreadId), "/projects/doomed");

    // Mirrors ChatView's handleDeleteThread cleanup (clearFocusPath(goneKey)).
    clearFocusPath(threadDraftKey(agentId, deletedThreadId, defaultThreadId));

    const state = useFocusPathStore.getState();
    expect(state.focusPaths[agentId]).toBe("/projects/main-survives");
    expect(state.focusPaths[threadDraftKey(agentId, deletedThreadId, defaultThreadId)]).toBeUndefined();
  });
});
