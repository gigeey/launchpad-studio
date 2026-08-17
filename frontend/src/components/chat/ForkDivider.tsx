import { GitBranch } from "lucide-react";

/** Marks the boundary between a branch thread's inherited (pre-fork)
 *  messages and its own post-fork turns. Rendered once, right before the
 *  first message whose `ts` falls after the thread's `history_floor_ts` —
 *  see `buildMessageItems`'s `historyFloorTs` param in `MessageList.tsx`. */
export function ForkDivider() {
  return (
    <div className="flex items-center gap-[8px] py-[8px] mb-[4px] px-[16px]">
      <div className="flex-1 h-px bg-[var(--border-primary)]" />
      <span className="flex items-center gap-[4px] px-[10px] py-[4px] rounded-full bg-[var(--bg-hover)] text-[11px] font-medium text-[var(--text-tertiary)] select-none whitespace-nowrap">
        <GitBranch size={11} className="shrink-0 opacity-70" />
        Forked here
      </span>
      <div className="flex-1 h-px bg-[var(--border-primary)]" />
    </div>
  );
}
