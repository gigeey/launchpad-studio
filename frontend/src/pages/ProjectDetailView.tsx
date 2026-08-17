import { useEffect, useState, useCallback } from "react";
import { useParams } from "react-router-dom";
import { Loader2, RefreshCw, Users } from "lucide-react";
import { useProjectStore } from "../stores/projectStore";
import { useChatStore, isFormMinimized } from "../stores/chatStore";
import { useProjectChatChannel } from "../hooks/useProjectChatChannel";
import { teamBannerGradient, teamTintColor } from "../lib/agentColors";
import { useIsDark } from "../stores/userPreferencesStore";
import { ProjectWorkspace } from "../components/projects/ProjectWorkspace";
import { AgentOrgTree } from "../components/shared/AgentOrgTree";
import { MessageList } from "../components/chat/MessageList";
import { ChatInput } from "../components/chat/ChatInput";
import { TypingIndicator } from "../components/chat/TypingIndicator";
import { PendingFormOverlay } from "../components/chat/PendingFormOverlay";
import { MinimizedFormBar } from "../components/chat/MinimizedFormBar";
import * as api from "../lib/api";
import { formatPendingFormWaitingLabel } from "../lib/pendingFormLabel";
import type { ProjectStatus, AgentProfile, DelegateTarget } from "../types/api";
import { ErrorBoundary } from "../components/shared/ErrorBoundary";
import { ContentGate } from "../components/ContentGate";
import { MessageThreadSkeleton } from "../components/shared/Skeletons";
import { useReadyLatch } from "../hooks/useReadyLatch";

// ---------------------------------------------------------------------------
// Status pill — solid block-color background, never transparent/full-width
// ---------------------------------------------------------------------------

const STATUS_LABELS: Record<ProjectStatus, string> = {
  interviewing: "Interviewing",
  active: "Active",
  completed: "Completed",
  archived: "Archived",
  needs_review: "Needs Review",
};

const STATUS_PILL_CLASSES: Record<ProjectStatus, string> = {
  interviewing: "bg-amber-500 text-white",
  active: "bg-green-500 text-white",
  completed: "bg-gray-500 text-white",
  archived: "bg-gray-400 text-white",
  needs_review: "bg-red-500 text-white",
};

function StatusPill({ status }: { status: ProjectStatus }) {
  return (
    <span className={`inline-flex w-fit flex-shrink-0 items-center rounded-full px-2.5 py-0.5 text-[11px] font-semibold ${STATUS_PILL_CLASSES[status]}`}>
      {STATUS_LABELS[status]}
    </span>
  );
}

// ---------------------------------------------------------------------------
// InterviewView — full-page chat, status=interviewing
// ---------------------------------------------------------------------------

function InterviewView({ projectId, agentId, agentName, agentEmoji, projectName }: {
  projectId: string;
  agentId: string;
  agentName: string;
  agentEmoji: string;
  projectName: string;
}) {
  const isDark = useIsDark();
  const {
    projectKey,
    isTyping,
    streamingText,
    activeToolCalls,
    hasMessages,
    hasTrailingSkillLoad,
    selectedAgentProfile,
    messagesLoading,
    canSend,
    sendProjectMessage,
  } = useProjectChatChannel(projectId, agentName, agentEmoji);

  const projectLoadError = useProjectStore((s) => s.projectLoadError);

  // Sync form request (AskUserQuestionWithForm): swap the composer for the
  // form until the operator answers — same gate the personal chat uses.
  const pendingForm = useChatStore((s) => s.pendingFormByAgent[projectKey]);
  // Minimized forms swap the composer slot for a bar instead of ChatInput —
  // the form itself stays mounted inside `PendingFormOverlay`, just hidden.
  const formMinimized = useChatStore((s) => isFormMinimized(s.minimizedFormByKey, projectKey, pendingForm?.thread_id));
  const setFormMinimized = useChatStore((s) => s.setFormMinimized);

  // Re-arms per project+agent channel so switching projects re-gates the
  // reveal instead of showing the previous project's thread as a stand-in.
  const messagesReady = useReadyLatch(hasMessages, messagesLoading, projectKey);

  return (
    <div
      className="flex-1 min-h-0 flex flex-col overflow-hidden"
      style={{
        backgroundImage: `linear-gradient(180deg, ${teamTintColor(projectName, isDark)}${isDark ? "26" : "1F"} 0%, ${teamTintColor(projectName, isDark)}0A 38%, transparent 60%)`,
      }}
    >
      <ContentGate ready={messagesReady} skeleton={<MessageThreadSkeleton />} className="flex-1 min-h-0 flex flex-col overflow-hidden">
        {projectLoadError ? (
          <div className="mx-auto mt-8 max-w-md rounded-[12px] border border-red-200 bg-red-50 px-4 py-3 text-[13px] text-red-700">
            {projectLoadError}
          </div>
        ) : hasMessages ? (
          <MessageList key={projectKey} />
        ) : null}
      </ContentGate>
      <div className="relative px-[10px] pb-[12px] pt-[8px] flex-shrink-0">
        {isTyping && !streamingText && activeToolCalls.length === 0 && !hasTrailingSkillLoad && (
          <div className="absolute bottom-full left-0 right-0 px-[10px] pointer-events-none">
            <TypingIndicator emoji={selectedAgentProfile?.emoji ?? agentEmoji} />
          </div>
        )}
        {pendingForm ? (
          // A form is pending — `PendingFormOverlay` below owns this area
          // while expanded. Only surface the composer slot when the operator
          // has minimized it; ChatInput must never mount while a form is
          // pending (that's the mechanism that blocks typing underneath it).
          formMinimized && (
            <MinimizedFormBar
              title={formatPendingFormWaitingLabel(selectedAgentProfile?.name ?? agentName, pendingForm.fields.length)}
              onExpand={() => setFormMinimized(projectKey, pendingForm.thread_id, false)}
            />
          )
        ) : (
          <ChatInput
            conversationId={projectKey}
            agentId={agentId}
            fileCapabilitiesSupported
            disabled={!canSend}
            isProcessing={isTyping}
            onStop={() => api.cancelProjectRun(projectId).catch(() => {})}
            onUploadAttachment={(file) => api.uploadProjectAttachment(projectId, file)}
            onAddFolderReference={(path) => api.uploadProjectFolderReference(projectId, path)}
            onDeleteAttachment={(id) => api.deleteProjectAttachment(projectId, id)}
            onSend={(content, attachmentIds) => {
              sendProjectMessage(projectId, content, attachmentIds ?? []);
            }}
            placeholder={`Reply to ${agentName}…`}
          />
        )}
        <PendingFormOverlay channelKey={projectKey} />
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Project profile view — org tree using the same AgentOrgTree Teams uses
// ---------------------------------------------------------------------------
function ProjectProfileView({ agentId }: { agentId: string }) {
  const [coordProfile, setCoordProfile] = useState<AgentProfile | null>(null);

  useEffect(() => {
    if (!agentId) return;
    api.getAgent(agentId).then(setCoordProfile).catch(console.error);
  }, [agentId]);

  const delegates: DelegateTarget[] = coordProfile?.delegates_to ?? [];
  const members = delegates.map(d => ({
    agent_id: d.target_agent_id,
    role_description: d.purpose,
  }));
  const aliasMap = Object.fromEntries(delegates.map(d => [d.target_agent_id, d.name]));

  return (
    <div className="px-[30px] py-[24px] flex-shrink-0">
      <h3 className="text-[14px] font-semibold text-[var(--text-primary)] mb-[20px]">
        Members
      </h3>
      <AgentOrgTree
        coordinatorId={agentId}
        members={members}
        aliasMap={aliasMap}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Main view
// ---------------------------------------------------------------------------

export function ProjectDetailView() {
  const { projectId } = useParams<{ projectId: string }>();
  const isDark = useIsDark();

  const selectProject = useProjectStore((s) => s.selectProject);
  const invalidateCache = useProjectStore((s) => s.invalidateCache);
  const activeProject = useProjectStore((s) => s.activeProject);

  const agents = useChatStore((s) => s.agents);
  const fetchAgents = useChatStore((s) => s.fetchAgents);

  const [refreshing, setRefreshing] = useState(false);
  const [viewMode, setViewMode] = useState<"workspace" | "profile">("workspace");

  const handleRefresh = useCallback(async () => {
    if (!projectId || refreshing) return;
    setRefreshing(true);
    try {
      invalidateCache(projectId);
      await selectProject(projectId);
    } finally {
      setRefreshing(false);
    }
  }, [projectId, refreshing, invalidateCache, selectProject]);

  useEffect(() => {
    if (projectId) selectProject(projectId);
  }, [projectId, selectProject]);

  useEffect(() => {
    if (agents.length === 0) fetchAgents();
  }, [agents.length, fetchAgents]);

  const agentId = activeProject?.agent_id ?? "";
  const agentSnapshot = agents.find((a) => a.agent_id === agentId);
  const agentName = agentSnapshot?.name ?? "Agent";
  const agentEmoji = agentSnapshot?.emoji ?? "🤖";
  const projectName = activeProject?.name ?? "Project";
  const projectEmoji = activeProject?.emoji ?? "📁";

  const isWorkspace =
    activeProject?.status === "active" || activeProject?.status === "completed";

  // Loading: skeleton header until the project resolves
  if (!activeProject) {
    return (
      <ErrorBoundary>
        <div className="flex flex-1 flex-col overflow-hidden">
          <div className="flex flex-shrink-0 items-center gap-3 border-b border-[var(--border-primary)] bg-[var(--bg-primary)] px-5 py-3">
            <div className="h-8 w-48 animate-pulse rounded-[8px] bg-[var(--bg-secondary)]" />
          </div>
          <div className="flex flex-1 items-center justify-center">
            <Loader2 size={20} className="animate-spin text-[var(--text-tertiary)]" />
          </div>
        </div>
      </ErrorBoundary>
    );
  }

  // Both the interview and workspace phases wear the same gradient banner so
  // the only thing that changes on the interviewing → active transition is the
  // body content (chat thread vs. tasklist panels).
  return (
    <ErrorBoundary>
      <div className="flex flex-1 flex-col overflow-hidden">
        {/* Gradient banner — mirrors TeamsView header exactly */}
        <div className={`w-full bg-gradient-to-br ${teamBannerGradient(projectName, isDark)} relative overflow-hidden flex-shrink-0 px-[24px] py-[16px]`}>
          <div className={`absolute inset-0 pointer-events-none ${isDark ? "bg-black/45" : "bg-white/60"}`} />
          <div
            className="absolute inset-0 opacity-[0.1] pointer-events-none"
            style={{
              backgroundImage: `linear-gradient(#fff 1px, transparent 1px), linear-gradient(90deg, #fff 1px, transparent 1px)`,
              backgroundSize: "30px 30px",
            }}
          />
          <div className="absolute -top-10 -right-10 w-40 h-40 bg-cyan-400/15 blur-[60px] rounded-full pointer-events-none" />
          <div className="absolute -bottom-20 -left-10 w-60 h-60 bg-blue-600/15 blur-[80px] rounded-full pointer-events-none" />

          <div className="relative z-10 flex items-center justify-between">
            <div className="flex items-center">
              <div className="w-[50px] h-[50px] bg-[var(--bg-secondary)] rounded-[14px] shadow-sm border border-[var(--border-secondary)] flex items-center justify-center text-[26px] z-10 select-none flex-shrink-0">
                {projectEmoji}
              </div>
              <div className="ml-[12px]">
                <div className="flex items-center gap-[8px]">
                  <h2 className="text-[18px] font-bold text-white drop-shadow-md">{projectName}</h2>
                  <StatusPill status={activeProject.status} />
                </div>
                <p className="text-[12px] text-white/90 font-medium leading-snug drop-shadow-sm mt-[1px]">
                  {agentName}
                </p>
              </div>
            </div>

            <div className="flex items-center gap-[6px]">
              {isWorkspace && (
                <button
                  onClick={() => setViewMode(viewMode === "profile" ? "workspace" : "profile")}
                  className={`p-[6px] rounded-[8px] transition-colors cursor-pointer border ${viewMode === "profile"
                    ? "bg-white/25 text-white border-white/30"
                    : "bg-black/10 text-white/90 hover:bg-black/20 hover:text-white border-white/10"
                    }`}
                  title="Project profile"
                >
                  <Users className="w-[16px] h-[16px]" />
                </button>
              )}
              <button
                onClick={handleRefresh}
                disabled={refreshing}
                className="p-[6px] rounded-[8px] bg-black/10 text-white/90 hover:bg-black/20 hover:text-white transition-colors cursor-pointer border border-white/10 disabled:opacity-50"
                title="Refresh"
              >
                <RefreshCw className={`w-[16px] h-[16px] ${refreshing ? "animate-spin" : ""}`} />
              </button>
            </div>
          </div>
        </div>

        {isWorkspace ? (
          viewMode === "profile" ? (
            <div className="flex-1 overflow-y-auto">
              <ProjectProfileView agentId={agentId} />
            </div>
          ) : (
            <ProjectWorkspace
              project={activeProject}
              projectId={projectId!}
              agentId={agentId}
              agentName={agentName}
              agentEmoji={agentEmoji}
            />
          )
        ) : (
          <InterviewView
            projectId={projectId!}
            agentId={agentId}
            agentName={agentName}
            agentEmoji={agentEmoji}
            projectName={projectName}
          />
        )}
      </div>
    </ErrorBoundary>
  );
}
