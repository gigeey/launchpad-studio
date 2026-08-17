/**
 * Draft store integration tests for per-thread ChatInput drafts in ChatView.
 *
 * ChatView computes a `draftKey` via `threadDraftKey` (see lib/threadNavigation.ts)
 * and feeds it to ChatInput as `conversationId`, which flushes the outgoing
 * thread's draft and restores the incoming thread's draft whenever that key
 * changes (ChatInput's own "reset editor when conversationId changes" effect).
 * These tests verify the store side of that contract: each thread's draft is
 * independent, the default thread keeps using the pre-threads bare-agentId
 * key so old drafts aren't orphaned, and switching threads doesn't clobber it.
 */

import { beforeEach, describe, it, expect } from "vitest";
import { useDraftStore } from "../stores/draftStore";
import { threadDraftKey } from "../lib/threadNavigation";

function resetDraftStore() {
  localStorage.clear();
  useDraftStore.setState({ drafts: {}, draftHtml: {}, draftAttachments: {} });
}

describe("ChatView per-thread drafts", () => {
  beforeEach(resetDraftStore);

  it("keeps a legacy bare-agentId draft intact and readable for the default thread", () => {
    const agentId = "agent-legacy";
    const defaultThreadId = "thread-default-1";
    const { setDraft } = useDraftStore.getState();

    // Simulate a draft saved before per-thread scoping existed.
    setDraft(agentId, "typed before threads shipped");

    const key = threadDraftKey(agentId, defaultThreadId, defaultThreadId);
    expect(key).toBe(agentId);
    expect(useDraftStore.getState().drafts[key]).toBe("typed before threads shipped");
  });

  it("gives a non-default thread its own draft, independent of the default thread's", () => {
    const agentId = "agent-multi";
    const defaultThreadId = "thread-default-1";
    const otherThreadId = "thread-other-2";
    const { setDraft } = useDraftStore.getState();

    const defaultKey = threadDraftKey(agentId, defaultThreadId, defaultThreadId);
    const otherKey = threadDraftKey(agentId, otherThreadId, defaultThreadId);

    setDraft(defaultKey, "main thread draft");
    setDraft(otherKey, "side thread draft");

    const state = useDraftStore.getState();
    expect(state.drafts[defaultKey]).toBe("main thread draft");
    expect(state.drafts[otherKey]).toBe("side thread draft");
  });

  it("switching threads (simulated flush-then-restore) preserves both drafts", () => {
    const agentId = "agent-switch";
    const defaultThreadId = "thread-default-1";
    const otherThreadId = "thread-other-2";
    const { setDraft, clearDraft } = useDraftStore.getState();
    const defaultKey = threadDraftKey(agentId, defaultThreadId, defaultThreadId);
    const otherKey = threadDraftKey(agentId, otherThreadId, defaultThreadId);

    // User types on the default thread, then switches away — ChatInput's
    // conversationId-change effect flushes the outgoing draft via onUnmount.
    setDraft(defaultKey, "half-typed on main");

    // User types on the other thread, then switches back to default.
    setDraft(otherKey, "half-typed on side");

    // Neither flush should have touched the other thread's entry.
    const state = useDraftStore.getState();
    expect(state.drafts[defaultKey]).toBe("half-typed on main");
    expect(state.drafts[otherKey]).toBe("half-typed on side");

    // Sending from the default thread only clears that thread's draft.
    clearDraft(defaultKey);
    const afterSend = useDraftStore.getState();
    expect(afterSend.drafts[defaultKey]).toBeUndefined();
    expect(afterSend.drafts[otherKey]).toBe("half-typed on side");
  });

  it("deleting a non-default thread's draft never touches the default thread's bare-agentId draft", () => {
    const agentId = "agent-delete";
    const defaultThreadId = "thread-default-1";
    const deletedThreadId = "thread-deleted-2";
    const { setDraft, clearDraft } = useDraftStore.getState();

    setDraft(agentId, "main thread draft survives");
    setDraft(threadDraftKey(agentId, deletedThreadId, defaultThreadId), "draft on the thread being deleted");

    // Mirrors ThreadsPanel's handleDelete cleanup.
    clearDraft(threadDraftKey(agentId, deletedThreadId, defaultThreadId));

    const state = useDraftStore.getState();
    expect(state.drafts[agentId]).toBe("main thread draft survives");
    expect(state.drafts[threadDraftKey(agentId, deletedThreadId, defaultThreadId)]).toBeUndefined();
  });
});
