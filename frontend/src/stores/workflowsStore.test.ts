// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { isWorkflowEnabled, resolveEnabledWorkflowIds } from "./workflowsStore";
import type { WorkflowSummary } from "../types/workflow";

const catalog: WorkflowSummary[] = [
  { id: "wf-a", name: "A", phase_count: 3 },
  { id: "wf-b", name: "B", phase_count: 1 },
];

describe("isWorkflowEnabled", () => {
  it("returns true for every id when agent.workflows is 'all'", () => {
    expect(isWorkflowEnabled("all", "wf-a")).toBe(true);
    expect(isWorkflowEnabled("all", "anything")).toBe(true);
  });

  it("returns membership for an array", () => {
    expect(isWorkflowEnabled(["wf-a"], "wf-a")).toBe(true);
    expect(isWorkflowEnabled(["wf-a"], "wf-b")).toBe(false);
  });

  it("returns false for null/undefined", () => {
    expect(isWorkflowEnabled(null, "wf-a")).toBe(false);
    expect(isWorkflowEnabled(undefined, "wf-a")).toBe(false);
  });
});

describe("resolveEnabledWorkflowIds", () => {
  it("expands 'all' to the full catalog id list", () => {
    expect(resolveEnabledWorkflowIds("all", catalog)).toEqual(["wf-a", "wf-b"]);
  });

  it("drops orphan ids that are no longer in the catalog", () => {
    expect(resolveEnabledWorkflowIds(["wf-a", "deleted-wf"], catalog)).toEqual(["wf-a"]);
  });

  it("returns an empty list for null/undefined/empty array", () => {
    expect(resolveEnabledWorkflowIds(null, catalog)).toEqual([]);
    expect(resolveEnabledWorkflowIds(undefined, catalog)).toEqual([]);
    expect(resolveEnabledWorkflowIds([], catalog)).toEqual([]);
  });

  it("expands 'all' against an empty catalog as []", () => {
    expect(resolveEnabledWorkflowIds("all", [])).toEqual([]);
  });
});
