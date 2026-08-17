/**
 * Tests for agentTasklistStore SSE event routing and state shape updates.
 *
 * Covers:
 * - Initial empty state per agentId
 * - applyTasklistCreated fetches and slots the tasklist into the active slot
 * - applyTaskUpdated patches the task in place
 * - applyTasklistCompleted moves active → recent with completed status
 * - applyTasklistFailed moves active → recent with failed status
 * - applyTasklistStatusChanged updates status on active tasklist
 * - countInProgress returns correct in-progress count
 * - reset wipes all state
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

vi.mock("../lib/api", () => ({
  listAgentTasklists: vi.fn(),
  getAgentTasklist: vi.fn(),
  appendAgentTask: vi.fn(),
  stopAgentTasklist: vi.fn(),
  skipAgentTask: vi.fn(),
}));

import { useAgentTasklistStore, countInProgress } from "./agentTasklistStore";
import * as api from "../lib/api";
import type { Tasklist, Task, TaskGroup } from "../types/api";

const AGENT_ID = "agent-test-001";
const TASKLIST_ID = "tl-001";

function makeTask(overrides: Partial<Task> = {}): Task {
  return {
    id: "task-1",
    owner_agent_id: AGENT_ID,
    prompt: "Do something",
    expected_outputs: [],
    status: "pending",
    group_id: "g-1",
    attempt_count: 0,
    error_log: [],
    ...overrides,
  };
}

function makeGroup(tasks: Task[], mode: "SEQ" | "PAR" = "SEQ"): TaskGroup {
  return { id: "g-1", mode, tasks };
}

function makeTasklist(overrides: Partial<Tasklist> = {}): Tasklist {
  const task = makeTask();
  return {
    id: TASKLIST_ID,
    team_id: AGENT_ID,
    title: "Test Tasklist",
    description: "",
    status: "active",
    groups: [makeGroup([task])],
    workspace_dir: "/tmp/workspace",
    transcripts_dir: "/tmp/transcripts",
    created_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function store() {
  return useAgentTasklistStore.getState();
}

beforeEach(() => {
  useAgentTasklistStore.getState().reset();
  vi.clearAllMocks();
});

describe("initial state", () => {
  it("returns empty byAgent map", () => {
    expect(store().byAgent.size).toBe(0);
  });
});

describe("applyTasklistCreated", () => {
  it("fetches the tasklist and slots it as active", async () => {
    const tl = makeTasklist();
    vi.mocked(api.getAgentTasklist).mockResolvedValue(tl);

    await store().applyTasklistCreated(AGENT_ID, TASKLIST_ID);

    const entry = store().byAgent.get(AGENT_ID);
    expect(entry?.active?.id).toBe(TASKLIST_ID);
    expect(entry?.active?.status).toBe("active");
  });

  it("logs warning and leaves state unchanged on fetch error", async () => {
    vi.mocked(api.getAgentTasklist).mockRejectedValue(new Error("network error"));
    const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});

    await store().applyTasklistCreated(AGENT_ID, TASKLIST_ID);

    expect(store().byAgent.get(AGENT_ID)).toBeUndefined();
    expect(warnSpy).toHaveBeenCalledWith(
      expect.stringContaining("failed to fetch created tasklist"),
    );
  });

  it("does NOT set active when the fetched tasklist has a project_id (project leak guard)", async () => {
    const tl = makeTasklist({ project_id: "proj-xyz" });
    vi.mocked(api.getAgentTasklist).mockResolvedValue(tl);

    await store().applyTasklistCreated(AGENT_ID, TASKLIST_ID);

    // project-scoped tasklist must never enter the agent store
    const entry = store().byAgent.get(AGENT_ID);
    expect(entry).toBeUndefined();
  });

  it("DOES set active when the fetched tasklist has no project_id (personal tasklist)", async () => {
    const tl = makeTasklist({ project_id: undefined });
    vi.mocked(api.getAgentTasklist).mockResolvedValue(tl);

    await store().applyTasklistCreated(AGENT_ID, TASKLIST_ID);

    const entry = store().byAgent.get(AGENT_ID);
    expect(entry?.active?.id).toBe(TASKLIST_ID);
  });
});

describe("applyTasklistCompleted", () => {
  it("moves active tasklist to recent with completed status", () => {
    const tl = makeTasklist();
    useAgentTasklistStore.setState({
      byAgent: new Map([[AGENT_ID, { active: tl, recent: [], loading: false, error: null }]]),
    });

    store().applyTasklistCompleted(AGENT_ID, TASKLIST_ID);

    const entry = store().byAgent.get(AGENT_ID);
    expect(entry?.active).toBeNull();
    expect(entry?.recent).toHaveLength(1);
    expect(entry?.recent[0].status).toBe("completed");
  });

  it("no-ops if tasklist is not in state", () => {
    store().applyTasklistCompleted(AGENT_ID, "nonexistent");
    expect(store().byAgent.get(AGENT_ID)).toBeUndefined();
  });
});

describe("applyTasklistFailed", () => {
  it("moves active tasklist to recent with failed status", () => {
    const tl = makeTasklist();
    useAgentTasklistStore.setState({
      byAgent: new Map([[AGENT_ID, { active: tl, recent: [], loading: false, error: null }]]),
    });

    store().applyTasklistFailed(AGENT_ID, TASKLIST_ID, "some error");

    const entry = store().byAgent.get(AGENT_ID);
    expect(entry?.active).toBeNull();
    expect(entry?.recent[0].status).toBe("failed");
  });
});

describe("applyTasklistStatusChanged", () => {
  it("updates status on active tasklist for non-terminal status", () => {
    const tl = makeTasklist({ status: "active" });
    useAgentTasklistStore.setState({
      byAgent: new Map([[AGENT_ID, { active: tl, recent: [], loading: false, error: null }]]),
    });

    store().applyTasklistStatusChanged(AGENT_ID, TASKLIST_ID, "paused");

    const entry = store().byAgent.get(AGENT_ID);
    expect(entry?.active?.status).toBe("paused");
  });

  it("updates status on a recent tasklist", () => {
    const tl = makeTasklist({ status: "completed" });
    useAgentTasklistStore.setState({
      byAgent: new Map([[AGENT_ID, { active: null, recent: [tl], loading: false, error: null }]]),
    });

    store().applyTasklistStatusChanged(AGENT_ID, TASKLIST_ID, "cancelled");

    const entry = store().byAgent.get(AGENT_ID);
    expect(entry?.recent[0].status).toBe("cancelled");
  });

  it("clears active and moves to recent immediately when terminal status (completed) arrives", () => {
    const tl = makeTasklist({ status: "active" });
    useAgentTasklistStore.setState({
      byAgent: new Map([[AGENT_ID, { active: tl, recent: [], loading: false, error: null }]]),
    });

    store().applyTasklistStatusChanged(AGENT_ID, TASKLIST_ID, "completed");

    const entry = store().byAgent.get(AGENT_ID);
    expect(entry?.active).toBeNull();
    expect(entry?.recent).toHaveLength(1);
    expect(entry?.recent[0].status).toBe("completed");
  });

  it("clears active and moves to recent when status changes to failed", () => {
    const tl = makeTasklist({ status: "active" });
    useAgentTasklistStore.setState({
      byAgent: new Map([[AGENT_ID, { active: tl, recent: [], loading: false, error: null }]]),
    });

    store().applyTasklistStatusChanged(AGENT_ID, TASKLIST_ID, "failed");

    const entry = store().byAgent.get(AGENT_ID);
    expect(entry?.active).toBeNull();
    expect(entry?.recent[0].status).toBe("failed");
  });

  it("clears active and moves to recent when status changes to cancelled", () => {
    const tl = makeTasklist({ status: "active" });
    useAgentTasklistStore.setState({
      byAgent: new Map([[AGENT_ID, { active: tl, recent: [], loading: false, error: null }]]),
    });

    store().applyTasklistStatusChanged(AGENT_ID, TASKLIST_ID, "cancelled");

    const entry = store().byAgent.get(AGENT_ID);
    expect(entry?.active).toBeNull();
    expect(entry?.recent[0].status).toBe("cancelled");
  });
});

describe("applyTaskUpdated", () => {
  it("patches the task in the active tasklist without refetching", async () => {
    const task = makeTask({ status: "pending" });
    const tl = makeTasklist({ groups: [makeGroup([task])] });
    useAgentTasklistStore.setState({
      byAgent: new Map([[AGENT_ID, { active: tl, recent: [], loading: false, error: null }]]),
    });

    const updatedTask: Task = { ...task, status: "completed" };
    await store().applyTaskUpdated(AGENT_ID, TASKLIST_ID, updatedTask);

    const entry = store().byAgent.get(AGENT_ID);
    const storedTask = entry?.active?.groups[0].tasks[0];
    expect(storedTask?.status).toBe("completed");
  });
});

describe("countInProgress", () => {
  it("returns 0 for null tasklist", () => {
    expect(countInProgress(null)).toBe(0);
  });

  it("counts tasks with in_progress status", () => {
    const tl = makeTasklist({
      groups: [
        makeGroup([
          makeTask({ id: "t1", status: "in_progress" }),
          makeTask({ id: "t2", status: "pending" }),
          makeTask({ id: "t3", status: "in_progress" }),
          makeTask({ id: "t4", status: "completed" }),
        ]),
      ],
    });
    expect(countInProgress(tl)).toBe(2);
  });

  it("returns 0 when no tasks are in_progress", () => {
    const tl = makeTasklist({
      groups: [
        makeGroup([
          makeTask({ id: "t1", status: "pending" }),
          makeTask({ id: "t2", status: "completed" }),
        ]),
      ],
    });
    expect(countInProgress(tl)).toBe(0);
  });
});

describe("reset", () => {
  it("clears all state", () => {
    const tl = makeTasklist();
    useAgentTasklistStore.setState({
      byAgent: new Map([[AGENT_ID, { active: tl, recent: [], loading: false, error: null }]]),
    });
    store().reset();
    expect(store().byAgent.size).toBe(0);
  });
});
