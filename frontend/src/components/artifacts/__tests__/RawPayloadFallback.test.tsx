// @vitest-environment jsdom
//
// `RawPayloadFallback` is the last-resort display for a typed payload that
// doesn't match its renderer's expected shape — it must always
// read as a formatted card, never a raw JSON code blob, no matter what shape
// of value it's handed.
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { RawPayloadFallback } from "../RawPayloadFallback";

describe("RawPayloadFallback", () => {
  let container: HTMLDivElement;
  let root: Root;

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

  async function render(payload: unknown) {
    await act(async () => {
      root.render(React.createElement(RawPayloadFallback, { payload }));
    });
  }

  it("never renders a <pre> JSON dump", async () => {
    await render({ foo: "bar", count: 3 });
    expect(container.querySelector("pre")).toBeNull();
  });

  it("renders an object as labeled key/value rows", async () => {
    await render({ foo: "bar", count: 3 });
    expect(container.textContent).toContain("foo");
    expect(container.textContent).toContain("bar");
    expect(container.textContent).toContain("count");
    expect(container.textContent).toContain("3");
  });

  it("formats a nested object/array value via formatCell rather than [object Object]", async () => {
    await render({ nested: { a: 1 } });
    expect(container.textContent).toContain('{"a":1}');
    expect(container.textContent).not.toContain("[object Object]");
  });

  it("renders an array payload as a simple list", async () => {
    await render(["first", "second", 3]);
    expect(container.textContent).toContain("first");
    expect(container.textContent).toContain("second");
    expect(container.textContent).toContain("3");
  });

  it("renders a primitive payload as plain text", async () => {
    await render("just a string");
    expect(container.textContent).toContain("just a string");
  });

  it("handles an empty object without throwing", async () => {
    expect(() => {
      act(() => {
        root.render(React.createElement(RawPayloadFallback, { payload: {} }));
      });
    }).not.toThrow();
  });

  it("truncates a very long value with an ellipsis", async () => {
    const longValue = "x".repeat(1000);
    await render({ big: longValue });
    const text = container.textContent ?? "";
    expect(text).toContain("…");
    expect(text.length).toBeLessThan(longValue.length);
  });
});
