/**
 * Translates SSE `system_message` events into `notify()` calls.
 *
 * This is pure translation — matching, shaping the `NotifiableEvent`, and
 * handing it to `notify()`. All gating (permission/prefs/snooze/presence)
 * lives in `notify()`'s composed gate; this file must never duplicate it.
 *
 * The only `system_message` text this currently recognizes is the
 * assignment-fire signal emitted once per run by
 * `crates/ao-engine/src/assignment_runner.rs` (`"Assignment run started: {run_id}"`).
 * Assignment runs also emit a "succeeded"/"failed" `system_message` from
 * `queue_manager.rs` on completion — those intentionally do NOT match here,
 * so a run notifies once (on fire), not twice.
 */
import { notify } from "./notify";
import { useChatStore } from "../../stores/chatStore";

const ASSIGNMENT_FIRE_PREFIX = "Assignment run started";

/**
 * Inspects a parsed `system_message` payload and, if it's an assignment-fire
 * signal, hands a `NotifiableEvent` to `notify()`. No-ops for any other
 * `system_message` text. Never throws — callers still wrap this in
 * try/catch as defense in depth, since it reads live store state.
 */
export function maybeNotifyForSystemMessage(
    agentId: string,
    data: Record<string, unknown> | null,
    eventThreadId: string | undefined,
): void {
    const text = data?.text;
    if (typeof text !== "string" || !text.startsWith(ASSIGNMENT_FIRE_PREFIX)) return;

    const agentName = useChatStore
        .getState()
        .agents.find((a) => a.agent_id === agentId)?.name;

    notify({
        kind: "assignment.fired",
        title: "Assignment fired",
        body: agentName ? `${agentName}: ${text}` : text,
        agentId,
        threadId: eventThreadId,
        dedupeKey: "assignment.fired",
    });
}
