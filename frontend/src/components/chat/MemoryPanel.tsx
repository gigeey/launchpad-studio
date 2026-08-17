import { useState, useEffect, useCallback } from "react";
import { Loader2, Plus, Trash2 } from "lucide-react";
import type { MemoryEntry } from "../../types/api";
import {
  getMemories,
  addMemory,
  deleteMemory,
  getGlobalMemories,
  addGlobalMemory,
  deleteGlobalMemory,
  getProjectMemories,
  addProjectMemory,
  deleteProjectMemory,
  getThreadMemories,
  addThreadMemory,
  deleteThreadMemory,
} from "../../lib/api";

/** Newest-first — so the most recent memory is at the top of the list
 *  instead of requiring a scroll to the bottom to see it. */
function sortByRecent(memories: MemoryEntry[]): MemoryEntry[] {
  return [...memories].sort(
    (a, b) => new Date(b.created_at).getTime() - new Date(a.created_at).getTime(),
  );
}

/** Format a date string as relative time (e.g., "2m ago", "1h ago"). */
function formatRelativeTime(isoString: string): string {
  const date = new Date(isoString);
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  const diffSec = Math.floor(diffMs / 1000);

  if (diffSec < 60) return "just now";
  const diffMin = Math.floor(diffSec / 60);
  if (diffMin < 60) return `${diffMin}m ago`;
  const diffHr = Math.floor(diffMin / 60);
  if (diffHr < 24) return `${diffHr}h ago`;
  const diffDay = Math.floor(diffHr / 24);
  if (diffDay < 30) return `${diffDay}d ago`;
  return date.toLocaleDateString([], { month: "short", day: "numeric" });
}

type MemoryTab = "thread" | "all" | "global" | "agent" | "project";

const TABS: { id: MemoryTab; label: string }[] = [
  { id: "thread", label: "This thread" },
  { id: "all", label: "All" },
  { id: "global", label: "Global" },
  { id: "agent", label: "Agent" },
  { id: "project", label: "Project" },
];

interface MemoryPanelProps {
  agentId: string;
  /** The chat's currently-active thread id. Falsy when no conversation is
   *  open yet — the "This thread" tab disables itself rather than crashing. */
  threadId?: string;
}

export function MemoryPanel({ agentId, threadId }: MemoryPanelProps) {
  const [threadMemories, setThreadMemories] = useState<MemoryEntry[]>([]);
  const [threadLoading, setThreadLoading] = useState(true);
  const [agentMemories, setAgentMemories] = useState<MemoryEntry[]>([]);
  const [globalMemories, setGlobalMemories] = useState<MemoryEntry[]>([]);
  const [projectMemories, setProjectMemories] = useState<MemoryEntry[]>([]);
  const [activeTab, setActiveTab] = useState<MemoryTab>("all");
  const [loading, setLoading] = useState(true);
  const [newMemory, setNewMemory] = useState("");
  const [adding, setAdding] = useState(false);

  const fetchAllMemories = useCallback(async () => {
    try {
      const [agent, global, project] = await Promise.all([
        getMemories(agentId),
        getGlobalMemories(),
        getProjectMemories(agentId),
      ]);
      setAgentMemories(sortByRecent(agent));
      setGlobalMemories(sortByRecent(global));
      setProjectMemories(sortByRecent(project));
    } catch (err) {
      console.error("Failed to fetch memories:", err);
    } finally {
      setLoading(false);
    }
  }, [agentId]);

  useEffect(() => {
    setLoading(true);
    fetchAllMemories();
  }, [fetchAllMemories]);

  const fetchThreadMemories = useCallback(async () => {
    if (!threadId) {
      setThreadMemories([]);
      setThreadLoading(false);
      return;
    }
    setThreadLoading(true);
    try {
      const entries = await getThreadMemories(threadId);
      setThreadMemories(sortByRecent(entries));
    } catch (err) {
      console.error("Failed to fetch thread memories:", err);
    } finally {
      setThreadLoading(false);
    }
  }, [threadId]);

  useEffect(() => {
    fetchThreadMemories();
  }, [fetchThreadMemories]);

  // New entries written from the input box land in the scope of the active
  // tab. "All" has no single scope, so it defaults to agent — the most common
  // place a user jots a per-assistant note.
  const addTargetScope: Exclude<MemoryTab, "all"> =
    activeTab === "all" ? "agent" : activeTab;

  const handleAdd = async () => {
    const content = newMemory.trim();
    if (!content || adding) return;
    if (addTargetScope === "thread" && !threadId) return;
    setAdding(true);
    try {
      if (addTargetScope === "thread") {
        await addThreadMemory(threadId as string, content);
        setNewMemory("");
        await fetchThreadMemories();
        return;
      }
      if (addTargetScope === "global") {
        await addGlobalMemory(content);
      } else if (addTargetScope === "project") {
        await addProjectMemory(agentId, content);
      } else {
        await addMemory(agentId, content);
      }
      setNewMemory("");
      await fetchAllMemories();
    } catch (err) {
      console.error("Failed to add memory:", err);
    } finally {
      setAdding(false);
    }
  };

  const handleDeleteThread = async (memoryId: string) => {
    if (!threadId) return;
    try {
      await deleteThreadMemory(threadId, memoryId);
      await fetchThreadMemories();
    } catch (err) {
      console.error("Failed to delete thread memory:", err);
    }
  };

  const handleDeleteAgent = async (memoryId: string) => {
    try {
      await deleteMemory(agentId, memoryId);
      await fetchAllMemories();
    } catch (err) {
      console.error("Failed to delete memory:", err);
    }
  };

  const handleDeleteGlobal = async (memoryId: string) => {
    try {
      await deleteGlobalMemory(memoryId);
      await fetchAllMemories();
    } catch (err) {
      console.error("Failed to delete global memory:", err);
    }
  };

  const handleDeleteProject = async (memoryId: string) => {
    try {
      await deleteProjectMemory(agentId, memoryId);
      await fetchAllMemories();
    } catch (err) {
      console.error("Failed to delete project memory:", err);
    }
  };

  const counts: Record<MemoryTab, number> = {
    thread: threadMemories.length,
    all: agentMemories.length + globalMemories.length + projectMemories.length,
    global: globalMemories.length,
    agent: agentMemories.length,
    project: projectMemories.length,
  };

  const showAgent = activeTab === "all" || activeTab === "agent";
  const showProject = activeTab === "all" || activeTab === "project";
  const showGlobal = activeTab === "all" || activeTab === "global";

  const visibleCount =
    (showAgent ? agentMemories.length : 0) +
    (showProject ? projectMemories.length : 0) +
    (showGlobal ? globalMemories.length : 0);

  const placeholder =
    addTargetScope === "thread"
      ? "Pin a note to this conversation..."
      : addTargetScope === "global"
        ? "Store a new global memory..."
        : addTargetScope === "project"
          ? "Store a new project memory..."
          : "Store a new agent memory...";

  return (
    <div className="flex flex-col h-full overflow-hidden">
      {/* Scope tabs */}
      <div className="flex items-center gap-[4px] px-[12px] pt-[12px] pb-[8px] border-b border-[var(--border-secondary)] shrink-0">
        {TABS.map((tab) => {
          const active = activeTab === tab.id;
          const disabled = tab.id === "thread" && !threadId;
          return (
            <button
              key={tab.id}
              data-testid={`memory-tab-${tab.id}`}
              disabled={disabled}
              title={disabled ? "No active conversation" : undefined}
              onClick={() => {
                if (disabled) return;
                setActiveTab(tab.id);
              }}
              className={`flex items-center gap-[5px] px-[10px] py-[5px] rounded-full text-[12px] font-medium transition-colors ${disabled
                ? "text-[var(--text-tertiary)] opacity-40 cursor-not-allowed"
                : "cursor-pointer"
                } ${active
                  ? "bg-[var(--accent)] text-white"
                  : disabled
                    ? ""
                    : "text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
                }`}
            >
              {tab.label}
              <div
                className={`text-[10px] font-bold  h-5 w-5 items-center flex justify-center rounded-full ${active
                  ? "bg-white/25 text-white"
                  : "bg-[var(--bg-tertiary)] text-[var(--text-tertiary)]"
                  }`}
              >
                {counts[tab.id]}
              </div>
            </button>
          );
        })}
      </div>

      {/* Memory list */}
      <div className="flex-1 overflow-y-auto px-[16px] py-[16px] custom-scrollbar">
        {activeTab === "thread" ? (
          <div className="flex flex-col gap-[16px]">
            <p className="text-[11px] text-[var(--text-tertiary)] leading-relaxed">
              Notes pinned to this conversation — applied immediately, cleared when the thread is deleted.
            </p>
            {!threadId ? (
              <div className="py-[32px] text-center text-[13px] text-[var(--text-secondary)] leading-relaxed">
                No active conversation to pin notes to.
              </div>
            ) : threadLoading ? (
              <div className="flex items-center justify-center py-[32px]">
                <Loader2 className="w-[20px] h-[20px] text-[var(--text-secondary)] animate-spin" />
              </div>
            ) : threadMemories.length === 0 ? (
              <div className="py-[32px] text-center text-[13px] text-[var(--text-secondary)] leading-relaxed">
                No memories pinned to this conversation yet. Add one below.
              </div>
            ) : (
              <MemorySection memories={threadMemories} onDelete={handleDeleteThread} />
            )}
          </div>
        ) : loading ? (
          <div className="flex items-center justify-center py-[48px]">
            <Loader2 className="w-[20px] h-[20px] text-[var(--text-secondary)] animate-spin" />
          </div>
        ) : visibleCount === 0 ? (
          <div className="py-[48px] text-center text-[13px] text-[var(--text-secondary)] leading-relaxed flex flex-col items-center gap-3">
            <div className="w-10 h-10 rounded-full bg-[var(--bg-tertiary)] flex items-center justify-center border border-[var(--border-secondary)] text-[18px]">
              🧠
            </div>
            <span>
              No memories in this view. Memories are saved<br />
              automatically as you chat, or add one below.
            </span>
          </div>
        ) : (
          <div className="flex flex-col gap-[20px]">
            {showAgent && agentMemories.length > 0 && (
              <MemorySection
                title={activeTab === "all" ? "Agent" : undefined}
                memories={agentMemories}
                onDelete={handleDeleteAgent}
              />
            )}
            {showProject && projectMemories.length > 0 && (
              <MemorySection
                title={activeTab === "all" ? "Project" : undefined}
                memories={projectMemories}
                onDelete={handleDeleteProject}
              />
            )}
            {showGlobal && globalMemories.length > 0 && (
              <MemorySection
                title={activeTab === "all" ? "Global" : undefined}
                memories={globalMemories}
                onDelete={handleDeleteGlobal}
              />
            )}
          </div>
        )}
      </div>

      {/* Add memory input - Fixed at bottom */}
      <div className="p-[8px] bg-[var(--bg-tertiary)] border-t border-[var(--border-secondary)]">
        <div className="relative group">
          <textarea
            value={newMemory}
            onChange={(e) => setNewMemory(e.target.value)}
            placeholder={placeholder}
            rows={1}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                handleAdd();
              }
            }}
            className="w-full pl-[12px] pr-[40px] py-[10px] rounded-[10px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] text-[13px] text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] outline-none focus:border-[var(--accent)] transition-all resize-none leading-relaxed"
          />
          <button
            onClick={handleAdd}
            disabled={!newMemory.trim() || adding}
            className="absolute right-[8px] top-[7px] w-[28px] h-[28px] rounded-[10px] bg-[var(--text-primary)] text-[var(--bg-primary)] flex items-center justify-center hover:opacity-80 disabled:opacity-0 disabled:scale-95 transition-all cursor-pointer shadow-sm"
          >
            {adding ? (
              <Loader2 className="w-[14px] h-[14px] animate-spin" />
            ) : (
              <Plus className="w-[14px] h-[14px]" />
            )}
          </button>
        </div>
        <p className="mt-0 text-center text-[11px] text-[var(--text-tertiary)]">
          Press Enter to save to {addTargetScope} memory
        </p>
      </div>
    </div>
  );
}

function MemorySection({
  title,
  memories,
  onDelete,
}: {
  title?: string;
  memories: MemoryEntry[];
  onDelete: (memoryId: string) => void;
}) {
  return (
    <div>
      {title && (
        <p className="text-[11px] font-bold text-[var(--text-tertiary)] uppercase tracking-wider mb-[12px]">
          {title} Memories
        </p>
      )}
      <div className="flex flex-col gap-[8px]">
        {memories.map((mem) => (
          <MemoryItem
            key={mem.id}
            memory={mem}
            onDelete={() => onDelete(mem.id)}
          />
        ))}
      </div>
    </div>
  );
}

function MemoryItem({
  memory,
  onDelete,
}: {
  memory: MemoryEntry;
  onDelete: () => void;
}) {
  return (
    <div className="group flex items-start gap-[8px] p-[12px] rounded-[8px] border-[1px] border-[var(--border-primary)] bg-[var(--bg-tertiary)] hover:border-[var(--border-primary)] transition-all">
      <div className="flex-1 min-w-0">
        <p className="text-[14px] text-[var(--text-primary)] leading-relaxed whitespace-pre-wrap break-words">
          {memory.content}
        </p>
        <div className="flex items-center gap-[2px] mt-[4px]">
          <span className="text-[11px] text-[var(--text-tertiary)]">
            {formatRelativeTime(memory.created_at)}
          </span>
          {memory.source && (
            <span
              className={`text-[10px] font-bold px-[6px] py-[1px] rounded-[4px] ${memory.source === "Agent"
                ? "text-[var(--accent)]"
                : memory.source === "Manual"
                  ? "text-[var(--text-secondary)]"
                  : "text-[#F59E0B]"
                }`}
            >
              by {memory.source}
            </span>
          )}
        </div>
      </div>
      <button
        onClick={onDelete}
        className="flex-shrink-0 w-[24px] h-[24px] rounded-[6px] flex items-center justify-center text-[var(--text-tertiary)] opacity-0 group-hover:opacity-100 hover:text-[var(--error)] transition-all cursor-pointer"
        aria-label="Delete memory"
      >
        <Trash2 className="w-[18px] h-[18px]" />
      </button>
    </div>
  );
}
