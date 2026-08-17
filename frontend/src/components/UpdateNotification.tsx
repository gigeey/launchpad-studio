import { motion, AnimatePresence } from "framer-motion";
import { X, Download, RotateCcw, RefreshCw, AlertTriangle } from "lucide-react";
import { useUpdateStore } from "../stores/updateStore";
import { downloadAndInstallUpdate, relaunchApp } from "../stores/updateStore";

export function UpdateNotification() {
  const status = useUpdateStore((s) => s.status);
  const newVersion = useUpdateStore((s) => s.newVersion);
  const currentVersion = useUpdateStore((s) => s.currentVersion);
  // const releaseNotes = useUpdateStore((s) => s.releaseNotes);
  const downloadProgress = useUpdateStore((s) => s.downloadProgress);
  const error = useUpdateStore((s) => s.error);
  const dismissed = useUpdateStore((s) => s.dismissed);
  const dismiss = useUpdateStore((s) => s.dismiss);

  // Only show for actionable states and when not dismissed. `checkFailed` is
  // absent on purpose: a check that could not run is not something the user can
  // act on, and on any platform missing from the release manifest it is the
  // outcome of every check. See UpdateStatus in stores/updateStore.ts.
  const visible =
    !dismissed &&
    ["available", "downloading", "installed", "error"].includes(status);

  return (
    <AnimatePresence>
      {visible && (
        <motion.div
          initial={{ y: -20, opacity: 0, scale: 0.95 }}
          animate={{ y: 0, opacity: 1, scale: 1 }}
          exit={{ y: -20, opacity: 0, scale: 0.95 }}
          transition={{ duration: 0.2, ease: "easeOut" }}
          className="fixed top-6 left-1/2 -translate-x-1/2 z-50 w-full max-w-sm pointer-events-auto"
        >
          <div className="flex flex-col gap-3 p-4 rounded-2xl border border-[var(--border-primary)] bg-[var(--bg-secondary)] shadow-2xl">
            {/* Header row */}
            <div className="flex items-center justify-between gap-2">
              <div className="flex items-center gap-2 text-sm font-medium text-[var(--text-primary)]">
                {status === "error" ? (
                  <AlertTriangle size={16} className="text-red-500" />
                ) : status === "installed" ? (
                  <RefreshCw size={16} className="text-[#036D51]" />
                ) : (
                  <Download size={16} className="text-[#1064A3]" />
                )}
                <span>
                  {status === "error"
                    ? "Update failed to install"
                    : status === "installed"
                      ? "Update installed — restart to apply"
                      : status === "downloading"
                        ? "Downloading update…"
                        : "Update available"}
                </span>
              </div>

              {/* Dismiss button */}
              <button
                type="button"
                className="p-1 rounded transition-colors text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
                onClick={dismiss}
                aria-label="Dismiss"
              >
                <X size={16} />
              </button>
            </div>

            {/* Version info */}
            {currentVersion && newVersion && (
              <div className="text-xs text-[var(--text-secondary)]">
                {currentVersion} → {newVersion}
              </div>
            )}

            {/* Release notes */}
            {/* {releaseNotes && (
              <div className="text-xs text-[var(--text-secondary)] max-h-[80px] overflow-y-auto whitespace-pre-wrap leading-relaxed">
                {releaseNotes}
              </div>
            )} */}

            {/* Error message. Not gated on status === "error": a relaunch that
                fails leaves the status at "installed" (the install did work)
                and reports itself here. */}
            {error && (
              <div className="text-xs text-red-500 break-all">
                {error}
              </div>
            )}

            {/* Download progress bar */}
            {status === "downloading" && downloadProgress != null && (
              <div className="w-full h-1.5 rounded-full bg-[var(--bg-hover)] overflow-hidden">
                <div
                  className="h-full rounded-full bg-[#1064A3] transition-all duration-200"
                  style={{ width: `${downloadProgress}%` }}
                />
              </div>
            )}

            {/* Action buttons */}
            <div className="flex items-center gap-2">
              {status === "available" && (
                <button
                  type="button"
                  className="flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium bg-[#1064A3] text-white hover:bg-[#1064A3]/90 transition-colors cursor-pointer"
                  onClick={downloadAndInstallUpdate}
                >
                  <Download size={14} />
                  Update Now
                </button>
              )}

              {status === "installed" && (
                <button
                  type="button"
                  id="relaunch-btn"
                  className="flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium bg-[#036D51] text-white hover:bg-[#036D51]/90 transition-colors cursor-pointer"
                  onClick={relaunchApp}
                >
                  <RotateCcw size={14} />
                  Relaunch Now
                </button>
              )}

              {status === "error" && (
                <button
                  type="button"
                  className="flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium bg-red-600 text-white hover:bg-red-700 transition-colors cursor-pointer"
                  onClick={downloadAndInstallUpdate}
                >
                  <RotateCcw size={14} />
                  Retry
                </button>
              )}

              {(status === "available" || status === "installed") && (
                <button
                  type="button"
                  className="px-3 py-1.5 rounded-md text-xs font-medium text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
                  onClick={dismiss}
                >
                  Dismiss
                </button>
              )}
            </div>
          </div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
