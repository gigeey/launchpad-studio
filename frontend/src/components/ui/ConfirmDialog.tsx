import { useCallback, useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, motion } from "framer-motion";
import { Loader2 } from "lucide-react";

export interface ConfirmDialogProps {
    open: boolean;
    title: string;
    message: React.ReactNode;
    confirmLabel?: string;
    cancelLabel?: string;
    destructive?: boolean;
    onConfirm: () => void | Promise<void>;
    onCancel: () => void;
    children?: React.ReactNode;
}

export default function ConfirmDialog({
    open,
    title,
    message,
    confirmLabel = "Confirm",
    cancelLabel = "Cancel",
    destructive = false,
    onConfirm,
    onCancel,
    children,
}: ConfirmDialogProps) {
    const [loading, setLoading] = useState(false);

    const handleCancel = useCallback(() => {
        if (loading) return;
        onCancel();
    }, [loading, onCancel]);

    const handleConfirm = useCallback(async () => {
        if (loading) return;
        try {
            setLoading(true);
            await onConfirm();
        } finally {
            setLoading(false);
        }
    }, [loading, onConfirm]);

    useEffect(() => {
        if (!open) return;
        const handler = (e: KeyboardEvent) => {
            if (e.key === "Escape") handleCancel();
        };
        document.addEventListener("keydown", handler);
        return () => document.removeEventListener("keydown", handler);
    }, [open, handleCancel]);

    useEffect(() => {
        if (!open) setLoading(false);
    }, [open]);

    const confirmBg = destructive ? "#E01E5A" : "#006E51";
    const confirmHover = destructive ? "#B9184A" : "#005a43";

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
                        onClick={handleCancel}
                    />
                    <motion.div
                        initial={{ opacity: 0, scale: 0.96 }}
                        animate={{ opacity: 1, scale: 1 }}
                        exit={{ opacity: 0, scale: 0.96 }}
                        transition={{ duration: 0.15, ease: "easeOut" }}
                        role="dialog"
                        aria-modal="true"
                        aria-labelledby="confirm-dialog-title"
                        className="relative w-full max-w-[440px] rounded-[12px] overflow-hidden bg-[var(--bg-secondary)] border border-[var(--border-secondary)] flex flex-col"
                        style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
                    >
                        <div className="flex flex-col gap-[12px] px-[24px] py-[22px]">
                            <h2
                                id="confirm-dialog-title"
                                className="text-[18px] font-semibold tracking-tight text-[var(--text-primary)]"
                            >
                                {title}
                            </h2>
                            <div className="text-[13px] text-[var(--text-secondary)] leading-[20px]">
                                {message}
                            </div>
                            {children && <div className="mt-[4px]">{children}</div>}
                        </div>
                        <div className="flex items-center justify-end gap-[10px] px-[24px] py-[14px] bg-[var(--bg-secondary)]">
                            <button
                                type="button"
                                onClick={handleCancel}
                                disabled={loading}
                                className="h-[36px] px-[14px] rounded-[8px] text-[13px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                            >
                                {cancelLabel}
                            </button>
                            <button
                                type="button"
                                onClick={handleConfirm}
                                disabled={loading}
                                className="h-[36px] px-[16px] rounded-[8px] text-[13px] font-semibold text-white transition-colors flex items-center gap-[8px] cursor-pointer disabled:cursor-not-allowed"
                                style={{ backgroundColor: loading ? confirmHover : confirmBg }}
                                onMouseEnter={(e) => {
                                    if (!loading) e.currentTarget.style.backgroundColor = confirmHover;
                                }}
                                onMouseLeave={(e) => {
                                    if (!loading) e.currentTarget.style.backgroundColor = confirmBg;
                                }}
                            >
                                {loading && <Loader2 className="w-[13px] h-[13px] animate-spin" />}
                                <span>{confirmLabel}</span>
                            </button>
                        </div>
                    </motion.div>
                </div>
            )}
        </AnimatePresence>,
        document.body,
    );
}
