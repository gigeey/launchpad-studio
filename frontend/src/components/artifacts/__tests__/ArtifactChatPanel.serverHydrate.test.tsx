// @vitest-environment jsdom
//
// Coverage for `ArtifactChatPanel` hydrating from the durable server
// transcript (`GET .../artifacts/{id}/chat`) on mount, seeding the in-memory
// runtime store (`artifactChatTranscriptStore`, no localStorage involved)
// rather than replacing it outright — an in-flight optimistic bubble the
// server hasn't caught up to yet must survive hydration, and a bubble the
// server has already confirmed must not be duplicated.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { ArtifactChatPanel } from "../ArtifactChatPanel";
import { useArtifactRegen, type UseArtifactRegenResult } from "../useArtifactRegen";
import { useDraftStore } from "../../../stores/draftStore";
import { useArtifactChatTranscriptStore } from "../../../stores/artifactChatTranscriptStore";

const getArtifactMock = vi.fn();
const regenerateArtifactMock = vi.fn();
const chatArtifactMock = vi.fn();
const getArtifactChatMock = vi.fn();

vi.mock("../../../lib/api", () => ({
  getArtifact: (...args: unknown[]) => getArtifactMock(...args),
  regenerateArtifact: (...args: unknown[]) => regenerateArtifactMock(...args),
  chatArtifact: (...args: unknown[]) => chatArtifactMock(...args),
  getArtifactChat: (...args: unknown[]) => getArtifactChatMock(...args),
  getAttachmentUrl: (agentId: string, id: string) => `mock://attachment/${agentId}/${id}`,
}));

// The panel renders its transcript through the real `MessageBubble`, which
// statically imports `ArtifactPreview` (`ArtifactRenderer.tsx`) for its
// artifact-card-in-a-bubble branch (unused by this suite, but still part of
// the module graph) — same transitive mocks `MessageBubble.artifactCard.test.tsx`
// needs for the same reason. The composer itself isn't exercised here, so
// `ChatInput` (a TipTap rich editor) is stubbed out entirely.
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
vi.mock("../../chat/ChatInput", () => ({ ChatInput: () => null }));

const AGENT_ID = "agent-1";
const ARTIFACT_ID = "artifact-1";

let container: HTMLDivElement;
let root: Root;
let latestRegen: UseArtifactRegenResult | null = null;

function Harness({ onClose }: { onClose: () => void }) {
  latestRegen = useArtifactRegen(AGENT_ID, ARTIFACT_ID);
  return React.createElement(ArtifactChatPanel, {
    agentId: AGENT_ID,
    artifactId: ARTIFACT_ID,
    regen: latestRegen,
    onClose,
  });
}

async function mount(onClose: () => void = vi.fn()) {
  await act(async () => {
    root.render(React.createElement(Harness, { onClose }));
    // Flush the hydrate effect's microtask chain (getArtifactChat -> merge
    // -> setMessages) inside the same act() batch as the initial render.
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  getArtifactMock.mockReset();
  regenerateArtifactMock.mockReset();
  chatArtifactMock.mockReset();
  getArtifactChatMock.mockReset();
  latestRegen = null;
  useDraftStore.setState({ drafts: {}, draftHtml: {}, draftAttachments: {} });
  useArtifactChatTranscriptStore.setState({ transcripts: {} });
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  document.body.removeChild(container);
});

describe("ArtifactChatPanel server hydration", () => {
  it("hydrates messages from getArtifactChat on mount", async () => {
    getArtifactChatMock.mockResolvedValue({
      entries: [
        { ts: "t0", role: "user", content: "Make the header blue.", event_type: "message", metadata: null },
        {
          ts: "t1",
          role: "assistant",
          content: "Changed the header to blue.",
          event_type: "message",
          metadata: null,
        },
      ],
      cursor: null,
    });

    await mount();

    expect(getArtifactChatMock).toHaveBeenCalledWith(AGENT_ID, ARTIFACT_ID);
    // `.textContent` on the real `MessageBubble` also picks up its
    // avatar/name/timestamp chrome, not just the message body — `toContain`
    // (not `toBe`) is the right assertion post-reuse, same convention
    // `MessageBubble.artifactCard.test.tsx` already uses.
    expect(container.querySelector('[data-testid="artifact-chat-message-user"]')?.textContent).toContain(
      "Make the header blue.",
    );
    expect(container.querySelector('[data-testid="artifact-chat-message-assistant"]')?.textContent).toContain(
      "Changed the header to blue.",
    );
  });

  it("handles an empty server transcript gracefully (new artifact, never chatted)", async () => {
    getArtifactChatMock.mockResolvedValue({ entries: [], cursor: null });

    await mount();

    expect(container.querySelector('[data-testid="artifact-chat-panel"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="artifact-chat-message-user"]')).toBeNull();
    expect(container.querySelector('[data-testid="artifact-chat-message-assistant"]')).toBeNull();
  });

  it("falls back to the local store without crashing when getArtifactChat rejects", async () => {
    getArtifactChatMock.mockRejectedValue(new Error("network hiccup"));
    useArtifactChatTranscriptStore.setState({
      transcripts: { [`artifact:${ARTIFACT_ID}`]: [{ role: "user", content: "Local-only draft turn" }] },
    });

    await mount();

    expect(container.querySelector('[data-testid="artifact-chat-panel"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="artifact-chat-message-user"]')?.textContent).toContain(
      "Local-only draft turn",
    );
  });

  it("keeps an in-flight optimistic bubble that hasn't reached the server yet", async () => {
    // Local store already has the user's just-sent message; the server
    // transcript hasn't observed it yet (simulating the network round-trip
    // still being in flight when this panel mounts / remounts).
    useArtifactChatTranscriptStore.setState({
      transcripts: { [`artifact:${ARTIFACT_ID}`]: [{ role: "user", content: "Not on the server yet" }] },
    });
    getArtifactChatMock.mockResolvedValue({ entries: [], cursor: null });

    await mount();

    const bubbles = container.querySelectorAll('[data-testid="artifact-chat-message-user"]');
    expect(bubbles.length).toBe(1);
    expect(bubbles[0].textContent).toContain("Not on the server yet");
  });

  it("de-dupes a local bubble once the server confirms it, instead of showing it twice", async () => {
    useArtifactChatTranscriptStore.setState({
      transcripts: { [`artifact:${ARTIFACT_ID}`]: [{ role: "user", content: "Make the header blue." }] },
    });
    getArtifactChatMock.mockResolvedValue({
      entries: [
        { ts: "t0", role: "user", content: "Make the header blue.", event_type: "message", metadata: null },
      ],
      cursor: null,
    });

    await mount();

    const bubbles = container.querySelectorAll('[data-testid="artifact-chat-message-user"]');
    expect(bubbles.length).toBe(1);
    expect(bubbles[0].textContent).toContain("Make the header blue.");
  });

  it("merges a confirmed message with a still-in-flight one, preserving both without duplication", async () => {
    useArtifactChatTranscriptStore.setState({
      transcripts: {
        [`artifact:${ARTIFACT_ID}`]: [
          { role: "user", content: "Make the header blue." },
          { role: "user", content: "Also enlarge the logo." },
        ],
      },
    });
    getArtifactChatMock.mockResolvedValue({
      entries: [
        { ts: "t0", role: "user", content: "Make the header blue.", event_type: "message", metadata: null },
      ],
      cursor: null,
    });

    await mount();

    const bubbles = container.querySelectorAll('[data-testid="artifact-chat-message-user"]');
    expect(bubbles.length).toBe(2);
    expect(bubbles[0].textContent).toContain("Make the header blue.");
    expect(bubbles[1].textContent).toContain("Also enlarge the logo.");
  });

  it("skips entries marked hidden_from_user", async () => {
    getArtifactChatMock.mockResolvedValue({
      entries: [
        {
          ts: "t0",
          role: "user",
          content: "synthetic injection",
          event_type: "message",
          metadata: null,
          hidden_from_user: true,
        },
        { ts: "t1", role: "assistant", content: "visible reply", event_type: "message", metadata: null },
      ],
      cursor: null,
    });

    await mount();

    expect(container.querySelector('[data-testid="artifact-chat-message-user"]')).toBeNull();
    expect(container.querySelector('[data-testid="artifact-chat-message-assistant"]')?.textContent).toContain(
      "visible reply",
    );
  });
});
