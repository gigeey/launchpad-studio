import { useEffect, useRef, useState } from "react";
import { ChevronDown, Check, SlidersHorizontal } from "lucide-react";

export type CompetencyBulkMode = "all" | "none" | "select";

interface CompetencyBulkControlProps {
  mode: CompetencyBulkMode;
  onModeChange: (mode: CompetencyBulkMode) => void;
}

const OPTIONS: { id: CompetencyBulkMode; label: string }[] = [
  { id: "all", label: "All" },
  { id: "none", label: "None" },
  { id: "select", label: "Select" },
];

export function CompetencyBulkControl({ mode, onModeChange }: CompetencyBulkControlProps) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  const activeLabel = OPTIONS.find((o) => o.id === mode)?.label ?? "Select";

  return (
    <div ref={ref} className="relative flex-shrink-0">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-haspopup="listbox"
        aria-expanded={open}
        className="h-[34px] px-[12px] rounded-[10px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] text-[13px] font-medium text-[var(--text-primary)] flex items-center gap-[6px] cursor-pointer hover:bg-[var(--bg-tertiary)] transition-colors"
      >
        <SlidersHorizontal className="w-[13px] h-[13px] opacity-80" />
        <span>{activeLabel}</span>
        <ChevronDown className="w-[12px] h-[12px] opacity-80" />
      </button>
      {open && (
        <div
          role="listbox"
          className="absolute left-0 top-full mt-1 w-[140px] rounded-[10px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] shadow-xl z-20 p-1"
        >
          {OPTIONS.map((opt) => {
            const active = opt.id === mode;
            return (
              <button
                key={opt.id}
                type="button"
                role="option"
                aria-selected={active}
                onClick={() => {
                  setOpen(false);
                  onModeChange(opt.id);
                }}
                className="w-full flex items-center justify-between gap-2 px-2.5 py-1.5 rounded-[6px] text-left text-[13px] text-[var(--text-primary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
              >
                <span>{opt.label}</span>
                {active && <Check className="w-[13px] h-[13px] text-[var(--accent)]" />}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
