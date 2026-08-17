import { useState } from "react";
import { MessageSquareX, ChevronDown, ChevronUp } from "lucide-react";
import { useChatStore } from "../../stores/chatStore";

export interface FormWithdrawnIndicatorProps {
  /** Self-contained text naming the withdrawn question — see the backend's
   *  `form_withdrawn_content` (crates/ao-engine-tools-core/src/form_events.rs).
   *  Falls back to a neutral placeholder for any legacy/malformed entry that
   *  somehow carries no content, so this never renders a blank pill. */
  content: string;
  /** This entry's own `metadata.form_id` (see `form_withdrawn_entry`,
   *  crates/ao-engine-tools-core/src/form_events.rs) — the id of the form
   *  that got withdrawn. Used purely as a client-side lookup key into
   *  `chatStore.withdrawnFormDrafts` (see `AskUserQuestionForm`'s
   *  `onWithdraw` prop): when the operator had typed into the form before
   *  it was superseded, that draft is reachable here. Undefined for any
   *  legacy entry with no id, or simply absent whenever nothing was typed
   *  — both render exactly as this did before draft recovery existed. */
  formId?: string;
}

/** Plain, non-interactive trace line for a pending form that got dropped
 *  because a newer one replaced it on the same thread (see
 *  `SnapshotStore::set_pending_form`'s single-pending-form-per-thread
 *  invariant). Same visual register as `FormDismissedIndicator` — a small
 *  centered pill, never a card, never answerable, and consuming no
 *  `ChatInput` real estate. When a client-side draft was captured for this
 *  form (see `formId` prop doc), an expandable "Recover draft" disclosure
 *  hangs off the pill so the operator can read — and copy — whatever they'd
 *  typed before the newer form pushed it out. */
export function FormWithdrawnIndicator({ content, formId }: FormWithdrawnIndicatorProps) {
  const draft = useChatStore((s) => (formId ? s.withdrawnFormDrafts[formId] : undefined));
  const [expanded, setExpanded] = useState(false);
  const hasDraft = !!draft && draft.length > 0;

  return (
    <div className="flex flex-col items-center py-[6px] px-[16px] gap-[6px]">
      <span
        className="inline-flex items-center gap-[6px] text-[11px] text-[var(--text-tertiary)] bg-[var(--bg-tertiary)] rounded-full px-[10px] py-[4px] max-w-[80%]"
        data-testid="form-withdrawn-indicator"
      >
        <MessageSquareX size={12} className="shrink-0 opacity-60" />
        <span className="truncate">{content || "A pending question was withdrawn."}</span>
        {hasDraft && (
          <button
            type="button"
            onClick={() => setExpanded(v => !v)}
            data-testid="form-withdrawn-draft-toggle"
            className="shrink-0 inline-flex items-center gap-[2px] underline hover:text-[var(--text-secondary)] cursor-pointer"
          >
            {expanded ? "Hide draft" : "Recover draft"}
            {expanded ? <ChevronUp size={11} /> : <ChevronDown size={11} />}
          </button>
        )}
      </span>
      {hasDraft && expanded && (
        <div
          data-testid="form-withdrawn-draft"
          className="max-w-[80%] w-full rounded-[8px] border border-[var(--border-secondary)] bg-[var(--bg-secondary)] px-[12px] py-[8px] text-[12px] text-[var(--text-primary)] flex flex-col gap-[4px]"
        >
          {draft!.map(entry => (
            <div key={entry.fieldId}>
              <span className="font-semibold">{entry.label}: </span>
              <span className="whitespace-pre-wrap select-text">{entry.value}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
