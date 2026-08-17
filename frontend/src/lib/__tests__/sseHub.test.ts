import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

const managedInstances: Array<{
  url: string;
  listeners: Record<string, (e: MessageEvent) => void>;
  onOpen?: () => void;
  close: ReturnType<typeof vi.fn>;
}> = [];

vi.mock("../../hooks/sseUtils", () => ({
  createManagedEventSource: vi.fn(
    (url: string, options: { listeners: Record<string, (e: MessageEvent) => void>; onOpen?: () => void }) => {
      const close = vi.fn();
      managedInstances.push({ url, listeners: options.listeners, onOpen: options.onOpen, close });
      return { close };
    }
  ),
}));

import { createManagedEventSource } from "../../hooks/sseUtils";
import { subscribeChannel, channel, __dispatchForTest, isHubRecentlyAlive, type SseEnvelope } from "../sseHub";

function envelope(overrides: Partial<SseEnvelope> = {}): SseEnvelope {
  return {
    agent_id: "agent-1",
    run_id: "run-1",
    thread_id: null,
    eventName: "text_delta",
    raw: JSON.stringify({ agent_id: "agent-1", run_id: "run-1", payload: { type: "text_delta", data: { text: "hi" } } }),
    ...overrides,
  };
}

beforeEach(() => {
  managedInstances.length = 0;
  vi.mocked(createManagedEventSource).mockClear();
});

describe("channel matchers", () => {
  it("channel.agent matches only its exact agent_id", () => {
    const match = channel.agent("agent-1");
    expect(match(envelope({ agent_id: "agent-1" }))).toBe(true);
    expect(match(envelope({ agent_id: "agent-2" }))).toBe(false);
    expect(match(envelope({ agent_id: "tasklist:agent-1" }))).toBe(false);
    expect(match(envelope({ agent_id: "team:agent-1" }))).toBe(false);
  });

  it("channel.team matches the team: prefix only", () => {
    const match = channel.team("t1");
    expect(match(envelope({ agent_id: "team:t1" }))).toBe(true);
    expect(match(envelope({ agent_id: "t1" }))).toBe(false);
    expect(match(envelope({ agent_id: "tasklist:t1" }))).toBe(false);
  });

  it("channel.project matches the project: prefix only", () => {
    const match = channel.project("p1");
    expect(match(envelope({ agent_id: "project:p1" }))).toBe(true);
    expect(match(envelope({ agent_id: "p1" }))).toBe(false);
  });

  it("channel.agentTasklist matches the tasklist: prefix only, isolated from plain agent/team ids", () => {
    const match = channel.agentTasklist("l1");
    expect(match(envelope({ agent_id: "tasklist:l1" }))).toBe(true);
    expect(match(envelope({ agent_id: "l1" }))).toBe(false);
    expect(match(envelope({ agent_id: "team:l1" }))).toBe(false);
  });

  it("channel.task matches task:{id}:phase: prefixed agent ids or an exact run_id", () => {
    const match = channel.task("task-1");
    expect(match(envelope({ agent_id: "task:task-1:phase:0", run_id: "unrelated" }))).toBe(true);
    expect(match(envelope({ agent_id: "some-agent", run_id: "task-1" }))).toBe(true);
    expect(match(envelope({ agent_id: "task:other:phase:0", run_id: "unrelated" }))).toBe(false);
  });

  it("channel.system matches everything", () => {
    const match = channel.system();
    expect(match(envelope({ agent_id: "anything" }))).toBe(true);
    expect(match(envelope({ agent_id: "tasklist:x" }))).toBe(true);
  });
});

describe("connection ref-counting", () => {
  it("opens exactly one connection for the first subscriber and reuses it for later subscribers", () => {
    const sub1 = subscribeChannel(channel.agent("a1"), { listeners: {} });
    expect(createManagedEventSource).toHaveBeenCalledTimes(1);

    const sub2 = subscribeChannel(channel.agent("a2"), { listeners: {} });
    expect(createManagedEventSource).toHaveBeenCalledTimes(1);

    sub1.close();
    sub2.close();
  });

  it("closes the underlying connection only once the last subscriber unsubscribes", () => {
    const sub1 = subscribeChannel(channel.agent("a1"), { listeners: {} });
    const sub2 = subscribeChannel(channel.agent("a2"), { listeners: {} });
    const [instance] = managedInstances;

    sub1.close();
    expect(instance.close).not.toHaveBeenCalled();

    sub2.close();
    expect(instance.close).toHaveBeenCalledTimes(1);
  });

  it("re-opens a fresh connection after the shared connection was fully released", () => {
    const sub1 = subscribeChannel(channel.system(), { listeners: {} });
    sub1.close();
    expect(managedInstances[0].close).toHaveBeenCalledTimes(1);

    const sub2 = subscribeChannel(channel.system(), { listeners: {} });
    expect(createManagedEventSource).toHaveBeenCalledTimes(2);
    sub2.close();
  });

  it("close() is idempotent and doesn't double-release the ref count", () => {
    const sub1 = subscribeChannel(channel.system(), { listeners: {} });
    const sub2 = subscribeChannel(channel.system(), { listeners: {} });
    const [instance] = managedInstances;

    sub1.close();
    sub1.close(); // second close should be a no-op, not release sub2's connection
    expect(instance.close).not.toHaveBeenCalled();

    sub2.close();
    expect(instance.close).toHaveBeenCalledTimes(1);
  });
});

describe("fan-out dispatch", () => {
  it("delivers one event to every matching subscriber that has a listener for that event name", () => {
    const received: string[] = [];
    const sub1 = subscribeChannel(channel.agent("agent-1"), {
      listeners: { text_delta: (e) => received.push(`sub1:${e.data}`) },
    });
    const sub2 = subscribeChannel(channel.agent("agent-1"), {
      listeners: { text_delta: (e) => received.push(`sub2:${e.data}`) },
    });
    const sub3 = subscribeChannel(channel.agent("agent-other"), {
      listeners: { text_delta: (e) => received.push(`sub3:${e.data}`) },
    });

    const env = envelope({ agent_id: "agent-1", eventName: "text_delta", raw: "raw-payload" });
    __dispatchForTest(env);

    expect(received).toEqual(["sub1:raw-payload", "sub2:raw-payload"]);

    sub1.close();
    sub2.close();
    sub3.close();
  });

  it("does not deliver to a matching subscriber that has no listener for that event name", () => {
    const received: string[] = [];
    const sub = subscribeChannel(channel.agent("agent-1"), {
      listeners: { run_started: () => received.push("run_started") },
    });

    __dispatchForTest(envelope({ agent_id: "agent-1", eventName: "text_delta", raw: "x" }));

    expect(received).toEqual([]);
    sub.close();
  });

  it("hands the listener the raw string via a synthetic MessageEvent, not a parsed object", () => {
    let capturedData: unknown;
    let capturedType: string | undefined;
    const sub = subscribeChannel(channel.system(), {
      listeners: {
        agent_busy: (e) => {
          capturedData = e.data;
          capturedType = e.type;
        },
      },
    });

    __dispatchForTest(envelope({ eventName: "agent_busy", raw: '{"agent_id":"agent-1"}' }));

    expect(capturedType).toBe("agent_busy");
    expect(capturedData).toBe('{"agent_id":"agent-1"}');
    sub.close();
  });

  it("isolates tasklist: and team: channels so a tasklist subscriber never sees a same-suffix team/agent event", () => {
    const tasklistReceived: string[] = [];
    const teamReceived: string[] = [];
    const agentReceived: string[] = [];

    const s1 = subscribeChannel(channel.agentTasklist("x1"), {
      listeners: { run_started: () => tasklistReceived.push("hit") },
    });
    const s2 = subscribeChannel(channel.team("x1"), {
      listeners: { run_started: () => teamReceived.push("hit") },
    });
    const s3 = subscribeChannel(channel.agent("x1"), {
      listeners: { run_started: () => agentReceived.push("hit") },
    });

    __dispatchForTest(envelope({ agent_id: "tasklist:x1", eventName: "run_started" }));
    expect(tasklistReceived).toEqual(["hit"]);
    expect(teamReceived).toEqual([]);
    expect(agentReceived).toEqual([]);

    __dispatchForTest(envelope({ agent_id: "team:x1", eventName: "run_started" }));
    expect(teamReceived).toEqual(["hit"]);
    expect(tasklistReceived).toEqual(["hit"]);
    expect(agentReceived).toEqual([]);

    __dispatchForTest(envelope({ agent_id: "x1", eventName: "run_started" }));
    expect(agentReceived).toEqual(["hit"]);
    expect(tasklistReceived).toEqual(["hit"]);
    expect(teamReceived).toEqual(["hit"]);

    s1.close();
    s2.close();
    s3.close();
  });
});

describe("wildcard dispatch through the underlying connection", () => {
  it("routes an arbitrary/unregistered event name from the managed connection without a hardcoded list", () => {
    const received: string[] = [];
    const sub = subscribeChannel(channel.agent("agent-1"), {
      listeners: { some_future_event_type: (e) => received.push(e.data as string) },
    });

    const [instance] = managedInstances;
    // Simulate the real connection dispatching an event name the hub never
    // pre-registered — the Proxy-backed listeners map must still resolve it.
    const handler = instance.listeners["some_future_event_type"];
    expect(handler).toBeTypeOf("function");
    handler(
      new MessageEvent("some_future_event_type", {
        data: JSON.stringify({ agent_id: "agent-1", run_id: "r1" }),
      })
    );

    expect(received).toEqual([JSON.stringify({ agent_id: "agent-1", run_id: "r1" })]);
    sub.close();
  });

  it("drops an event whose raw payload has no string agent_id instead of throwing", () => {
    const received: string[] = [];
    const sub = subscribeChannel(channel.system(), {
      listeners: { weird_event: (e) => received.push(e.data as string) },
    });

    const [instance] = managedInstances;
    const handler = instance.listeners["weird_event"];
    handler(new MessageEvent("weird_event", { data: "not json" }));
    handler(new MessageEvent("weird_event", { data: JSON.stringify({ run_id: "r1" }) }));

    expect(received).toEqual([]);
    sub.close();
  });
});

describe("open/reconnect callbacks", () => {
  it("fires onOpen on the first connect and onReconnect on every subsequent reconnect", () => {
    const opens: string[] = [];
    const sub = subscribeChannel(channel.system(), {
      listeners: {},
      onOpen: () => opens.push("open"),
      onReconnect: () => opens.push("reconnect"),
    });

    const [instance] = managedInstances;
    instance.onOpen?.();
    expect(opens).toEqual(["open"]);

    instance.onOpen?.();
    instance.onOpen?.();
    expect(opens).toEqual(["open", "reconnect", "reconnect"]);

    sub.close();
  });

  it("notifies every active subscriber on (re)connect, and replayed events dispatch to matching subscribers via the test seam", () => {
    const agentOpens: string[] = [];
    const systemOpens: string[] = [];
    const agentEvents: string[] = [];

    const agentSub = subscribeChannel(channel.agent("agent-1"), {
      listeners: { agent_busy: (e) => agentEvents.push(e.data as string) },
      onOpen: () => agentOpens.push("open"),
      onReconnect: () => agentOpens.push("reconnect"),
    });
    const systemSub = subscribeChannel(channel.system(), {
      listeners: {},
      onOpen: () => systemOpens.push("open"),
      onReconnect: () => systemOpens.push("reconnect"),
    });

    const [instance] = managedInstances;

    // Initial connect.
    instance.onOpen?.();
    expect(agentOpens).toEqual(["open"]);
    expect(systemOpens).toEqual(["open"]);

    // Server replays an AgentBusy for the still-active run before/after the
    // hub's own onOpen fires; either way it flows through normal dispatch.
    __dispatchForTest(
      envelope({ agent_id: "agent-1", eventName: "agent_busy", raw: "replayed-busy" })
    );
    expect(agentEvents).toEqual(["replayed-busy"]);

    // A drop + reconnect notifies every still-registered subscriber via
    // onReconnect (not onOpen again).
    instance.onOpen?.();
    expect(agentOpens).toEqual(["open", "reconnect"]);
    expect(systemOpens).toEqual(["open", "reconnect"]);

    agentSub.close();
    systemSub.close();
  });
});

describe("every AgentEventPayload variant round-trips through the hub", () => {
  // Mirrors `event_type_name` in `crates/ao-server/src/routes/stream.rs` —
  // the authoritative mapping from each `AgentEventPayload` variant to its
  // SSE event name. Keep in sync if a variant is added/renamed there; this
  // guards the hub's wildcard `Proxy` dispatch (see `wildcardListeners` in
  // `../sseHub.ts`) against silently dropping a name it never registered.
  const EVENT_TYPE_NAMES = [
    "run_started",
    "run_ended",
    "text_delta",
    "text_complete",
    "thinking_started",
    "thinking_delta",
    "thinking_ended",
    "tool_call_started",
    "tool_call_completed",
    "message_received",
    "message_processing_started",
    "agent_busy",
    "error",
    "usage",
    "delegation_started",
    "delegation_completed",
    "team_round_started",
    "team_round_completed",
    "workflow_task_created",
    "phase_started",
    "phase_completed",
    "phase_skipped",
    "phase_failed",
    "phase_paused",
    "workflow_phase_progress",
    "workflow_completed",
    "workflow_task_started",
    "workflow_task_failed",
    "workflow_task_stopped",
    "workflow_task_reopened",
    "system_message",
    "agent_action_started",
    "agent_action_completed",
    "tool_use_started",
    "tool_use_completed",
    "hidden_transcript_entry",
    "tasklist.created",
    "tasklist.task_updated",
    "tasklist.completed",
    "tasklist.failed",
    "tasklist.status_changed",
    "tasklist.task_added",
    "tasklist.woke",
    "tasklist.slept",
    "memory_saved",
    "tool_progress",
    "todo_list.created",
    "todo_list.complete",
    "delegate.complete",
    "task.deferred",
    "form_request",
    "form_posted",
    "project.state_changed",
    "agent.snapshot_updated",
    "thread_renamed",
    "thread_created",
  ] as const;

  it.each(EVENT_TYPE_NAMES)("delivers a '%s' event to a matching system subscriber with its raw payload intact", (eventName) => {
    const received: string[] = [];
    const sub = subscribeChannel(channel.system(), {
      listeners: { [eventName]: (e) => received.push(e.data as string) },
    });

    const raw = JSON.stringify({ agent_id: "agent-1", run_id: "run-1", payload: { type: eventName, data: { marker: eventName } } });
    __dispatchForTest(envelope({ agent_id: "agent-1", eventName, raw }));

    expect(received).toEqual([raw]);
    sub.close();
  });

  it("covers every variant exactly once (no accidental duplicates/typos in the mirrored name list)", () => {
    expect(new Set(EVENT_TYPE_NAMES).size).toBe(EVENT_TYPE_NAMES.length);
    expect(EVENT_TYPE_NAMES.length).toBe(56);
  });
});

describe("replay-on-reconnect delivers AgentBusy to every channel type's subscribers", () => {
  it("routes one replayed AgentBusy per routing key to exactly its own agent/team/project/task/tasklist subscriber", () => {
    const agentReceived: string[] = [];
    const teamReceived: string[] = [];
    const projectReceived: string[] = [];
    const taskReceived: string[] = [];
    const tasklistReceived: string[] = [];

    const agentSub = subscribeChannel(channel.agent("a1"), {
      listeners: { agent_busy: (e) => agentReceived.push(e.data as string) },
    });
    const teamSub = subscribeChannel(channel.team("t1"), {
      listeners: { agent_busy: (e) => teamReceived.push(e.data as string) },
    });
    const projectSub = subscribeChannel(channel.project("p1"), {
      listeners: { agent_busy: (e) => projectReceived.push(e.data as string) },
    });
    // Task channels match either a `task:{id}:phase:` prefixed agent_id or an
    // exact run_id — the server-side replay keys off the phase-prefixed form
    // (see `channel.task` in ../sseHub.ts).
    const taskSub = subscribeChannel(channel.task("k1"), {
      listeners: { agent_busy: (e) => taskReceived.push(e.data as string) },
    });
    const tasklistSub = subscribeChannel(channel.agentTasklist("l1"), {
      listeners: { agent_busy: (e) => tasklistReceived.push(e.data as string) },
    });

    // Simulate the connection (re)opening — replay travels over the same
    // wire as any other event and reaches subscribers via normal dispatch
    // (see the `handleConnectionOpen` doc comment in ../sseHub.ts).
    const [instance] = managedInstances;
    instance.onOpen?.();

    // One AgentBusy replay per still-active run, keyed exactly as
    // `InstanceRegistry::all_active_runs()` would report them server-side.
    __dispatchForTest(envelope({ agent_id: "a1", run_id: "run-a", eventName: "agent_busy", raw: "busy-agent" }));
    __dispatchForTest(envelope({ agent_id: "team:t1", run_id: "run-t", eventName: "agent_busy", raw: "busy-team" }));
    __dispatchForTest(envelope({ agent_id: "project:p1", run_id: "run-p", eventName: "agent_busy", raw: "busy-project" }));
    __dispatchForTest(envelope({ agent_id: "task:k1:phase:0", run_id: "run-k", eventName: "agent_busy", raw: "busy-task" }));
    __dispatchForTest(envelope({ agent_id: "tasklist:l1", run_id: "run-l", eventName: "agent_busy", raw: "busy-tasklist" }));

    expect(agentReceived).toEqual(["busy-agent"]);
    expect(teamReceived).toEqual(["busy-team"]);
    expect(projectReceived).toEqual(["busy-project"]);
    expect(taskReceived).toEqual(["busy-task"]);
    expect(tasklistReceived).toEqual(["busy-tasklist"]);

    agentSub.close();
    teamSub.close();
    projectSub.close();
    taskSub.close();
    tasklistSub.close();
  });

  it("a task subscriber also receives replay keyed by run_id alone (the non-phase-prefixed match branch)", () => {
    const taskReceived: string[] = [];
    const sub = subscribeChannel(channel.task("task-42"), {
      listeners: { agent_busy: (e) => taskReceived.push(e.data as string) },
    });

    __dispatchForTest(
      envelope({ agent_id: "some-other-agent", run_id: "task-42", eventName: "agent_busy", raw: "busy-by-run-id" })
    );

    expect(taskReceived).toEqual(["busy-by-run-id"]);
    sub.close();
  });
});

describe("isHubRecentlyAlive", () => {
  // `networkStore.ts`'s health ping short-circuits to healthy when this
  // returns true, so it can skip a `fetch()` that would otherwise race the
  // browser's per-host connection pool against the hub's own connection
  // `lastEnvelopeAt` is
  // updated unconditionally at the top of `dispatch()` — no subscriber
  // needs to match for this to work.
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("is true immediately after a dispatch and false once the default 20s window elapses", () => {
    vi.setSystemTime(1_000_000);
    __dispatchForTest(envelope({ eventName: "text_delta" }));

    expect(isHubRecentlyAlive()).toBe(true);

    vi.setSystemTime(1_000_000 + 19_999);
    expect(isHubRecentlyAlive()).toBe(true);

    vi.setSystemTime(1_000_000 + 20_001);
    expect(isHubRecentlyAlive()).toBe(false);
  });

  it("respects a custom maxAgeMs argument", () => {
    vi.setSystemTime(2_000_000);
    __dispatchForTest(envelope({ eventName: "usage" }));

    expect(isHubRecentlyAlive(5_000)).toBe(true);

    vi.setSystemTime(2_000_000 + 5_001);
    expect(isHubRecentlyAlive(5_000)).toBe(false);
    // A longer window still considers the same event fresh.
    expect(isHubRecentlyAlive(60_000)).toBe(true);
  });

  it("a later dispatch resets the liveness window", () => {
    vi.setSystemTime(3_000_000);
    __dispatchForTest(envelope({ eventName: "run_started" }));

    vi.setSystemTime(3_000_000 + 15_000);
    expect(isHubRecentlyAlive()).toBe(true);

    __dispatchForTest(envelope({ eventName: "run_ended" }));
    vi.setSystemTime(3_000_000 + 15_000 + 15_000);
    // 15s since the second dispatch, still within the default 20s window —
    // would be false if the first dispatch's timestamp had stuck.
    expect(isHubRecentlyAlive()).toBe(true);
  });
});
