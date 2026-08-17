import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import {
  FileText,
  X,
  CassetteTape,
  Loader2,
  Bot,
  ChevronRight,
} from "lucide-react";
import { useIsDark } from "../../stores/userPreferencesStore";
import { useChatStore, pendingSyncFormForThread, hasPendingSyncFormForThread, isFormMinimized } from "../../stores/chatStore";
import { useDraftStore, type DraftAttachment } from "../../stores/draftStore";
import { useProjectChatChannel } from "../../hooks/useProjectChatChannel";
import { TaskProgressWidget } from "../tasklists/TaskProgressWidget";
import { teamTintColor } from "../../lib/agentColors";
import { displayOutputFilename } from "../../lib/expectedOutputs";
import { MessageList } from "../chat/MessageList";
import { ChatInput } from "../chat/ChatInput";
import { TypingIndicator } from "../chat/TypingIndicator";
import { PendingFormOverlay } from "../chat/PendingFormOverlay";
import { MinimizedFormBar } from "../chat/MinimizedFormBar";
import { AsyncFormRequestCard } from "../chat/AsyncFormRequestCard";
import { InlineTasklistPanel } from "../tasklist/InlineTasklistPanel";
import { TaskDetailModal } from "../tasklist/TaskDetailModal";
import { useCurrentAndArchivedTasklistsForScope, useTasklistStore } from "../../stores/tasklistStore";
import { useTasklistOutputStore } from "../../stores/tasklistOutputStore";
import * as api from "../../lib/api";
import { formatPendingFormWaitingLabel } from "../../lib/pendingFormLabel";
import type { Project, Task, Tasklist, TasklistStatus } from "../../types/api";
import type { TasklistScope } from "../../types/api";
import type { AsyncFormRequestMeta, FormAction } from "../../types/form";

const FALLBACK_EMOJI = "🤖";
const EMPTY_DRAFT_ATTACHMENTS: DraftAttachment[] = [];

// ---------------------------------------------------------------------------
// ProjectCopilotOverlay — reuses the real chat stack (MessageList + ChatInput)
// ---------------------------------------------------------------------------

function ProjectCopilotOverlay({
  projectId,
  agentId,
  agentName,
  agentEmoji,
  onClose,
}: {
  projectId: string;
  agentId: string;
  agentName: string;
  agentEmoji: string;
  onClose: () => void;
}) {
  const {
    projectKey,
    isTyping,
    streamingText,
    activeToolCalls,
    hasMessages,
    hasTrailingSkillLoad,
    selectedAgentProfile,
    messagesLoading,
    canSend,
    sendProjectMessage,
  } = useProjectChatChannel(projectId, agentName, agentEmoji);

  const currentDraft = useDraftStore((s) => s.drafts[projectKey] ?? "");
  const currentDraftHtml = useDraftStore((s) => s.draftHtml[projectKey]);
  const currentDraftAttachments = useDraftStore((s) => s.draftAttachments[projectKey] ?? EMPTY_DRAFT_ATTACHMENTS);
  const setDraft = useDraftStore((s) => s.setDraft);
  const setDraftAttachments = useDraftStore((s) => s.setDraftAttachments);
  const clearDraft = useDraftStore((s) => s.clearDraft);

  // Sync form (AskUserQuestionWithForm): swap the composer for the form.
  // Project channels have no thread concept, so this always resolves against
  // the default-thread bucket.
  const pendingForm = useChatStore((s) => pendingSyncFormForThread(s.pendingFormByAgent, projectKey, undefined));
  // Async form (AsyncForm tool): show AsyncFormRequestCard overlay.
  const pendingAsyncFormId = useChatStore((s) => s.pendingAsyncFormIdByChannel[projectKey] ?? null);
  // Minimized flag is shared by both form kinds — project channels have no
  // thread concept, so both `clearPendingForm` and `clearPendingAsyncFormId`
  // key their minimized-flag cleanup off the plain channel key.
  const formMinimized = useChatStore((s) => isFormMinimized(s.minimizedFormByKey, projectKey, undefined));
  const setFormMinimized = useChatStore((s) => s.setFormMinimized);
  // Forwarded to `AsyncFormRequestCard`'s `onWithdraw` below — same
  // client-side draft-recovery wiring `ChatView.tsx`'s async nudge card and
  // `PendingFormOverlay` use (see `AskUserQuestionForm`'s `onWithdraw` prop).
  const stashWithdrawnFormDraft = useChatStore((s) => s.stashWithdrawnFormDraft);
  const messages = useChatStore((s) => s.messages);
  const pendingAsyncFormMeta = useMemo<AsyncFormRequestMeta | null>(() => {
    if (!pendingAsyncFormId) return null;
    const msg = messages.find(
      (m) => m.event_type === "form_request" &&
        (m.metadata as unknown as AsyncFormRequestMeta | null)?.form_id === pendingAsyncFormId,
    );
    return msg ? (msg.metadata as unknown as AsyncFormRequestMeta) : null;
  }, [pendingAsyncFormId, messages]);

  return (
    <motion.div
      key="project-copilot-overlay"
      className="absolute inset-0 z-30 flex flex-col rounded-[20px] overflow-hidden border border-[var(--border-primary)]"
      style={{
        backgroundColor: "var(--bg-secondary)",
        boxShadow: "0 1px 2px rgba(0,0,0,0.04), 0 4px 12px rgba(0,0,0,0.06)",
      }}
      initial={{ opacity: 0, scale: 0.98 }}
      animate={{ opacity: 1, scale: 1 }}
      exit={{ opacity: 0, scale: 0.98 }}
      transition={{
        scale: { type: "spring", stiffness: 320, damping: 30, mass: 0.7 },
        opacity: { duration: 0.18, ease: "easeOut" },
      }}
    >
      {/* Header */}
      <div className="px-4 py-3 flex items-center gap-2 shrink-0 border-b" style={{ borderColor: "var(--border-primary)" }}>
        <span
          className="w-[24px] h-[24px] rounded-[8px] flex items-center justify-center"
          style={{ backgroundColor: "var(--bg-tertiary)", color: "var(--accent)" }}
        >
          <Bot className="w-[13px] h-[13px]" />
        </span>
        <span className="text-[13px] font-semibold flex-1" style={{ color: "var(--text-primary)" }}>
          Project co-pilot
        </span>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close co-pilot"
          className="w-[26px] h-[26px] rounded-[6px] flex items-center justify-center hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
          style={{ color: "var(--text-secondary)" }}
        >
          <X className="w-[14px] h-[14px]" />
        </button>
      </div>

      {/* Body */}
      <div className="flex-1 min-h-0 flex flex-col">
        {messagesLoading ? (
          <div className="flex flex-1 items-center justify-center">
            <div className="h-[24px] w-[24px] animate-spin rounded-full border-2 border-[var(--border-primary)] border-t-[var(--accent)]" />
          </div>
        ) : (
          <>
            <div className="flex-1 min-h-0 flex flex-col">
              {hasMessages && <MessageList key={projectKey} />}

              {!messagesLoading && !hasMessages && (
                <div className="flex flex-1 flex-col items-center justify-center gap-3 px-8 text-center">
                  <div
                    className="w-[52px] h-[52px] rounded-[16px] flex items-center justify-center"
                    style={{ backgroundColor: "var(--bg-tertiary)", color: "var(--accent)" }}
                  >
                    <Bot className="w-[22px] h-[22px]" />
                  </div>
                  <span className="text-[14px] font-semibold" style={{ color: "var(--text-primary)" }}>
                    Chat with the project agent
                  </span>
                  <span className="text-[12px] max-w-[280px]" style={{ color: "var(--text-tertiary)" }}>
                    Ask questions, give directions, and steer the project work.
                  </span>
                </div>
              )}
            </div>

            <div className="relative px-[10px] @sm:px-[12px] pb-[12px] pt-[8px] flex-shrink-0">
              {/* Warmup typing indicator — floats above input while agent is active
               *  but no content has arrived yet. Mirrors CopilotOverlay in TeamsView. */}
              {isTyping && !streamingText && activeToolCalls.length === 0 && !hasTrailingSkillLoad && (
                <div className="absolute bottom-full left-0 right-0 px-[10px] @sm:px-[12px] pointer-events-none">
                  <TypingIndicator emoji={selectedAgentProfile?.emoji ?? agentEmoji} />
                </div>
              )}
              {/* SYNC form pending — `PendingFormOverlay` owns this area
               *  while expanded (the agent's turn is genuinely suspended,
               *  so hard-blocking the composer is correct here). Only
               *  surface the minimized bar directly; ChatInput stays
               *  unmounted below until the sync form clears or is
               *  minimized. */}
              {pendingForm && formMinimized && (
                <MinimizedFormBar
                  title={formatPendingFormWaitingLabel(selectedAgentProfile?.name ?? agentName, pendingForm.fields.length)}
                  onExpand={() => setFormMinimized(projectKey, undefined, false)}
                />
              )}

              {/* Async form pinned nudge — non-blocking. A `mode: "async"`
               *  form never suspends the agent's turn, so unlike the sync
               *  form above it must never take over the composer — it sits
               *  in normal flow just above ChatInput (rendered below,
               *  unconditionally whenever no sync form is pending),
               *  expanded or minimized, with the composer always still
               *  mounted and usable underneath it. */}
              {!pendingForm && pendingAsyncFormMeta && pendingAsyncFormId && (
                <div className="pb-[8px]">
                  {formMinimized ? (
                    <MinimizedFormBar
                      title={formatPendingFormWaitingLabel(selectedAgentProfile?.name ?? agentName, pendingAsyncFormMeta.spec.fields.length)}
                      onExpand={() => setFormMinimized(projectKey, undefined, false)}
                    />
                  ) : (
                    <AsyncFormRequestCard
                      // Same remount-on-supersession reasoning as
                      // `ChatView.tsx`'s async nudge card: without an
                      // explicit key here, this render slot's position alone
                      // was the (static) implicit key, so a new async form
                      // superseding a still-pending one on this project
                      // channel's slot would reuse this instance and bleed
                      // its typed answers into the new form.
                      key={pendingAsyncFormMeta.form_id}
                      meta={pendingAsyncFormMeta}
                      agentId={agentId}
                      onSubmit={async (answers) => {
                        await api.submitAsyncFormAnswerForProject(projectId, pendingAsyncFormId, answers);
                        useChatStore.getState().clearPendingAsyncFormId(projectKey);
                      }}
                      // Exit affordance — Cancel / Regenerate / Something else
                      // all collapse onto the same dismiss the old standalone
                      // Decline button used: nothing is suspended server-side
                      // for an async form to differentiate them against
                      // (`action`/`note` unused for the same reason).
                      onAction={async (_action: FormAction, _note?: string) => {
                        await api.dismissAsyncFormForProject(projectId, pendingAsyncFormId);
                        useChatStore.getState().clearPendingAsyncFormId(projectKey);
                      }}
                      onMinimize={() => setFormMinimized(projectKey, undefined, true)}
                      onWithdraw={stashWithdrawnFormDraft}
                    />
                  )}
                </div>
              )}

              {!pendingForm && (
                <ChatInput
                  conversationId={projectKey}
                  agentId={agentId}
                  fileCapabilitiesSupported
                  disabled={!canSend}
                  isProcessing={isTyping}
                  onStop={() => api.cancelProjectRun(projectId).catch(() => {})}
                  onUploadAttachment={(file) => api.uploadProjectAttachment(projectId, file)}
                  onAddFolderReference={(path) => api.uploadProjectFolderReference(projectId, path)}
                  onDeleteAttachment={(id) => api.deleteProjectAttachment(projectId, id)}
                  onSend={(content, attachmentIds) => {
                    clearDraft(projectKey);
                    sendProjectMessage(projectId, content, attachmentIds ?? []);
                  }}
                  placeholder="Ask the project agent…"
                  initialDraft={currentDraft}
                  initialDraftHtml={currentDraftHtml}
                  initialDraftAttachments={currentDraftAttachments.length > 0 ? currentDraftAttachments : undefined}
                  onUnmount={(text, html, id) => {
                    if (id && text.trim()) setDraft(id, text, html);
                    else if (id) clearDraft(id);
                  }}
                  onUnmountAttachments={(attachments, id) => {
                    if (id) setDraftAttachments(id, attachments);
                  }}
                />
              )}
              <PendingFormOverlay channelKey={projectKey} />
            </div>
          </>
        )}
      </div>
    </motion.div>
  );
}

// ---------------------------------------------------------------------------
// ProjectOutputsAndTasklistsWidget — clickable outputs + tasklist switcher
// ---------------------------------------------------------------------------

const STATUS_CONFIG: Record<TasklistStatus, { bg: string; fg: string; label: string }> = {
  active: { bg: "rgba(59,130,246,0.12)", fg: "rgb(37,99,235)", label: "active" },
  paused: { bg: "rgba(217,119,6,0.14)", fg: "rgb(180,83,9)", label: "paused" },
  completed: { bg: "rgba(16,185,129,0.12)", fg: "rgb(5,150,105)", label: "done" },
  failed: { bg: "rgba(244,63,94,0.12)", fg: "rgb(190,18,60)", label: "failed" },
  cancelled: { bg: "var(--bg-tertiary)", fg: "var(--text-tertiary)", label: "cancelled" },
};

function ProjectOutputsAndTasklistsWidget({
  scope,
  currentTasklist,
  allTasklists,
  agentNameMap,
  agentEmojiMap,
}: {
  scope: TasklistScope | null;
  currentTasklist: Tasklist | null;
  allTasklists: Tasklist[];
  agentNameMap: Record<string, string>;
  agentEmojiMap: Record<string, string>;
}) {
  const openOutput = useTasklistOutputStore((s) => s.open);
  const setSelectedTasklist = useTasklistStore((s) => s.setSelectedTasklist);

  const tiles = useMemo(() => {
    if (!currentTasklist) return [] as { task: Task; filename: string }[];
    const out: { task: Task; filename: string }[] = [];
    for (const group of currentTasklist.groups) {
      for (const task of group.tasks) {
        for (const filename of task.expected_outputs ?? []) {
          out.push({ task, filename });
        }
      }
    }
    return out;
  }, [currentTasklist]);

  const handleOutputClick = (e: React.MouseEvent<HTMLButtonElement>, task: Task, filename: string) => {
    if (!scope || !currentTasklist) return;
    const rect = e.currentTarget.getBoundingClientRect();
    openOutput({
      scope,
      tasklistId: currentTasklist.id,
      filename,
      ownerAgentId: task.owner_agent_id,
      rect: { left: rect.left, top: rect.top, width: rect.width, height: rect.height },
    });
  };

  return (
    <div className="flex-1 flex flex-col rounded-[20px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] overflow-hidden">
      {/* Outputs section */}
      <div className="flex items-center justify-between px-[14px] py-[10px] flex-shrink-0">
        <div className="flex items-center gap-[8px]">
          <span className="text-[var(--text-secondary)]"><FileText className="w-[14px] h-[14px]" /></span>
          <span className="text-[13px] font-semibold text-[var(--text-primary)]">Outputs</span>
        </div>
        <span className="text-[11px] text-[var(--text-tertiary)]">
          {tiles.length} {tiles.length === 1 ? "file" : "files"}
        </span>
      </div>
      <div className="px-[12px] pb-[12px]">
        {tiles.length === 0 ? (
          <div className="py-[24px] flex items-center justify-center text-[12px] text-[var(--text-tertiary)]">
            {currentTasklist ? "No expected outputs declared yet." : "No active tasklist."}
          </div>
        ) : (
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-[8px]">
            {tiles.map(({ task, filename }, idx) => {
              const ownerName = agentNameMap[task.owner_agent_id] ?? task.owner_agent_id;
              const ownerEmoji = agentEmojiMap[task.owner_agent_id] ?? FALLBACK_EMOJI;
              const ready = task.status === "completed";
              const displayName = displayOutputFilename(filename);
              const tooltip = `${displayName}\n${task.prompt} — ${ownerName}`;
              return (
                <button
                  key={`${task.id}:${filename}:${idx}`}
                  type="button"
                  onClick={(e) => handleOutputClick(e, task, filename)}
                  className="group flex items-center gap-[8px] rounded-[10px] px-[10px] py-[8px] text-left transition-colors hover:bg-[var(--bg-hover)] cursor-pointer"
                  style={{ backgroundColor: "var(--bg-tertiary)" }}
                  title={tooltip}
                >
                  <span
                    className="shrink-0 w-[26px] h-[26px] rounded-[6px] flex items-center justify-center"
                    style={{
                      backgroundColor: ready ? "var(--accent)" : "var(--bg-secondary)",
                      color: ready ? "#fff" : "var(--text-secondary)",
                    }}
                  >
                    <FileText size={13} />
                  </span>
                  <div className="flex-1 min-w-0">
                    <div className="truncate text-[12.5px] font-medium" style={{ color: "var(--text-primary)" }}>
                      {displayName}
                    </div>
                    <div className="truncate text-[10.5px] text-[var(--text-tertiary)]">
                      {ready ? "ready" : task.status.replace("_", " ")}
                    </div>
                  </div>
                  <span className="shrink-0 leading-none text-[16px]" aria-label={ownerName} title={ownerName}>
                    {ownerEmoji}
                  </span>
                </button>
              );
            })}
          </div>
        )}
      </div>

      {/* Tasklists section */}
      {allTasklists.length > 0 && (
        <>
          <div className="flex items-center justify-between px-[14px] py-[10px] flex-shrink-0">
            <div className="flex items-center gap-[8px]">
              <span className="text-[var(--text-secondary)]"><CassetteTape className="w-[14px] h-[14px]" /></span>
              <span className="text-[13px] font-semibold text-[var(--text-primary)]">Tasklists</span>
            </div>
            <span className="text-[11px] text-[var(--text-tertiary)]">
              {allTasklists.length} {allTasklists.length === 1 ? "tasklist" : "tasklists"}
            </span>
          </div>
          <div className="px-[12px] pb-[12px] flex flex-col gap-[6px]">
            {allTasklists.map((tl) => {
              const allTasks = tl.groups.flatMap((g) => g.tasks);
              const done = allTasks.filter((t) => t.status === "completed").length;
              const c = STATUS_CONFIG[tl.status];
              const isSelected = tl.id === currentTasklist?.id;
              return (
                <button
                  key={tl.id}
                  type="button"
                  onClick={() => scope && setSelectedTasklist(scope, tl.id)}
                  aria-pressed={isSelected}
                  className="group flex items-center gap-[8px] rounded-[10px] px-[10px] py-[8px] text-left transition-colors hover:bg-[var(--bg-hover)] cursor-pointer"
                  style={{
                    backgroundColor: isSelected ? "var(--sidebar-active-bg)" : "var(--bg-tertiary)",
                  }}
                  title={isSelected ? `${tl.title} (showing)` : tl.title}
                >
                  <div className="flex-1 min-w-0">
                    <div className="truncate text-[12.5px] font-medium flex items-center gap-[6px]" style={{ color: "var(--text-primary)" }}>
                      <span className="truncate">{tl.title}</span>
                      {isSelected && (
                        <span className="shrink-0 px-[6px] rounded-full text-[9.5px] font-semibold uppercase tracking-wide" style={{ backgroundColor: "var(--accent)", color: "#fff" }}>
                          showing
                        </span>
                      )}
                    </div>
                    <div className="truncate text-[10.5px] text-[var(--text-tertiary)]">{done}/{allTasks.length} tasks</div>
                  </div>
                  <span className="shrink-0 px-[8px] py-[1px] rounded-full text-[10px] font-medium" style={{ backgroundColor: c.bg, color: c.fg }}>
                    {c.label}
                  </span>
                </button>
              );
            })}
          </div>
        </>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// ProjectParallaxRightColumn — sticky progress widget + co-pilot + outputs
// ---------------------------------------------------------------------------

function ProjectParallaxRightColumn({
  projectId,
  scope,
  allTasks,
  currentTasklist,
  allTasklists,
  agentIds,
  agentNameMap,
  agentEmojiMap,
  isDark,
  projectEmoji,
  agentId,
  agentName,
  agentEmoji,
}: {
  projectId: string;
  scope: TasklistScope;
  allTasks: Task[];
  currentTasklist: Tasklist | null;
  allTasklists: Tasklist[];
  agentIds: string[];
  agentNameMap: Record<string, string>;
  agentEmojiMap: Record<string, string>;
  isDark: boolean;
  projectEmoji: string;
  agentId: string;
  agentName: string;
  agentEmoji: string;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const backLayerRef = useRef<HTMLDivElement>(null);
  const tintRef = useRef<HTMLDivElement>(null);
  const rafRef = useRef<number | null>(null);
  const [copilotOpen, setCopilotOpen] = useState(false);
  // A pending sync form means the agent is suspended waiting for the operator.
  // Open the co-pilot automatically so the form is immediately reachable.
  const hasPendingForm = useChatStore(
    (s) => hasPendingSyncFormForThread(s.pendingFormByAgent, `project:${projectId}`, undefined),
  );

  const applyParallax = useCallback(() => {
    rafRef.current = null;
    const scrollTop = scrollRef.current?.scrollTop ?? 0;
    const lift = Math.min(1, scrollTop / 120);
    const scale = 1 - 0.06 * lift;
    const blurLift = Math.min(1, scrollTop / 480);
    const blur = 18 * blurLift;
    const innerStop = 100 - 70 * blurLift;
    const tintAlpha = (isDark ? 0.22 : 0.16) * lift;
    if (backLayerRef.current) {
      backLayerRef.current.style.transform = `scale(${scale})`;
      backLayerRef.current.style.filter = `blur(${blur}px)`;
      if (blurLift > 0.005) {
        const m = `radial-gradient(ellipse farthest-corner at center, black ${innerStop}%, transparent 100%)`;
        backLayerRef.current.style.maskImage = m;
        backLayerRef.current.style.webkitMaskImage = m;
        backLayerRef.current.style.maskComposite = "";
        backLayerRef.current.style.webkitMaskComposite = "";
      } else {
        backLayerRef.current.style.maskImage = "";
        backLayerRef.current.style.webkitMaskImage = "";
      }
    }
    if (tintRef.current) {
      tintRef.current.style.backgroundColor = isDark
        ? `rgba(0,0,0,${tintAlpha})`
        : `rgba(15,23,42,${tintAlpha})`;
    }
  }, [isDark]);

  const handleScroll = useCallback(() => {
    if (rafRef.current != null) return;
    rafRef.current = requestAnimationFrame(applyParallax);
  }, [applyParallax]);

  useEffect(() => {
    applyParallax();
    return () => {
      if (rafRef.current != null) {
        cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
    };
  }, [applyParallax]);

  return (
    <div className="relative min-h-0 flex flex-col">
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="flex-1 min-h-0 flex flex-col overflow-y-auto isolate pr-[8px]"
      >
        <div className="sticky top-0 z-0 flex-shrink-0 h-[342px] w-full">
          <div
            ref={backLayerRef}
            className="absolute inset-0 flex flex-col gap-[12px]"
            style={{ transformOrigin: "center top" }}
          >
            {/* Co-pilot launcher */}
            <button
              type="button"
              onClick={() => setCopilotOpen(true)}
              className="h-[50px] flex-shrink-0 flex items-center gap-[10px] px-[14px] rounded-[20px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] hover:bg-[var(--bg-hover)] transition-colors text-left cursor-pointer"
              aria-label="Open project co-pilot"
            >
              <span
                className="w-[28px] h-[28px] rounded-[10px] flex items-center justify-center flex-shrink-0"
                style={{ backgroundColor: "var(--bg-tertiary)", color: "var(--accent)" }}
              >
                <Bot className="w-[14px] h-[14px]" />
              </span>
              <span className="text-[13px] font-semibold flex-1 truncate" style={{ color: "var(--text-primary)" }}>
                Open project co-pilot
              </span>
              {hasPendingForm && (
                <span
                  className="shrink-0 px-[8px] py-[2px] rounded-full text-[10px] font-semibold animate-pulse"
                  style={{ backgroundColor: "rgb(245,158,11)", color: "#fff" }}
                >
                  Form waiting
                </span>
              )}
              <ChevronRight className="w-[16px] h-[16px] flex-shrink-0" style={{ color: "var(--text-tertiary)" }} aria-hidden />
            </button>

            {/* Progress widget */}
            <div className="flex-1 min-h-0 relative">
              <TaskProgressWidget
                allTasks={allTasks}
                agentIds={agentIds}
                agentNameMap={agentNameMap}
                agentEmojiMap={agentEmojiMap}
                isDark={isDark}
                fallbackEmoji={projectEmoji}
              />
            </div>

            {/* Tint overlay (imperatively driven by rAF) */}
            <div
              ref={tintRef}
              aria-hidden
              className="pointer-events-none absolute inset-0 rounded-[20px]"
            />
          </div>
        </div>

        {/* Outputs + Tasklists widget scrolls upward over the sticky panel */}
        <div className="relative z-10 mt-[16px] grow shrink-0 w-full rounded-[20px] flex flex-col">
          <ProjectOutputsAndTasklistsWidget
            scope={scope}
            currentTasklist={currentTasklist}
            allTasklists={allTasklists}
            agentNameMap={agentNameMap}
            agentEmojiMap={agentEmojiMap}
          />
        </div>
      </div>

      {/* Co-pilot overlay */}
      <AnimatePresence>
        {copilotOpen && (
          <ProjectCopilotOverlay
            projectId={projectId}
            agentId={agentId}
            agentName={agentName}
            agentEmoji={agentEmoji}
            onClose={() => setCopilotOpen(false)}
          />
        )}
      </AnimatePresence>
    </div>
  );
}

// ---------------------------------------------------------------------------
// ProjectWorkspace — scope-keyed tasklist data, real Teams components
// ---------------------------------------------------------------------------

export function ProjectWorkspace({
  project,
  projectId,
  agentId,
  agentName,
  agentEmoji,
}: {
  project: Project;
  projectId: string;
  agentId: string;
  agentName: string;
  agentEmoji: string;
}) {
  const isDark = useIsDark();
  const agents = useChatStore((s) => s.agents);
  const fetchAgents = useChatStore((s) => s.fetchAgents);

  const projectScope: TasklistScope = useMemo(() => ({ kind: "project", id: projectId }), [projectId]);

  const { current: currentTasklist, all: allTasklists, loading } =
    useCurrentAndArchivedTasklistsForScope(projectScope);

  const [openTaskId, setOpenTaskId] = useState<string | null>(null);

  useEffect(() => {
    if (agents.length === 0) fetchAgents();
  }, [agents.length, fetchAgents]);

  const agentNameMap = useMemo(() => {
    const m: Record<string, string> = {};
    for (const a of agents) m[a.agent_id] = a.name;
    return m;
  }, [agents]);

  const agentEmojiMap = useMemo(() => {
    const m: Record<string, string> = {};
    for (const a of agents) { if (a.emoji) m[a.agent_id] = a.emoji; }
    return m;
  }, [agents]);

  const allTasks = useMemo(
    () => currentTasklist?.groups.flatMap((g) => g.tasks) ?? [],
    [currentTasklist],
  );

  // Agents to render as bars in the "Tasks by agent" widget. Projects have no
  // stored agent roster (unlike Teams), so the set of agents genuinely "in the
  // project" is: the coordinator (always a member, even though it delegates
  // rather than owning tasks) plus any agent that has ever been assigned a task
  // here. The coordinator is listed first so it's always visible — previously it
  // never appeared because it owns no tasks. Counts of 0 are intentional.
  const agentIds = useMemo(() => {
    const seen = new Set<string>();
    const ids: string[] = [];
    if (agentId && !seen.has(agentId)) {
      seen.add(agentId);
      ids.push(agentId);
    }
    for (const t of allTasks) {
      const effectiveId = t.assignment?.owner_agent_id || t.owner_agent_id;
      if (effectiveId && !seen.has(effectiveId)) {
        seen.add(effectiveId);
        ids.push(effectiveId);
      }
    }
    return ids.slice(0, 6);
  }, [allTasks, agentId]);

  const projectEmoji = project.emoji ?? "📁";

  // The grid below is always mounted: InlineTasklistPanel owns the tasklist SSE
  // subscription + initial hydrate (it's what sets `loading`). If we swapped the
  // whole grid out for a spinner while `loading`, the panel would unmount, its
  // hydrate would resolve and flip `loading` back to false, the grid would
  // remount and hydrate again → for a freshly-created project with zero
  // tasklists this oscillates forever (frozen UI + a torn-down SSE that misses
  // the first tasklist.created event). So the first-load spinner is an OVERLAY
  // over the live grid, never a replacement for it.
  const showInitialSpinner = loading && allTasklists.length === 0;

  return (
    <div className="relative flex flex-1 flex-col min-h-0">
      <TaskDetailModal
        open={openTaskId !== null}
        scope={projectScope}
        taskId={openTaskId ?? ""}
        onClose={() => setOpenTaskId(null)}
      />

      <div
        className="flex-1 overflow-y-auto px-[20px] py-[20px]"
        style={{
          backgroundImage: `linear-gradient(180deg, ${teamTintColor(project.name, isDark)}${isDark ? "26" : "1F"} 0%, ${teamTintColor(project.name, isDark)}0A 38%, transparent 60%)`,
        }}
      >
        <div className="grid grid-cols-1 lg:grid-cols-2 gap-[16px] h-full min-h-[560px]">
          {/* Left column — real Teams InlineTasklistPanel with project scope.
              TasklistOutputPortal is mounted inside InlineTasklistPanel and
              positions absolute over this panel when an output is open. */}
          <div className="min-h-0 flex">
            <InlineTasklistPanel
              scope={projectScope}
              teamMembers={[]}
              coordinatorId={agentId}
              agentNameMap={agentNameMap}
              agentEmojiMap={agentEmojiMap}
              fallbackEmoji={projectEmoji}
              onTaskClick={setOpenTaskId}
            />
          </div>

          {/* Right column — parallax: progress widget sticky, outputs+tasklists scrolls up */}
          <ProjectParallaxRightColumn
            projectId={projectId}
            scope={projectScope}
            allTasklists={allTasklists}
            allTasks={allTasks}
            currentTasklist={currentTasklist}
            agentIds={agentIds}
            agentNameMap={agentNameMap}
            agentEmojiMap={agentEmojiMap}
            isDark={isDark}
            projectEmoji={projectEmoji}
            agentId={agentId}
            agentName={agentName}
            agentEmoji={agentEmoji}
          />
        </div>
      </div>

      {showInitialSpinner && (
        <div className="absolute inset-0 z-20 flex items-center justify-center bg-[var(--bg-primary)]">
          <Loader2 size={20} className="animate-spin text-[var(--text-tertiary)]" />
        </div>
      )}
    </div>
  );
}
