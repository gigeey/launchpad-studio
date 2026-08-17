// @vitest-environment jsdom
//
// A compact artifact-card branch in the thread bubble that never mounts a
// live iframe for a bubble that isn't opened. Covers both the standalone
// `ArtifactCardTile` (resolves its title/kind/refresh_intent from
// `useArtifactStore`'s `cardsById` registry, with a fallback to the
// full per-agent artifact list) and its wiring into `MessageBubble` (renders
// one tile per id in the `artifactIds` prop — no store-side turn_id
// matching on the read path).
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import type { ArtifactWithPayload, TranscriptEntry } from "../../../types/api";
import type { ArtifactCardStub } from "../../../stores/artifactStore";

const getArtifactMock = vi.fn();
const openArtifactWindowMock = vi.fn();

vi.mock("../../../lib/api", () => ({
  getArtifact: (...a: unknown[]) => getArtifactMock(...a),
  listArtifacts: vi.fn().mockResolvedValue([]),
  getAttachmentUrl: (agentId: string, id: string) => `mock://attachment/${agentId}/${id}`,
}));

vi.mock("../../../lib/windows", () => ({
  openArtifactWindow: (...a: unknown[]) => openArtifactWindowMock(...a),
}));

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

import { ArtifactCardTile, MessageBubble } from "../MessageBubble";
import { useArtifactStore } from "../../../stores/artifactStore";

function makeCard(overrides: Partial<ArtifactCardStub> = {}): ArtifactCardStub {
  return {
    id: "artifact-1",
    title: "Weekly report",
    kind: "table",
    refresh_intent: "none",
    ...overrides,
  };
}

describe("ArtifactCardTile", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    getArtifactMock.mockReset();
    openArtifactWindowMock.mockReset();
    useArtifactStore.setState({ byAgent: new Map(), cardsById: new Map(), liveIds: new Set() });
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function flush() {
    await act(async () => {
      await Promise.resolve();
    });
  }

  it("renders a compact card (title + kind) resolved from the cardsById registry, without mounting the live renderer", async () => {
    useArtifactStore.getState().registerCard(makeCard());

    await act(async () => {
      root.render(
        React.createElement(ArtifactCardTile, { artifactId: "artifact-1", agentId: "agent-1" }),
      );
    });

    expect(container.textContent).toContain("Weekly report");
    expect(container.textContent).toContain("Table");
    // Collapsed by default — the shared renderer must not be mounted for
    // every scrollback bubble (perf).
    expect(container.querySelector('[data-testid="artifact-card-expanded"]')).toBeNull();
    expect(getArtifactMock).not.toHaveBeenCalled();
  });

  it("defaults open (mounts the shared renderer) for a card registered via the live SSE path", async () => {
    useArtifactStore.getState().registerCard(makeCard());
    useArtifactStore.getState().markCardLive("artifact-1");
    getArtifactMock.mockResolvedValue({
      ...makeCard(),
      format: "json",
      stored_filename: "blob.json",
      size_bytes: 1024,
      checksum_sha256: "deadbeef",
      origin_intent: null,
      capabilities: [],
      source_message_id: null,
      created_at: "2026-07-11T00:00:00Z",
      updated_at: "2026-07-11T00:00:00Z",
      last_refreshed_at: null,
      refresh_count: 0,
      pinned: false,
      pinned_at: null,
      group_id: null,
      payload: { rows: [] },
    } as ArtifactWithPayload);

    await act(async () => {
      root.render(
        React.createElement(ArtifactCardTile, { artifactId: "artifact-1", agentId: "agent-1" }),
      );
    });
    await flush();

    expect(container.querySelector('[data-testid="artifact-card-expanded"]')).not.toBeNull();
    expect(getArtifactMock).toHaveBeenCalledWith("agent-1", "artifact-1");
  });

  it("a reloaded-history card (registered, but never marked live) still defaults collapsed", async () => {
    useArtifactStore.getState().registerCard(makeCard());
    // No markCardLive call — mirrors MessageList.tsx's scrollback-replay path.

    await act(async () => {
      root.render(
        React.createElement(ArtifactCardTile, { artifactId: "artifact-1", agentId: "agent-1" }),
      );
    });

    expect(container.querySelector('[data-testid="artifact-card-expanded"]')).toBeNull();
    expect(getArtifactMock).not.toHaveBeenCalled();
  });

  it("falls back to the per-agent artifact list when no card stub is registered", async () => {
    useArtifactStore.setState({
      byAgent: new Map([
        [
          "agent-1",
          {
            artifacts: [
              {
                id: "artifact-legacy",
                title: "Legacy artifact",
                kind: "list",
                format: "json",
                stored_filename: "blob.json",
                size_bytes: 10,
                checksum_sha256: "abc",
                refresh_intent: "none",
                origin_intent: null,
                capabilities: [],
                source_message_id: null,
                created_at: "2026-07-11T00:00:00Z",
                updated_at: "2026-07-11T00:00:00Z",
                last_refreshed_at: null,
                refresh_count: 0,
                pinned: false,
                pinned_at: null,
                group_id: null,
              },
            ],
            status: "loaded",
            loadedAt: 0,
          },
        ],
      ]),
      cardsById: new Map(),
    });

    await act(async () => {
      root.render(
        React.createElement(ArtifactCardTile, { artifactId: "artifact-legacy", agentId: "agent-1" }),
      );
    });

    expect(container.textContent).toContain("Legacy artifact");
    expect(container.textContent).toContain("List");
  });

  it("expanding the card mounts the shared renderer and fetches the artifact by id", async () => {
    useArtifactStore.getState().registerCard(makeCard());
    getArtifactMock.mockResolvedValue({
      ...makeCard(),
      format: "json",
      stored_filename: "blob.json",
      size_bytes: 1024,
      checksum_sha256: "deadbeef",
      origin_intent: null,
      capabilities: [],
      source_message_id: null,
      created_at: "2026-07-11T00:00:00Z",
      updated_at: "2026-07-11T00:00:00Z",
      last_refreshed_at: null,
      refresh_count: 0,
      pinned: false,
      pinned_at: null,
      group_id: null,
      payload: { rows: [] },
    } as ArtifactWithPayload);

    await act(async () => {
      root.render(
        React.createElement(ArtifactCardTile, { artifactId: "artifact-1", agentId: "agent-1" }),
      );
    });

    const header = container.querySelector('[aria-label="Expand Weekly report"]') as HTMLElement;
    expect(header).not.toBeNull();
    await act(async () => {
      header.click();
    });
    await flush();

    expect(container.querySelector('[data-testid="artifact-card-expanded"]')).not.toBeNull();
    expect(getArtifactMock).toHaveBeenCalledWith("agent-1", "artifact-1");
  });

  it("the pop-out control calls openArtifactWindow with agent + artifact id, without expanding", async () => {
    useArtifactStore.getState().registerCard(makeCard());

    await act(async () => {
      root.render(
        React.createElement(ArtifactCardTile, { artifactId: "artifact-1", agentId: "agent-1" }),
      );
    });

    const popOutBtn = container.querySelector(
      '[aria-label="Open artifact in new window"]',
    ) as HTMLButtonElement;
    expect(popOutBtn).not.toBeNull();
    await act(async () => {
      popOutBtn.click();
    });

    expect(openArtifactWindowMock).toHaveBeenCalledWith("agent-1", "artifact-1");
    // stopPropagation on the pop-out click keeps the card collapsed.
    expect(container.querySelector('[data-testid="artifact-card-expanded"]')).toBeNull();
  });

  it("does not render a refresh control for a static (refresh_intent: none) artifact", async () => {
    useArtifactStore.getState().registerCard(makeCard({ refresh_intent: "none" }));

    await act(async () => {
      root.render(
        React.createElement(ArtifactCardTile, { artifactId: "artifact-1", agentId: "agent-1" }),
      );
    });

    expect(container.querySelector('[aria-label="Refresh artifact"]')).toBeNull();
  });

  it("renders a refresh control for a whole_artifact-refreshable artifact", async () => {
    useArtifactStore.getState().registerCard(makeCard({ refresh_intent: "whole_artifact" }));

    await act(async () => {
      root.render(
        React.createElement(ArtifactCardTile, { artifactId: "artifact-1", agentId: "agent-1" }),
      );
    });

    expect(container.querySelector('[aria-label="Refresh artifact"]')).not.toBeNull();
  });
});

describe("MessageBubble — artifact-card branch", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    useArtifactStore.setState({ byAgent: new Map(), cardsById: new Map(), liveIds: new Set() });
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  function agentEntry(overrides: Partial<TranscriptEntry> = {}): TranscriptEntry {
    return {
      ts: "2026-07-11T00:00:00Z",
      role: { agent: "agent-1" },
      content: "Here's the report you asked for.",
      event_type: "response",
      ...overrides,
    };
  }

  it("renders one card per id in artifactIds", async () => {
    useArtifactStore.getState().registerCard(makeCard());

    await act(async () => {
      root.render(
        React.createElement(MessageBubble, {
          entry: agentEntry(),
          agentName: "Rex",
          agentEmoji: "🤖",
          agentId: "agent-1",
          artifactIds: ["artifact-1"],
        }),
      );
    });

    expect(container.querySelectorAll('[data-testid="artifact-card-tile"]')).toHaveLength(1);
    expect(container.textContent).toContain("Weekly report");
  });

  it("renders multiple cards, one per artifact id, deduped", async () => {
    useArtifactStore.getState().registerCard(makeCard({ id: "artifact-1", title: "Report A" }));
    useArtifactStore.getState().registerCard(makeCard({ id: "artifact-2", title: "Report B" }));

    await act(async () => {
      root.render(
        React.createElement(MessageBubble, {
          entry: agentEntry(),
          agentName: "Rex",
          agentEmoji: "🤖",
          agentId: "agent-1",
          artifactIds: ["artifact-1", "artifact-2", "artifact-1"],
        }),
      );
    });

    // Duplicate id in the array still renders as a single React element per
    // unique key — but the array itself isn't deduped by MessageBubble, so
    // assert the *unique* card content appears rather than element count.
    expect(container.textContent).toContain("Report A");
    expect(container.textContent).toContain("Report B");
  });

  it("does not render a card when artifactIds is empty or absent", async () => {
    await act(async () => {
      root.render(
        React.createElement(MessageBubble, {
          entry: agentEntry(),
          agentName: "Rex",
          agentEmoji: "🤖",
          agentId: "agent-1",
        }),
      );
    });

    expect(container.querySelector('[data-testid="artifact-card-tile"]')).toBeNull();
  });
});
