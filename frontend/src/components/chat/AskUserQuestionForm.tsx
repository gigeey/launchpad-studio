import { useState, useRef, useCallback, useMemo, useEffect } from "react";
import { Paperclip, Loader2, X, AlertCircle, FileIcon, RefreshCw, HelpCircle, ChevronDown } from "lucide-react";
import type { FormRequestPayload, FormFieldDef, FormAnswerMap, FormAction, WithdrawnFormDraftEntry } from "../../types/form";
import { uploadAttachment, deleteAttachment } from "../../lib/api";
import type { Attachment } from "../../types/api";

type FileEntry = {
  id: string;
  file: File;
  previewUrl: string | null;
  status: "uploading" | "uploaded" | "error";
  serverId: string | null;
  attachment: Attachment | null;
};

export interface AskUserQuestionFormProps {
  agentId: string;
  form: FormRequestPayload;
  onSubmit: (answers: FormAnswerMap) => Promise<void>;
  /** Slim action row rendered above the title — Cancel / Regenerate form /
   *  Something else. This is the form's only exit mechanism (there used to
   *  be a second, standalone Decline button behind a now-removed `onDecline`
   *  prop — see `AsyncFormRequestCard`, the one caller that had it, for why
   *  it was folded in here instead of living side by side with this row).
   *  For a *live, blocking* (sync-mode) form each click resolves the agent's
   *  suspended tool call so it can react, the same way a real submission
   *  does. Async forms (posted via `mode: "async"`) aren't blocking anything
   *  server-side, so there's no per-action backend distinction yet — callers
   *  wire all three to the same dismiss the old standalone Decline button
   *  used. Leave this prop undefined entirely for a caller with no exit
   *  mechanism at all and the row won't render.
   *  `note` is populated for `"other"` (typed into the note box that takes
   *  over the form body, see `otherOpen` below) and omitted for `"cancel"`/
   *  `"regenerate"`, which still fire immediately. */
  onAction?: (action: FormAction, note?: string) => void | Promise<void>;
  /** When provided, render a minimize icon button in the header that calls
   *  this instead of the form's own answer-state handling — collapsing the
   *  form into a bar in the chat input slot is purely a caller-side layout
   *  concern. Left undefined for callers that don't support minimizing
   *  (e.g. the read-only transcript view), in which case the button is
   *  omitted and the header renders exactly as it did before this prop
   *  existed. */
  onMinimize?: () => void;
  /** When true, render a non-interactive, already-answered view: controls are
   *  disabled, the Submit/Cancel footer and the internal scroll are dropped,
   *  and the card is aligned to the user side of the conversation. Used for the
   *  answered form in the transcript. */
  readOnly?: boolean;
  /** Submitted values used to pre-fill the controls in read-only mode. */
  initialAnswers?: FormAnswerMap;
  /** Fired from this instance's UNMOUNT cleanup — never during normal
   *  typing — when it goes away with unsaved answers and no submit/action
   *  ever fired (i.e. a newer form superseded this one on the same slot;
   *  see `ChatView.tsx`'s/`PendingFormOverlay.tsx`'s per-form `key` one
   *  level up, on THIS component — not on the `motion.div`/`AnimatePresence`
   *  wrapping it, so this remount — and this cleanup — fire synchronously,
   *  not gated behind an exit animation completing). A real
   *  Submit/Cancel/Regenerate/"Something else" always suppresses this — see
   *  `finalizedRef` — so callers don't have to distinguish "the operator
   *  finished" from "the operator got cut off" themselves. Omitted entirely
   *  (never called) when nothing was typed, so callers never have to filter
   *  out empty drafts. Not fired in `readOnly`/orphaned mode — both are
   *  already fully non-interactive, so there is never anything to lose. */
  onWithdraw?: (formId: string, draft: WithdrawnFormDraftEntry[]) => void;
}

/** Build the human-readable draft `onWithdraw` hands off — see
 *  `WithdrawnFormDraftEntry`'s doc comment for why labels are resolved
 *  here rather than left as raw ids. Mirrors `handleSubmit`'s per-kind
 *  switch below, minus `file` (nothing to recover for an upload — see the
 *  prop doc) and minus empty/untouched fields (an entry only exists for
 *  something the operator actually typed or picked). */
function buildWithdrawnDraft(
  form: FormRequestPayload,
  textAnswers: Record<string, string>,
  selectionAnswers: Record<string, string[]>,
): WithdrawnFormDraftEntry[] {
  const out: WithdrawnFormDraftEntry[] = [];
  for (const field of form.fields) {
    if (field.kind === "text" || field.kind === "textarea") {
      const value = textAnswers[field.id]?.trim();
      if (value) out.push({ fieldId: field.id, label: field.label, value });
    } else if (field.kind === "checkbox" || field.kind === "radio") {
      const ids = selectionAnswers[field.id];
      if (ids?.length) {
        const value = ids.map(id => field.options?.find(o => o.id === id)?.label ?? id).join(", ");
        out.push({ fieldId: field.id, label: field.label, value });
      }
    }
  }
  return out;
}

/** Seed the text/textarea state from a submitted answer map (read-only view). */
function deriveInitialText(form: FormRequestPayload, answers?: FormAnswerMap): Record<string, string> {
  const out: Record<string, string> = {};
  if (!answers) return out;
  for (const field of form.fields) {
    const a = answers[field.id];
    if (a?.kind === "text" && typeof a.value === "string") out[field.id] = a.value;
  }
  return out;
}

/** Seed the checkbox/radio state from a submitted answer map (read-only view). */
function deriveInitialSelections(form: FormRequestPayload, answers?: FormAnswerMap): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  if (!answers) return out;
  for (const field of form.fields) {
    const a = answers[field.id];
    if (a?.kind === "selections" && Array.isArray(a.values)) out[field.id] = a.values;
  }
  return out;
}

export function AskUserQuestionForm({ agentId, form, onSubmit, onAction, onMinimize, readOnly = false, initialAnswers, onWithdraw }: AskUserQuestionFormProps) {
  // Set by the backend's startup sync-form reaper (see `types/form.ts`'s
  // `FormRequestPayload.orphaned` doc) when this form's owning run ended
  // before it could ever be answered. Folded into the same disabling logic
  // as `readOnly` below so every control freezes, but kept distinct from
  // `readOnly` for layout purposes — an orphaned form is still the live
  // composer overlay, not an already-answered transcript bubble.
  const orphaned = form.orphaned === true;
  const fieldsDisabled = readOnly || orphaned;
  const [textAnswers, setTextAnswers] = useState<Record<string, string>>(() => deriveInitialText(form, initialAnswers));
  const [selectionAnswers, setSelectionAnswers] = useState<Record<string, string[]>>(() => deriveInitialSelections(form, initialAnswers));
  const [filesByField, setFilesByField] = useState<Record<string, FileEntry[]>>({});
  const [submitting, setSubmitting] = useState(false);
  // Set when the caller's `onSubmit`/`onAction` rejects (the POST failed, or
  // the queued turn couldn't be started) — surfaced inline rather than left
  // as a silent, console-only unhandled rejection (the click handlers below
  // discard the promise). Cleared at the start of the next attempt. The form
  // itself already re-enables on failure via `submitting`/`actionPending`'s
  // own `finally` resets below — this only adds the missing "tell the
  // operator it didn't go through" half of that.
  const [submitError, setSubmitError] = useState<string | null>(null);
  // Which action button (if any) is mid-flight. A single flag rather than a
  // per-button one, since only one of Submit/Cancel/Regenerate/Something-else
  // can meaningfully be in flight at once — the button click resolves the
  // agent's one-shot suspended tool call, so a second concurrent click would
  // just 404 against an already-consumed form.
  const [actionPending, setActionPending] = useState<FormAction | null>(null);
  // "Something else" doesn't fire immediately — it takes over the form body
  // with a note box so the operator can say what they actually want, instead
  // of the agent having to guess-then-ask a follow-up in chat. Typing here
  // means the operator has no intention of answering the questions, so the
  // shared Submit button at the bottom sends the note and required-field
  // validation is bypassed while this is open (see `isValid` below).
  const [otherOpen, setOtherOpen] = useState(false);
  const [otherNote, setOtherNote] = useState("");
  const otherInputRef = useRef<HTMLTextAreaElement | null>(null);
  const fileInputs = useRef<Record<string, HTMLInputElement | null>>({});
  const filesByFieldRef = useRef(filesByField);
  filesByFieldRef.current = filesByField;

  useEffect(() => {
    return () => {
      for (const entries of Object.values(filesByFieldRef.current)) {
        for (const e of entries) {
          if (e.previewUrl) URL.revokeObjectURL(e.previewUrl);
        }
      }
    };
  }, []);

  // `onWithdraw` support — see its prop doc. `finalizedRef` starts false and
  // flips true the instant a real Submit/Cancel/Regenerate/"Something else"
  // fires (handleSubmit/handleAction below), so the unmount cleanup below
  // can tell "the operator finished with this form" apart from "this
  // instance just vanished out from under them" (a newer form took the slot
  // — see the caller's per-form `key`, one level up). `latestRef` mirrors
  // every render's live values into a ref for the same reason
  // `filesByFieldRef` does above: an unmount cleanup registered with
  // `useEffect(..., [])` only ever runs once, so its closure would
  // otherwise see this render's (i.e. the FIRST render's) stale
  // `textAnswers`/`selectionAnswers` — reading through a ref at cleanup
  // time always gets what was last typed.
  const finalizedRef = useRef(false);
  const latestRef = useRef({ form, textAnswers, selectionAnswers, readOnly, orphaned });
  latestRef.current = { form, textAnswers, selectionAnswers, readOnly, orphaned };
  const onWithdrawRef = useRef(onWithdraw);
  onWithdrawRef.current = onWithdraw;

  useEffect(() => {
    return () => {
      if (finalizedRef.current) return;
      const { form: f, textAnswers: t, selectionAnswers: s, readOnly: ro, orphaned: orp } = latestRef.current;
      if (ro || orp) return;
      const draft = buildWithdrawnDraft(f, t, s);
      if (draft.length === 0) return;
      onWithdrawRef.current?.(f.form_id, draft);
    };
  }, []);

  useEffect(() => {
    if (otherOpen) otherInputRef.current?.focus();
  }, [otherOpen]);

  const hasUploading = useMemo(
    () => Object.values(filesByField).some(entries => entries.some(e => e.status === "uploading")),
    [filesByField],
  );

  const isValid = useMemo(() => {
    // "Something else" replaces the questions entirely, so required-field
    // validation doesn't apply — the only thing gating Submit is the note.
    if (otherOpen) return otherNote.trim().length > 0;
    for (const field of form.fields) {
      if (!field.required) continue;
      if (field.kind === "text" || field.kind === "textarea") {
        if (!textAnswers[field.id]?.trim()) return false;
      } else if (field.kind === "checkbox" || field.kind === "radio") {
        if (!(selectionAnswers[field.id]?.length)) return false;
      } else if (field.kind === "file") {
        const ok = (filesByField[field.id] ?? []).some(e => e.status === "uploaded" && e.serverId);
        if (!ok) return false;
      }
    }
    return true;
  }, [otherOpen, otherNote, form.fields, textAnswers, selectionAnswers, filesByField]);

  const canSubmit = isValid && (otherOpen || !hasUploading) && !submitting && !actionPending;

  const handleCheckboxChange = useCallback((fieldId: string, optionId: string) => {
    setSelectionAnswers(prev => {
      const cur = prev[fieldId] ?? [];
      const next = cur.includes(optionId) ? cur.filter(id => id !== optionId) : [...cur, optionId];
      return { ...prev, [fieldId]: next };
    });
  }, []);

  const handleRadioChange = useCallback((fieldId: string, optionId: string) => {
    setSelectionAnswers(prev => ({ ...prev, [fieldId]: [optionId] }));
  }, []);

  const handleFileSelect = useCallback(async (fieldId: string, files: FileList | null, maxFiles: number) => {
    if (!files || files.length === 0) return;
    const currentCount = filesByField[fieldId]?.length ?? 0;
    const toAdd = Array.from(files).slice(0, Math.max(0, maxFiles - currentCount));
    if (!toAdd.length) return;

    const newEntries: FileEntry[] = toAdd.map(file => ({
      id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      file,
      previewUrl: file.type.startsWith("image/") ? URL.createObjectURL(file) : null,
      status: "uploading" as const,
      serverId: null,
      attachment: null,
    }));

    setFilesByField(prev => ({ ...prev, [fieldId]: [...(prev[fieldId] ?? []), ...newEntries] }));

    for (const entry of newEntries) {
      try {
        const att = await uploadAttachment(agentId, entry.file);
        setFilesByField(prev => ({
          ...prev,
          [fieldId]: (prev[fieldId] ?? []).map(e =>
            e.id === entry.id ? { ...e, status: "uploaded" as const, serverId: att.id, attachment: att } : e,
          ),
        }));
      } catch {
        setFilesByField(prev => ({
          ...prev,
          [fieldId]: (prev[fieldId] ?? []).map(e =>
            e.id === entry.id ? { ...e, status: "error" as const } : e,
          ),
        }));
      }
    }
  }, [agentId, filesByField]);

  const handleRemoveFile = useCallback(async (fieldId: string, entryId: string) => {
    const entry = (filesByField[fieldId] ?? []).find(e => e.id === entryId);
    if (!entry) return;
    if (entry.previewUrl) URL.revokeObjectURL(entry.previewUrl);
    if (entry.status === "uploaded" && entry.serverId) {
      try { await deleteAttachment(agentId, entry.serverId); } catch { /* best-effort */ }
    }
    setFilesByField(prev => ({ ...prev, [fieldId]: (prev[fieldId] ?? []).filter(e => e.id !== entryId) }));
  }, [agentId, filesByField]);

  const handleAction = useCallback(async (action: FormAction, note?: string) => {
    if (!onAction || submitting || actionPending) return;
    setSubmitError(null);
    setActionPending(action);
    // Marks this as a deliberate exit BEFORE the await — see `finalizedRef`'s
    // doc comment — so `onWithdraw` never fires for this unmount. Reset in
    // `catch` if the call fails: the form stays mounted for a retry, and a
    // *later* supersession should still be able to recover the draft.
    finalizedRef.current = true;
    try {
      await onAction(action, note);
      // No local state to clear on success — the parent removes this form
      // from `pendingForm`/`pendingFormByAgent` once its POST resolves, same
      // as a real submission, which unmounts this component.
    } catch (err) {
      finalizedRef.current = false;
      // Not rethrown — every call site below discards this promise (`void
      // handleAction(...)`), so a throw here would only ever surface as a
      // silent unhandled rejection. Surfacing inline instead is the actual
      // failure signal the operator sees.
      setSubmitError(err instanceof Error ? err.message : "Something went wrong — try again.");
    } finally {
      // On failure the form re-enables so the operator can retry; mirrors
      // handleSubmit's finally-based reset. `otherOpen`/`otherNote` are left
      // alone on failure so a failed "other" send keeps the typed note.
      setActionPending(null);
    }
  }, [onAction, submitting, actionPending]);

  const handleSubmit = useCallback(async () => {
    if (!canSubmit) return;
    // "Something else" took over the form — the shared Submit button sends
    // the note through the action channel instead of the answer map.
    if (otherOpen) {
      const note = otherNote.trim();
      if (!note) return;
      void handleAction("other", note);
      return;
    }
    setSubmitError(null);
    setSubmitting(true);
    // See `handleAction`'s identical comment — this is the other of the two
    // "deliberate exit" paths `onWithdraw` must stay silent for.
    finalizedRef.current = true;
    try {
      const answers: FormAnswerMap = {};
      for (const field of form.fields) {
        if (field.kind === "text" || field.kind === "textarea") {
          const val = textAnswers[field.id];
          if (val?.trim()) answers[field.id] = { kind: "text", value: val };
        } else if (field.kind === "checkbox" || field.kind === "radio") {
          const vals = selectionAnswers[field.id];
          if (vals?.length) answers[field.id] = { kind: "selections", values: vals };
        } else if (field.kind === "file") {
          const ids = (filesByField[field.id] ?? [])
            .filter(e => e.status === "uploaded" && e.serverId)
            .map(e => e.serverId as string);
          if (ids.length) answers[field.id] = { kind: "files", attachment_ids: ids };
        }
      }
      await onSubmit(answers);
    } catch (err) {
      finalizedRef.current = false;
      // Not rethrown — see the identical comment in `handleAction` above.
      // The parent's `onSubmit` (both the sync and async wiring in
      // ChatView.tsx) only clears the pending-form state and appends the
      // answered bubble AFTER this resolves, so a rejection here means
      // neither happened: nothing was left half-applied, and this is the
      // only place that failure is visible from.
      setSubmitError(err instanceof Error ? err.message : "Couldn't submit — try again.");
    } finally {
      setSubmitting(false);
    }
  }, [canSubmit, otherOpen, otherNote, handleAction, form.fields, textAnswers, selectionAnswers, filesByField, onSubmit]);

  const handleOpenOther = useCallback(() => {
    setOtherOpen(true);
  }, []);

  const handleCloseOther = useCallback(() => {
    setOtherOpen(false);
    setOtherNote("");
  }, []);

  function renderField(field: FormFieldDef) {
    switch (field.kind) {
      case "text":
        return (
          <input
            type="text"
            value={textAnswers[field.id] ?? ""}
            onChange={e => setTextAnswers(prev => ({ ...prev, [field.id]: e.target.value }))}
            placeholder={field.placeholder}
            readOnly={fieldsDisabled}
            tabIndex={fieldsDisabled ? -1 : undefined}
            className="w-full h-[42px] px-[14px] rounded-[8px] border-[2px] border-[#C4C4C4] dark:border-gray-500 bg-[var(--pinned-bg)] dark:bg-[var(--bg-input)] text-[14px] text-[var(--text-primary)] placeholder:text-gray-400 dark:placeholder-[var(--text-tertiary)] outline-none focus:border-[#007A59] focus:ring-[3px] focus:ring-[#007A59]/20 dark:focus:border-[var(--accent)] dark:focus:ring-[var(--accent)]/30 transition-all"
          />
        );
      case "textarea":
        return (
          <textarea
            value={textAnswers[field.id] ?? ""}
            onChange={e => setTextAnswers(prev => ({ ...prev, [field.id]: e.target.value }))}
            placeholder={field.placeholder}
            rows={3}
            readOnly={fieldsDisabled}
            tabIndex={fieldsDisabled ? -1 : undefined}
            className="w-full px-[14px] py-[12px] rounded-[8px] border-[2px] border-[#C4C4C4] dark:border-gray-500 bg-[var(--pinned-bg)] dark:bg-[var(--bg-input)] text-[14px] text-[var(--text-primary)] placeholder:text-gray-400 dark:placeholder-[var(--text-tertiary)] outline-none focus:border-[#007A59] focus:ring-[3px] focus:ring-[#007A59]/20 dark:focus:border-[var(--accent)] dark:focus:ring-[var(--accent)]/30 transition-all resize-none leading-relaxed"
          />
        );
      case "checkbox":
        return (
          <div className="flex flex-col gap-[12px]">
            {(field.options ?? []).map(opt => {
              const isSelected = (selectionAnswers[field.id] ?? []).includes(opt.id);
              return (
                <label key={opt.id} className="flex items-start gap-[12px] cursor-pointer select-none group relative">
                  <input
                    type="checkbox"
                    checked={isSelected}
                    onChange={() => handleCheckboxChange(field.id, opt.id)}
                    disabled={fieldsDisabled}
                    className="sr-only"
                  />
                  <div className={`mt-[2px] w-[20px] h-[20px] rounded-[6px] border-[2px] flex items-center justify-center shrink-0 transition-all duration-150 ${
                    isSelected
                      ? "border-[#007A59] bg-[#007A59]"
                      : "border-[#C4C4C4] bg-[var(--pinned-bg)] dark:bg-[var(--bg-input)] group-hover:border-[#007A59] dark:border-gray-500 dark:group-hover:border-[var(--accent)]"
                  }`}>
                    <svg className={`w-[12px] h-[12px] text-white transition-all duration-150 ${isSelected ? "opacity-100 scale-100" : "opacity-0 scale-50"}`} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round">
                      <polyline points="20 6 9 17 4 12" />
                    </svg>
                  </div>
                  <div className="flex-1">
                    <span className="text-[15px] text-[var(--text-primary)] group-hover:text-[#007A59] dark:group-hover:text-[var(--accent)] transition-colors inline-block">{opt.label}</span>
                    {opt.description && (
                      <span className="block text-[14px] text-[var(--text-secondary)] mt-[3px] leading-snug">{opt.description}</span>
                    )}
                  </div>
                </label>
              );
            })}
          </div>
        );
      case "radio":
        return (
          <div className="flex flex-col gap-[12px]">
            {(field.options ?? []).map(opt => {
              const isSelected = (selectionAnswers[field.id] ?? []).includes(opt.id);
              return (
                <label key={opt.id} className="flex items-start gap-[12px] cursor-pointer select-none group relative">
                  <input
                    type="radio"
                    name={`form-${form.form_id}-${field.id}`}
                    checked={isSelected}
                    onChange={() => handleRadioChange(field.id, opt.id)}
                    disabled={fieldsDisabled}
                    className="sr-only"
                  />
                  <div className={`mt-[2px] w-[20px] h-[20px] rounded-full border-[2px] flex items-center justify-center shrink-0 transition-all duration-150 ${
                    isSelected
                      ? "border-[#007A59] bg-[var(--pinned-bg)] dark:bg-transparent"
                      : "border-[#C4C4C4] bg-[var(--pinned-bg)] dark:bg-[var(--bg-input)] group-hover:border-[#007A59] dark:border-gray-500 dark:group-hover:border-[var(--accent)]"
                  }`}>
                    <div className={`w-[10px] h-[10px] rounded-full bg-[#007A59] dark:bg-[var(--accent)] transition-transform duration-150 ${isSelected ? "scale-100" : "scale-0"}`} />
                  </div>
                  <div className="flex-1">
                    <span className="text-[15px] text-[var(--text-primary)] group-hover:text-[#007A59] dark:group-hover:text-[var(--accent)] transition-colors inline-block">{opt.label}</span>
                    {opt.description && (
                      <span className="block text-[14px] text-[var(--text-secondary)] mt-[3px] leading-snug">{opt.description}</span>
                    )}
                  </div>
                </label>
              );
            })}
          </div>
        );
      case "file": {
        if (fieldsDisabled) {
          const ids = initialAnswers?.[field.id]?.attachment_ids ?? [];
          if (!ids.length) {
            return <span className="text-[13px] text-[var(--text-tertiary)]">No files attached</span>;
          }
          return (
            <div className="flex flex-wrap gap-[8px]">
              {ids.map(id => (
                <span
                  key={id}
                  className="inline-flex items-center gap-[6px] h-[32px] px-[10px] rounded-[6px] border text-[13px] font-medium"
                  style={{ borderColor: "var(--border-primary)", backgroundColor: "var(--bg-secondary)", color: "var(--text-primary)" }}
                >
                  <FileIcon size={13} style={{ color: "var(--text-secondary)" }} />
                  {id.slice(0, 8)}
                </span>
              ))}
            </div>
          );
        }
        const maxFiles = field.max_files ?? 1;
        const entries = filesByField[field.id] ?? [];
        const canAddMore = entries.length < maxFiles;
        return (
          <div className="flex flex-wrap gap-[8px] items-center">
            {entries.map(entry => {
              const name = entry.attachment?.original_filename ?? entry.file.name;
              return (
                <div
                  key={entry.id}
                  className="relative group flex items-center gap-[8px] h-[32px] pl-[10px] pr-[6px] rounded-[6px] border max-w-[220px]"
                  style={{
                    borderColor: entry.status === "error" ? "rgba(239,68,68,0.6)" : "var(--border-primary)",
                    backgroundColor: "var(--bg-secondary)",
                  }}
                >
                  {entry.status === "uploading" ? (
                    <Loader2 size={13} className="shrink-0 animate-spin" style={{ color: "var(--text-secondary)" }} />
                  ) : entry.status === "error" ? (
                    <AlertCircle size={13} className="shrink-0" style={{ color: "#ef4444" }} />
                  ) : (
                    <FileIcon size={13} className="shrink-0" style={{ color: "var(--text-secondary)" }} />
                  )}
                  <span className="truncate text-[13px] font-medium" style={{ color: "var(--text-primary)" }}>{name}</span>
                  <button
                    type="button"
                    onClick={() => void handleRemoveFile(field.id, entry.id)}
                    aria-label={`Remove ${name}`}
                    className="shrink-0 w-[20px] h-[20px] flex items-center justify-center rounded-[4px] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer ml-[2px]"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    <X size={12} />
                  </button>
                </div>
              );
            })}
            {canAddMore && (
              <>
                <input
                  type="file"
                  ref={el => { fileInputs.current[field.id] = el; }}
                  className="hidden"
                  accept={field.accept}
                  multiple={maxFiles > 1}
                  onChange={e => {
                    void handleFileSelect(field.id, e.target.files, maxFiles);
                    e.target.value = "";
                  }}
                />
                <button
                  type="button"
                  onClick={() => fileInputs.current[field.id]?.click()}
                  className="flex items-center gap-[6px] h-[32px] px-[12px] rounded-[6px] border-[2px] border-[#C4C4C4] dark:border-gray-500 bg-[var(--bg-secondary)] text-[13px] font-medium text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
                >
                  <Paperclip size={13} />
                  {entries.length === 0 ? "Add file" : "Add more"}
                </button>
              </>
            )}
          </div>
        );
      }
      default:
        return null;
    }
  }

  // Icon-only action row buttons share this styling — small footprint ("top
  // but should not take too much real estate"), disabled together whenever
  // any submission is in flight so a click can't race the one-shot form
  // delivery (see `handleAction`'s doc comment).
  const actionBtnClass = "w-[26px] h-[26px] flex items-center justify-center rounded-[6px] text-[var(--text-primary)] hover:text-[var(--accent)] hover:bg-[var(--bg-hover)] transition-colors disabled:opacity-40 disabled:cursor-not-allowed cursor-pointer";

  const card = (
    <div
      // `cqh` (container query height), not `vh` — `vh` measured the
      // viewport, which exceeds the actual chat pane whenever app chrome
      // (header/sidebar) eats into it, so the cap did nothing there. `cqh`
      // resolves against the nearest `container-type:size` ancestor instead
      // — ChatView's `[container-type:size]` pane wrapper — regardless of
      // the auto-height overlay wrappers in between. Shared by both the
      // sync overlay and the async nudge card in `ChatView.tsx`, so fixing
      // it here covers both call sites at once.
      className={`flex flex-col bg-[var(--chat-input-bg)] border border-[var(--border-primary)] rounded-[12px] shadow-sm ${readOnly ? "" : "max-h-[85cqh]"}`}
      aria-label={form.title}
      aria-disabled={fieldsDisabled || undefined}
    >
      {!readOnly && !orphaned && onAction && !otherOpen && (
        <div className="flex items-center justify-end gap-[2px] px-[10px] pt-[8px] flex-shrink-0">
          <button
            type="button"
            onClick={() => void handleAction("cancel")}
            disabled={submitting || !!actionPending}
            title="Cancel"
            aria-label="Cancel this question"
            data-testid="form-action-cancel-btn"
            className={actionBtnClass}
          >
            {actionPending === "cancel" ? <Loader2 size={13} className="animate-spin" /> : <X size={13} />}
          </button>
          <button
            type="button"
            onClick={() => void handleAction("regenerate")}
            disabled={submitting || !!actionPending}
            title="Regenerate form"
            aria-label="Ask for a different form"
            data-testid="form-action-regenerate-btn"
            className={actionBtnClass}
          >
            {actionPending === "regenerate" ? <Loader2 size={13} className="animate-spin" /> : <RefreshCw size={13} />}
          </button>
          <button
            type="button"
            onClick={handleOpenOther}
            disabled={submitting || !!actionPending}
            title="Something else"
            aria-label="I want something else"
            data-testid="form-action-other-btn"
            className={actionBtnClass}
          >
            <HelpCircle size={13} />
          </button>
        </div>
      )}
      <div className="relative px-[20px] pt-[20px] pb-[16px] flex-shrink-0 border-b border-[var(--border-secondary)]/50">
        {onMinimize && (
          <button
            type="button"
            onClick={onMinimize}
            aria-label="Answer later"
            title="Answer later"
            data-testid="form-minimize-btn"
            className={`absolute top-[8px] right-[10px] ${actionBtnClass}`}
          >
            <ChevronDown size={13} />
          </button>
        )}
        <h3 className={`text-[18px] font-bold text-[var(--text-primary)] leading-tight ${onMinimize ? "pr-[36px]" : ""}`}>{form.title}</h3>
        {form.intro && (
          <p className="text-[14px] text-[var(--text-secondary)] mt-[6px] leading-relaxed">{form.intro}</p>
        )}
        {orphaned && (
          <p
            role="status"
            data-testid="form-orphaned-notice"
            className="flex items-center gap-[8px] text-[13px] font-medium text-[#ef4444] mt-[10px]"
          >
            <AlertCircle size={14} className="shrink-0" />
            The run waiting on this answer ended.
          </p>
        )}
      </div>

      <div className={`flex flex-col gap-[20px] px-[20px] py-[16px] flex-1 ${fieldsDisabled ? "pointer-events-none" : "overflow-y-auto"}`}>
        {otherOpen ? (
          // Takes over the whole form body — typing here means the operator
          // isn't going to answer the questions, so they're hidden rather
          // than left dangling behind a small inline box.
          <div className="flex flex-col">
            <div className="flex items-center justify-between mb-[4px]">
              <label className="block text-[16px] font-bold text-[var(--text-primary)]">
                What would you like instead?
              </label>
              <button
                type="button"
                onClick={handleCloseOther}
                disabled={submitting || !!actionPending}
                title="Back to form"
                aria-label="Back to form"
                data-testid="form-action-other-close-btn"
                className={actionBtnClass}
              >
                <X size={13} />
              </button>
            </div>
            <textarea
              ref={otherInputRef}
              value={otherNote}
              onChange={e => setOtherNote(e.target.value)}
              onKeyDown={e => {
                if (e.key === "Escape") { e.preventDefault(); handleCloseOther(); }
              }}
              placeholder="Tell us what you'd like instead..."
              rows={5}
              disabled={submitting || !!actionPending}
              data-testid="form-action-other-input"
              className="w-full px-[14px] py-[12px] rounded-[8px] border-[2px] border-[#C4C4C4] dark:border-gray-500 bg-[var(--pinned-bg)] dark:bg-[var(--bg-input)] text-[14px] text-[var(--text-primary)] placeholder:text-gray-400 dark:placeholder-[var(--text-tertiary)] outline-none focus:border-[#007A59] focus:ring-[3px] focus:ring-[#007A59]/20 dark:focus:border-[var(--accent)] dark:focus:ring-[var(--accent)]/30 transition-all resize-none leading-relaxed"
            />
          </div>
        ) : (
          form.fields.map(field => (
            <div key={field.id} className="flex flex-col">
              <label className="block text-[16px] font-bold text-[var(--text-primary)] mb-[4px]">
                {field.label}
                {field.required && <span className="text-[var(--accent)] ml-[4px]">*</span>}
              </label>
              {field.description && (
                <p className="text-[14px] text-[var(--text-secondary)] mb-[10px] leading-snug">{field.description}</p>
              )}
              {renderField(field)}
            </div>
          ))
        )}
      </div>

      {!readOnly && !orphaned && (
        <div className="flex items-center justify-end gap-[12px] px-[20px] py-[16px] flex-shrink-0 border-t border-[var(--border-secondary)]/50 bg-[var(--bg-secondary)]/30 rounded-b-[12px]">
          {submitError && (
            <p
              role="alert"
              data-testid="form-submit-error"
              className="flex-1 flex items-center gap-[6px] text-[13px] font-medium text-[#ef4444]"
            >
              <AlertCircle size={14} className="shrink-0" />
              {submitError}
            </p>
          )}
          <button
            type="button"
            onClick={() => void handleSubmit()}
            disabled={!canSubmit}
            data-testid="form-submit-btn"
            className={`flex items-center justify-center gap-[6px] px-[20px] py-[8px] rounded-[4px] text-[15px] font-bold transition-colors min-w-[100px] bg-[#007A59] text-white ${canSubmit
              ? "hover:bg-[#006A4D] cursor-pointer"
              : "opacity-50 cursor-not-allowed"
              }`}
          >
            {(otherOpen ? actionPending === "other" : submitting)
              ? <Loader2 size={14} className="animate-spin" />
              : null}
            {otherOpen ? "Send" : "Submit"}
          </button>
        </div>
      )}
    </div>
  );

  // Read-only (answered) forms render on the user's side of the conversation,
  // sized like a user message — a bordered card sitting inside the right column.
  if (readOnly) {
    return (
      <div className="flex justify-end">
        <div className="w-full max-w-[85%] @3xl:max-w-[75%] @5xl:max-w-[70%]">
          {card}
        </div>
      </div>
    );
  }

  return card;
}
