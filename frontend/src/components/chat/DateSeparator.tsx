interface DateSeparatorProps {
  dateStr: string; // "YYYY-MM-DD"
}

/** Convert a UTC ISO timestamp to a local "YYYY-MM-DD" string. */
export function utcToLocalDate(utcTs: string): string {
  const d = new Date(utcTs);
  const year = d.getFullYear();
  const month = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

export function formatDateLabel(dateStr: string): string {
  const date = new Date(dateStr + "T00:00:00");
  const now = new Date();
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const target = new Date(date.getFullYear(), date.getMonth(), date.getDate());
  const diffDays = Math.round(
    (today.getTime() - target.getTime()) / (1000 * 60 * 60 * 24)
  );

  if (diffDays === 0) return "Today";
  if (diffDays === 1) return "Yesterday";
  if (diffDays < 7) {
    return date.toLocaleDateString("en-US", { weekday: "long" });
  }
  // "Tue, Feb 24" style
  return date.toLocaleDateString("en-US", {
    weekday: "short",
    month: "short",
    day: "numeric",
  });
}

export function DateSeparator({ dateStr }: DateSeparatorProps) {
  const label = formatDateLabel(dateStr);

  return (
    <div className="flex items-center justify-center py-[8px] mb-[4px]">
      <span className="px-[12px] py-[4px] rounded-full bg-[var(--bg-hover)] text-[13px] font-bold text-[var(--text-primary)] select-none">
        {label}
      </span>
    </div>
  );
}
