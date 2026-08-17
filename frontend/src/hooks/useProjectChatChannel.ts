import { useEffect, useMemo, useRef } from "react";
import {
  useChatStore,
  useIsTyping,
  useStreamingText,
  useActiveToolCalls,
  type ActiveToolCall,
} from "../stores/chatStore";
import { useProjectStore } from "../stores/projectStore";
import { useNetworkStore } from "../stores/networkStore";
import { parseSkillLoadInfo } from "../components/chat/MessageList";
import type { AgentProfile } from "../types/api";

export interface ProjectChatChannelResult {
  projectKey: string;
  isTyping: boolean;
  streamingText: string;
  activeToolCalls: ActiveToolCall[];
  hasMessages: boolean;
  hasTrailingSkillLoad: boolean;
  selectedAgentProfile: AgentProfile | null;
  messagesLoading: boolean;
  canSend: boolean;
  sendProjectMessage: (projectId: string, content: string, attachmentIds?: string[]) => Promise<void>;
}

/**
 * Bridges the projectStore message list into a chatStore virtual channel keyed
 * `project:<projectId>`, enabling MessageList + ChatInput to render project
 * threads without duplicating channel-bridge logic across call sites.
 */
export function useProjectChatChannel(
  projectId: string,
  agentName: string,
  agentEmoji: string,
): ProjectChatChannelResult {
  const projectKey = `project:${projectId}`;

  const mountProjectChannel = useChatStore((s) => s.mountProjectChannel);
  const syncProjectChannelMessages = useChatStore((s) => s.syncProjectChannelMessages);

  const projectMessages = useProjectStore((s) => s.messages);
  const projectCursor = useProjectStore((s) => s.cursor);
  const messagesLoading = useProjectStore((s) => s.messagesLoading);
  const sendProjectMessage = useProjectStore((s) => s.sendMessage);
  const activeProject = useProjectStore((s) => s.activeProject);
  const scrollToBottomTrigger = useProjectStore((s) => s.scrollToBottomTrigger);

  const isInternetOnline = useNetworkStore((s) => s.isInternetOnline);
  const isServerOnline = useNetworkStore((s) => s.isServerOnline);
  const canSend = isInternetOnline && isServerOnline;

  const isTyping = useIsTyping(projectKey);
  const streamingText = useStreamingText(projectKey);
  const activeToolCalls = useActiveToolCalls(projectKey);
  const selectedAgentProfile = useChatStore((s) => s.selectedAgentProfile);

  const goalText = activeProject?.goal ?? null;
  // Find the first visible user message to decide whether the goal is already
  // present in the transcript. We search by role rather than using [0] because
  // the transcript may start with system/marker entries that have different
  // content, which would cause a false-positive and create a duplicate bubble.
  const firstUserMessage = projectMessages.find(
    (m) => typeof m.role === "string" && m.role === "user",
  );
  const needsGoalBubble =
    !!goalText &&
    (projectMessages.length === 0 ||
      (firstUserMessage?.content?.trim() ?? "") !== goalText.trim());

  const augmentedMessages = useMemo(() => {
    if (!needsGoalBubble || !goalText || !activeProject) return projectMessages;
    const goalEntry = {
      ts: activeProject.created_at ?? new Date().toISOString(),
      role: "user" as const,
      content: goalText,
      event_type: "message" as const,
    };
    return [goalEntry, ...projectMessages];
  }, [needsGoalBubble, goalText, activeProject, projectMessages]);

  // Mount once per projectId — subsequent syncs go via syncProjectChannelMessages
  // so the virtualizer doesn't remount on every message change.
  useEffect(() => {
    mountProjectChannel(projectId, augmentedMessages, projectCursor, agentName, agentEmoji);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectId]);

  useEffect(() => {
    syncProjectChannelMessages(projectId, augmentedMessages, projectCursor);
  }, [projectId, augmentedMessages, projectCursor, syncProjectChannelMessages]);

  const prevScrollTriggerRef = useRef(scrollToBottomTrigger);
  useEffect(() => {
    if (scrollToBottomTrigger > prevScrollTriggerRef.current) {
      syncProjectChannelMessages(projectId, augmentedMessages, projectCursor, true);
    }
    prevScrollTriggerRef.current = scrollToBottomTrigger;
  }, [scrollToBottomTrigger, projectId, augmentedMessages, projectCursor, syncProjectChannelMessages]);

  const chatMessages = useChatStore((s) => s.messages);
  const hasMessages = chatMessages.length > 0;

  const hasTrailingSkillLoad = useMemo(() => {
    for (let i = chatMessages.length - 1; i >= 0; i--) {
      const m = chatMessages[i];
      if (!m.hidden_from_user) break;
      if (parseSkillLoadInfo(m)) return true;
    }
    return false;
  }, [chatMessages]);

  return {
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
  };
}
