// @vitest-environment jsdom
import { describe, it, expect, afterEach } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import type { AssignmentWatchHealth } from "../../../types/api";
import { AssignmentLivenessBadge } from "../AssignmentLivenessBadge";

// A neutral, fully-populated AgentWatch-shaped health object — tests below
// override only the generic liveness fields under test
// (`last_evaluated_at`/`fire_count`/`quiescence_reason`/`quiescence_explanation`),
// keeping the AgentWatch-specific fields at their "never polled" defaults so
// the AgentWatch assertions exercise `AssignmentWatchHealthBadge`'s own
// unmodified "not yet evaluated" presentation alongside the new fields.
function makeHealth(overrides: Partial<AssignmentWatchHealth> = {}): AssignmentWatchHealth {
  return {
    has_evaluated: false,
    tier: null,
    extraction_path: null,
    degraded_reason: null,
    extraction_plan_degraded: false,
    extraction_plan_degraded_reason: null,
    extraction_health: "pending",
    extraction_health_reason: null,
    model_calls_today: 0,
    last_extraction_path: null,
    identity_probe_inconclusive: false,
    identity_probe_inconclusive_reason: null,
    extraction_provenance: null,
    contract_status: { state: "not_yet_attempted" },
    last_evaluated_at: null,
    fire_count: 0,
    quiescence_reason: null,
    quiescence_explanation: null,
    ...overrides,
  };
}

describe("AssignmentLivenessBadge", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function render(health: AssignmentWatchHealth | null | undefined, triggerType: "Cron" | "ConnectorEvent" | "AgentWatch") {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root.render(React.createElement(AssignmentLivenessBadge, { health, triggerType }));
    });
  }

  function livenessState(): string | null {
    return container.querySelector("[data-testid='assignment-liveness-badge']")?.getAttribute("data-liveness-state") ?? null;
  }

  function label(): string | null {
    return container.querySelector("[data-testid='assignment-liveness-label']")?.textContent ?? null;
  }

  function explanation(): string | null {
    return container.querySelector("[data-testid='assignment-liveness-explanation']")?.textContent ?? null;
  }

  function lastChecked(): string | null {
    return container.querySelector("[data-testid='assignment-liveness-last-checked']")?.textContent ?? null;
  }

  describe("Cron / ConnectorEvent rows (generic badge)", () => {
    it("never evaluated: renders the neutral never-evaluated state with no explanation line", async () => {
      await render(makeHealth({ last_evaluated_at: null, quiescence_reason: null, quiescence_explanation: null }), "Cron");

      expect(livenessState()).toBe("never_evaluated");
      expect(label()).toBe("Never evaluated");
      expect(explanation()).toBeNull();
      expect(lastChecked()).toContain("never");
    });

    it("evaluated, not fired: renders the quiescent state with the server-rendered explanation", async () => {
      await render(
        makeHealth({
          last_evaluated_at: "2026-08-05T14:00:00Z",
          quiescence_reason: { kind: "server_not_connected", server: "notion", state: null },
          quiescence_explanation: "Not fired: the notion MCP server has not been connected, as of the last check at 2026-08-05 14:00 UTC.",
        }),
        "ConnectorEvent",
      );

      expect(livenessState()).toBe("quiescent");
      expect(label()).toBe("Server not connected");
      expect(explanation()).toBe(
        "Not fired: the notion MCP server has not been connected, as of the last check at 2026-08-05 14:00 UTC.",
      );
    });

    it("fired recently: renders the fired state with no explanation line and a distinct label/tone from both other states", async () => {
      await render(
        makeHealth({ last_evaluated_at: "2026-08-05T14:00:00Z", fire_count: 3, quiescence_reason: null, quiescence_explanation: null }),
        "Cron",
      );

      expect(livenessState()).toBe("fired");
      expect(label()).toBe("Fired");
      expect(explanation()).toBeNull();
    });

    it("never-evaluated and evaluated-not-fired render with different data-liveness-state and different tone classes — never conflated", async () => {
      await render(makeHealth({ last_evaluated_at: null }), "Cron");
      const neverEvaluatedState = livenessState();
      const neverEvaluatedClass = container.querySelector("[data-testid='assignment-liveness-badge']")?.className ?? "";

      await render(
        makeHealth({
          last_evaluated_at: "2026-08-05T14:00:00Z",
          quiescence_reason: { kind: "not_due", next_fire_at: "2026-08-06T09:00:00Z" },
          quiescence_explanation: "Not fired: not due to run again until 2026-08-06 09:00 UTC.",
        }),
        "Cron",
      );
      const notDueState = livenessState();

      expect(neverEvaluatedState).toBe("never_evaluated");
      expect(notDueState).toBe("quiescent");
      expect(neverEvaluatedState).not.toBe(notDueState);
      expect(neverEvaluatedClass).toBeTruthy();
    });

    it("a genuinely blocked reason (not `not_due`) renders in the error tone, distinct from the neutral 'waiting' tone", async () => {
      await render(
        makeHealth({
          last_evaluated_at: "2026-08-05T14:00:00Z",
          quiescence_reason: { kind: "poll_failed", server: "linear", reason: "timed out" },
          quiescence_explanation: "Not fired: the last poll of the linear MCP server failed — timed out.",
        }),
        "ConnectorEvent",
      );
      const errorClass = container.querySelector("[data-testid='assignment-liveness-badge']")?.className ?? "";

      await render(
        makeHealth({
          last_evaluated_at: "2026-08-05T14:00:00Z",
          quiescence_reason: { kind: "not_due", next_fire_at: null },
          quiescence_explanation: "Not fired: no next run time has been scheduled yet.",
        }),
        "Cron",
      );
      const waitingClass = container.querySelector("[data-testid='assignment-liveness-badge']")?.className ?? "";

      expect(errorClass).toContain("--error");
      expect(waitingClass).not.toContain("--error");
    });

    it("treats a missing/undefined health prop the same as never-evaluated, not a crash", async () => {
      await render(undefined, "Cron");
      expect(livenessState()).toBe("never_evaluated");
    });
  });

  describe("AgentWatch rows", () => {
    it("renders the existing AssignmentWatchHealthBadge unchanged, plus a last-checked line", async () => {
      await render(makeHealth({ last_evaluated_at: null }), "AgentWatch");

      // The pre-existing badge's own testids must still be present and
      // unaffected — "keep the existing AgentWatch badge behaviour intact".
      expect(container.querySelector("[data-testid='watch-health-badge']")).toBeTruthy();
      expect(container.querySelector("[data-testid='watch-health-label']")?.textContent).toBe("Not yet evaluated");
      // The new generic testid ("assignment-liveness-badge") is NOT used for
      // AgentWatch rows — only the last-checked line is appended alongside
      // the untouched badge.
      expect(container.querySelector("[data-testid='assignment-liveness-badge']")).toBeNull();
      expect(lastChecked()).toContain("never");
    });

    it("shows a relative last-checked time once the watch has been evaluated, alongside the unchanged tier badge", async () => {
      await render(
        makeHealth({
          has_evaluated: true,
          tier: "deterministic",
          last_evaluated_at: new Date(Date.now() - 5 * 60 * 1000).toISOString(),
          contract_status: { state: "bound", bound_after_repairs: null },
        }),
        "AgentWatch",
      );

      expect(container.querySelector("[data-testid='watch-health-label']")?.textContent).toBe("Deterministic");
      expect(lastChecked()).toContain("5m ago");
    });
  });
});
