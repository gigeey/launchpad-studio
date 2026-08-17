import { useState, useEffect, useRef } from "react";
import { Search, Check, X } from "lucide-react";
import type { DelegateTarget } from "../../types/api";
import { getAgents } from "../../lib/api";
import { CoordinatorBadge } from "./CoordinatorBadge";

/** Minimal shape needed for agent picker rows (populated from AgentSnapshot). */
interface AgentRow {
  id: string;
  name: string;
  emoji?: string;
  /** Server-computed across the full delegation graph (see AgentSnapshot.
   *  coordinator_level) — read directly rather than recomputed client-side,
   *  since this picker never loads delegates_to for every agent. */
  coordinator_level?: number;
}

interface AddressBookEditorProps {
  /** ID of the agent being edited (excluded from the candidate list). */
  profileId: string;
  /** Current delegates_to list. */
  value: DelegateTarget[];
  /** Called when the list changes. */
  onChange: (entries: DelegateTarget[]) => void;
}

/** Small toggle switch, in a "sm" size (inline in a collapsed row) or "md" size
 *  (the labeled row inside an expanded entry). Same on/off visual language at
 *  both sizes so the collapsed pill and the expanded control read as one control. */
function ToggleSwitch({
  checked,
  onChange,
  size = "md",
  testId,
  ariaLabel,
}: {
  checked: boolean;
  onChange: () => void;
  size?: "sm" | "md";
  testId?: string;
  ariaLabel?: string;
}) {
  const track = size === "sm" ? "w-[28px] h-[16px]" : "w-[36px] h-[20px]";
  const knob = size === "sm" ? "w-[12px] h-[12px]" : "w-[16px] h-[16px]";
  const travel = size === "sm" ? "translate-x-[12px]" : "translate-x-[16px]";
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={ariaLabel}
      // The collapsed "Current delegates" row wraps this switch inside its own
      // onClick (expand/collapse). Stop propagation so flipping the switch
      // never also toggles the row open/closed — that combo read as broken,
      // since the row would flip expanded state on every share-context edit.
      onClick={(e) => {
        e.stopPropagation();
        onChange();
      }}
      data-testid={testId}
      className={`relative ${track} rounded-full transition-colors duration-200 cursor-pointer flex-shrink-0 ${
        checked ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-secondary)]"
      }`}
    >
      <span
        className={`absolute top-[2px] left-[2px] ${knob} rounded-full bg-white shadow-sm transition-transform duration-200 ${
          checked ? travel : "translate-x-0"
        }`}
      />
    </button>
  );
}

/** Checkbox used in "Add more" picker rows. Its border matches the search
 *  input's own border (`--modal-border-primary`) so the control reads as
 *  part of the same system rather than a one-off.
 *
 *  Toggling it adds/removes the delegate immediately but deliberately does
 *  NOT close the picker — stopPropagation keeps the row's own onClick (which
 *  both selects *and* dismisses) from also firing, so the row this checkbox
 *  belongs to stays put in the list instead of vanishing, letting a user
 *  check several agents in one open pass. Clicking anywhere else on the row
 *  is the quick path: pick one and close. */
function Checkbox({
  checked,
  onToggle,
  ariaLabel,
  testId,
}: {
  checked: boolean;
  onToggle: () => void;
  ariaLabel?: string;
  testId?: string;
}) {
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={checked}
      aria-label={ariaLabel}
      data-testid={testId}
      onClick={(e) => {
        e.stopPropagation();
        onToggle();
      }}
      className={`w-[18px] h-[18px] rounded-[5px] border flex items-center justify-center flex-shrink-0 transition-colors cursor-pointer ${
        checked
          ? "bg-[var(--modal-accent)] border-[var(--modal-accent)]"
          : "bg-[var(--modal-bg)] border-[var(--modal-border-primary)]"
      }`}
    >
      {checked && <Check className="w-[12px] h-[12px] text-white" strokeWidth={3} />}
    </button>
  );
}

/** Shared icon square used by both already-added and candidate rows, so the
 *  two list blocks read as one visual system. */
function AgentIcon({ emoji }: { emoji?: string }) {
  return (
    <span className="w-[28px] h-[28px] rounded-[8px] bg-[var(--modal-bg-tertiary)] flex items-center justify-center text-[14px] flex-shrink-0">
      {emoji ?? "🤖"}
    </span>
  );
}

/** Section editor for an agent's delegate address book.
 *
 *  "Current delegates" is the only list block shown at rest. The "Add more"
 *  candidate list lives in a floating picker anchored to the search input,
 *  opened on focus (and closed on outside click / Escape) rather than
 *  permanently occupying vertical space below the search box — this keeps
 *  the search box doing double duty as both a filter for the delegates
 *  already added and the trigger for picking new ones, mirroring the
 *  combobox pattern used elsewhere (e.g. `AddMenu` in CompetenciesModal.tsx).
 *  Candidate rows still don't carry a purpose + share-toggle + remove control
 *  the way added rows do, so the two stay visually distinct blocks even
 *  though only one is ever on screen below the input at a time.
 *
 *  Picker rows show a checkbox rather than the added/not-added state being
 *  implied by the row simply vanishing once picked. The row lists every
 *  non-self agent regardless of membership, so checking one on/off never
 *  reshuffles the list — clicking the checkbox itself toggles membership
 *  and leaves the picker open (multi-select), while clicking anywhere else
 *  on the row adds it (if not already added) and closes the picker (quick
 *  single pick). See `Checkbox`'s docstring for the click-target split.
 *
 *  Alias editing is deliberately NOT exposed here right now — entries still
 *  carry a `name` field (defaulted to the target's own name at add time) for
 *  a future alias feature, but there is no UI to customize it today; every
 *  row displays the target agent's live name.
 *
 *  Purpose (real, load-bearing free text sent to the backend) stays out of
 *  the collapsed row — a ≤240-char field can't be shown untruncated inline,
 *  and an always-open textarea would just rebuild the bulky card this
 *  replaces. Clicking an added row expands it in place to reveal the purpose
 *  textarea and the full share-context toggle + description.
 *
 *  This component is modal-only (only ever rendered inside AgentProfileModal,
 *  a portaled surface), so every CSS var below is deliberately from the
 *  `--modal-*` namespace rather than the plain page-level family. */
export function AddressBookEditor({
  profileId,
  value,
  onChange,
}: AddressBookEditorProps) {
  const [agents, setAgents] = useState<AgentRow[]>([]);
  const [search, setSearch] = useState("");
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  const pickerRef = useRef<HTMLDivElement>(null);

  // Close the "Add more" picker on outside click / Escape. Scoped to
  // pickerRef (search input + dropdown share one wrapper) rather than a
  // blur handler, since clicking a candidate row would otherwise blur the
  // input before the row's onClick fires.
  useEffect(() => {
    if (!pickerOpen) return;
    function handlePointerDown(e: MouseEvent) {
      if (pickerRef.current && !pickerRef.current.contains(e.target as Node)) {
        setPickerOpen(false);
      }
    }
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") {
        setPickerOpen(false);
        (document.activeElement as HTMLElement | null)?.blur();
      }
    }
    document.addEventListener("mousedown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [pickerOpen]);

  useEffect(() => {
    getAgents()
      .then((snaps) =>
        setAgents(
          snaps.map((s) => ({
            id: s.agent_id,
            name: s.name,
            emoji: s.emoji,
            coordinator_level: s.coordinator_level,
          })),
        ),
      )
      .catch(() => {});
  }, []);

  // Name lookup map for target resolution in existing entries
  const agentById = new Map<string, AgentRow>(agents.map((a) => [a.id, a]));

  const addedIds = new Set(value.map((e) => e.target_agent_id));

  // Everyone eligible to show in the "Add more" picker (only self is excluded).
  // Unlike the old plus-icon flow, already-added agents are NOT filtered out
  // here — they stay in the list checked, so ticking a box never reshuffles
  // rows out from under a user mid multi-select. The checkbox is the only
  // thing that reflects membership; see `Checkbox` docstring above.
  const pickerAgents = agents.filter((a) => a.id !== profileId);

  const query = search.trim().toLowerCase();
  const visibleEntries = query
    ? value.filter((e) => {
        const name = agentById.get(e.target_agent_id)?.name ?? e.name;
        return name.toLowerCase().includes(query);
      })
    : value;
  const visiblePickerAgents = query
    ? pickerAgents.filter((a) => a.name.toLowerCase().includes(query))
    : pickerAgents;

  function addEntry(agent: AgentRow) {
    const entry: DelegateTarget = {
      target_agent_id: agent.id,
      name: agent.name,
      purpose: "",
      share_context_allowed: false,
    };
    onChange([...value, entry]);
    setExpandedId(agent.id);
  }

  function removeEntry(idx: number) {
    onChange(value.filter((_, i) => i !== idx));
  }

  function updateEntry(idx: number, patch: Partial<DelegateTarget>) {
    onChange(value.map((e, i) => (i === idx ? { ...e, ...patch } : e)));
  }

  // Checkbox path: bidirectional add/remove, used only by the picker's
  // per-row checkbox (never by the row click itself — that one only adds).
  function toggleAgent(agent: AgentRow) {
    const idx = value.findIndex((e) => e.target_agent_id === agent.id);
    if (idx === -1) {
      addEntry(agent);
    } else {
      removeEntry(idx);
    }
  }

  return (
    <div className="flex flex-col gap-[14px]">
      {/* Search — filters the "Current delegates" block below by agent name,
          and doubles as the trigger for the "Add more" picker, which opens
          as a floating dropdown anchored to this input on focus (closes on
          outside click / Escape) rather than sitting permanently on screen.
          Same corner radius as the list block below (not a pill) so it reads
          as one system with it. */}
      <div className="relative" ref={pickerRef}>
        <Search className="absolute left-[12px] top-1/2 -translate-y-1/2 w-[15px] h-[15px] text-[var(--modal-text-tertiary)] pointer-events-none" />
        <input
          type="text"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          onFocus={() => setPickerOpen(true)}
          placeholder="Search agents…"
          className="w-full h-[40px] pl-[36px] pr-[12px] rounded-[12px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] text-[14px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] transition-colors"
          data-testid="address-book-search"
        />

        {pickerOpen && (
          <div
            className="absolute left-0 right-0 top-full mt-[6px] rounded-[12px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] shadow-xl z-20 overflow-hidden"
            data-testid="address-book-picker"
          >
            <p className="px-[12px] pt-[10px] pb-[6px] text-[11px] font-medium text-[var(--modal-text-tertiary)] uppercase tracking-wide">
              Add more
            </p>
            <div className="divide-y divide-[var(--modal-border-primary)] max-h-[240px] overflow-y-auto">
              {pickerAgents.length === 0 ? (
                <p className="px-[12px] py-[10px] text-[13px] text-[var(--modal-text-tertiary)]">
                  No agents available to add
                </p>
              ) : visiblePickerAgents.length === 0 ? (
                <p className="px-[12px] py-[10px] text-[13px] text-[var(--modal-text-tertiary)]">
                  No agents match &quot;{search.trim()}&quot;
                </p>
              ) : (
                visiblePickerAgents.map((agent) => {
                  const level = agent.coordinator_level ?? 0;
                  const added = addedIds.has(agent.id);
                  return (
                    <div
                      key={agent.id}
                      // Row click (anywhere but the checkbox): select this
                      // agent (if not already added) and dismiss the picker —
                      // the quick "pick one and go" path.
                      onClick={() => {
                        if (!added) addEntry(agent);
                        setPickerOpen(false);
                      }}
                      className="flex items-center gap-[10px] px-[12px] py-[10px] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                      data-testid={`candidate-agent-${agent.id}`}
                    >
                      <AgentIcon emoji={agent.emoji} />
                      <span className="flex-1 min-w-0 text-[15px] font-semibold text-[var(--modal-text-primary)] truncate">
                        {agent.name}
                      </span>
                      <CoordinatorBadge level={level} size={16} />
                      <Checkbox
                        checked={added}
                        ariaLabel={`${added ? "Remove" : "Add"} ${agent.name}`}
                        testId={`candidate-checkbox-${agent.id}`}
                        onToggle={() => toggleAgent(agent)}
                      />
                    </div>
                  );
                })
              )}
            </div>
          </div>
        )}
      </div>

      {/* Reachable agents — the section title only shows once there's a list
          it's actually labeling; an empty state reads fine as bare
          placeholder text with no label sitting above it. */}
      <div className="flex flex-col gap-[6px]">
        {value.length === 0 ? (
          // No delegates added at all — show the placeholder as plain text,
          // with no title and no bordered container around it. Both only
          // earn their keep once there's something to label/frame.
          <p className="px-[2px] text-[13px] text-[var(--modal-text-tertiary)]">
            No delegates added yet
          </p>
        ) : (
          <>
            <p className="text-[11px] font-medium text-[var(--modal-text-tertiary)] uppercase tracking-wide">
              Reachable agents
            </p>
            <div
              data-testid="address-book-delegates"
              className="rounded-[12px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] divide-y divide-[var(--modal-border-primary)] overflow-hidden"
            >
            {visibleEntries.length === 0 ? (
              <p className="px-[12px] py-[10px] text-[13px] text-[var(--modal-text-tertiary)]">
                No delegates match &quot;{search.trim()}&quot;
              </p>
            ) : (
              visibleEntries.map((entry) => {
                const idx = value.indexOf(entry);
                const targetAgent = agentById.get(entry.target_agent_id);
                const level = targetAgent?.coordinator_level ?? 0;
                const displayName = targetAgent?.name ?? entry.name ?? entry.target_agent_id;
                const expanded = expandedId === entry.target_agent_id;

                return (
                  <div key={entry.target_agent_id} data-testid="address-book-entry">
                    <div
                      onClick={() =>
                        setExpandedId(expanded ? null : entry.target_agent_id)
                      }
                      className="flex items-center gap-[10px] px-[12px] py-[10px] cursor-pointer hover:bg-[var(--modal-bg-hover)] transition-colors min-w-0"
                    >
                      <AgentIcon emoji={targetAgent?.emoji} />
                      <div className="flex-1 min-w-0">
                        <div className="flex items-center gap-[6px] min-w-0">
                          <span className="min-w-0 text-[15px] font-semibold text-[var(--modal-text-primary)] truncate">
                            {displayName}
                          </span>
                          <CoordinatorBadge level={level} size={16} />
                        </div>
                        {!expanded && (
                          <p className="text-[11px] text-[var(--modal-text-tertiary)] truncate">
                            {entry.purpose.trim() || "No purpose set — click to add one"}
                          </p>
                        )}
                      </div>
                      <ToggleSwitch
                        size="sm"
                        checked={entry.share_context_allowed}
                        ariaLabel={`Toggle context sharing for ${displayName}`}
                        onChange={() => {
                          updateEntry(idx, {
                            share_context_allowed: !entry.share_context_allowed,
                          });
                        }}
                      />
                      <button
                        type="button"
                        onClick={(e) => {
                          e.stopPropagation();
                          removeEntry(idx);
                        }}
                        className="w-[26px] h-[26px] rounded-[7px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--error-bg)] hover:text-[var(--error)] transition-colors cursor-pointer flex-shrink-0"
                        aria-label={`Remove ${displayName}`}
                        data-testid="remove-entry-button"
                      >
                        <X className="w-[12px] h-[12px]" />
                      </button>
                    </div>

                    {expanded && (
                      <div className="px-[12px] pb-[12px] pt-[2px] flex flex-col gap-[10px]">
                        <div>
                          <label className="block text-[12px] font-medium text-[var(--modal-text-secondary)] mb-[4px]">
                            Purpose{" "}
                            <span className="font-normal text-[var(--modal-text-tertiary)]">
                              (≤240 chars)
                            </span>
                          </label>
                          <textarea
                            value={entry.purpose}
                            onChange={(e) =>
                              updateEntry(idx, { purpose: e.target.value.slice(0, 240) })
                            }
                            placeholder="Describe what this agent handles…"
                            rows={2}
                            className="w-full px-[10px] py-[8px] rounded-[8px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-input)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] outline-none focus:border-[var(--modal-accent)] transition-all resize-none leading-relaxed"
                          />
                        </div>
                        <div className="flex items-center justify-between">
                          <div>
                            <p className="text-[12px] font-medium text-[var(--modal-text-primary)]">
                              Allow context sharing
                            </p>
                            <p className="text-[11px] text-[var(--modal-text-tertiary)]">
                              Lets this agent receive your full conversation context (transcript sharing)
                            </p>
                          </div>
                          <ToggleSwitch
                            checked={entry.share_context_allowed}
                            testId="share-context-toggle"
                            ariaLabel={`Toggle context sharing for ${displayName}`}
                            onChange={() =>
                              updateEntry(idx, {
                                share_context_allowed: !entry.share_context_allowed,
                              })
                            }
                          />
                        </div>
                      </div>
                    )}
                  </div>
                );
              })
            )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
