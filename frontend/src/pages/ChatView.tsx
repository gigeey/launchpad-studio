import { useEffect, useState, useCallback, useRef, useMemo, useDeferredValue } from "react";
import { useParams } from "react-router-dom";
import { AnimatePresence, motion } from "framer-motion";
import { useShallow } from "zustand/react/shallow";
import { useChatStore, useStreamingText, useActiveToolCalls, useIsTyping, useIsAgentTurnActive, useStreamingThreadId, isPendingFormStillCurrent, pendingFormForThread, pendingSyncFormForThread, isFormMinimized } from "../stores/chatStore";
import { useDraftStore, type DraftAttachment } from "../stores/draftStore";
import { useReadStatusStore } from "../stores/readStatusStore";
import { useUserPreferencesStore } from "../stores/userPreferencesStore";
import { isAgentMessage, type Assignment, type Attachment } from "../types/api";
import { ChatHeader } from "../components/chat/ChatHeader";
import { ThreadTabStrip } from "../components/chat/ThreadTabStrip";
import { ChannelsColumn } from "../components/chat/ChannelsColumn";
import { AssignmentsColumn } from "../components/chat/AssignmentsColumn";
import { resolveChannelThreadPartition } from "../lib/channelThreads";
import { resolveAssignmentStreamingByThreadId, resolveAssignmentThreadPartition } from "../lib/assignmentThreads";
import { switchToThread, threadDraftKey, getBridgeChannelKind, CHANNEL_KIND_LABELS } from "../lib/threadNavigation";
import type { Thread } from "../types/api";
import { MessageList, PinnedBookmarkOverlay, PinnedSearchOverlay, parseSkillLoadInfo } from "../components/chat/MessageList";
import { TypingIndicator } from "../components/chat/TypingIndicator";
import { DelegatePillRow } from "../components/chat/DelegatePillRow";
import { ChatInput } from "../components/chat/ChatInput";
import { AskUserQuestionForm } from "../components/chat/AskUserQuestionForm";
import { AsyncFormRequestCard } from "../components/chat/AsyncFormRequestCard";
import { MinimizedFormBar } from "../components/chat/MinimizedFormBar";
import { MemoryPanel } from "../components/chat/MemoryPanel";
import { BookmarkPanel } from "../components/chat/BookmarkPanel";
import { AssetsPanel } from "../components/chat/AssetsPanel";
import { TodoPanel } from "../components/chat/TodoPanel";
import { useAgentTasklistStore, useAgentTasklistsForAgent, countInProgress } from "../stores/agentTasklistStore";
import { useBookmarkStore } from "../stores/bookmarkStore";
import { useNetworkStore } from "../stores/networkStore";
import { useFocusPathStore } from "../stores/focusPathStore";
import { precomputeContext, submitFormAnswer, submitAsyncFormAnswer, dismissAsyncForm } from "../lib/api";
import { formatPendingFormWaitingLabel } from "../lib/pendingFormLabel";
import { X, StickyNote, Bookmark, Paperclip, ListTodo, Clock, ChevronDown, Pencil } from "lucide-react";
import messageBubble from "../assets/messageBubbleNoBG.png";
import type { FormAnswerMap, AsyncFormRequestMeta, FormAction, WithdrawnFormDraftEntry } from "../types/form";
import { ContentGate } from "../components/ContentGate";
import { MessageThreadSkeleton } from "../components/shared/Skeletons";
import { useReadyLatch } from "../hooks/useReadyLatch";
import { useQueuedMessageSend } from "../hooks/useQueuedMessageSend";

const EMPTY_DRAFT_ATTACHMENTS: DraftAttachment[] = [];
const EMPTY_THREADS: Thread[] = [];
const EMPTY_ASSIGNMENTS: Assignment[] = [];

export function ChatView() {
  const { subMenuSlug: agentId } = useParams<{ subMenuSlug: string }>();

  const selectAgent = useChatStore((s) => s.selectAgent);
  const sendMessage = useChatStore((s) => s.sendMessage);
  const messages = useChatStore((s) => s.messages);
  const messagesLoading = useChatStore((s) => s.messagesLoading);
  const selectedAgentProfile = useChatStore((s) => s.selectedAgentProfile);
  // Scoped to whichever thread is actually active for this agent — without
  // this, the warmup/floating "is responding" indicators always read the
  // default thread's bucket, so switching to a non-default thread showed
  // nothing until MessageList's own (correctly thread-scoped) streaming
  // bubble appeared once real tokens arrived.
  const streamingThreadId = useStreamingThreadId(agentId ?? null);
  const isTyping = useIsTyping(agentId ?? null, streamingThreadId);
  // Distinct from `isTyping`: stays true across the tool-call gaps and
  // chained-run boundaries within a single turn (see useIsAgentTurnActive's
  // docstring) — the signal the queued-message flush below needs so it
  // doesn't fire the instant the first text segment finalizes.
  const isTurnActive = useIsAgentTurnActive(agentId ?? null, streamingThreadId);
  const streamingText = useStreamingText(agentId ?? null, streamingThreadId);
  const activeToolCalls = useActiveToolCalls(agentId ?? null, streamingThreadId);
  const cancelRun = useChatStore((s) => s.cancelRun);
  const isStreamingAreaVisible = useChatStore((s) => s.isStreamingAreaVisible);

  const clearFocusPath = useFocusPathStore((s) => s.clearFocusPath);

  // Scoped to streamingThreadId (computed above) — a sync form pending on a
  // different thread of this agent must not swap the composer out here.
  const pendingForm = useChatStore((s) =>
    agentId ? pendingSyncFormForThread(s.pendingFormByAgent, agentId, streamingThreadId) : undefined
  );
  // Whether the currently-pending form (sync or async, resolved below) has
  // been collapsed into the composer-slot bar. Scoped identically to
  // `pendingForm` (agent + streamingThreadId) — see `isFormMinimized`'s
  // docstring.
  const formMinimized = useChatStore((s) =>
    agentId ? isFormMinimized(s.minimizedFormByKey, agentId, streamingThreadId) : false
  );
  const setFormMinimized = useChatStore((s) => s.setFormMinimized);
  const clearPendingForm = useChatStore((s) => s.clearPendingForm);
  const addFormAnswerEntry = useChatStore((s) => s.addFormAnswerEntry);
  const addAsyncFormAnswerEntry = useChatStore((s) => s.addAsyncFormAnswerEntry);
  const stashWithdrawnFormDraft = useChatStore((s) => s.stashWithdrawnFormDraft);
  const fetchAgents = useChatStore((s) => s.fetchAgents);
  const agents = useChatStore((s) => s.agents);
  const clearPendingAsyncForm = useChatStore((s) => s.clearPendingAsyncForm);

  // Scoped to streamingThreadId (computed above, `undefined` = default
  // thread) — a form pending on a different thread of this agent must not
  // hand the composer over here. `isPendingFormStillCurrent` used to also
  // gate this on a staleness check (cleared the instant the operator skipped
  // past the form — typed a new message, or the agent replied) but that
  // auto-release invariant is gone: the composer now blocks on this form
  // until it's explicitly answered, dismissed, or superseded by a newer one
  // (see `formSlotOccupied` below) — all of which remove it from
  // `pending_forms` outright, so presence here already means "still open."
  const pendingAsyncForm = agentId
    ? pendingFormForThread(agents.find((a) => a.agent_id === agentId)?.pending_forms, streamingThreadId)
    : undefined;
  const pendingAsyncFormId = isPendingFormStillCurrent(pendingAsyncForm)
    ? (pendingAsyncForm?.form_id ?? null)
    : null;
  // Read straight off the `pending_forms` snapshot pointer (`pendingAsyncForm`,
  // computed above) instead of scanning the transcript for a `form_request`
  // message — that entry is now written `hidden_from_user: true` (mirrors the
  // sync form's own write site) so it never surfaces as a visible transcript
  // entry, and this is the one mechanism that already renders the card on
  // both the live-SSE path and a page reload (`GET /agents` refetch), same as
  // the sync overlay does. `pendingAsyncForm.spec.mode` is always `"async"`
  // here — `pendingFormForThread` already filtered out `mode: "sync"` entries.
  const pendingAsyncFormMeta = useMemo<AsyncFormRequestMeta | null>(() => {
    if (!pendingAsyncFormId || !pendingAsyncForm?.spec) return null;
    return { ...pendingAsyncForm.spec, mode: "async" };
  }, [pendingAsyncFormId, pendingAsyncForm]);

  // Exactly ONE form slot per thread (owner-locked invariant): only the main
  // agent ever posts a form, so a sync (`pendingForm`) and an async
  // (`pendingAsyncFormMeta`) form can never legitimately be pending on the
  // same thread at once — this is a union for defensiveness/clarity, not a
  // race two forms could actually win. Both branches feeding this are
  // already symmetric across the live-SSE and page-reload paths on their
  // own (`pendingForm` via `setPendingForm` + `hydratePendingSyncFormsFromAgents`
  // for sync; `pendingAsyncFormMeta` via the `form_posted` handler +
  // `fetchAgents()` for async — see the comment above `pendingAsyncFormMeta`),
  // so this single boolean is all the composer gate below needs to block on.
  const formSlotOccupied = !!pendingForm || !!pendingAsyncFormMeta;

  // Always enable file attachment UI — the server handles uploads regardless of
  // whether the underlying CLI agent consumes them.
  const fileCapabilitiesSupported = true;

  const markRead = useReadStatusStore((s) => s.markRead);
  const memoryPanelWidth = useUserPreferencesStore((s) => s.memoryPanelWidth);
  const setMemoryPanelWidth = useUserPreferencesStore((s) => s.setMemoryPanelWidth);

  const handleFormSubmit = useCallback(async (answers: FormAnswerMap) => {
    if (!agentId || !pendingForm) return;
    await submitFormAnswer(agentId, { form_id: pendingForm.form_id, answers });
    clearPendingForm(agentId, pendingForm.thread_id);
    addFormAnswerEntry(agentId, { form: pendingForm, answers });
  }, [agentId, pendingForm, clearPendingForm, addFormAnswerEntry]);

  // Action row (Cancel / Regenerate / Something else) — the one exit
  // pathway shared by both the sync form overlay and the async form card
  // below, so there's a single handler per branch rather than a parallel
  // async-only one. Sync: resolves the agent's suspended tool call the same
  // way a submission does, just via `action` instead of `answers` (see
  // `AskUserQuestionForm`'s `onAction` prop docs). Async: nothing is
  // suspended server-side, so there's no per-action backend distinction yet
  // — every action collapses onto the same dismiss the old standalone
  // Decline button used (`dismissAsyncForm` + `clearPendingAsyncForm`).
  const handleFormAction = useCallback(async (action: FormAction, note?: string) => {
    if (!agentId) return;
    if (pendingForm) {
      await submitFormAnswer(agentId, { form_id: pendingForm.form_id, answers: {}, action, note });
      clearPendingForm(agentId, pendingForm.thread_id);
      addFormAnswerEntry(agentId, { form: pendingForm, answers: {}, action, note });
      return;
    }
    if (pendingAsyncFormId) {
      await dismissAsyncForm(agentId, pendingAsyncFormId);
      clearPendingAsyncForm(agentId, pendingAsyncFormId);
    }
  }, [agentId, pendingForm, pendingAsyncFormId, clearPendingForm, addFormAnswerEntry, clearPendingAsyncForm]);

  // See `AskUserQuestionForm`'s `onWithdraw` prop doc — fires only when this
  // sync form's own instance unmounts with unsaved answers and no
  // submit/action ever went through, i.e. it just got superseded by a newer
  // form on the same slot (see the `key={pendingForm.form_id}` overlay
  // below). Stashed by the withdrawn form's own id so `FormWithdrawnIndicator`
  // can surface it against the matching `form_withdrawn` timeline entry.
  const handleFormWithdraw = useCallback((formId: string, draft: WithdrawnFormDraftEntry[]) => {
    stashWithdrawnFormDraft(formId, draft);
  }, [stashWithdrawnFormDraft]);

  // Which side panel (Memory/Bookmarks/Assets/Todos) is open, scoped per
  // agent in chatStore rather than local component state — see
  // `activePanelByAgent`'s docstring for why: it makes each chat recall its
  // own panel state instead of one agent's open panel (or auto-open on
  // tasklist creation) leaking into whichever other agent's chat the user
  // switches to.
  const setChatPanel = useChatStore((s) => s.setActivePanel);

  // Thread tab strip: the default "Main thread" plus any threads the operator
  // has created, rendered as the tab row that floats over the header's
  // bottom border. Selection lives in chatStore so it survives tab-strip
  // remounts and is shared with anything else that needs "which thread".
  const loadThreads = useChatStore((s) => s.loadThreads);
  const loadAssignments = useChatStore((s) => s.loadAssignments);
  const createFreshThread = useChatStore((s) => s.createFreshThread);
  const deleteThread = useChatStore((s) => s.deleteThread);
  const archiveThread = useChatStore((s) => s.archiveThread);
  const unarchiveThread = useChatStore((s) => s.unarchiveThread);
  const renameThread = useChatStore((s) => s.renameThread);
  const markThreadViewed = useChatStore((s) => s.markThreadViewed);
  const threads = useChatStore((s) => (agentId ? s.threadsByAgent.get(agentId) ?? EMPTY_THREADS : EMPTY_THREADS));
  const selectedThreadId = useChatStore((s) => (agentId ? s.selectedThreadIdByAgent.get(agentId) : undefined));
  const defaultThreadId = useMemo(
    () => threads.find((t) => t.kind === "default")?.id ?? `default-${agentId}`,
    [threads, agentId],
  );
  const activeThreadId = selectedThreadId ?? defaultThreadId;

  // Channel-originated threads, grouped by channel kind — same derivation
  // `ThreadTabStrip` computes for its own collapsed Channels tile (see
  // `resolveChannelThreadPartition`'s doc comment: it's meant to be called
  // independently by every surface that needs this grouping, HomeSidebar
  // already being a second one). Needed here too so the pinned
  // `ChannelsColumn` below has the same list to render without threading it
  // down through `ThreadTabStrip`'s own props.
  const unreadThreadIds = useChatStore((s) => s.unreadThreadIds);
  const channelPartition = useMemo(
    () => resolveChannelThreadPartition(agentId ?? "", threads, unreadThreadIds),
    [agentId, threads, unreadThreadIds],
  );
  const channelsColumnPinned = useChatStore((s) =>
    agentId ? s.channelsColumnPinnedByAgent.get(agentId) ?? false : false,
  );
  const setChannelsColumnPinned = useChatStore((s) => s.setChannelsColumnPinned);
  const showChannelsColumn = channelsColumnPinned && channelPartition.channelGroups.length > 0;

  // Assignment-originated threads, grouped by assignment — same derivation
  // `ThreadTabStrip` computes for its own collapsed Assignments tile (see
  // `resolveAssignmentThreadPartition`'s doc comment). Needed here too so
  // the pinned `AssignmentsColumn` below has the same list to render without
  // threading it down through `ThreadTabStrip`'s own props. `assignmentLookup`
  // resolves each group's `assignmentId` to its human name — same
  // construction HomeSidebar's own sidebar uses — from whichever assignments
  // `loadAssignments` has already populated for this agent (the same fetch
  // ChatHeader's assignment-count badge reads, triggered below).
  const assignmentsForAgent = useChatStore((s) =>
    agentId ? s.assignmentsByAgent.get(agentId) ?? EMPTY_ASSIGNMENTS : EMPTY_ASSIGNMENTS,
  );
  const assignmentLookup = useMemo(
    () => new Map(assignmentsForAgent.map((a) => [a.id, { name: a.name }])),
    [assignmentsForAgent],
  );
  const assignmentPartition = useMemo(
    () => resolveAssignmentThreadPartition(agentId ?? "", threads, unreadThreadIds, assignmentLookup),
    [agentId, threads, unreadThreadIds, assignmentLookup],
  );
  // Same "streaming right now" derivation ThreadTabStrip computes for its own
  // collapsed Assignments tile (see `resolveAssignmentStreamingByThreadId`'s
  // doc comment) — needed here too so the pinned `AssignmentsColumn` below
  // shows the exact same live badge, not just when the tile's popover happens
  // to be open.
  const assignmentStreamingByThreadId = useChatStore(
    useShallow((s) =>
      resolveAssignmentStreamingByThreadId(agentId ?? "", assignmentPartition.assignmentThreads, s.inFlightByAgent, s.runningDelegatesByThread),
    ),
  );
  const assignmentsColumnPinned = useChatStore((s) =>
    agentId ? s.assignmentsColumnPinnedByAgent.get(agentId) ?? false : false,
  );
  const setAssignmentsColumnPinned = useChatStore((s) => s.setAssignmentsColumnPinned);
  const showAssignmentsColumn = assignmentsColumnPinned && assignmentPartition.assignmentGroups.length > 0;

  // The active thread's own row, when loaded — carries `channel_origin` for
  // a channel that provisions one thread per *conversation* instead of one
  // per binding (Slack), which never populates `ChannelBinding.bridge_thread_id`
  // at all. See `getBridgeChannelKind`'s docstring.
  const activeThread = threads.find((t) => t.id === activeThreadId);

  // The `kind` of the enabled channel binding (Telegram, Discord, Email,
  // Slack, ...) whose dedicated bridge thread this is, or `null` on every
  // other thread. Replies typed here from the app never reach the external
  // channel: the outbound relay only records a reply target when the
  // inbound channel poll/socket loop delivers a message, so an app-typed
  // message on this thread would silently never relay. The composer
  // defaults to a read-only hint below — naming whichever channel actually
  // matched, not hardcoded to Telegram (see `getBridgeChannelKind`'s
  // docstring) — with a button to reveal the real composer anyway; see
  // `revealBridgeComposer`.
  const bridgeChannelKind = getBridgeChannelKind(
    selectedAgentProfile,
    activeThreadId,
    activeThread?.channel_origin,
  );
  const isBridgeThread = bridgeChannelKind !== null;
  const bridgeChannelLabel = bridgeChannelKind ? CHANNEL_KIND_LABELS[bridgeChannelKind] : "the channel";

  // Stable identity for "which conversation is showing" — agent + thread —
  // so switching either re-gates the reveal (below) and re-keys the list.
  const conversationKey = `${agentId}:${selectedThreadId ?? "default"}`;

  // Whether the operator clicked past the bridge-thread hint to reveal the
  // real composer anyway, to steer the agent directly even though the reply
  // won't relay back to the external channel. Re-arms per conversation (not
  // just per agent) so revealing it on one bridge thread doesn't leak into
  // the next thread switched to.
  const [revealBridgeComposer, setRevealBridgeComposer] = useState(false);
  useEffect(() => {
    setRevealBridgeComposer(false);
  }, [conversationKey]);

  // Re-arms per agent+thread (not just agent) so switching threads within the
  // same agent re-gates the reveal instead of showing the previous thread's
  // messages as a stand-in while the new one's history loads.
  const messagesReady = useReadyLatch(messages.length > 0, messagesLoading, conversationKey);

  // Defer ONLY the heavy MessageList remount behind the urgent frame paint.
  // Switching chats changes the URL param, which re-renders the sidebar
  // highlight, this view's header, and the composer in a single urgent commit.
  // Tearing down and rebuilding the virtualized MessageList (virtualizer
  // rebuild + mount-time scroll-pin loop) in that same commit is what made the
  // switch feel like it "waited for the content" before the sidebar updated.
  // Keying the list on a deferred copy of the conversation identity lets React
  // paint the shell first — sidebar highlight, header, input — then rebuild the
  // message list a beat later at transition priority. Rapid A→B→C switches
  // coalesce to the latest, skipping intermediate rebuilds.
  const deferredConversationKey = useDeferredValue(conversationKey);

  const draftKey = agentId ? threadDraftKey(agentId, activeThreadId, defaultThreadId) : undefined;

  // The focus path (which project/dir a message runs against) is scoped per
  // thread, not per agent — the same agent can have different threads pointed
  // at different projects at once. It shares the draft key on purpose: both
  // answer "which conversation am I composing into", and reusing the key means
  // the default/main thread keeps reading its historical bare-`agentId` focus
  // entry, so existing users' focus paths survive with zero migration. Every
  // other thread gets its own namespaced entry.
  const focusPath = useFocusPathStore((s) => (draftKey ? s.focusPaths[draftKey] ?? null : null));

  const currentDraft = useDraftStore((s) => s.drafts[draftKey ?? ""] ?? "");
  const currentDraftAttachments = useDraftStore((s) => s.draftAttachments[draftKey ?? ""] ?? EMPTY_DRAFT_ATTACHMENTS);
  const setDraft = useDraftStore((s) => s.setDraft);
  const setDraftAttachments = useDraftStore((s) => s.setDraftAttachments);
  const clearDraft = useDraftStore((s) => s.clearDraft);

  // Actually deliver a message to the currently active agent/thread —
  // wrapped below so a message sent while that thread is already streaming a
  // response gets queued instead of racing the in-flight run's own reply
  // into the transcript out of order.
  const deliverMessage = useCallback(
    (content: string, attachmentIds?: string[], attachments?: Attachment[]) => {
      if (draftKey) clearDraft(draftKey);
      sendMessage(content, attachmentIds, attachments, focusPath);
      if (agentId) markRead(agentId);
    },
    [draftKey, clearDraft, sendMessage, focusPath, agentId, markRead],
  );

  // ChatView deliberately stays mounted across agent/thread switches (see
  // useReadyLatch's docstring), so a queued message can't just be re-sent
  // once the identity changes — `deliverMessage` resolves the target thread
  // through ambient store state that's already moved on by then. Restore it
  // as an unsent draft on the conversation being left instead.
  const handleAbandonQueuedSend = useCallback(
    (prevKey: string, content: string) => {
      setDraft(prevKey, content);
    },
    [setDraft],
  );

  const { send: sendOrQueue, queue: queuedMessages, removeQueued } = useQueuedMessageSend(
    draftKey ?? `no-agent:${agentId ?? ""}`,
    isTurnActive,
    deliverMessage,
    handleAbandonQueuedSend,
  );

  // Click-to-expand state for the queued-messages preview below — collapsed
  // by default (matches ThreadsPanel's "Archived" section), and reset shut
  // whenever the queue itself empties out (flushed or abandoned) so it
  // doesn't linger open over an empty list for the next thing that gets
  // queued.
  const [queuedPanelOpen, setQueuedPanelOpen] = useState(false);
  useEffect(() => {
    if (queuedMessages.length === 0) setQueuedPanelOpen(false);
  }, [queuedMessages.length]);

  // The strip must never render without a "Main thread" pill — not even for
  // the one tick between selecting an agent and `loadThreads` resolving. The backend
  // guarantees every agent has a default thread (lazily materialized), so
  // until the real one arrives we stand in a placeholder with the same id
  // `activeThreadId` already falls back to; the moment `threads` loads and
  // contains its real "default" row, that replaces the placeholder outright.
  const displayThreads = useMemo<Thread[]>(() => {
    if (threads.some((t) => t.kind === "default")) return threads;
    const placeholder: Thread = {
      id: defaultThreadId,
      title: null,
      scope: { type: "AgentChat", agent_id: agentId ?? "" },
      transcript_path: "",
      kind: "default",
      created_at: "",
      updated_at: "",
    };
    return [placeholder, ...threads];
  }, [threads, defaultThreadId, agentId]);

  // Make a thread sticky and reload its transcript so the switch is
  // immediately visible in the conversation column.
  const handleSelectThread = useCallback(
    (threadId: string) => {
      if (!agentId) return;
      void switchToThread(agentId, threadId);
    },
    [agentId],
  );

  const creatingThreadRef = useRef(false);
  const handleCreateThread = useCallback(async () => {
    if (!agentId || creatingThreadRef.current) return;
    creatingThreadRef.current = true;
    try {
      const thread = await createFreshThread(agentId);
      await switchToThread(agentId, thread.id);
    } finally {
      creatingThreadRef.current = false;
    }
  }, [agentId, createFreshThread]);

  // Archive a thread pill — persists server-side (chatStore's
  // `archiveThread`) and hides it from every surface (this strip, the
  // overflow panel, ThreadsPanel's main list, Home) until it's unarchived
  // from ThreadsPanel's "Archived" section; the thread and its history are
  // otherwise untouched. If the archived thread was active, fall back to the
  // default thread so the Chat tab doesn't keep pointing at a pill that just
  // disappeared.
  const handleArchiveThread = useCallback(
    (threadId: string) => {
      if (!agentId) return;
      const wasActive = activeThreadId === threadId;
      void archiveThread(threadId);
      if (wasActive) void switchToThread(agentId, defaultThreadId);
    },
    [agentId, activeThreadId, archiveThread, defaultThreadId],
  );

  // Restore an archived thread AND focus it — reached only from the overflow
  // panel's "Archived" tab. Clicking an archived row is meant to feel like
  // clicking an active one (which both switches to it and loads its
  // transcript via `switchToThread`), so this chains unarchive ->
  // switchToThread rather than stopping at the plain "unarchive, then
  // separately go find it" flow `ThreadsPanel`'s own `handleUnarchive` still
  // uses (that sidebar panel isn't even mounted today — see its own comment).
  const handleUnarchiveThread = useCallback(
    async (threadId: string) => {
      await unarchiveThread(threadId);
      if (agentId) await switchToThread(agentId, threadId);
    },
    [agentId, unarchiveThread],
  );

  // Permanently delete a thread — reached only from the overflow panel's
  // bulk-delete (there's no per-pill delete control). Unlike closing a tab,
  // this actually removes the thread server-side, so its draft is cleared
  // too. If the deleted thread was active, fall back to the default thread.
  const handleDeleteThread = useCallback(
    async (threadId: string) => {
      if (!agentId) return;
      const wasActive = activeThreadId === threadId;
      await deleteThread(threadId);
      const goneKey = threadDraftKey(agentId, threadId, defaultThreadId);
      clearDraft(goneKey);
      // Focus paths share the draft key (see the focusPath read above), so the
      // deleted thread's persisted focus entry must be dropped here too — else
      // it lingers in localStorage forever after its thread is gone.
      clearFocusPath(goneKey);
      if (wasActive) await switchToThread(agentId, defaultThreadId);
    },
    [agentId, activeThreadId, deleteThread, clearDraft, clearFocusPath, defaultThreadId],
  );

  const resetBookmarks = useBookmarkStore((s) => s.reset);
  const fetchBookmarks = useBookmarkStore((s) => s.fetchBookmarks);
  const [isResizing, setIsResizing] = useState(false);
  const isInternetOnline = useNetworkStore((s) => s.isInternetOnline);
  const isServerOnline = useNetworkStore((s) => s.isServerOnline);
  // Block sending when either the internet or the backend is unreachable. A
  // message sent while the server is down would hit a dead endpoint and be
  // lost (never persisted), so the composer must refuse it the same way it
  // refuses an offline send.
  const canSend = isInternetOnline && isServerOnline;
  const panelRef = useRef<HTMLDivElement>(null);
  const resizingRef = useRef(false);

  const activePanel = useChatStore((s) => (agentId ? s.activePanelByAgent.get(agentId) : undefined)) ?? null;
  const anyPanelOpen = activePanel !== null;
  const memoryOpen = activePanel === 'memory';
  const bookmarkOpen = activePanel === 'bookmark';
  const assetsOpen = activePanel === 'assets';
  const todosOpen = activePanel === 'todos';

  const hydrateTasklist = useAgentTasklistStore((s) => s.hydrate);
  useEffect(() => {
    if (!agentId) return;
    const entry = useAgentTasklistStore.getState().byAgent.get(agentId);
    if (!entry) void hydrateTasklist(agentId);
  }, [agentId, hydrateTasklist]);

  const agentTasklistEntry = useAgentTasklistsForAgent(agentId ?? null);
  const inProgressCount = countInProgress(agentTasklistEntry.active);
  // The header kebab indicator only reflects a live (active/paused) list; the
  // store clears `active` to null once a list reaches a terminal state.
  const activeTodoStatus =
    agentTasklistEntry.active?.status === "active"
      ? "active"
      : agentTasklistEntry.active?.status === "paused"
        ? "paused"
        : null;

  const startResizing = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    resizingRef.current = true;
    setIsResizing(true);
  }, []);

  const stopResizing = useCallback(() => {
    if (resizingRef.current && panelRef.current) {
      // Commit final width to state on mouseup
      const finalWidth = panelRef.current.offsetWidth;
      setMemoryPanelWidth(finalWidth);
    }
    resizingRef.current = false;
    setIsResizing(false);
  }, [setMemoryPanelWidth]);

  const resize = useCallback(
    (e: MouseEvent) => {
      if (!resizingRef.current || !panelRef.current) return;
      const newWidth = window.innerWidth - e.clientX - 12;
      if (newWidth >= 240 && newWidth <= 800) {
        // Update DOM directly — no React re-render, no virtualizer thrash
        panelRef.current.style.width = `${newWidth}px`;
      }
    },
    []
  );

  useEffect(() => {
    if (isResizing) {
      window.addEventListener("mousemove", resize);
      window.addEventListener("mouseup", stopResizing);
      document.body.style.cursor = "col-resize";
    } else {
      window.removeEventListener("mousemove", resize);
      window.removeEventListener("mouseup", stopResizing);
      document.body.style.cursor = "default";
    }
    return () => {
      window.removeEventListener("mousemove", resize);
      window.removeEventListener("mouseup", stopResizing);
      document.body.style.cursor = "default";
    };
  }, [isResizing, resize, stopResizing]);

  // Fetch agent profile + messages when agentId changes
  useEffect(() => {
    if (agentId) {
      selectAgent(agentId);
      markRead(agentId);
      fetchBookmarks(agentId);
      // Refresh snapshot list to pick up pending_forms and other runtime state
      fetchAgents();
      // Load the agent's threads (lazily ensures the default) so the tab
      // strip renders immediately for the newly-opened agent.
      void loadThreads(agentId);
      // Eagerly load assignments so the header's Assignments badge count is
      // accurate before the modal is first opened.
      void loadAssignments(agentId);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentId]);

  // Eagerly precompute context when chat opens or focus path changes
  useEffect(() => {
    if (agentId) {
      precomputeContext(agentId, focusPath);
    }
  }, [agentId, focusPath]);

  // Reset bookmarks on unmount, but do NOT reset the chat store — selectAgent
  // already overwrites all active state, and resetting here causes a flash of
  // defaults when navigating back. Panel open/closed state also intentionally
  // survives unmount now: it lives in chatStore's `activePanelByAgent`, keyed
  // per agent, so each chat recalls its own panel next time it's viewed
  // instead of always resetting to closed.
  useEffect(() => {
    return () => {
      resetBookmarks();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // If the user is actively viewing this thread, mark any incoming agent
  // message as read using the message timestamp to avoid client/server race.
  useEffect(() => {
    if (!agentId || messages.length === 0) return;
    const lastMessage = messages[messages.length - 1];
    if (isAgentMessage(lastMessage)) {
      markRead(agentId, lastMessage.ts);
    }
  }, [agentId, messages, markRead]);

  // This is the one place that knows "the user is now actually looking at
  // (agentId, streamingThreadId)" — clear its unread marker (see
  // `finalizeInFlightText`/`unreadThreadIds`) whenever that pair changes, so
  // ThreadTabStrip's dot disappears the moment the thread is opened rather
  // than needing an explicit action wired through every navigation path.
  useEffect(() => {
    if (!agentId) return;
    markThreadViewed(agentId, streamingThreadId);
  }, [agentId, streamingThreadId, markThreadViewed]);

  // No agent selected — empty state
  if (!agentId) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center text-[var(--text-secondary)] text-[13px] gap-2">
        <img src={messageBubble} alt="" className="w-[216px] h-[216px] object-contain select-none" draggable={false} />
        Select an agent to start chatting
      </div>
    );
  }

  const hasMessages = messages.length > 0;

  // A hidden skill-load message at the tail (no visible message after it)
  // causes MessageList to render a streaming bubble with a prefix chip, even
  // when `streamingText` is empty. The bubble's own typing dots signal state,
  // so suppress the warmup pill to avoid a duplicate indicator.
  let hasTrailingSkillLoad = false;
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (!m.hidden_from_user) break;
    if (parseSkillLoadInfo(m)) { hasTrailingSkillLoad = true; break; }
  }

  return (
    <div className="flex flex-1 flex-row min-h-0 relative">
        {/* Pinned Channels column — appears once the operator pins the
            Channels tile's popover (see ThreadTabStrip/ChannelsTilePanel's
            "Pin" button), so switching between channel conversations no
            longer costs a popover-reopen on every switch. Sits to the left of
            the chat area, ahead of everything else in the row. */}
        {agentId && showChannelsColumn && (
          <ChannelsColumn
            channelGroups={channelPartition.channelGroups}
            activeThreadId={activeThreadId}
            onSelectThread={handleSelectThread}
            onArchiveThread={handleArchiveThread}
            onRenameThread={renameThread}
            onUnpin={() => setChannelsColumnPinned(agentId, false)}
          />
        )}

        {/* Pinned Assignments column — same pin/unpin flow as the Channels
            column above, from the Assignments tile's own popover (see
            ThreadTabStrip/AssignmentsTilePanel's "Pin" button). Rendered
            alongside it (not exclusive with it) — an agent can have either,
            both, or neither pinned at once. */}
        {agentId && showAssignmentsColumn && (
          <AssignmentsColumn
            assignmentGroups={assignmentPartition.assignmentGroups}
            activeThreadId={activeThreadId}
            onSelectThread={handleSelectThread}
            onArchiveThread={handleArchiveThread}
            onRenameThread={renameThread}
            streamingByThreadId={assignmentStreamingByThreadId}
            onUnpin={() => setAssignmentsColumnPinned(agentId, false)}
          />
        )}

        {/* Chat area */}
        <div className="@container flex flex-col min-h-0 min-w-0 relative flex-1">
          {/* Agent profile header — stays in normal flow with its own bottom
              border. The tab strip no longer merges into it; it floats over the
              content below instead. */}
          <ChatHeader
            memoryOpen={memoryOpen}
            onToggleMemory={() => setChatPanel(agentId, memoryOpen ? null : 'memory')}
            bookmarkOpen={bookmarkOpen}
            onToggleBookmark={() => setChatPanel(agentId, bookmarkOpen ? null : 'bookmark')}
            assetsOpen={assetsOpen}
            onToggleAssets={() => setChatPanel(agentId, assetsOpen ? null : 'assets')}
            todosOpen={todosOpen}
            onToggleTodos={() => setChatPanel(agentId, todosOpen ? null : 'todos')}
            todosInProgressCount={inProgressCount}
            todoStatus={activeTodoStatus}
            focusPath={focusPath}
          />

          {/* Content region — the tab strip is absolutely positioned over the
              top of this so conversation bubbles render beneath the frosted
              bar and scroll under it. */}
          <div className="relative flex-1 min-h-0 flex flex-col">
            {/* Floating frosted tab strip — one pill per thread (default
                thread first, labelled "Main thread") plus a trailing `+` to
                create a new one. z-40 keeps the tabs above the pinned bookmark/search
                overlays (z-30) so navigation stays reachable. The bar is
                click-through; only its pills capture pointer events. */}
            <div className="absolute top-0 inset-x-0 z-40 pointer-events-none">
              <ThreadTabStrip
                agentId={agentId}
                threads={displayThreads}
                activeThreadId={activeThreadId}
                onSelectThread={handleSelectThread}
                onCreateThread={handleCreateThread}
                onArchiveThread={handleArchiveThread}
                onDeleteThread={handleDeleteThread}
                onRenameThread={renameThread}
                onUnarchiveThread={handleUnarchiveThread}
              />
            </div>

            {/* Messages + input wrapper — relative for bookmark overlay, and
                the query container the form overlays below size themselves
                against (`[container-type:size]`) so their `cqh` max-height
                measures THIS pane, not the viewport. Its own block size is
                already definite via `flex-1 min-h-0` in the column above, so
                containment doesn't collapse it. The message list adds its
                own top padding so bubbles start below the floating strip
                while still scrolling under it. */}
            <div className="flex-1 min-h-0 flex flex-col relative [container-type:size]">
            {/* Messages — scrollable middle. ContentGate holds this blank (then a
                skeleton, on a slow load) until messagesReady latches true, so the
                thread never flashes an empty state before its history has
                actually loaded. */}
            <ContentGate ready={messagesReady} skeleton={<MessageThreadSkeleton />} className="flex-1 min-h-0 flex flex-col">
              {hasMessages ? (
                <MessageList key={deferredConversationKey} />
              ) : (
                selectedAgentProfile && (
                  <div className="flex flex-1 flex-col items-center justify-center text-[var(--text-secondary)] text-[13px] gap-2">
                    <img src={messageBubble} alt="" className="w-[216px] h-[216px] object-contain select-none" draggable={false} />
                    Send a message to start the conversation
                  </div>
                )
              )}
            </ContentGate>

            {/* Warmup typing indicator — floats above input while the run is
           *  active but no content has arrived yet (no tokens, no tools, and
           *  no trailing skill-load chip that would render its own bubble). */}
            {isTyping && !streamingText && activeToolCalls.length === 0 && !hasTrailingSkillLoad && (
              <div className="relative">
                <div className="absolute bottom-full left-0 right-0 px-[10px] @sm:px-[16px] @lg:px-[20px] pointer-events-none">
                  <TypingIndicator emoji={selectedAgentProfile?.emoji ?? "🤖"} />
                </div>
              </div>
            )}

            {/* Floating typing indicator — shown when streaming content or a
           *  tool call is active but the bubble is scrolled out of view.
           *  Warmup (isTyping with no content) is handled by the pill above
           *  the input, which is always visible regardless of scroll. */}
            <AnimatePresence>
              {(streamingText || activeToolCalls.length > 0) && !isStreamingAreaVisible && (
                <motion.div
                  key="floating-typing-indicator"
                  initial={{ opacity: 0, y: 8 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, y: 8 }}
                  transition={{ duration: 0.2, ease: "easeOut" }}
                  className="relative pointer-events-none"
                >
                  <div className="absolute bottom-full left-0 right-0 px-[10px] @sm:px-[16px] @lg:px-[20px] pb-[4px]">
                    <div className="inline-flex items-center gap-[8px] px-[10px] py-[6px] rounded-full bg-[var(--bg-secondary)] shadow-sm">
                      <span className="text-[20px] leading-none select-none">
                        {selectedAgentProfile?.emoji ?? "🤖"}
                      </span>
                      <div className="flex items-center gap-[4px]">
                        <span className="text-[12px] text-[var(--text-secondary)]">is responding</span>
                        <div className="flex items-center gap-[3px] pt-[2px]">
                          {[0, 1, 2].map((i) => (
                            <motion.span
                              key={i}
                              className="block w-[5px] h-[5px] rounded-full bg-[var(--text-secondary)]"
                              animate={{ y: [0, -4, 0] }}
                              transition={{
                                duration: 0.6,
                                repeat: Infinity,
                                delay: i * 0.2,
                                ease: "easeInOut",
                              }}
                            />
                          ))}
                        </div>
                      </div>
                    </div>
                  </div>
                </motion.div>
              )}
            </AnimatePresence>

            {/* Async-delegate activity row — one pill per `Delegate` tool
           *  currently running in the background for THIS thread. Distinct
           *  from the typing indicators above: those cover the parent
           *  agent's own turn (which may already be idle again), this
           *  covers subagents still running behind the scenes. Normal flow,
           *  not floated, since it can wrap to more than one line once a
           *  few delegates are active — see `DelegatePillRow`'s own doc
           *  comment for the pill lifecycle. */}
            {agentId && <DelegatePillRow agentId={agentId} threadId={streamingThreadId} />}

            {/* Async form pinned nudge — RECONSIDERED, now blocking, same as
             *  the sync form handled below. Originally this card sat in
             *  normal flow above a still-live `ChatInput` on the reasoning
             *  that a `mode: "async"` form never suspends the agent's turn,
             *  so it had nothing to gate the composer over. That's been
             *  deliberately overturned: the owner-locked invariant is
             *  exactly ONE form slot per thread, sync and async sharing it,
             *  both blocking the composer — because only the main agent
             *  ever posts a form, blocking can never gag the operator
             *  toward some other agent, and async form cards now carry the
             *  same Cancel/Regenerate/Something-else exit affordances the
             *  sync overlay does (see `AsyncFormRequestCard`'s `onAction`),
             *  so blocking no longer risks a lockout with no way out. This
             *  card itself still renders here, in normal flow just above
             *  the input — only `formSlotOccupied` (below) actually hides
             *  `ChatInput` while it's pending. */}
            {!pendingForm && pendingAsyncFormMeta && agentId && pendingAsyncFormId && (
              // Floats over the composer slot exactly like the sync overlay
              // below (`absolute inset-x-0 bottom-0 z-50`) instead of
              // sitting in normal flow — an unbounded-height card here used
              // to compete with MessageList for space and push the thread
              // (and the agent-name header) off the top of the pane. Unlike
              // the sync overlay, this wrapper has no `formMinimized`
              // visibility toggle: its own ternary below always renders
              // something (the mini bar or the full card), so both states
              // stay visible while floating rather than one being hidden in
              // favor of a bar rendered elsewhere.
              <div className="absolute inset-x-0 bottom-0 z-50 px-[10px] @sm:px-[16px] @lg:px-[20px] pb-[8px]">
                {formMinimized ? (
                  <MinimizedFormBar
                    title={formatPendingFormWaitingLabel(selectedAgentProfile?.name, pendingAsyncFormMeta.spec.fields.length)}
                    onExpand={() => setFormMinimized(agentId, streamingThreadId, false)}
                  />
                ) : (
                  <AsyncFormRequestCard
                    // Same remount-on-supersession reasoning as the sync
                    // overlay's `key={pendingForm.form_id}` above — an async
                    // form replacing another async form on this slot is the
                    // same newest-wins swap, just without an
                    // `AnimatePresence` wrapper to hang a key off of
                    // implicitly. Without an explicit key here this render
                    // slot's position alone was the (static) implicit key,
                    // so the identical cross-form answer-state bleed applied.
                    key={pendingAsyncFormMeta.form_id}
                    meta={pendingAsyncFormMeta}
                    agentId={agentId}
                    onSubmit={async (values) => {
                      await submitAsyncFormAnswer(agentId, pendingAsyncFormId, values);
                      clearPendingAsyncForm(agentId, pendingAsyncFormId);
                      // Mirror the sync form's optimistic append just below
                      // (`handleFormSubmit` -> `addFormAnswerEntry`) — an
                      // async answer's own `FormResolved` SSE push carries
                      // only `form_id`, no values/spec, so there's nothing
                      // for a live event handler to render from. `values`
                      // (just submitted) and `pendingAsyncFormMeta.spec`
                      // (still in scope, captured before the pending pointer
                      // was cleared above) are everything needed to build
                      // the same shape locally instead of leaving the bubble
                      // to show up only after the next full transcript
                      // reload (see `addAsyncFormAnswerEntry`'s doc comment
                      // in chatStore.ts for the full reasoning, including
                      // why this can never produce a duplicate). Only
                      // reached on a successful submit — on failure this
                      // throws before either call above runs, so no
                      // optimistic entry is ever added for an answer that
                      // didn't persist, and `AskUserQuestionForm` re-enables
                      // the form for retry.
                      addAsyncFormAnswerEntry(agentId, {
                        form_id: pendingAsyncFormId,
                        threadId: streamingThreadId,
                        values,
                        spec: pendingAsyncFormMeta.spec,
                      });
                    }}
                    onAction={handleFormAction}
                    onMinimize={() => setFormMinimized(agentId, streamingThreadId, true)}
                    onWithdraw={handleFormWithdraw}
                  />
                )}
              </div>
            )}

            {/* Input — fixed at bottom */}
            <div className="px-[10px] @sm:px-[16px] @lg:px-[20px] pb-[16px] pt-[8px] flex-shrink-0">
              {/* Queued-message notice — a message sent while the agent was
             *  still responding is held rather than fired off immediately
             *  (see useQueuedMessageSend). Clicking it expands a preview of
             *  what's actually queued (content + attachment count per part,
             *  each removable) instead of leaving you to remember what you
             *  typed — the bare count alone used to be the only surfacing
             *  that anything survived. */}
              {queuedMessages.length > 0 && (
                <div className="pb-[6px]">
                  <button
                    type="button"
                    onClick={() => setQueuedPanelOpen((open) => !open)}
                    data-testid="queued-messages-toggle"
                    aria-expanded={queuedPanelOpen}
                    className="w-full flex items-center gap-[6px] px-[4px] py-[2px] text-[12px] text-[var(--text-secondary)] hover:text-[var(--text-primary)] cursor-pointer transition-colors"
                  >
                    <Clock className="w-[12px] h-[12px] flex-shrink-0" />
                    <span className="flex-1 min-w-0 text-left truncate">
                      {queuedMessages.length === 1 ? "Message queued" : `${queuedMessages.length} messages queued`} — sending once{" "}
                      {selectedAgentProfile?.name ?? "the agent"} finishes responding
                    </span>
                    <ChevronDown
                      className={`w-[12px] h-[12px] flex-shrink-0 transition-transform ${queuedPanelOpen ? "rotate-180" : ""}`}
                    />
                  </button>

                  <AnimatePresence initial={false}>
                    {queuedPanelOpen && (
                      <motion.div
                        initial={{ opacity: 0, height: 0 }}
                        animate={{ opacity: 1, height: "auto" }}
                        exit={{ opacity: 0, height: 0 }}
                        transition={{ duration: 0.14, ease: "easeOut" }}
                        className="overflow-hidden"
                      >
                        <div
                          data-testid="queued-messages-list"
                          className="mt-[4px] rounded-[10px] border border-[var(--border-secondary)] bg-[var(--bg-secondary)] max-h-[160px] overflow-y-auto"
                        >
                          {queuedMessages.map((part, index) => {
                            const attachmentCount = part.attachmentIds?.length ?? 0;
                            return (
                              <div
                                key={index}
                                data-testid={`queued-message-${index}`}
                                className="group/queued flex items-start gap-[8px] px-[10px] py-[8px] border-b last:border-b-0 border-[var(--border-secondary)]"
                              >
                                <span className="flex-1 min-w-0 text-[12px] text-[var(--text-primary)] whitespace-pre-wrap break-words line-clamp-3">
                                  {part.content.trim().length > 0 ? part.content : "(empty message)"}
                                </span>
                                {attachmentCount > 0 && (
                                  <span className="shrink-0 pt-[1px] text-[10px] text-[var(--text-tertiary)]">
                                    {attachmentCount} file{attachmentCount === 1 ? "" : "s"}
                                  </span>
                                )}
                                <button
                                  type="button"
                                  onClick={() => removeQueued(index)}
                                  data-testid={`queued-message-remove-${index}`}
                                  aria-label="Remove queued message"
                                  className="shrink-0 w-[18px] h-[18px] rounded-[4px] flex items-center justify-center text-[var(--text-tertiary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] cursor-pointer opacity-0 group-hover/queued:opacity-100 transition-opacity"
                                >
                                  <X className="w-[11px] h-[11px]" />
                                </button>
                              </div>
                            );
                          })}
                        </div>
                      </motion.div>
                    )}
                  </AnimatePresence>
                </div>
              )}
              {formSlotOccupied ? (
                // The single form slot is occupied — sync or async, both
                // block the composer the same way now (see
                // `formSlotOccupied`'s docstring above). A SYNC form's
                // minimized bar has no home of its own, so it surfaces
                // right here, in the composer's spot, while expanded it's
                // the floating overlay below that owns this area instead. An
                // ASYNC form's minimized bar already renders in the "Async
                // form pinned nudge" block just above this container — so
                // this branch stays empty for it; rendering a second bar
                // here would duplicate it.
                pendingForm &&
                  formMinimized && (
                    <MinimizedFormBar
                      title={formatPendingFormWaitingLabel(selectedAgentProfile?.name, pendingForm.fields.length)}
                      onExpand={() => setFormMinimized(agentId, streamingThreadId, false)}
                    />
                  )
              ) : (
                isBridgeThread && !revealBridgeComposer ? (
                  <div
                    data-testid="channel-bridge-hint"
                    className="flex items-center gap-2 min-h-[50px] px-3 py-2.5 rounded-[14px] border border-[var(--border-secondary)] bg-[var(--bg-secondary)] text-[13px] text-[var(--text-secondary)]"
                  >
                    <span className="flex-1 text-center select-none">
                      This thread mirrors your {bridgeChannelLabel} conversation — reply there, or type here instead.
                    </span>
                    <button
                      type="button"
                      onClick={() => setRevealBridgeComposer(true)}
                      data-testid="channel-bridge-reveal-btn"
                      aria-label={`Type here instead of ${bridgeChannelLabel}`}
                      title="Type here instead"
                      className="shrink-0 w-[26px] h-[26px] rounded-[8px] flex items-center justify-center text-[var(--text-tertiary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] cursor-pointer transition-colors"
                    >
                      <Pencil className="w-[13px] h-[13px]" />
                    </button>
                  </div>
                ) : (
                  <>
                    {isBridgeThread && (
                      <div
                        data-testid="channel-bridge-composer-notice"
                        className="flex items-center justify-between gap-2 px-[10px] py-[6px] mb-[6px] rounded-[10px] border border-[var(--border-secondary)] bg-[var(--bg-secondary)] text-[11px] text-[var(--text-tertiary)]"
                      >
                        <span>
                          Typing here won't reach {bridgeChannelLabel} — you're steering the agent directly.
                        </span>
                        <button
                          type="button"
                          onClick={() => setRevealBridgeComposer(false)}
                          data-testid="channel-bridge-hide-btn"
                          aria-label="Back to the read-only hint"
                          className="shrink-0 text-[var(--text-tertiary)] hover:text-[var(--text-primary)] cursor-pointer"
                        >
                          <X className="w-[11px] h-[11px]" />
                        </button>
                      </div>
                    )}
                    <ChatInput
                      conversationId={draftKey}
                      agentId={agentId}
                      focusStoreKey={draftKey}
                      fileCapabilitiesSupported={fileCapabilitiesSupported}
                      disabled={!canSend}
                      isProcessing={isTyping}
                      onStop={cancelRun}
                      onSend={sendOrQueue}
                      placeholder="Send a message..."
                      initialDraft={currentDraft}
                      initialDraftAttachments={currentDraftAttachments.length > 0 ? currentDraftAttachments : undefined}
                      onUnmount={(text, html, id) => {
                        if (id && text.trim()) setDraft(id, text, html);
                        else if (id) clearDraft(id);
                      }}
                      onUnmountAttachments={(attachments, id) => {
                        if (id) setDraftAttachments(id, attachments);
                      }}
                    />
                  </>
                )
              )}
            </div>

            {/* Sync form overlay — floats above the message list, replacing
             *  ChatInput. Sync forms only (`AskUserQuestionWithForm` tool →
             *  `pendingForm`): the agent's turn is genuinely suspended
             *  awaiting this answer, so hard-blocking the composer is
             *  correct here. z-50 — above the thread tab strip (z-40) and
             *  bookmark/search overlays (z-30) — since an operator-blocking
             *  question takes precedence over chrome that merely floats
             *  above the message list. Async forms (`pendingAsyncFormMeta`)
             *  now block the composer too (see `formSlotOccupied` and the
             *  "Async form pinned nudge" block's comment for why) — they
             *  just don't route through THIS particular overlay component,
             *  keeping their own in-flow rendering above the composer
             *  instead of this absolutely-positioned one. */}
            <AnimatePresence>
              {pendingForm && (
                <motion.div
                  // Stable — this is `AnimatePresence`'s own presence
                  // identity for "a sync form is pending at all", so the
                  // slide-in/out animation plays once per appear/disappear
                  // of the overlay, not on every form-to-form swap (that
                  // would key AnimatePresence itself into treating the swap
                  // as an exit+enter, leaving both the old and new forms in
                  // the DOM together for the transition's duration). The
                  // remount that actually needs to happen per form_id lives
                  // one level down, on `AskUserQuestionForm` itself — see
                  // its `key` below.
                  key="form-overlay"
                  initial={{ opacity: 0, y: 8 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, y: 8 }}
                  transition={{ duration: 0.18, ease: "easeOut" }}
                  // Minimizing must hide, never unmount — `AskUserQuestionForm`
                  // keeps in-progress answers in local component state, so
                  // gating this out of the tree (or off `AnimatePresence`)
                  // would silently destroy whatever the operator already
                  // typed. `hidden` only toggles display; the form below
                  // stays mounted the whole time.
                  className={`absolute inset-x-0 bottom-0 z-50 px-[10px] @sm:px-[16px] @lg:px-[20px] pb-[16px] pt-[8px] ${formMinimized ? "hidden" : ""}`}
                >
                  <AskUserQuestionForm
                    // Keyed by the form's own id — NOT a static string — so
                    // a newer sync form superseding a still-pending one on
                    // this same (newest-wins) slot forces a full remount
                    // instead of React reusing this instance.
                    // `AskUserQuestionForm` seeds its answer state via lazy
                    // `useState` initializers that only run on first mount;
                    // reusing the instance across a form swap would
                    // otherwise leave the new form rendering with the old
                    // one's typed answers still in state (or, worse,
                    // submittable as answers to fields they were never
                    // written for, whenever the two forms' field ids
                    // collide). Ordinary React key — not tracked by
                    // `AnimatePresence` — so this remount is synchronous,
                    // with no exit animation and no old/new coexistence
                    // period. See `handleFormWithdraw`/`onWithdraw` for what
                    // happens to the old draft when this remount fires.
                    key={pendingForm.form_id}
                    agentId={agentId}
                    form={pendingForm}
                    onSubmit={handleFormSubmit}
                    onAction={handleFormAction}
                    onMinimize={() => setFormMinimized(agentId, streamingThreadId, true)}
                    onWithdraw={handleFormWithdraw}
                  />
                </motion.div>
              )}
            </AnimatePresence>

            {/* Pinned bookmark overlay — covers messages + input */}
            <PinnedBookmarkOverlay />
            {/* Pinned search result overlay — covers messages + input */}
            <PinnedSearchOverlay />
            </div>
          </div>
        </div>

        {/* Side panel — always rendered, width animates between 0 and target so the
           chat area resizes gradually and the virtualizer's per-element ResizeObservers
           can keep up without causing overlap or flicker. */}
        {agentId && (
          <div
            ref={panelRef}
            className="flex flex-col overflow-hidden flex-shrink-0 relative"
            style={{
              width: anyPanelOpen ? memoryPanelWidth : 0,
            }}
          >
            {/* Resizer Handle — only interactive when panel is open */}
            {anyPanelOpen && (
              <div
                className="absolute left-[-4px] top-0 bottom-0 w-[8px] cursor-col-resize z-30 group"
                onMouseDown={startResizing}
              >
                <div className="absolute inset-y-0 left-1/2 w-[1px] bg-transparent group-hover:bg-[var(--accent)] transition-colors" />
              </div>
            )}

            {/* Panel content — rendered directly inside the border-l container,
               no inner card, so the content is flush with the panel edges. */}
            <div className="flex-1 relative overflow-hidden border-l border-[var(--border-secondary)] flex flex-col">
              <div className="flex items-center justify-between px-[16px] py-[12px] min-h-[65px] border-b border-[var(--border-secondary)] shrink-0">
                <div className="flex items-center gap-2">
                  {activePanel === 'memory' && <StickyNote className="w-[16px] h-[16px] text-[var(--text-primary)]" />}
                  {activePanel === 'bookmark' && <Bookmark className="w-[16px] h-[16px] text-[var(--text-primary)]" />}
                  {activePanel === 'assets' && <Paperclip className="w-[16px] h-[16px] text-[var(--text-primary)]" />}
                  {activePanel === 'todos' && <ListTodo className="w-[16px] h-[16px] text-[var(--text-primary)]" />}
                  <span className="text-[14px] font-bold text-[var(--text-primary)]">
                    {activePanel === 'memory' ? 'Memories' : activePanel === 'bookmark' ? 'Bookmarks' : activePanel === 'assets' ? 'Assets' : 'Todos'}
                  </span>
                </div>
                <button
                  onClick={() => setChatPanel(agentId, null)}
                  className="w-[28px] h-[28px] rounded-[8px] flex items-center justify-center text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] transition-colors cursor-pointer"
                >
                  <X className="w-[14px] h-[14px]" />
                </button>
              </div>
              {activePanel === 'memory' && <MemoryPanel agentId={agentId} threadId={activeThreadId} />}
              {activePanel === 'bookmark' && <BookmarkPanel agentId={agentId} agentName={selectedAgentProfile?.name} />}
              {activePanel === 'assets' && <AssetsPanel agentId={agentId} />}
              {activePanel === 'todos' && <TodoPanel agentId={agentId} />}
            </div>
          </div>
        )}
      </div>
  );
}
