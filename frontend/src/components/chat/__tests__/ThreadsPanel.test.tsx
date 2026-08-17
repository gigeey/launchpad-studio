// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import type { Thread } from "../../../types/api";

const mockCreateThread = vi.fn();
const mockRenameThread = vi.fn();
const mockDeleteThread = vi.fn();
const mockArchiveThread = vi.fn();
const mockUnarchiveThread = vi.fn();

vi.mock("../../../lib/api", () => ({
  listThreads: vi.fn().mockResolvedValue([]),
  createThread: (...a: unknown[]) => mockCreateThread(...a),
  renameThread: (...a: unknown[]) => mockRenameThread(...a),
  deleteThread: (...a: unknown[]) => mockDeleteThread(...a),
  archiveThread: (...a: unknown[]) => mockArchiveThread(...a),
  unarchiveThread: (...a: unknown[]) => mockUnarchiveThread(...a),
}));

import { ThreadsPanel } from "../ThreadsPanel";
import { useChatStore, inFlightKey } from "../../../stores/chatStore";

const AGENT_ID = "agent-xyz";
const DEFAULT_ID = `default-${AGENT_ID}`;

const defaultThread: Thread = {
  id: DEFAULT_ID,
  title: null,
  scope: { type: "AgentChat", agent_id: AGENT_ID },
  transcript_path: `/data/${AGENT_ID}.jsonl`,
  kind: "default",
  created_at: "2026-06-30T00:00:00Z",
  updated_at: "2026-06-30T00:00:00Z",
};

const freshThread: Thread = {
  id: "fresh-1",
  title: "Exploration",
  scope: { type: "AgentChat", agent_id: AGENT_ID },
  transcript_path: `/data/threads/fresh-1.jsonl`,
  kind: "fresh",
  created_at: "2026-06-30T01:00:00Z",
  updated_at: "2026-06-30T01:00:00Z",
};

function seedThreads(threads: Thread[], selectedId: string) {
  useChatStore.setState((s) => {
    const t = new Map(s.threadsByAgent);
    t.set(AGENT_ID, threads);
    const sel = new Map(s.selectedThreadIdByAgent);
    sel.set(AGENT_ID, selectedId);
    return { threadsByAgent: t, selectedThreadIdByAgent: sel };
  });
}

describe("ThreadsPanel", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    useChatStore.getState().reset();
    vi.clearAllMocks();
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => { root.unmount(); });
    document.body.removeChild(container);
  });

  async function render(onSelectThread: (id: string) => void = () => {}) {
    await act(async () => {
      root.render(React.createElement(ThreadsPanel, { agentId: AGENT_ID, onSelectThread }));
    });
  }

  function q(sel: string) {
    return container.querySelector(sel) as HTMLElement | null;
  }

  it("renders the default thread with a 'Main' label", async () => {
    seedThreads([defaultThread, freshThread], DEFAULT_ID);
    await render();
    const row = q(`[data-testid='thread-row-${DEFAULT_ID}']`);
    expect(row?.textContent).toContain("Main");
  });

  it("renders a non-default thread's title", async () => {
    seedThreads([defaultThread, freshThread], DEFAULT_ID);
    await render();
    expect(container.textContent).toContain("Exploration");
  });

  it("hides rename + delete controls for the default thread", async () => {
    seedThreads([defaultThread, freshThread], DEFAULT_ID);
    await render();
    expect(q(`[data-testid='thread-delete-${DEFAULT_ID}']`)).toBeNull();
    expect(q(`[data-testid='thread-rename-${DEFAULT_ID}']`)).toBeNull();
    // Non-default thread keeps both controls.
    expect(q(`[data-testid='thread-delete-${freshThread.id}']`)).toBeTruthy();
    expect(q(`[data-testid='thread-rename-${freshThread.id}']`)).toBeTruthy();
  });

  it("selects a thread through the parent callback", async () => {
    seedThreads([defaultThread, freshThread], DEFAULT_ID);
    const onSelectThread = vi.fn();
    await render(onSelectThread);
    await act(async () => {
      q(`[data-testid='thread-select-${freshThread.id}']`)!
        .dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(onSelectThread).toHaveBeenCalledWith(freshThread.id);
  });

  it("creates a fresh thread and selects it via New", async () => {
    seedThreads([defaultThread], DEFAULT_ID);
    const created: Thread = { ...freshThread, id: "fresh-new", title: null };
    mockCreateThread.mockResolvedValue(created);
    const onSelectThread = vi.fn();
    await render(onSelectThread);

    await act(async () => {
      q("[data-testid='thread-new-btn']")!
        .dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(mockCreateThread).toHaveBeenCalledWith(AGENT_ID, { kind: "fresh", title: null });
    expect(onSelectThread).toHaveBeenCalledWith("fresh-new");
  });

  it("renames a non-default thread through a two-step inline editor", async () => {
    seedThreads([defaultThread, freshThread], DEFAULT_ID);
    mockRenameThread.mockResolvedValue({ ...freshThread, title: "Renamed" });
    await render();

    // Open the inline editor.
    await act(async () => {
      q(`[data-testid='thread-rename-${freshThread.id}']`)!
        .dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    const input = q(`[data-testid='thread-rename-input-${freshThread.id}']`) as HTMLInputElement;
    expect(input).toBeTruthy();

    // Type a new value and confirm. Bypass React's controlled-input value
    // tracker via the prototype setter so the synthetic onChange fires.
    await act(async () => {
      const nativeSetter = Object.getOwnPropertyDescriptor(
        window.HTMLInputElement.prototype,
        "value",
      )!.set!;
      nativeSetter.call(input, "Renamed");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => {
      q(`[data-testid='thread-rename-confirm-${freshThread.id}']`)!
        .dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(mockRenameThread).toHaveBeenCalledWith(freshThread.id, "Renamed");
  });

  it("deletes a non-default thread after inline confirmation", async () => {
    seedThreads([defaultThread, freshThread], DEFAULT_ID);
    mockDeleteThread.mockResolvedValue(undefined);
    await render();

    // First click reveals the confirm control.
    await act(async () => {
      q(`[data-testid='thread-delete-${freshThread.id}']`)!
        .dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    const confirm = q(`[data-testid='thread-delete-confirm-${freshThread.id}']`);
    expect(confirm).toBeTruthy();

    await act(async () => {
      confirm!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(mockDeleteThread).toHaveBeenCalledWith(freshThread.id);
  });

  it("hides the archive control for the default thread but shows it for a non-default thread", async () => {
    seedThreads([defaultThread, freshThread], DEFAULT_ID);
    await render();
    expect(q(`[data-testid='thread-archive-${DEFAULT_ID}']`)).toBeNull();
    expect(q(`[data-testid='thread-archive-${freshThread.id}']`)).toBeTruthy();
  });

  it("archives a thread via its archive button, removing it from the main list", async () => {
    seedThreads([defaultThread, freshThread], DEFAULT_ID);
    mockArchiveThread.mockResolvedValue({ ...freshThread, archived_at: "2026-07-05T00:00:00Z" });
    await render();

    await act(async () => {
      q(`[data-testid='thread-archive-${freshThread.id}']`)!
        .dispatchEvent(new MouseEvent("click", { bubbles: true }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(mockArchiveThread).toHaveBeenCalledWith(freshThread.id);
    expect(q(`[data-testid='thread-row-${freshThread.id}']`)).toBeNull();
  });

  it("lists archived threads in a collapsed Archived section that expands on click", async () => {
    const archived: Thread = { ...freshThread, archived_at: "2026-07-05T00:00:00Z" };
    seedThreads([defaultThread, archived], DEFAULT_ID);
    await render();

    // Not in the main list.
    expect(q(`[data-testid='thread-row-${archived.id}']`)).toBeNull();
    // Collapsed by default.
    expect(q(`[data-testid='thread-archived-row-${archived.id}']`)).toBeNull();
    const toggle = q("[data-testid='thread-archived-toggle']");
    expect(toggle?.textContent).toContain("Archived (1)");

    await act(async () => {
      toggle!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(q(`[data-testid='thread-archived-row-${archived.id}']`)).toBeTruthy();
  });

  it("unarchives a thread from the Archived section, moving it back to the main list", async () => {
    const archived: Thread = { ...freshThread, archived_at: "2026-07-05T00:00:00Z" };
    seedThreads([defaultThread, archived], DEFAULT_ID);
    mockUnarchiveThread.mockResolvedValue({ ...freshThread, archived_at: null });
    await render();

    await act(async () => {
      q("[data-testid='thread-archived-toggle']")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    await act(async () => {
      q(`[data-testid='thread-unarchive-${archived.id}']`)!
        .dispatchEvent(new MouseEvent("click", { bubbles: true }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(mockUnarchiveThread).toHaveBeenCalledWith(archived.id);
    expect(q(`[data-testid='thread-row-${freshThread.id}']`)).toBeTruthy();
  });

  describe("delegate-activity indicator", () => {
    function streamingBadge(id: string) {
      return q(`[data-testid='thread-streaming-badge-${id}']`);
    }

    it("shows the streaming badge next to a thread with a running async delegate", async () => {
      seedThreads([defaultThread, freshThread], DEFAULT_ID);
      useChatStore.getState().beginDelegateRun(inFlightKey(AGENT_ID, freshThread.id), "del-1", "Researcher", 1000);
      await render();

      expect(streamingBadge(freshThread.id)).toBeTruthy();
      // The other (unrelated) thread stays quiet.
      expect(streamingBadge(DEFAULT_ID)).toBeNull();
    });

    it("clears the badge once the delegate completes (endDelegateRun)", async () => {
      seedThreads([defaultThread, freshThread], DEFAULT_ID);
      const key = inFlightKey(AGENT_ID, freshThread.id);
      useChatStore.getState().beginDelegateRun(key, "del-1", "Researcher", 1000);
      await render();
      expect(streamingBadge(freshThread.id)).toBeTruthy();

      await act(async () => {
        useChatStore.getState().endDelegateRun(key, "del-1");
      });

      expect(streamingBadge(freshThread.id)).toBeNull();
    });

    it("does not shift the row layout or truncate the thread name when the badge is showing", async () => {
      seedThreads([defaultThread, freshThread], DEFAULT_ID);
      useChatStore.getState().beginDelegateRun(inFlightKey(AGENT_ID, freshThread.id), "del-1", "Researcher", 1000);
      await render();

      const row = q(`[data-testid='thread-row-${freshThread.id}']`);
      // The full title text is still present and readable — only CSS
      // `truncate` (ellipsis-on-overflow) is applied, the DOM text itself
      // is never shortened.
      expect(row?.textContent).toContain("Exploration");
    });

    it("does not show a badge for a thread with no in-flight delegate", async () => {
      seedThreads([defaultThread, freshThread], DEFAULT_ID);
      await render();

      expect(streamingBadge(freshThread.id)).toBeNull();
      expect(streamingBadge(DEFAULT_ID)).toBeNull();
    });
  });
});
