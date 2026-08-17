// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { MemoryRouter } from "react-router-dom";
import type { Assignment, AgentSnapshot, AssignmentWatchHealth, WatchContract } from "../../../types/api";

const mockCreateAssignment = vi.fn();
const mockUpdateAssignment = vi.fn();
const mockDeleteAssignment = vi.fn();
const mockListAssignments = vi.fn();
const mockGetAssignment = vi.fn();
const mockListMcpServers = vi.fn();
const mockGetAppConfig = vi.fn();
const mockGetWebhookRouteSecretStatus = vi.fn();
const mockSetWebhookRouteSecret = vi.fn();
const mockTestWebhookRoute = vi.fn();
const mockRefineTemplate = vi.fn();
const mockTriggerAssignment = vi.fn();
const mockListAssignmentRuns = vi.fn();
const mockGetMessages = vi.fn();

vi.mock("../../../lib/api", () => ({
  BASE_URL: "http://localhost:3001",
  createAssignment: (...a: unknown[]) => mockCreateAssignment(...a),
  updateAssignment: (...a: unknown[]) => mockUpdateAssignment(...a),
  deleteAssignment: (...a: unknown[]) => mockDeleteAssignment(...a),
  listAssignments: (...a: unknown[]) => mockListAssignments(...a),
  getAssignment: (...a: unknown[]) => mockGetAssignment(...a),
  listMcpServers: (...a: unknown[]) => mockListMcpServers(...a),
  getAppConfig: (...a: unknown[]) => mockGetAppConfig(...a),
  getWebhookRouteSecretStatus: (...a: unknown[]) => mockGetWebhookRouteSecretStatus(...a),
  setWebhookRouteSecret: (...a: unknown[]) => mockSetWebhookRouteSecret(...a),
  testWebhookRoute: (...a: unknown[]) => mockTestWebhookRoute(...a),
  refineTemplate: (...a: unknown[]) => mockRefineTemplate(...a),
  triggerAssignment: (...a: unknown[]) => mockTriggerAssignment(...a),
  listAssignmentRuns: (...a: unknown[]) => mockListAssignmentRuns(...a),
  getAgent: vi.fn().mockResolvedValue(null),
  getMessages: (...a: unknown[]) => mockGetMessages(...a),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: vi.fn().mockResolvedValue(null),
}));

import { open as mockTauriOpen } from "@tauri-apps/plugin-dialog";
import { AssignmentEditorModal } from "../AssignmentEditorModal";
import { useAssignmentEditorModalStore } from "../../../stores/assignmentEditorModalStore";
import { useChatStore } from "../../../stores/chatStore";

const AGENT_ID = "agent-xyz";
const OTHER_AGENT_ID = "agent-other";

const AGENTS: AgentSnapshot[] = [
  {
    agent_id: AGENT_ID,
    name: "Assistant",
    last_activity_at: null,
    message_count: 0,
    has_active_run: false,
    queue_depth: 0,
    thread_id: null,
    created_at: "2026-01-01T00:00:00Z",
  },
  {
    agent_id: OTHER_AGENT_ID,
    name: "Researcher",
    last_activity_at: null,
    message_count: 0,
    has_active_run: false,
    queue_depth: 0,
    thread_id: null,
    created_at: "2026-01-01T00:00:00Z",
  },
];

const existingAssignment: Assignment = {
  id: "asg-1",
  agent_id: AGENT_ID,
  name: "Morning digest",
  instruction: "Summarize overnight events.",
  working_directory: null,
  trigger: { type: "Cron", cron_expr: "0 9 * * *", is_recurring: true },
  bindings: [{ kind: "mcp_server", ref_id: "github" }],
  output_mode: "background",
  thread_policy: "dedicated",
  enabled: true,
  expires_at: null,
  created_ts: "2026-06-28T00:00:00Z",
  updated_ts: "2026-06-28T00:00:00Z",
};

const otherAssignment: Assignment = {
  ...existingAssignment,
  id: "asg-2",
  name: "Evening wrap-up",
  next_fire_at: "2026-07-20T21:00:00Z",
};

describe("AssignmentEditorModal", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    useAssignmentEditorModalStore.getState().close();
    // The modal now reads/writes assignments through chatStore's
    // `assignmentsByAgent` cache (left-side tiles panel + create/update/
    // delete) — reset it and seed a stable agent list each test so the
    // in-modal owner picker and left panel start from a known state.
    useChatStore.setState({ agents: AGENTS, assignmentsByAgent: new Map() });
    vi.clearAllMocks();
    mockListAssignments.mockResolvedValue([]);
    mockListMcpServers.mockResolvedValue([]);
    mockGetAppConfig.mockResolvedValue({ min_agent_watch_poll_interval_secs: 900 });
    mockGetWebhookRouteSecretStatus.mockResolvedValue({ configured: false });
    mockSetWebhookRouteSecret.mockResolvedValue(undefined);
    mockGetMessages.mockResolvedValue({ messages: [], cursor: null });
    mockListAssignmentRuns.mockResolvedValue([]);
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
    useAssignmentEditorModalStore.getState().close();
  });

  async function render(onSaved?: () => void) {
    await act(async () => {
      root.render(
        React.createElement(MemoryRouter, null, React.createElement(AssignmentEditorModal, { onSaved })),
      );
    });
  }

  function q(sel: string) {
    return container.querySelector(sel) as HTMLElement | null;
  }

  function qa(sel: string) {
    return Array.from(container.querySelectorAll(sel)) as HTMLElement[];
  }

  function setInputValue(input: HTMLInputElement | HTMLTextAreaElement, value: string) {
    const proto =
      input instanceof HTMLTextAreaElement
        ? window.HTMLTextAreaElement.prototype
        : window.HTMLInputElement.prototype;
    const nativeSetter = Object.getOwnPropertyDescriptor(proto, "value")!.set!;
    nativeSetter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  }

  function click(el: HTMLElement) {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  }

  function selectValue(select: HTMLSelectElement, value: string) {
    const nativeSetter = Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, "value")!.set!;
    nativeSetter.call(select, value);
    select.dispatchEvent(new Event("change", { bubbles: true }));
  }

  it("renders nothing when the store is empty", async () => {
    await render();
    expect(q("[data-testid='assignment-editor-modal']")).toBeNull();
  });

  it("creates a one-shot Cron assignment seeded from a calendar date, defaulting to 9am local", async () => {
    const onSaved = vi.fn();
    mockCreateAssignment.mockResolvedValue({ ...existingAssignment, id: "asg-new" });
    await render(onSaved);

    // seedCronDate is always a bare "YYYY-MM-DD" from the calendar's "+"
    // entry point (ScheduledCalendar's onNewForDate) — not a full datetime.
    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID, "2026-08-01");
    });
    // Let the left panel's listAssignments() promise resolve and re-render.
    await act(async () => {});

    expect(q("[data-testid='assignment-editor-modal']")).toBeTruthy();

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-name-input']") as HTMLInputElement, "Daily digest");
      setInputValue(
        q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement,
        "Summarize overnight events.",
      );
    });

    await act(async () => {
      click(q("[data-testid='assignment-editor-submit']")!);
    });

    expect(mockCreateAssignment).toHaveBeenCalledTimes(1);
    const [agentId, req] = mockCreateAssignment.mock.calls[0];
    expect(agentId).toBe(AGENT_ID);
    expect(req).toMatchObject({
      name: "Daily digest",
      instruction: "Summarize overnight events.",
      working_directory: null,
      trigger: { type: "Cron", cron_expr: "0 9 1 8 *", is_recurring: false },
      bindings: [],
      thread_policy: "fresh",
      enabled: true,
      expires_at: null,
    });

    expect(onSaved).toHaveBeenCalledTimes(1);
    expect(q("[data-testid='assignment-editor-modal']")).toBeNull();
  });

  it("prefills from the existing assignment in edit mode and patches on submit", async () => {
    mockGetAssignment.mockResolvedValue(existingAssignment);
    mockUpdateAssignment.mockResolvedValue(existingAssignment);
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openEdit(AGENT_ID, existingAssignment.id);
    });
    await act(async () => {});

    expect(mockGetAssignment).toHaveBeenCalledWith(existingAssignment.id);
    expect((q("[data-testid='assignment-editor-name-input']") as HTMLInputElement).value).toBe("Morning digest");

    const recurringInput = q("[data-testid='assignment-editor-recurring-input']") as HTMLInputElement;
    expect(recurringInput).toBeTruthy();
    expect(recurringInput.checked).toBe(true);

    // "Enabled" is edit-mode-only chrome, restyled as a switch but still a
    // real checkbox underneath.
    const enabledInput = q("[data-testid='assignment-editor-enabled-input']") as HTMLInputElement;
    expect(enabledInput).toBeTruthy();
    expect(enabledInput.checked).toBe(true);

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-name-input']") as HTMLInputElement, "Morning digest v2");
    });

    await act(async () => {
      click(q("[data-testid='assignment-editor-submit']")!);
    });

    expect(mockUpdateAssignment).toHaveBeenCalledTimes(1);
    const [assignmentId, patch] = mockUpdateAssignment.mock.calls[0];
    expect(assignmentId).toBe(existingAssignment.id);
    expect(patch).toMatchObject({
      name: "Morning digest v2",
      trigger: { type: "Cron", cron_expr: "0 9 * * *", is_recurring: true },
      thread_policy: "dedicated",
      bindings: [],
    });
  });

  it("does not show the Enabled switch when creating a brand new assignment", async () => {
    await render();
    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    expect(q("[data-testid='assignment-editor-enabled-input']")).toBeNull();
  });

  it("refines the Instruction from the single top Refine button, mode-aware per trigger tab, and accepts the suggestion", async () => {
    mockRefineTemplate.mockResolvedValue({ refined_template: "A clearer instruction." });
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    await act(async () => {
      setInputValue(
        q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement,
        "draft instruction",
      );
    });

    // Defaults to the Cron tab — no {dot.path} placeholders in scope.
    await act(async () => {
      click(q("[data-testid='assignment-editor-instruction-refine']")!);
    });
    await act(async () => {});

    expect(mockRefineTemplate).toHaveBeenCalledWith({
      agent_id: AGENT_ID,
      prompt_template: "draft instruction",
      mode: "cron",
    });
    expect(q("[data-testid='assignment-editor-instruction-refine-suggestion']")?.textContent).toContain(
      "A clearer instruction.",
    );

    await act(async () => {
      click(q("[data-testid='assignment-editor-instruction-refine-accept']")!);
    });
    expect((q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement).value).toBe(
      "A clearer instruction.",
    );
    expect(q("[data-testid='assignment-editor-instruction-refine-suggestion']")).toBeNull();

    // Switching tabs both clears the stale suggestion and maps to the
    // connector tab's distinct wire mode name (`poll_connector`).
    await act(async () => {
      click(q("[data-testid='assignment-editor-trigger-connector_event-tab']")!);
    });
    expect(q("[data-testid='assignment-editor-instruction-refine-suggestion']")).toBeNull();

    await act(async () => {
      click(q("[data-testid='assignment-editor-instruction-refine']")!);
    });
    await act(async () => {});

    expect(mockRefineTemplate).toHaveBeenLastCalledWith({
      agent_id: AGENT_ID,
      prompt_template: "A clearer instruction.",
      mode: "poll_connector",
    });
  });

  it("lists the agent's other assignments in the left panel and switches to one on click", async () => {
    mockListAssignments.mockResolvedValue([existingAssignment, otherAssignment]);
    mockGetAssignment.mockImplementation((id: string) =>
      Promise.resolve(id === otherAssignment.id ? otherAssignment : existingAssignment),
    );
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openEdit(AGENT_ID, existingAssignment.id);
    });
    await act(async () => {});

    expect(mockListAssignments).toHaveBeenCalledWith(AGENT_ID);
    const tiles = qa("[data-testid='assignment-editor-tile']");
    expect(tiles).toHaveLength(2);

    const otherTile = tiles.find((t) => t.textContent?.includes("Evening wrap-up"));
    expect(otherTile).toBeTruthy();

    await act(async () => {
      click(otherTile!);
    });
    await act(async () => {});

    expect(mockGetAssignment).toHaveBeenCalledWith(otherAssignment.id);
    expect((q("[data-testid='assignment-editor-name-input']") as HTMLInputElement).value).toBe("Evening wrap-up");
  });

  it("shows an agent picker when opened with no agent, and unblocks the form once one is picked", async () => {
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate();
    });
    await act(async () => {});

    const agentSelect = q("[data-testid='assignment-editor-agent-select']") as HTMLSelectElement;
    expect(agentSelect).toBeTruthy();
    // Submitting is blocked (and the rest of the form disabled) until an
    // agent is chosen.
    expect((q("[data-testid='assignment-editor-submit']") as HTMLButtonElement).disabled).toBe(true);

    // Pick the agent first — the rest of the form is disabled (via a
    // `<fieldset disabled>`) until an owner is chosen, mirroring the
    // pre-convergence modal.
    await act(async () => {
      selectValue(agentSelect, OTHER_AGENT_ID);
    });

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-name-input']") as HTMLInputElement, "Weekly review");
      setInputValue(
        q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement,
        "Summarize the week.",
      );
      // Webhook has no required sub-fields — sidesteps interacting with the
      // full CronPicker widget, which isn't what this test is about.
      click(q("[data-testid='assignment-editor-trigger-webhook-tab']")!);
    });

    mockCreateAssignment.mockResolvedValue({ ...existingAssignment, agent_id: OTHER_AGENT_ID, id: "asg-new" });

    await act(async () => {
      click(q("[data-testid='assignment-editor-submit']")!);
    });

    expect(mockCreateAssignment).toHaveBeenCalledTimes(1);
    expect(mockCreateAssignment.mock.calls[0][0]).toBe(OTHER_AGENT_ID);
  });

  it("deletes the assignment being edited", async () => {
    mockGetAssignment.mockResolvedValue(existingAssignment);
    mockDeleteAssignment.mockResolvedValue(undefined);
    const onSaved = vi.fn();
    await render(onSaved);

    await act(async () => {
      useAssignmentEditorModalStore.getState().openEdit(AGENT_ID, existingAssignment.id);
    });
    await act(async () => {});

    await act(async () => {
      click(q("[data-testid='assignment-editor-delete']")!);
    });

    expect(mockDeleteAssignment).toHaveBeenCalledWith(existingAssignment.id);
    expect(onSaved).toHaveBeenCalledTimes(1);
    expect(q("[data-testid='assignment-editor-modal']")).toBeNull();
  });

  it("Run now triggers the assignment, opens its run's thread, and closes the modal", async () => {
    mockGetAssignment.mockResolvedValue(existingAssignment);
    mockTriggerAssignment.mockResolvedValue({
      id: "run-1",
      assignment_id: existingAssignment.id,
      agent_id: AGENT_ID,
      trigger_kind: "manual",
      status: "queued",
      thread_id: "thread-from-run",
      queued_at: "2026-07-25T00:00:00Z",
    });
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openEdit(AGENT_ID, existingAssignment.id);
    });
    await act(async () => {});

    await act(async () => {
      click(q("[data-testid='assignment-editor-run-now']")!);
    });
    await act(async () => {});

    expect(mockTriggerAssignment).toHaveBeenCalledWith(existingAssignment.id, undefined);
    // switchToThread (lib/threadNavigation.ts) is the mechanism that actually
    // opens the run's thread — it fetches that thread's messages directly.
    // `selectAgent` passes its per-selection AbortController's signal through
    // as the 3rd argument (see chatStore.ts).
    expect(mockGetMessages).toHaveBeenCalledWith(AGENT_ID, "thread-from-run", expect.any(AbortSignal));
    expect(q("[data-testid='assignment-editor-modal']")).toBeNull();
  });

  it("disables Run now for a disabled assignment", async () => {
    mockGetAssignment.mockResolvedValue({ ...existingAssignment, enabled: false });
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openEdit(AGENT_ID, existingAssignment.id);
    });
    await act(async () => {});

    const runNowButton = q("[data-testid='assignment-editor-run-now']") as HTMLButtonElement;
    expect(runNowButton.disabled).toBe(true);
  });

  it("does not show Run now when creating a brand new assignment", async () => {
    await render();
    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});
    expect(q("[data-testid='assignment-editor-run-now']")).toBeNull();
  });

  it("fills the working directory from the native folder picker", async () => {
    vi.mocked(mockTauriOpen).mockResolvedValue("/home/dev/project");
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    await act(async () => {
      click(q("[data-testid='assignment-editor-browse-working-directory']")!);
    });
    await act(async () => {});

    expect((q("[data-testid='assignment-editor-working-directory-input']") as HTMLInputElement).value).toBe(
      "/home/dev/project",
    );
  });

  it("creates an AgentWatch assignment from the shared instruction and cadence, with no raw tool/args/cursor fields", async () => {
    const onSaved = vi.fn();
    mockCreateAssignment.mockResolvedValue({ ...existingAssignment, id: "asg-new" });
    await render(onSaved);

    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-name-input']") as HTMLInputElement, "Watch GitHub issues");
      setInputValue(
        q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement,
        "When a new open GitHub issue appears, summarize it.",
      );
    });

    await act(async () => {
      click(q("[data-testid='assignment-editor-trigger-connector_event-tab']")!);
    });

    // Full removal, not a hidden "Advanced" disclosure — none of the old raw
    // server/tool/arguments/cursor fields exist on this tab anymore.
    expect(q("[data-testid='assignment-editor-connector-server-select']")).toBeNull();
    expect(q("[data-testid='assignment-editor-connector-tool-input']")).toBeNull();
    expect(q("[data-testid='assignment-editor-connector-arguments-input']")).toBeNull();
    expect(q("[data-testid='assignment-editor-connector-cursor-path-input']")).toBeNull();

    // Default poll interval (900s / 15 min, the AgentWatch floor) is already
    // valid; submit should be enabled. Connector defaults to "All connectors"
    // (connector_scope omitted/null).
    const submitBtn = q("[data-testid='assignment-editor-submit']") as HTMLButtonElement;
    expect(submitBtn.disabled).toBe(false);

    await act(async () => {
      click(submitBtn);
    });

    expect(mockCreateAssignment).toHaveBeenCalledTimes(1);
    const [, req] = mockCreateAssignment.mock.calls[0];
    expect(req).toMatchObject({
      name: "Watch GitHub issues",
      instruction: "When a new open GitHub issue appears, summarize it.",
      trigger: {
        type: "AgentWatch",
        instruction: "When a new open GitHub issue appears, summarize it.",
        poll_interval_secs: 900,
        connector_scope: null,
      },
    });
    expect(onSaved).toHaveBeenCalledTimes(1);
  });

  it("rejects an AgentWatch poll interval below the 900s (15 min) floor with inline validation", async () => {
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-name-input']") as HTMLInputElement, "Too frequent");
      setInputValue(
        q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement,
        "Watch for the thing.",
      );
      click(q("[data-testid='assignment-editor-trigger-connector_event-tab']")!);
    });

    await act(async () => {
      setInputValue(
        q("[data-testid='assignment-editor-connector-poll-interval-input']") as HTMLInputElement,
        "300",
      );
    });

    expect(q("[data-testid='assignment-editor-connector-poll-interval-error']")?.textContent).toMatch(
      /at least 900 seconds \(15 minutes\)/,
    );
    expect((q("[data-testid='assignment-editor-submit']") as HTMLButtonElement).disabled).toBe(true);
  });

  it("accepts a poll interval below the shipped default when /system/config serves a lower floor", async () => {
    const onSaved = vi.fn();
    mockGetAppConfig.mockResolvedValue({ min_agent_watch_poll_interval_secs: 60 });
    mockCreateAssignment.mockResolvedValue({ ...existingAssignment, id: "asg-new" });
    await render(onSaved);

    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-name-input']") as HTMLInputElement, "Fast demo watch");
      setInputValue(
        q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement,
        "Watch for the demo event.",
      );
      click(q("[data-testid='assignment-editor-trigger-connector_event-tab']")!);
    });

    await act(async () => {
      setInputValue(
        q("[data-testid='assignment-editor-connector-poll-interval-input']") as HTMLInputElement,
        "60",
      );
    });

    expect(q("[data-testid='assignment-editor-connector-poll-interval-error']")).toBeNull();
    const submitBtn = q("[data-testid='assignment-editor-submit']") as HTMLButtonElement;
    expect(submitBtn.disabled).toBe(false);

    await act(async () => {
      click(submitBtn);
    });

    expect(mockCreateAssignment).toHaveBeenCalledTimes(1);
    const [, req] = mockCreateAssignment.mock.calls[0];
    expect(req).toMatchObject({
      trigger: {
        type: "AgentWatch",
        poll_interval_secs: 60,
      },
    });
    expect(onSaved).toHaveBeenCalledTimes(1);
  });

  it("fails closed to the shipped 900s floor when /system/config's response is missing the field, rejecting 60s", async () => {
    mockGetAppConfig.mockResolvedValue({} as unknown as { min_agent_watch_poll_interval_secs: number });
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-name-input']") as HTMLInputElement, "Too frequent");
      setInputValue(
        q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement,
        "Watch for the thing.",
      );
      click(q("[data-testid='assignment-editor-trigger-connector_event-tab']")!);
    });

    await act(async () => {
      setInputValue(
        q("[data-testid='assignment-editor-connector-poll-interval-input']") as HTMLInputElement,
        "60",
      );
    });

    expect(q("[data-testid='assignment-editor-connector-poll-interval-error']")?.textContent).toMatch(
      /at least 900 seconds \(15 minutes\)/,
    );
    expect((q("[data-testid='assignment-editor-submit']") as HTMLButtonElement).disabled).toBe(true);
  });

  it("interpolates the served floor into the validation error instead of a hardcoded 900s/15 minutes", async () => {
    mockGetAppConfig.mockResolvedValue({ min_agent_watch_poll_interval_secs: 120 });
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-name-input']") as HTMLInputElement, "Too frequent");
      setInputValue(
        q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement,
        "Watch for the thing.",
      );
      click(q("[data-testid='assignment-editor-trigger-connector_event-tab']")!);
    });

    await act(async () => {
      setInputValue(
        q("[data-testid='assignment-editor-connector-poll-interval-input']") as HTMLInputElement,
        "60",
      );
    });

    const errorText = q("[data-testid='assignment-editor-connector-poll-interval-error']")?.textContent;
    expect(errorText).toBe("Poll interval must be at least 120 seconds (2 minutes).");
    expect(errorText).not.toMatch(/900|15 minutes/);
  });

  it("populates the connector dropdown from connected MCP servers only, and scopes the watch to the selected one", async () => {
    const onSaved = vi.fn();
    mockListMcpServers.mockResolvedValue([
      { name: "github", transport: "stdio", endpoint: "", state: "connected", tool_names: [], source: "user" },
      { name: "notion", transport: "http", endpoint: "", state: "connected", tool_names: [], source: "user" },
      { name: "broken", transport: "http", endpoint: "", state: "error", tool_names: [], source: "user" },
    ]);
    mockCreateAssignment.mockResolvedValue({ ...existingAssignment, id: "asg-new" });
    await render(onSaved);

    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-name-input']") as HTMLInputElement, "Watch Notion");
      setInputValue(
        q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement,
        "When a new Notion page is created, summarize it.",
      );
      click(q("[data-testid='assignment-editor-trigger-connector_event-tab']")!);
    });
    await act(async () => {});

    const select = q("[data-testid='assignment-editor-connector-scope-select']") as HTMLSelectElement;
    const optionValues = Array.from(select.options).map((o) => o.value);
    // The unauthorized/"error"-state connector never appears — only
    // `state === "connected"` servers, plus the always-present "All
    // connectors" option (empty value).
    expect(optionValues).toEqual(["", "github", "notion"]);

    await act(async () => {
      selectValue(select, "notion");
    });

    await act(async () => {
      click(q("[data-testid='assignment-editor-submit']")!);
    });

    expect(mockCreateAssignment).toHaveBeenCalledTimes(1);
    const [, req] = mockCreateAssignment.mock.calls[0];
    expect(req).toMatchObject({
      trigger: {
        type: "AgentWatch",
        connector_scope: "notion",
      },
    });
  });

  it("prefills an AgentWatch assignment in edit mode, including its connector scope, and patches on submit", async () => {
    mockListMcpServers.mockResolvedValue([
      { name: "gmail", transport: "http", endpoint: "", state: "connected", tool_names: [], source: "user" },
    ]);
    const agentWatchAssignment: Assignment = {
      ...existingAssignment,
      id: "asg-watch",
      trigger: {
        type: "AgentWatch",
        instruction: "Watch my inbox for a new email from finance and summarize it.",
        poll_interval_secs: 1200,
        connector_scope: "gmail",
      },
    };
    mockGetAssignment.mockResolvedValue(agentWatchAssignment);
    mockUpdateAssignment.mockResolvedValue(agentWatchAssignment);
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openEdit(AGENT_ID, agentWatchAssignment.id);
    });
    await act(async () => {});

    expect(
      (q("[data-testid='assignment-editor-trigger-connector_event-tab']") as HTMLButtonElement).className,
    ).toContain("bg-[var(--modal-accent)]");
    // The trigger's own `instruction` (what the detect loop actually
    // evaluates) hydrates the shared top Instruction field — same precedent
    // as Webhook's `prompt_template`.
    expect(
      (q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement).value,
    ).toBe("Watch my inbox for a new email from finance and summarize it.");
    expect(
      (q("[data-testid='assignment-editor-connector-poll-interval-input']") as HTMLInputElement).value,
    ).toBe("1200");
    expect(
      (q("[data-testid='assignment-editor-connector-scope-select']") as HTMLSelectElement).value,
    ).toBe("gmail");

    await act(async () => {
      click(q("[data-testid='assignment-editor-submit']")!);
    });

    expect(mockUpdateAssignment).toHaveBeenCalledTimes(1);
    const [, patch] = mockUpdateAssignment.mock.calls[0];
    expect(patch).toMatchObject({
      trigger: {
        type: "AgentWatch",
        instruction: "Watch my inbox for a new email from finance and summarize it.",
        poll_interval_secs: 1200,
        connector_scope: "gmail",
      },
    });
  });

  it("refreshes a live AgentWatch's model-call count while the modal stays open, instead of freezing at whatever it first loaded", async () => {
    // Regression: the Watch tab's cost figure (`model_calls_today`) is
    // fetched once when the modal opens and never again — a live watch's
    // background polling keeps advancing that count on the server the
    // whole time the modal sits open, so without a periodic refetch the
    // panel silently under-reports an already-accrued cost (observed in
    // production as "UI showed 3, persisted scratchpad had 5").
    vi.useFakeTimers();
    try {
      mockListMcpServers.mockResolvedValue([]);
      const contract: WatchContract = {
        contract_version: 1,
        authored_at: "2026-07-30T00:00:00Z",
        authored_by_run: "run-1",
        source: { kind: "notion_database", ref: "db-1" },
        identity: { strategy: "native_id", source_field: "id", format: null, fields: [], rationale: "Stable id." },
        change: { material_fields: ["status"], version_hint_field: null },
        predicate: { natural_language: "status changes", fields: ["status"], predicate: {} },
        mode: "predicate_transition",
        fields: {},
      };
      const baseHealth: AssignmentWatchHealth = {
        has_evaluated: true,
        tier: null,
        extraction_path: "llm",
        degraded_reason: null,
        extraction_plan_degraded: false,
        extraction_plan_degraded_reason: null,
        extraction_health: "model_assisted",
        extraction_health_reason: "No deterministic extraction plan is bound for this watch yet, so a model reads its output on every poll.",
        model_calls_today: 3,
        model_calls_by_day: { "2026-07-30": 3 },
        last_extraction_path: "llm",
        contract_status: { state: "bound", bound_after_repairs: null },
      };
      const agentWatchAssignment: Assignment = {
        ...existingAssignment,
        id: "asg-watch-live",
        trigger: {
          type: "AgentWatch",
          instruction: "Watch the shared doc for new comments.",
          poll_interval_secs: 1200,
          connector_scope: null,
          contract,
        },
        watch_health: baseHealth,
      };
      const refreshedAssignment: Assignment = {
        ...agentWatchAssignment,
        watch_health: { ...baseHealth, model_calls_today: 5, model_calls_by_day: { "2026-07-30": 5 } },
      };
      mockGetAssignment.mockResolvedValueOnce(agentWatchAssignment).mockResolvedValue(refreshedAssignment);

      await render();
      await act(async () => {
        useAssignmentEditorModalStore.getState().openEdit(AGENT_ID, agentWatchAssignment.id);
      });
      await act(async () => {});

      expect(q("[data-testid='watch-model-assisted-disclosure']")?.textContent).toContain("3 model calls today");

      await act(async () => {
        await vi.advanceTimersByTimeAsync(5000);
      });

      expect(mockGetAssignment).toHaveBeenCalledTimes(2);
      expect(q("[data-testid='watch-model-assisted-disclosure']")?.textContent).toContain("5 model calls today");
    } finally {
      vi.useRealTimers();
    }
  });

  it("loads a legacy ConnectorEvent assignment without crashing, carries its server_name into connector_scope, and upgrades it to AgentWatch on save", async () => {
    // Pre-redesign rows may still carry the raw server/tool/arguments/cursor
    // trigger shape — the backend keeps `ConnectorEvent` intact (still a
    // real, running poll loop), but this editor no longer has fields for
    // those raw specifics. Loading one must not crash, and saving it should
    // upgrade the trigger to `AgentWatch` like every other row on this tab,
    // carrying the legacy `server_name` into `connector_scope` so the
    // upgrade doesn't silently widen the watch to every connector.
    mockListMcpServers.mockResolvedValue([
      { name: "github", transport: "stdio", endpoint: "", state: "connected", tool_names: [], source: "user" },
    ]);
    const legacyAssignment: Assignment = {
      ...existingAssignment,
      id: "asg-legacy-connector",
      instruction: "Summarize new issues.",
      trigger: {
        type: "ConnectorEvent",
        server_name: "github",
        poll: { tool_name: "search_issues", arguments: { query: "is:open" }, cursor_path: "content.0.text" },
        poll_interval_secs: 1800,
      },
    };
    mockGetAssignment.mockResolvedValue(legacyAssignment);
    mockUpdateAssignment.mockResolvedValue(legacyAssignment);
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openEdit(AGENT_ID, legacyAssignment.id);
    });
    await act(async () => {});

    expect(q("[data-testid='assignment-editor-modal']")).toBeTruthy();
    expect(
      (q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement).value,
    ).toBe("Summarize new issues.");
    expect(
      (q("[data-testid='assignment-editor-connector-poll-interval-input']") as HTMLInputElement).value,
    ).toBe("1800");
    expect(
      (q("[data-testid='assignment-editor-connector-scope-select']") as HTMLSelectElement).value,
    ).toBe("github");

    await act(async () => {
      click(q("[data-testid='assignment-editor-submit']")!);
    });

    expect(mockUpdateAssignment).toHaveBeenCalledTimes(1);
    const [, patch] = mockUpdateAssignment.mock.calls[0];
    expect(patch).toMatchObject({
      trigger: {
        type: "AgentWatch",
        instruction: "Summarize new issues.",
        poll_interval_secs: 1800,
        connector_scope: "github",
      },
    });
  });

  it("creates a Webhook assignment with route name, generated secret, events, a filter row, and the unified instruction as its prompt template", async () => {
    const onSaved = vi.fn();
    mockCreateAssignment.mockResolvedValue({ ...existingAssignment, id: "asg-webhook-new" });
    await render(onSaved);

    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-name-input']") as HTMLInputElement, "PR reviewer");
      // No separate "Prompt template" field exists anymore — the single top
      // Instruction textarea (with its {dot.path} placeholder) IS what's
      // rendered as the webhook route's prompt_template.
      setInputValue(
        q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement,
        "Review PR #{pull_request.number}",
      );
      click(q("[data-testid='assignment-editor-trigger-webhook-tab']")!);
    });

    await act(async () => {
      setInputValue(
        q("[data-testid='assignment-editor-webhook-route-name-input']") as HTMLInputElement,
        "github-prs",
      );
    });

    await act(async () => {
      click(q("[data-testid='assignment-editor-webhook-secret-generate']")!);
    });
    const secretInput = q("[data-testid='assignment-editor-webhook-secret-input']") as HTMLInputElement;
    expect(secretInput).toBeTruthy();
    expect(secretInput.value).toMatch(/^[0-9a-f]{64}$/);

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-webhook-event-input']") as HTMLInputElement, "pull_request");
      click(q("[data-testid='assignment-editor-webhook-event-add']")!);
    });
    expect(q("[data-testid='assignment-editor-webhook-event-chip-pull_request']")).toBeTruthy();

    await act(async () => {
      click(q("[data-testid='assignment-editor-webhook-advanced-toggle']")!);
    });
    await act(async () => {
      click(q("[data-testid='assignment-editor-webhook-filter-add']")!);
    });
    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-webhook-filter-field']") as HTMLInputElement, "action");
      setInputValue(q("[data-testid='assignment-editor-webhook-filter-value']") as HTMLInputElement, "opened");
    });

    await act(async () => {
      click(q("[data-testid='assignment-editor-submit']")!);
    });

    expect(mockCreateAssignment).toHaveBeenCalledTimes(1);
    const [, req] = mockCreateAssignment.mock.calls[0];
    expect(req).toMatchObject({
      name: "PR reviewer",
      instruction: "Review PR #{pull_request.number}",
      trigger: {
        type: "Webhook",
        token: null,
        route_name: "github-prs",
        secret_ref: "github-prs",
        events: ["pull_request"],
        filters: { field: "action", op: "equals", value: "opened" },
        prompt_template: "Review PR #{pull_request.number}",
        deliver: { type: "agent" },
      },
    });

    expect(mockSetWebhookRouteSecret).toHaveBeenCalledTimes(1);
    const [routeName, secret] = mockSetWebhookRouteSecret.mock.calls[0];
    expect(routeName).toBe("github-prs");
    expect(secret).toBe(secretInput.value);

    expect(onSaved).toHaveBeenCalledTimes(1);
    expect(q("[data-testid='assignment-editor-modal']")).toBeNull();
  });

  it("prefills a Webhook assignment (route/events/filters/prompt) in edit mode, forces deliver back to agent, and patches without resending an unchanged secret", async () => {
    const webhookAssignment: Assignment = {
      ...existingAssignment,
      id: "asg-webhook-existing",
      trigger: {
        type: "Webhook",
        token: null,
        route_name: "github-prs",
        secret_ref: "github-prs",
        events: ["pull_request"],
        filters: { field: "action", op: "equals", value: "opened" },
        prompt_template: "Review PR #{pull_request.number}",
        // Pre-existing rows may still carry a non-agent deliver target from
        // before the picker was removed — saving through this editor must
        // normalize it back to the only delivery target it now offers.
        deliver: { type: "github_comment" },
      },
    };
    mockGetAssignment.mockResolvedValue(webhookAssignment);
    mockUpdateAssignment.mockResolvedValue(webhookAssignment);
    mockGetWebhookRouteSecretStatus.mockResolvedValueOnce({ configured: true });
    await render();

    await act(async () => {
      useAssignmentEditorModalStore.getState().openEdit(AGENT_ID, webhookAssignment.id);
    });
    await act(async () => {});

    expect(
      (q("[data-testid='assignment-editor-trigger-webhook-tab']") as HTMLButtonElement).className,
    ).toContain("bg-[var(--modal-accent)]");
    expect((q("[data-testid='assignment-editor-webhook-route-name-input']") as HTMLInputElement).value).toBe(
      "github-prs",
    );
    expect(q("[data-testid='assignment-editor-webhook-event-chip-pull_request']")).toBeTruthy();
    // The trigger's own prompt_template (not the static `instruction`, which
    // differs on this fixture) is what actually rendered at fire time, so
    // the unified top Instruction field must show it on load.
    expect(
      (q("[data-testid='assignment-editor-instruction-input']") as HTMLTextAreaElement).value,
    ).toBe("Review PR #{pull_request.number}");
    // Secret is write-only — nothing to prefill; status is fetched instead.
    expect(mockGetWebhookRouteSecretStatus).toHaveBeenCalledWith("github-prs");
    expect(q("[data-testid='assignment-editor-webhook-secret-input']")).toBeNull();

    await act(async () => {
      click(q("[data-testid='assignment-editor-webhook-advanced-toggle']")!);
    });
    expect((q("[data-testid='assignment-editor-webhook-filter-field']") as HTMLInputElement).value).toBe("action");
    expect((q("[data-testid='assignment-editor-webhook-filter-value']") as HTMLInputElement).value).toBe("opened");

    await act(async () => {
      click(q("[data-testid='assignment-editor-submit']")!);
    });

    expect(mockUpdateAssignment).toHaveBeenCalledTimes(1);
    const [, patch] = mockUpdateAssignment.mock.calls[0];
    expect(patch).toMatchObject({
      trigger: {
        type: "Webhook",
        route_name: "github-prs",
        secret_ref: "github-prs",
        events: ["pull_request"],
        filters: { field: "action", op: "equals", value: "opened" },
        prompt_template: "Review PR #{pull_request.number}",
        deliver: { type: "agent" },
      },
    });
    // The user never typed/generated a new secret this session — nothing to save.
    expect(mockSetWebhookRouteSecret).not.toHaveBeenCalled();
  });

  it("renders the Expires at control on the Watch and Webhook trigger tabs, not just Cron", async () => {
    await render();
    await act(async () => {
      useAssignmentEditorModalStore.getState().openCreate(AGENT_ID);
    });
    await act(async () => {});

    // Cron tab only shows it while Recurring (already covered by the
    // control living behind `isRecurring` in AssignmentEditorModal); the
    // create-mode default is recurring, so it's present here too.
    expect(q("[data-testid='assignment-editor-expires-at-field']")).toBeTruthy();

    await act(async () => {
      click(q("[data-testid='assignment-editor-trigger-webhook-tab']")!);
    });
    expect(q("[data-testid='assignment-editor-expires-at-field']")).toBeTruthy();

    await act(async () => {
      click(q("[data-testid='assignment-editor-trigger-connector_event-tab']")!);
    });
    expect(q("[data-testid='assignment-editor-expires-at-field']")).toBeTruthy();
  });
});
