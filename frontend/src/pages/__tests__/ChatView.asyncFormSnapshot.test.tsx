// @vitest-environment jsdom
/**
 * The async form request's transcript entry is now persisted
 * `hidden_from_user: true` (mirrors the sync write site — see
 * `crates/ao-engine-tools-core/src/form_events.rs`'s `persist_posted_form`),
 * so it never appears as a visible message. The pinned nudge card
 * (`ChatView`'s `pendingAsyncFormMeta`) must instead render straight off the
 * `pending_forms` snapshot pointer on `AgentSnapshot` — the same mechanism
 * the sync form's composer overlay already uses.
 *
 * Two code paths populate that snapshot pointer independently, so both are
 * exercised here rather than just one:
 *   1. Live arrival — `useSSE.ts`'s `form_posted` handler optimistically
 *      upserts `agents[].pending_forms` the instant the SSE event lands,
 *      before any transcript fetch has necessarily caught up. `messages` is
 *      empty in this test to model that race explicitly — the old
 *      transcript-scanning approach rendered nothing here.
 *   2. Reload — `GET /agents` repopulates `pending_forms` from the
 *      persisted snapshot, and the transcript refetch separately returns the
 *      (now-hidden) `form_request` entry. The card must still render from
 *      the snapshot even with that hidden entry present in `messages`.
 *
 * These same two cases now also cover the composer gate (`formSlotOccupied`
 * in `ChatView.tsx`): an async form is exactly as composer-blocking as a
 * sync one — single form slot, shared by both modes (owner-locked
 * invariant) — on both the live-arrival and the reload path, since both
 * paths feed the same `agents[].pending_forms` → `pendingAsyncFormMeta`
 * derivation the gate reads. `ChatInput` is mocked to a stubbed
 * `data-testid="chat-input-stub"` (see below) purely so its presence/absence
 * is easy to assert without needing the real component's fetch-dependent
 * internals.
 *
 * Render harness mirrors `ChatView.telegramBridge.test.tsx`.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { MemoryRouter, Routes, Route } from "react-router-dom";
import { ChatView } from "../ChatView";
import { useChatStore } from "../../stores/chatStore";
import { useNetworkStore } from "../../stores/networkStore";
import type { AgentProfile, AgentSnapshot, Thread, TranscriptEntry } from "../../types/api";

const AGENT_ID = "agent-async-form-1";
const DEFAULT_THREAD_ID = "thread-default-1";
const FORM_ID = "form-xyz";

function makeProfile(): AgentProfile {
  return {
    id: AGENT_ID,
    name: "Form Agent",
    description: "",
    provider: {
      type: "",
      command: "",
      args: [],
      output_format: "",
      input_mode: "",
      model_aliases: {},
      resume_args: [],
      session_id_fields: [],
      clear_env: false,
      no_output_timeout_ms: 0,
    },
    model: null,
    skills: [],
    system_prompt: null,
    tools: null,
    env: {},
    max_instances: 1,
    timeout_seconds: 0,
    working_dir: null,
    home_dir: null,
    serialize: false,
  };
}

function makeThreads(): Thread[] {
  return [
    {
      id: DEFAULT_THREAD_ID,
      title: null,
      scope: { type: "AgentChat", agent_id: AGENT_ID },
      transcript_path: "",
      kind: "default",
      created_at: "2024-01-01T00:00:00Z",
      updated_at: "2024-01-01T00:00:00Z",
    },
  ];
}

function makeSnapshot(): AgentSnapshot {
  return {
    agent_id: AGENT_ID,
    name: "Form Agent",
    last_activity_at: null,
    message_count: 0,
    has_active_run: false,
    queue_depth: 0,
    thread_id: null,
    created_at: "2024-01-01T00:00:00Z",
    pending_forms: [
      {
        thread_id: null,
        form_id: FORM_ID,
        spec: {
          form_id: FORM_ID,
          spec: {
            form_id: FORM_ID,
            title: "Rate this response",
            intro: "A couple of quick questions",
            fields: [
              { id: "rating", kind: "radio", label: "How was it?", required: true, options: [{ id: "good", label: "Good" }, { id: "bad", label: "Bad" }] },
            ],
          },
          mode: "async",
        },
      },
    ],
  };
}

/** The `hidden_from_user: true` transcript entry the backend now writes for
 *  the async post — present in a reload's transcript fetch, but must play no
 *  role in what the card renders. */
function hiddenFormRequestEntry(): TranscriptEntry {
  return {
    ts: "2024-01-01T00:00:01Z",
    role: { agent: AGENT_ID },
    content: "",
    event_type: "form_request",
    metadata: {
      form_id: FORM_ID,
      spec: { form_id: FORM_ID, title: "Rate this response", intro: "A couple of quick questions", fields: [] },
      mode: "async",
    },
    hidden_from_user: true,
  };
}

vi.mock("../../lib/api", () => ({
  getAgent: vi.fn(),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  getAgents: vi.fn().mockResolvedValue([]),
  listThreads: vi.fn().mockResolvedValue([]),
  listAssignments: vi.fn().mockResolvedValue([]),
  getBookmarks: vi.fn().mockResolvedValue([]),
  precomputeContext: vi.fn(),
  submitFormAnswer: vi.fn(),
  submitAsyncFormAnswer: vi.fn(),
  dismissAsyncForm: vi.fn(),
}));

vi.mock("../../components/chat/MessageList", () => ({
  MessageList: () => null,
  PinnedBookmarkOverlay: () => null,
  PinnedSearchOverlay: () => null,
  parseSkillLoadInfo: () => null,
}));
vi.mock("../../components/chat/TypingIndicator", () => ({ TypingIndicator: () => null }));
vi.mock("../../components/chat/ChatInput", () => ({
  ChatInput: () => React.createElement("div", { "data-testid": "chat-input-stub" }),
}));

describe("ChatView: async form nudge card renders from the pending_forms snapshot", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;
  let errorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    useChatStore.getState().reset();
    useNetworkStore.setState({ isInternetOnline: true, isServerOnline: true });
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
    vi.clearAllMocks();
    errorSpy.mockRestore();
  });

  async function renderChatView(messages: TranscriptEntry[], snapshot: AgentSnapshot = makeSnapshot()) {
    const { getAgent, getAgents, getMessages, listThreads } = await import("../../lib/api");
    const profile = makeProfile();
    vi.mocked(getAgent).mockResolvedValue(profile);
    vi.mocked(listThreads).mockResolvedValue(makeThreads());
    // `ChatView`'s mount effect calls both `fetchAgents()` (→ `getAgents`)
    // and `selectAgent()` (→ `getMessages`, via a background refresh once
    // the store's optimistic seed below renders) — mock these to echo back
    // the same fixtures the test seeds directly on the store, so the mount
    // effects don't clobber them with the harness's default empty mocks.
    vi.mocked(getAgents).mockResolvedValue([snapshot]);
    vi.mocked(getMessages).mockResolvedValue({ messages, cursor: null });

    useChatStore.setState((state) => {
      const threadsByAgent = new Map(state.threadsByAgent);
      threadsByAgent.set(AGENT_ID, makeThreads());
      const selectedThreadIdByAgent = new Map(state.selectedThreadIdByAgent);
      selectedThreadIdByAgent.set(AGENT_ID, DEFAULT_THREAD_ID);
      return {
        selectedAgentId: AGENT_ID,
        selectedAgentProfile: profile,
        threadsByAgent,
        selectedThreadIdByAgent,
        agents: [snapshot],
        messages,
      };
    });

    await act(async () => {
      root.render(
        React.createElement(
          MemoryRouter,
          { initialEntries: [`/chat/${AGENT_ID}`] },
          React.createElement(
            Routes,
            null,
            React.createElement(Route, {
              path: "/chat/:subMenuSlug",
              element: React.createElement(ChatView),
            }),
          ),
        ),
      );
    });
  }

  it("renders the full spec on live arrival — before the transcript has caught up (messages empty) — and blocks the composer", async () => {
    await renderChatView([]);

    expect(container.textContent).toContain("Rate this response");
    expect(container.textContent).toContain("How was it?");
    // Single form slot, both modes share it: the composer must be hidden
    // here exactly as it would be for a sync form, not merely tucked below
    // a still-visible `ChatInput` (see `formSlotOccupied` in ChatView.tsx).
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();
  });

  it("renders the full spec after a reload — pending_forms snapshot plus the now-hidden transcript entry — and blocks the composer", async () => {
    await renderChatView([hiddenFormRequestEntry()]);

    expect(container.textContent).toContain("Rate this response");
    expect(container.textContent).toContain("How was it?");
    // Same gate, same result on the reload path — this is the second of the
    // two independent entry points (live SSE vs. page-load snapshot) that
    // must agree; asserting both in this file catches a reload-only or
    // live-only regression that a single test can't.
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();
  });

  it("shows the composer when no form is pending (baseline — sync's existing gating behavior is unchanged)", async () => {
    await renderChatView([], { ...makeSnapshot(), pending_forms: [] });

    expect(container.querySelector("[data-testid='chat-input-stub']")).not.toBeNull();
  });

  it("Cancel on the async card dismisses the form and restores the composer", async () => {
    const { dismissAsyncForm } = await import("../../lib/api");
    vi.mocked(dismissAsyncForm).mockResolvedValue(undefined);

    await renderChatView([]);
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();

    const cancelBtn = container.querySelector<HTMLButtonElement>("[data-testid='form-action-cancel-btn']");
    expect(cancelBtn).not.toBeNull();
    await act(async () => {
      cancelBtn!.click();
    });

    expect(dismissAsyncForm).toHaveBeenCalledWith(AGENT_ID, FORM_ID);
    // The slot is free again — composer comes back, card is gone.
    expect(container.querySelector("[data-testid='chat-input-stub']")).not.toBeNull();
    expect(container.textContent).not.toContain("Rate this response");
  });

  it("newest-wins slot handover: form B's snapshot pointer means B's card shows, fully interactive, not A's", async () => {
    const FORM_B_ID = "form-b-newer";
    const snapshotWithB: AgentSnapshot = {
      ...makeSnapshot(),
      // Posting a second form supersedes the first on the backend
      // (`crates/ao-engine-tools-core/src/form_events.rs::persist_posted_form`'s
      // `Ok(Some(replaced))` branch / `LiveFormBridge::persist_pending`'s
      // mirror of it) — by the time a client fetches `pending_forms` again
      // (live SSE upsert or a reload's `GET /agents`), only the newcomer's
      // record remains for this thread. `messages` here still carries A's
      // now-hidden `form_request` entry (MessageList itself is stubbed in
      // this harness, so the visible `form_withdrawn` trace it would also
      // carry is covered separately by `AsyncFormEntries.test.tsx`'s
      // `FormWithdrawnIndicator` suite, not here) — the point of this test is
      // that the PINNED CARD derivation reads only the snapshot pointer, so
      // a stale transcript entry for the displaced form can't leak through.
      pending_forms: [
        {
          thread_id: null,
          form_id: FORM_B_ID,
          spec: {
            form_id: FORM_B_ID,
            spec: {
              form_id: FORM_B_ID,
              title: "A follow-up question",
              intro: null,
              fields: [{ id: "note", kind: "text", label: "Anything else?", required: false }],
            },
            mode: "async",
          },
        },
      ],
    };

    await renderChatView([hiddenFormRequestEntry()], snapshotWithB);

    // B's card is what's showing — not A's.
    expect(container.textContent).toContain("A follow-up question");
    expect(container.textContent).not.toContain("Rate this response");

    // The newcomer is fully interactive — its input renders enabled, not
    // disabled/read-only, immediately after the swap.
    const input = container.querySelector<HTMLInputElement>("input[type='text']");
    expect(input).not.toBeNull();
    expect(input!.disabled).toBe(false);
    expect(input!.readOnly).toBe(false);

    // Single slot, still occupied by B — composer stays hidden.
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();
  });

  it("minimizing the async form collapses it to the bar, keeps the composer hidden, and expanding restores the card", async () => {
    await renderChatView([]);

    const minimizeBtn = container.querySelector<HTMLButtonElement>("[data-testid='form-minimize-btn']");
    expect(minimizeBtn).not.toBeNull();
    await act(async () => {
      minimizeBtn!.click();
    });

    expect(container.querySelector("[data-testid='minimized-form-bar']")).not.toBeNull();
    expect(container.textContent).not.toContain("How was it?");
    // Minimized ≠ slot freed — the composer stays hidden while the bar shows.
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();

    const expandBtn = container.querySelector<HTMLButtonElement>("[data-testid='minimized-form-expand-btn']");
    await act(async () => {
      expandBtn!.click();
    });

    expect(container.querySelector("[data-testid='minimized-form-bar']")).toBeNull();
    expect(container.textContent).toContain("How was it?");
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();
  });
});

/**
 * Sync-behaviour-unchanged control group for the same composer gate. A sync
 * form (`AskUserQuestionWithForm`) arrives via `pendingFormByAgent`
 * (`setPendingForm`, simulating `useSSE.ts`'s `form_request` handler) rather
 * than `agents[].pending_forms` — a completely separate store slice from the
 * async fixtures above — so this is run as its own suite to prove the gate
 * change didn't alter sync's pre-existing behavior (blocks the composer via
 * the floating overlay, minimizes to the same `MinimizedFormBar`).
 */
describe("ChatView: sync form composer gating is unchanged", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;
  let errorSpy: ReturnType<typeof vi.spyOn>;

  const SYNC_AGENT_ID = "agent-sync-form-1";
  const SYNC_FORM_ID = "sync-form-xyz";

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    useChatStore.getState().reset();
    useNetworkStore.setState({ isInternetOnline: true, isServerOnline: true });
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
    vi.clearAllMocks();
    errorSpy.mockRestore();
  });

  it("a pending sync form blocks the composer via the floating overlay, and minimize/restore round-trips", async () => {
    const { getAgent, getAgents, getMessages, listThreads } = await import("../../lib/api");
    const profile: AgentProfile = {
      id: SYNC_AGENT_ID,
      name: "Sync Agent",
      description: "",
      provider: {
        type: "", command: "", args: [], output_format: "", input_mode: "",
        model_aliases: {}, resume_args: [], session_id_fields: [], clear_env: false, no_output_timeout_ms: 0,
      },
      model: null, skills: [], system_prompt: null, tools: null, env: {},
      max_instances: 1, timeout_seconds: 0, working_dir: null, home_dir: null, serialize: false,
    };
    const threads: Thread[] = [{
      id: "thread-default-sync",
      title: null,
      scope: { type: "AgentChat", agent_id: SYNC_AGENT_ID },
      transcript_path: "",
      kind: "default",
      created_at: "2024-01-01T00:00:00Z",
      updated_at: "2024-01-01T00:00:00Z",
    }];
    vi.mocked(getAgent).mockResolvedValue(profile);
    vi.mocked(listThreads).mockResolvedValue(threads);
    vi.mocked(getAgents).mockResolvedValue([{
      agent_id: SYNC_AGENT_ID, name: "Sync Agent", last_activity_at: null, message_count: 0,
      has_active_run: false, queue_depth: 0, thread_id: null, created_at: "2024-01-01T00:00:00Z",
    }]);
    vi.mocked(getMessages).mockResolvedValue({ messages: [], cursor: null });

    useChatStore.setState((state) => {
      const threadsByAgent = new Map(state.threadsByAgent);
      threadsByAgent.set(SYNC_AGENT_ID, threads);
      const selectedThreadIdByAgent = new Map(state.selectedThreadIdByAgent);
      selectedThreadIdByAgent.set(SYNC_AGENT_ID, "thread-default-sync");
      return {
        selectedAgentId: SYNC_AGENT_ID,
        selectedAgentProfile: profile,
        threadsByAgent,
        selectedThreadIdByAgent,
        agents: [{
          agent_id: SYNC_AGENT_ID, name: "Sync Agent", last_activity_at: null, message_count: 0,
          has_active_run: false, queue_depth: 0, thread_id: null, created_at: "2024-01-01T00:00:00Z",
        }],
        messages: [],
      };
    });
    // Simulate `useSSE.ts`'s `form_request` handler → `setPendingForm`.
    useChatStore.getState().setPendingForm(SYNC_AGENT_ID, {
      form_id: SYNC_FORM_ID,
      agent_id: SYNC_AGENT_ID,
      session_id: "session-1",
      title: "Pick a color",
      fields: [{ id: "color", kind: "text", label: "Color?", required: true }],
    });

    await act(async () => {
      root.render(
        React.createElement(
          MemoryRouter,
          { initialEntries: [`/chat/${SYNC_AGENT_ID}`] },
          React.createElement(
            Routes,
            null,
            React.createElement(Route, { path: "/chat/:subMenuSlug", element: React.createElement(ChatView) }),
          ),
        ),
      );
    });

    expect(container.textContent).toContain("Pick a color");
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();

    const minimizeBtn = container.querySelector<HTMLButtonElement>("[data-testid='form-minimize-btn']");
    expect(minimizeBtn).not.toBeNull();
    await act(async () => {
      minimizeBtn!.click();
    });
    expect(container.querySelector("[data-testid='minimized-form-bar']")).not.toBeNull();
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();

    const expandBtn = container.querySelector<HTMLButtonElement>("[data-testid='minimized-form-expand-btn']");
    await act(async () => {
      expandBtn!.click();
    });
    expect(container.textContent).toContain("Pick a color");
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();
  });
});
