import { useState, useEffect, useRef, useCallback, useMemo } from "react";
import { WorkflowSummary } from "../../types/workflow";
import { AgentCommand, Skill } from "../../lib/api";
import { motion, AnimatePresence } from "framer-motion";
import { useWorkflowStore } from "../../stores/workflowStore";

type SlashItem =
  | { kind: "workflow"; item: WorkflowSummary }
  | { kind: "command"; item: AgentCommand }
  | { kind: "skill"; item: Skill };

interface SlashCommandPopoverProps {
  workflows: WorkflowSummary[];
  agentCommands: AgentCommand[];
  skills: Skill[];
  query: string;
  visible: boolean;
  onSelect: (workflow: WorkflowSummary) => void;
  onSelectCommand: (command: AgentCommand) => void;
  onSelectSkill: (skill: Skill) => void;
  onClose: () => void;
}

export function SlashCommandPopover({
  workflows,
  agentCommands,
  skills,
  query,
  visible,
  onSelect,
  onSelectCommand,
  onSelectSkill,
  onClose,
}: SlashCommandPopoverProps) {
  const [activeIndex, setActiveIndex] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);
  const fetchWorkflows = useWorkflowStore((s) => s.fetchWorkflows);

  // Fetch workflows when popover becomes visible
  useEffect(() => {
    if (visible) {
      fetchWorkflows();
    }
  }, [visible, fetchWorkflows]);

  const q = query.toLowerCase();

  const filteredWorkflows = useMemo(
    () =>
      workflows.filter(
        (w) =>
          w.name.toLowerCase().includes(q) ||
          (w.description ?? "").toLowerCase().includes(q),
      ),
    [workflows, q],
  );

  const filteredCommands = useMemo(
    () =>
      agentCommands.filter(
        (c) =>
          c.slug.toLowerCase().includes(q) ||
          c.name.toLowerCase().includes(q) ||
          (c.description ?? "").toLowerCase().includes(q),
      ),
    [agentCommands, q],
  );

  const filteredSkills = useMemo(
    () =>
      skills.filter(
        (s) =>
          s.id.toLowerCase().includes(q) ||
          s.title.toLowerCase().includes(q) ||
          (s.description ?? "").toLowerCase().includes(q),
      ),
    [skills, q],
  );

  // Build unified item list for keyboard navigation
  const allItems: SlashItem[] = useMemo(
    () => [
      ...filteredWorkflows.map((w): SlashItem => ({ kind: "workflow", item: w })),
      ...filteredCommands.map((c): SlashItem => ({ kind: "command", item: c })),
      ...filteredSkills.map((s): SlashItem => ({ kind: "skill", item: s })),
    ],
    [filteredWorkflows, filteredCommands, filteredSkills],
  );

  // Reset active index when query or visibility changes
  useEffect(() => {
    setActiveIndex(0);
  }, [query, visible]);

  // Auto-dismiss after typing 2+ chars with no results
  const noResultLenRef = useRef(0);
  useEffect(() => {
    if (!visible) { noResultLenRef.current = 0; return; }
    if (allItems.length === 0) {
      noResultLenRef.current++;
      if (noResultLenRef.current >= 2) {
        onClose();
        noResultLenRef.current = 0;
      }
    } else {
      noResultLenRef.current = 0;
    }
  }, [query, visible, allItems.length, onClose]);

  // Scroll active item into view
  useEffect(() => {
    const list = listRef.current;
    if (!list) return;
    const items = list.querySelectorAll("[role='option']");
    const active = items[activeIndex] as HTMLElement | undefined;
    active?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  const handleItemSelect = useCallback(
    (item: SlashItem) => {
      if (item.kind === "workflow") {
        onSelect(item.item);
      } else if (item.kind === "command") {
        onSelectCommand(item.item);
      } else {
        onSelectSkill(item.item);
      }
    },
    [onSelect, onSelectCommand, onSelectSkill],
  );

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (!visible) return;

      if (allItems.length === 0) {
        if (e.key === "Escape") {
          e.preventDefault();
          e.stopPropagation();
          onClose();
        }
        return;
      }

      if (e.key === "ArrowDown") {
        e.preventDefault();
        e.stopPropagation();
        setActiveIndex((i) => (i + 1) % allItems.length);
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        e.stopPropagation();
        setActiveIndex((i) => (i - 1 + allItems.length) % allItems.length);
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        e.stopPropagation();
        handleItemSelect(allItems[activeIndex]);
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      }
    },
    [visible, allItems, activeIndex, handleItemSelect, onClose],
  );

  useEffect(() => {
    if (!visible) return;
    window.addEventListener("keydown", handleKeyDown, true);
    return () => window.removeEventListener("keydown", handleKeyDown, true);
  }, [visible, handleKeyDown]);

  // Compute the global index offsets where each section starts
  const commandStartIndex = filteredWorkflows.length;
  const skillStartIndex = commandStartIndex + filteredCommands.length;

  // Category headers ("Workflows" / "Commands" / "Skills") are only shown
  // when more than one category is present — a single-category result list
  // doesn't need a label to disambiguate itself.
  const categoryCount = [
    filteredWorkflows.length > 0,
    filteredCommands.length > 0,
    filteredSkills.length > 0,
  ].filter(Boolean).length;
  const showCategoryLabels = categoryCount > 1;

  return (
    <AnimatePresence>
      {visible && (
        <motion.div
          initial={{ opacity: 0, y: 10, scale: 0.98 }}
          animate={{ opacity: 1, y: 0, scale: 1 }}
          exit={{ opacity: 0, y: 10, scale: 0.98 }}
          transition={{ duration: 0.15, ease: "easeOut" }}
          role="listbox"
          aria-label="Slash commands"
          id="slash-command-listbox"
          className="absolute bottom-full left-0 mb-3 w-[300px] max-h-[220px] overflow-hidden rounded-xl border border-[var(--border-primary)] bg-[var(--bg-secondary)] shadow-xl z-50"
        >
          <div ref={listRef} className="overflow-y-auto max-h-[220px] py-1.5 px-1.5 flex flex-col gap-[2px] custom-scrollbar">
            {allItems.length === 0 ? (
              <div className="px-3 py-2 text-[13px] text-[var(--text-tertiary)]">
                No results found
              </div>
            ) : (
              <>
                {/* Workflows section */}
                {filteredWorkflows.length > 0 && (
                  <>
                    {showCategoryLabels && (
                      <div className="px-3 pt-1 pb-0.5 text-[11px] font-medium uppercase tracking-wider text-[var(--text-tertiary)]">
                        Workflows
                      </div>
                    )}
                    {filteredWorkflows.map((workflow, i) => (
                      <button
                        key={`w-${workflow.id}`}
                        id={`slash-option-${i}`}
                        type="button"
                        role="option"
                        aria-selected={i === activeIndex}
                        onMouseDown={(e) => {
                          e.preventDefault();
                          onSelect(workflow);
                        }}
                        onMouseEnter={() => setActiveIndex(i)}
                        className={`w-full flex flex-col gap-0.5 text-left px-3 py-1.5 rounded-lg cursor-pointer transition-colors ${
                          i === activeIndex
                            ? "bg-[var(--bg-hover)] text-[var(--text-primary)]"
                            : "text-[var(--text-secondary)] hover:bg-[var(--bg-input)]"
                        }`}
                      >
                        <div className="text-[14px] leading-tight font-bold text-[var(--text-primary)]">
                          {workflow.name}
                        </div>
                        {workflow.description && (
                          <span
                            className={`text-[12px] leading-tight truncate max-w-full ${
                              i === activeIndex
                                ? "text-[var(--text-secondary)]"
                                : "text-[var(--text-tertiary)]"
                            }`}
                          >
                            {workflow.description}
                          </span>
                        )}
                      </button>
                    ))}
                  </>
                )}

                {/* Agent commands section */}
                {filteredCommands.length > 0 && (
                  <>
                    {showCategoryLabels && (
                      <div className="px-3 pt-2 pb-0.5 text-[11px] font-medium uppercase tracking-wider text-[var(--text-tertiary)]">
                        Commands
                      </div>
                    )}
                    {filteredCommands.map((cmd, i) => {
                      const globalIndex = commandStartIndex + i;
                      return (
                        <button
                          key={`c-${cmd.slug}`}
                          id={`slash-option-${globalIndex}`}
                          type="button"
                          role="option"
                          aria-selected={globalIndex === activeIndex}
                          onMouseDown={(e) => {
                            e.preventDefault();
                            onSelectCommand(cmd);
                          }}
                          onMouseEnter={() => setActiveIndex(globalIndex)}
                          className={`w-full flex flex-col gap-0.5 text-left px-3 py-1.5 rounded-lg cursor-pointer transition-colors ${
                            globalIndex === activeIndex
                              ? "bg-[var(--bg-hover)] text-[var(--text-primary)]"
                              : "text-[var(--text-secondary)] hover:bg-[var(--bg-input)]"
                          }`}
                        >
                          <div className="text-[14px] leading-tight font-bold text-[var(--text-primary)]">
                            /{cmd.slug}
                          </div>
                          {cmd.description && (
                            <span
                              className={`text-[12px] leading-tight truncate max-w-full ${
                                globalIndex === activeIndex
                                  ? "text-[var(--text-secondary)]"
                                  : "text-[var(--text-tertiary)]"
                              }`}
                            >
                              {cmd.description}
                            </span>
                          )}
                        </button>
                      );
                    })}
                  </>
                )}

                {/* Studio skills section */}
                {filteredSkills.length > 0 && (
                  <>
                    {showCategoryLabels && (
                      <div className="px-3 pt-2 pb-0.5 text-[11px] font-medium uppercase tracking-wider text-[var(--text-tertiary)]">
                        Skills
                      </div>
                    )}
                    {filteredSkills.map((skill, i) => {
                      const globalIndex = skillStartIndex + i;
                      return (
                        <button
                          key={`s-${skill.id}`}
                          id={`slash-option-${globalIndex}`}
                          type="button"
                          role="option"
                          aria-selected={globalIndex === activeIndex}
                          onMouseDown={(e) => {
                            e.preventDefault();
                            onSelectSkill(skill);
                          }}
                          onMouseEnter={() => setActiveIndex(globalIndex)}
                          className={`w-full flex flex-col gap-0.5 text-left px-3 py-1.5 rounded-lg cursor-pointer transition-colors ${
                            globalIndex === activeIndex
                              ? "bg-[var(--bg-hover)] text-[var(--text-primary)]"
                              : "text-[var(--text-secondary)] hover:bg-[var(--bg-input)]"
                          }`}
                        >
                          <div className="text-[14px] leading-tight font-bold text-[var(--text-primary)]">
                            /{skill.id}
                          </div>
                          {skill.description && (
                            <span
                              className={`text-[12px] leading-tight truncate max-w-full ${
                                globalIndex === activeIndex
                                  ? "text-[var(--text-secondary)]"
                                  : "text-[var(--text-tertiary)]"
                              }`}
                            >
                              {skill.description}
                            </span>
                          )}
                        </button>
                      );
                    })}
                  </>
                )}
              </>
            )}
          </div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
