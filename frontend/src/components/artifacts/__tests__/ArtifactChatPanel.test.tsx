// @vitest-environment jsdom
//
// Coverage for the artifact chat-to-adjust mini-thread panel: sending a
// message calls the chat endpoint through the shared `useArtifactRegen`
// instance (flipping it to "working"/"Adjusting…"), the reply bubble is
// read off the refetched artifact's `intent_ledger` once the run lands, and
// the composer's draft is isolated per `artifact:{artifactId}` so it never
// touches the main chat's drafts.
//
// The panel now renders its transcript through the real `MessageBubble`
// (same component Chat/Projects/Teams use) and its composer through the
// real `ChatInput` — see `ArtifactChatPanel.tsx`'s doc comment for why that
// reuse deliberately stops short of the shared `useChatStore` singleton.
// `ChatInput` itself (a TipTap rich-text editor with no test hooks of its
// own) is mocked to a plain textarea+button, mirroring how the rest of the
// codebase avoids exercising its internals directly (see
// `ProjectDetailView.transition.test.tsx`) — the mock also fires the same
// `onUnmount(text, html, conversationId)` contract the real component uses
// to persist a draft, so the draft-isolation test below stays meaningful.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { ArtifactChatPanel, artifactChatDraftKey } from "../ArtifactChatPanel";
import { useArtifactRegen, type UseArtifactRegenResult } from "../useArtifactRegen";
import { useDraftStore } from "../../../stores/draftStore";
import { useArtifactChatTranscriptStore } from "../../../stores/artifactChatTranscriptStore";
import type { ArtifactWithPayload } from "../../../types/api";

const getArtifactMock = vi.fn();
const regenerateArtifactMock = vi.fn();
const chatArtifactMock = vi.fn();

// `getArtifactChat` is deliberately NOT mocked here (unlike
// `ArtifactChatPanel.serverHydrate.test.tsx`, which exercises it directly):
// calling it as `undefined(...)` throws synchronously inside the panel's
// hydrate effect, which is swallowed by that effect's own try/catch — same
// as before this rewrite, and it keeps the effect's async IIFE resolving
// within the same synchronous tick as render (no dangling microtask for a
// bare, un-awaited `mount()` to race against).
vi.mock("../../../lib/api", () => ({
  getArtifact: (...args: unknown[]) => getArtifactMock(...args),
  regenerateArtifact: (...args: unknown[]) => regenerateArtifactMock(...args),
  chatArtifact: (...args: unknown[]) => chatArtifactMock(...args),
  getAttachmentUrl: (agentId: string, id: string) => `mock://attachment/${agentId}/${id}`,
}));

// `ArtifactChatPanel` -> `MessageBubble` -> `ArtifactRenderer` (for the
// artifact-card-in-a-bubble branch, unused here but still imported) pulls in
// these — same mocks `MessageBubble.artifactCard.test.tsx` needs for the
// same reason.
vi.mock("../../../lib/windows", () => ({ openArtifactWindow: vi.fn(), printArtifactWindow: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ save: vi.fn().mockResolvedValue(null) }));
vi.mock("@tauri-apps/plugin-fs", () => ({ writeFile: vi.fn().mockResolvedValue(undefined) }));
vi.mock("framer-motion", () => ({
  motion: {
    div: ({ children, ...rest }: React.HTMLAttributes<HTMLDivElement>) =>
      React.createElement("div", rest, children),
  },
  AnimatePresence: ({ children }: { children: React.ReactNode }) =>
    React.createElement(React.Fragment, null, children),
}));

// Minimal stand-in for the real `ChatInput` (a TipTap rich editor with no
// test hooks) that preserves the two contracts this panel depends on: the
// `onSend(text)` call, and the `onUnmount(text, html, conversationId)` draft
// hand-back fired when the composer tears down.
vi.mock("../../chat/ChatInput", () => ({
  ChatInput: (props: {
    onSend: (text: string) => void;
    disabled?: boolean;
    initialDraft?: string;
    conversationId?: string;
    onUnmount?: (text: string, html: string, conversationId: string) => void;
  }) => {
    const valueRef = React.useRef(props.initialDraft ?? "");
    React.useEffect(() => {
      const id = props.conversationId;
      return () => {
        if (id) props.onUnmount?.(valueRef.current, "", id);
      };
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [props.conversationId]);
    return React.createElement(
      "div",
      null,
      React.createElement("textarea", {
        "aria-label": "Artifact chat message",
        defaultValue: props.initialDraft,
        disabled: props.disabled,
        onChange: (e: React.ChangeEvent<HTMLTextAreaElement>) => {
          valueRef.current = e.target.value;
        },
      }),
      React.createElement(
        "button",
        {
          "aria-label": "Send chat message",
          disabled: props.disabled,
          onClick: () => props.onSend(valueRef.current),
        },
        "Send"
      )
    );
  },
}));

const AGENT_ID = "agent-1";
const ARTIFACT_ID = "artifact-1";

function makeArtifact(overrides: Partial<ArtifactWithPayload> = {}): ArtifactWithPayload {
  return {
    id: ARTIFACT_ID,
    title: "Weekly metrics",
    kind: "metric",
    format: "json",
    stored_filename: "blob.json",
    size_bytes: 0,
    checksum_sha256: "c0",
    refresh_intent: "none",
    origin_intent: null,
    capabilities: [],
    source_message_id: null,
    created_at: "2026-07-11T00:00:00Z",
    updated_at: "t0",
    last_refreshed_at: null,
    refresh_count: 0,
    pinned: false,
    pinned_at: null,
    group_id: null,
    intent_ledger: [],
    payload: { metrics: [] },
    ...overrides,
  };
}

let container: HTMLDivElement;
let root: Root;
let latestRegen: UseArtifactRegenResult | null = null;

// Mounts the real `useArtifactRegen` hook alongside the panel (not a stub) so
// the "send -> working -> done -> reply bubble" chain exercises the actual
// shared-instance contract the header's Refresh button also relies on.
function Harness({ onClose }: { onClose: () => void }) {
  latestRegen = useArtifactRegen(AGENT_ID, ARTIFACT_ID);
  return React.createElement(ArtifactChatPanel, {
    agentId: AGENT_ID,
    artifactId: ARTIFACT_ID,
    regen: latestRegen,
    onClose,
  });
}

function mount(onClose: () => void = vi.fn()) {
  act(() => {
    root.render(React.createElement(Harness, { onClose }));
  });
}

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  getArtifactMock.mockReset();
  regenerateArtifactMock.mockReset();
  chatArtifactMock.mockReset();
  latestRegen = null;
  useDraftStore.setState({ drafts: {}, draftHtml: {}, draftAttachments: {} });
  useArtifactChatTranscriptStore.setState({ transcripts: {} });
});

afterEach(async () => {
  await act(async () => {
    // The draft-isolation test below unmounts mid-test to exercise the
    // composer's onUnmount contract; guard against unmounting an
    // already-unmounted root a second time here.
    try {
      root.unmount();
    } catch {
      /* already unmounted */
    }
  });
  document.body.removeChild(container);
});

function typeMessage(text: string) {
  const textarea = container.querySelector('textarea[aria-label="Artifact chat message"]') as HTMLTextAreaElement;
  const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, "value")!.set!;
  setter.call(textarea, text);
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
}

describe("ArtifactChatPanel", () => {
  it("opens keyed by artifactId", () => {
    mount();
    const panel = container.querySelector('[data-testid="artifact-chat-panel"]');
    expect(panel).not.toBeNull();
    expect(panel!.getAttribute("data-artifact-id")).toBe(ARTIFACT_ID);
  });

  it("sending a message calls the chat endpoint and flips useArtifactRegen to working/Adjusting", async () => {
    getArtifactMock.mockResolvedValue(makeArtifact({ updated_at: "t0", checksum_sha256: "c0" }));
    chatArtifactMock.mockResolvedValue({ task_id: "bg-1" });

    mount();
    act(() => {
      typeMessage("Make the header blue.");
    });

    const sendBtn = container.querySelector('[aria-label="Send chat message"]') as HTMLButtonElement;
    expect(sendBtn.disabled).toBe(false);

    await act(async () => {
      sendBtn.click();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(chatArtifactMock).toHaveBeenCalledWith(AGENT_ID, ARTIFACT_ID, "Make the header blue.", []);
    expect(latestRegen!.status).toBe("working");
    expect(container.querySelector('[data-testid="artifact-chat-adjusting"]')).not.toBeNull();
    // The user's own message renders immediately, optimistically.
    expect(container.querySelector('[data-testid="artifact-chat-message-user"]')?.textContent).toContain(
      "Make the header blue.",
    );
  });

  it("shows the agent's intent_note as the reply once the adjust run lands", async () => {
    getArtifactMock
      .mockResolvedValueOnce(makeArtifact({ updated_at: "t0", checksum_sha256: "c0" })) // baseline snapshot
      .mockResolvedValueOnce(
        makeArtifact({
          updated_at: "t1",
          checksum_sha256: "c1",
          intent_ledger: [
            { timestamp: "t0", source: "chat", intent_note: "Changed the header to blue.", source_message_id: null },
          ],
        }),
      ) // poll: changed
      .mockResolvedValueOnce(
        makeArtifact({
          updated_at: "t1",
          checksum_sha256: "c1",
          intent_ledger: [
            { timestamp: "t0", source: "chat", intent_note: "Changed the header to blue.", source_message_id: null },
          ],
        }),
      ); // panel's own post-completion refetch for the reply text
    chatArtifactMock.mockResolvedValue({ task_id: "bg-1" });

    mount();
    act(() => {
      typeMessage("Make the header blue.");
    });
    const sendBtn = container.querySelector('[aria-label="Send chat message"]') as HTMLButtonElement;
    await act(async () => {
      sendBtn.click();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(latestRegen!.status).toBe("working");

    // useArtifactRegen's poll loop uses a real setTimeout, so this waits out a
    // real POLL_INTERVAL_MS (1500) plus the panel's own follow-up getArtifact.
    // That fixed 1.6s is why this test carries an explicit timeout below.
    await act(async () => {
      await new Promise((r) => setTimeout(r, 1600));
    });
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(latestRegen!.status).toBe("done");
    const reply = container.querySelector('[data-testid="artifact-chat-message-assistant"]');
    expect(reply?.textContent).toContain("Changed the header to blue.");
    // 20s, not vitest's default 5s. The real 1.6s wait above leaves only ~3.4s
    // for mount, render and three resolved fetches, and on a loaded machine
    // running the full suite in parallel workers that was not always enough —
    // this test failed roughly half of full-suite runs at ~5.2s. Raising the
    // ceiling costs no diagnostic power: a broken poll loop does not hang, it
    // fails the `toBe("done")` assertion above the moment the wait returns, so
    // the timeout could only ever fire on slowness, never on the regression
    // this test is here to catch.
  }, 20_000);

  it("keeps the composer draft isolated under the artifact:{artifactId} key, separate from main chat", async () => {
    // Simulate a draft already saved for the agent's *main* chat thread —
    // the panel must never read or clobber it.
    useDraftStore.getState().setDraft(AGENT_ID, "main chat draft, untouched");

    mount();
    act(() => {
      typeMessage("mini-thread draft text");
    });

    // The real `ChatInput` only hands a draft back on unmount / conversation
    // change (see `ProjectWorkspace.tsx`'s `ProjectCopilotOverlay` for the
    // same contract) — the mock above mirrors that, so tear the panel down
    // to trigger it.
    await act(async () => {
      root.unmount();
    });

    const state = useDraftStore.getState();
    const expectedKey = artifactChatDraftKey(ARTIFACT_ID);
    expect(expectedKey).toBe(`artifact:${ARTIFACT_ID}`);
    expect(state.drafts[expectedKey]).toBe("mini-thread draft text");
    // Main chat's draft (keyed by bare agentId) is untouched.
    expect(state.drafts[AGENT_ID]).toBe("main chat draft, untouched");
  });

  it("keeps the transcript in the in-memory store under the artifact:{artifactId} key so it reloads after remount", async () => {
    getArtifactMock.mockResolvedValue(makeArtifact({ updated_at: "t0", checksum_sha256: "c0" }));
    chatArtifactMock.mockResolvedValue({ task_id: "bg-1" });

    mount();
    act(() => {
      typeMessage("Make the header blue.");
    });
    const sendBtn = container.querySelector('[aria-label="Send chat message"]') as HTMLButtonElement;
    await act(async () => {
      sendBtn.click();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(container.querySelector('[data-testid="artifact-chat-message-user"]')?.textContent).toContain(
      "Make the header blue.",
    );
    const expectedKey = artifactChatDraftKey(ARTIFACT_ID);
    expect(useArtifactChatTranscriptStore.getState().transcripts[expectedKey]?.map((m) => ({
      role: m.role,
      content: m.content,
    }))).toEqual([{ role: "user", content: "Make the header blue." }]);

    // Unmount (simulating navigating away from the Assets view) and remount
    // fresh — the in-memory store is untouched by an unmount (it's a module-
    // level zustand store, not component state), so the transcript should
    // reappear without any network calls. This does NOT survive a real page
    // reload / app restart anymore — the store holds no localStorage copy,
    // only the running session's memory.
    await act(async () => {
      root.unmount();
    });
    getArtifactMock.mockClear();
    chatArtifactMock.mockClear();
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    mount();

    expect(container.querySelector('[data-testid="artifact-chat-message-user"]')?.textContent).toContain(
      "Make the header blue.",
    );
    expect(getArtifactMock).not.toHaveBeenCalled();
    expect(chatArtifactMock).not.toHaveBeenCalled();
  });
});
