import { describe, it, expect, vi, afterEach } from "vitest";
import { expandOccurrences, firstOccurrenceDriftWarning, hasOccurrenceOnDay } from "../scheduleOccurrences";
import type { ScheduledTask } from "../api";

function makeTask(partial: Partial<ScheduledTask> = {}): ScheduledTask {
  return {
    id: partial.id ?? "task-1",
    agent_id: partial.agent_id ?? "agent-1",
    name: partial.name ?? null,
    is_team: partial.is_team ?? false,
    cron: partial.cron ?? null,
    prompt: partial.prompt ?? "Do the thing",
    working_directory: partial.working_directory ?? null,
    is_recurring: partial.is_recurring ?? false,
    created_at: partial.created_at ?? "2026-01-01T00:00:00Z",
    last_run_at: partial.last_run_at ?? null,
    next_fire_at: partial.next_fire_at ?? null,
    enabled: partial.enabled ?? true,
    expires_at: partial.expires_at ?? null,
    thread_policy: partial.thread_policy ?? "main",
  };
}

const UTC = "UTC";

describe("expandOccurrences — recurring cron", () => {
  it("expands a daily cron once per day across a full month range", () => {
    const task = makeTask({ is_recurring: true, cron: "0 12 * * *" });
    // March 2026 has 31 days; a daily-at-noon cron fires exactly once per day.
    const start = new Date("2026-03-01T00:00:00.000Z");
    const end = new Date("2026-03-31T23:59:59.999Z");

    const occ = expandOccurrences(task, start, end, UTC);

    expect(occ).toHaveLength(31);
    // First and last land on the expected noon-UTC instants.
    expect(occ[0].toISOString()).toBe("2026-03-01T12:00:00.000Z");
    expect(occ[occ.length - 1].toISOString()).toBe("2026-03-31T12:00:00.000Z");
    // Strictly increasing and all inside the range.
    for (let i = 0; i < occ.length; i++) {
      expect(occ[i].getTime()).toBeGreaterThanOrEqual(start.getTime());
      expect(occ[i].getTime()).toBeLessThanOrEqual(end.getTime());
      if (i > 0) expect(occ[i].getTime()).toBeGreaterThan(occ[i - 1].getTime());
    }
  });

  it("includes a fire instant landing exactly on rangeStart", () => {
    const task = makeTask({ is_recurring: true, cron: "0 12 * * *" });
    // rangeStart is exactly a fire instant — it must not be skipped.
    const start = new Date("2026-03-01T12:00:00.000Z");
    const end = new Date("2026-03-03T12:00:00.000Z");

    const occ = expandOccurrences(task, start, end, UTC);

    expect(occ.map((d) => d.toISOString())).toEqual([
      "2026-03-01T12:00:00.000Z",
      "2026-03-02T12:00:00.000Z",
      "2026-03-03T12:00:00.000Z",
    ]);
  });

  it("stops expanding at expires_at", () => {
    const task = makeTask({
      is_recurring: true,
      cron: "0 12 * * *",
      expires_at: "2026-03-10T00:00:00.000Z",
    });
    const start = new Date("2026-03-01T00:00:00.000Z");
    const end = new Date("2026-03-31T23:59:59.999Z");

    const occ = expandOccurrences(task, start, end, UTC);

    // Noon fires on Mar 1..9 fall before the Mar 10 00:00 expiry; Mar 10 12:00
    // is past it and must be excluded.
    expect(occ).toHaveLength(9);
    expect(occ[occ.length - 1].toISOString()).toBe("2026-03-09T12:00:00.000Z");
  });

  it("returns [] for an invalid cron string without throwing", () => {
    const task = makeTask({ is_recurring: true, cron: "not a cron expression" });
    const start = new Date("2026-03-01T00:00:00.000Z");
    const end = new Date("2026-03-31T23:59:59.999Z");

    let occ: Date[] = [];
    expect(() => {
      occ = expandOccurrences(task, start, end, UTC);
    }).not.toThrow();
    expect(occ).toEqual([]);
  });

  it("returns [] when a recurring task has no cron expression", () => {
    const task = makeTask({ is_recurring: true, cron: null });
    const occ = expandOccurrences(
      task,
      new Date("2026-03-01T00:00:00.000Z"),
      new Date("2026-03-31T23:59:59.999Z"),
      UTC,
    );
    expect(occ).toEqual([]);
  });

  it("stays bounded and prompt for an every-minute cron over a huge range", () => {
    const task = makeTask({ is_recurring: true, cron: "* * * * *" });
    // Ten years of per-minute fires would be ~5.2M occurrences without a cap.
    const start = new Date("2020-01-01T00:00:00.000Z");
    const end = new Date("2030-01-01T00:00:00.000Z");

    const before = performance.now();
    const occ = expandOccurrences(task, start, end, UTC);
    const elapsed = performance.now() - before;

    // The runaway guard caps the result at 1000 entries…
    expect(occ.length).toBeLessThanOrEqual(1000);
    expect(occ).toHaveLength(1000);
    // …and returns quickly rather than materializing millions of dates.
    expect(elapsed).toBeLessThan(1000);
    expect(occ[0].toISOString()).toBe("2020-01-01T00:00:00.000Z");
  });
});

describe("expandOccurrences — one-time task", () => {
  it("returns the single fire instant when it lands inside the range", () => {
    const task = makeTask({ is_recurring: false, next_fire_at: "2026-03-15T09:00:00.000Z" });
    const occ = expandOccurrences(
      task,
      new Date("2026-03-01T00:00:00.000Z"),
      new Date("2026-03-31T23:59:59.999Z"),
      UTC,
    );
    expect(occ.map((d) => d.toISOString())).toEqual(["2026-03-15T09:00:00.000Z"]);
  });

  it("returns [] when the fire instant is outside the range", () => {
    const task = makeTask({ is_recurring: false, next_fire_at: "2026-03-15T09:00:00.000Z" });
    const occ = expandOccurrences(
      task,
      new Date("2026-04-01T00:00:00.000Z"),
      new Date("2026-04-30T23:59:59.999Z"),
      UTC,
    );
    expect(occ).toEqual([]);
  });

  it("returns [] when a one-time task has no next_fire_at", () => {
    const task = makeTask({ is_recurring: false, next_fire_at: null });
    const occ = expandOccurrences(
      task,
      new Date("2026-03-01T00:00:00.000Z"),
      new Date("2026-03-31T23:59:59.999Z"),
      UTC,
    );
    expect(occ).toEqual([]);
  });
});

describe("hasOccurrenceOnDay", () => {
  it("is true for a daily cron on every day and false the day before it starts mattering", () => {
    const task = makeTask({ is_recurring: true, cron: "0 12 * * *" });
    expect(hasOccurrenceOnDay(task, "2026-03-15", UTC)).toBe(true);
    expect(hasOccurrenceOnDay(task, "2026-03-16", UTC)).toBe(true);
  });

  it("respects day-of-week restriction", () => {
    // 2026-03-16 is a Monday.
    const task = makeTask({ is_recurring: true, cron: "0 9 * * 1" });
    expect(hasOccurrenceOnDay(task, "2026-03-16", UTC)).toBe(true);
    expect(hasOccurrenceOnDay(task, "2026-03-17", UTC)).toBe(false);
  });

  it("applies the cron day-of-month/day-of-week OR quirk when both are restricted", () => {
    // Fires on the 15th of the month OR any Monday. 2026-03-16 is a Monday
    // (not the 15th); 2026-03-15 is a Sunday (not the 15th... wait it is).
    // Use an unambiguous pair: DOM=20, DOW=Monday. 2026-03-20 is a Friday
    // (matches via DOM), 2026-03-16 is a Monday (matches via DOW), and
    // 2026-03-17 (Tue, not the 20th) matches neither.
    const task = makeTask({ is_recurring: true, cron: "0 9 20 * 1" });
    expect(hasOccurrenceOnDay(task, "2026-03-20", UTC)).toBe(true);
    expect(hasOccurrenceOnDay(task, "2026-03-16", UTC)).toBe(true);
    expect(hasOccurrenceOnDay(task, "2026-03-17", UTC)).toBe(false);
  });

  it("stops matching once the day is entirely past expires_at", () => {
    const task = makeTask({
      is_recurring: true,
      cron: "0 12 * * *",
      expires_at: "2026-03-10T00:00:00.000Z",
    });
    expect(hasOccurrenceOnDay(task, "2026-03-09", UTC)).toBe(true);
    expect(hasOccurrenceOnDay(task, "2026-03-10", UTC)).toBe(false);
  });

  it("still finds an earlier same-day fire when expires_at cuts off a later one", () => {
    const task = makeTask({
      is_recurring: true,
      cron: "0 8,12,16 * * *",
      expires_at: "2026-01-05T14:00:00.000Z",
    });
    // 08:00 and 12:00 fire before the 14:00 cutoff, so the day still counts.
    expect(hasOccurrenceOnDay(task, "2026-01-05", UTC)).toBe(true);
    expect(hasOccurrenceOnDay(task, "2026-01-06", UTC)).toBe(false);
  });

  it("handles one-time tasks via next_fire_at", () => {
    const task = makeTask({ is_recurring: false, next_fire_at: "2026-03-15T09:00:00.000Z" });
    expect(hasOccurrenceOnDay(task, "2026-03-15", UTC)).toBe(true);
    expect(hasOccurrenceOnDay(task, "2026-03-16", UTC)).toBe(false);
  });

  it("returns false for an invalid cron string without throwing", () => {
    const task = makeTask({ is_recurring: true, cron: "not a cron expression" });
    expect(() => hasOccurrenceOnDay(task, "2026-03-15", UTC)).not.toThrow();
    expect(hasOccurrenceOnDay(task, "2026-03-15", UTC)).toBe(false);
  });

  it("is correct across a DST spring-forward boundary in a non-UTC timezone", () => {
    // US DST started 2026-03-08. A 9am-local daily cron must still resolve
    // to the same civil day on both sides of the transition.
    const task = makeTask({ is_recurring: true, cron: "0 9 * * *" });
    expect(hasOccurrenceOnDay(task, "2026-03-07", "America/New_York")).toBe(true);
    expect(hasOccurrenceOnDay(task, "2026-03-08", "America/New_York")).toBe(true);
    expect(hasOccurrenceOnDay(task, "2026-03-09", "America/New_York")).toBe(true);
  });

  it("stays fast for a dense every-minute cron across a full month grid", () => {
    // This is a regression guard for the exact freeze this function was
    // rewritten to fix: hasOccurrenceOnDay used to walk one candidate
    // instant at a time from a day of slack before the target day, which for
    // an unrestricted every-minute cron cost ~1400 steps per call and, used
    // across a 42-cell month grid, measured multiple seconds of blocked
    // render time. It must now resolve in O(1) regardless of cron density.
    const task = makeTask({ is_recurring: true, cron: "* * * * *" });
    const before = performance.now();
    let hits = 0;
    for (let i = 0; i < 42; i++) {
      const d = new Date(Date.UTC(2026, 2, 1 + i));
      const key = d.toISOString().slice(0, 10);
      if (hasOccurrenceOnDay(task, key, "America/New_York")) hits++;
    }
    const elapsed = performance.now() - before;
    expect(hits).toBe(42);
    expect(elapsed).toBeLessThan(500);
  });
});

describe("firstOccurrenceDriftWarning", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("does not throw and does not warn for non-recurring or disabled tasks", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    expect(() =>
      firstOccurrenceDriftWarning(
        makeTask({ is_recurring: false, next_fire_at: "2026-03-15T09:00:00.000Z" }),
        UTC,
      ),
    ).not.toThrow();
    expect(() =>
      firstOccurrenceDriftWarning(
        makeTask({ is_recurring: true, enabled: false, cron: "0 12 * * *", next_fire_at: "2026-03-15T09:00:00.000Z" }),
        UTC,
      ),
    ).not.toThrow();
    expect(warn).not.toHaveBeenCalled();
  });

  it("warns once when the stored next_fire_at grossly disagrees with the client computation", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    // A daily cron whose stored next fire is stuck at the epoch — far from any
    // client-computed upcoming instant — should trip the drift warning.
    const task = makeTask({
      id: "drift-task",
      is_recurring: true,
      enabled: true,
      cron: "0 12 * * *",
      next_fire_at: "1970-01-01T00:00:00.000Z",
    });

    firstOccurrenceDriftWarning(task, UTC);
    firstOccurrenceDriftWarning(task, UTC); // second call must be deduped

    expect(warn).toHaveBeenCalledTimes(1);
    expect(String(warn.mock.calls[0][0])).toContain("drift-task");
  });

  it("does not warn when the stored next_fire_at matches the client computation", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const task = makeTask({
      id: "aligned-task",
      is_recurring: true,
      enabled: true,
      cron: "0 12 * * *",
    });
    // Derive the true next instant from the same util, then store it — the two
    // should agree to well within the 60s tolerance.
    const now = new Date();
    const upcoming = expandOccurrences(task, now, new Date(now.getTime() + 2 * 24 * 60 * 60 * 1000), UTC);
    expect(upcoming.length).toBeGreaterThan(0);
    task.next_fire_at = upcoming[0].toISOString();

    firstOccurrenceDriftWarning(task, UTC);
    expect(warn).not.toHaveBeenCalled();
  });
});
