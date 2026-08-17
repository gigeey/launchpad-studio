import { useEffect, useState } from "react";
import { ChevronDown, ChevronRight, Workflow, Play, Loader2 } from "lucide-react";
import { AnimatePresence, motion } from "framer-motion";
import { useWorkflowStore } from "../../stores/workflowStore";
import { getWorkflow } from "../../lib/api";
import type { PhaseDefinition } from "../../types/workflow";
import { PhaseStatusIcon } from "./PhaseStatus";
import { PhaseOutputViewer } from "./PhaseOutputViewer";

function formatTimestamp(iso: string): string {
  const d = new Date(iso);
  return d.toLocaleString([], {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** Banner shown for paused phases with reason and Resume button. */
function PausedBanner({ reason, taskId }: { reason?: string; taskId: string }) {
  const resumeTask = useWorkflowStore((s) => s.resumeTask);
  const [resuming, setResuming] = useState(false);

  const handleResume = async () => {
    setResuming(true);
    try {
      await resumeTask(taskId);
    } finally {
      setResuming(false);
    }
  };

  return (
    <div className="mt-[4px] rounded-[6px] bg-[#F5A623]/10 border border-[#F5A623]/30 px-[8px] py-[6px]">
      <div className="text-[11px] text-[#F5A623] font-medium">
        Paused{reason ? `: ${reason}` : ""}
      </div>
      <button
        onClick={handleResume}
        disabled={resuming}
        className="mt-[4px] flex items-center gap-[4px] text-[11px] font-medium text-[#F5A623] hover:text-[#D4911E] cursor-pointer disabled:opacity-50"
      >
        <Play className="w-[10px] h-[10px]" />
        {resuming ? "Resuming..." : "Resume"}
      </button>
    </div>
  );
}

/** Banner shown for stopped tasks with Resume button. */
function StoppedBanner({ taskId }: { taskId: string }) {
  const resumeTask = useWorkflowStore((s) => s.resumeTask);
  const [resuming, setResuming] = useState(false);

  const handleResume = async () => {
    setResuming(true);
    try {
      await resumeTask(taskId);
    } finally {
      setResuming(false);
    }
  };

  return (
    <div className="mt-[4px] rounded-[6px] bg-red-500/10 border border-red-500/30 px-[8px] py-[6px]">
      <div className="text-[11px] text-red-500 font-medium">
        Stopped
      </div>
      <button
        onClick={handleResume}
        disabled={resuming}
        className="mt-[4px] flex items-center gap-[4px] text-[11px] font-medium text-red-500 hover:text-red-600 cursor-pointer disabled:opacity-50"
      >
        <Play className="w-[10px] h-[10px]" />
        {resuming ? "Resuming..." : "Resume"}
      </button>
    </div>
  );
}

/** Stop button shown alongside the processing spinner when a workflow is running. */
function StopButton({ taskId }: { taskId: string }) {
  const cancelTask = useWorkflowStore((s) => s.cancelTask);
  const [isCancelling, setIsCancelling] = useState(false);

  const handleStop = async () => {
    if (isCancelling) return;
    setIsCancelling(true);
    try {
      await cancelTask(taskId);
    } finally {
      setIsCancelling(false);
    }
  };

  return (
    <AnimatePresence mode="wait">
      <motion.button
        key="workflow-stop-button"
        initial={{ opacity: 0, scale: 0.8 }}
        animate={{ opacity: 1, scale: 1 }}
        exit={{ opacity: 0, scale: 0.8 }}
        transition={{ duration: 0.15 }}
        onClick={handleStop}
        disabled={isCancelling}
        className={`flex-shrink-0 w-[24px] h-[24px] flex items-center justify-center rounded-full transition-colors ${
          isCancelling
            ? "bg-[var(--bg-tertiary)] cursor-not-allowed"
            : "bg-red-500/15 hover:bg-red-500/25 cursor-pointer"
        }`}
        title={isCancelling ? "Stopping..." : "Stop workflow"}
      >
        {isCancelling ? (
          <Loader2 size={12} className="animate-spin text-[var(--text-tertiary)]" />
        ) : (
          <div className="w-[8px] h-[8px] rounded-[2px] bg-red-500" />
        )}
      </motion.button>
    </AnimatePresence>
  );
}

export function WorkflowPanel() {
  const currentTask = useWorkflowStore((s) => s.currentTask);
  const currentTaskId = useWorkflowStore((s) => s.currentTaskId);
  const [collapsed, setCollapsed] = useState(false);
  const [phaseDefinitions, setPhaseDefinitions] = useState<PhaseDefinition[]>(
    [],
  );

  // Fetch workflow definition to get the ordered phase list with names/intents
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
        // If workflow definition unavailable, derive phases from snapshot keys
        if (!cancelled) setPhaseDefinitions([]);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [currentTask?.workflow]);

  if (!currentTask) return null;

  // Build the ordered phase list — use definitions if available, fall back to snapshot keys
  const phases =
    phaseDefinitions.length > 0
      ? phaseDefinitions
      : Object.keys(currentTask.phases).map((id) => ({
          id,
          name: id,
          intent: null as string | null | undefined,
          path: "",
          inputs: [],
          outputs: [],
        }));

  const completedCount = Object.values(currentTask.phases).filter(
    (p) => p.status === "completed",
  ).length;
  const totalCount = phases.length;

  // Check if all phases are done (completed or skipped)
  const allDone =
    totalCount > 0 &&
    phases.every((p) => {
      const state = currentTask.phases[p.id];
      return state && (state.status === "completed" || state.status === "skipped");
    });

  const isRunning = currentTask.status === "running";
  const isStopped = currentTask.status === "stopped";

  return (
    <div className="flex flex-col h-full overflow-hidden">
      {/* Header */}
      <div className="px-[16px] pt-[16px] pb-[8px]">
        <button
          onClick={() => setCollapsed((c) => !c)}
          className="flex items-center gap-[8px] w-full cursor-pointer"
        >
          {collapsed ? (
            <ChevronRight className="w-[14px] h-[14px] text-[var(--text-secondary)]" />
          ) : (
            <ChevronDown className="w-[14px] h-[14px] text-[var(--text-secondary)]" />
          )}
          <Workflow className="w-[16px] h-[16px] text-[var(--text-primary)]" />
          <span className="text-[14px] font-semibold text-[var(--text-primary)]">
            Workflow
          </span>
          <span className="text-[11px] font-bold text-[var(--text-secondary)] bg-[var(--bg-hover)] px-[6px] py-[1px] rounded-[4px]">
            {completedCount}/{totalCount}
          </span>
          {isRunning && (
            <Loader2 className="w-[14px] h-[14px] animate-spin text-[var(--accent)]" />
          )}
          {allDone && (
            <span className="text-[11px] font-bold text-[var(--success,#34C759)] bg-[var(--success,#34C759)]/10 px-[6px] py-[1px] rounded-[4px]">
              Done
            </span>
          )}
          {isStopped && (
            <span className="text-[11px] font-bold text-red-500 bg-red-500/10 px-[6px] py-[1px] rounded-[4px]">
              Stopped
            </span>
          )}
          {isRunning && currentTaskId && (
            <StopButton taskId={currentTaskId} />
          )}
        </button>
      </div>

      {!collapsed && (
        <>
          {/* Task info */}
          <div className="px-[16px] pb-[8px]">
            <div className="text-[13px] font-medium text-[var(--text-primary)]">
              {currentTask.project_name}
            </div>
            <div className="text-[11px] text-[var(--text-tertiary)]">
              {currentTask.workflow} &middot; {formatTimestamp(currentTask.created)}
            </div>
          </div>

          {/* Phase list */}
          <div className="flex-1 overflow-y-auto px-[16px] pb-[16px] custom-scrollbar">
            <div className="flex flex-col gap-[2px]">
              {phases.map((phase, idx) => {
                const state = currentTask.phases[phase.id];
                const status = state?.status ?? "pending";
                const isRunning = status === "running";
                const isPaused = status === "paused";
                const isStopped = status === "stopped";
                const isCompleted = status === "completed";

                return (
                  <div
                    key={phase.id}
                    className={`flex items-start gap-[10px] p-[10px] rounded-[10px] transition-colors ${
                      isRunning
                        ? "bg-[var(--accent)]/8"
                        : isPaused
                          ? "bg-[#F5A623]/8"
                          : isStopped
                            ? "bg-red-500/8"
                            : "bg-transparent"
                    }`}
                  >
                    {/* Status icon + connector line */}
                    <div className="flex flex-col items-center gap-[2px]">
                      <PhaseStatusIcon status={status} />
                      {idx < phases.length - 1 && (
                        <div className="w-[2px] h-[16px] bg-[var(--border-secondary)]" />
                      )}
                    </div>

                    {/* Phase details */}
                    <div className="flex-1 min-w-0 pt-[1px]">
                      <div className="text-[13px] font-medium text-[var(--text-primary)]">
                        {phase.name}
                      </div>
                      {phase.intent && (
                        <div className="text-[11px] text-[var(--text-secondary)] mt-[2px] leading-relaxed">
                          {phase.intent}
                        </div>
                      )}
                      {/* Completion timestamp */}
                      {state?.status === "completed" && state.completed_at && (
                        <div className="text-[11px] text-[var(--text-tertiary)] mt-[2px]">
                          Completed {formatTimestamp(state.completed_at)}
                        </div>
                      )}
                      {/* Skip reason */}
                      {state?.status === "skipped" && state.reason && (
                        <div className="text-[11px] text-[var(--text-tertiary)] mt-[2px] italic">
                          Skipped: {state.reason}
                        </div>
                      )}
                      {/* Failed error */}
                      {state?.status === "failed" && state.error && (
                        <div className="text-[11px] text-[var(--error,#FF3B30)] mt-[2px] italic">
                          Error: {state.error}
                        </div>
                      )}
                      {/* Paused banner with reason and resume button */}
                      {state?.status === "paused" && (
                        <PausedBanner
                          reason={state.paused_reason ?? undefined}
                          taskId={currentTaskId!}
                        />
                      )}
                      {/* Stopped banner with resume button */}
                      {isStopped && currentTaskId && (
                        <StoppedBanner taskId={currentTaskId} />
                      )}
                      {/* Output viewer for completed phases */}
                      {isCompleted && currentTaskId && phase.outputs.length > 0 && (
                        <PhaseOutputViewer
                          taskId={currentTaskId}
                          outputs={phase.outputs}
                        />
                      )}
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        </>
      )}
    </div>
  );
}
