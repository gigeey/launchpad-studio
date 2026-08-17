import { Check, SkipForward, Loader2, Circle, XCircle, Pause, Square } from "lucide-react";
import type { PhaseStatus as PhaseStatusType } from "../../types/workflow";

interface PhaseStatusProps {
  status: PhaseStatusType | "pending";
}

export function PhaseStatusIcon({ status }: PhaseStatusProps) {
  switch (status) {
    case "completed":
      return (
        <div className="w-[20px] h-[20px] rounded-full bg-[var(--success,#34C759)] flex items-center justify-center flex-shrink-0">
          <Check className="w-[12px] h-[12px] text-white" />
        </div>
      );
    case "skipped":
      return (
        <div className="w-[20px] h-[20px] rounded-full bg-[var(--text-tertiary)] flex items-center justify-center flex-shrink-0">
          <SkipForward className="w-[12px] h-[12px] text-white" />
        </div>
      );
    case "running":
      return (
        <div className="w-[20px] h-[20px] rounded-full bg-[var(--accent)] flex items-center justify-center flex-shrink-0">
          <Loader2 className="w-[12px] h-[12px] text-white animate-spin" />
        </div>
      );
    case "failed":
      return (
        <div className="w-[20px] h-[20px] rounded-full bg-[var(--error,#FF3B30)] flex items-center justify-center flex-shrink-0">
          <XCircle className="w-[12px] h-[12px] text-white" />
        </div>
      );
    case "paused":
      return (
        <div className="w-[20px] h-[20px] rounded-full bg-[#F5A623] flex items-center justify-center flex-shrink-0">
          <Pause className="w-[12px] h-[12px] text-white" />
        </div>
      );
    case "stopped":
      return (
        <div className="w-[20px] h-[20px] rounded-full bg-red-500 flex items-center justify-center flex-shrink-0">
          <Square className="w-[8px] h-[8px] text-white fill-white" />
        </div>
      );
    case "pending":
    default:
      return (
        <Circle className="w-[20px] h-[20px] text-[var(--text-tertiary)] flex-shrink-0" />
      );
  }
}
