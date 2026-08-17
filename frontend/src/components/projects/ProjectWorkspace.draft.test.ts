/**
 * Draft store integration tests for the ProjectCopilotOverlay ChatInput.
 *
 * Verifies that the `project:{projectId}` key used by ProjectCopilotOverlay
 * correctly round-trips through useDraftStore: draft is preserved on unmount
 * and restored on remount, and cleared on successful send.
 */

import { beforeEach, describe, it, expect } from "vitest";
import { useDraftStore } from "../../stores/draftStore";

function resetDraftStore() {
  localStorage.clear();
  useDraftStore.setState({ drafts: {}, draftHtml: {}, draftAttachments: {} });
}

describe("ProjectCopilotOverlay draft store", () => {
  beforeEach(resetDraftStore);

  it("persists draft on unmount (onUnmount callback) and restores on remount", () => {
    const projectKey = "project:proj-round-trip";
    const text = "Ask the agent to summarize progress";
    const html = "<p>Ask the agent to summarize progress</p>";
    const { setDraft } = useDraftStore.getState();

    // Simulate ChatInput onUnmount callback
    if (text.trim()) setDraft(projectKey, text, html);

    // Simulate remount: read from store
    const state = useDraftStore.getState();
    expect(state.drafts[projectKey]).toBe(text);
    expect(state.draftHtml[projectKey]).toBe(html);
  });

  it("clears draft on successful send (onSend callback)", () => {
    const projectKey = "project:proj-clear-on-send";
    const { setDraft, clearDraft } = useDraftStore.getState();

    setDraft(projectKey, "Draft to be sent");
    expect(useDraftStore.getState().drafts[projectKey]).toBe("Draft to be sent");

    // Simulate clearDraft(projectKey) called in onSend
    clearDraft(projectKey);
    expect(useDraftStore.getState().drafts[projectKey]).toBeUndefined();
  });

  it("project: key does not collide with bare agentId chat draft", () => {
    const agentId = "agent-proj-collision";
    const projectKey = `project:${agentId}`;
    const { setDraft } = useDraftStore.getState();

    setDraft(agentId, "agent chat draft");
    setDraft(projectKey, "project copilot draft");

    const state = useDraftStore.getState();
    expect(state.drafts[agentId]).toBe("agent chat draft");
    expect(state.drafts[projectKey]).toBe("project copilot draft");
  });

  it("clearDraft on empty input removes stale draft (onUnmount with empty text)", () => {
    const projectKey = "project:proj-clear-empty";
    const { setDraft, clearDraft } = useDraftStore.getState();

    setDraft(projectKey, "old draft");

    // Simulate onUnmount when textarea is empty: clearDraft(id)
    clearDraft(projectKey);
    expect(useDraftStore.getState().drafts[projectKey]).toBeUndefined();
  });
});
