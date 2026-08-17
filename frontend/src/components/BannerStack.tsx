import { X } from "lucide-react";
import { motion, AnimatePresence } from "framer-motion";
import { useBannerStore, type Banner } from "../stores/bannerStore";

const variantStyles: Record<Banner["variant"], string> = {
  warning:
    "bg-yellow-100 dark:bg-yellow-900/30 text-yellow-900 dark:text-yellow-200",
  error:
    "bg-red-100 dark:bg-red-900/30 text-red-900 dark:text-red-200",
  info:
    "bg-blue-100 dark:bg-blue-900/30 text-blue-900 dark:text-blue-200",
};

const dismissHoverStyles: Record<Banner["variant"], string> = {
  warning: "hover:bg-yellow-200 dark:hover:bg-yellow-800/50",
  error: "hover:bg-red-200 dark:hover:bg-red-800/50",
  info: "hover:bg-blue-200 dark:hover:bg-blue-800/50",
};

export function BannerStack() {
  const banners = useBannerStore((s) => s.banners);
  const dismissed = useBannerStore((s) => s.dismissed);
  const dismissBanner = useBannerStore((s) => s.dismissBanner);

  // Filter out dismissed, sort by priority descending
  const visible = banners
    .filter((b) => !dismissed.has(b.id))
    .sort((a, b) => b.priority - a.priority);

  const current = visible[0] ?? null;
  const total = visible.length;

  // When there are more banners queued, keep the height on exit (fade only).
  // When the last banner leaves, collapse height too.
  const hasMore = total > 1;

  return (
    <AnimatePresence mode="wait">
      {current && (
        <motion.div
          key={current.id}
          initial={{ height: 0, opacity: 0 }}
          animate={{ height: "auto", opacity: 1 }}
          exit={hasMore ? { opacity: 0 } : { height: 0, opacity: 0 }}
          transition={{ duration: 0.2, ease: "easeOut" }}
          className="overflow-hidden"
        >
          <div
            className={`flex items-center gap-2 px-4 py-2 mb-1 rounded-lg text-sm ${variantStyles[current.variant]}`}
          >
            <span className="flex-1">
              {current.message}
              {current.action && (
                <>
                  {" "}
                  <button
                    type="button"
                    className="underline font-medium hover:opacity-80"
                    onClick={current.action.onClick}
                  >
                    {current.action.label}
                  </button>
                </>
              )}
            </span>

            <div className="flex items-center gap-2 flex-shrink-0">
              {/* Stack indicator */}
              {total > 1 && (
                <span className="text-xs opacity-70 tabular-nums">
                  1 of {total}
                </span>
              )}

              {/* Dismiss button */}
              {current.dismissible && (
                <button
                  type="button"
                  className={`p-1 rounded transition-colors ${dismissHoverStyles[current.variant]}`}
                  onClick={() => dismissBanner(current.id)}
                  aria-label="Dismiss"
                >
                  <X size={16} />
                </button>
              )}
            </div>
          </div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
