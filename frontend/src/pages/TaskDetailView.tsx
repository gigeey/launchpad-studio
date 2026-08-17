import { useEffect, useRef, useState } from "react";
import { useParams, useNavigate } from "react-router-dom";
import { ArrowLeft, ChevronRight, Loader2, CheckCircle2, MessageSquare, FolderOpen, GitBranch } from "lucide-react";
import { useWorkflowStore } from "../stores/workflowStore";
import { getWorkflow } from "../lib/api";
import type { PhaseDefinition, PhaseState, PhaseStatus, PhaseType } from "../types/workflow";
import { useTaskSSE } from "../hooks/useTaskSSE";
import { PhaseChat } from "../components/workflow/PhaseChat";
import { InputPhaseForm } from "../components/workflow/InputPhaseForm";
import { OutputPreview } from "../components/workflow/OutputPreview";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function inferPhaseType(phase: { phase_type?: PhaseType | null }): PhaseType {
  // "input" is never inferred — it must be explicitly declared in the
  // workflow YAML because it represents a user-facing form phase.
  return phase.phase_type ?? "prompt";
}

const PHASE_TYPE_COLORS: Record<PhaseType, string> = {
  folder: "bg-[#2eb67d]",
  prompt: "bg-[#e01e5a]",
  input: "bg-[#36c5f0]",
  pause: "bg-[#ecb22e]",
};

function getDuration(state: PhaseState | undefined): string | null {
  if (!state?.started_at) return null;
  const end = state.completed_at || state.failed_at || state.skipped_at;
  if (!end) return null;

  const s = new Date(state.started_at).getTime();
  const e = new Date(end).getTime();
  const diff = e - s;
  const mins = Math.floor(diff / 60000);
  if (mins === 0) return "< 1m";
  return `${mins}m`;
}

// ---------------------------------------------------------------------------
// TaskDetailView
// ---------------------------------------------------------------------------

export function TaskDetailView() {
  const { taskId } = useParams<{ taskId: string }>();
  const navigate = useNavigate();
  const currentTask = useWorkflowStore((s) => s.currentTask);
  const fetchTask = useWorkflowStore((s) => s.fetchTask);
  const startTask = useWorkflowStore((s) => s.startTask);
  const resumeTask = useWorkflowStore((s) => s.resumeTask);
  const cancelTask = useWorkflowStore((s) => s.cancelTask);
  const loading = useWorkflowStore((s) => s.loading);
  const [starting, setStarting] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [selectedPhaseId, setSelectedPhaseId] = useState<string | null>(null);
  const [previewFile, setPreviewFile] = useState<string | null>(null);
  const [resuming, setResuming] = useState(false);
  const [phaseDefinitions, setPhaseDefinitions] = useState<PhaseDefinition[]>([]);


  // Always fetch fresh task data when this view mounts or the taskId changes.
  // Previously guarded by `taskId !== currentTaskId`, which skipped the fetch
  // when returning to a task already in the store — showing stale phase states.
  const prevTaskIdRef = useRef<string | undefined>(undefined);
  useEffect(() => {
    if (!taskId) return;
    // Reset view state when switching tasks
    if (prevTaskIdRef.current !== taskId) {
      prevTaskIdRef.current = taskId;
      setSelectedPhaseId(null);
      setPreviewFile(null);
      setPhaseDefinitions([]);
    }
    // Always fetch fresh data — even for the same taskId (e.g. navigating back)
    fetchTask(taskId);
  }, [taskId, fetchTask]);

  // Fetch workflow definition for phase details
  useEffect(() => {
    if (!currentTask) {
      setPhaseDefinitions([]);
      return;
    }
    let cancelled = false;
    getWorkflow(currentTask.workflow).then(
      (def) => {
        if (!cancelled) setPhaseDefinitions(def.phases);
      },
      () => {
        if (!cancelled) setPhaseDefinitions([]);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [currentTask?.workflow]);

  // Derived state — safe to compute even when currentTask is null
  const phases =
    currentTask && phaseDefinitions.length > 0
      ? phaseDefinitions
      : currentTask
        ? Object.keys(currentTask.phases).map((id) => ({
          id,
          name: id,
          intent: null as string | null | undefined,
          path: "",
          phase_type: "prompt" as PhaseType,
          inputs: [],
          outputs: [],
          fields: [],
        }))
        : [];

  const activePhase = currentTask
    ? phases.find((p) => {
      const s = currentTask.phases[p.id];
      return s?.status === "running" || s?.status === "paused";
    })
    : undefined;

  // Auto-select the active phase when it changes
  // NOTE: This must be called before any early returns to satisfy the Rules of Hooks.
  useEffect(() => {
    if (activePhase && !selectedPhaseId) {
      setSelectedPhaseId(activePhase.id);
    }
  }, [activePhase, selectedPhaseId]);

  // Connect SSE for real-time task events
  // NOTE: Must be called before early returns to satisfy Rules of Hooks.
  useTaskSSE(taskId ?? null);

  if (loading && !currentTask) {
    return (
      <div className="flex items-center justify-center flex-1 py-16">
        <Loader2 size={24} className="animate-spin text-[var(--text-secondary)]" />
      </div>
    );
  }

  if (!currentTask || !taskId) {
    return (
      <div className="flex flex-col items-center justify-center flex-1 py-16 gap-3">
        <p className="text-[14px] text-[var(--text-secondary)]">Task not found</p>
        <button
          onClick={() => navigate("/tasks")}
          className="text-[13px] text-[var(--accent)] hover:underline cursor-pointer"
        >
          Back to tasks
        </button>
      </div>
    );
  }

  const taskStatus = currentTask.status ?? "pending";
  const pausedPhaseEntry = Object.entries(currentTask.phases).find(([, state]) => state.status === "paused");

  // The phase to show in the right panel: user-selected or active
  const viewPhase = selectedPhaseId
    ? phases.find((p) => p.id === selectedPhaseId)
    : activePhase;
  const viewPhaseStatus: PhaseStatus | undefined = viewPhase
    ? currentTask.phases[viewPhase.id]?.status
    : undefined;

  return (
    <div className="flex flex-col sm:flex-row flex-1 min-h-0 bg-[var(--bg-secondary)] overflow-hidden p-4 gap-4">
      {/* ------------------------------------------------------------------ */}
      {/* LEFT PANEL : PHASE TREE (1/3 Width)                              */}
      {/* ------------------------------------------------------------------ */}
      <div className="w-full sm:w-[35%] sm:min-w-[340px] sm:max-w-[450px] bg-white dark:bg-[var(--bg-primary)] rounded-[16px] shadow-sm border border-[var(--border-secondary)] overflow-hidden flex flex-col relative z-0">

        {/* Decorative Background at Top */}
        <div
          className="absolute top-0 left-0 right-0 h-64 pointer-events-none z-0 opacity-80 dark:opacity-30"
          style={{
            background: `
              radial-gradient(ellipse 60% 80% at 0% 0%, rgba(134, 215, 210, 0.35), transparent),
              radial-gradient(ellipse 50% 70% at 50% 0%, rgba(200, 220, 210, 0.2), transparent),
              radial-gradient(ellipse 60% 80% at 100% 0%, rgba(230, 180, 190, 0.3), transparent)
            `,
          }}
        />

        {/* Header */}
        <div className="relative z-10 px-6 py-6 flex flex-col items-center justify-center border-b border-transparent">
          <button
            onClick={() => navigate("/tasks")}
            className="absolute left-6 top-6 p-1.5 -ml-1.5 rounded-lg transition-colors cursor-pointer text-[var(--text-secondary)] hover:text-[var(--text-primary)] hover:bg-[var(--bg-secondary)]"
          >
            <ArrowLeft size={18} />
          </button>

          <div className="mt-1 text-[13px] font-bold text-white px-5 py-1.5 rounded-full truncate max-w-[200px] bg-[#1a1a2e] dark:bg-[#e0e0e0] dark:text-[#1a1a2e]">
            {currentTask.project_name}
          </div>
        </div>

        {/* Tree Content */}
        <div className="flex-1 overflow-y-auto custom-scrollbar px-6 pb-6 relative z-10">
          <div className="relative flex flex-col w-full pt-4 pb-12">
            {/* Center Timeline Line */}
            <div className="absolute top-6 bottom-0 w-[1px] bg-teal-600/20 dark:bg-teal-400/20" style={{ left: '50%', transform: 'translateX(-50%)' }} />

            {phases.map((phase, idx) => {
              const state = currentTask.phases[phase.id];
              const status: PhaseStatus = state?.status ?? "pending";
              const isRight = idx % 2 === 1;
              const duration = getDuration(state);

              // Status localized text for timeline style
              const statusText = status === "completed" ? "Done" : status === "running" ? "Running" : status === "failed" ? "Failed" : status === "paused" ? "Paused" : status === "stopped" ? "Stopped" : "Pending";

              return (
                <div key={phase.id} className="relative w-full flex mb-12" style={{ justifyContent: isRight ? 'flex-end' : 'flex-start' }}>
                  {/* Timeline Dot */}
                  <div className={`absolute top-[8px] left-1/2 -translate-x-1/2 z-10 w-[9px] h-[9px] rounded-full ring-4 ring-white dark:ring-[var(--bg-primary)] ${status === 'completed' ? 'bg-teal-600 dark:bg-teal-400' : status === 'running' ? 'bg-orange-500 animate-pulse' : status === 'failed' ? 'bg-red-500' : status === 'stopped' ? 'bg-amber-500' : 'bg-gray-300 dark:bg-gray-600'}`} />

                  {/* Content Container */}
                  <div className={`w-1/2 flex flex-col ${isRight ? 'pl-5 md:pl-7 items-start text-left' : 'pr-5 md:pr-7 items-end text-right'}`}>
                    {/* Status Text (like 'Sep' in mockup) */}
                    <div className="text-[12px] text-teal-800 dark:text-teal-400 font-bold uppercase">
                      {statusText}
                    </div>

                    {/* Title — clickable to view phase chat */}
                    <button
                      type="button"
                      onClick={() => { setSelectedPhaseId(phase.id); setPreviewFile(null); }}
                      className={`flex items-center gap-1 px-2 py-1 -ml-2 rounded-md text-[14px] font-bold leading-tight cursor-pointer transition-colors max-w-full ${status === 'running' ? 'shimmer-text' : selectedPhaseId === phase.id ? 'bg-[var(--sidebar-active-bg)] text-[var(--sidebar-active-text-primary)]' : 'hover:bg-[var(--bg-hover)] text-[var(--text-primary)]'
                        }`}
                    >
                      <span className="truncate">{phase.name}</span>
                      <ChevronRight size={14} className="flex-shrink-0 opacity-50" />
                    </button>

                    {/* Description (Max 3 lines) */}
                    {(phase.intent || state?.error) && (
                      <div className={`text-[12px] mt-1 line-clamp-3 leading-relaxed ${state?.error ? 'text-red-500' : 'text-[var(--text-secondary)]'}`}>
                        {state?.error || phase.intent}
                      </div>
                    )}

                    {/* Type Block Label */}
                    <div className="mt-1">
                      {(() => {
                        const phaseType = inferPhaseType(phase);
                        return (
                          <span className={`inline-flex leading-[15px] px-1.5 py-[1px] rounded-[6px] text-[10px] font-medium tracking-wider text-white uppercase ${PHASE_TYPE_COLORS[phaseType]}`}>
                            {phaseType}
                          </span>
                        );
                      })()}
                    </div>

                    {/* Metadata (Duration, Tokens - Flex Between) */}
                    <div className={`mt-[2px] flex items-center justify-between w-full text-[10px] text-[var(--text-tertiary)]`}>
                      <span title="Duration">{duration || "--"}</span>
                      <span title="Tokens Used">{(() => {
                        const total = (state?.input_tokens ?? 0) + (state?.output_tokens ?? 0);
                        if (total === 0) return "--";
                        if (total >= 1000000) return `${(total / 1000000).toFixed(1)}M`;
                        if (total >= 1000) return `${(total / 1000).toFixed(1)}k`;
                        return `${total}`;
                      })()}</span>
                    </div>

                    {/* Outputs */}
                    {status === 'completed' && phase.outputs && phase.outputs.length > 0 && (
                      <div className={`mt-0.5 w-full flex flex-col ${!isRight ? 'items-end text-right' : 'items-start text-left'}`}>
                        {/* Divider & Title */}
                        <div className={`flex items-center gap-2 w-full mb-0 leading-[10px] ${!isRight ? 'flex-row-reverse' : ''}`}>
                          <div className="text-[9px] uppercase tracking-widest font-bold text-[var(--text-secondary)]">Outputs</div>
                          <div className="h-[1px] flex-1 bg-gray-300 dark:bg-gray-600" />
                        </div>

                        {/* Links */}
                        <div className={`flex flex-col gap-0.5 ${!isRight ? 'items-end' : 'items-start'}`}>
                          {phase.outputs.map((out, outIdx) => (
                            <button
                              key={out.id}
                              title="Click to preview output"
                              onClick={() => {
                                setPreviewFile(out.filename || out.id);
                                setSelectedPhaseId(null);
                              }}
                              className="text-[11px] text-blue-600 dark:text-blue-400 hover:underline hover:text-blue-800 dark:hover:text-blue-300 transition-colors cursor-pointer truncate max-w-full"
                            >
                              {outIdx + 1}. {out.filename || out.id}
                            </button>
                          ))}
                        </div>
                      </div>
                    )}
                  </div>
                </div>
              );
            })}

          </div>
        </div>

        {/* Working Directory */}
        {currentTask.working_directory && (
          <div className="relative z-10 px-4 py-2 border-t border-[var(--border-secondary)] bg-white dark:bg-[var(--bg-primary)]">
            <div className="flex items-center gap-2">
              <FolderOpen size={12} className="flex-shrink-0 text-[var(--text-tertiary)]" />
              <span className="text-[10px] font-mono text-[var(--text-tertiary)] truncate" title={currentTask.working_directory}>
                {currentTask.working_directory}
              </span>
            </div>
          </div>
        )}

        {/* Left Panel Footer Area: Task Controls */}
        <div className="relative z-10 p-4 border-t border-[var(--border-secondary)] bg-white dark:bg-[var(--bg-primary)] mt-auto flex flex-col">
          {pausedPhaseEntry ? (
            <div className="flex flex-col gap-2 items-start w-full">
              <div className="text-[13px] font-bold text-[var(--text-primary)]">Status: Paused</div>
              {pausedPhaseEntry[1].paused_reason && (
                <div className="text-[11px] text-[var(--text-secondary)] line-clamp-2 w-full">
                  Reason: {pausedPhaseEntry[1].paused_reason}
                </div>
              )}
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
                className="w-full px-4 py-2 mt-1 border border-transparent dark:border-[var(--border-primary)] rounded-[8px] font-bold bg-[#007A5A] text-white hover:bg-[#00684c] dark:bg-[var(--bg-secondary)] dark:text-[var(--text-primary)] dark:hover:bg-[var(--bg-hover)] transition-colors cursor-pointer text-[12px]"
              >
                {resuming ? "Continuing..." : "Continue Workflow"}
              </button>
            </div>
          ) : taskStatus === "pending" ? (
            <div className="flex flex-col gap-2 items-start w-full">
              <div className="text-[13px] font-bold text-[var(--text-primary)]">Status: Pending</div>
              <button
                onClick={async () => {
                  setStarting(true);
                  try {
                    await startTask(taskId);
                  } finally {
                    setStarting(false);
                  }
                }}
                disabled={starting}
                className="w-full px-4 py-2 mt-1 border border-transparent dark:border-[var(--border-primary)] rounded-[8px] font-bold bg-[#007A5A] text-white hover:bg-[#00684c] dark:bg-[var(--bg-secondary)] dark:text-[var(--text-primary)] dark:hover:bg-[var(--bg-hover)] transition-colors cursor-pointer text-[12px]"
              >
                {starting ? "Starting..." : "Start Task Workflow"}
              </button>
            </div>
          ) : taskStatus === "stopped" ? (
            <div className="flex flex-col gap-2 items-start w-full">
              <div className="text-[13px] font-bold text-red-500">Status: Stopped</div>
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
                className="w-full px-4 py-2 mt-1 border border-transparent dark:border-[var(--border-primary)] rounded-[8px] font-bold bg-[#007A5A] text-white hover:bg-[#00684c] dark:bg-[var(--bg-secondary)] dark:text-[var(--text-primary)] dark:hover:bg-[var(--bg-hover)] transition-colors cursor-pointer text-[12px]"
              >
                {resuming ? "Resuming..." : "Resume Workflow"}
              </button>
            </div>
          ) : taskStatus === "running" ? (
            <div className="flex flex-col gap-2 items-start w-full">
              <div className="text-[13px] font-bold text-orange-500 flex items-center gap-2">
                <Loader2 size={14} className="animate-spin" />
                Processing...
              </div>
              <button
                onClick={async () => {
                  setStopping(true);
                  try {
                    await cancelTask(taskId);
                  } finally {
                    setStopping(false);
                  }
                }}
                disabled={stopping}
                className="w-full px-4 py-2 mt-1 border border-transparent dark:border-[var(--border-primary)] rounded-[8px] font-bold bg-[#D32F2F] text-white hover:bg-[#B71C1C] dark:bg-[var(--bg-secondary)] dark:text-red-400 dark:hover:bg-[var(--bg-hover)] transition-colors cursor-pointer text-[12px] disabled:opacity-50 disabled:cursor-not-allowed"
              >
                {stopping ? "Stopping..." : "Stop Workflow"}
              </button>
            </div>
          ) : (
            <div className="flex items-center justify-center w-full">
              <div className="text-[13px] font-bold text-[#007A5A] dark:text-[#2eb67d]">Workflow Completed 🎉</div>
            </div>
          )}
        </div>
      </div>

      {/* ------------------------------------------------------------------ */}
      {/* RIGHT PANEL : CHAT/OUTPUT AREA (2/3 Width)                       */}
      {/* ------------------------------------------------------------------ */}
      <div className="flex-1 overflow-hidden flex flex-col relative bg-white dark:bg-[var(--bg-primary)] rounded-[16px] shadow-sm border border-[var(--border-secondary)]">
        {previewFile && taskId ? (
          <OutputPreview
            key={`preview-${previewFile}`}
            taskId={taskId}
            filename={previewFile}
            onClose={() => setPreviewFile(null)}
          />
        ) : viewPhase && taskId && inferPhaseType(viewPhase) === "input" ? (
          <InputPhaseForm
            key={`input-${viewPhase.id}`}
            taskId={taskId}
            phaseId={viewPhase.id}
            phaseName={viewPhase.name}
            fields={viewPhase.fields ?? []}
            phaseStatus={viewPhaseStatus}
          />
        ) : viewPhase && taskId ? (
          <PhaseChat
            key={`chat-${viewPhase.id}`}
            taskId={taskId}
            phaseId={viewPhase.id}
            phaseName={viewPhase.name}
            projectName={currentTask.project_name}
            phaseStatus={viewPhaseStatus}
            phaseType={inferPhaseType(viewPhase)}
            phaseStartedAt={currentTask.phases[viewPhase.id]?.started_at}
          />
        ) : (
          <div className="flex-1 flex flex-col items-center justify-center px-6 py-8">
            <div className="max-w-md text-center">
              <div className={`w-12 h-12 rounded-full flex items-center justify-center mb-4 mx-auto ${taskStatus === "completed"
                ? "bg-[#2EB57D] text-white"
                : "bg-[#1a1a2e] dark:bg-[#e0e0e0] text-white dark:text-[#1a1a2e]"
                }`}>
                {taskStatus === "completed"
                  ? <CheckCircle2 size={28} fill="white" stroke="#2EB57D" />
                  : <MessageSquare size={22} />
                }
              </div>
              <h3 className="text-[15px] font-bold text-[var(--text-primary)] mb-1.5">
                {taskStatus === "completed" ? "All Done" : "Chat & Outputs"}
              </h3>
              <p className="text-[13px] text-[var(--text-secondary)] leading-relaxed">
                {taskStatus === "pending"
                  ? "Start the task to run through the workflow phases."
                  : taskStatus === "completed"
                    ? "Workflow finished. Select a phase or click an output file to review results."
                    : "Select a phase to chat with its agent, or click an output file to preview it."}
              </p>

              {/* Workflow & working directory info */}
              <div className="mt-5 flex flex-col gap-2 text-left w-full">
                <div className="flex items-center gap-2.5 rounded-lg px-3 py-2 bg-white dark:bg-[var(--bg-secondary)] border border-[var(--border-secondary)]">
                  <div className="w-[26px] h-[26px] rounded-full bg-[var(--bg-secondary)] dark:bg-[var(--bg-hover)] flex items-center justify-center flex-shrink-0">
                    <GitBranch size={13} className="text-[var(--text-secondary)]" />
                  </div>
                  <span className="text-[12px] text-[var(--text-primary)] truncate">{currentTask.workflow}</span>
                </div>
                {currentTask.working_directory && (
                  <div className="flex items-center gap-2.5 rounded-lg px-3 py-2 bg-white dark:bg-[var(--bg-secondary)] border border-[var(--border-secondary)]">
                    <div className="w-[26px] h-[26px] rounded-full bg-[var(--bg-secondary)] dark:bg-[var(--bg-hover)] flex items-center justify-center flex-shrink-0">
                      <FolderOpen size={13} className="text-[var(--text-secondary)]" />
                    </div>
                    <span className="text-[12px] text-[var(--text-primary)] truncate">{currentTask.working_directory}</span>
                  </div>
                )}
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
