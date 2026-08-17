// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { AsyncFormRequestCard } from "../AsyncFormRequestCard";
import { AsyncFormAnswerBubble, AsyncFormAnswerContentBubble } from "../FormAnswerBubble";
import { FormDismissedIndicator } from "../FormDismissedIndicator";
import { FormWithdrawnIndicator } from "../FormWithdrawnIndicator";
import type { AsyncFormRequestMeta, AsyncFormAnswerMeta } from "../../../types/form";

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

const FORM_META: AsyncFormRequestMeta = {
  form_id: "f-1",
  spec: {
    form_id: "f-1",
    title: "Async test form",
    intro: "Please fill in",
    fields: [
      { id: "name", kind: "text", label: "Your name", required: true },
      { id: "lang", kind: "radio", label: "Language", required: false, options: [{ id: "ts", label: "TypeScript" }, { id: "rs", label: "Rust" }] },
    ],
  },
  mode: "async",
};

const ANSWER_META: AsyncFormAnswerMeta = {
  form_id: "f-1",
  values: {
    name: { kind: "text", value: "Alice" },
    lang: { kind: "selections", values: ["ts"] },
  },
};

describe("AsyncFormRequestCard", () => {
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

  it("renders the form title from spec", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormRequestCard, { meta: FORM_META, agentId: "agent-1" }));
    });
    expect(container.textContent).toContain("Async test form");
  });

  it("renders the intro text", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormRequestCard, { meta: FORM_META, agentId: "agent-1" }));
    });
    expect(container.textContent).toContain("Please fill in");
  });

  it("renders all field labels", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormRequestCard, { meta: FORM_META, agentId: "agent-1" }));
    });
    expect(container.textContent).toContain("Your name");
    expect(container.textContent).toContain("Language");
  });

  it("renders the Cancel / Regenerate / Something-else action row when an action handler is provided", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormRequestCard, { meta: FORM_META, agentId: "agent-1", onAction: async () => {} }));
    });
    expect(container.querySelector("[data-testid='form-action-cancel-btn']")).toBeTruthy();
    expect(container.querySelector("[data-testid='form-action-regenerate-btn']")).toBeTruthy();
    expect(container.querySelector("[data-testid='form-action-other-btn']")).toBeTruthy();
    // The old standalone Decline button is gone — Cancel is its replacement,
    // folded into the shared action row instead of living alongside it.
    expect(container.querySelector("[data-testid='async-form-decline-btn']")).toBeNull();
  });

  it("calls onAction('cancel') when Cancel is clicked — the async form's exit", async () => {
    const onAction = vi.fn(async () => {});
    await act(async () => {
      root.render(React.createElement(AsyncFormRequestCard, { meta: FORM_META, agentId: "agent-1", onAction }));
    });
    const btn = container.querySelector("[data-testid='form-action-cancel-btn']") as HTMLButtonElement;
    await act(async () => { btn.click(); });
    expect(onAction).toHaveBeenCalledWith("cancel", undefined);
  });

  it("does not render the action row when no action handler is provided", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormRequestCard, { meta: FORM_META, agentId: "agent-1" }));
    });
    expect(container.querySelector("[data-testid='form-action-cancel-btn']")).toBeNull();
  });

  it("calls onSubmit when form is submitted", async () => {
    const onSubmit = vi.fn(async () => {});
    await act(async () => {
      root.render(React.createElement(AsyncFormRequestCard, { meta: { ...FORM_META, spec: { ...FORM_META.spec, fields: [] } }, agentId: "agent-1", onSubmit }));
    });
    const submitBtn = container.querySelector("[data-testid='form-submit-btn']") as HTMLButtonElement;
    await act(async () => { submitBtn.click(); });
    expect(onSubmit).toHaveBeenCalledOnce();
  });

  // Pins the MessageList inline "interactive" branch: an unanswered async
  // form that's fallen out of "current" now renders this exact component,
  // fully interactive rather than a blank read-only bubble — confirm the
  // fields are genuinely enabled (not disabled/read-only), not merely that a
  // Submit button exists.
  it("renders enabled, editable inputs — no readOnly/disabled leaking through", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormRequestCard, { meta: FORM_META, agentId: "agent-1", onSubmit: async () => {} }));
    });
    const nameInput = container.querySelector("input[type='text']") as HTMLInputElement;
    expect(nameInput).toBeTruthy();
    expect(nameInput.disabled).toBe(false);
    expect(nameInput.readOnly).toBe(false);
  });

  it("submitting sends the filled-in values through onSubmit for this form_id", async () => {
    const onSubmit = vi.fn(async () => {});
    await act(async () => {
      root.render(React.createElement(AsyncFormRequestCard, {
        meta: { ...FORM_META, spec: { ...FORM_META.spec, fields: [FORM_META.spec.fields[0]] } },
        agentId: "agent-1",
        onSubmit,
      }));
    });
    const nameInput = container.querySelector("input[type='text']") as HTMLInputElement;
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
      setter.call(nameInput, "Bob");
      nameInput.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const submitBtn = container.querySelector("[data-testid='form-submit-btn']") as HTMLButtonElement;
    expect(submitBtn.disabled).toBe(false);
    await act(async () => { submitBtn.click(); });
    expect(onSubmit).toHaveBeenCalledWith({ name: { kind: "text", value: "Bob" } });
  });
});

describe("AsyncFormAnswerBubble", () => {
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

  it("renders the bubble container", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerBubble, { meta: ANSWER_META }));
    });
    const bubble = container.querySelector("[data-testid='async-form-answer-bubble']");
    expect(bubble).toBeTruthy();
  });

  it("renders 'Form response' header", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerBubble, { meta: ANSWER_META }));
    });
    expect(container.textContent).toContain("Form response");
  });

  it("renders text answer value", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerBubble, { meta: ANSWER_META }));
    });
    expect(container.textContent).toContain("Alice");
  });

  it("renders selections answer value", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerBubble, { meta: ANSWER_META }));
    });
    expect(container.textContent).toContain("ts");
  });

  it("renders 'Submitted' when values map is empty", async () => {
    const emptyMeta: AsyncFormAnswerMeta = { form_id: "f-1", values: {} };
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerBubble, { meta: emptyMeta }));
    });
    expect(container.textContent).toContain("Submitted");
  });
});

/** Self-rendering `form_answer` bubble — the preferred renderer for any
 *  entry written post-fix (non-empty `content`). Legacy `content: ""`
 *  entries fall back to `AsyncFormAnswerBubble` above, covered by
 *  "renders 'Submitted' when values map is empty". */
describe("AsyncFormAnswerContentBubble", () => {
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

  const CONTENT = "**Async test form**\n- Your name: Alice\n- Language: TypeScript";

  it("renders the bubble container", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerContentBubble, { content: CONTENT }));
    });
    expect(container.querySelector("[data-testid='form-answer-content-bubble']")).toBeTruthy();
  });

  it("renders the form title", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerContentBubble, { content: CONTENT }));
    });
    expect(container.textContent).toContain("Async test form");
  });

  it("renders each question label and its answer", async () => {
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerContentBubble, { content: CONTENT }));
    });
    expect(container.textContent).toContain("Your name: Alice");
    expect(container.textContent).toContain("Language: TypeScript");
  });

  it("never depends on a form_request entry or pending_forms — renders from content alone", async () => {
    // No AsyncFormRequestCard, no pending-form data anywhere in this test —
    // the bubble must still render fully from `content`.
    await act(async () => {
      root.render(React.createElement(AsyncFormAnswerContentBubble, { content: CONTENT }));
    });
    expect(container.textContent).toContain("Async test form");
    expect(container.textContent).toContain("Alice");
  });
});

describe("FormDismissedIndicator", () => {
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

  it("renders the indicator element", async () => {
    await act(async () => {
      root.render(React.createElement(FormDismissedIndicator));
    });
    const el = container.querySelector("[data-testid='form-dismissed-indicator']");
    expect(el).toBeTruthy();
  });

  it("displays declined text", async () => {
    await act(async () => {
      root.render(React.createElement(FormDismissedIndicator));
    });
    expect(container.textContent).toContain("Form declined");
  });
});

/** Plain, non-interactive trace line for a form dropped because a newer one
 *  replaced it on the same thread (change B — see the backend's
 *  `form_withdrawn_content`, crates/ao-engine-tools-core/src/form_events.rs). */
describe("FormWithdrawnIndicator", () => {
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

  it("renders the indicator element", async () => {
    await act(async () => {
      root.render(React.createElement(FormWithdrawnIndicator, { content: "\"Ship it?\" was withdrawn — a newer question replaced it before it could be answered." }));
    });
    expect(container.querySelector("[data-testid='form-withdrawn-indicator']")).toBeTruthy();
  });

  it("includes the withdrawn question's text — self-contained, no join required", async () => {
    await act(async () => {
      root.render(React.createElement(FormWithdrawnIndicator, { content: "\"Ship it?\" was withdrawn — a newer question replaced it before it could be answered." }));
    });
    expect(container.textContent).toContain("Ship it?");
    expect(container.textContent).toContain("withdrawn");
  });

  it("falls back to a neutral placeholder rather than rendering blank", async () => {
    await act(async () => {
      root.render(React.createElement(FormWithdrawnIndicator, { content: "" }));
    });
    expect(container.textContent?.trim()).not.toBe("");
  });

  it("is a plain pill, not a card — renders no buttons or inputs", async () => {
    await act(async () => {
      root.render(React.createElement(FormWithdrawnIndicator, { content: "\"Ship it?\" was withdrawn." }));
    });
    expect(container.querySelectorAll("button, input, textarea, select").length).toBe(0);
  });
});
