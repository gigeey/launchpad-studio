import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { Tooltip } from "./ui/Tooltip";
import { WorkspaceAvatar } from "./WorkspaceAvatar";
import { WorkspaceSwitcherPopover } from "./WorkspaceSwitcherPopover";
import { WorkspaceEditModal, type WorkspaceEditModalMode } from "./WorkspaceEditModal";
import {
  BASE_URL,
  getActiveWorkspace,
  getWorkspaces,
  type ActiveWorkspaceResponse,
  type WorkspaceEntry,
} from "../lib/api";

/** Port this build's frontend talks to (`VITE_API_BASE_URL`, baked in at
 *  build time) — `null` if `BASE_URL` doesn't parse as a URL with an
 *  explicit port (shouldn't happen for the `http://localhost:PORT` values
 *  this app ships, but a malformed override shouldn't crash the rail). */
function apiPort(): string | null {
  try {
    return new URL(BASE_URL).port || null;
  } catch {
    return null;
  }
}

// Named literally so the person who exported it can grep this string and
// find both the UI copy and their own shell config in one search.
const ENV_VAR_NAME = "LAUNCHPAD_STUDIO_DATA_DIR";

/** Whether a `StartupFallback.error` string (a Rust error's `Display` text)
 *  is short and plain enough to show a user directly in the fallback
 *  banner, rather than a raw multi-line debug dump. A judgement call, not a
 *  contract — errs toward omitting rather than showing something
 *  unreadable, since `WorkspaceSwitcherPopover`'s banner already states the
 *  actionable part (pick a workspace below) without it. */
function isHumanLegibleFallbackError(error: string | null | undefined): error is string {
  if (!error) return false;
  const trimmed = error.trim();
  return trimmed.length > 0 && trimmed.length <= 140 && !trimmed.includes("\n");
}

// Neutral fallback for the tile background when there's no matching
// registry entry to pull a real `color` from (see `activeEntry` below) —
// never a real WORKSPACE_COLOR_PALETTE value, so it can't be mistaken for
// one.
const FALLBACK_TILE_COLOR = "var(--bg-hover)";

/** Rail tile naming the workspace this window's backend is currently
 *  reading and writing, and the entry point into the workspace switcher
 *  popover.
 *
 * The active workspace's *name/path* are sourced entirely from
 * `GET /workspaces/active`, which reports the same env-var → registry →
 * home-default precedence the server itself applies
 * (`ao_protocol::data_root::resolve_data_root_with_provenance`). This is
 * deliberate: `GET /workspaces` reads the on-disk registry
 * (`~/.launchpad_studio/workspaces.json`, one fixed path shared by every
 * process on the machine) and has no way to reflect a
 * `LAUNCHPAD_STUDIO_DATA_DIR` override, which always outranks the registry
 * when the server resolves its real data root. This component must never
 * derive the active *label* from `getWorkspaces()` — the provenance
 * `/workspaces/active` reports is the only thing that decides what's shown
 * for that.
 *
 * `getWorkspaces()` is still fetched, for two purposes only: the tile's
 * cosmetic `color`/`emoji` (looked up by matching the active path against
 * a registry entry — `/workspaces/active` doesn't carry those fields), and
 * the full list handed to the switcher popover.
 */
export function WorkspaceIndicator() {
  const [active, setActive] = useState<ActiveWorkspaceResponse | null>(null);
  const [workspaces, setWorkspaces] = useState<WorkspaceEntry[]>([]);
  const [popoverOpen, setPopoverOpen] = useState(false);
  // Create/rename modal — `null` when closed. `workspace` is only ever set
  // for `mode: "rename"`; create mode has nothing to pre-fill from.
  const [editModal, setEditModal] = useState<{ mode: WorkspaceEditModalMode; workspace: WorkspaceEntry | null } | null>(
    null,
  );
  const anchorRef = useRef<HTMLDivElement>(null);
  // Guards the two loaders below against setting state after unmount —
  // shared across the mount-time effects and the post-save reload the edit
  // modal triggers, so it has to live outside any single effect's closure
  // (unlike the old per-effect `cancelled` locals this replaces).
  //
  // The setup half is load-bearing, not ceremonial: StrictMode double-invokes
  // effects (mount -> cleanup -> mount), so a cleanup-only effect latches this
  // to false on the simulated unmount and never recovers, permanently
  // discarding every loader response. Re-arming on each setup is what makes a
  // shared ref behave like the per-effect locals it replaced.
  const mountedRef = useRef(true);
  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const loadActive = useCallback(() => {
    getActiveWorkspace()
      .then((res) => {
        if (mountedRef.current) setActive(res);
      })
      .catch((err) => {
        // Stay hidden rather than showing a banner — an unreachable
        // /workspaces/active endpoint isn't worth one on its own, and every
        // other surface that actually depends on the backend already has
        // its own offline handling. Still worth a console line, though: a
        // failure here is also the only way this component could miss a
        // real `provenance: "fallback"` response (the fetch itself failing,
        // as opposed to succeeding with a fallback payload), and that
        // shouldn't be silently invisible even if it isn't worth a banner.
        console.warn("[WorkspaceIndicator] failed to load active workspace:", err);
        if (mountedRef.current) setActive(null);
      });
  }, []);

  const loadWorkspaces = useCallback(() => {
    getWorkspaces()
      .then((res) => {
        if (mountedRef.current) setWorkspaces(res.workspaces);
      })
      .catch((err) => {
        // Same reasoning as above — the tile still renders off `active`
        // alone (with a neutral fallback color/emoji), and the popover just
        // shows an empty list rather than an error.
        console.warn("[WorkspaceIndicator] failed to load workspace list:", err);
        if (mountedRef.current) setWorkspaces([]);
      });
  }, []);

  useEffect(() => {
    loadActive();
  }, [loadActive]);

  useEffect(() => {
    loadWorkspaces();
  }, [loadWorkspaces]);

  if (!active) return null;

  // Only a genuine, inherited env pin — never a self-inflicted startup
  // fallback (`provenance === "fallback"` reports as its own branch
  // precisely so this stays false for it). Gates the same
  // switch/rename/create disabling `WorkspaceSwitcherPopover` applies for
  // `envOverrideActive`; a fallback boot must leave all of that enabled,
  // since activating a different workspace is the only way out of it.
  const isEnvOverride = active.provenance === "env_override";
  const isFallback = active.provenance === "fallback";
  // Registry branch: the registry name is authoritative. Home-default and
  // fallback branches: there is no registry entry at all — for home-default
  // that's the ordinary, expected state for a fresh install; for fallback
  // it's because startup redirected here after the resolved workspace
  // failed to open, but the resolved *path* is the same default root
  // either way, so the label is too. Env-override branch: there is no
  // workspace *name* to show (the registry was never consulted), so the
  // resolved path itself is the only honest label.
  const label =
    active.provenance === "registry"
      ? (active.name ?? active.path)
      : active.provenance === "home_default" || isFallback
        ? "Default profile"
        : active.path;

  const port = apiPort();

  // `isHumanLegibleFallbackError` is the only place that decides whether to
  // show `startup_fallback.error` at all — see its doc comment.
  const fallbackReason = isHumanLegibleFallbackError(active.startup_fallback?.error)
    ? active.startup_fallback.error
    : null;

  const tooltipLabel = isEnvOverride
    ? `Data root pinned by ${ENV_VAR_NAME}=${active.path}. Switching profiles requires unsetting that environment variable and relaunching Launchpad Studio.`
    : isFallback
      ? "Your selected workspace could not be opened, so Launchpad Studio started on the default profile. Open the switcher to recover."
      : `Active profile: ${label}${port ? ` — port ${port}` : ""}`;

  // Cosmetic-only lookup — see the docstring above for why this never
  // decides the label itself, only the swatch/emoji.
  const activeEntry = workspaces.find((ws) => ws.path === active.path) ?? null;
  const tileColor = activeEntry?.color ?? FALLBACK_TILE_COLOR;

  // After a successful create/rename, reload both — the new/renamed entry
  // needs to show up in the popover's list (`loadWorkspaces`) and, if it
  // was the active workspace being renamed, the tile's own label
  // (`loadActive`, since that label is sourced from `/workspaces/active`'s
  // `name`, never from `getWorkspaces` — see the docstring above).
  const reloadAfterSave = () => {
    loadActive();
    loadWorkspaces();
  };

  return (
    <div className="relative w-full flex-shrink-0" ref={anchorRef}>
      <Tooltip label={tooltipLabel} className="w-full justify-center">
        <div
          role="button"
          tabIndex={0}
          aria-label={`Active workspace: ${label}. Open workspace switcher.`}
          data-testid="workspace-tile"
          className="w-full cursor-pointer flex items-center justify-center transition-opacity group"
          onClick={() => setPopoverOpen((o) => !o)}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              setPopoverOpen((o) => !o);
            }
          }}
        >
          <div className="relative w-[36px] h-[36px] transition-opacity group-hover:opacity-85">
            <WorkspaceAvatar
              name={activeEntry?.name ?? active.name}
              path={active.path}
              emoji={activeEntry?.emoji}
              color={tileColor}
              size={36}
            />
            {isEnvOverride && (
              <span
                aria-hidden="true"
                data-testid="workspace-tile-env-badge"
                className="absolute -bottom-[3px] -right-[3px] w-[14px] h-[14px] rounded-full bg-amber-500 border-2 border-[var(--bg-primary)] flex items-center justify-center"
              >
                <AlertTriangle size={8} className="text-white" strokeWidth={3} />
              </span>
            )}
            {isFallback && (
              <span
                aria-hidden="true"
                data-testid="workspace-tile-fallback-badge"
                className="absolute -bottom-[3px] -right-[3px] w-[14px] h-[14px] rounded-full bg-amber-500 border-2 border-[var(--bg-primary)] flex items-center justify-center"
              >
                <AlertTriangle size={8} className="text-white" strokeWidth={3} />
              </span>
            )}
          </div>
        </div>
      </Tooltip>
      <WorkspaceSwitcherPopover
        open={popoverOpen}
        onClose={() => setPopoverOpen(false)}
        anchorRef={anchorRef}
        workspaces={workspaces}
        activePath={active.path}
        envOverrideActive={isEnvOverride}
        fallbackActive={isFallback}
        fallbackFailedRoot={isFallback ? (active.startup_fallback?.failed_root ?? null) : null}
        fallbackReason={isFallback ? fallbackReason : null}
        onCreateWorkspace={() => setEditModal({ mode: "create", workspace: null })}
        onRenameWorkspace={(ws) => setEditModal({ mode: "rename", workspace: ws })}
      />
      <WorkspaceEditModal
        open={editModal !== null}
        mode={editModal?.mode ?? "create"}
        workspace={editModal?.workspace ?? null}
        onClose={() => setEditModal(null)}
        onSaved={reloadAfterSave}
      />
    </div>
  );
}

export default WorkspaceIndicator;
