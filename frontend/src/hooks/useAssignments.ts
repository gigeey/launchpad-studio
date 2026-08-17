import { useCallback, useEffect, useMemo, useState } from "react";
import type { Assignment } from "../lib/api";
import { listAssignments } from "../lib/api";
import { useChatStore } from "../stores/chatStore";
import { useAssignmentEditorModalStore } from "../stores/assignmentEditorModalStore";

/**
 * The resolved owner of an assignment. Assignments are single-agent (teams are
 * deprecated), so the owner is always
 * an agent and `isTeam` is always false. The field is retained so this shape
 * stays structurally compatible with `ScheduledTaskOwner`, letting the
 * Assignments surfaces reuse `ScheduledTaskOwnerChip` and the calendar's
 * owner-tinting helpers unchanged.
 */
export interface AssignmentOwner {
  id: string;
  name: string;
  emoji?: string;
  isTeam: boolean;
}

/** An assignment with its owning agent already resolved. */
export interface AssignmentWithOwner extends Assignment {
  owner: AssignmentOwner;
}

/**
 * The label to show for an assignment wherever a single identifying string is
 * needed (calendar tiles, list rows, tooltips): the user-supplied `name` when
 * set, falling back to the raw `instruction`.
 */
export function assignmentDisplayLabel(a: Pick<Assignment, "name" | "instruction">): string {
  return a.name?.trim() || a.instruction;
}

export interface UseAssignmentsResult {
  assignments: AssignmentWithOwner[];
  loading: boolean;
  error: string | null;
  refetch: () => Promise<void>;
}

/**
 * Aggregate data hook for the Assignments page. There is no global
 * `GET /assignments` roll-up endpoint yet — assignments are listed per agent —
 * so this fans out one `listAssignments` request per known agent and flattens
 * the results, then resolves each assignment's owning agent for display.
 *
 * The agent list is sourced from `chatStore` (already prefetched app-wide), and
 * the per-agent assignment fetch re-runs whenever the set of agent ids changes
 * so newly created/removed agents are reflected. A single agent's fetch failing
 * degrades to an empty list for that agent rather than failing the whole page.
 */
export function useAssignments(): UseAssignmentsResult {
  const agents = useChatStore((s) => s.agents);
  const fetchAgents = useChatStore((s) => s.fetchAgents);
  const savedAt = useAssignmentEditorModalStore((s) => s.savedAt);

  const [assignments, setAssignments] = useState<Assignment[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const refetch = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      // Read the freshest agent list at call time rather than closing over a
      // possibly-stale render snapshot.
      const current = useChatStore.getState().agents;
      const perAgent = await Promise.all(
        current.map((a) =>
          listAssignments(a.agent_id).catch(() => [] as Assignment[]),
        ),
      );
      setAssignments(perAgent.flat());
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load assignments");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void fetchAgents();
  }, [fetchAgents]);

  // Re-fan-out only when the *set* of agent ids changes, not on every agents
  // array mutation (SSE activity updates replace the array reference without
  // changing membership). Also re-fan-out whenever the app-shell-mounted
  // AssignmentEditorModal reports a save — it's mounted once for every view,
  // not just this page, so there's no local onSaved closure to hook instead.
  const agentIdsKey = useMemo(
    () => agents.map((a) => a.agent_id).sort().join(","),
    [agents],
  );
  useEffect(() => {
    void refetch();
  }, [agentIdsKey, savedAt, refetch]);

  const assignmentsWithOwner = useMemo(() => {
    const byId = new Map(agents.map((a) => [a.agent_id, a] as const));
    const out: AssignmentWithOwner[] = [];
    for (const a of assignments) {
      const agent = byId.get(a.agent_id);
      const owner: AssignmentOwner = agent
        ? { id: agent.agent_id, name: agent.name, emoji: agent.emoji, isTeam: false }
        : { id: a.agent_id, name: "Unknown agent", isTeam: false };
      out.push({ ...a, owner });
    }
    return out;
  }, [assignments, agents]);

  return {
    assignments: assignmentsWithOwner,
    loading,
    error,
    refetch,
  };
}
