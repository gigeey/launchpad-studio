import type { TaskStatus } from "../../types/api";

interface PillStyle {
    label: string;
    bg: string;
    fg: string;
}

const STATUS_STYLES: Record<TaskStatus, PillStyle> = {
    pending: {
        label: "pending",
        bg: "var(--bg-tertiary)",
        fg: "var(--text-tertiary)",
    },
    in_progress: {
        label: "in progress",
        bg: "rgba(59,130,246,0.12)",
        fg: "rgb(37,99,235)",
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
    blocked: {
        label: "blocked",
        bg: "rgba(217,119,6,0.14)",
        fg: "rgb(180,83,9)",
    },
    skipped: {
        label: "skipped",
        bg: "var(--bg-tertiary)",
        fg: "var(--text-tertiary)",
    },
    stopped: {
        label: "stopped",
        bg: "rgba(217,119,6,0.12)",
        fg: "rgb(180,83,9)",
    },
};

const UNKNOWN_STYLE: PillStyle = {
    label: "",
    bg: "var(--bg-tertiary)",
    fg: "var(--text-tertiary)",
};

export interface TaskStatusBadgeProps {
    status: TaskStatus | string;
}

/**
 * Status pill for a single task. Mirrors TasklistStatusPill's shape and tokens
 * so task-status pills read consistently next to tasklist-status pills.
 * Unknown status values render with a muted fallback rather than throwing.
 */
export function TaskStatusBadge({ status }: TaskStatusBadgeProps) {
    const known = (STATUS_STYLES as Record<string, PillStyle | undefined>)[status];
    const style = known ?? { ...UNKNOWN_STYLE, label: String(status) };
    return (
        <span
            className="px-[10px] py-[3px] rounded-full text-[11px] font-medium"
            style={{ backgroundColor: style.bg, color: style.fg }}
        >
            {style.label}
        </span>
    );
}
