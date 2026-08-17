// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import type { MemoryEntry } from "../../../types/api";

const mockGetMemories = vi.fn();
const mockAddMemory = vi.fn();
const mockDeleteMemory = vi.fn();
const mockGetGlobalMemories = vi.fn();
const mockAddGlobalMemory = vi.fn();
const mockDeleteGlobalMemory = vi.fn();
const mockGetProjectMemories = vi.fn();
const mockAddProjectMemory = vi.fn();
const mockDeleteProjectMemory = vi.fn();
const mockGetThreadMemories = vi.fn();
const mockAddThreadMemory = vi.fn();
const mockDeleteThreadMemory = vi.fn();

vi.mock("../../../lib/api", () => ({
  getMemories: (...a: unknown[]) => mockGetMemories(...a),
  addMemory: (...a: unknown[]) => mockAddMemory(...a),
  deleteMemory: (...a: unknown[]) => mockDeleteMemory(...a),
  getGlobalMemories: (...a: unknown[]) => mockGetGlobalMemories(...a),
  addGlobalMemory: (...a: unknown[]) => mockAddGlobalMemory(...a),
  deleteGlobalMemory: (...a: unknown[]) => mockDeleteGlobalMemory(...a),
  getProjectMemories: (...a: unknown[]) => mockGetProjectMemories(...a),
  addProjectMemory: (...a: unknown[]) => mockAddProjectMemory(...a),
  deleteProjectMemory: (...a: unknown[]) => mockDeleteProjectMemory(...a),
  getThreadMemories: (...a: unknown[]) => mockGetThreadMemories(...a),
  addThreadMemory: (...a: unknown[]) => mockAddThreadMemory(...a),
  deleteThreadMemory: (...a: unknown[]) => mockDeleteThreadMemory(...a),
}));

import { MemoryPanel } from "../MemoryPanel";

const AGENT_ID = "agent-1";
const THREAD_ID = "thread-1";

const threadEntry: MemoryEntry = {
  id: "mem-thread-1",
  content: "keep an eye on the flaky test",
  created_at: "2026-07-19T00:00:00Z",
  source: "Manual",
  scope: "Thread",
};

describe("MemoryPanel — This thread tab", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    vi.clearAllMocks();
    mockGetMemories.mockResolvedValue([]);
    mockGetGlobalMemories.mockResolvedValue([]);
    mockGetProjectMemories.mockResolvedValue([]);
    mockGetThreadMemories.mockResolvedValue([]);
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => { root.unmount(); });
    document.body.removeChild(container);
  });

  async function render(threadId: string | undefined) {
    await act(async () => {
      root.render(React.createElement(MemoryPanel, { agentId: AGENT_ID, threadId }));
      await Promise.resolve();
      await Promise.resolve();
    });
  }

  function q(sel: string) {
    return container.querySelector(sel) as HTMLElement | null;
  }

  async function switchToThreadTab() {
    await act(async () => {
      q("[data-testid='memory-tab-thread']")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
  }

  it("renders 'This thread' as the first tab", async () => {
    await render(THREAD_ID);
    const tabs = Array.from(container.querySelectorAll("[data-testid^='memory-tab-']"));
    expect(tabs[0].getAttribute("data-testid")).toBe("memory-tab-thread");
    expect(tabs[0].textContent).toContain("This thread");
  });

  it("lists thread memories fetched via getThreadMemories when the tab is active", async () => {
    mockGetThreadMemories.mockResolvedValue([threadEntry]);
    await render(THREAD_ID);
    await switchToThreadTab();

    expect(mockGetThreadMemories).toHaveBeenCalledWith(THREAD_ID);
    expect(container.textContent).toContain("keep an eye on the flaky test");
  });

  it("adds a thread memory via addThreadMemory and refreshes the list", async () => {
    mockGetThreadMemories.mockResolvedValueOnce([]).mockResolvedValueOnce([threadEntry]);
    mockAddThreadMemory.mockResolvedValue(threadEntry);
    await render(THREAD_ID);
    await switchToThreadTab();

    const textarea = q("textarea") as HTMLTextAreaElement;
    await act(async () => {
      const nativeSetter = Object.getOwnPropertyDescriptor(
        window.HTMLTextAreaElement.prototype,
        "value",
      )!.set!;
      nativeSetter.call(textarea, "keep an eye on the flaky test");
      textarea.dispatchEvent(new Event("input", { bubbles: true }));
    });

    await act(async () => {
      textarea.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }),
      );
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(mockAddThreadMemory).toHaveBeenCalledWith(THREAD_ID, "keep an eye on the flaky test");
    expect(mockGetThreadMemories).toHaveBeenCalledTimes(2);
  });

  it("deletes a thread memory via deleteThreadMemory and refreshes the list", async () => {
    mockGetThreadMemories.mockResolvedValue([threadEntry]);
    mockDeleteThreadMemory.mockResolvedValue(undefined);
    await render(THREAD_ID);
    await switchToThreadTab();

    const deleteBtn = q("[aria-label='Delete memory']");
    expect(deleteBtn).toBeTruthy();

    await act(async () => {
      deleteBtn!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(mockDeleteThreadMemory).toHaveBeenCalledWith(THREAD_ID, threadEntry.id);
    expect(mockGetThreadMemories).toHaveBeenCalledTimes(2);
  });

  it("disables the thread tab gracefully when threadId is falsy", async () => {
    await render(undefined);

    const tab = q("[data-testid='memory-tab-thread']") as HTMLButtonElement;
    expect(tab.disabled).toBe(true);

    // Clicking a disabled tab must not switch to it or throw.
    await act(async () => {
      tab.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(mockGetThreadMemories).not.toHaveBeenCalled();
    // Still showing the default "All" tab content, not a crash.
    expect(q("[data-testid='memory-tab-all']")).toBeTruthy();
  });
});
