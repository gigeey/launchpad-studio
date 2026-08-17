/**
 * Tests for notify.ts: the single chokepoint every OS notification flows
 * through. Covers the LOCKED tier-gating order (permission -> prefs ->
 * snooze -> presence) and the 2500ms coalescing window, using
 * `__flushNow`/`__resetNotifyStateForTests` so nothing depends on real OS
 * permission prompts or real timers.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import type { Thread } from "../../types/api";
import { useChatStore } from "../../stores/chatStore";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { useWindowFocusStore } from "../../stores/windowFocusStore";

vi.mock("@tauri-apps/plugin-notification", () => ({
    isPermissionGranted: vi.fn(),
    requestPermission: vi.fn(),
    sendNotification: vi.fn(),
}));

import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import {
    notify,
    computeTier,
    ensureNotificationPermission,
    __flushNow,
    __resetNotifyStateForTests,
} from "./notify";

function makeThread(id: string, kind: Thread["kind"]): Thread {
    return {
        id,
        title: null,
        scope: { type: "AgentChat", agent_id: "agent-1" },
        transcript_path: `/tmp/${id}.jsonl`,
        kind,
        created_at: "2026-01-01T00:00:00Z",
        updated_at: "2026-01-01T00:00:00Z",
    };
}

/** Grants permission deterministically (bypasses the real plugin round-trip
 *  via the mocked `isPermissionGranted`). */
async function grantPermission(): Promise<void> {
    vi.mocked(isPermissionGranted).mockResolvedValueOnce(true);
    await ensureNotificationPermission();
}

beforeEach(() => {
    vi.clearAllMocks();
    __resetNotifyStateForTests();
    useWindowFocusStore.setState({ isFocused: true });
    useUserPreferencesStore.setState({
        notificationsEnabled: true,
        notifyBanner: true,
        notifySound: true,
        notifySnoozedUntil: null,
    });
    useChatStore.setState({
        selectedAgentId: null,
        threadsByAgent: new Map(),
        selectedThreadIdByAgent: new Map(),
    });
});

describe("computeTier", () => {
    it("viewing the exact agent+thread while focused -> none", async () => {
        await grantPermission();
        useChatStore.setState({
            selectedAgentId: "agent-1",
            threadsByAgent: new Map(),
            selectedThreadIdByAgent: new Map(), // no entry -> resolves to default (undefined)
        });
        const tier = computeTier({ kind: "assignment.fired", title: "t", body: "b", agentId: "agent-1", threadId: undefined });
        expect(tier).toBe("none");
    });

    it("focused but on a different thread of the same agent -> silent", async () => {
        await grantPermission();
        useChatStore.setState({
            selectedAgentId: "agent-1",
            threadsByAgent: new Map([["agent-1", [makeThread("thread-b", "fresh")]]]),
            selectedThreadIdByAgent: new Map([["agent-1", "thread-b"]]),
        });
        const tier = computeTier({ kind: "assignment.fired", title: "t", body: "b", agentId: "agent-1", threadId: "thread-a" });
        expect(tier).toBe("silent");
    });

    it("focused but on a totally different agent -> silent (not none)", async () => {
        // Regression guard: isEventForActiveThread alone only checks agent-1's
        // OWN remembered thread selection, not whether agent-1 is on screen.
        // Without an explicit selectedAgentId match, viewing an unrelated
        // agent-2 could wrongly resolve to "viewing" agent-1's default thread.
        await grantPermission();
        useChatStore.setState({
            selectedAgentId: "agent-2",
            threadsByAgent: new Map(),
            selectedThreadIdByAgent: new Map(),
        });
        const tier = computeTier({ kind: "assignment.fired", title: "t", body: "b", agentId: "agent-1", threadId: undefined });
        expect(tier).toBe("silent");
    });

    it("backgrounded/away with sound enabled -> sound", async () => {
        // Regression guard: with notificationsEnabled=true (the default) and
        // OS permission granted/primed, a backgrounded window must resolve
        // to the banner+sound tier, not "none". This is the exact path that
        // was silently broken when nothing ever primed the permission cache
        // at startup — see ensureNotificationPermission() and its App-level
        // caller.
        await grantPermission();
        useWindowFocusStore.setState({ isFocused: false });
        const tier = computeTier({ kind: "assignment.fired", title: "t", body: "b", agentId: "agent-1" });
        expect(tier).toBe("sound");
    });

    it("backgrounded/away with sound disabled -> silent", async () => {
        await grantPermission();
        useWindowFocusStore.setState({ isFocused: false });
        useUserPreferencesStore.setState({ notifySound: false });
        const tier = computeTier({ kind: "assignment.fired", title: "t", body: "b", agentId: "agent-1" });
        expect(tier).toBe("silent");
    });

    it("snoozed -> none", async () => {
        await grantPermission();
        useWindowFocusStore.setState({ isFocused: false });
        useUserPreferencesStore.setState({ notifySnoozedUntil: Date.now() + 60_000 });
        const tier = computeTier({ kind: "assignment.fired", title: "t", body: "b", agentId: "agent-1" });
        expect(tier).toBe("none");
    });

    it("OS permission not granted -> none regardless of everything else", () => {
        // Permission cache starts false after __resetNotifyStateForTests();
        // deliberately not calling grantPermission() here.
        useWindowFocusStore.setState({ isFocused: false });
        const tier = computeTier({ kind: "assignment.fired", title: "t", body: "b", agentId: "agent-1" });
        expect(tier).toBe("none");
    });
});

describe("notify coalescing", () => {
    it("a single event in the window fires once, at its own tier", async () => {
        await grantPermission();
        useWindowFocusStore.setState({ isFocused: false }); // away + sound -> 'sound'

        notify({ kind: "assignment.fired", title: "Assignment X fired", body: "details", agentId: "agent-1" });
        __flushNow("assignment.fired");

        expect(sendNotification).toHaveBeenCalledTimes(1);
        expect(sendNotification).toHaveBeenCalledWith({
            title: "Assignment X fired",
            body: "details",
            sound: "default",
        });
    });

    it("two events within the window collapse into one summary banner at the highest tier", async () => {
        await grantPermission();
        // First event fires while focused-elsewhere ('silent'); second fires
        // while away with sound enabled ('sound') -> batch fires at 'sound'.
        useChatStore.setState({ selectedAgentId: "agent-2" });
        notify({ kind: "assignment.fired", title: "A fired", body: "a", agentId: "agent-1" });

        useWindowFocusStore.setState({ isFocused: false });
        notify({ kind: "assignment.fired", title: "B fired", body: "b", agentId: "agent-1" });

        __flushNow("assignment.fired");

        expect(sendNotification).toHaveBeenCalledTimes(1);
        expect(sendNotification).toHaveBeenCalledWith({
            title: "2 assignments fired",
            body: "",
            sound: "default",
        });
    });

    it("does not fire before the coalescing window is flushed", async () => {
        await grantPermission();
        notify({ kind: "assignment.fired", title: "t", body: "b", agentId: "agent-1" });
        expect(sendNotification).not.toHaveBeenCalled();
    });
});

describe("ensureNotificationPermission", () => {
    it("requests permission when not already granted, and caches a denial", async () => {
        vi.mocked(isPermissionGranted).mockResolvedValueOnce(false);
        vi.mocked(requestPermission).mockResolvedValueOnce("denied");

        const granted = await ensureNotificationPermission();

        expect(granted).toBe(false);
        expect(requestPermission).toHaveBeenCalledTimes(1);

        const tier = computeTier({ kind: "assignment.fired", title: "t", body: "b" });
        expect(tier).toBe("none");
    });

    it("skips requestPermission entirely once already granted", async () => {
        vi.mocked(isPermissionGranted).mockResolvedValueOnce(true);
        await ensureNotificationPermission();
        vi.mocked(isPermissionGranted).mockClear();

        await ensureNotificationPermission();

        expect(isPermissionGranted).not.toHaveBeenCalled();
        expect(requestPermission).not.toHaveBeenCalled();
    });
});
