import { useEffect } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { X } from "lucide-react";
import { SettingsPanel } from "../pages/SettingsView";
import type { SettingsModalView } from "./SettingsPopover";

export function SettingsModal({
    view,
    onClose,
}: {
    view: SettingsModalView | null;
    onClose: () => void;
}) {
    // Close on Escape
    useEffect(() => {
        if (!view) return;
        const handler = (e: KeyboardEvent) => {
            if (e.key === "Escape") onClose();
        };
        document.addEventListener("keydown", handler);
        return () => document.removeEventListener("keydown", handler);
    }, [view, onClose]);

    return (
        <AnimatePresence>
            {view && (
                <div className="fixed inset-0 z-[300] flex items-center justify-center">
                    {/* Backdrop */}
                    <motion.div
                        initial={{ opacity: 0 }}
                        animate={{ opacity: 1 }}
                        exit={{ opacity: 0 }}
                        transition={{ duration: 0.15 }}
                        className="absolute inset-0 bg-black/40"
                        onClick={onClose}
                    />

                    {/* Modal */}
                    <motion.div
                        initial={{ opacity: 0, scale: 0.96 }}
                        animate={{ opacity: 1, scale: 1 }}
                        exit={{ opacity: 0, scale: 0.96 }}
                        transition={{ duration: 0.15, ease: "easeOut" }}
                        className={`settings-modal ${view === "docs" ? "docs-modal " : ""}relative ${
                            view === "docs" ? "w-[1100px]" : "w-full max-w-[800px]"
                        } h-[750px] max-h-[90vh] rounded-[12px] overflow-hidden bg-[var(--modal-bg)] border border-transparent dark:border-[var(--modal-border-secondary)]`}
                        style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
                    >
                        {/* Close button */}
                        <button
                            type="button"
                            onClick={onClose}
                            className="absolute top-4 right-4 z-10 p-1.5 rounded-[8px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
                        >
                            <X size={18} />
                        </button>

                        <SettingsPanel isDocs={view === "docs"} />
                    </motion.div>
                </div>
            )}
        </AnimatePresence>
    );
}
