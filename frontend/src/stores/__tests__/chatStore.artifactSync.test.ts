/**
 * `syncRunArtifacts` (fired from the `run_ended` SSE handler) force-refreshes
 * the agent's artifact list so a just-produced artifact shows up in the
 * Assets panel without the user navigating away and back.
 *
 * It used to also carry a turn_id-matching transcript refetch so the inline
 * thread-bubble card could resolve (`getBySourceMessageId` against
 * `metadata.turn_id`). That whole mechanism is gone: inline cards now
 * resolve straight off the `ArtifactWrite` tool result (live via
 * `appendInFlightArtifactId`, or from the persisted transcript via
 * `MessageList`'s `extractArtifactWriteResults`), so `syncRunArtifacts` has
 * nothing left to do beyond the list refresh — see
 * `MessageBubble.artifactCard.test.tsx` and
 * `MessageList.artifactCards.test.ts` for the inline-card coverage.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import type { Artifact } from "../../types/api";

const AGENT_ID = "artifact-sync-agent";

const mockListArtifacts = vi.fn();
const mockGetMessages = vi.fn();
const mockGetAgents = vi.fn();

vi.mock("../../lib/api", () => ({
  getAgents: (...a: unknown[]) => mockGetAgents(...a),
  listArtifacts: (...a: unknown[]) => mockListArtifacts(...a),
  getMessages: (...a: unknown[]) => mockGetMessages(...a),
}));

import { useChatStore, inFlightKey } from "../chatStore";
import { useArtifactStore } from "../../stores/artifactStore";

function chatStore() {
  return useChatStore.getState();
}

function makeArtifact(overrides: Partial<Artifact> = {}): Artifact {
  return {
    id: "artifact-1",
    title: "Weekly report",
    kind: "table",
    format: "json",
    stored_filename: "blob.json",
    size_bytes: 1024,
    checksum_sha256: "deadbeef",
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
    ...overrides,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  chatStore().reset();
  useArtifactStore.setState({ byAgent: new Map(), cardsById: new Map() });
  mockGetAgents.mockResolvedValue([]);
  mockListArtifacts.mockResolvedValue([]);
  mockGetMessages.mockResolvedValue({ messages: [], cursor: null });
});

describe("syncRunArtifacts — force-refreshes the artifact list", () => {
  it("force-loads the agent's artifact list, even if already marked loaded", async () => {
    // Pre-seed the store as already "loaded" — plain loadArtifacts would no-op.
    useArtifactStore.setState({
      byAgent: new Map([[AGENT_ID, { artifacts: [], status: "loaded", loadedAt: 0 }]]),
    });
    mockListArtifacts.mockResolvedValue([makeArtifact()]);

    useChatStore.setState({ selectedAgentId: AGENT_ID, allMessages: [], messages: [] });
    chatStore().syncRunArtifacts(AGENT_ID);

    await vi.waitFor(() => expect(mockListArtifacts).toHaveBeenCalledWith(AGENT_ID));
    await vi.waitFor(() => expect(useArtifactStore.getState().getArtifacts(AGENT_ID)).toHaveLength(1));
  });

  it("issues exactly one artifact-list GET per ended run and never touches the transcript", async () => {
    mockListArtifacts.mockResolvedValue([makeArtifact()]);
    useChatStore.setState({ selectedAgentId: AGENT_ID, allMessages: [], messages: [] });
    chatStore().syncRunArtifacts(AGENT_ID);

    await vi.waitFor(() => expect(mockListArtifacts).toHaveBeenCalledTimes(1));
    // Give any further (incorrect) async work a chance to run before asserting it didn't.
    await new Promise((r) => setTimeout(r, 10));
    expect(mockListArtifacts).toHaveBeenCalledTimes(1);
    expect(mockGetMessages).not.toHaveBeenCalled();
  });

  it("resolves the composite inFlightKey down to the plain agent id for the list fetch", async () => {
    mockListArtifacts.mockResolvedValue([]);
    chatStore().syncRunArtifacts(inFlightKey(AGENT_ID, "thread-42"));

    await vi.waitFor(() => expect(mockListArtifacts).toHaveBeenCalledWith(AGENT_ID));
  });
});
