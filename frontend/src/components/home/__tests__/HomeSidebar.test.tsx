// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { MemoryRouter, Routes, Route } from "react-router-dom";
import { HomeSidebar } from "../HomeSidebar";
import { useChatStore, inFlightKey } from "../../../stores/chatStore";
import { useUserPreferencesStore } from "../../../stores/userPreferencesStore";
import { __dispatchForTest } from "../../../lib/sseHub";
import type { AgentSnapshot, PendingForm, Thread } from "../../../types/api";
import type { FormRequestPayload } from "../../../types/form";

// Only exercised by the "clears on send" test below — every other test in
// this file drives `useChatStore.setState` directly and never touches the
// network. `sendMessage` is mocked (rest of `lib/api` stays real) so
// chatStore's real `sendMessage` action can be driven end-to-end without an
// actual network call.
const mockSendMessage = vi.fn().mockResolvedValue({ message_id: "msg-1", status: "queued" });
// `listThreads` backs both the real `loadThreads` store action (used by the
// SSE backfill, the per-agent expand effect, and eager hydration — none of
// them are mocked at the store level except where a test explicitly swaps
// `loadThreads` itself) and eager hydration in particular fires it for every
// agent in a test's roster on mount. Mocked here so that firing never hits a
// real (non-existent, in jsdom) network call — most tests never override
// this default; the "eager thread hydration" describe block below controls
// its resolution/rejection per test to exercise concurrency/partial-failure/
// abort behavior precisely.
const mockListThreads = vi.fn().mockResolvedValue([]);
// Backs `api.listAllThreads` — the bulk `GET /threads` call HomeSidebar's
// hydration effect fires once per mount (via the store's `loadAllThreads`
// action) instead of fanning out per-agent. Mocked at this layer (not by
// swapping out the store's `loadAllThreads` action itself) so the ported
// "bulk thread hydration" tests below still exercise the store's own real
// reducer logic — in particular the empty-array seeding for agents the
// grouped response omits, which is the exact behavior those tests assert.
const mockListAllThreads = vi.fn().mockResolvedValue({});
// `useAssignments()` (used for the Assignments-section name lookup) fans out
// its own `listAssignments` call per agent, unrelated to thread hydration —
// mocked here purely so it never attempts a real (non-existent, in jsdom)
// network call and never contributes async state updates of its own that'd
// muddy assertions about the bulk-hydration effect below.
vi.mock("../../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../../lib/api")>();
  return {
    ...actual,
    sendMessage: (...args: unknown[]) => mockSendMessage(...args),
    listThreads: (...args: unknown[]) => mockListThreads(...args),
    listAllThreads: (...args: unknown[]) => mockListAllThreads(...args),
    listAssignments: vi.fn().mockResolvedValue([]),
  };
});

// The SSE hub lazily opens a real fetch-based connection on first
// subscription. Stub it out so mounting HomeSidebar (which now subscribes to
// the system channel) never attempts a network call in jsdom — events below
// are injected directly via `__dispatchForTest`.
vi.mock("../../../hooks/sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../../hooks/sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

function injectThreadCreated(agentId: string, thread: Thread): void {
  act(() => {
    __dispatchForTest({
      agent_id: agentId,
      run_id: "run-1",
      thread_id: null,
      eventName: "thread_created",
      raw: JSON.stringify({
        agent_id: agentId,
        run_id: "run-1",
        payload: { type: "thread_created", data: { thread } },
      }),
    });
  });
}

function makeThread(overrides: Partial<Thread> & { id: string }): Thread {
  return {
    title: null,
    scope: { type: "AgentChat", agent_id: "agent-1" },
    transcript_path: `/tmp/${overrides.id}.jsonl`,
    kind: "default",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function makeAgentSnapshot(overrides: Partial<AgentSnapshot> = {}): AgentSnapshot {
  return {
    agent_id: "agent-1",
    name: "Agent One",
    last_activity_at: "2026-01-01T00:00:00Z",
    message_count: 3,
    has_active_run: false,
    queue_depth: 0,
    thread_id: null,
    created_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function makeSyncForm(overrides: Partial<FormRequestPayload> = {}): FormRequestPayload {
  return {
    form_id: "form-1",
    agent_id: "agent-1",
    session_id: "session-1",
    title: "Pick one",
    fields: [],
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
      spec: { form_id: "async-form-1", title: "Pick one", fields: [] },
    },
    ...overrides,
  };
}

describe("HomeSidebar", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    useChatStore.getState().reset();
    // HomeSidebar fetches agents on mount — stub it out so tests drive the
    // agent list directly via `setState` instead of hitting the real API.
    useChatStore.setState({ fetchAgents: async () => {} });
    // Reset (not just clear) so a per-test `.mockImplementation(...)` from
    // the "bulk thread hydration" block below never leaks into the next
    // test — restore the default resolved-empty-array behavior every time.
    mockListThreads.mockReset();
    mockListThreads.mockResolvedValue([]);
    mockListAllThreads.mockReset();
    mockListAllThreads.mockResolvedValue({});
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function render(agents: AgentSnapshot[]) {
    useChatStore.setState({ agents });
    await act(async () => {
      root.render(
        <MemoryRouter initialEntries={["/home/none-selected"]}>
          <Routes>
            <Route path="/home/:subMenuSlug" element={<HomeSidebar />} />
          </Routes>
        </MemoryRouter>,
      );
    });
  }

  // Same as `render`, but lands on `/home/:agentId` so that agent reads as
  // the one currently open in the main panel (`isActiveAgent`/`isActiveThread`
  // in HomeSidebar) — needed only for the "active row stays suppressed" case.
  async function renderActive(agents: AgentSnapshot[], agentId: string) {
    useChatStore.setState({ agents });
    await act(async () => {
      root.render(
        <MemoryRouter initialEntries={[`/home/${agentId}`]}>
          <Routes>
            <Route path="/home/:subMenuSlug" element={<HomeSidebar />} />
          </Routes>
        </MemoryRouter>,
      );
    });
  }

  function questionBadge(id: string) {
    return container.querySelector(`[data-testid='thread-question-badge-${id}']`);
  }

  describe("agent-level question rollup badge", () => {
    it("shows the badge when a sync form is pending on any thread of the agent", async () => {
      useChatStore.setState({
        pendingFormByAgent: { [inFlightKey("agent-1", "branch-1")]: makeSyncForm() },
      });
      await render([makeAgentSnapshot()]);
      expect(questionBadge("agent-1")).toBeTruthy();
    });

    it("shows the badge when the agent has a non-empty async pending_forms array", async () => {
      await render([makeAgentSnapshot({ pending_forms: [makeAsyncPendingForm()] })]);
      expect(questionBadge("agent-1")).toBeTruthy();
    });

    it("does not show the badge when the agent has no pending forms of either kind", async () => {
      await render([makeAgentSnapshot()]);
      expect(questionBadge("agent-1")).toBeNull();
    });

    it("does not show the badge for an agent whose only pending_forms entry is empty", async () => {
      await render([makeAgentSnapshot({ pending_forms: [] })]);
      expect(questionBadge("agent-1")).toBeNull();
    });

    it("does not leak a badge onto an unrelated agent in the same list", async () => {
      useChatStore.setState({
        pendingFormByAgent: { [inFlightKey("agent-1", "branch-1")]: makeSyncForm() },
      });
      await render([
        makeAgentSnapshot(),
        makeAgentSnapshot({ agent_id: "agent-2", name: "Agent Two", pending_forms: [] }),
      ]);
      expect(questionBadge("agent-1")).toBeTruthy();
      expect(questionBadge("agent-2")).toBeNull();
    });

    // Regression coverage for the default/main thread specifically — the
    // sync-form case above pends on a non-default thread (`branch-1`). A
    // default-thread sync form carries no `thread_id` on the wire, so it
    // buckets under the bare agent id (see `pendingFormByAgent`'s docstring
    // in chatStore.ts) — asserted explicitly here since the any-thread
    // rollup must still resolve it.
    it("shows the badge when a sync form is pending on the agent's default thread (no thread_id)", async () => {
      useChatStore.setState({
        pendingFormByAgent: { "agent-1": makeSyncForm() },
      });
      await render([makeAgentSnapshot()]);
      expect(questionBadge("agent-1")).toBeTruthy();
    });

    it("shows the badge for an async pending_forms entry on the default thread (thread_id: null)", async () => {
      await render([makeAgentSnapshot({ pending_forms: [makeAsyncPendingForm({ thread_id: null })] })]);
      expect(questionBadge("agent-1")).toBeTruthy();
    });

    it("renders the louder sync treatment when the agent has a pending sync form on any thread", async () => {
      useChatStore.setState({
        pendingFormByAgent: { [inFlightKey("agent-1", "branch-1")]: makeSyncForm() },
      });
      await render([makeAgentSnapshot()]);
      expect(questionBadge("agent-1")?.getAttribute("data-sync")).toBe("true");
    });

    it("renders the passive (non-sync) treatment when the agent only has an async pending_forms entry", async () => {
      await render([makeAgentSnapshot({ pending_forms: [makeAsyncPendingForm()] })]);
      expect(questionBadge("agent-1")?.getAttribute("data-sync")).toBeNull();
    });
  });

  describe("per-thread question badge (expanded agent row)", () => {
    const defaultThread = makeThread({ id: "default-1", kind: "default" });
    const branchThread = makeThread({ id: "branch-1", kind: "branch", title: "Investigate bug" });

    beforeEach(() => {
      useUserPreferencesStore.setState({ homeExpandedAgentIds: ["agent-1"] });
      useChatStore.setState({ threadsByAgent: new Map([["agent-1", [defaultThread, branchThread]]]) });
    });

    afterEach(() => {
      useUserPreferencesStore.setState({ homeExpandedAgentIds: [] });
    });

    it("shows the (loud, sync) question badge on an inactive thread row for a pending sync form", async () => {
      useChatStore.setState({
        pendingFormByAgent: { [inFlightKey("agent-1", "branch-1")]: makeSyncForm({ thread_id: "branch-1" }) },
      });
      await render([makeAgentSnapshot()]);
      expect(questionBadge("branch-1")?.getAttribute("data-sync")).toBe("true");
    });

    it("shows the passive (non-sync) question badge on an inactive thread row for an async pending_forms entry", async () => {
      await render([makeAgentSnapshot({ pending_forms: [makeAsyncPendingForm({ thread_id: "branch-1" })] })]);
      expect(questionBadge("branch-1")).toBeTruthy();
      expect(questionBadge("branch-1")?.getAttribute("data-sync")).toBeNull();
    });

    it("never shows a question badge on the active thread row", async () => {
      // No `selectedThreadIdByAgent` entry set, so HomeSidebar falls back to
      // the default thread as the active one — pend the form there (bare
      // agent id key, see chatStore's `pendingFormByAgent` docstring).
      useChatStore.setState({
        pendingFormByAgent: { "agent-1": makeSyncForm() },
      });
      await renderActive([makeAgentSnapshot()], "agent-1");
      expect(questionBadge("default-1")).toBeNull();
    });

    // Regression: the sidebar row's `?` badge used to stay stuck forever
    // once the operator superseded a form's question any way other than
    // answering it via the form overlay or the run ending outright. Sending
    // a plain new message into that thread is the common miss — it now
    // clears that thread's pending sync-form slot up front (chatStore's
    // `sendMessage`), so this row's badge disappears live off the same
    // store HomeSidebar already subscribes to.
    it("removes the row's question badge once the user sends a message to that thread", async () => {
      mockSendMessage.mockClear();
      useChatStore.setState((s) => {
        const next = new Map(s.selectedThreadIdByAgent);
        next.set("agent-1", "branch-1");
        return {
          selectedThreadIdByAgent: next,
          selectedAgentId: "agent-1",
          pendingFormByAgent: { [inFlightKey("agent-1", "branch-1")]: makeSyncForm({ thread_id: "branch-1" }) },
        };
      });
      await render([makeAgentSnapshot()]);
      expect(questionBadge("branch-1")).toBeTruthy();

      await act(async () => {
        await useChatStore.getState().sendMessage("never mind, I'll just say it here");
      });

      expect(questionBadge("branch-1")).toBeNull();
    });

    // GUARD: a still-pending question on a different thread of the same
    // agent must survive — sending into "branch-1" must not sweep
    // "default-1"'s genuinely-unrelated pending form along with it.
    it("GUARD: sending a message on one thread does not clear a pending question on another thread", async () => {
      mockSendMessage.mockClear();
      useChatStore.setState((s) => {
        const next = new Map(s.selectedThreadIdByAgent);
        next.set("agent-1", "branch-1");
        return {
          selectedThreadIdByAgent: next,
          selectedAgentId: "agent-1",
          pendingFormByAgent: {
            "agent-1": makeSyncForm({ form_id: "form-default" }),
            [inFlightKey("agent-1", "branch-1")]: makeSyncForm({ form_id: "form-branch", thread_id: "branch-1" }),
          },
        };
      });
      await render([makeAgentSnapshot()]);
      expect(questionBadge("default-1")).toBeTruthy();
      expect(questionBadge("branch-1")).toBeTruthy();

      await act(async () => {
        await useChatStore.getState().sendMessage("answering branch-1's question inline");
      });

      expect(questionBadge("branch-1")).toBeNull();
      expect(questionBadge("default-1")).toBeTruthy();
    });
  });

  describe("live thread_created via system channel", () => {
    const defaultThread = makeThread({ id: "default-1", kind: "default" });

    it("splices a newly-created thread into an already-loaded agent's list and renders it", async () => {
      useUserPreferencesStore.setState({ homeExpandedAgentIds: ["agent-1"] });
      useChatStore.setState({ threadsByAgent: new Map([["agent-1", [defaultThread]]]) });

      await render([makeAgentSnapshot()]);

      injectThreadCreated(
        "agent-1",
        makeThread({ id: "live-thread-1", kind: "branch", title: "Live thread" }),
      );

      const threads = useChatStore.getState().threadsByAgent.get("agent-1") ?? [];
      expect(threads.map((t) => t.id)).toContain("live-thread-1");
      expect(container.textContent).toContain("Live thread");

      useUserPreferencesStore.setState({ homeExpandedAgentIds: [] });
    });

    // Bug 1 fix: previously this event was silently dropped for an
    // un-hydrated agent, on the assumption the agent would "load fresh the
    // first time it's expanded" — true for a UI-initiated thread, false for
    // a channel-originated one (the user may never expand that agent). Now
    // it schedules a background `loadThreads` backfill instead.
    describe("un-hydrated agent (Bug 1 fix: schedules a backfill instead of dropping the event)", () => {
      const realLoadThreads = useChatStore.getState().loadThreads;

      afterEach(() => {
        useChatStore.setState({ loadThreads: realLoadThreads });
      });

      // Both tests below render an EMPTY roster (`render([])`) rather than
      // `[makeAgentSnapshot()]` — deliberately, since HomeSidebar now also
      // eager-hydrates every agent in its roster on mount (see
      // "eager thread hydration" describe block further down), sharing the
      // very same `requestedThreadsRef` in-flight guard this SSE backfill
      // path uses. If "agent-1" were in the rendered roster, eager hydration
      // would claim it before `injectThreadCreated` ever fires, and these
      // assertions would end up exercising eager hydration's own dedup
      // instead of this SSE path's. An empty roster means eager hydration
      // has nothing to do, isolating the behavior this block actually tests:
      // an SSE event for an agent Home doesn't (yet) know about at all.
      it("schedules a loadThreads backfill for an agent not yet in threadsByAgent, instead of discarding the event", async () => {
        let resolveLoad!: () => void;
        const mockLoadThreads = vi.fn().mockImplementation(
          () =>
            new Promise<void>((resolve) => {
              resolveLoad = resolve;
            }),
        );
        useChatStore.setState({ loadThreads: mockLoadThreads });

        await render([]);
        expect(useChatStore.getState().threadsByAgent.has("agent-1")).toBe(false);

        injectThreadCreated("agent-1", makeThread({ id: "live-thread-2", kind: "branch" }));

        expect(mockLoadThreads).toHaveBeenCalledWith("agent-1");
        expect(mockLoadThreads).toHaveBeenCalledTimes(1);

        await act(async () => {
          resolveLoad();
          // Two ticks: one for the `.catch` pass-through, one for `.finally`.
          await Promise.resolve();
          await Promise.resolve();
        });
      });

      it("does not fire a second loadThreads for a burst of events on the same still-un-hydrated agent", async () => {
        let resolveLoad!: () => void;
        const mockLoadThreads = vi.fn().mockImplementation(
          () =>
            new Promise<void>((resolve) => {
              resolveLoad = resolve;
            }),
        );
        useChatStore.setState({ loadThreads: mockLoadThreads });

        await render([]);

        injectThreadCreated("agent-1", makeThread({ id: "live-thread-3a", kind: "branch" }));
        injectThreadCreated("agent-1", makeThread({ id: "live-thread-3b", kind: "branch" }));
        injectThreadCreated("agent-1", makeThread({ id: "live-thread-3c", kind: "branch" }));

        // Still in flight — the dedup guard must hold until the first call settles.
        expect(mockLoadThreads).toHaveBeenCalledTimes(1);

        await act(async () => {
          resolveLoad();
          // Two ticks: one for the `.catch` pass-through, one for `.finally`.
          await Promise.resolve();
          await Promise.resolve();
        });

        // Once the in-flight hydration settles, a later burst for the same
        // agent is free to trigger a fresh backfill (e.g. it's still not
        // hydrated because the fetch raced with more channel activity).
        injectThreadCreated("agent-1", makeThread({ id: "live-thread-3d", kind: "branch" }));
        expect(mockLoadThreads).toHaveBeenCalledTimes(2);
      });
    });

    it("unsubscribes on unmount so a later event can't touch a stale component", async () => {
      useChatStore.setState({ threadsByAgent: new Map([["agent-1", [defaultThread]]]) });
      await render([makeAgentSnapshot()]);

      await act(async () => {
        root.unmount();
      });

      // No listener left registered — dispatching now must not throw and
      // must not mutate the store (there's nothing left to splice into).
      expect(() =>
        injectThreadCreated("agent-1", makeThread({ id: "live-thread-3", kind: "branch" })),
      ).not.toThrow();
      const threads = useChatStore.getState().threadsByAgent.get("agent-1") ?? [];
      expect(threads.map((t) => t.id)).not.toContain("live-thread-3");

      // Re-create the root so the shared `afterEach` unmount is a harmless no-op.
      root = createRoot(container);
    });
  });

  // Regression coverage for the double-render bug: a channel-origin thread
  // (non-null `channel_origin`, e.g. a Slack bridge conversation) used to
  // render both under its agent (Agents section) and in the Channels
  // section, with the active highlight only ever landing on the Agents-
  // section copy. `resolveChannelThreadPartition` (channelThreads.ts) is now
  // applied to the Agents section too, and the Channels-section rows share
  // the same `activeThreadId` the Agents section computes.
  describe("channel-origin threads (Channels section)", () => {
    const workingThread = makeThread({ id: "default-1", kind: "default" });
    const channelThread = makeThread({
      id: "channel-1",
      kind: "branch",
      title: "Slack thread",
      channel_origin: { kind: "slack", binding_id: "binding-1" },
    });

    beforeEach(() => {
      // Both the agent's own thread list and the "slack" channel group need
      // to be expanded for their thread rows to actually render — grouped by
      // channel (`channelsGroupBy` default), so the group key is the channel
      // kind itself (see `channelGroupExpandKey`).
      useUserPreferencesStore.setState({
        homeExpandedAgentIds: ["agent-1"],
        homeExpandedChannelGroupKeys: ["channel:slack"],
      });
      useChatStore.setState({ threadsByAgent: new Map([["agent-1", [workingThread, channelThread]]]) });
    });

    afterEach(() => {
      useUserPreferencesStore.setState({ homeExpandedAgentIds: [], homeExpandedChannelGroupKeys: [] });
    });

    function agentRow(threadId: string) {
      return container.querySelector(`[data-testid='home-agent-thread-row-${threadId}']`);
    }

    function channelRow(threadId: string) {
      return container.querySelector(`[data-testid='home-channel-thread-row-${threadId}']`);
    }

    it("renders a channel-origin thread only in the Channels section, never under its agent", async () => {
      await render([makeAgentSnapshot()]);

      expect(agentRow("channel-1")).toBeNull();
      expect(channelRow("channel-1")).toBeTruthy();
      // Non-channel ("working") threads are unaffected — still under the agent.
      expect(agentRow("default-1")).toBeTruthy();
    });

    it("resolves the active-thread highlight to the Channels-section row, not an Agents-section row", async () => {
      useChatStore.setState((s) => {
        const next = new Map(s.selectedThreadIdByAgent);
        next.set("agent-1", "channel-1");
        return { selectedThreadIdByAgent: next };
      });
      await renderActive([makeAgentSnapshot()], "agent-1");

      const classTokens = (el: Element | null) => el?.className.split(/\s+/) ?? [];
      expect(classTokens(channelRow("channel-1"))).toContain("bg-[var(--bg-hover)]");
      // Never rendered in the Agents section at all, active or otherwise.
      expect(agentRow("channel-1")).toBeNull();
    });

    // Coverage for the rename affordance added to the Channels section
    // (mirrors the Agents-section rows' own kebab, which shares this exact
    // `renameTarget`/modal instance in HomeSidebar).
    describe("rename affordance", () => {
      function channelMenuButton(threadId: string) {
        return container.querySelector(`[data-testid='home-channel-thread-menu-${threadId}']`) as HTMLButtonElement | null;
      }

      function renameInput() {
        return document.querySelector("[data-testid='rename-thread-input']") as HTMLInputElement | null;
      }

      function renameSubmit() {
        return document.querySelector("[data-testid='rename-thread-submit']") as HTMLButtonElement | null;
      }

      // Bypasses React's controlled-input value tracker so the synthetic
      // onChange actually fires (mirrors ThreadTabStrip.test.tsx's own
      // `setInputValue` helper for the same modal).
      function setInputValue(input: HTMLInputElement, value: string) {
        const nativeSetter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
        nativeSetter.call(input, value);
        input.dispatchEvent(new Event("input", { bubbles: true }));
      }

      it("opens the rename modal for a channel thread via its kebab, pre-filled with its current title", async () => {
        await render([makeAgentSnapshot()]);
        await act(async () => {
          channelMenuButton("channel-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        expect(renameInput()?.value).toBe("Slack thread");
      });

      it("calls the store's renameThread with the trimmed title and closes the modal", async () => {
        const mockRenameThread = vi.fn().mockResolvedValue(channelThread);
        useChatStore.setState({ renameThread: mockRenameThread });
        await render([makeAgentSnapshot()]);
        await act(async () => {
          channelMenuButton("channel-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        const input = renameInput()!;
        await act(async () => {
          setInputValue(input, "  Renamed channel  ");
        });
        await act(async () => {
          renameSubmit()!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
          await Promise.resolve();
          await Promise.resolve();
        });
        expect(mockRenameThread).toHaveBeenCalledWith("channel-1", "Renamed channel");
        expect(renameInput()).toBeNull();
      });

      it("calls renameThread with null when the field is cleared, reverting to the channel's fallback label", async () => {
        const mockRenameThread = vi.fn().mockResolvedValue(channelThread);
        useChatStore.setState({ renameThread: mockRenameThread });
        await render([makeAgentSnapshot()]);
        await act(async () => {
          channelMenuButton("channel-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        const input = renameInput()!;
        await act(async () => {
          setInputValue(input, "   ");
        });
        await act(async () => {
          renameSubmit()!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
          await Promise.resolve();
          await Promise.resolve();
        });
        expect(mockRenameThread).toHaveBeenCalledWith("channel-1", null);
      });

      it("does not select the thread as a side effect of opening its rename modal", async () => {
        await render([makeAgentSnapshot()]);
        await act(async () => {
          channelMenuButton("channel-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        // Selecting a thread (the row's own select button) goes through
        // `switchToThread`, which synchronously sets `selectedThreadIdByAgent`
        // — the kebab must never trigger that path.
        expect(useChatStore.getState().selectedThreadIdByAgent.get("agent-1")).not.toBe("channel-1");
      });
    });

    // Coverage for the archive ("close") button added to the Channels
    // section's rows — reuses `handleArchiveThread`, the exact same callback
    // the Agents-section rows' own `X` already calls (generic over
    // agentId/threadId, nothing channel-specific needed).
    describe("archive affordance", () => {
      function channelArchiveButton(threadId: string) {
        return container.querySelector(`[data-testid='home-channel-thread-archive-${threadId}']`) as HTMLButtonElement | null;
      }

      it("calls the store's archiveThread with the row's thread id", async () => {
        const mockArchiveThread = vi.fn().mockResolvedValue(channelThread);
        useChatStore.setState({ archiveThread: mockArchiveThread });
        await render([makeAgentSnapshot()]);

        expect(channelRow("channel-1")).toBeTruthy();
        await act(async () => {
          channelArchiveButton("channel-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        expect(mockArchiveThread).toHaveBeenCalledWith("channel-1");
      });

      it("does not select the thread as a side effect of clicking its archive button", async () => {
        const mockArchiveThread = vi.fn().mockResolvedValue(channelThread);
        useChatStore.setState({ archiveThread: mockArchiveThread });
        await render([makeAgentSnapshot()]);

        await act(async () => {
          channelArchiveButton("channel-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        expect(useChatStore.getState().selectedThreadIdByAgent.get("agent-1")).not.toBe("channel-1");
      });

      it("removes the row from the Channels section once the thread is archived", async () => {
        const mockArchiveThread = vi.fn().mockImplementation(async (threadId: string) => {
          useChatStore.setState((s) => {
            const next = new Map(s.threadsByAgent);
            const threads = (next.get("agent-1") ?? []).map((t) =>
              t.id === threadId ? { ...t, archived_at: "2026-02-01T00:00:00Z" } : t,
            );
            next.set("agent-1", threads);
            return { threadsByAgent: next };
          });
          return channelThread;
        });
        useChatStore.setState({ archiveThread: mockArchiveThread });
        await render([makeAgentSnapshot()]);

        expect(channelRow("channel-1")).toBeTruthy();
        await act(async () => {
          channelArchiveButton("channel-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        expect(channelRow("channel-1")).toBeNull();
      });
    });
  });

  // Coverage for bulk thread hydration (ported from the eager per-agent
  // hydration-pump tests; that pump was deleted and replaced by
  // a single bulk `loadAllThreads()` call into the store — see this effect's
  // own doc comment, right after `sortedAgents`). The original suite's
  // churn/concurrency harness simulated contention against that per-agent
  // pump's concurrency cap; there is no cap and no fan-out left to contend
  // over, so that harness is dropped as noise rather than ported. The two
  // OUTCOME-level assertions it protected survive here, unchanged in
  // substance:
  //
  // 1. every agent Home renders ends up with an entry in `threadsByAgent`,
  //    including a zero-thread agent as an EMPTY ARRAY rather than an
  //    absent key — the exact failure mode `loadAllThreads` (chatStore.ts)
  //    now guards against explicitly.
  // 2. the section-level loading affordance (`home-channels-loading` /
  //    `home-assignments-loading`) actually CLEARS once hydration resolves.
  //    The pump this replaced could leave it stuck on, because its effect
  //    was keyed off an array whose identity changed on every render; the
  //    bulk call must not reintroduce that.
  //
  // `mockListAllThreads` (module-level, backing the mocked
  // `api.listAllThreads`) defaults to an immediately-resolved `{}` via the
  // outer `beforeEach` — each test below overrides it as needed to control
  // timing/contents precisely.
  describe("bulk thread hydration", () => {
    // A real macrotask tick, not just a microtask (`Promise.resolve()`) —
    // guarantees every pending microtask (the mocked `listAllThreads`
    // promise resolving, `loadAllThreads`'s own `await` continuation, its
    // `.finally`) has drained, however many hops deep the chain is. Wrapped
    // in `act` since it's expected to flush React state updates.
    async function flush() {
      await act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 0));
      });
    }

    afterEach(() => {
      useUserPreferencesStore.setState({
        homeExpandedAgentIds: [],
        homeExpandedChannelGroupKeys: [],
        homeExpandedAssignmentGroupKeys: [],
      });
    });

    // (1) The exact failure mode `loadAllThreads` (chatStore.ts) guards
    // against: the bulk `GET /threads` response only ever mentions agents
    // that HAVE threads, so the store must seed an empty array for every
    // known agent missing from the grouped response — `threadsByAgent`
    // membership is used elsewhere as a proxy for "has this agent been
    // hydrated yet", so a dropped key here reads as "never hydrated" and
    // can drive an infinite refetch loop or a spinner that never clears.
    // "agent-2" below is a legitimately-zero-thread agent, absent from the
    // mocked grouped response exactly as the real endpoint would omit it.
    //
    // NEUTERING CHECK (see task report for the full transcript): with the
    // real `loadAllThreads` (stores/chatStore.ts) temporarily replaced by a
    // no-op that never touches `threadsByAgent`, this test fails as
    // expected on the `toBe(true)` assertion below — "agent-2" never
    // appears in the map at all. Restored afterward.
    //
    // DOM-LEVEL EXTENSION (task: "Restore the test coverage that was lost
    // with the pump"): the version of this test ported in from the pump-era
    // suite only ever asserted on `threadsByAgent` — store state, not
    // anything the sidebar actually renders. Nothing in this file proved
    // bulk hydration RENDERS anything, for one agent or several. Extended
    // below (after the pre-existing store-state assertions) to expand two
    // DIFFERENT already-hydrated agents' rows and confirm both of their
    // thread titles are actually visible in the DOM — not just present in
    // the store.
    //
    // NEUTERING CHECK for the DOM extension: with `isOpen` in HomeSidebar.tsx
    // (`const isOpen = expandedAgentIds.has(agent.agent_id);`) temporarily
    // hardcoded to `false`, this test fails as expected — both
    // `home-agent-thread-row-*` queries below return `null` since the
    // indented thread list never renders regardless of expansion state.
    // Restored afterward.
    it("gives every known agent an entry in threadsByAgent, including a zero-thread agent as an empty array (not an absent key), and renders the other agents' thread rows once expanded", async () => {
      const agentOneThread = makeThread({
        id: "thread-1",
        kind: "branch",
        title: "Ship the changelog",
        scope: { type: "AgentChat", agent_id: "agent-1" },
      });
      const agentThreeThread = makeThread({
        id: "thread-3",
        kind: "branch",
        title: "Investigate flaky test",
        scope: { type: "AgentChat", agent_id: "agent-3" },
      });
      mockListAllThreads.mockResolvedValue({
        "agent-1": [agentOneThread],
        "agent-3": [agentThreeThread],
      });

      await render([
        makeAgentSnapshot({ agent_id: "agent-1" }),
        makeAgentSnapshot({ agent_id: "agent-2", name: "Agent Two" }),
        makeAgentSnapshot({ agent_id: "agent-3", name: "Agent Three" }),
      ]);
      await flush();

      const threadsByAgent = useChatStore.getState().threadsByAgent;
      expect(threadsByAgent.get("agent-1")).toEqual([agentOneThread]);
      // The exact assertion the old per-agent pump's starvation tests stood
      // in for: presence, not just "was a fetch attempted" — an absent key
      // reads as un-hydrated, an empty array reads as hydrated-and-empty.
      expect(threadsByAgent.has("agent-2")).toBe(true);
      expect(threadsByAgent.get("agent-2")).toEqual([]);

      // DOM: expand agent-1 and agent-3's rows. Both are already hydrated at
      // this point (bulk hydration resolved above), so this only exercises
      // rendering — the per-agent expand-effect's own
      // `threadsByAgent.has(agentId)` guard means it fires no additional
      // fetch for either.
      await act(async () => {
        useUserPreferencesStore.setState({ homeExpandedAgentIds: ["agent-1", "agent-3"] });
      });
      await flush();

      expect(
        container.querySelector("[data-testid='home-agent-thread-row-thread-1']")?.textContent,
      ).toContain("Ship the changelog");
      expect(
        container.querySelector("[data-testid='home-agent-thread-row-thread-3']")?.textContent,
      ).toContain("Investigate flaky test");
    });

    // RESTORE (ported from the deleted "eager thread hydration" suite's
    // "populates Channels/Assignments on mount without any row being
    // expanded" test — see task report's classification table). Still
    // matters on the bulk path: the Channels/Assignments sections read
    // straight off `threadsByAgent` regardless of `expandedAgentIds` (see
    // `homeChannelThreadItems`/`homeAssignmentThreadItems`'s doc comments
    // above `sortedAgents` in HomeSidebar.tsx), so bulk hydration populating
    // those sections without the user ever expanding an agent row is a real,
    // still-current behavior — and, unlike the pre-existing "channel-origin
    // threads" describe block further up (which sets `threadsByAgent`
    // directly via `setState`, bypassing hydration entirely), nothing else
    // in this file exercises it end-to-end from the mocked bulk fetch
    // through to these sections' DOM.
    //
    // NEUTERING CHECK (see task report for the full transcript): with
    // `homeChannelThreadItems` and `homeAssignmentThreadItems`
    // (HomeSidebar.tsx) temporarily short-circuited to `return [];` before
    // their real bodies, this test fails as expected — both row queries
    // below return `null` even though `threadsByAgent` was populated with
    // exactly the data they should have rendered. Restored afterward.
    it("populates Channels/Assignments sections on mount without any agent row being expanded", async () => {
      function slackThread(id: string, agentId: string): Thread {
        return makeThread({
          id,
          kind: "branch",
          scope: { type: "AgentChat", agent_id: agentId },
          channel_origin: { kind: "slack", binding_id: `binding-${id}` },
        });
      }
      function assignmentThread(id: string, agentId: string, assignmentId: string): Thread {
        return makeThread({
          id,
          kind: "branch",
          scope: { type: "AgentChat", agent_id: agentId },
          assignment_origin: { assignment_id: assignmentId },
        });
      }

      useUserPreferencesStore.setState({
        homeExpandedChannelGroupKeys: ["channel:slack"],
        homeExpandedAssignmentGroupKeys: ["assignment:assign-1"],
      });
      mockListAllThreads.mockResolvedValue({
        "agent-1": [slackThread("chan-1", "agent-1"), assignmentThread("assign-thread-1", "agent-1", "assign-1")],
      });

      await render([makeAgentSnapshot({ agent_id: "agent-1" })]);
      await flush();

      // No agent row was ever expanded — sanity-checks the premise of the test.
      expect(useUserPreferencesStore.getState().homeExpandedAgentIds).toEqual([]);
      expect(container.querySelector("[data-testid='home-channel-thread-row-chan-1']")).toBeTruthy();
      expect(
        container.querySelector("[data-testid='home-assignment-thread-row-assign-thread-1']"),
      ).toBeTruthy();
    });

    // (2) The second outcome carried over from the pump's suite: the
    // section-level loading affordance must actually CLEAR once hydration
    // resolves, and the
    // "genuinely empty" copy must not render while it's still in flight — a
    // not-yet-verified list must never read as a confirmed-empty one.
    it("shows the loading affordance while the bulk fetch is in flight, then clears it once hydration resolves", async () => {
      let resolveThreads!: (grouped: Record<string, Thread[]>) => void;
      mockListAllThreads.mockImplementation(
        () =>
          new Promise<Record<string, Thread[]>>((resolve) => {
            resolveThreads = resolve;
          }),
      );

      await render([makeAgentSnapshot({ agent_id: "agent-1" })]);

      // Still hydrating: the loading affordance is up, and the "empty" copy
      // must NOT render.
      expect(container.querySelector("[data-testid='home-channels-loading']")).toBeTruthy();
      expect(container.querySelector("[data-testid='home-assignments-loading']")).toBeTruthy();
      expect(container.textContent).not.toContain("No channel conversations yet");
      expect(container.textContent).not.toContain("No assignment run threads yet");

      await act(async () => {
        resolveThreads({}); // genuinely empty result
      });
      await flush();

      // Settled: the loading affordance is gone and the genuine-empty copy
      // now renders — the two states are distinguishable, not just "not
      // wrong at any single instant".
      expect(container.querySelector("[data-testid='home-channels-loading']")).toBeNull();
      expect(container.querySelector("[data-testid='home-assignments-loading']")).toBeNull();
      expect(container.textContent).toContain("No channel conversations yet");
      expect(container.textContent).toContain("No assignment run threads yet");
    });

    // (3) Fix: distinguishing a FAILED bulk fetch from a genuinely EMPTY
    // one. Previously `loadAllThreads` had no try/catch at all — a rejected
    // `api.listAllThreads` call left `threadsByAgent` completely untouched,
    // and HomeSidebar's loading flag still cleared (its `.finally`), so it
    // fell straight through to the same "No … yet" copy a genuinely-empty
    // workspace renders. `threadsHydrationError` (chatStore.ts) makes the
    // two distinguishable here, with a retry affordance to recover.
    //
    // NEUTERING CHECK (see task report for the full transcript): with
    // `loadAllThreads`'s try/catch temporarily removed (reverted to the
    // original unguarded `const grouped = await api.listAllThreads();`),
    // this test fails as expected — `threadsHydrationError` is never set,
    // so `home-channels-error` never renders and the assertion on it fails.
    // Restored afterward.
    it("renders a distinguishable error state instead of the empty-state copy when the bulk fetch fails, and a retry recovers", async () => {
      mockListAllThreads.mockRejectedValue(new Error("boom"));

      await render([makeAgentSnapshot({ agent_id: "agent-1" })]);
      await flush();

      expect(container.querySelector("[data-testid='home-channels-error']")).toBeTruthy();
      expect(container.querySelector("[data-testid='home-assignments-error']")).toBeTruthy();
      // The critical assertion: the empty-state copy must NOT render for a
      // failed fetch — that's exactly the confusion this fix exists to kill.
      expect(container.textContent).not.toContain("No channel conversations yet");
      expect(container.textContent).not.toContain("No assignment run threads yet");
      // Settled (failed), not still in flight — the loading affordance from
      // test (2) above must be gone too, not layered underneath the error.
      expect(container.querySelector("[data-testid='home-channels-loading']")).toBeNull();
      expect(container.querySelector("[data-testid='home-assignments-loading']")).toBeNull();

      // Retry re-fires the bulk fetch; a subsequent success clears the
      // error and renders the real (here, genuinely empty) result.
      mockListAllThreads.mockResolvedValueOnce({});
      const retryButton = container.querySelector(
        "[data-testid='home-channels-retry']",
      ) as HTMLButtonElement;
      await act(async () => {
        retryButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      await flush();

      expect(container.querySelector("[data-testid='home-channels-error']")).toBeNull();
      expect(container.textContent).toContain("No channel conversations yet");
    });

    // (4) Fix: the AbortSignal capability was previously dropped on the
    // floor — HomeSidebar created no `AbortController` at all, so unmounting
    // mid-fetch could only ignore the eventual result, never actually cancel
    // the outstanding request. Now unmounting aborts the signal
    // `api.listAllThreads` receives.
    //
    // NEUTERING CHECK (see task report): with the effect's AbortController
    // wiring reverted to call `loadAllThreads(knownAgentIds)` with no signal
    // argument, this test fails as expected — `capturedSignal` stays
    // `undefined`, so `expect(capturedSignal).toBeInstanceOf(AbortSignal)`
    // fails. Restored afterward.
    it("aborts the in-flight bulk fetch's signal on unmount", async () => {
      let capturedSignal: AbortSignal | undefined;
      mockListAllThreads.mockImplementation(
        (signal?: AbortSignal) =>
          new Promise<Record<string, Thread[]>>(() => {
            capturedSignal = signal;
            // Deliberately never resolves — unmount, not settlement, is
            // what this test exercises.
          }),
      );

      await render([makeAgentSnapshot({ agent_id: "agent-1" })]);

      expect(capturedSignal).toBeInstanceOf(AbortSignal);
      expect(capturedSignal?.aborted).toBe(false);

      await act(async () => {
        root.unmount();
      });

      expect(capturedSignal?.aborted).toBe(true);

      // Re-create the root so the shared `afterEach` unmount is a harmless no-op.
      root = createRoot(container);
    });
  });
});
