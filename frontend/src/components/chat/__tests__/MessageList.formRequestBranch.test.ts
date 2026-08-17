/**
 * Regression suite for the inline `form_request` bubble's render decision.
 *
 * Before this fix, the branch had exactly one boolean — `isPending` — and
 * used "not pending" as an implicit stand-in for "answered." That's wrong
 * for an async form: `isPendingFormStillCurrent` (chatStore.ts) flips false
 * the instant *anything else* happens in the thread — the near-universal
 * case, since the whole point of "async" is that the agent keeps narrating
 * right after posting one. A genuinely unanswered, undismissed form then
 * fell through to the read-only branch with no `initialAnswers`: a blank,
 * permanently unanswerable bubble, even though the backend's
 * `async_form_answer` route (`crates/.../form_answers.rs`) only ever checked
 * plain `pending_forms` membership — never latestness — so a late answer
 * was accepted server-side the whole time.
 *
 * `resolveFormRequestBranch` and `resolveInlineAsyncFormTarget` are the pure
 * decision points `MessageList.tsx`'s `form_request` render branch now runs
 * through; `buildMessageItems` proves the invariant that makes a fourth
 * ("is this a sync form?") branch unnecessary. Per the sibling
 * `MessageList.pendingFormPlaceholder.test.ts`, a full DOM render of
 * `MessageList` itself isn't feasible in jsdom (the virtualizer + missing
 * `IntersectionObserver`) — these pure functions carry the real decision
 * logic, so testing them directly is testing the real behavior verbatim.
 * The interactive/read-only widgets they feed (`AsyncFormRequestCard`,
 * `AskUserQuestionForm`) have their own direct-render coverage in
 * `AsyncFormEntries.test.tsx` / `AskUserQuestionForm.test.tsx`.
 */

import { describe, it, expect } from "vitest";
import { resolveFormRequestBranch, resolveInlineAsyncFormTarget, buildMessageItems } from "../MessageList";
import type { TranscriptEntry } from "../../../types/api";

describe("resolveFormRequestBranch", () => {
  it("is 'pending' regardless of answered/dismissed — the floating overlay owns this one", () => {
    expect(resolveFormRequestBranch({ isPending: true, isDismissed: true, isAnswered: true })).toBe("pending");
    expect(resolveFormRequestBranch({ isPending: true, isDismissed: false, isAnswered: false })).toBe("pending");
  });

  it("is 'dismissed' when declined and not pending, regardless of answered", () => {
    expect(resolveFormRequestBranch({ isPending: false, isDismissed: true, isAnswered: true })).toBe("dismissed");
    expect(resolveFormRequestBranch({ isPending: false, isDismissed: true, isAnswered: false })).toBe("dismissed");
  });

  it("is 'answered' when not pending, not dismissed, and answers exist", () => {
    expect(resolveFormRequestBranch({ isPending: false, isDismissed: false, isAnswered: true })).toBe("answered");
  });

  // The core regression: previously-implicit "not pending ⟹ answered" is
  // gone — an explicit `isAnswered: false` now falls through to a state
  // that keeps the form answerable instead of freezing it blank.
  it("is 'interactive' — the fix — when not pending, not dismissed, and NOT answered", () => {
    expect(resolveFormRequestBranch({ isPending: false, isDismissed: false, isAnswered: false })).toBe("interactive");
  });
});

describe("resolveInlineAsyncFormTarget", () => {
  it("routes a plain agent channel to the agent-scoped endpoints", () => {
    expect(resolveInlineAsyncFormTarget("agent-1")).toEqual({ kind: "agent", agentId: "agent-1" });
  });

  it("routes a project channel (selectedAgentId = 'project:{id}') to the project-scoped endpoints", () => {
    expect(resolveInlineAsyncFormTarget("project:proj-1")).toEqual({ kind: "project", projectId: "proj-1" });
  });

  it("returns 'none' when no channel is selected", () => {
    expect(resolveInlineAsyncFormTarget(null)).toEqual({ kind: "none" });
  });
});

describe("buildMessageItems — form_request entries hidden_from_user never reach the inline branch", () => {
  // Both the sync (AskUserQuestionWithForm) and async (posted form tool)
  // write paths now persist their own `form_request` transcript entry with
  // `hidden_from_user: true` (see `types/form.ts`'s `AsyncFormRequestMeta`
  // doc and `form_events.rs::form_request_entry`/`persist_posted_form`) —
  // neither is ever shown as a transcript message; the visible surface is
  // the `pending_forms` snapshot pointer (composer overlay for sync,
  // `ChatView`'s pinned nudge card for async — see `pendingAsyncFormMeta`).
  // This pins that the visible-item builder drops such an entry before
  // `resolveFormRequestBranch` ever runs, so the "is it sync or async"
  // question the render branch would otherwise need to ask is structurally
  // moot.
  it("produces zero items for a hidden_from_user form_request entry", () => {
    const entries: TranscriptEntry[] = [
      {
        ts: "2026-01-01T00:00:00Z",
        role: { agent: "agent-1" },
        content: "",
        event_type: "form_request",
        metadata: { form_id: "sync-form-1", spec: { form_id: "sync-form-1", title: "T", fields: [] }, mode: "async" },
        hidden_from_user: true,
      },
    ];
    const result = buildMessageItems(entries);
    expect(result.items).toHaveLength(0);
  });

  // Kept as a generic (not "the async path" — that write site is hidden too
  // now) proof that a non-hidden `form_request` entry, if one ever existed,
  // would still reach the inline branch — i.e. the filtering is driven
  // purely by `hidden_from_user`, not by `mode`/event shape.
  it("produces one item for the same entry when it isn't hidden", () => {
    const entries: TranscriptEntry[] = [
      {
        ts: "2026-01-01T00:00:00Z",
        role: { agent: "agent-1" },
        content: "",
        event_type: "form_request",
        metadata: { form_id: "async-form-1", spec: { form_id: "async-form-1", title: "T", fields: [] }, mode: "async" },
        hidden_from_user: false,
      },
    ];
    const result = buildMessageItems(entries);
    expect(result.items).toHaveLength(1);
    expect(result.items[0].entry.event_type).toBe("form_request");
  });
});
