import { useCallback } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { twMerge } from "tailwind-merge";
import { useChatStore, pendingSyncFormForThread, isFormMinimized } from "../../stores/chatStore";
import { submitFormAnswer } from "../../lib/api";
import { AskUserQuestionForm } from "./AskUserQuestionForm";
import type { FormAnswerMap, FormAction, WithdrawnFormDraftEntry } from "../../types/form";

/**
 * Sync-form overlay for any chat channel, keyed by the channel's conversation
 * key — a raw agent id for the personal chat, or a synthetic key like
 * `project:{id}` for project channels.
 *
 * Renders the channel's pending AskUserQuestionWithForm request (if any) and
 * submits the operator's answers back to the *real* agent carried inside the
 * form payload — the form-bridge registry is keyed by agent id, so answer
 * delivery is independent of which channel surfaced the form. Callers should
 * hide their composer while a form is pending (read `pendingFormByAgent` from
 * the chat store) and decide positioning via `className`.
 *
 * `channelKey` has no thread concept for these channels (project/copilot
 * chats have no ThreadTabStrip), so it resolves against the default-thread
 * bucket — same collapse rule `pendingSyncFormForThread` uses everywhere.
 */
export function PendingFormOverlay({
  channelKey,
  className,
}: {
  channelKey: string;
  className?: string;
}) {
  const pendingForm = useChatStore((s) => pendingSyncFormForThread(s.pendingFormByAgent, channelKey, undefined));
  const formMinimized = useChatStore((s) => isFormMinimized(s.minimizedFormByKey, channelKey, pendingForm?.thread_id));
  const setFormMinimized = useChatStore((s) => s.setFormMinimized);
  const clearPendingForm = useChatStore((s) => s.clearPendingForm);
  const addFormAnswerEntry = useChatStore((s) => s.addFormAnswerEntry);
  const stashWithdrawnFormDraft = useChatStore((s) => s.stashWithdrawnFormDraft);

  const handleWithdraw = useCallback(
    (formId: string, draft: WithdrawnFormDraftEntry[]) => stashWithdrawnFormDraft(formId, draft),
    [stashWithdrawnFormDraft],
  );

  const handleSubmit = useCallback(
    async (answers: FormAnswerMap) => {
      // `AskUserQuestionForm` never renders a submit path for an orphaned
      // form (see its `orphaned` prop handling), so this should be
      // unreachable — guarded anyway so there is no route to a real POST
      // even if a caller invoked this directly.
      if (!pendingForm || pendingForm.orphaned) return;
      await submitFormAnswer(pendingForm.agent_id, {
        form_id: pendingForm.form_id,
        answers,
      });
      clearPendingForm(channelKey, pendingForm.thread_id);
      addFormAnswerEntry(channelKey, { form: pendingForm, answers });
    },
    [pendingForm, channelKey, clearPendingForm, addFormAnswerEntry],
  );

  // Action row (Cancel / Regenerate / Something else) — same delivery path as
  // a submission, just with `action` instead of `answers`. See
  // `AskUserQuestionForm`'s `onAction` prop docs.
  const handleAction = useCallback(
    async (action: FormAction, note?: string) => {
      if (!pendingForm || pendingForm.orphaned) return;
      await submitFormAnswer(pendingForm.agent_id, {
        form_id: pendingForm.form_id,
        answers: {},
        action,
        note,
      });
      clearPendingForm(channelKey, pendingForm.thread_id);
      addFormAnswerEntry(channelKey, { form: pendingForm, answers: {}, action, note });
    },
    [pendingForm, channelKey, clearPendingForm, addFormAnswerEntry],
  );

  return (
    <AnimatePresence>
      {pendingForm && (
        <motion.div
          // Stable — this is `AnimatePresence`'s own presence identity for
          // "a form is pending on this slot at all", so the slide-in/out
          // animation plays once per appear/disappear of the overlay, not on
          // every form-to-form swap (that would key AnimatePresence itself
          // into treating the swap as an exit+enter, leaving both the old
          // and new forms in the DOM together for the transition's
          // duration — exactly what broke the "form B renders empty" and
          // "form A's draft got stashed" assertions here when this was
          // briefly keyed by `pendingForm.form_id` instead). The remount
          // that actually needs to happen per form_id lives one level down,
          // on `AskUserQuestionForm` itself — see its `key` below.
          key="pending-form-overlay"
          initial={{ opacity: 0, y: 8 }}
          animate={{ opacity: 1, y: 0 }}
          exit={{ opacity: 0, y: 8 }}
          transition={{ duration: 0.18, ease: "easeOut" }}
          // Minimizing must hide, never unmount — `AskUserQuestionForm` keeps
          // in-progress answers in local component state, so gating it out of
          // the tree here (or off `AnimatePresence`) would silently destroy
          // whatever the operator already typed. `hidden` only toggles
          // display; the form stays mounted the whole time.
          className={formMinimized ? twMerge("hidden", className) : className}
        >
          <AskUserQuestionForm
            // Keyed by the form's own id — NOT a static string — so a newer
            // form superseding a still-pending one on this same
            // (newest-wins) slot forces a full remount instead of React
            // reusing this instance. `AskUserQuestionForm` seeds its answer
            // state via lazy `useState` initializers that only run on first
            // mount; reusing the instance across a form swap would
            // otherwise leave the new form rendering with the old one's
            // typed answers still in state (or, worse, submittable as
            // answers to fields they were never written for, whenever the
            // two forms' field ids collide). Ordinary React key —  not
            // tracked by `AnimatePresence` — so this remount is synchronous,
            // with no exit animation and no old/new coexistence period. See
            // `onWithdraw` below for what happens to the old draft when this
            // remount fires.
            key={pendingForm.form_id}
            agentId={pendingForm.agent_id}
            form={pendingForm}
            onSubmit={handleSubmit}
            onAction={handleAction}
            onMinimize={() => setFormMinimized(channelKey, pendingForm.thread_id, true)}
            onWithdraw={handleWithdraw}
          />
        </motion.div>
      )}
    </AnimatePresence>
  );
}
