import { useState, useEffect, useRef, useCallback } from "react";
import { TeamMember } from "../../types/api";
import { motion, AnimatePresence } from "framer-motion";

interface MentionAutocompleteProps {
  members: TeamMember[];
  query: string;
  visible: boolean;
  onSelect: (agentId: string) => void;
  onClose: () => void;
  /** Map of agent_id → display name. Falls back to agent_id when missing. */
  agentNameMap?: Record<string, string>;
}

export function MentionAutocomplete({
  members,
  query,
  visible,
  onSelect,
  onClose,
  agentNameMap = {},
}: MentionAutocompleteProps) {
  const [activeIndex, setActiveIndex] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  const resolveName = (agentId: string) => agentNameMap[agentId] ?? agentId;

  const filtered = members.filter((m) => {
    const q = query.toLowerCase();
    return resolveName(m.agent_id).toLowerCase().includes(q);
  });

  // Reset active index when query or visibility changes
  useEffect(() => {
    setActiveIndex(0);
  }, [query, visible]);

  // Scroll active item into view
  useEffect(() => {
    const list = listRef.current;
    if (!list) return;
    const active = list.children[activeIndex] as HTMLElement | undefined;
    active?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (!visible || filtered.length === 0) return;

      if (e.key === "ArrowDown") {
        e.preventDefault();
        e.stopPropagation();
        setActiveIndex((i) => (i + 1) % filtered.length);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        e.stopPropagation();
        setActiveIndex((i) => (i - 1 + filtered.length) % filtered.length);
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        e.stopPropagation();
        onSelect(filtered[activeIndex].agent_id);
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      }
    },
    [visible, filtered, activeIndex, onSelect, onClose]
  );

  useEffect(() => {
    if (!visible) return;
    window.addEventListener("keydown", handleKeyDown, true);
    return () => window.removeEventListener("keydown", handleKeyDown, true);
  }, [visible, handleKeyDown]);

  return (
    <AnimatePresence>
      {visible && filtered.length > 0 && (
        <motion.div
          initial={{ opacity: 0, y: 10, scale: 0.98 }}
          animate={{ opacity: 1, y: 0, scale: 1 }}
          exit={{ opacity: 0, y: 10, scale: 0.98 }}
          transition={{ duration: 0.15, ease: "easeOut" }}
          ref={listRef}
          role="listbox"
          aria-label="Agent mentions"
          id="mention-autocomplete-listbox"
          className="absolute bottom-full left-0 mb-3 w-[260px] max-h-[220px] overflow-y-auto rounded-xl border border-[var(--border-primary)] bg-[var(--bg-secondary)] shadow-xl z-50 p-1 flex flex-col gap-[2px]"
        >
          {filtered.map((member, i) => (
            <button
              key={member.agent_id}
              id={`mention-option-${i}`}
              type="button"
              role="option"
              aria-selected={i === activeIndex}
              onMouseDown={(e) => {
                e.preventDefault();
                onSelect(member.agent_id);
              }}
              onMouseEnter={() => setActiveIndex(i)}
              className={`w-full flex flex-col gap-0.5 text-left px-3 py-1.5 rounded-lg cursor-pointer transition-colors ${i === activeIndex
                ? "bg-[var(--bg-hover)] text-[var(--text-primary)]"
                : "text-[var(--text-secondary)] hover:bg-[var(--bg-input)]"
                }`}
            >
              <div className="text-[14px] leading-tight flex items-center gap-[2px]">
                @
                <span className={`font-bold ${i === activeIndex ? "text-[var(--text-primary)]" : "text-[var(--text-primary)]"}`}>
                  {resolveName(member.agent_id)}
                </span>
              </div>
              {member.role_description && (
                <span className={`text-[12px] leading-tight truncate max-w-full ${i === activeIndex ? "text-[var(--text-secondary)]" : "text-[var(--text-tertiary)]"
                  }`}>
                  {member.role_description}
                </span>
              )}
            </button>
          ))}
        </motion.div>
      )}
    </AnimatePresence>
  );
}
