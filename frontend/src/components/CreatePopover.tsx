import { useRef, useEffect } from "react";
import { User, CalendarClock } from "lucide-react";
import { motion, AnimatePresence } from "framer-motion";

export type CreateMenuOption = "agent" | "scheduled";

const MENU_ITEMS: { id: CreateMenuOption; label: string; icon: typeof User }[] = [
    { id: "agent", label: "Agent", icon: User },
    { id: "scheduled", label: "Assignment", icon: CalendarClock },
];

/**
 * Popover shown from the sidebar's "+" icon, mirroring `SettingsPopover`'s
 * look/behavior — pick what kind of thing to create before the relevant
 * modal (AgentProfileModal, or the Assignment editor via
 * useAssignmentEditorModalStore, both already mounted in AppShell) opens.
 */
export function CreatePopover({
    open,
    onClose,
    onSelect,
    anchorRef,
}: {
    open: boolean;
    onClose: () => void;
    onSelect: (option: CreateMenuOption) => void;
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
                    className="create-popover absolute bottom-0 left-full ml-2 z-[1012] w-52 rounded-xl border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] shadow-lg p-1.5 select-none"
                >
                    <div className="px-3 pt-1.5 pb-1 text-sm font-bold text-[var(--modal-text-primary)]">
                        Create
                    </div>
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
