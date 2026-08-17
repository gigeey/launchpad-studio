import { describe, it, expect } from "vitest";
import { formatPendingFormWaitingLabel } from "./pendingFormLabel";

describe("formatPendingFormWaitingLabel", () => {
  it("names the asking agent and pluralizes the field count", () => {
    expect(formatPendingFormWaitingLabel("Developer", 2)).toBe("Developer is waiting on 2 answers");
  });

  it("singularizes a single-field form", () => {
    expect(formatPendingFormWaitingLabel("Developer", 1)).toBe("Developer is waiting on 1 answer");
  });

  it("falls back to a generic label when no agent name is available", () => {
    expect(formatPendingFormWaitingLabel(undefined, 3)).toBe("The agent is waiting on 3 answers");
    expect(formatPendingFormWaitingLabel(null, 3)).toBe("The agent is waiting on 3 answers");
    expect(formatPendingFormWaitingLabel("   ", 3)).toBe("The agent is waiting on 3 answers");
  });

  it("floors a zero field count at 1 answer", () => {
    expect(formatPendingFormWaitingLabel("Developer", 0)).toBe("Developer is waiting on 1 answer");
  });
});
