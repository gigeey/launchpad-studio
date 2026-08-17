import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import { ArrowUp, ChevronDown, Folder, FolderOpen, Loader2, Paperclip, X } from "lucide-react";
import { open as tauriOpen } from "@tauri-apps/plugin-dialog";
import Picker from "@emoji-mart/react";
import data from "@emoji-mart/data";
import { useChatStore } from "../stores/chatStore";
import { agentAvatarColor } from "../lib/agentColors";
import { useIsDark } from "../stores/userPreferencesStore";
import { useProjectStore } from "../stores/projectStore";
import { useNavigationStore } from "../stores/navigationStore";
import * as api from "../lib/api";
import { CoordinatorBadge } from "../components/profile/CoordinatorBadge";

/** `/projects` index route. Projects don't share the generic `ViewRedirect`
 *  fallback (its "select X" empty state doesn't apply here — the intended
 *  landing when nothing is selected IS the "start a new project" composer
 *  below) but they DO need the same "remember what was opened" behavior as
 *  every other sidebar: if a project was previously opened this session
 *  (recorded in navigationStore by ProjectsSidebar's row click), redirect
 *  straight back to it instead of dropping the user on a blank form.
 */
export function ProjectsIndex() {
  const lastProjectId = useNavigationStore(
    (s) => s.selectedSubMenuByView["projects"] ?? null
  );
  if (lastProjectId) {
    return <Navigate to={`/projects/${lastProjectId}`} replace />;
  }
  return <ProjectsView />;
}

// Project icons are randomized at the start of each new project. The pool is a
// curated set that reads as "building / shipping work" rather than the full
// unicode range, so a fresh project always lands on something on-theme.
const PROJECT_EMOJIS = [
  "\u{1F680}", "\u{2728}", "\u{1F3AF}", "\u{1F525}", "\u{1F4A1}", "\u{1F4E6}",
  "\u{1F6E0}\u{FE0F}", "\u{1F9E9}", "\u{1F331}", "\u{26A1}", "\u{1F3A8}", "\u{1F4CA}",
  "\u{1F52E}", "\u{1F9E0}", "\u{1F3D7}\u{FE0F}", "\u{1F31F}", "\u{1FA84}", "\u{1F9EA}",
  "\u{1F4D0}", "\u{1F5C2}\u{FE0F}", "\u{1F6F0}\u{FE0F}", "\u{1F9ED}", "\u{1F4AB}", "\u{1F381}",
];

function randomProjectEmoji(): string {
  return PROJECT_EMOJIS[Math.floor(Math.random() * PROJECT_EMOJIS.length)];
}

export function ProjectsView() {
  const isDark = useIsDark();
  const navigate = useNavigate();

  const agents = useChatStore((s) => s.agents);
  const fetchAgents = useChatStore((s) => s.fetchAgents);
  const addProjectSnapshot = useProjectStore((s) => s.addProjectSnapshot);
  const fetchProjects = useProjectStore((s) => s.fetchProjects);

  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [agentMenuOpen, setAgentMenuOpen] = useState(false);
  const [name, setName] = useState("");
  const [goal, setGoal] = useState("");
  const [workingDir, setWorkingDir] = useState<string | null>(null);
  const [files, setFiles] = useState<string[]>([]);
  const [submitting, setSubmitting] = useState(false);
  // Each new project starts on a random icon; the user can override it via the picker.
  const [emoji, setEmoji] = useState(randomProjectEmoji);
  const [emojiPickerOpen, setEmojiPickerOpen] = useState(false);

  const agentMenuRef = useRef<HTMLDivElement>(null);
  const nameInputRef = useRef<HTMLInputElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const emojiPickerRef = useRef<HTMLDivElement>(null);
  const emojiButtonRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (agents.length === 0) fetchAgents();
  }, [agents.length, fetchAgents]);

  // Auto-grow the composer: reset to natural height, then clamp to a max so the
  // box behaves like a chat input (starts small, grows a few rows, then scrolls).
  useLayoutEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 200)}px`;
  }, [goal]);

  // Only agents that can delegate to subagents (c1+) may run projects.
  // Also exclude inline team coordinator agents (owning_team_id set).
  const selectableAgents = agents.filter(
    (a) => !a.owning_team_id && (a.coordinator_level ?? 0) >= 1,
  );
  const selectedAgent = selectableAgents.find((a) => a.agent_id === selectedAgentId) ?? null;

  // Close the agent menu on outside click.
  useEffect(() => {
    if (!agentMenuOpen) return;
    const onClick = (e: MouseEvent) => {
      if (agentMenuRef.current && !agentMenuRef.current.contains(e.target as Node)) {
        setAgentMenuOpen(false);
      }
    };
    window.addEventListener("mousedown", onClick);
    return () => window.removeEventListener("mousedown", onClick);
  }, [agentMenuOpen]);

  // Close the emoji picker on outside click (ignore clicks on its trigger).
  useEffect(() => {
    if (!emojiPickerOpen) return;
    const onClick = (e: MouseEvent) => {
      const target = e.target as Node;
      if (
        emojiPickerRef.current &&
        !emojiPickerRef.current.contains(target) &&
        emojiButtonRef.current &&
        !emojiButtonRef.current.contains(target)
      ) {
        setEmojiPickerOpen(false);
      }
    };
    window.addEventListener("mousedown", onClick);
    return () => window.removeEventListener("mousedown", onClick);
  }, [emojiPickerOpen]);

  const handleAttach = async () => {
    try {
      const selected = await tauriOpen({ multiple: true });
      if (!selected) return;
      const paths = Array.isArray(selected) ? selected : [selected];
      setFiles((prev) => [...prev, ...paths]);
    } catch {
      // Dialog cancelled or failed — no-op.
    }
  };

  const removeFile = (path: string) => setFiles((prev) => prev.filter((p) => p !== path));

  // Pick an optional working directory the agent should operate in.
  const handleSelectFolder = async () => {
    try {
      const selected = await tauriOpen({ directory: true, multiple: false });
      if (typeof selected === "string") setWorkingDir(selected);
    } catch {
      // Dialog cancelled or failed — no-op.
    }
  };

  const isValid = selectedAgentId !== null && goal.trim().length > 0;

  const handleCreate = async () => {
    if (!isValid || !selectedAgentId) return;
    setSubmitting(true);
    try {
      const project = await api.createProject({
        goal: goal.trim(),
        name: name.trim() || undefined,
        agent_id: selectedAgentId,
        emoji,
        working_dir: workingDir ?? undefined,
        attachments: files,
      });
      addProjectSnapshot({
        id: project.id,
        name: project.name,
        emoji: project.emoji,
        status: project.status,
        agent_id: project.agent_id,
        created_at: project.created_at,
        updated_at: project.updated_at,
      });
      fetchProjects(); // background reconcile
      setName("");
      setGoal("");
      setFiles([]);
      setWorkingDir(null);
      setEmoji(randomProjectEmoji());
      navigate(`/projects/${project.id}`);
    } catch (err) {
      console.error("[ProjectsView] createProject failed:", err);
      setSubmitting(false);
    }
  };

  // Enter sends (kicks off the chat); Shift+Enter inserts a newline.
  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleCreate();
    }
  };

  return (
    <div className="flex flex-1 flex-col overflow-y-auto px-8 py-10">
      <div
        className="mx-auto flex w-full max-w-[720px] flex-1 flex-col justify-center"
        style={{ transform: "translateY(-20px)" }}
      >
        {/* Prompt heading — sets the "start a conversation" framing */}
        <div className="mb-5 flex flex-col items-center text-center">
          {/* Project icon — randomized per project, click to pick a different one.
              Background is a light tint of the app's theme accent color (same
              color-mix convention as the sidebar's active-tasklist badge) rather
              than a flat neutral fill, so it reads as on-brand. */}
          <div className="relative mb-3">
            <button
              ref={emojiButtonRef}
              type="button"
              onClick={() => setEmojiPickerOpen((o) => !o)}
              title="Change project icon"
              className="flex h-[64px] w-[64px] items-center justify-center rounded-[20px] border border-[var(--border-primary)] text-[34px] leading-none shadow-sm transition-colors hover:border-[var(--input-focus-border)] cursor-pointer select-none"
              style={{ backgroundColor: "color-mix(in srgb, var(--accent) 15%, transparent)" }}
            >
              {emoji}
            </button>
            {emojiPickerOpen && (
              <div
                ref={emojiPickerRef}
                className="absolute left-1/2 top-full z-40 mt-2 -translate-x-1/2 overflow-hidden rounded-[16px] shadow-2xl"
              >
                <Picker
                  data={data}
                  onEmojiSelect={(em: { native: string }) => {
                    setEmoji(em.native);
                    setEmojiPickerOpen(false);
                  }}
                  theme={isDark ? "dark" : "light"}
                  previewPosition="none"
                  skinTonePosition="none"
                  set="native"
                />
              </div>
            )}
          </div>
          <h1 className="text-[22px] font-semibold text-[var(--text-primary)]">
            Start a new project
          </h1>
          <p className="mt-1 text-[14px] text-[var(--text-secondary)]">
            Describe what you want to do — your agent will ask for the details it needs.
          </p>
        </div>

        {/* Agent selector */}
        <div ref={agentMenuRef} className="relative mb-3 inline-flex self-start">
          <button
            type="button"
            onClick={() => setAgentMenuOpen((o) => !o)}
            className="flex items-center gap-2 rounded-[10px] px-2 py-1.5 text-[15px] text-[var(--text-primary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
          >
            {selectedAgent ? (
              <span
                className="flex h-[28px] w-[28px] flex-shrink-0 items-center justify-center rounded-full text-[14px]"
                style={{ backgroundColor: agentAvatarColor(selectedAgent.name, isDark) }}
              >
                {selectedAgent.emoji ?? "\u{1F916}"}
              </span>
            ) : (
              <span className="h-[28px] w-[28px] flex-shrink-0 rounded-full bg-[var(--text-primary)]" />
            )}
            <span className={selectedAgent ? "font-medium" : "text-[var(--text-secondary)]"}>
              {selectedAgent ? selectedAgent.name : "Select Agent"}
            </span>
            {selectedAgent && (selectedAgent.coordinator_level ?? 0) > 0 && (
              <CoordinatorBadge level={selectedAgent.coordinator_level ?? 0} size={18} className="flex-shrink-0" />
            )}
            <ChevronDown size={16} className="text-[var(--text-secondary)]" />
          </button>

          {agentMenuOpen && (
            <div className="absolute left-0 top-full z-30 mt-1 max-h-[280px] w-[260px] overflow-y-auto rounded-[12px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] py-1 shadow-lg">
              {selectableAgents.length === 0 ? (
                <div className="px-3 py-2 text-[13px] text-[var(--text-secondary)]">
                  No delegation-capable agents yet — give an agent delegates in its profile.
                </div>
              ) : (
                selectableAgents.map((agent) => (
                  <button
                    key={agent.agent_id}
                    type="button"
                    onClick={() => {
                      setSelectedAgentId(agent.agent_id);
                      setAgentMenuOpen(false);
                    }}
                    className="flex w-full items-center gap-2 px-3 py-2 text-left text-[14px] text-[var(--text-primary)] hover:bg-[var(--bg-hover)] transition-colors cursor-pointer"
                  >
                    <span
                      className="flex h-[24px] w-[24px] flex-shrink-0 items-center justify-center rounded-full text-[13px]"
                      style={{ backgroundColor: agentAvatarColor(agent.name, isDark) }}
                    >
                      {agent.emoji ?? "\u{1F916}"}
                    </span>
                    <span className="truncate flex-1 min-w-0">{agent.name}</span>
                    {(agent.coordinator_level ?? 0) > 0 && (
                      <CoordinatorBadge level={agent.coordinator_level ?? 0} size={18} className="flex-shrink-0" />
                    )}
                  </button>
                ))
              )}
            </div>
          )}
        </div>

        {/* Chat-style composer — a single rounded box with the input and its
            controls inside. Submitting this kicks off the project conversation. */}
        <div
          onClick={(e) => {
            // Clicking the box's empty space focuses the goal textarea, but a
            // click that already landed on an interactive child (the name
            // input, a button) must keep its own focus.
            if ((e.target as HTMLElement).closest("input, textarea, button")) return;
            textareaRef.current?.focus();
          }}
          className="flex cursor-text flex-col rounded-[16px] border border-[var(--border-primary)] bg-[var(--chat-input-bg)] transition-colors focus-within:border-[var(--input-focus-border)]"
        >
          {/* Optional name field */}
          <input
            ref={nameInputRef}
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                textareaRef.current?.focus();
              }
            }}
            placeholder="Project name (optional)"
            className="w-full bg-transparent px-4 pt-3 text-[13px] font-medium text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none"
          />
          {/* Goal input */}
          <textarea
            ref={textareaRef}
            value={goal}
            onChange={(e) => setGoal(e.target.value)}
            onKeyDown={handleKeyDown}
            rows={1}
            placeholder="Describe what you want to build or get done..."
            className="max-h-[200px] w-full resize-none bg-transparent px-4 pt-2 text-[15px] leading-relaxed text-[var(--text-primary)] placeholder:text-[var(--text-tertiary)] focus:outline-none"
          />

          {/* Controls row: attach (left) + send (right) */}
          <div className="flex items-center justify-between px-3 py-2.5">
            <button
              type="button"
              onClick={(e) => {
                e.stopPropagation();
                handleAttach();
              }}
              title="Attach files"
              className="flex h-8 w-8 items-center justify-center rounded-full text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] transition-colors cursor-pointer"
            >
              <Paperclip size={18} />
            </button>

            <button
              type="button"
              onClick={(e) => {
                e.stopPropagation();
                handleCreate();
              }}
              disabled={!isValid || submitting}
              title="Start project"
              className="flex h-8 w-8 items-center justify-center rounded-full bg-[var(--text-primary)] text-[var(--bg-secondary)] hover:opacity-90 transition-opacity disabled:cursor-not-allowed disabled:opacity-30 cursor-pointer"
            >
              {submitting ? (
                <Loader2 size={16} className="animate-spin" />
              ) : (
                <ArrowUp size={18} />
              )}
            </button>
          </div>
        </div>

        {/* Attached files — chips directly under the composer. These move with
            the input once the conversation begins. */}
        {files.length > 0 && (
          <div className="mt-3 flex flex-wrap gap-2">
            {files.map((path) => (
              <span
                key={path}
                className="flex items-center gap-1.5 rounded-[8px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] px-2.5 py-1 text-[12px] text-[var(--text-primary)]"
              >
                <span className="max-w-[200px] truncate">{path.split("/").pop()}</span>
                <button
                  type="button"
                  onClick={() => removeFile(path)}
                  className="text-[var(--text-secondary)] hover:text-[var(--text-primary)] cursor-pointer"
                >
                  <X size={13} />
                </button>
              </span>
            ))}
          </div>
        )}

        {/* Working directory — optional folder the agent operates in. Sits
            below the attachment chips. */}
        <div className="mt-3">
          {workingDir ? (
            <div className="flex items-center gap-2 rounded-[10px] border border-[var(--border-primary)] bg-[var(--bg-secondary)] px-3 py-2">
              <FolderOpen size={16} className="flex-shrink-0 text-[var(--text-secondary)]" />
              <span
                className="flex-1 truncate text-[13px] text-[var(--text-primary)]"
                title={workingDir}
              >
                {workingDir}
              </span>
              <button
                type="button"
                onClick={handleSelectFolder}
                className="flex-shrink-0 rounded-[6px] px-2 py-0.5 text-[12px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] transition-colors cursor-pointer"
              >
                Change
              </button>
              <button
                type="button"
                onClick={() => setWorkingDir(null)}
                title="Clear working directory"
                className="flex-shrink-0 text-[var(--text-secondary)] hover:text-[var(--text-primary)] cursor-pointer"
              >
                <X size={14} />
              </button>
            </div>
          ) : (
            <button
              type="button"
              onClick={handleSelectFolder}
              className="flex w-full items-center gap-2 rounded-[10px] border border-dashed border-[var(--border-primary)] px-3 py-2 text-[13px] text-[var(--text-secondary)] hover:border-[var(--input-focus-border)] hover:text-[var(--text-primary)] transition-colors cursor-pointer"
            >
              <Folder size={16} className="flex-shrink-0" />
              <span>Set working directory (optional)</span>
            </button>
          )}
        </div>

      </div>
    </div>
  );
}
