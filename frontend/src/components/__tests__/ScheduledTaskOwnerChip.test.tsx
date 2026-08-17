// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { ScheduledTaskOwnerChip, ScheduledTaskOwnerDot } from "../scheduled/ScheduledTaskOwnerChip";
import type { ScheduledTaskOwner } from "../../lib/scheduledTaskShared";

const AGENT_NO_EMOJI: ScheduledTaskOwner = {
  id: "agent-1",
  name: "Axew",
  isTeam: false,
};

const AGENT_WITH_EMOJI: ScheduledTaskOwner = {
  id: "agent-2",
  name: "Buildbot",
  emoji: "🤖",
  isTeam: false,
};

const TEAM: ScheduledTaskOwner = {
  id: "team-1",
  name: "Platform Team",
  isTeam: true,
};

describe("ScheduledTaskOwnerChip", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => { root.unmount(); });
    document.body.removeChild(container);
  });

  it("renders agent name", async () => {
    await act(async () => {
      root.render(React.createElement(ScheduledTaskOwnerChip, { owner: AGENT_NO_EMOJI }));
    });
    expect(container.textContent).toContain("Axew");
  });

  it("renders team name", async () => {
    await act(async () => {
      root.render(React.createElement(ScheduledTaskOwnerChip, { owner: TEAM }));
    });
    expect(container.textContent).toContain("Platform Team");
  });

  it("shows emoji when present", async () => {
    await act(async () => {
      root.render(React.createElement(ScheduledTaskOwnerChip, { owner: AGENT_WITH_EMOJI }));
    });
    expect(container.textContent).toContain("🤖");
  });

  it("falls back to initial letter when no emoji", async () => {
    await act(async () => {
      root.render(React.createElement(ScheduledTaskOwnerChip, { owner: AGENT_NO_EMOJI }));
    });
    expect(container.textContent).toContain("A");
    expect(container.textContent).not.toContain("🤖");
  });

  it("renders both size variants without throwing", async () => {
    await act(async () => {
      root.render(
        React.createElement(
          React.Fragment,
          null,
          React.createElement(ScheduledTaskOwnerChip, { owner: AGENT_WITH_EMOJI, size: "sm" }),
          React.createElement(ScheduledTaskOwnerChip, { owner: AGENT_WITH_EMOJI, size: "md" }),
        ),
      );
    });
    expect(container.textContent).toContain("Buildbot");
  });

  it("renders ScheduledTaskOwnerDot without a label", async () => {
    await act(async () => {
      root.render(React.createElement(ScheduledTaskOwnerDot, { owner: TEAM }));
    });
    expect(container.textContent).not.toContain("Platform Team");
    expect(container.querySelector("span")).not.toBeNull();
  });
});
