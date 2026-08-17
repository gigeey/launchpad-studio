// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { createRoot } from "react-dom/client";
import { act } from "react";

// Mock Tauri + API dependencies that AddressBookEditor uses
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn().mockResolvedValue(false),
}));
vi.mock("../../../lib/api", () => ({
  getAgents: vi.fn().mockResolvedValue([]),
}));

import { CoordinatorBadge } from "../CoordinatorBadge";
import { AddressBookEditor } from "../AddressBookEditor";
import { coordinatorLevel } from "../../../lib/coordinatorLevel";
import type { DelegateTarget } from "../../../types/api";
import type { AgentProfileLike } from "../../../lib/coordinatorLevel";

// ─── helpers ─────────────────────────────────────────────────────────────────

function makeDelegateTarget(overrides: Partial<DelegateTarget> = {}): DelegateTarget {
  return {
    target_agent_id: "agent-b",
    name: "Reviewer",
    purpose: "Reviews code",
    share_context_allowed: false,
    ...overrides,
  };
}

let container: HTMLDivElement;
let root: ReturnType<typeof createRoot>;

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  document.body.removeChild(container);
});

// ─── 1. CoordinatorBadge: leaf renders "L" ───────────────────────────────────

describe("CoordinatorBadge", () => {
  it("renders 'L' for level 0 (leaf agent)", async () => {
    await act(async () => {
      root.render(<CoordinatorBadge level={0} />);
    });
    const badge = container.querySelector("[data-testid='coordinator-badge']");
    expect(badge).not.toBeNull();
    expect(badge!.textContent).toBe("L");
  });

  it("renders 'C1' for level 1", async () => {
    await act(async () => {
      root.render(<CoordinatorBadge level={1} />);
    });
    const badge = container.querySelector("[data-testid='coordinator-badge']");
    expect(badge!.textContent).toBe("C1");
  });

  it("renders 'C2' for level 2", async () => {
    await act(async () => {
      root.render(<CoordinatorBadge level={2} />);
    });
    const badge = container.querySelector("[data-testid='coordinator-badge']");
    expect(badge!.textContent).toBe("C2");
  });

  it("tooltip text contains correct level description", () => {
    // Render badge with level 1 — the tooltip text is determined at render time.
    // We verify the tooltip aria-label / content at the component level rather
    // than testing the hover-show mechanic (which requires real pointer events).
    // The badge's aria-label already encodes the level string for accessibility.
    act(() => {
      root.render(<CoordinatorBadge level={1} />);
    });
    const badge = container.querySelector("[data-testid='coordinator-badge']");
    expect(badge).not.toBeNull();
    // aria-label encodes "Coordinator badge: C1"
    expect(badge!.getAttribute("aria-label")).toContain("C1");
  });
});

// ─── 2. coordinatorLevel utility ─────────────────────────────────────────────

describe("coordinatorLevel", () => {
  it("returns 0 for a leaf agent (no delegates_to)", () => {
    const profile: AgentProfileLike = { id: "a" };
    const idx = new Map([["a", profile]]);
    expect(coordinatorLevel("a", idx)).toBe(0);
  });

  it("terminates on A↔B mutual cycle without infinite recursion", () => {
    const a: AgentProfileLike = {
      id: "a",
      delegates_to: [{ target_agent_id: "b", name: "B", purpose: "", share_context_allowed: false }],
    };
    const b: AgentProfileLike = {
      id: "b",
      delegates_to: [{ target_agent_id: "a", name: "A", purpose: "", share_context_allowed: false }],
    };
    const idx = new Map([["a", a], ["b", b]]);
    // Should not throw or hang — returns a bounded level
    const level = coordinatorLevel("a", idx);
    expect(level).toBeGreaterThanOrEqual(0);
    expect(level).toBeLessThan(100);
  });
});

// ─── 3. AddressBookEditor ─────────────────────────────────────────────────────

describe("AddressBookEditor", () => {
  it("lists existing entries", async () => {
    const entry = makeDelegateTarget();
    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={[entry]}
          onChange={() => {}}
        />
      );
    });
    const entries = container.querySelectorAll("[data-testid='address-book-entry']");
    expect(entries).toHaveLength(1);
    expect(entries[0].textContent).toContain("Reviewer");
  });

  it("candidate list excludes self (profileId)", async () => {
    // Override getAgents to return self + one other
    const { getAgents } = await import("../../../lib/api");
    (getAgents as ReturnType<typeof vi.fn>).mockResolvedValueOnce([
      { agent_id: "agent-a", name: "Self", message_count: 0, has_active_run: false, queue_depth: 0, thread_id: null, created_at: "" },
      { agent_id: "agent-b", name: "Other", message_count: 0, has_active_run: false, queue_depth: 0, thread_id: null, created_at: "" },
    ]);

    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={[]}
          onChange={() => {}}
        />
      );
    });
    // Async getAgents() resolution needs a flush before the list re-renders.
    await act(async () => {});

    // The candidate picker only renders once the search input is focused.
    const searchInput = container.querySelector("[data-testid='address-book-search']") as HTMLInputElement;
    await act(async () => { searchInput.focus(); });

    // Candidate list should not contain self
    const selfRow = container.querySelector("[data-testid='candidate-agent-agent-a']");
    expect(selfRow).toBeNull();

    // But should contain the other agent
    const otherRow = container.querySelector("[data-testid='candidate-agent-agent-b']");
    expect(otherRow).not.toBeNull();
  });

  it("share_context_allowed defaults to off for new entries", async () => {
    const { getAgents } = await import("../../../lib/api");
    (getAgents as ReturnType<typeof vi.fn>).mockResolvedValueOnce([
      { agent_id: "agent-b", name: "Reviewer", message_count: 0, has_active_run: false, queue_depth: 0, thread_id: null, created_at: "" },
    ]);

    let captured: DelegateTarget[] = [];
    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={[]}
          onChange={(v) => { captured = v; }}
        />
      );
    });
    await act(async () => {});

    const searchInput = container.querySelector("[data-testid='address-book-search']") as HTMLInputElement;
    await act(async () => { searchInput.focus(); });

    const agentRow = container.querySelector("[data-testid='candidate-agent-agent-b']") as HTMLDivElement;
    await act(async () => { agentRow.click(); });

    expect(captured).toHaveLength(1);
    expect(captured[0].share_context_allowed).toBe(false);
  });

  // Clicking anywhere on a candidate row other than its checkbox both adds
  // the entry AND dismisses the picker — the quick "pick one and go" path.
  it("clicking a candidate row (not its checkbox) adds the entry and closes the picker", async () => {
    const { getAgents } = await import("../../../lib/api");
    (getAgents as ReturnType<typeof vi.fn>).mockResolvedValueOnce([
      { agent_id: "agent-b", name: "Reviewer", message_count: 0, has_active_run: false, queue_depth: 0, thread_id: null, created_at: "" },
    ]);

    let captured: DelegateTarget[] = [];
    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={[]}
          onChange={(v) => { captured = v; }}
        />
      );
    });
    await act(async () => {});

    const searchInput = container.querySelector("[data-testid='address-book-search']") as HTMLInputElement;
    await act(async () => { searchInput.focus(); });
    expect(container.querySelector("[data-testid='address-book-picker']")).not.toBeNull();

    const agentRow = container.querySelector("[data-testid='candidate-agent-agent-b']") as HTMLDivElement;
    await act(async () => { agentRow.click(); });

    expect(captured).toHaveLength(1);
    expect(container.querySelector("[data-testid='address-book-picker']")).toBeNull();
  });

  // The checkbox is the multi-select path: it toggles membership immediately
  // but must NOT close the picker (stopPropagation keeps the row's own
  // onClick — which both selects and dismisses — from also firing), and the
  // row itself must not disappear so a second toggle (uncheck) is possible.
  it("candidate checkbox toggles membership without closing the picker, and can uncheck", async () => {
    const { getAgents } = await import("../../../lib/api");
    (getAgents as ReturnType<typeof vi.fn>).mockResolvedValueOnce([
      { agent_id: "agent-b", name: "Reviewer", message_count: 0, has_active_run: false, queue_depth: 0, thread_id: null, created_at: "" },
    ]);

    let captured: DelegateTarget[] = [];
    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={[]}
          onChange={(v) => { captured = v; }}
        />
      );
    });
    await act(async () => {});

    const searchInput = container.querySelector("[data-testid='address-book-search']") as HTMLInputElement;
    await act(async () => { searchInput.focus(); });

    const checkbox = container.querySelector("[data-testid='candidate-checkbox-agent-b']") as HTMLButtonElement;
    await act(async () => { checkbox.click(); });

    // Added, and the picker stayed open.
    expect(captured).toHaveLength(1);
    expect(container.querySelector("[data-testid='address-book-picker']")).not.toBeNull();
    // The row is still present (not filtered out now that it's added) — but
    // `value` is a controlled prop the test doesn't feed back in, so re-render
    // with the captured entries to see the checkbox reflect the new state.
    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={captured}
          onChange={(v) => { captured = v; }}
        />
      );
    });
    await act(async () => {});

    expect(container.querySelector("[data-testid='candidate-agent-agent-b']")).not.toBeNull();
    const updatedCheckbox = container.querySelector("[data-testid='candidate-checkbox-agent-b']");
    expect(updatedCheckbox?.getAttribute("aria-checked")).toBe("true");
  });

  // The "sm" ToggleSwitch sits inside the collapsed delegate row's own
  // onClick (expand/collapse). Clicking it must only flip share_context_allowed
  // and must NOT also expand the row — that combo previously fired both,
  // since the switch's click bubbled up to the row.
  it("toggling the collapsed row's share-context switch does not also expand the row", async () => {
    const entry = makeDelegateTarget();
    let current = [entry];

    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={current}
          onChange={(v) => { current = v; }}
        />
      );
    });

    const row = container.querySelector("[data-testid='address-book-entry']") as HTMLDivElement;
    // Only one ToggleSwitch renders while collapsed — the "sm" inline one.
    const toggle = row.querySelector("[role='switch']") as HTMLButtonElement;
    await act(async () => { toggle.click(); });

    expect(current[0].share_context_allowed).toBe(true);
    // Still collapsed: no purpose textarea rendered.
    expect(row.querySelector("textarea")).toBeNull();
  });

  it("delete-entry removes the row", async () => {
    const entry = makeDelegateTarget();
    let current = [entry];

    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={current}
          onChange={(v) => { current = v; }}
        />
      );
    });

    const removeBtn = container.querySelector("[data-testid='remove-entry-button']") as HTMLButtonElement;
    await act(async () => { removeBtn.click(); });

    expect(current).toHaveLength(0);
  });

  // Regression: the picker used to build its own AgentProfileLike index by
  // stripping delegates_to off every AgentSnapshot, so coordinatorLevel()
  // always bottomed out at 0 ("L") no matter how deep an agent's real
  // delegation chain was. It must read the server-computed
  // AgentSnapshot.coordinator_level field directly instead.
  it("existing entry's badge reflects the target's server-computed coordinator_level", async () => {
    const { getAgents } = await import("../../../lib/api");
    (getAgents as ReturnType<typeof vi.fn>).mockResolvedValueOnce([
      { agent_id: "agent-b", name: "Reviewer", coordinator_level: 2, message_count: 0, has_active_run: false, queue_depth: 0, thread_id: null, created_at: "" },
    ]);
    const entry = makeDelegateTarget({ target_agent_id: "agent-b" });

    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={[entry]}
          onChange={() => {}}
        />
      );
    });
    // Async getAgents() resolution needs a flush before the badge re-renders.
    await act(async () => {});

    const row = container.querySelector("[data-testid='address-book-entry']");
    const badge = row?.querySelector("[data-testid='coordinator-badge']");
    expect(badge?.textContent).toBe("C2");
  });

  it("candidate row's badge reflects the target's server-computed coordinator_level", async () => {
    const { getAgents } = await import("../../../lib/api");
    (getAgents as ReturnType<typeof vi.fn>).mockResolvedValueOnce([
      { agent_id: "agent-b", name: "Reviewer", coordinator_level: 2, message_count: 0, has_active_run: false, queue_depth: 0, thread_id: null, created_at: "" },
    ]);

    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={[]}
          onChange={() => {}}
        />
      );
    });
    // Async getAgents() resolution needs a flush before the list re-renders.
    await act(async () => {});

    const searchInput = container.querySelector("[data-testid='address-book-search']") as HTMLInputElement;
    await act(async () => { searchInput.focus(); });

    const row = container.querySelector("[data-testid='candidate-agent-agent-b']");
    const badge = row?.querySelector("[data-testid='coordinator-badge']");
    expect(badge?.textContent).toBe("C2");
  });

  it("the add-more picker is closed until the search input is focused, and closes again on outside click", async () => {
    const { getAgents } = await import("../../../lib/api");
    (getAgents as ReturnType<typeof vi.fn>).mockResolvedValueOnce([
      { agent_id: "agent-b", name: "Reviewer", message_count: 0, has_active_run: false, queue_depth: 0, thread_id: null, created_at: "" },
    ]);

    await act(async () => {
      root.render(
        <AddressBookEditor
          profileId="agent-a"
          value={[]}
          onChange={() => {}}
        />
      );
    });
    await act(async () => {});

    // Closed at rest.
    expect(container.querySelector("[data-testid='address-book-picker']")).toBeNull();

    // Opens on focus.
    const searchInput = container.querySelector("[data-testid='address-book-search']") as HTMLInputElement;
    await act(async () => { searchInput.focus(); });
    expect(container.querySelector("[data-testid='address-book-picker']")).not.toBeNull();

    // Closes on outside click (a mousedown target outside the search+picker wrapper).
    await act(async () => {
      document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    });
    expect(container.querySelector("[data-testid='address-book-picker']")).toBeNull();
  });
});
