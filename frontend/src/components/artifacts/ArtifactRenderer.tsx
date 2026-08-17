import { lazy, Suspense, useEffect, useMemo, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import {
  Box,
  Check,
  Copy,
  Download,
  ExternalLink,
  Loader2,
  MessageSquare,
  Pin,
  PinOff,
  Printer,
  RotateCw,
  Trash2,
  Undo2,
  X,
} from "lucide-react";
import { save } from "@tauri-apps/plugin-dialog";
import { writeFile } from "@tauri-apps/plugin-fs";
import * as api from "../../lib/api";
import { printArtifactWindow } from "../../lib/windows";
import type { ArtifactWithPayload } from "../../types/api";
import { useArtifactViewStore } from "../../stores/artifactViewStore";
import { useArtifactStore } from "../../stores/artifactStore";
import { Tooltip } from "../ui/Tooltip";
import ConfirmDialog from "../ui/ConfirmDialog";
import { resolveArtifactRenderer } from "./registry";
import { useArtifactRegen } from "./useArtifactRegen";

// Lazy, not a static import: `ArtifactChatPanel` now renders through the
// real `MessageBubble` (`components/chat/MessageBubble.tsx`) for bubble
// parity with Chat/Projects/Teams, and `MessageBubble` itself statically
// imports `ArtifactPreview` from *this* file — a static import here would
// close that into a load-time module cycle (this file -> ArtifactChatPanel
// -> MessageBubble -> this file). Deferring the import until the panel is
// actually opened (it's already conditionally rendered on `showChatPanel`
// below) breaks the cycle rather than papering over it: this module finishes
// defining/exporting `ArtifactPreview` before `ArtifactChatPanel`'s module
// graph ever loads, so `MessageBubble`'s circular reference back here always
// resolves against a fully-initialized module. Confirmed empirically —
// the static-import version reproducibly threw "element type is invalid"
// for `ChatInput` (a module with no artifact-side dependency at all,
// collateral damage from the cycle) when this file was the load's entry
// point, e.g. `ArtifactRenderer.test.tsx`.
const ArtifactChatPanel = lazy(() =>
  import("./ArtifactChatPanel").then((mod) => ({ default: mod.ArtifactChatPanel })),
);

/** Serialize an artifact's current payload to text — pretty-printed JSON for
 *  typed kinds, the raw markup for `html`. Shared by copy and download. */
function artifactText(artifact: ArtifactWithPayload): string {
  if (artifact.format === "html") {
    return typeof artifact.payload === "string" ? artifact.payload : "";
  }
  return JSON.stringify(artifact.payload, null, 2);
}

function downloadFilename(artifact: ArtifactWithPayload): string {
  const ext = artifact.format === "html" ? "html" : "json";
  const base =
    artifact.title
      .trim()
      .replace(/[^a-zA-Z0-9-_ ]/g, "")
      .trim()
      .replace(/\s+/g, "-") || artifact.id;
  return `${base}.${ext}`;
}

export interface ArtifactPreviewProps {
  /** Owning agent id (artifacts are scoped per-agent). */
  agentId: string | null;
  artifactId: string | null;
  onClose: () => void;
  /** Reserved seam for the pop-out-window surface: when supplied, a
   *  pop-out control appears in the header. This component never opens the
   *  window itself — `lib/windows.ts`'s per-artifact registry is a separate
   *  mount-point concern owned by the caller. */
  onPopOut?: (agentId: string, artifactId: string) => void;
  /** `"overlay"` (default) — a rounded, inset card meant to float above
   *  sibling content (inline chat card, Assets panel). `"window"` — this
   *  component IS the entire content of its own OS window
   *  (`ArtifactWindowView`), which already supplies its own frame/rounding,
   *  so the card renders flush/square instead of as a redundant nested
   *  rounded rect with a gap around it. */
  chrome?: "overlay" | "window";
  /** Fires once the artifact body has rendered (for `html`, the iframe's
   *  `load`). Only the popped-out window (`ArtifactWindowView`) passes this —
   *  it uses the signal to print itself once the artifact is on screen. */
  onBodyReady?: () => void;
}

/**
 * The self-contained artifact renderer — fetches an artifact by id, classifies
 * it into an `ArtifactKind`, and dispatches to the matching entry in the
 * renderer registry. Ships the same chrome as `TasklistOutputPreview` (copy /
 * download / close) plus loading/error states.
 *
 * Drivable by props (this component) or by the shared `useArtifactViewStore`
 * (via `ArtifactPortal` below) — the same store-or-props reuse
 * `TasklistOutputPortal`/`TaskDetailModal` get from `TasklistOutputPreview`.
 * That reuse is what lets one component serve inline, the Assets panel, and a
 * popped-out standalone window without a main-window singleton
 * assumption anywhere in it.
 */
export function ArtifactPreview({
  agentId,
  artifactId,
  onClose,
  onPopOut,
  chrome = "overlay",
  onBodyReady,
}: ArtifactPreviewProps) {
  const [artifact, setArtifact] = useState<ArtifactWithPayload | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [downloading, setDownloading] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [undoing, setUndoing] = useState(false);
  const [pinning, setPinning] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [showChatPanel, setShowChatPanel] = useState(false);
  // Ephemeral "Adjusting…/Regenerating… failed" notice — mirrors the
  // self-dismissing toast pattern `ChatInput`'s drop-rejection message uses
  // (local state + a timeout ref, no shared toast provider exists in this
  // app). Owned here rather than in `ArtifactChatPanel` because `regen` is a
  // single shared instance: whether the failed run was kicked off from the
  // header's Refresh button or the chat mini-thread, this is the one place
  // that sees every transition into `"error"` regardless of which (or
  // whether any) sub-panel is currently mounted to show it.
  const [regenErrorToast, setRegenErrorToast] = useState<string | null>(null);
  const regenErrorToastTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const togglePin = useArtifactStore((s) => s.togglePin);
  const deleteArtifact = useArtifactStore((s) => s.deleteArtifact);
  // Owned here, not inside `HtmlArtifactBody`, because the print button lives
  // in this component's shared header — the one header both the inline card
  // and the pop-out window (`ArtifactWindowView`) render through.
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const regen = useArtifactRegen(agentId, artifactId);
  // Gates `regen.resume()` to firing at most once per running task id, so
  // the resume effect below (which re-runs on every `artifact` update, not
  // just the initial mount fetch) doesn't re-call `resume()` on unrelated
  // re-renders. Reset alongside the rest of this component's per-artifact
  // state whenever `artifactId` changes (see the effect a few lines down).
  const resumedTaskIdRef = useRef<string | null>(null);

  useEffect(() => {
    if (!agentId || !artifactId) {
      setArtifact(null);
      setError(null);
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setError(null);
    setArtifact(null);
    api
      .getArtifact(agentId, artifactId)
      .then((a) => {
        if (cancelled) return;
        setArtifact(a);
        setLoading(false);
      })
      .catch((err) => {
        if (cancelled) return;
        setError((err as Error).message);
        setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [agentId, artifactId]);

  useEffect(() => {
    setCopied(false);
    setDownloading(false);
    setRefreshing(false);
    setShowChatPanel(false);
    if (regenErrorToastTimerRef.current) {
      clearTimeout(regenErrorToastTimerRef.current);
      regenErrorToastTimerRef.current = null;
    }
    setRegenErrorToast(null);
    resumedTaskIdRef.current = null;
  }, [artifactId]);

  // Resume-spinner-on-mount: the fetch above already pulls the full artifact
  // (including `running_task_id`) on every mount, so this reacts to that
  // same fetch rather than issuing a second request just to check whether a
  // background task is still running. If the freshly-fetched artifact says
  // one is, and this hook instance is otherwise idle (a fresh mount always
  // starts idle — see `useArtifactRegen`'s identity effect — since the
  // spinner state itself doesn't survive an unmount), hand it to
  // `regen.resume()` to restore "Adjusting…" and pick the poll loop back up
  // where a prior mount left off. `resumedTaskIdRef` gates this to firing
  // once per task id: `artifact` (and therefore this effect) can re-run for
  // reasons unrelated to resuming — e.g. a Refresh/Undo re-fetch — and
  // without the gate each of those would try to resume again.
  useEffect(() => {
    const taskId = artifact?.running_task_id;
    if (!taskId) return;
    if (regen.status !== "idle") return;
    if (resumedTaskIdRef.current === taskId) return;
    resumedTaskIdRef.current = taskId;
    regen.resume(taskId, artifact.updated_at, artifact.checksum_sha256);
    // Depends on `regen.status`/`regen.resume` individually, not the whole
    // `regen` object — `useArtifactRegen` returns a fresh object every
    // render, so depending on it directly would re-run this effect on every
    // render instead of only when the artifact or the hook's status change.
  }, [artifact, regen.status, regen.resume]);

  const isOpen = agentId !== null && artifactId !== null;

  const Body = useMemo(() => (artifact ? resolveArtifactRenderer(artifact.kind) : null), [artifact]);

  const handleCopy = async () => {
    if (!artifact) return;
    try {
      await navigator.clipboard.writeText(artifactText(artifact));
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch {
      // Silent — the button just won't flip to "copied".
    }
  };

  const handleDownload = async () => {
    if (!artifact) return;
    setDownloading(true);
    try {
      const filename = downloadFilename(artifact);
      const dot = filename.lastIndexOf(".");
      const ext = dot >= 0 ? filename.slice(dot + 1) : "";
      const savePath = await save({
        defaultPath: filename,
        filters: ext ? [{ name: ext.toUpperCase(), extensions: [ext] }] : undefined,
      });
      if (!savePath) return; // user cancelled
      await writeFile(savePath, new TextEncoder().encode(artifactText(artifact)));
    } catch {
      // Silent failure; the button resets.
    } finally {
      setDownloading(false);
    }
  };

  /** Whole-artifact-refreshable AND replayable (invariant
   *  ii): the server has an `origin_intent.refresh_prompt` it can replay
   *  through a fresh background agent. Mirrors the regenerate route's own
   *  gating (`crates/ao-server/src/routes/artifacts.rs::regenerate_artifact`)
   *  so the button never fires a request the server would just 409. */
  const canRegenerate =
    artifact?.refresh_intent === "whole_artifact" && !!artifact.origin_intent?.refresh_prompt?.trim();

  // Once a regenerate run lands, pull the freshly-written payload so the
  // renderer picks it up; surface a failed/timed-out run as the same error
  // banner the initial load uses, PLUS a toast — a rejected POST, a network
  // error, or the background adjust/regenerate agent itself failing all
  // collapse into this same `"error"` status (see `useArtifactRegen`), and
  // this is the one place that reacts to it whether the failure was
  // triggered from the header's Refresh button or the chat mini-thread, and
  // whether or not that chat panel is even still open to show its own
  // "Sorry, that didn't work" bubble. Without this, a user who closed the
  // chat panel (or never had it open) while a run was in flight got no
  // signal at all that it died — just the spinner quietly reverting.
  useEffect(() => {
    if (regen.status === "done") {
      if (!agentId || !artifactId) return;
      api
        .getArtifact(agentId, artifactId)
        .then((updated) => {
          setArtifact(updated);
          setError(null);
        })
        .catch((err) => setError((err as Error).message));
    } else if (regen.status === "error") {
      const message = regen.error ?? "Regenerate failed.";
      setError(message);
      if (regenErrorToastTimerRef.current) clearTimeout(regenErrorToastTimerRef.current);
      setRegenErrorToast(message);
      regenErrorToastTimerRef.current = setTimeout(() => setRegenErrorToast(null), 4000);
    }
  }, [regen.status, regen.error, agentId, artifactId]);

  /**
   * Refresh. When the artifact is
   * whole-artifact-refreshable AND carries a replayable origin prompt,
   * regenerates it from scratch via `useArtifactRegen` (re-runs the
   * original request through a background agent, then this component
   * re-fetches once that write lands). Otherwise falls back to a cheap
   * re-fetch of the already-stored payload — the one refresh-shaped action
   * available for artifacts without a replayable recipe.
   */
  const handleRefresh = async () => {
    if (!agentId || !artifactId || refreshing) return;
    if (canRegenerate) {
      if (regen.status === "working") return;
      await regen.start();
      return;
    }
    setRefreshing(true);
    try {
      const updated = await api.getArtifact(agentId, artifactId);
      setArtifact(updated);
      setError(null);
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setRefreshing(false);
    }
  };

  /**
   * Undo — a synchronous single-step revert of the artifact's last edit, so
   * unlike {@link handleRefresh}'s regenerate path it needs neither
   * `useArtifactRegen`'s polling nor a spawned background agent: the POST
   * itself completes the revert. Re-fetches via the exact same
   * `api.getArtifact` call the Refresh button's non-regenerate branch uses
   * so the restored body renders through the one shared fresh-by-id path
   * (no duplicate card, pin state preserved) instead of rendering off the
   * undo response directly.
   */
  const handleUndo = async () => {
    if (!agentId || !artifactId || !artifact || undoing || !artifact.undo_available) return;
    setUndoing(true);
    try {
      await api.undoArtifact(agentId, artifactId);
      const updated = await api.getArtifact(agentId, artifactId);
      setArtifact(updated);
      setError(null);
    } catch (err) {
      if (err instanceof api.ApiError && err.status === 409) {
        // Nothing left to undo — reflect that immediately rather than
        // waiting on a subsequent getArtifact to catch up.
        setArtifact((current) => (current ? { ...current, undo_available: false } : current));
      } else {
        setError((err as Error).message);
      }
    } finally {
      setUndoing(false);
    }
  };

  /** Save-to-Assets toggle (PRD: pin-to-save, global across agents). Flips
   *  local state optimistically so the icon responds instantly, and relies
   *  on `artifactStore.togglePin` to keep the Assets page's cross-agent
   *  cache and this artifact's per-agent cache in sync — reverts local state
   *  too if the request fails. */
  const handleTogglePin = async () => {
    if (!agentId || !artifactId || !artifact || pinning) return;
    const next = !artifact.pinned;
    setPinning(true);
    setArtifact({ ...artifact, pinned: next });
    try {
      await togglePin(agentId, artifactId, next);
    } catch {
      setArtifact((current) => (current ? { ...current, pinned: !next } : current));
    } finally {
      setPinning(false);
    }
  };

  /** Delete confirmed via `ConfirmDialog` below. Surfaces a failure through
   *  the same error banner the initial load uses, rather than closing the
   *  view — `deleteArtifact` already rolled its optimistic store update back
   *  by the time this catch runs. */
  const handleDelete = async () => {
    if (!agentId || !artifactId || deleting) return;
    setDeleting(true);
    try {
      await deleteArtifact(agentId, artifactId);
      setConfirmingDelete(false);
      onClose();
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setDeleting(false);
    }
  };

  /** Prints just the artifact's own content, using its own `@page`/print CSS
   *  — the in-app replacement for "open in a browser tab, ⌘P, toggle
   *  Background graphics". Routes through the artifact's own pop-out window
   *  rather than trying to print from here: the artifact is a sandboxed
   *  opaque-origin child frame, `Window.print` is not cross-origin-accessible
   *  on it (reads as `undefined` from the parent), and Tauri only patches
   *  `window.print()` to reach the native macOS print panel on a webview's
   *  *top* frame — never a nested one, and never without printing the whole
   *  app window (sidebar/chat) if we called it on this window's top frame.
   *  The pop-out is a genuine separate top-level webview containing only the
   *  artifact, so it satisfies both constraints. See `printArtifactWindow`. */
  const handlePrint = () => {
    if (!agentId || !artifactId) return;
    // Fire-and-forget: a failed pop-out open/focus shouldn't surface as an
    // unhandled rejection or crash the header.
    void printArtifactWindow(agentId, artifactId).catch(() => {});
  };

  const isIframeKind = artifact?.kind === "html";

  return (
    <>
      <AnimatePresence>
        {isOpen && (
          <motion.div
            key={`${agentId}:${artifactId}`}
            className={
              chrome === "window"
                ? "relative w-full h-full flex flex-col overflow-hidden"
                : "absolute inset-0 z-30 flex flex-col rounded-[16px] overflow-hidden"
            }
            style={{ backgroundColor: "var(--bg-secondary)" }}
            initial={{ opacity: 0, scale: 0.98 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.98 }}
            transition={{
              scale: { type: "spring", stiffness: 320, damping: 30, mass: 0.7 },
              opacity: { duration: 0.18, ease: "easeOut" },
            }}
          >
            {/* Header — `data-artifact-chrome` marks it as app chrome the
                pop-out window's print stylesheet hides, so a printed artifact
                is its content only, not this toolbar (see App.css `@media
                print`). */}
            <div
              data-artifact-chrome="header"
              className="px-4 py-3 flex items-center gap-2 shrink-0 border-b"
              style={{ borderColor: "var(--border-primary)" }}
            >
              <Box size={14} style={{ color: "var(--text-secondary)" }} />
              <Tooltip placement="top" label={artifact?.title ?? ""} className="flex-1 min-w-0">
                <span
                  className="block text-[13px] font-semibold truncate"
                  style={{ color: "var(--text-primary)" }}
                >
                  {artifact?.title ?? ""}
                </span>
              </Tooltip>

              {artifact && (
                <Tooltip placement="top" label={copied ? "Copied" : "Copy to clipboard"}>
                  <button
                    type="button"
                    onClick={handleCopy}
                    aria-label="Copy to clipboard"
                    className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)]"
                    style={{ color: copied ? "#16a34a" : "var(--text-secondary)" }}
                  >
                    {copied ? <Check size={14} /> : <Copy size={13} />}
                  </button>
                </Tooltip>
              )}

              {artifact && (
                <Tooltip placement="top" label="Download">
                  <button
                    type="button"
                    onClick={handleDownload}
                    disabled={downloading}
                    aria-label="Download"
                    className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)] disabled:opacity-50"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    {downloading ? <Loader2 size={13} className="animate-spin" /> : <Download size={13} />}
                  </button>
                </Tooltip>
              )}

              {artifact && isIframeKind && (
                <Tooltip placement="top" label="Print">
                  <button
                    type="button"
                    onClick={handlePrint}
                    aria-label="Print"
                    className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)]"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    <Printer size={13} />
                  </button>
                </Tooltip>
              )}

              {artifact && artifact.refresh_intent !== "none" && (() => {
                const regenerating = canRegenerate && regen.status === "working";
                const busy = refreshing || regenerating;
                return (
                  <Tooltip placement="top" label={regenerating ? "Regenerating…" : "Refresh"}>
                    <button
                      type="button"
                      onClick={handleRefresh}
                      disabled={busy}
                      aria-label={regenerating ? "Regenerating artifact" : "Refresh artifact"}
                      className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)] disabled:opacity-50"
                      style={{ color: "var(--text-secondary)" }}
                    >
                      <RotateCw size={13} className={busy ? "animate-spin" : undefined} />
                    </button>
                  </Tooltip>
                );
              })()}

              {artifact && agentId && artifactId && (
                <Tooltip placement="top" label={artifact.undo_available ? "Undo last edit" : "Nothing to undo"}>
                  <button
                    type="button"
                    onClick={handleUndo}
                    disabled={!artifact.undo_available || undoing}
                    aria-label="Undo last edit"
                    className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)] disabled:opacity-50"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    {undoing ? <Loader2 size={13} className="animate-spin" /> : <Undo2 size={13} />}
                  </button>
                </Tooltip>
              )}

              {artifact && agentId && artifactId && (
                <Tooltip placement="top" label={showChatPanel ? "Close chat" : "Chat to adjust"}>
                  <button
                    type="button"
                    onClick={() => setShowChatPanel((v) => !v)}
                    aria-pressed={showChatPanel}
                    aria-label="Toggle chat panel"
                    className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)]"
                    style={{ color: showChatPanel ? "var(--accent-primary, #6366f1)" : "var(--text-secondary)" }}
                  >
                    <MessageSquare size={13} />
                  </button>
                </Tooltip>
              )}

              {artifact && agentId && artifactId && (
                <Tooltip placement="top" label={artifact.pinned ? "Unpin from Assets" : "Pin to Assets"}>
                  <button
                    type="button"
                    onClick={handleTogglePin}
                    disabled={pinning}
                    aria-label={artifact.pinned ? "Unpin from Assets" : "Pin to Assets"}
                    aria-pressed={artifact.pinned}
                    className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)] disabled:opacity-50"
                    style={{ color: artifact.pinned ? "var(--accent-primary, #6366f1)" : "var(--text-secondary)" }}
                  >
                    {artifact.pinned ? <Pin size={13} fill="currentColor" /> : <PinOff size={13} />}
                  </button>
                </Tooltip>
              )}

              {onPopOut && agentId && artifactId && (
                <Tooltip placement="top" label="Open in new window">
                  <button
                    type="button"
                    onClick={() => onPopOut(agentId, artifactId)}
                    aria-label="Open in new window"
                    className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)]"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    <ExternalLink size={13} />
                  </button>
                </Tooltip>
              )}

              {/* Delete — separate from the controls above, kept minimal to
                  avoid conflicting with concurrent edits to the refresh
                  button's block. */}
              {artifact && agentId && artifactId && (
                <Tooltip placement="top" label="Delete">
                  <button
                    type="button"
                    onClick={() => setConfirmingDelete(true)}
                    disabled={deleting}
                    aria-label="Delete artifact"
                    className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)] disabled:opacity-50"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    {deleting ? <Loader2 size={13} className="animate-spin" /> : <Trash2 size={13} />}
                  </button>
                </Tooltip>
              )}

              <button
                type="button"
                onClick={onClose}
                aria-label="Close artifact"
                className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center transition-colors hover:bg-[var(--bg-hover)]"
                style={{ color: "var(--text-secondary)" }}
              >
                <X size={14} />
              </button>
            </div>

            {/* Regenerate/chat-adjust failure toast — self-dismissing,
                mirrors `ChatInput`'s drop-rejection toast styling/behavior
                (no shared toast provider exists in this app to hook into
                instead). Floats just under the header, visible regardless
                of whether the chat panel is open — see the `regen.status
                === "error"` effect above for why this lives here rather
                than in `ArtifactChatPanel`. */}
            <AnimatePresence>
              {regenErrorToast && (
                <motion.div
                  data-testid="artifact-regen-error-toast"
                  data-artifact-chrome="toast"
                  initial={{ opacity: 0, y: -8 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, y: -8 }}
                  transition={{ duration: 0.2 }}
                  className="absolute top-14 left-1/2 -translate-x-1/2 z-40 px-3 py-1.5 rounded-lg bg-red-500/90 text-white text-xs whitespace-nowrap shadow-lg"
                >
                  {regenErrorToast}
                </motion.div>
              )}
            </AnimatePresence>

            {/* Body row — the per-kind body fills the remaining space, and
                the chat mini-thread (when open) sits alongside it as a
                fixed-width column rather than an overlay, so neither ever
                covers the other. */}
            <div className="flex-1 min-h-0 flex overflow-hidden">
              {isIframeKind ? (
                // No padding here (unlike the typed-kind wrapper below) — the
                // iframe sits edge-to-edge against the card chrome. This wrapper
                // carries its own `overflow-hidden` + bottom radius (matching the
                // card's) instead of trusting the outer `motion.div`'s clip to
                // reach the iframe: WebKit doesn't reliably clip an iframe through
                // a *transformed* ancestor (the card is animated via framer-motion
                // `scale`), which otherwise leaves the iframe's square corners
                // poking a sliver of its own background past the rounded clip —
                // visible as a stray light corner against dark artifact content.
                // Being the iframe's direct (untransformed) parent helps, but
                // doesn't fully sidestep it: WebKit's clipping bug isn't scoped
                // to the immediate parent, it's scoped to the iframe's whole
                // compositing-layer ancestry, and this wrapper still sits under
                // the animated `motion.div` two levels up. So `roundedBottom` is
                // threaded down to `HtmlArtifactBody` too, which puts a matching
                // radius directly on the iframe's own box — clipping the
                // iframe's *own* paint rather than relying on an ancestor's mask
                // to reach through it. Window chrome renders flush/square
                // already, so no radius is needed there. Loading/error states
                // get their own margin since they're plain text/spinner, not a
                // panel.
                <div
                  className={`flex-1 min-h-0 flex flex-col overflow-hidden ${
                    chrome === "overlay" ? "rounded-b-[16px]" : ""
                  }`}
                >
                  {loading && (
                    <div className="flex-1 min-h-0 flex items-center justify-center">
                      <Loader2 size={18} className="animate-spin" style={{ color: "var(--text-secondary)" }} />
                    </div>
                  )}
                  {error && (
                    <div
                      className="m-3 px-3 py-2 rounded-[10px] text-[12px]"
                      style={{ backgroundColor: "rgba(244,63,94,0.12)", color: "#be123c" }}
                    >
                      {error}
                    </div>
                  )}
                  {!loading && artifact && Body && (
                    <Body
                      artifact={artifact}
                      iframeRef={iframeRef}
                      roundedBottom={chrome === "overlay"}
                      onReady={onBodyReady}
                    />
                  )}
                </div>
              ) : (
                <div className="flex-1 overflow-y-auto px-4 py-3 min-h-0 custom-scrollbar">
                  {loading && (
                    <div className="flex items-center justify-center py-12">
                      <Loader2 size={18} className="animate-spin" style={{ color: "var(--text-secondary)" }} />
                    </div>
                  )}
                  {error && (
                    <div
                      className="px-3 py-2 rounded-[10px] text-[12px]"
                      style={{ backgroundColor: "rgba(244,63,94,0.12)", color: "#be123c" }}
                    >
                      {error}
                    </div>
                  )}
                  {!loading && artifact && Body && <Body artifact={artifact} />}
                </div>
              )}
              {showChatPanel && agentId && artifactId && (
                // `display:contents` wrapper keeps the panel a direct flex
                // item of this row (no layout change), while giving the print
                // stylesheet a `data-artifact-chrome` hook to hide it so it
                // never lands in a popped-out artifact's printout.
                <div data-artifact-chrome="chat" className="contents">
                  <Suspense fallback={null}>
                    <ArtifactChatPanel
                      agentId={agentId}
                      artifactId={artifactId}
                      regen={regen}
                      onClose={() => setShowChatPanel(false)}
                    />
                  </Suspense>
                </div>
              )}
            </div>
          </motion.div>
        )}
      </AnimatePresence>

      <ConfirmDialog
        open={confirmingDelete}
        title="Delete artifact"
        message={
          artifact ? (
            <>
              Delete{" "}
              <span className="font-semibold" style={{ color: "var(--modal-text-primary)" }}>
                {artifact.title}
              </span>
              ? This can&apos;t be undone.
            </>
          ) : (
            ""
          )
        }
        confirmLabel="Delete"
        destructive
        onConfirm={handleDelete}
        onCancel={() => setConfirmingDelete(false)}
      />
    </>
  );
}

/**
 * Store-driven wrapper over `ArtifactPreview`, reading the active
 * agent/artifact from `useArtifactViewStore`. Mirrors `TasklistOutputPortal`'s
 * relationship to `TasklistOutputPreview` — a mount point that already has
 * the id pair in local state should render `ArtifactPreview` directly instead.
 */
export function ArtifactPortal() {
  const agentId = useArtifactViewStore((s) => s.agentId);
  const artifactId = useArtifactViewStore((s) => s.artifactId);
  const close = useArtifactViewStore((s) => s.close);

  return <ArtifactPreview agentId={agentId} artifactId={artifactId} onClose={close} />;
}
