/**
 * Regression suite: `resolveChannelFallbackFormId` — the gate on the
 * project-channel fallback path (`MessageList`'s `channelFallbackFormId`
 * input, folded into its `pendingFormId` derivation). `pendingAsyncFormIdByChannel`
 * is a flat per-channel-key map; reading it by raw `selectedAgentId` with no
 * gate meant any channel key — not just a genuine `project:{id}` channel —
 * could pick up whatever entry happened to sit under that key, surfacing the
 * wrong form in the wrong place. The fix restricts the lookup to channels
 * that are actually project channels.
 *
 * Split out of a sibling test file that otherwise covered the inline
 * placeholder's staleness-gated form_id derivation — removed once the async
 * `form_request` transcript entry it reasoned about became permanently
 * hidden and the composer gate stopped auto-releasing on staleness. This
 * function's own coverage is unrelated to that removal and stays live.
 */

import { describe, it, expect } from "vitest";
import { resolveChannelFallbackFormId } from "../MessageList";

describe("resolveChannelFallbackFormId — gates the project-channel fallback", () => {
  it("resolves the entry for a genuine project channel", () => {
    expect(
      resolveChannelFallbackFormId("project:proj-1", { "project:proj-1": "form-1" }),
    ).toBe("form-1");
  });

  it("does not resolve a form for a plain agent channel, even if the map happens to hold an entry under that exact key", () => {
    // `pendingAsyncFormIdByChannel` is only ever written with `project:{id}`
    // keys in the real app, but the gate must not rely on that as an
    // unenforced invariant — an agent channel must never pick up a
    // project-shaped (or any other) entry it doesn't own.
    expect(
      resolveChannelFallbackFormId("agent-1", { "agent-1": "form-that-does-not-belong-here" }),
    ).toBeNull();
  });

  it("does not resolve a different project channel's form", () => {
    expect(
      resolveChannelFallbackFormId("project:proj-2", { "project:proj-1": "form-1" }),
    ).toBeNull();
  });

  it("returns null when selectedAgentId is null", () => {
    expect(resolveChannelFallbackFormId(null, { "project:proj-1": "form-1" })).toBeNull();
  });

  it("returns null when the project channel has no pending fallback form", () => {
    expect(resolveChannelFallbackFormId("project:proj-1", {})).toBeNull();
  });
});
