// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { MemoriesSettings } from "../MemoriesSettings";
import {
    getAgents,
    getMemories,
    getMemoryReviewQueue,
    getAgentMemorySummaries,
    getSkillReviewQueue,
    actOnSkillReview,
    promoteSkillObservation,
} from "../../../lib/api";

// The panel drives the skill-review surface entirely through the api client,
// so mocking the module lets us assert the exact endpoints/payloads each button
// fires — mirroring how the memory-review side is exercised.
vi.mock("../../../lib/api", () => ({
    getAgents: vi.fn(),
    getMemories: vi.fn(),
    getMemoryReviewQueue: vi.fn(),
    getAgentMemorySummaries: vi.fn(),
    actOnMemoryReviewCandidate: vi.fn(),
    undoMemoryWrite: vi.fn(),
    getSkillReviewQueue: vi.fn(),
    actOnSkillReview: vi.fn(),
    promoteSkillObservation: vi.fn(),
}));

// The agent-list column width comes from a zustand selector hook — feed it a
// fixed state so the selector resolves without a real store.
vi.mock("../../../stores/userPreferencesStore", () => ({
    useUserPreferencesStore: (sel: (s: unknown) => unknown) =>
        sel({ memoriesAgentListWidth: 240, setMemoriesAgentListWidth: () => {} }),
}));

const AGENT_ID = "a1";

const PARKED = {
    name: "greet-users",
    description: "Greet users warmly",
    body: "# Greeting\nBe warm and concise.",
    origin: "distilled" as const,
    distilled_from: ["o1", "o2"],
    created_at: "2026-07-14T00:00:00Z",
};

/** A skill an agent wrote on request via SkillRegister: parked exactly like a
 *  distilled one, but `user_authored` with no source observations. It lands in
 *  the same queue because this is the only surface that can un-park it. */
const AGENT_WRITTEN = {
    name: "run-migrations",
    description: "Run the DB migration checklist",
    body: "# Migrations\nRun them in order.",
    origin: "user_authored" as const,
    distilled_from: [],
    created_at: "2026-07-14T00:00:00Z",
};

const OBSERVATION = {
    id: "obs-1",
    content: "When a user says hi, respond with a short friendly greeting.",
    source_thread_id: "t1",
    created_at: "2026-07-14T00:00:00Z",
};

/** Flush pending microtasks + timers (the panel's load chain resolves over a
 *  few ticks: getAgents → refresh() fan-out → finally setLoading(false)). */
async function flush() {
    await act(async () => {
        await new Promise((r) => setTimeout(r, 0));
    });
}

describe("MemoriesSettings — skill review surface", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        vi.mocked(getAgents).mockResolvedValue([
            { agent_id: AGENT_ID, name: "Agent One", emoji: "🤖" },
        ] as never);
        vi.mocked(getMemories).mockResolvedValue([] as never);
        vi.mocked(getMemoryReviewQueue).mockResolvedValue([] as never);
        vi.mocked(getAgentMemorySummaries).mockResolvedValue({} as never);
        vi.mocked(getSkillReviewQueue).mockResolvedValue({
            candidates: [PARKED, AGENT_WRITTEN],
            observations: [OBSERVATION],
        } as never);
        vi.mocked(actOnSkillReview).mockResolvedValue({} as never);
        vi.mocked(promoteSkillObservation).mockResolvedValue(PARKED as never);

        container = document.createElement("div");
        document.body.appendChild(container);
        root = createRoot(container);
    });

    afterEach(async () => {
        await act(async () => {
            root.unmount();
        });
        document.body.removeChild(container);
        vi.clearAllMocks();
    });

    async function render() {
        await act(async () => {
            root.render(React.createElement(MemoriesSettings));
        });
        await flush();
    }

    function button(label: string): HTMLButtonElement {
        const btns = Array.from(container.querySelectorAll("button"));
        const found = btns.find((b) => b.textContent?.trim() === label);
        if (!found) {
            throw new Error(
                `button "${label}" not found; have: ${btns
                    .map((b) => JSON.stringify(b.textContent?.trim()))
                    .join(", ")}`,
            );
        }
        return found as HTMLButtonElement;
    }

    async function click(el: HTMLElement) {
        await act(async () => {
            el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
        });
        await flush();
    }

    it("renders the parked skill and its promotable observation", async () => {
        await render();
        expect(container.textContent).toContain("Skills held for review");
        expect(container.textContent).toContain("greet-users");
        expect(container.textContent).toContain("Greet users warmly");
        expect(container.textContent).toContain("distilled from 2 observations");
        expect(container.textContent).toContain("Promote an observation");
        expect(container.textContent).toContain(
            "When a user says hi, respond with a short friendly greeting.",
        );
    });

    it("renders an agent-written skill and labels its origin", async () => {
        await render();
        expect(container.textContent).toContain("run-migrations");
        expect(container.textContent).toContain("written by the agent on request");
        // It must not be described as distilled from zero observations — the
        // provenance label is the only thing telling a reviewer which of the
        // two writers produced what they are about to approve.
        expect(container.textContent).not.toContain("distilled from 0 observations");
    });

    it("KEEP posts the accept action for the skill", async () => {
        await render();
        await click(button("Keep"));
        expect(actOnSkillReview).toHaveBeenCalledWith(AGENT_ID, "greet-users", {
            action: "accept",
        });
    });

    it("EDIT then save posts the edit action with body + description", async () => {
        await render();
        await click(button("Edit"));
        await click(button("Save & keep"));
        expect(actOnSkillReview).toHaveBeenCalledWith(AGENT_ID, "greet-users", {
            action: "edit",
            body: PARKED.body,
            description: PARKED.description,
        });
    });

    it("FORGET posts the reject action for the skill", async () => {
        await render();
        await click(button("Forget"));
        expect(actOnSkillReview).toHaveBeenCalledWith(AGENT_ID, "greet-users", {
            action: "reject",
        });
    });

    it("PROMOTE calls the promote endpoint and refreshes the queue", async () => {
        await render();
        const before = vi.mocked(getSkillReviewQueue).mock.calls.length;
        await click(button("Promote"));
        expect(promoteSkillObservation).toHaveBeenCalledWith(AGENT_ID, "obs-1");
        // Refresh re-fetches the queue so the promoted observation moves into
        // "Skills held for review".
        expect(vi.mocked(getSkillReviewQueue).mock.calls.length).toBeGreaterThan(before);
    });
});
