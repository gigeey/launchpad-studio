import { useEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, motion } from "framer-motion";
import Picker from "@emoji-mart/react";
import data from "@emoji-mart/data";

export interface EmojiPickerProps {
    /** Currently selected emoji (native character), or null if none is set yet. */
    value: string | null;
    /** Called with the native character of the emoji the user picked. */
    onChange: (emoji: string) => void;
    /** Classes applied to the trigger button; `value` (or `placeholder`) is its only child. */
    triggerClassName?: string;
    /** Rendered inside the trigger button when `value` is null. */
    placeholder?: ReactNode;
    /** Accessible label for the trigger button. */
    ariaLabel?: string;
}

/**
 * A button showing the current emoji that opens an emoji-mart picker
 * anchored beneath it when clicked. Selecting an emoji calls `onChange`
 * and closes the popover; clicking outside the trigger/popover also closes
 * it. Theme (light/dark) tracks the app's `data-theme` attribute.
 */
export function EmojiPicker({
    value,
    onChange,
    triggerClassName,
    placeholder = null,
    ariaLabel = "Pick emoji",
}: EmojiPickerProps) {
    const [open, setOpen] = useState(false);
    const [pos, setPos] = useState({ top: 0, left: 0 });
    const triggerRef = useRef<HTMLButtonElement>(null);
    const popoverRef = useRef<HTMLDivElement>(null);

    const [resolvedTheme, setResolvedTheme] = useState<"light" | "dark">(() =>
        document.documentElement.getAttribute("data-theme") === "dark" ? "dark" : "light"
    );
    useEffect(() => {
        const observer = new MutationObserver(() => {
            setResolvedTheme(document.documentElement.getAttribute("data-theme") === "dark" ? "dark" : "light");
        });
        observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
        return () => observer.disconnect();
    }, []);

    useEffect(() => {
        function handleClick(e: MouseEvent) {
            if (popoverRef.current && !popoverRef.current.contains(e.target as Node) &&
                triggerRef.current && !triggerRef.current.contains(e.target as Node)) {
                setOpen(false);
            }
        }
        if (open) document.addEventListener("mousedown", handleClick);
        return () => document.removeEventListener("mousedown", handleClick);
    }, [open]);

    return (
        <>
            <button
                ref={triggerRef}
                type="button"
                onClick={() => {
                    if (!open && triggerRef.current) {
                        const rect = triggerRef.current.getBoundingClientRect();
                        setPos({ top: rect.bottom + 8, left: rect.left });
                    }
                    setOpen((p) => !p);
                }}
                className={triggerClassName}
                aria-label={ariaLabel}
            >
                {value ?? placeholder}
            </button>

            {createPortal(
                <AnimatePresence>
                    {open && (
                        <motion.div
                            ref={popoverRef}
                            initial={{ opacity: 0, scale: 0.95, y: -4 }}
                            animate={{ opacity: 1, scale: 1, y: 0 }}
                            exit={{ opacity: 0, scale: 0.95, y: -4 }}
                            transition={{ duration: 0.12, ease: "easeOut" }}
                            style={{ position: "fixed", top: pos.top, left: pos.left, zIndex: 9999 }}
                            className="shadow-2xl rounded-[16px] overflow-hidden"
                        >
                            <Picker
                                data={data}
                                onEmojiSelect={(em: { native: string }) => {
                                    onChange(em.native);
                                    setOpen(false);
                                }}
                                theme={resolvedTheme}
                                previewPosition="none"
                                skinTonePosition="none"
                                set="native"
                            />
                        </motion.div>
                    )}
                </AnimatePresence>,
                document.body
            )}
        </>
    );
}
