// @vitest-environment jsdom
/**
 * Regression coverage for the "answered form doesn't appear until you
 * navigate away and back" bug report: submitting an async form used to only
 * clear the pending-form pointer — nothing appended the answered bubble to
 * the local transcript, so it stayed invisible until the next full
 * `selectAgent` refetch replaced `messages` wholesale with the server's
 * (by-then-persisted) copy.
 *
 * Root cause (see `addAsyncFormAnswerEntry`'s doc comment in chatStore.ts):
 * the async answer's own live-push event (`FormResolved`) carries only
 * `form_id` — no values, no spec — so there was nothing for an SSE handler
 * to consume even if one existed. The fix builds the same synthetic
 * `form_answer` entry `addFormAnswerEntry` already builds for SYNC forms,
 * locally, right after the POST resolves.
 *
 * This suite exercises the real submit click path (`AskUserQuestionForm` /
 * `AsyncFormRequestCard` are NOT mocked, unlike `ChatView.asyncFormSnapshot
 * .test.tsx`) and asserts directly against `useChatStore` state — the same
 * `messages`/`allMessages` arrays `MessageList` renders from (not mountable
 * in jsdom itself; see `MessageList.formAnswerSpecSnapshot.test.tsx`'s doc
 * comment for why the DOM renderer isn't exercised directly here either).
 *
 * Render harness mirrors `ChatView.asyncFormSnapshot.test.tsx`.
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

const AGENT_ID = "agent-async-answer-1";
const DEFAULT_THREAD_ID = "thread-default-answer-1";
const FORM_ID = "form-answer-xyz";

function makeProfile(): AgentProfile {
  return {
    id: AGENT_ID,
    name: "Form Agent",
    description: "",
    provider: {
      type: "", command: "", args: [], output_format: "", input_mode: "",
      model_aliases: {}, resume_args: [], session_id_fields: [], clear_env: false, no_output_timeout_ms: 0,
    },
    model: null, skills: [], system_prompt: null, tools: null, env: {},
    max_instances: 1, timeout_seconds: 0, working_dir: null, home_dir: null, serialize: false,
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

const FORM_SPEC = {
  form_id: FORM_ID,
  title: "Rate this response",
  intro: "A couple of quick questions",
  fields: [
    { id: "rating", kind: "radio" as const, label: "How was it?", required: true, options: [{ id: "good", label: "Good" }, { id: "bad", label: "Bad" }] },
  ],
};

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
        spec: { form_id: FORM_ID, spec: FORM_SPEC, mode: "async" },
      },
    ],
  };
}

/** The transcript entry the backend persists once the answer lands — same
 *  shape `build_form_answer_entry` writes (crates/ao-server/src/routes/
 *  form_answers.rs): `event_type: "form_answer"`, `metadata: {form_id,
 *  values, spec}`. Used to model "a fresh transcript fetch after the answer
 *  was persisted" in the hydration test below. */
function persistedFormAnswerEntry(): TranscriptEntry {
  return {
    ts: "2024-01-01T00:00:02Z",
    role: { agent: AGENT_ID },
    content: "Rate this response\n\nHow was it? Good",
    event_type: "form_answer",
    metadata: {
      form_id: FORM_ID,
      values: { rating: { kind: "selections", values: ["good"] } },
      spec: FORM_SPEC,
    },
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

describe("ChatView: async form answer appears immediately on submit, without a remount", () => {
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

  async function renderChatView() {
    const { getAgent, getAgents, getMessages, listThreads } = await import("../../lib/api");
    const profile = makeProfile();
    const snapshot = makeSnapshot();
    vi.mocked(getAgent).mockResolvedValue(profile);
    vi.mocked(listThreads).mockResolvedValue(makeThreads());
    vi.mocked(getAgents).mockResolvedValue([snapshot]);
    vi.mocked(getMessages).mockResolvedValue({ messages: [], cursor: null });

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
        messages: [],
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

  function pickGoodAndSubmit() {
    const radios = Array.from(container.querySelectorAll("input[type='radio']")) as HTMLInputElement[];
    expect(radios.length).toBe(2);
    const goodIndex = FORM_SPEC.fields[0].options.findIndex((o) => o.id === "good");
    const good = radios[goodIndex];
    good.click();
    const submitBtn = container.querySelector<HTMLButtonElement>("[data-testid='form-submit-btn']");
    expect(submitBtn).not.toBeNull();
    submitBtn!.click();
  }

  it("appends exactly one form_answer entry to `messages`/`allMessages` immediately after a successful submit — no remount involved", async () => {
    const { submitAsyncFormAnswer } = await import("../../lib/api");
    vi.mocked(submitAsyncFormAnswer).mockResolvedValue({ message_id: "msg-1", status: "queued" });

    await renderChatView();
    expect(container.textContent).toContain("Rate this response");

    await act(async () => {
      pickGoodAndSubmit();
      // Flush the awaited submitAsyncFormAnswer + the store updates chained
      // after it.
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(submitAsyncFormAnswer).toHaveBeenCalledWith(AGENT_ID, FORM_ID, { rating: { kind: "selections", values: ["good"] } });

    const { messages, allMessages, agents } = useChatStore.getState();
    const answerEntries = messages.filter((m) => m.event_type === "form_answer");
    expect(answerEntries).toHaveLength(1);
    expect((answerEntries[0].metadata as Record<string, unknown>).form_id).toBe(FORM_ID);
    expect((answerEntries[0].metadata as Record<string, unknown>).values).toEqual({ rating: { kind: "selections", values: ["good"] } });
    expect(allMessages.filter((m) => m.event_type === "form_answer")).toHaveLength(1);

    // Pending pointer cleared — form slot is free again.
    expect(agents.find((a) => a.agent_id === AGENT_ID)?.pending_forms ?? []).toHaveLength(0);
    expect(container.querySelector("[data-testid='chat-input-stub']")).not.toBeNull();
  });

  it("a subsequent hydration from server data (selectAgent, the navigate-away-and-back path) does not produce a second copy", async () => {
    const { submitAsyncFormAnswer, getMessages } = await import("../../lib/api");
    vi.mocked(submitAsyncFormAnswer).mockResolvedValue({ message_id: "msg-1", status: "queued" });

    await renderChatView();

    await act(async () => {
      pickGoodAndSubmit();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(useChatStore.getState().messages.filter((m) => m.event_type === "form_answer")).toHaveLength(1);

    // Model the server having actually persisted the answer by now — the
    // next transcript fetch (same call `selectAgent` makes on a real
    // navigate-away-and-back remount) returns it for real.
    vi.mocked(getMessages).mockResolvedValue({ messages: [persistedFormAnswerEntry()], cursor: null });

    await act(async () => {
      await useChatStore.getState().selectAgent(AGENT_ID);
    });

    const { messages, allMessages } = useChatStore.getState();
    expect(messages.filter((m) => m.event_type === "form_answer")).toHaveLength(1);
    expect(allMessages.filter((m) => m.event_type === "form_answer")).toHaveLength(1);
    expect((messages.find((m) => m.event_type === "form_answer")?.metadata as Record<string, unknown>).form_id).toBe(FORM_ID);
  });

  it("a failed submit leaves no optimistic entry behind, keeps the form pending, and surfaces the failure", async () => {
    const { submitAsyncFormAnswer } = await import("../../lib/api");
    vi.mocked(submitAsyncFormAnswer).mockRejectedValue(new Error("network error"));

    await renderChatView();

    await act(async () => {
      pickGoodAndSubmit();
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });

    const { messages, agents } = useChatStore.getState();
    expect(messages.filter((m) => m.event_type === "form_answer")).toHaveLength(0);
    // Still pending — the form wasn't cleared out from under a failed submit.
    expect(agents.find((a) => a.agent_id === AGENT_ID)?.pending_forms?.[0]?.form_id).toBe(FORM_ID);
    // Composer stays hidden — the slot is still occupied by the (still
    // answerable) form.
    expect(container.querySelector("[data-testid='chat-input-stub']")).toBeNull();
    // The form re-enabled itself for a retry...
    const submitBtn = container.querySelector<HTMLButtonElement>("[data-testid='form-submit-btn']");
    expect(submitBtn?.disabled).toBe(false);
    // ...and the failure is visible, not just a console-only unhandled
    // rejection.
    expect(container.querySelector("[data-testid='form-submit-error']")).not.toBeNull();
  });
});
