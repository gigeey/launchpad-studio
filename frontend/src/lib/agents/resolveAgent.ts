import { useChatStore } from "../../stores/chatStore";
import type { AgentSnapshot } from "../../types/api";

export interface ResolvedAgent {
  id: string;
  displayName: string;
  emoji: string;
  isFallback: boolean;
}

export const FALLBACK_EMOJI = "🤖";

export function truncateId(id: string): string {
  if (id.length <= 8) return id;
  return `${id.slice(0, 8)}…`;
}

export function resolveAgent(
  id: string,
  snapshots: AgentSnapshot[],
): ResolvedAgent {
  if (!id) {
    return {
      id: id ?? "",
      displayName: truncateId(id ?? ""),
      emoji: FALLBACK_EMOJI,
      isFallback: true,
    };
  }

  const snapshot = snapshots.find((s) => s.agent_id === id);
  if (snapshot) {
    return {
      id,
      displayName: snapshot.name,
      emoji: snapshot.emoji ?? FALLBACK_EMOJI,
      isFallback: false,
    };
  }

  return {
    id,
    displayName: truncateId(id),
    emoji: FALLBACK_EMOJI,
    isFallback: true,
  };
}

export function useResolvedAgent(id: string): ResolvedAgent {
  const snapshots = useChatStore((s) => s.agents);
  const selectedAgentId = useChatStore((s) => s.selectedAgentId);
  const selectedAgentProfile = useChatStore((s) => s.selectedAgentProfile);

  const resolved = resolveAgent(id, snapshots);

  // Hidden agents (e.g. tasklist co-pilots, inline coordinators) are kept out
  // of the snapshot list via `owning_team_id`, so a snapshot miss is expected
  // when the user is chatting with one. If the id we're resolving is the
  // currently selected agent, the full profile has already been fetched and
  // cached in `selectedAgentProfile` — use it instead of the truncated-id
  // fallback.
  if (resolved.isFallback && id && id === selectedAgentId && selectedAgentProfile) {
    return {
      id,
      displayName: selectedAgentProfile.name,
      emoji: selectedAgentProfile.emoji ?? FALLBACK_EMOJI,
      isFallback: false,
    };
  }

  return resolved;
}
