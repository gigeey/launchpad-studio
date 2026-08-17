import { useEffect, useRef, useState, useCallback, useMemo } from "react";
import { Loader2, Play, Clock } from "lucide-react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { motion } from "framer-motion";
import { usePhaseChatStore, ActiveToolCall } from "../../stores/phaseChatStore";
import { useUserPreferencesStore, useIsDark } from "../../stores/userPreferencesStore";
import { adjustBubbleColor } from "../../lib/colorUtils";
import { ChatInput } from "../chat/ChatInput";
import { ImageThumbnail, FileAttachmentTile } from "../chat/MessageBubble";
import { stripMcpPrefix, describeToolCall } from "../chat/toolCallLabel";
import { TranscriptEntry, Attachment, isUserMessage } from "../../types/api";
import { useWorkflowStore } from "../../stores/workflowStore";
import { useDraftStore } from "../../stores/draftStore";
import type { AttachmentFetcher } from "../../stores/attachmentStore";
import * as api from "../../lib/api";
import type { PhaseStatus, PhaseType } from "../../types/workflow";

interface PhaseChatProps {
  taskId: string;
  phaseId: string;
  phaseName: string;
  projectName: string;
  phaseStatus?: PhaseStatus;
  phaseType?: PhaseType;
  phaseStartedAt?: string | null;
}

function useElapsedTimer(startedAt: string | null | undefined, active: boolean): string {
  const [elapsed, setElapsed] = useState("");

  const compute = useCallback(() => {
    if (!startedAt) return "";
    const start = new Date(startedAt).getTime();
    const diff = Date.now() - start;
    if (diff < 0) return "";
    const secs = Math.floor(diff / 1000);
    const mins = Math.floor(secs / 60);
    const hrs = Math.floor(mins / 60);
    if (hrs > 0) return `${hrs}h ${mins % 60}m`;
    if (mins > 0) return `${mins}m ${secs % 60}s`;
    return `${secs}s`;
  }, [startedAt]);

  useEffect(() => {
    if (!active || !startedAt) { setElapsed(""); return; }
    setElapsed(compute());
    const id = setInterval(() => setElapsed(compute()), 1000);
    return () => clearInterval(id);
  }, [active, startedAt, compute]);

  return elapsed;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function formatTime(ts: string): string {
  const date = new Date(ts);
  if (isNaN(date.getTime())) return "";
  return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });
}

// ---------------------------------------------------------------------------
// Message bubble (matching ChatView style)
// ---------------------------------------------------------------------------

function PhaseChatMessage({ entry, attachmentFetcher }: { entry: TranscriptEntry; attachmentFetcher?: AttachmentFetcher }) {
  const isUser = isUserMessage(entry);
  const isColdStart = entry.event_type === "cold_start";
  const isDark = useIsDark();
  const bubbleColor = useUserPreferencesStore((s) => s.bubbleColor);
  const time = formatTime(entry.ts);

  const attachments = useMemo(() => {
    if (!entry.metadata) return [];
    return ((entry.metadata as Record<string, unknown>)?.attachments as Attachment[] | undefined) ?? [];
  }, [entry.metadata]);

  const imageAttachments = useMemo(() => attachments.filter((a) => a.mime_type?.startsWith("image/")), [attachments]);
  const fileAttachments = useMemo(() => attachments.filter((a) => !a.mime_type?.startsWith("image/")), [attachments]);

  if (isColdStart) return null;

  const entityId = "phase-" + entry.ts;
  const hasAttachments = attachments.length > 0 && attachmentFetcher;

  if (isUser) {
    return (
      <div className="flex flex-col items-end mb-4">
        {hasAttachments && (
          <div className="flex flex-wrap gap-1 justify-end mb-1.5">
            {imageAttachments.map((a) => (
              <ImageThumbnail key={a.id} attachment={a} agentId={entityId} attachmentFetcher={attachmentFetcher} />
            ))}
            {fileAttachments.map((a) => (
              <FileAttachmentTile key={a.id} attachment={a} agentId={entityId} attachmentFetcher={attachmentFetcher} />
            ))}
          </div>
        )}
        <div
          className="max-w-[85%] px-[14px] py-[10px] text-[var(--text-user-message)] text-[15px] leading-[1.45] whitespace-pre-wrap break-words"
          style={{ borderRadius: "16px 4px 16px 16px", backgroundColor: adjustBubbleColor(bubbleColor || "#1164A3", isDark) }}
        >
          {entry.content}
        </div>
        {time && (
          <span className="text-[11px] text-[var(--text-tertiary)] mt-[4px] mr-[4px]">{time}</span>
        )}
      </div>
    );
  }

  // Agent message
  return (
    <div className="flex items-start gap-[10px] mb-4">
      <div className="flex-shrink-0 w-[34px] h-[34px] rounded-[10px] flex items-center justify-center text-[18px] select-none mt-[2px] bg-[var(--bg-secondary)] border border-[var(--border-secondary)]">
        🤖
      </div>
      <div className="flex flex-col items-start min-w-0">
        <span className="text-[14px] font-bold text-[var(--text-primary)] mb-[4px]">Agent</span>
        <div
          className="max-w-[95%] px-[14px] py-[10px] bg-[var(--bg-agent-message)] text-[15px] text-[var(--text-primary)] leading-[1.45] break-words prose prose-sm prose-neutral dark:prose-invert max-w-none [&_p]:my-1 [&_ul]:my-1 [&_ol]:my-1 [&_li]:my-0.5 [&_pre]:my-2 [&_code]:text-[13px]"
          style={{ borderRadius: "4px 16px 16px 16px" }}
        >
          {hasAttachments && fileAttachments.length > 0 && (
            <div className="flex flex-wrap gap-1 mb-1.5">
              {fileAttachments.map((a) => (
                <FileAttachmentTile key={a.id} attachment={a} agentId={entityId} attachmentFetcher={attachmentFetcher} />
              ))}
            </div>
          )}
          <Markdown remarkPlugins={[remarkGfm]}>{entry.content}</Markdown>
        </div>
        {time && (
          <span className="text-[11px] text-[var(--text-secondary)] mt-[4px]">{time}</span>
        )}
      </div>
    </div>
  );
}

function formatElapsed(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const mins = Math.floor(seconds / 60);
  const secs = seconds % 60;
  return `${mins}m ${secs}s`;
}

const AGENT_STATUS_MESSAGES = [
  "Working…", "Cooking…", "Wrestling…", "Brewing…", "Tinkering…",
  "Marinating…", "Simmering…", "Chewing on it…", "Noodling…",
  "Percolating…", "Conjuring…", "Wrangling…",
];

function ToolIndicator({ label, detail, accentColor }: { label: string; detail?: string; accentColor: string }) {
  const connectorColor = "#b0b8c1";
  const detailColor = "#7a8a9e";
  return (
    <div className="mt-[3px]">
      <div className="flex" style={{ height: "16px" }}>
        <div className="flex-shrink-0" style={{ width: "14px", height: "100%", borderLeft: `2px solid ${connectorColor}`, borderBottom: `2px solid ${connectorColor}`, borderRadius: "0 0 0 4px" }} />
        <motion.span key={label} initial={{ opacity: 0, x: -4 }} animate={{ opacity: 1, x: 0 }} className="tool-shimmer self-end text-[12px] font-medium pl-[4px] overflow-hidden text-ellipsis whitespace-nowrap" style={{ marginBottom: "-7px", color: accentColor, maxWidth: "calc(100% - 18px)" }}>
          {label}
        </motion.span>
      </div>
      {detail && (
        <motion.div initial={{ opacity: 0, y: -2 }} animate={{ opacity: 1, y: 0 }} className="text-[11px] mt-[6px] pl-[18px] whitespace-nowrap" style={{ color: detailColor }}>
          {detail}
        </motion.div>
      )}
    </div>
  );
}

function AgentToolIndicator({ detail, startedAt, accentColor }: { detail?: string; startedAt: number; accentColor: string }) {
  const [elapsed, setElapsed] = useState(0);
  const [statusIdx, setStatusIdx] = useState(0);

  useEffect(() => {
    const timer = setInterval(() => setElapsed(Math.floor((Date.now() - startedAt) / 1000)), 1000);
    return () => clearInterval(timer);
  }, [startedAt]);

  useEffect(() => {
    if (elapsed < 3) return;
    setStatusIdx(Math.floor((elapsed - 3) / 5) % AGENT_STATUS_MESSAGES.length);
  }, [elapsed]);

  const connectorColor = "#b0b8c1";
  const detailColor = "#7a8a9e";

  return (
    <div className="mt-[3px]">
      <div className="flex" style={{ height: "16px" }}>
        <div className="flex-shrink-0" style={{ width: "14px", height: "100%", borderLeft: `2px solid ${connectorColor}`, borderBottom: `2px solid ${connectorColor}`, borderRadius: "0 0 0 4px" }} />
        <motion.span key="agent-label" initial={{ opacity: 0, x: -4 }} animate={{ opacity: 1, x: 0 }} className="tool-shimmer self-end text-[12px] font-medium pl-[4px]" style={{ marginBottom: "-7px", color: accentColor }}>
          Using Agent
        </motion.span>
        {elapsed >= 2 && (
          <motion.span initial={{ opacity: 0 }} animate={{ opacity: 0.5 }} className="self-end text-[11px] pl-[6px] tabular-nums" style={{ marginBottom: "-7px", color: detailColor }}>
            {formatElapsed(elapsed)}
          </motion.span>
        )}
      </div>
      {detail && (
        <motion.div initial={{ opacity: 0, y: -2 }} animate={{ opacity: 1, y: 0 }} className="text-[11px] mt-[6px] pl-[18px] whitespace-nowrap" style={{ color: detailColor }}>
          {detail}
        </motion.div>
      )}
      {elapsed >= 3 && (
        <motion.div key={statusIdx} initial={{ opacity: 0, y: 2 }} animate={{ opacity: 0.6, y: 0 }} exit={{ opacity: 0, y: -2 }} transition={{ duration: 0.3 }} className="text-[11px] mt-[3px] pl-[18px] italic" style={{ color: detailColor }}>
          {AGENT_STATUS_MESSAGES[statusIdx]}
        </motion.div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Streaming bubble
// ---------------------------------------------------------------------------

function StreamingBubble({ text, toolCalls }: { text: string; toolCalls: ActiveToolCall[] }) {
  const bubbleColor = useUserPreferencesStore((s) => s.bubbleColor);
  const indicators = toolCalls.map((tc) => ({
    ...describeToolCall(tc.tool, tc.input),
    isAgent: stripMcpPrefix(tc.tool) === "Agent",
    startedAt: tc.startedAt,
  }));

  return (
    <div className="flex items-start gap-[10px] mb-4">
      <div className="flex-shrink-0 w-[34px] h-[34px] rounded-[10px] flex items-center justify-center text-[18px] select-none mt-[2px] bg-[var(--bg-secondary)] border border-[var(--border-secondary)]">
        🤖
      </div>
      <div className="flex flex-col items-start min-w-0">
        <div className="flex items-center gap-[6px] mb-[4px]">
          <span className="text-[14px] font-bold text-[var(--text-primary)]">Agent</span>
          <div className="flex items-center gap-[3px]">
            {[0, 1, 2].map((i) => (
              <motion.span
                key={i}
                className="block w-[4px] h-[4px] rounded-full bg-[var(--text-secondary)]"
                animate={{ opacity: [0.3, 1, 0.3] }}
                transition={{ duration: 1.2, repeat: Infinity, delay: i * 0.2, ease: "easeInOut" }}
              />
            ))}
          </div>
        </div>
        <div
          className={`max-w-[95%] px-[14px] py-[10px] bg-[var(--bg-agent-message)] text-[15px] text-[var(--text-primary)] leading-[1.45] break-words prose prose-sm prose-neutral dark:prose-invert max-w-none [&_p]:my-1 [&_ul]:my-1 [&_ol]:my-1 [&_li]:my-0.5 [&_pre]:my-2 [&_code]:text-[13px] ${indicators.length > 0 ? "pb-[14px]" : ""}`}
          style={{ borderRadius: "4px 16px 16px 16px" }}
        >
          {text ? (
            <Markdown remarkPlugins={[remarkGfm]}>{text}</Markdown>
          ) : indicators.length === 0 ? (
            <div className="flex items-center gap-[4px]">
              <span className="text-[12px] text-[var(--text-secondary)]">is typing</span>
              <div className="flex items-center gap-[3px] pt-[2px]">
                {[0, 1, 2].map((i) => (
                  <motion.span
                    key={i}
                    className="block w-[5px] h-[5px] rounded-full bg-[var(--text-secondary)]"
                    animate={{ y: [0, -4, 0] }}
                    transition={{ duration: 0.6, repeat: Infinity, delay: i * 0.2, ease: "easeInOut" }}
                  />
                ))}
              </div>
            </div>
          ) : null}
          {indicators.length > 0 && (
            <>
              {indicators.length > 3 && (
                <div className="text-[11px] mb-[2px]" style={{ color: bubbleColor, opacity: 0.8 }}>
                  + {indicators.length - 3} more
                </div>
              )}
              {indicators.slice(-3).map((ind, i) =>
                ind.isAgent ? (
                  <AgentToolIndicator key={`tool-${i}`} detail={ind.detail} startedAt={ind.startedAt} accentColor={bubbleColor} />
                ) : (
                  <ToolIndicator key={`tool-${i}`} label={ind.label} detail={ind.detail} accentColor={bubbleColor} />
                )
              )}
            </>
          )}
          {!text && indicators.length === 0 && null}
          {indicators.length === 0 && text && (
            <span className="inline-block w-[2px] h-[15px] bg-[var(--text-primary)] animate-pulse mt-[2px]" />
          )}
        </div>
      </div>

      {/* Shimmer animation for tool indicators */}
      <style>{`
        @keyframes shimmer-sweep {
          0% { background-position: -200% center; }
          100% { background-position: 200% center; }
        }
        .tool-shimmer {
          background: linear-gradient(
            90deg,
            ${bubbleColor} 0%,
            ${bubbleColor} 35%,
            color-mix(in srgb, ${bubbleColor} 50%, white) 50%,
            ${bubbleColor} 65%,
            ${bubbleColor} 100%
          );
          background-size: 200% auto;
          -webkit-background-clip: text;
          background-clip: text;
          -webkit-text-fill-color: transparent;
          animation: shimmer-sweep 2s ease-in-out infinite;
        }
      `}</style>
    </div>
  );
}

// ---------------------------------------------------------------------------
// PhaseChat
// ---------------------------------------------------------------------------

export function PhaseChat({ taskId, phaseId, phaseName, projectName, phaseStatus, phaseType, phaseStartedAt }: PhaseChatProps) {
  const messages = usePhaseChatStore((s) => s.messages);
  const messagesLoading = usePhaseChatStore((s) => s.messagesLoading);
  const streamingText = usePhaseChatStore((s) => s.streamingText);
  const isTyping = usePhaseChatStore((s) => s.isTyping);
  const activeToolCalls = usePhaseChatStore((s) => s.activeToolCalls);
  const scrollToBottomTrigger = usePhaseChatStore((s) => s.scrollToBottomTrigger);
  const selectPhase = usePhaseChatStore((s) => s.selectPhase);
  const sendMessage = usePhaseChatStore((s) => s.sendMessage);
  const coldStart = usePhaseChatStore((s) => s.coldStart);

  const resumeTask = useWorkflowStore((s) => s.resumeTask);
  const [resuming, setResuming] = useState(false);

  const setDraftAttachments = useDraftStore((s) => s.setDraftAttachments);
  const clearDraft = useDraftStore((s) => s.clearDraft);

  // Phase attachment handlers for ChatInput
  const handleUploadAttachment = useCallback(
    (file: File) => api.uploadPhaseAttachment(taskId, phaseId, file),
    [taskId, phaseId]
  );
  const handleAddFolderReference = useCallback(
    (path: string) => api.addPhaseFolderReference(taskId, phaseId, path),
    [taskId, phaseId]
  );
  const handleDeleteAttachment = useCallback(
    (attachmentId: string) => api.deletePhaseAttachment(taskId, phaseId, attachmentId),
    [taskId, phaseId]
  );

  // Stable attachment fetcher for rendering attachments in messages
  const attachmentFetcher = useMemo<AttachmentFetcher>(() => ({
    getInfo: (attachmentId: string) => api.getPhaseAttachmentInfo(taskId, phaseId, attachmentId),
    getUrl: (attachmentId: string) => api.getPhaseAttachmentUrl(taskId, phaseId, attachmentId),
  }), [taskId, phaseId]);

  const isRunning = phaseStatus === "running";
  const isActive = isRunning || phaseStatus === "paused";
  const elapsed = useElapsedTimer(phaseStartedAt, isRunning);
  const isPaused = phaseStatus === "paused";
  const isPausePhase = phaseType === "pause";
  const isFolderPhase = phaseType === "folder";
  const isCompleted = phaseStatus === "completed";
  const showChatInput = isActive || isCompleted; // allow modification requests on completed phases
  const bottomRef = useRef<HTMLDivElement>(null);

  // Load messages when phase changes
  useEffect(() => {
    selectPhase(taskId, phaseId);
  }, [taskId, phaseId, selectPhase]);

  // Cold-start the agent only for running prompt phases (never paused, folder, or pause-type).
  // Also skip if phaseType is undefined — definitions may not have loaded yet, and the
  // fallback defaults all phases to "prompt" which would incorrectly trigger a cold start
  // for folder phases (whose run.sh is already executing via the queue manager).
  useEffect(() => {
    if (isRunning && !isPaused && !isFolderPhase && !isPausePhase && phaseType != null) {
      coldStart(taskId, phaseId);
    }
  }, [taskId, phaseId, isRunning, isPaused, isFolderPhase, isPausePhase, coldStart]);

  // Auto-scroll on new messages and streaming updates
  useEffect(() => {
    requestAnimationFrame(() => {
      bottomRef.current?.scrollIntoView({ behavior: "smooth" });
    });
  }, [scrollToBottomTrigger, messages.length, streamingText, activeToolCalls]);

  if (messagesLoading) {
    return (
      <div className="flex-1 flex items-center justify-center">
        <Loader2 size={24} className="animate-spin text-[var(--text-secondary)]" />
      </div>
    );
  }

  const visibleMessages = messages.filter((m) => m.event_type !== "cold_start");

  const statusLabel = isPaused ? "paused" : phaseStatus === "running" ? "running" : phaseStatus === "completed" ? "completed" : phaseStatus === "failed" ? "failed" : phaseStatus ?? "pending";
  const statusDotColor = isPaused ? "bg-amber-500 animate-pulse" : phaseStatus === "running" ? "bg-green-500 animate-pulse" : phaseStatus === "completed" ? "bg-teal-500" : phaseStatus === "failed" ? "bg-red-500" : "bg-gray-400";

  return (
    <div className="flex flex-col flex-1 min-h-0 overflow-hidden">
      {/* Header */}
      <div className="px-6 py-3 border-b border-[var(--border-secondary)] flex items-center justify-between">
        <div className="flex items-center gap-2">
          <div className={`w-2 h-2 rounded-full ${statusDotColor}`} />
          <span className="text-[13px] font-bold text-[var(--text-primary)]">{phaseName}</span>
          <span className="text-[12px] text-[var(--text-tertiary)]">{statusLabel}</span>
        </div>
        {elapsed && (
          <div className="flex items-center gap-1 text-[12px] text-[var(--text-tertiary)]">
            <Clock size={12} />
            <span className="tabular-nums">{elapsed}</span>
          </div>
        )}
      </div>

      {/* Messages */}
      <div className="flex-1 overflow-y-auto px-6 py-4 custom-scrollbar">
        {visibleMessages.length === 0 && !isTyping && !isPausePhase && (
          <div className="flex flex-col items-center justify-center h-full text-center max-w-sm mx-auto">
            <div className="w-10 h-10 bg-[var(--bg-secondary)] rounded-full flex items-center justify-center mb-3 border border-[var(--border-secondary)]">
              <span className="text-[18px]">💬</span>
            </div>
            <p className="text-[13px] text-[var(--text-secondary)] leading-relaxed">
              {isRunning
                ? "The agent is starting up for this phase. Messages will appear here shortly."
                : "No messages recorded for this phase."}
            </p>
          </div>
        )}

        {visibleMessages.map((entry, i) => (
          <PhaseChatMessage key={`${entry.ts}-${i}`} entry={entry} attachmentFetcher={attachmentFetcher} />
        ))}

        {(isTyping || activeToolCalls.length > 0) && <StreamingBubble text={streamingText} toolCalls={activeToolCalls} />}

        <div ref={bottomRef} />
      </div>

      {/* Approve button for paused phases */}
      {isPaused && (
        <div className="px-6 py-3 border-t border-[var(--border-secondary)] bg-amber-50 dark:bg-amber-950/20">
          <div className="flex items-center justify-between">
            <div className="flex flex-col">
              <span className="text-[13px] font-bold text-amber-700 dark:text-amber-400">
                {isPausePhase ? "Approval Required" : "Phase Paused"}
              </span>
              <span className="text-[12px] text-[var(--text-secondary)]">
                {isPausePhase
                  ? "Review the outputs above and approve to continue the workflow."
                  : "This phase is paused. Approve to advance to the next phase."}
              </span>
            </div>
            <button
              onClick={async () => {
                setResuming(true);
                try {
                  await resumeTask(taskId);
                } finally {
                  setResuming(false);
                }
              }}
              disabled={resuming}
              className="flex items-center gap-1.5 px-4 py-2 rounded-lg font-bold text-[13px] bg-[#007A5A] text-white hover:bg-[#00684c] transition-colors cursor-pointer disabled:opacity-50"
            >
              <Play size={14} />
              {resuming ? "Advancing..." : "Approve & Continue"}
            </button>
          </div>
        </div>
      )}

      {/* Chat input for active and completed phases */}
      {showChatInput && !isPaused && (
        <div className="px-4 pb-4 pt-2">
          <ChatInput
            conversationId={`${taskId}-${phaseId}`}
            entityId={`${taskId}-${phaseId}`}
            fileCapabilitiesSupported={true}
            onUploadAttachment={handleUploadAttachment}
            onAddFolderReference={handleAddFolderReference}
            onDeleteAttachment={handleDeleteAttachment}
            onSend={(content, attachmentIds, attachments) => {
              clearDraft(`${taskId}-${phaseId}`);
              sendMessage(content, attachmentIds, attachments);
            }}
            onUnmountAttachments={(attachments, id) => {
              if (id && attachments.length > 0) setDraftAttachments(id, attachments);
            }}
            placeholder={isCompleted ? `Suggest changes to ${phaseName}...` : `Chat about ${projectName}...`}
          />
        </div>
      )}
    </div>
  );
}
