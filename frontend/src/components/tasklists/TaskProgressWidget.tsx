import type { ReactNode } from "react";
import { BarChart3 } from "lucide-react";
import { agentAvatarColor } from "../../lib/agentColors";
import type { Task } from "../../types/api";

// ---------------------------------------------------------------------------
// WidgetCard — generic card shell used by progress and outputs widgets
// ---------------------------------------------------------------------------

export function WidgetCard({
  title,
  icon,
  action,
  children,
}: {
  title: string;
  icon: ReactNode;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="h-full flex flex-col rounded-[20px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] shadow-0 overflow-hidden">
      <div className="flex items-center justify-between px-[14px] py-[10px] border-b-0 border-[var(--border-secondary)] flex-shrink-0">
        <div className="flex items-center gap-[8px] text-[var(--text-primary)]">
          <span className="text-[var(--text-secondary)]">{icon}</span>
          <span className="text-[13px] font-semibold">{title}</span>
        </div>
        {action}
      </div>
      <div className="flex-1 min-h-0 overflow-hidden">{children}</div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// TaskProgressWidget — progress ring + per-agent bar chart, data-prop driven.
// No store dependencies — callers supply pre-computed task list and agent ids.
// ---------------------------------------------------------------------------

export function TaskProgressWidget({
  allTasks,
  agentIds,
  agentNameMap,
  agentEmojiMap,
  isDark,
  fallbackEmoji,
}: {
  allTasks: Task[];
  /** Ordered list of agent ids to render as bars (max 6). */
  agentIds: string[];
  agentNameMap: Record<string, string>;
  agentEmojiMap: Record<string, string>;
  isDark: boolean;
  fallbackEmoji: string;
}) {
  const doneCount = allTasks.filter((t) => t.status === "completed").length;
  const totalCount = allTasks.length;
  const pct = totalCount === 0 ? 0 : doneCount / totalCount;
  const ringRadius = 44;
  const ringCircumference = 2 * Math.PI * ringRadius;
  const ringDash = ringCircumference * pct;

  const countsByAgent = new Map<string, number>();
  for (const t of allTasks) {
    if (t.status !== "completed") continue;
    const effectiveId = t.assignment?.owner_agent_id || t.owner_agent_id;
    if (!effectiveId) continue;
    countsByAgent.set(effectiveId, (countsByAgent.get(effectiveId) ?? 0) + 1);
  }

  const members = agentIds.slice(0, 6);
  const data = members.map((id, i) => ({
    id,
    name: agentNameMap[id] ?? `Agent ${i + 1}`,
    emoji: agentEmojiMap[id] ?? fallbackEmoji,
    count: countsByAgent.get(id) ?? 0,
  }));
  const max = Math.max(1, ...data.map((d) => d.count));

  const stripeOverlay = isDark
    ? "repeating-linear-gradient(45deg, rgba(255,255,255,0.05) 0px, rgba(255,255,255,0.05) 1px, transparent 1px, transparent 6px)"
    : "repeating-linear-gradient(45deg, rgba(0,0,0,0.05) 0px, rgba(0,0,0,0.05) 1px, transparent 1px, transparent 6px)";

  return (
    <WidgetCard
      title="Tasks by agent"
      icon={<BarChart3 className="w-[14px] h-[14px]" />}
      action={<span className="text-[11px] text-[var(--text-tertiary)]">completed</span>}
    >
      <div className="h-full px-[14px] pt-[10px] pb-[12px] flex items-stretch gap-[12px]">
        {/* Progress ring */}
        <div className="flex flex-col items-center justify-center flex-shrink-0 w-[120px] gap-[6px]">
          <div className="relative w-[104px] h-[104px]">
            <svg width="104" height="104" viewBox="0 0 104 104" className="-rotate-90">
              <circle cx="52" cy="52" r={ringRadius} fill="none" stroke="var(--border-secondary)" strokeWidth="8" />
              <circle
                cx="52" cy="52" r={ringRadius} fill="none"
                stroke="var(--accent)" strokeWidth="8" strokeLinecap="round"
                strokeDasharray={`${ringDash} ${ringCircumference}`}
                style={{ transition: "stroke-dasharray 400ms ease" }}
              />
            </svg>
            <div className="absolute inset-0 flex flex-col items-center justify-center">
              <span className="text-[22px] font-bold text-[var(--text-primary)] leading-none">
                {Math.round(pct * 100)}%
              </span>
            </div>
          </div>
          <span className="text-[12px] text-[var(--text-tertiary)] tabular-nums">
            {doneCount}/{totalCount} done
          </span>
        </div>

        {/* Per-agent bars */}
        <div className="flex-1 min-w-0 flex items-end justify-end gap-[8px]">
          {data.map((d) => {
            const heightPct = Math.max(8, (d.count / max) * 100);
            const color = agentAvatarColor(d.name, isDark);
            return (
              <div key={d.id} className="flex-1 flex flex-col items-center min-w-0 h-full max-w-[44px]" title={d.name}>
                <span className="text-[13px] font-bold tabular-nums leading-none text-[var(--text-primary)] mb-[4px]">
                  {d.count}
                </span>
                <div
                  className="relative w-full flex-1 rounded-full overflow-hidden"
                  style={{ backgroundColor: "var(--bg-tertiary)", backgroundImage: stripeOverlay }}
                >
                  <div
                    className="absolute bottom-0 left-0 right-0 rounded-full"
                    style={{
                      height: `${heightPct}%`,
                      background: `linear-gradient(180deg, ${color} 0%, ${color}D9 100%)`,
                      boxShadow: `0 0 0 1px ${color}33`,
                      transition: "height 500ms cubic-bezier(0.22, 1, 0.36, 1)",
                    }}
                  />
                  <div className="absolute bottom-[4px] left-[4px] right-[4px] aspect-square">
                    <div
                      className="w-full h-full rounded-full flex items-center justify-center select-none"
                      style={{
                        backgroundColor: isDark ? "rgba(255,255,255,0.92)" : "#ffffff",
                        boxShadow: `inset 0 0 0 1px ${isDark ? "rgba(0,0,0,0.15)" : "rgba(0,0,0,0.08)"}, 0 1px 2px ${isDark ? "rgba(0,0,0,0.4)" : "rgba(0,0,0,0.08)"}`,
                        fontSize: "clamp(14px, 60%, 22px)",
                      }}
                    >
                      {d.emoji}
                    </div>
                  </div>
                </div>
                <div className="mt-[6px] text-[10px] text-[var(--text-tertiary)] truncate w-full text-center">
                  {d.name}
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </WidgetCard>
  );
}
