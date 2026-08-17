import { useEffect, useMemo, useState } from "react";
import { Box, ChevronDown, ChevronRight, Code2, Loader2, MoreVertical, Pin, Search, Trash2 } from "lucide-react";
import { useArtifactStore } from "../../stores/artifactStore";
import { useArtifactViewStore } from "../../stores/artifactViewStore";
import { useChatStore } from "../../stores/chatStore";
import type { ArtifactGroup, ArtifactKind, PinnedArtifact } from "../../types/api";
import { AssetGroupPickerModal } from "./AssetGroupPickerModal";
import ConfirmDialog from "../ui/ConfirmDialog";

// ---------------------------------------------------------------------------
// Body of the collapsible "Assets" sub-menu column — the global, cross-agent
// list of pinned artifacts (pin-to-save), shown irrespective of which agent
// produced them. AppShell already renders the "Assets" title above this.
// Selecting a row drives the main pane (`AssetsView`) via
// `artifactViewStore`, the same store-driven seam `ArtifactPortal` already
// exists for — this is that store's first real mount point.
//
// Two organizing features layer on top of the flat pinned list: newest-first
// ordering (so a just-pinned artifact never requires scrolling to reach) and
// user-defined, collapsible groups (rendered above the ungrouped remainder).
// A row's hover-revealed 3-dot button opens `AssetGroupPickerModal` to file
// it under a group or create a new one on the spot.
// ---------------------------------------------------------------------------

function artifactKindIcon(kind: ArtifactKind) {
  return kind === "html" ? Code2 : Box;
}

function artifactKindLabel(kind: ArtifactKind): string {
  if (kind === "html") return "HTML";
  return kind.charAt(0).toUpperCase() + kind.slice(1);
}

/** Newest-pinned-first sort key. Falls back to `created_at` for rows pinned
 *  before `pinned_at` existed (or any other legacy gap), so nothing silently
 *  sorts to the bottom instead of just landing in original-creation order. */
function pinnedSortKey(artifact: PinnedArtifact): string {
  return artifact.pinned_at ?? artifact.created_at;
}

/** Sentinel key for the ungrouped section's own collapsed-state entry in the
 *  same `collapsedGroups` Set real group ids live in — avoids a second piece
 *  of state for what's otherwise an identical collapsible-header pattern. No
 *  real `ArtifactGroup.id` can collide with this (server ids are uuids). */
const UNGROUPED_SECTION_KEY = "__ungrouped__";

export function AssetsSidebar() {
  const pinned = useArtifactStore((s) => s.pinned);
  const pinnedStatus = useArtifactStore((s) => s.pinnedStatus);
  const loadPinned = useArtifactStore((s) => s.loadPinned);
  const groups = useArtifactStore((s) => s.groups);
  const loadGroups = useArtifactStore((s) => s.loadGroups);
  const deleteGroup = useArtifactStore((s) => s.deleteGroup);
  const selectedAgentId = useArtifactViewStore((s) => s.agentId);
  const selectedArtifactId = useArtifactViewStore((s) => s.artifactId);
  const openArtifact = useArtifactViewStore((s) => s.open);
  const agents = useChatStore((s) => s.agents);
  const [search, setSearch] = useState("");
  const [collapsedGroups, setCollapsedGroups] = useState<Set<string>>(new Set());
  const [groupPickerArtifact, setGroupPickerArtifact] = useState<PinnedArtifact | null>(null);
  const [groupToDelete, setGroupToDelete] = useState<ArtifactGroup | null>(null);

  useEffect(() => {
    loadPinned();
    loadGroups();
  }, [loadPinned, loadGroups]);

  const agentLabel = (agentId: string) => {
    const agent = agents.find((a) => a.agent_id === agentId);
    return agent?.name ?? "Unknown agent";
  };

  // Filter by title (same simple "search the primary name" pattern as
  // ProjectsSidebar's project search), then sort newest-pinned-first.
  const filteredPinned = useMemo(() => {
    const q = search.trim().toLowerCase();
    const base = q ? pinned.filter((artifact) => artifact.title.toLowerCase().includes(q)) : pinned;
    return [...base].sort((a, b) => pinnedSortKey(b).localeCompare(pinnedSortKey(a)));
  }, [pinned, search]);

  // Group sections render above the ungrouped remainder. An artifact whose
  // `group_id` points at a since-deleted group (shouldn't happen — deleting a
  // group clears it server-side — but cheap to guard) falls back to ungrouped
  // rather than vanishing.
  const knownGroupIds = useMemo(() => new Set(groups.map((g) => g.id)), [groups]);
  const groupSections = useMemo(
    () =>
      groups.map((group) => ({
        group,
        artifacts: filteredPinned.filter((a) => a.group_id === group.id),
      })),
    [groups, filteredPinned],
  );
  const ungroupedArtifacts = useMemo(
    () => filteredPinned.filter((a) => !a.group_id || !knownGroupIds.has(a.group_id)),
    [filteredPinned, knownGroupIds],
  );

  const toggleGroupCollapsed = (groupId: string) => {
    setCollapsedGroups((prev) => {
      const next = new Set(prev);
      if (next.has(groupId)) next.delete(groupId);
      else next.add(groupId);
      return next;
    });
  };

  const renderArtifactRow = (artifact: PinnedArtifact) => {
    const Icon = artifactKindIcon(artifact.kind);
    const isSelected = selectedAgentId === artifact.agent_id && selectedArtifactId === artifact.id;
    return (
      <div
        key={artifact.id}
        onClick={() => openArtifact({ agentId: artifact.agent_id, artifactId: artifact.id })}
        title={artifact.title}
        className={`group isolate relative mx-[4px] flex items-center gap-[8px] px-[8px] py-[6px] cursor-pointer transition-colors ${
          isSelected ? "text-[var(--sidebar-active-text-primary)]" : "text-[var(--sidebar-text-primary,var(--text-primary))]"
        }`}
      >
        {/* Edge-to-edge hover/active highlight, same pattern as
            ProjectsSidebar/ChatSidebar/HomeSidebar/TeamsSidebar: its own
            layer (not the row's own background) so it bleeds past the
            row's mx-[4px] using the exact same -left-[4px]/-right-[8px]
            offsets as the divider below, instead of stopping at the
            row's own margin-bounded box the way a plain `hover:bg-*` on
            the row would. `isolate` on the row + `-z-10` here keep it
            behind the icon/text. No rounded corners — it bleeds flush
            to the true edges, so rounding it would look inset again.
            Selected uses the dedicated --sidebar-active-bg token (not
            --bg-hover) so a selected row reads distinctly from a merely
            hovered one, matching every other sidebar. */}
        <div
          aria-hidden
          className={`absolute inset-y-0 -left-[4px] -right-[8px] -z-10 transition-colors ${
            isSelected ? "bg-[var(--sidebar-active-bg)]" : "group-hover:bg-[var(--bg-hover)]"
          }`}
        />
        <div className="w-[26px] h-[26px] rounded-[8px] bg-[var(--bg-tertiary)] flex items-center justify-center flex-shrink-0">
          <Icon className="w-[13px] h-[13px] text-[var(--text-secondary)]" />
        </div>
        <div className="flex-1 min-w-0">
          <div className="text-[13px] truncate leading-tight">{artifact.title}</div>
          <div
            className={`text-[11px] truncate leading-tight mt-[1px] ${
              isSelected ? "text-[var(--sidebar-active-text-secondary)]" : "text-[var(--text-tertiary)]"
            }`}
          >
            {artifactKindLabel(artifact.kind)} · {agentLabel(artifact.agent_id)}
          </div>
        </div>
        {/* Hover-revealed 3-dot menu — opens the group picker. Space is
            reserved even when invisible (no opacity-0 collapse) so rows
            don't reflow on hover. `stopPropagation` keeps this from also
            firing the row's own `openArtifact` click. */}
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            setGroupPickerArtifact(artifact);
          }}
          aria-label={`Add ${artifact.title} to a group`}
          title="Add to group"
          className={`flex-shrink-0 w-[22px] h-[22px] rounded-[6px] flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity cursor-pointer hover:bg-[var(--bg-hover)] ${
            isSelected ? "text-[var(--sidebar-active-text-secondary)]" : "text-[var(--text-secondary)]"
          }`}
        >
          <MoreVertical className="w-[14px] h-[14px]" />
        </button>
        {/* Same -left-[4px]/-right-[8px] bleed as the highlight layer
            above (this row's own mx-[4px] + the scroll wrapper's own
            pr-[4px] on the right, now that -mr-[5px] above has already
            cancelled AppShell's shared pr-[5px]). */}
        <div className="absolute bottom-0 -left-[4px] -right-[8px] border-b border-[var(--border-primary)] group-last:hidden" />
      </div>
    );
  };

  return (
    <div className="flex flex-col flex-1 min-h-0">
      {/* Search input — same surface styling as ChatSidebar (app-search-surface
          + --search-bg/--search-border tokens). `ml-[12px]` = the usual 4px
          own-inset + 8px compensating for AppShell.tsx's Projects/Assets
          `-ml-[8px]` on the sub-menu wrapper (see comment there) — keeps this
          bar's visual position unchanged even though its containing box now
          starts flush with the true sidebar edge instead of 8px in. Sits
          outside the scrollable region below so it never scrolls away, and
          stays visible across the loading/empty/list states. */}
      <div className="ml-[12px] mr-[4px] mb-[8px] flex items-center gap-2">
        <div className="app-search-surface cursor-text border-[1px] border-[var(--search-border)] h-[32px] flex-1 flex items-center gap-1 px-[10px] rounded-[8px] bg-[var(--search-bg)] text-[var(--text-secondary)]">
          <Search className="w-[14px] h-[14px] text-[var(--text-secondary)] flex-shrink-0" />
          <input
            type="text"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            placeholder="Find asset..."
            className="flex-1 text-[15px] leading-[1.4667] bg-transparent outline-none text-[var(--sidebar-text-primary,var(--text-primary))] placeholder:text-[var(--text-secondary)]"
          />
        </div>
      </div>

      {pinnedStatus === "loading" && pinned.length === 0 ? (
        <div className="flex-1 flex items-center justify-center py-[24px]">
          <Loader2 className="w-[16px] h-[16px] text-[var(--text-secondary)] animate-spin" />
        </div>
      ) : pinned.length === 0 ? (
        <div className="flex-1 px-[8px] py-[24px] text-center text-[12px] text-[var(--text-secondary)] leading-relaxed flex flex-col items-center gap-2">
          <Pin className="w-[24px] h-[24px] text-[var(--text-tertiary)]" />
          <span>Nothing pinned yet. Pin an artifact from any agent to save it here.</span>
        </div>
      ) : filteredPinned.length === 0 ? (
        <div className="flex-1 px-[8px] py-[10px] text-[13px] text-[var(--text-secondary)]">
          No assets found
        </div>
      ) : (
        <div className="flex-1 min-h-0 overflow-y-auto custom-scrollbar pr-[4px] -mr-[5px]">
          {/* `-mr-[5px]` above cancels AppShell.tsx's sub-menu wrapper's own
              shared `pr-[5px]` (applied unconditionally to every sidebar, not
              just Projects — see the comment there) at the source, exactly
              like ProjectsSidebar's scroll wrapper does. Without it, that 5px
              stacks on top of this wrapper's own pr-[4px] + the row's own
              mr-[4px], leaving a gap the divider/highlight below can't reach
              no matter their own offset. This wrapper's own pr-[4px] is now
              the only *real* right inset left to cancel below. */}
          {groupSections.map(({ group, artifacts }) => (
            <div key={group.id} className="mb-[4px]">
              <GroupHeader
                label={group.name}
                count={artifacts.length}
                collapsed={collapsedGroups.has(group.id)}
                onToggle={() => toggleGroupCollapsed(group.id)}
                onRequestDelete={() => setGroupToDelete(group)}
              />
              {!collapsedGroups.has(group.id) && artifacts.map(renderArtifactRow)}
            </div>
          ))}
          {/* "Ungrouped" is only shown once real groups exist — with zero
              groups every artifact is ungrouped by definition, so the label
              would be redundant noise. Once groups exist, it keeps the two
              buckets visually consistent (same header/collapse affordance)
              instead of groups looking like headed sections and the
              remainder looking like an unlabeled leftover pile. */}
          {groups.length > 0 && (
            <div className="mb-[4px]">
              <GroupHeader
                label="Ungrouped"
                count={ungroupedArtifacts.length}
                collapsed={collapsedGroups.has(UNGROUPED_SECTION_KEY)}
                onToggle={() => toggleGroupCollapsed(UNGROUPED_SECTION_KEY)}
              />
              {!collapsedGroups.has(UNGROUPED_SECTION_KEY) && ungroupedArtifacts.map(renderArtifactRow)}
            </div>
          )}
          {groups.length === 0 && ungroupedArtifacts.map(renderArtifactRow)}
        </div>
      )}

      <AssetGroupPickerModal artifact={groupPickerArtifact} onClose={() => setGroupPickerArtifact(null)} />

      <ConfirmDialog
        open={groupToDelete !== null}
        title="Delete group"
        message={
          groupToDelete ? (
            <>
              Delete <span className="font-semibold text-[var(--modal-text-primary)]">{groupToDelete.name}</span>?
              Its artifacts move back to the ungrouped list — nothing is unpinned.
            </>
          ) : (
            ""
          )
        }
        confirmLabel="Delete"
        destructive
        onConfirm={async () => {
          if (groupToDelete) await deleteGroup(groupToDelete.id);
          setGroupToDelete(null);
        }}
        onCancel={() => setGroupToDelete(null)}
      />
    </div>
  );
}

/** Shared collapsible section header for both real groups and the
 *  "Ungrouped" pseudo-group below. `onRequestDelete` is omitted for
 *  Ungrouped — it isn't a real group, so there's nothing to delete. */
function GroupHeader({
  label,
  count,
  collapsed,
  onToggle,
  onRequestDelete,
}: {
  label: string;
  count: number;
  collapsed: boolean;
  onToggle: () => void;
  onRequestDelete?: () => void;
}) {
  return (
    <div
      className="group/header flex items-center gap-[6px] mx-[4px] px-[8px] py-[5px] rounded-[6px] cursor-pointer select-none hover:bg-[var(--bg-hover)] transition-colors"
      onClick={onToggle}
      role="button"
      aria-expanded={!collapsed}
    >
      {collapsed ? (
        <ChevronRight className="w-[13px] h-[13px] text-[var(--text-secondary)] flex-shrink-0" />
      ) : (
        <ChevronDown className="w-[13px] h-[13px] text-[var(--text-secondary)] flex-shrink-0" />
      )}
      <span className="flex-1 min-w-0 truncate text-[12px] font-semibold uppercase tracking-wide text-[var(--text-secondary)]" title={label}>
        {label}
      </span>
      {/* Fixed-size slot shared by the count and the delete button — same
          footprint whether or not `onRequestDelete` exists, so "Ungrouped"
          (no delete button) and real groups (delete button overlaid on
          hover, not appended beside the count) right-align identically
          instead of groups' counts sitting shifted left of Ungrouped's. */}
      <div className="relative flex-shrink-0 w-[20px] h-[20px] flex items-center justify-center">
        <span
          className={`text-[11px] text-[var(--text-tertiary)] transition-opacity ${
            onRequestDelete ? "group-hover/header:opacity-0" : ""
          }`}
        >
          {count}
        </span>
        {onRequestDelete && (
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onRequestDelete();
            }}
            aria-label={`Delete group ${label}`}
            title="Delete group"
            className="absolute inset-0 rounded-[5px] flex items-center justify-center text-[var(--text-secondary)] hover:text-[var(--error)] hover:bg-[var(--bg-tertiary)] opacity-0 group-hover/header:opacity-100 transition-opacity cursor-pointer"
          >
            <Trash2 className="w-[12px] h-[12px]" />
          </button>
        )}
      </div>
    </div>
  );
}
