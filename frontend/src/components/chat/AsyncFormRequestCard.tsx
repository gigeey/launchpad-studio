import { AskUserQuestionForm } from "./AskUserQuestionForm";
import type { AsyncFormRequestMeta, FormAction, FormAnswerMap, FormRequestPayload, WithdrawnFormDraftEntry } from "../../types/form";

export interface AsyncFormRequestCardProps {
  meta: AsyncFormRequestMeta;
  agentId: string;
  /** Supplied by the caller — no-op when undefined. */
  onSubmit?: (answers: FormAnswerMap) => Promise<void>;
  /** Forwarded straight through to `AskUserQuestionForm`'s `onAction` — the
   *  card's only exit mechanism (Cancel / Regenerate / Something else). This
   *  used to be a separate `onDismiss` prop rendering its own standalone
   *  Decline button alongside a `onAction`-less form; that left two parallel
   *  exit APIs on one component. Decline is now just what a caller's
   *  `"cancel"` branch does — every render site collapses all three actions
   *  onto the same dismiss its old `onDismiss` used, since nothing is
   *  suspended server-side for an async form to differentiate "regenerate"
   *  or "other" against. Supplied by the caller — the row doesn't render
   *  when undefined. */
  onAction?: (action: FormAction, note?: string) => void | Promise<void>;
  /** Forwarded to the underlying `AskUserQuestionForm` — renders its minimize
   *  button in the header when set. */
  onMinimize?: () => void;
  /** Forwarded straight through to `AskUserQuestionForm`'s `onWithdraw` — see
   *  its prop doc. The nudge-card caller (`ChatView.tsx`) keys this card by
   *  `meta.form_id` for the same newest-wins-remount reason as the sync
   *  overlay, so an async form superseding another async form on the same
   *  slot tears this instance down the same way; without this forwarded,
   *  whatever the operator had typed would vanish with no trace instead of
   *  reaching `FormWithdrawnIndicator`. */
  onWithdraw?: (formId: string, draft: WithdrawnFormDraftEntry[]) => void;
}

export function AsyncFormRequestCard({ meta, agentId, onSubmit, onAction, onMinimize, onWithdraw }: AsyncFormRequestCardProps) {
  const form: FormRequestPayload = {
    form_id: meta.spec.form_id,
    agent_id: agentId,
    session_id: "",
    title: meta.spec.title,
    intro: meta.spec.intro ?? undefined,
    fields: meta.spec.fields,
  };

  return (
    <div className="py-[4px]">
      <AskUserQuestionForm
        agentId={agentId}
        form={form}
        onSubmit={onSubmit ?? (async () => { })}
        onAction={onAction}
        onMinimize={onMinimize}
        onWithdraw={onWithdraw}
      />
    </div>
  );
}
