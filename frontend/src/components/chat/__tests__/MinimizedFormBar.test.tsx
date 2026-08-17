// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { MinimizedFormBar } from "../MinimizedFormBar";

describe("MinimizedFormBar", () => {
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

  it("renders the form title", async () => {
    await act(async () => {
      root.render(React.createElement(MinimizedFormBar, { title: "Pick a direction", onExpand: () => {} }));
    });
    expect(container.textContent).toContain("Pick a direction");
  });

  it("clicking the expand button fires onExpand", async () => {
    let calls = 0;
    await act(async () => {
      root.render(React.createElement(MinimizedFormBar, { title: "Pick a direction", onExpand: () => { calls++; } }));
    });
    const btn = container.querySelector("[data-testid='minimized-form-expand-btn']") as HTMLButtonElement;
    expect(btn).toBeTruthy();
    await act(async () => { btn.click(); });
    expect(calls).toBe(1);
  });

  it("renders the expected testids", async () => {
    await act(async () => {
      root.render(React.createElement(MinimizedFormBar, { title: "Pick a direction", onExpand: () => {} }));
    });
    expect(container.querySelector("[data-testid='minimized-form-bar']")).toBeTruthy();
    expect(container.querySelector("[data-testid='minimized-form-expand-btn']")).toBeTruthy();
  });
});
