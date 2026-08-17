import { useEffect, useRef } from "react";
import { parsePayloadData } from "./sseUtils";
import { channel, subscribeChannel, type HubSubscription } from "../lib/sseHub";
import { useTasklistStore } from "../stores/tasklistStore";
import type { Task, TasklistStatus } from "../types/api";
import type { TasklistScope } from "../types/api";

/**
 * Subscribes to the SSE channel for the given scope (team or project) and
 * routes tasklist events into `useTasklistStore`. Hydrates on scope change.
 */
export function useTasklistSSE(scope: TasklistScope | null): void {
  const subscriptionRef = useRef<HubSubscription | null>(null);

  useEffect(() => {
    if (subscriptionRef.current) {
      subscriptionRef.current.close();
      subscriptionRef.current = null;
    }

    if (!scope) return;

    const s = scope;
    const store = useTasklistStore.getState;

    void store().hydrate(s);

    const handleCreated = (e: MessageEvent) => {
      const data = parsePayloadData(e.data);
      const tasklistId = data?.tasklist_id;
      if (typeof tasklistId === "string") {
        void store().applyTasklistCreated(s, tasklistId);
      }
    };

    const handleTaskUpdated = (e: MessageEvent) => {
      const data = parsePayloadData(e.data);
      const tasklistId = data?.tasklist_id;
      const task = data?.task;
      if (
        typeof tasklistId === "string" &&
        task &&
        typeof task === "object" &&
        typeof (task as Task).id === "string"
      ) {
        void store().applyTaskUpdated(s, tasklistId, task as Task);
      }
    };

    const handleTaskAdded = (e: MessageEvent) => {
      const data = parsePayloadData(e.data);
      const tasklistId = data?.tasklist_id;
      const taskId = data?.task_id;
      if (typeof tasklistId === "string" && typeof taskId === "string") {
        void store().applyTaskAdded(s, tasklistId, taskId);
      }
    };

    const handleCompleted = (e: MessageEvent) => {
      const data = parsePayloadData(e.data);
      const tasklistId = data?.tasklist_id;
      if (typeof tasklistId === "string") {
        store().applyTasklistCompleted(s, tasklistId);
      }
    };

    const handleFailed = (e: MessageEvent) => {
      const data = parsePayloadData(e.data);
      const tasklistId = data?.tasklist_id;
      const reason = typeof data?.reason === "string" ? data.reason : null;
      if (typeof tasklistId === "string") {
        store().applyTasklistFailed(s, tasklistId, reason);
      }
    };

    const handleStatusChanged = (e: MessageEvent) => {
      const data = parsePayloadData(e.data);
      const tasklistId = data?.tasklist_id;
      const status = data?.status;
      if (typeof tasklistId === "string" && typeof status === "string") {
        store().applyTasklistStatusChanged(s, tasklistId, status as TasklistStatus);
      }
    };

    const matcher = s.kind === "project" ? channel.project(s.id) : channel.team(s.id);

    subscriptionRef.current = subscribeChannel(matcher, {
      listeners: {
        "tasklist.created": handleCreated,
        "tasklist.task_updated": handleTaskUpdated,
        "tasklist.task_added": handleTaskAdded,
        "tasklist.completed": handleCompleted,
        "tasklist.failed": handleFailed,
        "tasklist.status_changed": handleStatusChanged,
      },
    });

    return () => {
      if (subscriptionRef.current) {
        subscriptionRef.current.close();
        subscriptionRef.current = null;
      }
    };
  }, [scope?.kind, scope?.id]); // eslint-disable-line react-hooks/exhaustive-deps
}
