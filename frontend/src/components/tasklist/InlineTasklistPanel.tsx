import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { motion } from "framer-motion";
import { useEditor, EditorContent, type Editor } from "@tiptap/react";
import { Extension } from "@tiptap/core";
import StarterKit from "@tiptap/starter-kit";
import Placeholder from "@tiptap/extension-placeholder";
import Suggestion from "@tiptap/suggestion";
import {
  Check,
  Loader2,
  Circle,
  AlertTriangle,
  AlertCircle,
  FileText,
  File as FileIcon,
  Image as ImageIcon,
  Paperclip,
  ChevronDown,
  // GripVertical, // re-enable when drag-to-reorder lands
  Pause,
  Play,
  Trash2,
  RotateCcw,
  CircleUserRound,
  MessageSquare,
  Send,
  SkipForward,
  Square,
  Columns2,
  Rows2,
  X,
} from "lucide-react";
import { Tooltip } from "../ui/Tooltip";
import {
  useCurrentAndArchivedTasklistsForScope,
  useTasklistStore,
} from "../../stores/tasklistStore";
import { useTasklistSSE } from "../../hooks/useTasklistSSE";
import type { TasklistScope } from "../../types/api";
import { agentAvatarColor } from "../../lib/agentColors";
import { useUserPreferencesStore } from "../../stores/userPreferencesStore";
import ConfirmDialog from "../ui/ConfirmDialog";
import * as api from "../../lib/api";
import { displayOutputFilename, filterVisibleOutputs } from "../../lib/expectedOutputs";
import { TasklistOutputPortal } from "./TasklistOutputPortal";
import type {
  Attachment,
  Task,
  TaskComment,
  TaskStatus,
  Tasklist,
  TasklistStatus,
  TaskGroupMode,
  TeamMember,
} from "../../types/api";

function StatusCheckbox({
  status,
}: {
  status: TaskStatus;
}) {
  if (status === "completed") {
    return (
      <span
        className="shrink-0 w-[18px] h-[18px] rounded-[6px] flex items-center justify-center"
        style={{
          backgroundColor: "var(--text-primary)",
        }}
      >
        <Check size={12} strokeWidth={3} style={{ color: "var(--bg-primary)" }} />
      </span>
    );
  }
  if (status === "in_progress") {
    // Outline uses --checkbox-border (a per-theme dedicated swatch) rather
    // than --border-secondary so the checkbox stays visible against the
    // muted --bg-tertiary pill background without forcing every other
    // bordered element in the theme to brighten.
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
        <Loader2 size={12} className="animate-spin" style={{ color: "var(--text-secondary)" }} />
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
  if (status === "stopped") {
    // Halted mid-flight by the user/agent — non-terminal, resumable. A small
    // filled square (the universal "stop" glyph) inside the standard outline.
    return (
      <span
        className="shrink-0 w-[18px] h-[18px] rounded-[6px] border flex items-center justify-center"
        style={{ borderColor: "var(--checkbox-border)" }}
      >
        <Square
          size={8}
          fill="currentColor"
          style={{ color: "var(--text-tertiary)" }}
        />
      </span>
    );
  }
  // Default (pending / unassigned) — see the in_progress branch above for
  // why the outline pulls from --checkbox-border instead of --border-secondary.
  return (
    <span
      className="shrink-0 w-[18px] h-[18px] rounded-[6px] border"
      style={{ borderColor: "var(--checkbox-border)" }}
    />
  );
}

/** Inline Resume call-to-action shown when the tasklist is Paused with at
 *  least one Pending task. Surfaces the Resume action prominently next to
 *  the status pill so revived-from-terminal tasklists are
 *  one click from running, instead of buried in the kebab menu. */
function ResumeButton({
  scope,
  tasklistId,
}: {
  scope: TasklistScope;
  tasklistId: string;
}) {
  const setStatus = useTasklistStore((s) => s.setTasklistStatus);
  const [busy, setBusy] = useState(false);
  const handleClick = async () => {
    if (busy) return;
    setBusy(true);
    try {
      await setStatus(scope, tasklistId, "active");
    } catch (err) {
      console.warn("[InlineTasklistPanel] resume failed", err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <button
      type="button"
      onClick={handleClick}
      disabled={busy}
      aria-label="Resume tasklist"
      className="flex items-center gap-1 h-[20px] px-[8px] rounded-full text-[11px] font-medium transition-colors cursor-pointer disabled:opacity-60 disabled:cursor-not-allowed"
      style={{
        backgroundColor: "rgba(16,185,129,0.14)",
        color: "rgb(5,150,105)",
      }}
    >
      {busy ? (
        <Loader2 size={10} className="animate-spin" />
      ) : (
        <Play size={10} />
      )}
      Resume
    </button>
  );
}

function TasklistStatusPill({ status }: { status: TasklistStatus }) {
  const config: Record<TasklistStatus, { label: string; bg: string; fg: string }> = {
    active: { label: "active", bg: "rgba(59,130,246,0.12)", fg: "rgb(37,99,235)" },
    paused: { label: "paused", bg: "rgba(217,119,6,0.14)", fg: "rgb(180,83,9)" },
    completed: { label: "completed", bg: "rgba(16,185,129,0.12)", fg: "rgb(5,150,105)" },
    failed: { label: "failed", bg: "rgba(244,63,94,0.12)", fg: "rgb(190,18,60)" },
    cancelled: { label: "cancelled", bg: "var(--bg-tertiary)", fg: "var(--text-tertiary)" },
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

function TasklistMoreMenu({
  scope,
  tasklist,
}: {
  scope: TasklistScope;
  tasklist: Tasklist | null;
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const wrapperRef = useRef<HTMLDivElement | null>(null);
  const setStatus = useTasklistStore((s) => s.setTasklistStatus);
  const continueTasklist = useTasklistStore((s) => s.continueTasklist);
  const discardTasklist = useTasklistStore((s) => s.discardTasklist);
  const replayTasklist = useTasklistStore((s) => s.replayTasklist);

  const [showContinueDialog, setShowContinueDialog] = useState(false);
  const [continueError, setContinueError] = useState<string | null>(null);
  const [showDiscardDialog, setShowDiscardDialog] = useState(false);
  const [discardError, setDiscardError] = useState<string | null>(null);
  const [showReplayDialog, setShowReplayDialog] = useState(false);
  const [replayError, setReplayError] = useState<string | null>(null);

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

  const isActive = tasklist?.status === "active";
  const isPaused = tasklist?.status === "paused";
  const isFailed = tasklist?.status === "failed";
  const isCompleted = tasklist?.status === "completed";
  const isCancelled = tasklist?.status === "cancelled";
  const canDiscard = isActive || isPaused || isFailed;
  const canReplay = isCompleted || isFailed || isCancelled;

  const allTasks = tasklist?.groups.flatMap((g) => g.tasks) ?? [];
  const failedTaskCount = allTasks.filter((t) => t.status === "failed").length;
  const inFlightTaskCount = allTasks.filter(
    (t) => t.status === "in_progress",
  ).length;

  const handlePauseResume = async () => {
    if (!tasklist) return;
    if (!isActive && !isPaused) return;
    setOpen(false);
    setBusy(true);
    try {
      await setStatus(scope, tasklist.id, isActive ? "paused" : "active");
    } catch (err) {
      console.warn("[InlineTasklistPanel] pause/resume failed", err);
    } finally {
      setBusy(false);
    }
  };

  const itemClass =
    "w-full flex items-center gap-2 px-3 py-[7px] text-[12.5px] text-left disabled:opacity-50 disabled:cursor-not-allowed hover:bg-[var(--bg-hover)] transition-colors";

  return (
    <div ref={wrapperRef} className="relative shrink-0">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-label="More options"
        aria-expanded={open}
        className="w-[28px] h-[28px] rounded-[8px] flex items-center justify-center hover:bg-[var(--bg-hover)] transition-colors"
        style={{ color: "var(--text-tertiary)" }}
      >
        <span className="text-[18px] tracking-tighter leading-none">⋯</span>
      </button>
      {open && (
        <div
          className="absolute right-0 mt-1 z-20 min-w-[200px] rounded-[10px] border py-1 shadow-lg"
          style={{
            backgroundColor: "var(--bg-secondary)",
            borderColor: "var(--border-primary)",
          }}
          role="menu"
        >
          {tasklist && (isActive || isPaused) && (
            <button
              type="button"
              onClick={handlePauseResume}
              disabled={busy}
              className={itemClass}
              style={{ color: "var(--text-primary)" }}
              role="menuitem"
            >
              {isActive ? (
                <>
                  <Pause size={13} />
                  Pause tasklist
                </>
              ) : (
                <>
                  <Play size={13} />
                  Resume tasklist
                </>
              )}
            </button>
          )}
          {isFailed && (
            <button
              type="button"
              onClick={() => {
                setOpen(false);
                setContinueError(null);
                setShowContinueDialog(true);
              }}
              className={itemClass}
              style={{ color: "var(--text-primary)" }}
              role="menuitem"
            >
              <Play size={13} />
              Continue tasklist
            </button>
          )}
          {canDiscard && (
            <button
              type="button"
              onClick={() => {
                setOpen(false);
                setDiscardError(null);
                setShowDiscardDialog(true);
              }}
              className={itemClass}
              style={{ color: "var(--text-primary)" }}
              role="menuitem"
            >
              <Trash2 size={13} />
              Discard tasklist
            </button>
          )}
          {canReplay && (
            <button
              type="button"
              onClick={() => {
                setOpen(false);
                setReplayError(null);
                setShowReplayDialog(true);
              }}
              className={itemClass}
              style={{ color: "var(--text-primary)" }}
              role="menuitem"
            >
              <RotateCcw size={13} />
              Replay tasklist
            </button>
          )}
        </div>
      )}

      <ConfirmDialog
        open={showContinueDialog}
        title="Continue tasklist?"
        message={
          <div className="space-y-2">
            <p>
              Retry {failedTaskCount} failed{" "}
              {failedTaskCount === 1 ? "task" : "tasks"} from where the
              tasklist stopped. The tasklist will go back to Active and the
              failed tasks will reset to Pending with a fresh attempt count.
            </p>
            <p className="text-[12px] opacity-80">
              Use this after fixing the underlying cause (permissions, missing
              skill, broken script, etc.).
            </p>
            {continueError && (
              <p className="text-[12px]" style={{ color: "#be123c" }}>
                {continueError}
              </p>
            )}
          </div>
        }
        confirmLabel="Continue"
        onConfirm={async () => {
          if (!tasklist) return;
          try {
            await continueTasklist(scope, tasklist.id);
            setShowContinueDialog(false);
          } catch (err) {
            setContinueError(err instanceof Error ? err.message : String(err));
            throw err;
          }
        }}
        onCancel={() => {
          setShowContinueDialog(false);
          setContinueError(null);
        }}
      />

      <ConfirmDialog
        open={showDiscardDialog}
        title="Discard tasklist?"
        message={
          <div className="space-y-2">
            <p>
              Mark the tasklist as <strong>cancelled</strong> and stop the
              feeder. Pending tasks will be skipped, and the tasklist will move
              to the archive.
            </p>
            {inFlightTaskCount > 0 && (
              <p className="text-[12px] opacity-80">
                {inFlightTaskCount} in-flight{" "}
                {inFlightTaskCount === 1 ? "task is" : "tasks are"} mid-run.
                The agent will finish its current turn naturally; nothing new
                will be dispatched.
              </p>
            )}
            <p className="text-[12px] opacity-80">This can&apos;t be undone.</p>
            {discardError && (
              <p className="text-[12px]" style={{ color: "#be123c" }}>
                {discardError}
              </p>
            )}
          </div>
        }
        confirmLabel="Discard"
        destructive
        onConfirm={async () => {
          if (!tasklist) return;
          try {
            await discardTasklist(scope, tasklist.id);
            setShowDiscardDialog(false);
          } catch (err) {
            setDiscardError(err instanceof Error ? err.message : String(err));
            throw err;
          }
        }}
        onCancel={() => {
          setShowDiscardDialog(false);
          setDiscardError(null);
        }}
      />

      <ConfirmDialog
        open={showReplayDialog}
        title="Replay tasklist?"
        message={
          <div className="space-y-2">
            <p>
              Clone this tasklist&apos;s plan into a brand-new tasklist with
              fresh ids, fresh task statuses, and a fresh workspace +
              transcripts directory. The new tasklist will start in Active and
              dispatch will begin immediately.
            </p>
            <p className="text-[12px] opacity-80">
              The current tasklist is preserved untouched in its current state
              for history.
            </p>
            {replayError && (
              <p className="text-[12px]" style={{ color: "#be123c" }}>
                {replayError}
              </p>
            )}
          </div>
        }
        confirmLabel="Replay"
        onConfirm={async () => {
          if (!tasklist) return;
          try {
            await replayTasklist(scope, tasklist.id);
            setShowReplayDialog(false);
          } catch (err) {
            setReplayError(err instanceof Error ? err.message : String(err));
            throw err;
          }
        }}
        onCancel={() => {
          setShowReplayDialog(false);
          setReplayError(null);
        }}
      />
    </div>
  );
}

/** Inline composer for appending tasks to the current tasklist.
 *  Layout:
 *    Row 1 — [TipTap editor]
 *    Row 2 — [agent picker]                    [Send {mode} ▼]
 *  - The prompt is a TipTap editor (mirrors ChatInput) so `@agent` mentions
 *    work via TipTap's Suggestion plugin and the typed text stays visible.
 *    Unlike ChatInput, selecting an agent does NOT insert a chip into the
 *    prompt — instead the typed `@query` is deleted and the picker at the
 *    bottom-left adopts the chosen agent. The chosen name "flies" from the
 *    cursor position to the picker label (framer-motion) so the state change
 *    is visually obvious.
 *  - `ownerAgentId === ""` means "Coordinator picks" — the request body omits
 *    `owner_agent_id` so the backend treats it as unowned and routes via the
 *    coordinator.
 *  - SEQ/PAR mode lives inside the split send button — main click sends with
 *    current mode, dropdown picks a mode AND sends, updating the default for
 *    next time.
 *  - Shortcuts: `@` opens the agent picker (via Suggestion); arrows/Enter/Esc
 *    drive the menu via a global capture-phase listener (mirrors how
 *    MentionAutocomplete works in chat). `Shift+Space` toggles SEQ↔PAR.
 *  - On submit success, prompt is cleared but mode + agent selection persist
 *    so consecutive appends are fast.
 *  - On submit error, the message is shown inline and inputs are preserved. */
function TasklistComposer({
  scope,
  tasklist,
  teamMembers,
  agentNameMap,
  agentEmojiMap,
  fallbackEmoji,
}: {
  scope: TasklistScope | null;
  tasklist: Tasklist | null;
  teamMembers: TeamMember[];
  agentNameMap: Record<string, string>;
  agentEmojiMap: Record<string, string>;
  fallbackEmoji: string;
}) {
  const [prompt, setPrompt] = useState("");
  const [mode, setMode] = useState<TaskGroupMode>("SEQ");
  const [ownerAgentId, setOwnerAgentId] = useState<string>("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Border-flash signals an invalid submit attempt (empty prompt, no team /
  // tasklist, or uploads still in flight). The button itself stays
  // visually active in all states; the composer border pulses red instead.
  const [borderFlash, setBorderFlash] = useState(false);
  const flashTimerRef = useRef<number | null>(null);
  const triggerBorderFlash = useCallback(() => {
    if (flashTimerRef.current !== null) {
      window.clearTimeout(flashTimerRef.current);
    }
    setBorderFlash(true);
    flashTimerRef.current = window.setTimeout(() => {
      setBorderFlash(false);
      flashTimerRef.current = null;
    }, 600);
  }, []);
  useEffect(() => {
    return () => {
      if (flashTimerRef.current !== null) {
        window.clearTimeout(flashTimerRef.current);
      }
    };
  }, []);

  // Pending attachment state — mirrors ChatInput. Each entry tracks one
  // upload from selection through resolution; only `uploaded` entries
  // contribute their server id to the appendTask request.
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

  // Menu state — agentMenuOpen is unified across click-trigger and @-trigger.
  // Highlight is tracked by agent ID (not index) so it stays anchored to the
  // same item as the visible list shrinks/grows during typing — index-based
  // tracking caused visible blinking when the filtered list re-ordered.
  const [agentMenuOpen, setAgentMenuOpen] = useState(false);
  const [agentMenuHighlightId, setAgentMenuHighlightId] = useState<
    string | null
  >(null);
  const [sendMenuOpen, setSendMenuOpen] = useState(false);

  // Mention (@-trigger) state — when active, the menu filters on `mentionQuery`
  // and selecting an item triggers the flight animation + range deletion.
  const [mentionActive, setMentionActive] = useState(false);
  const [mentionQuery, setMentionQuery] = useState("");
  const mentionRangeRef = useRef<{ from: number; to: number } | null>(null);

  // Flight animation: ghost element that morphs from cursor → picker label
  // when an @-mention is selected. nextOwnerId is applied on animation end so
  // the picker label flips to the new agent at the moment of arrival.
  type Flight = {
    label: string;
    emoji: string | null;
    from: { left: number; top: number };
    to: { left: number; top: number };
    nextOwnerId: string;
  };
  const [flight, setFlight] = useState<Flight | null>(null);

  const agentMenuRef = useRef<HTMLDivElement | null>(null);
  const agentPickerButtonRef = useRef<HTMLButtonElement | null>(null);
  const sendMenuRef = useRef<HTMLDivElement | null>(null);

  // Hover-to-highlight is gated until the user actually moves the mouse after
  // the menu opens. Why: the menu opens upward and often appears under a
  // stationary cursor (especially after typing `@` in the editor), which fires
  // mouseenter on whatever item happens to land under the cursor — usually the
  // bottom-most item — and visibly hijacks the seeded highlight. Real mouse
  // movement flips this to true; opening the menu resets it to false.
  const hoverEnabledRef = useRef(false);

  // Full menu items (Coordinator + members).
  const allMenuItems = useMemo(
    () => [
      { id: "", label: "Coordinator picks", emoji: null as string | null },
      ...teamMembers.map((m) => ({
        id: m.agent_id,
        label: agentNameMap[m.agent_id] ?? m.agent_id,
        emoji: agentEmojiMap[m.agent_id] ?? fallbackEmoji,
      })),
    ],
    [teamMembers, agentNameMap, agentEmojiMap, fallbackEmoji],
  );

  // When @ is active, filter on mention query; otherwise show the full list.
  const visibleMenuItems = useMemo(() => {
    if (!mentionActive) return allMenuItems;
    const q = mentionQuery.toLowerCase().trim();
    if (!q) return allMenuItems;
    return allMenuItems.filter((it) => it.label.toLowerCase().includes(q));
  }, [allMenuItems, mentionActive, mentionQuery]);

  // Derived index of the currently-highlighted item in the visible list.
  // -1 when nothing is highlighted (e.g., visible list is empty).
  const agentMenuHighlight = useMemo(() => {
    if (agentMenuHighlightId === null) return -1;
    return visibleMenuItems.findIndex((i) => i.id === agentMenuHighlightId);
  }, [visibleMenuItems, agentMenuHighlightId]);

  // When the menu opens (click or @), seed the highlight with the
  // currently-selected owner so the user sees "where I am right now" instead
  // of a flicker between -1 ↔ 0 as the query changes. Reset to null when the
  // menu closes so a fresh open re-seeds cleanly. Also gate hover-driven
  // highlight changes until the user actually moves the mouse — otherwise
  // a menu that opens under a stationary cursor immediately hijacks the
  // highlight to whatever item happens to be under the cursor.
  useEffect(() => {
    if (!agentMenuOpen) {
      setAgentMenuHighlightId(null);
      hoverEnabledRef.current = false;
      return;
    }
    setAgentMenuHighlightId(ownerAgentId);
    hoverEnabledRef.current = false;
    const onMove = () => {
      hoverEnabledRef.current = true;
    };
    window.addEventListener("mousemove", onMove, { once: true });
    return () => window.removeEventListener("mousemove", onMove);
    // Intentionally exclude ownerAgentId — we only want to seed on open, not
    // jump the highlight if the owner changes mid-session.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentMenuOpen]);

  // As the @-query narrows the list, keep the highlight on the same item by
  // ID. If that item filtered out, fall back to the first visible item. If
  // nothing is visible, drop to null (Enter becomes a no-op until something
  // matches). This is the key blink-fix: once an item is highlighted, the
  // highlight stays on something concrete instead of toggling on/off.
  useEffect(() => {
    if (!agentMenuOpen) return;
    if (visibleMenuItems.length === 0) {
      setAgentMenuHighlightId(null);
      return;
    }
    setAgentMenuHighlightId((prev) => {
      if (prev !== null && visibleMenuItems.some((i) => i.id === prev)) {
        return prev;
      }
      return visibleMenuItems[0].id;
    });
  }, [agentMenuOpen, visibleMenuItems]);

  // Reset owner selection when the scope or tasklist changes.
  useEffect(() => {
    setOwnerAgentId("");
  }, [scope?.kind, scope?.id, tasklist?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  const composerDisabled = !scope || !tasklist;
  const trimmed = prompt.trim();
  // We deliberately keep the Send button visually active in all states; the
  // composer border pulses red when the user tries to submit with no prompt,
  // no scope/tasklist, or uploads still in flight. `canSubmit` here governs
  // whether submit goes through, NOT whether the button is dimmed.
  const canSubmit =
    !!scope &&
    !!tasklist &&
    trimmed.length > 0 &&
    !submitting &&
    !hasUploading;

  const ownerLabel =
    ownerAgentId === ""
      ? "Coordinator picks"
      : agentNameMap[ownerAgentId] ?? ownerAgentId;
  const ownerEmoji =
    ownerAgentId === ""
      ? fallbackEmoji
      : agentEmojiMap[ownerAgentId] ?? fallbackEmoji;

  const placeholder = !scope
    ? "Select a scope to add tasks"
    : !tasklist
      ? "Create a tasklist to add tasks"
      : "What's the next task?";

  // Refs for state values used inside captured editor callbacks (the editor
  // is created once on mount; closures see only the initial setters/values).
  const submitRef = useRef<(overrideMode?: TaskGroupMode) => void>(() => { });
  const handleFileSelectRef = useRef<(files: FileList | null) => void>(() => { });
  const mentionActiveRef = useRef(mentionActive);
  useEffect(() => {
    mentionActiveRef.current = mentionActive;
  }, [mentionActive]);
  const agentMenuOpenRef = useRef(agentMenuOpen);
  useEffect(() => {
    agentMenuOpenRef.current = agentMenuOpen;
  }, [agentMenuOpen]);

  const editor = useEditor({
    extensions: [
      StarterKit.configure({
        blockquote: false,
        bulletList: false,
        codeBlock: false,
        heading: false,
        horizontalRule: false,
        listItem: false,
        orderedList: false,
        code: false,
        bold: false,
        italic: false,
        strike: false,
      }),
      Placeholder.configure({ placeholder }),
      Extension.create({
        name: "tasklistAgentMention",
        addProseMirrorPlugins() {
          return [
            Suggestion({
              editor: this.editor as Editor,
              char: "@",
              allowSpaces: false,
              // No-op: handleAgentSelect is the real selection path. It deletes
              // the @range and updates state directly.
              command: () => { },
              items: () => [1],
              render: () => ({
                onStart: (props: {
                  query: string;
                  range: { from: number; to: number };
                }) => {
                  mentionRangeRef.current = props.range;
                  setMentionActive(true);
                  setMentionQuery(props.query);
                  setAgentMenuOpen(true);
                },
                onUpdate: (props: {
                  query: string;
                  range: { from: number; to: number };
                }) => {
                  mentionRangeRef.current = props.range;
                  setMentionQuery(props.query);
                },
                onExit: () => {
                  mentionRangeRef.current = null;
                  setMentionActive(false);
                  setMentionQuery("");
                  // Close menu when @ exits — handleAgentSelect already closed
                  // it on click/Enter, so this fires only on backspace-out.
                  setAgentMenuOpen(false);
                },
                onKeyDown: () => false,
              }),
            }),
          ];
        },
      }),
    ],
    content: "",
    editable: !composerDisabled && !submitting,
    editorProps: {
      attributes: {
        class:
          "outline-none text-[13px] text-[var(--text-primary)] !leading-[20px] min-h-[20px] max-h-[120px] overflow-y-auto",
      },
      handlePaste(_view, event) {
        // Image-only paste interception, mirroring ChatInput. Plain-text
        // pastes (including code/URLs) fall through to TipTap so we don't
        // disrupt the typing flow.
        const cb = event.clipboardData;
        if (!cb) return false;
        const imageFiles: File[] = [];
        for (let i = 0; i < cb.items.length; i++) {
          const item = cb.items[i];
          if (item.type.startsWith("image/")) {
            const f = item.getAsFile();
            if (f) imageFiles.push(f);
          }
        }
        if (imageFiles.length === 0) return false;
        const dt = new DataTransfer();
        imageFiles.forEach((f) => dt.items.add(f));
        handleFileSelectRef.current(dt.files);
        return true;
      },
      handleKeyDown(_view, event) {
        // While the agent menu is open, the global capture-phase listener owns
        // navigation keys — short-circuit here so the editor doesn't double-act.
        if (agentMenuOpenRef.current) {
          if (
            event.key === "ArrowDown" ||
            event.key === "ArrowUp" ||
            event.key === "Enter" ||
            event.key === "Escape"
          ) {
            return false;
          }
        }
        if (event.key === "Enter" && !event.shiftKey) {
          event.preventDefault();
          submitRef.current();
          return true;
        }
        if (event.shiftKey && event.key === " ") {
          if (mentionActiveRef.current) return false;
          event.preventDefault();
          setMode((m) => (m === "SEQ" ? "PAR" : "SEQ"));
          return true;
        }
        return false;
      },
    },
    onUpdate: ({ editor: ed }) => {
      setPrompt(ed.getText());
    },
  });

  // Keep placeholder in sync as composer context changes (no team / no tasklist
  // / ready). Same pattern as ChatInput — mutate the extension option in place.
  useEffect(() => {
    if (!editor) return;
    editor.extensionManager.extensions.forEach((ext) => {
      if (ext.name === "placeholder") {
        (ext.options as Record<string, unknown>).placeholder = placeholder;
        editor.view.dispatch(editor.state.tr);
      }
    });
  }, [editor, placeholder]);

  // Keep editor's editable flag in sync.
  useEffect(() => {
    if (!editor) return;
    editor.setEditable(!composerDisabled && !submitting);
  }, [editor, composerDisabled, submitting]);

  // Close the agent menu on outside click.
  useEffect(() => {
    if (!agentMenuOpen) return;
    const onMouseDown = (e: MouseEvent) => {
      const node = agentMenuRef.current;
      if (node && !node.contains(e.target as Node)) {
        setAgentMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, [agentMenuOpen]);

  // Close the send-mode menu on outside click.
  useEffect(() => {
    if (!sendMenuOpen) return;
    const onMouseDown = (e: MouseEvent) => {
      const node = sendMenuRef.current;
      if (node && !node.contains(e.target as Node)) {
        setSendMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, [sendMenuOpen]);

  const submit = useCallback(
    async (overrideMode?: TaskGroupMode) => {
      if (!canSubmit || !scope || !tasklist) {
        // Visual feedback for the always-active send button: the composer
        // border pulses red. Covers empty prompt, no scope/tasklist, and
        // uploads-still-in-flight cases.
        triggerBorderFlash();
        return;
      }
      const m = overrideMode ?? mode;
      setSubmitting(true);
      setError(null);
      try {
        await api.appendTaskForScope(scope, tasklist.id, {
          prompt: trimmed,
          mode: m,
          // Omit when "Coordinator picks" so the server treats it as unowned
          // and triggers coordinator routing.
          ...(ownerAgentId ? { owner_agent_id: ownerAgentId } : {}),
          ...(uploadedAttachmentIds.length > 0
            ? { attachment_ids: uploadedAttachmentIds }
            : {}),
        });
        setPrompt("");
        editor?.commands.clearContent();
        // Reset the picker to "Coordinator picks" so each new task starts at
        // the default — prevents the previous task's owner from sticking.
        setOwnerAgentId("");
        // Clear attachment chips on successful append. Local previewUrls were
        // only used for the chip UI; revoking keeps memory tidy.
        for (const p of pendingAttachments) {
          if (p.previewUrl) URL.revokeObjectURL(p.previewUrl);
        }
        setPendingAttachments([]);
        requestAnimationFrame(() => editor?.commands.focus());
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setSubmitting(false);
      }
    },
    [
      canSubmit,
      scope,
      tasklist,
      trimmed,
      mode,
      ownerAgentId,
      editor,
      uploadedAttachmentIds,
      pendingAttachments,
      triggerBorderFlash,
    ],
  );
  submitRef.current = submit;

  const handleSubmit = useCallback(
    (e?: React.FormEvent) => {
      e?.preventDefault();
      void submit();
    },
    [submit],
  );

  // Attachment upload was only supported for team scopes, which are no
  // longer reachable — this is a no-op kept so the file input's onChange
  // and the pending-attachments strip below still have a stable handler.
  const handleFileSelect = useCallback(async (_files: FileList | null) => {
    return;
  }, []);

  const handleRemoveAttachment = useCallback(
    async (pendingId: string) => {
      const pending = pendingAttachments.find((p) => p.id === pendingId);
      if (!pending) return;
      if (pending.previewUrl) URL.revokeObjectURL(pending.previewUrl);
      setPendingAttachments((prev) => prev.filter((p) => p.id !== pendingId));
    },
    [pendingAttachments],
  );
  handleFileSelectRef.current = handleFileSelect;

  const FLIGHT_DURATION_MS = 280;

  // Selecting an agent: when @-triggered, animate the typed text from the
  // cursor → picker label and clear the @range; when picker-triggered, just
  // update state immediately.
  const handleAgentSelect = useCallback(
    (item: (typeof allMenuItems)[number]) => {
      const range = mentionRangeRef.current;
      const triggeredByMention = mentionActive && !!range && !!editor;
      if (triggeredByMention) {
        const startCoords = (() => {
          try {
            return editor!.view.coordsAtPos(range!.from);
          } catch {
            return null;
          }
        })();
        const buttonRect = agentPickerButtonRef.current?.getBoundingClientRect();
        if (startCoords && buttonRect) {
          setFlight({
            label: item.label,
            emoji: item.emoji,
            from: { left: startCoords.left, top: startCoords.top },
            to: { left: buttonRect.left + 8, top: buttonRect.top + 4 },
            nextOwnerId: item.id,
          });
        } else {
          // No coords available (very early or detached) — fall back to a
          // plain state update so we never strand the user.
          setOwnerAgentId(item.id);
        }
        editor!
          .chain()
          .focus()
          .deleteRange({ from: range!.from, to: range!.to })
          .run();
      } else {
        setOwnerAgentId(item.id);
        requestAnimationFrame(() => editor?.commands.focus());
      }
      setAgentMenuOpen(false);
    },
    [mentionActive, editor],
  );

  // Global capture-phase keyboard nav while the agent menu is open. Mirrors
  // MentionAutocomplete in chat — capture-phase wins over the editor's own
  // keydown so arrows/Enter/Esc drive the menu cleanly.
  useEffect(() => {
    if (!agentMenuOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (visibleMenuItems.length === 0) return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        e.stopPropagation();
        const cur = agentMenuHighlight < 0 ? -1 : agentMenuHighlight;
        const next = (cur + 1) % visibleMenuItems.length;
        setAgentMenuHighlightId(visibleMenuItems[next].id);
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        e.stopPropagation();
        const cur =
          agentMenuHighlight < 0 ? visibleMenuItems.length : agentMenuHighlight;
        const next =
          (cur - 1 + visibleMenuItems.length) % visibleMenuItems.length;
        setAgentMenuHighlightId(visibleMenuItems[next].id);
        return;
      }
      if (e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        // No-op when nothing is highlighted (e.g., empty visible list).
        if (agentMenuHighlight < 0) return;
        const item = visibleMenuItems[agentMenuHighlight];
        if (item) handleAgentSelect(item);
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        setAgentMenuOpen(false);
        requestAnimationFrame(() => editor?.commands.focus());
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [
    agentMenuOpen,
    visibleMenuItems,
    agentMenuHighlight,
    handleAgentSelect,
    editor,
  ]);

  const modeLabel = mode === "SEQ" ? "Sequential" : "Parallel";
  const ModeIcon = mode === "SEQ" ? Rows2 : Columns2;

  return (
    <form
      onSubmit={handleSubmit}
      className="shrink-0 px-3 pb-3 pt-2"
      style={{ backgroundColor: "var(--bg-secondary)" }}
    >
      <div
        className={`rounded-[12px] border flex flex-col cursor-text ${borderFlash ? "animate-border-flash" : ""
          }`}
        onMouseDown={(e) => {
          // Click anywhere on the composer (padding, row-2 empty space)
          // focuses the editor — except on interactive children, which
          // have their own handlers, and clicks inside the editor itself,
          // which ProseMirror handles natively.
          const target = e.target as HTMLElement;
          if (target.closest("button")) return;
          if (target.closest(".ProseMirror")) return;
          e.preventDefault();
          editor?.commands.focus();
        }}
        style={{
          backgroundColor: "var(--bg-input)",
          borderColor: "var(--border-primary)",
        }}
      >
        {/* Hidden file input for the paperclip. */}
        <input
          ref={fileInputRef}
          type="file"
          multiple
          className="hidden"
          onChange={(e) => {
            handleFileSelect(e.target.files);
            e.target.value = "";
          }}
        />

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
                      onClick={() => handleRemoveAttachment(pa.id)}
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

        {/* Row 1: TipTap prompt editor. */}
        <div className="px-3 pt-3 pb-2">
          <EditorContent editor={editor} />
        </div>

        {/* Row 2: paperclip + agent picker (left) + split send button (right). */}
        <div className="flex items-center justify-between px-2 pb-2 pt-1 gap-2">
          <div className="flex items-center gap-1 shrink-0">
            <Tooltip placement="top" label="Attach files">
              <button
                type="button"
                onClick={() => fileInputRef.current?.click()}
                disabled={composerDisabled || submitting}
                aria-label="Attach files"
                className="flex items-center justify-center h-[28px] w-[28px] rounded-[6px] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                style={{ color: "var(--text-secondary)" }}
              >
                <Paperclip size={14} />
              </button>
            </Tooltip>
            <div ref={agentMenuRef} className="relative shrink-0">
              <Tooltip
                placement="top"
                label={`Owner: ${ownerLabel} — type @ in the prompt to switch`}
              >
                <button
                  ref={agentPickerButtonRef}
                  type="button"
                  onClick={() => {
                    // Click-trigger always shows the unfiltered list, regardless
                    // of any in-flight @ in the editor.
                    setMentionActive(false);
                    setMentionQuery("");
                    mentionRangeRef.current = null;
                    setAgentMenuOpen((v) => !v);
                  }}
                  disabled={composerDisabled || submitting}
                  aria-label="Pick task owner"
                  aria-haspopup="listbox"
                  aria-expanded={agentMenuOpen}
                  className="flex items-center gap-1.5 h-[28px] px-2 rounded-[6px] text-[12px] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                  style={{ color: "var(--text-secondary)" }}
                >
                  {ownerAgentId === "" ? (
                    <CircleUserRound size={14} />
                  ) : (
                    <span className="text-[13px] leading-none">{ownerEmoji}</span>
                  )}
                  <span className="max-w-[120px] truncate">{ownerLabel}</span>
                  <ChevronDown size={11} />
                </button>
              </Tooltip>
              {agentMenuOpen && visibleMenuItems.length > 0 && (
                <div
                  role="listbox"
                  className="absolute left-0 bottom-[34px] z-30 min-w-[200px] rounded-[10px] border py-1 shadow-lg max-h-[240px] overflow-y-auto"
                  style={{
                    backgroundColor: "var(--bg-secondary)",
                    borderColor: "var(--border-primary)",
                  }}
                >
                  {visibleMenuItems.map((item, idx) => {
                    const isHighlighted = idx === agentMenuHighlight;
                    // Only show the divider on the unfiltered list (where
                    // index 1 is the first member after "Coordinator picks").
                    const showDivider = !mentionActive && idx === 1;
                    return (
                      <div key={item.id || "__coord__"}>
                        {showDivider && (
                          <div
                            className="my-1 h-px"
                            style={{
                              backgroundColor: "var(--border-primary)",
                            }}
                          />
                        )}
                        <button
                          type="button"
                          role="option"
                          aria-selected={ownerAgentId === item.id}
                          onMouseEnter={() => {
                            if (!hoverEnabledRef.current) return;
                            setAgentMenuHighlightId(item.id);
                          }}
                          onMouseDown={(e) => {
                            // Prevent blur of the editor before we capture the
                            // cursor coords for the flight animation.
                            e.preventDefault();
                          }}
                          onClick={() => handleAgentSelect(item)}
                          className="w-full flex items-center gap-2 px-3 py-[7px] text-[12.5px] text-left transition-colors"
                          style={{
                            color: "var(--text-primary)",
                            backgroundColor: isHighlighted
                              ? "var(--bg-hover)"
                              : "transparent",
                          }}
                        >
                          {item.emoji === null ? (
                            <CircleUserRound size={14} />
                          ) : (
                            <span className="text-[13px] leading-none">
                              {item.emoji}
                            </span>
                          )}
                          <span className="truncate">{item.label}</span>
                        </button>
                      </div>
                    );
                  })}
                </div>
              )}
            </div>
          </div>

          <div className="flex items-center gap-2 min-w-0">
            {error && (
              <Tooltip placement="top" label={error} className="min-w-0">
                <span
                  role="alert"
                  className="block text-[11px] truncate max-w-[220px]"
                  style={{ color: "#be123c" }}
                >
                  {error}
                </span>
              </Tooltip>
            )}
            <div ref={sendMenuRef} className="relative min-w-0">
              <div
                className="flex items-stretch rounded-[8px] overflow-hidden min-w-0"
                style={{
                  backgroundColor: "var(--text-primary)",
                  color: "var(--bg-primary)",
                }}
              >
                <Tooltip
                  placement="top"
                  label={`Send as ${modeLabel} — Shift+Space to toggle`}
                  className="flex min-w-0"
                >
                  <button
                    type="submit"
                    className="flex items-center gap-1.5 pl-3 pr-2 py-[5px] text-[12px] font-medium cursor-pointer min-w-0"
                  >
                    {submitting ? (
                      <Loader2 size={12} className="shrink-0 animate-spin" />
                    ) : (
                      <ModeIcon size={11} className="shrink-0" />
                    )}
                    <span className="truncate">Send {modeLabel}</span>
                  </button>
                </Tooltip>
                <span
                  className="shrink-0 w-px my-[5px]"
                  style={{ backgroundColor: "currentColor", opacity: 0.3 }}
                />
                <button
                  type="button"
                  onClick={() => setSendMenuOpen((v) => !v)}
                  disabled={composerDisabled || submitting}
                  aria-label="Choose dispatch mode"
                  aria-haspopup="menu"
                  aria-expanded={sendMenuOpen}
                  className="shrink-0 flex items-center justify-center px-1.5 cursor-pointer disabled:cursor-not-allowed"
                >
                  <ChevronDown size={11} />
                </button>
              </div>
              {sendMenuOpen && (
                <div
                  role="menu"
                  className="absolute right-0 bottom-[34px] z-30 min-w-[200px] rounded-[10px] border py-1 shadow-lg"
                  style={{
                    backgroundColor: "var(--bg-secondary)",
                    borderColor: "var(--border-primary)",
                  }}
                >
                  <Tooltip placement="top" label="Runs after prior tasks finish">
                    <button
                      type="button"
                      role="menuitem"
                      onClick={() => {
                        setMode("SEQ");
                        setSendMenuOpen(false);
                        void submit("SEQ");
                      }}
                      className="w-full flex items-center gap-2 px-3 py-[7px] text-[12.5px] text-left hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
                      style={{ color: "var(--text-primary)" }}
                    >
                      <Rows2 size={12} />
                      Send Sequential
                    </button>
                  </Tooltip>
                  <Tooltip placement="top" label="Runs alongside other parallel tasks">
                    <button
                      type="button"
                      role="menuitem"
                      onClick={() => {
                        setMode("PAR");
                        setSendMenuOpen(false);
                        void submit("PAR");
                      }}
                      className="w-full flex items-center gap-2 px-3 py-[7px] text-[12.5px] text-left hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
                      style={{ color: "var(--text-primary)" }}
                    >
                      <Columns2 size={12} />
                      Send Parallel
                    </button>
                  </Tooltip>
                </div>
              )}
            </div>
          </div>
        </div>
      </div>

      {/* Flight ghost: animates @-typed name from the cursor to the picker
       *  label. nextOwnerId is applied on animation end so the picker label
       *  flips to the new agent at the moment of arrival. */}
      {flight &&
        createPortal(
          <motion.div
            key={`${flight.from.left}-${flight.from.top}-${flight.nextOwnerId}`}
            initial={{
              left: flight.from.left,
              top: flight.from.top,
              opacity: 1,
              scale: 1,
            }}
            animate={{
              left: flight.to.left,
              top: flight.to.top,
              opacity: 0,
              scale: 0.85,
            }}
            transition={{
              duration: FLIGHT_DURATION_MS / 1000,
              ease: "easeInOut",
            }}
            onAnimationComplete={() => {
              setOwnerAgentId(flight.nextOwnerId);
              setFlight(null);
              requestAnimationFrame(() => editor?.commands.focus());
            }}
            style={{
              position: "fixed",
              pointerEvents: "none",
              zIndex: 1000,
              color: "var(--text-secondary)",
            }}
            className="flex items-center gap-1 text-[13px] font-medium"
          >
            {flight.emoji ? (
              <span className="text-[13px] leading-none">{flight.emoji}</span>
            ) : (
              <CircleUserRound size={13} />
            )}
            <span>{flight.label}</span>
          </motion.div>,
          document.body,
        )}
    </form>
  );
}

function OwnerAvatar({
  ownerName,
  ownerEmoji,
  circular,
  unassigned = false,
  active = false,
}: {
  ownerName: string;
  ownerEmoji: string;
  circular: boolean;
  unassigned?: boolean;
  /** True when this agent's task is in flight — adds a pulsing ring so the
   *  user can see at a glance which agent is currently working. */
  active?: boolean;
}) {
  if (unassigned) {
    return (
      <Tooltip placement="top" label="Unassigned — awaiting routing" className="shrink-0">
        <span
          className={`w-[24px] h-[24px] ${circular ? "rounded-full" : "rounded-[7px]"} flex items-center justify-center select-none`}
          style={{
            backgroundColor: "var(--bg-secondary)",
            color: "var(--text-tertiary)",
          }}
          aria-label="Unassigned"
        >
          <CircleUserRound size={16} />
        </span>
      </Tooltip>
    );
  }
  const color = agentAvatarColor(ownerName);
  const tooltipLabel = active ? `${ownerName} — working…` : ownerName;
  return (
    <Tooltip placement="top" label={tooltipLabel} className="shrink-0">
      <span
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

function TaskPillRow({
  scope,
  tasklistId,
  task,
  ownerName,
  ownerEmoji,
  circularAvatars,
  coordinatorId,
  agentNameMap,
  onSkip,
  onStop,
  onResume,
  onTaskClick,
}: {
  scope: TasklistScope | null;
  tasklistId: string;
  task: Task;
  ownerName: string;
  ownerEmoji: string;
  circularAvatars: boolean;
  coordinatorId: string | null;
  agentNameMap: Record<string, string>;
  /** Opens the panel-level skip ConfirmDialog. Only relevant for failed
   *  tasks; pill renders a Skip affordance only when this is provided. */
  onSkip?: (task: Task) => void;
  /** Opens the panel-level stop ConfirmDialog. Only relevant for in-progress
   *  tasks; pill renders a Stop affordance only when this is provided. */
  onStop?: (task: Task) => void;
  /** Re-queues a stopped task as Pending. Only relevant for stopped tasks;
   *  pill renders a Resume affordance only when this is provided. */
  onResume?: (task: Task) => void;
  /** Opens the task detail modal for this row's task. Bails on
   *  clicks that originate inside an inner button (skip, comment toggle)
   *  so existing handlers don't double-fire. */
  onTaskClick?: (taskId: string) => void;
}) {
  // A task is unassigned only when it has neither a coordinator-recorded
  // routing decision (`assignment`) nor a legacy pinned owner. Coordinator-
  // picked tasks stamp `assignment.owner_agent_id` while leaving the legacy
  // field empty, so reading only `owner_agent_id` mis-renders them as
  // unassigned. Mirrors TodoPanel's resolution order.
  const unassigned = !task.assignment && task.owner_agent_id === "";
  const isComplete = task.status === "completed";
  const isFailed = task.status === "failed";
  const isActive = task.status === "in_progress";
  const isStopped = task.status === "stopped";
  const titleLine = task.prompt.split("\n")[0]?.trim() || task.prompt;
  const comments = task.comments ?? [];
  const commentCount = comments.length;
  const [threadOpen, setThreadOpen] = useState(false);

  // Joined error_log for the failure tooltip — keeps each entry on its own
  // visual block by separating with a blank line. Falls back to a generic
  // message when error_log is empty (rare, but possible if the failure path
  // didn't push anything before the feeder gave up).
  const errorTooltipLabel =
    isFailed && task.error_log.length > 0
      ? task.error_log.join("\n\n")
      : "Failed — no error reason was recorded.";

  const clickable = !!onTaskClick;
  const handleRowActivate = (e: React.SyntheticEvent) => {
    if (!onTaskClick) return;
    // Skip when the activation originated inside an inner interactive
    // element (skip button, comment toggle). The Tooltip-wrapped title and
    // owner avatar are non-interactive spans, so they fall through here and
    // open the modal as expected.
    const target = e.target as HTMLElement | null;
    if (target?.closest("button, a")) return;
    onTaskClick(task.id);
  };
  return (
    <div
      className="group flex flex-col gap-0 rounded-[12px] transition-colors"
      style={{ backgroundColor: "var(--bg-tertiary)" }}
    >
      <div
        className={`flex items-center gap-2 px-3 py-[10px] ${clickable ? "cursor-pointer hover:bg-[var(--bg-hover)] rounded-[12px]" : ""}`}
        role={clickable ? "button" : undefined}
        tabIndex={clickable ? 0 : undefined}
        onClick={clickable ? handleRowActivate : undefined}
        onKeyDown={
          clickable
            ? (e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                handleRowActivate(e);
              }
            }
            : undefined
        }
        aria-label={clickable ? `Open task details: ${titleLine}` : undefined}
      >
        <StatusCheckbox status={task.status} />

        <Tooltip placement="top" label={task.prompt} className="flex-1 min-w-0">
          <span
            className={`block truncate text-[13px] ${isComplete ? "line-through opacity-60" : ""}`}
            style={{ color: "var(--text-primary)" }}
          >
            {titleLine}
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

        {isFailed && onSkip && (
          <Tooltip
            placement="top"
            label="Skip this task and let the tasklist continue past it"
            className="shrink-0"
          >
            <button
              type="button"
              onClick={() => onSkip(task)}
              aria-label="Skip task"
              className="inline-flex items-center gap-1 px-[8px] h-[20px] rounded-full text-[10.5px] font-medium transition-colors cursor-pointer hover:bg-[var(--bg-hover)]"
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

        {isStopped && (
          <span
            className="shrink-0 inline-flex items-center gap-1 px-[8px] h-[20px] rounded-full text-[10.5px] font-medium uppercase tracking-wide"
            style={{
              backgroundColor: "rgba(217,119,6,0.12)",
              color: "rgb(180,83,9)",
            }}
            aria-label="Stopped — resume to re-queue"
          >
            <Square size={9} fill="currentColor" />
            stopped
          </span>
        )}

        {isActive && onStop && (
          <Tooltip
            placement="top"
            label="Stop this task — kills its in-flight run; you can resume it later"
            className="shrink-0"
          >
            <button
              type="button"
              onClick={() => onStop(task)}
              aria-label="Stop task"
              className="inline-flex items-center gap-1 px-[8px] h-[20px] rounded-full text-[10.5px] font-medium transition-colors cursor-pointer hover:bg-[var(--bg-hover)]"
              style={{
                color: "var(--text-secondary)",
                border: "1px solid var(--border-primary)",
              }}
            >
              <Square size={9} fill="currentColor" />
              Stop
            </button>
          </Tooltip>
        )}

        {isStopped && onResume && (
          <Tooltip
            placement="top"
            label="Resume this task — re-queues it as Pending for dispatch"
            className="shrink-0"
          >
            <button
              type="button"
              onClick={() => onResume(task)}
              aria-label="Resume task"
              className="inline-flex items-center gap-1 px-[8px] h-[20px] rounded-full text-[10.5px] font-medium transition-colors cursor-pointer hover:bg-[var(--bg-hover)]"
              style={{
                color: "var(--text-secondary)",
                border: "1px solid var(--border-primary)",
              }}
            >
              <Play size={9} fill="currentColor" />
              Resume
            </button>
          </Tooltip>
        )}

        {(() => {
          const visibleOutputs = filterVisibleOutputs(task.expected_outputs);
          if (visibleOutputs.length === 0) return null;
          return (
            <Tooltip
              placement="top"
              label={visibleOutputs.map(displayOutputFilename).join(", ")}
              className="hidden sm:block"
            >
              <span
                className="inline-flex items-center gap-1 px-[6px] py-[1px] rounded text-[10px] font-mono"
                style={{
                  backgroundColor: "var(--bg-tertiary)",
                  color: "var(--text-secondary)",
                }}
              >
                <FileText size={9} />
                {visibleOutputs.length}
              </span>
            </Tooltip>
          );
        })()}

        <Tooltip
          placement="top"
          label={
            commentCount > 0
              ? `${commentCount} comment${commentCount === 1 ? "" : "s"}`
              : "Add a comment"
          }
          className="shrink-0"
        >
          <button
            type="button"
            onClick={() => setThreadOpen((v) => !v)}
            aria-label={
              commentCount > 0
                ? `${commentCount} comment${commentCount === 1 ? "" : "s"} (${threadOpen ? "hide" : "show"})`
                : `Add a comment`
            }
            aria-expanded={threadOpen}
            className={`inline-flex items-center gap-1 px-[6px] h-[20px] rounded-[6px] text-[11px] font-medium transition-colors cursor-pointer ${commentCount > 0
              ? "opacity-90"
              : "opacity-30 group-hover:opacity-80"
              }`}
            style={{
              color: threadOpen ? "var(--text-primary)" : "var(--text-secondary)",
              backgroundColor: threadOpen
                ? "var(--bg-hover)"
                : "transparent",
            }}
          >
            <MessageSquare size={11} />
            {commentCount > 0 && (
              <span className="tabular-nums">{commentCount}</span>
            )}
          </button>
        </Tooltip>

        <OwnerAvatar
          ownerName={ownerName}
          ownerEmoji={ownerEmoji}
          circular={circularAvatars}
          unassigned={unassigned}
          active={isActive}
        />

        {/* Drag handle — disabled until reorder is supported.
        <span
          className="shrink-0 opacity-30 group-hover:opacity-60 transition-opacity"
          style={{ color: "var(--text-tertiary)" }}
        >
          <GripVertical size={14} />
        </span>
        */}
      </div>

      {threadOpen && (
        <TaskCommentThread
          scope={scope}
          tasklistId={tasklistId}
          taskId={task.id}
          comments={comments}
          coordinatorId={coordinatorId}
          agentNameMap={agentNameMap}
        />
      )}
    </div>
  );
}

/** Inline comment thread for a single task. Shows comments in
 *  chronological (insertion) order with author attribution; coordinator-
 *  authored agent comments get a visually distinct chip so the user can tell
 *  them apart from peer-agent self-notes and user comments. */
function TaskCommentThread({
  scope,
  tasklistId,
  taskId,
  comments,
  coordinatorId,
  agentNameMap,
}: {
  scope: TasklistScope | null;
  tasklistId: string;
  taskId: string;
  comments: TaskComment[];
  coordinatorId: string | null;
  agentNameMap: Record<string, string>;
}) {
  const [draft, setDraft] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const refreshTasklist = useTasklistStore((s) => s.refreshTasklist);

  const trimmed = draft.trim();
  const canSubmit = !!scope && trimmed.length > 0 && !submitting;

  const handleSubmit = useCallback(
    async (e?: React.FormEvent) => {
      e?.preventDefault();
      if (!canSubmit || !scope) return;
      setSubmitting(true);
      setError(null);
      try {
        // V1 author resolution defaults to user — backend stamps author_kind=user
        // with author_id="user" when omitted. The inline panel is the user's
        // surface, so that default is correct here.
        await api.addTaskCommentForScope(scope, tasklistId, taskId, { body: trimmed });
        setDraft("");
        // Backend doesn't currently emit an SSE event for comment-added, so
        // pull the latest tasklist into the cache to surface the new comment.
        await refreshTasklist(scope, tasklistId);
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setSubmitting(false);
      }
    },
    [canSubmit, scope, tasklistId, taskId, trimmed, refreshTasklist],
  );

  return (
    <div
      className="flex flex-col gap-2 px-3 pb-3 pt-1 border-t"
      style={{ borderColor: "var(--border-primary)" }}
    >
      {comments.length === 0 ? (
        <div
          className="text-[11px] italic pt-1"
          style={{ color: "var(--text-tertiary)" }}
        >
          No comments yet — leave one to augment this task with extra context.
        </div>
      ) : (
        <div className="flex flex-col gap-1.5 pt-1">
          {comments.map((c) => {
            const isCoordinator =
              c.author_kind === "agent" &&
              !!coordinatorId &&
              c.author_id === coordinatorId;
            const authorLabel =
              c.author_kind === "user"
                ? "You"
                : agentNameMap[c.author_id] ?? c.author_id ?? "Agent";
            return (
              <div
                key={c.id}
                className="flex rounded-[8px] overflow-hidden"
                style={{ backgroundColor: "var(--bg-secondary)" }}
              >
                {isCoordinator && (
                  <div
                    className="w-[2px] shrink-0"
                    style={{ backgroundColor: "rgb(168,85,247)" }}
                  />
                )}
                <div className="flex flex-col gap-0.5 px-2 py-1.5 flex-1 min-w-0">
                <div className="flex items-center gap-1.5 text-[10.5px]">
                  {!isCoordinator && (
                    <span
                      className="font-medium"
                      style={{ color: "var(--text-secondary)" }}
                    >
                      {authorLabel}
                    </span>
                  )}
                  {isCoordinator && (
                    <span
                      className="px-[5px] py-[1px] rounded-full text-[9.5px] font-semibold uppercase tracking-wide"
                      style={{
                        backgroundColor: "rgba(168,85,247,0.14)",
                        color: "rgb(126,34,206)",
                      }}
                    >
                      Coordinator
                    </span>
                  )}
                  {c.author_kind === "agent" && !isCoordinator && (
                    <span
                      className="px-[5px] py-[1px] rounded-full text-[9.5px] font-semibold uppercase tracking-wide"
                      style={{
                        backgroundColor: "var(--bg-tertiary)",
                        color: "var(--text-tertiary)",
                      }}
                    >
                      Agent
                    </span>
                  )}
                  <Tooltip placement="top" label={c.created_at}>
                    <time
                      className="opacity-60"
                      style={{ color: "var(--text-tertiary)" }}
                      dateTime={c.created_at}
                    >
                      {formatCommentTimestamp(c.created_at)}
                    </time>
                  </Tooltip>
                </div>
                <p
                  className="text-[12px] whitespace-pre-wrap break-words"
                  style={{ color: "var(--text-primary)" }}
                >
                  {c.body}
                </p>
                </div>
              </div>
            );
          })}
        </div>
      )}

      <form onSubmit={handleSubmit} className="flex items-center gap-2">
        <input
          type="text"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          placeholder="Add a comment…"
          disabled={!scope || submitting}
          className="flex-1 min-w-0 bg-transparent outline-none text-[12px] rounded-[8px] px-2 py-[6px] border placeholder:text-[var(--text-tertiary)] disabled:cursor-not-allowed"
          style={{
            color: "var(--text-primary)",
            backgroundColor: "var(--bg-secondary)",
            borderColor: "var(--border-primary)",
          }}
        />
        <button
          type="submit"
          disabled={!canSubmit}
          aria-label="Post comment"
          className="shrink-0 inline-flex items-center justify-center h-[26px] w-[26px] rounded-[6px] transition-opacity cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
          style={{
            backgroundColor: "var(--text-primary)",
            color: "var(--bg-primary)",
          }}
        >
          {submitting ? (
            <Loader2 size={12} className="animate-spin" />
          ) : (
            <Send size={11} />
          )}
        </button>
      </form>
      {error && (
        <Tooltip placement="top" label={error}>
          <span
            role="alert"
            className="text-[11px]"
            style={{ color: "#be123c" }}
          >
            {error}
          </span>
        </Tooltip>
      )}
    </div>
  );
}

function formatCommentTimestamp(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  // Local short form: 2:34 PM, or 2026-04-27 if not today.
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  if (sameDay) {
    return d.toLocaleTimeString(undefined, {
      hour: "numeric",
      minute: "2-digit",
    });
  }
  return d.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
  });
}

/** Group section header — surfaces the SEQ/PAR mode visually so the user
 *  understands why some tasks run in parallel and others don't. Rendered as
 *  an edge-to-edge banded block matching the progress row's visual style.
 *  When the header sits directly below the progress row (`isFirst`), the
 *  top border is dropped so the two bands don't double up. */
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
      className={`flex items-center gap-2 px-5 py-2 ${isFirst ? "border-b-0" : "border-y-0"}`}
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

function TasklistContent({
  scope,
  tasklist,
  coordinatorId,
  agentNameMap,
  agentEmojiMap,
  fallbackEmoji,
  circularAvatars,
  showCompleted,
  onSkipTask,
  onStopTask,
  onResumeTask,
  onTaskClick,
}: {
  scope: TasklistScope | null;
  tasklist: Tasklist;
  coordinatorId: string | null;
  agentNameMap: Record<string, string>;
  agentEmojiMap: Record<string, string>;
  fallbackEmoji: string;
  circularAvatars: boolean;
  showCompleted: boolean;
  onSkipTask?: (task: Task) => void;
  onStopTask?: (task: Task) => void;
  onResumeTask?: (task: Task) => void;
  onTaskClick?: (taskId: string) => void;
}) {
  // Filter per-group so the group header sticks with its tasks. Groups whose
  // visible-task count drops to zero (e.g. all-completed under "hide
  // completed") are dropped entirely so we don't leave dangling headers.
  const visibleGroups = useMemo(() => {
    return tasklist.groups
      .map((g, idx) => ({
        group: g,
        index: idx,
        tasks: showCompleted
          ? g.tasks
          : g.tasks.filter((t) => t.status !== "completed"),
      }))
      .filter((g) => g.tasks.length > 0);
  }, [tasklist, showCompleted]);

  const totalVisible = visibleGroups.reduce((n, g) => n + g.tasks.length, 0);

  return (
    <div className="flex flex-col">
      {visibleGroups.map(({ group, index, tasks }, displayIdx) => (
        <div key={group.id} className="flex flex-col">
          <GroupHeader
            index={index}
            mode={group.mode}
            isFirst={displayIdx === 0}
          />
          <div className="flex flex-col gap-2 px-5 py-2">
            {tasks.map((task) => {
              // Resolve the avatar from the routing decision first, falling
              // back to the legacy pinned owner. Coordinator-picked tasks
              // record `assignment.owner_agent_id` but leave the legacy field
              // empty; reading only the legacy field renders them as
              // unassigned. Mirrors TodoPanel's resolution order.
              const resolvedOwnerId =
                task.assignment?.owner_agent_id || task.owner_agent_id;
              return (
                <TaskPillRow
                  key={task.id}
                  scope={scope}
                  tasklistId={tasklist.id}
                  task={task}
                  ownerName={agentNameMap[resolvedOwnerId] ?? resolvedOwnerId}
                  ownerEmoji={agentEmojiMap[resolvedOwnerId] ?? fallbackEmoji}
                  circularAvatars={circularAvatars}
                  coordinatorId={coordinatorId}
                  agentNameMap={agentNameMap}
                  onSkip={onSkipTask}
                  onStop={onStopTask}
                  onResume={onResumeTask}
                  onTaskClick={onTaskClick}
                />
              );
            })}
          </div>
        </div>
      ))}
      {totalVisible === 0 && (
        <div
          className="text-center text-[12px] py-6 px-5 italic"
          style={{ color: "var(--text-tertiary)" }}
        >
          {showCompleted ? "No tasks yet." : "Nothing left to do."}
        </div>
      )}
    </div>
  );
}

export function InlineTasklistPanel({
  scope,
  teamMembers,
  coordinatorId,
  agentNameMap,
  agentEmojiMap,
  fallbackEmoji,
  onTaskClick,
}: {
  scope: TasklistScope | null;
  /** Members surfaced in the composer's agent picker. Empty array reduces
   *  the picker to just "Coordinator picks". */
  teamMembers: TeamMember[];
  /** Agent id of the coordinator/lead — used to visually distinguish
   *  coordinator-authored comments in task threads. */
  coordinatorId: string | null;
  agentNameMap: Record<string, string>;
  agentEmojiMap: Record<string, string>;
  fallbackEmoji: string;
  /** Opens the task detail modal when a row is clicked. When
   *  omitted, rows render non-clickable (preserves the prior UX for any
   *  caller that doesn't want this behavior). */
  onTaskClick?: (taskId: string) => void;
}) {
  useTasklistSSE(scope);
  const { current, loading, error } =
    useCurrentAndArchivedTasklistsForScope(scope);
  const skipTask = useTasklistStore((s) => s.skipTask);
  const stopTask = useTasklistStore((s) => s.stopTask);
  const resumeTask = useTasklistStore((s) => s.resumeTask);

  const circularAvatars = useUserPreferencesStore((s) => s.circularAvatars);
  const [showCompleted, setShowCompleted] = useState(true);
  // Skip flow lives at the panel level (not per-row) so the ConfirmDialog
  // mounts once and shares state with whichever failed row triggered it.
  const [pendingSkipTask, setPendingSkipTask] = useState<Task | null>(null);
  const [skipError, setSkipError] = useState<string | null>(null);
  // Stop flow mirrors the skip flow: per-row Stop opens a panel-level
  // ConfirmDialog (stopping kills the in-flight run, so it warrants one).
  // Resume is a direct action; failures surface in the body error strip.
  const [pendingStopTask, setPendingStopTask] = useState<Task | null>(null);
  const [stopError, setStopError] = useState<string | null>(null);
  const [resumeError, setResumeError] = useState<string | null>(null);
  // Per-task stop/resume is a project-workspace capability. Team scopes keep
  // their existing surface (teams are headed for retirement; their UI stays
  // frozen), so the handlers — and therefore the affordances — only exist
  // for project scopes.
  const canControlTasks = scope?.kind === "project";

  const allTasks = current?.groups.flatMap((g) => g.tasks) ?? [];
  const doneCount = allTasks.filter((t) => t.status === "completed").length;
  const totalCount = allTasks.length;

  // Auto-scroll the body to the bottom when a task is appended to the
  // *current* tasklist. We track (tasklistId, totalCount) so switching
  // tasklists re-baselines without scrolling.
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const prevTaskCountRef = useRef<{ tasklistId: string | null; count: number }>(
    { tasklistId: null, count: 0 },
  );
  useEffect(() => {
    const tasklistId = current?.id ?? null;
    const prev = prevTaskCountRef.current;
    if (prev.tasklistId === tasklistId && totalCount > prev.count) {
      const el = bodyRef.current;
      if (el) {
        // Use rAF so the new task row has been laid out before we measure.
        requestAnimationFrame(() => {
          el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
        });
      }
    }
    prevTaskCountRef.current = { tasklistId, count: totalCount };
  }, [totalCount, current?.id]);
  // Surface the inline Resume button only when the tasklist is Paused AND has
  // at least one Pending task. A Paused tasklist with
  // nothing Pending has nothing to resume into, so we keep the kebab as the
  // sole entry point in that edge case.
  const hasPendingTasks = allTasks.some((t) => t.status === "pending");
  const showInlineResume =
    !!scope && current?.status === "paused" && hasPendingTasks;

  return (
    <div
      className="relative flex-1 flex flex-col rounded-[16px] overflow-hidden border"
      style={{
        backgroundColor: "var(--bg-secondary)",
        borderColor: "var(--border-primary)",
        boxShadow: "0 1px 2px rgba(0,0,0,0.04), 0 4px 12px rgba(0,0,0,0.04)",
      }}
    >
      {/* Header */}
      <div className="px-5 pt-5 pb-3 shrink-0">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-center gap-2 min-w-0">
            <Tooltip
              placement="top"
              label={current?.title ?? "No tasklist"}
              className="min-w-0"
            >
              <h3
                className="text-[18px] font-semibold truncate"
                style={{ color: "var(--text-primary)" }}
              >
                {current?.title ?? "No tasklist"}
              </h3>
            </Tooltip>
            {current && <TasklistStatusPill status={current.status} />}
            {showInlineResume && current && scope && (
              <ResumeButton scope={scope} tasklistId={current.id} />
            )}
          </div>
          {scope ? (
            <TasklistMoreMenu scope={scope} tasklist={current} />
          ) : (
            <button
              type="button"
              disabled
              aria-label="More options"
              className="shrink-0 w-[28px] h-[28px] rounded-[8px] flex items-center justify-center opacity-50"
              style={{ color: "var(--text-tertiary)" }}
            >
              <span className="text-[18px] tracking-tighter leading-none">⋯</span>
            </button>
          )}
        </div>

        {current?.description && (
          <Tooltip placement="top" label={current.description} className="mt-1.5">
            <p
              className="text-[12px] truncate"
              style={{ color: "var(--text-tertiary)" }}
            >
              {current.description}
            </p>
          </Tooltip>
        )}
      </div>

      {/* Progress + show-completed toggle (banded edge-to-edge block) */}
      {current && (
        <div
          className="px-5 py-3 flex items-center justify-between shrink-0 border-y"
          style={{ borderColor: "var(--border-primary)" }}
        >
          <div className="flex items-baseline gap-2">
            <span
              className="text-[14px] font-semibold tabular-nums"
              style={{ color: "var(--text-primary)" }}
            >
              {doneCount}/{totalCount}
            </span>
            <span
              className="text-[13px]"
              style={{ color: "var(--text-secondary)" }}
            >
              Tasks
            </span>
          </div>
          <button
            type="button"
            onClick={() => setShowCompleted((v) => !v)}
            className="flex items-center gap-2 group"
          >
            {/* iOS-style toggle */}
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
              className="text-[12px]"
              style={{ color: "var(--text-secondary)" }}
            >
              Show completed
            </span>
          </button>
        </div>
      )}

      {/* Body */}
      <div ref={bodyRef} className="flex-1 min-h-0 overflow-y-auto pb-2">
        {error && (
          <div
            className="mx-5 mb-2 px-3 py-2 rounded-[10px] text-[12px]"
            style={{
              backgroundColor: "rgba(244,63,94,0.08)",
              color: "#be123c",
            }}
          >
            {error}
          </div>
        )}

        {resumeError && (
          <div
            className="mx-5 mb-2 px-3 py-2 rounded-[10px] text-[12px]"
            style={{
              backgroundColor: "rgba(244,63,94,0.08)",
              color: "#be123c",
            }}
          >
            Failed to resume task: {resumeError}
          </div>
        )}

        {current ? (
          <TasklistContent
            scope={scope}
            tasklist={current}
            coordinatorId={coordinatorId}
            agentNameMap={agentNameMap}
            agentEmojiMap={agentEmojiMap}
            fallbackEmoji={fallbackEmoji}
            circularAvatars={circularAvatars}
            showCompleted={showCompleted}
            onSkipTask={(task) => {
              setSkipError(null);
              setPendingSkipTask(task);
            }}
            onStopTask={
              canControlTasks
                ? (task) => {
                  setStopError(null);
                  setPendingStopTask(task);
                }
                : undefined
            }
            onResumeTask={
              canControlTasks
                ? (task) => {
                  setResumeError(null);
                  void (async () => {
                    if (!scope || !current) return;
                    try {
                      await resumeTask(scope, current.id, task.id);
                    } catch (err) {
                      setResumeError(
                        err instanceof Error ? err.message : String(err),
                      );
                    }
                  })();
                }
                : undefined
            }
            onTaskClick={onTaskClick}
          />
        ) : (
          <div className="px-5 py-10 h-full flex flex-col items-center justify-center gap-3 text-center">
            {loading ? (
              <span
                className="text-[12px]"
                style={{ color: "var(--text-tertiary)" }}
              >
                Loading…
              </span>
            ) : (
              <>
                <span
                  className="text-[13px] font-medium"
                  style={{ color: "var(--text-primary)" }}
                >
                  No tasklist yet.
                </span>
              </>
            )}
          </div>
        )}
      </div>


      {/* Composer — hidden when there is no tasklist */}
      {current && (
        <TasklistComposer
          scope={scope}
          tasklist={current}
          teamMembers={teamMembers}
          agentNameMap={agentNameMap}
          agentEmojiMap={agentEmojiMap}
          fallbackEmoji={fallbackEmoji}
        />
      )}

      <ConfirmDialog
        open={!!pendingSkipTask}
        title="Skip this task?"
        message={
          <div className="space-y-2">
            <p>
              Mark this task as <strong>skipped</strong> and let the tasklist
              continue past it. Skipped tasks are treated as terminal — the
              feeder will move on without them. This can&apos;t be undone.
            </p>
            {pendingSkipTask && (
              <p
                className="text-[12px] font-mono px-2 py-1 rounded"
                style={{
                  backgroundColor: "var(--bg-tertiary)",
                  color: "var(--text-secondary)",
                }}
              >
                {pendingSkipTask.prompt.split("\n")[0]?.trim() ||
                  pendingSkipTask.prompt}
              </p>
            )}
            {current &&
              pendingSkipTask &&
              current.status === "failed" &&
              current.groups
                .flatMap((g) => g.tasks)
                .filter(
                  (t) => t.id !== pendingSkipTask.id && t.status === "failed",
                ).length === 0 && (
                <p className="text-[12px] opacity-80">
                  This is the last failed task — the tasklist will go back to
                  Active and resume from this group.
                </p>
              )}
            {skipError && (
              <p className="text-[12px]" style={{ color: "#be123c" }}>
                {skipError}
              </p>
            )}
          </div>
        }
        confirmLabel="Skip"
        onConfirm={async () => {
          if (!scope || !current || !pendingSkipTask) return;
          try {
            await skipTask(scope, current.id, pendingSkipTask.id);
            setPendingSkipTask(null);
          } catch (err) {
            setSkipError(err instanceof Error ? err.message : String(err));
            throw err;
          }
        }}
        onCancel={() => {
          setPendingSkipTask(null);
          setSkipError(null);
        }}
      />

      <ConfirmDialog
        open={!!pendingStopTask}
        title="Stop this task?"
        message={
          <div className="space-y-2">
            <p>
              Mark this task as <strong>stopped</strong> and kill its
              in-flight run. Stopped tasks are not terminal — you can resume
              them later to re-queue them for dispatch.
            </p>
            {pendingStopTask && (
              <p
                className="text-[12px] font-mono px-2 py-1 rounded"
                style={{
                  backgroundColor: "var(--bg-tertiary)",
                  color: "var(--text-secondary)",
                }}
              >
                {pendingStopTask.prompt.split("\n")[0]?.trim() ||
                  pendingStopTask.prompt}
              </p>
            )}
            {stopError && (
              <p className="text-[12px]" style={{ color: "#be123c" }}>
                {stopError}
              </p>
            )}
          </div>
        }
        confirmLabel="Stop"
        onConfirm={async () => {
          if (!scope || !current || !pendingStopTask) return;
          try {
            await stopTask(scope, current.id, pendingStopTask.id);
            setPendingStopTask(null);
          } catch (err) {
            setStopError(err instanceof Error ? err.message : String(err));
            throw err;
          }
        }}
        onCancel={() => {
          setPendingStopTask(null);
          setStopError(null);
        }}
      />

      {/* Output preview overlay — fills this panel when a file is opened
          from the Outputs widget. */}
      <TasklistOutputPortal />
    </div>
  );
}
