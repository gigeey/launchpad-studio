import { describe, it, expect } from "vitest";
import { threadDraftKey } from "./threadNavigation";

describe("threadDraftKey", () => {
  it("resolves to the bare agentId for the default thread", () => {
    const agentId = "agent-1";
    const defaultThreadId = "thread-default-abc";
    expect(threadDraftKey(agentId, defaultThreadId, defaultThreadId)).toBe(agentId);
  });

  it("resolves to the bare agentId when the default thread is still the pre-load placeholder", () => {
    const agentId = "agent-1";
    const placeholderDefaultId = `default-${agentId}`;
    expect(threadDraftKey(agentId, placeholderDefaultId, placeholderDefaultId)).toBe(agentId);
  });

  it("namespaces non-default threads under `${agentId}:${threadId}`", () => {
    const agentId = "agent-1";
    const defaultThreadId = "thread-default-abc";
    const otherThreadId = "thread-branch-xyz";
    expect(threadDraftKey(agentId, otherThreadId, defaultThreadId)).toBe(`${agentId}:${otherThreadId}`);
  });

  it("gives distinct keys to distinct non-default threads on the same agent", () => {
    const agentId = "agent-1";
    const defaultThreadId = "thread-default-abc";
    const keyA = threadDraftKey(agentId, "thread-a", defaultThreadId);
    const keyB = threadDraftKey(agentId, "thread-b", defaultThreadId);
    expect(keyA).not.toBe(keyB);
  });
});
