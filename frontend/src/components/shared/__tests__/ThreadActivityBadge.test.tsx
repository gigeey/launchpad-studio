// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { ThreadQuestionBadge } from "../ThreadActivityBadge";

describe("ThreadQuestionBadge", () => {
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

  it("async (non-sync) badge renders the literal '?' text, not a vector glyph", async () => {
    await act(async () => {
      root.render(React.createElement(ThreadQuestionBadge, { id: "t1" }));
    });
    const badge = container.querySelector('[data-testid="thread-question-badge-t1"]');
    expect(badge).not.toBeNull();
    expect(badge?.querySelector("svg")).toBeNull();
    expect(badge?.textContent).toContain("?");
    expect(badge?.getAttribute("data-sync")).toBeNull();
  });

  it("sync badge renders the same '?' text glyph, flagged via data-sync", async () => {
    await act(async () => {
      root.render(React.createElement(ThreadQuestionBadge, { id: "t2", sync: true }));
    });
    const badge = container.querySelector('[data-testid="thread-question-badge-t2"]');
    expect(badge).not.toBeNull();
    expect(badge?.querySelector("svg")).toBeNull();
    expect(badge?.textContent).toContain("?");
    expect(badge?.getAttribute("data-sync")).toBe("true");
  });

  it("sync badge renders a pinging ring instead of the old borderRadius morph", async () => {
    await act(async () => {
      root.render(React.createElement(ThreadQuestionBadge, { id: "t3", sync: true }));
    });
    const badge = container.querySelector('[data-testid="thread-question-badge-t3"]') as HTMLElement;
    expect(badge.style.transform).toBe("");
    expect(badge.style.borderRadius).toBe("");
    expect(badge.querySelector(".animate-ping")).not.toBeNull();
  });

  it("async badge is fully static — no inline transform/borderRadius, and no pinging ring", async () => {
    await act(async () => {
      root.render(React.createElement(ThreadQuestionBadge, { id: "t4" }));
    });
    const badge = container.querySelector('[data-testid="thread-question-badge-t4"]') as HTMLElement;
    expect(badge.style.transform).toBe("");
    expect(badge.querySelector(".animate-ping")).toBeNull();
  });
});
