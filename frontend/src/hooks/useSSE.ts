import { useEffect, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { channel, subscribeChannel, type HubSubscription } from "../lib/sseHub";
import { useChatStore, inFlightKey, agentIdFromInFlightKey, isEventForActiveThread } from "../stores/chatStore";
import { useWorkflowStore } from "../stores/workflowStore";
import { useAgentTasklistStore } from "../stores/agentTasklistStore";
import { useArtifactStore, parseArtifactWriteOutput } from "../stores/artifactStore";
import { stripMcpPrefix } from "../components/chat/toolCallLabel";
import { maybeNotifyForSystemMessage } from "../lib/notifications/systemMessageAdapter";
import { maybeNotifyForAgentReply } from "../lib/notifications/agentReplyAdapter";
import type { Task, TasklistStatus, Thread } from "../types/api";
import type { FormPostedPayload, FormRequestPayload } from "../types/form";

/** On reconnect, how long to wait for the server to replay AgentBusy before
 *  concluding that the previously-active run ended while we were disconnected.
 *  The server emits AgentBusy synchronously at the start of the SSE stream,
 *  so this only needs to cover network + parse latency. */
const RECONNECT_GRACE_MS = 500;

/**
 * Manages an agent's real-time event subscription via the shared SSE hub.
 *
 * Subscribes when agentId is non-null, unsubscribes on agentId change or
 * unmount. Dispatches events to the chat store.
 */
export function useSSE(agentId: string | null): { connected: boolean } {
  const [connected, setConnected] = useState(false);
  const subscriptionRef = useRef<HubSubscription | null>(null);
  const receivedContentRef = useRef(false);
  // Keyed by inFlightKey (plain agent id, or an agent+thread composite) —
  // the SSE channel is per-agent, but in-flight state (and therefore what
  // needs re-confirming on reconnect) is per-thread, so a single ref can't
  // represent "all the threads of this agent that were streaming when we
  // dropped." See `cancelGraceTimer` / `handleStreamOpen` below.
  const graceTimersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());
  // Separate from `graceTimersRef` even though the key shape is identical —
  // a thread can have both an in-flight parent turn AND a running async
  // delegate at once, sharing the same key. Sharing one timer map would mean
  // reconfirming one (e.g. `run_started` cancelling the parent-turn timer)
  // incorrectly cancels the other's independent reconfirmation. See
  // `cancelDelegateGraceTimer` / `handleStreamOpen` below.
  const delegateGraceTimersRef = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());

  useEffect(() => {
    // Clean up any previous subscription
    cleanup();

    if (!agentId) {
      setConnected(false);
      return;
    }

    connect(agentId);

    return () => {
      cleanup();
      // Preserve the in-flight entry across disconnect so partial streaming
      // text survives a navigate-away / navigate-back. If the run ends while
      // we're disconnected we'll miss run_ended; on the next reconnect the
      // onopen grace timer clears stale state if AgentBusy isn't replayed.
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentId]);

  /** Cancels one thread's grace timer (`key` given), or every grace timer for
   *  this connection (`key` omitted — used on cleanup/unmount). */
  function cancelGraceTimer(key?: string) {
    const timers = graceTimersRef.current;
    if (key === undefined) {
      for (const t of timers.values()) clearTimeout(t);
      timers.clear();
      return;
    }
    const t = timers.get(key);
    if (t) {
      clearTimeout(t);
      timers.delete(key);
    }
  }

  /** Same shape as `cancelGraceTimer`, for `delegateGraceTimersRef`. */
  function cancelDelegateGraceTimer(key?: string) {
    const timers = delegateGraceTimersRef.current;
    if (key === undefined) {
      for (const t of timers.values()) clearTimeout(t);
      timers.clear();
      return;
    }
    const t = timers.get(key);
    if (t) {
      clearTimeout(t);
      timers.delete(key);
    }
  }

  function connect(id: string) {
    const store = useChatStore.getState;
    const wfStore = useWorkflowStore.getState;
    const agentTlStore = useAgentTasklistStore.getState;

    // Runs on the hub's first open AND on every subsequent reconnect of the
    // shared connection — mirrors the old per-agent EventSource's `onopen`,
    // which fired on initial connect, native reconnects, and manual
    // reconnects alike.
    function handleStreamOpen() {
      setConnected(true);
      // If there's preserved in-flight state (partial stream buffer) for this
      // agent — on ANY of its threads, since the channel is per-agent but
      // in-flight state is per-thread — the server has RECONNECT_GRACE_MS to
      // replay AgentBusy before we conclude that particular thread's run
      // ended while we were disconnected and clear its stale bubble. Each
      // thread gets its own timer so one thread's reconfirmation doesn't
      // cancel another's. Cancelled per-key by agent_busy / run_started below.
      for (const key of useChatStore.getState().inFlightByAgent.keys()) {
        if (agentIdFromInFlightKey(key) !== id) continue;
        cancelGraceTimer(key);
        graceTimersRef.current.set(
          key,
          setTimeout(() => {
            graceTimersRef.current.delete(key);
            useChatStore.getState().deleteInFlight(key);
          }, RECONNECT_GRACE_MS),
        );
      }
      // Same reconfirm-or-clear dance for outstanding async Delegate runs.
      // The server replays `delegate.started` at connect time for any
      // delegation still live in its `BackgroundAgentRegistry` (see
      // `build_system_replay_events` in `ao-server/src/routes/stream.rs`) —
      // cancelled below by the `delegate.started` listener. A server
      // restart drops that registry along with everything else, so nothing
      // replays and this timer clears the stale badge instead of leaving it
      // stuck "running" forever.
      for (const key of useChatStore.getState().runningDelegatesByThread.keys()) {
        if (agentIdFromInFlightKey(key) !== id) continue;
        cancelDelegateGraceTimer(key);
        delegateGraceTimersRef.current.set(
          key,
          setTimeout(() => {
            delegateGraceTimersRef.current.delete(key);
            useChatStore.getState().clearDelegateRunsForKey(key);
          }, RECONNECT_GRACE_MS),
        );
      }
    }

    // --- Named SSE event listeners ---

    const listeners: Record<string, (e: MessageEvent) => void> = {
      message_received(e) {
        const data = parsePayloadData(e.data);
        if (data?.message_id) {
          store().markMessageSent(data.message_id as string);
        }
      },

      message_processing_started(e) {
        const data = parsePayloadData(e.data);
        if (data?.message_id) {
          store().markMessageSeen(data.message_id as string);
        }
      },

      run_started(e) {
        if (isTasklistChannelEvent(e.data)) return;
        receivedContentRef.current = false;
        const data = parsePayloadData(e.data);
        const key = keyFor(id, data);
        cancelGraceTimer(key);
        store().ensureInFlight(key);
        store().patchAgentSnapshot(id, { has_active_run: true });
      },

      tool_call_started(e) {
        const data = parsePayloadData(e.data);
        if (data?.tool_name) {
          store().addInFlightToolCall(keyFor(id, data), {
            tool: data.tool_name as string,
            input: data.tool_input as Record<string, unknown> | undefined,
            label: data.label as string | undefined,
          });
        }
      },

      tool_call_completed(e) {
        const data = parsePayloadData(e.data);
        const key = keyFor(id, data);
        store().markInFlightToolCallDone(key);
        // Note: an async Delegate's own spawn call failing outright (e.g. no
        // spawner wired, bad target) needs no cleanup here — the backend only
        // emits `delegate.started` (which drives `beginDelegateRun`) after
        // the background handle is actually registered, so a synchronous
        // spawn failure never set a running marker in the first place.
        // ArtifactWrite's output JSON already carries everything the compact
        // inline card needs (id, title, kind, refresh_intent) — no separate
        // fetch. Register the card metadata and attach the id to this turn's
        // in-flight entry so the card renders immediately, mid-stream.
        // `tool_name` arrives MCP-qualified for CLI-mode agents (e.g.
        // `mcp__launchpad__ArtifactWrite`) — strip the transport prefix
        // before comparing, same as the chip-label path (`toolCallLabel.ts`).
        if (typeof data?.tool_name === "string" && stripMcpPrefix(data.tool_name) === "ArtifactWrite") {
          const card = parseArtifactWriteOutput(data.output);
          if (card) {
            useArtifactStore.getState().registerCard(card);
            // Freshly published this turn — default its inline card open
            // (see `liveIds` doc comment). Reload/scrollback registration in
            // `MessageList.tsx` never calls this, so old cards stay collapsed.
            useArtifactStore.getState().markCardLive(card.id);
            store().appendInFlightArtifactId(key, card.id);
          }
        }
      },

      tool_use_started(e) {
        const data = parsePayloadData(e.data);
        if (data?.tool_use_id && data?.tool_name) {
          store().addInFlightToolUse(
            keyFor(id, data),
            data.tool_use_id as string,
            data.tool_name as string,
            data.input as Record<string, unknown> | undefined,
          );
        }
      },

      tool_use_completed(e) {
        const data = parsePayloadData(e.data);
        if (data?.tool_use_id) {
          store().removeInFlightAgentAction(keyFor(id, data), data.tool_use_id as string);
        }
      },

      thinking_started(e) {
        const data = parsePayloadData(e.data);
        // Force a synchronous commit so the empty "Thinking…" pill renders
        // BEFORE any subsequent thinking_delta events can batch in. Without
        // flushSync, EventSource often delivers thinking_started + several
        // thinking_delta messages within the same task tick — React 18
        // auto-batches the resulting setState calls into one paint and the
        // pill mounts already populated, robbing the user of the
        // "Thinking…" → text-streaming-in transition. flushSync isolates
        // the start commit so the streaming feel survives even when the
        // server's chunks arrive bunched up. The mount itself is cheap
        // (one in-flight Map insert + a tiny pill component) so the
        // synchronous render cost is negligible.
        flushSync(() => {
          store().startInFlightThinking(keyFor(id, data));
        });
      },

      thinking_delta(e) {
        const data = parsePayloadData(e.data);
        const text = data?.text;
        if (typeof text === "string") {
          store().appendInFlightThinkingDelta(keyFor(id, data), text);
        }
      },

      thinking_ended(e) {
        const data = parsePayloadData(e.data);
        const elapsedMs = (data?.elapsed_ms as number | undefined) ?? 0;
        store().endInFlightThinking(keyFor(id, data), elapsedMs);
      },

      text_delta(e) {
        if (isTasklistChannelEvent(e.data)) return;
        const data = parsePayloadData(e.data);
        if (data?.text != null) {
          receivedContentRef.current = true;
          const key = keyFor(id, data);
          // Clear classic tool-call chips (pre-streaming) but preserve agent
          // action chips — those close via agent_action_completed.
          useChatStore.setState((state) => {
            const current = state.inFlightByAgent.get(key);
            if (!current || current.activeToolCalls.length === 0) return state;
            const filtered = current.activeToolCalls.filter((tc) => tc.action_id != null);
            if (filtered.length === current.activeToolCalls.length) return state;
            const next = new Map(state.inFlightByAgent);
            next.set(key, { ...current, activeToolCalls: filtered });
            return { inFlightByAgent: next };
          });
          store().appendInFlightDelta(key, data.text as string);
        }
      },

      agent_action_started(e) {
        const data = parsePayloadData(e.data);
        const actionId = data?.action_id as string | undefined;
        const summary = data?.summary as string | undefined;
        if (actionId && summary) {
          store().addInFlightAgentAction(keyFor(id, data), actionId, summary);
        }
      },

      agent_action_completed(e) {
        const data = parsePayloadData(e.data);
        const actionId = data?.action_id as string | undefined;
        if (actionId) {
          store().removeInFlightAgentAction(keyFor(id, data), actionId);
        }
      },

      tool_progress(e) {
        const data = parsePayloadData(e.data);
        if (!data?.tasklist_id) return;
        const itemsDone = (data.items_done as number) ?? 0;
        const itemsTotal = (data.items_total as number) ?? 0;
        const lastTitle = data.last_terminal_task_title as string | undefined;
        let label = "Using TodoList";
        if (itemsTotal > 0) {
          label = `Using TodoList · ${itemsDone}/${itemsTotal} done`;
          if (lastTitle) label += ` · ${lastTitle}`;
        }
        store().patchTodoCreateProgress(keyFor(id, data), label);
      },

      usage(e) {
        const data = parsePayloadData(e.data);
        if (!data) return;
        // Backend emits one `usage` event per provider call. The CLI runner's
        // tool-use continuation respawn loop produces one usage event per
        // respawn — accumulate across them so the strip reflects the whole
        // turn, not just the final loop. Reset happens in deleteInFlight, i.e.
        // when the bubble truly tears down (the "completion marker" — not an
        // intermediate text_complete between loops).
        store().accumulateUsage(keyFor(id, data), {
          input: (data.input_tokens as number) ?? 0,
          output: (data.output_tokens as number) ?? 0,
          cacheRead: (data.cache_read_tokens as number) ?? 0,
          cacheCreation: (data.cache_creation_tokens as number) ?? 0,
          total: (data.total_tokens as number) ?? 0,
        });
      },

      text_complete(e) {
        if (isTasklistChannelEvent(e.data)) return;
        const data = parsePayloadData(e.data);
        if (data?.text != null) {
          // Finalizes the agent message into the transcript but keeps the
          // in-flight entry alive — the bubble stays mounted across any
          // skill-load handoff and the next RunStarted/text_delta picks up
          // immediately without a flash.
          store().finalizeInFlightText(keyFor(id, data), data.text as string);
          const eventThreadId = typeof data?.thread_id === "string" ? data.thread_id : undefined;
          try {
            maybeNotifyForAgentReply({ agentId: id, threadId: eventThreadId, text: data.text as string });
          } catch (err) {
            console.warn("[SSE] maybeNotifyForAgentReply failed:", err);
          }
        }
      },

      // Hidden transcript entries persisted mid-run (e.g. the skill-body
      // injection written between two agent turns). Only push into the visible
      // arrays when this SSE stream's agent is the currently selected one AND
      // the entry's thread is the one currently loaded — otherwise the entry
      // leaks into whatever thread the user happens to be viewing. The source
      // agent's cache entry for that specific thread (per-thread key, see
      // `messageCache`'s doc comment in chatStore.ts) is only updated when the
      // thread matches too; a mismatched thread's entry is already persisted
      // server-side and surfaces on its own the next time the user switches
      // there.
      hidden_transcript_entry(e) {
        const data = parsePayloadData(e.data);
        const entry = data?.entry as import("../types/api").TranscriptEntry | undefined;
        if (!entry) return;
        const eventThreadId = typeof data?.thread_id === "string" ? data.thread_id : undefined;
        useChatStore.setState((state) => {
          const onActiveThread = isEventForActiveThread(id, eventThreadId, state.threadsByAgent, state.selectedThreadIdByAgent);
          if (!onActiveThread) {
            console.debug(
              `[SSE] hidden_transcript_entry source=${id} thread=${eventThreadId ?? "(default)"} not active thread — dropped`,
            );
            return state;
          }
          const isSelected = state.selectedAgentId === id;
          console.debug(
            `[SSE] hidden_transcript_entry source=${id} selected=${state.selectedAgentId ?? "(none)"} applied_to_visible=${isSelected}`,
          );
          const cacheKey = inFlightKey(id, eventThreadId);
          const cachedEntry = state.messageCache.get(cacheKey);
          let cache = state.messageCache;
          if (cachedEntry) {
            cache = new Map(state.messageCache);
            cache.set(cacheKey, {
              ...cachedEntry,
              allMessages: [...cachedEntry.allMessages, entry],
              lastAccessed: Date.now(),
            });
          }
          if (!isSelected) {
            return { messageCache: cache };
          }
          return {
            messages: [...state.messages, entry],
            allMessages: [...state.allMessages, entry],
            messageCache: cache,
          };
        });
      },

      run_ended(e) {
        const data = parsePayloadData(e.data);
        const reason = data?.reason as string | undefined;
        const key = keyFor(id, data);
        const eventThreadId = typeof data?.thread_id === "string" ? data.thread_id : undefined;

        // Defense-in-depth: if there's still unfinalized text OR a pending
        // artifact id sitting in the in-flight entry when the run ends,
        // drain it into the transcript before teardown. Backend `flush_text`
        // + `persist_pending` is supposed to emit a `text_complete` event so
        // `finalizeInFlightText` runs first, but the upstream pipeline has
        // had a few ways to drop it on the floor: a runner-task panic (now
        // caught upstream, but the symptom existed), a tool-failure mid-stream
        // that breaks the continuation loop, or just a `text_complete` that
        // arrives out of order on a slow connection. The artifact case is
        // architectural rather than a rare drop: when `ArtifactWrite` is the
        // terminal action of a turn (text, then the tool call, then nothing
        // else), the backend's only `text_complete` fires BEFORE the tool
        // completes — so it finalizes with an empty `artifactIds` list, and
        // `appendInFlightArtifactId` (from `tool_call_completed`) lands the id
        // on the live in-flight entry with no further `text_complete` ever
        // arriving to snapshot it. Without this guard, `scheduleInFlightTeardown`
        // / `deleteInFlight` unmounts the bubble — with the streaming text
        // still in `textBuffer`, or the artifact id still only on the
        // in-flight entry — so the user sees the message/card disappear and
        // only reappear after a navigate-away / navigate-back (which
        // refetches from disk, where `persist_pending` did its job). One
        // layer of belt for one layer of suspenders.
        const inFlightEntry = useChatStore.getState().inFlightByAgent.get(key);
        if (inFlightEntry && (inFlightEntry.textBuffer.length > 0 || inFlightEntry.artifactIds.length > 0)) {
          store().finalizeInFlightText(key, inFlightEntry.textBuffer);
        }

        store().clearInFlightToolCalls(key);
        // Thread-scoped: only the run's OWN thread's pending form is
        // affected by this run ending — a form still pending on a different
        // thread of this same agent (or a different agent entirely, since
        // this handler only ever fires for events on this agent's own SSE
        // channel) must survive untouched.
        //
        // A sync form genuinely suspends its run, so a run ending while its
        // own (agentId, threadId) slot is still populated means that form
        // was never answered — that's the orphaned case, not a "the run
        // finished normally" case. Mark it orphaned in place rather than
        // deleting it: deleting here is exactly the silent-disappearance bug
        // (the form the user is looking at vanishes with no trace) that the
        // orphaned representation exists to prevent. The common path — an
        // answered form already cleared its own slot before `run_ended` ever
        // arrives (see `PendingFormOverlay`/`ChatView`'s submit handlers) —
        // is a no-op here.
        store().markPendingFormOrphaned(id, eventThreadId);
        store().patchAgentSnapshot(id, { has_active_run: false });

        // Run ended → the backend has flushed this turn's transcript entries to
        // disk (persist_pending precedes the run_ended emit). Reconcile any
        // artifact the run produced so its card resolves inline in the thread
        // bubble instead of only in the Assets panel. Fired here (not on
        // text_complete) precisely because that flush has now happened.
        store().syncRunArtifacts(key);

        // Schedule debounced teardown — a skill-load follow-up queues a new
        // user message that triggers RunStarted for the same agent within a
        // few ms, and ensureInFlight cancels the pending teardown so the
        // bubble stays mounted. On a true end, the timer fires and the entry
        // is cleared. Cancelled runs delete immediately.
        if (reason === "Cancelled") {
          store().deleteInFlight(key);
        } else {
          store().scheduleInFlightTeardown(key);
        }

        // Surface timeout / error feedback to the user
        let label: string | null = null;
        if (reason === "TimedOut") {
          label = "Run timed out. The agent process was terminated. You can try sending your message again.";
        } else if (reason === "NoOutputTimeout") {
          label = "No output received — timed out. The agent process was terminated. You can try sending your message again.";
        } else if (reason === "Error") {
          label = "The agent encountered an error and the run was terminated. Check agent profile settings or try again.";
        }

        if (!label && reason === "Completed" && !receivedContentRef.current) {
          label = "Run completed but no output was received. The agent may have exited immediately — check the agent profile configuration.";
        }

        if (label) {
          const systemMsg = {
            ts: new Date().toISOString(),
            role: "system" as const,
            content: `⚠️ ${label}`,
            event_type: "system" as const,
          };
          const eventThreadId = typeof data?.thread_id === "string" ? data.thread_id : undefined;
          useChatStore.setState((state) => {
            // This run's own conversation may not be the one on-screen (e.g. a
            // dormant background agent, or a different thread of this same
            // agent) — pushing unconditionally would attribute the error to
            // whatever the user happens to be looking at right now.
            const applies =
              state.selectedAgentId === id &&
              isEventForActiveThread(id, eventThreadId, state.threadsByAgent, state.selectedThreadIdByAgent);
            console.debug(
              `[SSE] run_ended_system source=${id} selected=${state.selectedAgentId ?? "(none)"} reason=${reason ?? "(none)"} applied=${applies}`,
            );
            if (!applies) return state;
            return { messages: [...state.messages, systemMsg] };
          });
        }
      },

      agent_busy(e) {
        // Reconnect/connect-synthesized event: `stream_events` in ao-server
        // replays one of these per still-active run so a client that
        // (re)connects mid-run doesn't miss the fact that it's running.
        // `thread_id` on it now reflects the actual thread the run belongs to
        // (via InstanceRegistry::thread_for_run) rather than always being
        // omitted — omitted previously meant every replay landed on the plain
        // agent key, i.e. the default/Main thread, regardless of which thread
        // was really busy. That was the root cause of Main's typing indicator
        // getting stuck "on" for runs that were actually happening on a
        // different thread, with no matching run_ended ever arriving to clear
        // it. keyFor is unchanged — it already read `thread_id` correctly.
        const key = keyFor(id, parsePayloadData(e.data));
        cancelGraceTimer(key);
        store().ensureInFlight(key);
      },

      // --- Workflow SSE event listeners ---

      workflow_task_created(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handleTaskCreated(data as unknown as import("../stores/workflowStore").WorkflowTaskCreatedEvent);
        }
      },

      phase_started(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handlePhaseStarted(data as unknown as import("../stores/workflowStore").PhaseStartedEvent);
        }
      },

      phase_completed(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handlePhaseCompleted(data as unknown as import("../stores/workflowStore").PhaseCompletedEvent);
        }
      },

      phase_skipped(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handlePhaseSkipped(data as unknown as import("../stores/workflowStore").PhaseSkippedEvent);
        }
      },

      phase_failed(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handlePhaseFailed(data as unknown as import("../stores/workflowStore").PhaseFailedEvent);
        }
      },

      phase_paused(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handlePhasePaused(data as unknown as import("../stores/workflowStore").PhasePausedEvent);
        }
      },

      workflow_completed(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handleWorkflowCompleted(data as unknown as import("../stores/workflowStore").WorkflowCompletedEvent);
        }
      },

      workflow_task_started(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handleTaskStarted(data as unknown as import("../stores/workflowStore").WorkflowTaskStartedEvent);
        }
      },

      workflow_task_failed(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handleTaskFailed(data as unknown as import("../stores/workflowStore").WorkflowTaskFailedEvent);
        }
      },

      workflow_task_stopped(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handleTaskStopped(data as unknown as import("../stores/workflowStore").WorkflowTaskStoppedEvent);
        }
      },

      system_message(e) {
        const data = parsePayloadData(e.data);
        if (data?.text) {
          const severity = typeof data?.severity === "string" ? data.severity : undefined;
          const systemMsg = {
            ts: new Date().toISOString(),
            role: "system" as const,
            content: data.text as string,
            event_type: "workflow_system" as const,
            metadata: severity ? { severity } : null,
          };
          const eventThreadId = typeof data?.thread_id === "string" ? data.thread_id : undefined;
          try {
            maybeNotifyForSystemMessage(id, data, eventThreadId);
          } catch (err) {
            console.warn("[SSE] maybeNotifyForSystemMessage failed:", err);
          }
          useChatStore.setState((state) => {
            const applies =
              state.selectedAgentId === id &&
              isEventForActiveThread(id, eventThreadId, state.threadsByAgent, state.selectedThreadIdByAgent);
            console.debug(
              `[SSE] system_message source=${id} selected=${state.selectedAgentId ?? "(none)"} applied=${applies}`,
            );
            if (!applies) return state;
            return {
              messages: [...state.messages, systemMsg],
              allMessages: [...state.allMessages, systemMsg],
            };
          });
        }
      },

      // --- Agent-scoped tasklist SSE event listeners ---

      "tasklist.created"(e) {
        const data = parsePayloadData(e.data);
        const owner = data?.owner as { kind?: string; agent_id?: string } | undefined;
        const tasklistId = data?.tasklist_id;
        if (
          owner?.kind === "agent" &&
          owner.agent_id === id &&
          typeof tasklistId === "string" &&
          !data?.project_id
        ) {
          void agentTlStore().applyTasklistCreated(id, tasklistId);
        }
      },

      "tasklist.task_updated"(e) {
        const data = parsePayloadData(e.data);
        const owner = data?.owner as { kind?: string; agent_id?: string } | undefined;
        const tasklistId = data?.tasklist_id;
        const task = data?.task;
        if (
          owner?.kind === "agent" &&
          owner.agent_id === id &&
          typeof tasklistId === "string" &&
          task &&
          typeof task === "object" &&
          typeof (task as Task).id === "string" &&
          !data?.project_id
        ) {
          void agentTlStore().applyTaskUpdated(id, tasklistId, task as Task);
        }
      },

      "tasklist.task_added"(e) {
        const data = parsePayloadData(e.data);
        const owner = data?.owner as { kind?: string; agent_id?: string } | undefined;
        const tasklistId = data?.tasklist_id;
        const taskId = data?.task_id;
        if (
          owner?.kind === "agent" &&
          owner.agent_id === id &&
          typeof tasklistId === "string" &&
          typeof taskId === "string" &&
          !data?.project_id
        ) {
          void agentTlStore().applyTaskAdded(id, tasklistId, taskId);
        }
      },

      "tasklist.completed"(e) {
        const data = parsePayloadData(e.data);
        const owner = data?.owner as { kind?: string; agent_id?: string } | undefined;
        const tasklistId = data?.tasklist_id;
        if (
          owner?.kind === "agent" &&
          owner.agent_id === id &&
          typeof tasklistId === "string" &&
          !data?.project_id
        ) {
          agentTlStore().applyTasklistCompleted(id, tasklistId);
        }
      },

      "tasklist.failed"(e) {
        const data = parsePayloadData(e.data);
        const owner = data?.owner as { kind?: string; agent_id?: string } | undefined;
        const tasklistId = data?.tasklist_id;
        const reason = typeof data?.reason === "string" ? data.reason : null;
        if (
          owner?.kind === "agent" &&
          owner.agent_id === id &&
          typeof tasklistId === "string" &&
          !data?.project_id
        ) {
          agentTlStore().applyTasklistFailed(id, tasklistId, reason);
        }
      },

      "tasklist.status_changed"(e) {
        const data = parsePayloadData(e.data);
        const owner = data?.owner as { kind?: string; agent_id?: string } | undefined;
        const tasklistId = data?.tasklist_id;
        const status = data?.status;
        if (
          owner?.kind === "agent" &&
          owner.agent_id === id &&
          typeof tasklistId === "string" &&
          typeof status === "string" &&
          !data?.project_id
        ) {
          agentTlStore().applyTasklistStatusChanged(
            id,
            tasklistId,
            status as TasklistStatus,
          );
        }
      },

      // When the agent creates a tasklist, auto-open the Todo panel so the user
      // sees the in-flight work without having to click the header icon. The
      // header's ListTodo button still carries the persistent in-progress badge
      // (driven separately by `useAgentTasklistsForAgent`) for after the user
      // closes the panel — opening here is a one-shot nudge, not a sticky
      // override. The streaming "Using TodoList · X/Y done" pill on the agent
      // bubble continues to surface in-flight progress for sync tasklists; we
      // no longer render the inline "X tasks created" / "X/X succeeded" banners
      // since they duplicated information the pill + panel already convey.
      // `id` is this SSE channel's own agent, so this only opens the panel for
      // whichever chat actually created the tasklist — never for whatever
      // other agent's chat the user happens to be looking at (see
      // `activePanelByAgent`'s docstring in chatStore.ts).
      "todo_list.created"(e) {
        const data = parsePayloadData(e.data);
        if (data?.tasklist_id && typeof data.item_count === "number") {
          store().setActivePanel(id, "todos");
        }
      },

      // When an agent-owned todo list reaches a terminal state, the engine wakes
      // the agent with a hidden completion-summary message that triggers a fresh
      // turn. Without a marker, that follow-up reply looks like it arrived out of
      // nowhere. Drop a system pill into the timeline so the user can see the
      // reply was triggered by the todo list finishing.
      //
      // Gating note: the backend does not yet thread a real thread_id onto this
      // event (see `emit_todo_list_complete` in `task_feeder.rs`) — it always
      // persists the marker to the agent's default-thread transcript. So
      // `eventThreadId` below is always `undefined` today, and the gate only
      // lets the live pill through while the default thread is the one being
      // viewed — which matches where it's actually written on disk. If a
      // non-default thread launched the todo list, the pill won't show live
      // there, but it also won't show up in the WRONG thread the way it did
      // before this gate (it'll be visible on the default thread, matching
      // persisted state). keyFor-style thread tagging can be added here for
      // free once the backend closes that gap.
      "todo_list.complete"(e) {
        const data = parsePayloadData(e.data);
        if (!data?.tasklist_id) return;
        const counts = data.counts as
          | { succeeded?: number; failed?: number; skipped?: number }
          | undefined;
        const succeeded = counts?.succeeded ?? 0;
        const failed = counts?.failed ?? 0;
        const skipped = counts?.skipped ?? 0;
        const status = typeof data.status === "string" ? data.status : "completed";
        const verb =
          status === "failed"
            ? "ended with failures"
            : status === "cancelled"
              ? "was cancelled"
              : "completed";
        const detail: string[] = [`${succeeded} done`];
        if (failed > 0) detail.push(`${failed} failed`);
        if (skipped > 0) detail.push(`${skipped} skipped`);
        const systemMsg = {
          ts: new Date().toISOString(),
          role: "system" as const,
          content: `Todo list ${verb} · ${detail.join(", ")}`,
          event_type: "todo_list_complete" as const,
        };
        const eventThreadId = typeof data?.thread_id === "string" ? data.thread_id : undefined;
        useChatStore.setState((state) => {
          const applies =
            state.selectedAgentId === id &&
            isEventForActiveThread(id, eventThreadId, state.threadsByAgent, state.selectedThreadIdByAgent);
          if (!applies) return state;
          return {
            messages: [...state.messages, systemMsg],
            allMessages: [...state.allMessages, systemMsg],
          };
        });
      },

      // Fired once, the instant an async Delegate's background run is
      // registered server-side (before it starts producing output) — see
      // `AgentEventPayload::DelegateStarted`. Also replayed synthetically at
      // stream-connect time for any delegation still live server-side (see
      // `handleStreamOpen` above), which is what lets `cancelDelegateGraceTimer`
      // reconfirm a run that survived a mere reconnect rather than a server
      // restart. `beginDelegateRun` is a `Map` upsert keyed by id, so
      // reconfirming an already-tracked `delegation_id` is a harmless no-op
      // (same name/start time), never a double-count. Runs regardless of
      // which thread is active — thread-list surfaces (ThreadsPanel,
      // ThreadTabStrip, HomeSidebar) need this for threads the user never
      // opened. `spawned_at` is the backend's real spawn timestamp (ISO 8601
      // UTC, carried through unchanged on replay) — converted to epoch ms
      // here so the store never has to parse dates, and so a reconnect replay
      // reports the delegate's true elapsed time instead of restarting the
      // clock at replay time.
      "delegate.started"(e) {
        const data = parsePayloadData(e.data);
        if (typeof data?.delegation_id !== "string") return;
        if (typeof data?.delegate_name !== "string") return;
        if (typeof data?.spawned_at !== "string") return;
        const startedAt = Date.parse(data.spawned_at);
        if (Number.isNaN(startedAt)) return;
        const key = keyFor(id, data);
        store().beginDelegateRun(key, data.delegation_id, data.delegate_name, startedAt);
        cancelDelegateGraceTimer(key);
      },

      // When an async delegate finishes, drop a completion pill into the parent
      // agent's timeline so the user can see it without reloading the page.
      // `QueueDelegateCompletionSink` (delegate_completion.rs) tags this event
      // with the originating thread's real thread_id (falling back to the
      // agent's default-thread transcript only for callers that never wired
      // one), so `eventThreadId` below correctly gates the pill to the thread
      // that actually launched the delegate rather than always the default.
      "delegate.complete"(e) {
        const data = parsePayloadData(e.data);
        // Clear the running-delegate marker `delegate.started` set, even
        // when this event is for a thread the user isn't currently looking
        // at — thread-list surfaces need this to fire regardless of which
        // thread is active, unlike the pill below (which is gated to the
        // active thread/agent).
        if (typeof data?.delegation_id === "string") {
          store().endDelegateRun(keyFor(id, data), data.delegation_id);
        }
        if (!data?.delegate_name) return;
        const name = data.delegate_name as string;
        const status = typeof data.status === "string" ? data.status : "completed";
        const durationMs =
          typeof data.duration_ms === "number" ? data.duration_ms : null;
        const verb =
          status === "failed"
            ? "failed"
            : status === "cancelled"
              ? "cancelled"
              : "completed";
        const durationSuffix =
          durationMs !== null ? ` · ${(durationMs / 1000).toFixed(1)}s` : "";
        const systemMsg = {
          ts: new Date().toISOString(),
          role: "system" as const,
          content: `Delegate '${name}' ${verb}${durationSuffix}`,
          event_type: "delegate_complete" as const,
          metadata: { status, delegate_name: name },
        };
        const eventThreadId = typeof data?.thread_id === "string" ? data.thread_id : undefined;
        useChatStore.setState((state) => {
          const applies =
            state.selectedAgentId === id &&
            isEventForActiveThread(id, eventThreadId, state.threadsByAgent, state.selectedThreadIdByAgent);
          if (!applies) return state;
          return {
            messages: [...state.messages, systemMsg],
            allMessages: [...state.allMessages, systemMsg],
          };
        });
      },

      memory_saved(e) {
        const data = parsePayloadData(e.data);
        if (data?.content) {
          const raw = data.content as string;
          const truncated = raw.length > 80 ? raw.slice(0, 80) + "…" : raw;
          const scope = data.scope === "Global" ? "global" : "agent";
          const systemMsg = {
            ts: new Date().toISOString(),
            role: "system" as const,
            content: `Memory saved (${scope}): ${truncated}`,
            event_type: "memory_saved" as const,
          };
          const eventThreadId = typeof data?.thread_id === "string" ? data.thread_id : undefined;
          useChatStore.setState((state) => {
            const applies =
              state.selectedAgentId === id &&
              isEventForActiveThread(id, eventThreadId, state.threadsByAgent, state.selectedThreadIdByAgent);
            if (!applies) return state;
            return {
              messages: [...state.messages, systemMsg],
              allMessages: [...state.allMessages, systemMsg],
            };
          });
        }
      },

      // Fired by the `RenameThread` tool (explicit `title`) and by the
      // server's first-message auto-title hook (`auto_title`) — exactly one of
      // the two fields is present per event. Patch the matching thread row in
      // place so the tab strip updates live instead of waiting for the next
      // full thread-list refetch.
      thread_renamed(e) {
        const data = parsePayloadData(e.data);
        const threadId = data?.thread_id as string | undefined;
        if (!threadId) return;
        const patch: { title?: string; auto_title?: string } = {};
        if (typeof data?.title === "string") patch.title = data.title;
        if (typeof data?.auto_title === "string") patch.auto_title = data.auto_title;
        if (Object.keys(patch).length === 0) return;
        useChatStore.getState().patchThreadLive(threadId, patch);
      },

      // Fired the instant a scheduled task fire or recurring assignment run
      // creates a new `Fresh`/`Dedicated` thread server-side — there's no
      // interactive HTTP request in that path to hand the row back directly
      // (contrast the "+" new-thread button, whose REST response IS the
      // thread). Append it into this agent's thread list live so the tab strip
      // picks it up immediately instead of only on the next full `loadThreads`
      // refetch (e.g. navigating away and back).
      thread_created(e) {
        const data = parsePayloadData(e.data);
        const thread = data?.thread as Thread | undefined;
        if (!thread?.id) return;
        store().addThreadLive(id, thread);
      },

      form_request(e) {
        // `data.thread_id` (merged in by `parsePayloadData` from the SSE
        // envelope) is what lets `setPendingForm` bucket this under the
        // owning thread instead of a single per-agent slot — see
        // `pendingFormByAgent`'s docstring in chatStore.ts.
        const data = parsePayloadData(e.data) as FormRequestPayload | null;
        if (!data?.form_id) return;
        store().setPendingForm(id, data);
      },

      // Async form posted: a `pending_forms` entry is now set on the server
      // snapshot, scoped to the run's own thread (`event.thread_id`; `undefined`
      // = default thread), and the form_request transcript entry is on disk.
      // Upsert the matching entry in the in-memory snapshot immediately — keyed
      // by thread_id, same as the backend — so the derivation in ChatView can
      // resolve as soon as the transcript entry arrives, then trigger a
      // background transcript refresh via selectAgent (cache-hit path: instant
      // render from cache + background fetch).
      //
      // Unlike `error`/`memory_saved`, this write is NOT gated on
      // `isEventForActiveThread`: `agents` is a global snapshot list, not a
      // single-thread-shaped array like `messages`, so recording a pending
      // form for a background thread here is correct, not a leak — it's what
      // lets switching to that thread later show the form immediately instead
      // of waiting on the next `fetchAgents` poll. `pendingFormForThread`
      // (chatStore.ts) is what actually scopes rendering to the active thread.
      form_posted(e) {
        const data = parsePayloadData(e.data) as FormPostedPayload | null;
        if (!data?.form_id) return;
        const formId = data.form_id;
        const eventThreadId = typeof data.thread_id === "string" ? data.thread_id : undefined;
        // Wrap the event's flat spec in the `{form_id, spec, mode}` envelope
        // `PendingForm.spec` expects (`PendingFormRequestMeta`) — same shape
        // the backend's own `pending_forms` snapshot pointer carries, so this
        // optimistic entry needs no follow-up patch once `fetchAgents()` lands.
        const spec = data.spec ? { form_id: formId, spec: data.spec, mode: "async" as const } : null;
        useChatStore.setState((state) => {
          // Same sparse-map hygiene as `setPendingForm`/`setPendingAsyncFormId` —
          // a freshly-arriving form must never inherit a stale minimized flag
          // left over on this (agent, thread) slot.
          const key = inFlightKey(id, eventThreadId);
          const nextMinimized = { ...state.minimizedFormByKey };
          delete nextMinimized[key];
          return {
            agents: state.agents.map((a) => {
              if (a.agent_id !== id) return a;
              const siblings = (a.pending_forms ?? []).filter(
                (f) => (f.thread_id ?? undefined) !== eventThreadId
              );
              return {
                ...a,
                pending_forms: [...siblings, { thread_id: eventThreadId ?? null, form_id: formId, spec }],
              };
            }),
            minimizedFormByKey: nextMinimized,
          };
        });
        void useChatStore.getState().fetchAgents();
        void useChatStore.getState().selectAgent(id);
      },

      error(e) {
        // Named "error" event from server (AgentEventPayload::Error)
        const data = parsePayloadData((e as MessageEvent).data);
        if (data) {
          console.error("[SSE] agent error:", data);
          const errorMsg = (data.message as string) || "An unexpected error occurred.";
          const systemMsg = {
            ts: new Date().toISOString(),
            role: "system" as const,
            content: `⚠️ ${errorMsg}`,
            event_type: "system" as const,
          };
          const eventThreadId = typeof data?.thread_id === "string" ? data.thread_id : undefined;
          useChatStore.setState((state) => {
            const applies =
              state.selectedAgentId === id &&
              isEventForActiveThread(id, eventThreadId, state.threadsByAgent, state.selectedThreadIdByAgent);
            console.debug(
              `[SSE] error_system source=${id} selected=${state.selectedAgentId ?? "(none)"} applied=${applies}`,
            );
            if (!applies) return state;
            return { messages: [...state.messages, systemMsg] };
          });
        }
        store().deleteInFlight(keyFor(id, data));
      },
    };

    subscriptionRef.current = subscribeChannel(channel.agent(id), {
      listeners,
      onOpen: handleStreamOpen,
      onReconnect: handleStreamOpen,
    });
  }

  function cleanup() {
    cancelGraceTimer();
    cancelDelegateGraceTimer();
    if (subscriptionRef.current) {
      subscriptionRef.current.close();
      subscriptionRef.current = null;
    }
  }

  return { connected };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Returns true if the raw SSE event JSON belongs to a tasklist sub-channel
 * (`agent_id` starts with `"tasklist:"`).
 *
 * Per-task subagent runs emit events on `tasklist:{id}` rather
 * than the parent agent channel, so the backend SSE endpoint already filters
 * them out. This check is client-side defense-in-depth: if a future routing
 * change were to let a tasklist-channel event slip through to the agent stream,
 * this guard prevents it from rendering as a main-chat bubble. `channel.agent`
 * already excludes `tasklist:{id}` keys by construction, so this should never
 * trip in practice — kept as a second, independent layer.
 *
 * Exported for use in tests.
 */
export function isTasklistChannelEvent(raw: string): boolean {
  try {
    const event = JSON.parse(raw) as { agent_id?: unknown };
    return typeof event.agent_id === "string" && event.agent_id.startsWith("tasklist:");
  } catch {
    return false;
  }
}


/**
 * Parse the SSE data string (full AgentEvent JSON) and extract the payload's
 * `data` field, merging in the envelope's `thread_id` tag.
 *
 * AgentEvent.payload is serialised as `{ type: "...", data: { ... } }`, with
 * `thread_id` living as a sibling of `payload` rather than inside it — merged
 * in here so callers can read `data.thread_id` alongside the payload's own
 * fields via `keyFor` below, without a second JSON.parse per event. Present
 * only for runs on a non-default thread; the default thread omits it
 * server-side for byte-exact back-compat, so most events see it stay
 * `undefined` here too. Returns the inner `data` object (possibly just
 * `{ thread_id }` for unit variants like RunStarted), or null if parsing
 * fails / there's genuinely nothing to report.
 */
function parsePayloadData(raw: string): Record<string, unknown> | null {
  try {
    const event = JSON.parse(raw);
    const base = (event?.payload?.data as Record<string, unknown>) ?? {};
    if (typeof event?.thread_id === "string") {
      base.thread_id = event.thread_id;
    }
    return Object.keys(base).length > 0 ? base : null;
  } catch {
    console.warn("[SSE] failed to parse event data:", raw);
    return null;
  }
}

/**
 * Resolves the `inFlightByAgent`/`usageByAgent` map key for an event: the
 * event's own `thread_id` tag (see `parsePayloadData`) composed with the
 * connection's agent id, or just the plain agent id when untagged (the
 * common default-thread case).
 */
function keyFor(id: string, data: Record<string, unknown> | null): string {
  const threadId = data?.thread_id;
  return typeof threadId === "string" ? inFlightKey(id, threadId) : id;
}
