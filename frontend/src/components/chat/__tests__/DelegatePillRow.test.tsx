// @vitest-environment jsdom
/**
 * DelegatePillRow — one pill per async delegate running in the active
 * thread, sourced from `runningDelegatesByThread` (see that store field's
 * own doc comment, and DelegatePillRow.tsx's).
 *
 * Covers the acceptance criteria from the delegate-pills task:
 *  (a) a pill renders per running delegate with its name
 *  (b) clicking kill calls the cancel endpoint with the right delegation id
 *      and enters "Stopping…" WITHOUT removing the pill
 *  (c) the pill disappears when `delegate.complete` arrives for that id
 *      (simulated here via `endDelegateRun`, the same store action
 *      useSSE.ts's real `delegate.complete` handler calls)
 *  (d) a stuck "Stopping…" pill reaches the failure state after the
 *      10s regression-detector timeout (fake timers)
 *  (e) with more than 3 running, exactly 3 render plus a "+N more"
 *  (f) kill-all issues one cancel call per delegation in the thread and
 *      none for delegations in other threads
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

const mockCancelDelegate = vi.fn().mockResolvedValue({ status: "cancelled", id: "unused" });
vi.mock("../../../lib/api", () => ({
  cancelDelegate: (...args: unknown[]) => mockCancelDelegate(...args),
}));

import { DelegatePillRow } from "../DelegatePillRow";
import { useChatStore, inFlightKey } from "../../../stores/chatStore";

const AGENT_ID = "agent-1";
const THREAD_ID = "thread-1";
const KEY = inFlightKey(AGENT_ID, THREAD_ID);

function seedDelegate(key: string, id: string, name: string, startedAt: number) {
  useChatStore.getState().beginDelegateRun(key, id, name, startedAt);
}

describe("DelegatePillRow", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    useChatStore.getState().reset();
    mockCancelDelegate.mockClear();
    mockCancelDelegate.mockResolvedValue({ status: "cancelled", id: "unused" });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function render(threadId: string | undefined = THREAD_ID) {
    await act(async () => {
      root.render(React.createElement(DelegatePillRow, { agentId: AGENT_ID, threadId }));
    });
  }

  function pill(id: string): HTMLElement | null {
    return container.querySelector(`[data-testid="delegate-pill-${id}"]`);
  }

  function killButton(id: string): HTMLButtonElement | null {
    return container.querySelector(`[data-testid="delegate-pill-kill-${id}"]`);
  }

  it("renders nothing when no delegates are running on this thread", async () => {
    await render();
    expect(container.querySelector('[data-testid="delegate-pill-row"]')).toBeNull();
  });

  it("(a) renders a pill per running delegate with its name", async () => {
    seedDelegate(KEY, "d1", "Researcher", Date.now());
    seedDelegate(KEY, "d2", "Reviewer", Date.now());
    await render();

    expect(pill("d1")).toBeTruthy();
    expect(pill("d1")?.textContent).toContain("Researcher");
    expect(pill("d2")).toBeTruthy();
    expect(pill("d2")?.textContent).toContain("Reviewer");
  });

  it("(b) clicking kill calls the cancel endpoint with the right delegation id and enters 'Stopping…' without removing the pill", async () => {
    seedDelegate(KEY, "d1", "Researcher", Date.now());
    await render();

    await act(async () => {
      killButton("d1")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(mockCancelDelegate).toHaveBeenCalledTimes(1);
    expect(mockCancelDelegate).toHaveBeenCalledWith("d1");
    // Still present — never removed on click, only on `delegate.complete`.
    expect(pill("d1")).toBeTruthy();
    expect(pill("d1")?.getAttribute("data-kill-state")).toBe("stopping");
    expect(pill("d1")?.textContent).toContain("Stopping…");
    expect(killButton("d1")?.disabled).toBe(true);
  });

  it("(c) the pill disappears when delegate.complete arrives for that id", async () => {
    seedDelegate(KEY, "d1", "Researcher", Date.now());
    seedDelegate(KEY, "d2", "Reviewer", Date.now());
    await render();

    expect(pill("d1")).toBeTruthy();

    // Mirrors what useSSE.ts's real `delegate.complete` handler does on
    // receipt of the SSE event: it calls `endDelegateRun` with the
    // delegation id, nothing more.
    await act(async () => {
      useChatStore.getState().endDelegateRun(KEY, "d1");
    });

    expect(pill("d1")).toBeNull();
    expect(pill("d2")).toBeTruthy();
  });

  it("(d) a stuck 'Stopping…' pill reaches the failure state after the 10s regression-detector timeout", async () => {
    seedDelegate(KEY, "d1", "Researcher", Date.now());
    await render();

    vi.useFakeTimers();
    try {
      await act(async () => {
        killButton("d1")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      expect(pill("d1")?.getAttribute("data-kill-state")).toBe("stopping");

      // Just under the timeout — still "stopping", not yet stuck. Proves
      // this isn't firing prematurely.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(9_999);
      });
      expect(pill("d1")?.getAttribute("data-kill-state")).toBe("stopping");

      // Crossing the 10s mark — no `delegate.complete` ever arrived (this
      // test never calls `endDelegateRun`), so the pill must resolve to the
      // visible failure state on its own.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(1);
      });
      expect(pill("d1")?.getAttribute("data-kill-state")).toBe("stuck");
      expect(pill("d1")?.textContent).toContain("Stop failed");
    } finally {
      vi.useRealTimers();
    }
  });

  it("(e) with more than 3 running, exactly 3 render plus a '+N more'", async () => {
    const now = Date.now();
    // Staggered, oldest-first startedAt so the visible/overflow split is
    // deterministic: d1..d3 are oldest (visible), d4 is newest (overflow).
    seedDelegate(KEY, "d1", "Agent One", now - 4_000);
    seedDelegate(KEY, "d2", "Agent Two", now - 3_000);
    seedDelegate(KEY, "d3", "Agent Three", now - 2_000);
    seedDelegate(KEY, "d4", "Agent Four", now - 1_000);
    await render();

    expect(pill("d1")).toBeTruthy();
    expect(pill("d2")).toBeTruthy();
    expect(pill("d3")).toBeTruthy();
    expect(pill("d4")).toBeNull();

    const overflow = container.querySelector('[data-testid="delegate-pill-overflow"]');
    expect(overflow).toBeTruthy();
    expect(overflow?.textContent).toBe("+1 more");
  });

  it("(f) kill-all issues one cancel call per delegation in the thread and none for delegations in other threads", async () => {
    const now = Date.now();
    seedDelegate(KEY, "d1", "Agent One", now - 4_000);
    seedDelegate(KEY, "d2", "Agent Two", now - 3_000);
    seedDelegate(KEY, "d3", "Agent Three", now - 2_000);
    seedDelegate(KEY, "d4", "Agent Four", now - 1_000);

    // A delegate running on a DIFFERENT thread of the same agent — must
    // never be touched by this row's kill-all.
    const otherKey = inFlightKey(AGENT_ID, "thread-2");
    seedDelegate(otherKey, "other-1", "Elsewhere", now);

    await render();

    const killAll = container.querySelector('[data-testid="delegate-pill-kill-all"]') as HTMLButtonElement | null;
    expect(killAll).toBeTruthy();

    await act(async () => {
      killAll?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(mockCancelDelegate).toHaveBeenCalledTimes(4);
    const calledIds = mockCancelDelegate.mock.calls.map((c) => c[0]);
    expect(calledIds.sort()).toEqual(["d1", "d2", "d3", "d4"]);
    expect(calledIds).not.toContain("other-1");
  });

  it("does not show a kill-all button when there are 3 or fewer delegates (no overflow to reach)", async () => {
    const now = Date.now();
    seedDelegate(KEY, "d1", "Agent One", now - 2_000);
    seedDelegate(KEY, "d2", "Agent Two", now - 1_000);
    await render();

    expect(container.querySelector('[data-testid="delegate-pill-kill-all"]')).toBeNull();
    expect(container.querySelector('[data-testid="delegate-pill-overflow"]')).toBeNull();
  });

  it("only shows delegates for the requested thread, not other threads of the same agent", async () => {
    seedDelegate(KEY, "d1", "Researcher", Date.now());
    const otherKey = inFlightKey(AGENT_ID, "thread-2");
    seedDelegate(otherKey, "other-1", "Elsewhere", Date.now());
    await render();

    expect(pill("d1")).toBeTruthy();
    expect(pill("other-1")).toBeNull();
  });
});
