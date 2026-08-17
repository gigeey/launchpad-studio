import { useEffect, useRef, useState } from "react";
import { flushSync } from "react-dom";
import { useProjectStore } from "../stores/projectStore";
import { useChatStore } from "../stores/chatStore";
import { channel, subscribeChannel, type HubSubscription } from "../lib/sseHub";
import { parsePayloadData } from "./sseUtils";
import type { TranscriptEntry } from "../types/api";
import type { FormRequestPayload } from "../types/form";

// Minimum ms between tasklist-event-triggered re-fetches (debounce rapid bursts).
const TASKLIST_REFETCH_DELAY = 800;

export function useProjectSSE(projectId: string | null): { connected: boolean } {
  const [connected, setConnected] = useState(false);
  const subscriptionRef = useRef<HubSubscription | null>(null);

  useEffect(() => {
    if (subscriptionRef.current) {
      subscriptionRef.current.close();
      subscriptionRef.current = null;
    }

    if (!projectId) {
      setConnected(false);
      return;
    }

    const id = projectId;
    const store = useProjectStore.getState;
    const chatStore = useChatStore.getState;
    const projectKey = `project:${id}`;

    // Debounced tasklist refresh — collapses rapid bursts from task_updated storms.
    let tasklistRefetchTimer: ReturnType<typeof setTimeout> | null = null;
    const scheduleTasklistRefetch = () => {
      if (tasklistRefetchTimer) clearTimeout(tasklistRefetchTimer);
      tasklistRefetchTimer = setTimeout(() => {
        store().fetchProjectTasklists(id).catch(() => {});
      }, TASKLIST_REFETCH_DELAY);
    };

    const listeners: Record<string, (e: MessageEvent) => void> = {
      run_started() {
        store().setTyping(true);
        chatStore().ensureInFlight(projectKey);
        chatStore().setInFlightTyping(projectKey, true);
      },

      text_delta(e) {
        const data = parsePayloadData(e.data);
        if (typeof data?.text === "string") {
          store().appendStreamingDelta(data.text as string);
          // Drop classic (input-less lifecycle) tool chips on first text, but
          // keep action-keyed chips — those close via their own *_completed events.
          useChatStore.setState((state) => {
            const current = state.inFlightByAgent.get(projectKey);
            if (!current || current.activeToolCalls.length === 0) return state;
            const filtered = current.activeToolCalls.filter((tc) => tc.action_id != null);
            if (filtered.length === current.activeToolCalls.length) return state;
            const next = new Map(state.inFlightByAgent);
            next.set(projectKey, { ...current, activeToolCalls: filtered });
            return { inFlightByAgent: next };
          });
          chatStore().appendInFlightDelta(projectKey, data.text as string);
        }
      },

      // ── Streaming-bubble parity with the agent channel (useSSE.ts) ──

      tool_call_started(e) {
        const data = parsePayloadData(e.data);
        if (data?.tool_name) {
          chatStore().addInFlightToolCall(projectKey, {
            tool: data.tool_name as string,
            input: data.tool_input as Record<string, unknown> | undefined,
            label: data.label as string | undefined,
          });
        }
      },

      tool_call_completed() {
        chatStore().markInFlightToolCallDone(projectKey);
      },

      tool_use_started(e) {
        const data = parsePayloadData(e.data);
        if (data?.tool_use_id && data?.tool_name) {
          chatStore().addInFlightToolUse(
            projectKey,
            data.tool_use_id as string,
            data.tool_name as string,
            data.input as Record<string, unknown> | undefined,
          );
        }
      },

      tool_use_completed(e) {
        const data = parsePayloadData(e.data);
        if (data?.tool_use_id) {
          chatStore().removeInFlightAgentAction(projectKey, data.tool_use_id as string);
        }
      },

      agent_action_started(e) {
        const data = parsePayloadData(e.data);
        const actionId = data?.action_id as string | undefined;
        const summary = data?.summary as string | undefined;
        if (actionId && summary) {
          chatStore().addInFlightAgentAction(projectKey, actionId, summary);
        }
      },

      agent_action_completed(e) {
        const data = parsePayloadData(e.data);
        const actionId = data?.action_id as string | undefined;
        if (actionId) {
          chatStore().removeInFlightAgentAction(projectKey, actionId);
        }
      },

      thinking_started() {
        // Synchronous commit so the empty "Thinking…" pill paints before
        // batched thinking_delta events fill it (same rationale as useSSE).
        flushSync(() => {
          chatStore().startInFlightThinking(projectKey);
        });
      },

      thinking_delta(e) {
        const data = parsePayloadData(e.data);
        if (typeof data?.text === "string") {
          chatStore().appendInFlightThinkingDelta(projectKey, data.text as string);
        }
      },

      thinking_ended(e) {
        const data = parsePayloadData(e.data);
        const elapsedMs = (data?.elapsed_ms as number | undefined) ?? 0;
        chatStore().endInFlightThinking(projectKey, elapsedMs);
      },

      usage(e) {
        const data = parsePayloadData(e.data);
        if (!data) return;
        chatStore().accumulateUsage(projectKey, {
          input: (data.input_tokens as number) ?? 0,
          output: (data.output_tokens as number) ?? 0,
          cacheRead: (data.cache_read_tokens as number) ?? 0,
          cacheCreation: (data.cache_creation_tokens as number) ?? 0,
          total: (data.total_tokens as number) ?? 0,
        });
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
        chatStore().patchTodoCreateProgress(projectKey, label);
      },

      // Sync form request (AskUserQuestionWithForm): the tool is suspended
      // server-side. Stash the form under the project channel key so both
      // interview and copilot surfaces swap their composer for the form.
      // NOT gated — the form must be stashed regardless of whether the
      // project chat is focused when the event arrives.
      form_request(e) {
        const data = parsePayloadData(e.data) as FormRequestPayload | null;
        if (!data?.form_id) return;
        chatStore().setPendingForm(projectKey, data);
      },

      // Async form posted: stash the form_id so the copilot overlay can render
      // AsyncFormRequestCard. Triggers a background transcript refresh.
      form_posted(e) {
        const data = parsePayloadData(e.data);
        const formId = data?.form_id as string | undefined;
        if (!formId) return;
        chatStore().setPendingAsyncFormId(projectKey, formId);
        store().refreshMessages(id).catch(() => {});
      },

      text_complete(e) {
        const data = parsePayloadData(e.data);
        const text = typeof data?.text === "string" ? (data.text as string) : "";
        store().setTyping(false);
        store().finalizeStreamingMessage(text);
        // Finalize into transcript. Keep entry alive (same skip-teardown logic
        // as the agent channel) so the bubble persists across skill-load handoffs.
        chatStore().finalizeInFlightText(projectKey, text);
      },

      run_ended(e) {
        const data = parsePayloadData(e.data);
        const reason = (data?.reason as string) ?? "unknown";
        store().setTyping(false);
        store().finalizeStreamingMessage("");
        // Pending forms belong to a suspended tool call; if the run is over
        // they are orphaned — restore the composer (mirrors the agent channel).
        chatStore().clearPendingForm(projectKey);
        chatStore().clearPendingAsyncFormId(projectKey);
        chatStore().clearInFlightToolCalls(projectKey);

        // Defense-in-depth: drain any still-buffered text (mirrors useSSE.ts).
        const inFlightEntry = chatStore().inFlightByAgent.get(projectKey);
        if (inFlightEntry && inFlightEntry.textBuffer.length > 0) {
          chatStore().finalizeInFlightText(projectKey, inFlightEntry.textBuffer);
        }

        if (reason === "Cancelled") {
          chatStore().deleteInFlight(projectKey);
        } else {
          chatStore().scheduleInFlightTeardown(projectKey);
        }

        if (reason === "TimedOut" || reason === "NoOutputTimeout") {
          const label =
            reason === "TimedOut" ? "Run timed out" : "No output received — timed out";
          const systemMsg: TranscriptEntry = {
            ts: new Date().toISOString(),
            role: "system",
            content: `⚠️ ${label}. The agent was terminated. You can try sending your message again.`,
            event_type: "system",
          };
          useProjectStore.setState((s) => ({
            messages: [...s.messages, systemMsg],
            allMessages: [...s.allMessages, systemMsg],
          }));
        }
      },

      agent_busy() {
        store().setTyping(true);
        chatStore().ensureInFlight(projectKey);
        chatStore().setInFlightTyping(projectKey, true);
      },

      error(e) {
        const data = parsePayloadData(e.data);
        if (data) console.error("[ProjectSSE] error event:", data);
        store().setTyping(false);
        chatStore().clearInFlightToolCalls(projectKey);
        chatStore().setInFlightTyping(projectKey, false);
      },

      // Project status/name transition (e.g. interviewing → active once the
      // interview yields a tasklist). Patch the store so the detail view swaps
      // the interview chat for the workspace live, without a manual re-navigate.
      "project.state_changed"(e) {
        const data = parsePayloadData(e.data);
        if (!data) return;
        const pid = (data.project_id as string | undefined) ?? id;
        store().applyProjectStateChange(
          pid,
          data.status as string | undefined,
          data.name as string | undefined,
        );
      },

      // Tasklist lifecycle: re-fetch project tasklists so the workspace stays live.
      "tasklist.created": scheduleTasklistRefetch,
      "tasklist.task_updated": scheduleTasklistRefetch,
      "tasklist.task_added": scheduleTasklistRefetch,
      "tasklist.completed": scheduleTasklistRefetch,
      "tasklist.failed": scheduleTasklistRefetch,
      "tasklist.status_changed": scheduleTasklistRefetch,

      // System pills mirroring agent chat (useSSE.ts) — emitted on the project
      // channel by the backend so they surface in project chat too.
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
        const systemMsg: TranscriptEntry = {
          ts: new Date().toISOString(),
          role: "system",
          content: `Todo list ${verb} · ${detail.join(", ")}`,
          event_type: "todo_list_complete",
        };
        useProjectStore.setState((s) => ({
          messages: [...s.messages, systemMsg],
          allMessages: [...s.allMessages, systemMsg],
        }));
      },

      "delegate.complete"(e) {
        const data = parsePayloadData(e.data);
        if (!data?.delegate_name) return;
        const name = data.delegate_name as string;
        const status = typeof data.status === "string" ? data.status : "completed";
        const durationMs =
          typeof data.duration_ms === "number" ? data.duration_ms : null;
        const verb =
          status === "failed" ? "failed" : status === "cancelled" ? "cancelled" : "completed";
        const durationSuffix =
          durationMs !== null ? ` · ${(durationMs / 1000).toFixed(1)}s` : "";
        const systemMsg: TranscriptEntry = {
          ts: new Date().toISOString(),
          role: "system",
          content: `Delegate '${name}' ${verb}${durationSuffix}`,
          event_type: "delegate_complete",
          metadata: { status, delegate_name: name },
        };
        useProjectStore.setState((s) => ({
          messages: [...s.messages, systemMsg],
          allMessages: [...s.allMessages, systemMsg],
        }));
      },

      // memory_saved is emitted only on the agent channel; the project hook
      // mirrors it by listening on any memory_saved event forwarded here.
      memory_saved(e) {
        const data = parsePayloadData(e.data);
        if (!data?.content) return;
        const raw = data.content as string;
        const truncated = raw.length > 80 ? raw.slice(0, 80) + "…" : raw;
        const scope = data.scope === "Global" ? "global" : "agent";
        const systemMsg: TranscriptEntry = {
          ts: new Date().toISOString(),
          role: "system",
          content: `Memory saved (${scope}): ${truncated}`,
          event_type: "memory_saved",
        };
        useProjectStore.setState((s) => ({
          messages: [...s.messages, systemMsg],
          allMessages: [...s.allMessages, systemMsg],
        }));
      },
    };

    subscriptionRef.current = subscribeChannel(channel.project(id), {
      listeners,
      onOpen: () => setConnected(true),
      onReconnect: () => setConnected(true),
    });

    return () => {
      if (tasklistRefetchTimer) clearTimeout(tasklistRefetchTimer);
      if (subscriptionRef.current) {
        subscriptionRef.current.close();
        subscriptionRef.current = null;
      }
    };
  }, [projectId]);

  return { connected };
}
