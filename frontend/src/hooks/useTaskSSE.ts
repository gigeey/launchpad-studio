import { useEffect, useRef, useState } from "react";
import { channel, subscribeChannel, type HubSubscription } from "../lib/sseHub";
import { usePhaseChatStore } from "../stores/phaseChatStore";
import { useWorkflowStore } from "../stores/workflowStore";

/**
 * Manages a task's real-time event subscription (all phases) via the shared
 * SSE hub. Dispatches chat events to phaseChatStore and workflow events to
 * workflowStore.
 */
export function useTaskSSE(taskId: string | null): { connected: boolean } {
  const [connected, setConnected] = useState(false);
  const subscriptionRef = useRef<HubSubscription | null>(null);
  const receivedContentRef = useRef(false);

  useEffect(() => {
    cleanup();

    if (!taskId) {
      setConnected(false);
      return;
    }

    connect(taskId);

    return () => {
      cleanup();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [taskId]);

  function connect(id: string) {
    const chatStore = usePhaseChatStore.getState;
    const wfStore = useWorkflowStore.getState;

    // Helper: check if an SSE event belongs to the currently viewed phase
    function isCurrentPhase(rawData: string): boolean {
      try {
        const event = JSON.parse(rawData);
        const agentId = event?.agent_id as string | undefined;
        if (!agentId) return false;
        const { phaseId } = chatStore();
        return agentId === `task:${id}:phase:${phaseId}`;
      } catch {
        return false;
      }
    }

    const listeners: Record<string, (e: MessageEvent) => void> = {
      run_started(e) {
        if (!isCurrentPhase((e as MessageEvent).data)) return;
        receivedContentRef.current = false;
        chatStore().setTyping(true);
      },

      tool_call_started(e) {
        if (!isCurrentPhase((e as MessageEvent).data)) return;
        const data = parsePayloadData((e as MessageEvent).data);
        if (data?.tool_name) {
          chatStore().addActiveToolCall({
            tool: data.tool_name as string,
            input: data.tool_input as Record<string, unknown> | undefined,
          });
        }
      },

      tool_call_completed(e) {
        if (!isCurrentPhase((e as MessageEvent).data)) return;
        chatStore().removeActiveToolCall();
      },

      text_delta(e) {
        if (!isCurrentPhase(e.data)) return;
        const data = parsePayloadData(e.data);
        if (data?.text != null) {
          receivedContentRef.current = true;
          chatStore().appendStreamingDelta(data.text as string);
        }
      },

      text_complete(e) {
        if (!isCurrentPhase(e.data)) return;
        const data = parsePayloadData(e.data);
        if (data?.text != null) {
          chatStore().finalizeStreamingMessage(data.text as string);
        }
        chatStore().setTyping(false);
      },

      run_ended(e) {
        if (!isCurrentPhase((e as MessageEvent).data)) return;
        chatStore().setTyping(false);
      },

      agent_busy(e) {
        if (!isCurrentPhase((e as MessageEvent).data)) return;
        chatStore().setTyping(true);
      },

      // Workflow phase events
      phase_started(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handlePhaseStarted(data as never);
        }
      },

      phase_completed(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handlePhaseCompleted(data as never);
        }
      },

      phase_skipped(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handlePhaseSkipped(data as never);
        }
      },

      phase_failed(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handlePhaseFailed(data as never);
        }
      },

      phase_paused(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handlePhasePaused(data as never);
        }
      },

      workflow_completed(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handleWorkflowCompleted(data as never);
        }
      },

      workflow_task_started(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handleTaskStarted(data as never);
        }
      },

      workflow_task_failed(e) {
        const data = parsePayloadData(e.data);
        if (data?.task_id) {
          wfStore().handleTaskFailed(data as never);
        }
      },

      error(e) {
        const data = parsePayloadData((e as MessageEvent).data);
        if (data) {
          console.error("[TaskSSE] error:", data);
        }
        chatStore().setTyping(false);
      },
    };

    subscriptionRef.current = subscribeChannel(channel.task(id), {
      listeners,
      onOpen: () => setConnected(true),
      onReconnect: () => setConnected(true),
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

function parsePayloadData(raw: string): Record<string, unknown> | null {
  try {
    const event = JSON.parse(raw);
    return (event?.payload?.data as Record<string, unknown>) ?? null;
  } catch {
    return null;
  }
}
