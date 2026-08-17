import { useEffect, useState } from "react";

/**
 * Returns the current epoch-ms, re-rendering the caller on a fixed cadence
 * while `active` is true. Drives live elapsed-time counters without each
 * consumer owning its own interval: one panel ticks once per second and every
 * row derives its own elapsed value from the shared `now`.
 *
 * When `active` flips false the interval is torn down so idle panels don't tick.
 */
export function useNow(active: boolean, intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!active) return;
    // Re-baseline immediately so the first frame after activation is current,
    // then tick on the interval.
    setNow(Date.now());
    const id = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(id);
  }, [active, intervalMs]);

  return now;
}
