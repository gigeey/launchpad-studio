import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, motion } from "framer-motion";
import { Loader2, X } from "lucide-react";

export interface RenameThreadModalProps {
  open: boolean;
  /** Current custom title, empty string when the thread has none yet — the
   *  field starts empty and shows `placeholder` as a preview of the
   *  kind-derived label the thread would keep if left unrenamed. */
  initialValue: string;
  /** Kind-derived fallback label (e.g. "New thread", "Branch") — mirrors
   *  `ThreadTabStrip`'s `tabLabel` so the modal previews the same text the
   *  pill would fall back to if the name is cleared. */
  placeholder: string;
  onClose: () => void;
  /** Submits the trimmed value, or `null` when cleared — `null` tells the
   *  backend to drop the custom title so the thread reverts to its
   *  kind-derived placeholder, matching `ThreadsPanel`'s inline rename. */
  onSubmit: (title: string | null) => Promise<void>;
}

/** Modal for renaming a thread, opened via right-click on its tab pill (see
 *  `ThreadTabStrip`). Rendered through a portal because the tab strip's root
 *  is `pointer-events-none` (it's a click-through overlay — only individual
 *  pills opt back in), so nesting a modal inside it directly would inherit
 *  that and silently swallow every click. Portaling to `document.body`
 *  sidesteps that instead of threading an explicit `pointer-events-auto`
 *  override through every wrapper between here and the strip's root. */
export function RenameThreadModal({ open, initialValue, placeholder, onClose, onSubmit }: RenameThreadModalProps) {
  const [value, setValue] = useState(initialValue);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // Re-seed the field from the thread's current title each time the modal
  // opens (rather than once at mount) — the same pill can be renamed more
  // than once across the component's lifetime, and a stale `value` from a
  // previous open would otherwise leak into the next one.
  useEffect(() => {
    if (!open) {
      setSubmitting(false);
      setError(null);
      return;
    }
    setValue(initialValue);
    const id = window.setTimeout(() => inputRef.current?.select(), 0);
    return () => window.clearTimeout(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !submitting) onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [open, onClose, submitting]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (submitting) return;
    setSubmitting(true);
    setError(null);
    try {
      const trimmed = value.trim();
      await onSubmit(trimmed.length > 0 ? trimmed : null);
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setSubmitting(false);
    }
  };

  const handleBackdropClick = () => {
    if (!submitting) onClose();
  };

  return createPortal(
    <AnimatePresence>
      {open && (
        <div className="fixed inset-0 z-[400] flex items-center justify-center">
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
            className="absolute inset-0 bg-black/40"
            onClick={handleBackdropClick}
          />
          <motion.div
            initial={{ opacity: 0, scale: 0.96 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.96 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            role="dialog"
            aria-modal="true"
            aria-labelledby="rename-thread-title"
            className="rename-thread-modal relative w-full max-w-[440px] rounded-[12px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)]"
            style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
          >
            <form onSubmit={handleSubmit} className="flex flex-col">
              <div className="flex items-center justify-between px-[20px] pt-[18px] pb-[8px]">
                <h2 id="rename-thread-title" className="text-[20px] font-bold text-[var(--modal-text-primary)]">
                  Rename thread
                </h2>
                <button
                  type="button"
                  onClick={onClose}
                  disabled={submitting}
                  className="w-[28px] h-[28px] rounded-[8px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                  aria-label="Close"
                >
                  <X className="w-[16px] h-[16px]" />
                </button>
              </div>

              <div className="flex flex-col gap-[14px] px-[20px] py-[14px]">
                <div className="flex flex-col gap-[6px]">
                  <label htmlFor="rename-thread-input" className="text-[14px] font-semibold text-[var(--modal-text-label)]">
                    Thread name
                  </label>
                  <input
                    ref={inputRef}
                    id="rename-thread-input"
                    type="text"
                    value={value}
                    onChange={(e) => setValue(e.target.value)}
                    placeholder={placeholder}
                    autoCorrect="off"
                    autoCapitalize="off"
                    spellCheck={false}
                    disabled={submitting}
                    data-testid="rename-thread-input"
                    className="w-full h-[48px] px-[12px] rounded-[8px] border-[1.5px] border-[var(--modal-border-secondary)] text-[17px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_4px_color-mix(in_srgb,var(--modal-accent)_22%,transparent)] transition-all disabled:opacity-60"
                  />
                  <p className="text-[11px] text-[var(--modal-text-tertiary)]">Leave blank to reset to the default name.</p>
                </div>

                {error && (
                  <div className="rounded-[8px] border border-[var(--error-border)] bg-[var(--error-bg)] px-[10px] py-[8px] text-[12px] text-[var(--error)]">
                    {error}
                  </div>
                )}
              </div>

              <div className="flex items-center justify-end px-[20px] py-[14px]">
                <button
                  type="submit"
                  disabled={submitting}
                  data-testid="rename-thread-submit"
                  className={`h-[40px] px-[28px] rounded-[8px] text-[14px] font-semibold flex items-center justify-center gap-[6px] transition-colors ${!submitting
                    ? "bg-[var(--modal-accent)] text-[var(--text-on-accent)] hover:bg-[var(--modal-accent-hover)] cursor-pointer"
                    : "bg-[var(--modal-bg-hover)] text-[var(--modal-text-tertiary)] cursor-not-allowed"
                    }`}
                >
                  {submitting && <Loader2 className="w-[14px] h-[14px] animate-spin" />}
                  <span>{submitting ? "Saving…" : "Save"}</span>
                </button>
              </div>
            </form>
          </motion.div>
        </div>
      )}
    </AnimatePresence>,
    document.body,
  );
}
