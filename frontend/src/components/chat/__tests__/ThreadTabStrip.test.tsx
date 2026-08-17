// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { ThreadTabStrip } from "../ThreadTabStrip";
import { useChatStore, inFlightKey } from "../../../stores/chatStore";
import type { AgentSnapshot, PendingForm, Thread } from "../../../types/api";
import type { FormRequestPayload } from "../../../types/form";

// Only exercised by the "clears on send" describe block below — every other
// test in this file drives `useChatStore.setState` directly and never
// touches the network. `sendMessage`/`getAgents` are mocked so `chatStore`'s
// real `sendMessage` action (which posts, then refetches agents) can be
// driven end-to-end here without an actual network call.
const mockSendMessage = vi.fn().mockResolvedValue({ message_id: "msg-1", status: "queued" });
const mockGetAgents = vi.fn().mockResolvedValue([]);
vi.mock("../../../lib/api", () => ({
  getAgents: (...args: unknown[]) => mockGetAgents(...args),
  getAgent: vi.fn().mockResolvedValue(null),
  getMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  listThreads: vi.fn().mockResolvedValue([]),
  sendMessage: (...args: unknown[]) => mockSendMessage(...args),
}));

const AGENT_ID = "agent-1";

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

function makeSyncForm(overrides: Partial<FormRequestPayload> = {}): FormRequestPayload {
  return {
    form_id: "form-1",
    agent_id: AGENT_ID,
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

describe("ThreadTabStrip", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    useChatStore.getState().reset();
  });

  afterEach(async () => {
    await act(async () => { root.unmount(); });
    document.body.removeChild(container);
  });

  async function render(
    threads: Thread[],
    activeThreadId: string,
    onSelectThread: (id: string) => void = () => {},
    onCreateThread: () => void = () => {},
    onArchiveThread: (id: string) => void = () => {},
    agentId: string = AGENT_ID,
    onRenameThread: (id: string, title: string | null) => Promise<unknown> = () => Promise.resolve(),
    onDeleteThread: (id: string) => void | Promise<void> = () => {},
    onUnarchiveThread: (id: string) => void | Promise<void> = () => {},
  ) {
    await act(async () => {
      root.render(
        React.createElement(ThreadTabStrip, {
          agentId,
          threads,
          activeThreadId,
          onSelectThread,
          onCreateThread,
          onArchiveThread,
          onDeleteThread,
          onRenameThread,
          onUnarchiveThread,
        }),
      );
    });
  }

  function tab(id: string) {
    return container.querySelector(`[data-testid='thread-tab-${id}']`) as HTMLButtonElement;
  }

  function archiveBtn(id: string) {
    return container.querySelector(`[data-testid='thread-archive-${id}']`) as HTMLButtonElement | null;
  }

  const defaultThread = makeThread({ id: "default-1", kind: "default" });
  const branchThread = makeThread({ id: "branch-1", kind: "branch", title: "Investigate bug" });
  const freshThread = makeThread({ id: "fresh-1", kind: "fresh" });

  it("renders one pill per thread, default thread labelled Main thread", async () => {
    await render([defaultThread, branchThread], "default-1");
    expect(tab("default-1")).toBeTruthy();
    expect(tab("branch-1")).toBeTruthy();
    expect(container.textContent).toContain("Main thread");
    expect(container.textContent).toContain("Investigate bug");
  });

  it("pins the default thread first regardless of input order", async () => {
    await render([branchThread, defaultThread], "default-1");
    const tabs = container.querySelectorAll("[role='tab']");
    expect(tabs[0].getAttribute("data-testid")).toBe("thread-tab-default-1");
  });

  it("falls back to a kind-derived label when a thread has no title", async () => {
    await render([defaultThread, freshThread], "default-1");
    expect(container.textContent).toContain("New thread");
  });

  it("hints 'Right-click to rename' in the tooltip for a thread still on its placeholder label", async () => {
    await render([defaultThread, freshThread], "default-1");
    const labelSpan = tab("fresh-1").querySelector("span.truncate") as HTMLElement;
    await act(async () => {
      labelSpan.dispatchEvent(
        new MouseEvent("mouseover", { bubbles: true, relatedTarget: document.body }),
      );
      await new Promise((resolve) => setTimeout(resolve, 750));
    });
    expect(document.body.textContent).toContain("Right-click to rename");
  });

  it("does not show the rename hint once a thread has a title or auto_title", async () => {
    await render([defaultThread, branchThread], "default-1");
    const labelSpan = tab("branch-1").querySelector("span.truncate") as HTMLElement;
    await act(async () => {
      labelSpan.dispatchEvent(
        new MouseEvent("mouseover", { bubbles: true, relatedTarget: document.body }),
      );
      await new Promise((resolve) => setTimeout(resolve, 750));
    });
    expect(document.body.textContent).not.toContain("Right-click to rename");
  });

  it("never shows the rename hint on the default (Main) thread pill", async () => {
    await render([defaultThread, freshThread], "default-1");
    const labelSpan = tab("default-1").querySelector("span.truncate") as HTMLElement;
    await act(async () => {
      labelSpan.dispatchEvent(
        new MouseEvent("mouseover", { bubbles: true, relatedTarget: document.body }),
      );
      await new Promise((resolve) => setTimeout(resolve, 750));
    });
    expect(document.body.textContent).not.toContain("Right-click to rename");
  });

  it("falls back to auto_title when title is unset", async () => {
    const autoTitled = makeThread({ id: "auto-1", kind: "fresh", auto_title: "Fix login redirect" });
    await render([defaultThread, autoTitled], "default-1");
    expect(container.textContent).toContain("Fix login redirect");
  });

  it("prefers an explicit title over auto_title", async () => {
    const both = makeThread({
      id: "both-1",
      kind: "fresh",
      title: "Explicit name",
      auto_title: "Should not show",
    });
    await render([defaultThread, both], "default-1");
    expect(container.textContent).toContain("Explicit name");
    expect(container.textContent).not.toContain("Should not show");
  });

  it("truncates a long label in the pill but exposes the full label via the shared tooltip on hover", async () => {
    const longTitle = "This is a very long thread title that exceeds the tab budget";
    const longThread = makeThread({ id: "long-1", kind: "fresh", title: longTitle });
    await render([defaultThread, longThread], "default-1");
    const pillText = tab("long-1").textContent ?? "";
    expect(pillText.length).toBeLessThan(longTitle.length);
    expect(pillText.endsWith("…")).toBe(true);
    // No native `title` anymore — replaced by the shared Tooltip component so
    // it doesn't double up with the fancy hover pill.
    expect(tab("long-1").getAttribute("title")).toBeNull();

    // React synthesizes onMouseEnter/onMouseLeave from native "mouseover"/
    // "mouseout" (mouseenter/mouseleave themselves don't bubble, so React
    // doesn't listen for them directly) — dispatch "mouseover" with a
    // `relatedTarget` outside the tree so React sees it as entering from
    // outside, on the label span itself so the Tooltip's anchor div (which
    // wraps only that span, not the whole tab button) is on the path. Real
    // timers (not `vi.useFakeTimers`) on purpose: Tooltip's warm/cooldown
    // state is a module-level singleton shared by every Tooltip instance in
    // the process, and faking timers here previously left it desynced from
    // real wall-clock time for every later test in this file.
    const labelSpan = tab("long-1").querySelector("span.truncate") as HTMLElement;
    await act(async () => {
      labelSpan.dispatchEvent(
        new MouseEvent("mouseover", { bubbles: true, relatedTarget: document.body }),
      );
      await new Promise((resolve) => setTimeout(resolve, 750));
    });
    expect(document.body.textContent).toContain(longTitle);
  });

  it("marks the active thread's tab as selected", async () => {
    await render([defaultThread, branchThread], "branch-1");
    expect(tab("branch-1").getAttribute("aria-selected")).toBe("true");
    expect(tab("default-1").getAttribute("aria-selected")).toBe("false");
  });

  it("fires onSelectThread with the clicked thread id", async () => {
    const onSelectThread = vi.fn();
    await render([defaultThread, branchThread], "default-1", onSelectThread);
    await act(async () => {
      tab("branch-1").dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(onSelectThread).toHaveBeenCalledWith("branch-1");
  });

  it("fires onCreateThread when the + button is clicked", async () => {
    const onCreateThread = vi.fn();
    await render([defaultThread], "default-1", () => {}, onCreateThread);
    const newBtn = container.querySelector("[data-testid='thread-tab-new']") as HTMLButtonElement;
    await act(async () => {
      newBtn.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(onCreateThread).toHaveBeenCalled();
  });

  it("places the + button right after Main thread, ahead of other pills", async () => {
    await render([defaultThread, branchThread, freshThread], "default-1");
    const tablist = container.querySelector("[role='tablist']") as HTMLElement;
    const testids: string[] = [];
    tablist.querySelectorAll("[data-testid]").forEach((el) => {
      const id = el.getAttribute("data-testid") ?? "";
      if (!id.startsWith("thread-archive-")) testids.push(id);
    });
    expect(testids).toEqual([
      "thread-tab-default-1",
      "thread-tab-new",
      "thread-tab-branch-1",
      "thread-tab-fresh-1",
      "thread-tab-more",
    ]);
  });

  it("orders non-default pills newest-first, so a just-created thread lands next to the + button rather than at the tail", async () => {
    const older = makeThread({ id: "older-1", kind: "fresh", created_at: "2026-01-01T00:00:00Z" });
    const newer = makeThread({ id: "newer-1", kind: "fresh", created_at: "2026-01-02T00:00:00Z" });
    // Fetched/stored in chronological (oldest-first) order, matching the store.
    await render([defaultThread, older, newer], "default-1");
    const tablist = container.querySelector("[role='tablist']") as HTMLElement;
    const testids: string[] = [];
    tablist.querySelectorAll("[data-testid]").forEach((el) => {
      const id = el.getAttribute("data-testid") ?? "";
      if (!id.startsWith("thread-archive-")) testids.push(id);
    });
    expect(testids).toEqual([
      "thread-tab-default-1",
      "thread-tab-new",
      "thread-tab-newer-1",
      "thread-tab-older-1",
      "thread-tab-more",
    ]);
  });

  it("does not render an archive button on the default thread's pill", async () => {
    await render([defaultThread, branchThread], "default-1");
    expect(archiveBtn("default-1")).toBeNull();
  });

  it("renders an archive button on non-default pills, active or not", async () => {
    await render([defaultThread, branchThread, freshThread], "branch-1");
    expect(archiveBtn("branch-1")).toBeTruthy();
    expect(archiveBtn("fresh-1")).toBeTruthy();
  });

  it("fires onArchiveThread with the pill's thread id, without also selecting it", async () => {
    const onSelectThread = vi.fn();
    const onArchiveThread = vi.fn();
    await render([defaultThread, branchThread], "default-1", onSelectThread, () => {}, onArchiveThread);
    await act(async () => {
      archiveBtn("branch-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(onArchiveThread).toHaveBeenCalledWith("branch-1");
    expect(onSelectThread).not.toHaveBeenCalled();
  });

  it("excludes an archived thread from the pill row entirely", async () => {
    const archived = makeThread({ id: "archived-1", kind: "fresh", archived_at: "2026-01-03T00:00:00Z" });
    await render([defaultThread, branchThread, archived], "default-1");
    expect(tab("archived-1")).toBeNull();
    expect(tab("branch-1")).toBeTruthy();
  });

  it("nests the + button inside Main's own pill (active), not as a separate strip-level button", async () => {
    await render([defaultThread, branchThread], "default-1");
    const mainPill = tab("default-1").parentElement;
    const newBtn = container.querySelector("[data-testid='thread-tab-new']");
    expect(mainPill?.contains(newBtn)).toBe(true);
  });

  it("keeps the + button inside Main's pill when Main is inactive", async () => {
    await render([defaultThread, branchThread], "branch-1");
    const mainPill = tab("default-1").parentElement;
    const newBtn = container.querySelector("[data-testid='thread-tab-new']");
    expect(mainPill?.contains(newBtn)).toBe(true);
  });

  it("gives the + button a contrasting accent fill so it reads at a glance", async () => {
    await render([defaultThread, branchThread], "default-1");
    const newBtn = container.querySelector("[data-testid='thread-tab-new']") as HTMLElement;
    const swatch = newBtn.querySelector("span");
    expect(swatch?.className).toContain("bg-[var(--accent)]");
  });

  it("never renders the + button on non-default pills", async () => {
    await render([defaultThread, branchThread, freshThread], "branch-1");
    const branchPill = tab("branch-1").parentElement;
    const freshPill = tab("fresh-1").parentElement;
    expect(branchPill?.querySelector("[data-testid='thread-tab-new']")).toBeNull();
    expect(freshPill?.querySelector("[data-testid='thread-tab-new']")).toBeNull();
  });

  it("fires onCreateThread from the pill without also selecting Main", async () => {
    const onSelectThread = vi.fn();
    const onCreateThread = vi.fn();
    await render([defaultThread, branchThread], "branch-1", onSelectThread, onCreateThread);
    const newBtn = container.querySelector("[data-testid='thread-tab-new']") as HTMLButtonElement;
    await act(async () => {
      newBtn.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(onCreateThread).toHaveBeenCalled();
    expect(onSelectThread).not.toHaveBeenCalled();
  });

  describe("rename via right-click", () => {
    function rightClick(el: Element) {
      el.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
    }

    function renameInput() {
      return document.querySelector("[data-testid='rename-thread-input']") as HTMLInputElement | null;
    }

    function renameSubmit() {
      return document.querySelector("[data-testid='rename-thread-submit']") as HTMLButtonElement | null;
    }

    // Bypass React's controlled-input value tracker via the prototype setter
    // so the synthetic onChange actually fires (mirrors ThreadsPanel.test.tsx).
    function setInputValue(input: HTMLInputElement, value: string) {
      const nativeSetter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
      nativeSetter.call(input, value);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    }

    it("opens the rename modal, pre-filled with the pill's current title, on right-click", async () => {
      await render([defaultThread, branchThread], "default-1");
      await act(async () => { rightClick(tab("branch-1")); });
      expect(renameInput()?.value).toBe("Investigate bug");
    });

    it("never opens the rename modal for the default (Main) thread", async () => {
      await render([defaultThread, branchThread], "default-1");
      await act(async () => { rightClick(tab("default-1")); });
      expect(renameInput()).toBeNull();
    });

    it("submits the trimmed title and closes the modal", async () => {
      const onRenameThread = vi.fn().mockResolvedValue(undefined);
      await render([defaultThread, branchThread], "default-1", () => {}, () => {}, () => {}, AGENT_ID, onRenameThread);
      await act(async () => { rightClick(tab("branch-1")); });
      const input = renameInput()!;
      await act(async () => {
        setInputValue(input, "  Renamed  ");
      });
      await act(async () => {
        renameSubmit()!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(onRenameThread).toHaveBeenCalledWith("branch-1", "Renamed");
      expect(renameInput()).toBeNull();
    });

    it("submits null when the field is cleared, reverting to the kind-derived placeholder", async () => {
      const onRenameThread = vi.fn().mockResolvedValue(undefined);
      await render([defaultThread, branchThread], "default-1", () => {}, () => {}, () => {}, AGENT_ID, onRenameThread);
      await act(async () => { rightClick(tab("branch-1")); });
      const input = renameInput()!;
      await act(async () => {
        setInputValue(input, "   ");
      });
      await act(async () => {
        renameSubmit()!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(onRenameThread).toHaveBeenCalledWith("branch-1", null);
    });

    it("does not select the thread as a side effect of right-clicking to rename it", async () => {
      const onSelectThread = vi.fn();
      await render([defaultThread, branchThread], "default-1", onSelectThread);
      await act(async () => { rightClick(tab("branch-1")); });
      expect(onSelectThread).not.toHaveBeenCalled();
    });
  });

  describe("background activity — streaming badge and unread dot", () => {
    function streamingBadge(id: string) {
      return container.querySelector(`[data-testid='thread-streaming-badge-${id}']`);
    }
    function unreadDot(id: string) {
      return container.querySelector(`[data-testid='thread-unread-dot-${id}']`);
    }

    it("shows the streaming badge on an inactive pill whose thread is actively producing output", async () => {
      useChatStore.setState({
        inFlightByAgent: new Map([[inFlightKey(AGENT_ID, "branch-1"), {
          textBuffer: "partial reply",
          activeToolCalls: [],
          isTyping: true,
          startedAt: 0,
          thinkingActive: false,
          thinkingBuffer: "",
          thinkingStartedAt: null,
          thinkingElapsedMs: null,
          thinkingShown: false,
          artifactIds: [],
        }]]),
      });
      // Active thread is default-1, so branch-1's badge is the inactive-pill one.
      await render([defaultThread, branchThread], "default-1");
      expect(streamingBadge("branch-1")).toBeTruthy();
      expect(unreadDot("branch-1")).toBeNull();
    });

    it("never shows the streaming badge on the active pill itself", async () => {
      useChatStore.setState({
        inFlightByAgent: new Map([[inFlightKey(AGENT_ID, "branch-1"), {
          textBuffer: "partial reply",
          activeToolCalls: [],
          isTyping: true,
          startedAt: 0,
          thinkingActive: false,
          thinkingBuffer: "",
          thinkingStartedAt: null,
          thinkingElapsedMs: null,
          thinkingShown: false,
          artifactIds: [],
        }]]),
      });
      // branch-1 is now the active tab — its own content is already visible below.
      await render([defaultThread, branchThread], "branch-1");
      expect(streamingBadge("branch-1")).toBeNull();
    });

    it("shows an unread dot on an inactive pill that finished streaming while unopened", async () => {
      useChatStore.setState({
        unreadThreadIds: new Set([inFlightKey(AGENT_ID, "branch-1")]),
      });
      await render([defaultThread, branchThread], "default-1");
      expect(unreadDot("branch-1")).toBeTruthy();
      expect(streamingBadge("branch-1")).toBeNull();
    });

    it("prefers the live streaming badge over a stale unread dot for the same pill", async () => {
      useChatStore.setState({
        inFlightByAgent: new Map([[inFlightKey(AGENT_ID, "branch-1"), {
          textBuffer: "new run started",
          activeToolCalls: [],
          isTyping: true,
          startedAt: 0,
          thinkingActive: false,
          thinkingBuffer: "",
          thinkingStartedAt: null,
          thinkingElapsedMs: null,
          thinkingShown: false,
          artifactIds: [],
        }]]),
        unreadThreadIds: new Set([inFlightKey(AGENT_ID, "branch-1")]),
      });
      await render([defaultThread, branchThread], "default-1");
      expect(streamingBadge("branch-1")).toBeTruthy();
      expect(unreadDot("branch-1")).toBeNull();
    });

    it("resolves Main's activity key to the plain agent id (no thread suffix)", async () => {
      useChatStore.setState({
        unreadThreadIds: new Set([inFlightKey(AGENT_ID)]),
      });
      // Active thread is branch-1, so Main's own pill is the inactive one here.
      await render([defaultThread, branchThread], "branch-1");
      expect(unreadDot("default-1")).toBeTruthy();
    });

    it("renders neither badge when a thread has no in-flight activity or unread marker", async () => {
      await render([defaultThread, branchThread], "default-1");
      expect(streamingBadge("branch-1")).toBeNull();
      expect(unreadDot("branch-1")).toBeNull();
    });
  });

  describe("background activity — question badge", () => {
    // `useChatStore.getState().reset()` (outer `beforeEach`) doesn't clear
    // `agents` — it's populated by `fetchAgents`, not any of the streaming/
    // form reducers `reset()` was written to cover — so a `pending_forms`
    // fixture set by one test here would otherwise leak into the next.
    beforeEach(() => {
      useChatStore.setState({ agents: [] });
    });

    function questionBadge(id: string) {
      return container.querySelector(`[data-testid='thread-question-badge-${id}']`);
    }
    function streamingBadge(id: string) {
      return container.querySelector(`[data-testid='thread-streaming-badge-${id}']`);
    }
    function unreadDot(id: string) {
      return container.querySelector(`[data-testid='thread-unread-dot-${id}']`);
    }

    it("shows the question badge when a sync AskUserQuestionWithForm form is pending for that thread", async () => {
      useChatStore.setState({
        pendingFormByAgent: { [inFlightKey(AGENT_ID, "branch-1")]: makeSyncForm({ thread_id: "branch-1" }) },
      });
      await render([defaultThread, branchThread], "default-1");
      expect(questionBadge("branch-1")).toBeTruthy();
    });

    it("shows the question badge when an async pending_forms entry exists for that thread", async () => {
      useChatStore.setState({
        agents: [makeAgentSnapshot({ pending_forms: [makeAsyncPendingForm({ thread_id: "branch-1" })] })],
      });
      await render([defaultThread, branchThread], "default-1");
      expect(questionBadge("branch-1")).toBeTruthy();
    });

    it("never shows the question badge on the active pill itself", async () => {
      useChatStore.setState({
        pendingFormByAgent: { [inFlightKey(AGENT_ID, "branch-1")]: makeSyncForm({ thread_id: "branch-1" }) },
      });
      await render([defaultThread, branchThread], "branch-1");
      expect(questionBadge("branch-1")).toBeNull();
    });

    it("prefers the question badge over the streaming badge for the same pill", async () => {
      useChatStore.setState({
        inFlightByAgent: new Map([[inFlightKey(AGENT_ID, "branch-1"), {
          textBuffer: "partial reply",
          activeToolCalls: [],
          isTyping: true,
          startedAt: 0,
          thinkingActive: false,
          thinkingBuffer: "",
          thinkingStartedAt: null,
          thinkingElapsedMs: null,
          thinkingShown: false,
          artifactIds: [],
        }]]),
        pendingFormByAgent: { [inFlightKey(AGENT_ID, "branch-1")]: makeSyncForm({ thread_id: "branch-1" }) },
      });
      await render([defaultThread, branchThread], "default-1");
      expect(questionBadge("branch-1")).toBeTruthy();
      expect(streamingBadge("branch-1")).toBeNull();
    });

    it("prefers the question badge over a stale unread dot for the same pill", async () => {
      useChatStore.setState({
        unreadThreadIds: new Set([inFlightKey(AGENT_ID, "branch-1")]),
        agents: [makeAgentSnapshot({ pending_forms: [makeAsyncPendingForm({ thread_id: "branch-1" })] })],
      });
      await render([defaultThread, branchThread], "default-1");
      expect(questionBadge("branch-1")).toBeTruthy();
      expect(unreadDot("branch-1")).toBeNull();
    });

    it("does not badge an unrelated thread of the same agent", async () => {
      useChatStore.setState({
        pendingFormByAgent: { [inFlightKey(AGENT_ID, "some-other-thread")]: makeSyncForm() },
      });
      await render([defaultThread, branchThread], "default-1");
      expect(questionBadge("branch-1")).toBeNull();
    });

    // Regression coverage for the default/main thread's OWN pill — every
    // case above pends the form on `branch-1` (non-default). A default-
    // thread sync form carries no `thread_id` on the wire (see
    // `pendingFormByAgent`'s docstring in chatStore.ts), so it buckets under
    // the bare agent id, not `inFlightKey(AGENT_ID, "default-1")` — asserted
    // explicitly here since using the thread's literal row id would be the
    // wrong key and silently fail to match.
    it("shows the question badge on the default thread's own pill when a sync form is pending there", async () => {
      useChatStore.setState({
        pendingFormByAgent: { [AGENT_ID]: makeSyncForm() },
      });
      // Viewing the branch thread — the default thread's pill is the
      // (non-active) one that must carry the badge.
      await render([defaultThread, branchThread], "branch-1");
      expect(questionBadge("default-1")).toBeTruthy();
    });

    it("shows the question badge on the default thread's own pill for an async pending_forms entry (thread_id: null)", async () => {
      useChatStore.setState({
        agents: [makeAgentSnapshot({ pending_forms: [makeAsyncPendingForm({ thread_id: null })] })],
      });
      await render([defaultThread, branchThread], "branch-1");
      expect(questionBadge("default-1")).toBeTruthy();
    });

    it("a default-thread form does not bleed onto a non-default thread's pill, and vice versa", async () => {
      useChatStore.setState({
        pendingFormByAgent: {
          [AGENT_ID]: makeSyncForm({ form_id: "form-default" }),
          [inFlightKey(AGENT_ID, "branch-1")]: makeSyncForm({ form_id: "form-branch", thread_id: "branch-1" }),
        },
      });
      // Neither pill is active, so both badges are free to render — proves
      // isolation rather than one masking the other via active-pill suppression.
      await render([defaultThread, branchThread, freshThread], "fresh-1");
      expect(questionBadge("default-1")).toBeTruthy();
      expect(questionBadge("branch-1")).toBeTruthy();
      expect(questionBadge("fresh-1")).toBeNull();
    });

    it("renders the louder sync treatment (data-sync) when the pending form is a sync AskUserQuestionWithForm call", async () => {
      useChatStore.setState({
        pendingFormByAgent: { [inFlightKey(AGENT_ID, "branch-1")]: makeSyncForm({ thread_id: "branch-1" }) },
      });
      await render([defaultThread, branchThread], "default-1");
      expect(questionBadge("branch-1")?.getAttribute("data-sync")).toBe("true");
    });

    it("renders the passive (non-sync) treatment for an async pending_forms entry", async () => {
      useChatStore.setState({
        agents: [makeAgentSnapshot({ pending_forms: [makeAsyncPendingForm({ thread_id: "branch-1" })] })],
      });
      await render([defaultThread, branchThread], "default-1");
      expect(questionBadge("branch-1")?.getAttribute("data-sync")).toBeNull();
    });

    // Regression: the badge used to stay stuck forever once a form's
    // question was superseded any way other than answering it through the
    // form overlay or the run ending. The common miss was the operator just
    // typing a new message instead — `sendMessage` now clears that thread's
    // pending sync-form slot up front (see chatStore.ts), so the badge
    // disappears live off the same store the strip already subscribes to.
    describe("clears live when the user sends a new message", () => {
      beforeEach(() => {
        vi.clearAllMocks();
        mockGetAgents.mockResolvedValue([]);
        mockSendMessage.mockResolvedValue({ message_id: "msg-1", status: "queued" });
        useChatStore.setState({ selectedAgentId: AGENT_ID });
      });

      it("removes the question badge from a thread once the user sends a message to it", async () => {
        useChatStore.setState((s) => {
          const next = new Map(s.selectedThreadIdByAgent);
          next.set(AGENT_ID, "branch-1");
          return {
            selectedThreadIdByAgent: next,
            pendingFormByAgent: { [inFlightKey(AGENT_ID, "branch-1")]: makeSyncForm({ thread_id: "branch-1" }) },
          };
        });
        await render([defaultThread, branchThread], "default-1");
        expect(questionBadge("branch-1")).toBeTruthy();

        await act(async () => {
          await useChatStore.getState().sendMessage("never mind, I'll just say it here");
        });

        expect(questionBadge("branch-1")).toBeNull();
      });

      it("GUARD: does not clear a still-pending question on a different thread of the same agent", async () => {
        useChatStore.setState((s) => {
          const next = new Map(s.selectedThreadIdByAgent);
          next.set(AGENT_ID, "branch-1");
          return {
            selectedThreadIdByAgent: next,
            pendingFormByAgent: {
              [inFlightKey(AGENT_ID, "branch-1")]: makeSyncForm({ form_id: "form-branch", thread_id: "branch-1" }),
              [inFlightKey(AGENT_ID, "fresh-1")]: makeSyncForm({ form_id: "form-fresh", thread_id: "fresh-1" }),
            },
          };
        });
        await render([defaultThread, branchThread, freshThread], "default-1");
        expect(questionBadge("branch-1")).toBeTruthy();
        expect(questionBadge("fresh-1")).toBeTruthy();

        // Sends into "branch-1" (the currently-selected thread) only.
        await act(async () => {
          await useChatStore.getState().sendMessage("answering branch-1's question inline");
        });

        expect(questionBadge("branch-1")).toBeNull();
        // fresh-1's question is unrelated and still genuinely pending.
        expect(questionBadge("fresh-1")).toBeTruthy();
      });
    });
  });

  describe("More pill — overflow panel", () => {
    function moreBtn() {
      return container.querySelector("[data-testid='thread-tab-more']") as HTMLButtonElement;
    }
    // Portaled straight to `document.body` (see ThreadOverflowPanel — it
    // escapes the chat column's `overflow-hidden` ancestor the same way
    // RenameThreadModal does), so these query `document`, not `container`,
    // mirroring `renameInput`/`renameSubmit` above.
    function panel() {
      return document.querySelector("[data-testid='thread-overflow-panel']");
    }
    function overflowRow(id: string) {
      return document.querySelector(`[data-testid='thread-overflow-row-${id}']`) as HTMLButtonElement | null;
    }
    function overflowCheckbox(id: string) {
      return document.querySelector(`[data-testid='thread-overflow-checkbox-${id}']`) as HTMLButtonElement | null;
    }
    function archivedTabBtn() {
      return document.querySelector("[data-testid='thread-overflow-tab-archived']") as HTMLButtonElement;
    }
    function activeTabBtn() {
      return document.querySelector("[data-testid='thread-overflow-tab-active']") as HTMLButtonElement;
    }
    function archivedRow(id: string) {
      return document.querySelector(`[data-testid='thread-overflow-archived-row-${id}']`) as HTMLElement | null;
    }
    function unarchiveBtn(id: string) {
      return document.querySelector(`[data-testid='thread-overflow-unarchive-${id}']`) as HTMLButtonElement | null;
    }
    async function click(el: Element) {
      await act(async () => {
        el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
    }

    it("is closed by default and opens on click", async () => {
      await render([defaultThread, branchThread], "default-1");
      expect(panel()).toBeNull();
      await click(moreBtn());
      expect(panel()).toBeTruthy();
    });

    it("lists every non-default thread, never Main", async () => {
      await render([defaultThread, branchThread, freshThread], "default-1");
      await click(moreBtn());
      expect(overflowRow("branch-1")).toBeTruthy();
      expect(overflowRow("fresh-1")).toBeTruthy();
      expect(overflowRow("default-1")).toBeNull();
    });

    it("excludes an archived thread from the Active tab — it lives on the Archived tab instead", async () => {
      const archived = makeThread({ id: "archived-1", kind: "fresh", archived_at: "2026-01-03T00:00:00Z" });
      await render([defaultThread, branchThread, freshThread, archived], "default-1");
      // Archived pill is gone from the row...
      expect(tab("archived-1")).toBeNull();
      expect(tab("fresh-1")).toBeTruthy();
      // ...and the overflow panel opens straight to the Active tab, which
      // doesn't show it either — it's reachable from the Archived tab
      // instead (see the "Archived tab" describe block below).
      await click(moreBtn());
      expect(overflowRow("archived-1")).toBeNull();
      expect(overflowRow("branch-1")).toBeTruthy();
    });

    it("toggles a row's checkbox into the selected set without closing the panel or switching threads", async () => {
      const onSelectThread = vi.fn();
      await render([defaultThread, branchThread], "default-1", onSelectThread);
      await click(moreBtn());
      await click(overflowCheckbox("branch-1")!);
      expect(panel()).toBeTruthy();
      expect(overflowCheckbox("branch-1")!.getAttribute("aria-pressed")).toBe("true");
      // Portaled to `document.body`, not a descendant of `container` — see
      // the `panel()`/`overflowRow`/`overflowCheckbox` comment above.
      expect(document.body.textContent).toContain("1 selected");
      expect(onSelectThread).not.toHaveBeenCalled();
    });

    it("deselects a checkbox on a second click, still without closing", async () => {
      await render([defaultThread, branchThread], "default-1");
      await click(moreBtn());
      await click(overflowCheckbox("branch-1")!);
      await click(overflowCheckbox("branch-1")!);
      expect(panel()).toBeTruthy();
      expect(overflowCheckbox("branch-1")!.getAttribute("aria-pressed")).toBe("false");
    });

    it("clicking a row switches to that thread and keeps the panel open", async () => {
      const onSelectThread = vi.fn();
      await render([defaultThread, branchThread], "default-1", onSelectThread);
      await click(moreBtn());
      await click(overflowRow("branch-1")!);
      expect(onSelectThread).toHaveBeenCalledWith("branch-1");
      expect(panel()).toBeTruthy();
    });

    it("deletes every checked thread via onDeleteThread and clears the selection", async () => {
      const onDeleteThread = vi.fn().mockResolvedValue(undefined);
      await render(
        [defaultThread, branchThread, freshThread],
        "default-1",
        () => {}, () => {}, () => {},
        AGENT_ID,
        () => Promise.resolve(),
        onDeleteThread,
      );
      await click(moreBtn());
      await click(overflowCheckbox("branch-1")!);
      await click(overflowCheckbox("fresh-1")!);
      const deleteBtn = document.querySelector("[data-testid='thread-overflow-delete-selected']") as HTMLButtonElement;
      await act(async () => {
        deleteBtn.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(onDeleteThread).toHaveBeenCalledWith("branch-1");
      expect(onDeleteThread).toHaveBeenCalledWith("fresh-1");
      expect(document.body.textContent).not.toContain("selected");
    });

    // The panel stays mounted with its own AnimatePresence toggling
    // internally (unlike RenameThreadModal, which the parent fully
    // unmounts on close) so the exit transition the user asked for — the
    // panel actually animating away rather than vanishing — has something
    // to play. That means the DOM node lingers briefly through a real exit
    // tween after `open` flips false, so these two assertions poll with
    // real timers instead of checking synchronously right after the click.
    it("closes on the explicit close button", async () => {
      await render([defaultThread, branchThread], "default-1");
      await click(moreBtn());
      await click(document.querySelector("[data-testid='thread-overflow-close']")!);
      await vi.waitFor(() => expect(panel()).toBeNull(), { timeout: 1000 });
    });

    it("closes on an outside click", async () => {
      await render([defaultThread, branchThread], "default-1");
      await click(moreBtn());
      expect(panel()).toBeTruthy();
      await act(async () => {
        document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
      });
      await vi.waitFor(() => expect(panel()).toBeNull(), { timeout: 1000 });
    });

    it("never fires onSelectThread as a side effect of merely opening the panel", async () => {
      const onSelectThread = vi.fn();
      await render([defaultThread, branchThread], "default-1", onSelectThread);
      await click(moreBtn());
      expect(onSelectThread).not.toHaveBeenCalled();
    });

    describe("Archived tab", () => {
      const archived1 = makeThread({ id: "archived-1", kind: "fresh", archived_at: "2026-01-03T00:00:00Z" });
      const archived2 = makeThread({ id: "archived-2", kind: "fresh", archived_at: "2026-01-04T00:00:00Z" });

      it("shows Active/Archived counts and opens to Active by default", async () => {
        await render([defaultThread, branchThread, archived1], "default-1");
        await click(moreBtn());
        expect(activeTabBtn().textContent).toContain("Active (1)");
        expect(archivedTabBtn().textContent).toContain("Archived (1)");
        expect(activeTabBtn().getAttribute("aria-selected")).toBe("true");
      });

      it("switches to the Archived tab and lists archived threads there, not on Active", async () => {
        await render([defaultThread, branchThread, archived1], "default-1");
        await click(moreBtn());
        await click(archivedTabBtn());
        expect(archivedTabBtn().getAttribute("aria-selected")).toBe("true");
        expect(archivedRow("archived-1")).toBeTruthy();
        expect(overflowRow("branch-1")).toBeNull();
      });

      it("restores an archived thread by clicking its row, without closing the panel", async () => {
        const onUnarchiveThread = vi.fn().mockResolvedValue(undefined);
        await render(
          [defaultThread, archived1],
          "default-1",
          () => {}, () => {}, () => {},
          AGENT_ID,
          () => Promise.resolve(),
          () => {},
          onUnarchiveThread,
        );
        await click(moreBtn());
        await click(archivedTabBtn());
        // The row itself is the unarchive control now — no separate icon
        // button to hunt for (see ThreadOverflowPanel's archived-tab row).
        await click(unarchiveBtn("archived-1")!);
        expect(onUnarchiveThread).toHaveBeenCalledWith("archived-1");
        expect(panel()).toBeTruthy();
      });

      it("deletes checked archived threads the same way as active ones", async () => {
        const onDeleteThread = vi.fn().mockResolvedValue(undefined);
        await render(
          [defaultThread, archived1, archived2],
          "default-1",
          () => {}, () => {}, () => {},
          AGENT_ID,
          () => Promise.resolve(),
          onDeleteThread,
        );
        await click(moreBtn());
        await click(archivedTabBtn());
        await click(overflowCheckbox("archived-1")!);
        const deleteBtn = document.querySelector("[data-testid='thread-overflow-delete-selected']") as HTMLButtonElement;
        await act(async () => {
          deleteBtn.dispatchEvent(new MouseEvent("click", { bubbles: true }));
          await Promise.resolve();
          await Promise.resolve();
        });
        expect(onDeleteThread).toHaveBeenCalledWith("archived-1");
      });

      it("hides the tab switcher while a selection is active, so a selection from one tab can never leak into the other", async () => {
        await render([defaultThread, branchThread, archived1], "default-1");
        await click(moreBtn());
        await click(overflowCheckbox("branch-1")!);
        expect(document.body.textContent).toContain("1 selected");
        // The tab buttons themselves are swapped out for the selection
        // controls (Clear/Delete) while anything is checked — the only way
        // back to the tab switcher is Clear or Delete, both of which empty
        // `selected` as a side effect.
        expect(archivedTabBtn()).toBeNull();
        expect(activeTabBtn()).toBeNull();
        await click(document.querySelector("[data-testid='thread-overflow-clear-selection']")!);
        expect(document.body.textContent).not.toContain("selected");
        await click(archivedTabBtn());
        expect(archivedTabBtn().getAttribute("aria-selected")).toBe("true");
        expect(overflowCheckbox("archived-1")!.getAttribute("aria-pressed")).toBe("false");
      });

      it("resets back to the Active tab every time the panel is reopened", async () => {
        await render([defaultThread, branchThread, archived1], "default-1");
        await click(moreBtn());
        await click(archivedTabBtn());
        expect(archivedTabBtn().getAttribute("aria-selected")).toBe("true");
        await click(document.querySelector("[data-testid='thread-overflow-close']")!);
        await vi.waitFor(() => expect(panel()).toBeNull(), { timeout: 1000 });
        await click(moreBtn());
        expect(activeTabBtn().getAttribute("aria-selected")).toBe("true");
      });
    });
  });

  // The strip floats over, not inside, the message scroll container — see
  // `forwardWheelToMessages` in ThreadTabStrip.tsx. A `[data-scroll-container]`
  // element has to exist in the DOM (as it does in the real ChatView tree) for
  // the pills to find and forward to it.
  describe("wheel forwarding to the message scroll container", () => {
    let scrollContainer: HTMLDivElement;
    let scrollBySpy: ReturnType<typeof vi.fn>;

    beforeEach(() => {
      scrollContainer = document.createElement("div");
      scrollContainer.setAttribute("data-scroll-container", "");
      scrollBySpy = vi.fn();
      scrollContainer.scrollBy = scrollBySpy as unknown as typeof scrollContainer.scrollBy;
      document.body.appendChild(scrollContainer);
    });

    afterEach(() => {
      document.body.removeChild(scrollContainer);
    });

    function wheel(el: Element, deltaY: number, deltaMode: number) {
      el.dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY, deltaMode }));
    }

    it("forwards a pixel-mode (deltaMode 0) wheel gesture over a pill as-is", async () => {
      await render([defaultThread, branchThread], "default-1");
      await act(async () => { wheel(tab("branch-1"), 120, 0); });
      expect(scrollBySpy).toHaveBeenCalledWith({ top: 120 });
    });

    it("scales a line-mode (deltaMode 1) wheel gesture up to an approximate pixel delta", async () => {
      await render([defaultThread, branchThread], "default-1");
      await act(async () => { wheel(tab("branch-1"), 3, 1); });
      expect(scrollBySpy).toHaveBeenCalledWith({ top: 48 });
    });

    it("scales a page-mode (deltaMode 2) wheel gesture by the scroll container's own height", async () => {
      Object.defineProperty(scrollContainer, "clientHeight", { value: 600, configurable: true });
      await render([defaultThread, branchThread], "default-1");
      await act(async () => { wheel(tab("branch-1"), 1, 2); });
      expect(scrollBySpy).toHaveBeenCalledWith({ top: 600 });
    });

    it("also forwards a wheel gesture over the More pill", async () => {
      await render([defaultThread, branchThread], "default-1");
      const moreButton = container.querySelector("[data-testid='thread-tab-more']") as HTMLButtonElement;
      await act(async () => { wheel(moreButton, 75, 0); });
      expect(scrollBySpy).toHaveBeenCalledWith({ top: 75 });
    });

    it("does not throw when no scroll container is mounted", async () => {
      document.body.removeChild(scrollContainer);
      await render([defaultThread, branchThread], "default-1");
      expect(() => wheel(tab("branch-1"), 120, 0)).not.toThrow();
      document.body.appendChild(scrollContainer); // restore for afterEach's removeChild
    });

    it("does not forward a wheel gesture from inside the More popover's own scrollable list", async () => {
      await render([defaultThread, branchThread], "default-1");
      const moreButton = container.querySelector("[data-testid='thread-tab-more']") as HTMLButtonElement;
      await act(async () => {
        moreButton.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      // Portaled to `document.body` (see ThreadOverflowPanel) — not a DOM
      // descendant of the "More" pill wrapper, but still nested under it in
      // JSX, which is what let the bug through (React bubbles synthetic
      // events along the JSX tree, not the DOM tree).
      const panelEl = document.querySelector("[data-testid='thread-overflow-panel']") as HTMLElement;
      const popoverList = panelEl.querySelector(".overflow-y-auto") as HTMLElement;
      await act(async () => { wheel(popoverList, 120, 0); });
      expect(scrollBySpy).not.toHaveBeenCalled();
    });
  });
});
