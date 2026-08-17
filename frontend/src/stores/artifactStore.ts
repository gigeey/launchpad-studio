import { create } from "zustand";
import type { Artifact, ArtifactGroup, ArtifactKind, PinnedArtifact, RefreshIntent } from "../types/api";
import * as api from "../lib/api";

// Per-agent cache of an agent's artifact *list* (metadata only — no
// payload), mirroring `attachmentStore.ts`'s cache-then-share shape. The
// Assets panel renders this list directly. Inline thread-bubble cards no
// longer depend on it (see `cardsById` below) — they resolve straight off
// the `ArtifactWrite` tool result, live or persisted, with no fetch in the
// loop.

interface AgentArtifactCache {
  artifacts: Artifact[];
  status: "loading" | "loaded" | "error";
  loadedAt: number;
}

/** Just enough to render a collapsed inline artifact card — title, kind
 *  badge, and whether a refresh control applies. Sourced directly from an
 *  `ArtifactWrite` tool result (its JSON output already carries all four
 *  fields), never from a `listArtifacts` fetch, so a card can render the
 *  instant the id is known — live mid-stream or from a reloaded transcript —
 *  with no network round trip. The full payload is fetched separately by
 *  `ArtifactPreview` only once a card is expanded. */
export interface ArtifactCardStub {
  id: string;
  title: string;
  kind: ArtifactKind;
  refresh_intent: RefreshIntent;
}

/** Parses an `ArtifactWrite` tool result's `output` string (identical JSON
 *  shape whether it arrives live via the `tool_call_completed` SSE event or
 *  persisted in a `tool_result` transcript entry's `metadata.output`) into a
 *  card stub. Returns `null` for anything that isn't a well-formed success
 *  payload (a validation-error result serializes as `"error: ..."`, not
 *  JSON, so it naturally falls through here). */
export function parseArtifactWriteOutput(raw: unknown): ArtifactCardStub | null {
  if (typeof raw !== "string") return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  const id = (parsed as Record<string, unknown> | null)?.id;
  if (typeof id !== "string") return null;
  const obj = parsed as Record<string, unknown>;
  return {
    id,
    title: typeof obj.title === "string" ? obj.title : "Artifact",
    kind: (typeof obj.renderer === "string" ? obj.renderer : "unknown") as ArtifactKind,
    refresh_intent: (typeof obj.refresh_intent === "string" ? obj.refresh_intent : "none") as RefreshIntent,
  };
}

interface ArtifactStoreState {
  byAgent: Map<string, AgentArtifactCache>;

  /** Fetch (or re-fetch with `force`) an agent's artifact list. No-ops while
   *  a load for that agent is already in flight or already loaded, unless
   *  `force` is set. */
  loadArtifacts: (agentId: string, opts?: { force?: boolean }) => Promise<void>;
  getArtifacts: (agentId: string) => Artifact[];
  /** Drop an agent's cached list (e.g. after a delete) so the next read
   *  re-fetches. */
  invalidate: (agentId: string) => void;
  /** Delete an artifact outright (not just unpin). Optimistically drops it
   *  from the per-agent list, the card-stub cache, and the pinned list, then
   *  calls the API; rolls all three back on failure, mirroring `deleteGroup`
   *  below. */
  deleteArtifact: (agentId: string, artifactId: string) => Promise<void>;

  /** Lightweight card stubs keyed by artifact id (ids are globally unique,
   *  so no per-agent scoping is needed). Registered by both the live SSE
   *  path and the reloaded-transcript path from the same parsed
   *  `ArtifactWrite` output — see `parseArtifactWriteOutput`. */
  cardsById: Map<string, ArtifactCardStub>;
  registerCard: (card: ArtifactCardStub) => void;
  getCard: (id: string) => ArtifactCardStub | null;

  /** Ids registered via the live SSE path (`useSSE.ts`'s `tool_call_completed`
   *  handler) during THIS session, as opposed to the reloaded-transcript path
   *  (`MessageList.tsx`) that replays a whole scrollback on thread load.
   *  `ArtifactCardTile` reads this once at mount to decide whether to default
   *  open — a card the agent just published this turn is worth showing
   *  immediately, but auto-expanding every card in a long historical thread
   *  on load would reintroduce the mount-a-sandboxed-renderer-per-card perf
   *  trap the collapsed-by-default design exists to avoid. Session-scoped
   *  (never cleared) so a card that starts live and later re-renders via the
   *  finalized-message path still resolves to the same default. */
  liveIds: Set<string>;
  markCardLive: (id: string) => void;

  /** The cross-agent pinned listing that backs the global Assets page
   *  (`GET /artifacts/pinned`) — deliberately its own top-level cache rather
   *  than derived from `byAgent`, since a pinned artifact's owning agent may
   *  never have had its per-agent list loaded in this session. */
  pinned: PinnedArtifact[];
  pinnedStatus: "idle" | "loading" | "loaded" | "error";
  loadPinned: (opts?: { force?: boolean }) => Promise<void>;
  /** Optimistic pin/unpin: flips the flag in both the pinned cache and (if
   *  present) the owning agent's per-agent cache, then calls the API. Rolls
   *  back on failure so the UI never drifts from the server. */
  togglePin: (agentId: string, artifactId: string, pinned: boolean) => Promise<void>;

  /** The full set of user-defined artifact groups, backing the Assets
   *  sidebar's collapsible sections and the group-picker modal. */
  groups: ArtifactGroup[];
  groupsStatus: "idle" | "loading" | "loaded" | "error";
  loadGroups: (opts?: { force?: boolean }) => Promise<void>;
  /** Create a group and add it to the cache immediately (no optimistic id —
   *  the server mints it, so callers await the real `ArtifactGroup` back). */
  createGroup: (name: string) => Promise<ArtifactGroup>;
  /** Delete a group, drop it from the cache, and clear `group_id` off any
   *  pinned artifact that referenced it (mirroring the backend's cascade). */
  deleteGroup: (groupId: string) => Promise<void>;
  /** Optimistic group assignment (or `null` to clear back to ungrouped) on
   *  the pinned cache. Rolls back on failure. */
  setArtifactGroup: (agentId: string, artifactId: string, groupId: string | null) => Promise<void>;
}

export const useArtifactStore = create<ArtifactStoreState>()((set, get) => ({
  byAgent: new Map(),

  loadArtifacts: async (agentId, opts) => {
    const existing = get().byAgent.get(agentId);
    if (existing && !opts?.force && (existing.status === "loaded" || existing.status === "loading")) {
      return;
    }

    set((state) => {
      const next = new Map(state.byAgent);
      next.set(agentId, {
        artifacts: existing?.artifacts ?? [],
        status: "loading",
        loadedAt: existing?.loadedAt ?? 0,
      });
      return { byAgent: next };
    });

    try {
      const artifacts = await api.listArtifacts(agentId);
      set((state) => {
        const next = new Map(state.byAgent);
        next.set(agentId, { artifacts, status: "loaded", loadedAt: Date.now() });
        return { byAgent: next };
      });
    } catch {
      set((state) => {
        const next = new Map(state.byAgent);
        next.set(agentId, {
          artifacts: existing?.artifacts ?? [],
          status: "error",
          loadedAt: existing?.loadedAt ?? 0,
        });
        return { byAgent: next };
      });
    }
  },

  getArtifacts: (agentId) => get().byAgent.get(agentId)?.artifacts ?? [],

  invalidate: (agentId) => {
    set((state) => {
      if (!state.byAgent.has(agentId)) return state;
      const next = new Map(state.byAgent);
      next.delete(agentId);
      return { byAgent: next };
    });
  },

  deleteArtifact: async (agentId, artifactId) => {
    const prevAgentCache = get().byAgent.get(agentId);
    const prevCard = get().cardsById.get(artifactId);
    const prevPinned = get().pinned;

    set((state) => {
      const nextByAgent = new Map(state.byAgent);
      if (prevAgentCache) {
        nextByAgent.set(agentId, {
          ...prevAgentCache,
          artifacts: prevAgentCache.artifacts.filter((a) => a.id !== artifactId),
        });
      }
      const nextCardsById = new Map(state.cardsById);
      nextCardsById.delete(artifactId);
      return {
        byAgent: nextByAgent,
        cardsById: nextCardsById,
        pinned: state.pinned.filter((p) => p.id !== artifactId),
      };
    });

    try {
      await api.deleteArtifact(agentId, artifactId);
    } catch (err) {
      set((state) => {
        const nextByAgent = new Map(state.byAgent);
        if (prevAgentCache) nextByAgent.set(agentId, prevAgentCache);
        const nextCardsById = new Map(state.cardsById);
        if (prevCard) nextCardsById.set(artifactId, prevCard);
        return { byAgent: nextByAgent, cardsById: nextCardsById, pinned: prevPinned };
      });
      throw err;
    }
  },

  cardsById: new Map(),

  registerCard: (card) => {
    set((state) => {
      const existing = state.cardsById.get(card.id);
      if (
        existing &&
        existing.title === card.title &&
        existing.kind === card.kind &&
        existing.refresh_intent === card.refresh_intent
      ) {
        return state;
      }
      const next = new Map(state.cardsById);
      next.set(card.id, card);
      return { cardsById: next };
    });
  },

  getCard: (id) => get().cardsById.get(id) ?? null,

  liveIds: new Set(),

  markCardLive: (id) => {
    set((state) => {
      if (state.liveIds.has(id)) return state;
      const next = new Set(state.liveIds);
      next.add(id);
      return { liveIds: next };
    });
  },

  pinned: [],
  pinnedStatus: "idle",

  loadPinned: async (opts) => {
    const { pinnedStatus } = get();
    if (!opts?.force && (pinnedStatus === "loaded" || pinnedStatus === "loading")) {
      return;
    }
    set({ pinnedStatus: "loading" });
    try {
      const pinned = await api.listPinnedArtifacts();
      set({ pinned, pinnedStatus: "loaded" });
    } catch {
      set({ pinnedStatus: "error" });
    }
  },

  togglePin: async (agentId, artifactId, pinned) => {
    const prevPinnedList = get().pinned;
    const prevAgentCache = get().byAgent.get(agentId);

    // Optimistic update: patch the per-agent cache in place, and add/remove
    // from the pinned list immediately so the Assets page reflects the click
    // without waiting on the round trip.
    set((state) => {
      const nextByAgent = new Map(state.byAgent);
      if (prevAgentCache) {
        nextByAgent.set(agentId, {
          ...prevAgentCache,
          artifacts: prevAgentCache.artifacts.map((a) => (a.id === artifactId ? { ...a, pinned } : a)),
        });
      }

      let nextPinned: PinnedArtifact[];
      if (pinned) {
        const already = state.pinned.some((p) => p.id === artifactId);
        if (already) {
          nextPinned = state.pinned.map((p) => (p.id === artifactId ? { ...p, pinned: true } : p));
        } else {
          const source = prevAgentCache?.artifacts.find((a) => a.id === artifactId);
          nextPinned = source
            ? [...state.pinned, { ...source, pinned: true, agent_id: agentId }]
            : state.pinned;
        }
      } else {
        nextPinned = state.pinned.filter((p) => p.id !== artifactId);
      }

      return { byAgent: nextByAgent, pinned: nextPinned };
    });

    try {
      await api.setArtifactPinned(agentId, artifactId, pinned);
    } catch (err) {
      // Roll back on failure.
      set({ pinned: prevPinnedList });
      set((state) => {
        const nextByAgent = new Map(state.byAgent);
        if (prevAgentCache) nextByAgent.set(agentId, prevAgentCache);
        return { byAgent: nextByAgent };
      });
      throw err;
    }
  },

  groups: [],
  groupsStatus: "idle",

  loadGroups: async (opts) => {
    const { groupsStatus } = get();
    if (!opts?.force && (groupsStatus === "loaded" || groupsStatus === "loading")) {
      return;
    }
    set({ groupsStatus: "loading" });
    try {
      const groups = await api.listArtifactGroups();
      set({ groups, groupsStatus: "loaded" });
    } catch {
      set({ groupsStatus: "error" });
    }
  },

  createGroup: async (name) => {
    const group = await api.createArtifactGroup(name);
    set((state) => ({ groups: [...state.groups, group] }));
    return group;
  },

  deleteGroup: async (groupId) => {
    const prevGroups = get().groups;
    const prevPinned = get().pinned;

    set((state) => ({
      groups: state.groups.filter((g) => g.id !== groupId),
      pinned: state.pinned.map((p) => (p.group_id === groupId ? { ...p, group_id: null } : p)),
    }));

    try {
      await api.deleteArtifactGroup(groupId);
    } catch (err) {
      set({ groups: prevGroups, pinned: prevPinned });
      throw err;
    }
  },

  setArtifactGroup: async (agentId, artifactId, groupId) => {
    const prevPinned = get().pinned;

    set((state) => ({
      pinned: state.pinned.map((p) => (p.id === artifactId ? { ...p, group_id: groupId } : p)),
    }));

    try {
      await api.setArtifactGroup(agentId, artifactId, groupId);
    } catch (err) {
      set({ pinned: prevPinned });
      throw err;
    }
  },
}));
