import { useChatStore } from "../stores/chatStore";
import type { AgentProfile, ChannelBinding, ChannelBridgeOrigin, TranscriptEntry } from "../types/api";

/** Draft storage key for a thread's ChatInput text/attachments (see
 *  `stores/draftStore.ts`). The default/main thread keeps using the bare
 *  `agentId` — the key chat drafts have always used — so drafts saved before
 *  per-thread scoping existed keep working with zero migration. Every other
 *  thread gets its own namespaced key (mirrors the `project:{id}` convention
 *  `ProjectWorkspace` already uses on the same shared store) so switching
 *  threads never clobbers or leaks another thread's in-progress draft. */
export function threadDraftKey(agentId: string, threadId: string, defaultThreadId: string): string {
  return threadId === defaultThreadId ? agentId : `${agentId}:${threadId}`;
}

/** Human-facing name for a `ChannelBinding.kind`, keyed by the wire value —
 *  mirrors the labels `AgentProfileModal`'s channel setup tabs use, so any
 *  surface naming "which channel" a bridge thread belongs to stays in sync
 *  with the channel-picker UI instead of drifting into its own copy. */
export const CHANNEL_KIND_LABELS: Record<ChannelBinding["kind"], string> = {
  telegram: "Telegram",
  discord: "Discord",
  email: "Email",
  slack: "Slack",
  whatsapp: "WhatsApp",
  webhook: "Webhook",
};

/** The `kind` of the enabled channel binding whose dedicated bridge thread is
 *  `threadId`, or `null` when `threadId` isn't a bridge thread at all.
 *  Mirrors the backend's `is_channel_bridge_thread` predicate
 *  (`crates/ao-engine/src/agent_runner/native.rs`) exactly — two checks,
 *  either sufficient on its own:
 *
 *  1. The classic reverse lookup: some binding in `profile.channels` is
 *     enabled and its `bridge_thread_id` equals `threadId`. Covers every
 *     channel that provisions one thread per binding (Telegram, Discord,
 *     Email). `profile.channels` is the only field a fetched `AgentProfile`
 *     actually carries this on; the legacy `profile.telegram` field is
 *     input-only and never appears in server output.
 *  2. `threadChannelOrigin` — the thread's own `channel_origin`, which the
 *     caller reads off the already-fetched `Thread` row and passes in —
 *     names a `binding_id` that's enabled in `profile.channels`. Covers a
 *     channel that provisions one thread per *conversation* instead (Slack),
 *     where `bridge_thread_id` is never populated at all — see
 *     `ChannelBridgeOrigin`'s docstring (`types/api.ts`).
 *
 *  This is the source of truth `isChannelBridgeThread` below derives from —
 *  callers that need to *name* the matched channel (e.g. ChatView's
 *  read-only hint, which used to hardcode "Telegram" regardless of which
 *  channel actually bridged the thread) should call this directly instead of
 *  re-deriving the match themselves. */
export function getBridgeChannelKind(
  profile: AgentProfile | null | undefined,
  threadId: string | undefined,
  threadChannelOrigin?: ChannelBridgeOrigin | null,
): ChannelBinding["kind"] | null {
  if (!threadId) return null;
  const channels = profile?.channels ?? [];
  const viaBridgeThreadId = channels.find(
    (binding) => binding.enabled && binding.bridge_thread_id === threadId,
  );
  if (viaBridgeThreadId) return viaBridgeThreadId.kind;
  if (!threadChannelOrigin) return null;
  const viaOrigin = channels.find(
    (binding) => binding.enabled && binding.binding_id === threadChannelOrigin.binding_id,
  );
  return viaOrigin?.kind ?? null;
}

/** True when `threadId` is the dedicated bridge thread of any *enabled*
 *  channel binding on `profile` (Telegram, Discord, Email, Slack, ...).
 *  Matching on the specific thread (not just "this agent has some channel
 *  enabled") matters — an agent with, say, Discord enabled must keep a
 *  writable composer on its normal desktop threads. See
 *  `getBridgeChannelKind` for what `threadChannelOrigin` is for. */
export function isChannelBridgeThread(
  profile: AgentProfile | null | undefined,
  threadId: string | undefined,
  threadChannelOrigin?: ChannelBridgeOrigin | null,
): boolean {
  return getBridgeChannelKind(profile, threadId, threadChannelOrigin) !== null;
}

/** Resolve the agent's currently-selected thread id, falling back to the
 *  deterministic default-thread id when nothing has been chosen yet. Unlike
 *  the store's internal `effectiveThreadId` (which collapses the default to
 *  `undefined` for the API), branch sources need the concrete row id, so the
 *  default thread is surfaced as `default-{agentId}` here. */
export function selectedThreadIdFor(agentId: string): string {
  return useChatStore.getState().selectedThreadIdByAgent.get(agentId) ?? `default-${agentId}`;
}

/** Make `threadId` the active thread for `agentId` and reload its transcript.
 *
 *  Selection is updated first so `selectAgent`'s thread-aware fetch resolves
 *  against the newly chosen thread. `selectAgent` no longer needs an
 *  `invalidateCache` push before it: `messageCache` is keyed per-THREAD (see
 *  its doc comment in chatStore.ts), so `selectAgent` looks up the target
 *  thread's own cache entry directly — instant render + background refresh
 *  on a hit (a thread visited before), a normal fetch on a miss (a thread
 *  never visited this session) — instead of always forcing a blocking
 *  network round-trip like the old per-agent-keyed cache required. Both
 *  calls are existing store actions — no network logic is duplicated here.
 *
 *  Uses `selectThreadForAgent` (not the plain `selectThread`) deliberately:
 *  `agentId` is already known here, so there's no reason to make the store
 *  reverse-scan `threadsByAgent` to rediscover it. That scan has a fallback
 *  ("attribute to whichever agent is currently selected") that's only safe
 *  when the caller's target agent IS the one already open — not guaranteed
 *  for every caller of this function (e.g. a sidebar row for a *different*
 *  agent than the one presently on screen, whose thread list may not be
 *  loaded yet). Passing `agentId` straight through sidesteps that ambiguity
 *  entirely. */
export async function switchToThread(agentId: string, threadId: string): Promise<void> {
  const store = useChatStore.getState();
  store.selectThreadForAgent(agentId, threadId);
  await store.selectAgent(agentId);
}

/** Branch a new thread off `entry` in the agent's current thread, then show
 *  the branch's transcript. The store's `branchThread` persists the branch
 *  point and selects the new thread; we only reload the transcript afterwards
 *  so the Chat tab reflects the freshly-grafted history. */
export async function branchFromMessage(
  agentId: string,
  entry: TranscriptEntry,
  title?: string,
): Promise<void> {
  const store = useChatStore.getState();
  const sourceThreadId = selectedThreadIdFor(agentId);
  const sourceMessageId =
    store.messageIdByTs[entry.ts] ??
    ((entry.metadata as Record<string, unknown> | null | undefined)?.message_id as string | undefined) ??
    null;

  await store.branchThread(
    agentId,
    { source_thread_id: sourceThreadId, branch_at: entry.ts, source_message_id: sourceMessageId },
    title,
  );

  // branchThread already moved the sticky selection onto the new thread. No
  // need to invalidate anything first — the branch's thread id is freshly
  // minted, so its per-thread `messageCache` entry (see chatStore.ts) can
  // never already exist; selectAgent's own cache-miss path fetches its
  // history straight from the API.
  await store.selectAgent(agentId);
}
