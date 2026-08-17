import { useCallback, useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { AlertTriangle, CheckCircle2, Pencil, Plus } from "lucide-react";
import { twMerge } from "tailwind-merge";
import { invoke } from "@tauri-apps/api/core";
import ConfirmDialog from "./ui/ConfirmDialog";
import { WorkspaceAvatar } from "./WorkspaceAvatar";
import { activateWorkspace, type WorkspaceEntry } from "../lib/api";
import { useBannerStore } from "../stores/bannerStore";

// Named literally so the person who exported LAUNCHPAD_STUDIO_DATA_DIR can
// grep this string and find both the UI copy and their own shell config in
// one search — same convention WorkspaceIndicator and SettingsView use.
const ENV_VAR_NAME = "LAUNCHPAD_STUDIO_DATA_DIR";

const ENV_OVERRIDE_DISABLED_TITLE = `Disabled: the data root is pinned by ${ENV_VAR_NAME}. Unset it and relaunch to switch profiles.`;

// Distinct from the env-override copy above on purpose — the two states
// mean opposite things. Env override: nothing to do, switching is disabled.
// Fallback: switching is exactly how the user recovers, so this is the
// action prompt, not a status note.
const FALLBACK_RECOVERY_MESSAGE =
  "Your selected workspace could not be opened, so the app started on the default one. Pick a workspace below to recover.";

export interface WorkspaceSwitcherPopoverProps {
  open: boolean;
  onClose: () => void;
  anchorRef: React.RefObject<HTMLElement | null>;
  /** Every registered workspace, from `getWorkspaces()`. */
  workspaces: WorkspaceEntry[];
  /** The resolved path this window is actually running against right now
   *  (`GET /workspaces/active`'s `path`) — compared against each row's
   *  `path`, not the registry's bare `active` id, so the active row stays
   *  honest under an env override. `null` while the active workspace hasn't
   *  loaded yet — no row is marked active in that case. */
  activePath: string | null;
  /** `true` when `LAUNCHPAD_STUDIO_DATA_DIR` was INHERITED from this
   *  process's environment — a deliberate operator pin. Activation and
   *  creation stay visible but disabled — the server would 409 either way
   *  (`WORKSPACE_MUTATION_BLOCKED_MESSAGE` in `crates/ao-server/src/error.rs`),
   *  so disabling here is a UX nicety on top of a guard that already exists
   *  server-side, not a substitute for it. Must be `false` whenever
   *  `fallbackActive` is `true` — the two are mutually exclusive states
   *  reported by distinct `provenance` values. */
  envOverrideActive: boolean;
  /** `true` when this process's startup fell back to the default data root
   *  after the workspace it actually resolved to failed to open
   *  (`provenance === "fallback"`) — a self-inflicted pin, not an operator
   *  one. Unlike `envOverrideActive`, this must NOT disable anything:
   *  activating a different workspace here is the only way to recover, so
   *  every control stays fully interactive and a distinct, actionable
   *  banner is shown in place of the env-override one. */
  fallbackActive: boolean;
  /** The workspace path that failed to open during startup, for the
   *  fallback banner. `null` when `fallbackActive` is `false` or the
   *  backend didn't report one. */
  fallbackFailedRoot: string | null;
  /** A short, human-legible reason the workspace above failed to open, for
   *  the fallback banner. `null` when `fallbackActive` is `false`, no
   *  reason was reported, or the reported error text wasn't judged fit to
   *  show a user directly — see `isHumanLegibleFallbackError` in
   *  `WorkspaceIndicator.tsx`, the sole place that decision is made. */
  fallbackReason: string | null;
  /** Fired when "Create workspace" is clicked (and the popover has already
   *  closed) — opens `WorkspaceEditModal` in create mode. */
  onCreateWorkspace: () => void;
  /** Fired when a row's rename affordance is clicked (and the popover has
   *  already closed) — opens `WorkspaceEditModal` in rename mode for `ws`. */
  onRenameWorkspace: (ws: WorkspaceEntry) => void;
}

/**
 * Workspace switcher, opened from the rail's WorkspaceIndicator tile.
 * Lists every registered workspace and lets the user activate a different
 * one — this is the only surface for that mutation; there is no longer a
 * Settings equivalent. Activating restarts the whole app process because
 * the backend is embedded in it, and the confirm-then-restart sequencing
 * (with a persistent failure banner if the restart call itself fails) is
 * the owner-settled shape for a change this disruptive — do not diverge
 * from it.
 */
export function WorkspaceSwitcherPopover({
  open,
  onClose,
  anchorRef,
  workspaces,
  activePath,
  envOverrideActive,
  fallbackActive,
  fallbackFailedRoot,
  fallbackReason,
  onCreateWorkspace,
  onRenameWorkspace,
}: WorkspaceSwitcherPopoverProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [activateTarget, setActivateTarget] = useState<WorkspaceEntry | null>(null);
  const [activateError, setActivateError] = useState<string | null>(null);
  const addBanner = useBannerStore((s) => s.addBanner);

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

  const handleActivate = useCallback(async () => {
    if (!activateTarget) return;
    setActivateError(null);
    try {
      await activateWorkspace(activateTarget.id);
    } catch (err) {
      // Activation itself failed — the pointer was never moved (the server
      // probes the target data root before writing it; see
      // `crates/ao-server/src/routes/workspaces.rs`'s `activate_workspace`).
      // Surface why, right where the user is looking, and stop here: leave
      // `activateTarget` set so the confirm dialog stays open on the
      // previously-active workspace rather than proceeding to a restart
      // that would strand the user on a workspace that never actually
      // switched. This also covers a response we fail to parse as expected
      // — `activateWorkspace`/`throwApiError` always throws in that case
      // too, so it lands here rather than falling through to a restart.
      setActivateError(err instanceof Error ? err.message : "Failed to switch profile. Please try again.");
      return;
    }
    // From here on the registry's pointer has already moved. This process
    // MUST restart or the app is left silently running against the old
    // workspace while the registry claims a different one is active. Never
    // return past this point without either restarting or raising the
    // persistent banner below — see SettingsView.tsx's handleActivate,
    // which this mirrors.
    try {
      const outcome = await invoke("restart_app");
      if (outcome === "dev_restart_required") {
        // Debug build under `npm run tauri dev` — see `restart_app`'s doc
        // comment in `frontend/src-tauri/src/lib.rs`. The tauri-cli watcher
        // owns the dev server, so this process restarting itself would only
        // reconnect to a torn-down Vite instance. Nothing failed; the
        // pointer already switched, and finishing the job just requires a
        // manual dev-server restart. Reuses the same persistent-banner
        // affordance as the failure case below (worded as expected, not as
        // an error) rather than inventing a second banner.
        addBanner({
          id: "workspace-restart-pending",
          priority: 100,
          variant: "info",
          dismissible: false,
          message: "Switched the active profile. This is a dev build, so it won't restart itself — restart your dev server (npm run tauri dev) to finish switching.",
        });
        setActivateTarget(null);
      }
      // Otherwise the release path: `request_restart()` already fired and
      // this process is on its way down — there's nothing further to do.
    } catch (err) {
      console.error("[WorkspaceSwitcherPopover] restart_app failed after activation:", err);
      addBanner({
        id: "workspace-restart-pending",
        priority: 100,
        variant: "error",
        dismissible: false,
        message: "Switched the active profile, but the app failed to restart automatically. This window is still running on the previous profile — quit and reopen Launchpad Studio manually to finish switching.",
      });
      setActivateTarget(null);
    }
  }, [activateTarget, addBanner]);

  return (
    <>
      <AnimatePresence>
        {open && (
          <motion.div
            ref={ref}
            initial={{ opacity: 0, scale: 0.95, y: -4 }}
            animate={{ opacity: 1, scale: 1, y: 0 }}
            exit={{ opacity: 0, scale: 0.95, y: -4 }}
            transition={{ duration: 0.12, ease: "easeOut" }}
            className="workspace-switcher-popover absolute top-0 left-full ml-2 z-[1012] w-80 rounded-xl border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] shadow-lg p-1.5 select-none"
          >
            <div className="px-3 pt-1.5 pb-1 text-sm font-bold text-[var(--modal-text-primary)]">Workspaces</div>

            {envOverrideActive && (
              <div className="mx-1.5 mb-1.5 flex items-start gap-1.5 px-2.5 py-2 bg-amber-500/10 border border-amber-500/30 rounded-lg text-[11px] text-amber-700 dark:text-amber-400">
                <AlertTriangle className="w-[12px] h-[12px] mt-[1px] flex-shrink-0" />
                <span>Data root pinned by <span className="font-mono">{ENV_VAR_NAME}</span>. Unset it and relaunch to switch profiles.</span>
              </div>
            )}

            {fallbackActive && (
              <div
                data-testid="workspace-fallback-banner"
                className="mx-1.5 mb-1.5 flex items-start gap-1.5 px-2.5 py-2 bg-amber-500/10 border border-amber-500/30 rounded-lg text-[11px] text-amber-700 dark:text-amber-400"
              >
                <AlertTriangle className="w-[12px] h-[12px] mt-[1px] flex-shrink-0" />
                <span>
                  {FALLBACK_RECOVERY_MESSAGE}
                  {fallbackFailedRoot && (
                    <>
                      {" "}Failed to open: <span className="font-mono">{fallbackFailedRoot}</span>
                      {fallbackReason ? ` (${fallbackReason})` : ""}.
                    </>
                  )}
                </span>
              </div>
            )}

            <div className="max-h-[280px] overflow-y-auto flex flex-col gap-[2px]">
              {workspaces.length === 0 && (
                <div className="px-3 py-2 text-[13px] text-[var(--modal-text-secondary)]">No profiles yet.</div>
              )}
              {workspaces.map((ws) => {
                const isActive = ws.path === activePath;
                const disabled = envOverrideActive || isActive;
                return (
                  <div key={ws.id} className="group relative flex items-center">
                    <button
                      type="button"
                      data-testid={`workspace-row-${ws.id}`}
                      disabled={disabled}
                      title={
                        isActive
                          ? "Active profile"
                          : envOverrideActive
                            ? ENV_OVERRIDE_DISABLED_TITLE
                            : `Switch to ${ws.name} — restarts Launchpad Studio`
                      }
                      onClick={() => {
                        if (isActive || envOverrideActive) return;
                        setActivateError(null);
                        setActivateTarget(ws);
                      }}
                      className={twMerge(
                        "flex items-center gap-2.5 w-full px-3 py-2 pr-8 text-sm text-[var(--modal-text-primary)] rounded-lg transition-colors",
                        isActive ? "bg-[var(--modal-bg-hover)]" : "hover:bg-[var(--modal-bg-hover)] cursor-pointer",
                        envOverrideActive && !isActive && "opacity-50 cursor-not-allowed",
                      )}
                    >
                      <WorkspaceAvatar
                        name={ws.name}
                        path={ws.path}
                        emoji={ws.emoji}
                        color={ws.color}
                        size={22}
                        className="flex-shrink-0"
                      />
                      <span className="flex-1 min-w-0 truncate text-left">{ws.name}</span>
                      {isActive && <CheckCircle2 className="w-[14px] h-[14px] text-[var(--success)] flex-shrink-0" />}
                    </button>
                    <button
                      type="button"
                      data-testid={`workspace-rename-${ws.id}`}
                      aria-label={`Rename ${ws.name}`}
                      title={envOverrideActive ? ENV_OVERRIDE_DISABLED_TITLE : `Rename ${ws.name}`}
                      disabled={envOverrideActive}
                      onClick={(e) => {
                        e.stopPropagation();
                        if (envOverrideActive) return;
                        onClose();
                        onRenameWorkspace(ws);
                      }}
                      className="absolute right-[6px] top-1/2 -translate-y-1/2 w-[22px] h-[22px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)] opacity-0 group-hover:opacity-100 hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-opacity cursor-pointer disabled:opacity-0 disabled:cursor-not-allowed"
                    >
                      <Pencil className="w-[12px] h-[12px]" />
                    </button>
                  </div>
                );
              })}
            </div>

            <div className="mt-1 pt-1 border-t border-[var(--modal-border-secondary)]">
              <button
                type="button"
                data-testid="workspace-create-action"
                disabled={envOverrideActive}
                title={envOverrideActive ? ENV_OVERRIDE_DISABLED_TITLE : "Create workspace"}
                onClick={() => {
                  onClose();
                  onCreateWorkspace();
                }}
                className="flex items-center gap-2.5 w-full px-3 py-2.5 text-sm font-medium text-[var(--modal-accent)] hover:bg-[var(--modal-accent)]/10 rounded-lg transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:hover:bg-transparent"
              >
                <Plus size={16} />
                Create workspace
              </button>
            </div>
          </motion.div>
        )}
      </AnimatePresence>

      <ConfirmDialog
        open={activateTarget !== null}
        title="Restart to switch profile?"
        message={
          activateTarget ? (
            <div className="flex flex-col gap-3">
              <p>
                Switching to <span className="font-semibold text-[var(--modal-text-primary)]">{activateTarget.name}</span> writes the new active profile, then restarts Launchpad Studio for the switch to take effect. Any agent work currently in flight in this window will be interrupted.
              </p>
              {activateError && (
                <div className="flex items-start gap-2 px-3 py-2 bg-red-500/10 border border-red-500/30 rounded-[8px] text-[13px] text-red-600 dark:text-red-400">
                  <AlertTriangle className="w-[14px] h-[14px] mt-[1px] flex-shrink-0" />
                  <span>{activateError}</span>
                </div>
              )}
            </div>
          ) : (
            ""
          )
        }
        confirmLabel="Restart & switch"
        destructive
        onConfirm={handleActivate}
        onCancel={() => {
          setActivateTarget(null);
          setActivateError(null);
        }}
      />
    </>
  );
}

export default WorkspaceSwitcherPopover;
