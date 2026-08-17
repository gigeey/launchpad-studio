// @vitest-environment jsdom
//
// Coverage for `useArtifactRegen`'s completion paths: the artifact record
// changing before the polling deadline (-> "done", including a late change
// past the old ~90s bound), and it never changing all the way out to the
// ~10min deadline (-> soft "still working" message, status stays "working",
// never "error"). All drive the hook through a real mounted component (not
// a hand-rolled reimplementation) so the actual setTimeout-based poll loop
// and generation-guarded state writes run.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { useArtifactRegen, type UseArtifactRegenResult } from "../useArtifactRegen";
import type { ArtifactWithPayload } from "../../../types/api";

const getArtifactMock = vi.fn();
const regenerateArtifactMock = vi.fn();

vi.mock("../../../lib/api", () => ({
  getArtifact: (...args: unknown[]) => getArtifactMock(...args),
  regenerateArtifact: (...args: unknown[]) => regenerateArtifactMock(...args),
}));

const AGENT_ID = "agent-1";
const ARTIFACT_ID = "artifact-1";
const POLL_INTERVAL_MS = 1500;
const TIMEOUT_MS = 600_000;
const OLD_TIMEOUT_MS = 90_000;

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
  latest = null;
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  document.body.removeChild(container);
  vi.useRealTimers();
});

describe("useArtifactRegen", () => {
  it("starts idle and posts the regenerate request on start()", async () => {
    getArtifactMock.mockResolvedValue(makeArtifact());
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });
    mountHook(AGENT_ID, ARTIFACT_ID);

    expect(latest!.status).toBe("idle");

    await act(async () => {
      await latest!.start();
    });

    expect(regenerateArtifactMock).toHaveBeenCalledWith(AGENT_ID, ARTIFACT_ID);
    expect(latest!.status).toBe("working");
  });

  it("polls getArtifact until updated_at advances, then flips to done", async () => {
    vi.useFakeTimers();
    const baseline = makeArtifact({ updated_at: "t0", checksum_sha256: "c0" });
    const changed = makeArtifact({ updated_at: "t1", checksum_sha256: "c1" });
    getArtifactMock
      .mockResolvedValueOnce(baseline) // snapshot taken at start()
      .mockResolvedValueOnce(baseline) // 1st poll — unchanged
      .mockResolvedValueOnce(changed); // 2nd poll — regenerate landed
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });

    mountHook(AGENT_ID, ARTIFACT_ID);

    await act(async () => {
      await latest!.start();
    });
    expect(latest!.status).toBe("working");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("working");
    expect(getArtifactMock).toHaveBeenCalledTimes(2);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("done");
    expect(latest!.error).toBeNull();
    expect(getArtifactMock).toHaveBeenCalledTimes(3);
  });

  it("resolves to done on a completion that lands after the old ~90s bound", async () => {
    vi.useFakeTimers();
    const baseline = makeArtifact({ updated_at: "t0", checksum_sha256: "c0" });
    const changed = makeArtifact({ updated_at: "t1", checksum_sha256: "c1" });
    getArtifactMock.mockResolvedValueOnce(baseline); // snapshot taken at start()
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });

    mountHook(AGENT_ID, ARTIFACT_ID);

    await act(async () => {
      await latest!.start();
    });

    // Sit unchanged well past the old 90s bound (but short of the new 10min
    // deadline) — must not error, since the only bound left is the new one.
    getArtifactMock.mockResolvedValue(baseline);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(OLD_TIMEOUT_MS + POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("working");
    expect(latest!.error).toBeNull();

    // The regen finally lands.
    getArtifactMock.mockResolvedValue(changed);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });

    expect(latest!.status).toBe("done");
    expect(latest!.error).toBeNull();
    expect(latest!.message).toBeNull();
  });

  it("goes soft — not error — once the full ~10min deadline is reached with no change", async () => {
    vi.useFakeTimers();
    getArtifactMock.mockResolvedValue(makeArtifact({ updated_at: "t0", checksum_sha256: "c0" }));
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });

    mountHook(AGENT_ID, ARTIFACT_ID);

    await act(async () => {
      await latest!.start();
    });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(TIMEOUT_MS + POLL_INTERVAL_MS);
    });

    expect(latest!.status).toBe("working");
    expect(latest!.error).toBeNull();
    expect(latest!.message).toBe("Still working — this is taking longer than usual…");
  });

  it("surfaces a rejected regenerate call as an immediate error, without polling", async () => {
    getArtifactMock.mockResolvedValue(makeArtifact());
    regenerateArtifactMock.mockRejectedValue(new Error("API 409: not refreshable"));

    mountHook(AGENT_ID, ARTIFACT_ID);

    await act(async () => {
      await latest!.start();
    });

    expect(latest!.status).toBe("error");
    expect(latest!.error).toBe("API 409: not refreshable");
  });

  it("resume() seeds working + the given taskId and polls the same loop start() uses, through to done", async () => {
    // Simulates the spinner-resume-on-mount fix: a task was already running
    // before this hook instance existed (kicked off by a prior mount that
    // has since unmounted), so there's no `start()` call — the caller
    // (`ArtifactPreview`) hands `resume()` the task id plus the
    // updated_at/checksum baseline straight from its own mount-time
    // `getArtifact` fetch, with no extra snapshot request from the hook.
    vi.useFakeTimers();
    const baseline = makeArtifact({ updated_at: "t0", checksum_sha256: "c0" });
    const changed = makeArtifact({ updated_at: "t1", checksum_sha256: "c1" });
    getArtifactMock
      .mockResolvedValueOnce(baseline) // 1st poll — unchanged
      .mockResolvedValueOnce(changed); // 2nd poll — landed

    mountHook(AGENT_ID, ARTIFACT_ID);
    expect(latest!.status).toBe("idle");

    act(() => {
      latest!.resume("bg-resumed", "t0", "c0");
    });

    // Working immediately, synchronously — no snapshot fetch to await first
    // (that's the whole point: the caller already has the baseline).
    expect(latest!.status).toBe("working");
    expect(regenerateArtifactMock).not.toHaveBeenCalled();
    expect(getArtifactMock).not.toHaveBeenCalled();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("working");
    expect(getArtifactMock).toHaveBeenCalledTimes(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("done");
    expect(latest!.error).toBeNull();
    expect(getArtifactMock).toHaveBeenCalledTimes(2);
  });

  it("resume() is a no-op when a run is already working, so it can't stomp an in-flight generation", async () => {
    vi.useFakeTimers();
    getArtifactMock.mockResolvedValue(makeArtifact({ updated_at: "t0", checksum_sha256: "c0" }));
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });

    mountHook(AGENT_ID, ARTIFACT_ID);
    await act(async () => {
      await latest!.start();
    });
    expect(latest!.status).toBe("working");
    getArtifactMock.mockClear();

    act(() => {
      latest!.resume("bg-resumed", "t0", "c0");
    });

    // Still working off the original start() run — resume() must not reset
    // the generation or start a second poll loop on top of it.
    expect(latest!.status).toBe("working");
    expect(getArtifactMock).not.toHaveBeenCalled();
  });

  it("resets to idle when the artifact identity changes", async () => {
    vi.useFakeTimers();
    getArtifactMock.mockResolvedValue(makeArtifact({ updated_at: "t0", checksum_sha256: "c0" }));
    regenerateArtifactMock.mockResolvedValue({ task_id: "bg-1" });

    mountHook(AGENT_ID, ARTIFACT_ID);
    await act(async () => {
      await latest!.start();
    });
    expect(latest!.status).toBe("working");

    mountHook(AGENT_ID, "artifact-2");
    expect(latest!.status).toBe("idle");

    // The stale poll loop from the superseded artifact must not resurrect
    // "working"/"error" state after the identity switch.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(TIMEOUT_MS + POLL_INTERVAL_MS);
    });
    expect(latest!.status).toBe("idle");
  });
});
