import { useEffect, useMemo, useRef, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import {
  X,
  User,
  Bot,
  MoreHorizontal,
  Trash2,
  Search,
  Plus,
  ChevronDown,
  Puzzle,
  Package,
  FilePlus,
  Github,
  Link2,
  Upload,
  FolderPlus,
  Check,
  RefreshCw,
  ScrollText,
  FileText,
  Settings,
  AlertTriangle,
  GitBranch,
} from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";
import { useCompetenciesModalStore } from "../../stores/competenciesModalStore";
import { useSkillsStore } from "../../stores/skillsStore";
import { useRulesStore } from "../../stores/rulesStore";
import { useInstructionsStore } from "../../stores/instructionsStore";
import { useWorkflowsStore, isWorkflowEnabled } from "../../stores/workflowsStore";
import {
  getAgent,
  ApiError,
  listLaunchpadGlobalSkills,
  listLaunchpadProjectSkills,
  setLaunchpadGlobalSkillEnabled,
  setLaunchpadProjectSkillEnabled,
  promoteLaunchpadSkill,
} from "../../lib/api";
import type { Skill, Rule, Instruction, AgentProfile, PluginEnablement, LaunchpadSkillEntry } from "../../lib/api";
import type { PluginEntry, PluginSkill, PluginCommandError } from "../../types/plugins";
import { LinkImportModal } from "./LinkImportModal";
import { InstructionFilenamesEditor } from "./InstructionFilenamesEditor";
import { MarkdownViewerModal } from "../MarkdownViewerModal";
import { WorkflowTile } from "./WorkflowTile";
import { CompetencyBulkControl, type CompetencyBulkMode } from "./CompetencyBulkControl";

type TabId = "skills" | "rules" | "workflows" | "instructions";
type AddedBy = Skill["added_by"];
type AddAction = "create" | "link" | "file" | "folder";

const RULES_INSTRUCTIONS_WARN_THRESHOLD = 20;

// The Rules and Instructions tabs are hidden pending a product decision on
// whether to keep the feature. The implementation (panels, stores, handlers)
// is left in place so it can be revived quickly — flip this to false to
// restore both tabs.
const RULES_INSTRUCTIONS_HIDDEN = true;

interface AddMenuItem {
  id: AddAction;
  label: string;
  icon: typeof FilePlus;
}

const SKILL_ADD_ITEMS: AddMenuItem[] = [
  { id: "create", label: "Create skill", icon: FilePlus },
  { id: "link", label: "Import from link", icon: Link2 },
  { id: "file", label: "Import from file", icon: Upload },
  { id: "folder", label: "Import from folder", icon: FolderPlus },
];

const RULE_ADD_ITEMS: AddMenuItem[] = [
  { id: "file", label: "Import file…", icon: Upload },
  { id: "folder", label: "Import folder…", icon: FolderPlus },
  { id: "link", label: "Import from link…", icon: Link2 },
];

const WORKFLOW_ADD_ITEMS: AddMenuItem[] = [
  { id: "folder", label: "Import folder…", icon: FolderPlus },
];

const ADDED_BY_ICON: Record<AddedBy, typeof User> = {
  user: User,
  agent: Bot,
  github: Github,
  link: Link2,
};

const ADDED_BY_LABEL: Record<AddedBy, string> = {
  user: "Added by you",
  agent: "Added by agent",
  github: "Added from GitHub",
  link: "Added from a link",
};

function formatUpdated(iso: string): string {
  const d = new Date(iso);
  return d.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
  });
}

function formatRelativeTime(iso: string): string {
  const date = new Date(iso);
  const diffMs = Date.now() - date.getTime();
  const diffSec = Math.floor(diffMs / 1000);
  if (diffSec < 60) return "just now";
  const diffMin = Math.floor(diffSec / 60);
  if (diffMin < 60) return `${diffMin}m ago`;
  const diffHr = Math.floor(diffMin / 60);
  if (diffHr < 24) return `${diffHr}h ago`;
  const diffDay = Math.floor(diffHr / 24);
  if (diffDay < 30) return `${diffDay}d ago`;
  return date.toLocaleDateString([], { month: "short", day: "numeric" });
}

function formatUsageBadge(count: number | undefined, lastUsed: string | null | undefined): string | null {
  const n = count ?? 0;
  if (n === 0) return null;
  const base = n === 1 ? "Used 1 time" : `Used ${n} times`;
  if (lastUsed) return `${base} · ${formatRelativeTime(lastUsed)}`;
  return base;
}

function pluginErrorMessage(err: unknown): string {
  if (err && typeof err === "object" && "type" in err) {
    const e = err as PluginCommandError;
    if ("detail" in e && typeof e.detail === "string") return e.detail;
    return e.type;
  }
  if (err instanceof Error) return err.message;
  return String(err);
}

function useOutsideClick<T extends HTMLElement>(
  ref: React.RefObject<T | null>,
  open: boolean,
  close: () => void,
) {
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) close();
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open, close, ref]);
}

function AddMenu({
  size = "sm",
  align = "right",
  items = SKILL_ADD_ITEMS,
  onAction,
}: {
  size?: "sm" | "lg";
  align?: "left" | "right";
  items?: AddMenuItem[];
  onAction: (a: AddAction) => void;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useOutsideClick(ref, open, () => setOpen(false));

  const btnClass =
    size === "lg"
      ? "h-[40px] px-[16px] text-[14px] rounded-[10px] gap-[8px]"
      : "h-[34px] px-[12px] text-[13px] rounded-[10px] gap-[6px]";

  return (
    <div ref={ref} className="relative flex-shrink-0">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className={`${btnClass} bg-[var(--modal-accent)] text-[var(--text-on-accent)] font-medium flex items-center cursor-pointer hover:bg-[var(--modal-accent-hover)] transition-colors`}
      >
        <Plus className={size === "lg" ? "w-[16px] h-[16px]" : "w-[14px] h-[14px]"} />
        <span>Add</span>
        <ChevronDown
          className={size === "lg" ? "w-[14px] h-[14px] opacity-85" : "w-[12px] h-[12px] opacity-85"}
        />
      </button>
      {open && (
        <div
          className={`absolute ${
            align === "right" ? "right-0" : "left-1/2 -translate-x-1/2"
          } top-full mt-1 w-[210px] rounded-[10px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] shadow-xl z-20 p-1`}
        >
          {items.map((it) => {
            const Icon = it.icon;
            return (
              <button
                key={it.id}
                type="button"
                onClick={() => {
                  setOpen(false);
                  onAction(it.id);
                }}
                className="w-full flex items-center gap-2 px-2.5 py-1.5 rounded-[6px] text-left text-[13px] text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
              >
                <Icon className="w-[14px] h-[14px] text-[var(--modal-text-secondary)]" />
                <span>{it.label}</span>
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

function SkillTile({
  skill,
  onToggle,
  onDelete,
  onToggleAutoSync,
}: {
  skill: Skill;
  onToggle: () => void;
  onDelete: () => void;
  onToggleAutoSync: () => void;
}) {
  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
  useOutsideClick(menuRef, menuOpen, () => setMenuOpen(false));

  const AddedByIcon = ADDED_BY_ICON[skill.added_by];
  const isGithub = skill.added_by === "github";

  return (
    <div className="competency-tile rounded-[14px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg)] px-[16px] py-[14px] flex flex-col gap-[8px]">
      <div className="flex items-start justify-between gap-[12px]">
        <h3 className="text-[15px] font-semibold text-[var(--modal-text-primary)] truncate">
          {skill.title}
        </h3>
        <button
          type="button"
          onClick={onToggle}
          className={`relative w-[42px] h-[24px] rounded-full transition-colors cursor-pointer flex-shrink-0 ${
            skill.enabled ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"
          }`}
          aria-label={skill.enabled ? "Disable skill" : "Enable skill"}
        >
          <div
            className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${
              skill.enabled ? "translate-x-[20px]" : "translate-x-[2px]"
            }`}
          />
        </button>
      </div>

      <p className="text-[13px] text-[var(--modal-text-secondary)] leading-[18px] line-clamp-2 min-h-[36px]">
        {skill.description}
      </p>

      <div className="border-t border-[var(--modal-border-secondary)] -mx-[16px]" />

      <div className="flex items-center justify-between gap-[8px]">
        <div className="flex items-center gap-[8px] min-w-0 text-[12px] text-[var(--modal-text-tertiary)]">
          <span
            className="w-[20px] h-[20px] rounded-full border border-[var(--modal-border-secondary)] flex items-center justify-center flex-shrink-0"
            title={ADDED_BY_LABEL[skill.added_by]}
          >
            <AddedByIcon className="w-[11px] h-[11px]" />
          </span>
          <span className="truncate">Updated on {formatUpdated(skill.updated_on)}</span>
          {(() => {
            const badge = formatUsageBadge(skill.usage_count, skill.last_used);
            return badge ? (
              <span
                className="flex-shrink-0 px-[6px] py-[1px] rounded-full border border-[var(--modal-border-secondary)] text-[11px] text-[var(--modal-text-tertiary)]"
                title={skill.last_used ? `Last used ${formatRelativeTime(skill.last_used)}` : undefined}
              >
                {badge}
              </span>
            ) : null;
          })()}
          {isGithub && skill.auto_sync && (
            <span
              className="flex items-center gap-[3px] text-[var(--modal-accent)] flex-shrink-0"
              title="Auto-syncs from GitHub"
            >
              <RefreshCw className="w-[10px] h-[10px]" />
              <span>Syncing</span>
            </span>
          )}
        </div>

        <div className="relative flex-shrink-0" ref={menuRef}>
          <button
            type="button"
            onClick={() => setMenuOpen((o) => !o)}
            className="w-[28px] h-[28px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
            aria-label="More options"
          >
            <MoreHorizontal className="w-[16px] h-[16px]" />
          </button>
          {menuOpen && (
            <div className="absolute right-0 bottom-full mb-1 w-[200px] rounded-[10px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] shadow-xl z-10 p-1">
              {isGithub && (
                <>
                  <button
                    type="button"
                    onClick={() => {
                      setMenuOpen(false);
                      onToggleAutoSync();
                    }}
                    className="w-full flex items-center justify-between gap-2 px-2.5 py-1.5 rounded-[6px] text-left text-[13px] text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)] transition-colors cursor-pointer"
                  >
                    <span className="flex items-center gap-2">
                      <RefreshCw className="w-[14px] h-[14px] text-[var(--modal-text-secondary)]" />
                      <span>Auto-sync from repo</span>
                    </span>
                    {skill.auto_sync && <Check className="w-[13px] h-[13px] text-[var(--modal-accent)]" />}
                  </button>
                  <div className="my-1 border-t border-[var(--modal-border-secondary)]" />
                </>
              )}
              <button
                type="button"
                onClick={() => {
                  setMenuOpen(false);
                  onDelete();
                }}
                className="w-full flex items-center gap-2 px-2.5 py-1.5 rounded-[6px] text-left text-[13px] text-[var(--error)] hover:bg-[var(--error-bg)] transition-colors cursor-pointer"
              >
                <Trash2 className="w-[14px] h-[14px]" />
                <span>Delete</span>
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function PluginSkillTile({
  skill,
  checked,
  disabled,
  busy,
  usageCount,
  lastUsed,
  onToggle,
}: {
  skill: PluginSkill;
  checked: boolean;
  disabled: boolean;
  busy: boolean;
  usageCount?: number;
  lastUsed?: string | null;
  onToggle: () => void;
}) {
  return (
    <div
      className={`competency-tile rounded-[14px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg)] px-[16px] py-[14px] flex flex-col gap-[8px] ${
        disabled ? "opacity-60" : ""
      }`}
    >
      <div className="flex items-start justify-between gap-[12px]">
        <h3 className="text-[15px] font-semibold text-[var(--modal-text-primary)] truncate">
          {skill.skill_name}
        </h3>
        <button
          type="button"
          onClick={onToggle}
          disabled={disabled || busy}
          role="switch"
          aria-checked={checked}
          aria-label={checked ? "Disable skill" : "Enable skill"}
          className={`relative w-[42px] h-[24px] rounded-full transition-colors flex-shrink-0 ${
            checked ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"
          } ${disabled || busy ? "cursor-default" : "cursor-pointer"}`}
        >
          <div
            className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${
              checked ? "translate-x-[20px]" : "translate-x-[2px]"
            }`}
          />
        </button>
      </div>

      <p
        className="text-[13px] text-[var(--modal-text-secondary)] leading-[18px] line-clamp-2 min-h-[36px] font-mono truncate"
        title={skill.id}
      >
        {skill.id}
      </p>

      <div className="border-t border-[var(--modal-border-secondary)] -mx-[16px]" />

      <div className="flex items-center gap-[8px] text-[12px] text-[var(--modal-text-tertiary)]">
        <span
          className="w-[20px] h-[20px] rounded-full border border-[var(--modal-border-secondary)] flex items-center justify-center flex-shrink-0"
          title={`From plugin ${skill.plugin_name}`}
        >
          <Package className="w-[11px] h-[11px]" />
        </span>
        <span className="truncate">From plugin {skill.plugin_name}</span>
        {(() => {
          const badge = formatUsageBadge(usageCount, lastUsed);
          return badge ? (
            <span
              className="flex-shrink-0 px-[6px] py-[1px] rounded-full border border-[var(--modal-border-secondary)] text-[11px] text-[var(--modal-text-tertiary)]"
              title={lastUsed ? `Last used ${formatRelativeTime(lastUsed)}` : undefined}
            >
              {badge}
            </span>
          ) : null;
        })()}
      </div>
    </div>
  );
}

function LaunchpadSkillTile({
  skill,
  checked,
  busy,
  onToggle,
  overridesGlobal,
  overriddenByProject,
  onPromote,
  promoteBusy,
  promoteMessage,
}: {
  skill: LaunchpadSkillEntry;
  checked: boolean;
  busy: boolean;
  onToggle: () => void;
  /** Project tile only: a global skill shares this name. */
  overridesGlobal?: boolean;
  /** Global tile only: a project skill shadows this one — dim + annotate. */
  overriddenByProject?: boolean;
  /** Project tile only: "Make available globally". Omit on global tiles. */
  onPromote?: () => void;
  promoteBusy?: boolean;
  promoteMessage?: string | null;
}) {
  return (
    <div
      className={`competency-tile rounded-[14px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg)] px-[16px] py-[14px] flex flex-col gap-[8px] ${
        overriddenByProject ? "opacity-60" : ""
      }`}
    >
      <div className="flex items-start justify-between gap-[12px]">
        <h3 className="text-[15px] font-semibold text-[var(--modal-text-primary)] truncate" title={skill.path}>
          {skill.name}
        </h3>
        <button
          type="button"
          onClick={onToggle}
          disabled={busy}
          role="switch"
          aria-checked={checked}
          aria-label={checked ? "Disable skill" : "Enable skill"}
          className={`relative w-[42px] h-[24px] rounded-full transition-colors flex-shrink-0 ${
            checked ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"
          } ${busy ? "opacity-60 cursor-default" : "cursor-pointer"}`}
        >
          <div
            className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${
              checked ? "translate-x-[20px]" : "translate-x-[2px]"
            }`}
          />
        </button>
      </div>

      <p className="text-[13px] text-[var(--modal-text-secondary)] leading-[18px] line-clamp-2 min-h-[36px]">
        {skill.description || "No description"}
      </p>

      {(overridesGlobal || overriddenByProject) && (
        <div className="flex items-center gap-[6px]">
          {overridesGlobal && (
            <span className="text-[11px] px-[6px] py-[1px] rounded-full border border-[var(--modal-accent)] text-[var(--modal-accent)]">
              Overrides global
            </span>
          )}
          {overriddenByProject && (
            <span className="text-[11px] px-[6px] py-[1px] rounded-full border border-[var(--modal-border-secondary)] text-[var(--modal-text-tertiary)]">
              Overridden by project
            </span>
          )}
        </div>
      )}

      {onPromote && (
        <>
          <div className="border-t border-[var(--modal-border-secondary)] -mx-[16px]" />
          <div className="flex items-center justify-between gap-[8px]">
            <button
              type="button"
              onClick={onPromote}
              disabled={promoteBusy}
              className="text-[12px] text-[var(--modal-accent)] hover:underline cursor-pointer disabled:cursor-default disabled:opacity-60 text-left"
            >
              Make available globally
            </button>
            {promoteMessage && (
              <span className="text-[11px] text-[var(--modal-text-tertiary)]">{promoteMessage}</span>
            )}
          </div>
        </>
      )}
    </div>
  );
}

function RuleTile({
  rule,
  onToggle,
  onView,
  onDelete,
}: {
  rule: Rule;
  onToggle: () => void;
  onView: () => void;
  onDelete: () => void;
}) {
  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
  useOutsideClick(menuRef, menuOpen, () => setMenuOpen(false));

  const AddedByIcon = ADDED_BY_ICON[rule.added_by];
  const isGithub = rule.added_by === "github";
  const isTopLevel = !rule.id.includes("/");
  const snippet = (rule.description?.trim() || rule.content).trim();

  return (
    <div
      role="button"
      tabIndex={0}
      onClick={onView}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onView();
        }
      }}
      className="competency-tile rounded-[14px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg)] px-[16px] py-[14px] flex flex-col gap-[8px] cursor-pointer hover:border-[var(--modal-border-primary)] transition-colors"
    >
      <div className="flex items-start justify-between gap-[12px]">
        <div className="flex flex-col gap-[2px] min-w-0">
          <h3
            className="text-[15px] font-semibold text-[var(--modal-text-primary)] truncate"
            title={rule.title}
          >
            {rule.title}
          </h3>
          <p
            className="text-[12px] text-[var(--modal-text-tertiary)] truncate font-mono"
            title={rule.id}
          >
            {rule.id}
          </p>
        </div>
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onToggle();
          }}
          className={`relative w-[42px] h-[24px] rounded-full transition-colors cursor-pointer flex-shrink-0 ${
            rule.enabled ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"
          }`}
          aria-label={rule.enabled ? "Disable rule" : "Enable rule"}
        >
          <div
            className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${
              rule.enabled ? "translate-x-[20px]" : "translate-x-[2px]"
            }`}
          />
        </button>
      </div>

      <div className="relative">
        <p className="text-[13px] text-[var(--modal-text-secondary)] leading-[18px] line-clamp-3 min-h-[54px] whitespace-pre-wrap break-words">
          {snippet || "(empty)"}
        </p>
        <div
          className="pointer-events-none absolute inset-x-0 bottom-0 h-[28px]"
          style={{
            background:
              "linear-gradient(to top, var(--modal-bg), rgba(0,0,0,0))",
          }}
        />
      </div>

      <div className="border-t border-[var(--modal-border-secondary)] -mx-[16px]" />

      <div className="flex items-center justify-between gap-[8px]">
        <div className="flex items-center gap-[8px] min-w-0 text-[12px] text-[var(--modal-text-tertiary)]">
          <span
            className="w-[20px] h-[20px] rounded-full border border-[var(--modal-border-secondary)] flex items-center justify-center flex-shrink-0"
            title={ADDED_BY_LABEL[rule.added_by]}
          >
            <AddedByIcon className="w-[11px] h-[11px]" />
          </span>
          <span className="truncate">Updated on {formatUpdated(rule.updated_on)}</span>
          {isGithub && rule.auto_sync && (
            <span
              className="flex items-center gap-[3px] text-[var(--modal-accent)] flex-shrink-0"
              title="Auto-syncs from GitHub"
            >
              <RefreshCw className="w-[10px] h-[10px]" />
              <span>Syncing</span>
            </span>
          )}
        </div>

        <div className="relative flex-shrink-0" ref={menuRef}>
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              setMenuOpen((o) => !o);
            }}
            className="w-[28px] h-[28px] rounded-[6px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
            aria-label="More options"
          >
            <MoreHorizontal className="w-[16px] h-[16px]" />
          </button>
          {menuOpen && (
            <div
              className="absolute right-0 bottom-full mb-1 w-[240px] rounded-[10px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] shadow-xl z-10 p-1"
              onClick={(e) => e.stopPropagation()}
            >
              <button
                type="button"
                disabled={!isTopLevel}
                title={
                  isTopLevel
                    ? undefined
                    : "disable it or delete the top-level bundle"
                }
                onClick={() => {
                  if (!isTopLevel) return;
                  setMenuOpen(false);
                  onDelete();
                }}
                className={`w-full flex items-center gap-2 px-2.5 py-1.5 rounded-[6px] text-left text-[13px] transition-colors ${
                  isTopLevel
                    ? "text-[var(--error)] hover:bg-[var(--error-bg)] cursor-pointer"
                    : "text-[var(--modal-text-tertiary)] cursor-not-allowed opacity-60"
                }`}
              >
                <Trash2 className="w-[14px] h-[14px]" />
                <span>Delete</span>
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function InstructionTile({
  instruction,
  onToggle,
  onView,
}: {
  instruction: Instruction;
  onToggle: () => void;
  onView: () => void;
}) {
  const snippet = instruction.content.trim();

  return (
    <div
      role="button"
      tabIndex={0}
      onClick={onView}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onView();
        }
      }}
      className="competency-tile rounded-[14px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg)] px-[16px] py-[14px] flex flex-col gap-[8px] cursor-pointer hover:border-[var(--modal-border-primary)] transition-colors"
    >
      <div className="flex items-start justify-between gap-[12px]">
        <h3
          className="text-[15px] font-semibold text-[var(--modal-text-primary)] truncate font-mono"
          title={instruction.path}
        >
          {instruction.name}
        </h3>
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onToggle();
          }}
          className={`relative w-[42px] h-[24px] rounded-full transition-colors cursor-pointer flex-shrink-0 ${
            instruction.enabled ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"
          }`}
          aria-label={instruction.enabled ? "Disable instruction" : "Enable instruction"}
        >
          <div
            className={`absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white shadow transition-transform ${
              instruction.enabled ? "translate-x-[20px]" : "translate-x-[2px]"
            }`}
          />
        </button>
      </div>

      <div className="relative">
        <p className="text-[13px] text-[var(--modal-text-secondary)] leading-[18px] line-clamp-3 min-h-[54px] whitespace-pre-wrap break-words">
          {snippet || "(empty)"}
        </p>
        <div
          className="pointer-events-none absolute inset-x-0 bottom-0 h-[28px]"
          style={{
            background:
              "linear-gradient(to top, var(--modal-bg), rgba(0,0,0,0))",
          }}
        />
      </div>

      <div className="border-t border-[var(--modal-border-secondary)] -mx-[16px]" />

      <div className="flex items-center gap-[8px] text-[12px] text-[var(--modal-text-tertiary)]">
        <span
          className="w-[20px] h-[20px] rounded-full border border-[var(--modal-border-secondary)] flex items-center justify-center flex-shrink-0"
          title="Instruction file"
        >
          <FileText className="w-[11px] h-[11px]" />
        </span>
        <span className="truncate">Updated on {formatUpdated(instruction.updated_on)}</span>
      </div>
    </div>
  );
}

export function CompetenciesModal() {
  const agentId = useCompetenciesModalStore((s) => s.agentId);
  const focusPath = useCompetenciesModalStore((s) => s.focusPath);
  const close = useCompetenciesModalStore((s) => s.close);

  const skills = useSkillsStore((s) => s.skills);
  const loading = useSkillsStore((s) => s.loading);
  const refreshing = useSkillsStore((s) => s.refreshing);
  const error = useSkillsStore((s) => s.error);
  const loadSkills = useSkillsStore((s) => s.load);
  const resetSkills = useSkillsStore((s) => s.reset);
  const refreshSkills = useSkillsStore((s) => s.refresh);
  const importSkillFile = useSkillsStore((s) => s.importFile);
  const importSkillFolder = useSkillsStore((s) => s.importFolder);
  const removeSkill = useSkillsStore((s) => s.remove);
  const setSkillEnabled = useSkillsStore((s) => s.setEnabled);
  const setAllSkillsEnabled = useSkillsStore((s) => s.setAllEnabled);
  const setSkillAutoSync = useSkillsStore((s) => s.setAutoSync);

  const rules = useRulesStore((s) => s.rules);
  const rulesLoading = useRulesStore((s) => s.loading);
  const rulesRefreshing = useRulesStore((s) => s.refreshing);
  const rulesError = useRulesStore((s) => s.error);
  const loadRules = useRulesStore((s) => s.load);
  const resetRules = useRulesStore((s) => s.reset);
  const removeRule = useRulesStore((s) => s.remove);
  const setRuleEnabled = useRulesStore((s) => s.setEnabled);
  const setAllRulesEnabled = useRulesStore((s) => s.setAllEnabled);
  const refreshRules = useRulesStore((s) => s.refresh);
  const importRuleFile = useRulesStore((s) => s.importFile);
  const importRuleFolder = useRulesStore((s) => s.importFolder);
  const importRuleLink = useRulesStore((s) => s.importLink);

  const instructions = useInstructionsStore((s) => s.instructions);
  const instructionsLoading = useInstructionsStore((s) => s.loading);
  const instructionsError = useInstructionsStore((s) => s.error);
  const loadInstructions = useInstructionsStore((s) => s.load);
  const resetInstructions = useInstructionsStore((s) => s.reset);
  const setInstructionEnabled = useInstructionsStore((s) => s.setEnabled);
  const instructionFilenames = useInstructionsStore((s) => s.filenames);
  const loadInstructionFilenames = useInstructionsStore((s) => s.loadFilenames);
  const setInstructionFilenames = useInstructionsStore((s) => s.setFilenames);

  const workflows = useWorkflowsStore((s) => s.workflows);
  const workflowsLoading = useWorkflowsStore((s) => s.loading);
  const workflowsRefreshing = useWorkflowsStore((s) => s.refreshing);
  const workflowsError = useWorkflowsStore((s) => s.error);
  const loadWorkflows = useWorkflowsStore((s) => s.loadWorkflows);
  const refreshWorkflows = useWorkflowsStore((s) => s.refreshWorkflows);
  const resetWorkflows = useWorkflowsStore((s) => s.reset);
  const setWorkflowEnabled = useWorkflowsStore((s) => s.setWorkflowEnabled);
  const setAllWorkflows = useWorkflowsStore((s) => s.setAllWorkflows);
  const importWorkflowFolder = useWorkflowsStore((s) => s.importFolder);

  const [agentProfile, setAgentProfile] = useState<AgentProfile | null>(null);
  const [workflowBulkMode, setWorkflowBulkMode] = useState<CompetencyBulkMode>("select");
  const [skillBulkMode, setSkillBulkMode] = useState<CompetencyBulkMode>("select");
  const skillBulkModeInitRef = useRef(false);
  const [ruleBulkMode, setRuleBulkMode] = useState<CompetencyBulkMode>("select");
  const ruleBulkModeInitRef = useRef(false);

  const [activeTab, setActiveTab] = useState<TabId>("skills");
  const [search, setSearch] = useState("");
  const [linkRuleImportOpen, setLinkRuleImportOpen] = useState(false);
  const [filenamesEditorOpen, setFilenamesEditorOpen] = useState(false);
  const [viewerItem, setViewerItem] = useState<{ title: string; content: string } | null>(null);
  const [warningDismissed, setWarningDismissed] = useState(false);

  // Plugin data: fetched once per modal open via the Tauri bridge.
  const [plugins, setPlugins] = useState<PluginEntry[]>([]);
  const [globalSkills, setGlobalSkills] = useState<PluginSkill[]>([]);
  const [pluginsError, setPluginsError] = useState<string | null>(null);
  const [pluginBusy, setPluginBusy] = useState<Record<string, boolean>>({});

  // Launchpad convention-folder skills.
  // Global is agent-scoped and stable; project is scoped to the opening thread's
  // focus path, snapshotted into the store at modal-open time.
  const [launchpadGlobalSkills, setLaunchpadGlobalSkills] = useState<LaunchpadSkillEntry[]>([]);
  const [launchpadGlobalLoading, setLaunchpadGlobalLoading] = useState(false);
  const [launchpadGlobalError, setLaunchpadGlobalError] = useState<string | null>(null);
  const [launchpadProjectSkills, setLaunchpadProjectSkills] = useState<LaunchpadSkillEntry[]>([]);
  const [launchpadProjectKey, setLaunchpadProjectKey] = useState("");
  const [launchpadProjectLoading, setLaunchpadProjectLoading] = useState(false);
  const [launchpadProjectError, setLaunchpadProjectError] = useState<string | null>(null);
  const [launchpadBusy, setLaunchpadBusy] = useState<Record<string, boolean>>({});
  const [promoteBusyMap, setPromoteBusyMap] = useState<Record<string, boolean>>({});
  const [promoteMessages, setPromoteMessages] = useState<Record<string, string>>({});

  useEffect(() => {
    if (!agentId) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    document.addEventListener("keydown", handler);
    return () => document.removeEventListener("keydown", handler);
  }, [agentId, close]);

  useEffect(() => {
    if (agentId) {
      setActiveTab("skills");
      setSearch("");
      setViewerItem(null);
      setFilenamesEditorOpen(false);
      setWarningDismissed(false);
      setSkillBulkMode("select");
      skillBulkModeInitRef.current = false;
      setRuleBulkMode("select");
      ruleBulkModeInitRef.current = false;
      setPluginBusy({});
      setLaunchpadBusy({});
      setPromoteBusyMap({});
      setPromoteMessages({});
      void loadSkills(agentId);
      if (!RULES_INSTRUCTIONS_HIDDEN) {
        void loadRules(agentId);
        void loadInstructions(agentId);
        void loadInstructionFilenames();
      }
      void loadWorkflows();
    } else {
      resetSkills();
      resetRules();
      resetInstructions();
      resetWorkflows();
      setAgentProfile(null);
      setWorkflowBulkMode("select");
      setSkillBulkMode("select");
      skillBulkModeInitRef.current = false;
      setRuleBulkMode("select");
      ruleBulkModeInitRef.current = false;
      setViewerItem(null);
      setFilenamesEditorOpen(false);
      setWarningDismissed(false);
      setPlugins([]);
      setGlobalSkills([]);
      setPluginsError(null);
      setPluginBusy({});
      setLaunchpadGlobalSkills([]);
      setLaunchpadGlobalLoading(false);
      setLaunchpadGlobalError(null);
      setLaunchpadProjectSkills([]);
      setLaunchpadProjectKey("");
      setLaunchpadProjectLoading(false);
      setLaunchpadProjectError(null);
      setLaunchpadBusy({});
      setPromoteBusyMap({});
      setPromoteMessages({});
    }
  }, [
    agentId,
    loadSkills,
    resetSkills,
    loadRules,
    resetRules,
    loadInstructions,
    resetInstructions,
    loadInstructionFilenames,
    loadWorkflows,
    resetWorkflows,
  ]);

  useEffect(() => {
    if (!agentId) return;
    let cancelled = false;
    setPluginsError(null);
    Promise.all([
      invoke<PluginEntry[]>("list_plugins"),
      invoke<PluginSkill[]>("list_global_skills"),
    ]).then(
      ([entries, skills]) => {
        if (cancelled) return;
        setPlugins(entries);
        setGlobalSkills(skills);
      },
      (err: unknown) => {
        if (cancelled) return;
        setPlugins([]);
        setGlobalSkills([]);
        setPluginsError(pluginErrorMessage(err));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  // Launchpad global skills: fetched once per modal open, mirroring the
  // list_global_skills plugin fetch above.
  useEffect(() => {
    if (!agentId) return;
    let cancelled = false;
    setLaunchpadGlobalLoading(true);
    setLaunchpadGlobalError(null);
    listLaunchpadGlobalSkills().then(
      (skills) => {
        if (cancelled) return;
        setLaunchpadGlobalSkills(skills);
        setLaunchpadGlobalLoading(false);
      },
      (err: unknown) => {
        if (cancelled) return;
        setLaunchpadGlobalSkills([]);
        setLaunchpadGlobalError(err instanceof Error ? err.message : String(err));
        setLaunchpadGlobalLoading(false);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  // Launchpad project skills: scoped to the opening thread's focus path
  // (snapshotted into the competencies-modal store at open time). No focus
  // path means nothing to scan — the section renders an empty-state hint.
  useEffect(() => {
    if (!agentId) return;
    if (!focusPath) {
      setLaunchpadProjectSkills([]);
      setLaunchpadProjectKey("");
      setLaunchpadProjectError(null);
      setLaunchpadProjectLoading(false);
      return;
    }
    let cancelled = false;
    setLaunchpadProjectLoading(true);
    setLaunchpadProjectError(null);
    listLaunchpadProjectSkills(focusPath).then(
      (res) => {
        if (cancelled) return;
        setLaunchpadProjectSkills(res.skills);
        setLaunchpadProjectKey(res.project_key);
        setLaunchpadProjectLoading(false);
      },
      (err: unknown) => {
        if (cancelled) return;
        setLaunchpadProjectSkills([]);
        setLaunchpadProjectError(err instanceof Error ? err.message : String(err));
        setLaunchpadProjectLoading(false);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId, focusPath]);

  useEffect(() => {
    if (!agentId) return;
    let cancelled = false;
    void getAgent(agentId).then(
      (profile) => {
        if (cancelled) return;
        setAgentProfile(profile);
        if (profile.workflows === "all") {
          setWorkflowBulkMode("all");
        } else if (Array.isArray(profile.workflows) && profile.workflows.length === 0) {
          setWorkflowBulkMode("none");
        } else {
          setWorkflowBulkMode("select");
        }
      },
      () => {
        // Profile fetch failure is non-fatal; tiles render with enabled=false
        // and toggles are inert until the profile arrives.
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  const toggleSkill = (id: string, currentEnabled: boolean) => {
    void setSkillEnabled(id, !currentEnabled);
  };

  const deleteSkill = (id: string) => {
    void removeSkill(id);
  };

  const toggleAutoSync = (id: string, currentAutoSync: boolean) => {
    void setSkillAutoSync(id, !currentAutoSync);
  };

  const handleAdd = async (action: AddAction) => {
    // `create` and `link` are intentionally stubbed for v1 and deliberately
    // out of scope — do NOT wire these to real handlers without revisiting
    // that decision.
    if (action === "create" || action === "link") {
      console.log("[CompetenciesModal] add action:", action);
      return;
    }
    if (action === "file") {
      try {
        const selected = await tauriOpen({
          multiple: false,
          filters: [{ name: "Markdown", extensions: ["md"] }],
        });
        if (!selected) return;
        await importSkillFile(selected as string);
      } catch {
        // Error is already surfaced via the store's `error` banner.
      }
      return;
    }
    if (action === "folder") {
      try {
        const selected = await tauriOpen({ directory: true, multiple: false });
        if (!selected) return;
        await importSkillFolder(selected as string);
      } catch {
        // Error is already surfaced via the store's `error` banner.
      }
    }
  };

  const handleRefresh = () => {
    void refreshSkills();
  };

  const handleRuleAdd = async (action: AddAction) => {
    if (action === "create") {
      // Rules tab does not support inline creation in V1.
      return;
    }
    if (action === "link") {
      setLinkRuleImportOpen(true);
      return;
    }
    if (action === "file") {
      try {
        const selected = await tauriOpen({
          multiple: false,
          filters: [{ name: "Markdown", extensions: ["md"] }],
        });
        if (!selected) return;
        await importRuleFile(selected as string);
      } catch {
        // Error is already surfaced via the store's `error` banner.
      }
      return;
    }
    if (action === "folder") {
      try {
        const selected = await tauriOpen({ directory: true, multiple: false });
        if (!selected) return;
        await importRuleFolder(selected as string);
      } catch {
        // Error is already surfaced via the store's `error` banner.
      }
    }
  };

  const handleRulesRefresh = () => {
    void refreshRules();
  };

  const handleWorkflowAdd = async (action: AddAction) => {
    if (action === "folder") {
      try {
        const selected = await tauriOpen({ directory: true, multiple: false });
        if (!selected) return;
        await importWorkflowFolder(selected as string);
      } catch {
        // Error is already surfaced via the store's `error` banner.
      }
    }
  };

  // Exclude plugin-pool skills from the "Agent skills" section — they are
  // already shown per-plugin in the sections below via list_global_skills.
  const userPoolSkills = useMemo(
    () => skills.filter((s) => !s.source || s.source === "user"),
    [skills],
  );

  // Usage data for plugin skills from the REST API (keyed by skill_name, which
  // matches the unnamespaced id the backend stores in .usage.json).
  const pluginUsageMap = useMemo(() => {
    const map = new Map<string, { usage_count: number; last_used: string | null | undefined }>();
    for (const s of skills) {
      if (s.source === "plugin" && (s.usage_count ?? 0) > 0) {
        map.set(s.id, { usage_count: s.usage_count ?? 0, last_used: s.last_used });
      }
    }
    return map;
  }, [skills]);

  const visibleSkills = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return userPoolSkills;
    return userPoolSkills.filter(
      (s) =>
        s.title.toLowerCase().includes(q) ||
        s.description.toLowerCase().includes(q),
    );
  }, [userPoolSkills, search]);

  const visibleWorkflows = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return workflows;
    return workflows.filter(
      (w) =>
        w.name.toLowerCase().includes(q) ||
        (w.description ?? "").toLowerCase().includes(q),
    );
  }, [workflows, search]);

  useEffect(() => {
    if (skillBulkModeInitRef.current) return;
    if (userPoolSkills.length === 0) return;
    skillBulkModeInitRef.current = true;
    const allOn = userPoolSkills.every((s) => s.enabled);
    const allOff = userPoolSkills.every((s) => !s.enabled);
    setSkillBulkMode(allOn ? "all" : allOff ? "none" : "select");
  }, [userPoolSkills]);

  useEffect(() => {
    if (ruleBulkModeInitRef.current) return;
    if (rules.length === 0) return;
    ruleBulkModeInitRef.current = true;
    const allOn = rules.every((r) => r.enabled);
    const allOff = rules.every((r) => !r.enabled);
    setRuleBulkMode(allOn ? "all" : allOff ? "none" : "select");
  }, [rules]);

  const allTabs: { id: TabId; label: string }[] = [
    { id: "skills", label: "Skills" },
    { id: "workflows", label: "Workflows" },
    { id: "rules", label: "Rules" },
    { id: "instructions", label: "Instructions" },
  ];
  const tabs = RULES_INSTRUCTIONS_HIDDEN
    ? allTabs.filter((t) => t.id !== "rules" && t.id !== "instructions")
    : allTabs;

  const hasAnyRules = rules.length > 0;
  const hasAnyInstructions = instructions.length > 0;
  const hasAnyWorkflows = workflows.length > 0;
  const hasVisibleWorkflows = visibleWorkflows.length > 0;

  const handleSkillBulkModeChange = async (nextMode: CompetencyBulkMode) => {
    if (nextMode === "select") {
      setSkillBulkMode("select");
      return;
    }
    const previousMode = skillBulkMode;
    setSkillBulkMode(nextMode);
    try {
      await setAllSkillsEnabled(nextMode === "all");
    } catch {
      setSkillBulkMode(previousMode);
    }
  };

  const handleRuleBulkModeChange = async (nextMode: CompetencyBulkMode) => {
    if (nextMode === "select") {
      setRuleBulkMode("select");
      return;
    }
    const previousMode = ruleBulkMode;
    setRuleBulkMode(nextMode);
    try {
      await setAllRulesEnabled(nextMode === "all");
    } catch {
      setRuleBulkMode(previousMode);
    }
  };

  const handleWorkflowBulkModeChange = async (nextMode: CompetencyBulkMode) => {
    if (nextMode === "select") {
      setWorkflowBulkMode("select");
      return;
    }
    if (!agentProfile) return;
    const previous = agentProfile;
    const previousMode = workflowBulkMode;
    const nextWorkflows: AgentProfile["workflows"] = nextMode === "all" ? "all" : [];
    setAgentProfile({ ...previous, workflows: nextWorkflows });
    setWorkflowBulkMode(nextMode);
    try {
      const updated = await setAllWorkflows(previous.id, nextMode === "all");
      setAgentProfile(updated);
    } catch {
      setAgentProfile(previous);
      setWorkflowBulkMode(previousMode);
    }
  };

  const handleToggleWorkflow = async (workflowId: string, nextEnabled: boolean) => {
    if (!agentProfile) return;
    const previous = agentProfile;
    const catalog = workflows;
    const current = previous.workflows;
    let nextList: string[];
    if (current === "all") nextList = catalog.map((w) => w.id);
    else if (Array.isArray(current)) nextList = [...current];
    else nextList = [];
    if (nextEnabled) {
      if (!nextList.includes(workflowId)) nextList.push(workflowId);
    } else {
      nextList = nextList.filter((id) => id !== workflowId);
    }
    setAgentProfile({ ...previous, workflows: nextList });
    try {
      const updated = await setWorkflowEnabled(previous.id, workflowId, nextEnabled);
      setAgentProfile(updated);
    } catch {
      setAgentProfile(previous);
    }
  };

  // Skills grouped by plugin name (for the Plugins tab). Derived from the
  // global catalog — registry is the source of truth, so a plugin with zero
  // skills contributes an empty array here.
  const skillsByPlugin = useMemo(() => {
    const map: Record<string, PluginSkill[]> = {};
    for (const s of globalSkills) {
      (map[s.plugin_name] ??= []).push(s);
    }
    for (const name of Object.keys(map)) {
      map[name].sort((a, b) => a.skill_name.localeCompare(b.skill_name));
    }
    return map;
  }, [globalSkills]);

  const sortedPlugins = useMemo(
    () => [...plugins].sort((a, b) => a.name.localeCompare(b.name)),
    [plugins],
  );

  // Plugin skills filtered by the Skills-tab search query, grouped by plugin.
  // Used to render per-plugin sections in the Skills tab. A plugin with no
  // matching skills after filtering is omitted at render time.
  const visiblePluginSkillsByPlugin = useMemo(() => {
    const q = search.trim().toLowerCase();
    const out: Record<string, PluginSkill[]> = {};
    for (const name of Object.keys(skillsByPlugin)) {
      const list = skillsByPlugin[name];
      if (!q) {
        out[name] = list;
      } else {
        out[name] = list.filter(
          (s) =>
            s.skill_name.toLowerCase().includes(q) ||
            s.id.toLowerCase().includes(q) ||
            s.plugin_name.toLowerCase().includes(q),
        );
      }
    }
    return out;
  }, [skillsByPlugin, search]);

  const hasAnyLocalSkills = userPoolSkills.length > 0;
  const hasAnyPluginSkills = globalSkills.length > 0;
  const hasAnySkills = hasAnyLocalSkills || hasAnyPluginSkills;
  const hasVisibleLocalSkills = visibleSkills.length > 0;
  const hasVisiblePluginSkills = Object.values(visiblePluginSkillsByPlugin).some(
    (list) => list.length > 0,
  );
  const hasVisibleSkills = hasVisibleLocalSkills || hasVisiblePluginSkills;

  const getEnablement = (pluginName: string): PluginEnablement | undefined =>
    agentProfile?.enabled_plugins?.[pluginName];

  const togglePluginEnabled = async (pluginName: string, nextEnabled: boolean) => {
    if (!agentId || !agentProfile) return;
    if (pluginBusy[pluginName]) return;
    setPluginBusy((b) => ({ ...b, [pluginName]: true }));
    const previous = agentProfile;
    // Optimistic update of the local profile copy so the toggle feels instant.
    const prevEntry = previous.enabled_plugins?.[pluginName];
    const optimisticEntry: PluginEnablement = {
      enabled: nextEnabled,
      enabled_skills: prevEntry?.enabled_skills ?? null,
    };
    setAgentProfile({
      ...previous,
      enabled_plugins: {
        ...(previous.enabled_plugins ?? {}),
        [pluginName]: optimisticEntry,
      },
    });
    try {
      const updated = await invoke<AgentProfile>("set_agent_plugin_enabled", {
        agentId,
        pluginName,
        enabled: nextEnabled,
      });
      setAgentProfile(updated);
    } catch (err) {
      setAgentProfile(previous);
      setPluginsError(pluginErrorMessage(err));
    } finally {
      setPluginBusy((b) => {
        const next = { ...b };
        delete next[pluginName];
        return next;
      });
    }
  };

  const toggleSkillInSubset = async (
    pluginName: string,
    skillName: string,
    nextChecked: boolean,
  ) => {
    if (!agentId || !agentProfile) return;
    const busyKey = `${pluginName}:${skillName}`;
    if (pluginBusy[busyKey]) return;
    setPluginBusy((b) => ({ ...b, [busyKey]: true }));
    const previous = agentProfile;
    const current = previous.enabled_plugins?.[pluginName];
    const allSkillsForPlugin = (skillsByPlugin[pluginName] ?? []).map(
      (s) => s.skill_name,
    );
    // Seed the subset from current state. `null`/undefined means "all skills";
    // the user's first checkbox interaction materializes the current all-on
    // list so we can flip a single member without losing the rest.
    const baseline =
      current?.enabled_skills === null || current?.enabled_skills === undefined
        ? [...allSkillsForPlugin]
        : [...current.enabled_skills];
    let nextSubset: string[];
    if (nextChecked) {
      nextSubset = baseline.includes(skillName) ? baseline : [...baseline, skillName];
    } else {
      nextSubset = baseline.filter((s) => s !== skillName);
    }
    // If the next subset matches the full catalog, collapse back to `null`
    // ("all enabled") so the profile stays compact on disk.
    const isAll =
      nextSubset.length === allSkillsForPlugin.length &&
      allSkillsForPlugin.every((s) => nextSubset.includes(s));
    const payload: string[] | null = isAll ? null : nextSubset;
    setAgentProfile({
      ...previous,
      enabled_plugins: {
        ...(previous.enabled_plugins ?? {}),
        [pluginName]: {
          enabled: current?.enabled ?? true,
          enabled_skills: payload,
        },
      },
    });
    try {
      const updated = await invoke<AgentProfile>("set_agent_skill_subset", {
        agentId,
        pluginName,
        subset: payload,
      });
      setAgentProfile(updated);
    } catch (err) {
      setAgentProfile(previous);
      setPluginsError(pluginErrorMessage(err));
    } finally {
      setPluginBusy((b) => {
        const next = { ...b };
        delete next[busyKey];
        return next;
      });
    }
  };

  const isSkillInSubset = (enablement: PluginEnablement | undefined, skillName: string): boolean => {
    if (!enablement || !enablement.enabled) return false;
    const subset = enablement.enabled_skills;
    if (subset === null || subset === undefined) return true;
    return subset.includes(skillName);
  };

  // Launchpad convention-folder skills — name sets drive the shadow badge
  // (a name present in both pools means the project copy shadows the global one).
  const launchpadGlobalNames = useMemo(
    () => new Set(launchpadGlobalSkills.map((s) => s.name)),
    [launchpadGlobalSkills],
  );
  const launchpadProjectNames = useMemo(
    () => new Set(launchpadProjectSkills.map((s) => s.name)),
    [launchpadProjectSkills],
  );
  const enabledLaunchpadGlobalSkills = useMemo(
    () => new Set(agentProfile?.enabled_launchpad_global_skills ?? []),
    [agentProfile],
  );
  const enabledLaunchpadProjectSkills = useMemo(
    () =>
      new Set(
        launchpadProjectKey
          ? agentProfile?.enabled_launchpad_project_skills?.[launchpadProjectKey] ?? []
          : [],
      ),
    [agentProfile, launchpadProjectKey],
  );
  // Basename of the focus path, for the "Project skills — <project name>" header.
  const launchpadProjectName = useMemo(() => {
    if (!focusPath) return null;
    const trimmed = focusPath.replace(/[/\\]+$/, "");
    const parts = trimmed.split(/[/\\]/);
    return parts[parts.length - 1] || focusPath;
  }, [focusPath]);

  const toggleLaunchpadGlobalSkill = async (skillName: string, nextEnabled: boolean) => {
    if (!agentId || !agentProfile) return;
    const busyKey = `global:${skillName}`;
    if (launchpadBusy[busyKey]) return;
    setLaunchpadBusy((b) => ({ ...b, [busyKey]: true }));
    const previous = agentProfile;
    const current = previous.enabled_launchpad_global_skills ?? [];
    const next = nextEnabled
      ? current.includes(skillName)
        ? current
        : [...current, skillName]
      : current.filter((s) => s !== skillName);
    setAgentProfile({ ...previous, enabled_launchpad_global_skills: next });
    try {
      await setLaunchpadGlobalSkillEnabled(agentId, skillName, nextEnabled);
    } catch (err) {
      setAgentProfile(previous);
      setLaunchpadGlobalError(err instanceof Error ? err.message : String(err));
    } finally {
      setLaunchpadBusy((b) => {
        const next = { ...b };
        delete next[busyKey];
        return next;
      });
    }
  };

  const toggleLaunchpadProjectSkill = async (skillName: string, nextEnabled: boolean) => {
    if (!agentId || !agentProfile || !launchpadProjectKey) return;
    const busyKey = `project:${skillName}`;
    if (launchpadBusy[busyKey]) return;
    setLaunchpadBusy((b) => ({ ...b, [busyKey]: true }));
    const previous = agentProfile;
    const currentMap = previous.enabled_launchpad_project_skills ?? {};
    const current = currentMap[launchpadProjectKey] ?? [];
    const next = nextEnabled
      ? current.includes(skillName)
        ? current
        : [...current, skillName]
      : current.filter((s) => s !== skillName);
    setAgentProfile({
      ...previous,
      enabled_launchpad_project_skills: { ...currentMap, [launchpadProjectKey]: next },
    });
    try {
      await setLaunchpadProjectSkillEnabled(agentId, launchpadProjectKey, skillName, nextEnabled);
    } catch (err) {
      setAgentProfile(previous);
      setLaunchpadProjectError(err instanceof Error ? err.message : String(err));
    } finally {
      setLaunchpadBusy((b) => {
        const next = { ...b };
        delete next[busyKey];
        return next;
      });
    }
  };

  const promoteProjectSkill = async (skillName: string) => {
    if (!focusPath) return;
    if (promoteBusyMap[skillName]) return;
    setPromoteBusyMap((b) => ({ ...b, [skillName]: true }));
    setPromoteMessages((m) => {
      const next = { ...m };
      delete next[skillName];
      return next;
    });
    try {
      await promoteLaunchpadSkill(focusPath, skillName);
      const refreshed = await listLaunchpadGlobalSkills();
      setLaunchpadGlobalSkills(refreshed);
    } catch (err) {
      const message =
        err instanceof ApiError && err.status === 409
          ? "Already exists globally"
          : err instanceof Error
            ? err.message
            : String(err);
      setPromoteMessages((m) => ({ ...m, [skillName]: message }));
    } finally {
      setPromoteBusyMap((b) => {
        const next = { ...b };
        delete next[skillName];
        return next;
      });
    }
  };

  const sortedRules = useMemo(
    () => [...rules].sort((a, b) => a.id.localeCompare(b.id)),
    [rules],
  );

  const visibleRules = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return sortedRules;
    return sortedRules.filter(
      (r) =>
        r.title.toLowerCase().includes(q) ||
        r.id.toLowerCase().includes(q) ||
        (r.description ?? "").toLowerCase().includes(q) ||
        r.content.toLowerCase().includes(q),
    );
  }, [sortedRules, search]);

  const sortedInstructions = useMemo(
    () =>
      [...instructions].sort((a, b) =>
        a.name.toLowerCase().localeCompare(b.name.toLowerCase()),
      ),
    [instructions],
  );

  const visibleInstructions = useMemo(() => {
    const q = search.trim().toLowerCase();
    if (!q) return sortedInstructions;
    return sortedInstructions.filter(
      (ins) =>
        ins.name.toLowerCase().includes(q) ||
        ins.path.toLowerCase().includes(q) ||
        ins.content.toLowerCase().includes(q),
    );
  }, [sortedInstructions, search]);

  const hasVisibleRules = visibleRules.length > 0;
  const hasVisibleInstructions = visibleInstructions.length > 0;

  const activeRulesInstructionsCount = useMemo(() => {
    const activeRules = rules.filter((r) => r.enabled).length;
    const activeInstructions = instructions.filter((i) => i.enabled).length;
    return activeRules + activeInstructions;
  }, [rules, instructions]);

  const showWarningBanner =
    activeRulesInstructionsCount > RULES_INSTRUCTIONS_WARN_THRESHOLD &&
    !warningDismissed;

  const openRuleViewer = (rule: Rule) =>
    setViewerItem({ title: rule.id, content: rule.content });
  const openInstructionViewer = (ins: Instruction) =>
    setViewerItem({ title: ins.name, content: ins.content });

  const persistInstructionFilenames = async (list: string[]) => {
    const normalized = await setInstructionFilenames(list);
    if (agentId) {
      void loadInstructions(agentId);
    }
    return normalized;
  };

  return (
    <AnimatePresence>
      {agentId && (
        <div className="fixed inset-0 z-[300] flex items-center justify-center">
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
            className="absolute inset-0 bg-black/40"
            onClick={close}
          />

          <motion.div
            initial={{ opacity: 0, scale: 0.96 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.96 }}
            transition={{ duration: 0.15, ease: "easeOut" }}
            className="competencies-modal relative w-full max-w-[780px] h-[640px] rounded-[16px] overflow-hidden bg-[var(--modal-bg)] border border-[var(--modal-border-secondary)] flex flex-col"
            style={{ boxShadow: "0 0 0 1px rgba(0,0,0,0.13), 0 18px 48px 0 rgba(0,0,0,0.35)" }}
          >
            <div className="flex items-center justify-between px-[24px] pt-[18px] pb-[12px]">
              <h2 className="text-[18px] font-semibold text-[var(--modal-text-primary)]">
                Competencies
              </h2>
              <button
                onClick={close}
                className="w-[32px] h-[32px] rounded-[8px] flex items-center justify-center text-[var(--modal-text-secondary)] hover:bg-[var(--modal-bg-hover)] hover:text-[var(--modal-text-primary)] transition-colors cursor-pointer"
                title="Close"
              >
                <X className="w-[18px] h-[18px]" />
              </button>
            </div>

            {showWarningBanner && (
              <div
                role="status"
                className="mx-[24px] mb-[10px] rounded-[10px] border border-amber-500/40 bg-amber-500/10 px-[12px] py-[8px] text-[13px] text-amber-600 dark:text-amber-300 flex items-start gap-[8px]"
              >
                <AlertTriangle
                  className="w-[14px] h-[14px] flex-shrink-0 mt-[2px]"
                  strokeWidth={2}
                />
                <span className="flex-1 leading-[18px]">
                  You have {activeRulesInstructionsCount} rules and instructions
                  loaded. Large context may impact performance.
                </span>
                <button
                  type="button"
                  onClick={() => setWarningDismissed(true)}
                  title="Dismiss"
                  aria-label="Dismiss warning"
                  className="flex-shrink-0 w-[20px] h-[20px] rounded-[4px] flex items-center justify-center hover:bg-amber-500/15 transition-colors cursor-pointer"
                >
                  <X className="w-[12px] h-[12px]" />
                </button>
              </div>
            )}

            <div className="flex items-center gap-[4px] px-[20px] border-b border-[var(--modal-border-secondary)]">
              {tabs.map((t) => {
                const active = activeTab === t.id;
                return (
                  <button
                    key={t.id}
                    type="button"
                    onClick={() => setActiveTab(t.id)}
                    className={`relative px-[14px] py-[10px] text-[14px] font-medium transition-colors cursor-pointer ${
                      active
                        ? "text-[var(--modal-text-primary)]"
                        : "text-[var(--modal-text-secondary)] hover:text-[var(--modal-text-primary)]"
                    }`}
                  >
                    {t.label}
                    {active && (
                      <span className="absolute left-[10px] right-[10px] bottom-[-1px] h-[2px] rounded-full bg-[var(--modal-accent)]" />
                    )}
                  </button>
                );
              })}
            </div>

            <div className="flex flex-col flex-1 min-h-0 bg-[var(--modal-bg-primary)]">
              {activeTab === "skills" && (
                <>
                  <div className="flex items-center gap-[10px] px-[24px] pt-[16px] pb-[14px]">
                    <CompetencyBulkControl
                      mode={skillBulkMode}
                      onModeChange={(m) => void handleSkillBulkModeChange(m)}
                    />
                    <div className="relative flex-1 max-w-[320px]">
                      <Search className="absolute left-[10px] top-1/2 -translate-y-1/2 w-[14px] h-[14px] text-[var(--modal-text-tertiary)] pointer-events-none" />
                      <input
                        type="text"
                        value={search}
                        onChange={(e) => setSearch(e.target.value)}
                        placeholder="Search Skill"
                        className="w-full h-[34px] pl-[32px] pr-[12px] rounded-[10px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors"
                      />
                    </div>
                    <div className="flex-1" />
                    <button
                      type="button"
                      onClick={handleRefresh}
                      disabled={refreshing}
                      title="Refresh skills"
                      aria-label="Refresh skills"
                      className="h-[34px] w-[34px] rounded-[10px] flex items-center justify-center text-[var(--modal-text-primary)] bg-transparent hover:bg-[var(--modal-bg-tertiary)] transition-colors cursor-pointer disabled:cursor-default flex-shrink-0"
                    >
                      <RefreshCw
                        className={`w-[14px] h-[14px] ${refreshing ? "animate-spin" : ""}`}
                      />
                    </button>
                    <AddMenu onAction={handleAdd} />
                  </div>

                  {error && (
                    <div className="mx-[24px] mb-[10px] rounded-[10px] border border-[var(--error-border)] bg-[var(--error-bg)] px-[12px] py-[8px] text-[13px] text-[var(--error)]">
                      {error}
                    </div>
                  )}

                  {pluginsError && (
                    <div className="mx-[24px] mb-[10px] rounded-[10px] border border-[var(--error-border)] bg-[var(--error-bg)] px-[12px] py-[8px] text-[13px] text-[var(--error)] flex items-start gap-[8px]">
                      <AlertTriangle className="w-[14px] h-[14px] flex-shrink-0 mt-[2px]" />
                      <span className="flex-1 leading-[18px]">{pluginsError}</span>
                      <button
                        type="button"
                        onClick={() => setPluginsError(null)}
                        aria-label="Dismiss error"
                        className="flex-shrink-0 w-[20px] h-[20px] rounded-[4px] flex items-center justify-center hover:bg-[var(--error-bg)] transition-colors cursor-pointer"
                      >
                        <X className="w-[12px] h-[12px]" />
                      </button>
                    </div>
                  )}

                  <div className="flex-1 overflow-y-auto px-[24px] pb-[20px]">
                    <div className="flex flex-col gap-[20px]">
                    {/* Project skills — convention-folder skills scoped to the active thread's focus path. */}
                    <div className="flex flex-col gap-[10px]">
                      <div className="flex items-center gap-[8px] px-[2px]">
                        <FolderPlus className="w-[13px] h-[13px] text-[var(--modal-text-secondary)] flex-shrink-0" />
                        <h3 className="text-[13px] font-semibold text-[var(--modal-text-primary)]">
                          Project skills{launchpadProjectName ? ` — ${launchpadProjectName}` : ""}
                        </h3>
                        {focusPath && (
                          <span className="text-[12px] text-[var(--modal-text-tertiary)]">
                            · {launchpadProjectSkills.length}
                          </span>
                        )}
                      </div>
                      {launchpadProjectError && (
                        <div className="rounded-[10px] border border-[var(--error-border)] bg-[var(--error-bg)] px-[12px] py-[8px] text-[13px] text-[var(--error)]">
                          {launchpadProjectError}
                        </div>
                      )}
                      {!focusPath ? (
                        <p className="text-[13px] text-[var(--modal-text-tertiary)]">
                          Focus this thread on a project to see its skills.
                        </p>
                      ) : launchpadProjectLoading && launchpadProjectSkills.length === 0 ? (
                        <p className="text-[13px] text-[var(--modal-text-tertiary)]">Loading…</p>
                      ) : launchpadProjectSkills.length === 0 ? (
                        <p className="text-[13px] text-[var(--modal-text-tertiary)]">
                          No project skills found in this project's .launchpad/skills folder.
                        </p>
                      ) : (
                        <div className="grid grid-cols-2 gap-[14px] auto-rows-min">
                          {launchpadProjectSkills.map((skill) => (
                            <LaunchpadSkillTile
                              key={skill.name}
                              skill={skill}
                              checked={enabledLaunchpadProjectSkills.has(skill.name)}
                              busy={launchpadBusy[`project:${skill.name}`] === true}
                              overridesGlobal={launchpadGlobalNames.has(skill.name)}
                              onToggle={() =>
                                void toggleLaunchpadProjectSkill(
                                  skill.name,
                                  !enabledLaunchpadProjectSkills.has(skill.name),
                                )
                              }
                              onPromote={() => void promoteProjectSkill(skill.name)}
                              promoteBusy={promoteBusyMap[skill.name] === true}
                              promoteMessage={promoteMessages[skill.name] ?? null}
                            />
                          ))}
                        </div>
                      )}
                    </div>

                    {/* Global skills — convention-folder skills at <data_root>/.launchpad/skills, stable per agent. */}
                    <div className="flex flex-col gap-[10px]">
                      <div className="flex items-center gap-[8px] px-[2px]">
                        <FolderPlus className="w-[13px] h-[13px] text-[var(--modal-text-secondary)] flex-shrink-0" />
                        <h3 className="text-[13px] font-semibold text-[var(--modal-text-primary)]">
                          Global skills
                        </h3>
                        <span className="text-[12px] text-[var(--modal-text-tertiary)]">
                          · {launchpadGlobalSkills.length}
                        </span>
                      </div>
                      {launchpadGlobalError && (
                        <div className="rounded-[10px] border border-[var(--error-border)] bg-[var(--error-bg)] px-[12px] py-[8px] text-[13px] text-[var(--error)]">
                          {launchpadGlobalError}
                        </div>
                      )}
                      {launchpadGlobalLoading && launchpadGlobalSkills.length === 0 ? (
                        <p className="text-[13px] text-[var(--modal-text-tertiary)]">Loading…</p>
                      ) : launchpadGlobalSkills.length === 0 ? (
                        <p className="text-[13px] text-[var(--modal-text-tertiary)]">
                          No global skills yet. Drop a folder into the .launchpad/skills directory to see it here.
                        </p>
                      ) : (
                        <div className="grid grid-cols-2 gap-[14px] auto-rows-min">
                          {launchpadGlobalSkills.map((skill) => (
                            <LaunchpadSkillTile
                              key={skill.name}
                              skill={skill}
                              checked={enabledLaunchpadGlobalSkills.has(skill.name)}
                              busy={launchpadBusy[`global:${skill.name}`] === true}
                              overriddenByProject={launchpadProjectNames.has(skill.name)}
                              onToggle={() =>
                                void toggleLaunchpadGlobalSkill(
                                  skill.name,
                                  !enabledLaunchpadGlobalSkills.has(skill.name),
                                )
                              }
                            />
                          ))}
                        </div>
                      )}
                    </div>

                    {loading && !hasAnySkills ? (
                      <div className="flex items-center justify-center py-[40px]">
                        <div className="h-[24px] w-[24px] animate-spin rounded-full border-2 border-[var(--modal-border-primary)] border-t-[var(--modal-accent)]" />
                      </div>
                    ) : !hasAnySkills ? (
                      <div className="flex flex-col items-center justify-center text-center gap-[14px] py-[24px]">
                        <Puzzle className="w-[44px] h-[44px] text-[var(--modal-text-tertiary)]" strokeWidth={1.5} />
                        <div className="flex flex-col gap-[4px]">
                          <p className="text-[15px] font-medium text-[var(--modal-text-primary)]">
                            No skills available
                          </p>
                          <p className="text-[13px] text-[var(--modal-text-tertiary)]">
                            Get started by creating or importing a skill.
                          </p>
                        </div>
                        <AddMenu size="lg" align="left" onAction={handleAdd} />
                      </div>
                    ) : !hasVisibleSkills ? (
                      <div className="flex flex-col items-center justify-center text-center gap-[4px] py-[24px]">
                        <p className="text-[14px] text-[var(--modal-text-secondary)]">No matches</p>
                        <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                          Try a different search term.
                        </p>
                      </div>
                    ) : (
                      <>
                        {hasVisibleLocalSkills && (
                          <div className="flex flex-col gap-[10px]">
                            {hasAnyPluginSkills && (
                              <div className="flex items-center gap-[8px] px-[2px]">
                                <User className="w-[13px] h-[13px] text-[var(--modal-text-secondary)] flex-shrink-0" />
                                <h3 className="text-[13px] font-semibold text-[var(--modal-text-primary)]">
                                  Agent skills
                                </h3>
                                <span className="text-[12px] text-[var(--modal-text-tertiary)]">
                                  · {visibleSkills.length}
                                </span>
                              </div>
                            )}
                            <div className="grid grid-cols-2 gap-[14px] auto-rows-min">
                              {visibleSkills.map((s) => (
                                <SkillTile
                                  key={s.id}
                                  skill={s}
                                  onToggle={() => toggleSkill(s.id, s.enabled)}
                                  onDelete={() => deleteSkill(s.id)}
                                  onToggleAutoSync={() => toggleAutoSync(s.id, s.auto_sync)}
                                />
                              ))}
                            </div>
                          </div>
                        )}

                        {sortedPlugins.map((plugin) => {
                          const pluginSkills = visiblePluginSkillsByPlugin[plugin.name] ?? [];
                          if (pluginSkills.length === 0) return null;
                          const enablement = getEnablement(plugin.name);
                          const pluginEnabled = enablement?.enabled === true;
                          const toggleBusy = pluginBusy[plugin.name] === true;
                          return (
                            <div key={plugin.name} className="flex flex-col gap-[10px]">
                              <div className="flex items-center gap-[10px] px-[2px]">
                                <Package className="w-[13px] h-[13px] text-[var(--modal-text-secondary)] flex-shrink-0" />
                                <div className="flex items-baseline gap-[8px] min-w-0 flex-1">
                                  <h3 className="text-[13px] font-semibold text-[var(--modal-text-primary)] truncate">
                                    {plugin.name}
                                  </h3>
                                  <span className="text-[12px] text-[var(--modal-text-tertiary)] flex-shrink-0">
                                    v{plugin.version} · {pluginSkills.length}
                                    {pluginSkills.length === 1 ? " skill" : " skills"}
                                  </span>
                                </div>
                                <button
                                  type="button"
                                  onClick={() =>
                                    void togglePluginEnabled(plugin.name, !pluginEnabled)
                                  }
                                  disabled={toggleBusy || !agentProfile}
                                  role="switch"
                                  aria-checked={pluginEnabled}
                                  aria-label={
                                    pluginEnabled
                                      ? `Disable ${plugin.name}`
                                      : `Enable ${plugin.name}`
                                  }
                                  title={
                                    pluginEnabled
                                      ? `Disable all skills from ${plugin.name}`
                                      : `Enable all skills from ${plugin.name}`
                                  }
                                  className={`relative w-[36px] h-[20px] rounded-full transition-colors flex-shrink-0 ${
                                    pluginEnabled ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-primary)]"
                                  } ${toggleBusy || !agentProfile ? "opacity-60 cursor-default" : "cursor-pointer"}`}
                                >
                                  <div
                                    className={`absolute top-[2px] w-[16px] h-[16px] rounded-full bg-white shadow transition-transform ${
                                      pluginEnabled ? "translate-x-[18px]" : "translate-x-[2px]"
                                    }`}
                                  />
                                </button>
                              </div>
                              <div className="grid grid-cols-2 gap-[14px] auto-rows-min">
                                {pluginSkills.map((ps) => {
                                  const checked = isSkillInSubset(enablement, ps.skill_name);
                                  const busyKey = `${plugin.name}:${ps.skill_name}`;
                                  const skillBusy = pluginBusy[busyKey] === true;
                                  const pluginUsage = pluginUsageMap.get(ps.skill_name);
                                  return (
                                    <PluginSkillTile
                                      key={ps.id}
                                      skill={ps}
                                      checked={checked}
                                      disabled={!pluginEnabled}
                                      busy={skillBusy}
                                      usageCount={pluginUsage?.usage_count}
                                      lastUsed={pluginUsage?.last_used}
                                      onToggle={() =>
                                        void toggleSkillInSubset(
                                          plugin.name,
                                          ps.skill_name,
                                          !checked,
                                        )
                                      }
                                    />
                                  );
                                })}
                              </div>
                            </div>
                          );
                        })}
                      </>
                    )}
                    </div>
                  </div>
                </>
              )}

              {activeTab === "rules" && (
                <>
                  <div className="flex items-center gap-[10px] px-[24px] pt-[16px] pb-[14px]">
                    <CompetencyBulkControl
                      mode={ruleBulkMode}
                      onModeChange={(m) => void handleRuleBulkModeChange(m)}
                    />
                    <div className="relative flex-1 max-w-[320px]">
                      <Search className="absolute left-[10px] top-1/2 -translate-y-1/2 w-[14px] h-[14px] text-[var(--modal-text-tertiary)] pointer-events-none" />
                      <input
                        type="text"
                        value={search}
                        onChange={(e) => setSearch(e.target.value)}
                        placeholder="Search Rule"
                        className="w-full h-[34px] pl-[32px] pr-[12px] rounded-[10px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors"
                      />
                    </div>
                    <div className="flex-1" />
                    <button
                      type="button"
                      onClick={handleRulesRefresh}
                      disabled={rulesRefreshing}
                      title="Refresh rules"
                      aria-label="Refresh rules"
                      className="h-[34px] w-[34px] rounded-[10px] flex items-center justify-center text-[var(--modal-text-primary)] bg-transparent hover:bg-[var(--modal-bg-tertiary)] transition-colors cursor-pointer disabled:cursor-default flex-shrink-0"
                    >
                      <RefreshCw
                        className={`w-[14px] h-[14px] ${rulesRefreshing ? "animate-spin" : ""}`}
                      />
                    </button>
                    <AddMenu items={RULE_ADD_ITEMS} onAction={handleRuleAdd} />
                  </div>

                  {rulesError && (
                    <div className="mx-[24px] mb-[10px] rounded-[10px] border border-[var(--error-border)] bg-[var(--error-bg)] px-[12px] py-[8px] text-[13px] text-[var(--error)]">
                      {rulesError}
                    </div>
                  )}

                  <div className="flex-1 overflow-y-auto px-[24px] pb-[20px]">
                    {rulesLoading && !hasAnyRules ? (
                      <div className="flex items-center justify-center h-full">
                        <div className="h-[24px] w-[24px] animate-spin rounded-full border-2 border-[var(--modal-border-primary)] border-t-[var(--modal-accent)]" />
                      </div>
                    ) : !hasAnyRules ? (
                      <div className="flex flex-col items-center justify-center h-full text-center gap-[14px]">
                        <ScrollText
                          className="w-[44px] h-[44px] text-[var(--modal-text-tertiary)]"
                          strokeWidth={1.5}
                        />
                        <div className="flex flex-col gap-[4px]">
                          <p className="text-[15px] font-medium text-[var(--modal-text-primary)]">
                            No rules available
                          </p>
                          <p className="text-[13px] text-[var(--modal-text-tertiary)]">
                            Get started by importing a rule.
                          </p>
                        </div>
                        <AddMenu
                          size="lg"
                          align="left"
                          items={RULE_ADD_ITEMS}
                          onAction={handleRuleAdd}
                        />
                      </div>
                    ) : !hasVisibleRules ? (
                      <div className="flex flex-col items-center justify-center h-full text-center gap-[4px]">
                        <p className="text-[14px] text-[var(--modal-text-secondary)]">No matches</p>
                        <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                          Try a different search term.
                        </p>
                      </div>
                    ) : (
                      <div className="grid grid-cols-2 gap-[14px] auto-rows-min">
                        {visibleRules.map((r) => (
                          <RuleTile
                            key={r.id}
                            rule={r}
                            onToggle={() => void setRuleEnabled(r.id, !r.enabled)}
                            onView={() => openRuleViewer(r)}
                            onDelete={() => void removeRule(r.id)}
                          />
                        ))}
                      </div>
                    )}
                  </div>
                </>
              )}

              {activeTab === "workflows" && (
                <>
                  <div className="flex items-center gap-[10px] px-[24px] pt-[16px] pb-[14px]">
                    <CompetencyBulkControl
                      mode={workflowBulkMode}
                      onModeChange={(m) => void handleWorkflowBulkModeChange(m)}
                    />
                    <div className="relative flex-1 max-w-[320px]">
                      <Search className="absolute left-[10px] top-1/2 -translate-y-1/2 w-[14px] h-[14px] text-[var(--modal-text-tertiary)] pointer-events-none" />
                      <input
                        type="text"
                        value={search}
                        onChange={(e) => setSearch(e.target.value)}
                        placeholder="Search Workflow"
                        className="w-full h-[34px] pl-[32px] pr-[12px] rounded-[10px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors"
                      />
                    </div>
                    <div className="flex-1" />
                    <button
                      type="button"
                      onClick={() => void refreshWorkflows()}
                      disabled={workflowsRefreshing}
                      title="Refresh workflows"
                      aria-label="Refresh workflows"
                      className="h-[34px] w-[34px] rounded-[10px] flex items-center justify-center text-[var(--modal-text-primary)] bg-transparent hover:bg-[var(--modal-bg-tertiary)] transition-colors cursor-pointer disabled:cursor-default flex-shrink-0"
                    >
                      <RefreshCw
                        className={`w-[14px] h-[14px] ${workflowsRefreshing ? "animate-spin" : ""}`}
                      />
                    </button>
                    <AddMenu
                      items={WORKFLOW_ADD_ITEMS}
                      onAction={(a) => void handleWorkflowAdd(a)}
                    />
                  </div>

                  {workflowsError && (
                    <div className="mx-[24px] mb-[10px] rounded-[10px] border border-[var(--error-border)] bg-[var(--error-bg)] px-[12px] py-[8px] text-[13px] text-[var(--error)]">
                      {workflowsError}
                    </div>
                  )}

                  <div className="flex-1 overflow-y-auto px-[24px] pb-[20px]">
                    {workflowsLoading && !hasAnyWorkflows ? (
                      <div className="flex items-center justify-center h-full">
                        <div className="h-[24px] w-[24px] animate-spin rounded-full border-2 border-[var(--modal-border-primary)] border-t-[var(--modal-accent)]" />
                      </div>
                    ) : !hasAnyWorkflows ? (
                      <div className="flex flex-col items-center justify-center h-full text-center gap-[14px]">
                        <GitBranch
                          className="w-[44px] h-[44px] text-[var(--modal-text-tertiary)]"
                          strokeWidth={1.5}
                        />
                        <div className="flex flex-col gap-[4px]">
                          <p className="text-[15px] font-medium text-[var(--modal-text-primary)]">
                            No workflows available
                          </p>
                          <p className="text-[13px] text-[var(--modal-text-tertiary)]">
                            Install or create a workflow to get started.
                          </p>
                        </div>
                        <AddMenu
                          size="lg"
                          align="left"
                          items={WORKFLOW_ADD_ITEMS}
                          onAction={(a) => void handleWorkflowAdd(a)}
                        />
                      </div>
                    ) : !hasVisibleWorkflows ? (
                      <div className="flex flex-col items-center justify-center h-full text-center gap-[4px]">
                        <p className="text-[14px] text-[var(--modal-text-secondary)]">No matches</p>
                        <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                          Try a different search term.
                        </p>
                      </div>
                    ) : (
                      <div className="grid grid-cols-2 gap-[14px] auto-rows-min">
                        {visibleWorkflows.map((wf) => (
                          <WorkflowTile
                            key={wf.id}
                            workflow={wf}
                            enabled={
                              agentProfile
                                ? isWorkflowEnabled(agentProfile.workflows, wf.id)
                                : false
                            }
                            selectMode={workflowBulkMode === "select"}
                            onToggle={(next) =>
                              void handleToggleWorkflow(wf.id, next)
                            }
                          />
                        ))}
                      </div>
                    )}
                  </div>
                </>
              )}

              {activeTab === "instructions" && (
                <>
                  <div className="flex items-center gap-[10px] px-[24px] pt-[16px] pb-[14px]">
                    <div className="relative flex-1 max-w-[320px]">
                      <Search className="absolute left-[10px] top-1/2 -translate-y-1/2 w-[14px] h-[14px] text-[var(--modal-text-tertiary)] pointer-events-none" />
                      <input
                        type="text"
                        value={search}
                        onChange={(e) => setSearch(e.target.value)}
                        placeholder="Search Instruction"
                        className="w-full h-[34px] pl-[32px] pr-[12px] rounded-[10px] border border-[var(--modal-border-primary)] bg-[var(--modal-bg)] text-[13px] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-tertiary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors"
                      />
                    </div>
                    <div className="flex-1" />
                    <button
                      type="button"
                      onClick={() => setFilenamesEditorOpen(true)}
                      title="Edit filename patterns"
                      aria-label="Edit filename patterns"
                      className="h-[34px] w-[34px] rounded-[10px] flex items-center justify-center text-[var(--modal-text-primary)] bg-transparent hover:bg-[var(--modal-bg-tertiary)] transition-colors cursor-pointer flex-shrink-0"
                    >
                      <Settings className="w-[14px] h-[14px]" />
                    </button>
                  </div>

                  {instructionsError && (
                    <div className="mx-[24px] mb-[10px] rounded-[10px] border border-[var(--error-border)] bg-[var(--error-bg)] px-[12px] py-[8px] text-[13px] text-[var(--error)]">
                      {instructionsError}
                    </div>
                  )}

                  <div className="flex-1 overflow-y-auto px-[24px] pb-[20px]">
                    {instructionsLoading && !hasAnyInstructions ? (
                      <div className="flex items-center justify-center h-full">
                        <div className="h-[24px] w-[24px] animate-spin rounded-full border-2 border-[var(--modal-border-primary)] border-t-[var(--modal-accent)]" />
                      </div>
                    ) : !hasAnyInstructions ? (
                      <div className="flex flex-col items-center justify-center h-full text-center gap-[14px]">
                        <FileText
                          className="w-[44px] h-[44px] text-[var(--modal-text-tertiary)]"
                          strokeWidth={1.5}
                        />
                        <div className="flex flex-col gap-[4px]">
                          <p className="text-[15px] font-medium text-[var(--modal-text-primary)]">
                            No instruction files found
                          </p>
                          <p className="text-[13px] text-[var(--modal-text-tertiary)]">
                            Drop a CLAUDE.md into your agent home or edit the filename patterns.
                          </p>
                        </div>
                      </div>
                    ) : !hasVisibleInstructions ? (
                      <div className="flex flex-col items-center justify-center h-full text-center gap-[4px]">
                        <p className="text-[14px] text-[var(--modal-text-secondary)]">No matches</p>
                        <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                          Try a different search term.
                        </p>
                      </div>
                    ) : (
                      <div className="grid grid-cols-2 gap-[14px] auto-rows-min">
                        {visibleInstructions.map((ins) => (
                          <InstructionTile
                            key={ins.id}
                            instruction={ins}
                            onToggle={() =>
                              void setInstructionEnabled(ins.id, !ins.enabled)
                            }
                            onView={() => openInstructionViewer(ins)}
                          />
                        ))}
                      </div>
                    )}
                  </div>
                </>
              )}

            </div>
          </motion.div>

          <LinkImportModal
            open={linkRuleImportOpen}
            onClose={() => setLinkRuleImportOpen(false)}
            title="Import rule from link"
            helperText="Paste the URL of a single .md file."
            onSubmit={(url) => importRuleLink(url)}
          />

          <InstructionFilenamesEditor
            open={filenamesEditorOpen}
            onClose={() => setFilenamesEditorOpen(false)}
            filenames={instructionFilenames}
            onPersist={persistInstructionFilenames}
          />

          <MarkdownViewerModal
            open={viewerItem !== null}
            onClose={() => setViewerItem(null)}
            title={viewerItem?.title ?? ""}
            content={viewerItem?.content ?? ""}
          />
        </div>
      )}
    </AnimatePresence>
  );
}
