import type { DelegateTarget } from "../types/api";

/** Minimal shape needed for coordinator-level computation. */
export interface AgentProfileLike {
  id: string;
  delegates_to?: DelegateTarget[];
}

/**
 * Compute the coordinator level for a given agent profile.
 *
 * - Level 0 ("L") = leaf agent with no delegates_to entries.
 * - Level N ("CN") = agent that coordinates N levels of delegation depth.
 *
 * Uses a visited-set guard to terminate on A↔B mutual cycles (a revisited
 * node is treated as a leaf for level-computation purposes, matching the
 * Rust-side `coordinator_level_inner` function in system_prompt.rs).
 */
export function coordinatorLevel(
  profileId: string,
  profileIndex: Map<string, AgentProfileLike>,
): number {
  const visited = new Set<string>();
  return coordinatorLevelInner(profileId, profileIndex, visited);
}

function coordinatorLevelInner(
  profileId: string,
  profileIndex: Map<string, AgentProfileLike>,
  visited: Set<string>,
): number {
  if (visited.has(profileId)) return 0;
  visited.add(profileId);

  const profile = profileIndex.get(profileId);
  if (!profile || !profile.delegates_to || profile.delegates_to.length === 0) {
    visited.delete(profileId);
    return 0;
  }

  let maxChildLevel = 0;
  for (const target of profile.delegates_to) {
    const childLevel = coordinatorLevelInner(
      target.target_agent_id,
      profileIndex,
      visited,
    );
    if (childLevel > maxChildLevel) maxChildLevel = childLevel;
  }

  visited.delete(profileId);
  return maxChildLevel + 1;
}
