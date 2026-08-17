import { useRef, useEffect } from "react";
import { Settings, Puzzle } from "lucide-react";
import { motion, AnimatePresence } from "framer-motion";

export type SettingsModalView = "settings" | "docs";

// Collections isn't a `SettingsModalView` — it opens its own modal
// (`CollectionsModal`, via `collectionsModalStore`), not `SettingsModal`.
// `onSelect` below takes this wider action type so the popover can host both
// kinds of destinations without conflating them.
export type SettingsPopoverAction = SettingsModalView | "collections";

const MENU_ITEMS: { id: SettingsPopoverAction; label: string; icon: typeof Settings }[] = [
    { id: "settings", label: "Settings", icon: Settings },
    { id: "collections", label: "Collections", icon: Puzzle },
    // { id: "docs", label: "Docs", icon: FileText },
];

export function SettingsPopover({
    open,
    onClose,
    onSelect,
    anchorRef,
}: {
    open: boolean;
    onClose: () => void;
    onSelect: (action: SettingsPopoverAction) => void;
    anchorRef: React.RefObject<HTMLDivElement | null>;
}) {
    const ref = useRef<HTMLDivElement>(null);

    useEffect(() => {
        if (!open) return;
        const handler = (e: MouseEvent) => {
            if (
                ref.current && !ref.current.contains(e.target as Node) &&
                anchorRef.current && !anchorRef.current.contains(e.target as Node)
            ) {
                onClose();
            }
        };
        document.addEventListener("mousedown", handler);
        return () => document.removeEventListener("mousedown", handler);
    }, [open, onClose, anchorRef]);

    return (
        <AnimatePresence>
            {open && (
                <motion.div
                    ref={ref}
                    initial={{ opacity: 0, scale: 0.95, y: 4 }}
                    animate={{ opacity: 1, scale: 1, y: 0 }}
                    exit={{ opacity: 0, scale: 0.95, y: 4 }}
                    transition={{ duration: 0.12, ease: "easeOut" }}
                    className="settings-popover absolute bottom-0 left-full ml-2 z-[1012] w-52 rounded-xl border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] shadow-lg p-1.5 select-none"
                >
                    {MENU_ITEMS.map((item) => (
                        <button
                            key={item.id}
                            type="button"
                            onClick={() => {
                                onSelect(item.id);
                                onClose();
                            }}
                            className="flex items-center gap-2.5 w-full px-3 py-2.5 text-sm text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] rounded-lg transition-colors cursor-pointer"
                        >
                            <item.icon size={16} className="text-[var(--modal-text-secondary)]" />
                            {item.label}
                        </button>
                    ))}
                </motion.div>
            )}
        </AnimatePresence>
    );
}
