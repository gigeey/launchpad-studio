/**
 * Tests for the pendingAsyncFormIdByChannel slice of chatStore.
 *
 * Covers set, clear, multi-channel isolation, and reset.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

vi.mock("../lib/api", () => ({
  getAgents: vi.fn().mockResolvedValue([]),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  getAgent: vi.fn().mockResolvedValue(null),
}));

import { useChatStore, isFormMinimized } from "./chatStore";

beforeEach(() => {
  useChatStore.getState().reset();
});

describe("pendingAsyncFormIdByChannel", () => {
  it("starts empty", () => {
    expect(useChatStore.getState().pendingAsyncFormIdByChannel).toEqual({});
  });

  it("setPendingAsyncFormId stores the form id under the channel key", () => {
    useChatStore.getState().setPendingAsyncFormId("project:abc", "form-1");
    expect(useChatStore.getState().pendingAsyncFormIdByChannel["project:abc"]).toBe("form-1");
  });

  it("clearPendingAsyncFormId removes the entry", () => {
    useChatStore.getState().setPendingAsyncFormId("project:abc", "form-1");
    useChatStore.getState().clearPendingAsyncFormId("project:abc");
    expect(useChatStore.getState().pendingAsyncFormIdByChannel["project:abc"]).toBeUndefined();
  });

  it("multiple channels are tracked independently", () => {
    useChatStore.getState().setPendingAsyncFormId("project:abc", "form-1");
    useChatStore.getState().setPendingAsyncFormId("project:xyz", "form-2");
    expect(useChatStore.getState().pendingAsyncFormIdByChannel["project:abc"]).toBe("form-1");
    expect(useChatStore.getState().pendingAsyncFormIdByChannel["project:xyz"]).toBe("form-2");
    useChatStore.getState().clearPendingAsyncFormId("project:abc");
    expect(useChatStore.getState().pendingAsyncFormIdByChannel["project:abc"]).toBeUndefined();
    expect(useChatStore.getState().pendingAsyncFormIdByChannel["project:xyz"]).toBe("form-2");
  });

  it("reset clears all entries", () => {
    useChatStore.getState().setPendingAsyncFormId("project:abc", "form-1");
    useChatStore.getState().reset();
    expect(useChatStore.getState().pendingAsyncFormIdByChannel).toEqual({});
  });

  it("setPendingAsyncFormId clears a stale minimized flag on the same channel key — a freshly-arriving form must never inherit the previous form's minimized state", () => {
    useChatStore.getState().setFormMinimized("project:abc", undefined, true);
    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "project:abc", undefined)).toBe(true);

    useChatStore.getState().setPendingAsyncFormId("project:abc", "form-2");

    expect(isFormMinimized(useChatStore.getState().minimizedFormByKey, "project:abc", undefined)).toBe(false);
    expect(useChatStore.getState().pendingAsyncFormIdByChannel["project:abc"]).toBe("form-2");
  });
});
