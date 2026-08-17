import { createManagedEventSource, type ManagedEventSource } from "../hooks/sseUtils";
import { systemStreamUrl } from "./api";

// ---------------------------------------------------------------------------
// SSE hub — a single `/system/stream` connection fanned out to in-process
// subscribers by routing key.
//
// `/system/stream` is the unfiltered event bus: every other stream endpoint
// (`/agents/{id}/stream`, `/teams/{id}/stream`, ...) is just that same feed
// with a server-side `agent_id`/`run_id` filter. Moving the filter to the
// client lets every caller share one connection instead of opening one per
// entity, which is what keeps the browser's per-host connection pool from
// starving out ordinary REST calls (health pings included) once a handful of
// agents/tasks/teams are active at once.
// ---------------------------------------------------------------------------

/** Envelope parsed once per event, before any subscriber matching happens. */
export type SseEnvelope = {
  agent_id: string;
  run_id: string;
  thread_id?: string | null;
  eventName: string;
  raw: string;
};

/** Returns true when an event (by its parsed envelope) belongs to a channel. */
export type ChannelMatcher = (e: SseEnvelope) => boolean;

export interface HubSubscription {
  close(): void;
}

export interface SubscribeChannelOptions {
  /** Same `{ eventName -> (MessageEvent) => void }` shape callers already
   *  build for `createManagedEventSource` / `EventSource`. The hub delivers a
   *  synthetic `MessageEvent(eventName, { data: raw })` so existing
   *  `parsePayloadData(e.data)` bodies keep working unchanged. */
  listeners: Record<string, (e: MessageEvent) => void>;
  /** Called the first time the shared connection opens while this subscriber
   *  is registered. */
  onOpen?: () => void;
  /** Called on every subsequent (re)connect of the shared connection. */
  onReconnect?: () => void;
}

/** Prebuilt matchers mirroring the server-side filters each per-entity
 *  stream endpoint used to apply (see `crates/ao-server/src/routes/stream.rs`). */
export const channel = {
  agent: (id: string): ChannelMatcher => (e) => e.agent_id === id,
  team: (id: string): ChannelMatcher => (e) => e.agent_id === `team:${id}`,
  project: (id: string): ChannelMatcher => (e) => e.agent_id === `project:${id}`,
  task: (id: string): ChannelMatcher => (e) =>
    e.agent_id.startsWith(`task:${id}:phase:`) || e.run_id === id,
  agentTasklist: (id: string): ChannelMatcher => (e) => e.agent_id === `tasklist:${id}`,
  system: (): ChannelMatcher => () => true,
};

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

interface Subscriber extends SubscribeChannelOptions {
  id: number;
  match: ChannelMatcher;
}

let subscribers: Subscriber[] = [];
let nextSubscriberId = 1;
let connection: ManagedEventSource | null = null;
/** True once the connection has fired `onOpen` at least once since it was
 *  (re)created — distinguishes the initial open (subscriber `onOpen`) from a
 *  later reconnect (subscriber `onReconnect`). */
let hasOpenedOnce = false;
/** Wall-clock time (ms) of the most recent dispatched envelope, i.e. the last
 *  time *any* event arrived on the shared connection. Used by callers that
 *  want to treat "the hub just heard from the server" as proof of liveness
 *  without spending a separate request — see `isHubRecentlyAlive`.
 *
 *  Note this tracks real, named events only. The server also sends a plain
 *  SSE keep-alive comment every ~15s, but `readSSEStream` (in
 *  `sseUtils.ts`) discards comment-only lines before a frame is built, so
 *  they never reach `dispatch()`. That means this timestamp goes stale
 *  during periods with zero agent/task/team activity even though the
 *  connection is still healthy — which is fine for a liveness *shortcut*
 *  (it only ever under-uses the shortcut, never falsely claims liveness) but
 *  means it's not a literal keep-alive tracker. */
let lastEnvelopeAt: number | null = null;

/**
 * Parses the outer envelope (`agent_id`, `run_id`, `thread_id`) exactly once
 * per event. Matched subscribers still receive the raw string so their
 * existing `parsePayloadData` bodies are untouched — this only avoids N
 * redundant `JSON.parse` calls for the routing fields.
 */
function parseEnvelope(eventName: string, raw: string): SseEnvelope | null {
  try {
    const parsed = JSON.parse(raw) as {
      agent_id?: unknown;
      run_id?: unknown;
      thread_id?: unknown;
    };
    if (typeof parsed?.agent_id !== "string") return null;
    return {
      agent_id: parsed.agent_id,
      run_id: typeof parsed.run_id === "string" ? parsed.run_id : "",
      thread_id: typeof parsed.thread_id === "string" ? parsed.thread_id : null,
      eventName,
      raw,
    };
  } catch {
    console.warn("[sseHub] failed to parse event envelope:", raw);
    return null;
  }
}

/** Walks the subscriber registry and delivers the synthetic MessageEvent to
 *  every subscriber whose matcher accepts this envelope and that registered a
 *  listener for this event name. */
function dispatch(envelope: SseEnvelope): void {
  lastEnvelopeAt = Date.now();
  const messageEvent = new MessageEvent(envelope.eventName, { data: envelope.raw });
  for (const sub of subscribers) {
    const handler = sub.listeners[envelope.eventName];
    if (!handler) continue;
    if (!sub.match(envelope)) continue;
    handler(messageEvent);
  }
}

/**
 * The single `listeners` map handed to `createManagedEventSource`. The
 * backend can emit any of an open-ended set of event names (`text_delta`,
 * `agent_busy`, `tool_call_started`, ...), and the hub must route every one
 * of them without hardcoding the list here (that list drifts as the backend
 * grows new event types). A `Proxy` lets `readSSEStream`'s plain
 * `listeners[frame.event]` lookup resolve to the same dispatch function for
 * ANY property name — i.e. a dynamic wildcard keyed on whatever event name
 * actually arrives, rather than a fixed set of pre-registered handlers.
 */
const wildcardListeners: Record<string, (e: MessageEvent) => void> = new Proxy(
  {},
  {
    get(_target, prop) {
      if (typeof prop !== "string") return undefined;
      return (e: MessageEvent) => {
        const envelope = parseEnvelope(prop, e.data as string);
        if (envelope) dispatch(envelope);
      };
    },
  }
);

function handleConnectionOpen(): void {
  const isReconnect = hasOpenedOnce;
  hasOpenedOnce = true;
  // Active-run replay is handled server-side: `/system/stream` replays an
  // `AgentBusy` event for every in-flight run on connect, which flows through
  // the same `dispatch` path as any other event and reaches the matching
  // subscribers' own listeners. Nothing extra to inject here — this hook
  // exists purely to sequence subscriber `onOpen`/`onReconnect` callbacks
  // after that replay has had a chance to arrive on the wire first.
  for (const sub of subscribers) {
    if (isReconnect) {
      sub.onReconnect?.();
    } else {
      sub.onOpen?.();
    }
  }
}

function ensureConnection(): void {
  if (connection) return;
  hasOpenedOnce = false;
  connection = createManagedEventSource(systemStreamUrl(), {
    listeners: wildcardListeners,
    onOpen: handleConnectionOpen,
  });
}

function releaseConnectionIfIdle(): void {
  if (subscribers.length > 0) return;
  connection?.close();
  connection = null;
  hasOpenedOnce = false;
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/**
 * Registers a subscriber against the shared `/system/stream` connection,
 * lazily opening it on the first subscription and ref-counting it closed
 * once the last subscriber unsubscribes.
 */
export function subscribeChannel(
  match: ChannelMatcher,
  opts: SubscribeChannelOptions
): HubSubscription {
  const sub: Subscriber = {
    id: nextSubscriberId++,
    match,
    listeners: opts.listeners,
    onOpen: opts.onOpen,
    onReconnect: opts.onReconnect,
  };
  subscribers.push(sub);
  ensureConnection();

  let closed = false;
  return {
    close() {
      if (closed) return;
      closed = true;
      subscribers = subscribers.filter((s) => s.id !== sub.id);
      releaseConnectionIfIdle();
    },
  };
}

/**
 * True when the hub has dispatched an event within the last `maxAgeMs`
 * (default 20s — a bit over the server's 15s keep-alive cadence). Callers
 * can treat this as proof the server is reachable without spending a
 * dedicated request, e.g. `networkStore.ts`'s health ping short-circuits to
 * healthy rather than racing its `fetch()` against the browser's per-host
 * connection pool while the hub's own connection is active.
 */
export function isHubRecentlyAlive(maxAgeMs = 20_000): boolean {
  return lastEnvelopeAt !== null && Date.now() - lastEnvelopeAt < maxAgeMs;
}

/**
 * Test seam: injects a fully-formed envelope straight into the dispatch
 * path, bypassing the network connection and envelope parsing entirely so
 * tests can exercise matcher/fan-out/replay behavior without a real
 * `/system/stream`.
 */
export function __dispatchForTest(envelope: SseEnvelope): void {
  dispatch(envelope);
}

/**
 * Test seam: fires `handleConnectionOpen` directly, exactly as a real
 * (re)connect of the shared `EventSource` would — every current subscriber's
 * `onOpen` (first ever open) or `onReconnect` (every open after) runs. Tests
 * mock `createManagedEventSource` to a stub that never itself invokes the
 * `onOpen` callback it's given (see `useSSE.delegateActivity.test.ts`), so
 * without this seam a subscriber's reconnect-grace-timer logic is
 * unreachable from a test.
 */
export function __triggerConnectionOpenForTest(): void {
  handleConnectionOpen();
}
