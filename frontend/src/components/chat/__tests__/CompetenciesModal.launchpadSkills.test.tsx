// @vitest-environment jsdom
//
// Convention-folder skills: the Competencies modal's Skills tab gains a
// "Global skills" section
// (agent-scoped, always visible) and a "Project skills — <project>" section
// driven by the opening thread's focus path. Mirrors the per-plugin section
// markup/toggle wiring already in CompetenciesModal.tsx.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import type { AgentProfile } from "../../../types/api";
import type { LaunchpadSkillEntry } from "../../../lib/api";

const listAgentSkillsMock = vi.fn();
const listRulesMock = vi.fn();
const listInstructionsMock = vi.fn();
const getInstructionFilenamesMock = vi.fn();
const getWorkflowsMock = vi.fn();
const getAgentMock = vi.fn();
const listLaunchpadGlobalSkillsMock = vi.fn();
const listLaunchpadProjectSkillsMock = vi.fn();
const setLaunchpadGlobalSkillEnabledMock = vi.fn();
const setLaunchpadProjectSkillEnabledMock = vi.fn();
const promoteLaunchpadSkillMock = vi.fn();

const { MockApiError } = vi.hoisted(() => {
  class MockApiError extends Error {
    status: number;
    constructor(status: number, message: string) {
      super(message);
      this.name = "ApiError";
      this.status = status;
    }
  }
  return { MockApiError };
});

vi.mock("../../../lib/api", () => ({
  getAgent: (...a: unknown[]) => getAgentMock(...a),
  listAgentSkills: (...a: unknown[]) => listAgentSkillsMock(...a),
  listRules: (...a: unknown[]) => listRulesMock(...a),
  listInstructions: (...a: unknown[]) => listInstructionsMock(...a),
  getInstructionFilenames: (...a: unknown[]) => getInstructionFilenamesMock(...a),
  getWorkflows: (...a: unknown[]) => getWorkflowsMock(...a),
  listLaunchpadGlobalSkills: (...a: unknown[]) => listLaunchpadGlobalSkillsMock(...a),
  listLaunchpadProjectSkills: (...a: unknown[]) => listLaunchpadProjectSkillsMock(...a),
  setLaunchpadGlobalSkillEnabled: (...a: unknown[]) => setLaunchpadGlobalSkillEnabledMock(...a),
  setLaunchpadProjectSkillEnabled: (...a: unknown[]) => setLaunchpadProjectSkillEnabledMock(...a),
  promoteLaunchpadSkill: (...a: unknown[]) => promoteLaunchpadSkillMock(...a),
  ApiError: MockApiError,
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn().mockResolvedValue([]),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: vi.fn().mockResolvedValue(null),
}));
vi.mock("framer-motion", () => ({
  motion: {
    div: ({ children, ...rest }: React.HTMLAttributes<HTMLDivElement>) =>
      React.createElement("div", rest, children),
  },
  AnimatePresence: ({ children }: { children: React.ReactNode }) =>
    React.createElement(React.Fragment, null, children),
}));

import { CompetenciesModal } from "../CompetenciesModal";
import { useCompetenciesModalStore } from "../../../stores/competenciesModalStore";
import { useSkillsStore } from "../../../stores/skillsStore";
import { useRulesStore } from "../../../stores/rulesStore";
import { useInstructionsStore } from "../../../stores/instructionsStore";
import { useWorkflowsStore } from "../../../stores/workflowsStore";

const AGENT_ID = "agent-1";
const FOCUS_PATH = "/Users/dev/my-project";

function makeAgentProfile(overrides: Partial<AgentProfile> = {}): AgentProfile {
  return {
    id: AGENT_ID,
    name: "Test Agent",
    description: "Test",
    provider: {
      type: "Cli",
      command: "echo",
      args: [],
      output_format: "Text",
      input_mode: "Arg",
      model_aliases: {},
      resume_args: [],
      session_id_fields: [],
      clear_env: false,
      no_output_timeout_ms: 30000,
    },
    model: null,
    skills: [],
    system_prompt: null,
    tools: null,
    env: {},
    max_instances: 1,
    timeout_seconds: 300,
    working_dir: null,
    home_dir: null,
    serialize: true,
    ...overrides,
  };
}

function makeSkill(overrides: Partial<LaunchpadSkillEntry> = {}): LaunchpadSkillEntry {
  return {
    name: "code-review",
    description: "Review the current diff",
    path: "/data/.launchpad/skills/code-review/SKILL.md",
    ...overrides,
  };
}

describe("CompetenciesModal — launchpad convention-folder skills", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    vi.clearAllMocks();
    listAgentSkillsMock.mockResolvedValue([]);
    listRulesMock.mockResolvedValue([]);
    listInstructionsMock.mockResolvedValue([]);
    getInstructionFilenamesMock.mockResolvedValue([]);
    getWorkflowsMock.mockResolvedValue([]);
    getAgentMock.mockResolvedValue(makeAgentProfile());
    listLaunchpadGlobalSkillsMock.mockResolvedValue([]);
    listLaunchpadProjectSkillsMock.mockResolvedValue({ project_key: "", skills: [] });
    setLaunchpadGlobalSkillEnabledMock.mockResolvedValue({ skill_name: "", enabled: true });
    setLaunchpadProjectSkillEnabledMock.mockResolvedValue({ project_key: "", skill_name: "", enabled: true });
    promoteLaunchpadSkillMock.mockResolvedValue({ promoted: "" });

    useCompetenciesModalStore.getState().close();
    useSkillsStore.getState().reset();
    useRulesStore.getState().reset();
    useInstructionsStore.getState().reset();
    useWorkflowsStore.getState().reset();
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function flush() {
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
  }

  function render() {
    act(() => {
      root.render(React.createElement(CompetenciesModal));
    });
  }

  it("renders the Global skills section and toggles call setLaunchpadGlobalSkillEnabled", async () => {
    listLaunchpadGlobalSkillsMock.mockResolvedValue([makeSkill({ name: "code-review", description: "Review the current diff" })]);

    render();
    act(() => {
      useCompetenciesModalStore.getState().open(AGENT_ID);
    });
    await flush();

    expect(container.textContent).toContain("Global skills");
    expect(container.textContent).toContain("code-review");
    expect(container.textContent).toContain("Review the current diff");

    const toggle = container.querySelector('button[aria-label="Enable skill"]') as HTMLButtonElement;
    expect(toggle).toBeTruthy();
    await act(async () => {
      toggle.click();
      await Promise.resolve();
    });

    expect(setLaunchpadGlobalSkillEnabledMock).toHaveBeenCalledWith(AGENT_ID, "code-review", true);
  });

  it("shows an empty-state hint in the Project skills section when the thread has no focus path", async () => {
    render();
    act(() => {
      useCompetenciesModalStore.getState().open(AGENT_ID);
    });
    await flush();

    expect(container.textContent).toContain("Project skills");
    expect(container.textContent).toContain("Focus this thread on a project to see its skills.");
    // No focus path means listLaunchpadProjectSkills is never called.
    expect(listLaunchpadProjectSkillsMock).not.toHaveBeenCalled();
  });

  it("renders project tiles with the shadow badge when a skill name also exists globally", async () => {
    listLaunchpadGlobalSkillsMock.mockResolvedValue([makeSkill({ name: "code-review", description: "Global version" })]);
    listLaunchpadProjectSkillsMock.mockResolvedValue({
      project_key: "/canonical/my-project",
      skills: [makeSkill({ name: "code-review", description: "Project-local override" })],
    });

    render();
    act(() => {
      useCompetenciesModalStore.getState().open(AGENT_ID, FOCUS_PATH);
    });
    await flush();

    expect(listLaunchpadProjectSkillsMock).toHaveBeenCalledWith(FOCUS_PATH);
    expect(container.textContent).toContain("Project skills — my-project");
    expect(container.textContent).toContain("Project-local override");
    expect(container.textContent).toContain("Overrides global");
    expect(container.textContent).toContain("Overridden by project");
  });

  it("promote control calls promoteLaunchpadSkill and shows a non-fatal notice on 409", async () => {
    listLaunchpadProjectSkillsMock.mockResolvedValue({
      project_key: "/canonical/my-project",
      skills: [makeSkill({ name: "code-review" })],
    });
    promoteLaunchpadSkillMock.mockRejectedValueOnce(
      new MockApiError(409, "a global skill named 'code-review' already exists"),
    );

    render();
    act(() => {
      useCompetenciesModalStore.getState().open(AGENT_ID, FOCUS_PATH);
    });
    await flush();

    const promoteButton = Array.from(container.querySelectorAll("button")).find(
      (b) => b.textContent === "Make available globally",
    ) as HTMLButtonElement;
    expect(promoteButton).toBeTruthy();

    await act(async () => {
      promoteButton.click();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(promoteLaunchpadSkillMock).toHaveBeenCalledWith(FOCUS_PATH, "code-review");
    expect(container.textContent).toContain("Already exists globally");
  });
});
