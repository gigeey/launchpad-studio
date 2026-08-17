import { useEffect } from "react";
import { motion } from "framer-motion";
import { AlertTriangle, Download, ExternalLink, RotateCcw, RefreshCw } from "lucide-react";
import { useUpdateStore } from "../stores/updateStore";
import { downloadAndInstallUpdate, relaunchApp, checkForUpdates } from "../stores/updateStore";

const DMG_URL =
  "https://github.com/gigeey/launchpad-studio-releases/releases/latest/download/Launchpad_Studio_universal.dmg";

interface ForceUpdateGateProps {
  currentVersion: string;
  latestVersion: string;
}

export function ForceUpdateGate({
  currentVersion,
  latestVersion,
}: ForceUpdateGateProps) {
  const update = useUpdateStore((s) => s.update);
  const status = useUpdateStore((s) => s.status);
  const downloadProgress = useUpdateStore((s) => s.downloadProgress);
  const error = useUpdateStore((s) => s.error);

  // The gate blocks dev builds too, on purpose — a stale checkout meeting a
  // data directory a newer build has already migrated forward is the exact
  // hazard it exists for, and a developer is the likeliest person to hit it
  // (utils/versionCheck.ts). But the remedy differs. A released build should
  // install the newer one; a dev build should pull and rebuild, and installing
  // a signed DMG over it would be actively wrong. So the copy and the actions
  // branch, while the gate itself does not.
  const isDevBuild = import.meta.env.DEV;

  // If the update monitor hasn't run yet, trigger a check so
  // downloadAndInstallUpdate has a populated update object to work with. Not
  // needed in dev, where nothing here can start an install.
  useEffect(() => {
    if (isDevBuild) return;
    if (!update && status !== "checking") {
      checkForUpdates();
    }
  }, [update, status, isDevBuild]);

  return (
    <motion.div
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      transition={{ duration: 0.3, ease: "easeOut" }}
      className="fixed inset-0 z-[400] flex items-center justify-center bg-[var(--bg-primary)]"
    >
      <motion.div
        initial={{ opacity: 0, y: 12 }}
        animate={{ opacity: 1, y: 0 }}
        transition={{ duration: 0.35, ease: "easeOut", delay: 0.1 }}
        className="flex flex-col items-center gap-6 max-w-md px-8 text-center"
      >
        <div className="flex items-center justify-center w-14 h-14 rounded-2xl bg-[var(--bg-secondary)] border border-[var(--border-primary)]">
          <AlertTriangle size={28} className="text-amber-500" />
        </div>

        <div className="flex flex-col gap-2">
          <h1 className="text-xl font-semibold text-[var(--text-primary)]">
            {isDevBuild ? "Checkout Too Far Behind" : "Update Required"}
          </h1>
          <p className="text-sm leading-relaxed text-[var(--text-secondary)]">
            {isDevBuild
              ? "This development build is too far behind the published release line to run safely against your data directory. Pull the latest changes and rebuild."
              : "Your version of Launchpad Studio is too far behind to continue. Please update to the latest version to keep using the app."}
          </p>
        </div>

        <div className="flex items-center gap-3 text-xs text-[var(--text-tertiary)]">
          <span className="px-2 py-1 rounded-md bg-[var(--bg-secondary)] border border-[var(--border-primary)] font-mono">
            {currentVersion}
          </span>
          <span>→</span>
          <span className="px-2 py-1 rounded-md bg-[var(--bg-secondary)] border border-[var(--border-primary)] font-mono">
            {latestVersion}
          </span>
        </div>

        {/* Download progress bar */}
        {status === "downloading" && downloadProgress != null && (
          <div className="w-full max-w-xs">
            <div className="w-full h-2 rounded-full bg-[var(--bg-hover)] overflow-hidden">
              <motion.div
                className="h-full rounded-full bg-[#1064A3]"
                initial={{ width: 0 }}
                animate={{ width: `${downloadProgress}%` }}
                transition={{ duration: 0.2 }}
              />
            </div>
            <p className="mt-2 text-xs text-[var(--text-tertiary)]">
              Downloading update… {downloadProgress}%
            </p>
          </div>
        )}

        {/* Error message */}
        {status === "error" && error && (
          <p className="text-xs text-red-500 max-w-xs break-all">{error}</p>
        )}

        {/* Action buttons — install actions are production-only; see isDevBuild above. */}
        {isDevBuild ? (
          <code className="px-3 py-2 rounded-md text-xs font-mono bg-[var(--bg-secondary)] border border-[var(--border-primary)] text-[var(--text-secondary)]">
            git pull &amp;&amp; npm run tauri dev
          </code>
        ) : (
        <div className="flex flex-col items-center gap-3">
          {status === "installed" ? (
            <button
              type="button"
              onClick={relaunchApp}
              className="flex items-center gap-2 px-5 py-2.5 rounded-lg text-sm font-medium bg-[#036D51] text-white hover:bg-[#036D51]/90 transition-colors cursor-pointer"
            >
              <RefreshCw size={16} />
              Relaunch Now
            </button>
          ) : status === "downloading" ? null : status === "error" ? (
            <button
              type="button"
              onClick={downloadAndInstallUpdate}
              className="flex items-center gap-2 px-5 py-2.5 rounded-lg text-sm font-medium bg-red-600 text-white hover:bg-red-700 transition-colors cursor-pointer"
            >
              <RotateCcw size={16} />
              Retry Update
            </button>
          ) : (
            <button
              type="button"
              onClick={downloadAndInstallUpdate}
              className="flex items-center gap-2 px-5 py-2.5 rounded-lg text-sm font-medium bg-[#1064A3] text-white hover:bg-[#1064A3]/90 transition-colors cursor-pointer"
            >
              <Download size={16} />
              Update Now
            </button>
          )}

          <a
            href={DMG_URL}
            target="_blank"
            rel="noopener noreferrer"
            className="flex items-center gap-1.5 text-xs text-[var(--text-tertiary)] hover:text-[var(--text-secondary)] transition-colors"
          >
            <ExternalLink size={12} />
            Or download manually
          </a>
        </div>
        )}
      </motion.div>
    </motion.div>
  );
}
