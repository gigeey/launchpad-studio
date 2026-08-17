import { useMemo } from "react";
import { useChatStore, agentIdFromInFlightKey } from "../stores/chatStore";
import { useProjectStore } from "../stores/projectStore";
import { useSSE } from "../hooks/useSSE";
import { useProjectSSE } from "../hooks/useProjectSSE";

function AgentSSEChannel({ agentId }: { agentId: string }) {
  useSSE(agentId);
  return null;
}

function ProjectSSEChannel({ projectId }: { projectId: string }) {
  useProjectSSE(projectId);
  return null;
}

/**
 * App-level SSE owner. Lives in AppShell, outside <Outlet />, so it is never
 * torn down by React Router route transitions.
 *
 * Agent connections: one per agent that is either the currently-selected agent
 * OR has an active in-flight entry (streaming while the user navigated away).
 * Using a stable key-set string as a selector so only key additions/removals
 * trigger re-renders, not individual token updates.
 *
 * Project connection: one for the active project, kept alive until a different
 * project is opened, so the stream survives navigation and keeps writing to the
 * chatStore keyed buffer.
 */
export function SSEManager() {
  const selectedAgentId = useChatStore((s) => s.selectedAgentId);
  const activeProjectId = useProjectStore((s) => s.activeProjectId);

  // Build a stable string from in-flight agent keys — only changes when agents
  // are added/removed, not on every text_delta that creates a new Map reference.
  // Keys may be thread-scoped composites (`agentId::thread:xyz`) since
  // in-flight state is per-thread; the SSE channel itself is per-agent, so
  // unwrap back to the underlying agent id before deduping.
  const inFlightKeyStr = useChatStore((s) => {
    const keys = new Set<string>();
    for (const k of s.inFlightByAgent.keys()) {
      if (!k.startsWith("project:")) keys.add(agentIdFromInFlightKey(k));
    }
    return [...keys].sort().join("\0");
  });

  const agentIds = useMemo(() => {
    const ids = new Set<string>();
    if (selectedAgentId && !selectedAgentId.startsWith("project:")) {
      ids.add(selectedAgentId);
    }
    for (const k of inFlightKeyStr.split("\0")) {
      if (k) ids.add(k);
    }
    return [...ids];
  }, [selectedAgentId, inFlightKeyStr]);

  return (
    <>
      {agentIds.map((id) => (
        <AgentSSEChannel key={id} agentId={id} />
      ))}
      {activeProjectId && (
        <ProjectSSEChannel key={activeProjectId} projectId={activeProjectId} />
      )}
    </>
  );
}
