// @vitest-environment jsdom
//
// Coverage for channel thread splitting:
// a channel-originated thread (`channel_origin != null`) must never render
// as a loose pill in ThreadTabStrip anymore — it's folded into the collapsed
// "Channels" tile instead, sub-grouped by `channel_origin.kind` via the
// shared `resolveChannelThreadPartition` selector (lib/channelThreads.ts).
// These tests drive the real component: they assert channel threads are
// absent from the normal pill row, that the Channels tile appears with the
// correct aggregate unread badge, that it hides when there are none, and
// that clicking a channel row reuses the strip's own `onSelectThread`.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { ThreadTabStrip } from "../ThreadTabStrip";
import { useChatStore, inFlightKey } from "../../../stores/chatStore";
import type { ChannelBridgeOrigin, Thread } from "../../../types/api";

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

function slackOrigin(bindingId = "slack-binding-1"): ChannelBridgeOrigin {
  return { kind: "slack", binding_id: bindingId };
}

const defaultThread = makeThread({ id: "default-1", kind: "default" });

describe("ThreadTabStrip — collapsed Channels tile (D4)", () => {
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

  function channelsTile() {
    return container.querySelector("[data-testid='thread-tab-channels']") as HTMLButtonElement | null;
  }

  function unreadBadge() {
    return container.querySelector("[data-testid='channels-tile-unread-badge']");
  }

  it("never renders a loose pill for a channel-originated thread", async () => {
    const slackThread = makeThread({ id: "slack-1", title: "general", channel_origin: slackOrigin() });
    await render([defaultThread, slackThread]);
    expect(tab("default-1")).toBeTruthy();
    expect(tab("slack-1")).toBeNull();
  });

  it("excludes a channel thread from the tab row's testids entirely (tablist scan)", async () => {
    const slackThread = makeThread({ id: "slack-1", channel_origin: slackOrigin() });
    const working = makeThread({ id: "working-1" });
    await render([defaultThread, slackThread, working]);
    const tablist = container.querySelector("[role='tablist']") as HTMLElement;
    const testids: string[] = [];
    tablist.querySelectorAll("[data-testid]").forEach((el) => {
      const id = el.getAttribute("data-testid") ?? "";
      if (id.startsWith("thread-tab-") && !id.startsWith("thread-tab-new")) testids.push(id);
    });
    expect(testids).toContain("thread-tab-default-1");
    expect(testids).toContain("thread-tab-working-1");
    expect(testids).toContain("thread-tab-channels");
    expect(testids).not.toContain("thread-tab-slack-1");
  });

  it("hides the Channels tile entirely when the agent has no channel threads", async () => {
    await render([defaultThread, makeThread({ id: "working-1" })]);
    expect(channelsTile()).toBeNull();
  });

  it("shows the Channels tile once at least one channel thread exists", async () => {
    const slackThread = makeThread({ id: "slack-1", channel_origin: slackOrigin() });
    await render([defaultThread, slackThread]);
    expect(channelsTile()).toBeTruthy();
  });

  it("renders no aggregate unread badge when no channel thread is unread", async () => {
    const slackThread = makeThread({ id: "slack-1", channel_origin: slackOrigin() });
    await render([defaultThread, slackThread]);
    expect(unreadBadge()).toBeNull();
  });

  it("renders the correct aggregate unread badge across every channel kind", async () => {
    const unreadSlack = makeThread({ id: "slack-unread", channel_origin: slackOrigin() });
    const readSlack = makeThread({ id: "slack-read", channel_origin: slackOrigin() });
    const unreadDiscord = makeThread({
      id: "discord-unread",
      channel_origin: { kind: "discord", binding_id: "discord-binding-1" },
    });

    useChatStore.setState({
      unreadThreadIds: new Set([
        inFlightKey(AGENT_ID, "slack-unread"),
        inFlightKey(AGENT_ID, "discord-unread"),
      ]),
    });

    await render([defaultThread, unreadSlack, readSlack, unreadDiscord]);
    expect(unreadBadge()?.textContent).toBe("2");
  });

  it("expands the tile to reveal channel conversations sub-grouped by kind, and clicking a row reuses onSelectThread", async () => {
    const onSelectThread = vi.fn();
    const slackThread = makeThread({ id: "slack-1", title: "#general", channel_origin: slackOrigin() });
    const discordThread = makeThread({
      id: "discord-1",
      title: "#random",
      channel_origin: { kind: "discord", binding_id: "discord-binding-1" },
    });
    await render([defaultThread, slackThread, discordThread], "default-1", onSelectThread);

    expect(document.querySelector("[data-testid='channels-tile-panel']")).toBeNull();
    await act(async () => {
      channelsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(document.querySelector("[data-testid='channels-tile-panel']")).toBeTruthy();
    expect(document.querySelector("[data-testid='channels-tile-group-slack']")).toBeTruthy();
    expect(document.querySelector("[data-testid='channels-tile-group-discord']")).toBeTruthy();

    const slackRow = document.querySelector("[data-testid='channels-tile-row-slack-1']") as HTMLButtonElement;
    expect(slackRow.textContent).toContain("#general");

    await act(async () => {
      slackRow.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(onSelectThread).toHaveBeenCalledWith("slack-1");
  });

  // Coverage for the rename affordance added to the Channels tile's rows —
  // right-click, same as an ordinary pill (see ThreadTabStrip.test.tsx's own
  // "rename via right-click" block), reusing the strip's single shared
  // `renameTarget`/modal instance via `ChannelsTilePanel`'s `onOpenRename`.
  describe("rename via right-click on a channel tile row", () => {
    function renameInput() {
      return document.querySelector("[data-testid='rename-thread-input']") as HTMLInputElement | null;
    }

    function renameSubmit() {
      return document.querySelector("[data-testid='rename-thread-submit']") as HTMLButtonElement | null;
    }

    // Bypasses React's controlled-input value tracker so the synthetic
    // onChange actually fires (mirrors ThreadTabStrip.test.tsx's own helper).
    function setInputValue(input: HTMLInputElement, value: string) {
      const nativeSetter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
      nativeSetter.call(input, value);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    }

    async function openChannelsTileAndRightClickRow(threads: Thread[], threadId: string, onRenameThread?: (id: string, title: string | null) => Promise<unknown>) {
      await render(threads, "default-1", () => {}, onRenameThread);
      await act(async () => {
        channelsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      const row = document.querySelector(`[data-testid='channels-tile-row-${threadId}']`) as HTMLButtonElement;
      await act(async () => {
        row.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
      });
    }

    it("opens the rename modal, pre-filled with the row's current title", async () => {
      const slackThread = makeThread({ id: "slack-1", title: "#general", channel_origin: slackOrigin() });
      await openChannelsTileAndRightClickRow([defaultThread, slackThread], "slack-1");
      expect(renameInput()?.value).toBe("#general");
    });

    it("previews the channel's own display name as the placeholder when the thread has no title yet", async () => {
      const slackThread = makeThread({ id: "slack-1", title: null, channel_origin: slackOrigin() });
      await openChannelsTileAndRightClickRow([defaultThread, slackThread], "slack-1");
      expect(renameInput()?.placeholder).toBe("Slack");
    });

    it("does not select the thread as a side effect of right-clicking to rename it", async () => {
      const onSelectThread = vi.fn();
      const slackThread = makeThread({ id: "slack-1", channel_origin: slackOrigin() });
      await render([defaultThread, slackThread], "default-1", onSelectThread);
      await act(async () => {
        channelsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      const row = document.querySelector("[data-testid='channels-tile-row-slack-1']") as HTMLButtonElement;
      await act(async () => {
        row.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, cancelable: true }));
      });
      expect(onSelectThread).not.toHaveBeenCalled();
    });

    it("submits the trimmed title via the strip's onRenameThread and closes the modal", async () => {
      const onRenameThread = vi.fn().mockResolvedValue(undefined);
      const slackThread = makeThread({ id: "slack-1", title: "#general", channel_origin: slackOrigin() });
      await openChannelsTileAndRightClickRow([defaultThread, slackThread], "slack-1", onRenameThread);
      const input = renameInput()!;
      await act(async () => {
        setInputValue(input, "  Renamed channel  ");
      });
      await act(async () => {
        renameSubmit()!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(onRenameThread).toHaveBeenCalledWith("slack-1", "Renamed channel");
      expect(renameInput()).toBeNull();
    });

    it("submits null when the field is cleared, reverting to the channel's fallback label", async () => {
      const onRenameThread = vi.fn().mockResolvedValue(undefined);
      const slackThread = makeThread({ id: "slack-1", title: "#general", channel_origin: slackOrigin() });
      await openChannelsTileAndRightClickRow([defaultThread, slackThread], "slack-1", onRenameThread);
      const input = renameInput()!;
      await act(async () => {
        setInputValue(input, "   ");
      });
      await act(async () => {
        renameSubmit()!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
        await Promise.resolve();
        await Promise.resolve();
      });
      expect(onRenameThread).toHaveBeenCalledWith("slack-1", null);
    });
  });

  // Coverage for the archive ("close") button added to the Channels tile's
  // rows — reuses the strip's own `onArchiveThread` prop directly (the same
  // one an ordinary pill's `X` calls), passed straight through to
  // `ChannelsTilePanel`.
  describe("archive via the row's close button", () => {
    async function openChannelsTile(threads: Thread[], onArchiveThread?: (id: string) => void) {
      await render(threads, "default-1", () => {}, undefined, onArchiveThread);
      await act(async () => {
        channelsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
    }

    function archiveButton(threadId: string) {
      return document.querySelector(`[data-testid='channels-tile-archive-${threadId}']`) as HTMLButtonElement | null;
    }

    it("calls the strip's onArchiveThread with the row's thread id", async () => {
      const onArchiveThread = vi.fn();
      const slackThread = makeThread({ id: "slack-1", channel_origin: slackOrigin() });
      await openChannelsTile([defaultThread, slackThread], onArchiveThread);

      await act(async () => {
        archiveButton("slack-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      expect(onArchiveThread).toHaveBeenCalledWith("slack-1");
    });

    it("does not select the thread as a side effect of clicking its archive button", async () => {
      const onSelectThread = vi.fn();
      const slackThread = makeThread({ id: "slack-1", channel_origin: slackOrigin() });
      await render([defaultThread, slackThread], "default-1", onSelectThread);
      await act(async () => {
        channelsTile()!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });

      await act(async () => {
        archiveButton("slack-1")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      expect(onSelectThread).not.toHaveBeenCalled();
    });

    it("removes just the archived row from the tile, leaving a sibling channel thread and the tile itself in place", async () => {
      // A second, still-live channel thread keeps `channelGroups` non-empty
      // once the first is archived, so this specifically exercises the row
      // disappearing rather than the whole tile unmounting because its last
      // channel thread is gone (see `resolveChannelThreadPartition`'s own
      // "drops an archived channel thread entirely" coverage for that case).
      const slackThread = makeThread({ id: "slack-1", channel_origin: slackOrigin() });
      const otherSlackThread = makeThread({ id: "slack-2", channel_origin: slackOrigin() });
      await openChannelsTile([defaultThread, slackThread, otherSlackThread]);
      expect(document.querySelector("[data-testid='channels-tile-row-slack-1']")).toBeTruthy();
      expect(document.querySelector("[data-testid='channels-tile-row-slack-2']")).toBeTruthy();

      const archived = { ...slackThread, archived_at: "2026-02-01T00:00:00Z" };
      await act(async () => {
        root.render(
          React.createElement(ThreadTabStrip, {
            agentId: AGENT_ID,
            threads: [defaultThread, archived, otherSlackThread],
            activeThreadId: "default-1",
            onSelectThread: () => {},
            onCreateThread: () => {},
            onArchiveThread: () => {},
            onDeleteThread: () => {},
            onRenameThread: () => Promise.resolve(),
            onUnarchiveThread: () => {},
          }),
        );
      });
      expect(document.querySelector("[data-testid='channels-tile-row-slack-1']")).toBeNull();
      expect(document.querySelector("[data-testid='channels-tile-row-slack-2']")).toBeTruthy();
      expect(channelsTile()).toBeTruthy();
    });
  });
});
