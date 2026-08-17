import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ComponentType } from "react";
import type { LucideProps } from "lucide-react";
import { motion, AnimatePresence } from "framer-motion";

export interface NavMoreItem {
  id: string;
  label: string;
  icon: ComponentType<LucideProps>;
}

interface Placement {
  left: number;
  top?: number;
  bottom?: number;
  maxHeight: number;
}

const VIEWPORT_MARGIN = 8;
const MIN_PREFERRED_HEIGHT = 160;
const MIN_USABLE_HEIGHT = 120;

/**
 * Popover shown from the nav rail's "More" row once nav items overflow —
 * mirrors CreatePopover/SettingsPopover's chrome (bold header, icon+label
 * rows, click-outside-to-close). Unlike those two, which are pinned at the
 * rail's bottom edge and can safely anchor with a fixed `bottom-0`, More
 * sits at a variable mid-list position that can land anywhere on an
 * already-short screen — so position and max-height are computed from the
 * anchor's actual viewport rect each time it opens, instead of assumed from
 * static CSS alone, with an internal scroll as a backstop if it still can't
 * fit everything.
 */
export function NavMorePopover({
  open,
  onClose,
  onSelect,
  items,
  anchorRef,
}: {
  open: boolean;
  onClose: () => void;
  onSelect: (id: string) => void;
  items: NavMoreItem[];
  anchorRef: React.RefObject<HTMLDivElement | null>;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [placement, setPlacement] = useState<Placement | null>(null);

  useLayoutEffect(() => {
    if (!open || !anchorRef.current) {
      setPlacement(null);
      return;
    }
    const rect = anchorRef.current.getBoundingClientRect();
    const left = rect.right + 8; // mirrors the `ml-2` gap Create/Settings use

    const spaceBelow = window.innerHeight - rect.top - VIEWPORT_MARGIN;
    const spaceAbove = rect.bottom - VIEWPORT_MARGIN;

    // Prefer opening downward from the anchor's top edge, like a normal
    // dropdown; only flip to opening upward when space below is tight AND
    // there's meaningfully more room above.
    if (spaceBelow >= MIN_PREFERRED_HEIGHT || spaceBelow >= spaceAbove) {
      setPlacement({ top: rect.top, left, maxHeight: Math.max(MIN_USABLE_HEIGHT, spaceBelow) });
    } else {
      setPlacement({ bottom: window.innerHeight - rect.bottom, left, maxHeight: Math.max(MIN_USABLE_HEIGHT, spaceAbove) });
    }
  }, [open, anchorRef]);

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
      {open && placement && (
        <motion.div
          ref={ref}
          initial={{ opacity: 0, scale: 0.95, y: 4 }}
          animate={{ opacity: 1, scale: 1, y: 0 }}
          exit={{ opacity: 0, scale: 0.95, y: 4 }}
          transition={{ duration: 0.12, ease: "easeOut" }}
          className="nav-more-popover fixed z-[1012] w-52 rounded-xl border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] shadow-lg p-1.5 select-none overflow-y-auto"
          style={{
            left: placement.left,
            top: placement.top,
            bottom: placement.bottom,
            maxHeight: placement.maxHeight,
          }}
        >
          <div className="px-3 pt-1.5 pb-1 text-sm font-bold text-[var(--modal-text-primary)]">
            More
          </div>
          {items.map((item) => (
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
