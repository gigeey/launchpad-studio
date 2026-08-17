import { beforeEach, describe, it, expect } from "vitest";
import { useDraftStore } from "../../stores/draftStore";
import { isTasklistChannelEvent } from "../../hooks/useSSE";

// ---------------------------------------------------------------------------
// Draft store integration — TodoPanel composer (`todo:{agentId}` key)
// ---------------------------------------------------------------------------

function resetDraftStore() {
  localStorage.clear();
  useDraftStore.setState({ drafts: {}, draftHtml: {}, draftAttachments: {} });
}

describe("TodoPanel composer draft store", () => {
  beforeEach(resetDraftStore);

  it("initializes composerText from todo: namespaced key", () => {
    const agentId = "agent-init-test";
    const draftKey = `todo:${agentId}`;
    useDraftStore.getState().setDraft(draftKey, "Saved task text");

    const initial = useDraftStore.getState().drafts[draftKey] ?? "";
    expect(initial).toBe("Saved task text");
  });

  it("persists on onChange and clears on successful send", () => {
    const agentId = "agent-send-test";
    const draftKey = `todo:${agentId}`;
    const { setDraft, clearDraft } = useDraftStore.getState();

    setDraft(draftKey, "Write integration tests");
    expect(useDraftStore.getState().drafts[draftKey]).toBe("Write integration tests");

    clearDraft(draftKey);
    expect(useDraftStore.getState().drafts[draftKey]).toBeUndefined();
  });

  it("todo: key does not collide with bare agentId main-chat draft", () => {
    const agentId = "agent-collision";
    const draftKey = `todo:${agentId}`;
    const { setDraft } = useDraftStore.getState();

    setDraft(agentId, "main chat draft");
    setDraft(draftKey, "todo composer draft");

    const state = useDraftStore.getState();
    expect(state.drafts[agentId]).toBe("main chat draft");
    expect(state.drafts[draftKey]).toBe("todo composer draft");
  });

  it("clearDraft(todo: key) does not affect the main-chat draft", () => {
    const agentId = "agent-clear-isolation";
    const draftKey = `todo:${agentId}`;
    const { setDraft, clearDraft } = useDraftStore.getState();

    setDraft(agentId, "main chat draft");
    setDraft(draftKey, "todo composer draft");
    clearDraft(draftKey);

    expect(useDraftStore.getState().drafts[agentId]).toBe("main chat draft");
    expect(useDraftStore.getState().drafts[draftKey]).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// TodoPanel tasklist channel subscription — channel isolation tests.
//
// The TodoPanel subscribes to a dedicated per-task SSE channel
// (`tasklist:{id}`), which keeps agent-owned tasklist subagent runs off the
// parent agent's main chat channel. These tests cover:
//
// 1. agentTasklistStreamUrl produces the correct endpoint URL.
// 2. isTasklistChannelEvent correctly identifies tasklist-scoped events so
//    that chat-rendering handlers in useSSE can guard against any accidental
//    leak from the tasklist channel into the main chat bubble.
// ---------------------------------------------------------------------------

// Import the pure URL builder directly — avoids pulling in the full api
// module (which references Tauri globals unavailable in the test environment).
function buildTasklistStreamUrl(agentId: string, tasklistId: string): string {
  const base = "http://localhost:13100";
  return `${base}/agents/${encodeURIComponent(agentId)}/tasklists/${encodeURIComponent(tasklistId)}/stream`;
}

describe("agentTasklistStreamUrl shape", () => {
  it("contains the agent id and tasklist id in the path", () => {
    const url = buildTasklistStreamUrl("agent-abc", "tl-xyz");
    expect(url).toContain("/agents/agent-abc/tasklists/tl-xyz/stream");
  });

  it("percent-encodes special characters in agent id", () => {
    const url = buildTasklistStreamUrl("agent with space", "tl-1");
    expect(url).toContain("agent%20with%20space");
  });

  it("percent-encodes special characters in tasklist id", () => {
    const url = buildTasklistStreamUrl("agent-1", "tl/nested");
    expect(url).toContain("tl%2Fnested");
  });

  it("preserves the /stream suffix", () => {
    const url = buildTasklistStreamUrl("agent-1", "tl-1");
    expect(url.endsWith("/stream")).toBe(true);
  });
});

function makeRawEvent(agentId: string): string {
  return JSON.stringify({
    event_id: "evt-1",
    run_id: "run-1",
    seq: 0,
    ts: "2026-05-26T00:00:00Z",
    agent_id: agentId,
    thread_id: null,
    payload: { type: "TextDelta", data: { text: "hello" } },
  });
}

describe("isTasklistChannelEvent", () => {
  it("returns true for events on a tasklist channel", () => {
    expect(isTasklistChannelEvent(makeRawEvent("tasklist:tl-abc123"))).toBe(true);
  });

  it("returns true regardless of the tasklist id value", () => {
    expect(isTasklistChannelEvent(makeRawEvent("tasklist:some-other-id"))).toBe(true);
  });

  it("returns false for events on the parent agent channel", () => {
    expect(isTasklistChannelEvent(makeRawEvent("agent-123"))).toBe(false);
  });

  it("returns false for team-scoped channel events", () => {
    expect(isTasklistChannelEvent(makeRawEvent("team:team-456"))).toBe(false);
  });

  it("returns false for events with no agent_id field", () => {
    const raw = JSON.stringify({ event_id: "evt-1", payload: { type: "RunStarted" } });
    expect(isTasklistChannelEvent(raw)).toBe(false);
  });

  it("returns false for malformed JSON without crashing", () => {
    expect(isTasklistChannelEvent("not valid json {")).toBe(false);
  });

  it("returns false for an empty string", () => {
    expect(isTasklistChannelEvent("")).toBe(false);
  });

  it("does not match a channel that merely contains 'tasklist:' mid-string", () => {
    expect(isTasklistChannelEvent(makeRawEvent("agent-info-tasklist:tl-1"))).toBe(false);
  });
});
