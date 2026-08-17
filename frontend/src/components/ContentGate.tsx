import { AnimatePresence, motion } from "framer-motion";
import { useEffect, useState, type ReactNode } from "react";

interface ContentGateProps {
  /** From useReadyLatch — true once the real content is actually available. */
  ready: boolean;
  /** Shown only if the load is slow enough to notice (see `graceMs`). */
  skeleton?: ReactNode;
  children: ReactNode;
  /** How long to stay blank before considering the load "slow" and revealing the skeleton. */
  graceMs?: number;
  /** Safety net — reveal `children` even if `ready` never flips, so a stuck loading flag can't blank the pane forever. */
  failsafeMs?: number;
  className?: string;
}

const DEFAULT_CLASS = "flex flex-col flex-1 min-h-0";

/**
 * Single "reveal" primitive for content backed by an async fetch — pairs
 * with useReadyLatch. Renders nothing until `ready`, which avoids the
 * spinner-then-pop double motion you get from swapping a loading spinner for
 * real content mid-fade: instead of animating a state nobody asked to see,
 * this only ever animates the one transition that matters — blank/skeleton
 * to the real thing, exactly once.
 *
 * Phases: blank -> (after `graceMs`, only if still not ready) skeleton ->
 * (once `ready`) content. In the common case (data already cached) `ready`
 * is true on mount and this is just one clean fade-in; the skeleton only
 * ever appears on a genuinely slow/cold load.
 */
export function ContentGate({
  ready,
  skeleton = null,
  children,
  graceMs = 120,
  failsafeMs = 8000,
  className,
}: ContentGateProps) {
  const [showSkeleton, setShowSkeleton] = useState(false);
  const [forced, setForced] = useState(false);

  useEffect(() => {
    if (ready || forced) return;
    const t = setTimeout(() => setShowSkeleton(true), graceMs);
    return () => clearTimeout(t);
  }, [ready, forced, graceMs]);

  useEffect(() => {
    if (ready) return;
    const t = setTimeout(() => setForced(true), failsafeMs);
    return () => clearTimeout(t);
  }, [ready, failsafeMs]);

  const phase = ready || forced ? "content" : showSkeleton ? "skeleton" : "blank";
  const cls = className ?? DEFAULT_CLASS;

  return (
    <AnimatePresence initial={false} mode="wait">
      {phase === "content" ? (
        <motion.div
          key="content"
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.12, ease: "easeOut" }}
          className={cls}
        >
          {children}
        </motion.div>
      ) : phase === "skeleton" ? (
        <motion.div
          key="skeleton"
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.12, ease: "easeOut" }}
          className={cls}
        >
          {skeleton}
        </motion.div>
      ) : (
        <div key="blank" className={cls} />
      )}
    </AnimatePresence>
  );
}
