import { useEffect, useState, type FormEvent } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, motion } from "framer-motion";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen, Loader2, X } from "lucide-react";
import { twMerge } from "tailwind-merge";
import {
  createWorkspace,
  renameWorkspace,
  WORKSPACE_COLOR_PALETTE,
  type WorkspaceEntry,
} from "../lib/api";
import { EmojiPicker } from "./ui/EmojiPicker";
import { WorkspaceAvatar } from "./WorkspaceAvatar";
import { useBannerStore } from "../stores/bannerStore";

export type WorkspaceEditModalMode = "create" | "rename";

export interface WorkspaceEditModalProps {
  open: boolean;
  mode: WorkspaceEditModalMode;
  /** The workspace being renamed. Read in `mode === "rename"` only — ignored
   *  (and may be null) in create mode. */
  workspace: WorkspaceEntry | null;
  onClose: () => void;
  /** Fired right after a successful create/rename, before `onClose` — lets
   *  the caller refresh its workspace list/active-tile state before the
   *  modal unmounts. */
  onSaved: () => void;
}

const FIELD_INPUT_CLASS =
  "w-full h-[36px] px-[12px] rounded-[8px] border border-[#bbb] dark:border-[var(--modal-border-secondary)] bg-white dark:bg-[var(--modal-bg-tertiary)] text-[14px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] focus:shadow-[0_0_0_1px_var(--modal-accent)] transition-all disabled:opacity-60 disabled:cursor-not-allowed";

// Fixed id so a second failed attempt (e.g. retrying after fixing the name)
// replaces the prior banner instead of stacking a duplicate — same
// replace-by-id behavior `addBanner` already gives the restart-failure
// banner in WorkspaceSwitcherPopover.
const ERROR_BANNER_ID = "workspace-edit-error";

/**
 * Create/rename workspace modal, opened from the rail's workspace switcher
 * (`WorkspaceIndicator` → `WorkspaceSwitcherPopover`). One component covers
 * both flows since the fields are almost identical — `mode` only decides
 * whether `path` is editable and which `lib/api.ts` call `handleSubmit`
 * makes.
 *
 * Only mounts `WorkspaceEditModalBody` while `open` is true (rather than
 * rendering it always and toggling visibility), so every field's `useState`
 * initializer re-seeds from `workspace`/`mode` on each open instead of
 * needing a separate reset effect — same pattern `AgentProfileModal` uses.
 */
export function WorkspaceEditModal({ open: isOpen, mode, workspace, onClose, onSaved }: WorkspaceEditModalProps) {
  return (
    <AnimatePresence>
      {isOpen && (
        <WorkspaceEditModalBody
          key={mode === "rename" ? `rename-${workspace?.id}` : "create"}
          mode={mode}
          workspace={workspace}
          onClose={onClose}
          onSaved={onSaved}
        />
      )}
    </AnimatePresence>
  );
}

function WorkspaceEditModalBody({
  mode,
  workspace,
  onClose,
  onSaved,
}: {
  mode: WorkspaceEditModalMode;
  workspace: WorkspaceEntry | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const isRename = mode === "rename";
  const [name, setName] = useState(() => (isRename ? (workspace?.name ?? "") : ""));
  const [path, setPath] = useState(() => (isRename ? (workspace?.path ?? "") : ""));
  // `null` means "no emoji" (the letter avatar) — genuinely unset, not a
  // placeholder waiting to be filled in. The picker must open with nothing
  // preselected: create mode always starts here, and rename mode only
  // starts with a value when the workspace already has one.
  const [emoji, setEmoji] = useState<string | null>(() => (isRename ? (workspace?.emoji ?? null) : null));
  // First palette entry is the create-mode default — fixed rather than
  // random so the picker always opens with a real, visibly-selected swatch
  // instead of nothing highlighted; the user can still change it before
  // submitting.
  const [color, setColor] = useState(() => (isRename ? (workspace?.color ?? WORKSPACE_COLOR_PALETTE[0]) : WORKSPACE_COLOR_PALETTE[0]));
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const addBanner = useBannerStore((s) => s.addBanner);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !submitting) onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [onClose, submitting]);

  const handleBrowse = async () => {
    const selected = await open({ directory: true, multiple: false });
    if (selected) setPath(selected as string);
  };

  const requestClose = () => {
    if (!submitting) onClose();
  };

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    if (!name.trim()) {
      setError(isRename ? "Name must not be empty." : "Name is required.");
      return;
    }
    if (!isRename && !path.trim()) {
      setError("Choose a folder for this profile.");
      return;
    }
    setSubmitting(true);
    try {
      if (isRename) {
        if (!workspace) throw new Error("No profile selected to rename.");
        await renameWorkspace(workspace.id, name.trim(), color, emoji);
      } else {
        await createWorkspace(name.trim(), path.trim(), color, emoji);
      }
      onSaved();
      onClose();
    } catch (err) {
      const message =
        err instanceof Error ? err.message : `Failed to ${isRename ? "rename" : "create"} profile. Please try again.`;
      setError(message);
      // Also raised as a banner — a rejection here (most notably the 409
      // pinned-data-root case) reflects a whole-app condition, not just a
      // mistake in this one field, so it should stay visible even if the
      // modal is dismissed before the inline message above is read. Same
      // store WorkspaceSwitcherPopover already uses for its own persistent
      // restart-failure banner.
      addBanner({
        id: ERROR_BANNER_ID,
        priority: 60,
        variant: "error",
        dismissible: true,
        message,
      });
    } finally {
      setSubmitting(false);
    }
  };

  const title = isRename ? "Rename Workspace" : "Create Workspace";

  return createPortal(
    <div className="fixed inset-0 z-[400] flex items-center justify-center">
      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        exit={{ opacity: 0 }}
        transition={{ duration: 0.15 }}
        className="absolute inset-0 bg-black/40"
        onClick={requestClose}
      />
      <motion.div
        initial={{ opacity: 0, scale: 0.96 }}
        animate={{ opacity: 1, scale: 1 }}
        exit={{ opacity: 0, scale: 0.96 }}
        transition={{ duration: 0.15, ease: "easeOut" }}
        role="dialog"
        aria-modal="true"
        aria-labelledby="workspace-edit-modal-title"
        className="relative w-full max-w-[420px] rounded-[12px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)] flex flex-col"
        style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
      >
        <form onSubmit={handleSubmit} className="flex flex-col">
          <div className="flex items-center justify-between px-[22px] pt-[20px] pb-[4px]">
            <h2 id="workspace-edit-modal-title" className="text-[17px] font-semibold text-[var(--modal-text-primary)]">
              {title}
            </h2>
            <button
              type="button"
              onClick={requestClose}
              aria-label="Close"
              className="w-[28px] h-[28px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
            >
              <X className="w-[15px] h-[15px]" />
            </button>
          </div>

          <div className="flex flex-col gap-[14px] px-[22px] py-[14px]">
            {error && (
              <div
                data-testid="workspace-edit-error"
                className="px-3 py-2 bg-red-500/10 border border-red-500/30 rounded-[8px] text-[13px] text-red-600 dark:text-red-400"
              >
                {error}
              </div>
            )}

            <div className="flex items-center gap-[14px]">
              <div className="flex flex-col items-center gap-[6px]">
                <EmojiPicker
                  value={emoji}
                  onChange={setEmoji}
                  ariaLabel="Pick profile emoji"
                  triggerClassName="w-[52px] h-[52px] flex-shrink-0 rounded-[12px] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] flex items-center justify-center text-[26px] hover:border-[var(--modal-accent)] transition-colors cursor-pointer select-none"
                />
                <button
                  type="button"
                  data-testid="workspace-edit-clear-emoji"
                  onClick={() => setEmoji(null)}
                  disabled={!emoji}
                  className="text-[11px] font-medium text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
                >
                  No emoji
                </button>
              </div>
              {/* Live preview — the actual `WorkspaceAvatar` render, not a
                  hand-rolled approximation, so it shows exactly which of
                  the two avatar states (letter vs. emoji) the current
                  picks resolve to. */}
              <div
                data-testid="workspace-edit-avatar-preview"
                title="Preview"
                aria-hidden="true"
                className="flex-shrink-0"
              >
                <WorkspaceAvatar name={name} path={path} emoji={emoji} color={color} size={52} />
              </div>
              <div className="flex-1 flex flex-col gap-[6px]">
                <label className="text-[12px] font-medium text-[var(--modal-text-secondary)]">Name</label>
                <input
                  type="text"
                  autoFocus
                  data-testid="workspace-edit-name"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="Workspace name"
                  disabled={submitting}
                  className={FIELD_INPUT_CLASS}
                />
              </div>
            </div>

            <div className="flex flex-col gap-[6px]">
              <label className="text-[12px] font-medium text-[var(--modal-text-secondary)]">Folder</label>
              {isRename ? (
                <input
                  type="text"
                  readOnly
                  disabled
                  data-testid="workspace-edit-path"
                  value={path}
                  title="The folder can't be changed after a Workspace is created — duplicate it to relocate."
                  className={twMerge(FIELD_INPUT_CLASS, "font-mono text-[13px] cursor-not-allowed")}
                />
              ) : (
                <div className="relative w-full">
                  <input
                    type="text"
                    readOnly
                    data-testid="workspace-edit-path"
                    value={path}
                    placeholder="Choose a folder…"
                    onClick={handleBrowse}
                    disabled={submitting}
                    className={twMerge(FIELD_INPUT_CLASS, "pr-[44px] font-mono text-[13px] cursor-pointer")}
                  />
                  <button
                    type="button"
                    onClick={handleBrowse}
                    disabled={submitting}
                    aria-label="Choose folder"
                    className="absolute right-[6px] top-1/2 -translate-y-1/2 w-[28px] h-[28px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                  >
                    <FolderOpen className="w-[15px] h-[15px]" />
                  </button>
                </div>
              )}
            </div>

            <div className="flex flex-col gap-[6px]">
              <label className="text-[12px] font-medium text-[var(--modal-text-secondary)]">Color</label>
              <div className="flex items-center gap-[8px]" role="radiogroup" aria-label="Workspace color">
                {WORKSPACE_COLOR_PALETTE.map((swatch) => (
                  <button
                    key={swatch}
                    type="button"
                    role="radio"
                    aria-checked={color === swatch}
                    aria-label={`Color ${swatch}`}
                    data-testid={`workspace-color-${swatch}`}
                    onClick={() => setColor(swatch)}
                    disabled={submitting}
                    className={twMerge(
                      "w-[26px] h-[26px] rounded-full transition-all cursor-pointer disabled:cursor-not-allowed",
                      color === swatch
                        ? "ring-2 ring-offset-2 ring-[var(--modal-accent)] ring-offset-[var(--modal-bg)]"
                        : "opacity-80 hover:opacity-100",
                    )}
                    style={{ backgroundColor: swatch }}
                  />
                ))}
              </div>
            </div>
          </div>

          <div className="flex items-center justify-end gap-[10px] px-[22px] py-[14px] border-t border-[var(--modal-border-secondary)]">
            <button
              type="button"
              onClick={requestClose}
              disabled={submitting}
              className="h-[34px] px-[14px] rounded-[8px] text-[13px] text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={submitting}
              className="h-[34px] px-[16px] rounded-[8px] text-[13px] font-semibold text-white bg-[var(--modal-accent)] hover:opacity-90 transition-opacity cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-[6px]"
            >
              {submitting && <Loader2 className="w-[13px] h-[13px] animate-spin" />}
              {isRename ? "Save" : "Create"}
            </button>
          </div>
        </form>
      </motion.div>
    </div>,
    document.body,
  );
}

export default WorkspaceEditModal;
