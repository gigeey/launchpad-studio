/**
 * Unit coverage for `resolveAssignmentThreadPartition` (lib/assignmentThreads.ts)
 * — the shared data layer both the Home "Assignments" section and the Chat
 * pinned column derive their rendering from. Verifies the partition
 * (working vs. assignment threads), the per-assignment grouping + recency
 * ordering, the label derivation (lookup name, truncated-id fallback,
 * title-empty fallback), and the aggregate/per-group unread counts — all
 * pure derivation over plain `Thread` fixtures, no store or React rendering
 * involved.
 */
import { describe, it, expect } from "vitest";
import { resolveAssignmentThreadPartition } from "./assignmentThreads";
import { threadActivityKey } from "../components/shared/ThreadActivityBadge";
import type { Assignment, AssignmentBridgeOrigin, Thread } from "../types/api";

const AGENT_ID = "agent-1";

function makeThread(overrides: Partial<Thread> & { id: string }): Thread {
  return {
    title: null,
    scope: { type: "AgentChat", agent_id: AGENT_ID },
    transcript_path: `/tmp/${overrides.id}.jsonl`,
    kind: "fresh",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function assignmentOrigin(assignmentId = "assignment-1", runId?: string): AssignmentBridgeOrigin {
  return runId === undefined ? { assignment_id: assignmentId } : { assignment_id: assignmentId, run_id: runId };
}

describe("resolveAssignmentThreadPartition", () => {
  it("puts a null-assignment_origin thread in the working bucket, not the assignment bucket", () => {
    const working = makeThread({ id: "thread-working-1", assignment_origin: null });
    const result = resolveAssignmentThreadPartition(AGENT_ID, [working], new Set());

    expect(result.workingThreads).toEqual([working]);
    expect(result.assignmentThreads).toEqual([]);
    expect(result.assignmentGroups).toEqual([]);
  });

  it("puts an undefined assignment_origin thread in the working bucket too", () => {
    const working = makeThread({ id: "thread-working-2" });
    const result = resolveAssignmentThreadPartition(AGENT_ID, [working], new Set());

    expect(result.workingThreads).toEqual([working]);
    expect(result.assignmentThreads).toEqual([]);
  });

  it("puts a non-null assignment_origin thread in the assignment bucket, not the working bucket", () => {
    const assignmentThread = makeThread({
      id: "thread-assignment-1",
      title: "Nightly digest run",
      assignment_origin: assignmentOrigin(),
    });
    const result = resolveAssignmentThreadPartition(AGENT_ID, [assignmentThread], new Set());

    expect(result.workingThreads).toEqual([]);
    expect(result.assignmentThreads).toHaveLength(1);
    expect(result.assignmentThreads[0].thread).toBe(assignmentThread);
  });

  it("groups assignment threads by assignment_origin.assignment_id, one group per assignment", () => {
    const run1 = makeThread({ id: "thread-assignment-1a", assignment_origin: assignmentOrigin("assignment-1") });
    const run2 = makeThread({ id: "thread-assignment-1b", assignment_origin: assignmentOrigin("assignment-1") });
    const other = makeThread({ id: "thread-assignment-2a", assignment_origin: assignmentOrigin("assignment-2") });
    const working = makeThread({ id: "thread-working-1" });

    const result = resolveAssignmentThreadPartition(AGENT_ID, [run1, run2, other, working], new Set());

    expect(result.workingThreads).toEqual([working]);
    expect(result.assignmentGroups).toHaveLength(2);
    const group1 = result.assignmentGroups.find((g) => g.assignmentId === "assignment-1");
    const group2 = result.assignmentGroups.find((g) => g.assignmentId === "assignment-2");
    expect(group1?.threads.map((t) => t.thread.id).sort()).toEqual(["thread-assignment-1a", "thread-assignment-1b"]);
    expect(group2?.threads.map((t) => t.thread.id)).toEqual(["thread-assignment-2a"]);
  });

  it("orders threads within an assignment by most-recent created_at first", () => {
    const older = makeThread({
      id: "thread-older",
      created_at: "2026-01-01T00:00:00Z",
      assignment_origin: assignmentOrigin(),
    });
    const newer = makeThread({
      id: "thread-newer",
      created_at: "2026-06-01T00:00:00Z",
      assignment_origin: assignmentOrigin(),
    });

    const result = resolveAssignmentThreadPartition(AGENT_ID, [older, newer], new Set());
    const group = result.assignmentGroups.find((g) => g.assignmentId === "assignment-1");

    expect(group?.threads.map((t) => t.thread.id)).toEqual(["thread-newer", "thread-older"]);
  });

  it("orders groups themselves by their own freshest thread, freshest group first", () => {
    const staleGroup = makeThread({
      id: "thread-stale",
      created_at: "2026-01-01T00:00:00Z",
      assignment_origin: assignmentOrigin("assignment-stale"),
    });
    const freshGroup = makeThread({
      id: "thread-fresh",
      created_at: "2026-06-01T00:00:00Z",
      assignment_origin: assignmentOrigin("assignment-fresh"),
    });

    const result = resolveAssignmentThreadPartition(AGENT_ID, [staleGroup, freshGroup], new Set());

    expect(result.assignmentGroups.map((g) => g.assignmentId)).toEqual(["assignment-fresh", "assignment-stale"]);
  });

  it("uses the assignment's name from assignmentLookup as the group label", () => {
    const assignmentThread = makeThread({ id: "thread-1", assignment_origin: assignmentOrigin("assignment-1") });
    const lookup = new Map<string, Pick<Assignment, "name">>([["assignment-1", { name: "Nightly Digest" }]]);

    const result = resolveAssignmentThreadPartition(AGENT_ID, [assignmentThread], new Set(), lookup);

    expect(result.assignmentGroups[0].label).toBe("Nightly Digest");
  });

  it("falls back to a truncated assignment id when assignmentLookup is absent or has no entry", () => {
    const assignmentThread = makeThread({
      id: "thread-1",
      assignment_origin: assignmentOrigin("assignment-abcdefgh-1234"),
    });

    const withoutLookup = resolveAssignmentThreadPartition(AGENT_ID, [assignmentThread], new Set());
    expect(withoutLookup.assignmentGroups[0].label).toBe("assignme");

    const emptyLookup = resolveAssignmentThreadPartition(
      AGENT_ID,
      [assignmentThread],
      new Set(),
      new Map(),
    );
    expect(emptyLookup.assignmentGroups[0].label).toBe("assignme");
  });

  it("labels an assignment thread from its own title/auto_title before falling back to the group label", () => {
    const withTitle = makeThread({
      id: "thread-titled",
      title: "Custom title",
      assignment_origin: assignmentOrigin("assignment-1"),
    });
    const withAutoTitle = makeThread({
      id: "thread-auto",
      title: null,
      auto_title: "Auto-derived title",
      assignment_origin: assignmentOrigin("assignment-1"),
    });
    const withNeither = makeThread({
      id: "thread-blank",
      title: "   ",
      auto_title: null,
      assignment_origin: assignmentOrigin("assignment-1"),
    });
    const lookup = new Map<string, Pick<Assignment, "name">>([["assignment-1", { name: "Nightly Digest" }]]);

    const result = resolveAssignmentThreadPartition(
      AGENT_ID,
      [withTitle, withAutoTitle, withNeither],
      new Set(),
      lookup,
    );
    const byId = new Map(result.assignmentThreads.map((t) => [t.thread.id, t]));

    expect(byId.get("thread-titled")?.label).toBe("Custom title");
    expect(byId.get("thread-auto")?.label).toBe("Auto-derived title");
    expect(byId.get("thread-blank")?.label).toBe("Nightly Digest");
  });

  it("marks an assignment thread unread via the same unreadThreadIds source, and rolls it up into the aggregate + per-group counts", () => {
    const unread1 = makeThread({ id: "thread-unread-1", assignment_origin: assignmentOrigin("assignment-1") });
    const read1 = makeThread({ id: "thread-read-1", assignment_origin: assignmentOrigin("assignment-1") });
    const unread2 = makeThread({ id: "thread-unread-2", assignment_origin: assignmentOrigin("assignment-2") });

    const unreadThreadIds = new Set<string>([
      threadActivityKey(AGENT_ID, unread1),
      threadActivityKey(AGENT_ID, unread2),
    ]);

    const result = resolveAssignmentThreadPartition(AGENT_ID, [unread1, read1, unread2], unreadThreadIds);
    const byId = new Map(result.assignmentThreads.map((t) => [t.thread.id, t]));

    expect(byId.get("thread-unread-1")?.unread).toBe(true);
    expect(byId.get("thread-read-1")?.unread).toBe(false);
    expect(byId.get("thread-unread-2")?.unread).toBe(true);

    const group1 = result.assignmentGroups.find((g) => g.assignmentId === "assignment-1");
    const group2 = result.assignmentGroups.find((g) => g.assignmentId === "assignment-2");
    expect(group1?.unreadCount).toBe(1);
    expect(group2?.unreadCount).toBe(1);
    expect(result.totalUnreadCount).toBe(2);
  });

  it("returns empty groups and zero unread when there are no assignment threads at all", () => {
    const working1 = makeThread({ id: "thread-working-1" });
    const working2 = makeThread({ id: "thread-working-2", assignment_origin: null });

    const result = resolveAssignmentThreadPartition(AGENT_ID, [working1, working2], new Set());

    expect(result.workingThreads).toEqual([working1, working2]);
    expect(result.assignmentThreads).toEqual([]);
    expect(result.assignmentGroups).toEqual([]);
    expect(result.totalUnreadCount).toBe(0);
  });

  it("returns empty everything for an empty thread list", () => {
    const result = resolveAssignmentThreadPartition(AGENT_ID, [], new Set());

    expect(result.workingThreads).toEqual([]);
    expect(result.assignmentThreads).toEqual([]);
    expect(result.assignmentGroups).toEqual([]);
    expect(result.totalUnreadCount).toBe(0);
  });

  it("drops an archived assignment thread from assignmentThreads/assignmentGroups entirely", () => {
    const archived = makeThread({
      id: "thread-archived",
      assignment_origin: assignmentOrigin(),
      archived_at: "2026-02-01T00:00:00Z",
    });
    const live = makeThread({ id: "thread-live", assignment_origin: assignmentOrigin() });

    const result = resolveAssignmentThreadPartition(AGENT_ID, [archived, live], new Set());

    expect(result.assignmentThreads.map((t) => t.thread.id)).toEqual(["thread-live"]);
    const group = result.assignmentGroups.find((g) => g.assignmentId === "assignment-1");
    expect(group?.threads.map((t) => t.thread.id)).toEqual(["thread-live"]);
  });

  it("does not leak an archived assignment thread into workingThreads either", () => {
    const archived = makeThread({
      id: "thread-archived",
      assignment_origin: assignmentOrigin(),
      archived_at: "2026-02-01T00:00:00Z",
    });

    const result = resolveAssignmentThreadPartition(AGENT_ID, [archived], new Set());

    expect(result.workingThreads).toEqual([]);
  });

  it("removes an entire assignment's group once its only thread is archived", () => {
    const archived = makeThread({
      id: "thread-archived",
      assignment_origin: assignmentOrigin("assignment-2"),
      archived_at: "2026-02-01T00:00:00Z",
    });

    const result = resolveAssignmentThreadPartition(AGENT_ID, [archived], new Set());

    expect(result.assignmentGroups).toEqual([]);
  });

  it("still includes an archived non-assignment (working) thread — only assignment-origin archiving is filtered here", () => {
    const archivedWorking = makeThread({ id: "thread-working-archived", archived_at: "2026-02-01T00:00:00Z" });

    const result = resolveAssignmentThreadPartition(AGENT_ID, [archivedWorking], new Set());

    expect(result.workingThreads).toEqual([archivedWorking]);
  });
});
