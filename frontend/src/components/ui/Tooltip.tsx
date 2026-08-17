import { useEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, motion } from "framer-motion";

// Shared "menu-style" tooltip. The look (dark pill with arrow, 700ms warmup,
// 500ms cooldown that lets neighbouring tooltips pop instantly) matches the
// sidebar-menu tooltip used in AppShell. Two placements:
//   - "right":  anchor-relative absolute positioning. Cheap and good for
//               sidebar buttons where there's room to the right.
//   - "top":    portal-positioned above the anchor. Survives parent
//               `overflow: auto` clipping, which makes it the right choice
//               for tooltips on rows inside scrollable panels.

let isWarm = false;
let warmTimeout: ReturnType<typeof setTimeout> | null = null;

export type TooltipPlacement = "right" | "top";

export function Tooltip({
  children,
  label,
  placement = "right",
  className,
}: {
  children: ReactNode;
  label: ReactNode;
  placement?: TooltipPlacement;
  // Applied to the wrapper element. Required when the tooltip wraps a child
  // whose layout depends on its parent (e.g. `truncate` inside a flex row
  // needs `min-w-0 flex-1` on the wrapper to preserve ellipsis behavior).
  className?: string;
}) {
  const [show, setShow] = useState(false);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const anchorRef = useRef<HTMLDivElement>(null);

  const updatePos = () => {
    if (placement !== "top") return;
    const node = anchorRef.current;
    if (!node) return;
    const rect = node.getBoundingClientRect();
    setPos({ top: rect.top - 8, left: rect.left + rect.width / 2 });
  };

  const handleMouseEnter = () => {
    updatePos();
    if (isWarm) {
      setShow(true);
    } else {
      timerRef.current = setTimeout(() => {
        updatePos();
        setShow(true);
        isWarm = true;
      }, 700);
    }
    if (warmTimeout) clearTimeout(warmTimeout);
  };

  const handleMouseLeave = () => {
    if (timerRef.current) clearTimeout(timerRef.current);
    setShow(false);
    warmTimeout = setTimeout(() => {
      isWarm = false;
    }, 500);
  };

  const handleClick = () => {
    if (timerRef.current) clearTimeout(timerRef.current);
    setShow(false);
  };

  useEffect(() => {
    return () => {
      if (timerRef.current) clearTimeout(timerRef.current);
    };
  }, []);

  if (placement === "top") {
    return (
      <div
        ref={anchorRef}
        className={className}
        onMouseEnter={handleMouseEnter}
        onMouseLeave={handleMouseLeave}
        onMouseDown={handleClick}
      >
        {children}
        {show &&
          pos &&
          createPortal(
            <AnimatePresence>
              <div
                style={{
                  position: "fixed",
                  top: pos.top,
                  left: pos.left,
                  transform: "translate(-50%, -100%)",
                  pointerEvents: "none",
                  zIndex: 9999,
                }}
              >
                <motion.div
                  initial={{ opacity: 0, scale: 0.95, y: 4 }}
                  animate={{ opacity: 1, scale: 1, y: 0 }}
                  exit={{ opacity: 0, scale: 0.95, y: 4 }}
                  transition={{ duration: 0.15, ease: "easeOut" }}
                  className="px-2 py-1 text-xs font-medium text-[var(--bg-primary)] bg-[var(--text-primary)] rounded shadow-lg max-w-[360px] whitespace-pre-wrap"
                >
                  {label}
                  <div className="absolute top-full left-1/2 -translate-x-1/2 -mt-1 w-2 h-2 bg-[var(--text-primary)] rotate-45" />
                </motion.div>
              </div>
            </AnimatePresence>,
            document.body,
          )}
      </div>
    );
  }

  // placement === "right"
  return (
    <div
      className={`relative flex items-center${className ? ` ${className}` : ""}`}
      onMouseEnter={handleMouseEnter}
      onMouseLeave={handleMouseLeave}
      onMouseDown={handleClick}
    >
      {children}
      <AnimatePresence>
        {show && (
          <motion.div
            initial={{ opacity: 0, scale: 0.95, x: -5 }}
            animate={{ opacity: 1, scale: 1, x: 5 }}
            exit={{ opacity: 0, scale: 0.95, x: -5 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            className="absolute left-full z-[100] px-2 py-1 ml-1 text-xs font-medium text-[var(--bg-primary)] bg-[var(--text-primary)] rounded shadow-lg whitespace-nowrap pointer-events-none"
          >
            {label}
            <div className="absolute top-1/2 -left-1 -translate-y-1/2 w-2 h-2 bg-[var(--text-primary)] rotate-45" />
          </motion.div>
        )}
      </AnimatePresence>
    </div>
  );
}

export default Tooltip;
