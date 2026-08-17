/**
 * Tests for the assignment-fire -> notify() SSE translation adapter. Pure
 * translation only: these tests assert notify() is (or isn't) called with
 * the right shape, not any gating — gating is notify()'s job and is covered
 * in notify.test.ts.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

vi.mock("./notify", () => ({
    notify: vi.fn(),
}));

import { notify } from "./notify";
import { maybeNotifyForSystemMessage } from "./systemMessageAdapter";
import { useChatStore } from "../../stores/chatStore";

beforeEach(() => {
    vi.clearAllMocks();
    useChatStore.setState({ agents: [] });
});

describe("maybeNotifyForSystemMessage", () => {
    it("notifies once for an assignment-fire system_message", () => {
        maybeNotifyForSystemMessage("agent-1", { text: "Assignment run started: run-42" }, undefined);

        expect(notify).toHaveBeenCalledTimes(1);
        expect(notify).toHaveBeenCalledWith(
            expect.objectContaining({
                kind: "assignment.fired",
                agentId: "agent-1",
                dedupeKey: "assignment.fired",
            }),
        );
    });

    it("prefers the agent's display name in the body when resolvable", () => {
        useChatStore.setState({
            agents: [
                {
                    agent_id: "agent-1",
                    name: "Axew",
                    last_activity_at: null,
                    message_count: 0,
                    has_active_run: false,
                    queue_depth: 0,
                } as import("../../types/api").AgentSnapshot,
            ],
        });

        maybeNotifyForSystemMessage("agent-1", { text: "Assignment run started: run-42" }, undefined);

        expect(notify).toHaveBeenCalledWith(
            expect.objectContaining({ body: expect.stringContaining("Axew") }),
        );
    });

    it("falls back to the raw text when the agent's name isn't resolvable", () => {
        maybeNotifyForSystemMessage("agent-1", { text: "Assignment run started: run-42" }, undefined);

        expect(notify).toHaveBeenCalledWith(
            expect.objectContaining({ body: "Assignment run started: run-42" }),
        );
    });

    it("passes the event thread id through when present", () => {
        maybeNotifyForSystemMessage("agent-1", { text: "Assignment run started: run-42" }, "thread-9");

        expect(notify).toHaveBeenCalledWith(expect.objectContaining({ threadId: "thread-9" }));
    });

    it("does not notify for a non-matching system message", () => {
        maybeNotifyForSystemMessage("agent-1", { text: "Assignment run succeeded: run-42" }, undefined);
        maybeNotifyForSystemMessage("agent-1", { text: "Assignment run failed: run-42" }, undefined);
        maybeNotifyForSystemMessage("agent-1", { text: "some unrelated system message" }, undefined);

        expect(notify).not.toHaveBeenCalled();
    });

    it("does not notify when text is missing or non-string", () => {
        maybeNotifyForSystemMessage("agent-1", {}, undefined);
        maybeNotifyForSystemMessage("agent-1", { text: 42 }, undefined);
        maybeNotifyForSystemMessage("agent-1", null, undefined);

        expect(notify).not.toHaveBeenCalled();
    });
});
