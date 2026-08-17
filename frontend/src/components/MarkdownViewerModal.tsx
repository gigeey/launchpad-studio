import { useEffect, useRef, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { X, Copy, Check } from "lucide-react";
import { RichMarkdown } from "./shared/RichMarkdown";

interface MarkdownViewerModalProps {
    open: boolean;
    onClose: () => void;
    title: string;
    content: string;
}

export function MarkdownViewerModal({
    open,
    onClose,
    title,
    content,
}: MarkdownViewerModalProps) {
    const containerRef = useRef<HTMLDivElement | null>(null);
    const previouslyFocusedRef = useRef<HTMLElement | null>(null);
    const [copied, setCopied] = useState(false);

    useEffect(() => {
        if (!open) return;
        const handler = (e: KeyboardEvent) => {
            if (e.key === "Escape") onClose();
        };
        document.addEventListener("keydown", handler);
        return () => document.removeEventListener("keydown", handler);
    }, [open, onClose]);

    // Focus trap: keep Tab within the modal; restore focus on close.
    useEffect(() => {
        if (!open) return;
        previouslyFocusedRef.current = document.activeElement as HTMLElement | null;
        const container = containerRef.current;
        if (!container) return;

        const focusFirst = () => {
            const focusables = container.querySelectorAll<HTMLElement>(
                'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
            );
            (focusables[0] ?? container).focus();
        };
        const id = window.setTimeout(focusFirst, 0);

        const trap = (e: KeyboardEvent) => {
            if (e.key !== "Tab") return;
            const focusables = container.querySelectorAll<HTMLElement>(
                'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
            );
            if (focusables.length === 0) {
                e.preventDefault();
                return;
            }
            const first = focusables[0];
            const last = focusables[focusables.length - 1];
            const active = document.activeElement as HTMLElement | null;
            if (e.shiftKey && active === first) {
                e.preventDefault();
                last.focus();
            } else if (!e.shiftKey && active === last) {
                e.preventDefault();
                first.focus();
            }
        };
        document.addEventListener("keydown", trap);

        return () => {
            window.clearTimeout(id);
            document.removeEventListener("keydown", trap);
            previouslyFocusedRef.current?.focus?.();
        };
    }, [open]);

    useEffect(() => {
        if (!copied) return;
        const t = window.setTimeout(() => setCopied(false), 1500);
        return () => window.clearTimeout(t);
    }, [copied]);

    const handleCopyId = async () => {
        try {
            await navigator.clipboard.writeText(title);
            setCopied(true);
        } catch {
            // Clipboard unavailable — swallow silently; the label still shows the id.
        }
    };

    return (
        <AnimatePresence>
            {open && (
                <div className="fixed inset-0 z-[350] flex items-center justify-center">
                    <motion.div
                        initial={{ opacity: 0 }}
                        animate={{ opacity: 1 }}
                        exit={{ opacity: 0 }}
                        transition={{ duration: 0.15 }}
                        className="absolute inset-0 bg-black/40"
                        onClick={onClose}
                    />

                    <motion.div
                        ref={containerRef}
                        role="dialog"
                        aria-modal="true"
                        aria-label={title}
                        tabIndex={-1}
                        initial={{ opacity: 0, scale: 0.96 }}
                        animate={{ opacity: 1, scale: 1 }}
                        exit={{ opacity: 0, scale: 0.96 }}
                        transition={{ duration: 0.15, ease: "easeOut" }}
                        className="relative w-full max-w-[720px] max-h-[80vh] rounded-[16px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)] flex flex-col"
                        style={{
                            boxShadow:
                                "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)",
                        }}
                    >
                        <div className="flex items-center justify-between gap-[12px] px-[24px] pt-[18px] pb-[12px] border-b border-[var(--modal-border-secondary)]">
                            <div className="flex items-center gap-[8px] min-w-0">
                                <h2
                                    className="text-[15px] font-semibold text-[var(--modal-text-primary)] truncate"
                                    title={title}
                                >
                                    {title}
                                </h2>
                                <button
                                    type="button"
                                    onClick={handleCopyId}
                                    title={copied ? "Copied" : "Copy id"}
                                    aria-label="Copy id"
                                    className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-tertiary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer flex-shrink-0"
                                >
                                    {copied ? (
                                        <Check className="w-[13px] h-[13px]" />
                                    ) : (
                                        <Copy className="w-[13px] h-[13px]" />
                                    )}
                                </button>
                            </div>
                            <button
                                type="button"
                                onClick={onClose}
                                className="w-[32px] h-[32px] rounded-[8px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer flex-shrink-0"
                                title="Close"
                                aria-label="Close"
                            >
                                <X className="w-[18px] h-[18px]" />
                            </button>
                        </div>

                        <div className="flex-1 min-h-0 overflow-y-auto px-[24px] py-[18px] text-[14px] leading-[1.55] text-[var(--modal-text-primary)]">
                            {content.trim().length === 0 ? (
                                <div className="text-[13px] text-[var(--modal-text-tertiary)] italic">
                                    (empty file)
                                </div>
                            ) : (
                                <RichMarkdown>{content}</RichMarkdown>
                            )}
                        </div>
                    </motion.div>
                </div>
            )}
        </AnimatePresence>
    );
}
