/**
 * `isPendingFormStillCurrent` — now a plain null check (see its own
 * docstring in `chatStore.ts` for why the staleness gating it used to do,
 * pinned by the now-removed `chatStore.pendingFormLatestInThread.test.ts`,
 * is gone). Kept as a direct pin since it's still exported and still called
 * live (`ChatView.tsx`'s `pendingAsyncFormId`).
 */

import { describe, it, expect } from "vitest";
import { isPendingFormStillCurrent } from "../chatStore";
import type { PendingForm } from "../../types/api";

function makeAsyncPendingForm(overrides: Partial<PendingForm> = {}): PendingForm {
  return {
    thread_id: null,
    form_id: "async-form-1",
    spec: {
      form_id: "async-form-1",
      mode: "async",
      spec: { form_id: "async-form-1", title: "Pick one", fields: [] },
    },
    ...overrides,
  };
}

describe("isPendingFormStillCurrent", () => {
  it("is false for a missing form", () => {
    expect(isPendingFormStillCurrent(undefined)).toBe(false);
  });

  it("is true for any present form", () => {
    expect(isPendingFormStillCurrent(makeAsyncPendingForm())).toBe(true);
  });
});
