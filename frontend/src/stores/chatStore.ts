import { useMemo } from "react";
import { create } from "zustand";
import {
  AgentSnapshot,
  AgentProfile,
  TranscriptEntry,
  CacheEntry,
  PaginationCursor,
  Attachment,
  Thread,
  Assignment,
  AssignmentRun,
  PendingForm,
  isUserMessage,
  isAgentMessage,
} from "../types/api";
import type { FormRequestPayload, FormAnswerMap, FormAction, WithdrawnFormDraftEntry, AsyncFormSpec } from "../types/form";
import * as api from "../lib/api";
import type { CreateAssignmentRequest, PatchAssignmentRequest, TriggerAssignmentRequest } from "../lib/api";
import { useReadStatusStore } from "./readStatusStore";
import { useArtifactStore } from "./artifactStore";
export type { AgentProfile };

const PAGE_SIZE = 50;
/** Upper bound on `messageCache` entries, enforced by `evictLRU`. Originally
 *  chosen back when the cache held exactly one entry per agent (20 warm
 *  agents' worth of transcripts). Now that entries are keyed per-THREAD (see
 *  `messageCache`'s doc comment), a single agent with several open threads
 *  can occupy multiple slots, so the old cap would cover fewer distinct
 *  agents than before and could evict a just-visited sibling thread of the
 *  agent the user is actively switching between. Bumped 2x to keep roughly
 *  the same number of warm AGENTS in the common case (most agents have one
 *  or a handful of threads) without letting the cache grow unbounded. */
export const MAX_CACHED_CONVERSATIONS = 40;

/** Ms to wait after run_ended before tearing down an agent's in-flight entry.
 *  A skill-load handoff queues a follow-up user message and starts a new run
 *  for the same agent shortly after; debouncing the delete keeps the streaming
 *  bubble visually continuous across that boundary. */
const IN_FLIGHT_TEARDOWN_DELAY_MS = 400;

/** Upper bound the optimistic typing indicator can spin before we assume the
 *  run never actually started. `sendMessage` shows the dots the instant a
 *  message is posted (rather than waiting for the server's RunStarted to
 *  round-trip back over the shared SSE stream), so on a dropped/reconnecting
 *  connection that never replays RunStarted the dots would otherwise hang
 *  forever. When this fires we only retract the indicator if the entry is
 *  still in its untouched optimistic state — a real run that has produced any
 *  text, tool call, or thinking activity is left alone. */
const OPTIMISTIC_TYPING_WATCHDOG_MS = 20000;

/** ChatView's four mutually-exclusive side panels. See `activePanelByAgent`. */
export type ChatSidePanel = "memory" | "bookmark" | "assets" | "todos";

export type ActiveToolCall = {
  tool: string;
  input?: Record<string, unknown>;
  startedAt: number;
  action_id?: string;
  label?: string;
  /** Set once a *classic* (native tool-calling, no `action_id`) chip's
   *  `tool_call_completed` has fired. The chip is intentionally NOT removed
   *  at that point (see `markInFlightToolCallDone`) — it keeps rendering
   *  identically to an active chip until `text_delta`/finalize/run_ended
   *  flushes the whole classic stack, so the bubble doesn't shrink-then-regrow
   *  between one tool finishing and the next starting. Unused for
   *  `action_id`-keyed chips, which still self-remove on their own
   *  completion event. */
  done?: boolean;
};

/** State for an agent's current run — unifies the typing indicator, streaming
 *  text buffer, and active tool-call chips into a single per-agent entry. The
 *  entry outlives individual `RunEnded`/`RunStarted` transitions (see
 *  IN_FLIGHT_TEARDOWN_DELAY_MS) so skill-load follow-ups read as one
 *  continuous bubble rather than flashing empty between runs. */
export interface InFlightAgentMessage {
  textBuffer: string;
  activeToolCalls: ActiveToolCall[];
  isTyping: boolean;
  startedAt: number;
  /** Thinking-channel state for the current turn. Kept inline rather than in a
   *  parallel map so the streaming bubble can read everything it needs from a
   *  single selector. `thinkingActive` flips to true on `thinking_started` (or
   *  the first delta if no start arrives), and back to false on
   *  `thinking_ended`. `thinkingBuffer` accumulates progressive deltas; when
   *  the provider runs with `display = "omitted"` this stays empty and the UI
   *  shows a plain "Thinking…" pill on the strength of `thinkingActive` alone.
   *  `thinkingStartedAt` / `thinkingElapsedMs` drive the "Thought for Ns"
   *  footer once the block closes. */
  thinkingActive: boolean;
  thinkingBuffer: string;
  thinkingStartedAt: number | null;
  thinkingElapsedMs: number | null;
  /** True once a thinking block has opened at least once *this turn*.
   *  Distinct from `thinkingActive`: a turn with `display = "omitted"` can
   *  have several sequential thinking blocks with no buffered text, so
   *  `thinkingActive` alone flips false→true→false→true... between blocks
   *  with nothing else to keep the pill's data truthy in the gaps. Without
   *  this flag `useInFlightThinking` would return `null` in each gap and the
   *  pill would unmount/remount — the "jumpy bubble" bug. Reset alongside
   *  the rest of the thinking fields (see `EMPTY_THINKING`), i.e. per-turn,
   *  unlike `everShownThisTurn` which survives to the entry's teardown. */
  thinkingShown: boolean;
  /** Ids of artifacts an `ArtifactWrite` tool call has produced so far this
   *  turn, in call order. Populated live from the `tool_call_completed` SSE
   *  event so the card renders while the turn is still streaming — no wait
   *  for `run_ended`. Cleared on finalize; the snapshot at that point is
   *  carried onto the finalized `TranscriptEntry`'s `metadata.artifact_ids`
   *  (see `finalizeInFlightText`) so the card survives the handoff from the
   *  streaming bubble to the persisted one. */
  artifactIds: string[];
  /** Sticky "has this turn shown anything yet" latch — true once text,
   *  a tool call, thinking, or an artifact has appeared at least once this
   *  turn. Lives here (not a component-local ref) so `StreamingMessage`
   *  can keep the bubble mounted through a momentary content gap — e.g. the
   *  beat between one tool call finishing and the next starting — even
   *  across a remount. `MessageList` remounts on every conversation switch
   *  (`key={deferredConversationKey}` in `ChatView`), which used to reset a
   *  local ref and could blank the bubble mid-turn if the remount landed in
   *  such a gap. Optional so existing call sites/tests that build a literal
   *  entry without it still type-check; missing reads as `false`. Reducers
   *  that add real content (text/tool calls/thinking/artifacts) set it to
   *  `true`; reducers that only remove or relabel content leave it as-is via
   *  spread. Only reset by the entry itself being deleted (`deleteInFlight`)
   *  and later recreated fresh. */
  everShownThisTurn?: boolean;
}

/** Token-usage breakdown accumulated across one logical user turn. A single
 *  turn can produce multiple `usage` events when the provider does a
 *  continuation (CLI runner's tool-use respawn loop) or any other multi-call
 *  pattern, so each `usage` event is summed into the running totals here.
 *  Reset only when the agent's in-flight entry tears down (deleteInFlight),
 *  i.e. once the turn is truly complete — not on intermediate text_complete
 *  events that fire between continuation loops. */
export interface TurnUsage {
  /** Fresh prompt tokens (uncached). */
  input: number;
  /** Model-generated reply tokens. */
  output: number;
  /** Tokens served from the provider's prompt cache (Anthropic's
   *  `cache_read_input_tokens`). Non-zero = cache hit on the stable prefix. */
  cacheRead: number;
  /** Tokens newly written to the provider's prompt cache this turn
   *  (Anthropic's `cache_creation_input_tokens`). */
  cacheCreation: number;
  /** Server-reported total (matches whatever convention the provider
   *  normalizer uses; we surface it as-is rather than re-summing). */
  total: number;
}

/** One running async `Delegate`'s pill-worthy metadata, as tracked per-id in
 *  `runningDelegatesByThread`. `startedAt` is an epoch-ms timestamp derived
 *  from the backend's `spawned_at` (see `DelegateStartedPayload`), NOT the
 *  moment this client happened to receive the SSE event — a connect-time
 *  replay of an already-running delegate must show its true elapsed time,
 *  not restart the clock at reconnect. */
export interface RunningDelegateInfo {
  delegateName: string;
  startedAt: number;
}

const EMPTY_TOOL_CALLS: ActiveToolCall[] = [];
const EMPTY_ARTIFACT_IDS: string[] = [];

/** Default-empty thinking fields. Used by reducers that rebuild an in-flight
 *  entry where the previous turn's thinking state is no longer relevant
 *  (e.g. on finalize, where the next turn starts with a clean reasoning
 *  channel). For continuation reducers — `appendInFlightDelta`,
 *  `ensureInFlight`, tool-call mutations — `preserveThinking(current)` is
 *  the right call so we don't blow away an in-progress pill on every
 *  text delta. */
const EMPTY_THINKING = {
  thinkingActive: false,
  thinkingBuffer: "",
  thinkingStartedAt: null as number | null,
  thinkingElapsedMs: null as number | null,
  thinkingShown: false,
};

/** Carries an existing in-flight entry's thinking fields forward into a
 *  rebuilt entry. Returns the default-empty shape when there is no current
 *  entry yet (first event of a turn). */
function preserveThinking(current?: InFlightAgentMessage) {
  return {
    thinkingActive: current?.thinkingActive ?? false,
    thinkingBuffer: current?.thinkingBuffer ?? "",
    thinkingStartedAt: current?.thinkingStartedAt ?? null,
    thinkingElapsedMs: current?.thinkingElapsedMs ?? null,
    thinkingShown: current?.thinkingShown ?? false,
  };
}

/** Upper bound on stacked *classic* (native tool-calling, no `action_id`)
 *  indicator chips shown at once. Finished classic chips now linger in place
 *  (see `markInFlightToolCallDone`) instead of disappearing the instant their
 *  tool completes, so a long tool-heavy turn needs a cap to keep the bubble
 *  from growing unbounded. `action_id`-keyed chips (tool_use/agent_action)
 *  are exempt from this cap — they already self-remove on their own
 *  completion event and can legitimately appear inline mid-text-stream. */
const MAX_STACKED_CLASSIC_TOOL_CALLS = 5;

/** Enforces `MAX_STACKED_CLASSIC_TOOL_CALLS` after a classic chip is added.
 *  Evicts the oldest *done* classic chip first so a genuinely still-running
 *  tool's indicator never disappears; only falls back to evicting the oldest
 *  classic chip outright if every classic chip currently on the stack is
 *  still active (all 5 running in parallel — rare, but possible with
 *  parallel Agent/subagent calls). Callers only ever add one chip at a time,
 *  so the stack can be at most one over the cap — a single eviction pass
 *  suffices. */
function capClassicToolCalls(calls: ActiveToolCall[]): ActiveToolCall[] {
  const classicCount = calls.filter((tc) => tc.action_id == null).length;
  if (classicCount <= MAX_STACKED_CLASSIC_TOOL_CALLS) return calls;
  let evictIdx = calls.findIndex((tc) => tc.action_id == null && tc.done);
  if (evictIdx === -1) evictIdx = calls.findIndex((tc) => tc.action_id == null);
  if (evictIdx === -1) return calls;
  return [...calls.slice(0, evictIdx), ...calls.slice(evictIdx + 1)];
}

/** Debounced-teardown timers for in-flight entries. Kept outside store state
 *  so scheduling does not itself trigger subscriber re-renders. */
const inFlightDeleteTimers = new Map<string, ReturnType<typeof setTimeout>>();

/** Watchdog timers guarding the optimistic typing indicator raised in
 *  `sendMessage` (see OPTIMISTIC_TYPING_WATCHDOG_MS). Keyed by in-flight key,
 *  kept outside store state for the same reason as the teardown timers. */
const optimisticTypingWatchdogs = new Map<string, ReturnType<typeof setTimeout>>();

/** Monotonically increasing counter bumped at the very start of every
 *  `selectAgent` call — i.e. every time the user's active selection (agent
 *  AND/OR thread) changes. Every async continuation inside `selectAgent`,
 *  plus the `loadMoreMessages` pagination path that shares the same hazard,
 *  captures this value at dispatch and bails once it's resolved if the
 *  current generation has moved past it.
 *
 *  This replaces the old `get().selectedAgentId !== agentId` staleness
 *  check, which only caught an agent *switch* — two different threads of the
 *  SAME agent passed it trivially, so a slow response for a thread the user
 *  had already navigated away from could land and overwrite whatever thread
 *  they were actually looking at (or, on a 404/500, wipe a perfectly valid
 *  current selection). A plain incrementing counter catches both an agent
 *  change and a thread change with one comparison, since `selectAgent` is
 *  re-invoked on every thread switch too (see `switchToThread` in
 *  `lib/threadNavigation.ts`).
 *
 *  Kept module-level (not store state) so bumping it never itself triggers a
 *  re-render — same rationale as `inFlightDeleteTimers` above. */
let selectionGeneration = 0;

/** `AbortController` for whatever `selectAgent` (or a `loadMoreMessages` call
 *  made during its lifetime) fetch is currently in flight. A new selection
 *  aborts the previous one outright — via the `.abort()` call at the top of
 *  `selectAgent` — rather than merely ignoring its result once the
 *  generation check catches it on arrival, so a superseded request stops
 *  doing pointless network work instead of running to completion unread.
 *  `null` before the first selection. */
let selectionAbortController: AbortController | null = null;

/** Monotonically increasing counter bumped at the start of every
 *  `loadAllThreads` call — the same stale-response hazard `selectionGeneration`
 *  guards against above, scoped to bulk thread hydration instead of agent/
 *  thread selection: two overlapping `loadAllThreads` calls (e.g. the
 *  bulk-hydration effect re-firing before the previous request lands) can
 *  resolve out of order, and without this an older response landing last
 *  would silently overwrite what a newer response already applied. Applies
 *  to both the success and error path — see `loadAllThreads`.
 *
 *  Kept module-level (not store state) so bumping it never itself triggers a
 *  re-render — same rationale as `selectionGeneration` above. */
let threadsHydrationGeneration = 0;

function clearOptimisticTypingWatchdog(key: string): void {
  const existing = optimisticTypingWatchdogs.get(key);
  if (existing) {
    clearTimeout(existing);
    optimisticTypingWatchdogs.delete(key);
  }
}

/** Arm (or re-arm) the safety-net watchdog for an optimistically-shown typing
 *  indicator. When it fires it retracts the indicator *only* if the entry is
 *  still exactly as `sendMessage` left it — typing dots with no streamed text,
 *  no tool calls, and no thinking activity — which means the run never started
 *  (e.g. the SSE stream was down and never replayed RunStarted). Any real
 *  progress makes the guard fail, so a genuinely-slow run is never disturbed. */
function armOptimisticTypingWatchdog(
  getEntry: (key: string) => InFlightAgentMessage | undefined,
  deleteEntry: (key: string) => void,
  key: string,
): void {
  clearOptimisticTypingWatchdog(key);
  const timer = setTimeout(() => {
    optimisticTypingWatchdogs.delete(key);
    const entry = getEntry(key);
    if (
      entry &&
      entry.isTyping &&
      entry.textBuffer === "" &&
      entry.activeToolCalls.length === 0 &&
      !entry.thinkingActive &&
      entry.thinkingBuffer.length === 0
    ) {
      deleteEntry(key);
    }
  }, OPTIMISTIC_TYPING_WATCHDOG_MS);
  optimisticTypingWatchdogs.set(key, timer);
}

interface ChatState {
  // Agent list
  agents: AgentSnapshot[];
  agentsLoading: boolean;

  // Selected agent
  selectedAgentId: string | null;
  selectedAgentProfile: AgentProfile | null;

  // Messages
  messages: TranscriptEntry[];
  allMessages: TranscriptEntry[];
  messagesLoading: boolean;
  loadingMore: boolean;
  displayCount: number;
  hasMoreMessages: boolean;

  // Cursor for server-side pagination
  cursor: PaginationCursor | null;

  // Scroll trigger — bumped when user sends a message to force scroll-to-bottom
  scrollToBottomTrigger: number;

  /** Per-agent in-flight run state (typing + streaming text + tool calls).
   *  Replaces the previous global `streamingText`/`isTyping`/`activeToolCalls`
   *  singletons. Scoped selector hooks below read individual entries. */
  inFlightByAgent: Map<string, InFlightAgentMessage>;

  /** Per-agent accumulated token usage for the current turn. Kept in a
   *  separate map (rather than nested in InFlightAgentMessage) so reducers
   *  that rebuild the in-flight entry don't need to remember to thread the
   *  usage field through every code path. Cleared in `deleteInFlight` —
   *  i.e. the turn's true completion marker, not on intermediate
   *  `text_complete` events that fire between continuation loops. */
  usageByAgent: Map<string, TurnUsage>;

  /** True iff the streaming bubble is scrolled into view for the currently
   *  selected agent. Drives the floating "is responding" overlay in ChatView.
   *  Stays global — only one visible chat at a time. */
  isStreamingAreaVisible: boolean;

  /** Which of ChatView's four mutually-exclusive side panels (Memory /
   *  Bookmarks / Assets / Todos) is open, per agent. Keyed by agent id (not
   *  thread — all four panels show agent-scoped data) so each chat recalls
   *  its own panel state: opening a panel in one agent's chat, or that agent
   *  auto-opening its Todo panel via the `todo_list.created` SSE event,
   *  never leaks into whichever other agent's chat the user happens to be
   *  viewing. Absence of an entry means no panel is open for that agent. */
  activePanelByAgent: Map<string, ChatSidePanel>;

  /** Whether the persistent Channels column (`ChannelsColumn`, rendered next
   *  to the chat area in `ChatView`) is pinned open for a given agent — set
   *  via the "Pin" control in the Channels tile's popover (
   *  `ChannelsTilePanel`) and cleared via the column's own unpin button.
   *  Keyed by agent id, same per-agent scoping as `activePanelByAgent`
   *  above, so pinning it for one agent's chat doesn't leak into another
   *  agent's. Absence of an entry means unpinned — the default, popover-only
   *  experience. */
  channelsColumnPinnedByAgent: Map<string, boolean>;

  /** Same pin-state map as `channelsColumnPinnedByAgent` above, for the
   *  persistent Assignments column (`AssignmentsColumn`) — set via the "Pin"
   *  control in the Assignments tile's popover (`AssignmentsTilePanel`) and
   *  cleared via the column's own unpin button. Kept as a separate map
   *  (rather than folding into the channels one) since a given agent can
   *  have either, both, or neither column pinned independently. */
  assignmentsColumnPinnedByAgent: Map<string, boolean>;

  /** Per-conversation message cache — keyed PER THREAD via `inFlightKey`
   *  (plain `agentId` for the default thread or a virtual channel key like
   *  `project:{projectId}`, `inFlightKey(agentId, threadId)` for any other
   *  thread of that agent — mirrors the same composite-key convention
   *  `inFlightByAgent`/`usageByAgent` already use, see `inFlightKey`'s doc
   *  comment). Was keyed by bare `agentId` alone until each agent could have
   *  more than one thread open — see `getCacheEntry`/`invalidateCache`. */
  messageCache: Map<string, CacheEntry>;

  // Message status tracking
  pendingMessageIds: Set<string>;
  seenMessageIds: Set<string>;
  messageIdByTs: Record<string, string>;

  // Actions
  fetchAgents: () => Promise<void>;
  createAgent: (profile: AgentProfile) => Promise<AgentProfile>;
  updateAgent: (profile: AgentProfile) => Promise<AgentProfile>;
  cloneAgent: (parentId: string) => Promise<AgentProfile>;
  selectAgent: (agentId: string) => Promise<void>;
  loadMoreMessages: () => Promise<void>;
  sendMessage: (content: string, attachmentIds?: string[], attachments?: Attachment[], focusPath?: string | null) => Promise<void>;

  // In-flight actions (per-agent)
  ensureInFlight: (agentId: string) => void;
  appendInFlightDelta: (agentId: string, text: string) => void;
  // `key` is an inFlightKey (plain agent id, or an agent+thread composite —
  // see `inFlightKey`), not necessarily a plain agent id.
  finalizeInFlightText: (key: string, text: string) => void;
  /** Force-refresh the agent's artifact list (Assets panel) after a run ends.
   *  Inline thread-bubble cards don't need this — they're already resolved
   *  via `artifactIds` by the time this fires. `key` is an `inFlightKey`
   *  (agent id, optionally thread-scoped). */
  syncRunArtifacts: (key: string) => void;
  setInFlightTyping: (agentId: string, typing: boolean) => void;
  addInFlightToolCall: (agentId: string, toolCall: { tool: string; input?: Record<string, unknown>; label?: string }) => void;
  /** Marks the oldest not-yet-done *classic* chip (no `action_id`) as done,
   *  in place — it stays in `activeToolCalls` rather than being removed, so
   *  the bubble doesn't shrink-then-regrow between one tool finishing and
   *  the next starting or text beginning. Renamed from the old
   *  `popInFlightToolCall`, which used to `slice(1)` the array on every
   *  `tool_call_completed` — the FIFO/oldest-first assumption is carried
   *  forward unchanged (`tool_call_completed` carries no id tying it back to
   *  a specific chip, same as before; classic tool calls complete in
   *  dispatch order in practice). Actual removal now only happens via
   *  `text_delta`'s classic-chip flush, `finalizeInFlightText`, or
   *  `clearInFlightToolCalls` (run_ended) — plus `capClassicToolCalls`
   *  evicting the oldest once the stack exceeds
   *  `MAX_STACKED_CLASSIC_TOOL_CALLS`. */
  markInFlightToolCallDone: (agentId: string) => void;
  /** Append an artifact id produced by an `ArtifactWrite` tool call to the
   *  in-flight entry — the live half of inline card rendering. Idempotent
   *  (a duplicate id is a no-op) and immutably replaces the entry so
   *  subscribers re-render. */
  appendInFlightArtifactId: (agentId: string, artifactId: string) => void;
  addInFlightToolUse: (agentId: string, toolUseId: string, toolName: string, input?: Record<string, unknown>) => void;
  addInFlightAgentAction: (agentId: string, actionId: string, label: string) => void;
  removeInFlightAgentAction: (agentId: string, actionId: string) => void;
  clearInFlightToolCalls: (agentId: string) => void;
  /** Update the label on the first in-flight TodoCreate chip (from tool_progress events). */
  patchTodoCreateProgress: (agentId: string, label: string) => void;
  /** Open `panel` for `agentId` (mutually exclusive with the other three),
   *  or close whichever panel that agent has open when `panel` is `null`.
   *  Scoped per agent so it never affects any other agent's chat. */
  setActivePanel: (agentId: string, panel: ChatSidePanel | null) => void;
  /** Pin or unpin the persistent Channels column for `agentId` — see
   *  `channelsColumnPinnedByAgent`'s doc comment above. */
  setChannelsColumnPinned: (agentId: string, pinned: boolean) => void;
  /** Pin or unpin the persistent Assignments column for `agentId` — see
   *  `assignmentsColumnPinnedByAgent`'s doc comment above. */
  setAssignmentsColumnPinned: (agentId: string, pinned: boolean) => void;
  scheduleInFlightTeardown: (agentId: string) => void;
  deleteInFlight: (agentId: string) => void;
  /** Mark the start of a reasoning block. Idempotent — safe to call again if
   *  the provider emits a delta before an explicit start event (some
   *  normalizers auto-synth a start from the first delta). */
  startInFlightThinking: (agentId: string) => void;
  /** Append a chunk of reasoning text to the agent's in-flight thinking
   *  buffer. Implicitly starts the block if no start arrived yet. */
  appendInFlightThinkingDelta: (agentId: string, text: string) => void;
  /** Close the reasoning block, recording elapsed time so the UI can render
   *  a "Thought for Ns" footer when the disclosure collapses. */
  endInFlightThinking: (agentId: string, elapsedMs: number) => void;
  /** Sum a single provider-call's token usage into the agent's running
   *  per-turn total. Accumulates across the CLI runner's tool-use
   *  continuation loops, the Anthropic API path's potential multi-emit,
   *  and any other multi-call pattern within one user turn. */
  accumulateUsage: (agentId: string, partial: TurnUsage) => void;

  markMessageSent: (messageId: string) => void;
  markMessageSeen: (messageId: string) => void;
  setStreamingAreaVisible: (visible: boolean) => void;
  patchAgentSnapshot: (agentId: string, patch: Partial<AgentSnapshot>) => void;
  /** Remove one entry from an agent's `pending_forms` by `form_id`, wherever
   *  it's scoped — mirrors the backend's `clear_pending_form(form_id)`
   *  (form_ids are server-generated and globally unique, so a plain filter
   *  can't clear the wrong thread's entry). Call after an async form answer
   *  or dismiss lands, so the overlay drops immediately instead of waiting
   *  on the next `fetchAgents` refetch. */
  clearPendingAsyncForm: (agentId: string, formId: string) => void;
  cancelRun: () => Promise<void>;
  loadMessagesAroundTimestamp: (agentId: string, targetTs: string) => Promise<void>;
  invalidateCache: (agentId: string) => void;
  reset: () => void;

  /** Per-agent, thread-scoped pending sync form spec — set when the agent
   *  invokes AskUserQuestionWithForm, cleared on submit or run end. Keyed the
   *  same way as `inFlightByAgent` (see `inFlightKey`): the plain agent id
   *  for the default thread, `inFlightKey(agentId, threadId)` for any other
   *  thread — so a form pending on one thread of an agent never renders
   *  under, or gets wiped by, a run finishing on another thread of the same
   *  agent. Channels with no thread concept (project/copilot chats) always
   *  key by the plain channel id, unaffected by this scoping. */
  pendingFormByAgent: Record<string, FormRequestPayload | undefined>;
  setPendingForm(agentId: string, form: FormRequestPayload): void;
  clearPendingForm(agentId: string, threadId?: string): void;
  /** Sparse (agent, thread) → minimized flag for the currently pending sync
   *  form, keyed identically to `pendingFormByAgent` (see `inFlightKey`).
   *  Absence means "expanded" — the common case — so a fresh key never has
   *  to be explicitly initialized. `setFormMinimized` deletes the entry
   *  rather than storing `false` to keep this sparse. Every path that
   *  removes or replaces the pending form it shadows (`setPendingForm`,
   *  `clearPendingForm`, `clearPendingAsyncFormId`, `markPendingFormOrphaned`)
   *  also deletes the matching entry here, so the next form an agent posts —
   *  including one that replaces a still-pending form on the same
   *  (agent, thread) slot, e.g. on reconnect/replay/orphan-recovery — always
   *  starts expanded instead of inheriting a stale minimized flag. This is
   *  the arrives-expanded invariant: minimized is reachable only by an
   *  explicit user click, never inherited. */
  minimizedFormByKey: Record<string, boolean>;
  setFormMinimized(agentId: string, threadId: string | undefined, minimized: boolean): void;
  /** Marks the (agentId, threadId) slot's pending form as orphaned in place —
   *  same live representation `hydratePendingSyncFormsFromAgents` gives a
   *  form the backend's restart-time reaper found abandoned (see
   *  `FormRequestPayload.orphaned`). Used by `run_ended` (`useSSE.ts`)
   *  instead of `clearPendingForm` when the ending run OWNS this exact slot:
   *  a sync form genuinely suspends its run, so a run ending while its own
   *  slot is still populated means that form was never answered — deleting
   *  it there would make it vanish with no trace, the exact silent-orphan
   *  failure this whole mechanism exists to surface. No-op if the slot is
   *  already empty (the common case — an answered form clears its own slot
   *  before `run_ended` ever arrives) or already orphaned. */
  markPendingFormOrphaned(agentId: string, threadId?: string): void;
  /** Seed `pendingFormByAgent` from every agent's own `pending_forms` snapshot
   *  entries tagged `mode: "sync"` — the sync-form rehydration path. Call
   *  once, right after the app-mount `fetchAgents()` resolves (see
   *  `AppShell`), so a page reload restores an answerable sync form instead
   *  of losing it while the backend is still parked on the oneshot. Never
   *  overwrites a key that's already present — a live SSE `form_request` or
   *  an already-cleared entry both take priority over this REST snapshot
   *  read. */
  hydratePendingSyncFormsFromAgents(): void;
  /** Append a synthetic display entry immediately after the user submits a form
   *  (or clicks an action button — pass `action`/`note` instead of a filled
   *  `answers` map). Not persisted — the server's tool_use/tool_result pair
   *  covers the reload case (see `preprocessFormToolPairs` in MessageList). */
  addFormAnswerEntry(agentId: string, payload: { form: FormRequestPayload; answers: FormAnswerMap; action?: FormAction; note?: string }): void;

  /** Async-form counterpart to `addFormAnswerEntry` above — appends the
   *  answered async form_answer bubble the instant `submitAsyncFormAnswer`
   *  (ChatView.tsx) resolves. Needed because, unlike a sync form's answer,
   *  an async form's own live-push (`FormResolved`, see
   *  `AgentEventPayload::FormResolved` in `crates/ao-protocol/src/event.rs`)
   *  carries only `form_id` — no values, no spec — so there is nothing for
   *  an SSE handler to render from; the caller already has both (the just-
   *  submitted `values` and this form's own spec, still in scope before the
   *  pending pointer is cleared) at the moment of submission. Metadata shape
   *  (`form_id`/`values`/`spec`) matches the backend's own `FormAnswerMeta`
   *  (`build_form_answer_entry`, crates/ao-server/src/routes/form_answers.rs)
   *  exactly, so the next full transcript refetch (`selectAgent`, which
   *  REPLACES `messages`/`allMessages` wholesale rather than merging) swaps
   *  this optimistic entry out for the authoritative persisted one with no
   *  duplicate ever visible — same reconciliation-by-replacement the plain
   *  `sendMessage` optimistic bubble already relies on. Not persisted itself
   *  — same "local only until the next real fetch" contract as
   *  `addFormAnswerEntry`. */
  addAsyncFormAnswerEntry(agentId: string, payload: { form_id: string; threadId?: string; values: FormAnswerMap; spec: AsyncFormSpec }): void;

  /** Draft text/selections captured from a sync form the operator was
   *  mid-fill on, the instant a newer form supersedes it on the same
   *  newest-wins slot (see `AskUserQuestionForm`'s `onWithdraw` prop —
   *  swapping the pending form remounts it, which would otherwise silently
   *  drop whatever was typed). Keyed by the WITHDRAWN form's own `form_id`
   *  (globally unique, matches the `form_withdrawn` transcript entry's
   *  `metadata.form_id` — see `form_withdrawn_entry`,
   *  crates/ao-engine-tools-core/src/form_events.rs) so
   *  `FormWithdrawnIndicator` can look its own draft up with no other
   *  context. Never populated for a form nobody typed into. Not persisted —
   *  same lifetime as this browser session; the draft was never sent to the
   *  backend to begin with, so there is nothing to rehydrate on reload. */
  withdrawnFormDrafts: Record<string, WithdrawnFormDraftEntry[]>;
  stashWithdrawnFormDraft(formId: string, draft: WithdrawnFormDraftEntry[]): void;

  /** Per-channel pending async form ID — set when form_posted arrives for a channel key
   *  (e.g. `project:{id}`), cleared on submit/dismiss/run end. */
  pendingAsyncFormIdByChannel: Record<string, string | null>;
  setPendingAsyncFormId(channelKey: string, formId: string): void;
  clearPendingAsyncFormId(channelKey: string): void;

  // ---------------------------------------------------------------------------
  // Thread dimension
  // ---------------------------------------------------------------------------

  /** Per-agent list of threads, populated by loadThreads. */
  threadsByAgent: Map<string, Thread[]>;

  /** Non-null iff the most recent `loadAllThreads` call failed (and hasn't
   *  been superseded by a newer attempt or since succeeded). Holds a
   *  human-readable message for display; `null` means "no error" — either
   *  nothing has failed, or a later attempt already cleared it.
   *
   *  Exists so a failed bulk hydration and a genuinely-empty result are
   *  distinguishable at the UI layer (`threadsByAgent` alone can't tell
   *  them apart — see `loadAllThreads`'s doc comment). Consumers (e.g.
   *  `HomeSidebar`) must check this BEFORE falling back to an empty-state
   *  render, since this app's thread store has lost data to exactly this
   *  confusion before. */
  threadsHydrationError: string | null;

  /** Per-agent sticky selected thread id. Survives navigation and re-renders.
   *  When absent (or equal to `default-{agentId}`), message operations use the
   *  agent-keyed default transcript path. */
  selectedThreadIdByAgent: Map<string, string>;

  /** Composite keys (same shape as `inFlightByAgent` — plain agent id for the
   *  default thread, `inFlightKey(agentId, threadId)` otherwise) for threads
   *  whose run finalized while the user was not looking at them (a different
   *  thread, or a different agent entirely, was active). Read by
   *  `ThreadTabStrip` to render an unread dot on that thread's pill; cleared
   *  via `markThreadViewed` once the user actually navigates to it. */
  unreadThreadIds: Set<string>;

  /** Composite keys (same shape as `inFlightByAgent` — plain agent id for the
   *  default thread, `inFlightKey(agentId, threadId)` otherwise) mapped to a
   *  `delegation_id` → `RunningDelegateInfo` map of that thread's currently
   *  running background `Delegate` calls. Kept as its own map rather than
   *  folded into `inFlightByAgent` because an async delegate keeps running
   *  well after the `Delegate` tool call itself returns (and often after the
   *  parent's own turn/run has already ended) — see `spawn_named_async_core`
   *  in `ao-engine-tools-core/src/background_agents/spawner.rs`. The value is
   *  a `Map` (not a plain count) so a connect-time replay of an
   *  already-tracked delegation (see `useSSE.ts`'s reconnect handling) is a
   *  harmless no-op upsert rather than a double-count, while still keeping
   *  `.size`/`.has()` available for callers that only ask "are any running?".
   *  Added on the backend's `delegate.started` SSE event, removed on the
   *  matching `delegate.complete` — see `beginDelegateRun`/`endDelegateRun`
   *  and their call sites in `useSSE.ts`. Read by `resolveThreadActivity` so
   *  any thread-list surface can show "a delegate is running here" — and,
   *  with the name/start time now carried alongside the id, render an actual
   *  pill rather than a bare presence flag — without the user opening that
   *  thread. */
  runningDelegatesByThread: Map<string, Map<string, RunningDelegateInfo>>;

  /** Fetch threads for an agent and initialize the default selection if none
   *  has been explicitly chosen. `signal` is forwarded to `api.listThreads`
   *  (optional — every pre-existing call site omits it and is unaffected;
   *  HomeSidebar's eager thread hydration fan-out is the one caller that
   *  passes one, so a superseded/unmounted hydration pass actually cancels
   *  its still-outstanding requests). */
  loadThreads(agentId: string, signal?: AbortSignal): Promise<void>;

  /** Bulk-hydrate `threadsByAgent` for every agent in one request via
   *  `api.listAllThreads`, instead of one `loadThreads` call per agent.
   *
   *  `knownAgentIds` MUST list every agent the caller considers live. The
   *  grouped response omits any agent with zero threads entirely — it is
   *  metadata-only, so a legitimately-empty agent never appears as a key —
   *  so for every id in `knownAgentIds` absent from the response, this seeds
   *  `threadsByAgent` with an empty array rather than leaving it unset.
   *  Several consumers use `threadsByAgent.has(agentId)` as a proxy for
   *  "has this agent been hydrated yet"; without the empty-array seed, an
   *  agent with no threads would look permanently un-hydrated and could
   *  drive an infinite refetch loop or a spinner that never clears. Do not
   *  "optimize" this seeding away. THIS SEEDING MUST ONLY HAPPEN ON THE
   *  SUCCESS PATH — see below.
   *
   *  `signal` is forwarded to `api.listAllThreads` so a caller (HomeSidebar's
   *  bulk-hydration effect) can actually cancel an outstanding request on
   *  unmount/dependency-change instead of merely ignoring its result. An
   *  abort is normal control flow, not a failure: it never populates
   *  `threadsHydrationError`.
   *
   *  Never rejects — a failure is caught internally and recorded in
   *  `threadsHydrationError` instead, specifically so it can NOT be confused
   *  with (or accidentally trigger) the empty-array seeding above. A caller
   *  that awaits this and finds `threadsByAgent` unchanged for some agent
   *  must check `threadsHydrationError` before treating that as "this agent
   *  has no threads" — the two are deliberately kept distinguishable.
   *
   *  Guards against out-of-order resolution: two overlapping calls (e.g. the
   *  effect re-firing before the previous request lands) can resolve in
   *  either order, so an internal monotonic generation counter is bumped on
   *  every call and a response (success OR failure) is only applied if its
   *  call is still the most recent one dispatched — an older response that
   *  resolves after a newer one is silently dropped rather than clobbering
   *  fresher state. */
  loadAllThreads(knownAgentIds: string[], signal?: AbortSignal): Promise<void>;

  /** Persistently select a thread for an agent. Subsequent message sends and
   *  fetches route to this thread until changed.
   *
   *  Resolves the owning agent by scanning `threadsByAgent` for whichever
   *  agent's loaded list contains `threadId`, falling back to
   *  `selectedAgentId` when the thread isn't loaded anywhere yet. That
   *  fallback is only safe when the caller already knows (or doesn't care)
   *  that the currently-selected agent IS the thread's owner — e.g. a click
   *  inside that agent's own already-open thread strip. A caller that knows
   *  the target agent id upfront (it's cross-agent, like a sidebar row for
   *  an agent that isn't currently open) must use `selectThreadForAgent`
   *  instead, or this heuristic can silently attribute the thread to the
   *  wrong agent. */
  selectThread(threadId: string): void;

  /** Same effect as `selectThread`, but for a known agent id — sets
   *  `selectedThreadIdByAgent[agentId] = threadId` directly, with no
   *  reverse-lookup/fallback ambiguity. Use this whenever the caller already
   *  has the concrete agent id (e.g. `lib/threadNavigation.ts`'s
   *  `switchToThread`), since `selectThread`'s scan-and-fallback can
   *  misattribute the thread to whichever agent is merely *currently
   *  selected* if the target agent's thread list hasn't been loaded yet —
   *  exactly the case for a sidebar row belonging to a different agent than
   *  the one presently open. */
  selectThreadForAgent(agentId: string, threadId: string): void;

  /** Archive a thread — the tab strip's own "X" control. Persists
   *  server-side (see `archived_at` on `Thread`), so it stays hidden from the
   *  tab strip, the overflow panel, `ThreadsPanel`'s main list, and Home's
   *  thread list across reloads/restarts, not just for the current session.
   *  The thread and its history are completely untouched — this is strictly
   *  a visibility flag. Reversed only via `unarchiveThread`, reached from
   *  `ThreadsPanel`'s "Archived" section. Never called for the default
   *  thread, whose pill has no close control. */
  archiveThread(threadId: string): Promise<Thread>;

  /** Reverse of `archiveThread` — restores the thread to every surface it
   *  was hidden from. Reached from `ThreadsPanel`'s "Archived" section. */
  unarchiveThread(threadId: string): Promise<Thread>;

  /** Clear the unread flag for a thread the user is now actively viewing.
   *  `threadId` should already be collapsed via `resolveStreamingThreadId`
   *  (undefined for the default thread) — mirrors the key shape
   *  `finalizeInFlightText` writes into `unreadThreadIds`. */
  markThreadViewed(agentId: string, threadId: string | undefined): void;

  /** Record that an async `Delegate` run identified by `delegationId` is live
   *  on `key`'s thread — upserts its `{ delegateName, startedAt }` into
   *  `key`'s entry in `runningDelegatesByThread`. `key` is an `inFlightKey`
   *  (plain agent id, or an agent+thread composite), same shape every other
   *  per-thread reducer here takes. `startedAt` is an epoch-ms timestamp the
   *  caller derives from the backend's `spawned_at` — this reducer does not
   *  stamp its own receive time, so a connect-time replay reports the
   *  delegate's true elapsed time rather than restarting the clock. Fires
   *  from both a live `delegate.started` SSE event and a connect-time replay
   *  of one, so re-adding an already-tracked `delegationId` is a harmless
   *  no-op upsert (same name/time) rather than a double-count. */
  beginDelegateRun(key: string, delegationId: string, delegateName: string, startedAt: number): void;

  /** Record that `delegationId` resolved (background run
   *  completed/failed/cancelled) — removes it from `key`'s map, dropping the
   *  map entry entirely once it's empty. */
  endDelegateRun(key: string, delegationId: string): void;

  /** Reconnect-only escape hatch: unconditionally drops `key`'s entire entry
   *  from `runningDelegatesByThread`, independent of which delegation ids it
   *  holds. Used by `useSSE.ts`'s reconnect-grace timer to clear a delegation
   *  that died along with a restarted server — nothing will ever replay a
   *  `delegate.started` to reconfirm it, so per-id removal via `endDelegateRun`
   *  never happens for it. Not for use as a `delegate.complete` handler:
   *  that must keep removing by id via `endDelegateRun` so sibling
   *  delegations on the same thread survive. */
  clearDelegateRunsForKey(key: string): void;

  /** Create a new fresh (no inherited history) thread and select it. */
  createFreshThread(agentId: string, title?: string): Promise<Thread>;

  /** Create a branch thread that inherits history up to `branch_at` and select it. */
  branchThread(
    agentId: string,
    branchParams: { source_thread_id: string; branch_at: string; source_message_id?: string | null },
    title?: string,
  ): Promise<Thread>;

  /** Rename a thread and update the in-store list. */
  renameThread(threadId: string, title: string | null): Promise<Thread>;

  /** Patch a thread's `title`/`auto_title` in local state without a network
   *  round-trip — driven by the `thread_renamed` SSE event (fired for both
   *  an agent-tool rename and the server's first-message auto-title). Only
   *  the fields present in `patch` are overwritten; omit a field to leave it
   *  untouched (the payload only ever carries the one field that changed). */
  patchThreadLive(threadId: string, patch: { title?: string; auto_title?: string }): void;

  /** Append a server-created thread into local state without a network
   *  round-trip — driven by the `thread_created` SSE event, fired when a
   *  scheduled task fire or recurring assignment run creates a `Fresh`/
   *  `Dedicated` thread with no interactive request in the loop to hand it
   *  back directly (contrast `createFreshThread`, which already appends the
   *  REST response). Without this, the new thread only appears in the tab
   *  strip on the next full `loadThreads` refetch (e.g. navigating away and
   *  back), even though the automation's reply is already streaming into it.
   *  No-ops if `agentId`'s thread list already contains this id (e.g. the
   *  event arrives after a `loadThreads` that already picked it up). */
  addThreadLive(agentId: string, thread: Thread): void;

  /** Delete a thread and update the in-store list. Reverts selection to the
   *  default thread when the deleted thread was selected. */
  deleteThread(threadId: string): Promise<void>;

  // ---------------------------------------------------------------------------
  // Assignment dimension
  // ---------------------------------------------------------------------------

  /** Per-agent list of assignments, populated by loadAssignments. */
  assignmentsByAgent: Map<string, Assignment[]>;

  /** Per-assignment run history (newest first), populated by loadAssignmentRuns. */
  runsByAssignment: Map<string, AssignmentRun[]>;

  /** Fetch all assignments for an agent into the store. */
  loadAssignments(agentId: string): Promise<void>;

  /** Create an assignment for an agent and append it to that agent's list. */
  createAssignment(agentId: string, req: CreateAssignmentRequest): Promise<Assignment>;

  /** Patch an assignment and replace it in the owning agent's list. */
  updateAssignment(assignmentId: string, patch: PatchAssignmentRequest): Promise<Assignment>;

  /** Delete an assignment and drop it from the owning agent's list. */
  deleteAssignment(assignmentId: string): Promise<void>;

  /** Fetch run history for an assignment into the store (newest first). */
  loadAssignmentRuns(assignmentId: string): Promise<void>;

  /** Fire an assignment immediately, then refresh its run history. */
  triggerAssignment(assignmentId: string, body?: TriggerAssignmentRequest): Promise<AssignmentRun>;

  /** Mount the project channel as the active conversation in the chat store so
   *  MessageList + ChatInput can render it without targeting the agent's personal
   *  transcript. Uses virtual key `project:{projectId}` as selectedAgentId. */
  mountProjectChannel(
    projectId: string,
    messages: TranscriptEntry[],
    cursor: PaginationCursor | null,
    agentName: string,
    agentEmoji: string | undefined,
  ): void;

  /** Push a fresh message/cursor snapshot for the active project channel.
   *  Called by the overlay whenever projectStore messages change (e.g. after an
   *  SSE finalise refetch). Pass bumpScroll=true when the user just sent a message. */
  syncProjectChannelMessages(
    projectId: string,
    messages: TranscriptEntry[],
    cursor: PaginationCursor | null,
    bumpScroll?: boolean,
  ): void;
}

/**
 * Infer message statuses from transcript order.
 * If a user message is followed (anywhere later) by an agent message,
 * the agent "saw" it → status "seen". Otherwise → "delivered".
 */
function inferMessageStatuses(messages: TranscriptEntry[]): TranscriptEntry[] {
  // Find the index of the last agent message
  let lastAgentIdx = -1;
  for (let i = messages.length - 1; i >= 0; i--) {
    if (isAgentMessage(messages[i])) {
      lastAgentIdx = i;
      break;
    }
  }

  return messages.map((m, i) => {
    if (!isUserMessage(m)) return m;
    return { ...m, status: i < lastAgentIdx ? "seen" as const : "delivered" as const };
  });
}

/** Returns the cache entry for the given agent + thread (or the agent's
 *  default-thread/virtual-channel entry when `threadId` is omitted), or
 *  undefined. See `messageCache`'s doc comment for the key scheme. */
export function getCacheEntry(state: ChatState, agentId: string, threadId?: string): CacheEntry | undefined {
  return state.messageCache.get(inFlightKey(agentId, threadId));
}

/** Evicts the least-recently-accessed cache entry when the cache exceeds MAX_CACHED_CONVERSATIONS. */
export function evictLRU(cache: Map<string, CacheEntry>): void {
  while (cache.size > MAX_CACHED_CONVERSATIONS) {
    let oldestKey: string | null = null;
    let oldestTime = Infinity;
    for (const [key, entry] of cache) {
      if (entry.lastAccessed < oldestTime) {
        oldestTime = entry.lastAccessed;
        oldestKey = key;
      }
    }
    if (oldestKey) cache.delete(oldestKey);
  }
}

/** Returns the thread_id to pass to API calls for the given agent.
 *  Returns `undefined` when the default thread is selected (or no thread is
 *  selected yet), preserving the pre-thread agent-keyed transcript path.
 *
 *  Delegates to `resolveStreamingThreadId` for the actual collapse rule
 *  rather than re-deriving it: once `loadThreads` resolves,
 *  `selectedThreadIdByAgent` holds the default thread's REAL backend id
 *  (see `loadThreads`'s `defaultThread?.id`), not the `default-{agentId}`
 *  placeholder — a bare sentinel check here would miss that case and this
 *  function would stop agreeing with every other consumer (`inFlightKey`
 *  bucketing, `pendingFormByAgent`, SSE thread tagging) about which thread
 *  counts as "the default one". */
function effectiveThreadId(
  agentId: string,
  selectedThreadIdByAgent: Map<string, string>,
  threadsByAgent: Map<string, Thread[]>,
): string | undefined {
  return resolveStreamingThreadId(agentId, selectedThreadIdByAgent.get(agentId), threadsByAgent.get(agentId));
}

/** True when `err` is the "API 404" error `fetchJson` throws for an unknown
 *  resource (e.g. a thread id restored from a stale/different data root).
 *  `getAgent`/`getMessages` don't use the status-carrying `ApiError` class,
 *  so this matches on the `fetchJson` message shape instead. */
function isNotFoundError(err: unknown): boolean {
  return err instanceof Error && /^API 404:/.test(err.message);
}

/** Separator used to compose a thread-scoped `inFlightByAgent`/`usageByAgent`
 *  map key. Chosen to never collide with a real agent or thread id (both are
 *  UUIDs or slugs, neither contains `::`). */
const THREAD_KEY_SEPARATOR = "::thread:";

/** Composes the `inFlightByAgent`/`usageByAgent` map key for a given agent +
 *  thread. Non-default threads get a distinct key so their streaming state
 *  (typing indicator, text buffer, tool-call chips, thinking pill) never
 *  bleeds into the agent's default-thread bucket or another thread's —
 *  mirrors the backend's `AgentEvent.thread_id` tagging, which is `None` for
 *  the default thread (omitted for byte-exact back-compat) and `Some(id)`
 *  for any fresh/branch thread. Falls back to the plain `agentId` when no
 *  thread is given, preserving the single-bucket pre-thread behavior. */
export function inFlightKey(agentId: string, threadId?: string | null): string {
  return threadId ? `${agentId}${THREAD_KEY_SEPARATOR}${threadId}` : agentId;
}

/** Inverse of `inFlightKey` — recovers the underlying agent id from either a
 *  plain agent key or a thread-scoped composite key. The SSE connection
 *  itself is always per-agent (only the in-flight *state* is thread-scoped),
 *  so callers that need to open/track a channel by agent id must unwrap any
 *  composite key first. */
export function agentIdFromInFlightKey(key: string): string {
  const idx = key.indexOf(THREAD_KEY_SEPARATOR);
  return idx === -1 ? key : key.slice(0, idx);
}

/** Companion to `agentIdFromInFlightKey` — recovers the thread id portion of
 *  a composite key, or `undefined` for a plain agent-only key (the
 *  default-thread case). Together the two fully decompose a key produced by
 *  `inFlightKey`. */
export function threadIdFromInFlightKey(key: string): string | undefined {
  const idx = key.indexOf(THREAD_KEY_SEPARATOR);
  return idx === -1 ? undefined : key.slice(idx + THREAD_KEY_SEPARATOR.length);
}

export const useChatStore = create<ChatState>((set, get) => ({
  agents: [],
  agentsLoading: false,
  selectedAgentId: null,
  selectedAgentProfile: null,
  messages: [],
  allMessages: [],
  messagesLoading: false,
  loadingMore: false,
  displayCount: PAGE_SIZE,
  hasMoreMessages: false,
  cursor: null,
  scrollToBottomTrigger: 0,
  inFlightByAgent: new Map<string, InFlightAgentMessage>(),
  usageByAgent: new Map<string, TurnUsage>(),
  isStreamingAreaVisible: true,
  activePanelByAgent: new Map<string, ChatSidePanel>(),
  channelsColumnPinnedByAgent: new Map<string, boolean>(),
  assignmentsColumnPinnedByAgent: new Map<string, boolean>(),
  messageCache: new Map<string, CacheEntry>(),
  pendingMessageIds: new Set<string>(),
  seenMessageIds: new Set<string>(),
  messageIdByTs: {},
  pendingFormByAgent: {},
  minimizedFormByKey: {},
  withdrawnFormDrafts: {},
  pendingAsyncFormIdByChannel: {},
  threadsByAgent: new Map<string, Thread[]>(),
  threadsHydrationError: null,
  selectedThreadIdByAgent: new Map<string, string>(),
  unreadThreadIds: new Set<string>(),
  runningDelegatesByThread: new Map<string, Map<string, RunningDelegateInfo>>(),
  assignmentsByAgent: new Map<string, Assignment[]>(),
  runsByAssignment: new Map<string, AssignmentRun[]>(),

  fetchAgents: async () => {
    set({ agentsLoading: true });
    try {
      const agents = await api.getAgents();
      set({ agents });
    } finally {
      set({ agentsLoading: false });
    }
  },

  createAgent: async (profile: AgentProfile) => {
    const created = await api.createAgent(profile);
    await get().fetchAgents();
    return created;
  },

  updateAgent: async (profile: AgentProfile) => {
    const updated = await api.updateAgent(profile);
    // Refresh snapshot so sidebar reflects any name/emoji changes
    await get().fetchAgents();
    // Update selectedAgentProfile if the edited agent is the currently selected one
    if (get().selectedAgentId === updated.id) {
      set({ selectedAgentProfile: updated });
    }
    return updated;
  },

  cloneAgent: async (parentId: string) => {
    const cloned = await api.cloneAgent(parentId);
    await get().fetchAgents();
    return cloned;
  },

  selectAgent: async (agentId: string) => {
    // Every selection — a new agent OR just a new thread of the same agent
    // (selectAgent is re-invoked on every thread switch, see `switchToThread`
    // in `lib/threadNavigation.ts`) — supersedes whatever selection came
    // before it. Bump the generation and swap in a fresh AbortController
    // FIRST, before any async work starts, so:
    //  1. The previous selection's in-flight fetch(es) — including any
    //     `loadMoreMessages` pagination fetch dispatched during its lifetime,
    //     which shares this same controller — are actually cancelled, not
    //     just ignored on arrival.
    //  2. Every async continuation below (and in `loadMoreMessages`) can
    //     compare its captured generation against the live counter and bail
    //     out silently if it's stale by the time it resolves. See the
    //     module-level doc comments on `selectionGeneration` /
    //     `selectionAbortController` for why this replaces the old
    //     `selectedAgentId !== agentId` check (agent-scoped only, not
    //     thread-scoped).
    selectionAbortController?.abort();
    const myGeneration = ++selectionGeneration;
    const controller = new AbortController();
    selectionAbortController = controller;
    const isStale = () => myGeneration !== selectionGeneration;

    const threadId = effectiveThreadId(agentId, get().selectedThreadIdByAgent, get().threadsByAgent);
    const cached = getCacheEntry(get(), agentId, threadId);

    if (cached) {
      // Cache hit: render cached data instantly, then background-refresh
      // to pick up messages that arrived while SSE was disconnected.
      const mappedMessages = inferMessageStatuses(cached.allMessages);
      const displayCount = cached.displayCount;
      const messages = mappedMessages.slice(-displayCount);
      const cacheKey = inFlightKey(agentId, threadId);

      // Update lastAccessed
      const cache = new Map(get().messageCache);
      cache.set(cacheKey, { ...cached, lastAccessed: Date.now() });

      set({
        selectedAgentId: agentId,
        selectedAgentProfile: (cached.profile as AgentProfile) ?? null,
        allMessages: mappedMessages,
        messages,
        displayCount,
        hasMoreMessages: mappedMessages.length > displayCount || cached.cursor !== null,
        cursor: cached.cursor,
        messagesLoading: false,
        isStreamingAreaVisible: true,
        pendingMessageIds: new Set<string>(),
        seenMessageIds: new Set<string>(),
        messageIdByTs: {},
        messageCache: cache,
      });

      // Background refresh: fetch fresh data without blocking render
      Promise.all([api.getAgent(agentId, controller.signal), api.getMessages(agentId, threadId, controller.signal)])
        .then(([profile, response]) => {
          // Superseded by a newer selection (agent or thread) — swallow
          // silently rather than overwriting whatever the user is now
          // looking at (or clobbering it with a background refresh whose
          // result no longer applies).
          if (isStale()) return;

          const freshMessages = inferMessageStatuses(response.messages);
          const freshCursor = response.cursor;
          const currentDisplayCount = get().displayCount;
          const freshDisplayMessages = freshMessages.slice(-currentDisplayCount);

          const bgCache = new Map(get().messageCache);
          bgCache.set(cacheKey, {
            allMessages: freshMessages,
            displayCount: currentDisplayCount,
            lastAccessed: Date.now(),
            cursor: freshCursor,
            profile,
          });

          set({
            selectedAgentProfile: profile,
            allMessages: freshMessages,
            messages: freshDisplayMessages,
            hasMoreMessages: freshMessages.length > currentDisplayCount || freshCursor !== null,
            cursor: freshCursor,
            messageCache: bgCache,
          });
        })
        .catch((err) => {
          // A superseded request's rejection (whether from our own abort()
          // above or a genuine 404/500 that arrived too late to matter) is
          // expected control flow, not a real error — only log when this
          // fetch is still the one the user actually cares about.
          if (isStale()) return;
          console.error("[chatStore] background refresh failed:", err);
        });
    } else {
      // Cache miss: hydrate profile from snapshot immediately to avoid flash,
      // then fetch full data from API.
      const snapshot = get().agents.find((a) => a.agent_id === agentId);
      const snapshotProfile: AgentProfile | null = snapshot
        ? {
            id: snapshot.agent_id,
            name: snapshot.name,
            emoji: snapshot.emoji,
            description: "",
            provider: {
              type: "", command: "", args: [], output_format: "", input_mode: "",
              model_aliases: {}, resume_args: [], session_id_fields: [],
              clear_env: false, no_output_timeout_ms: 0,
            },
            model: null,
            skills: [],
            system_prompt: null,
            tools: null,
            env: {},
            max_instances: 1,
            timeout_seconds: 0,
            working_dir: null,
            home_dir: null,
            serialize: false,
          }
        : null;

      set({
        selectedAgentId: agentId,
        selectedAgentProfile: snapshotProfile,
        messages: [],
        allMessages: [],
        messagesLoading: true,
        isStreamingAreaVisible: true,
        pendingMessageIds: new Set<string>(),
        seenMessageIds: new Set<string>(),
        messageIdByTs: {},
      });
      try {
        const [profile, response] = await Promise.all([
          api.getAgent(agentId, controller.signal),
          api.getMessages(agentId, threadId, controller.signal),
        ]);
        // Superseded by a newer selection — bail before touching any
        // visible/cached state. See the module-level doc comments.
        if (isStale()) return;

        const mappedMessages: TranscriptEntry[] = inferMessageStatuses(response.messages);
        const responseCursor = response.cursor;
        const displayCount = PAGE_SIZE;
        const messages = mappedMessages.slice(-displayCount);

        // Insert into cache (with profile)
        const cache = new Map(get().messageCache);
        cache.set(inFlightKey(agentId, threadId), {
          allMessages: mappedMessages,
          displayCount,
          lastAccessed: Date.now(),
          cursor: responseCursor,
          profile,
        });
        evictLRU(cache);

        set({
          selectedAgentProfile: profile,
          allMessages: mappedMessages,
          messages,
          displayCount,
          hasMoreMessages: mappedMessages.length > displayCount || responseCursor !== null,
          cursor: responseCursor,
          messageCache: cache,
        });
      } catch (err) {
        // A superseded request's rejection — including our own abort()
        // above — is expected control flow: swallow it silently, the same
        // as the background-refresh path, rather than surfacing an error or
        // mutating `selectedThreadIdByAgent` on behalf of a selection the
        // user has already navigated away from.
        if (isStale()) return;
        // A thread id restored from a stale/different data root (or a
        // deleted thread) 404s here. Drop the stale selection and leave the
        // agent on its default "no thread selected" state instead of
        // letting the rejection go unhandled. Any other failure (network
        // error, 500, auth, or an unknown agent id) still propagates.
        if (threadId && isNotFoundError(err)) {
          set((state) => {
            const nextSelected = new Map(state.selectedThreadIdByAgent);
            nextSelected.delete(agentId);
            return { selectedThreadIdByAgent: nextSelected };
          });
        } else {
          throw err;
        }
      } finally {
        // Don't flip a newer (still in-flight) selection's own loading flag
        // off on this stale call's behalf.
        if (!isStale()) set({ messagesLoading: false });
      }
    }
  },

  loadMoreMessages: async () => {
    if (get().loadingMore) return;
    set({ loadingMore: true });

    const { selectedAgentId, allMessages, displayCount, messageCache, cursor } = get();
    const newDisplayCount = displayCount + PAGE_SIZE;
    const paginationThreadId = selectedAgentId
      ? effectiveThreadId(selectedAgentId, get().selectedThreadIdByAgent, get().threadsByAgent)
      : undefined;
    const cacheKey = selectedAgentId ? inFlightKey(selectedAgentId, paginationThreadId) : null;

    // If we still have local messages to show, just expand the display window
    if (newDisplayCount <= allMessages.length) {
      const messages = allMessages.slice(-newDisplayCount);

      if (cacheKey) {
        const entry = messageCache.get(cacheKey);
        if (entry) {
          const cache = new Map(messageCache);
          cache.set(cacheKey, {
            ...entry,
            displayCount: newDisplayCount,
            lastAccessed: Date.now(),
          });
          set({
            loadingMore: false,
            displayCount: newDisplayCount,
            messages,
            hasMoreMessages: allMessages.length > newDisplayCount || cursor !== null,
            messageCache: cache,
          });
          return;
        }
      }

      set({
        loadingMore: false,
        displayCount: newDisplayCount,
        messages,
        hasMoreMessages: allMessages.length > newDisplayCount || cursor !== null,
      });
      return;
    }

    // Local cache exhausted — fetch more from the API if cursor is available
    if (!cursor || !selectedAgentId || !cacheKey) {
      set({ loadingMore: false, hasMoreMessages: false });
      return;
    }

    // Capture the generation and reuse the active selection's own
    // AbortController — this pagination fetch is a continuation of whatever
    // selection is currently loaded, not a new one, so a superseding
    // `selectAgent` call cancels it via the SAME `.abort()` call that cancels
    // the selection's own fetch, in addition to the generation check below
    // catching it on arrival. See `selectAgent`'s doc comments.
    const myGeneration = selectionGeneration;
    const signal = selectionAbortController?.signal;

    try {
      const response = selectedAgentId.startsWith("project:")
        ? await api.getProjectMessages(selectedAgentId.slice("project:".length), cursor, PAGE_SIZE, signal)
        : await api.getMessagesBefore(selectedAgentId, cursor, PAGE_SIZE, paginationThreadId, signal);
      // Bail if the selection has moved on since this fetch was dispatched —
      // an agent switch, a thread switch within the same agent, or a fresh
      // re-selection of the very same (agent, thread) pair.
      if (myGeneration !== selectionGeneration) {
        set({ loadingMore: false });
        return;
      }

      const fetchedMessages = response.messages;
      const newCursor = response.cursor;

      // Prepend fetched messages to existing allMessages
      const updatedAllMessages = [...fetchedMessages, ...get().allMessages];
      const updatedDisplayCount = get().displayCount + fetchedMessages.length;
      const updatedMessages = updatedAllMessages.slice(-updatedDisplayCount);

      const existingEntry = get().messageCache.get(cacheKey);
      const cache = new Map(get().messageCache);
      cache.set(cacheKey, {
        allMessages: updatedAllMessages,
        displayCount: updatedDisplayCount,
        lastAccessed: Date.now(),
        cursor: newCursor,
        profile: existingEntry?.profile,
      });

      set({
        loadingMore: false,
        allMessages: updatedAllMessages,
        messages: updatedMessages,
        displayCount: updatedDisplayCount,
        hasMoreMessages: updatedAllMessages.length > updatedDisplayCount || newCursor !== null,
        cursor: newCursor,
        messageCache: cache,
      });
    } catch (err) {
      // Superseded (including our own abort() firing) — expected control
      // flow, not a real error; swallow silently rather than logging.
      if (myGeneration !== selectionGeneration) {
        set({ loadingMore: false });
        return;
      }
      console.error("[chatStore] loadMoreMessages API call failed:", err);
      set({ loadingMore: false });
    }
  },

  sendMessage: async (content: string, attachmentIds?: string[], attachments?: Attachment[], focusPath?: string | null) => {
    const { selectedAgentId, allMessages, messages, messageCache } = get();
    if (!selectedAgentId) return;

    // Resolved once, up front, and reused both for the cache key below and
    // for the backend send further down.
    const sendThreadId = effectiveThreadId(selectedAgentId, get().selectedThreadIdByAgent, get().threadsByAgent);
    const cacheKey = inFlightKey(selectedAgentId, sendThreadId);

    // Optimistic user message — include attachment metadata so bubbles render immediately
    const optimisticMetadata: Record<string, unknown> = {};
    if (attachments && attachments.length > 0) {
      optimisticMetadata.attachments = attachments;
    }

    const optimisticEntry: TranscriptEntry = {
      ts: new Date().toISOString(),
      role: "user",
      content,
      event_type: "message",
      status: "sending",
      ...(Object.keys(optimisticMetadata).length > 0 ? { metadata: optimisticMetadata } : {}),
    };

    const newAllMessages = [...allMessages, optimisticEntry];
    const newMessages = [...messages, optimisticEntry];

    // Update cache entry with optimistic message
    const entry = messageCache.get(cacheKey);
    if (entry) {
      const cache = new Map(messageCache);
      cache.set(cacheKey, {
        ...entry,
        allMessages: newAllMessages,
        displayCount: get().displayCount + 1,
        lastAccessed: Date.now(),
      });
      set({
        allMessages: newAllMessages,
        messages: newMessages,
        displayCount: get().displayCount + 1,
        scrollToBottomTrigger: get().scrollToBottomTrigger + 1,
        messageCache: cache,
      });
    } else {
      set({
        allMessages: newAllMessages,
        messages: newMessages,
        displayCount: get().displayCount + 1,
        scrollToBottomTrigger: get().scrollToBottomTrigger + 1,
      });
    }

    // Send to backend and track message_id

    // The user sending anything into this thread means they've moved past
    // whatever sync `AskUserQuestionWithForm` question was pending here —
    // an explicit action, unlike a run simply ending, so deleting outright
    // (rather than `markPendingFormOrphaned`'s keep-and-flag) is correct
    // here: clear it so the `?` badge (`ThreadTabStrip`/`HomeSidebar`, via
    // `resolveThreadActivity`) doesn't stay stuck forever when the operator
    // answers by just typing instead of using the form overlay. Scoped to
    // this thread only — see `clearPendingForm`.
    get().clearPendingForm(selectedAgentId, sendThreadId);

    // Raise the typing indicator immediately rather than waiting for the
    // server's RunStarted to travel back down the shared SSE stream. That
    // round-trip is usually a few ms, but if the single shared connection is
    // mid-reconnect (laptop wake, network blip, or an ao-server restart) the
    // live RunStarted is missed and the dots only appear once the stream
    // re-establishes and replays AgentBusy — which reads as send lag. This is
    // idempotent: the real RunStarted / replayed AgentBusy no-ops on arrival
    // (ensureInFlight guards on isTyping and preserves the buffer). A watchdog
    // retracts it if the run never actually starts.
    const flightKey = inFlightKey(selectedAgentId, sendThreadId);
    get().ensureInFlight(flightKey);
    armOptimisticTypingWatchdog(
      (k) => get().inFlightByAgent.get(k),
      (k) => get().deleteInFlight(k),
      flightKey,
    );

    let ack: Awaited<ReturnType<typeof api.sendMessage>>;
    try {
      ack = await api.sendMessage(selectedAgentId, content, attachmentIds, focusPath, sendThreadId);
    } catch (err) {
      // The send itself failed, so no run will start — retract the optimistic
      // typing indicator we just raised instead of leaving the dots spinning,
      // then rethrow so existing caller-side error handling still runs.
      clearOptimisticTypingWatchdog(flightKey);
      get().deleteInFlight(flightKey);
      throw err;
    }
    set((state) => ({
      pendingMessageIds: new Set(state.pendingMessageIds).add(ack.message_id),
      messageIdByTs: { ...state.messageIdByTs, [optimisticEntry.ts]: ack.message_id },
    }));

    // Refresh agent list so the sidebar reflects the new message (e.g. first message to a new agent)
    await get().fetchAgents();
    // Re-mark read after refresh so fast agent responses don't flash the unread dot
    useReadStatusStore.getState().markRead(selectedAgentId);
  },

  ensureInFlight: (agentId: string) => {
    const existing = inFlightDeleteTimers.get(agentId);
    if (existing) {
      clearTimeout(existing);
      inFlightDeleteTimers.delete(agentId);
    }
    set((state) => {
      const current = state.inFlightByAgent.get(agentId);
      if (current && current.isTyping) return state;
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, {
        textBuffer: current?.textBuffer ?? "",
        activeToolCalls: current?.activeToolCalls ?? EMPTY_TOOL_CALLS,
        isTyping: true,
        startedAt: current?.startedAt ?? Date.now(),
        artifactIds: current?.artifactIds ?? EMPTY_ARTIFACT_IDS,
        everShownThisTurn: current?.everShownThisTurn ?? false,
        ...preserveThinking(current),
      });
      return { inFlightByAgent: next };
    });
  },

  appendInFlightDelta: (agentId: string, text: string) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId);
      const next = new Map(state.inFlightByAgent);
      // Receiving a text_delta is the implicit signal that any thinking
      // block which lacked an explicit `thinking_ended` is now closed —
      // the model has moved on to user-facing output. If thinking was
      // still flagged active, snapshot the elapsed time so the pill can
      // collapse to "Thought for Ns" without waiting for the missing end
      // event. Buffer survives so the disclosure can still be expanded.
      const wasThinking = current?.thinkingActive ?? false;
      const startedAt = current?.thinkingStartedAt ?? null;
      const closedThinking = wasThinking && startedAt != null
        ? {
            thinkingActive: false,
            thinkingBuffer: current?.thinkingBuffer ?? "",
            thinkingStartedAt: startedAt,
            thinkingElapsedMs:
              current?.thinkingElapsedMs ?? Math.max(0, Date.now() - startedAt),
            thinkingShown: current?.thinkingShown ?? true,
          }
        : preserveThinking(current);
      next.set(agentId, {
        textBuffer: (current?.textBuffer ?? "") + text,
        activeToolCalls: current?.activeToolCalls ?? EMPTY_TOOL_CALLS,
        isTyping: true,
        startedAt: current?.startedAt ?? Date.now(),
        artifactIds: current?.artifactIds ?? EMPTY_ARTIFACT_IDS,
        everShownThisTurn: (current?.everShownThisTurn ?? false) || text.length > 0,
        ...closedThinking,
      });
      return { inFlightByAgent: next };
    });
  },

  finalizeInFlightText: (key: string, text: string) => {
    // `key` may be a plain agent id or an agent+thread composite (see
    // `inFlightKey`) — `inFlightByAgent` is keyed by it directly, but
    // `messages`/`allMessages`/`messageCache` are per-agent (holding whichever
    // thread is currently loaded), so those need the unwrapped agent id plus
    // an explicit check that the composite key's thread is the one actually
    // loaded right now. Without this unwrap, `selectedAgentId === key` would
    // never match for a non-default thread (selectedAgentId is always a
    // plain id), silently dropping every finalized reply on a fresh/branch
    // thread from the visible transcript until the next refetch.
    const agentId = agentIdFromInFlightKey(key);
    const eventThreadId = threadIdFromInFlightKey(key);
    const { selectedAgentId, allMessages, messages, messageCache, threadsByAgent, selectedThreadIdByAgent, inFlightByAgent } = get();
    // Snapshot whatever artifact ids landed on this turn's in-flight entry
    // (via `appendInFlightArtifactId`, live from `tool_call_completed`) and
    // carry them onto the finalized entry's metadata — otherwise the card
    // would vanish the instant the bubble hands off from the streaming
    // buffer to the persisted transcript array. No dependency on turn_id or
    // a later refetch: this is the same id the live card already rendered.
    const finalizedArtifactIds = inFlightByAgent.get(key)?.artifactIds ?? EMPTY_ARTIFACT_IDS;
    const finalEntry: TranscriptEntry = {
      ts: new Date().toISOString(),
      role: { agent: agentId },
      content: text,
      event_type: "message",
      ...(finalizedArtifactIds.length > 0 ? { metadata: { artifact_ids: finalizedArtifactIds } } : {}),
    };

    const onActiveThread = isEventForActiveThread(agentId, eventThreadId, threadsByAgent, selectedThreadIdByAgent);
    const isSelected = selectedAgentId === agentId && onActiveThread;
    const newAllMessages = isSelected ? [...allMessages, finalEntry] : allMessages;
    const newMessages = isSelected ? [...messages, finalEntry] : messages;

    // Update the cache even when the agent isn't currently selected so
    // returning to this thread shows the finalized message without a
    // refetch — but only when the entry belongs to the thread the cache
    // actually holds a (per-thread) entry for. `key` is already exactly
    // `inFlightKey(agentId, eventThreadId)` (that's what `agentIdFromInFlightKey`/
    // `threadIdFromInFlightKey` decomposed it from above), so it doubles as
    // the correct per-thread `messageCache` key with no recomputation.
    const cachedEntry = onActiveThread ? messageCache.get(key) : undefined;
    let cache = messageCache;
    if (cachedEntry) {
      cache = new Map(messageCache);
      cache.set(key, {
        ...cachedEntry,
        allMessages: [...cachedEntry.allMessages, finalEntry],
        displayCount: cachedEntry.displayCount + 1,
        lastAccessed: Date.now(),
      });
    }

    set((state) => {
      const current = state.inFlightByAgent.get(key);
      const next = new Map(state.inFlightByAgent);
      // Keep the entry alive (empty buffer, no tool calls) so the streaming
      // bubble stays mounted across a skill-load RunEnded → RunStarted gap.
      // Drop `isTyping` here so the warmup pill above the input doesn't flash
      // back on between TextComplete and RunEnded (~10-80ms gap on the API
      // path while persist_pending awaits disk). The bubble's mount is gated
      // on streamingText/tools/prefix, NOT isTyping, so the skill-load handoff
      // still works — ensureInFlight re-flips isTyping=true on the next
      // RunStarted within the 400ms teardown window. If a follow-up tool_use
      // arrives mid-turn the bubble re-mounts via activeToolCalls.
      next.set(key, {
        textBuffer: "",
        activeToolCalls: EMPTY_TOOL_CALLS,
        isTyping: false,
        startedAt: current?.startedAt ?? Date.now(),
        // Drop thinking state on finalize — the next turn starts with a
        // fresh reasoning channel, and thinking text does not survive into
        // the finalized message bubble.
        ...EMPTY_THINKING,
        // Already snapshotted onto `finalEntry.metadata.artifact_ids` above —
        // the next turn's ArtifactWrite calls start a fresh list.
        artifactIds: EMPTY_ARTIFACT_IDS,
        // Preserve (never reset here) — the entry is kept alive across a
        // skill-load RunEnded -> RunStarted gap (see comment above), which
        // is exactly the kind of momentary content-empty window this latch
        // exists to survive.
        everShownThisTurn: current?.everShownThisTurn ?? false,
      });
      // A reply that finalizes while its thread isn't the one currently
      // visible (different thread of the same agent, a different agent
      // entirely, or no agent selected at all) leaves an unread marker for
      // ThreadTabStrip to surface — cleared by `markThreadViewed` once the
      // user actually navigates there. `key` is already the exact composite
      // shape (`inFlightKey`) that marker set uses.
      const nextUnread = new Set(state.unreadThreadIds);
      if (isSelected) nextUnread.delete(key);
      else nextUnread.add(key);
      return {
        ...(isSelected
          ? {
              allMessages: newAllMessages,
              messages: newMessages,
              displayCount: state.displayCount + 1,
              isStreamingAreaVisible: true,
            }
          : {}),
        inFlightByAgent: next,
        messageCache: cache,
        unreadThreadIds: nextUnread,
      };
    });

    // Refresh sidebar so it reflects the agent's latest reply
    get().fetchAgents();

    // NB: the inline card itself is already showing by this point — it was
    // attached to `finalEntry.metadata.artifact_ids` above, straight from
    // whatever `appendInFlightArtifactId` collected while the turn streamed.
    // `syncRunArtifacts` below only refreshes the Assets panel's list.
  },

  syncRunArtifacts: (key: string) => {
    const agentId = agentIdFromInFlightKey(key);
    void syncArtifactsAfterRun(agentId).catch((err) => {
      console.error("[chatStore] syncArtifactsAfterRun failed:", err);
    });
  },

  setInFlightTyping: (agentId: string, typing: boolean) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId);
      if (!current && !typing) return state;
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, {
        textBuffer: current?.textBuffer ?? "",
        activeToolCalls: current?.activeToolCalls ?? EMPTY_TOOL_CALLS,
        isTyping: typing,
        startedAt: current?.startedAt ?? Date.now(),
        artifactIds: current?.artifactIds ?? EMPTY_ARTIFACT_IDS,
        everShownThisTurn: current?.everShownThisTurn ?? false,
        ...preserveThinking(current),
      });
      return {
        inFlightByAgent: next,
        ...(typing ? {} : { isStreamingAreaVisible: true }),
      };
    });
  },

  addInFlightToolCall: (agentId: string, toolCall: { tool: string; input?: Record<string, unknown>; label?: string }) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId) ?? {
        textBuffer: "",
        activeToolCalls: EMPTY_TOOL_CALLS,
        isTyping: true,
        startedAt: Date.now(),
        artifactIds: EMPTY_ARTIFACT_IDS,
        ...EMPTY_THINKING,
      };
      const withTs: ActiveToolCall = { ...toolCall, startedAt: Date.now() };
      let nextCalls = current.activeToolCalls;
      if (toolCall.input) {
        for (let i = current.activeToolCalls.length - 1; i >= 0; i--) {
          const tc = current.activeToolCalls[i];
          // `!tc.done` excludes an already-finished chip from the merge — a
          // repeated call for the same tool (e.g. two separate `Read`s later
          // in the same turn) must not revive a completed chip's identity,
          // it needs its own fresh entry.
          if (tc.tool === toolCall.tool && !tc.input && !tc.done) {
            nextCalls = [...current.activeToolCalls];
            // Carry a Layer-2 override label forward across the input-fill merge:
            // the input-bearing event's label wins, but if it has none we keep
            // whatever the input-less chip already carried so the override is
            // not dropped when the input arrives in a later event.
            nextCalls[i] = { ...withTs, startedAt: tc.startedAt, label: withTs.label ?? tc.label };
            break;
          }
        }
      }
      if (nextCalls === current.activeToolCalls) {
        nextCalls = [...current.activeToolCalls, withTs];
      }
      nextCalls = capClassicToolCalls(nextCalls);
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, { ...current, activeToolCalls: nextCalls, everShownThisTurn: true });
      return { inFlightByAgent: next };
    });
  },

  markInFlightToolCallDone: (agentId: string) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId);
      if (!current) return state;
      const idx = current.activeToolCalls.findIndex((tc) => tc.action_id == null && !tc.done);
      if (idx === -1) return state;
      const nextCalls = [...current.activeToolCalls];
      nextCalls[idx] = { ...nextCalls[idx], done: true };
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, { ...current, activeToolCalls: nextCalls });
      return { inFlightByAgent: next };
    });
  },

  appendInFlightArtifactId: (agentId: string, artifactId: string) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId) ?? {
        textBuffer: "",
        activeToolCalls: EMPTY_TOOL_CALLS,
        isTyping: true,
        startedAt: Date.now(),
        artifactIds: EMPTY_ARTIFACT_IDS,
        ...EMPTY_THINKING,
      };
      if (current.artifactIds.includes(artifactId)) return state;
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, { ...current, artifactIds: [...current.artifactIds, artifactId], everShownThisTurn: true });
      return { inFlightByAgent: next };
    });
  },

  addInFlightToolUse: (agentId: string, toolUseId: string, toolName: string, input?: Record<string, unknown>) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId) ?? {
        textBuffer: "",
        activeToolCalls: EMPTY_TOOL_CALLS,
        isTyping: true,
        startedAt: Date.now(),
        artifactIds: EMPTY_ARTIFACT_IDS,
        ...EMPTY_THINKING,
      };
      if (current.activeToolCalls.some((tc) => tc.action_id === toolUseId)) {
        return state;
      }
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, {
        ...current,
        activeToolCalls: [
          ...current.activeToolCalls,
          { tool: toolName, startedAt: Date.now(), action_id: toolUseId, input },
        ],
        everShownThisTurn: true,
      });
      return { inFlightByAgent: next };
    });
  },

  addInFlightAgentAction: (agentId: string, actionId: string, label: string) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId) ?? {
        textBuffer: "",
        activeToolCalls: EMPTY_TOOL_CALLS,
        isTyping: true,
        startedAt: Date.now(),
        artifactIds: EMPTY_ARTIFACT_IDS,
        ...EMPTY_THINKING,
      };
      if (current.activeToolCalls.some((tc) => tc.action_id === actionId)) {
        return state;
      }
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, {
        ...current,
        activeToolCalls: [
          ...current.activeToolCalls,
          { tool: "", startedAt: Date.now(), action_id: actionId, label },
        ],
        everShownThisTurn: true,
      });
      return { inFlightByAgent: next };
    });
  },

  removeInFlightAgentAction: (agentId: string, actionId: string) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId);
      if (!current) return state;
      const filtered = current.activeToolCalls.filter((tc) => tc.action_id !== actionId);
      if (filtered.length === current.activeToolCalls.length) return state;
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, { ...current, activeToolCalls: filtered });
      return { inFlightByAgent: next };
    });
  },

  clearInFlightToolCalls: (agentId: string) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId);
      if (!current || current.activeToolCalls.length === 0) return state;
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, { ...current, activeToolCalls: EMPTY_TOOL_CALLS });
      return { inFlightByAgent: next };
    });
  },

  patchTodoCreateProgress: (agentId: string, label: string) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId);
      if (!current) return state;
      const idx = current.activeToolCalls.findIndex((tc) => tc.tool === "TodoCreate");
      if (idx < 0) return state;
      const updated = [...current.activeToolCalls];
      updated[idx] = { ...updated[idx], label };
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, { ...current, activeToolCalls: updated });
      return { inFlightByAgent: next };
    });
  },

  setActivePanel: (agentId: string, panel: ChatSidePanel | null) => {
    set((state) => {
      const next = new Map(state.activePanelByAgent);
      if (panel === null) next.delete(agentId);
      else next.set(agentId, panel);
      return { activePanelByAgent: next };
    });
  },

  setChannelsColumnPinned: (agentId: string, pinned: boolean) => {
    set((state) => {
      const next = new Map(state.channelsColumnPinnedByAgent);
      if (!pinned) next.delete(agentId);
      else next.set(agentId, true);
      return { channelsColumnPinnedByAgent: next };
    });
  },

  setAssignmentsColumnPinned: (agentId: string, pinned: boolean) => {
    set((state) => {
      const next = new Map(state.assignmentsColumnPinnedByAgent);
      if (!pinned) next.delete(agentId);
      else next.set(agentId, true);
      return { assignmentsColumnPinnedByAgent: next };
    });
  },

  scheduleInFlightTeardown: (agentId: string) => {
    const existing = inFlightDeleteTimers.get(agentId);
    if (existing) clearTimeout(existing);
    const timer = setTimeout(() => {
      inFlightDeleteTimers.delete(agentId);
      get().deleteInFlight(agentId);
    }, IN_FLIGHT_TEARDOWN_DELAY_MS);
    inFlightDeleteTimers.set(agentId, timer);
    // Clear typing immediately so the dots stop — if another RunStarted
    // arrives first, ensureInFlight will flip it back before the timer fires.
    get().setInFlightTyping(agentId, false);
  },

  deleteInFlight: (agentId: string) => {
    const existing = inFlightDeleteTimers.get(agentId);
    if (existing) {
      clearTimeout(existing);
      inFlightDeleteTimers.delete(agentId);
    }
    clearOptimisticTypingWatchdog(agentId);
    set((state) => {
      const hasEntry = state.inFlightByAgent.has(agentId);
      const hasUsage = state.usageByAgent.has(agentId);
      if (!hasEntry && !hasUsage) return state;
      const next = new Map(state.inFlightByAgent);
      next.delete(agentId);
      const nextUsage = new Map(state.usageByAgent);
      nextUsage.delete(agentId);
      return { inFlightByAgent: next, usageByAgent: nextUsage };
    });
  },

  accumulateUsage: (agentId: string, partial: TurnUsage) => {
    set((state) => {
      const prev = state.usageByAgent.get(agentId);
      const summed: TurnUsage = prev
        ? {
            input: prev.input + partial.input,
            output: prev.output + partial.output,
            cacheRead: prev.cacheRead + partial.cacheRead,
            cacheCreation: prev.cacheCreation + partial.cacheCreation,
            total: prev.total + partial.total,
          }
        : { ...partial };
      const next = new Map(state.usageByAgent);
      next.set(agentId, summed);
      return { usageByAgent: next };
    });
  },

  startInFlightThinking: (agentId: string) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId);
      // Idempotent: if a block is already open, leave its start time intact
      // so a stray duplicate event from the wire doesn't reset the clock.
      if (current?.thinkingActive) return state;
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, {
        textBuffer: current?.textBuffer ?? "",
        activeToolCalls: current?.activeToolCalls ?? EMPTY_TOOL_CALLS,
        isTyping: current?.isTyping ?? true,
        startedAt: current?.startedAt ?? Date.now(),
        artifactIds: current?.artifactIds ?? EMPTY_ARTIFACT_IDS,
        // Open the new block but keep any prior buffer — a turn can have
        // multiple thinking blocks in sequence on long reasoning chains
        // and the disclosure is more useful as a cumulative trace.
        thinkingActive: true,
        thinkingBuffer: current?.thinkingBuffer ?? "",
        thinkingStartedAt: Date.now(),
        thinkingElapsedMs: current?.thinkingElapsedMs ?? null,
        thinkingShown: true,
        everShownThisTurn: true,
      });
      return { inFlightByAgent: next };
    });
  },

  appendInFlightThinkingDelta: (agentId: string, text: string) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId);
      const next = new Map(state.inFlightByAgent);
      // Synthesize a start if the wire didn't carry one — protects against
      // normalizers that emit delta-only streams.
      const wasActive = current?.thinkingActive ?? false;
      next.set(agentId, {
        textBuffer: current?.textBuffer ?? "",
        activeToolCalls: current?.activeToolCalls ?? EMPTY_TOOL_CALLS,
        isTyping: current?.isTyping ?? true,
        startedAt: current?.startedAt ?? Date.now(),
        artifactIds: current?.artifactIds ?? EMPTY_ARTIFACT_IDS,
        thinkingActive: true,
        thinkingBuffer: (current?.thinkingBuffer ?? "") + text,
        thinkingStartedAt: wasActive
          ? current?.thinkingStartedAt ?? Date.now()
          : Date.now(),
        thinkingElapsedMs: null,
        thinkingShown: true,
        everShownThisTurn: true,
      });
      return { inFlightByAgent: next };
    });
  },

  endInFlightThinking: (agentId: string, elapsedMs: number) => {
    set((state) => {
      const current = state.inFlightByAgent.get(agentId);
      if (!current) return state;
      const next = new Map(state.inFlightByAgent);
      next.set(agentId, {
        ...current,
        thinkingActive: false,
        // Trust the server's elapsed value — it was measured at the
        // normalizer with a single monotonic clock. Falling back to a
        // wall-clock subtraction is the appendInFlightDelta auto-close
        // path, not this one.
        thinkingElapsedMs: elapsedMs,
      });
      return { inFlightByAgent: next };
    });
  },

  markMessageSent: (messageId: string) => {
    const { selectedAgentId, selectedThreadIdByAgent, threadsByAgent, messageCache } = get();
    set((state) => ({
      pendingMessageIds: new Set(state.pendingMessageIds).add(messageId),
    }));

    // Update cache entry lastAccessed
    if (selectedAgentId) {
      const cacheKey = inFlightKey(selectedAgentId, effectiveThreadId(selectedAgentId, selectedThreadIdByAgent, threadsByAgent));
      const entry = messageCache.get(cacheKey);
      if (entry) {
        const cache = new Map(messageCache);
        cache.set(cacheKey, { ...entry, lastAccessed: Date.now() });
        set({ messageCache: cache });
      }
    }
  },

  markMessageSeen: (messageId: string) => {
    const { selectedAgentId, selectedThreadIdByAgent, threadsByAgent, messageCache } = get();
    set((state) => {
      const newPending = new Set(state.pendingMessageIds);
      newPending.delete(messageId);
      return {
        pendingMessageIds: newPending,
        seenMessageIds: new Set(state.seenMessageIds).add(messageId),
      };
    });

    // Update cache entry lastAccessed
    if (selectedAgentId) {
      const cacheKey = inFlightKey(selectedAgentId, effectiveThreadId(selectedAgentId, selectedThreadIdByAgent, threadsByAgent));
      const entry = messageCache.get(cacheKey);
      if (entry) {
        const cache = new Map(messageCache);
        cache.set(cacheKey, { ...entry, lastAccessed: Date.now() });
        set({ messageCache: cache });
      }
    }
  },

  setStreamingAreaVisible: (visible: boolean) => {
    set({ isStreamingAreaVisible: visible });
  },

  patchAgentSnapshot: (agentId: string, patch: Partial<AgentSnapshot>) => {
    set((state) => ({
      agents: state.agents.map((a) =>
        a.agent_id === agentId ? { ...a, ...patch } : a
      ),
    }));
  },

  clearPendingAsyncForm: (agentId: string, formId: string) => {
    set((state) => ({
      agents: state.agents.map((a) =>
        a.agent_id === agentId
          ? { ...a, pending_forms: (a.pending_forms ?? []).filter((f) => f.form_id !== formId) }
          : a
      ),
    }));
  },

  cancelRun: async () => {
    const agentId = get().selectedAgentId;
    if (!agentId) return;
    // Scope the cancel to the thread the user is actually looking at — an
    // agent can have concurrent runs across multiple threads, and cancelling
    // one must not stop the others. Mirrors how sendMessage resolves the
    // thread to send to.
    const threadId = effectiveThreadId(agentId, get().selectedThreadIdByAgent, get().threadsByAgent);
    await api.cancelAgentRun(agentId, threadId);
    get().deleteInFlight(inFlightKey(agentId, threadId));
    // A stop is an explicit user action, so clear this thread's pending sync
    // form outright rather than relying solely on the `run_ended` SSE
    // event's own handling (useSSE.ts, which marks the slot orphaned instead
    // of deleting it) — that event can race or drop entirely on a stop (the
    // run may terminate without ever emitting one), which otherwise leaves
    // the `?` badge and the form-answer composer stuck forever. Harmless
    // belt-and-suspenders if `run_ended` does still arrive afterward — its
    // orphan-marking is a no-op against an already-cleared slot.
    get().clearPendingForm(agentId, threadId);
    set((state) => ({
      isStreamingAreaVisible: true,
      messages: [
        ...state.messages,
        {
          ts: new Date().toISOString(),
          role: "system" as const,
          content: "Run cancelled",
          event_type: "system" as const,
        },
      ],
    }));
  },

  loadMessagesAroundTimestamp: async (agentId: string, targetTs: string) => {
    try {
      const response = await api.getMessagesAround(agentId, targetTs);
      // Only apply if user is still on the same agent
      if (get().selectedAgentId !== agentId) return;

      const mappedMessages = inferMessageStatuses(response.messages);

      // Replace the current message window with messages around the target
      const threadId = effectiveThreadId(agentId, get().selectedThreadIdByAgent, get().threadsByAgent);
      const cacheKey = inFlightKey(agentId, threadId);
      const cache = new Map(get().messageCache);
      const existing = cache.get(cacheKey);
      cache.set(cacheKey, {
        allMessages: mappedMessages,
        displayCount: mappedMessages.length,
        lastAccessed: Date.now(),
        cursor: null, // no cursor-based pagination for around-timestamp loads
        profile: existing?.profile ?? null,
      });

      set({
        allMessages: mappedMessages,
        messages: mappedMessages,
        displayCount: mappedMessages.length,
        hasMoreMessages: false,
        cursor: null,
        messageCache: cache,
      });
    } catch (err) {
      console.error("[chatStore] loadMessagesAroundTimestamp failed:", err);
    }
  },

  /** Drops EVERY cached entry belonging to `agentId` — every thread of it,
   *  plus its default-thread/virtual-channel entry (the bare `agentId` key).
   *  Signature unchanged for existing callers (ChatHeader's manual refresh,
   *  etc.); only the internals changed once `messageCache` became per-thread
   *  (see that field's doc comment) — it used to be a single `Map.delete`
   *  since one agent could only ever occupy one entry. */
  invalidateCache: (agentId: string) => {
    const cache = new Map(get().messageCache);
    const threadPrefix = `${agentId}${THREAD_KEY_SEPARATOR}`;
    for (const key of cache.keys()) {
      if (key === agentId || key.startsWith(threadPrefix)) cache.delete(key);
    }
    set({ messageCache: cache });
  },

  loadThreads: async (agentId: string, signal?: AbortSignal) => {
    const threads = await api.listThreads(agentId, signal);
    set((state) => {
      const nextThreads = new Map(state.threadsByAgent);
      nextThreads.set(agentId, threads);
      // Only set a selection if none has been explicitly chosen for this agent yet.
      const alreadySelected = state.selectedThreadIdByAgent.get(agentId);
      if (alreadySelected) return { threadsByAgent: nextThreads };
      const defaultThread = threads.find((t) => t.kind === "default");
      const defaultId = defaultThread?.id ?? `default-${agentId}`;
      const nextSelected = new Map(state.selectedThreadIdByAgent);
      nextSelected.set(agentId, defaultId);
      return { threadsByAgent: nextThreads, selectedThreadIdByAgent: nextSelected };
    });
  },

  loadAllThreads: async (knownAgentIds: string[], signal?: AbortSignal) => {
    const myGeneration = ++threadsHydrationGeneration;
    // A fresh attempt supersedes whatever error the last one left behind —
    // cleared unconditionally (not gated by the generation check below)
    // since this IS the newest dispatch by construction. If it fails too,
    // the catch block below sets it right back.
    set({ threadsHydrationError: null });
    try {
      const grouped = await api.listAllThreads(signal);
      // Stale guard: a newer `loadAllThreads` call has been dispatched since
      // this one — its result should win, so drop this older response on
      // the floor rather than letting an out-of-order resolve clobber
      // fresher state.
      if (myGeneration !== threadsHydrationGeneration) return;
      set((state) => {
        const nextThreads = new Map(state.threadsByAgent);
        for (const [agentId, threads] of Object.entries(grouped)) {
          nextThreads.set(agentId, threads);
        }
        // The grouped response omits agents with zero threads entirely, but
        // `threadsByAgent` membership is used elsewhere as a proxy for "has
        // this agent been hydrated yet". Seed an empty array for every known
        // agent id missing from the response so membership means "hydrated",
        // not "has threads" — otherwise a legitimately-thread-less agent
        // looks permanently un-hydrated and can drive an infinite refetch
        // loop or a spinner that never clears. SUCCESS PATH ONLY — the catch
        // block below must never do this (see `threadsHydrationError`'s doc
        // comment for why: seeding empties on failure is exactly what makes
        // a failed fetch indistinguishable from a genuinely-empty one).
        for (const agentId of knownAgentIds) {
          if (!nextThreads.has(agentId)) nextThreads.set(agentId, []);
        }
        return { threadsByAgent: nextThreads };
      });
    } catch (err) {
      if (myGeneration !== threadsHydrationGeneration) return;
      // A caller-initiated abort (HomeSidebar unmounting or re-firing this
      // effect on a dependency change) is normal control flow, not a
      // failure — must not paint the error state.
      if (err instanceof DOMException && err.name === "AbortError") return;
      console.error("[chatStore] loadAllThreads failed:", err);
      set({ threadsHydrationError: err instanceof Error ? err.message : String(err) });
    }
  },

  selectThread: (threadId: string) => {
    set((state) => {
      const nextSelected = new Map(state.selectedThreadIdByAgent);
      // Find the owning agent from the loaded thread lists
      let found = false;
      for (const [agentId, threads] of state.threadsByAgent) {
        if (threads.some((t) => t.id === threadId)) {
          nextSelected.set(agentId, threadId);
          found = true;
          break;
        }
      }
      // Fallback: associate with the currently selected agent (e.g. thread not yet in store)
      if (!found && state.selectedAgentId) {
        nextSelected.set(state.selectedAgentId, threadId);
      }
      return { selectedThreadIdByAgent: nextSelected };
    });
  },

  selectThreadForAgent: (agentId: string, threadId: string) => {
    set((state) => {
      const nextSelected = new Map(state.selectedThreadIdByAgent);
      nextSelected.set(agentId, threadId);
      return { selectedThreadIdByAgent: nextSelected };
    });
  },

  archiveThread: async (threadId: string) => {
    const updated = await api.archiveThread(threadId);
    set((state) => {
      const nextThreads = new Map(state.threadsByAgent);
      for (const [agentId, threads] of nextThreads) {
        const idx = threads.findIndex((t) => t.id === threadId);
        if (idx >= 0) {
          const next = [...threads];
          next[idx] = updated;
          nextThreads.set(agentId, next);
          break;
        }
      }
      return { threadsByAgent: nextThreads };
    });
    return updated;
  },

  unarchiveThread: async (threadId: string) => {
    const updated = await api.unarchiveThread(threadId);
    set((state) => {
      const nextThreads = new Map(state.threadsByAgent);
      for (const [agentId, threads] of nextThreads) {
        const idx = threads.findIndex((t) => t.id === threadId);
        if (idx >= 0) {
          const next = [...threads];
          next[idx] = updated;
          nextThreads.set(agentId, next);
          break;
        }
      }
      return { threadsByAgent: nextThreads };
    });
    return updated;
  },

  markThreadViewed: (agentId: string, threadId: string | undefined) => {
    set((state) => {
      const key = inFlightKey(agentId, threadId);
      if (!state.unreadThreadIds.has(key)) return state;
      const next = new Set(state.unreadThreadIds);
      next.delete(key);
      return { unreadThreadIds: next };
    });
  },

  beginDelegateRun: (key: string, delegationId: string, delegateName: string, startedAt: number) => {
    set((state) => {
      const next = new Map(state.runningDelegatesByThread);
      const entries = new Map(next.get(key));
      entries.set(delegationId, { delegateName, startedAt });
      next.set(key, entries);
      return { runningDelegatesByThread: next };
    });
  },

  endDelegateRun: (key: string, delegationId: string) => {
    set((state) => {
      const current = state.runningDelegatesByThread.get(key);
      if (!current || !current.has(delegationId)) return state;
      const next = new Map(state.runningDelegatesByThread);
      if (current.size <= 1) {
        next.delete(key);
      } else {
        const entries = new Map(current);
        entries.delete(delegationId);
        next.set(key, entries);
      }
      return { runningDelegatesByThread: next };
    });
  },

  clearDelegateRunsForKey: (key: string) => {
    set((state) => {
      if (!state.runningDelegatesByThread.has(key)) return state;
      const next = new Map(state.runningDelegatesByThread);
      next.delete(key);
      return { runningDelegatesByThread: next };
    });
  },

  createFreshThread: async (agentId: string, title?: string) => {
    const thread = await api.createThread(agentId, { kind: "fresh", title: title ?? null });
    set((state) => {
      const existing = state.threadsByAgent.get(agentId) ?? [];
      const nextThreads = new Map(state.threadsByAgent);
      nextThreads.set(agentId, [...existing, thread]);
      const nextSelected = new Map(state.selectedThreadIdByAgent);
      nextSelected.set(agentId, thread.id);
      return { threadsByAgent: nextThreads, selectedThreadIdByAgent: nextSelected };
    });
    return thread;
  },

  branchThread: async (agentId: string, branchParams: { source_thread_id: string; branch_at: string; source_message_id?: string | null }, title?: string) => {
    const thread = await api.createThread(agentId, {
      kind: "branch",
      title: title ?? null,
      branch_source: {
        source_thread_id: branchParams.source_thread_id,
        branch_at: branchParams.branch_at,
        source_message_id: branchParams.source_message_id ?? null,
      },
    });
    set((state) => {
      const existing = state.threadsByAgent.get(agentId) ?? [];
      const nextThreads = new Map(state.threadsByAgent);
      nextThreads.set(agentId, [...existing, thread]);
      const nextSelected = new Map(state.selectedThreadIdByAgent);
      nextSelected.set(agentId, thread.id);
      return { threadsByAgent: nextThreads, selectedThreadIdByAgent: nextSelected };
    });
    return thread;
  },

  renameThread: async (threadId: string, title: string | null) => {
    const updated = await api.renameThread(threadId, title);
    set((state) => {
      const nextThreads = new Map(state.threadsByAgent);
      for (const [agentId, threads] of nextThreads) {
        const idx = threads.findIndex((t) => t.id === threadId);
        if (idx >= 0) {
          const next = [...threads];
          next[idx] = updated;
          nextThreads.set(agentId, next);
          break;
        }
      }
      return { threadsByAgent: nextThreads };
    });
    return updated;
  },

  patchThreadLive: (threadId: string, patch: { title?: string; auto_title?: string }) => {
    set((state) => {
      const nextThreads = new Map(state.threadsByAgent);
      for (const [agentId, threads] of nextThreads) {
        const idx = threads.findIndex((t) => t.id === threadId);
        if (idx >= 0) {
          const next = [...threads];
          next[idx] = { ...next[idx], ...patch };
          nextThreads.set(agentId, next);
          break;
        }
      }
      return { threadsByAgent: nextThreads };
    });
  },

  addThreadLive: (agentId: string, thread: Thread) => {
    set((state) => {
      const existing = state.threadsByAgent.get(agentId) ?? [];
      if (existing.some((t) => t.id === thread.id)) return state;
      const nextThreads = new Map(state.threadsByAgent);
      nextThreads.set(agentId, [...existing, thread]);
      return { threadsByAgent: nextThreads };
    });
  },

  deleteThread: async (threadId: string) => {
    await api.deleteThread(threadId);
    set((state) => {
      const nextThreads = new Map(state.threadsByAgent);
      const nextSelected = new Map(state.selectedThreadIdByAgent);
      for (const [agentId, threads] of nextThreads) {
        const idx = threads.findIndex((t) => t.id === threadId);
        if (idx >= 0) {
          nextThreads.set(agentId, threads.filter((t) => t.id !== threadId));
          // Revert selection to default when the deleted thread was active
          if (nextSelected.get(agentId) === threadId) {
            nextSelected.set(agentId, `default-${agentId}`);
          }
          break;
        }
      }
      return { threadsByAgent: nextThreads, selectedThreadIdByAgent: nextSelected };
    });
  },

  loadAssignments: async (agentId: string) => {
    const assignments = await api.listAssignments(agentId);
    set((state) => {
      const next = new Map(state.assignmentsByAgent);
      next.set(agentId, assignments);
      return { assignmentsByAgent: next };
    });
  },

  createAssignment: async (agentId: string, req: CreateAssignmentRequest) => {
    const created = await api.createAssignment(agentId, req);
    set((state) => {
      const existing = state.assignmentsByAgent.get(agentId) ?? [];
      const next = new Map(state.assignmentsByAgent);
      next.set(agentId, [...existing, created]);
      return { assignmentsByAgent: next };
    });
    return created;
  },

  updateAssignment: async (assignmentId: string, patch: PatchAssignmentRequest) => {
    const updated = await api.updateAssignment(assignmentId, patch);
    set((state) => {
      const next = new Map(state.assignmentsByAgent);
      const existing = next.get(updated.agent_id) ?? [];
      const idx = existing.findIndex((a) => a.id === assignmentId);
      if (idx >= 0) {
        const list = [...existing];
        list[idx] = updated;
        next.set(updated.agent_id, list);
      }
      return { assignmentsByAgent: next };
    });
    return updated;
  },

  deleteAssignment: async (assignmentId: string) => {
    await api.deleteAssignment(assignmentId);
    set((state) => {
      const next = new Map(state.assignmentsByAgent);
      for (const [agentId, assignments] of next) {
        const idx = assignments.findIndex((a) => a.id === assignmentId);
        if (idx >= 0) {
          next.set(agentId, assignments.filter((a) => a.id !== assignmentId));
          break;
        }
      }
      return { assignmentsByAgent: next };
    });
  },

  loadAssignmentRuns: async (assignmentId: string) => {
    const runs = await api.listAssignmentRuns(assignmentId);
    set((state) => {
      const next = new Map(state.runsByAssignment);
      next.set(assignmentId, runs);
      return { runsByAssignment: next };
    });
  },

  triggerAssignment: async (assignmentId: string, body?: TriggerAssignmentRequest) => {
    const run = await api.triggerAssignment(assignmentId, body);
    // Refresh this assignment's run history so the freshly-queued run appears.
    await get().loadAssignmentRuns(assignmentId);
    return run;
  },

  setPendingForm: (agentId: string, form: FormRequestPayload) => {
    const key = inFlightKey(agentId, form.thread_id);
    set((state) => {
      // `inFlightKey` is stable for the thread's whole lifetime, not
      // per-form, so a fresh form landing on a slot that still carries a
      // stale minimized flag (a still-pending form replaced without going
      // through `clearPendingForm`, or a reconnect/replay/orphan-recovery
      // re-set) must not inherit it — every form has to arrive expanded.
      const nextMinimized = { ...state.minimizedFormByKey };
      delete nextMinimized[key];
      return { pendingFormByAgent: { ...state.pendingFormByAgent, [key]: form }, minimizedFormByKey: nextMinimized };
    });
  },

  stashWithdrawnFormDraft: (formId: string, draft: WithdrawnFormDraftEntry[]) => {
    set((state) => ({ withdrawnFormDrafts: { ...state.withdrawnFormDrafts, [formId]: draft } }));
  },

  clearPendingForm: (agentId: string, threadId?: string) => {
    const key = inFlightKey(agentId, threadId);
    set((state) => {
      const next = { ...state.pendingFormByAgent };
      delete next[key];
      // The slot this form occupied is gone, so any minimized flag riding
      // along with it must go too — otherwise the next form posted to this
      // same (agent, thread) slot would inherit a stale "minimized" state
      // and arrive already collapsed, with no live form behind it.
      const nextMinimized = { ...state.minimizedFormByKey };
      delete nextMinimized[key];
      return { pendingFormByAgent: next, minimizedFormByKey: nextMinimized };
    });
  },

  markPendingFormOrphaned: (agentId: string, threadId?: string) => {
    const key = inFlightKey(agentId, threadId);
    set((state) => {
      const existing = state.pendingFormByAgent[key];
      if (!existing || existing.orphaned) return {};
      // Force-expand: an orphaned form needs the user's attention, so a
      // minimized bar that quietly does nothing would hide the exact state
      // this mechanism exists to surface.
      const nextMinimized = { ...state.minimizedFormByKey };
      delete nextMinimized[key];
      return {
        pendingFormByAgent: { ...state.pendingFormByAgent, [key]: { ...existing, orphaned: true } },
        minimizedFormByKey: nextMinimized,
      };
    });
  },

  setFormMinimized: (agentId: string, threadId: string | undefined, minimized: boolean) => {
    const key = inFlightKey(agentId, threadId);
    set((state) => {
      const next = { ...state.minimizedFormByKey };
      if (minimized) {
        next[key] = true;
      } else {
        delete next[key];
      }
      return { minimizedFormByKey: next };
    });
  },

  hydratePendingSyncFormsFromAgents: () => {
    set((state) => {
      const next = { ...state.pendingFormByAgent };
      let changed = false;
      for (const agent of state.agents) {
        for (const form of agent.pending_forms ?? []) {
          const spec = form.spec;
          if (spec?.mode !== "sync") continue;
          const key = inFlightKey(agent.agent_id, form.thread_id ?? undefined);
          if (next[key] != null) continue; // a live/cleared entry always wins.
          next[key] = {
            form_id: spec.form_id,
            agent_id: agent.agent_id,
            session_id: "",
            title: spec.spec.title,
            intro: spec.spec.intro ?? undefined,
            fields: spec.spec.fields,
            thread_id: form.thread_id ?? undefined,
            orphaned: form.orphaned === true,
          };
          changed = true;
        }
      }
      return changed ? { pendingFormByAgent: next } : {};
    });
  },

  setPendingAsyncFormId: (channelKey: string, formId: string) => {
    set((state) => {
      // Same sparse-map hygiene as `setPendingForm` — a fresh async form
      // landing on a channel key that still carries a stale minimized flag
      // must not inherit it — every form has to arrive expanded.
      const nextMinimized = { ...state.minimizedFormByKey };
      delete nextMinimized[channelKey];
      return { pendingAsyncFormIdByChannel: { ...state.pendingAsyncFormIdByChannel, [channelKey]: formId }, minimizedFormByKey: nextMinimized };
    });
  },

  clearPendingAsyncFormId: (channelKey: string) => {
    set((state) => {
      const next = { ...state.pendingAsyncFormIdByChannel };
      delete next[channelKey];
      // Same sparse-map hygiene as `clearPendingForm` — drop any minimized
      // flag riding on this channel key so a future form on this channel
      // never arrives pre-minimized. No-op for channel keys that never had
      // one (the common case).
      const nextMinimized = { ...state.minimizedFormByKey };
      delete nextMinimized[channelKey];
      return { pendingAsyncFormIdByChannel: next, minimizedFormByKey: nextMinimized };
    });
  },

  addFormAnswerEntry: (agentId: string, payload: { form: FormRequestPayload; answers: FormAnswerMap; action?: FormAction; note?: string }) => {
    const { selectedAgentId, messages, allMessages, threadsByAgent, selectedThreadIdByAgent } = get();
    const syntheticEntry: TranscriptEntry = {
      ts: new Date().toISOString(),
      role: "user",
      content: "",
      event_type: "form_answer",
      metadata: { form: payload.form, answers: payload.answers, action: payload.action, note: payload.note },
    };
    // Gate on the form's OWNING thread, not just the selected agent — a form
    // answered on a background thread of the currently-selected agent must
    // not drop its optimistic bubble into whichever thread is on screen (see
    // `isEventForActiveThread`).
    const isSelected =
      selectedAgentId === agentId &&
      isEventForActiveThread(agentId, payload.form.thread_id, threadsByAgent, selectedThreadIdByAgent);
    set((state) => ({
      ...(isSelected
        ? {
            messages: [...messages, syntheticEntry],
            allMessages: [...allMessages, syntheticEntry],
            scrollToBottomTrigger: state.scrollToBottomTrigger + 1,
          }
        : {}),
    }));
  },

  addAsyncFormAnswerEntry: (agentId: string, payload: { form_id: string; threadId?: string; values: FormAnswerMap; spec: AsyncFormSpec }) => {
    const { selectedAgentId, messages, allMessages, threadsByAgent, selectedThreadIdByAgent } = get();
    const syntheticEntry: TranscriptEntry = {
      ts: new Date().toISOString(),
      role: "user",
      content: "",
      event_type: "form_answer",
      metadata: { form_id: payload.form_id, values: payload.values, spec: payload.spec },
    };
    // Same thread-scoped gating as `addFormAnswerEntry` above — a form
    // answered on a background thread must not drop its bubble into
    // whichever thread is on screen.
    const isSelected =
      selectedAgentId === agentId &&
      isEventForActiveThread(agentId, payload.threadId, threadsByAgent, selectedThreadIdByAgent);
    set((state) => ({
      ...(isSelected
        ? {
            messages: [...messages, syntheticEntry],
            allMessages: [...allMessages, syntheticEntry],
            scrollToBottomTrigger: state.scrollToBottomTrigger + 1,
          }
        : {}),
    }));
  },

  mountProjectChannel: (
    projectId: string,
    messages: TranscriptEntry[],
    cursor: PaginationCursor | null,
    agentName: string,
    agentEmoji: string | undefined,
  ) => {
    const key = `project:${projectId}`;
    const displayCount = PAGE_SIZE;
    const displayMessages = messages.slice(-displayCount);

    // Preserve in-flight state when streaming is active — the SSE connection
    // stays open across navigation and may still be delivering tokens. Only
    // clear a genuinely idle leftover from a previous session.
    const inFlight = new Map(get().inFlightByAgent);
    const existingInFlight = inFlight.get(key);
    if (
      existingInFlight &&
      !existingInFlight.isTyping &&
      existingInFlight.textBuffer === "" &&
      existingInFlight.activeToolCalls.length === 0
    ) {
      inFlight.delete(key);
    }

    // Minimal synthetic profile so useResolvedAgent renders the correct
    // name and emoji in the streaming bubble (selectedAgentProfile is the
    // fallback when the virtual key isn't in the agent snapshots list).
    const syntheticProfile: AgentProfile = {
      id: key,
      name: agentName,
      emoji: agentEmoji,
      description: "",
      provider: { type: "", command: "", args: [], output_format: "", input_mode: "", model_aliases: {}, resume_args: [], session_id_fields: [], clear_env: false, no_output_timeout_ms: 0 },
      model: null,
      skills: [],
      system_prompt: null,
      tools: null,
      env: {},
      max_instances: 1,
      timeout_seconds: 0,
      working_dir: null,
      home_dir: null,
      serialize: false,
    };

    set({
      selectedAgentId: key,
      selectedAgentProfile: syntheticProfile,
      messages: displayMessages,
      allMessages: messages,
      messagesLoading: false,
      loadingMore: false,
      displayCount,
      hasMoreMessages: messages.length > displayCount || cursor !== null,
      cursor,
      isStreamingAreaVisible: true,
      inFlightByAgent: inFlight,
      pendingMessageIds: new Set<string>(),
      seenMessageIds: new Set<string>(),
      messageIdByTs: {},
    });
  },

  syncProjectChannelMessages: (
    projectId: string,
    messages: TranscriptEntry[],
    cursor: PaginationCursor | null,
    bumpScroll = false,
  ) => {
    const key = `project:${projectId}`;
    if (get().selectedAgentId !== key) return;
    const displayCount = get().displayCount;
    const displayMessages = messages.slice(-displayCount);
    set({
      messages: displayMessages,
      allMessages: messages,
      hasMoreMessages: messages.length > displayCount || cursor !== null,
      cursor,
      ...(bumpScroll ? { scrollToBottomTrigger: get().scrollToBottomTrigger + 1 } : {}),
    });
  },

  reset: () => {
    for (const timer of inFlightDeleteTimers.values()) clearTimeout(timer);
    inFlightDeleteTimers.clear();
    for (const timer of optimisticTypingWatchdogs.values()) clearTimeout(timer);
    optimisticTypingWatchdogs.clear();
    // Bump (not reset to 0 — irrelevant which direction, only that it
    // changes) so any `loadAllThreads` call still in flight from before this
    // `reset()` can never pass its stale-response check afterward.
    threadsHydrationGeneration++;
    set({
      selectedAgentId: null,
      selectedAgentProfile: null,
      messages: [],
      allMessages: [],
      messagesLoading: false,
      loadingMore: false,
      displayCount: PAGE_SIZE,
      hasMoreMessages: false,
      cursor: null,
      scrollToBottomTrigger: 0,
      inFlightByAgent: new Map<string, InFlightAgentMessage>(),
      usageByAgent: new Map<string, TurnUsage>(),
      isStreamingAreaVisible: true,
      pendingMessageIds: new Set<string>(),
      seenMessageIds: new Set<string>(),
      messageIdByTs: {},
      pendingFormByAgent: {},
      minimizedFormByKey: {},
      withdrawnFormDrafts: {},
      pendingAsyncFormIdByChannel: {},
      threadsByAgent: new Map<string, Thread[]>(),
      threadsHydrationError: null,
      selectedThreadIdByAgent: new Map<string, string>(),
      unreadThreadIds: new Set<string>(),
      runningDelegatesByThread: new Map<string, Map<string, RunningDelegateInfo>>(),
    });
  },
}));

// ---------------------------------------------------------------------------
// Scoped selector hooks for in-flight state
// ---------------------------------------------------------------------------
//
// Subscribers to `useStreamingText(agentId)` only re-render when that agent's
// textBuffer changes — the returned string is compared by Object.is. Tool-call
// and typing selectors return stable references/booleans for the same reason.

/** Pure resolver behind `useStreamingThreadId` (split out so it's testable
 *  without a hook-rendering harness). Returns `undefined` for the default
 *  thread ("Main thread") — mirroring the backend's event tagging, which
 *  always omits `thread_id` for a default-kind thread regardless of which
 *  literal id was selected/sent (see `resolve_non_default_thread` in
 *  `ao-server/src/routes/messages.rs`). Returns the thread's real id for any
 *  other (fresh/branch) thread. Without this collapse, selecting "Main
 *  thread" by its real backend id would look up a thread-scoped bucket that
 *  the backend's events never actually tag, and the streaming bubble would
 *  never find its own content. */
export function resolveStreamingThreadId(
  agentId: string | null | undefined,
  selectedThreadId: string | null | undefined,
  threads: Thread[] | undefined,
): string | undefined {
  if (!agentId || !selectedThreadId) return undefined;
  const thread = threads?.find((t) => t.id === selectedThreadId);
  if (thread) return thread.kind === "default" ? undefined : selectedThreadId;
  // Thread list not loaded yet (e.g. the one tick between selecting an agent
  // and `loadThreads` resolving) — the only id `selectedThreadIdByAgent` can
  // hold before that is the `default-{agentId}` placeholder sentinel.
  return selectedThreadId === `default-${agentId}` ? undefined : selectedThreadId;
}

/** Resolves which thread (if any) should scope an agent's in-flight
 *  streaming state (typing bubble / text buffer / tool-call chips / thinking
 *  pill). See `resolveStreamingThreadId` for the collapse rule. */
export function useStreamingThreadId(agentId: string | null | undefined): string | undefined {
  const threads = useChatStore((s) => (agentId ? s.threadsByAgent.get(agentId) : undefined));
  const selectedThreadId = useChatStore((s) => (agentId ? s.selectedThreadIdByAgent.get(agentId) : undefined));
  return resolveStreamingThreadId(agentId, selectedThreadId, threads);
}

/** True when `eventThreadId` — the thread tag carried by a background SSE
 *  event (`undefined` for the default thread, mirroring backend tagging; see
 *  `resolveStreamingThreadId`) — is the thread currently active for
 *  `agentId`: either the thread explicitly selected for that agent, or the
 *  default thread when nothing else has been chosen yet.
 *
 *  Any handler that writes into `messages` / `allMessages` / `messageCache`
 *  must gate on this before merging a background event in — those fields
 *  hold exactly one thread's transcript per agent at a time (a thread switch
 *  invalidates and refetches them, see `switchToThread` in
 *  `lib/threadNavigation.ts`), so an event tagged for a *different* thread of
 *  the same agent would otherwise corrupt whichever thread is currently
 *  loaded. The entry itself isn't lost by skipping it here — it's already
 *  persisted server-side and will render correctly the next time the user
 *  actually switches to that thread. */
export function isEventForActiveThread(
  agentId: string,
  eventThreadId: string | undefined,
  threadsByAgent: Map<string, Thread[]>,
  selectedThreadIdByAgent: Map<string, string>,
): boolean {
  const activeThreadId = resolveStreamingThreadId(
    agentId,
    selectedThreadIdByAgent.get(agentId),
    threadsByAgent.get(agentId),
  );
  return eventThreadId === activeThreadId;
}

/** Find the `PendingForm` (if any) scoped to `activeThreadId` — `undefined`
 *  for the default thread, mirroring `resolveStreamingThreadId`'s collapse
 *  rule and the backend's `thread_id: null` convention for the same case.
 *  Use this instead of `pendingForms[0]` anywhere a form is rendered or
 *  gated on, so a form pending on a different thread of the same agent never
 *  bleeds into the thread currently on screen. */
/** The ASYNC pending form (if any) for a given thread — every call site here
 *  (ChatView's async-form takeover, MessageList's pending-placeholder id,
 *  ThreadActivityBadge's "question" rollup) has only ever meant "an async
 *  `AskUserQuestionWithForm` post," so entries tagged `spec.mode === "sync"`
 *  are filtered out here rather than at each call site. Sync forms are
 *  written into this same `pending_forms` array (tagged `mode: "sync"`,
 *  for `pendingFormByAgent` rehydration only — see
 *  `hydratePendingSyncFormsFromAgents`), so without this filter a sync form
 *  would get mistaken for an async one and offered through the async
 *  answer/dismiss UI, which hits the wrong backend endpoint entirely (queues
 *  a new agent turn instead of resolving the parked tool call — the exact
 *  failure mode this filter exists to prevent). `spec == null` (the transient
 *  optimistic-insert shape from `useSSE`'s `form_posted` handler) still
 *  passes through unfiltered, since that's always async. */
export function pendingFormForThread(
  pendingForms: PendingForm[] | undefined,
  activeThreadId: string | undefined,
): PendingForm | undefined {
  return pendingForms?.find(
    (f) => (f.thread_id ?? undefined) === activeThreadId && f.spec?.mode !== "sync",
  );
}

/** Whether an async `PendingForm` should still be treated as the thread's
 *  active, answerable question. Used to be a genuine staleness check — the
 *  form stayed answerable server-side but stopped claiming the `?` badge/
 *  composer the instant something else happened in the thread ("leave open,
 *  don't auto-dismiss on skip" plus "but don't keep blocking either"). That
 *  auto-release invariant is gone now that the composer gate blocks on a
 *  single form slot (sync ∪ async, see `ChatView`'s `formSlotOccupied`) until
 *  the form is explicitly answered/dismissed/superseded — all of which
 *  remove it from `pending_forms` outright (superseding writes a fresh
 *  record over it via upsert-by-thread). So a non-null `form` here already
 *  means "still the operator's open question"; nothing else can silently
 *  revoke that claim. Kept as a named predicate (rather than inlined `!=
 *  null` at each call site) purely for readability at the call sites that
 *  still phrase their gate as "is this form still current." */
export function isPendingFormStillCurrent(form: PendingForm | undefined): boolean {
  return form != null;
}

/** Sync-form (`AskUserQuestionWithForm`) counterpart to `pendingFormForThread`.
 *  `pendingFormByAgent` is keyed the same way as `inFlightByAgent` (see
 *  `inFlightKey`), so this resolves the entry for a specific (agent, thread)
 *  pair — `threadId: undefined` resolves to the default thread, matching
 *  `pendingFormForThread`'s back-compat rule. */
export function pendingSyncFormForThread(
  pendingFormByAgent: Record<string, FormRequestPayload | undefined>,
  agentId: string,
  threadId: string | undefined,
): FormRequestPayload | undefined {
  return pendingFormByAgent[inFlightKey(agentId, threadId)];
}

export function hasPendingSyncFormForThread(
  pendingFormByAgent: Record<string, FormRequestPayload | undefined>,
  agentId: string,
  threadId: string | undefined,
): boolean {
  return pendingSyncFormForThread(pendingFormByAgent, agentId, threadId) != null;
}

/** Whether the pending sync form for a given (agent, thread) pair is
 *  currently minimized to a composer-slot bar. Keyed identically to
 *  `pendingSyncFormForThread` — `minimizedFormByKey` is sparse, so absence
 *  (the common case) reads as "expanded". */
export function isFormMinimized(
  minimizedFormByKey: Record<string, boolean>,
  agentId: string,
  threadId: string | undefined,
): boolean {
  return minimizedFormByKey[inFlightKey(agentId, threadId)] === true;
}

/** True when `agentId` has a pending sync form on ANY of its threads —
 *  for surfaces (e.g. a sidebar badge) that need "does this agent need me"
 *  without knowing which thread it's currently on. */
export function agentHasPendingSyncForm(
  pendingFormByAgent: Record<string, FormRequestPayload | undefined>,
  agentId: string,
): boolean {
  return Object.keys(pendingFormByAgent).some(
    (key) => pendingFormByAgent[key] != null && agentIdFromInFlightKey(key) === agentId,
  );
}

/** True when `agentId` has ANY unanswered question outstanding — sync
 *  (`AskUserQuestionWithForm`, via `agentHasPendingSyncForm`) or async (the
 *  agent's own `pending_forms` snapshot field) — on any of its threads.
 *  Sidebar-badge rollups should read this rather than either source alone,
 *  so an agent-level indicator never misses one form type. */
export function agentHasAnyPendingForm(
  pendingFormByAgent: Record<string, FormRequestPayload | undefined>,
  agentId: string,
  pendingForms: PendingForm[] | undefined,
): boolean {
  return agentHasPendingSyncForm(pendingFormByAgent, agentId) || (pendingForms?.length ?? 0) > 0;
}

/** Called (fire-and-forget) from the `run_ended` SSE handler, after the
 *  backend has flushed this run's transcript entries to disk. Force-reloads
 *  the agent's artifact list so a just-produced artifact shows up in the
 *  Assets panel without the user having to navigate away and back —
 *  `loadArtifacts` no-ops once the cache is `"loaded"`, so an artifact
 *  written mid-run never enters the store on its own otherwise.
 *
 *  Inline thread-bubble cards do NOT depend on this: they resolve straight
 *  off the `ArtifactWrite` tool result (live via `appendInFlightArtifactId`,
 *  or from the persisted transcript via the tool_result scan in
 *  `MessageList`'s `extractArtifactWriteResults`), so there is no anchor
 *  match, no refetch, and nothing here can race a live reply. */
async function syncArtifactsAfterRun(agentId: string): Promise<void> {
  await useArtifactStore.getState().loadArtifacts(agentId, { force: true });
}

/** Raw in-flight entry for an agent (optionally thread-scoped). Stable
 *  reference — only changes when the entry itself is replaced via `set()`.
 *  Callers that want to avoid churn should prefer the field-scoped selectors
 *  below. */
export function useInFlight(agentId: string | null | undefined, threadId?: string): InFlightAgentMessage | undefined {
  return useChatStore((s) => (agentId ? s.inFlightByAgent.get(inFlightKey(agentId, threadId)) : undefined));
}

export function useStreamingText(agentId: string | null | undefined, threadId?: string): string {
  return useChatStore((s) => (agentId ? s.inFlightByAgent.get(inFlightKey(agentId, threadId))?.textBuffer ?? "" : ""));
}

export function useIsTyping(agentId: string | null | undefined, threadId?: string): boolean {
  return useChatStore((s) => (agentId ? s.inFlightByAgent.get(inFlightKey(agentId, threadId))?.isTyping ?? false : false));
}

/** True while a turn's in-flight entry exists at all, regardless of whether
 *  text happens to be streaming right now. Unlike `useIsTyping`, this does
 *  NOT blip false between text segments — `finalizeInFlightText` (fired on
 *  every `text_complete`, including mid-turn ones before a tool call, and on
 *  every RunEnded→RunStarted skill-load/tool-continuation handoff) clears
 *  `isTyping` but deliberately keeps the entry itself alive so the bubble
 *  reads as one continuous reply; the entry is only removed once
 *  `scheduleInFlightTeardown`'s debounced timer actually fires (or
 *  `deleteInFlight` on a genuine cancel). That makes this the correct signal
 *  for "is the agent's whole turn — including any tool calls and chained
 *  continuation runs — truly finished yet", e.g. gating when a client-side
 *  queued follow-up message is safe to flush. Using `isTyping` for that
 *  instead flushes (and thus sends + shows in the transcript) a queued
 *  message the moment the FIRST text segment finalizes, which can be well
 *  before the agent's reply is actually done. */
export function useIsAgentTurnActive(agentId: string | null | undefined, threadId?: string): boolean {
  return useChatStore((s) => (agentId ? s.inFlightByAgent.has(inFlightKey(agentId, threadId)) : false));
}

export function useActiveToolCalls(agentId: string | null | undefined, threadId?: string): ActiveToolCall[] {
  return useChatStore((s) => (agentId ? s.inFlightByAgent.get(inFlightKey(agentId, threadId))?.activeToolCalls ?? EMPTY_TOOL_CALLS : EMPTY_TOOL_CALLS));
}

/** Ids of artifacts produced so far by the currently-streaming turn — the
 *  live half of inline card rendering (see `appendInFlightArtifactId`).
 *  Cleared once the turn finalizes; from then on the same ids live on the
 *  finalized `TranscriptEntry`'s `metadata.artifact_ids` instead. */
export function useInFlightArtifactIds(agentId: string | null | undefined, threadId?: string): string[] {
  return useChatStore((s) => (agentId ? s.inFlightByAgent.get(inFlightKey(agentId, threadId))?.artifactIds ?? EMPTY_ARTIFACT_IDS : EMPTY_ARTIFACT_IDS));
}

/** Store-resident twin of `InFlightAgentMessage.everShownThisTurn` — see that
 *  field's docstring for why this can't be a `StreamingMessage`-local ref.
 *  `StreamingMessage` reads this (rather than re-deriving from a local ref)
 *  so the "keep the bubble mounted through a content gap" latch survives a
 *  `MessageList` remount. */
export function useEverShownThisTurn(agentId: string | null | undefined, threadId?: string): boolean {
  return useChatStore((s) => (agentId ? s.inFlightByAgent.get(inFlightKey(agentId, threadId))?.everShownThisTurn ?? false : false));
}

/** Accumulated token usage for the current turn, or `undefined` if no
 *  `usage` event has arrived yet for this agent/thread. Stable reference —
 *  only changes when `accumulateUsage` runs. */
export function useTurnUsage(agentId: string | null | undefined, threadId?: string): TurnUsage | undefined {
  return useChatStore((s) => (agentId ? s.usageByAgent.get(inFlightKey(agentId, threadId)) : undefined));
}

/** Boolean derived from whether ANY buffer or tool call is present for the
 *  agent/thread. Safe to use as a memo dep — doesn't flip on every token. */
export function useIsStreamingActive(agentId: string | null | undefined, threadId?: string): boolean {
  return useChatStore((s) => {
    if (!agentId) return false;
    const e = s.inFlightByAgent.get(inFlightKey(agentId, threadId));
    if (!e) return false;
    return e.textBuffer.length > 0 || e.activeToolCalls.length > 0;
  });
}

/** Snapshot of an agent's current thinking-channel state. Returned as a
 *  single object so consumers re-render at most once per state transition
 *  (active → inactive, buffer growth) rather than four times for four
 *  independent selectors. Returns `null` when there's nothing to show (no
 *  thinking block has opened at all yet this turn — gated on `thinkingShown`,
 *  NOT on `active`/`text`, so the pill stays mounted through the gap between
 *  two sequential thinking blocks instead of unmounting/remounting each time
 *  an `display: "omitted"` block closes with no buffered text — see
 *  `thinkingShown`'s doc comment on `InFlightAgentMessage`). */
export interface InFlightThinking {
  active: boolean;
  text: string;
  startedAt: number | null;
  elapsedMs: number | null;
}

export function useInFlightThinking(
  agentId: string | null | undefined,
  threadId?: string,
): InFlightThinking | null {
  // Each selector returns a primitive — primitives are reference-stable under
  // Object.is, satisfying React 18's useSyncExternalStore snapshot-caching
  // requirement. Combining them into an object here (outside useChatStore)
  // via useMemo avoids the "getSnapshot should be cached" infinite-loop warning
  // that a single object-returning selector triggers.
  const id = agentId ? inFlightKey(agentId, threadId) : "";
  const active = useChatStore(
    (s) => s.inFlightByAgent.get(id)?.thinkingActive ?? false,
  );
  const text = useChatStore(
    (s) => s.inFlightByAgent.get(id)?.thinkingBuffer ?? "",
  );
  const startedAt = useChatStore(
    (s) => s.inFlightByAgent.get(id)?.thinkingStartedAt ?? null,
  );
  const elapsedMs = useChatStore(
    (s) => s.inFlightByAgent.get(id)?.thinkingElapsedMs ?? null,
  );
  const shown = useChatStore(
    (s) => s.inFlightByAgent.get(id)?.thinkingShown ?? false,
  );
  const hasEntry = useChatStore((s) => (id ? s.inFlightByAgent.has(id) : false));

  return useMemo(() => {
    if (!agentId || !hasEntry || !shown) return null;
    return { active, text, startedAt, elapsedMs };
  }, [agentId, hasEntry, shown, active, text, startedAt, elapsedMs]);
}
