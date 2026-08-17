import { useEffect, useRef, useState } from "react";
import { channel, subscribeChannel, type HubSubscription } from "../lib/sseHub";
import { useAgentTasklistStore } from "../stores/agentTasklistStore";
import type { Task, TasklistStatus } from "../types/api";

/**
 * Subscribes to the per-task run event channel for an agent-owned tasklist.
 *
 * Agent-owned tasklist runs emit events on `tasklist:{id}`
 * rather than the parent agent channel, isolating subagent stdout from the
 * parent's main chat. This hook subscribes to that channel on the shared SSE
 * hub (`channel.agentTasklist(tasklistId)`, matching `agent_id ===
 * "tasklist:{id}"` on the single `/system/stream` connection) and routes the
 * incoming events to the agentTasklistStore so the TodoPanel keeps live task
 * status.
 *
 * Tasklist lifecycle events (task_updated, completed, etc.) may arrive on
 * both the tasklist channel and the parent agent channel; handlers here
 * accept them without the owner-field filter since we are already scoped to
 * the correct tasklist.
 *
 * Disconnects and stops reconnecting when either agentId or tasklistId
 * becomes null (e.g., the tasklist completes and the panel hides).
 */
export function useAgentTasklistRunSSE(
  agentId: string | null,
  tasklistId: string | null,
): { connected: boolean } {
  const [connected, setConnected] = useState(false);
  const subscriptionRef = useRef<HubSubscription | null>(null);

  useEffect(() => {
    cleanup();

    if (!agentId || !tasklistId) {
      setConnected(false);
      return;
    }

    connect(agentId, tasklistId);

    return cleanup;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentId, tasklistId]);

  function connect(aid: string, tid: string) {
    const agentTlStore = useAgentTasklistStore.getState;

    // Runs on the hub's first open AND on every subsequent reconnect of the
    // shared connection — mirrors the old per-tasklist EventSource's
    // `onopen`, which fired on initial connect and every manual
    // post-`onerror` reconnect alike.
    function handleStreamOpen() {
      setConnected(true);
    }

    const listeners: Record<string, (e: MessageEvent) => void> = {
      "tasklist.task_updated"(e) {
        const data = parseData(e.data);
        const tlId = data?.tasklist_id;
        const task = data?.task;
        if (
          typeof tlId === "string" &&
          task &&
          typeof task === "object" &&
          typeof (task as Task).id === "string"
        ) {
          void agentTlStore().applyTaskUpdated(aid, tlId, task as Task);
        }
      },

      "tasklist.task_added"(e) {
        const data = parseData(e.data);
        const tlId = data?.tasklist_id;
        const taskId = data?.task_id;
        if (typeof tlId === "string" && typeof taskId === "string") {
          void agentTlStore().applyTaskAdded(aid, tlId, taskId);
        }
      },

      "tasklist.completed"(e) {
        const data = parseData(e.data);
        const tlId = data?.tasklist_id;
        if (typeof tlId === "string") {
          agentTlStore().applyTasklistCompleted(aid, tlId);
        }
      },

      "tasklist.failed"(e) {
        const data = parseData(e.data);
        const tlId = data?.tasklist_id;
        const reason = typeof data?.reason === "string" ? data.reason : null;
        if (typeof tlId === "string") {
          agentTlStore().applyTasklistFailed(aid, tlId, reason);
        }
      },

      "tasklist.status_changed"(e) {
        const data = parseData(e.data);
        const tlId = data?.tasklist_id;
        const status = data?.status;
        if (typeof tlId === "string" && typeof status === "string") {
          agentTlStore().applyTasklistStatusChanged(aid, tlId, status as TasklistStatus);
        }
      },
    };

    subscriptionRef.current = subscribeChannel(channel.agentTasklist(tid), {
      listeners,
      onOpen: handleStreamOpen,
      onReconnect: handleStreamOpen,
    });
  }

  function cleanup() {
    if (subscriptionRef.current) {
      subscriptionRef.current.close();
      subscriptionRef.current = null;
    }
  }

  return { connected };
}

function parseData(raw: string): Record<string, unknown> | null {
  try {
    const event = JSON.parse(raw) as { payload?: { data?: unknown } };
    return (event?.payload?.data as Record<string, unknown>) ?? null;
  } catch {
    return null;
  }
}
