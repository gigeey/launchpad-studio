// @vitest-environment jsdom
/**
 * Regression coverage for rendering an answered form (sync or async) as the
 * SAME interactive form component, disabled and filled in — not the raw
 * serialized answer envelope that used to leak into the chat transcript as
 * plain text (`Label: {"kind":"selections","values":["clean"]}`, skipped
 * optionals as the literal string "(no answer)").
 *
 * `buildAnsweredAsyncForm` is the pure conversion `MessageList.tsx`'s
 * `form_answer` render branch runs an async entry's spec snapshot (see the
 * backend's `FormAnswerMeta.spec` — crates/ao-engine-tools-core/src/form_events.rs)
 * through before handing it to `AskUserQuestionForm`; splitting it out here
 * follows the same pattern `MessageList.formRequestBranch.test.ts` uses for
 * `resolveFormRequestBranch`/`resolveInlineAsyncFormTarget` — the DOM
 * renderer itself isn't feasible to mount in jsdom (virtualizer + missing
 * `IntersectionObserver`), so the decision logic is tested directly and the
 * shared widgets it feeds (`AskUserQuestionForm`, `AsyncFormAnswerBubble`)
 * are exercised with the exact shapes that conversion produces.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { buildAnsweredAsyncForm } from "../MessageList";
import { AskUserQuestionForm } from "../AskUserQuestionForm";
import { AsyncFormAnswerBubble } from "../FormAnswerBubble";
import type { AsyncFormSpec, FormAnswerMap, AsyncFormAnswerMeta } from "../../../types/form";

vi.mock("../../../lib/api", () => ({
  uploadAttachment: vi.fn(),
  deleteAttachment: vi.fn(),
}));

vi.mock("../../../stores/userPreferencesStore", () => ({
  useIsDark: () => false,
  useUserPreferencesStore: (sel: (s: { bubbleColor: string }) => unknown) =>
    sel({ bubbleColor: "#1164A3" }),
}));

vi.mock("../../../lib/colorUtils", () => ({
  adjustBubbleColor: (_color: string, _dark: boolean) => "#1164A3",
}));

// Mirrors the real bug report verbatim: a colliding text field left
// untouched (optional, skipped), plus a radio field answered by option id
// "clean" whose label is "PASS — both arrived blank/unticked".
const SPEC: AsyncFormSpec = {
  form_id: "form-b",
  title: "Form B — supersession test",
  intro: undefined,
  fields: [
    { id: "colliding_field_1", kind: "text", label: "COLLIDING FIELD 1 — is it empty?", required: false },
    {
      id: "verdict",
      kind: "radio",
      label: "Verdict on the two fields above",
      required: true,
      options: [
        { id: "clean", label: "PASS — both arrived blank/unticked" },
        { id: "dirty", label: "FAIL — something leaked" },
      ],
    },
  ],
};

const VALUES: FormAnswerMap = {
  // colliding_field_1 deliberately omitted — the operator never touched it.
  verdict: { kind: "selections", values: ["clean"] },
};

describe("buildAnsweredAsyncForm", () => {
  it("converts a spec snapshot into a FormRequestPayload", () => {
    const form = buildAnsweredAsyncForm("form-b", SPEC);
    expect(form).toEqual({
      form_id: "form-b",
      agent_id: "",
      session_id: "",
      title: "Form B — supersession test",
      intro: undefined,
      fields: SPEC.fields,
    });
  });

  it("falls back to the spec's own form_id when the entry's form_id isn't a string", () => {
    const form = buildAnsweredAsyncForm(undefined, SPEC);
    expect(form?.form_id).toBe("form-b");
  });

  it("returns null when spec is absent — legacy entry with no snapshot", () => {
    expect(buildAnsweredAsyncForm("form-b", undefined)).toBeNull();
  });
});

describe("answered form render path — spec-bearing entry (i, ii)", () => {
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

  async function renderAnswered() {
    const form = buildAnsweredAsyncForm("form-b", SPEC)!;
    await act(async () => {
      root.render(
        React.createElement(AskUserQuestionForm, {
          agentId: "agent-1",
          form,
          onSubmit: async () => {},
          readOnly: true,
          initialAnswers: VALUES,
        }),
      );
    });
  }

  it("(i) renders the ticked radio option's human-readable LABEL, never the raw option id", async () => {
    await renderAnswered();
    expect(container.textContent).toContain("PASS — both arrived blank/unticked");
    // "clean" (the option id) must not appear on its own anywhere in the
    // rendered output — it's a substring of nothing else in this fixture.
    expect(container.textContent).not.toContain("clean");
  });

  it("(i) never leaks the raw answer envelope JSON into the DOM", async () => {
    await renderAnswered();
    expect(container.textContent).not.toContain("kind");
    expect(container.textContent).not.toContain("{");
    expect(container.textContent).not.toContain("}");
  });

  it("(i) disables the controls instead of leaving them editable", async () => {
    await renderAnswered();
    const radios = container.querySelectorAll("input[type='radio']");
    expect(radios.length).toBeGreaterThan(0);
    radios.forEach((r) => expect((r as HTMLInputElement).disabled).toBe(true));
    // Read-only forms drop the Submit/action footer entirely.
    expect(container.querySelector("[data-testid='form-submit-btn']")).toBeNull();
  });

  it("(i) ticks the answered radio option and leaves the other unticked", async () => {
    await renderAnswered();
    const radios = Array.from(container.querySelectorAll("input[type='radio']")) as HTMLInputElement[];
    const checked = radios.filter((r) => r.checked);
    expect(checked).toHaveLength(1);
  });

  it("(ii) renders the skipped optional field as its normal control, empty", async () => {
    await renderAnswered();
    const textInput = container.querySelector("input[type='text']") as HTMLInputElement;
    expect(textInput).toBeTruthy();
    expect(textInput.value).toBe("");
    expect(textInput.readOnly).toBe(true);
  });

  it("(ii) the literal string '(no answer)' appears NOWHERE in the rendered output", async () => {
    await renderAnswered();
    expect(container.textContent).not.toContain("(no answer)");
  });

  it("(ii) still renders the skipped field's own label, not a text substitute", async () => {
    await renderAnswered();
    expect(container.textContent).toContain("COLLIDING FIELD 1 — is it empty?");
  });
});

describe("answered form render path — legacy spec-less entry (iii)", () => {
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

  // Shape of a `form_answer` entry persisted before the spec-snapshot fix —
  // `metadata.spec` is simply absent (backend omits it via
  // `skip_serializing_if`), never null or malformed.
  const LEGACY_META: AsyncFormAnswerMeta = {
    form_id: "legacy-form-1",
    values: {
      colliding_field_1: { kind: "text", value: "hello there" },
      verdict: { kind: "selections", values: ["clean"] },
    },
  };

  it("(iii) the routing decision falls back to the values-only bubble — no spec, no form", () => {
    expect(buildAnsweredAsyncForm(LEGACY_META.form_id, LEGACY_META.spec)).toBeNull();
  });

  it("(iii) renders without throwing", async () => {
    await expect(
      act(async () => {
        root.render(React.createElement(AsyncFormAnswerBubble, { meta: LEGACY_META }));
      }),
    ).resolves.not.toThrow();
  });

  it("(iii) renders human-readable values, never the raw JSON envelope", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerBubble, { meta: LEGACY_META }));
    });
    expect(container.textContent).toContain("hello there");
    expect(container.textContent).not.toContain('"kind"');
    expect(container.textContent).not.toContain('"value"');
  });

  it("(iii) never renders the literal string '(no answer)'", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerBubble, { meta: LEGACY_META }));
    });
    expect(container.textContent).not.toContain("(no answer)");
  });

  it("(iii) a field genuinely absent from `values` (fully skipped) is simply omitted, not stringified", async () => {
    const sparse: AsyncFormAnswerMeta = { form_id: "legacy-form-2", values: {} };
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerBubble, { meta: sparse }));
    });
    expect(container.textContent).not.toContain("(no answer)");
    expect(container.textContent).not.toContain("undefined");
  });
});
