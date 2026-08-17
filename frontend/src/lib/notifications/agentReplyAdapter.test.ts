/**
 * Tests for the completed-agent-reply -> notify() SSE translation adapter.
 * Pure translation only: these tests assert notify() is (or isn't) called
 * with the right shape, not any gating — gating is notify()'s job and is
 * covered in notify.test.ts.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

vi.mock("./notify", () => ({
    notify: vi.fn(),
}));

import { notify } from "./notify";
import { maybeNotifyForAgentReply } from "./agentReplyAdapter";
import { useChatStore } from "../../stores/chatStore";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";

beforeEach(() => {
    vi.clearAllMocks();
    useChatStore.setState({ agents: [] });
    useUserPreferencesStore.setState({ notifyAgentReplies: true });
});

describe("maybeNotifyForAgentReply", () => {
    it("does not notify when the notifyAgentReplies preference is off", () => {
        useUserPreferencesStore.setState({ notifyAgentReplies: false });

        maybeNotifyForAgentReply({ agentId: "agent-1", threadId: undefined, text: "Hello there" });

        expect(notify).not.toHaveBeenCalled();
    });

    it("does not notify for empty or whitespace-only text", () => {
        maybeNotifyForAgentReply({ agentId: "agent-1", threadId: undefined, text: "" });
        maybeNotifyForAgentReply({ agentId: "agent-1", threadId: undefined, text: "   \n\t  " });

        expect(notify).not.toHaveBeenCalled();
    });

    it("notifies with kind agent.reply, dedupeKey, and resolved title on the happy path", () => {
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

        maybeNotifyForAgentReply({ agentId: "agent-1", threadId: "thread-9", text: "Here's the fix." });

        expect(notify).toHaveBeenCalledTimes(1);
        expect(notify).toHaveBeenCalledWith({
            kind: "agent.reply",
            title: "Axew",
            body: "Here's the fix.",
            agentId: "agent-1",
            threadId: "thread-9",
            dedupeKey: "agent.reply:agent-1:thread-9",
        });
    });

    it("falls back to a generic title when the agent's name isn't resolvable", () => {
        maybeNotifyForAgentReply({ agentId: "agent-1", threadId: undefined, text: "Hello" });

        expect(notify).toHaveBeenCalledWith(
            expect.objectContaining({ title: "Launchpad Studio", dedupeKey: "agent.reply:agent-1:default" }),
        );
    });

    it("collapses whitespace/newlines and truncates the body preview", () => {
        const longText = `line one\n\nline   two\t\t${"x".repeat(200)}`;

        maybeNotifyForAgentReply({ agentId: "agent-1", threadId: undefined, text: longText });

        const call = (notify as unknown as ReturnType<typeof vi.fn>).mock.calls[0][0];
        expect(call.body.includes("\n")).toBe(false);
        expect(call.body.length).toBe(141);
        expect(call.body.endsWith("…")).toBe(true);
    });
});
