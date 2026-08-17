import { X } from "lucide-react";

export function FormDismissedIndicator() {
  return (
    <div className="flex justify-center py-[6px] px-[16px]">
      <span
        className="inline-flex items-center gap-[6px] text-[11px] text-[var(--text-tertiary)] bg-[var(--bg-tertiary)] rounded-full px-[10px] py-[4px]"
        data-testid="form-dismissed-indicator"
      >
        <X size={12} className="shrink-0 opacity-60" />
        <span>Form declined</span>
      </span>
    </div>
  );
}
