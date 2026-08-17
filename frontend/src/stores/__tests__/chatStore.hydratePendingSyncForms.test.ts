/**
 * Frontend half of sync-form persistence: rehydrate `pendingFormByAgent` from
 * the agent snapshot's
 * `pending_forms` on mount, so a page reload restores an answerable sync
 * form instead of losing it while the backend is still parked on the
 * oneshot (see `ao-engine-tools-runner`'s `LiveFormBridge::ask_form`).
 *
 * `hydratePendingSyncFormsFromAgents` is the mechanism: it reads
 * `state.agents[].pending_forms`, seeds `pendingFormByAgent` from every entry
 * tagged `spec.mode === "sync"`, and leaves `mode === "async"` entries alone
 * (those render through the separate `pendingFormForThread`/transcript path,
 * never through `pendingFormByAgent`).
 */

import { describe, it, expect, beforeEach } from "vitest";
import { useChatStore, pendingSyncFormForThread, inFlightKey } from "../chatStore";
import type { AgentSnapshot, PendingForm } from "../../types/api";

function store() {
  return useChatStore.getState();
}

const AGENT_ID = "agent-rehydrate";
const THREAD_A = "thread-rehydrate-a";

function makeSyncPendingForm(overrides: Partial<PendingForm> = {}): PendingForm {
  // `form_id` (if overridden) must land in all three nested copies — the
  // outer `PendingForm.form_id`, `spec.form_id`, and `spec.spec.form_id` —
  // exactly like the real backend payload keeps them in lockstep (see
  // `FormRequestMeta` in `ao-engine-tools-core`), so overriding it here can't
  // silently leave the nested id stale.
  const formId = overrides.form_id ?? "sync-form-1";
  return {
    thread_id: null,
    form_id: formId,
    spec: {
      form_id: formId,
      mode: "sync",
      spec: {
        form_id: formId,
        title: "Pick a color",
        intro: "Choose one to continue",
        fields: [{ id: "color", kind: "text", label: "Color?", required: true }],
      },
    },
    ...overrides,
  };
}

function makeAsyncPendingForm(overrides: Partial<PendingForm> = {}): PendingForm {
  return {
    thread_id: null,
    form_id: "async-form-1",
    spec: {
      form_id: "async-form-1",
      mode: "async",
      spec: { form_id: "async-form-1", title: "Background question", fields: [] },
    },
    ...overrides,
  };
}

function makeAgent(overrides: Partial<AgentSnapshot> = {}): AgentSnapshot {
  return {
    agent_id: AGENT_ID,
    name: "Agent",
    last_activity_at: null,
    message_count: 0,
    has_active_run: false,
    queue_depth: 0,
    thread_id: null,
    created_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

beforeEach(() => {
  useChatStore.getState().reset();
});

describe("hydratePendingSyncFormsFromAgents", () => {
  it("rehydrates pendingFormByAgent from a sync pending_forms entry, with no form_request SSE event ever received", () => {
    // No SSE `form_request` event fires anywhere in this test — the store
    // starts from a cold `agents` snapshot only, exactly the page-reload shape.
    useChatStore.setState({
      agents: [makeAgent({ pending_forms: [makeSyncPendingForm({ thread_id: null })] })],
    });

    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)).toBeUndefined();

    store().hydratePendingSyncFormsFromAgents();

    const rehydrated = pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined);
    expect(rehydrated?.form_id).toBe("sync-form-1");
    expect(rehydrated?.title).toBe("Pick a color");
    expect(rehydrated?.intro).toBe("Choose one to continue");
    expect(rehydrated?.fields).toEqual([{ id: "color", kind: "text", label: "Color?", required: true }]);
    expect(rehydrated?.agent_id).toBe(AGENT_ID);
  });

  it("resolves the thread-scoped bucket for a sync form pending on a non-default thread", () => {
    useChatStore.setState({
      agents: [makeAgent({ pending_forms: [makeSyncPendingForm({ thread_id: THREAD_A })] })],
    });

    store().hydratePendingSyncFormsFromAgents();

    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, THREAD_A)?.form_id).toBe("sync-form-1");
    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)).toBeUndefined();
    expect(store().pendingFormByAgent[inFlightKey(AGENT_ID, THREAD_A)]?.form_id).toBe("sync-form-1");
  });

  it("ignores async-mode pending_forms entries — they never populate pendingFormByAgent", () => {
    useChatStore.setState({
      agents: [makeAgent({ pending_forms: [makeAsyncPendingForm()] })],
    });

    store().hydratePendingSyncFormsFromAgents();

    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)).toBeUndefined();
    expect(Object.keys(store().pendingFormByAgent)).toHaveLength(0);
  });

  it("never overwrites an already-present entry — a live SSE arrival or an already-cleared form both outrank the REST snapshot", () => {
    // A live entry (as if `useSSE`'s form_request handler already set it,
    // possibly with fresher data than the snapshot) must survive untouched.
    useChatStore.setState({
      agents: [makeAgent({ pending_forms: [makeSyncPendingForm({ form_id: "stale-snapshot-id" })] })],
    });
    store().setPendingForm(AGENT_ID, {
      form_id: "live-form-id",
      agent_id: AGENT_ID,
      session_id: "s-1",
      title: "Already live",
      fields: [],
    });

    store().hydratePendingSyncFormsFromAgents();

    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)?.form_id).toBe("live-form-id");
  });

  it("is a no-op when no agent has any pending sync form", () => {
    useChatStore.setState({ agents: [makeAgent({ pending_forms: [] })] });

    store().hydratePendingSyncFormsFromAgents();

    expect(Object.keys(store().pendingFormByAgent)).toHaveLength(0);
  });

  it("carries `orphaned: true` from a reaped sync form into pendingFormByAgent", () => {
    useChatStore.setState({
      agents: [makeAgent({ pending_forms: [makeSyncPendingForm({ orphaned: true })] })],
    });

    store().hydratePendingSyncFormsFromAgents();

    const rehydrated = pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined);
    expect(rehydrated?.orphaned).toBe(true);
  });

  it("defaults `orphaned` to false for a non-reaped sync form", () => {
    useChatStore.setState({
      agents: [makeAgent({ pending_forms: [makeSyncPendingForm()] })],
    });

    store().hydratePendingSyncFormsFromAgents();

    const rehydrated = pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined);
    expect(rehydrated?.orphaned).toBe(false);
  });

  it("hydrates independently across multiple agents", () => {
    const OTHER_AGENT = "agent-rehydrate-2";
    useChatStore.setState({
      agents: [
        makeAgent({ pending_forms: [makeSyncPendingForm({ form_id: "form-a" })] }),
        makeAgent({
          agent_id: OTHER_AGENT,
          pending_forms: [makeSyncPendingForm({ form_id: "form-b" })],
        }),
      ],
    });

    store().hydratePendingSyncFormsFromAgents();

    expect(pendingSyncFormForThread(store().pendingFormByAgent, AGENT_ID, undefined)?.form_id).toBe("form-a");
    expect(pendingSyncFormForThread(store().pendingFormByAgent, OTHER_AGENT, undefined)?.form_id).toBe("form-b");
  });
});
