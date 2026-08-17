import { useCallback, useLayoutEffect, useRef, useState } from "react";

/**
 * Runtime-measures how many nav rail rows fit in the vertical space above
 * the rail's bottom utility cluster, collapsing the tail into a "More" row
 * when they don't all fit.
 *
 * The caller's always-rendered pinned row (e.g. "Home") is NOT part of
 * `items` — pass its own height into account by attaching `pinnedRowRef` to
 * it. This hook only decides how many of the *remaining* `items` fit
 * alongside that pinned row.
 *
 * `containerRef` must go on a wrapper whose height is driven purely by the
 * surrounding flex layout (e.g. `flex-1 min-h-0 overflow-hidden`), NOT by
 * its own children — measuring a box that grows/shrinks with the very items
 * we're deciding to show would create a ResizeObserver feedback loop.
 *
 * Row height is measured off `pinnedRowRef` at runtime rather than assumed
 * as a hardcoded pixel constant, so it stays correct across zoom/font
 * scaling/DPI.
 */
export function useNavRailOverflow<T>(items: readonly T[], gapPx: number = 12) {
  const containerRef = useRef<HTMLDivElement>(null);
  const pinnedRowRef = useRef<HTMLDivElement>(null);
  // Default to "everything fits" so nothing collapses before the first
  // measurement — useLayoutEffect below corrects this synchronously before
  // paint, so there's no visible flash of an incorrect count.
  const [visibleCount, setVisibleCount] = useState(items.length);

  const recompute = useCallback(() => {
    const container = containerRef.current;
    const pinnedRow = pinnedRowRef.current;
    if (!container || !pinnedRow) return;

    const rowHeight = pinnedRow.getBoundingClientRect().height;
    const availableHeight = container.clientHeight;
    if (rowHeight <= 0 || availableHeight <= 0) return;

    // Accumulate-loop, not a division formula — avoids off-by-one gap math.
    // `reserveForMore` keeps one extra row+gap held back at every step so a
    // trailing More row is guaranteed to still fit once the loop stops.
    const fitCount = (reserveForMore: boolean) => {
      let used = rowHeight; // the pinned row is always present
      let count = 0;
      for (let i = 0; i < items.length; i++) {
        const withThisItem = used + gapPx + rowHeight;
        const stillNeedsMoreRow = reserveForMore ? gapPx + rowHeight : 0;
        if (withThisItem + stillNeedsMoreRow > availableHeight) break;
        used = withThisItem;
        count++;
      }
      return count;
    };

    // Pass 1: does everything fit with no More row at all?
    const fullCount = fitCount(false);
    if (fullCount === items.length) {
      setVisibleCount(fullCount);
      return;
    }

    // Pass 2: something has to collapse — redo the walk reserving space for
    // the More row itself.
    setVisibleCount(fitCount(true));
  }, [items.length, gapPx]);

  useLayoutEffect(() => {
    recompute();
    const container = containerRef.current;
    if (!container) return;
    const ro = new ResizeObserver(() => {
      requestAnimationFrame(recompute);
    });
    ro.observe(container);
    return () => ro.disconnect();
  }, [recompute]);

  return {
    containerRef,
    pinnedRowRef,
    visibleItems: items.slice(0, visibleCount),
    overflowItems: items.slice(visibleCount),
  };
}
