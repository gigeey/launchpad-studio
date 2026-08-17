import { ChevronUp } from "lucide-react";
import { twMerge } from "tailwind-merge";

export interface MinimizedFormBarProps {
  /** The pending form's title, shown truncated on one line. */
  title: string;
  /** Called when the bar (or its expand button) is clicked. */
  onExpand: () => void;
  className?: string;
}

/** Renders in place of the chat input when a pending form has been minimized —
 *  same shell sizing as ChatInput (`min-h-[50px] rounded-[14px]`) so the
 *  layout doesn't jump when swapping between the two, but bordered in the
 *  form's own accent green (rather than a neutral border) since a minimized
 *  form is still action-required, not a passive notice. */
export function MinimizedFormBar({ title, onExpand, className }: MinimizedFormBarProps) {
  return (
    <div
      data-testid="minimized-form-bar"
      onClick={onExpand}
      className={twMerge(
        "flex items-center gap-2 min-h-[50px] px-3 py-2.5 rounded-[14px] border border-[#007A59] dark:border-[var(--accent)] bg-[var(--bg-secondary)] text-[13px] text-[var(--text-secondary)] cursor-pointer",
        className,
      )}
    >
      <span className="flex-1 min-w-0 truncate text-left text-[var(--text-primary)] font-medium">{title}</span>
      <button
        type="button"
        onClick={(e) => {
          e.stopPropagation();
          onExpand();
        }}
        data-testid="minimized-form-expand-btn"
        aria-label="Expand form"
        title="Expand form"
        className="shrink-0 w-[26px] h-[26px] flex items-center justify-center rounded-[6px] text-[var(--text-primary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
      >
        <ChevronUp className="w-[13px] h-[13px]" />
      </button>
    </div>
  );
}
