// @vitest-environment jsdom
import { describe, it, expect, beforeEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

const mockGetWebhookRouteSecretStatus = vi.fn();
const mockSetWebhookRouteSecret = vi.fn();
const mockTestWebhookRoute = vi.fn();

vi.mock("../../../lib/api", () => ({
  BASE_URL: "http://localhost:3001",
  getWebhookRouteSecretStatus: (...a: unknown[]) => mockGetWebhookRouteSecretStatus(...a),
  setWebhookRouteSecret: (...a: unknown[]) => mockSetWebhookRouteSecret(...a),
  testWebhookRoute: (...a: unknown[]) => mockTestWebhookRoute(...a),
}));

import {
  buildWebhookFilterFromRows,
  buildWebhookTrigger,
  DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE,
  emptyFilterRow,
  filterRowsFromWebhookFilter,
  webhookFieldsFromTrigger,
  WebhookRouteFields,
  type WebhookRouteFieldsValue,
} from "../WebhookRouteFields";
import type { AssignmentTrigger, WebhookFilter } from "../../../types/api";

describe("buildWebhookFilterFromRows", () => {
  it("returns null for no rows or all-blank fields", () => {
    expect(buildWebhookFilterFromRows([])).toBeNull();
    expect(buildWebhookFilterFromRows([emptyFilterRow()])).toBeNull();
  });

  it("returns a bare field filter for exactly one row, parsing JSON-ish values", () => {
    const row = { ...emptyFilterRow(), field: "pull_request.number", op: "equals" as const, value: "42" };
    expect(buildWebhookFilterFromRows([row])).toEqual({ field: "pull_request.number", op: "equals", value: 42 });
  });

  it("falls back to a literal string when the value isn't valid JSON", () => {
    const row = { ...emptyFilterRow(), field: "action", op: "equals" as const, value: "opened" };
    expect(buildWebhookFilterFromRows([row])).toEqual({ field: "action", op: "equals", value: "opened" });
  });

  it("combines multiple rows with an implicit all()", () => {
    const rows = [
      { ...emptyFilterRow(), field: "action", op: "equals" as const, value: "opened" },
      { ...emptyFilterRow(), field: "sender.login", op: "not_equals" as const, value: "bot" },
    ];
    expect(buildWebhookFilterFromRows(rows)).toEqual({
      all: [
        { field: "action", op: "equals", value: "opened" },
        { field: "sender.login", op: "not_equals", value: "bot" },
      ],
    });
  });

  it("builds an `in` filter from a comma-separated list, dropping blanks", () => {
    const row = { ...emptyFilterRow(), field: "action", op: "in" as const, valuesCsv: "opened, reopened, " };
    expect(buildWebhookFilterFromRows([row])).toEqual({ field: "action", op: "in", values: ["opened", "reopened"] });
  });

  it("builds exists/missing filters with no value field", () => {
    const row = { ...emptyFilterRow(), field: "pull_request.assignee", op: "missing" as const };
    expect(buildWebhookFilterFromRows([row])).toEqual({ field: "pull_request.assignee", op: "missing" });
  });
});

describe("filterRowsFromWebhookFilter", () => {
  it("returns no rows and no override for null", () => {
    expect(filterRowsFromWebhookFilter(null)).toEqual({ rows: [], rawOverride: null });
  });

  it("round-trips a single field filter", () => {
    const filter: WebhookFilter = { field: "action", op: "equals", value: "opened" };
    const { rows, rawOverride } = filterRowsFromWebhookFilter(filter);
    expect(rawOverride).toBeNull();
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({ field: "action", op: "equals", value: "opened" });
  });

  it("round-trips a flat all() list into rows", () => {
    const filter: WebhookFilter = {
      all: [
        { field: "action", op: "equals", value: "opened" },
        { field: "pull_request.number", op: "equals", value: 42 },
      ],
    };
    const { rows, rawOverride } = filterRowsFromWebhookFilter(filter);
    expect(rawOverride).toBeNull();
    expect(rows.map((r) => [r.field, r.op, r.value])).toEqual([
      ["action", "equals", "opened"],
      ["pull_request.number", "equals", "42"],
    ]);
  });

  it("preserves an any/not/nested tree verbatim as rawOverride instead of lossy rows", () => {
    const filter: WebhookFilter = {
      any: [
        { field: "action", op: "equals", value: "opened" },
        { field: "action", op: "equals", value: "reopened" },
      ],
    };
    const { rows, rawOverride } = filterRowsFromWebhookFilter(filter);
    expect(rows).toEqual([]);
    expect(rawOverride).toEqual(filter);
  });

  it("a nested all() containing a non-field subtree is not flattened into rows", () => {
    const filter: WebhookFilter = {
      all: [{ field: "action", op: "equals", value: "opened" }, { not: { field: "sender.login", op: "missing" } }],
    };
    const { rows, rawOverride } = filterRowsFromWebhookFilter(filter);
    expect(rows).toEqual([]);
    expect(rawOverride).toEqual(filter);
  });
});

describe("buildWebhookTrigger / webhookFieldsFromTrigger", () => {
  it("sets route_name and secret_ref to the same value", () => {
    const value: WebhookRouteFieldsValue = { ...DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE, routeName: "  github-prs  " };
    const trigger = buildWebhookTrigger(value, "");
    expect(trigger).toMatchObject({ type: "Webhook", route_name: "github-prs", secret_ref: "github-prs" });
  });

  it("nulls route_name/secret_ref when the route name is blank", () => {
    const trigger = buildWebhookTrigger(DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE, "");
    expect(trigger).toMatchObject({ type: "Webhook", route_name: null, secret_ref: null, events: [], filters: null });
  });

  it("preserves an existing legacy token unchanged even though no control edits it", () => {
    const value: WebhookRouteFieldsValue = { ...DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE, legacyToken: "old-shared-secret" };
    const trigger = buildWebhookTrigger(value, "");
    expect(trigger).toMatchObject({ token: "old-shared-secret" });
  });

  it("builds prompt_template from the caller-supplied instruction text, null when blank", () => {
    const trigger = buildWebhookTrigger(DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE, "Review PR #{pull_request.number}");
    expect(trigger).toMatchObject({ prompt_template: "Review PR #{pull_request.number}" });
    expect(buildWebhookTrigger(DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE, "   ")).toMatchObject({ prompt_template: null });
  });

  it("round-trips a full trigger through webhookFieldsFromTrigger -> buildWebhookTrigger, forcing deliver back to agent", () => {
    const original: Extract<AssignmentTrigger, { type: "Webhook" }> = {
      type: "Webhook",
      token: null,
      route_name: "deploy-events",
      secret_ref: "deploy-events",
      events: ["deploy.completed"],
      filters: { field: "action", op: "equals", value: "opened" },
      prompt_template: "Handle {event.type}",
      // A pre-existing route may still carry a non-agent deliver target from
      // before the picker was removed — round-tripping through this editor
      // must normalize it back to the only delivery target it now offers.
      deliver: { type: "github_comment" },
    };
    const fields = webhookFieldsFromTrigger(original);
    expect(fields.secret).toBe(""); // write-only — never prefilled
    // The caller (AssignmentEditorModal) is the one that threads the trigger's
    // prompt_template through the unified top Instruction field, not this
    // component — simulate that here.
    const rebuilt = buildWebhookTrigger(fields, original.prompt_template ?? "");
    expect(rebuilt).toEqual({ ...original, deliver: { type: "agent" } });
  });
});

describe("WebhookRouteFields component", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    vi.clearAllMocks();
    mockGetWebhookRouteSecretStatus.mockResolvedValue({ configured: false });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  function q(sel: string) {
    return container.querySelector(sel) as HTMLElement | null;
  }

  function setInputValue(input: HTMLInputElement | HTMLTextAreaElement, value: string) {
    const proto = input instanceof HTMLTextAreaElement ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
    const nativeSetter = Object.getOwnPropertyDescriptor(proto, "value")!.set!;
    nativeSetter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  }

  function click(el: HTMLElement) {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  }

  async function renderWith(
    value: WebhookRouteFieldsValue,
    onChange: (v: WebhookRouteFieldsValue) => void,
    promptTemplate = "",
  ) {
    await act(async () => {
      root.render(React.createElement(WebhookRouteFields, { value, onChange, promptTemplate }));
    });
  }

  it("shows the payload URL derived from BASE_URL + route name, and copies it", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });

    await renderWith({ ...DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE, routeName: "github-prs" }, () => {});
    await act(async () => {});

    expect(q("[data-testid='assignment-editor-webhook-payload-url']")?.textContent).toBe(
      "http://localhost:3001/webhooks/github-prs",
    );

    await act(async () => {
      click(q("[data-testid='assignment-editor-webhook-copy-url']")!);
    });
    expect(writeText).toHaveBeenCalledWith("http://localhost:3001/webhooks/github-prs");
  });

  it("sends a test webhook with the current draft config and renders the result", async () => {
    mockTestWebhookRoute.mockResolvedValue({
      matched: true,
      deliver: { type: "agent" },
      would_start_agent: true,
      rendered_instruction: "Review PR #42: Fix the flaky retry loop",
    });

    const value: WebhookRouteFieldsValue = {
      ...DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE,
      events: ["pull_request"],
    };
    await renderWith(value, () => {}, "Review PR #{pull_request.number}: {pull_request.title}");
    await act(async () => {});

    await act(async () => {
      click(q("[data-testid='assignment-editor-webhook-test-send']")!);
    });
    await act(async () => {});

    expect(mockTestWebhookRoute).toHaveBeenCalledTimes(1);
    const [req] = mockTestWebhookRoute.mock.calls[0];
    expect(req).toMatchObject({
      events: ["pull_request"],
      filters: null,
      prompt_template: "Review PR #{pull_request.number}: {pull_request.title}",
      deliver: { type: "agent" },
      event_type: "example.event",
    });
    expect(req.payload).toBeTruthy();

    expect(q("[data-testid='assignment-editor-webhook-test-result']")?.textContent).toContain("Matched");
    expect(q("[data-testid='assignment-editor-webhook-test-result']")?.textContent).toContain(
      "Review PR #42: Fix the flaky retry loop",
    );
  });

  it("shows an inline error instead of calling the API when the sample payload isn't valid JSON", async () => {
    await renderWith(DEFAULT_WEBHOOK_ROUTE_FIELDS_VALUE, () => {});
    await act(async () => {});

    await act(async () => {
      setInputValue(q("[data-testid='assignment-editor-webhook-test-payload']") as HTMLTextAreaElement, "{not json");
    });
    await act(async () => {
      click(q("[data-testid='assignment-editor-webhook-test-send']")!);
    });

    expect(mockTestWebhookRoute).not.toHaveBeenCalled();
    expect(q("[data-testid='assignment-editor-webhook-test-error']")?.textContent).toMatch(/valid JSON/);
  });
});
