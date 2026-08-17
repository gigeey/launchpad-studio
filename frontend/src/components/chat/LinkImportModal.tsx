import { useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { Link2, Loader2, X } from "lucide-react";

export interface LinkImportModalProps {
  open: boolean;
  onClose: () => void;
  onSubmit: (url: string) => Promise<void>;
  title?: string;
  helperText?: string;
}

function validateLinkUrl(raw: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed) return "URL is required";
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return "Enter a valid URL";
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    return "URL must use http:// or https://";
  }
  return null;
}

export function LinkImportModal({
  open,
  onClose,
  onSubmit,
  title = "Import from link",
  helperText,
}: LinkImportModalProps) {
  const [url, setUrl] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [touched, setTouched] = useState(false);

  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!open) {
      setUrl("");
      setSubmitting(false);
      setError(null);
      setTouched(false);
      return;
    }
    const id = window.setTimeout(() => inputRef.current?.focus(), 0);
    return () => window.clearTimeout(id);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !submitting) onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [open, onClose, submitting]);

  const validationError = validateLinkUrl(url);
  const canSubmit = !validationError && !submitting;

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setTouched(true);
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);
    try {
      await onSubmit(url.trim());
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setSubmitting(false);
    }
  };

  const handleBackdropClick = () => {
    if (!submitting) onClose();
  };

  const showValidationError = touched && validationError;

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
            onClick={handleBackdropClick}
          />
          <motion.div
            initial={{ opacity: 0, scale: 0.96 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.96 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            className="relative w-full max-w-[440px] rounded-[16px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)]"
            style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
          >
            <form onSubmit={handleSubmit} className="flex flex-col">
              <div className="flex items-center justify-between px-[20px] pt-[18px] pb-[8px]">
                <div className="flex items-center gap-[10px]">
                  <span className="w-[28px] h-[28px] rounded-[8px] bg-[var(--modal-bg-tertiary)] flex items-center justify-center">
                    <Link2 className="w-[15px] h-[15px] text-[var(--modal-text-primary)]" />
                  </span>
                  <h2 className="text-[16px] font-semibold text-[var(--modal-text-primary)]">
                    {title}
                  </h2>
                </div>
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
                  <label
                    htmlFor="link-import-url"
                    className="text-[12px] font-medium text-[var(--modal-text-secondary)]"
                  >
                    URL
                  </label>
                  <input
                    ref={inputRef}
                    id="link-import-url"
                    type="text"
                    value={url}
                    onChange={(e) => setUrl(e.target.value)}
                    onBlur={() => setTouched(true)}
                    placeholder="https://example.com/path/to/file.md"
                    autoCorrect="off"
                    autoCapitalize="off"
                    spellCheck={false}
                    disabled={submitting}
                    className="w-full h-[38px] px-[12px] rounded-[8px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[13px] font-mono text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-all disabled:opacity-60"
                  />
                  {showValidationError && (
                    <p className="text-[11px] text-[var(--error)]">{validationError}</p>
                  )}
                  {helperText && !showValidationError && (
                    <p className="text-[11px] text-[var(--modal-text-tertiary)]">{helperText}</p>
                  )}
                </div>

                {error && (
                  <div className="rounded-[8px] border border-[var(--error-border)] bg-[var(--error-bg)] px-[10px] py-[8px] text-[12px] text-[var(--error)]">
                    {error}
                  </div>
                )}
              </div>

              <div className="flex items-center justify-end gap-[8px] px-[20px] py-[14px] border-t border-[var(--modal-border-secondary)] bg-[var(--modal-bg)]">
                <button
                  type="button"
                  onClick={onClose}
                  disabled={submitting}
                  className="h-[34px] px-[14px] rounded-[8px] text-[13px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  Cancel
                </button>
                <button
                  type="submit"
                  disabled={!canSubmit}
                  className={`h-[34px] px-[14px] rounded-[8px] text-[13px] font-medium flex items-center gap-[6px] transition-colors ${
                    canSubmit
                      ? "bg-[var(--modal-accent)] text-[var(--text-on-accent)] hover:bg-[var(--modal-accent-hover)] cursor-pointer"
                      : "bg-[var(--modal-bg-hover)] text-[var(--modal-text-tertiary)] cursor-not-allowed"
                  }`}
                >
                  {submitting && <Loader2 className="w-[13px] h-[13px] animate-spin" />}
                  <span>{submitting ? "Importing…" : "Import"}</span>
                </button>
              </div>
            </form>
          </motion.div>
        </div>
      )}
    </AnimatePresence>
  );
}
