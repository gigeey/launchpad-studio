import { useCallback, useEffect, useRef, useState, type CSSProperties } from "react";
import { createPortal } from "react-dom";
import { motion, AnimatePresence } from "framer-motion";

import {
    PLACEHOLDERS,
    type SystemPromptPlaceholder,
} from "../../data/systemPromptPlaceholders";

type PlaceholderAutocompleteProps = {
    query: string;
    visible: boolean;
    anchorRect: DOMRect | null;
    onSelect: (id: string) => void;
    onDismiss: () => void;
};

/**
 * Filterable popover rendered when the user types `{{` inside the
 * SystemPromptEditor. Mirrors the keyboard-navigation / capture-listener
 * pattern of `MentionAutocomplete` but anchors to the caret via a fixed-position
 * portal so it escapes the editor's overflow-scroll container.
 */
export function PlaceholderAutocomplete({
    query,
    visible,
    anchorRect,
    onSelect,
    onDismiss,
}: PlaceholderAutocompleteProps) {
    const [activeIndex, setActiveIndex] = useState(0);
    const listRef = useRef<HTMLDivElement>(null);
    const rootRef = useRef<HTMLDivElement>(null);

    const q = query.toLowerCase();
    const filtered: SystemPromptPlaceholder[] = PLACEHOLDERS.filter(
        (p) =>
            p.id.toLowerCase().includes(q) ||
            p.label.toLowerCase().includes(q),
    );

    useEffect(() => {
        setActiveIndex(0);
    }, [query, visible]);

    useEffect(() => {
        const list = listRef.current;
        if (!list) return;
        const active = list.children[activeIndex] as HTMLElement | undefined;
        active?.scrollIntoView({ block: "nearest" });
    }, [activeIndex]);

    const handleKeyDown = useCallback(
        (e: KeyboardEvent) => {
            if (!visible) return;
            if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                onDismiss();
                return;
            }
            if (filtered.length === 0) return;
            if (e.key === "ArrowDown") {
                e.preventDefault();
                e.stopPropagation();
                setActiveIndex((i) => (i + 1) % filtered.length);
            } else if (e.key === "ArrowUp") {
                e.preventDefault();
                e.stopPropagation();
                setActiveIndex((i) => (i - 1 + filtered.length) % filtered.length);
            } else if (e.key === "Enter" || e.key === "Tab") {
                e.preventDefault();
                e.stopPropagation();
                onSelect(filtered[activeIndex].id);
            }
        },
        [visible, filtered, activeIndex, onSelect, onDismiss],
    );

    useEffect(() => {
        if (!visible) return;
        window.addEventListener("keydown", handleKeyDown, true);
        return () => window.removeEventListener("keydown", handleKeyDown, true);
    }, [visible, handleKeyDown]);

    // Click-outside dismissal: mousedown outside the popover root closes the
    // popover, leaving the typed `{{query` as literal text.
    useEffect(() => {
        if (!visible) return;
        const onMouseDown = (e: MouseEvent) => {
            const target = e.target as Node | null;
            if (rootRef.current && target && rootRef.current.contains(target)) return;
            onDismiss();
        };
        document.addEventListener("mousedown", onMouseDown);
        return () => document.removeEventListener("mousedown", onMouseDown);
    }, [visible, onDismiss]);

    if (!visible || !anchorRect) return null;

    const POPOVER_MAX_HEIGHT = 240;
    const GAP = 4;
    const EDGE_PADDING = 8;
    const spaceBelow = window.innerHeight - anchorRect.bottom;
    const spaceAbove = anchorRect.top;
    const flipUp =
        spaceBelow < POPOVER_MAX_HEIGHT + GAP + EDGE_PADDING &&
        spaceAbove > spaceBelow;
    const availableHeight = Math.max(
        120,
        (flipUp ? spaceAbove : spaceBelow) - GAP - EDGE_PADDING,
    );
    const maxHeight = Math.min(POPOVER_MAX_HEIGHT, availableHeight);
    const left = anchorRect.left;
    const positionStyle: CSSProperties = flipUp
        ? { bottom: window.innerHeight - anchorRect.top + GAP, left }
        : { top: anchorRect.bottom + GAP, left };

    return createPortal(
        <AnimatePresence>
            <motion.div
                ref={rootRef}
                initial={{ opacity: 0, y: flipUp ? 4 : -4, scale: 0.98 }}
                animate={{ opacity: 1, y: 0, scale: 1 }}
                exit={{ opacity: 0, y: flipUp ? 4 : -4, scale: 0.98 }}
                transition={{ duration: 0.12, ease: "easeOut" }}
                role="listbox"
                aria-label="System prompt placeholders"
                className="fixed w-[280px] overflow-hidden rounded-xl border border-[var(--border-primary)] bg-[var(--bg-secondary)] shadow-xl z-[500]"
                style={{ ...positionStyle, maxHeight }}
            >
                <div
                    ref={listRef}
                    className="overflow-y-auto py-1.5 px-1.5 flex flex-col gap-[2px] custom-scrollbar"
                    style={{ maxHeight }}
                >
                    {filtered.length === 0 ? (
                        <div className="px-3 py-2 text-[13px] text-[var(--text-tertiary)]">
                            No placeholders
                        </div>
                    ) : (
                        filtered.map((p, i) => (
                            <button
                                key={p.id}
                                type="button"
                                role="option"
                                aria-selected={i === activeIndex}
                                onMouseDown={(e) => {
                                    e.preventDefault();
                                    onSelect(p.id);
                                }}
                                onMouseEnter={() => setActiveIndex(i)}
                                className={`w-full flex flex-col gap-0.5 text-left px-3 py-1.5 rounded-lg cursor-pointer transition-colors ${i === activeIndex
                                    ? "bg-[var(--bg-hover)] text-[var(--text-primary)]"
                                    : "text-[var(--text-secondary)] hover:bg-[var(--bg-input)]"
                                    }`}
                            >
                                <div className="text-[13px] leading-tight font-mono text-[var(--text-primary)]">
                                    {p.id}
                                </div>
                                <span
                                    className={`text-[12px] leading-tight truncate max-w-full ${i === activeIndex
                                        ? "text-[var(--text-secondary)]"
                                        : "text-[var(--text-tertiary)]"
                                        }`}
                                >
                                    {p.description}
                                </span>
                            </button>
                        ))
                    )}
                </div>
            </motion.div>
        </AnimatePresence>,
        document.body,
    );
}
