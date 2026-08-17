import { useEffect, useId, useMemo, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import {
    CircleUserRound,
    FileText,
    Loader2,
    X,
    LayoutList,
    Clock,
    AlignLeft,
    Box,
    MessageSquare,
} from "lucide-react";

import { agentAvatarColor } from "../../lib/agentColors";
import { displayOutputFilename, filterVisibleOutputs } from "../../lib/expectedOutputs";
import { useTaskDetail } from "../../stores/tasklistStore";
import { useChatStore } from "../../stores/chatStore";
import type { TaskComment, TaskDetail, TasklistScope } from "../../types/api";
import { Tooltip } from "../ui/Tooltip";
import { TaskStatusBadge } from "./TaskStatusBadge";
import { TasklistOutputPreview } from "./TasklistOutputPortal";

export interface TaskDetailModalProps {
    open: boolean;
    scope: TasklistScope | null;
    taskId: string | null;
    onClose: () => void;
}

export function TaskDetailModal({ open, scope, taskId, onClose }: TaskDetailModalProps) {
    const titleId = useId();
    const containerRef = useRef<HTMLDivElement | null>(null);
    const previouslyFocusedRef = useRef<HTMLElement | null>(null);

    const { data, tasklistId, loading, error, refetch } = useTaskDetail(
        open ? scope : null,
        open ? taskId : null,
    );

    // Filename of the output the user has opened from the Outputs grid.
    // null means no overlay; the rest of the modal is interactive.
    const [previewFilename, setPreviewFilename] = useState<string | null>(null);
    const isPreviewing = previewFilename !== null;

    // Reset the preview whenever the modal closes or switches tasks so the
    // overlay doesn't carry over to a different surface.
    useEffect(() => {
        if (!open) setPreviewFilename(null);
    }, [open]);
    useEffect(() => {
        setPreviewFilename(null);
    }, [taskId]);

    // Escape closes the preview if open, otherwise closes the modal.
    useEffect(() => {
        if (!open) return;
        const handler = (e: KeyboardEvent) => {
            if (e.key !== "Escape") return;
            if (isPreviewing) {
                setPreviewFilename(null);
            } else {
                onClose();
            }
        };
        document.addEventListener("keydown", handler);
        return () => document.removeEventListener("keydown", handler);
    }, [open, onClose, isPreviewing]);

    // Focus trap: keep Tab within the modal; restore focus on close.
    useEffect(() => {
        if (!open) return;
        previouslyFocusedRef.current = document.activeElement as HTMLElement | null;
        const container = containerRef.current;
        if (!container) return;

        const focusables = () =>
            container.querySelectorAll<HTMLElement>(
                'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
            );

        const id = window.setTimeout(() => {
            const list = focusables();
            (list[0] ?? container).focus();
        }, 0);

        const trap = (e: KeyboardEvent) => {
            if (e.key !== "Tab") return;
            const list = focusables();
            if (list.length === 0) {
                e.preventDefault();
                return;
            }
            const first = list[0];
            const last = list[list.length - 1];
            const active = document.activeElement as HTMLElement | null;
            if (e.shiftKey && active === first) {
                e.preventDefault();
                last.focus();
            } else if (!e.shiftKey && active === last) {
                e.preventDefault();
                first.focus();
            }
        };
        document.addEventListener("keydown", trap);

        return () => {
            window.clearTimeout(id);
            document.removeEventListener("keydown", trap);
            previouslyFocusedRef.current?.focus?.();
        };
    }, [open]);

    return (
        <AnimatePresence>
            {open && (
                <div className="fixed inset-0 z-[300] flex items-center justify-center">
                    <motion.div
                        initial={{ opacity: 0 }}
                        animate={{ opacity: 1 }}
                        exit={{ opacity: 0 }}
                        transition={{ duration: 0.15 }}
                        className="absolute inset-0 bg-black/40"
                        onClick={onClose}
                    />
                    <motion.div
                        ref={containerRef}
                        role="dialog"
                        aria-modal="true"
                        aria-labelledby={titleId}
                        tabIndex={-1}
                        initial={{ opacity: 0, scale: 0.96 }}
                        animate={{ opacity: 1, scale: 1 }}
                        exit={{ opacity: 0, scale: 0.96 }}
                        transition={{ duration: 0.15, ease: "easeOut" }}
                        className={`relative w-full ${
                            isPreviewing
                                ? "max-w-[1200px] h-[92vh]"
                                : "max-w-[760px] max-h-[85vh]"
                        } rounded-[16px] overflow-hidden bg-[var(--bg-secondary)] border border-[var(--border-secondary)] flex flex-col shadow-2xl transition-[max-width,height,max-height] duration-200 ease-out`}
                        style={{
                            boxShadow:
                                "0 0 0 1px rgba(0,0,0,0.13), 0 24px 64px 0 rgba(0,0,0,0.35)",
                        }}
                    >
                        {/* Minimal Header */}
                        <div className="flex items-center justify-between px-[16px] py-[14px]">
                            <div className="text-[13px] font-medium text-[var(--text-tertiary)] flex items-center gap-[8px] pl-[8px]">
                                <LayoutList className="w-[16px] h-[16px]" />
                                <span>Task Detail</span>
                            </div>
                            <button
                                type="button"
                                onClick={onClose}
                                className="w-[32px] h-[32px] rounded-[8px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] transition-colors cursor-pointer flex-shrink-0"
                                aria-label="Close"
                            >
                                <X className="w-[18px] h-[18px]" />
                            </button>
                        </div>

                        <div className="flex-1 min-h-0 overflow-y-auto px-[40px] pb-[60px] pt-[8px]">
                            {loading && !data && (
                                <div className="py-[40px] flex justify-center items-center gap-[10px] text-[14px] text-[var(--text-secondary)]">
                                    <Loader2 className="w-[16px] h-[16px] animate-spin" />
                                    <span>Loading task…</span>
                                </div>
                            )}
                            {error && !data && (
                                <div className="my-[18px] flex flex-col gap-[10px] px-[16px] py-[12px] rounded-[8px] bg-[var(--error-bg)] border border-[var(--error-border)] text-[14px] text-[var(--error)]">
                                    <span>{error}</span>
                                    <div>
                                        <button
                                            type="button"
                                            onClick={refetch}
                                            className="h-[32px] px-[16px] rounded-[6px] text-[13px] font-semibold bg-[var(--bg-hover)] hover:bg-[var(--bg-tertiary)] text-[var(--text-primary)] transition-colors cursor-pointer"
                                        >
                                            Retry
                                        </button>
                                    </div>
                                </div>
                            )}
                            {data && (
                                <>
                                    <h2
                                        id={titleId}
                                        className="text-[18px] font-semibold text-[var(--text-primary)] leading-[1.4] mb-[32px]"
                                    >
                                        {data.title}
                                    </h2>

                                    {/* Properties Sheet */}
                                    <div className="flex flex-col gap-[16px] mb-[40px]">
                                        <div className="flex items-center group">
                                            <div className="w-[140px] flex-shrink-0 text-[14px] text-[var(--text-tertiary)] flex items-center gap-[8px]">
                                                <CircleUserRound className="w-[16px] h-[16px]" />
                                                Assignee
                                            </div>
                                            <div className="flex-1">
                                                <AssignedAgentRow agent={data.assigned_agent} />
                                            </div>
                                        </div>

                                        <div className="flex items-center group">
                                            <div className="w-[140px] flex-shrink-0 text-[14px] text-[var(--text-tertiary)] flex items-center gap-[8px]">
                                                <Clock className="w-[16px] h-[16px]" />
                                                Status
                                            </div>
                                            <div className="flex-1">
                                                <TaskStatusBadge status={data.status} />
                                            </div>
                                        </div>
                                    </div>

                                    {data.prompt?.trim() &&
                                        data.prompt.trim() !== data.title.trim() && (
                                            <div className="mb-[40px]">
                                                <div className="text-[16px] font-semibold text-[var(--text-primary)] flex items-center gap-[8px] mb-[12px]">
                                                    <FileText className="w-[18px] h-[18px] text-[var(--text-secondary)]" />
                                                    Prompt
                                                </div>
                                                <div className="text-[15px] text-[var(--text-secondary)] leading-[1.6] whitespace-pre-wrap break-words pl-[26px]">
                                                    {data.prompt}
                                                </div>
                                            </div>
                                        )}

                                    {data.description?.trim() && (
                                        <div className="mb-[40px]">
                                            <div className="text-[16px] font-semibold text-[var(--text-primary)] flex items-center gap-[8px] mb-[12px]">
                                                <AlignLeft className="w-[18px] h-[18px] text-[var(--text-secondary)]" />
                                                Description
                                            </div>
                                            <div className="text-[15px] text-[var(--text-secondary)] leading-[1.6] whitespace-pre-wrap break-words pl-[26px]">
                                                {data.description}
                                            </div>
                                        </div>
                                    )}

                                    {(() => {
                                        const visibleOutputs = data.expected_outputs
                                            ? filterVisibleOutputs(data.expected_outputs)
                                            : [];
                                        if (visibleOutputs.length === 0) return null;
                                        return (
                                            <div className="mb-[40px]">
                                                <div className="text-[16px] font-semibold text-[var(--text-primary)] flex items-center gap-[8px] mb-[16px]">
                                                    <Box className="w-[18px] h-[18px] text-[var(--text-secondary)]" />
                                                    Outputs
                                                </div>
                                                <div className="pl-[26px]">
                                                    <TaskOutputsGrid
                                                        outputs={visibleOutputs}
                                                        onSelect={
                                                            tasklistId
                                                                ? setPreviewFilename
                                                                : undefined
                                                        }
                                                    />
                                                </div>
                                            </div>
                                        );
                                    })()}

                                    <hr className="border-[var(--border-secondary)] my-[40px]" />

                                    <div className="mb-[16px]">
                                        <div className="text-[16px] font-semibold text-[var(--text-primary)] flex items-center gap-[8px] mb-[20px]">
                                            <MessageSquare className="w-[18px] h-[18px] text-[var(--text-secondary)]" />
                                            Comments
                                        </div>
                                        <div className="pl-[26px]">
                                            <TaskCommentsSection comments={data.comments ?? []} />
                                        </div>
                                    </div>
                                </>
                            )}
                        </div>

                        {/* Output preview overlay. Sits inside the modal, fills it
                         *  edge-to-edge. Closing the X here returns to the task
                         *  detail view; the modal backdrop still closes the
                         *  whole modal. */}
                        <TasklistOutputPreview
                            scope={isPreviewing ? scope : null}
                            tasklistId={isPreviewing ? tasklistId : null}
                            filename={previewFilename}
                            onClose={() => setPreviewFilename(null)}
                        />
                    </motion.div>
                </div>
            )}
        </AnimatePresence>
    );
}

function TaskCommentsSection({ comments }: { comments: TaskComment[] }) {
    const agentSnapshots = useChatStore((s) => s.agents);
    const agentNameMap = useMemo(() => {
        const map: Record<string, string> = {};
        for (const a of agentSnapshots) map[a.agent_id] = a.name;
        return map;
    }, [agentSnapshots]);

    const sorted = useMemo(() => {
        return [...comments].sort((a, b) => {
            const ta = Date.parse(a.created_at);
            const tb = Date.parse(b.created_at);
            const sa = Number.isNaN(ta) ? 0 : ta;
            const sb = Number.isNaN(tb) ? 0 : tb;
            return sa - sb;
        });
    }, [comments]);

    if (sorted.length === 0) {
        return (
            <p className="text-[14px] italic text-[var(--text-tertiary)]">
                No comments yet.
            </p>
        );
    }

    return (
        <ul className="flex flex-col gap-[24px] list-none p-0 m-0">
            {sorted.map((c) => (
                <CommentRow
                    key={c.id}
                    comment={c}
                    authorLabel={resolveCommentAuthor(c, agentNameMap)}
                />
            ))}
        </ul>
    );
}

function CommentRow({
    comment,
    authorLabel,
}: {
    comment: TaskComment;
    authorLabel: string;
}) {
    const relative = formatRelativeTimestamp(comment.created_at);
    const absolute = formatAbsoluteTimestamp(comment.created_at);
    // Simple placeholder for avatar
    const initial = authorLabel.charAt(0).toUpperCase();

    return (
        <li className="flex gap-[14px]">
            <div
                className="w-[36px] h-[36px] rounded-[10px] flex-shrink-0 flex items-center justify-center text-[16px] font-semibold text-white shadow-sm"
                style={{ backgroundColor: agentAvatarColor(authorLabel) }}
            >
                {initial}
            </div>
            <div className="flex-1 flex flex-col mt-[2px]">
                <div className="flex items-center gap-[8px] mb-[4px]">
                    <span className="font-semibold text-[14px] text-[var(--text-primary)]">
                        {authorLabel}
                    </span>
                    <Tooltip placement="top" label={absolute}>
                        <time
                            className="text-[12px] text-[var(--text-tertiary)] hover:underline cursor-default"
                            dateTime={comment.created_at}
                            title={absolute}
                        >
                            {relative}
                        </time>
                    </Tooltip>
                </div>
                <div className="text-[14px] text-[var(--text-secondary)] leading-[1.6] whitespace-pre-wrap break-words">
                    {comment.body}
                </div>
            </div>
        </li>
    );
}

function resolveCommentAuthor(
    comment: TaskComment,
    agentNameMap: Record<string, string>,
): string {
    if (comment.author_kind === "user") return "You";
    return agentNameMap[comment.author_id] ?? comment.author_id ?? "Agent";
}

function formatRelativeTimestamp(iso: string): string {
    const d = new Date(iso);
    const ms = d.getTime();
    if (Number.isNaN(ms)) return iso;
    const diffSec = Math.max(0, Math.floor((Date.now() - ms) / 1000));
    if (diffSec < 60) return "just now";
    const diffMin = Math.floor(diffSec / 60);
    if (diffMin < 60) return `${diffMin}m ago`;
    const diffHr = Math.floor(diffMin / 60);
    if (diffHr < 24) return `${diffHr}h ago`;
    const diffDay = Math.floor(diffHr / 24);
    if (diffDay < 30) return `${diffDay}d ago`;
    const diffMon = Math.floor(diffDay / 30);
    if (diffMon < 12) return `${diffMon}mo ago`;
    const diffYr = Math.floor(diffDay / 365);
    return `${diffYr}y ago`;
}

function formatAbsoluteTimestamp(iso: string): string {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    return d.toLocaleString(undefined, {
        year: "numeric",
        month: "short",
        day: "numeric",
        hour: "numeric",
        minute: "2-digit",
    });
}

function TaskOutputsGrid({
    outputs,
    onSelect,
}: {
    outputs: string[];
    /** When provided, tiles render as clickable buttons that open the
     *  output preview overlay. Omitted when the tasklist id can't be
     *  resolved yet. */
    onSelect?: (filename: string) => void;
}) {
    return (
        <ul className="grid grid-cols-1 sm:grid-cols-2 gap-[12px] list-none p-0 m-0">
            {outputs.map((filename) => {
                const displayName = displayOutputFilename(filename);
                const tileBody = (
                    <>
                        <span
                            className="w-[36px] h-[36px] rounded-[10px] bg-[var(--bg-secondary)] flex items-center justify-center flex-shrink-0 group-hover:bg-[var(--bg-tertiary)] transition-colors"
                            aria-hidden="true"
                        >
                            <FileText className="w-[18px] h-[18px] text-[var(--text-secondary)]" />
                        </span>
                        <div className="flex-1 min-w-0 text-left">
                            <div
                                className="text-[14px] font-medium text-[var(--text-primary)] truncate"
                                title={filename}
                            >
                                {displayName}
                            </div>
                        </div>
                    </>
                );
                return (
                    <li key={filename}>
                        {onSelect ? (
                            <button
                                type="button"
                                onClick={() => onSelect(filename)}
                                className="group w-full flex items-center gap-[12px] px-[14px] py-[12px] rounded-[12px] border border-[var(--border-secondary)] bg-[var(--bg-primary)] hover:border-[var(--border-hover)] hover:shadow-sm transition-all cursor-pointer outline-none focus-visible:ring-2 focus-visible:ring-[var(--focus-ring)]"
                                aria-label={`Preview ${displayName}`}
                            >
                                {tileBody}
                            </button>
                        ) : (
                            <div className="group flex items-center gap-[12px] px-[14px] py-[12px] rounded-[12px] border border-[var(--border-secondary)] bg-[var(--bg-primary)] hover:border-[var(--border-hover)] hover:shadow-sm transition-all">
                                {tileBody}
                            </div>
                        )}
                    </li>
                );
            })}
        </ul>
    );
}

function AssignedAgentRow({ agent }: { agent: TaskDetail["assigned_agent"] }) {
    if (!agent) {
        return (
            <div className="flex items-center gap-[8px] text-[14px] text-[var(--text-tertiary)]">
                <span
                    className="w-[28px] h-[28px] rounded-full flex items-center justify-center select-none bg-[var(--bg-secondary)] text-[var(--text-tertiary)]"
                    aria-hidden="true"
                >
                    <CircleUserRound size={16} />
                </span>
                <span>Unassigned</span>
            </div>
        );
    }
    const color = agentAvatarColor(agent.name);
    return (
        <div className="flex items-center gap-[8px] text-[14px] text-[var(--text-primary)] font-medium">
            <span
                className="w-[28px] h-[28px] rounded-full flex items-center justify-center text-[16px] leading-none select-none shadow-sm"
                style={{ backgroundColor: color }}
                aria-hidden="true"
            >
                {agent.emoji ?? ""}
            </span>
            <span className="truncate">{agent.name}</span>
        </div>
    );
}
