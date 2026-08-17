import { useMemo, useState, useCallback } from "react";
import { Plus, Pencil, Trash2, Check, X, GitBranch, MessageSquarePlus, Archive, ArchiveRestore, ChevronRight } from "lucide-react";
import { useChatStore } from "../../stores/chatStore";
import { useDraftStore } from "../../stores/draftStore";
import { threadDraftKey } from "../../lib/threadNavigation";
import { resolveThreadActivity, ThreadStreamingBadge, ThreadUnreadDot } from "../shared/ThreadActivityBadge";
import type { Thread } from "../../types/api";

const EMPTY_THREADS: Thread[] = [];

interface ThreadsPanelProps {
  agentId: string;
  /** Selecting a thread is orchestrated by the parent (ChatView): it makes the
   *  thread sticky, reloads its transcript, and switches back to the Chat tab. */
  onSelectThread: (threadId: string) => void;
}

/** The default thread is rendered with a localized "Main" label and
 *  cannot be renamed or deleted. */
function isDefault(thread: Thread): boolean {
  return thread.kind === "default";
}

/** Human label for a thread row. Default → "Main"; otherwise the operator
 *  title, falling back to the system-derived `auto_title` (set from the
 *  thread's first message — see `derive_auto_title` server-side), then a
 *  kind-derived placeholder. Rows here have room for the full ~48-char
 *  value, unlike the tab strip's short pills. */
function threadLabel(thread: Thread): string {
  if (isDefault(thread)) return "Main";
  if (thread.title && thread.title.trim().length > 0) return thread.title;
  if (thread.auto_title && thread.auto_title.trim().length > 0) return thread.auto_title;
  return thread.kind === "branch" ? "Branch" : "New thread";
}

export function ThreadsPanel({ agentId, onSelectThread }: ThreadsPanelProps) {
  const threads = useChatStore((s) => s.threadsByAgent.get(agentId) ?? EMPTY_THREADS);
  const selectedThreadId = useChatStore((s) => s.selectedThreadIdByAgent.get(agentId));
  // Same shared streaming/unread/delegate-running source ThreadTabStrip and
  // Home's sidebar read (see `resolveThreadActivity` in
  // `ThreadActivityBadge.tsx`) — reused here, not reinvented, so a thread
  // reads as "active" identically whether you're looking at its tab, its
  // Home row, or this full list.
  const inFlightByAgent = useChatStore((s) => s.inFlightByAgent);
  const unreadThreadIds = useChatStore((s) => s.unreadThreadIds);
  const runningDelegatesByThread = useChatStore((s) => s.runningDelegatesByThread);
  const createFreshThread = useChatStore((s) => s.createFreshThread);
  const renameThread = useChatStore((s) => s.renameThread);
  const deleteThread = useChatStore((s) => s.deleteThread);
  const archiveThread = useChatStore((s) => s.archiveThread);
  const unarchiveThread = useChatStore((s) => s.unarchiveThread);
  const clearDraft = useDraftStore((s) => s.clearDraft);

  const [editingId, setEditingId] = useState<string | null>(null);
  const [editValue, setEditValue] = useState("");
  const [confirmingDeleteId, setConfirmingDeleteId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [archivingId, setArchivingId] = useState<string | null>(null);
  const [unarchivingId, setUnarchivingId] = useState<string | null>(null);
  // Collapsed by default — this is a recovery path, not somewhere most
  // sessions need to look, so it shouldn't visually compete with the active
  // thread list above it.
  const [archivedOpen, setArchivedOpen] = useState(false);

  // Archived threads are hidden from the main list (and every other surface —
  // tab strip, overflow panel, Home) until unarchived here. Sorted most-
  // recently-archived-first via `updated_at` (bumped whenever `archived_at`
  // is set) so the top of the list is whatever you just closed.
  const visibleThreads = useMemo(() => threads.filter((t) => !t.archived_at), [threads]);
  const archivedThreads = useMemo(
    () => threads.filter((t) => t.archived_at).sort((a, b) => b.updated_at.localeCompare(a.updated_at)),
    [threads],
  );

  const defaultThreadId =
    threads.find(isDefault)?.id ?? `default-${agentId}`;

  const handleCreate = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      const thread = await createFreshThread(agentId);
      onSelectThread(thread.id);
    } finally {
      setBusy(false);
    }
  }, [agentId, busy, createFreshThread, onSelectThread]);

  const startRename = useCallback((thread: Thread) => {
    setConfirmingDeleteId(null);
    setEditingId(thread.id);
    setEditValue(thread.title ?? "");
  }, []);

  const commitRename = useCallback(
    async (thread: Thread) => {
      const next = editValue.trim();
      setEditingId(null);
      await renameThread(thread.id, next.length > 0 ? next : null);
    },
    [editValue, renameThread],
  );

  const handleDelete = useCallback(
    async (thread: Thread) => {
      const wasSelected = selectedThreadId === thread.id;
      setConfirmingDeleteId(null);
      await deleteThread(thread.id);
      // Drop the thread's namespaced draft too — non-default threads are
      // never deletable (see `canEdit` above), so this always resolves to
      // the composite key, never the bare-agentId default-thread draft.
      clearDraft(threadDraftKey(agentId, thread.id, defaultThreadId));
      // The store reverts selection to the default on delete; reload its
      // transcript so the Chat tab doesn't keep showing the gone thread.
      if (wasSelected) onSelectThread(defaultThreadId);
    },
    [deleteThread, clearDraft, agentId, selectedThreadId, defaultThreadId, onSelectThread],
  );

  // Archive a thread — unlike delete, this is non-destructive (the thread and
  // its history are untouched), so there's no confirmation step and no draft
  // cleanup. If the archived thread was active, fall back to the default
  // thread so the Chat tab doesn't keep pointing at a thread that just
  // disappeared from every list.
  const handleArchive = useCallback(
    async (thread: Thread) => {
      const wasSelected = selectedThreadId === thread.id;
      setArchivingId(thread.id);
      try {
        await archiveThread(thread.id);
        if (wasSelected) onSelectThread(defaultThreadId);
      } finally {
        setArchivingId(null);
      }
    },
    [archiveThread, selectedThreadId, defaultThreadId, onSelectThread],
  );

  const handleUnarchive = useCallback(
    async (thread: Thread) => {
      setUnarchivingId(thread.id);
      try {
        await unarchiveThread(thread.id);
      } finally {
        setUnarchivingId(null);
      }
    },
    [unarchiveThread],
  );

  return (
    <div className="flex flex-col min-h-0 flex-1">
      {/* Action bar */}
      <div className="flex items-center justify-between px-[16px] py-[10px] border-b border-[var(--border-secondary)] shrink-0">
        <span className="text-[13px] font-bold text-[var(--text-primary)]">Threads</span>
        <button
          type="button"
          onClick={handleCreate}
          disabled={busy}
          data-testid="thread-new-btn"
          className="inline-flex items-center gap-[6px] px-[10px] py-[5px] rounded-[8px] text-[12px] font-medium bg-[var(--accent)] text-white hover:opacity-90 disabled:opacity-50 transition-opacity cursor-pointer"
        >
          <Plus className="w-[13px] h-[13px]" />
          New
        </button>
      </div>

      {/* Thread list */}
      <div className="flex-1 min-h-0 overflow-y-auto py-[6px]">
        {visibleThreads.length === 0 ? (
          <div className="flex flex-col items-center justify-center gap-[8px] py-[40px] text-[var(--text-secondary)] text-[13px]">
            <MessageSquarePlus className="w-[28px] h-[28px] opacity-50" />
            No threads yet
          </div>
        ) : (
          visibleThreads.map((thread) => {
            const selected = thread.id === selectedThreadId;
            const editing = editingId === thread.id;
            const confirming = confirmingDeleteId === thread.id;
            const canEdit = !isDefault(thread);
            const activity = resolveThreadActivity(
              agentId,
              thread,
              inFlightByAgent,
              unreadThreadIds,
              runningDelegatesByThread,
            );
            return (
              <div
                key={thread.id}
                data-testid={`thread-row-${thread.id}`}
                className={`group/thread mx-[8px] mb-[2px] rounded-[8px] flex items-center gap-[8px] px-[10px] py-[8px] transition-colors ${
                  selected ? "bg-[var(--bg-hover)]" : "hover:bg-[var(--bg-hover)]"
                }`}
              >
                {/* Selectable label area */}
                {editing ? (
                  <input
                    autoFocus
                    value={editValue}
                    onChange={(e) => setEditValue(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") void commitRename(thread);
                      else if (e.key === "Escape") setEditingId(null);
                    }}
                    data-testid={`thread-rename-input-${thread.id}`}
                    className="flex-1 min-w-0 bg-[var(--bg-primary)] border border-[var(--border-primary)] rounded-[6px] px-[8px] py-[4px] text-[13px] text-[var(--text-primary)] outline-none focus:border-[var(--accent)]"
                    placeholder="Thread name"
                  />
                ) : (
                  <button
                    type="button"
                    onClick={() => onSelectThread(thread.id)}
                    data-testid={`thread-select-${thread.id}`}
                    className="flex-1 min-w-0 flex items-center gap-[8px] text-left cursor-pointer"
                  >
                    {thread.kind === "branch" && (
                      <GitBranch className="w-[13px] h-[13px] text-[var(--text-tertiary)] shrink-0" />
                    )}
                    <span
                      className={`truncate text-[13px] ${
                        selected ? "font-bold text-[var(--text-primary)]" : "text-[var(--text-primary)]"
                      }`}
                    >
                      {threadLabel(thread)}
                    </span>
                    {/* Subtle activity indicator — a breathing dot while a
                        delegate is running (or any other live run) on this
                        thread, a static dot for an unread finished reply.
                        `shrink-0` and placed after the (already-truncating)
                        label so it never steals space from the name or
                        shifts the row's layout. */}
                    {activity === "streaming" ? (
                      <span className="shrink-0">
                        <ThreadStreamingBadge id={thread.id} />
                      </span>
                    ) : activity === "unread" ? (
                      <span className="shrink-0">
                        <ThreadUnreadDot id={thread.id} />
                      </span>
                    ) : null}
                    {isDefault(thread) && (
                      <span className="shrink-0 text-[10px] uppercase tracking-wide text-[var(--text-tertiary)] border border-[var(--border-secondary)] rounded-[4px] px-[4px] py-[1px]">
                        Default
                      </span>
                    )}
                  </button>
                )}

                {/* Controls */}
                {editing ? (
                  <div className="flex items-center gap-[2px] shrink-0">
                    <button
                      type="button"
                      onClick={() => void commitRename(thread)}
                      data-testid={`thread-rename-confirm-${thread.id}`}
                      className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] hover:text-[var(--accent)] cursor-pointer"
                      aria-label="Confirm rename"
                    >
                      <Check className="w-[14px] h-[14px]" />
                    </button>
                    <button
                      type="button"
                      onClick={() => setEditingId(null)}
                      className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] hover:text-[var(--text-primary)] cursor-pointer"
                      aria-label="Cancel rename"
                    >
                      <X className="w-[14px] h-[14px]" />
                    </button>
                  </div>
                ) : confirming ? (
                  <div className="flex items-center gap-[4px] shrink-0">
                    <button
                      type="button"
                      onClick={() => void handleDelete(thread)}
                      data-testid={`thread-delete-confirm-${thread.id}`}
                      className="px-[8px] py-[3px] rounded-[6px] text-[11px] font-bold bg-red-500 text-white hover:bg-red-600 cursor-pointer"
                    >
                      Delete
                    </button>
                    <button
                      type="button"
                      onClick={() => setConfirmingDeleteId(null)}
                      className="px-[8px] py-[3px] rounded-[6px] text-[11px] text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] cursor-pointer"
                    >
                      Cancel
                    </button>
                  </div>
                ) : canEdit ? (
                  <div className="flex items-center gap-[2px] shrink-0 opacity-0 group-hover/thread:opacity-100 transition-opacity">
                    <button
                      type="button"
                      onClick={() => startRename(thread)}
                      data-testid={`thread-rename-${thread.id}`}
                      className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] hover:text-[var(--text-primary)] cursor-pointer"
                      aria-label="Rename thread"
                    >
                      <Pencil className="w-[13px] h-[13px]" />
                    </button>
                    <button
                      type="button"
                      onClick={() => void handleArchive(thread)}
                      disabled={archivingId === thread.id}
                      data-testid={`thread-archive-${thread.id}`}
                      className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] hover:text-[var(--text-primary)] disabled:opacity-50 cursor-pointer"
                      aria-label="Archive thread"
                    >
                      <Archive className="w-[13px] h-[13px]" />
                    </button>
                    <button
                      type="button"
                      onClick={() => { setEditingId(null); setConfirmingDeleteId(thread.id); }}
                      data-testid={`thread-delete-${thread.id}`}
                      className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] hover:text-red-500 cursor-pointer"
                      aria-label="Delete thread"
                    >
                      <Trash2 className="w-[13px] h-[13px]" />
                    </button>
                  </div>
                ) : null}
              </div>
            );
          })
        )}

        {/* Archived threads — collapsed by default (see `archivedOpen`
            above). Rows here are deliberately not clickable to switch the
            active thread: recovering one is a two-step "Unarchive, then pick
            it from the list above" flow, so an archived thread never ends up
            active without also being visibly back in the main list (and,
            via the Chat tab's tab strip, its pill). Delete is still offered
            here too — archiving isn't a dead end if you actually want it
            gone. */}
        {archivedThreads.length > 0 && (
          <div className="mt-[6px] pt-[6px] border-t border-[var(--border-secondary)]">
            <button
              type="button"
              onClick={() => setArchivedOpen((open) => !open)}
              data-testid="thread-archived-toggle"
              aria-expanded={archivedOpen}
              className="w-full flex items-center gap-[6px] mx-[8px] px-[8px] py-[6px] rounded-[8px] text-[12px] font-medium text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] cursor-pointer transition-colors"
              style={{ width: "calc(100% - 16px)" }}
            >
              <ChevronRight className={`w-[13px] h-[13px] transition-transform ${archivedOpen ? "rotate-90" : ""}`} />
              Archived ({archivedThreads.length})
            </button>

            {archivedOpen &&
              archivedThreads.map((thread) => (
                <div
                  key={thread.id}
                  data-testid={`thread-archived-row-${thread.id}`}
                  className="group/thread mx-[8px] mb-[2px] rounded-[8px] flex items-center gap-[8px] px-[10px] py-[8px]"
                >
                  <span className="flex-1 min-w-0 flex items-center gap-[8px]">
                    {thread.kind === "branch" && (
                      <GitBranch className="w-[13px] h-[13px] text-[var(--text-tertiary)] shrink-0" />
                    )}
                    <span className="truncate text-[13px] text-[var(--text-secondary)]">{threadLabel(thread)}</span>
                  </span>
                  {confirmingDeleteId === thread.id ? (
                    <div className="flex items-center gap-[4px] shrink-0">
                      <button
                        type="button"
                        onClick={() => void handleDelete(thread)}
                        data-testid={`thread-delete-confirm-${thread.id}`}
                        className="px-[8px] py-[3px] rounded-[6px] text-[11px] font-bold bg-red-500 text-white hover:bg-red-600 cursor-pointer"
                      >
                        Delete
                      </button>
                      <button
                        type="button"
                        onClick={() => setConfirmingDeleteId(null)}
                        className="px-[8px] py-[3px] rounded-[6px] text-[11px] text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] cursor-pointer"
                      >
                        Cancel
                      </button>
                    </div>
                  ) : (
                    <div className="flex items-center gap-[2px] shrink-0">
                      <button
                        type="button"
                        onClick={() => void handleUnarchive(thread)}
                        disabled={unarchivingId === thread.id}
                        data-testid={`thread-unarchive-${thread.id}`}
                        className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] hover:text-[var(--accent)] disabled:opacity-50 cursor-pointer"
                        aria-label="Unarchive thread"
                      >
                        <ArchiveRestore className="w-[13px] h-[13px]" />
                      </button>
                      <button
                        type="button"
                        onClick={() => setConfirmingDeleteId(thread.id)}
                        data-testid={`thread-delete-${thread.id}`}
                        className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-tertiary)] hover:text-red-500 cursor-pointer"
                        aria-label="Delete thread"
                      >
                        <Trash2 className="w-[13px] h-[13px]" />
                      </button>
                    </div>
                  )}
                </div>
              ))}
          </div>
        )}
      </div>
    </div>
  );
}
