// @vitest-environment jsdom
//
// Coverage for `useArtifactRegen`'s task-status fail-fast path
// (`GET .../task/{task_id}/status`, layered on top of the existing
// updated_at/checksum poll from `useArtifactRegen.test.ts`): a `"failed"`
// status must surface as a hook error within a single poll tick instead of
// waiting out the ~10min soft timeout, while a completion that lands via the
// updated_at diff must still resolve to `"done"` even when task status
// hasn't caught up yet — the updated_at/checksum diff remains the sole
// authority for the `"done"` transition.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { useArtifactRegen, type UseArtifactRegenResult } from "../useArtifactRegen";
import type { ArtifactWithPayload } from "../../../types/api";

const getArtifactMock = vi.fn();
const regenerateArtifactMock = vi.fn();
const getArtifactTaskStatusMock = vi.fn();

vi.mock("../../../lib/api", () => ({
  getArtifact: (...args: unknown[]) => getArtifactMock(...args),
  regenerateArtifact: (...args: unknown[]) => regenerateArtifactMock(...args),
  getArtifactTaskStatus: (...args: unknown[]) => getArtifactTaskStatusMock(...args),
}));

const AGENT_ID = "agent-1";
const ARTIFACT_ID = "artifact-1";
const POLL_INTERVAL_MS = 1500;
const TIMEOUT_MS = 600_000;

function makeArtifact(overrides: Partial<ArtifactWithPayload> = {}): ArtifactWithPayload {
  return {
    id: ARTIFACT_ID,
    title: "Weekly metrics",
    kind: "metric",
    format: "json",
    stored_filename: "blob.json",
    size_bytes: 0,
    checksum_sha256: "c0",
    refresh_intent: "whole_artifact",
    origin_intent: { refresh_prompt: "Summarize this week's metrics" },
    capabilities: [],
    source_message_id: null,
    created_at: "2026-07-11T00:00:00Z",
    updated_at: "t0",
    last_refreshed_at: null,
    refresh_count: 0,
    pinned: false,
    pinned_at: null,
    group_id: null,
    payload: { metrics: [] },
    ...overrides,
  };
}

let container: HTMLDivElement;
let root: Root;
let latest: UseArtifactRegenResult | null = null;

function Harness({ agentId, artifactId }: { agentId: string | null; artifactId: string | null }) {
  latest = useArtifactRegen(agentId, artifactId);
  return null;
}

function mountHook(agentId: string | null, artifactId: string | null) {
  act(() => {
    root.render(React.createElement(Harness, { agentId, artifactId }));
  });
}

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  getArtifactMock.mockReset();
  regenerateArtifactMock.mockReset();
  getArtifactTaskStatusMock.mockReset();
  latest = null;
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  document.body.removeChild(container);
  vi.useRealTimers();
});

describe("useArtifactRegen task status", () => {
  it("surfaces a failed task status as an error within one poll tick, not the 10min timeout", async () => {
    vi.useFakeTimers();
    const baseline = makeArtifact({ updated_at: "t0", checksum_sha256: "c0" });
    getArtifactMock.mockResolvedValue(baseline); // never changes
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });
    getArtifactTaskStatusMock.mockResolvedValue({ status: "failed", error: "the subagent crashed" });

    mountHook(AGENT_ID, ARTIFACT_ID);

    await act(async () => {
      await latest!.start();
    });
    expect(latest!.status).toBe("working");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });

    expect(latest!.status).toBe("error");
    expect(latest!.error).toBe("the subagent crashed");
    expect(getArtifactTaskStatusMock).toHaveBeenCalledWith(AGENT_ID, ARTIFACT_ID, "bg-1");

    // Must not still be polling — advancing well past the old 10min bound
    // shouldn't flip anything else.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(TIMEOUT_MS);
    });
    expect(latest!.status).toBe("error");
  });

  it("falls back to a default message when the failed status has no error string", async () => {
    vi.useFakeTimers();
    getArtifactMock.mockResolvedValue(makeArtifact({ updated_at: "t0", checksum_sha256: "c0" }));
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });
    getArtifactTaskStatusMock.mockResolvedValue({ status: "failed", error: null });

    mountHook(AGENT_ID, ARTIFACT_ID);
    await act(async () => {
      await latest!.start();
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });

    expect(latest!.status).toBe("error");
    expect(latest!.error).toBeTruthy();
  });

  it("treats 'running' and 'unknown' task status exactly like an ordinary in-flight poll", async () => {
    vi.useFakeTimers();
    const baseline = makeArtifact({ updated_at: "t0", checksum_sha256: "c0" });
    const changed = makeArtifact({ updated_at: "t1", checksum_sha256: "c1" });
    getArtifactMock
      .mockResolvedValueOnce(baseline) // snapshot at start()
      .mockResolvedValueOnce(baseline) // poll 1 — unchanged
      .mockResolvedValueOnce(baseline) // poll 2 — unchanged
      .mockResolvedValueOnce(changed); // poll 3 — regenerate landed
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });
    getArtifactTaskStatusMock
      .mockResolvedValueOnce({ status: "unknown", error: null })
      .mockResolvedValueOnce({ status: "running", error: null })
      .mockResolvedValueOnce({ status: "running", error: null });

    mountHook(AGENT_ID, ARTIFACT_ID);
    await act(async () => {
      await latest!.start();
    });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("working");
    expect(latest!.error).toBeNull();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("working");
    expect(latest!.error).toBeNull();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("done");
    expect(latest!.error).toBeNull();
  });

  it("still resolves to done via the updated_at diff even when task status hasn't caught up", async () => {
    vi.useFakeTimers();
    const baseline = makeArtifact({ updated_at: "t0", checksum_sha256: "c0" });
    const changed = makeArtifact({ updated_at: "t1", checksum_sha256: "c1" });
    getArtifactMock
      .mockResolvedValueOnce(baseline) // snapshot at start()
      .mockResolvedValueOnce(changed); // poll 1 — already landed
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });
    // Task status still says "running" even though the artifact write beat it.
    getArtifactTaskStatusMock.mockResolvedValue({ status: "running", error: null });

    mountHook(AGENT_ID, ARTIFACT_ID);
    await act(async () => {
      await latest!.start();
    });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });

    expect(latest!.status).toBe("done");
    expect(latest!.error).toBeNull();
  });

  it("swallows a getArtifactTaskStatus rejection and keeps polling via the updated_at path", async () => {
    vi.useFakeTimers();
    const baseline = makeArtifact({ updated_at: "t0", checksum_sha256: "c0" });
    const changed = makeArtifact({ updated_at: "t1", checksum_sha256: "c1" });
    getArtifactMock
      .mockResolvedValueOnce(baseline) // snapshot at start()
      .mockResolvedValueOnce(baseline) // poll 1 — unchanged
      .mockResolvedValueOnce(changed); // poll 2 — regenerate landed
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });
    getArtifactTaskStatusMock.mockRejectedValue(new Error("network hiccup"));

    mountHook(AGENT_ID, ARTIFACT_ID);
    await act(async () => {
      await latest!.start();
    });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("working");
    expect(latest!.error).toBeNull();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("done");
    expect(latest!.error).toBeNull();
  });
});
