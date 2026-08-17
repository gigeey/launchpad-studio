import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  noteTaskStatus,
  getTaskRunStart,
  formatRunElapsed,
  computeTasklistElapsedMs,
} from "./taskTimers";

describe("formatRunElapsed", () => {
  it("formats sub-minute spans as bare seconds", () => {
    expect(formatRunElapsed(0)).toBe("0s");
    expect(formatRunElapsed(45_000)).toBe("45s");
    expect(formatRunElapsed(59_999)).toBe("59s");
  });

  it("formats minutes with zero-padded seconds", () => {
    expect(formatRunElapsed(60_000)).toBe("1m 00s");
    expect(formatRunElapsed(125_000)).toBe("2m 05s");
  });

  it("formats hours with zero-padded minutes", () => {
    expect(formatRunElapsed(3_600_000)).toBe("1h 00m");
    expect(formatRunElapsed(3_780_000)).toBe("1h 03m");
  });

  it("clamps negative spans to 0s", () => {
    expect(formatRunElapsed(-5_000)).toBe("0s");
  });
});

describe("noteTaskStatus / getTaskRunStart", () => {
  beforeEach(() => {
    try {
      localStorage.clear();
    } catch {
      /* node env without localStorage — in-memory cache still applies */
    }
  });

  it("stamps an origin when a task enters in_progress", () => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
    noteTaskStatus("tl-1", "t-1", "in_progress");
    expect(getTaskRunStart("tl-1", "t-1")).toBe(1_000_000);
    vi.useRealTimers();
  });

  it("is idempotent — repeated in_progress keeps the first origin", () => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
    noteTaskStatus("tl-2", "t-2", "in_progress");
    vi.setSystemTime(2_000_000);
    noteTaskStatus("tl-2", "t-2", "in_progress");
    expect(getTaskRunStart("tl-2", "t-2")).toBe(1_000_000);
    vi.useRealTimers();
  });

  it("clears the origin on a terminal transition", () => {
    noteTaskStatus("tl-3", "t-3", "in_progress");
    expect(getTaskRunStart("tl-3", "t-3")).not.toBeNull();
    noteTaskStatus("tl-3", "t-3", "completed");
    expect(getTaskRunStart("tl-3", "t-3")).toBeNull();
  });

  it("resets the origin when a task returns to pending (re-dispatch)", () => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
    noteTaskStatus("tl-4", "t-4", "in_progress");
    noteTaskStatus("tl-4", "t-4", "pending");
    expect(getTaskRunStart("tl-4", "t-4")).toBeNull();
    vi.setSystemTime(5_000_000);
    noteTaskStatus("tl-4", "t-4", "in_progress");
    expect(getTaskRunStart("tl-4", "t-4")).toBe(5_000_000);
    vi.useRealTimers();
  });

  it("scopes origins per tasklist (same task id in two lists is independent)", () => {
    noteTaskStatus("tl-a", "shared", "in_progress");
    expect(getTaskRunStart("tl-b", "shared")).toBeNull();
  });
});

describe("computeTasklistElapsedMs", () => {
  const created = "2026-01-01T00:00:00.000Z";
  const createdMs = Date.parse(created);

  it("measures from created_at to now while running", () => {
    const now = createdMs + 125_000;
    expect(computeTasklistElapsedMs(created, null, true, now)).toBe(125_000);
  });

  it("ignores last_active_at while running (live span beats stale stamp)", () => {
    const lastActive = new Date(createdMs + 10_000).toISOString();
    const now = createdMs + 90_000;
    expect(computeTasklistElapsedMs(created, lastActive, true, now)).toBe(
      90_000,
    );
  });

  it("freezes at last_active_at once the list stops", () => {
    const lastActive = new Date(createdMs + 300_000).toISOString();
    const now = createdMs + 999_000; // wall clock kept moving; must be ignored
    expect(computeTasklistElapsedMs(created, lastActive, false, now)).toBe(
      300_000,
    );
  });

  it("falls back to a zero span when stopped with no last_active_at", () => {
    const now = createdMs + 999_000;
    expect(computeTasklistElapsedMs(created, null, false, now)).toBe(0);
  });

  it("clamps to 0 when last_active_at precedes created_at (clock skew)", () => {
    const lastActive = new Date(createdMs - 5_000).toISOString();
    expect(computeTasklistElapsedMs(created, lastActive, false, createdMs)).toBe(
      0,
    );
  });

  it("returns null when created_at is unparseable", () => {
    expect(computeTasklistElapsedMs("not-a-date", null, true, 1_000)).toBeNull();
  });
});
