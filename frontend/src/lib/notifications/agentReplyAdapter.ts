/**
 * Translates a completed agent reply segment (SSE `text_complete`) into a
 * `notify()` call.
 *
 * This is pure translation — matching, shaping the `NotifiableEvent`, and
 * handing it to `notify()`. All gating (permission/prefs/snooze/presence)
 * lives in `notify()`'s composed gate; this file must never duplicate it.
 * The `notifyAgentReplies` preference gates only this notification kind —
 * it does not affect `assignment.fired`.
 */
import { notify } from "./notify";
import { useChatStore } from "../../stores/chatStore";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";

const BODY_PREVIEW_MAX_LENGTH = 140;

/** Collapses whitespace/newlines into single spaces and truncates to a
 *  one-line banner preview, so a multi-paragraph reply doesn't blow out the
 *  OS notification body. */
function previewText(text: string): string {
    const collapsed = text.replace(/\s+/g, " ").trim();
    if (collapsed.length <= BODY_PREVIEW_MAX_LENGTH) return collapsed;
    return `${collapsed.slice(0, BODY_PREVIEW_MAX_LENGTH)}…`;
}

/**
 * Inspects a completed agent reply segment and, if the `notifyAgentReplies`
 * preference is on and the segment has visible text, hands a
 * `NotifiableEvent` to `notify()`. No-ops for empty/tool-only segments.
 * Never throws — callers still wrap this in try/catch as defense in depth,
 * since it reads live store state.
 */
export function maybeNotifyForAgentReply(params: {
    agentId: string;
    threadId: string | undefined;
    text: string;
}): void {
    const { agentId, threadId, text } = params;
    if (!useUserPreferencesStore.getState().notifyAgentReplies) return;
    if (!text.trim()) return;

    const agentName = useChatStore
        .getState()
        .agents.find((a) => a.agent_id === agentId)?.name;

    notify({
        kind: "agent.reply",
        title: agentName ?? "Launchpad Studio",
        body: previewText(text),
        agentId,
        threadId,
        dedupeKey: `agent.reply:${agentId}:${threadId ?? "default"}`,
    });
}
