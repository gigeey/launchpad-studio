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
    actOnMemoryReviewCandidate,
    getSkillReviewQueue,
} from "../../../lib/api";

// Same mocking approach as the skills-side test: the panel drives everything
// through the api client, so mocking it lets us assert exactly which
// endpoint/payload each button fires without a real backend.
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

vi.mock("../../../stores/userPreferencesStore", () => ({
    useUserPreferencesStore: (sel: (s: unknown) => unknown) =>
        sel({ memoriesAgentListWidth: 240, setMemoriesAgentListWidth: () => {} }),
}));

const AGENT_ID = "a1";

const CANDIDATE = {
    id: "cand-1",
    kind: "memory" as const,
    agent_id: AGENT_ID,
    source_thread_id: "t1",
    content: "User prefers concise commit messages.",
    status: "pending" as const,
    target_scope: "Agent" as const,
    target_scope_key: AGENT_ID,
    contradicts: null,
    reason: "self-improvement candidate defaults to quarantine pending confirmation",
    created_at: "2026-07-14T00:00:00Z",
};

/** Flush pending microtasks + timers (the panel's load chain resolves over a
 *  few ticks: getAgents → refresh() fan-out → finally setLoading(false)). */
async function flush() {
    await act(async () => {
        await new Promise((r) => setTimeout(r, 0));
    });
}

describe("MemoriesSettings — staged memory review queue", () => {
    let container: HTMLDivElement;
    let root: ReturnType<typeof createRoot>;

    beforeEach(() => {
        vi.mocked(getAgents).mockResolvedValue([
            { agent_id: AGENT_ID, name: "Agent One", emoji: "🤖" },
        ] as never);
        vi.mocked(getMemories).mockResolvedValue([] as never);
        vi.mocked(getMemoryReviewQueue).mockResolvedValue([CANDIDATE] as never);
        vi.mocked(getAgentMemorySummaries).mockResolvedValue({} as never);
        vi.mocked(getSkillReviewQueue).mockResolvedValue({
            candidates: [],
            observations: [],
        } as never);
        vi.mocked(actOnMemoryReviewCandidate).mockResolvedValue({} as never);

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

    it("lists a staged candidate with its scope and why-staged reason", async () => {
        await render();
        expect(container.textContent).toContain("Held for review");
        expect(container.textContent).toContain("User prefers concise commit messages.");
        expect(container.textContent).toContain(
            "self-improvement candidate defaults to quarantine pending confirmation",
        );
    });

    it("KEEP calls the review endpoint with the keep action and refreshes the queue", async () => {
        await render();
        const before = vi.mocked(getMemoryReviewQueue).mock.calls.length;

        // Once this candidate resolves, a re-fetch returns an empty queue —
        // proves the list updates instead of just leaving the stale entry on
        // screen.
        vi.mocked(getMemoryReviewQueue).mockResolvedValue([] as never);

        await click(button("Keep"));

        expect(actOnMemoryReviewCandidate).toHaveBeenCalledWith(
            AGENT_ID,
            "cand-1",
            "keep",
            undefined,
        );
        expect(vi.mocked(getMemoryReviewQueue).mock.calls.length).toBeGreaterThan(before);
        expect(container.textContent).not.toContain("User prefers concise commit messages.");
    });

    it("FORGET (dismiss) calls the review endpoint with the forget action and clears the entry", async () => {
        await render();
        vi.mocked(getMemoryReviewQueue).mockResolvedValue([] as never);

        await click(button("Forget"));

        expect(actOnMemoryReviewCandidate).toHaveBeenCalledWith(
            AGENT_ID,
            "cand-1",
            "forget",
            undefined,
        );
        expect(container.textContent).not.toContain("User prefers concise commit messages.");
    });
});
