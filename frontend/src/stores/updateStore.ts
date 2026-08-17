import { create } from "zustand";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

/**
 * Updater lifecycle state.
 *
 * `error` and `checkFailed` are deliberately separate, and the distinction is
 * the whole reason this union is not simpler.
 *
 * - `error` means an update exists and installing it failed. The user can act
 *   on that, so the banner shows it and offers a retry.
 * - `checkFailed` means we never got as far as knowing whether an update
 *   exists. That is not actionable, and — this is the part worth knowing — it
 *   is the NORMAL outcome on any platform the release manifest does not cover.
 *   The plugin resolves the artifact for the running target *before* it
 *   compares versions, so on a target with no listed artifact `check()` throws
 *   on every call whether or not a newer version exists. The published manifest
 *   carries `darwin-aarch64` and `darwin-x86_64` only, so on Linux and Windows
 *   this is what happens every four hours, forever. Rendering it as an update
 *   notification would announce a non-existent update to most readers of this
 *   repository. It is logged and otherwise silent.
 */
export type UpdateStatus =
  | "none"
  | "checking"
  | "available"
  | "downloading"
  | "installed"
  | "error"
  | "checkFailed";

interface UpdateState {
  status: UpdateStatus;
  /** The pending update object (null when no update available) */
  update: Update | null;
  /** New version string */
  newVersion: string | null;
  /** Current app version */
  currentVersion: string | null;
  /** Release notes / changelog */
  releaseNotes: string | null;
  /**
   * Message from the last failure, of whichever kind `status` names. Set on
   * both `error` and `checkFailed`; only the former reaches the UI.
   */
  error: string | null;
  /** Download progress (0–100), null when not downloading */
  downloadProgress: number | null;
  /** Whether the user dismissed the notification */
  dismissed: boolean;

  setUpdate: (update: Update) => void;
  dismiss: () => void;
}

export const useUpdateStore = create<UpdateState>((set) => ({
  status: "none",
  update: null,
  newVersion: null,
  currentVersion: null,
  releaseNotes: null,
  error: null,
  downloadProgress: null,
  dismissed: false,

  setUpdate: (update) =>
    set({
      update,
      newVersion: update.version,
      currentVersion: update.currentVersion,
      releaseNotes: update.body ?? null,
      status: "available",
      error: null,
      dismissed: false,
    }),

  dismiss: () => set({ dismissed: true }),
}));

// ---------------------------------------------------------------------------
// Update check interval (every 4 hours)
// ---------------------------------------------------------------------------

const CHECK_INTERVAL = 4 * 60 * 60 * 1000;

let checkIntervalId: ReturnType<typeof setInterval> | null = null;

/** Download and install the pending update */
export async function downloadAndInstallUpdate() {
  const { update } = useUpdateStore.getState();
  if (!update) {
    // No pending update to install. Not reachable from the banner, whose action
    // buttons only render in states that imply a non-null `update`; logged
    // rather than returned silently so that if it ever does happen it is
    // visible instead of looking like a button that does nothing.
    console.warn("[updater] install requested with no pending update");
    return;
  }

  useUpdateStore.setState({ status: "downloading", error: null, downloadProgress: 0 });

  try {
    let totalBytes = 0;
    let receivedBytes = 0;

    await update.downloadAndInstall((event) => {
      if (event.event === "Started" && event.data.contentLength) {
        totalBytes = event.data.contentLength;
      } else if (event.event === "Progress") {
        receivedBytes += event.data.chunkLength;
        const progress = totalBytes > 0 ? Math.round((receivedBytes / totalBytes) * 100) : null;
        useUpdateStore.setState({ downloadProgress: progress });
      } else if (event.event === "Finished") {
        useUpdateStore.setState({ downloadProgress: 100 });
      }
    });

    useUpdateStore.setState({ status: "installed", downloadProgress: null });
  } catch (err) {
    console.error("[updater] Failed to download/install update:", err);
    useUpdateStore.setState({
      status: "error",
      error: err instanceof Error ? err.message : String(err),
      downloadProgress: null,
    });
  }
}

/**
 * Check for updates and update the store accordingly.
 *
 * This deliberately does not feed the startup version gate in
 * utils/versionCheck.ts, even though both end up reading the same `latest.json`
 * and the duplication is the first thing a reader notices. They answer
 * different questions and only one of them can be answered everywhere — the
 * full reasoning is at the top of versionCheck.ts.
 */
export async function checkForUpdates() {
  const store = useUpdateStore.getState();

  // Don't re-check once an install is under way or finished.
  if (["downloading", "installed"].includes(store.status)) return;

  useUpdateStore.setState({ status: "checking", error: null });

  try {
    const update = await check();
    if (update) {
      useUpdateStore.getState().setUpdate(update);
    } else {
      useUpdateStore.setState({ status: "none" });
    }
  } catch (err) {
    // Not an error the user can do anything about, and on any platform outside
    // the release manifest it is the guaranteed outcome of every call — see the
    // UpdateStatus doc comment. Logged, never shown.
    console.error("[updater] failed to check for updates:", err);
    useUpdateStore.setState({
      status: "checkFailed",
      error: err instanceof Error ? err.message : String(err),
    });
  }
}

/** Relaunch the app to apply the installed update */
export async function relaunchApp() {
  try {
    await relaunch();
  } catch (err) {
    // Stay in `installed` rather than moving to `error`. The install genuinely
    // did succeed; only the restart failed, and the useful action is still
    // "relaunch" (or quit by hand), not "download it again" — which is what the
    // error state's retry button does.
    console.error("[updater] failed to relaunch:", err);
    useUpdateStore.setState({
      error: err instanceof Error ? err.message : String(err),
    });
  }
}

/** Start periodic update checks — call once at app startup */
export function startUpdateMonitor() {
  // Initial check after a short delay so the app finishes loading first
  setTimeout(checkForUpdates, 5_000);

  // Periodic checks
  if (checkIntervalId) clearInterval(checkIntervalId);
  checkIntervalId = setInterval(checkForUpdates, CHECK_INTERVAL);

  return () => {
    if (checkIntervalId) clearInterval(checkIntervalId);
    checkIntervalId = null;
  };
}
