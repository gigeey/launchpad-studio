// @vitest-environment jsdom
import { describe, it, expect, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import type { AssignmentWithOwner } from "../../../hooks/useAssignments";
import { AssignmentList } from "../AssignmentList";

const OWNER = { id: "agent-1", name: "Assistant", isTeam: false };

function makeAssignment(overrides: Partial<AssignmentWithOwner> = {}): AssignmentWithOwner {
  return {
    id: "asg-1",
    agent_id: OWNER.id,
    name: "Morning digest",
    instruction: "Summarize overnight events.",
    working_directory: null,
    trigger: { type: "Cron", cron_expr: "0 9 * * *", is_recurring: true },
    bindings: [],
    output_mode: "background",
    thread_policy: "dedicated",
    enabled: true,
    expires_at: null,
    next_fire_at: null,
    created_ts: "2026-06-28T00:00:00Z",
    updated_ts: "2026-06-28T00:00:00Z",
    owner: OWNER,
    ...overrides,
  };
}

describe("AssignmentList", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function render(
    assignments: AssignmentWithOwner[],
    onRunNow?: (...a: unknown[]) => void | Promise<void>,
  ) {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root.render(React.createElement(AssignmentList, { assignments, onSelect: vi.fn(), onRunNow }));
    });
  }

  function statusBadge(): HTMLElement {
    return container.querySelector("[data-testid='assignment-list-row-status']") as HTMLElement;
  }

  function runNowButton(): HTMLButtonElement {
    return container.querySelector("[data-testid='assignment-list-row-run-now']") as HTMLButtonElement;
  }

  it("shows a distinct 'Expired' badge for an assignment past its expires_at", async () => {
    await render([
      makeAssignment({
        enabled: true,
        expires_at: "2020-01-01T00:00:00Z",
      }),
    ]);

    const badge = statusBadge();
    expect(badge).toBeTruthy();
    expect(badge.title).toBe("Expired");
  });

  it("still shows 'Paused' for a manually-disabled assignment with no past expiry", async () => {
    await render([
      makeAssignment({
        enabled: false,
        expires_at: null,
      }),
    ]);

    const badge = statusBadge();
    expect(badge).toBeTruthy();
    expect(badge.title).toBe("Paused");
  });

  it("shows 'Active' for an enabled assignment with a future or no expiry", async () => {
    await render([
      makeAssignment({
        enabled: true,
        expires_at: "2999-01-01T00:00:00Z",
      }),
    ]);

    const badge = statusBadge();
    expect(badge).toBeTruthy();
    expect(badge.title).toBe("Active");
  });

  it("per-tile Run now invokes onRunNow with the assignment's owner and id", async () => {
    const onRunNow = vi.fn().mockResolvedValue(undefined);
    await render([makeAssignment({ enabled: true, expires_at: null })], onRunNow);

    const btn = runNowButton();
    expect(btn).toBeTruthy();
    expect(btn.disabled).toBe(false);

    await act(async () => {
      btn.click();
    });

    expect(onRunNow).toHaveBeenCalledTimes(1);
    expect(onRunNow).toHaveBeenCalledWith(OWNER, "asg-1");
  });

  it("disables per-tile Run now for a disabled assignment (same rule as the modal)", async () => {
    const onRunNow = vi.fn();
    await render([makeAssignment({ enabled: false, expires_at: null })], onRunNow);

    const btn = runNowButton();
    expect(btn).toBeTruthy();
    expect(btn.disabled).toBe(true);

    await act(async () => {
      btn.click();
    });

    expect(onRunNow).not.toHaveBeenCalled();
  });

  it("omits the per-tile Run now action when no onRunNow handler is provided", async () => {
    await render([makeAssignment({ enabled: true })]);
    expect(runNowButton()).toBeNull();
  });
});
