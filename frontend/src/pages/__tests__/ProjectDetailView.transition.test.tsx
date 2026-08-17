// @vitest-environment jsdom
/**
 * Regression test: interview → workspace transition.
 *
 * REPRODUCTION PATH: SSE-driven — events are injected through the real
 * `useProjectSSE` hook via the SSE hub's `__dispatchForTest` seam (see
 * `frontend/src/lib/sseHub.ts`), the same way the app receives them: the
 * project's `/system/stream` subscription is live for the whole test (via
 * `<SSEManager />`, mounted alongside `ProjectDetailView` exactly as it is
 * in `AppShell`), so the listener bodies in `useProjectSSE.ts` run for real
 * instead of the test calling projectStore actions directly.
 *
 * Root cause confirmed by this test: applyProjectStateChange's background
 * getProject was overwriting activeProject with stale data from the server
 * (which might still report `interviewing` because the HTTP snapshot races the
 * SSE event). Without fix #1, the mock returning stale `interviewing` causes
 * the status to revert, and the component stays on the interview view.
 *
 * Fix #1 (projectStore): don't let the background getProject revert to a
 *   lower-rank status than what was set optimistically.
 * Fix #2 (projectStore): dedupe finalizeStreamingMessage double-fire.
 * Fix #3 (tasklistStore): memoize useCurrentAndArchivedTasklistsForScope.
 * Fix #4 (ProjectDetailView): ErrorBoundary prevents blank screen on throw.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { MemoryRouter, Routes, Route } from "react-router-dom";
import { ProjectDetailView } from "../ProjectDetailView";
import { SSEManager } from "../../components/SSEManager";
import { useProjectStore, _resetFinalizeTimestamps } from "../../stores/projectStore";
import { useChatStore } from "../../stores/chatStore";
import { useNetworkStore } from "../../stores/networkStore";
import { __dispatchForTest } from "../../lib/sseHub";

const PROJECT_ID = "proj-transition-test";
const PROJECT_KEY = `project:${PROJECT_ID}`;

const interviewingProject = {
  id: PROJECT_ID,
  name: "Regression Project",
  emoji: "🧪",
  goal: "Test the transition",
  spec: null,
  agent_id: "agent-1",
  working_dir: null,
  attachments: [] as string[],
  status: "interviewing" as const,
  summary: null,
  created_at: "2024-01-01T00:00:00Z",
  updated_at: "2024-01-01T00:00:00Z",
};

const projectListItem = {
  id: PROJECT_ID,
  name: "Regression Project",
  emoji: "🧪",
  status: "interviewing" as const,
  agent_id: "agent-1",
  created_at: "2024-01-01T00:00:00Z",
  updated_at: "2024-01-01T00:00:00Z",
};

// getProject returns stale `interviewing` — this is the race condition that
// triggers the bug. Fix #1 must prevent this from reverting the status.
vi.mock("../../lib/api", () => ({
  getProject: vi.fn().mockResolvedValue({
    id: "proj-transition-test",
    name: "Regression Project",
    emoji: "🧪",
    goal: "Test the transition",
    spec: null,
    agent_id: "agent-1",
    working_dir: null,
    attachments: [],
    status: "interviewing", // stale — would revert without fix #1
    summary: null,
    created_at: "2024-01-01T00:00:00Z",
    updated_at: "2024-01-01T00:00:00Z",
  }),
  getProjects: vi.fn().mockResolvedValue([]),
  getProjectMessages: vi.fn().mockResolvedValue({ messages: [], cursor: null }),
  listProjectTasklists: vi.fn().mockResolvedValue({ active: null, recent: [] }),
  listTasklistsForScope: vi.fn().mockResolvedValue({ active: null, recent: [] }),
  getAgents: vi.fn().mockResolvedValue([]),
  getAgent: vi.fn().mockRejectedValue(new Error("no agent")),
  systemStreamUrl: vi.fn().mockReturnValue("http://localhost/noop"),
  cancelProjectRun: vi.fn().mockResolvedValue(undefined),
  uploadProjectAttachment: vi.fn(),
  uploadProjectFolderReference: vi.fn(),
  deleteProjectAttachment: vi.fn(),
}));

// The hub lazily opens a real fetch-based connection on first subscription.
// Stub it out so mounting <SSEManager /> (and its useProjectSSE subscription)
// in jsdom never attempts a network call — events are injected directly via
// `__dispatchForTest`.
vi.mock("../../hooks/sseUtils", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../hooks/sseUtils")>();
  return {
    ...actual,
    createManagedEventSource: vi.fn(() => ({ close: vi.fn() })),
  };
});

// Stub heavy chat sub-components to avoid jsdom render issues
vi.mock("../../components/chat/MessageList", () => ({
  MessageList: () => null,
  parseSkillLoadInfo: () => null,
}));
vi.mock("../../components/chat/ChatInput", () => ({ ChatInput: () => null }));
vi.mock("../../components/chat/TypingIndicator", () => ({ TypingIndicator: () => null }));
vi.mock("../../components/chat/PendingFormOverlay", () => ({ PendingFormOverlay: () => null }));
vi.mock("../../components/shared/AgentOrgTree", () => ({ AgentOrgTree: () => null }));

// Stub ProjectWorkspace — we only care that we REACH it, not its internals
vi.mock("../../components/projects/ProjectWorkspace", () => ({
  ProjectWorkspace: () =>
    React.createElement("div", { "data-testid": "project-workspace" }, "Workspace"),
}));

describe("ProjectDetailView: interview → workspace transition", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;
  let errorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    // Reset all stores to a known-clean state
    useProjectStore.getState().reset();
    useProjectStore.setState({ projects: [], messageCache: new Map() });
    useChatStore.getState().reset();
    useNetworkStore.setState({ isInternetOnline: true, isServerOnline: true });
    // Reset module-level debounce state so tests are isolated
    _resetFinalizeTimestamps();
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
    vi.clearAllMocks();
    errorSpy.mockRestore();
  });

  function renderDetailView() {
    return act(async () => {
      root.render(
        React.createElement(
          React.Fragment,
          null,
          // Mirrors AppShell: SSEManager lives outside the routed view and
          // owns the actual hub subscription for the active project.
          React.createElement(SSEManager),
          React.createElement(
            MemoryRouter,
            { initialEntries: [`/projects/${PROJECT_ID}`] },
            React.createElement(
              Routes,
              null,
              React.createElement(Route, {
                path: "/projects/:projectId",
                element: React.createElement(ProjectDetailView),
              }),
            ),
          ),
        ),
      );
    });
  }

  // Injects a raw AgentEvent through the hub's test seam on the project's
  // channel — exactly what `useProjectSSE`'s listeners parse in production.
  function inject(eventName: string, data: Record<string, unknown> = {}) {
    return act(async () => {
      __dispatchForTest({
        agent_id: PROJECT_KEY,
        run_id: "run-1",
        thread_id: null,
        eventName,
        raw: JSON.stringify({
          agent_id: PROJECT_KEY,
          run_id: "run-1",
          payload: { type: eventName, data },
        }),
      });
    });
  }

  it("transitions from interview to workspace when SSE fires applyProjectStateChange", async () => {
    // Seed store: project is in interviewing state
    useProjectStore.setState({
      activeProjectId: PROJECT_ID,
      activeProject: { ...interviewingProject },
      projects: [projectListItem],
    });

    await renderDetailView();

    // Initially shows Interviewing pill
    expect(container.textContent).toContain("Interviewing");

    // SSE: project.state_changed → applyProjectStateChange (fix #1 under test)
    // The background getProject mock returns stale "interviewing". Without fix #1
    // this would revert the status and keep the component on the interview view.
    await inject("project.state_changed", { status: "active" });
    // Flush microtasks so the background getProject promise resolves
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });

    // Status pill must be "Active" — the stale background fetch must NOT have reverted it
    expect(container.textContent).toContain("Active");
    expect(container.textContent).not.toContain("Interviewing");

    // ProjectWorkspace stub should be mounted
    expect(container.querySelector("[data-testid='project-workspace']")).not.toBeNull();
  });

  it("deduplicates finalizeStreamingMessage double-fire (text_complete + run_ended)", async () => {
    const { getProjectMessages } = await import("../../lib/api");

    useProjectStore.setState({
      activeProjectId: PROJECT_ID,
      activeProject: { ...interviewingProject, status: "active" },
      projects: [{ ...projectListItem, status: "active" }],
    });

    await renderDetailView();
    // Flush effects from initial load (selectProject also calls getProjectMessages)
    await act(async () => { await new Promise((r) => setTimeout(r, 0)); });
    // Reset call count so we only measure the finalizeStreamingMessage calls below
    vi.mocked(getProjectMessages).mockClear();

    // Both text_complete and run_ended fire finalizeStreamingMessage (see
    // useProjectSSE.ts) — the real dual-fire scenario fix #2 guards against.
    // Fix #2: only the first call within FINALIZE_DEBOUNCE_MS should trigger the refetch.
    await inject("text_complete", { text: "" });
    await inject("run_ended", { reason: "Completed" });
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });

    // getProjectMessages should have been called exactly once (dedupe working)
    expect(vi.mocked(getProjectMessages).mock.calls.length).toBe(1);
  });

  it("does not log Maximum update depth exceeded during transition", async () => {
    useProjectStore.setState({
      activeProjectId: PROJECT_ID,
      activeProject: { ...interviewingProject },
      projects: [projectListItem],
    });

    await renderDetailView();

    await inject("project.state_changed", { status: "active" });
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });

    await inject("text_complete", { text: "" });
    await inject("run_ended", { reason: "Completed" });
    await act(async () => {
      await useProjectStore.getState().fetchProjectTasklists(PROJECT_ID);
      await new Promise((r) => setTimeout(r, 0));
    });

    const hasMaxDepthError = errorSpy.mock.calls.some(
      (args: unknown[]) =>
        typeof args[0] === "string" && args[0].includes("Maximum update depth exceeded"),
    );
    expect(hasMaxDepthError).toBe(false);
  });
});
