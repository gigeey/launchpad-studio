import { twMerge } from "tailwind-merge";

/** A single shimmering placeholder block — the shared visual unit both skeletons below are built from. */
function ShimmerBar({ className, style }: { className?: string; style?: React.CSSProperties }) {
  return <div className={twMerge("rounded-[6px] bg-[var(--bg-hover)] animate-pulse", className)} style={style} />;
}

/**
 * Placeholder for a scrollable list of rows (chat/team/project/task lists in
 * their respective sidebars) — an avatar-shaped block plus two text lines,
 * repeated. Only ever shown via ContentGate on a genuinely slow/cold load.
 */
export function SidebarListSkeleton({ rows = 6 }: { rows?: number }) {
  return (
    <div className="flex flex-col gap-[6px] px-[4px] pt-[4px]" aria-hidden>
      {Array.from({ length: rows }).map((_, i) => (
        <div key={i} className="flex items-start gap-3 p-[12px]">
          <ShimmerBar className="w-[40px] h-[40px] rounded-[12px] flex-shrink-0" />
          <div className="flex-1 flex flex-col gap-[6px] pt-[2px]">
            <ShimmerBar className="h-[13px]" style={{ width: "55%" }} />
            <ShimmerBar className="h-[11px]" style={{ width: "80%" }} />
          </div>
        </div>
      ))}
    </div>
  );
}

/**
 * Placeholder for a kanban-style board (Tasks view) — a few column-shaped
 * groups of shimmering cards.
 */
export function BoardSkeleton({ columns = 3 }: { columns?: number }) {
  return (
    <div className="flex gap-4" aria-hidden>
      {Array.from({ length: columns }).map((_, i) => (
        <div key={i} className="flex-1 min-w-[220px] flex flex-col gap-2">
          <ShimmerBar className="h-[14px]" style={{ width: "40%" }} />
          <ShimmerBar className="h-[64px] rounded-[10px]" />
          <ShimmerBar className="h-[64px] rounded-[10px]" />
        </div>
      ))}
    </div>
  );
}

/**
 * Placeholder for a responsive grid of workflow tiles (Tasks → Workflows
 * view) — card-shaped shimmering blocks laid out in the same container-query
 * grid (`@container` + `@xl:`/`@5xl:` column breakpoints) the real tiles use,
 * plus a shimmer bar matching the search bar above the grid, so there's no
 * layout shift when the skeleton swaps for real content.
 */
export function WorkflowTilesSkeleton({ count = 6 }: { count?: number }) {
  return (
    <div className="@container" aria-hidden>
      <ShimmerBar className="h-[44px] rounded-2xl mb-4" />
      <div className="grid grid-cols-1 @xl:grid-cols-2 @5xl:grid-cols-3 gap-4">
        {Array.from({ length: count }).map((_, i) => (
          <div
            key={i}
            className="rounded-xl border border-[var(--border-secondary)] px-4 py-[14px] flex flex-col gap-2"
          >
            <ShimmerBar className="h-[16px]" style={{ width: "60%" }} />
            <ShimmerBar className="h-[13px]" style={{ width: "92%" }} />
            <ShimmerBar className="h-[13px]" style={{ width: "70%" }} />
            <ShimmerBar className="h-[13px] mt-2" style={{ width: "45%" }} />
          </div>
        ))}
      </div>
    </div>
  );
}

/**
 * Placeholder for a message thread (chat/team views) — a handful of
 * alternating left/right bubble-shaped blocks of varying width.
 */
export function MessageThreadSkeleton() {
  const widths = [62, 40, 78, 50];
  return (
    <div className="flex flex-col gap-[16px] px-[24px] pt-[24px] flex-1" aria-hidden>
      {widths.map((w, i) => (
        <div key={i} className={twMerge("flex", i % 2 === 0 ? "justify-start" : "justify-end")}>
          <ShimmerBar className="h-[48px] rounded-[14px]" style={{ width: `${w}%`, maxWidth: 420 }} />
        </div>
      ))}
    </div>
  );
}
