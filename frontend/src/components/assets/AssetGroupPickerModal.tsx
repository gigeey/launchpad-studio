import { useEffect, useMemo, useState } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, motion } from "framer-motion";
import { Check, FolderPlus, Search, X } from "lucide-react";
import { useArtifactStore } from "../../stores/artifactStore";
import type { PinnedArtifact } from "../../types/api";

// ---------------------------------------------------------------------------
// Opened from AssetsSidebar's per-row 3-dot menu. Lets the user file a pinned
// artifact under one of its existing groups, clear it back to ungrouped, or
// mint a brand new group on the spot — the "create new group" affordance
// sits directly under the search bar, always reachable rather than only
// appearing once a search misses.
// ---------------------------------------------------------------------------

interface AssetGroupPickerModalProps {
  artifact: PinnedArtifact | null;
  onClose: () => void;
}

export function AssetGroupPickerModal({ artifact, onClose }: AssetGroupPickerModalProps) {
  const open = artifact !== null;
  const groups = useArtifactStore((s) => s.groups);
  const groupsStatus = useArtifactStore((s) => s.groupsStatus);
  const loadGroups = useArtifactStore((s) => s.loadGroups);
  const createGroup = useArtifactStore((s) => s.createGroup);
  const setArtifactGroup = useArtifactStore((s) => s.setArtifactGroup);

  const [query, setQuery] = useState("");
  const [creating, setCreating] = useState(false);
  const [newGroupName, setNewGroupName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) loadGroups();
  }, [open, loadGroups]);

  useEffect(() => {
    if (!open) {
      setQuery("");
      setCreating(false);
      setNewGroupName("");
      setBusy(false);
      setError(null);
    }
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [open, busy, onClose]);

  const filteredGroups = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return groups;
    return groups.filter((g) => g.name.toLowerCase().includes(q));
  }, [groups, query]);

  const assign = async (groupId: string | null) => {
    if (!artifact || busy) return;
    setBusy(true);
    setError(null);
    try {
      await setArtifactGroup(artifact.agent_id, artifact.id, groupId);
      onClose();
    } catch {
      setError("Couldn't update the group. Try again.");
      setBusy(false);
    }
  };

  const startCreating = () => {
    setCreating(true);
    setNewGroupName(query.trim());
  };

  const confirmCreate = async () => {
    const name = newGroupName.trim();
    if (!name || !artifact || busy) return;
    setBusy(true);
    setError(null);
    try {
      const group = await createGroup(name);
      await setArtifactGroup(artifact.agent_id, artifact.id, group.id);
      onClose();
    } catch {
      setError("Couldn't create the group. Try again.");
      setBusy(false);
    }
  };

  if (!open) return null;

  return createPortal(
    <AnimatePresence>
      <div
        className="fixed inset-0 z-[300] flex items-center justify-center"
        role="dialog"
        aria-modal="true"
        aria-labelledby="asset-group-picker-title"
      >
        <motion.div
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.15 }}
          className="absolute inset-0 bg-black/40"
          onClick={() => {
            if (!busy) onClose();
          }}
        />
        <motion.div
          initial={{ opacity: 0, scale: 0.96 }}
          animate={{ opacity: 1, scale: 1 }}
          exit={{ opacity: 0, scale: 0.96 }}
          transition={{ duration: 0.15, ease: "easeOut" }}
          className="relative w-full max-w-[360px] rounded-[12px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)] flex flex-col"
          style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
        >
          <div className="flex items-center justify-between px-[16px] pt-[14px] pb-[8px]">
            <h2
              id="asset-group-picker-title"
              className="text-[14px] font-semibold text-[var(--modal-text-primary)] truncate"
              title={artifact?.title}
            >
              Add “{artifact?.title}” to a group
            </h2>
            <button
              type="button"
              onClick={() => {
                if (!busy) onClose();
              }}
              disabled={busy}
              aria-label="Close"
              className="w-[24px] h-[24px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed flex-shrink-0"
            >
              <X className="w-[14px] h-[14px]" />
            </button>
          </div>

          <div className="px-[16px] pb-[10px]">
            <div className="relative">
              <Search className="absolute left-[10px] top-1/2 -translate-y-1/2 w-[13px] h-[13px] text-[var(--modal-text-tertiary)] pointer-events-none" />
              <input
                type="text"
                autoFocus
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="Search groups..."
                disabled={busy}
                className="w-full h-[32px] pl-[30px] pr-[10px] rounded-[8px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg-primary)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors disabled:opacity-50"
              />
            </div>
          </div>

          {/* "Create new group" sits right after the search bar, always
              reachable rather than gated behind a search miss. */}
          <div className="px-[16px] pb-[10px]">
            {!creating ? (
              <button
                type="button"
                onClick={startCreating}
                disabled={busy}
                className="w-full inline-flex items-center gap-[8px] h-[32px] px-[10px] rounded-[8px] text-[13px] font-medium text-[var(--modal-accent)] border border-dashed border-[var(--modal-border-primary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
              >
                <FolderPlus className="w-[14px] h-[14px]" />
                <span>Create new group</span>
              </button>
            ) : (
              <div className="flex items-center gap-[6px]">
                <input
                  type="text"
                  autoFocus
                  value={newGroupName}
                  onChange={(e) => setNewGroupName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") void confirmCreate();
                    if (e.key === "Escape") setCreating(false);
                  }}
                  placeholder="New group name"
                  disabled={busy}
                  className="flex-1 h-[32px] px-[10px] rounded-[8px] border border-[var(--modal-accent)] bg-[var(--modal-bg-primary)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none disabled:opacity-50"
                />
                <button
                  type="button"
                  onClick={confirmCreate}
                  disabled={busy || !newGroupName.trim()}
                  className="h-[32px] px-[10px] rounded-[8px] text-[12px] font-medium text-white bg-[var(--modal-accent)] hover:opacity-90 transition-opacity cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                >
                  Create
                </button>
              </div>
            )}
          </div>

          {error && (
            <div className="px-[16px] pb-[8px] text-[12px] text-[var(--error)]">{error}</div>
          )}

          <div className="border-t border-[var(--modal-border-secondary)] max-h-[260px] overflow-y-auto py-[6px]">
            <GroupRow
              label="No group"
              selected={artifact?.group_id === null}
              onClick={() => assign(null)}
              disabled={busy}
            />
            {groupsStatus === "loading" && groups.length === 0 ? (
              <div className="px-[16px] py-[10px] text-[12px] text-[var(--modal-text-tertiary)]">Loading…</div>
            ) : filteredGroups.length === 0 ? (
              <div className="px-[16px] py-[10px] text-[12px] text-[var(--modal-text-tertiary)]">
                {groups.length === 0 ? "No groups yet." : "No groups match your search."}
              </div>
            ) : (
              filteredGroups.map((g) => (
                <GroupRow
                  key={g.id}
                  label={g.name}
                  selected={artifact?.group_id === g.id}
                  onClick={() => assign(g.id)}
                  disabled={busy}
                />
              ))
            )}
          </div>
        </motion.div>
      </div>
    </AnimatePresence>,
    document.body,
  );
}

function GroupRow({
  label,
  selected,
  onClick,
  disabled,
}: {
  label: string;
  selected?: boolean;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className="w-full flex items-center gap-[8px] px-[16px] py-[8px] text-left text-[13px] text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
    >
      <span className="flex-1 truncate">{label}</span>
      {selected && <Check className="w-[14px] h-[14px] text-[var(--modal-accent)] flex-shrink-0" />}
    </button>
  );
}
