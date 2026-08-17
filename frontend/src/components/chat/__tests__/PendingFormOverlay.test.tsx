// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { PendingFormOverlay } from "../PendingFormOverlay";
import { MinimizedFormBar } from "../MinimizedFormBar";
import { FormWithdrawnIndicator } from "../FormWithdrawnIndicator";
import { useChatStore, isFormMinimized } from "../../../stores/chatStore";
import { submitFormAnswer } from "../../../lib/api";
import type { FormRequestPayload } from "../../../types/form";

vi.mock("../../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../../lib/api")>();
  return {
    ...actual,
    submitFormAnswer: vi.fn(async () => ({})),
    uploadAttachment: vi.fn(),
    deleteAttachment: vi.fn(),
  };
});

const PROJECT_KEY = "project:proj-1";

// The form payload carries the REAL agent id — answer delivery targets it,
// independent of the (synthetic) channel key the form is stored under.
const FORM: FormRequestPayload = {
  form_id: "form-xyz",
  agent_id: "agent-real",
  session_id: "session-1",
  title: "Project channel form",
  fields: [{ id: "note", kind: "text", label: "Note", required: false }],
};

// Mirrors the composer-slot pattern ProjectDetailView/ProjectWorkspace use
// around `PendingFormOverlay` (minus their ChatInput branch, which is out of
// scope for this file): render `MinimizedFormBar` in place of the form when
// minimized, and let `PendingFormOverlay` own the expanded form itself.
// Exercises the same store-derived minimize/expand wiring the real call
// sites use, without pulling in their much heavier page-level dependencies.
function ComposerSlotHarness({ channelKey }: { channelKey: string }) {
  const pendingForm = useChatStore((s) => s.pendingFormByAgent[channelKey]);
  const formMinimized = useChatStore((s) => isFormMinimized(s.minimizedFormByKey, channelKey, pendingForm?.thread_id));
  const setFormMinimized = useChatStore((s) => s.setFormMinimized);
  return React.createElement(
    React.Fragment,
    null,
    pendingForm && formMinimized
      ? React.createElement(MinimizedFormBar, {
          title: pendingForm.title || "Waiting for your response",
          onExpand: () => setFormMinimized(channelKey, pendingForm.thread_id, false),
        })
      : null,
    React.createElement(PendingFormOverlay, { channelKey }),
  );
}

describe("PendingFormOverlay", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    useChatStore.setState({ pendingFormByAgent: {}, minimizedFormByKey: {}, withdrawnFormDrafts: {} });
    vi.mocked(submitFormAnswer).mockClear();
  });

  afterEach(async () => {
    await act(async () => { root.unmount(); });
    document.body.removeChild(container);
  });

  async function render(channelKey: string) {
    await act(async () => {
      root.render(React.createElement(PendingFormOverlay, { channelKey }));
    });
  }

  async function renderWithComposerSlot(channelKey: string) {
    await act(async () => {
      root.render(React.createElement(ComposerSlotHarness, { channelKey }));
    });
  }

  it("renders nothing when the channel has no pending form", async () => {
    await render(PROJECT_KEY);
    expect(container.textContent).not.toContain("Project channel form");
  });

  it("renders the form stored under a synthetic project channel key", async () => {
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM);
    });
    await render(PROJECT_KEY);
    expect(container.textContent).toContain("Project channel form");
  });

  it("does not render a form stored under a different channel", async () => {
    await act(async () => {
      useChatStore.getState().setPendingForm("agent-real", FORM);
    });
    await render(PROJECT_KEY);
    expect(container.textContent).not.toContain("Project channel form");
  });

  it("submits to the payload's real agent id and clears the channel's pending form", async () => {
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM);
    });
    await render(PROJECT_KEY);

    const btn = container.querySelector("[data-testid='form-submit-btn']") as HTMLButtonElement;
    expect(btn).toBeTruthy();
    expect(btn.disabled).toBe(false);
    await act(async () => {
      btn.click();
    });

    expect(submitFormAnswer).toHaveBeenCalledTimes(1);
    const [calledAgent, calledBody] = vi.mocked(submitFormAnswer).mock.calls[0];
    expect(calledAgent).toBe("agent-real");
    expect((calledBody as { form_id: string }).form_id).toBe("form-xyz");

    expect(useChatStore.getState().pendingFormByAgent[PROJECT_KEY]).toBeUndefined();
  });

  it("renders an orphaned sync form as non-interactive and blocks submission", async () => {
    const orphanedForm: FormRequestPayload = { ...FORM, form_id: "form-orphaned", orphaned: true };
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, orphanedForm);
    });
    await render(PROJECT_KEY);

    expect(container.textContent).toContain("The run waiting on this answer ended.");
    expect(container.querySelector("[data-testid='form-submit-btn']")).toBeNull();
    expect(submitFormAnswer).not.toHaveBeenCalled();

    // Still registered in the store — this test asserts there is no way to
    // clear/submit it from the UI, not that it silently vanished.
    expect(useChatStore.getState().pendingFormByAgent[PROJECT_KEY]).toEqual(orphanedForm);
  });

  it("hides the form (without unmounting it) and shows the minimized bar once minimized", async () => {
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM);
    });
    await renderWithComposerSlot(PROJECT_KEY);

    // Expanded by default: no bar, form visible.
    expect(container.querySelector("[data-testid='minimized-form-bar']")).toBeNull();
    expect(container.textContent).toContain("Project channel form");

    const minimizeBtn = container.querySelector("[data-testid='form-minimize-btn']") as HTMLButtonElement;
    expect(minimizeBtn).toBeTruthy();
    await act(async () => {
      minimizeBtn.click();
    });

    expect(useChatStore.getState().minimizedFormByKey[PROJECT_KEY]).toBe(true);

    // The bar takes the composer slot...
    const bar = container.querySelector("[data-testid='minimized-form-bar']");
    expect(bar).toBeTruthy();
    expect(bar?.textContent).toContain("Project channel form");

    // ...while the form itself stays mounted in the DOM — merely hidden via
    // the `hidden` utility class — so any in-progress answers survive.
    const hiddenWrapper = container.querySelector(".hidden");
    expect(hiddenWrapper).toBeTruthy();
    expect(hiddenWrapper?.querySelector("[data-testid='form-minimize-btn']")).toBeTruthy();
  });

  it("restores the form when the minimized bar's expand control is clicked", async () => {
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM);
    });
    await renderWithComposerSlot(PROJECT_KEY);

    const minimizeBtn = container.querySelector("[data-testid='form-minimize-btn']") as HTMLButtonElement;
    await act(async () => {
      minimizeBtn.click();
    });
    expect(useChatStore.getState().minimizedFormByKey[PROJECT_KEY]).toBe(true);
    expect(container.querySelector(".hidden")).toBeTruthy();

    const expandBtn = container.querySelector("[data-testid='minimized-form-expand-btn']") as HTMLButtonElement;
    expect(expandBtn).toBeTruthy();
    await act(async () => {
      expandBtn.click();
    });

    expect(useChatStore.getState().minimizedFormByKey[PROJECT_KEY]).toBeUndefined();
    expect(container.querySelector("[data-testid='minimized-form-bar']")).toBeNull();
    expect(container.querySelector(".hidden")).toBeNull();
    expect(container.textContent).toContain("Project channel form");
  });

  it("force-expands an orphaned form instead of leaving it minimized", async () => {
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM);
    });
    await renderWithComposerSlot(PROJECT_KEY);

    const minimizeBtn = container.querySelector("[data-testid='form-minimize-btn']") as HTMLButtonElement;
    await act(async () => {
      minimizeBtn.click();
    });
    expect(useChatStore.getState().minimizedFormByKey[PROJECT_KEY]).toBe(true);

    // The run backing this form ends — orphan it the way the SSE layer does.
    await act(async () => {
      useChatStore.getState().markPendingFormOrphaned(PROJECT_KEY, FORM.thread_id);
    });

    expect(useChatStore.getState().minimizedFormByKey[PROJECT_KEY]).toBeUndefined();
    expect(container.querySelector("[data-testid='minimized-form-bar']")).toBeNull();
    expect(container.querySelector(".hidden")).toBeNull();
    expect(container.textContent).toContain("The run waiting on this answer ended.");
  });

  // --- Cross-form answer bleed on newest-wins supersession ---------------
  // A second form landing on this same slot while the operator is mid-fill
  // on the first (the `occupied_slot_error()` removal made this reachable)
  // must never let the first form's typed answers survive into the second,
  // especially when the two forms happen to share a field id.

  function typeInto(input: HTMLInputElement, value: string) {
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
    setter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  }

  it("clears answer state when a newer form supersedes a still-typed-into one, even with a colliding field id", async () => {
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM);
    });
    await render(PROJECT_KEY);

    const input = container.querySelector("input[type='text']") as HTMLInputElement;
    expect(input).toBeTruthy();
    await act(async () => { typeInto(input, "some typed draft"); });
    expect((container.querySelector("input[type='text']") as HTMLInputElement).value).toBe("some typed draft");

    // Form B supersedes form A on the exact same slot — same field id
    // ("note"), different form_id and title, exactly the newest-wins swap
    // described in the bug report.
    const FORM_B: FormRequestPayload = {
      form_id: "form-superseding",
      agent_id: "agent-real",
      session_id: "session-2",
      title: "Superseding form",
      fields: [{ id: "note", kind: "text", label: "Note", required: false }],
    };
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM_B);
    });

    expect(container.textContent).toContain("Superseding form");
    expect(container.textContent).not.toContain("Project channel form");
    const inputAfter = container.querySelector("input[type='text']") as HTMLInputElement;
    expect(inputAfter).toBeTruthy();
    // The critical assertion: form B's "note" field must be empty, not
    // pre-populated with form A's typed answer for the colliding field id.
    expect(inputAfter.value).toBe("");
  });

  it("stashes the withdrawn form's draft in the store, reachable by FormWithdrawnIndicator, when superseded", async () => {
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM);
    });
    await render(PROJECT_KEY);

    const input = container.querySelector("input[type='text']") as HTMLInputElement;
    await act(async () => { typeInto(input, "please remember this"); });

    const FORM_B: FormRequestPayload = {
      form_id: "form-superseding-2",
      agent_id: "agent-real",
      session_id: "session-3",
      title: "Another superseding form",
      fields: [],
    };
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM_B);
    });

    const draft = useChatStore.getState().withdrawnFormDrafts["form-xyz"];
    expect(draft).toEqual([{ fieldId: "note", label: "Note", value: "please remember this" }]);

    // Reachable in the timeline: wherever the backend's `form_withdrawn`
    // entry (metadata.form_id === "form-xyz") renders, `FormWithdrawnIndicator`
    // can look this draft up by that same id and surface it for the
    // operator to read/copy back.
    await act(async () => {
      root.render(
        React.createElement(FormWithdrawnIndicator, {
          content: "\"Project channel form\" was withdrawn — a newer question replaced it before it could be answered.",
          formId: "form-xyz",
        }),
      );
    });
    const toggle = container.querySelector("[data-testid='form-withdrawn-draft-toggle']") as HTMLButtonElement;
    expect(toggle).toBeTruthy();
    await act(async () => { toggle.click(); });
    const draftBlock = container.querySelector("[data-testid='form-withdrawn-draft']");
    expect(draftBlock?.textContent).toContain("please remember this");
  });

  it("does not stash a draft when nothing was typed before supersession", async () => {
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM);
    });
    await render(PROJECT_KEY);

    const FORM_B: FormRequestPayload = {
      form_id: "form-superseding-3",
      agent_id: "agent-real",
      session_id: "session-4",
      title: "Yet another superseding form",
      fields: [],
    };
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM_B);
    });

    expect(useChatStore.getState().withdrawnFormDrafts["form-xyz"]).toBeUndefined();
  });

  it("does not stash a draft on a real submission (only on involuntary supersession)", async () => {
    await act(async () => {
      useChatStore.getState().setPendingForm(PROJECT_KEY, FORM);
    });
    await render(PROJECT_KEY);

    const input = container.querySelector("input[type='text']") as HTMLInputElement;
    await act(async () => { typeInto(input, "a real answer"); });

    const btn = container.querySelector("[data-testid='form-submit-btn']") as HTMLButtonElement;
    await act(async () => { btn.click(); });

    expect(submitFormAnswer).toHaveBeenCalledTimes(1);
    expect(useChatStore.getState().withdrawnFormDrafts["form-xyz"]).toBeUndefined();
  });
});
