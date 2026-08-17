// @vitest-environment jsdom
//
// Coverage for the Assignments pill/popover/pinned-column trio, built to
// mirror the Channels tile one-for-one (see ThreadTabStrip.channels.test.tsx,
// which this file's structure follows directly): an assignment-originated
// thread (`assignment_origin != null`) must never render as a loose pill in
// ThreadTabStrip — it's folded into the collapsed "Assignments" tile
// instead, sub-grouped by `assignment_origin.assignment_id` via the shared
// `resolveAssignmentThreadPartition` selector (lib/assignmentThreads.ts).
// These tests drive the real component: they assert assignment threads are
// absent from the normal pill row, that the Assignments tile appears with
// the correct aggregate unread badge, that it hides when there are none, and
// that clicking a row reuses the strip's own `onSelectThread`.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { ThreadTabStrip } from "../ThreadTabStrip";
import { useChatStore, inFlightKey } from "../../../stores/chatStore";
import type { Assignment, AssignmentBridgeOrigin, Thread } from "../../../types/api";

vi.mock("../../../lib/api", () => ({
  getAgents: vi.fn().mockResolvedValue([]),
  getAgent: vi.fn().mockResolvedValue(null),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  listThreads: vi.fn().mockResolvedValue([]),
  sendMessage: vi.fn().mockResolvedValue({ message_id: "msg-1", status: "queued" }),
}));

const AGENT_ID = "agent-1";

function makeThread(overrides: Partial<Thread> & { id: string }): Thread {
  return {
    title: null,
    scope: { type: "AgentChat", agent_id: AGENT_ID },
    transcript_path: `/tmp/${overrides.id}.jsonl`,
    kind: "fresh",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function assignmentOrigin(assignmentId = "assignment-1"): AssignmentBridgeOrigin {
  return { assignment_id: assignmentId };
}

const defaultThread = makeThread({ id: "default-1", kind: "default" });

describe("ThreadTabStrip — collapsed Assignments tile", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    useChatStore.getState().reset();
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function render(
    threads: Thread[],
    activeThreadId = "default-1",
    onSelectThread: (id: string) => void = () => {},
    onRenameThread: (id: string, title: string | null) => Promise<unknown> = () => Promise.resolve(),
    onArchiveThread: (id: string) => void = () => {},
  ) {
    await act(async () => {
      root.render(
        React.createElement(ThreadTabStrip, {
          agentId: AGENT_ID,
          threads,
          activeThreadId,
          onSelectThread,
          onCreateThread: () => {},
          onArchiveThread,
          onDeleteThread: () => {},
          onRenameThread,
          onUnarchiveThread: () => {},
        }),
      );
    });
  }

  function tab(id: string) {
    return container.querySelector(`[data-testid='thread-tab-${id}']`);
  }

  function assignmentsTile() {
    return container.querySelector("[data-testid='thread-tab-assignments']") as HTMLButtonElement | null;
  }

  function unreadBadge() {
    return container.querySelector("[data-testid='assignments-tile-unread-badge']");
  }

  it("never renders a loose pill for an assignment-originated thread", async () => {
    const assignmentThread = makeThread({ id: "run-1", title: "Nightly digest", assignment_origin: assignmentOrigin() });
    await render([defaultThread, assignmentThread]);
    expect(tab("default-1")).toBeTruthy();
    expect(tab("run-1")).toBeNull();
  });

  it("hides the Assignments tile entirely when the agent has no assignment threads", async () => {
    await render([defaultThread, makeThread({ id: "working-1" })]);
    expect(assignmentsTile()).toBeNull();
  });

  it("shows the Assignments tile once at least one assignment thread exists", async () => {
    const assignmentThread = makeThread({ id: "run-1", assignment_origin: assignmentOrigin() });
    await render([defaultThread, assignmentThread]);
    expect(assignmentsTile()).toBeTruthy();
  });

  it("renders no aggregate unread badge when no assignment thread is unread", async () => {
    const assignmentThread = makeThread({ id: "run-1", assignment_origin: assignmentOrigin() });
    await render([defaultThread, assignmentThread]);
    expect(unreadBadge()).toBeNull();
  });

  it("renders the correct aggregate unread badge across every assignment group", async () => {
    const unreadRun = makeThread({ id: "run-unread", assignment_origin: assignmentOrigin("assignment-1") });
    const readRun = makeThread({ id: "run-read", assignment_origin: assignmentOrigin("assignment-1") });
    const unreadOtherRun = makeThread({ id: "run-other-unread", assignment_origin: assignmentOrigin("assignment-2") });

    useChatStore.setState({
      unreadThreadIds: new Set([
        inFlightKey(AGENT_ID, "run-unread"),
        inFlightKey(AGENT_ID, "run-other-unread"),
      ]),
    });

    await render([defaultThread, unreadRun, readRun, unreadOtherRun]);
    expect(unreadBadge()?.textContent).toBe("2");
  });

  it("expands the tile to reveal assignment run-threads sub-grouped by assignment, resolving the group label from assignmentsByAgent, and clicking a row reuses onSelectThread", async () => {
    const assignment: Assignment = {
      id: "assignment-1",
      agent_id: AGENT_ID,
      name: "Nightly digest",
      instruction: "Summarize the day",
      trigger: { type: "Cron", cron_expr: "0 9 * * *", is_recurring: true },
      bindings: [],
      output_mode: "background",
      thread_policy: "fresh",
      enabled: true,
      created_ts: "2026-01-01T00:00:00Z",
      updated_ts: "2026-01-01T00:00:00Z",
    };
    useChatStore.setState((state) => ({
      assignmentsByAgent: new Map(state.assignmentsByAgent).set(AGENT_ID, [assignment]),
    }));

    const onSelectThread = vi.fn();
    const runThread = makeThread({ id: "run-1", assignment_origin: assignmentOrigin("assignment-1") });
    await render([defaultThread, runThread], "default-1", onSelectThread);

    expect(document.querySelector("[data-testid='assignments-tile-panel']")).toBeNull();
    await act(async () => {
      assignmentsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(document.querySelector("[data-testid='assignments-tile-panel']")).toBeTruthy();
    expect(document.querySelector("[data-testid='assignments-tile-group-assignment-1']")).toBeTruthy();
    expect(document.querySelector("[data-testid='assignments-tile-group-assignment-1']")?.textContent).toContain(
      "Nightly digest",
    );

    const row = document.querySelector("[data-testid='assignments-tile-row-run-1']") as HTMLButtonElement;
    await act(async () => {
      row.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(onSelectThread).toHaveBeenCalledWith("run-1");
  });

  describe("archive via the row's close button", () => {
    async function openAssignmentsTile(threads: Thread[], onArchiveThread?: (id: string) => void) {
      await render(threads, "default-1", () => {}, undefined, onArchiveThread);
      await act(async () => {
        assignmentsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
    }

    function archiveButton(threadId: string) {
      return document.querySelector(`[data-testid='assignments-tile-archive-${threadId}']`) as HTMLButtonElement | null;
    }

    it("calls the strip's onArchiveThread with the row's thread id", async () => {
      const onArchiveThread = vi.fn();
      const runThread = makeThread({ id: "run-1", assignment_origin: assignmentOrigin() });
      await openAssignmentsTile([defaultThread, runThread], onArchiveThread);

      await act(async () => {
        archiveButton("run-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      expect(onArchiveThread).toHaveBeenCalledWith("run-1");
    });
  });

  describe("live streaming badge on an assignment thread row", () => {
    it("shows the streaming badge on a non-selected assignment thread whose in-flight buffer is non-empty", async () => {
      const runThread = makeThread({ id: "run-1", assignment_origin: assignmentOrigin() });
      // activeThreadId stays "default-1" — run-1 is never selected, mirroring
      // a background/cron assignment fire the operator hasn't navigated to.
      await render([defaultThread, runThread]);

      await act(async () => {
        useChatStore.getState().appendInFlightDelta(inFlightKey(AGENT_ID, "run-1"), "Working on it…");
      });
      await act(async () => {
        assignmentsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });

      expect(document.querySelector("[data-testid='thread-streaming-badge-run-1']")).toBeTruthy();
      expect(document.querySelector("[data-testid='thread-unread-dot-run-1']")).toBeNull();
    });

    it("falls back to the unread dot once the thread's in-flight buffer clears and it finalized while unopened", async () => {
      const runThread = makeThread({ id: "run-1", assignment_origin: assignmentOrigin() });
      await render([defaultThread, runThread]);

      const key = inFlightKey(AGENT_ID, "run-1");
      await act(async () => {
        useChatStore.getState().appendInFlightDelta(key, "Working on it…");
        useChatStore.getState().finalizeInFlightText(key, "Working on it…");
        useChatStore.getState().deleteInFlight(key);
      });
      await act(async () => {
        assignmentsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });

      expect(document.querySelector("[data-testid='thread-streaming-badge-run-1']")).toBeNull();
      expect(document.querySelector("[data-testid='thread-unread-dot-run-1']")).toBeTruthy();
    });
  });

  describe("pin/unpin the Assignments column", () => {
    it("pinning from the popover sets the store's pinned flag for this agent", async () => {
      const runThread = makeThread({ id: "run-1", assignment_origin: assignmentOrigin() });
      await render([defaultThread, runThread]);
      await act(async () => {
        assignmentsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      const pinButton = document.querySelector("[data-testid='assignments-tile-panel-pin']") as HTMLButtonElement;
      expect(pinButton).toBeTruthy();

      await act(async () => {
        pinButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });

      expect(useChatStore.getState().assignmentsColumnPinnedByAgent.get(AGENT_ID)).toBe(true);
    });

    it("hides the Pin button once already pinned", async () => {
      useChatStore.getState().setAssignmentsColumnPinned(AGENT_ID, true);
      const runThread = makeThread({ id: "run-1", assignment_origin: assignmentOrigin() });
      await render([defaultThread, runThread]);
      await act(async () => {
        assignmentsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      expect(document.querySelector("[data-testid='assignments-tile-panel-pin']")).toBeNull();
    });
  });
});
