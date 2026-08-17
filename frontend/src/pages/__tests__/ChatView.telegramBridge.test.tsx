// @vitest-environment jsdom
/**
 * A channel binding's dedicated bridge thread
 * (`AgentProfile.channels[].bridge_thread_id`) only relays a reply back to
 * the external channel when the run was triggered by an inbound message —
 * see `InFlightChats` in `crates/ao-engine/src/telegram/outbound.rs`. A
 * message typed into that thread from the app itself never records a
 * sender, so it would never relay: a dead end unless the operator opts into
 * it. ChatView defaults the composer to a read-only hint on that one thread,
 * naming whichever channel actually matched (it used to hardcode "Telegram"
 * regardless of the bound channel's `kind` — see `getBridgeChannelKind`'s
 * docstring in `lib/threadNavigation.ts`), with a small button that reveals
 * the real composer anyway so the operator can steer the agent directly.
 *
 * These tests render ChatView (mirroring the render harness used by
 * `ProjectDetailView.transition.test.tsx`) and verify: the composer/hint
 * swap is gated on the bridge thread alone (every other thread of the same
 * agent, including its own default thread, keeps the real composer); the
 * hint names the actual bound channel, not just Telegram; and the reveal
 * button swaps the hint for the real composer plus a small "back" control.
 * The profile fixture uses `channels` (what a fetched `AgentProfile`
 * actually carries) rather than the legacy `telegram` field, which the
 * server never re-emits on output — see `isChannelBridgeThread` /
 * `getBridgeChannelKind` in `lib/threadNavigation.ts` for narrower unit
 * coverage of the predicates themselves, including Discord/Email bindings.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { MemoryRouter, Routes, Route } from "react-router-dom";
import { ChatView } from "../ChatView";
import { useChatStore } from "../../stores/chatStore";
import { useNetworkStore } from "../../stores/networkStore";
import type { AgentProfile, Thread } from "../../types/api";

const AGENT_ID = "agent-telegram-1";
const DEFAULT_THREAD_ID = "thread-default-1";
const BRIDGE_THREAD_ID = "thread-bridge-1";
const SLACK_CONVO_THREAD_ID = "thread-slack-convo-1";

function makeProfile(overrides: Partial<AgentProfile> = {}): AgentProfile {
  return {
    id: AGENT_ID,
    name: "Bridge Agent",
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
    ...overrides,
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
    {
      id: BRIDGE_THREAD_ID,
      title: "Telegram",
      scope: { type: "AgentChat", agent_id: AGENT_ID },
      transcript_path: "",
      kind: "fresh",
      created_at: "2024-01-01T00:00:00Z",
      updated_at: "2024-01-01T00:00:00Z",
    },
    {
      id: SLACK_CONVO_THREAD_ID,
      title: "💬 Slack — C123",
      scope: { type: "AgentChat", agent_id: AGENT_ID },
      transcript_path: "",
      kind: "fresh",
      // Slack provisions one thread per *conversation*, never a single
      // `ChannelBinding.bridge_thread_id` — this is the only signal that
      // recognizes it as a bridge thread. See `getBridgeChannelKind`.
      channel_origin: { kind: "slack", binding_id: "slack-1" },
      created_at: "2024-01-01T00:00:00Z",
      updated_at: "2024-01-01T00:00:00Z",
    },
  ];
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

describe("ChatView: Telegram bridge thread composer gating", () => {
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

  async function renderChatView(profile: AgentProfile, activeThreadId: string) {
    const { getAgent, listThreads } = await import("../../lib/api");
    vi.mocked(getAgent).mockResolvedValue(profile);
    vi.mocked(listThreads).mockResolvedValue(makeThreads());

    // Pre-seed the thread selection and profile so the composer/hint swap is
    // correct on the very first paint, not just after the async selectAgent/
    // loadThreads fetches resolve.
    useChatStore.setState((state) => {
      const threadsByAgent = new Map(state.threadsByAgent);
      threadsByAgent.set(AGENT_ID, makeThreads());
      const selectedThreadIdByAgent = new Map(state.selectedThreadIdByAgent);
      selectedThreadIdByAgent.set(AGENT_ID, activeThreadId);
      return {
        selectedAgentId: AGENT_ID,
        selectedAgentProfile: profile,
        threadsByAgent,
        selectedThreadIdByAgent,
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

  it("shows the read-only hint and hides the composer on the Telegram bridge thread", async () => {
    const profile = makeProfile({
      channels: [
        {
          binding_id: "telegram",
          kind: "telegram",
          enabled: true,
          bridge_thread_id: BRIDGE_THREAD_ID,
          allowed_senders: ["555"],
        },
      ],
    });

    await renderChatView(profile, BRIDGE_THREAD_ID);

    expect(container.querySelector("[data-testid='channel-bridge-hint']")).not.toBeNull();
    expect(container.textContent).toContain("This thread mirrors your Telegram conversation");
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();
  });

  it("names the actual bound channel in the hint instead of hardcoding Telegram (the bug being fixed)", async () => {
    const profile = makeProfile({
      channels: [
        {
          binding_id: "discord-1",
          kind: "discord",
          enabled: true,
          bridge_thread_id: BRIDGE_THREAD_ID,
          allowed_senders: [],
        },
      ],
    });

    await renderChatView(profile, BRIDGE_THREAD_ID);

    // Scoped to the hint element itself — the thread-tab-strip fixture
    // (`makeThreads()`) titles the bridge thread pill "Telegram" regardless
    // of which channel is under test, which is unrelated fixture noise the
    // hint's own copy must not be confused with.
    const hint = container.querySelector("[data-testid='channel-bridge-hint']");
    expect(hint).not.toBeNull();
    expect(hint!.textContent).toContain("This thread mirrors your Discord conversation");
    expect(hint!.textContent).not.toContain("Telegram");
  });

  it("reveals the real composer via the button, and hides it again via the back control", async () => {
    const profile = makeProfile({
      channels: [
        {
          binding_id: "telegram",
          kind: "telegram",
          enabled: true,
          bridge_thread_id: BRIDGE_THREAD_ID,
          allowed_senders: ["555"],
        },
      ],
    });

    await renderChatView(profile, BRIDGE_THREAD_ID);

    const revealBtn = container.querySelector("[data-testid='channel-bridge-reveal-btn']");
    expect(revealBtn).not.toBeNull();

    await act(async () => {
      revealBtn!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(container.querySelector("[data-testid='channel-bridge-hint']")).toBeNull();
    expect(container.querySelector("[data-testid='chat-input-stub']")).not.toBeNull();
    const notice = container.querySelector("[data-testid='channel-bridge-composer-notice']");
    expect(notice).not.toBeNull();
    expect(notice!.textContent).toContain("won't reach Telegram");

    const hideBtn = container.querySelector("[data-testid='channel-bridge-hide-btn']");
    await act(async () => {
      hideBtn!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });

    expect(container.querySelector("[data-testid='channel-bridge-hint']")).not.toBeNull();
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();
  });

  it("keeps the normal composer on a non-bridge thread of the same Telegram-enabled agent", async () => {
    const profile = makeProfile({
      channels: [
        {
          binding_id: "telegram",
          kind: "telegram",
          enabled: true,
          bridge_thread_id: BRIDGE_THREAD_ID,
          allowed_senders: ["555"],
        },
      ],
    });

    await renderChatView(profile, DEFAULT_THREAD_ID);

    expect(container.querySelector("[data-testid='chat-input-stub']")).not.toBeNull();
    expect(container.querySelector("[data-testid='channel-bridge-hint']")).toBeNull();
  });

  it("keeps the normal composer for an agent with no Telegram binding at all", async () => {
    const profile = makeProfile();

    await renderChatView(profile, DEFAULT_THREAD_ID);

    expect(container.querySelector("[data-testid='chat-input-stub']")).not.toBeNull();
    expect(container.querySelector("[data-testid='channel-bridge-hint']")).toBeNull();
  });

  it("shows the read-only hint on a Slack per-conversation thread with no bridge_thread_id anywhere (the bug being fixed)", async () => {
    // Slack never populates `ChannelBinding.bridge_thread_id` at runtime —
    // it provisions one thread per conversation instead (see
    // `ChannelBridgeOrigin`'s docstring in `types/api.ts`). Only the
    // thread's own `channel_origin` (set on `SLACK_CONVO_THREAD_ID` above)
    // can recognize it as a bridge thread.
    const profile = makeProfile({
      channels: [
        {
          binding_id: "slack-1",
          kind: "slack",
          enabled: true,
          allowed_senders: [],
        },
      ],
    });

    await renderChatView(profile, SLACK_CONVO_THREAD_ID);

    const hint = container.querySelector("[data-testid='channel-bridge-hint']");
    expect(hint).not.toBeNull();
    expect(hint!.textContent).toContain("This thread mirrors your Slack conversation");
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();
  });

  it("keeps the normal composer on a Slack conversation thread once the Slack binding is disabled", async () => {
    const profile = makeProfile({
      channels: [
        {
          binding_id: "slack-1",
          kind: "slack",
          enabled: false,
          allowed_senders: [],
        },
      ],
    });

    await renderChatView(profile, SLACK_CONVO_THREAD_ID);

    expect(container.querySelector("[data-testid='chat-input-stub']")).not.toBeNull();
    expect(container.querySelector("[data-testid='channel-bridge-hint']")).toBeNull();
  });
});
