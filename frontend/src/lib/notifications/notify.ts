/**
 * Single chokepoint every OS notification flows through.
 *
 * All gating — OS permission, user prefs, snooze, presence/focus tiering —
 * and all coalescing lives here, so any future `NotifiableKind` inherits it
 * for free without touching a single call site. `notify()` is the only
 * export call sites should use; everything else is exposed so tests can
 * drive/inspect the gating deterministically instead of depending on real
 * OS permission prompts or real `setTimeout` timers.
 */
import {
    isPermissionGranted,
    requestPermission,
    sendNotification,
} from "@tauri-apps/plugin-notification";
import { useChatStore, isEventForActiveThread } from "../../stores/chatStore";
import { useUserPreferencesStore, isNotificationSnoozed } from "../../stores/userPreferencesStore";
import { useWindowFocusStore } from "../../stores/windowFocusStore";

/** Event kinds that can request a notification. Extend this union as new
 *  kinds are wired in — every addition automatically inherits the gating
 *  and coalescing logic below. */
export type NotifiableKind = "assignment.fired" | "agent.reply";

export interface NotifiableEvent {
    kind: NotifiableKind;
    title: string;
    body: string;
    /** Owning agent, when the event is scoped to one. Omit for global-scope
     *  events (no single thread the user could already be "viewing"). */
    agentId?: string;
    threadId?: string;
    /** Coalescing key override. Defaults to `kind` when omitted, so events
     *  of the same kind within the coalescing window collapse into one
     *  summary banner regardless of which agent/thread they belong to. */
    dedupeKey?: string;
}

export type Tier = "none" | "silent" | "sound";

// Module-level permission cache. `ensureNotificationPermission` populates
// it; `computeTier` reads it synchronously so tiering never has to await an
// OS round-trip per event.
let permissionGranted = false;

/** Requests (or confirms) the OS notification permission, caching the
 *  result for `computeTier` to read synchronously. Safe to call repeatedly
 *  — it only prompts the user when permission isn't already granted. */
export async function ensureNotificationPermission(): Promise<boolean> {
    if (permissionGranted) return true;
    let granted = await isPermissionGranted();
    if (!granted) {
        const result = await requestPermission();
        granted = result === "granted";
    }
    permissionGranted = granted;
    return granted;
}

/** Computes the notification tier for a single event, reading live store
 *  state at call time (never cached) so a snooze/focus/thread-selection
 *  change mid-coalescing-window still applies at flush. Gating order:
 *    1. OS permission not granted -> 'none'.
 *    2. Notifications or the banner surface disabled by the user -> 'none'.
 *    3. Currently snoozed -> 'none'.
 *    4. Presence: viewing the exact agent+thread while focused -> 'none'
 *       (the unread indicator already covers it); focused elsewhere ->
 *       'silent'; backgrounded/away -> 'sound' or 'silent' per the user's
 *       sound preference. */
export function computeTier(e: NotifiableEvent): Tier {
    if (!permissionGranted) return "none";

    const prefs = useUserPreferencesStore.getState();
    if (!prefs.notificationsEnabled || !prefs.notifyBanner) return "none";
    if (isNotificationSnoozed(prefs)) return "none";

    const appFocused = useWindowFocusStore.getState().isFocused;
    const chat = useChatStore.getState();
    // `isEventForActiveThread` only answers "does this event's thread match
    // agent X's own remembered thread selection" — `selectedThreadIdByAgent`
    // persists per agent across agent switches, so it does NOT tell you
    // whether agent X is the agent currently on screen. Every other call
    // site in the codebase (chatStore's own merge-safety checks, useSSE.ts)
    // pairs it with an explicit `selectedAgentId === agentId` check, and we
    // do the same here: without it, "focused, different agent" would very
    // often collapse to "viewing" (most agents sit on their default thread,
    // which normalizes to the same `undefined` eventThreadId), wrongly
    // suppressing notifications the "focused, different thread -> silent"
    // rule above says should still fire.
    const viewing =
        appFocused &&
        e.agentId !== undefined &&
        chat.selectedAgentId === e.agentId &&
        isEventForActiveThread(e.agentId, e.threadId, chat.threadsByAgent, chat.selectedThreadIdByAgent);

    if (viewing) return "none";
    if (appFocused) return "silent";
    return prefs.notifySound ? "sound" : "silent";
}

/** Fires (or suppresses) a single native notification at the given tier. */
function fire(tier: Tier, title: string, body: string): void {
    if (tier === "none") return;
    sendNotification({
        title,
        body,
        ...(tier === "sound" ? { sound: "default" } : {}),
    });
}

const COALESCE_WINDOW_MS = 2500;

interface QueuedBatch {
    events: NotifiableEvent[];
    timer: ReturnType<typeof setTimeout>;
}

// Keyed by `dedupeKey ?? kind`. One pending batch + timer per key at a time.
const batches = new Map<string, QueuedBatch>();

const TIER_RANK: Record<Tier, number> = { none: 0, silent: 1, sound: 2 };

function highestTier(tiers: Tier[]): Tier {
    return tiers.reduce((best, t) => (TIER_RANK[t] > TIER_RANK[best] ? t : best), "none" as Tier);
}

/** Human labels for the coalesced summary banner, e.g. "3 assignments
 *  fired". Falls back to a generic "N <kind> events" for any kind this
 *  hasn't been taught a plural for yet, so new kinds never crash. */
const SUMMARY_LABELS: Partial<Record<NotifiableKind, string>> = {
    "assignment.fired": "assignments fired",
    "agent.reply": "new replies",
};

function summarize(kind: NotifiableKind, count: number): { title: string; body: string } {
    const label = SUMMARY_LABELS[kind] ?? `${kind} events`;
    return { title: `${count} ${label}`, body: "" };
}

function flush(key: string): void {
    const batch = batches.get(key);
    if (!batch) return;
    batches.delete(key);

    const { events } = batch;
    if (events.length === 1) {
        const e = events[0];
        fire(computeTier(e), e.title, e.body);
        return;
    }

    // Tier is computed per-event, at flush time, so live snooze/focus state
    // wins over whatever was true when each event was originally enqueued.
    const tier = highestTier(events.map(computeTier));
    const { title, body } = summarize(events[0].kind, events.length);
    fire(tier, title, body);
}

/** Test-only: force-flush a coalescing key immediately instead of waiting
 *  out the real 2500ms timer, so tests stay deterministic. */
export function __flushNow(key: string): void {
    const batch = batches.get(key);
    if (batch) clearTimeout(batch.timer);
    flush(key);
}

/** Test-only: reset all module-level state (permission cache + any pending
 *  coalescing timers/batches) for isolation between test cases. */
export function __resetNotifyStateForTests(): void {
    permissionGranted = false;
    for (const batch of batches.values()) clearTimeout(batch.timer);
    batches.clear();
}

/** The single public entry point every notification-worthy event should
 *  flow through. Enqueues `e` into its coalescing window (keyed by
 *  `dedupeKey ?? kind`); gating and tiering are resolved lazily at flush
 *  time — never here — so a snooze/focus change mid-window still applies. */
export function notify(e: NotifiableEvent): void {
    const key = e.dedupeKey ?? e.kind;
    const existing = batches.get(key);
    if (existing) {
        existing.events.push(e);
        return;
    }
    batches.set(key, {
        events: [e],
        timer: setTimeout(() => flush(key), COALESCE_WINDOW_MS),
    });
}
