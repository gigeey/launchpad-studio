import { useState, useEffect, useCallback, useMemo, useRef } from "react";
import {
    Loader2,
    Check,
    Pencil,
    Pin,
    Trash2,
    Undo2,
    AlertTriangle,
    Brain,
    X,
    Search,
    ChevronDown,
    Wrench,
    ArrowUpCircle,
} from "lucide-react";
import { twMerge } from "tailwind-merge";
import type { AgentSnapshot, MemoryEntry, MemoryScope, ReflectionCandidate } from "../../types/api";
import {
    getAgents,
    getMemories,
    getMemoryReviewQueue,
    getAgentMemorySummaries,
    actOnMemoryReviewCandidate,
    undoMemoryWrite,
    getSkillReviewQueue,
    actOnSkillReview,
    promoteSkillObservation,
    type MemoryReviewAction,
    type ParkedSkillCandidate,
    type SkillObservation,
} from "../../lib/api";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";

const MIN_AGENT_LIST_WIDTH = 160;
const MAX_AGENT_LIST_WIDTH = 480;

/** Format an ISO date as compact relative time (e.g. "2m ago", "1h ago"). */
function formatRelativeTime(isoString: string): string {
    const date = new Date(isoString);
    const diffSec = Math.floor((Date.now() - date.getTime()) / 1000);
    if (diffSec < 60) return "just now";
    const diffMin = Math.floor(diffSec / 60);
    if (diffMin < 60) return `${diffMin}m ago`;
    const diffHr = Math.floor(diffMin / 60);
    if (diffHr < 24) return `${diffHr}h ago`;
    const diffDay = Math.floor(diffHr / 24);
    if (diffDay < 30) return `${diffDay}d ago`;
    return date.toLocaleDateString([], { month: "short", day: "numeric" });
}

/** Per-agent rollup used for the list column's badge + "recent" sort — kept
 *  separate from `queue`/`learned`, which only ever hold the *selected*
 *  agent's full detail. */
interface AgentMemorySummary {
    pending: number;
    /** `created_at` of this agent's most recent *live, self-authored* memory
     *  (mirrors the "Learned on its own" section) — `null` if it has none. */
    lastMemoryAt: string | null;
}

/** ISO 8601 timestamps from the backend compare lexicographically the same
 *  as chronologically, so plain string comparison is enough here. */
function latestCreatedAt(entries: MemoryEntry[]): string | null {
    let latest: string | null = null;
    for (const e of entries) {
        if (!latest || e.created_at > latest) latest = e.created_at;
    }
    return latest;
}

type AgentSortMode = "name" | "pending" | "recent";

const SORT_LABELS: Record<AgentSortMode, string> = {
    name: "Name",
    pending: "Pending review",
    recent: "Recently active",
};

/** Detail-panel tab: split the sections apart so a long list in one doesn't
 *  force scrolling past the others to see it. "skills" covers the skill-side
 *  review queue (parked distilled skills + promotable raw observations). */
type DetailTab = "all" | "review" | "learned" | "skills";

const DETAIL_TABS: { id: DetailTab; label: string }[] = [
    { id: "all", label: "All" },
    { id: "review", label: "Held for review" },
    { id: "learned", label: "Learned" },
    { id: "skills", label: "Skills" },
];

/** Newest-first: ISO 8601 timestamps compare lexicographically the same as
 *  chronologically, so plain string comparison is enough here. */
function byNewestFirst<T extends { created_at: string }>(entries: T[]): T[] {
    return [...entries].sort((a, b) => b.created_at.localeCompare(a.created_at));
}

/** Sort selector styled like Slack's settings dropdowns (e.g. the "Font"
 *  picker): a full-width, select-like box with a rounded border and a
 *  trailing chevron, opening a rounded popover where the active row is a
 *  solid accent highlight and the selected item carries a leading checkmark
 *  in accent text. Restyled with `--modal-*` tokens since this panel also
 *  renders inside the standalone pop-out window, not just the Settings modal. */
function AgentSortMenu({
    value,
    onChange,
}: {
    value: AgentSortMode;
    onChange: (m: AgentSortMode) => void;
}) {
    const [open, setOpen] = useState(false);
    return (
        <div className="relative flex-shrink-0 self-start w-full">
            <button
                type="button"
                title="Sort agents"
                onClick={() => setOpen((o) => !o)}
                className="flex w-full items-center justify-between gap-[6px] h-[36px] px-[10px] rounded-[8px] border border-[var(--modal-border-secondary)] bg-transparent text-[13px] text-[var(--modal-text-primary)] hover:border-[var(--modal-text-secondary)] cursor-pointer transition-colors"
            >
                <span className="truncate">{SORT_LABELS[value]}</span>
                <ChevronDown
                    size={14}
                    className={twMerge(
                        "flex-shrink-0 text-[var(--modal-text-secondary)] transition-transform",
                        open && "rotate-180",
                    )}
                />
            </button>
            {open && (
                <>
                    <div className="fixed inset-0 z-30" onClick={() => setOpen(false)} />
                    <div className="absolute left-0 right-0 top-[calc(100%+4px)] z-40 rounded-[8px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)] py-[4px] shadow-lg overflow-hidden">
                        {(Object.keys(SORT_LABELS) as AgentSortMode[]).map((mode) => (
                            <button
                                key={mode}
                                type="button"
                                onClick={() => {
                                    onChange(mode);
                                    setOpen(false);
                                }}
                                className={twMerge(
                                    "flex w-full items-center gap-[8px] px-[12px] py-[8px] text-left text-[13px] cursor-pointer transition-colors",
                                    "hover:bg-[var(--modal-accent)] hover:text-white",
                                    value === mode
                                        ? "text-[var(--modal-accent)] font-medium"
                                        : "text-[var(--modal-text-primary)]",
                                )}
                            >
                                <Check size={13} className={value === mode ? "opacity-100" : "opacity-0"} />
                                {SORT_LABELS[mode]}
                            </button>
                        ))}
                    </div>
                </>
            )}
        </div>
    );
}

/**
 * Settings → Memories.
 *
 * The transparency-and-control surface for self-improving memory: for a chosen
 * agent it shows the **review queue** (staged candidates the trust gate held
 * back for a human — keep / edit / pin / forget) and everything the agent has
 * **already learned on its own** and made live, each with an undo. The review
 * routes are per-agent, so this panel carries its own agent list (left column)
 * rather than inheriting a selection from a chat context the Settings modal
 * doesn't have; clicking an agent swaps the right-hand detail panel.
 */
export function MemoriesSettings() {
    const [agents, setAgents] = useState<AgentSnapshot[]>([]);
    const [agentId, setAgentId] = useState<string | null>(null);
    const [queue, setQueue] = useState<ReflectionCandidate[]>([]);
    const [learned, setLearned] = useState<MemoryEntry[]>([]);
    // Skill-side review queue: parked distilled skills + promotable raw
    // observations. Kept separate from the memory `queue`/`learned` above so
    // each section's list + counts stay independent.
    const [skillCandidates, setSkillCandidates] = useState<ParkedSkillCandidate[]>([]);
    const [skillObservations, setSkillObservations] = useState<SkillObservation[]>([]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState<string | null>(null);
    const [busyId, setBusyId] = useState<string | null>(null);
    const [editingId, setEditingId] = useState<string | null>(null);
    const [editText, setEditText] = useState("");
    // Skill edit state is separate from the memory `editingId`/`editText` above
    // because a skill edit rewrites two fields (body + description), keyed by
    // the skill's name rather than a candidate id.
    const [editingSkill, setEditingSkill] = useState<string | null>(null);
    const [skillBodyDraft, setSkillBodyDraft] = useState("");
    const [skillDescDraft, setSkillDescDraft] = useState("");
    const [detailTab, setDetailTab] = useState<DetailTab>("all");

    // Search + sort over the agent list column.
    const [search, setSearch] = useState("");
    const [sortMode, setSortMode] = useState<AgentSortMode>("name");
    const [summaries, setSummaries] = useState<Record<string, AgentMemorySummary>>({});

    // Agent list column width — draggable, persisted across sessions/windows.
    const agentListWidth = useUserPreferencesStore((s) => s.memoriesAgentListWidth);
    const setAgentListWidth = useUserPreferencesStore((s) => s.setMemoriesAgentListWidth);
    const agentListRef = useRef<HTMLDivElement>(null);
    const resizingRef = useRef(false);
    const [isResizing, setIsResizing] = useState(false);

    const startResizing = useCallback((e: React.MouseEvent) => {
        e.preventDefault();
        resizingRef.current = true;
        setIsResizing(true);
    }, []);

    useEffect(() => {
        if (!isResizing) return;

        const handleMouseMove = (e: MouseEvent) => {
            if (!resizingRef.current || !agentListRef.current) return;
            const containerLeft = agentListRef.current.getBoundingClientRect().left;
            const newWidth = e.clientX - containerLeft;
            if (newWidth >= MIN_AGENT_LIST_WIDTH && newWidth <= MAX_AGENT_LIST_WIDTH) {
                // Update the DOM directly during the drag — no re-render per
                // mousemove — then commit the final value to the persisted
                // store once on mouseup below.
                agentListRef.current.style.width = `${newWidth}px`;
            }
        };

        const handleMouseUp = () => {
            if (resizingRef.current && agentListRef.current) {
                setAgentListWidth(agentListRef.current.offsetWidth);
            }
            resizingRef.current = false;
            setIsResizing(false);
        };

        window.addEventListener("mousemove", handleMouseMove);
        window.addEventListener("mouseup", handleMouseUp);
        document.body.style.cursor = "col-resize";
        return () => {
            window.removeEventListener("mousemove", handleMouseMove);
            window.removeEventListener("mouseup", handleMouseUp);
            document.body.style.cursor = "default";
        };
    }, [isResizing, setAgentListWidth]);

    const refresh = useCallback(async (id: string) => {
        setError(null);
        const [q, mem, skills] = await Promise.all([
            getMemoryReviewQueue(id),
            getMemories(id),
            getSkillReviewQueue(id),
        ]);
        // Newest first, so the most recent staged/learned items are always
        // what's on screen without scrolling.
        setQueue(byNewestFirst(q));
        setSkillCandidates(byNewestFirst(skills.candidates));
        setSkillObservations(byNewestFirst(skills.observations));
        // "Learned" = entries the agent authored itself and made live. A
        // user's own Manual notes belong to the chat-side memory viewer, not
        // this "what did my agent decide to remember" surface.
        const learnedEntries = byNewestFirst(mem.filter((m) => m.source === "Agent"));
        setLearned(learnedEntries);
        // Keep the list column's badge/sort data in sync with whatever the
        // detail panel just fetched, so switching agents (or acting on a
        // candidate) doesn't leave a stale count behind for this one.
        setSummaries((prev) => ({
            ...prev,
            [id]: { pending: q.length, lastMemoryAt: latestCreatedAt(learnedEntries) },
        }));
    }, []);

    // Initial load: agents, then the first agent's memories. Every agent's
    // summary (pending count + last memory time, for the list column's badge
    // and sort) comes from one bulk request rather than a review-queue +
    // memories fan-out per agent — that fan-out approach hits browsers'
    // ~6-connections-per-origin cap once the agent count reaches dozens, so
    // the badges/sort data would trickle in over several rounds instead of
    // loading at once.
    useEffect(() => {
        let cancelled = false;
        setLoading(true);
        getAgents()
            .then(async (list) => {
                if (cancelled) return;
                // Hide inline team coordinators — they're not chat agents.
                const visible = list.filter((a) => !a.owning_team_id);
                setAgents(visible);
                const first = visible[0]?.agent_id ?? null;
                setAgentId(first);
                const [, bulk] = await Promise.all([
                    first ? refresh(first) : Promise.resolve(),
                    getAgentMemorySummaries().catch((e) => {
                        // Don't let a summary-fetch failure block the list itself —
                        // it just shows no badges/won't sort into "recent"/"pending".
                        console.error("[MemoriesSettings] bulk summary load failed:", e);
                        return {} as Record<string, { pending: number; last_memory_at: string | null }>;
                    }),
                ]);
                if (cancelled) return;
                setSummaries((prev) => {
                    const next = { ...prev };
                    for (const [id, s] of Object.entries(bulk)) {
                        // The selected agent's `refresh()` call above already wrote a
                        // summary entry for it from its own detail fetch — don't
                        // stomp that with the (identical, just concurrently-fetched)
                        // bulk value.
                        if (id === first) continue;
                        next[id] = { pending: s.pending, lastMemoryAt: s.last_memory_at };
                    }
                    return next;
                });
            })
            .catch((e) => {
                if (cancelled) return;
                console.error("[MemoriesSettings] load failed:", e);
                setError("Failed to load agents.");
            })
            .finally(() => {
                if (!cancelled) setLoading(false);
            });
        return () => {
            cancelled = true;
        };
    }, [refresh]);

    // Filter by search, then order by the selected sort mode.
    const filteredAgents = useMemo(() => {
        const q = search.trim().toLowerCase();
        const matched = q ? agents.filter((a) => a.name.toLowerCase().includes(q)) : agents;
        return [...matched].sort((a, b) => {
            if (sortMode === "pending") {
                const diff =
                    (summaries[b.agent_id]?.pending ?? 0) - (summaries[a.agent_id]?.pending ?? 0);
                if (diff !== 0) return diff;
            } else if (sortMode === "recent") {
                const at = summaries[a.agent_id]?.lastMemoryAt;
                const bt = summaries[b.agent_id]?.lastMemoryAt;
                if (at && bt && at !== bt) return bt.localeCompare(at);
                if (at && !bt) return -1;
                if (!at && bt) return 1;
            }
            return a.name.localeCompare(b.name);
        });
    }, [agents, search, sortMode, summaries]);

    const handleSelectAgent = useCallback(
        async (id: string) => {
            setAgentId(id);
            setEditingId(null);
            setLoading(true);
            try {
                await refresh(id);
            } catch (e) {
                console.error("[MemoriesSettings] refresh failed:", e);
                setError("Failed to load this agent's memories.");
            } finally {
                setLoading(false);
            }
        },
        [refresh],
    );

    const runAction = useCallback(
        async (candidateId: string, action: MemoryReviewAction, content?: string) => {
            if (!agentId) return;
            setBusyId(candidateId);
            setError(null);
            try {
                await actOnMemoryReviewCandidate(agentId, candidateId, action, content);
                setEditingId(null);
                await refresh(agentId);
            } catch (e) {
                console.error(`[MemoriesSettings] ${action} failed:`, e);
                setError(`Failed to ${action} that memory.`);
            } finally {
                setBusyId(null);
            }
        },
        [agentId, refresh],
    );

    const handleUndo = useCallback(
        async (entry: MemoryEntry) => {
            if (!agentId) return;
            setBusyId(entry.id);
            setError(null);
            try {
                await undoMemoryWrite(agentId, entry.id);
                await refresh(agentId);
            } catch (e) {
                console.error("[MemoriesSettings] undo failed:", e);
                setError("Failed to undo that memory.");
            } finally {
                setBusyId(null);
            }
        },
        [agentId, refresh],
    );

    const startEdit = (c: ReflectionCandidate) => {
        setEditingId(c.id);
        setEditText(c.content);
    };

    // --- Skill review actions (mirror runAction/handleUndo above) ---------

    const runSkillAction = useCallback(
        async (
            skillName: string,
            opts: {
                action: "accept" | "edit" | "reject";
                body?: string;
                description?: string;
                keep_parked?: boolean;
            },
        ) => {
            if (!agentId) return;
            setBusyId(skillName);
            setError(null);
            try {
                await actOnSkillReview(agentId, skillName, opts);
                setEditingSkill(null);
                await refresh(agentId);
            } catch (e) {
                console.error(`[MemoriesSettings] skill ${opts.action} failed:`, e);
                setError(`Failed to ${opts.action} that skill.`);
            } finally {
                setBusyId(null);
            }
        },
        [agentId, refresh],
    );

    const handlePromote = useCallback(
        async (observationId: string) => {
            if (!agentId) return;
            setBusyId(observationId);
            setError(null);
            try {
                await promoteSkillObservation(agentId, observationId);
                // Refresh so the promoted observation drops off the observations
                // list and reappears under "Skills held for review".
                await refresh(agentId);
            } catch (e) {
                console.error("[MemoriesSettings] promote failed:", e);
                setError("Failed to promote that observation.");
            } finally {
                setBusyId(null);
            }
        },
        [agentId, refresh],
    );

    const startSkillEdit = (c: ParkedSkillCandidate) => {
        setEditingSkill(c.name);
        setSkillBodyDraft(c.body);
        setSkillDescDraft(c.description);
    };

    return (
        <div className="flex flex-col w-full h-full">
            <div className="flex-shrink-0 px-8 py-6 border-b border-[var(--modal-border-secondary)]">
                <h2 className="text-[18px] font-bold text-[var(--modal-text-primary)] mb-1">
                    Learning
                </h2>
                <p className="text-[14px] text-[var(--modal-text-secondary)] leading-relaxed">
                    Review what your agents have learned on their own — both memories and
                    skills. Approve, edit, pin, or forget anything held for review, promote
                    a raw observation into a skill, and undo anything that's already live.
                    You stay in control of what each agent remembers and can do.
                </p>
            </div>

            <div className="flex flex-1 min-h-0">
                {/* Agent list */}
                <div
                    ref={agentListRef}
                    style={{ width: agentListWidth }}
                    className="relative flex-shrink-0 flex flex-col border-r border-[var(--modal-border-secondary)]"
                >
                    {/* Search + sort — fixed above the scrolling list itself */}
                    <div className="flex-shrink-0 flex flex-col gap-[8px] px-3 pt-4 pb-2">
                        <div className="flex items-center gap-[6px] h-[30px] px-[9px] rounded-[8px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[var(--modal-text-secondary)] cursor-text">
                            <Search className="w-[13px] h-[13px] flex-shrink-0" />
                            <input
                                type="text"
                                value={search}
                                onChange={(e) => setSearch(e.target.value)}
                                placeholder="Search agents..."
                                className="flex-1 min-w-0 text-[13px] bg-transparent outline-none text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-secondary)]"
                            />
                        </div>
                        <div className="flex flex-col gap-[4px]">
                            <span className="px-[2px] text-[11px] font-semibold uppercase tracking-wide text-[var(--modal-text-secondary)]">
                                Sort by
                            </span>
                            <AgentSortMenu value={sortMode} onChange={setSortMode} />
                        </div>
                    </div>

                    <div className="flex-1 min-h-0 overflow-y-auto flex flex-col gap-[2px] px-2 pb-6">
                        {agents.length === 0 ? (
                            <p className="px-3 text-[13px] text-[var(--modal-text-secondary)]">
                                No agents yet.
                            </p>
                        ) : filteredAgents.length === 0 ? (
                            <p className="px-3 text-[13px] text-[var(--modal-text-secondary)]">
                                No agents match "{search.trim()}".
                            </p>
                        ) : (
                            filteredAgents.map((a) => {
                                const isActive = agentId === a.agent_id;
                                const pending = summaries[a.agent_id]?.pending ?? 0;
                                return (
                                    <button
                                        key={a.agent_id}
                                        type="button"
                                        onClick={() => handleSelectAgent(a.agent_id)}
                                        className={twMerge(
                                            "flex items-center gap-[10px] px-3 py-[8px] mx-1 rounded-[10px] text-[14px] text-left transition-colors duration-150 cursor-pointer select-none",
                                            isActive
                                                ? "bg-[var(--modal-accent)] text-white font-medium"
                                                : "text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)]",
                                        )}
                                    >
                                        <span className="text-[16px] flex-shrink-0 w-[20px] text-center">
                                            {a.emoji ?? "🤖"}
                                        </span>
                                        <span className="truncate">{a.name}</span>
                                        {pending > 0 && (
                                            <span
                                                title={`${pending} awaiting review`}
                                                className={twMerge(
                                                    "ml-auto flex-shrink-0 text-[10px] font-bold min-w-[18px] h-[18px] px-[5px] rounded-full flex items-center justify-center",
                                                    isActive
                                                        ? "bg-white/25 text-white"
                                                        : "bg-amber-500/15 text-amber-600 dark:text-amber-400",
                                                )}
                                            >
                                                {pending}
                                            </span>
                                        )}
                                    </button>
                                );
                            })
                        )}
                    </div>

                    {/* Drag handle — mirrors the AppShell sidebar resizer convention */}
                    <div
                        onMouseDown={startResizing}
                        className="absolute right-0 top-0 bottom-0 w-[6px] cursor-col-resize select-none z-10 group"
                    >
                        <div
                            className={twMerge(
                                "absolute inset-y-0 right-0 w-[1px] rounded-full bg-[var(--modal-accent)] transition-opacity",
                                isResizing ? "opacity-100 w-[2px]" : "opacity-0 group-hover:opacity-100",
                            )}
                        />
                    </div>
                </div>

                {/* Selected agent's memories */}
                <div className="flex-1 min-w-0 overflow-y-auto px-8 py-6">
                    {error && (
                        <div className="mb-4 px-3 py-2 bg-red-500/10 border border-red-500/30 rounded-[8px] text-[13px] text-red-600 dark:text-red-400">
                            {error}
                        </div>
                    )}

                    {loading ? (
                        <div className="flex items-center justify-center py-16">
                            <Loader2 className="w-5 h-5 text-[var(--modal-text-secondary)] animate-spin" />
                        </div>
                    ) : !agentId ? (
                        <EmptyState
                            icon={<Brain size={20} />}
                            text="No agents yet. Create an agent and its learnings will show up here."
                        />
                    ) : (
                        <div className="flex flex-col gap-6 max-w-3xl">
                            {/* Detail tabs — split review/learned apart so a long
                                list in one doesn't push the other one out of view. */}
                            <div className="flex items-center gap-[4px] flex-shrink-0">
                                {DETAIL_TABS.map((tab) => {
                                    const active = detailTab === tab.id;
                                    const skillCount =
                                        skillCandidates.length + skillObservations.length;
                                    const count =
                                        tab.id === "all"
                                            ? queue.length + learned.length + skillCount
                                            : tab.id === "review"
                                              ? queue.length
                                              : tab.id === "learned"
                                                ? learned.length
                                                : skillCount;
                                    return (
                                        <button
                                            key={tab.id}
                                            type="button"
                                            onClick={() => setDetailTab(tab.id)}
                                            className={twMerge(
                                                "flex items-center gap-[6px] px-[10px] py-[6px] rounded-[8px] text-[13px] font-medium transition-colors cursor-pointer",
                                                active
                                                    ? "bg-[var(--modal-accent)] text-white"
                                                    : "text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)]",
                                            )}
                                        >
                                            {tab.label}
                                            <span
                                                className={twMerge(
                                                    "text-[10px] font-bold px-[5px] py-[1px] rounded-[6px]",
                                                    active
                                                        ? "bg-white/25 text-white"
                                                        : "bg-[var(--modal-bg-tertiary)] text-[var(--modal-text-secondary)]",
                                                )}
                                            >
                                                {count}
                                            </span>
                                        </button>
                                    );
                                })}
                            </div>

                            {/* Review queue */}
                            {(detailTab === "all" || detailTab === "review") && (
                                <section className="flex flex-col gap-3">
                                    <SectionHeader
                                        title="Held for review"
                                        count={queue.length}
                                        hint="Staged automatically. Nothing here is live until you keep or pin it."
                                    />
                                    {queue.length === 0 ? (
                                        <EmptyState
                                            icon={<Check size={18} />}
                                            text="Nothing waiting for review. New learnings that need a human decision will appear here."
                                        />
                                    ) : (
                                        <div className="flex flex-col gap-3">
                                            {queue.map((c) => (
                                                <ReviewCard
                                                    key={c.id}
                                                    candidate={c}
                                                    busy={busyId === c.id}
                                                    editing={editingId === c.id}
                                                    editText={editText}
                                                    onEditTextChange={setEditText}
                                                    onStartEdit={() => startEdit(c)}
                                                    onCancelEdit={() => setEditingId(null)}
                                                    onKeep={() => runAction(c.id, "keep")}
                                                    onPin={() => runAction(c.id, "pin")}
                                                    onForget={() => runAction(c.id, "forget")}
                                                    onSaveEdit={() =>
                                                        runAction(c.id, "edit", editText.trim())
                                                    }
                                                />
                                            ))}
                                        </div>
                                    )}
                                </section>
                            )}

                            {detailTab === "all" && (
                                <div className="w-full border-t border-[var(--modal-border-secondary)]" />
                            )}

                            {/* Learned + live */}
                            {(detailTab === "all" || detailTab === "learned") && (
                                <section className="flex flex-col gap-3">
                                    <SectionHeader
                                        title="Learned on its own"
                                        count={learned.length}
                                        hint="Live memories this agent decided to keep. Undo any that don't belong."
                                    />
                                    {learned.length === 0 ? (
                                        <EmptyState
                                            icon={<Brain size={18} />}
                                            text="This agent hasn't saved any of its own memories yet."
                                        />
                                    ) : (
                                        <div className="flex flex-col gap-2">
                                            {learned.map((m) => (
                                                <LearnedCard
                                                    key={m.id}
                                                    entry={m}
                                                    busy={busyId === m.id}
                                                    onUndo={() => handleUndo(m)}
                                                />
                                            ))}
                                        </div>
                                    )}
                                </section>
                            )}

                            {detailTab === "all" && (
                                <div className="w-full border-t border-[var(--modal-border-secondary)]" />
                            )}

                            {/* Skills held for review — every parked skill,
                                from both writers: ones the distillation
                                pipeline generalized, and ones an agent wrote
                                on request via SkillRegister. Each candidate
                                carries `origin` ("distilled" | "user_authored")
                                so the card can say which it is.

                                This is the only surface that clears a skill's
                                disable-model-invocation flag, so a parked skill
                                missing from this list cannot be enabled by any
                                other route in the product.

                                NOTE: there is deliberately no "share with other
                                agents" control here. Cross-agent availability is
                                handled by the existing competencies toggle once a
                                skill is live — don't add a duplicate share button. */}
                            {(detailTab === "all" || detailTab === "skills") && (
                                <section className="flex flex-col gap-3">
                                    <SectionHeader
                                        title="Skills held for review"
                                        count={skillCandidates.length}
                                        hint="Skills staged for a human decision — both distilled from repeated work and written by an agent on request. Nothing here can be used by the agent until you keep it."
                                    />
                                    {skillCandidates.length === 0 ? (
                                        <EmptyState
                                            icon={<Wrench size={18} />}
                                            text="No skills waiting for review. Skills that need a human decision will appear here."
                                        />
                                    ) : (
                                        <div className="flex flex-col gap-3">
                                            {skillCandidates.map((c) => (
                                                <SkillReviewCard
                                                    key={c.name}
                                                    candidate={c}
                                                    busy={busyId === c.name}
                                                    editing={editingSkill === c.name}
                                                    bodyDraft={skillBodyDraft}
                                                    descDraft={skillDescDraft}
                                                    onBodyDraftChange={setSkillBodyDraft}
                                                    onDescDraftChange={setSkillDescDraft}
                                                    onStartEdit={() => startSkillEdit(c)}
                                                    onCancelEdit={() => setEditingSkill(null)}
                                                    onKeep={() =>
                                                        runSkillAction(c.name, { action: "accept" })
                                                    }
                                                    onForget={() =>
                                                        runSkillAction(c.name, { action: "reject" })
                                                    }
                                                    onSaveEdit={() =>
                                                        runSkillAction(c.name, {
                                                            action: "edit",
                                                            body: skillBodyDraft.trim(),
                                                            description: skillDescDraft.trim(),
                                                        })
                                                    }
                                                />
                                            ))}
                                        </div>
                                    )}
                                </section>
                            )}

                            {/* Promote an observation — raw single observations a
                                human can hand-promote into a parked skill above. */}
                            {(detailTab === "all" || detailTab === "skills") && (
                                <section className="flex flex-col gap-3">
                                    <SectionHeader
                                        title="Promote an observation"
                                        count={skillObservations.length}
                                        hint="Raw observations the agent noted but hasn't distilled. Promote one to turn it into a skill held for review."
                                    />
                                    {skillObservations.length === 0 ? (
                                        <EmptyState
                                            icon={<ArrowUpCircle size={18} />}
                                            text="No raw observations to promote right now."
                                        />
                                    ) : (
                                        <div className="flex flex-col gap-2">
                                            {skillObservations.map((o) => (
                                                <ObservationRow
                                                    key={o.id}
                                                    observation={o}
                                                    busy={busyId === o.id}
                                                    onPromote={() => handlePromote(o.id)}
                                                />
                                            ))}
                                        </div>
                                    )}
                                </section>
                            )}
                        </div>
                    )}
                </div>
            </div>
        </div>
    );
}

function SectionHeader({
    title,
    count,
    hint,
}: {
    title: string;
    count: number;
    hint: string;
}) {
    return (
        <div className="flex flex-col gap-0.5">
            <div className="flex items-center gap-2">
                <h3 className="text-[15px] font-semibold text-[var(--modal-text-primary)]">
                    {title}
                </h3>
                <span className="text-[11px] font-bold px-[7px] py-[1px] rounded-full bg-[var(--modal-bg-tertiary)] text-[var(--modal-text-secondary)] border border-[var(--modal-border-secondary)]">
                    {count}
                </span>
            </div>
            <p className="text-[12px] text-[var(--modal-text-secondary)]">{hint}</p>
        </div>
    );
}

function EmptyState({ icon, text }: { icon: React.ReactNode; text: string }) {
    return (
        <div className="flex items-center gap-3 px-4 py-5 rounded-[10px] border border-dashed border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]/40">
            <span className="text-[var(--modal-text-secondary)]">{icon}</span>
            <span className="text-[13px] text-[var(--modal-text-secondary)] leading-relaxed">
                {text}
            </span>
        </div>
    );
}

function ReviewCard({
    candidate,
    busy,
    editing,
    editText,
    onEditTextChange,
    onStartEdit,
    onCancelEdit,
    onKeep,
    onPin,
    onForget,
    onSaveEdit,
}: {
    candidate: ReflectionCandidate;
    busy: boolean;
    editing: boolean;
    editText: string;
    onEditTextChange: (v: string) => void;
    onStartEdit: () => void;
    onCancelEdit: () => void;
    onKeep: () => void;
    onPin: () => void;
    onForget: () => void;
    onSaveEdit: () => void;
}) {
    return (
        <div className="flex flex-col gap-3 p-4 rounded-[12px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
            <div className="flex items-center gap-2 flex-wrap">
                <KindBadge kind={candidate.kind} />
                <ScopeBadge scope={candidate.target_scope ?? "Agent"} />
                {candidate.contradicts && (
                    <span className="inline-flex items-center gap-1 text-[10px] font-semibold px-[6px] py-[2px] rounded-[5px] bg-amber-500/15 text-amber-600 dark:text-amber-400">
                        <AlertTriangle size={11} />
                        May contradict an existing memory
                    </span>
                )}
                <span className="text-[11px] text-[var(--modal-text-secondary)] ml-auto">
                    staged {formatRelativeTime(candidate.created_at)}
                </span>
            </div>

            {editing ? (
                <textarea
                    value={editText}
                    onChange={(e) => onEditTextChange(e.target.value)}
                    rows={3}
                    autoFocus
                    className="w-full px-3 py-2 bg-[var(--modal-bg-input)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors resize-none leading-relaxed"
                />
            ) : (
                <p className="text-[14px] text-[var(--modal-text-primary)] leading-relaxed whitespace-pre-wrap break-words">
                    {candidate.content}
                </p>
            )}

            {candidate.reason && !editing && (
                <p className="text-[12px] text-[var(--modal-text-secondary)] italic leading-relaxed">
                    {candidate.reason}
                </p>
            )}

            <div className="flex items-center gap-2 flex-wrap">
                {editing ? (
                    <>
                        <ActionButton
                            variant="primary"
                            disabled={busy || !editText.trim()}
                            busy={busy}
                            icon={<Check size={14} />}
                            label="Save & keep"
                            onClick={onSaveEdit}
                        />
                        <ActionButton
                            variant="ghost"
                            disabled={busy}
                            icon={<X size={14} />}
                            label="Cancel"
                            onClick={onCancelEdit}
                        />
                    </>
                ) : (
                    <>
                        <ActionButton
                            variant="primary"
                            disabled={busy}
                            busy={busy}
                            icon={<Check size={14} />}
                            label="Keep"
                            onClick={onKeep}
                        />
                        <ActionButton
                            variant="ghost"
                            disabled={busy}
                            icon={<Pencil size={14} />}
                            label="Edit"
                            onClick={onStartEdit}
                        />
                        <ActionButton
                            variant="ghost"
                            disabled={busy}
                            icon={<Pin size={14} />}
                            label="Pin"
                            onClick={onPin}
                        />
                        <ActionButton
                            variant="danger"
                            disabled={busy}
                            icon={<Trash2 size={14} />}
                            label="Forget"
                            onClick={onForget}
                        />
                    </>
                )}
            </div>
        </div>
    );
}

function LearnedCard({
    entry,
    busy,
    onUndo,
}: {
    entry: MemoryEntry;
    busy: boolean;
    onUndo: () => void;
}) {
    return (
        <div className="group flex items-start gap-3 p-3 rounded-[12px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
            <div className="flex-1 min-w-0">
                <p className="text-[14px] text-[var(--modal-text-primary)] leading-relaxed whitespace-pre-wrap break-words">
                    {entry.content}
                </p>
                <div className="flex items-center gap-2 mt-1">
                    <span className="text-[11px] text-[var(--modal-text-secondary)]">
                        {formatRelativeTime(entry.created_at)}
                    </span>
                    {entry.pinned && (
                        <span className="inline-flex items-center gap-1 text-[10px] font-semibold text-[var(--modal-accent)]">
                            <Pin size={10} />
                            Pinned
                        </span>
                    )}
                </div>
            </div>
            <ActionButton
                variant="ghost"
                disabled={busy}
                busy={busy}
                icon={<Undo2 size={14} />}
                label="Undo"
                onClick={onUndo}
            />
        </div>
    );
}

/** Review card for a parked distilled skill. Mirrors `ReviewCard` (memory
 *  side): keep / edit / forget, with an inline edit that rewrites both the
 *  description and the body. The body is collapsed behind a toggle since a
 *  distilled SKILL.md body can be long. */
function SkillReviewCard({
    candidate,
    busy,
    editing,
    bodyDraft,
    descDraft,
    onBodyDraftChange,
    onDescDraftChange,
    onStartEdit,
    onCancelEdit,
    onKeep,
    onForget,
    onSaveEdit,
}: {
    candidate: ParkedSkillCandidate;
    busy: boolean;
    editing: boolean;
    bodyDraft: string;
    descDraft: string;
    onBodyDraftChange: (v: string) => void;
    onDescDraftChange: (v: string) => void;
    onStartEdit: () => void;
    onCancelEdit: () => void;
    onKeep: () => void;
    onForget: () => void;
    onSaveEdit: () => void;
}) {
    const [expanded, setExpanded] = useState(false);
    const fromCount = candidate.distilled_from.length;
    return (
        <div className="flex flex-col gap-3 p-4 rounded-[12px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
            <div className="flex items-center gap-2 flex-wrap">
                <SkillBadge />
                <span className="text-[14px] font-semibold text-[var(--modal-text-primary)] break-words">
                    {candidate.name}
                </span>
                <span className="text-[11px] text-[var(--modal-text-secondary)] ml-auto">
                    staged {formatRelativeTime(candidate.created_at)}
                </span>
            </div>

            {editing ? (
                <div className="flex flex-col gap-2">
                    <label className="text-[11px] font-semibold uppercase tracking-wide text-[var(--modal-text-secondary)]">
                        Description
                    </label>
                    <input
                        type="text"
                        value={descDraft}
                        onChange={(e) => onDescDraftChange(e.target.value)}
                        className="w-full px-3 py-2 bg-[var(--modal-bg-input)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors"
                    />
                    <label className="text-[11px] font-semibold uppercase tracking-wide text-[var(--modal-text-secondary)] mt-1">
                        Body
                    </label>
                    <textarea
                        value={bodyDraft}
                        onChange={(e) => onBodyDraftChange(e.target.value)}
                        rows={6}
                        className="w-full px-3 py-2 bg-[var(--modal-bg-input)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[13px] font-mono text-[var(--modal-text-primary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors resize-y leading-relaxed"
                    />
                </div>
            ) : (
                <>
                    <p className="text-[14px] text-[var(--modal-text-primary)] leading-relaxed whitespace-pre-wrap break-words">
                        {candidate.description}
                    </p>
                    <div className="flex items-center gap-2 flex-wrap">
                        <button
                            type="button"
                            onClick={() => setExpanded((v) => !v)}
                            className="inline-flex items-center gap-1 text-[12px] text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
                        >
                            <ChevronDown
                                size={13}
                                className={twMerge(
                                    "transition-transform",
                                    expanded && "rotate-180",
                                )}
                            />
                            {expanded ? "Hide details" : "Show details"}
                        </button>
                        <span className="text-[11px] text-[var(--modal-text-secondary)]">
                            {candidate.origin === "user_authored"
                                ? "written by the agent on request"
                                : `distilled from ${fromCount} observation${fromCount === 1 ? "" : "s"}`}
                        </span>
                    </div>
                    {expanded && (
                        <pre className="w-full px-3 py-2 bg-[var(--modal-bg-input)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[12px] font-mono text-[var(--modal-text-primary)] whitespace-pre-wrap break-words leading-relaxed overflow-x-auto">
                            {candidate.body}
                        </pre>
                    )}
                </>
            )}

            <div className="flex items-center gap-2 flex-wrap">
                {editing ? (
                    <>
                        <ActionButton
                            variant="primary"
                            disabled={busy || !bodyDraft.trim()}
                            busy={busy}
                            icon={<Check size={14} />}
                            label="Save & keep"
                            onClick={onSaveEdit}
                        />
                        <ActionButton
                            variant="ghost"
                            disabled={busy}
                            icon={<X size={14} />}
                            label="Cancel"
                            onClick={onCancelEdit}
                        />
                    </>
                ) : (
                    <>
                        <ActionButton
                            variant="primary"
                            disabled={busy}
                            busy={busy}
                            icon={<Check size={14} />}
                            label="Keep"
                            onClick={onKeep}
                        />
                        <ActionButton
                            variant="ghost"
                            disabled={busy}
                            icon={<Pencil size={14} />}
                            label="Edit"
                            onClick={onStartEdit}
                        />
                        <ActionButton
                            variant="danger"
                            disabled={busy}
                            icon={<Trash2 size={14} />}
                            label="Forget"
                            onClick={onForget}
                        />
                    </>
                )}
            </div>
        </div>
    );
}

/** A single raw skill observation with a one-click Promote into a parked
 *  distilled skill (which then shows up under "Skills held for review"). */
function ObservationRow({
    observation,
    busy,
    onPromote,
}: {
    observation: SkillObservation;
    busy: boolean;
    onPromote: () => void;
}) {
    const preview =
        observation.content.length > 140
            ? `${observation.content.slice(0, 140)}…`
            : observation.content;
    return (
        <div className="group flex items-start gap-3 p-3 rounded-[12px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
            <div className="flex-1 min-w-0">
                <p className="text-[14px] text-[var(--modal-text-primary)] leading-relaxed whitespace-pre-wrap break-words">
                    {preview}
                </p>
                <span className="text-[11px] text-[var(--modal-text-secondary)]">
                    {formatRelativeTime(observation.created_at)}
                </span>
            </div>
            <ActionButton
                variant="ghost"
                disabled={busy}
                busy={busy}
                icon={<ArrowUpCircle size={14} />}
                label="Promote"
                onClick={onPromote}
            />
        </div>
    );
}

/** Purple "skill" badge, matching the skill styling in `KindBadge`. */
function SkillBadge() {
    return (
        <span className="text-[10px] font-bold uppercase tracking-wide px-[6px] py-[2px] rounded-[5px] bg-purple-500/15 text-purple-600 dark:text-purple-400">
            skill
        </span>
    );
}

function KindBadge({ kind }: { kind: ReflectionCandidate["kind"] }) {
    const isSkill = kind === "skill";
    return (
        <span
            className={twMerge(
                "text-[10px] font-bold uppercase tracking-wide px-[6px] py-[2px] rounded-[5px]",
                isSkill
                    ? "bg-purple-500/15 text-purple-600 dark:text-purple-400"
                    : "bg-blue-500/15 text-blue-600 dark:text-blue-400",
            )}
        >
            {kind}
        </span>
    );
}

/** Color per storage scope a staged candidate would land in once approved.
 *  `AgentProject` has no live writer today but gets a color anyway so an
 *  unexpected value still renders something sensible instead of falling
 *  through to a default. */
const SCOPE_BADGE_STYLES: Record<MemoryScope, string> = {
    Agent: "bg-sky-500/15 text-sky-600 dark:text-sky-400",
    Project: "bg-teal-500/15 text-teal-600 dark:text-teal-400",
    Global: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
    Thread: "bg-violet-500/15 text-violet-600 dark:text-violet-400",
    AgentProject: "bg-orange-500/15 text-orange-600 dark:text-orange-400",
};

function ScopeBadge({ scope }: { scope: MemoryScope }) {
    return (
        <span
            className={twMerge(
                "text-[10px] font-bold uppercase tracking-wide px-[8px] py-[2px] rounded-full",
                SCOPE_BADGE_STYLES[scope],
            )}
        >
            {scope}
        </span>
    );
}

function ActionButton({
    variant,
    disabled,
    busy,
    icon,
    label,
    onClick,
}: {
    variant: "primary" | "ghost" | "danger";
    disabled?: boolean;
    busy?: boolean;
    icon: React.ReactNode;
    label: string;
    onClick: () => void;
}) {
    const styles: Record<typeof variant, string> = {
        primary:
            "bg-[var(--modal-accent)] text-white hover:opacity-90 border border-transparent",
        ghost: "bg-transparent text-[var(--modal-text-primary)] border border-[var(--modal-border-secondary)] hover:bg-[var(--modal-bg-hover)]",
        danger: "bg-transparent text-red-600 dark:text-red-400 border border-[var(--modal-border-secondary)] hover:bg-red-500/10 hover:border-red-500/30",
    };
    return (
        <button
            type="button"
            onClick={onClick}
            disabled={disabled}
            className={twMerge(
                "inline-flex items-center gap-1.5 px-3 py-[6px] rounded-[7px] text-[13px] font-medium transition-all cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed",
                styles[variant],
            )}
        >
            {busy ? <Loader2 size={14} className="animate-spin" /> : icon}
            {label}
        </button>
    );
}
