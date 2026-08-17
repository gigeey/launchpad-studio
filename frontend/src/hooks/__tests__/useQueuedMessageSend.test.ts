import { describe, it, expect } from "vitest";
import { bootstrapQueueState, combineQueuedParts, resolveQueueTransition, type QueuedPart } from "../useQueuedMessageSend";
import type { Attachment } from "../../types/api";

function part(content: string, extra: Partial<QueuedPart> = {}): QueuedPart {
  return { content, ...extra };
}

function attachment(id: string): Attachment {
  return {
    id,
    file_path: `/tmp/${id}`,
    mime_type: "text/plain",
    original_filename: id,
    size_bytes: 0,
    attachment_type: "document",
  };
}

describe("combineQueuedParts", () => {
  it("joins content with a blank line and omits absent attachment fields", () => {
    const [content, attachmentIds, attachments] = combineQueuedParts([part("first"), part("second")]);
    expect(content).toBe("first\n\nsecond");
    expect(attachmentIds).toBeUndefined();
    expect(attachments).toBeUndefined();
  });

  it("flattens attachment ids and attachments across parts in order", () => {
    const [, attachmentIds, attachments] = combineQueuedParts([
      part("first", { attachmentIds: ["a1"], attachments: [attachment("a1")] }),
      part("second", { attachmentIds: ["a2", "a3"] }),
    ]);
    expect(attachmentIds).toEqual(["a1", "a2", "a3"]);
    expect(attachments?.map((a) => a.id)).toEqual(["a1"]);
  });

  it("returns an empty string for an empty queue rather than throwing", () => {
    const [content, attachmentIds, attachments] = combineQueuedParts([]);
    expect(content).toBe("");
    expect(attachmentIds).toBeUndefined();
    expect(attachments).toBeUndefined();
  });
});

describe("resolveQueueTransition", () => {
  it("does nothing while the conversation stays busy", () => {
    const queue = [part("follow-up")];
    const result = resolveQueueTransition({ key: "a:t", busy: true, queue }, { key: "a:t", busy: true });
    expect(result.action).toBe("none");
    expect(result.nextQueue).toBe(queue);
  });

  it("flushes the queue once the same conversation's run finishes", () => {
    const queue = [part("one"), part("two")];
    const result = resolveQueueTransition({ key: "a:t", busy: true, queue }, { key: "a:t", busy: false });
    expect(result.action).toBe("flush");
    expect(result.content).toBe("one\n\ntwo");
    expect(result.nextQueue).toEqual([]);
  });

  it("does nothing when the run ends with an empty queue", () => {
    const result = resolveQueueTransition({ key: "a:t", busy: true, queue: [] }, { key: "a:t", busy: false });
    expect(result.action).toBe("none");
    expect(result.nextQueue).toEqual([]);
  });

  it("does nothing when busy flips true->true or false->true with items still queued (never send early)", () => {
    const queue = [part("pending")];
    const stillBusy = resolveQueueTransition({ key: "a:t", busy: true, queue }, { key: "a:t", busy: true });
    expect(stillBusy.action).toBe("none");
    const goingBusy = resolveQueueTransition({ key: "a:t", busy: false, queue }, { key: "a:t", busy: true });
    expect(goingBusy.action).toBe("none");
  });

  it("abandons a non-empty queue attributed to the outgoing key when the conversation identity changes", () => {
    const queue = [part("orphaned")];
    const result = resolveQueueTransition({ key: "agent-a:thread-1", busy: true, queue }, { key: "agent-b:thread-1", busy: false });
    expect(result.action).toBe("abandon");
    expect(result.prevKey).toBe("agent-a:thread-1");
    expect(result.content).toBe("orphaned");
    expect(result.nextQueue).toEqual([]);
  });

  it("does nothing on an identity change with nothing queued", () => {
    const result = resolveQueueTransition({ key: "agent-a:thread-1", busy: true, queue: [] }, { key: "agent-b:thread-1", busy: false });
    expect(result.action).toBe("none");
    expect(result.nextQueue).toEqual([]);
  });
});

describe("bootstrapQueueState", () => {
  // Pins the fix for the queue getting force-sent when the surface holding
  // it (ChatView) unmounts on navigation — see `queuedSendStore`'s docstring.
  // A restored non-empty queue must be treated as "was busy" so the first
  // `resolveQueueTransition` call after remount can decide correctly, rather
  // than always sending on sight.

  it("treats a restored non-empty queue as busy, ignoring the live busy flag", () => {
    const queue = [part("survived a navigation away and back")];
    const state = bootstrapQueueState("a:t", queue, /* liveBusy */ false);
    expect(state).toEqual({ key: "a:t", busy: true, queue });
  });

  it("combined with resolveQueueTransition, flushes a restored queue once the run has actually finished", () => {
    const queue = [part("survived a navigation away and back")];
    const bootstrapped = bootstrapQueueState("a:t", queue, false);
    // The run finished while the surface was unmounted — the live busy flag
    // reads false on remount, same as what bootstrapQueueState observed.
    const result = resolveQueueTransition(bootstrapped, { key: "a:t", busy: false });
    expect(result.action).toBe("flush");
    expect(result.content).toBe("survived a navigation away and back");
  });

  it("combined with resolveQueueTransition, leaves a restored queue queued while the run is still active", () => {
    const queue = [part("survived a navigation away and back")];
    const bootstrapped = bootstrapQueueState("a:t", queue, false);
    // The run is still going — live busy reads true on remount even though
    // bootstrapQueueState was called with liveBusy=false at the time.
    const result = resolveQueueTransition(bootstrapped, { key: "a:t", busy: true });
    expect(result.action).toBe("none");
    expect(result.nextQueue).toBe(queue);
  });

  it("falls back to the live busy flag when there's nothing persisted to restore", () => {
    expect(bootstrapQueueState("a:t", [], true)).toEqual({ key: "a:t", busy: true, queue: [] });
    expect(bootstrapQueueState("a:t", [], false)).toEqual({ key: "a:t", busy: false, queue: [] });
  });
});
