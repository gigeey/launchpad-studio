import { useState, useEffect, useCallback, useMemo, useRef } from "react";
import { createPortal } from "react-dom";
import { AnimatePresence, motion } from "framer-motion";
import { useAgentTasklistRunSSE } from "../../hooks/useAgentTasklistRunSSE";
import {
  Loader2,
  Check,
  Circle,
  AlertTriangle,
  SkipForward,
  CircleUserRound,
  Rows2,
  Columns2,
  Send,
  Trash2,
  LayoutList,
  Clock,
  AlignLeft,
  X,
  ListPlus,
  Play,
  Paperclip,
  FileIcon,
  ImageIcon,
  AlertCircle,
  ListTodo
} from "lucide-react";
import { Tooltip } from "../ui/Tooltip";
import ConfirmDialog from "../ui/ConfirmDialog";
import {
  useAgentTasklistStore,
  useAgentTasklistsForAgent,
} from "../../stores/agentTasklistStore";
import { useChatStore } from "../../stores/chatStore";
import { useDraftStore } from "../../stores/draftStore";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";
import { agentAvatarColor } from "../../lib/agentColors";
import { useNow } from "../../hooks/useNow";
import {
  getTaskRunStart,
  formatRunElapsed,
  computeTasklistElapsedMs,
} from "../../lib/taskTimers";
import { TaskStatusBadge } from "../tasklist/TaskStatusBadge";
import type {
  Task,
  Tasklist,
  TaskGroup,
  TaskGroupMode,
  TaskStatus,
  TasklistStatus,
  DelegateTarget,
  Attachment,
} from "../../types/api";
import {
  appendAgentTask,
  createAgentTasklist,
  startAgentTasklist,
  stopAgentTasklist,
  skipAgentTask,
  uploadAttachment,
  deleteAttachment,
} from "../../lib/api";

// ---------------------------------------------------------------------------
// Ownership helper functions — exported for unit tests
// ---------------------------------------------------------------------------

/** Returns the visual chip mode for a task row based on its assignment. */
export function ownerChipMode(
  task: Task,
): "pinned" | "classified" | "classifying" {
  if (!task.assignment) return "classifying";
  return task.assignment.mode === "pinned" ? "pinned" : "classified";
}

/** Resolve the display name for the owner of a task from the agent's
 *  delegate targets. */
export function resolveOwnerDisplayName(
  task: Task,
  agentId: string,
  selfName: string,
  delegateTargets: DelegateTarget[],
): string | null {
  if (!task.assignment) return null;
  const ownerId = task.assignment.owner_agent_id;
  if (!ownerId) return null;
  if (ownerId === agentId) return selfName;
  const target = delegateTargets.find((t) => t.target_agent_id === ownerId);
  return target?.name ?? ownerId;
}

interface TodoPanelProps {
  agentId: string;
}

// Fallback emoji used when an agent profile has no emoji set. Matches the
// team panel's behavior (where the team's own emoji is the fallback) but
// chooses a neutral sparkle here since the agent surface has no team emoji.
const FALLBACK_OWNER_EMOJI = "✨";

// ---------------------------------------------------------------------------
// Shared visual primitives — mirror the team tasklist panel so an agent's
// todos read with the same vocabulary as a team's tasks (rounded pill rows,
// banded group headers, status pill, banded progress row).
// ---------------------------------------------------------------------------

/** Rounded-square checkbox that doubles as the per-row status indicator.
 *  Matches the team tasklist's StatusCheckbox so completed/in-progress rows
 *  read identically across the agent + team surfaces. Adds a `skipped` glyph
 *  for the agent-specific terminal state. */
function StatusCheckbox({ status }: { status: TaskStatus }) {
  if (status === "completed") {
    return (
      <span
        className="shrink-0 w-[18px] h-[18px] rounded-[6px] flex items-center justify-center"
        style={{ backgroundColor: "var(--text-primary)" }}
      >
        <Check size={12} strokeWidth={3} style={{ color: "var(--bg-primary)" }} />
      </span>
    );
  }
  if (status === "in_progress") {
    return (
      <span
        className="shrink-0 w-[18px] h-[18px] rounded-[6px] border flex items-center justify-center"
        style={{ borderColor: "var(--checkbox-border)" }}
      >
        {/* size=12 (not 11): the box is 18px with a 1px border, leaving a
            16px content area. An even icon size splits that remainder
            symmetrically (2px both sides); an odd size like 11 leaves a
            fractional 2.5/2.5 split that can round unevenly, and because
            this is the only status glyph that rotates, even a ~1px
            off-center offset reads as the disc visibly orbiting instead of
            spinning in place. */}
        <Loader2
          size={12}
          className="animate-spin block shrink-0"
          style={{ color: "var(--text-secondary)" }}
        />
      </span>
    );
  }
  if (status === "failed") {
    return (
      <span
        className="shrink-0 w-[18px] h-[18px] rounded-[6px] flex items-center justify-center"
        style={{ backgroundColor: "rgba(244,63,94,0.85)" }}
      >
        <AlertTriangle size={11} strokeWidth={2.5} style={{ color: "#fff" }} />
      </span>
    );
  }
  if (status === "blocked") {
    return (
      <span className="shrink-0 w-[18px] h-[18px] rounded-[6px] border border-amber-400 flex items-center justify-center">
        <Circle size={7} fill="currentColor" style={{ color: "rgb(217,119,6)" }} />
      </span>
    );
  }
  if (status === "skipped") {
    return (
      <span
        className="shrink-0 w-[18px] h-[18px] rounded-[6px] flex items-center justify-center"
        style={{ backgroundColor: "var(--bg-tertiary)" }}
      >
        <SkipForward size={11} style={{ color: "var(--text-tertiary)" }} />
      </span>
    );
  }
  // Pending / default — hollow rounded square.
  return (
    <span
      className="shrink-0 w-[18px] h-[18px] rounded-[6px] border"
      style={{ borderColor: "var(--checkbox-border)" }}
    />
  );
}

/** Compact pill that surfaces the overall tasklist lifecycle state next to
 *  the title. Color tokens match the team panel's pill so the two surfaces
 *  feel like one product. */
function TasklistStatusPill({ status }: { status: TasklistStatus }) {
  const config: Record<
    TasklistStatus,
    { label: string; bg: string; fg: string }
  > = {
    active: {
      label: "active",
      bg: "rgba(59,130,246,0.12)",
      fg: "rgb(37,99,235)",
    },
    paused: {
      label: "paused",
      bg: "rgba(217,119,6,0.14)",
      fg: "rgb(180,83,9)",
    },
    completed: {
      label: "completed",
      bg: "rgba(16,185,129,0.12)",
      fg: "rgb(5,150,105)",
    },
    failed: {
      label: "failed",
      bg: "rgba(244,63,94,0.12)",
      fg: "rgb(190,18,60)",
    },
    cancelled: {
      label: "cancelled",
      bg: "var(--bg-tertiary)",
      fg: "var(--text-tertiary)",
    },
  };
  const c = config[status];
  return (
    <span
      className="px-[10px] py-[3px] rounded-full text-[11px] font-medium"
      style={{ backgroundColor: c.bg, color: c.fg }}
    >
      {c.label}
    </span>
  );
}

/** Banded SEQ/PAR section header — matches the team panel's edge-to-edge
 *  blue/purple band so users instantly recognize which tasks run together
 *  vs in order. */
function GroupHeader({
  index,
  mode,
  isFirst,
}: {
  index: number;
  mode: TaskGroupMode;
  isFirst: boolean;
}) {
  const isParallel = mode === "PAR";
  const bandColor = isParallel ? "rgb(126,34,206)" : "rgb(37,99,235)";
  return (
    <div
      className={`flex items-center gap-2 px-4 py-2 ${isFirst ? "border-b-0" : "border-y-0"
        }`}
      style={{
        backgroundColor: isParallel
          ? "rgba(168,85,247,0.14)"
          : "rgba(59,130,246,0.12)",
        borderColor: "var(--border-primary)",
      }}
    >
      <span
        className="inline-flex items-center gap-1 text-[10.5px] font-semibold uppercase tracking-wider"
        style={{ color: bandColor }}
      >
        {isParallel ? <Columns2 size={10} /> : <Rows2 size={10} />}
        {isParallel ? "Parallel" : "Sequential"}
      </span>
      <span
        className="text-[10.5px] font-medium uppercase tracking-wider opacity-70"
        style={{ color: bandColor }}
      >
        Group {index + 1}
      </span>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Owner avatar — mirrors the team panel's OwnerAvatar so the two surfaces
// share one visual vocabulary for "who owns this task". Unassigned rows
// (still classifying, or no owner resolved) render a muted person icon;
// assigned rows render a colored square with the owner agent's emoji.
// ---------------------------------------------------------------------------

function OwnerAvatar({
  ownerName,
  ownerEmoji,
  circular,
  unassigned = false,
  active = false,
  classifying = false,
}: {
  ownerName: string;
  ownerEmoji: string;
  circular: boolean;
  /** True when no owner is resolved yet (no assignment + no legacy owner). */
  unassigned?: boolean;
  /** True when this agent's task is in flight — adds a pulsing ring so the
   *  user can see at a glance which agent is currently working. */
  active?: boolean;
  /** True when the classifier hasn't yet picked an owner. Renders the same
   *  unassigned glyph but with a tooltip that hints at the in-flight routing
   *  decision (distinct from "permanently unassigned"). */
  classifying?: boolean;
}) {
  if (unassigned || classifying) {
    const label = classifying
      ? "Classifying — waiting for routing"
      : "Unassigned";
    const testId = classifying
      ? "owner-chip-classifying"
      : "owner-chip-unassigned";
    return (
      <Tooltip placement="top" label={label} className="shrink-0">
        <span
          data-testid={testId}
          className={`relative w-[24px] h-[24px] ${circular ? "rounded-full" : "rounded-[7px]"} flex items-center justify-center select-none`}
          style={{
            backgroundColor: "var(--bg-secondary)",
            color: "var(--text-tertiary)",
            border: "1px dashed var(--border-secondary)",
          }}
          aria-label={label}
        >
          <CircleUserRound size={14} />
          {classifying && (
            <span
              className={`absolute inset-0 ${circular ? "rounded-full" : "rounded-[7px]"} animate-pulse pointer-events-none`}
              aria-hidden
            />
          )}
        </span>
      </Tooltip>
    );
  }
  const color = agentAvatarColor(ownerName);
  const tooltipLabel = active ? `${ownerName} — working…` : ownerName;
  return (
    <Tooltip placement="top" label={tooltipLabel} className="shrink-0">
      <span
        data-testid="owner-chip-assigned"
        className={`relative w-[24px] h-[24px] ${circular ? "rounded-full" : "rounded-[7px]"} flex items-center justify-center text-[14px] leading-none select-none`}
        style={{
          backgroundColor: color,
          boxShadow: active ? "0 0 0 2px rgba(59,130,246,0.55)" : undefined,
        }}
        aria-label={tooltipLabel}
      >
        {ownerEmoji}
        {active && (
          <span
            className={`absolute inset-0 ${circular ? "rounded-full" : "rounded-[7px]"} animate-ping pointer-events-none`}
            style={{ boxShadow: "0 0 0 2px rgba(59,130,246,0.45)" }}
            aria-hidden
          />
        )}
      </span>
    </Tooltip>
  );
}

// ---------------------------------------------------------------------------
// Task pill row — load-bearing visual unit. Clicking the row opens the
// detail modal (mirrors the team panel) so multi-line prompts no longer
// expand inline.
// ---------------------------------------------------------------------------

interface TaskRowProps {
  task: Task;
  tasklistId: string;
  /** Shared per-second tick used to recompute the live elapsed label without
   *  each row owning its own interval. */
  now: number;
  agentId: string;
  agentName: (agentId: string) => string;
  agentEmojiFor: (agentId: string) => string;
  delegateTargets: DelegateTarget[];
  selfName: string;
  circularAvatars: boolean;
  onSkip: (taskId: string) => void;
  skipping: boolean;
  onOpenDetail: (taskId: string) => void;
}

function TaskRow({
  task,
  tasklistId,
  now,
  agentId,
  agentName,
  agentEmojiFor,
  delegateTargets,
  selfName,
  circularAvatars,
  onSkip,
  skipping,
  onOpenDetail,
}: TaskRowProps) {
  const firstLine = task.prompt.split("\n")[0]?.trim() || task.prompt;
  const isCompleted = task.status === "completed";
  const isSkipped = task.status === "skipped";
  const isFailed = task.status === "failed";
  const isActive = task.status === "in_progress";

  // Live "running for…" label for the in-flight row. The origin is captured
  // client-side when the task first enters in_progress (persisted across
  // reloads); `now` ticks once per second at the panel level to advance it.
  const runStart = isActive ? getTaskRunStart(tasklistId, task.id) : null;
  const elapsedLabel =
    runStart != null ? formatRunElapsed(now - runStart) : null;

  const chipMode = ownerChipMode(task);
  const ownerLabel = resolveOwnerDisplayName(
    task,
    agentId,
    selfName,
    delegateTargets,
  );
  // Resolve the avatar emoji from whichever id is most authoritative —
  // assignment.owner_agent_id wins, falling back to the legacy
  // owner_agent_id field on the Task shape.
  const ownerIdForAvatar =
    task.assignment?.owner_agent_id || task.owner_agent_id || "";
  const ownerEmoji = ownerIdForAvatar
    ? agentEmojiFor(ownerIdForAvatar)
    : FALLBACK_OWNER_EMOJI;

  // "Unassigned" means no legacy owner AND no resolved assignment. While the
  // classifier is still picking we render the same glyph but with a hint
  // tooltip so users can tell the two states apart on hover.
  const isClassifying = chipMode === "classifying";
  const hasOwner = !!ownerLabel || !!task.owner_agent_id;
  const isUnassigned = !isClassifying && !hasOwner;
  const avatarOwnerName = ownerLabel ?? agentName(task.owner_agent_id);

  // Joined error_log for the failure tooltip. Falls back to a generic message
  // when the failure path didn't push anything before the feeder gave up.
  const errorTooltipLabel =
    isFailed && task.error_log.length > 0
      ? task.error_log.join("\n\n")
      : "Failed — no error reason was recorded.";

  // Row activation opens the detail modal — except when the activation
  // originated inside an inner button (skip), which has its own handler.
  const handleRowActivate = (e: React.SyntheticEvent) => {
    const target = e.target as HTMLElement | null;
    if (target?.closest("button, a")) return;
    onOpenDetail(task.id);
  };

  return (
    <div
      className="group flex flex-col gap-0 rounded-[12px] transition-colors"
      style={{ backgroundColor: "var(--bg-tertiary)" }}
    >
      <div
        className="flex items-center gap-2 px-3 py-[10px] cursor-pointer hover:bg-[var(--bg-hover)] rounded-[12px]"
        role="button"
        tabIndex={0}
        onClick={handleRowActivate}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            handleRowActivate(e);
          }
        }}
        aria-label={`Open task details: ${firstLine}`}
      >
        <StatusCheckbox status={task.status} />

        <Tooltip placement="top" label={task.prompt} className="flex-1 min-w-0">
          <span
            className={`block truncate text-[13px] ${isCompleted || isSkipped ? "line-through opacity-60" : ""
              }`}
            style={{ color: "var(--text-primary)" }}
          >
            {firstLine}
          </span>
        </Tooltip>

        {isFailed && (
          <Tooltip placement="top" label={errorTooltipLabel}>
            <span
              className="shrink-0 inline-flex items-center gap-1 px-[8px] h-[20px] rounded-full text-[10.5px] font-medium uppercase tracking-wide cursor-help"
              style={{
                backgroundColor: "rgba(244,63,94,0.12)",
                color: "rgb(190,18,60)",
              }}
              aria-label="Failed — hover for details"
            >
              <AlertTriangle size={10} strokeWidth={2.5} />
              failed
            </span>
          </Tooltip>
        )}

        {task.status === "pending" && (
          <Tooltip
            placement="top"
            label="Skip this task and let the tasklist continue past it"
            className="shrink-0"
          >
            <button
              type="button"
              onClick={(e) => {
                e.stopPropagation();
                onSkip(task.id);
              }}
              disabled={skipping}
              aria-label="Skip task"
              className="inline-flex items-center gap-1 px-[8px] h-[20px] rounded-full text-[10.5px] font-medium transition-colors cursor-pointer hover:bg-[var(--bg-hover)] opacity-0 group-hover:opacity-100 disabled:opacity-40 disabled:cursor-not-allowed"
              style={{
                color: "var(--text-secondary)",
                border: "1px solid var(--border-primary)",
              }}
            >
              <SkipForward size={10} />
              Skip
            </button>
          </Tooltip>
        )}

        {/* Live elapsed pill — only on the in-flight row. Shows how long the
         *  task has been running, ticking once per second. tabular-nums keeps
         *  the width stable so the row doesn't jitter on each tick. */}
        {isActive && elapsedLabel && (
          <Tooltip placement="top" label={`Running for ${elapsedLabel}`}>
            <span
              className="shrink-0 inline-flex items-center gap-1 px-[8px] h-[20px] rounded-full text-[10.5px] font-medium tabular-nums"
              style={{
                backgroundColor: "var(--bg-secondary)",
                color: "var(--text-secondary)",
              }}
              aria-label={`Running for ${elapsedLabel}`}
            >
              <Clock size={10} />
              {elapsedLabel}
            </span>
          </Tooltip>
        )}

        {/* Owner avatar — matches the team panel's avatar treatment so this
         *  surface shares one visual vocabulary for "who owns this task".
         *  Classifying / unassigned states render the neutral person glyph
         *  with a hint tooltip; assigned rows show the colored avatar with
         *  the owner agent's emoji. */}
        <OwnerAvatar
          ownerName={avatarOwnerName || ""}
          ownerEmoji={ownerEmoji}
          circular={circularAvatars}
          unassigned={isUnassigned}
          classifying={isClassifying}
          active={isActive && hasOwner}
        />
      </div>
    </div>
  );
}

interface TaskGroupSectionProps {
  group: TaskGroup;
  groupIndex: number;
  isFirstVisible: boolean;
  tasklistId: string;
  now: number;
  agentId: string;
  agentName: (agentId: string) => string;
  agentEmojiFor: (agentId: string) => string;
  delegateTargets: DelegateTarget[];
  selfName: string;
  circularAvatars: boolean;
  onSkip: (taskId: string) => void;
  skipping: string | null;
  onOpenDetail: (taskId: string) => void;
}

function TaskGroupSection({
  group,
  groupIndex,
  isFirstVisible,
  tasklistId,
  now,
  agentId,
  agentName,
  agentEmojiFor,
  delegateTargets,
  selfName,
  circularAvatars,
  onSkip,
  skipping,
  onOpenDetail,
}: TaskGroupSectionProps) {
  return (
    <div className="flex flex-col">
      <GroupHeader
        index={groupIndex}
        mode={group.mode}
        isFirst={isFirstVisible}
      />
      <div className="flex flex-col gap-2 px-4 py-2">
        {group.tasks.map((task) => (
          <TaskRow
            key={task.id}
            task={task}
            tasklistId={tasklistId}
            now={now}
            agentId={agentId}
            agentName={agentName}
            agentEmojiFor={agentEmojiFor}
            delegateTargets={delegateTargets}
            selfName={selfName}
            circularAvatars={circularAvatars}
            onSkip={onSkip}
            skipping={skipping === task.id}
            onOpenDetail={onOpenDetail}
          />
        ))}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Todo detail modal — agent surface counterpart of TaskDetailModal. Renders
// from the in-memory Task object (no backend fetch) since the agent
// tasklist store already carries the prompt, status, owner, error_log, and
// expected_outputs we want to surface. Mirrors the team modal's chrome and
// section layout so the two surfaces feel like one product.
// ---------------------------------------------------------------------------

function TodoDetailModal({
  open,
  task,
  ownerAvatarName,
  ownerEmoji,
  ownerSubtitle,
  circular,
  runningElapsedLabel,
  onClose,
}: {
  open: boolean;
  task: Task | null;
  /** Display name used for the avatar's hash-derived background color +
   *  tooltip. Falls back to "Unassigned" when the row has no owner yet. */
  ownerAvatarName: string;
  /** Emoji glyph rendered inside the avatar. Ignored when the row is
   *  unassigned / still classifying. */
  ownerEmoji: string;
  /** Sub-label shown under the avatar (e.g. "Pinned owner", "Classifier
   *  picked", "Classifying…"). */
  ownerSubtitle: string;
  /** True when the row is unassigned or the classifier hasn't picked yet. */
  circular: boolean;
  /** Live "running for…" label when the task is in flight; null otherwise.
   *  Advances with the panel's per-second tick. */
  runningElapsedLabel: string | null;
  onClose: () => void;
}) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const previouslyFocusedRef = useRef<HTMLElement | null>(null);

  // Escape closes the modal.
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [open, onClose]);

  // Focus trap matching the team modal's behavior.
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

  const titleLine =
    task?.prompt.split("\n")[0]?.trim() || task?.prompt || "Untitled task";
  const isUnassigned = ownerSubtitle === "Unassigned" || !task?.assignment;
  const isClassifying = ownerSubtitle === "Classifying…";
  const avatarColor = !isUnassigned && !isClassifying
    ? agentAvatarColor(ownerAvatarName)
    : "var(--bg-secondary)";

  return createPortal(
    <AnimatePresence>
      {open && task && (
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
            aria-labelledby="todo-detail-title"
            tabIndex={-1}
            initial={{ opacity: 0, scale: 0.96 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.96 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            className="relative w-full max-w-[640px] max-h-[85vh] rounded-[16px] overflow-hidden bg-[var(--bg-primary)] border border-[var(--border-secondary)] flex flex-col shadow-2xl"
            style={{
              boxShadow:
                "0 0 0 1px rgba(0,0,0,0.13), 0 24px 64px 0 rgba(0,0,0,0.35)",
            }}
          >
            {/* Minimal header mirroring the team task detail modal. */}
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

            <div className="flex-1 min-h-0 overflow-y-auto px-[32px] pb-[40px] pt-[4px]">
              <h2
                id="todo-detail-title"
                className="text-[22px] md:text-[26px] font-bold text-[var(--text-primary)] leading-[1.3] mb-[24px] break-words"
              >
                {titleLine}
              </h2>

              {/* Properties sheet */}
              <div className="flex flex-col gap-[14px] mb-[28px]">
                <div className="flex items-center group">
                  <div className="w-[120px] flex-shrink-0 text-[13px] text-[var(--text-tertiary)] flex items-center gap-[8px]">
                    <CircleUserRound className="w-[15px] h-[15px]" />
                    Assignee
                  </div>
                  <div className="flex-1 flex items-center gap-[10px]">
                    <span
                      className={`relative w-[28px] h-[28px] ${circular ? "rounded-full" : "rounded-[8px]"} flex items-center justify-center text-[16px] leading-none select-none`}
                      style={{
                        backgroundColor: avatarColor,
                        color: "var(--text-tertiary)",
                        border:
                          isUnassigned || isClassifying
                            ? "1px dashed var(--border-secondary)"
                            : undefined,
                      }}
                      aria-hidden
                    >
                      {isUnassigned || isClassifying ? (
                        <CircleUserRound size={16} />
                      ) : (
                        ownerEmoji
                      )}
                    </span>
                    <div className="flex flex-col">
                      <span className="text-[13.5px] text-[var(--text-primary)]">
                        {isUnassigned
                          ? "Unassigned"
                          : isClassifying
                            ? "Classifying…"
                            : ownerAvatarName}
                      </span>
                      {!isUnassigned && !isClassifying && (
                        <span className="text-[11.5px] text-[var(--text-tertiary)]">
                          {ownerSubtitle}
                        </span>
                      )}
                    </div>
                  </div>
                </div>

                <div className="flex items-center group">
                  <div className="w-[120px] flex-shrink-0 text-[13px] text-[var(--text-tertiary)] flex items-center gap-[8px]">
                    <Clock className="w-[15px] h-[15px]" />
                    Status
                  </div>
                  <div className="flex-1 flex items-center gap-[10px]">
                    <TaskStatusBadge status={task.status} />
                    {runningElapsedLabel && (
                      <span
                        className="inline-flex items-center gap-1 text-[12px] tabular-nums"
                        style={{ color: "var(--text-tertiary)" }}
                        aria-label={`Running for ${runningElapsedLabel}`}
                      >
                        <Clock className="w-[13px] h-[13px]" />
                        {runningElapsedLabel}
                      </span>
                    )}
                  </div>
                </div>
              </div>

              {task.prompt.trim() && (
                <div className="mb-[28px]">
                  <div className="text-[14.5px] font-semibold text-[var(--text-primary)] flex items-center gap-[8px] mb-[10px]">
                    <AlignLeft className="w-[16px] h-[16px] text-[var(--text-secondary)]" />
                    Prompt
                  </div>
                  <div className="text-[13.5px] text-[var(--text-secondary)] leading-[1.6] whitespace-pre-wrap break-words pl-[24px]">
                    {task.prompt}
                  </div>
                </div>
              )}

              {task.status === "failed" && (
                <div className="mb-[28px]">
                  <div className="text-[14.5px] font-semibold text-[var(--text-primary)] flex items-center gap-[8px] mb-[10px]">
                    <AlertTriangle className="w-[16px] h-[16px] text-[rgb(190,18,60)]" />
                    Failure log
                  </div>
                  <div className="pl-[24px]">
                    {task.error_log.length === 0 ? (
                      <p className="text-[13px] italic text-[var(--text-tertiary)]">
                        No error reason was recorded.
                      </p>
                    ) : (
                      <ul className="flex flex-col gap-[8px] list-none p-0 m-0">
                        {task.error_log.map((entry, i) => (
                          <li
                            key={i}
                            className="text-[12.5px] font-mono px-[12px] py-[8px] rounded-[8px] whitespace-pre-wrap break-words"
                            style={{
                              backgroundColor: "rgba(244,63,94,0.06)",
                              color: "var(--text-secondary)",
                              border: "1px solid rgba(244,63,94,0.18)",
                            }}
                          >
                            {entry}
                          </li>
                        ))}
                      </ul>
                    )}
                  </div>
                </div>
              )}

              {(task.attachments ?? []).length > 0 && (
                <div className="mb-[28px]">
                  <div className="text-[14.5px] font-semibold text-[var(--text-primary)] flex items-center gap-[8px] mb-[10px]">
                    <Paperclip className="w-[16px] h-[16px] text-[var(--text-secondary)]" />
                    Attachments
                  </div>
                  <div className="flex flex-wrap gap-2 pl-[24px]">
                    {(task.attachments ?? []).map((a) => (
                      <div
                        key={a.id}
                        className="flex items-center gap-1.5 h-[26px] pl-1.5 pr-2.5 rounded-[6px] border max-w-[220px]"
                        style={{
                          borderColor: "var(--border-primary)",
                          backgroundColor: "var(--bg-secondary)",
                        }}
                      >
                        {a.attachment_type === "image" ? (
                          <ImageIcon
                            size={11}
                            className="shrink-0"
                            style={{ color: "var(--text-secondary)" }}
                          />
                        ) : (
                          <FileIcon
                            size={11}
                            className="shrink-0"
                            style={{ color: "var(--text-secondary)" }}
                          />
                        )}
                        <span
                          className="truncate text-[11px]"
                          style={{ color: "var(--text-primary)" }}
                          title={a.original_filename}
                        >
                          {a.original_filename}
                        </span>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              {task.attempt_count > 0 && (
                <div className="text-[11.5px] text-[var(--text-tertiary)] pl-[24px]">
                  Attempt count: {task.attempt_count}
                </div>
              )}
            </div>
          </motion.div>
        </div>
      )}
    </AnimatePresence>,
    document.body,
  );
}

// ---------------------------------------------------------------------------
// More-actions kebab — mirrors the team panel's TasklistMoreMenu so the agent
// surface gains the same affordance shape. Today the agent tasklist API only
// exposes `stop`, so the menu has a single destructive item; the slot is
// structured so future actions (pause/resume, replay, continue) drop in as
// menu items without further header layout churn.
// ---------------------------------------------------------------------------

interface TodoMoreMenuProps {
  agentId: string;
  tasklist: Tasklist;
  onAfterAction: () => void;
}

function TodoMoreMenu({ agentId, tasklist, onAfterAction }: TodoMoreMenuProps) {
  const [open, setOpen] = useState(false);
  const [showStopDialog, setShowStopDialog] = useState(false);
  const [stopError, setStopError] = useState<string | null>(null);
  const wrapperRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!open) return;
    const onMouseDown = (e: MouseEvent) => {
      const node = wrapperRef.current;
      if (node && !node.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, [open]);

  const isActive = tasklist.status === "active";
  const isPaused = tasklist.status === "paused";
  const canStop = isActive || isPaused;

  const inFlightTaskCount = tasklist.groups
    .flatMap((g) => g.tasks)
    .filter((t) => t.status === "in_progress").length;

  const itemClass =
    "w-full flex items-center gap-2 px-3 py-[7px] text-[12.5px] text-left disabled:opacity-50 disabled:cursor-not-allowed hover:bg-[var(--bg-hover)] transition-colors cursor-pointer";

  return (
    <div ref={wrapperRef} className="relative shrink-0">
      <Tooltip placement="top" label="More options">
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          aria-label="More options"
          aria-expanded={open}
          className="w-[28px] h-[28px] rounded-[8px] flex items-center justify-center hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
          style={{ color: "var(--text-tertiary)" }}
        >
          <span className="text-[18px] tracking-tighter leading-none">⋯</span>
        </button>
      </Tooltip>
      {open && (
        <div
          className="absolute right-0 mt-1 z-20 min-w-[200px] rounded-[10px] border py-1 shadow-lg"
          style={{
            backgroundColor: "var(--bg-secondary)",
            borderColor: "var(--border-primary)",
          }}
          role="menu"
        >
          {canStop ? (
            <button
              type="button"
              onClick={() => {
                setOpen(false);
                setStopError(null);
                setShowStopDialog(true);
              }}
              className={itemClass}
              style={{ color: "var(--text-primary)" }}
              role="menuitem"
            >
              <Trash2 size={13} />
              Stop tasklist
            </button>
          ) : (
            <div
              className="px-3 py-[7px] text-[12px] italic"
              style={{ color: "var(--text-tertiary)" }}
            >
              No actions available
            </div>
          )}
        </div>
      )}

      <ConfirmDialog
        open={showStopDialog}
        title="Stop tasklist?"
        message={
          <div className="space-y-2">
            <p>
              Mark the tasklist as <strong>cancelled</strong> and stop the
              feeder. Pending tasks will be skipped.
            </p>
            {inFlightTaskCount > 0 && (
              <p className="text-[12px] opacity-80">
                {inFlightTaskCount} in-flight{" "}
                {inFlightTaskCount === 1 ? "task is" : "tasks are"} mid-run.
                The current turn will finish naturally; nothing new will be
                dispatched.
              </p>
            )}
            <p className="text-[12px] opacity-80">This can&apos;t be undone.</p>
            {stopError && (
              <p className="text-[12px]" style={{ color: "#be123c" }}>
                {stopError}
              </p>
            )}
          </div>
        }
        confirmLabel="Stop"
        destructive
        onConfirm={async () => {
          try {
            await stopAgentTasklist(agentId, tasklist.id);
            setShowStopDialog(false);
            onAfterAction();
          } catch (err) {
            setStopError(err instanceof Error ? err.message : String(err));
            throw err;
          }
        }}
        onCancel={() => {
          setShowStopDialog(false);
          setStopError(null);
        }}
      />
    </div>
  );
}

export function TodoPanel({ agentId }: TodoPanelProps) {
  const { active, loading, error } = useAgentTasklistsForAgent(agentId);
  const hydrate = useAgentTasklistStore((s) => s.hydrate);
  const profile = useChatStore((s) => s.selectedAgentProfile);
  const agents = useChatStore((s) => s.agents);
  const circularAvatars = useUserPreferencesStore((s) => s.circularAvatars);

  // Subscribe to the tasklist-scoped channel for per-task subagent run events.
  // These events are isolated from the parent agent's main chat
  // channel onto tasklist:{id}; this hook keeps the panel live without letting
  // subagent stdout bleed into the chat view.
  useAgentTasklistRunSSE(agentId, active?.id ?? null);

  const draftKey = `todo:${agentId}`;
  const setDraft = useDraftStore((s) => s.setDraft);
  const clearDraft = useDraftStore((s) => s.clearDraft);
  const [composerText, setComposerText] = useState(
    () => useDraftStore.getState().drafts[`todo:${agentId}`] ?? ""
  );
  const [composerMode, setComposerMode] = useState<"SEQ" | "PAR">("SEQ");
  const [sending, setSending] = useState(false);
  const [sendError, setSendError] = useState<string | null>(null);

  // Pending attachment state. Each entry tracks one upload from file selection
  // through resolution; only `uploaded` entries contribute their server id to
  // the appendTask request.
  type TaskPendingAttachment = {
    id: string;
    file: File | null;
    previewUrl: string | null;
    status: "uploading" | "uploaded" | "error";
    serverId: string | null;
    attachment: Attachment | null;
  };
  const [pendingAttachments, setPendingAttachments] = useState<
    TaskPendingAttachment[]
  >([]);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const hasUploading = pendingAttachments.some((p) => p.status === "uploading");
  const uploadedAttachmentIds = useMemo(
    () =>
      pendingAttachments
        .filter((p) => p.status === "uploaded" && p.serverId)
        .map((p) => p.serverId as string),
    [pendingAttachments],
  );

  const [skipping, setSkipping] = useState<string | null>(null);
  const [skipError, setSkipError] = useState<string | null>(null);

  // User-driven draft lifecycle: `creating` covers the empty-state "Create
  // todo list" action (POSTs an empty Paused shell); `starting` covers the
  // "Start" action that commits a drafted list (Paused→Active).
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string | null>(null);

  const [showCompleted, setShowCompleted] = useState(true);
  const [openTaskId, setOpenTaskId] = useState<string | null>(null);

  // Auto-scroll the body to the bottom when a task appends. Tracks
  // (tasklistId, totalCount) so switching tasklists rebaselines without
  // scrolling.
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const prevCountRef = useRef<{ tasklistId: string | null; count: number }>({
    tasklistId: null,
    count: 0,
  });

  useEffect(() => {
    void hydrate(agentId);
  }, [agentId, hydrate]);

  const delegatesTo = profile?.delegates_to ?? [];
  const hasDelegate = delegatesTo.length > 0;
  const selfName = profile?.name ?? "Self";
  const selfEmoji = profile?.emoji ?? FALLBACK_OWNER_EMOJI;

  // agent_id → emoji map sourced from the chat store's loaded agent
  // snapshots. The self agent's emoji also comes from the snapshot list when
  // present so the avatar matches the chat header. Falls back to the
  // FALLBACK_OWNER_EMOJI when no snapshot is loaded for that id.
  const agentEmojiFor = useCallback(
    (ownerId: string): string => {
      if (!ownerId) return FALLBACK_OWNER_EMOJI;
      if (ownerId === agentId) return selfEmoji;
      const snap = agents.find((a) => a.agent_id === ownerId);
      return snap?.emoji ?? FALLBACK_OWNER_EMOJI;
    },
    [agents, agentId, selfEmoji],
  );

  const agentName = useCallback(
    (ownerId: string): string => {
      if (!ownerId) return "Coordinator";
      if (ownerId === agentId) return selfName;
      const delegate = delegatesTo.find((d) => d.target_agent_id === ownerId);
      if (delegate) return delegate.name;
      const snap = agents.find((a) => a.agent_id === ownerId);
      return snap?.name ?? ownerId;
    },
    [agentId, selfName, delegatesTo, agents],
  );

  const handleSend = useCallback(async () => {
    if (!composerText.trim() || !active || sending || hasUploading) return;
    setSending(true);
    setSendError(null);
    try {
      await appendAgentTask(agentId, active.id, {
        prompt: composerText.trim(),
        mode: composerMode,
        owner_agent_id: hasDelegate ? undefined : agentId,
        ...(uploadedAttachmentIds.length > 0
          ? { attachment_ids: uploadedAttachmentIds }
          : {}),
      });
      setComposerText("");
      clearDraft(`todo:${agentId}`);
      // Clear attachment chips on successful append. Revoke blob URLs to keep
      // memory tidy.
      for (const p of pendingAttachments) {
        if (p.previewUrl) URL.revokeObjectURL(p.previewUrl);
      }
      setPendingAttachments([]);
      void hydrate(agentId);
    } catch (err) {
      setSendError(err instanceof Error ? err.message : String(err));
    } finally {
      setSending(false);
    }
  }, [
    composerText,
    active,
    sending,
    hasUploading,
    agentId,
    composerMode,
    hasDelegate,
    uploadedAttachmentIds,
    pendingAttachments,
    hydrate,
    clearDraft,
  ]);

  // Upload selected files to the agent attachment store, tracking each entry
  // through `uploading → uploaded` (or `error`). The Send button is blocked
  // while uploads are in flight via `hasUploading`.
  const handleFileSelect = useCallback(
    async (files: FileList | null) => {
      if (!files || files.length === 0) return;
      const newPending: TaskPendingAttachment[] = Array.from(files).map(
        (file) => ({
          id: `pending-${Date.now()}-${Math.random().toString(36).slice(2, 9)}`,
          file,
          previewUrl: file.type.startsWith("image/")
            ? URL.createObjectURL(file)
            : null,
          status: "uploading" as const,
          serverId: null,
          attachment: null,
        }),
      );
      setPendingAttachments((prev) => [...prev, ...newPending]);
      for (const pending of newPending) {
        try {
          const attachment = await uploadAttachment(agentId, pending.file as File);
          setPendingAttachments((prev) =>
            prev.map((p) =>
              p.id === pending.id
                ? {
                  ...p,
                  status: "uploaded" as const,
                  serverId: attachment.id,
                  attachment,
                }
                : p,
            ),
          );
        } catch {
          setPendingAttachments((prev) =>
            prev.map((p) =>
              p.id === pending.id ? { ...p, status: "error" as const } : p,
            ),
          );
        }
      }
    },
    [agentId],
  );

  const handleRemoveAttachment = useCallback(
    async (pendingId: string) => {
      const pending = pendingAttachments.find((p) => p.id === pendingId);
      if (!pending) return;
      if (pending.previewUrl) URL.revokeObjectURL(pending.previewUrl);
      // Best-effort server-side delete. Failures are non-fatal — the server GC
      // will reclaim uncommitted assets automatically.
      if (pending.status === "uploaded" && pending.serverId) {
        try {
          await deleteAttachment(agentId, pending.serverId);
        } catch {
          /* ignore */
        }
      }
      setPendingAttachments((prev) => prev.filter((p) => p.id !== pendingId));
    },
    [pendingAttachments, agentId],
  );

  // Empty-state action: draft a new agent-owned list. It's created with no
  // groups so the backend persists it Paused — items are staged via the
  // composer and don't execute until the user hits Start. The list is
  // agent-scoped, so there's no name/description prompt; we auto-title it.
  const handleCreate = useCallback(async () => {
    if (creating) return;
    setCreating(true);
    setCreateError(null);
    try {
      await createAgentTasklist(agentId, {
        title: "Todo list",
        description: "",
        groups: [],
        allow_empty_groups: true,
      });
      void hydrate(agentId);
    } catch (err) {
      setCreateError(err instanceof Error ? err.message : String(err));
    } finally {
      setCreating(false);
    }
  }, [creating, agentId, hydrate]);

  // Commit a drafted list: flip Paused→Active so the feeder classifies and
  // dispatches the staged tasks. No-op guard mirrors the other actions.
  const handleStart = useCallback(async () => {
    if (!active || starting) return;
    setStarting(true);
    setStartError(null);
    try {
      await startAgentTasklist(agentId, active.id);
      void hydrate(agentId);
    } catch (err) {
      setStartError(err instanceof Error ? err.message : String(err));
    } finally {
      setStarting(false);
    }
  }, [active, starting, agentId, hydrate]);

  const handleSkip = useCallback(
    async (taskId: string) => {
      if (!active || skipping) return;
      setSkipping(taskId);
      setSkipError(null);
      try {
        await skipAgentTask(agentId, active.id, taskId);
        void hydrate(agentId);
      } catch (err) {
        setSkipError(err instanceof Error ? err.message : String(err));
      } finally {
        setSkipping(null);
      }
    },
    [active, skipping, agentId, hydrate],
  );

  // Derived view: per-group filtered tasks with the group dropped entirely
  // when "hide completed" empties it. Avoids dangling group headers.
  const visibleGroups = useMemo(() => {
    if (!active) return [];
    return active.groups
      .map((g, idx) => ({
        group: g,
        index: idx,
        tasks: showCompleted
          ? g.tasks
          : g.tasks.filter(
            (t) => t.status !== "completed" && t.status !== "skipped",
          ),
      }))
      .filter((g) => g.tasks.length > 0);
  }, [active, showCompleted]);

  const allTasks = active?.groups.flatMap((g) => g.tasks) ?? [];
  const doneCount = allTasks.filter(
    (t) => t.status === "completed" || t.status === "skipped",
  ).length;
  const totalCount = allTasks.length;
  const totalVisible = visibleGroups.reduce((n, g) => n + g.tasks.length, 0);

  // Drive the live elapsed timers from a single panel-level tick. We tick while
  // any task is running OR while the list itself is active — the latter keeps
  // the list-total timer advancing across SEQ dispatch gaps (when no single
  // task is momentarily in_progress). An idle/terminal list never ticks.
  const hasRunningTask = allTasks.some((t) => t.status === "in_progress");
  const listIsRunning = active?.status === "active";
  const now = useNow(hasRunningTask || listIsRunning);

  // Total wall-clock time the list has been (or was) running, from the
  // backend's created_at anchor. Live while active; frozen at last_active_at
  // once the list stops. Hidden for a pristine paused draft that never ran
  // (all tasks still pending) so the header doesn't show a meaningless "0s".
  const listHasStarted =
    !!active &&
    (active.status === "active" ||
      active.status === "completed" ||
      active.status === "failed" ||
      active.status === "cancelled" ||
      allTasks.some((t) => t.status !== "pending"));
  let listElapsedLabel: string | null = null;
  if (active && listHasStarted) {
    const ms = computeTasklistElapsedMs(
      active.created_at,
      active.last_active_at,
      listIsRunning,
      now,
    );
    if (ms != null) listElapsedLabel = formatRunElapsed(ms);
  }

  // Resolve the task object currently surfaced in the detail modal. We look
  // it up against the live tasklist so SSE-driven status flips update the
  // modal in real time without a manual re-open.
  const openTask = useMemo(() => {
    if (!openTaskId || !active) return null;
    for (const g of active.groups) {
      const found = g.tasks.find((t) => t.id === openTaskId);
      if (found) return found;
    }
    return null;
  }, [openTaskId, active]);

  // Owner display strings for the detail modal — pre-computed at the panel
  // level since the modal itself is render-only.
  const openTaskOwnerInfo = useMemo(() => {
    if (!openTask) {
      return { name: "", emoji: FALLBACK_OWNER_EMOJI, subtitle: "Unassigned" };
    }
    const mode = ownerChipMode(openTask);
    const label = resolveOwnerDisplayName(
      openTask,
      agentId,
      selfName,
      delegatesTo,
    );
    const ownerIdForAvatar =
      openTask.assignment?.owner_agent_id || openTask.owner_agent_id || "";
    const emoji = ownerIdForAvatar
      ? agentEmojiFor(ownerIdForAvatar)
      : FALLBACK_OWNER_EMOJI;

    if (mode === "classifying") {
      return { name: "", emoji, subtitle: "Classifying…" };
    }
    if (label) {
      return {
        name: label,
        emoji,
        subtitle: mode === "pinned" ? "Pinned owner" : "Classifier picked",
      };
    }
    if (openTask.owner_agent_id) {
      return {
        name: agentName(openTask.owner_agent_id),
        emoji,
        subtitle: "Legacy owner",
      };
    }
    return { name: "", emoji, subtitle: "Unassigned" };
  }, [openTask, agentId, selfName, delegatesTo, agentEmojiFor, agentName]);

  // Live "running for…" label for the task currently open in the detail modal.
  // Recomputed each tick; null unless the open task is in flight with a known
  // origin.
  let openTaskElapsedLabel: string | null = null;
  if (active && openTask && openTask.status === "in_progress") {
    const start = getTaskRunStart(active.id, openTask.id);
    if (start != null) openTaskElapsedLabel = formatRunElapsed(now - start);
  }

  useEffect(() => {
    const tasklistId = active?.id ?? null;
    const prev = prevCountRef.current;
    if (prev.tasklistId === tasklistId && totalCount > prev.count) {
      const el = bodyRef.current;
      if (el) {
        requestAnimationFrame(() => {
          el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
        });
      }
    }
    prevCountRef.current = { tasklistId, count: totalCount };
  }, [totalCount, active?.id]);

  if (loading && !active) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <Loader2 className="w-[20px] h-[20px] animate-spin text-[var(--text-tertiary)]" />
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex flex-1 items-center justify-center px-[16px]">
        <span className="text-[12px] text-red-500 text-center">{error}</span>
      </div>
    );
  }

  if (!active) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-[10px] px-[16px] text-center">
        <ListTodo size={40} />

        <span className="text-[13px] text-[var(--text-secondary)]">
          No active todos
        </span>
        {/* <span className="text-[11px] text-[var(--text-tertiary)] max-w-[260px]">
          Create a todo list, add tasks (sequential or parallel), then start it
          — or let the agent create one when it needs to manage work items.
        </span> */}
        <button
          type="button"
          onClick={() => void handleCreate()}
          disabled={creating}
          className="mt-[4px] flex items-center gap-1.5 h-[30px] px-3 rounded-[8px] text-[12px] font-medium cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          style={{
            backgroundColor: "var(--text-primary)",
            color: "var(--bg-primary)",
          }}
        >
          {creating ? (
            <Loader2 size={13} className="animate-spin" />
          ) : (
            <ListPlus size={13} />
          )}
          Create todo list
        </button>
        {createError && (
          <span className="text-[11px] text-red-500 max-w-[260px]">
            {createError}
          </span>
        )}
      </div>
    );
  }

  const composerVisible =
    active.status === "active" || active.status === "paused";
  const modeLabel = composerMode === "SEQ" ? "Sequential" : "Parallel";
  const ModeIcon = composerMode === "SEQ" ? Rows2 : Columns2;

  // A Paused list with runnable tasks is a draft the user hasn't committed yet.
  // Surface a Start CTA above the composer to flip it Active and kick the feeder.
  const pendingCount = allTasks.filter(
    (t) => t.status === "pending" || t.status === "blocked",
  ).length;
  const showStart = active.status === "paused" && pendingCount > 0;

  return (
    <div className="flex flex-col h-full overflow-hidden">
      {/* Inner header: title + status pill + kebab menu. The outer side panel
       *  already provides the rounded card frame + "Todos" label, so this
       *  header sits flush inside it. Kebab mirrors the team panel's
       *  TasklistMoreMenu so actions live in a consistent slot across the
       *  two surfaces. */}
      <div className="px-4 pt-3 pb-2 shrink-0">
        <div className="flex items-start justify-between gap-2">
          <div className="flex items-center gap-2 min-w-0 flex-1">
            <Tooltip
              placement="top"
              label={active.title || "Untitled tasklist"}
              className="min-w-0"
            >
              <h3
                className="text-[15px] font-semibold truncate"
                style={{ color: "var(--text-primary)" }}
              >
                {active.title || "Untitled tasklist"}
              </h3>
            </Tooltip>
            <TasklistStatusPill status={active.status} />
          </div>
          <TodoMoreMenu
            agentId={agentId}
            tasklist={active}
            onAfterAction={() => void hydrate(agentId)}
          />
        </div>
      </div>

      {/* Progress + show-completed toggle — banded edge-to-edge so the count
       *  acts as a visual anchor between the title and the task body. */}
      <div
        className="px-4 py-2.5 flex items-center justify-between shrink-0 border-y"
        style={{ borderColor: "var(--border-primary)" }}
      >
        <div className="flex items-center gap-2 min-w-0">
          <span
            className="text-[13px] font-semibold tabular-nums"
            style={{ color: "var(--text-primary)" }}
          >
            {doneCount}/{totalCount}
          </span>
          <span
            className="text-[12px]"
            style={{ color: "var(--text-secondary)" }}
          >
            Tasks
          </span>
          {/* List-total run timer — how long the whole list has been (or was)
           *  running. Live while active (ticks each second), frozen at the
           *  final total once the list stops. Backed by the backend's
           *  created_at / last_active_at, so it stays accurate across reloads. */}
          {listElapsedLabel && (
            <Tooltip
              placement="top"
              label={listIsRunning ? "Running for" : "Total run time"}
            >
              <span
                className="inline-flex items-center gap-1 px-[8px] h-[20px] rounded-full text-[10.5px] font-medium tabular-nums"
                style={{
                  backgroundColor: "var(--bg-tertiary)",
                  color: "var(--text-secondary)",
                }}
                aria-label={`${listIsRunning ? "Running for" : "Ran for"} ${listElapsedLabel}`}
              >
                <Clock size={10} strokeWidth={2.5} />
                {listElapsedLabel}
              </span>
            </Tooltip>
          )}
        </div>
        <button
          type="button"
          onClick={() => setShowCompleted((v) => !v)}
          className="flex items-center gap-2 group cursor-pointer"
        >
          <span
            className="relative inline-flex h-[18px] w-[30px] rounded-full transition-colors"
            style={{
              backgroundColor: showCompleted
                ? "var(--text-primary)"
                : "var(--bg-tertiary)",
            }}
          >
            <span
              className="absolute top-[2px] w-[14px] h-[14px] rounded-full bg-white transition-all"
              style={{
                left: showCompleted ? "14px" : "2px",
                boxShadow: "0 1px 2px rgba(0,0,0,0.15)",
              }}
            />
          </span>
          <span
            className="text-[11.5px]"
            style={{ color: "var(--text-secondary)" }}
          >
            Show completed
          </span>
        </button>
      </div>

      {/* Body */}
      <div
        ref={bodyRef}
        className="flex-1 min-h-0 overflow-y-auto pb-2 custom-scrollbar"
      >
        {visibleGroups.map(({ group, index, tasks }, displayIdx) => (
          <TaskGroupSection
            key={group.id}
            // Pass the filtered group so children see only visible tasks.
            group={{ ...group, tasks }}
            groupIndex={index}
            isFirstVisible={displayIdx === 0}
            tasklistId={active.id}
            now={now}
            agentId={agentId}
            agentName={agentName}
            agentEmojiFor={agentEmojiFor}
            delegateTargets={delegatesTo}
            selfName={selfName}
            circularAvatars={circularAvatars}
            onSkip={handleSkip}
            skipping={skipping}
            onOpenDetail={setOpenTaskId}
          />
        ))}
        {totalVisible === 0 && (
          <div
            className="text-center text-[12px] py-6 px-5 italic"
            style={{ color: "var(--text-tertiary)" }}
          >
            {showCompleted ? "No tasks yet." : "Nothing left to do."}
          </div>
        )}
        {skipError && (
          <div className="mx-4 mt-2 text-[11px] text-red-500">{skipError}</div>
        )}
      </div>

      {/* Start CTA — shown only while the list is a Paused draft with runnable
       *  tasks. Committing it flips Paused→Active so the feeder classifies and
       *  dispatches the staged items. Sits above the composer so the user can
       *  keep adding tasks right up until they commit. */}
      {showStart && (
        <div
          className="shrink-0 px-3 pt-2 flex flex-col gap-1.5"
          style={{ backgroundColor: "var(--bg-secondary)" }}
        >
          <button
            type="button"
            onClick={() => void handleStart()}
            disabled={starting}
            className="flex items-center justify-center gap-2 h-[34px] w-full rounded-[8px] text-[13px] font-semibold cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
            style={{ backgroundColor: "#006E51", color: "#ffffff" }}
          >
            {starting ? (
              <Loader2 size={14} className="animate-spin" />
            ) : (
              <Play size={14} />
            )}
            Start {pendingCount} {pendingCount === 1 ? "task" : "tasks"}
          </button>
          {startError && (
            <span className="text-[11px] text-red-500 px-1">{startError}</span>
          )}
        </div>
      )}

      {/* Composer — rounded container matching the team tasklist composer.
       *  Row 0 (conditional): pending attachment chips.
       *  Row 1: prompt textarea.
       *  Row 2: paperclip + owner hint (left) | mode toggle + send (right). */}
      {composerVisible && (
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void handleSend();
          }}
          className="shrink-0 px-3 pb-3 pt-2"
          style={{ backgroundColor: "var(--bg-secondary)" }}
        >
          {/* Hidden file input wired to the paperclip button. */}
          <input
            ref={fileInputRef}
            type="file"
            multiple
            className="hidden"
            onChange={(e) => {
              void handleFileSelect(e.target.files);
              e.target.value = "";
            }}
          />

          <div
            className="rounded-[12px] border flex flex-col"
            style={{
              backgroundColor: "var(--bg-input)",
              borderColor: "var(--border-primary)",
            }}
          >
            {/* Pending attachments strip — only shown while there are entries. */}
            {pendingAttachments.length > 0 && (
              <div className="flex flex-wrap gap-1.5 px-3 pt-2.5">
                {pendingAttachments.map((pa) => {
                  const isImage =
                    pa.attachment?.attachment_type === "image" ||
                    pa.file?.type.startsWith("image/");
                  const label =
                    pa.attachment?.original_filename ?? pa.file?.name ?? "file";
                  return (
                    <Tooltip key={pa.id} placement="top" label={label}>
                      <div
                        className="relative group flex items-center gap-1.5 h-[26px] pl-1.5 pr-1 rounded-[6px] border max-w-[180px]"
                        style={{
                          borderColor:
                            pa.status === "error"
                              ? "rgba(239,68,68,0.6)"
                              : "var(--border-primary)",
                          backgroundColor: "var(--bg-secondary)",
                        }}
                      >
                        {pa.status === "uploading" ? (
                          <Loader2
                            size={11}
                            className="shrink-0 animate-spin"
                            style={{ color: "var(--text-secondary)" }}
                          />
                        ) : pa.status === "error" ? (
                          <AlertCircle
                            size={11}
                            className="shrink-0"
                            style={{ color: "#ef4444" }}
                          />
                        ) : isImage && pa.previewUrl ? (
                          <img
                            src={pa.previewUrl}
                            alt={label}
                            className="shrink-0 w-[16px] h-[16px] rounded-[3px] object-cover"
                          />
                        ) : isImage ? (
                          <ImageIcon
                            size={11}
                            className="shrink-0"
                            style={{ color: "var(--text-secondary)" }}
                          />
                        ) : (
                          <FileIcon
                            size={11}
                            className="shrink-0"
                            style={{ color: "var(--text-secondary)" }}
                          />
                        )}
                        <span
                          className="truncate text-[11px]"
                          style={{ color: "var(--text-primary)" }}
                        >
                          {label}
                        </span>
                        <button
                          type="button"
                          onClick={() => void handleRemoveAttachment(pa.id)}
                          aria-label="Remove attachment"
                          className="shrink-0 w-[16px] h-[16px] flex items-center justify-center rounded-[3px] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
                          style={{ color: "var(--text-secondary)" }}
                        >
                          <X size={10} />
                        </button>
                      </div>
                    </Tooltip>
                  );
                })}
              </div>
            )}

            <div className="px-3 pt-3 pb-2">
              <textarea
                value={composerText}
                onChange={(e) => {
                  setComposerText(e.target.value);
                  setDraft(draftKey, e.target.value);
                }}
                placeholder="What's the next task?"
                rows={2}
                className="w-full text-[13px] text-[var(--text-primary)] bg-transparent outline-none resize-none placeholder:text-[var(--text-tertiary)]"
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    void handleSend();
                  }
                  // Shift+Space toggles SEQ/PAR — matches the team composer.
                  if (e.shiftKey && e.key === " ") {
                    e.preventDefault();
                    setComposerMode((m) => (m === "SEQ" ? "PAR" : "SEQ"));
                  }
                }}
              />
            </div>
            <div className="flex items-center justify-between px-2 pb-2 pt-1 gap-2">
              <div className="flex items-center gap-1 shrink-0">
                <Tooltip placement="top" label="Attach files">
                  <button
                    type="button"
                    onClick={() => fileInputRef.current?.click()}
                    disabled={sending}
                    aria-label="Attach files"
                    className="flex items-center justify-center h-[28px] w-[28px] rounded-[6px] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    <Paperclip size={14} />
                  </button>
                </Tooltip>
                <span
                  className="text-[11.5px] px-2"
                  style={{ color: "var(--text-tertiary)" }}
                >
                  {hasDelegate ? "Coordinator picks" : selfName}
                </span>
              </div>
              <div className="flex items-center gap-2 min-w-0">
                {sendError && (
                  <Tooltip placement="top" label={sendError} className="min-w-0">
                    <span
                      role="alert"
                      className="block text-[11px] truncate max-w-[160px]"
                      style={{ color: "#be123c" }}
                    >
                      {sendError}
                    </span>
                  </Tooltip>
                )}
                <Tooltip
                  placement="top"
                  label={`Toggle to ${composerMode === "SEQ" ? "Parallel" : "Sequential"} (Shift+Space)`}
                >
                  <button
                    type="button"
                    onClick={() =>
                      setComposerMode((m) => (m === "SEQ" ? "PAR" : "SEQ"))
                    }
                    aria-label="Toggle dispatch mode"
                    className="flex items-center gap-1 h-[26px] px-2 rounded-[6px] text-[11px] font-medium transition-colors cursor-pointer hover:bg-[var(--bg-hover)]"
                    style={{ color: "var(--text-secondary)" }}
                  >
                    <ModeIcon size={11} />
                    {modeLabel}
                  </button>
                </Tooltip>
                <Tooltip placement="top" label={hasUploading ? "Waiting for uploads…" : `Send ${modeLabel}`}>
                  <button
                    type="submit"
                    disabled={sending || hasUploading || !composerText.trim()}
                    aria-label={`Send ${modeLabel}`}
                    className="flex items-center gap-1.5 h-[26px] px-3 rounded-[6px] text-[12px] font-medium cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                    style={{
                      backgroundColor: "var(--text-primary)",
                      color: "var(--bg-primary)",
                    }}
                  >
                    {sending ? (
                      <Loader2 size={11} className="animate-spin" />
                    ) : (
                      <Send size={11} />
                    )}
                    <span className="truncate">Send</span>
                  </button>
                </Tooltip>
              </div>
            </div>
          </div>
        </form>
      )}

      {/* Task detail modal — opens on row click. Renders from the live Task
       *  object so SSE-driven state updates (status flips, owner resolution)
       *  reflect immediately without re-opening. */}
      <TodoDetailModal
        open={!!openTask}
        task={openTask}
        ownerAvatarName={openTaskOwnerInfo.name}
        ownerEmoji={openTaskOwnerInfo.emoji}
        ownerSubtitle={openTaskOwnerInfo.subtitle}
        circular={circularAvatars}
        runningElapsedLabel={openTaskElapsedLabel}
        onClose={() => setOpenTaskId(null)}
      />
    </div>
  );
}
