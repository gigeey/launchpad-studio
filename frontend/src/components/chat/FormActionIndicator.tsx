import { X, RefreshCw, HelpCircle } from "lucide-react";
import type { FormAction } from "../../types/form";

const LABEL: Record<FormAction, string> = {
  cancel: "Question cancelled",
  regenerate: "Asked for a different form",
  other: "Wanted something else",
};

const ICON: Record<FormAction, typeof X> = {
  cancel: X,
  regenerate: RefreshCw,
  other: HelpCircle,
};

/** History-view counterpart to the sync form's action row (Cancel /
 *  Regenerate / Something else) — shown in place of the full read-only form
 *  once an action (rather than a real answer) resolved it, so the transcript
 *  reads as "the operator did X" instead of an oddly blank filled-in form. */
export function FormActionIndicator({ action }: { action: FormAction }) {
  const Icon = ICON[action];
  return (
    <div className="flex justify-center py-[6px] px-[16px]">
      <span
        className="inline-flex items-center gap-[6px] text-[11px] text-[var(--text-tertiary)] bg-[var(--bg-tertiary)] rounded-full px-[10px] py-[4px]"
        data-testid="form-action-indicator"
      >
        <Icon size={12} className="shrink-0 opacity-60" />
        <span>{LABEL[action]}</span>
      </span>
    </div>
  );
}
