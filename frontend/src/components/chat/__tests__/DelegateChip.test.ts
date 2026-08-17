/**
 * Pins the Delegate tool chip wording exposed by describeToolCall:
 *
 *   pending + target   → "Delegating to <target>…"
 *   completed + target → "Delegated to <target>"
 *   missing target     → "Delegating…" / "Delegated"   (no misleading "to agent" suffix)
 *
 * The missing-target path used to read "Delegating to agent…" — visually
 * indistinguishable from a generic "Agent" label and confusing during the
 * window before the streaming JSON delta lands the target name. Now the
 * suffix is dropped entirely when we don't actually know the target.
 *
 * These tests exercise the exported function in isolation — no component
 * mounting needed since the logic lives in the pure label helper.
 */

import { describe, it, expect } from "vitest";
import { describeToolCall } from "../StreamingMessage";

describe("describeToolCall — Delegate chip wording", () => {
  it("pending: returns 'Delegating to <target>…' while the tool is in-flight", () => {
    const result = describeToolCall("Delegate", { target: "Reviewer" });
    expect(result.label).toBe("Delegating to Reviewer…");
  });

  it("pending: trims surrounding whitespace from the target before rendering", () => {
    // Defensive — streaming JSON deltas can land with stray whitespace if
    // the model emits e.g. "  Reviewer  ". The chip must not show those.
    const result = describeToolCall("Delegate", { target: "  Reviewer  " });
    expect(result.label).toBe("Delegating to Reviewer…");
  });

  it("completed: returns 'Delegated to <target>' (no ellipsis) after the tool finishes", () => {
    const result = describeToolCall("Delegate", { target: "Developer" }, true);
    expect(result.label).toBe("Delegated to Developer");
  });

  it("missing target: drops the 'to <name>' suffix instead of falling back to a placeholder", () => {
    // The pre-stream / orphan case used to read "Delegating to agent…" — that
    // was the bug. The chip is more honest with a bare "Delegating…" when we
    // genuinely don't know who's on the other end.
    const noTarget = describeToolCall("Delegate", {});
    expect(noTarget.label).toBe("Delegating…");

    const undefinedInput = describeToolCall("Delegate", undefined);
    expect(undefinedInput.label).toBe("Delegating…");

    const emptyString = describeToolCall("Delegate", { target: "" });
    expect(emptyString.label).toBe("Delegating…");

    const whitespaceOnly = describeToolCall("Delegate", { target: "   " });
    expect(whitespaceOnly.label).toBe("Delegating…");
  });

  it("missing target + completed: returns 'Delegated' alone", () => {
    const result = describeToolCall("Delegate", {}, true);
    expect(result.label).toBe("Delegated");
  });

  it("non-string target value is treated as missing rather than coerced", () => {
    // If the streaming-JSON arrives mangled and the target field is e.g. a
    // number, we still want a clean chip rather than "Delegating to 42…".
    const numericTarget = describeToolCall("Delegate", { target: 42 as unknown as string });
    expect(numericTarget.label).toBe("Delegating…");
  });
});
