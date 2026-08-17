/**
 * Tone class selection for a `role: "system"` chat bubble — the render-side
 * half of surfacing watch-contract authoring convergence/retry/freeze
 * messages distinctly instead of every system message rendering identically.
 */

import { describe, it, expect } from "vitest";
import { systemMessageToneClass } from "../MessageList";

describe("systemMessageToneClass", () => {
  it("renders a success severity with the success theme tone", () => {
    expect(systemMessageToneClass("success")).toContain("--success");
  });

  it("renders an error severity with the error theme tone", () => {
    expect(systemMessageToneClass("error")).toContain("--error");
  });

  it("falls back to the original neutral tone when severity is undefined", () => {
    const toneClass = systemMessageToneClass(undefined);
    expect(toneClass).toContain("--text-tertiary");
    expect(toneClass).not.toContain("--success");
    expect(toneClass).not.toContain("--error");
  });

  it("falls back to neutral for any unrecognized value, never guessing at a new severity", () => {
    expect(systemMessageToneClass("warning")).toBe(systemMessageToneClass(undefined));
    expect(systemMessageToneClass(null)).toBe(systemMessageToneClass(undefined));
  });
});
