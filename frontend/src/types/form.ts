export interface FormOptionDef {
  id: string;
  label: string;
  description?: string;
}

export interface FormFieldDef {
  id: string;
  kind: "checkbox" | "radio" | "text" | "textarea" | "file";
  label: string;
  description?: string;
  required: boolean;
  options?: FormOptionDef[];
  placeholder?: string;
  max_files?: number;
  accept?: string;
}

export interface FormRequestPayload {
  form_id: string;
  agent_id: string;
  session_id: string;
  title: string;
  intro?: string;
  fields: FormFieldDef[];
  /** SSE envelope's `thread_id`, merged in by `parsePayloadData` (see
   *  `useSSE.ts`) — absent for the default thread, mirroring the backend's
   *  event tagging. Threads `pendingFormByAgent`'s bucket key. */
  thread_id?: string;
  /** Set by `hydratePendingSyncFormsFromAgents` from the snapshot's
   *  `PendingForm.orphaned` when a persisted sync form's owning run/session
   *  did not survive a process restart — see `types/api.ts`'s `PendingForm`
   *  for why this can never come back. A form delivered live over SSE never
   *  has this set. `AskUserQuestionForm` renders a form with this set as a
   *  fixed "the run waiting on this answer ended" message instead of the
   *  interactive fields — no Submit/action footer at all, not merely a
   *  disabled one, since nothing on the other end could ever receive an
   *  answer. */
  orphaned?: boolean;
}

export interface FormAnswerValue {
  kind: "text" | "selections" | "files";
  value?: string;
  values?: string[];
  attachment_ids?: string[];
}

export type FormAnswerMap = Record<string, FormAnswerValue>;

/** A non-submit action from the form UI's action row — the operator clicked
 *  Cancel / Regenerate / Something else instead of filling in and submitting
 *  the form. Delivered through the same submit endpoint as a real answer;
 *  see `submitFormAnswer` in `lib/api.ts`. Sync forms only (see
 *  `AskUserQuestionForm`'s `onAction` prop docs for why async forms don't
 *  get this). */
export type FormAction = "cancel" | "regenerate" | "other";

/** One field's worth of draft the operator had typed into a sync form that
 *  got superseded (newest-wins slot supersession) before they could submit
 *  it — see `AskUserQuestionForm`'s `onWithdraw` prop and
 *  `chatStore.withdrawnFormDrafts`. `value` is already the human-readable
 *  form (typed text as-is; a checkbox/radio's chosen option LABELS,
 *  comma-joined) rather than raw ids, since the only consumer
 *  (`FormWithdrawnIndicator`) has no access to the withdrawn form's field
 *  defs to re-derive labels from ids after the fact — this is captured
 *  once, at the moment of supersession, while those defs are still in
 *  scope. File fields are never captured (an uploaded attachment is a
 *  server object, not typed text, so there's nothing to "recover"). */
export interface WithdrawnFormDraftEntry {
  fieldId: string;
  label: string;
  value: string;
}

// Async transcript entry metadata — matches backend form_events shapes exactly.

export interface AsyncFormSpec {
  form_id: string;
  title: string;
  intro?: string | null;
  fields: FormFieldDef[];
}

/** SSE payload for the `form_posted` event — mirrors `FormRequestPayload`'s
 *  own title/intro/fields shape (backend: `AgentEventPayload::FormPosted`'s
 *  `spec: FormSpecEventPayload`), so a form posted asynchronously carries
 *  the same field-level data a sync `form_request` event does. `useSSE.ts`
 *  wraps `spec` in the `{form_id, spec, mode: "async"}` envelope
 *  (`PendingFormRequestMeta`) to upsert `AgentSnapshot.pending_forms`. */
export interface FormPostedPayload {
  form_id: string;
  spec: AsyncFormSpec;
  /** SSE envelope's `thread_id`, merged in by `parsePayloadData` — see
   *  `FormRequestPayload.thread_id` for the same convention. */
  thread_id?: string;
}

export interface AsyncFormRequestMeta {
  form_id: string;
  spec: AsyncFormSpec;
  mode: "async";
}

/** Same envelope as `AsyncFormRequestMeta` (form_id/spec/mode), but `mode` may
 *  also be `"sync"` — the shape of `AgentSnapshot.pending_forms[].spec` now
 *  that sync `AskUserQuestionWithForm` calls are persisted into that same
 *  structure for UI reconstruction after a reload (see `ao-engine-tools-runner`'s
 *  `LiveFormBridge::ask_form`). Transcript `form_request` *entries* stay
 *  `AsyncFormRequestMeta` (mode always `"async"`) since a sync form's own
 *  entry is written `hidden_from_user: true` and never reaches the message
 *  list — only `pending_forms` needs the wider type. */
export interface PendingFormRequestMeta {
  form_id: string;
  spec: AsyncFormSpec;
  mode: "async" | "sync";
}

export interface AsyncFormAnswerMeta {
  form_id: string;
  values: Record<string, unknown>;
  /** Snapshot of the answered form's own spec (title/intro/fields), taken by
   *  the backend at answer time (see `FormAnswerMeta::spec` —
   *  crates/ao-engine-tools-core/src/form_events.rs) — self-contained, so
   *  the UI can render this entry as the SAME interactive form the operator
   *  answered, disabled and filled in, with no lookup against a
   *  `form_request` entry or the live `pending_forms` registry (both can
   *  be long gone by the time this renders — forms get superseded and
   *  withdrawn). `undefined` for entries persisted before this field
   *  existed; `MessageList` falls back to a plain values list for those. */
  spec?: AsyncFormSpec;
}

export interface AsyncFormDismissedMeta {
  form_id: string;
}
