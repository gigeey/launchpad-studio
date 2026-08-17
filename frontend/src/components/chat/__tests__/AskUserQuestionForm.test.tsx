// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { AskUserQuestionForm } from "../AskUserQuestionForm";
import type { FormRequestPayload, FormAnswerMap, FormAction } from "../../../types/form";

vi.mock("../../../lib/api", () => ({
  uploadAttachment: vi.fn(),
  deleteAttachment: vi.fn(),
}));

const BASE_FORM: FormRequestPayload = {
  form_id: "form-1",
  agent_id: "agent-1",
  session_id: "session-1",
  title: "Test form",
  fields: [],
};

describe("AskUserQuestionForm", () => {
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

  async function render(
    form: FormRequestPayload,
    onSubmit: (a: FormAnswerMap) => Promise<void> = async () => {},
    onAction?: (a: FormAction, note?: string) => Promise<void>,
  ) {
    await act(async () => {
      root.render(React.createElement(AskUserQuestionForm, { agentId: "agent-1", form, onSubmit, onAction }));
    });
  }

  function submitBtn() {
    return container.querySelector("[data-testid='form-submit-btn']") as HTMLButtonElement;
  }

  function actionBtn(action: FormAction) {
    return container.querySelector(`[data-testid='form-action-${action}-btn']`) as HTMLButtonElement | null;
  }

  it("renders form title", async () => {
    await render({ ...BASE_FORM, title: "Pick a direction" });
    expect(container.textContent).toContain("Pick a direction");
  });

  it("renders optional intro text", async () => {
    await render({ ...BASE_FORM, intro: "Some context here" });
    expect(container.textContent).toContain("Some context here");
  });

  it("submit is enabled when there are no required fields", async () => {
    await render({ ...BASE_FORM, fields: [{ id: "note", kind: "text", label: "Note", required: false }] });
    expect(submitBtn().disabled).toBe(false);
  });

  it("submit is disabled when required text field is empty", async () => {
    await render({
      ...BASE_FORM,
      fields: [{ id: "name", kind: "text", label: "Name", required: true }],
    });
    expect(submitBtn().disabled).toBe(true);
  });

  it("renders a text input for text-kind fields", async () => {
    await render({
      ...BASE_FORM,
      fields: [{ id: "name", kind: "text", label: "Name", required: true, placeholder: "Enter name" }],
    });
    const input = container.querySelector("input[type='text']") as HTMLInputElement;
    expect(input).toBeTruthy();
    expect(input.placeholder).toBe("Enter name");
  });

  it("submit calls onSubmit with selections answer for radio", async () => {
    const received: FormAnswerMap[] = [];
    await render(
      {
        ...BASE_FORM,
        fields: [{
          id: "choice",
          kind: "radio",
          label: "Pick",
          required: false,
          options: [{ id: "x", label: "X" }, { id: "y", label: "Y" }],
        }],
      },
      async (answers) => { received.push(answers); },
    );
    // With no selection, submit works (optional field)
    await act(async () => { submitBtn().click(); });
    expect(received).toHaveLength(1);
    expect(Object.keys(received[0])).toHaveLength(0);
  });

  it("submit disabled when required checkbox has no selection", async () => {
    await render({
      ...BASE_FORM,
      fields: [{
        id: "cb1",
        kind: "checkbox",
        label: "Pick one",
        required: true,
        options: [{ id: "a", label: "A" }, { id: "b", label: "B" }],
      }],
    });
    expect(submitBtn().disabled).toBe(true);
  });

  it("submit enabled after checking a checkbox option", async () => {
    await render({
      ...BASE_FORM,
      fields: [{
        id: "cb1",
        kind: "checkbox",
        label: "Pick one",
        required: true,
        options: [{ id: "a", label: "A" }, { id: "b", label: "B" }],
      }],
    });
    const checkbox = container.querySelector("input[type='checkbox']") as HTMLInputElement;
    await act(async () => { checkbox.click(); });
    expect(submitBtn().disabled).toBe(false);
  });

  it("submit calls onSubmit with selections answer", async () => {
    const received: FormAnswerMap[] = [];
    await render(
      {
        ...BASE_FORM,
        fields: [{
          id: "choice",
          kind: "radio",
          label: "Pick",
          required: true,
          options: [{ id: "x", label: "X" }, { id: "y", label: "Y" }],
        }],
      },
      async (answers) => { received.push(answers); },
    );
    const radios = container.querySelectorAll("input[type='radio']");
    await act(async () => { (radios[1] as HTMLInputElement).click(); });
    await act(async () => { submitBtn().click(); });
    expect(received[0]["choice"]).toEqual({ kind: "selections", values: ["y"] });
  });

  it("omits empty optional fields from the answer map", async () => {
    const received: FormAnswerMap[] = [];
    await render(
      {
        ...BASE_FORM,
        fields: [
          { id: "opt", kind: "text", label: "Optional", required: false },
          { id: "req", kind: "text", label: "Required", required: false },
        ],
      },
      async (answers) => { received.push(answers); },
    );
    await act(async () => { submitBtn().click(); });
    expect(received).toHaveLength(1);
    expect(Object.keys(received[0])).toHaveLength(0);
  });

  describe("action row (Cancel / Regenerate / Something else)", () => {
    it("does not render when onAction is undefined", async () => {
      await render(BASE_FORM);
      expect(actionBtn("cancel")).toBeNull();
      expect(actionBtn("regenerate")).toBeNull();
      expect(actionBtn("other")).toBeNull();
    });

    it("renders all three buttons when onAction is provided", async () => {
      await render(BASE_FORM, async () => {}, async () => {});
      expect(actionBtn("cancel")).toBeTruthy();
      expect(actionBtn("regenerate")).toBeTruthy();
      expect(actionBtn("other")).toBeTruthy();
    });

    it("clicking Cancel calls onAction with 'cancel'", async () => {
      const received: FormAction[] = [];
      await render(BASE_FORM, async () => {}, async (a) => { received.push(a); });
      await act(async () => { actionBtn("cancel")!.click(); });
      expect(received).toEqual(["cancel"]);
    });

    it("clicking Regenerate calls onAction with 'regenerate'", async () => {
      const received: FormAction[] = [];
      await render(BASE_FORM, async () => {}, async (a) => { received.push(a); });
      await act(async () => { actionBtn("regenerate")!.click(); });
      expect(received).toEqual(["regenerate"]);
    });

    it("clicking Something else takes over the form body instead of firing immediately", async () => {
      const received: [FormAction, string | undefined][] = [];
      await render(
        { ...BASE_FORM, fields: [{ id: "name", kind: "text", label: "Name", required: true }] },
        async () => {},
        async (a, note) => { received.push([a, note]); },
      );
      await act(async () => { actionBtn("other")!.click(); });
      expect(received).toEqual([]);
      expect(container.querySelector("[data-testid='form-action-other-input']")).toBeTruthy();
      // The action row (Cancel/Regenerate/Something else) is gone while the note takes over.
      expect(actionBtn("cancel")).toBeNull();
      // The original required field is no longer rendered.
      expect(container.querySelector("input[type='text']")).toBeNull();
    });

    it("overrides required-field validation while the note is open, using the shared Submit button", async () => {
      const received: [FormAction, string | undefined][] = [];
      await render(
        { ...BASE_FORM, fields: [{ id: "name", kind: "text", label: "Name", required: true }] },
        async () => {},
        async (a, note) => { received.push([a, note]); },
      );
      // Required field is unanswered, so Submit starts disabled.
      expect(submitBtn().disabled).toBe(true);
      await act(async () => { actionBtn("other")!.click(); });
      // Still disabled — the note itself is empty — but no longer gated by the required field.
      expect(submitBtn().disabled).toBe(true);
      const input = container.querySelector("[data-testid='form-action-other-input']") as HTMLTextAreaElement;
      await act(async () => {
        const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, "value")!.set!;
        setter.call(input, "Let's try a different approach");
        input.dispatchEvent(new Event("input", { bubbles: true }));
      });
      expect(submitBtn().disabled).toBe(false);
      await act(async () => { submitBtn().click(); });
      expect(received).toEqual([["other", "Let's try a different approach"]]);
    });

    it("closing the note input without sending does not call onAction and restores the form", async () => {
      const received: FormAction[] = [];
      await render(
        { ...BASE_FORM, fields: [{ id: "name", kind: "text", label: "Name", required: true }] },
        async () => {},
        async (a) => { received.push(a); },
      );
      await act(async () => { actionBtn("other")!.click(); });
      const closeBtn = container.querySelector("[data-testid='form-action-other-close-btn']") as HTMLButtonElement;
      await act(async () => { closeBtn.click(); });
      expect(received).toEqual([]);
      expect(actionBtn("cancel")).toBeTruthy();
      expect(container.querySelector("[data-testid='form-action-other-input']")).toBeNull();
      expect(container.querySelector("input[type='text']")).toBeTruthy();
    });

    it("disables Submit and the other action buttons while an action is in flight", async () => {
      let resolveAction: () => void = () => {};
      const pending = new Promise<void>((resolve) => { resolveAction = resolve; });
      await render(
        { ...BASE_FORM, fields: [{ id: "note", kind: "text", label: "Note", required: false }] },
        async () => {},
        async () => { await pending; },
      );
      act(() => { actionBtn("cancel")!.click(); });
      expect(submitBtn().disabled).toBe(true);
      expect(actionBtn("regenerate")!.disabled).toBe(true);
      expect(actionBtn("other")!.disabled).toBe(true);
      await act(async () => { resolveAction(); await pending; });
    });

    it("does not render the action row in read-only mode even if onAction is passed", async () => {
      await act(async () => {
        root.render(React.createElement(AskUserQuestionForm, {
          agentId: "agent-1",
          form: BASE_FORM,
          onSubmit: async () => {},
          onAction: async () => {},
          readOnly: true,
        }));
      });
      expect(actionBtn("cancel")).toBeNull();
    });
  });

  // Regression: a rejected `onSubmit`/`onAction` used to be rethrown into a
  // `void handleSubmit()` / `void handleAction(...)` click handler — every
  // call site discards that promise, so the rejection surfaced nowhere but
  // an unhandled-rejection console warning. The form itself already
  // re-enabled via `submitting`/`actionPending`'s `finally` resets; what was
  // missing was ever telling the operator it didn't go through.
  describe("submit/action failure surfacing", () => {
    it("does not throw out of the click handler when onSubmit rejects", async () => {
      await render(BASE_FORM, async () => { throw new Error("boom"); });
      await expect(act(async () => { submitBtn().click(); })).resolves.not.toThrow();
    });

    it("shows an inline error and re-enables Submit after a rejected submit", async () => {
      await render(BASE_FORM, async () => { throw new Error("network error"); });
      await act(async () => { submitBtn().click(); });
      expect(container.querySelector("[data-testid='form-submit-error']")).toBeTruthy();
      expect(container.textContent).toContain("network error");
      expect(submitBtn().disabled).toBe(false);
    });

    it("clears the error on the next submit attempt", async () => {
      let shouldFail = true;
      const received: FormAnswerMap[] = [];
      await render(BASE_FORM, async (answers) => {
        if (shouldFail) throw new Error("first attempt fails");
        received.push(answers);
      });
      await act(async () => { submitBtn().click(); });
      expect(container.querySelector("[data-testid='form-submit-error']")).toBeTruthy();

      shouldFail = false;
      await act(async () => { submitBtn().click(); });
      expect(container.querySelector("[data-testid='form-submit-error']")).toBeNull();
      expect(received).toHaveLength(1);
    });

    it("does not throw out of the click handler when onAction rejects", async () => {
      await render(BASE_FORM, async () => {}, async () => { throw new Error("boom"); });
      await expect(act(async () => { actionBtn("cancel")!.click(); })).resolves.not.toThrow();
    });

    it("shows an inline error and re-enables the action row after a rejected action", async () => {
      await render(BASE_FORM, async () => {}, async () => { throw new Error("dismiss failed"); });
      await act(async () => { actionBtn("cancel")!.click(); });
      expect(container.querySelector("[data-testid='form-submit-error']")).toBeTruthy();
      expect(container.textContent).toContain("dismiss failed");
      expect(actionBtn("cancel")!.disabled).toBe(false);
      expect(actionBtn("regenerate")!.disabled).toBe(false);
    });
  });

  describe("minimize button (onMinimize)", () => {
    it("does not render form-minimize-btn when onMinimize is omitted", async () => {
      await render(BASE_FORM);
      expect(container.querySelector("[data-testid='form-minimize-btn']")).toBeNull();
    });

    it("renders form-minimize-btn when onMinimize is provided", async () => {
      await act(async () => {
        root.render(React.createElement(AskUserQuestionForm, {
          agentId: "agent-1",
          form: BASE_FORM,
          onSubmit: async () => {},
          onMinimize: () => {},
        }));
      });
      expect(container.querySelector("[data-testid='form-minimize-btn']")).toBeTruthy();
    });

    it("clicking form-minimize-btn calls onMinimize", async () => {
      let calls = 0;
      await act(async () => {
        root.render(React.createElement(AskUserQuestionForm, {
          agentId: "agent-1",
          form: BASE_FORM,
          onSubmit: async () => {},
          onMinimize: () => { calls++; },
        }));
      });
      const btn = container.querySelector("[data-testid='form-minimize-btn']") as HTMLButtonElement;
      await act(async () => { btn.click(); });
      expect(calls).toBe(1);
    });
  });

  describe("orphaned sync forms (startup reaper)", () => {
    const ORPHANED_FORM: FormRequestPayload = {
      ...BASE_FORM,
      fields: [{ id: "note", kind: "text", label: "Note", required: false }],
      orphaned: true,
    };

    it("shows the dead-run message instead of the interactive form", async () => {
      await render(ORPHANED_FORM);
      expect(container.textContent).toContain("The run waiting on this answer ended.");
    });

    it("does not render a submit button at all", async () => {
      await render(ORPHANED_FORM);
      expect(submitBtn()).toBeNull();
    });

    it("does not render the action row (Cancel/Regenerate/Something else)", async () => {
      await render(ORPHANED_FORM, async () => {}, async () => {});
      expect(actionBtn("cancel")).toBeNull();
      expect(actionBtn("regenerate")).toBeNull();
      expect(actionBtn("other")).toBeNull();
    });

    it("disables the form's own input fields", async () => {
      await render(ORPHANED_FORM);
      const input = container.querySelector("input[type='text']") as HTMLInputElement;
      expect(input).toBeTruthy();
      expect(input.readOnly).toBe(true);
    });

    it("onSubmit is never callable — there is no path left to invoke it", async () => {
      const received: FormAnswerMap[] = [];
      await render(ORPHANED_FORM, async (answers) => { received.push(answers); });
      // No submit button exists to click, and nothing else in the rendered
      // tree can reach `onSubmit` — confirm the DOM has no submit affordance
      // at all rather than merely a disabled one.
      expect(container.querySelector("button")).toBeNull();
      expect(received).toEqual([]);
    });
  });

  // MessageList's inline `form_request` "answered" branch renders exactly
  // this shape (readOnly + initialAnswers) once a form's form_id shows up in
  // the transcript's `form_answer` map — pins that it stays a read-only,
  // pre-filled bubble rather than a blank one.
  describe("read-only with initialAnswers (answered form_request bubble)", () => {
    const ANSWERED_FORM: FormRequestPayload = {
      ...BASE_FORM,
      fields: [
        { id: "name", kind: "text", label: "Name", required: true },
        { id: "lang", kind: "radio", label: "Language", required: false, options: [{ id: "ts", label: "TypeScript" }, { id: "rs", label: "Rust" }] },
      ],
    };
    const ANSWERS: FormAnswerMap = {
      name: { kind: "text", value: "Alice" },
      lang: { kind: "selections", values: ["ts"] },
    };

    async function renderAnswered() {
      await act(async () => {
        root.render(React.createElement(AskUserQuestionForm, {
          agentId: "agent-1",
          form: ANSWERED_FORM,
          onSubmit: async () => {},
          readOnly: true,
          initialAnswers: ANSWERS,
        }));
      });
    }

    it("pre-fills the submitted text value", async () => {
      await renderAnswered();
      const input = container.querySelector("input[type='text']") as HTMLInputElement;
      expect(input.value).toBe("Alice");
    });

    it("disables/read-only's the input instead of leaving it editable", async () => {
      await renderAnswered();
      const input = container.querySelector("input[type='text']") as HTMLInputElement;
      expect(input.readOnly).toBe(true);
    });

    it("renders no Submit button", async () => {
      await renderAnswered();
      expect(submitBtn()).toBeNull();
    });
  });
});
