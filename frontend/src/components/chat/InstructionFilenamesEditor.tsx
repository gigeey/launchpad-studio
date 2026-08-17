import { useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { FileText, Loader2, X } from "lucide-react";

export interface InstructionFilenamesEditorProps {
  open: boolean;
  onClose: () => void;
  filenames: string[];
  onPersist: (list: string[]) => Promise<string[]>;
}

function validatePattern(raw: string, existing: string[]): string | null {
  const trimmed = raw.trim();
  if (!trimmed) return "Enter a filename (no whitespace-only entries).";
  if (trimmed.includes("/") || trimmed.includes("\\")) {
    return "Filenames cannot contain / or \\.";
  }
  if (existing.some((f) => f.toLowerCase() === trimmed.toLowerCase())) {
    return "That filename is already in the list.";
  }
  return null;
}

export function InstructionFilenamesEditor({
  open,
  onClose,
  filenames,
  onPersist,
}: InstructionFilenamesEditorProps) {
  const [draft, setDraft] = useState<string[]>(filenames);
  const [input, setInput] = useState("");
  const [inputError, setInputError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [confirmClearOpen, setConfirmClearOpen] = useState(false);
  const [pendingRemoveIndex, setPendingRemoveIndex] = useState<number | null>(null);

  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) {
      setDraft(filenames);
      setInput("");
      setInputError(null);
      setSaveError(null);
      setConfirmClearOpen(false);
      setPendingRemoveIndex(null);
      const id = window.setTimeout(() => inputRef.current?.focus(), 0);
      return () => window.clearTimeout(id);
    }
  }, [open, filenames]);

  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !saving) onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [open, onClose, saving]);

  const commit = async (next: string[]) => {
    setSaving(true);
    setSaveError(null);
    try {
      const normalized = await onPersist(next);
      setDraft(normalized);
    } catch (err) {
      setSaveError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const handleAdd = async () => {
    const error = validatePattern(input, draft);
    if (error) {
      setInputError(error);
      return;
    }
    const trimmed = input.trim();
    setInput("");
    setInputError(null);
    await commit([...draft, trimmed]);
  };

  const handleRemoveAt = async (index: number) => {
    if (draft.length === 1) {
      setPendingRemoveIndex(index);
      setConfirmClearOpen(true);
      return;
    }
    const next = draft.filter((_, i) => i !== index);
    await commit(next);
  };

  const confirmClear = async () => {
    if (pendingRemoveIndex === null) return;
    const next = draft.filter((_, i) => i !== pendingRemoveIndex);
    setConfirmClearOpen(false);
    setPendingRemoveIndex(null);
    await commit(next);
  };

  const cancelClear = () => {
    setConfirmClearOpen(false);
    setPendingRemoveIndex(null);
  };

  return (
    <AnimatePresence>
      {open && (
        <div className="fixed inset-0 z-[400] flex items-center justify-center">
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
            className="absolute inset-0 bg-black/40"
            onClick={() => !saving && onClose()}
          />
          <motion.div
            initial={{ opacity: 0, scale: 0.96 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.96 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            className="relative w-full max-w-[460px] rounded-[16px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)]"
            style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
          >
            <div className="flex items-center justify-between px-[20px] pt-[18px] pb-[8px]">
              <div className="flex items-center gap-[10px]">
                <span className="w-[28px] h-[28px] rounded-[8px] bg-[var(--modal-bg-tertiary)] flex items-center justify-center">
                  <FileText className="w-[15px] h-[15px] text-[var(--modal-text-primary)]" />
                </span>
                <h2 className="text-[16px] font-semibold text-[var(--modal-text-primary)]">
                  Instruction filenames
                </h2>
              </div>
              <button
                type="button"
                onClick={onClose}
                disabled={saving}
                className="w-[28px] h-[28px] rounded-[8px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                aria-label="Close"
              >
                <X className="w-[16px] h-[16px]" />
              </button>
            </div>

            <div className="flex flex-col gap-[12px] px-[20px] py-[14px]">
              <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                Files in the agent home matching any of these names (case-insensitive) are loaded as instructions.
              </p>

              <div className="flex flex-wrap gap-[6px] min-h-[32px]">
                {draft.length === 0 ? (
                  <span className="text-[12px] text-[var(--modal-text-tertiary)] italic">
                    No patterns. The Instructions tab will be empty.
                  </span>
                ) : (
                  draft.map((name, i) => (
                    <span
                      key={`${name}-${i}`}
                      className="inline-flex items-center gap-[6px] h-[26px] pl-[10px] pr-[4px] rounded-full bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] text-[12px] font-mono text-[var(--modal-text-primary)]"
                    >
                      <span className="truncate max-w-[220px]" title={name}>
                        {name}
                      </span>
                      <button
                        type="button"
                        onClick={() => void handleRemoveAt(i)}
                        disabled={saving}
                        className="w-[18px] h-[18px] rounded-full flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                        aria-label={`Remove ${name}`}
                      >
                        <X className="w-[11px] h-[11px]" />
                      </button>
                    </span>
                  ))
                )}
              </div>

              <div className="flex flex-col gap-[6px]">
                <label
                  htmlFor="instruction-filename-input"
                  className="text-[12px] font-medium text-[var(--modal-text-secondary)]"
                >
                  Add pattern
                </label>
                <div className="flex items-center gap-[6px]">
                  <input
                    ref={inputRef}
                    id="instruction-filename-input"
                    type="text"
                    value={input}
                    onChange={(e) => {
                      setInput(e.target.value);
                      if (inputError) setInputError(null);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") {
                        e.preventDefault();
                        void handleAdd();
                      }
                    }}
                    placeholder="e.g. CLAUDE.md"
                    autoCorrect="off"
                    autoCapitalize="off"
                    spellCheck={false}
                    disabled={saving}
                    className="flex-1 h-[34px] px-[10px] rounded-[8px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[13px] font-mono text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-all disabled:opacity-60"
                  />
                  <button
                    type="button"
                    onClick={() => void handleAdd()}
                    disabled={saving || !input.trim()}
                    className="h-[34px] px-[12px] rounded-[8px] text-[13px] font-medium bg-[var(--modal-accent)] text-[var(--text-on-accent)] hover:bg-[var(--modal-accent-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                  >
                    Add
                  </button>
                </div>
                {inputError && (
                  <p className="text-[11px] text-[var(--error)]">{inputError}</p>
                )}
              </div>

              {saveError && (
                <div className="rounded-[8px] border border-[var(--error-border)] bg-[var(--error-bg)] px-[10px] py-[8px] text-[12px] text-[var(--error)]">
                  {saveError}
                </div>
              )}
            </div>

            <div className="flex items-center justify-between gap-[8px] px-[20px] py-[14px] border-t border-[var(--modal-border-secondary)] bg-[var(--modal-bg)]">
              <span className="text-[11px] text-[var(--modal-text-tertiary)] flex items-center gap-[6px]">
                {saving && <Loader2 className="w-[11px] h-[11px] animate-spin" />}
                {saving ? "Saving…" : `${draft.length} pattern${draft.length === 1 ? "" : "s"}`}
              </span>
              <button
                type="button"
                onClick={onClose}
                disabled={saving}
                className="h-[34px] px-[14px] rounded-[8px] text-[13px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
              >
                Done
              </button>
            </div>
          </motion.div>

          {confirmClearOpen && (
            <div className="fixed inset-0 z-[450] flex items-center justify-center">
              <motion.div
                initial={{ opacity: 0 }}
                animate={{ opacity: 1 }}
                exit={{ opacity: 0 }}
                transition={{ duration: 0.15 }}
                className="absolute inset-0 bg-black/40"
                onClick={cancelClear}
              />
              <motion.div
                initial={{ opacity: 0, scale: 0.96 }}
                animate={{ opacity: 1, scale: 1 }}
                exit={{ opacity: 0, scale: 0.96 }}
                transition={{ duration: 0.15, ease: "easeOut" }}
                className="relative w-full max-w-[360px] rounded-[16px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)]"
                style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
              >
                <div className="flex flex-col gap-[10px] px-[20px] py-[18px]">
                  <h3 className="text-[15px] font-semibold text-[var(--modal-text-primary)]">
                    Remove all patterns?
                  </h3>
                  <p className="text-[12px] text-[var(--modal-text-secondary)]">
                    The Instructions tab will be empty.
                  </p>
                </div>
                <div className="flex items-center justify-end gap-[8px] px-[20px] py-[12px] border-t border-[var(--modal-border-secondary)] bg-[var(--modal-bg)]">
                  <button
                    type="button"
                    onClick={cancelClear}
                    className="h-[32px] px-[12px] rounded-[8px] text-[13px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    onClick={() => void confirmClear()}
                    className="h-[32px] px-[12px] rounded-[8px] text-[13px] font-medium bg-[var(--error)] text-white hover:opacity-90 transition-opacity cursor-pointer"
                  >
                    Remove
                  </button>
                </div>
              </motion.div>
            </div>
          )}
        </div>
      )}
    </AnimatePresence>
  );
}
