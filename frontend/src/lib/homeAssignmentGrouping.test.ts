/**
 * Unit coverage for `groupHomeAssignmentThreads` (lib/homeAssignmentGrouping.ts)
 * — the merge-across-agents layer HomeSidebar's "Assignments" section builds
 * on top of `resolveAssignmentThreadPartition`'s per-agent output. Verifies
 * both group-by modes (by assignment, by owning agent), recency ordering
 * within a group and across groups, and the per-group unread rollup — all
 * pure derivation over plain fixtures, no store or React rendering involved.
 */
import { describe, it, expect } from "vitest";
import { groupHomeAssignmentThreads, type HomeAssignmentThreadInfo } from "./homeAssignmentGrouping";
import type { Thread } from "../types/api";

interface MakeItemOptions {
  id: string;
  assignmentId?: string;
  agentId?: string;
  agentName?: string;
  unread?: boolean;
  createdAt?: string;
}

function makeItem({
  id,
  assignmentId = "assignment-1",
  agentId = "agent-1",
  agentName = "Agent One",
  unread = false,
  createdAt = "2026-01-01T00:00:00Z",
}: MakeItemOptions): HomeAssignmentThreadInfo {
  const thread: Thread = {
    id,
    title: null,
    scope: { type: "AgentChat", agent_id: agentId },
    transcript_path: `/tmp/${id}.jsonl`,
    kind: "fresh",
    created_at: createdAt,
    updated_at: createdAt,
  };
  return { thread, assignmentId, label: id, unread, agentId, agentName, agentEmoji: null };
}

describe("groupHomeAssignmentThreads", () => {
  it("groups by assignment_origin.assignment_id when groupBy is 'assignment', merging threads from different agents into one assignment group", () => {
    const fromAgentA = makeItem({ id: "thread-1", assignmentId: "assignment-1", agentId: "agent-a", agentName: "Agent A" });
    const fromAgentB = makeItem({ id: "thread-2", assignmentId: "assignment-1", agentId: "agent-b", agentName: "Agent B" });
    const otherAssignment = makeItem({ id: "thread-3", assignmentId: "assignment-2", agentId: "agent-a", agentName: "Agent A" });

    const groups = groupHomeAssignmentThreads([fromAgentA, fromAgentB, otherAssignment], "assignment");

    expect(groups).toHaveLength(2);
    const group1 = groups.find((g) => g.key === "assignment-1");
    const group2 = groups.find((g) => g.key === "assignment-2");
    expect(group1?.threads.map((t) => t.thread.id).sort()).toEqual(["thread-1", "thread-2"]);
    expect(group2?.threads.map((t) => t.thread.id)).toEqual(["thread-3"]);
  });

  it("groups by owning agent when groupBy is 'agent', keeping different assignments from the same agent together", () => {
    const assignment1FromA = makeItem({ id: "thread-1", assignmentId: "assignment-1", agentId: "agent-a", agentName: "Agent A" });
    const assignment2FromA = makeItem({ id: "thread-2", assignmentId: "assignment-2", agentId: "agent-a", agentName: "Agent A" });
    const assignment1FromB = makeItem({ id: "thread-3", assignmentId: "assignment-1", agentId: "agent-b", agentName: "Agent B" });

    const groups = groupHomeAssignmentThreads([assignment1FromA, assignment2FromA, assignment1FromB], "agent");

    expect(groups).toHaveLength(2);
    const agentAGroup = groups.find((g) => g.key === "agent-a");
    const agentBGroup = groups.find((g) => g.key === "agent-b");
    expect(agentAGroup?.threads.map((t) => t.thread.id).sort()).toEqual(["thread-1", "thread-2"]);
    expect(agentBGroup?.threads.map((t) => t.thread.id)).toEqual(["thread-3"]);
  });

  it("orders threads within a group by most-recent created_at first, same recency rule as resolveAssignmentThreadPartition", () => {
    const older = makeItem({ id: "thread-older", createdAt: "2026-01-01T00:00:00Z" });
    const newer = makeItem({ id: "thread-newer", createdAt: "2026-06-01T00:00:00Z" });

    const groups = groupHomeAssignmentThreads([older, newer], "assignment");

    expect(groups[0].threads.map((t) => t.thread.id)).toEqual(["thread-newer", "thread-older"]);
  });

  it("orders groups themselves by their own freshest thread, freshest group first", () => {
    const stale = makeItem({ id: "thread-stale", assignmentId: "assignment-stale", createdAt: "2026-01-01T00:00:00Z" });
    const fresh = makeItem({ id: "thread-fresh", assignmentId: "assignment-fresh", createdAt: "2026-06-01T00:00:00Z" });

    const groups = groupHomeAssignmentThreads([stale, fresh], "assignment");

    expect(groups.map((g) => g.key)).toEqual(["assignment-fresh", "assignment-stale"]);
  });

  it("rolls up each group's unread count from its member threads' `unread` flag", () => {
    const unread1 = makeItem({ id: "thread-unread-1", assignmentId: "assignment-1", unread: true });
    const read1 = makeItem({ id: "thread-read-1", assignmentId: "assignment-1", unread: false });
    const unread2 = makeItem({ id: "thread-unread-2", assignmentId: "assignment-2", unread: true });

    const groups = groupHomeAssignmentThreads([unread1, read1, unread2], "assignment");

    expect(groups.find((g) => g.key === "assignment-1")?.unreadCount).toBe(1);
    expect(groups.find((g) => g.key === "assignment-2")?.unreadCount).toBe(1);
  });

  it("returns no groups for an empty input list, in either mode", () => {
    expect(groupHomeAssignmentThreads([], "assignment")).toEqual([]);
    expect(groupHomeAssignmentThreads([], "agent")).toEqual([]);
  });
});
