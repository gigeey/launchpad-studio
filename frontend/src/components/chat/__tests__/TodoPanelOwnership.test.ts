/**
 * TodoPanel owner chips — unit tests for the pure helper functions that drive
 * the chip rendering in TaskRow.
 *
 * Tests cover four row states:
 *   - pinned row  (assignment.mode == "pinned")
 *   - classified row  (assignment.mode == "classified")
 *   - classifying row  (assignment == null)
 *   - deferred row  (treated the same as classifying — assignment == null)
 *
 * No DOM rendering — pure function coverage.
 */

import { describe, it, expect } from "vitest";
import { ownerChipMode, resolveOwnerDisplayName } from "../TodoPanel";
import type { Task, DelegateTarget } from "../../../types/api";

function makeTask(overrides: Partial<Task> = {}): Task {
  return {
    id: "task-1",
    owner_agent_id: "agent-parent",
    prompt: "Do something",
    expected_outputs: [],
    status: "pending",
    group_id: "grp-1",
    attempt_count: 0,
    error_log: [],
    ...overrides,
  };
}

const BOOK_ENTRIES: DelegateTarget[] = [
  { target_agent_id: "agent-backend", name: "Backend Bot", purpose: "Handles API work", share_context_allowed: false },
  { target_agent_id: "agent-frontend", name: "Frontend Bot", purpose: "Handles UI work", share_context_allowed: false },
];

// ---------------------------------------------------------------------------
// ownerChipMode
// ---------------------------------------------------------------------------

describe("ownerChipMode", () => {
  it("returns 'classifying' when assignment is null", () => {
    const task = makeTask({ assignment: null });
    expect(ownerChipMode(task)).toBe("classifying");
  });

  it("returns 'classifying' when assignment is undefined (pre-Loop-L tasks)", () => {
    const task = makeTask({ assignment: undefined });
    expect(ownerChipMode(task)).toBe("classifying");
  });

  it("returns 'pinned' for a pinned assignment", () => {
    const task = makeTask({
      assignment: { owner_agent_id: "agent-backend", mode: "pinned" },
    });
    expect(ownerChipMode(task)).toBe("pinned");
  });

  it("returns 'classified' for a classified assignment", () => {
    const task = makeTask({
      assignment: { owner_agent_id: "agent-backend", mode: "classified" },
    });
    expect(ownerChipMode(task)).toBe("classified");
  });
});

// ---------------------------------------------------------------------------
// resolveOwnerDisplayName
// ---------------------------------------------------------------------------

describe("resolveOwnerDisplayName", () => {
  it("returns null when assignment is null (classifying)", () => {
    const task = makeTask({ assignment: null });
    expect(resolveOwnerDisplayName(task, "agent-parent", "Parent", BOOK_ENTRIES)).toBeNull();
  });

  it("returns null when assignment is undefined", () => {
    const task = makeTask({ assignment: undefined });
    expect(resolveOwnerDisplayName(task, "agent-parent", "Parent", BOOK_ENTRIES)).toBeNull();
  });

  it("returns selfName when owner is the parent agent itself (pinned to self)", () => {
    const task = makeTask({
      assignment: { owner_agent_id: "agent-parent", mode: "pinned" },
    });
    expect(resolveOwnerDisplayName(task, "agent-parent", "My Agent", BOOK_ENTRIES)).toBe("My Agent");
  });

  it("returns display_name from address book for a classified owner", () => {
    const task = makeTask({
      assignment: { owner_agent_id: "agent-backend", mode: "classified" },
    });
    expect(resolveOwnerDisplayName(task, "agent-parent", "Parent", BOOK_ENTRIES)).toBe("Backend Bot");
  });

  it("returns display_name from address book for a pinned owner", () => {
    const task = makeTask({
      assignment: { owner_agent_id: "agent-frontend", mode: "pinned" },
    });
    expect(resolveOwnerDisplayName(task, "agent-parent", "Parent", BOOK_ENTRIES)).toBe("Frontend Bot");
  });

  it("falls back to agent_id when the owner is not in the address book", () => {
    const task = makeTask({
      assignment: { owner_agent_id: "agent-unknown", mode: "classified" },
    });
    expect(resolveOwnerDisplayName(task, "agent-parent", "Parent", BOOK_ENTRIES)).toBe("agent-unknown");
  });

  it("works with an empty address book (fallback to agent_id)", () => {
    const task = makeTask({
      assignment: { owner_agent_id: "agent-backend", mode: "classified" },
    });
    expect(resolveOwnerDisplayName(task, "agent-parent", "Parent", [])).toBe("agent-backend");
  });
});

// ---------------------------------------------------------------------------
// Row state matrix: pinned / classified / classifying / deferred
// ---------------------------------------------------------------------------

describe("row state matrix", () => {
  it("pinned row: mode=pinned, label is resolved from book", () => {
    const task = makeTask({
      assignment: { owner_agent_id: "agent-backend", mode: "pinned" },
    });
    expect(ownerChipMode(task)).toBe("pinned");
    expect(resolveOwnerDisplayName(task, "p", "P", BOOK_ENTRIES)).toBe("Backend Bot");
  });

  it("classified row: mode=classified, label is resolved from book", () => {
    const task = makeTask({
      assignment: { owner_agent_id: "agent-frontend", mode: "classified" },
    });
    expect(ownerChipMode(task)).toBe("classified");
    expect(resolveOwnerDisplayName(task, "p", "P", BOOK_ENTRIES)).toBe("Frontend Bot");
  });

  it("classifying row: mode=classifying, no label", () => {
    const task = makeTask({ assignment: null });
    expect(ownerChipMode(task)).toBe("classifying");
    expect(resolveOwnerDisplayName(task, "p", "P", BOOK_ENTRIES)).toBeNull();
  });

  it("deferred row (same as classifying): assignment null, mode=classifying", () => {
    // task.deferred events keep assignment==null until classification resolves
    const task = makeTask({ assignment: null, status: "pending" });
    expect(ownerChipMode(task)).toBe("classifying");
    expect(resolveOwnerDisplayName(task, "p", "P", BOOK_ENTRIES)).toBeNull();
  });
});
