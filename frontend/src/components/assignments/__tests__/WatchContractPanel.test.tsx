// @vitest-environment jsdom
import { describe, it, expect, afterEach } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import type { AssignmentWatchHealth, WatchContract, WatchContractStatus } from "../../../types/api";
import { AssignmentWatchHealthBadge, WatchContractPanel } from "../WatchContractPanel";

/** Shorthand for the common case across this file's fixtures: a live,
 *  bound contract with no repair history to report. */
const BOUND_STATUS: WatchContractStatus = { state: "bound", bound_after_repairs: null };

const CONTRACT: WatchContract = {
  contract_version: 3,
  authored_at: "2026-07-27T09:00:00Z",
  authored_by_run: "run-123",
  source: { kind: "notion_database", ref: "abc-123-def" },
  identity: {
    strategy: "native_id",
    source_field: "unique_identifier",
    format: "^[0-9a-f-]{36}$",
    fields: [],
    rationale: "Source exposes a stable per-row key; verified unchanged across the stability probe.",
  },
  change: {
    material_fields: ["tag", "status"],
    version_hint_field: "last_edited_time",
  },
  predicate: {
    natural_language: "tag contains 'Very Important'",
    fields: ["tag"],
    // The typed predicate the runtime evaluates. The readable expression is
    // NOT carried here — the backend renders it onto the response-only
    // `watch_health.predicate_expr` instead, so that the fingerprinted
    // contract gains no new field.
    predicate: { ContainsCi: { path: "tag", literal: "Very Important" } },
  },
  mode: "predicate_transition",
  fields: {
    client_name: { type: "string", required: true, normalize: ["trim", "nfc", "casefold"] },
    client_email: { type: "string", required: true, normalize: ["trim", "casefold"] },
    tag: { type: "string", required: false, normalize: ["trim"] },
    last_edited_time: { type: "datetime", required: false, normalize: [] },
  },
};

describe("WatchContractPanel", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function render(
    contract: WatchContract | null | undefined,
    watchHealth?: AssignmentWatchHealth | null,
    extraction?: { extractionTool?: string | null; extractionArgs?: unknown },
  ) {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root.render(
        React.createElement(WatchContractPanel, {
          contract,
          watchHealth,
          extractionTool: extraction?.extractionTool,
          extractionArgs: extraction?.extractionArgs,
        }),
      );
    });
  }

  async function renderBadge(health: AssignmentWatchHealth | null | undefined) {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    await act(async () => {
      root.render(React.createElement(AssignmentWatchHealthBadge, { health }));
    });
  }

  it("renders the identity rationale and the fire predicate's natural language for a populated contract", async () => {
    await render(CONTRACT);

    const panel = container.querySelector("[data-testid='watch-contract-panel']");
    expect(panel).toBeTruthy();
    expect(panel?.textContent).toContain(
      "Source exposes a stable per-row key; verified unchanged across the stability probe.",
    );
    expect(panel?.textContent).toContain("tag contains 'Very Important'");
  });

  it("renders the server-rendered raw expression behind the disclosure when one is supplied", async () => {
    await render(CONTRACT, {
      has_evaluated: true,
      tier: "deterministic",
      extraction_path: "deterministic",
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      predicate_expr: "contains(tag, 'Very Important')",
      contract_status: BOUND_STATUS,
    });

    const expr = container.querySelector("[data-testid='watch-contract-expr']");
    expect(expr).toBeTruthy();
    expect(expr?.textContent).toBe("contains(tag, 'Very Important')");
    expect(container.querySelector("[data-testid='watch-contract-expr-unavailable']")).toBeNull();
  });

  // Regression: the disclosure used to read `contract.predicate.expr`, a field
  // the backend cannot emit (its `PredicateSpec` has no such field), so it
  // rendered a silently empty box. Absent must read as an explicit message.
  it.each([
    ["null", null],
    ["undefined", undefined],
  ])("explains rather than rendering an empty box when predicate_expr is %s", async (_label, value) => {
    await render(CONTRACT, {
      has_evaluated: true,
      tier: "deterministic",
      extraction_path: "deterministic",
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      predicate_expr: value,
      contract_status: BOUND_STATUS,
    });

    expect(container.querySelector("[data-testid='watch-contract-expr']")).toBeNull();
    const fallback = container.querySelector("[data-testid='watch-contract-expr-unavailable']");
    expect(fallback).toBeTruthy();
    expect(fallback?.textContent).toContain("No expression available");
    expect(fallback?.textContent?.trim()).not.toBe("");
  });

  it("explains rather than rendering an empty box when no watch health is supplied at all", async () => {
    await render(CONTRACT);

    expect(container.querySelector("[data-testid='watch-contract-expr']")).toBeNull();
    expect(container.querySelector("[data-testid='watch-contract-expr-unavailable']")?.textContent).toContain(
      "No expression available",
    );
  });

  it("shows a 'still learning' placeholder instead of an empty box when contract is undefined", async () => {
    await render(undefined);

    expect(container.querySelector("[data-testid='watch-contract-panel']")).toBeNull();
    const empty = container.querySelector("[data-testid='watch-contract-empty']");
    expect(empty).toBeTruthy();
    expect(empty?.textContent).toContain("Learning this source — the contract will appear after the first poll.");
  });

  it("shows the same placeholder for a null contract, not a crash", async () => {
    await render(null);

    const empty = container.querySelector("[data-testid='watch-contract-empty']");
    expect(empty).toBeTruthy();
  });

  it("never renders an input, textarea, button, select, or contenteditable element — this is a read-only surface", async () => {
    await render(CONTRACT);
    expect(container.querySelectorAll("input, textarea, button, select, [contenteditable]").length).toBe(0);

    await render(undefined);
    expect(container.querySelectorAll("input, textarea, button, select, [contenteditable]").length).toBe(0);
  });

  describe("AssignmentWatchHealthBadge", () => {
    const DETERMINISTIC: AssignmentWatchHealth = {
      has_evaluated: true,
      tier: "deterministic",
      extraction_path: "deterministic",
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      contract_status: BOUND_STATUS,
    };
    const PROBABILISTIC: AssignmentWatchHealth = {
      has_evaluated: true,
      tier: "probabilistic",
      extraction_path: "probabilistic",
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      contract_status: BOUND_STATUS,
    };
    const CHANGE_DETECTION_ONLY: AssignmentWatchHealth = {
      has_evaluated: true,
      tier: "change_detection_only",
      extraction_path: "llm",
      degraded_reason:
        "The connector has not declared an output schema for this tool, and the extraction plan has no selector for individual items, so it can only hash the whole response — it can tell you something changed, not what.",
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      contract_status: BOUND_STATUS,
    };
    // A poll whose extraction plan failed structurally and fell back to the
    // model for that poll — the engine never claims a tier it didn't
    // actually run, so `tier` reads `null` here, exactly like an unbound
    // watch. Only `extraction_plan_degraded`/`_reason` tell the two apart.
    const EXTRACTION_PLAN_DEGRADED: AssignmentWatchHealth = {
      has_evaluated: true,
      tier: null,
      extraction_path: "llm",
      degraded_reason: null,
      extraction_plan_degraded: true,
      extraction_plan_degraded_reason: "path \"items\" did not resolve against the tool's last response",
      contract_status: BOUND_STATUS,
    };
    const NEVER_EVALUATED: AssignmentWatchHealth = {
      has_evaluated: false,
      tier: null,
      extraction_path: null,
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      contract_status: { state: "not_yet_attempted" },
    };

    function badgeTier(): string | null {
      return container.querySelector("[data-testid='watch-health-badge']")?.getAttribute("data-watch-health-tier") ?? null;
    }

    function badgeLabel(): string | null {
      return container.querySelector("[data-testid='watch-health-label']")?.textContent ?? null;
    }

    it("renders Deterministic as its own distinct, healthy-looking state", async () => {
      await renderBadge(DETERMINISTIC);
      expect(badgeTier()).toBe("deterministic");
      expect(badgeLabel()).toBe("Deterministic");
    });

    it("renders Probabilistic as its own distinct, degraded-but-usable state", async () => {
      await renderBadge(PROBABILISTIC);
      expect(badgeTier()).toBe("probabilistic");
      expect(badgeLabel()).toBe("Probabilistic");
    });

    it("renders ChangeDetectionOnly as its own distinct, visibly unhealthy state and shows its concrete reason", async () => {
      await renderBadge(CHANGE_DETECTION_ONLY);
      expect(badgeTier()).toBe("change_detection_only");
      expect(badgeLabel()).toBe("Change detection only");

      const detail = container.querySelector("[data-testid='watch-health-detail']")?.textContent ?? "";
      expect(detail).toBe(CHANGE_DETECTION_ONLY.degraded_reason);
      // Never a generic placeholder — the actual engine-supplied cause must show.
      expect(detail.toLowerCase()).not.toBe("unavailable");
      expect(detail.length).toBeGreaterThan(0);
    });

    it("renders a poll that fell back to the model as a distinct, unhealthy state carrying the persisted reason — never as Deterministic", async () => {
      await renderBadge(EXTRACTION_PLAN_DEGRADED);
      expect(badgeTier()).toBe("degraded");
      expect(badgeLabel()).not.toBe("Deterministic");

      const detail = container.querySelector("[data-testid='watch-health-detail']")?.textContent ?? "";
      // Renders the backend-persisted reason verbatim — never invented copy.
      expect(detail).toBe(EXTRACTION_PLAN_DEGRADED.extraction_plan_degraded_reason);
    });

    it("leaves the badge unchanged from today when extraction_plan_degraded is false", async () => {
      await renderBadge(DETERMINISTIC);
      expect(badgeTier()).toBe("deterministic");
      expect(badgeLabel()).toBe("Deterministic");

      const detail = container.querySelector("[data-testid='watch-health-detail']")?.textContent ?? "";
      expect(detail).toBe("Extracting exact items with no model involved. Healthy.");
    });

    it("all three tiers render with mutually distinct labels and data-watch-health-tier values", async () => {
      await renderBadge(DETERMINISTIC);
      const deterministic = { tier: badgeTier(), label: badgeLabel() };

      await renderBadge(PROBABILISTIC);
      const probabilistic = { tier: badgeTier(), label: badgeLabel() };

      await renderBadge(CHANGE_DETECTION_ONLY);
      const changeDetectionOnly = { tier: badgeTier(), label: badgeLabel() };

      const tiers = [deterministic.tier, probabilistic.tier, changeDetectionOnly.tier];
      const labels = [deterministic.label, probabilistic.label, changeDetectionOnly.label];
      expect(new Set(tiers).size).toBe(3);
      expect(new Set(labels).size).toBe(3);
    });

    it("renders a never-evaluated watch distinctly from a healthy (deterministic) one — 'unknown' must not look like 'healthy'", async () => {
      await renderBadge(NEVER_EVALUATED);
      const neverEvaluatedTier = badgeTier();
      const neverEvaluatedLabel = badgeLabel();

      await renderBadge(DETERMINISTIC);
      const deterministicTier = badgeTier();
      const deterministicLabel = badgeLabel();

      expect(neverEvaluatedTier).toBe("unevaluated");
      expect(neverEvaluatedTier).not.toBe(deterministicTier);
      expect(neverEvaluatedLabel).not.toBe(deterministicLabel);
    });

    it("treats a missing/undefined health prop the same as never-evaluated, not a crash", async () => {
      await renderBadge(undefined);
      expect(badgeTier()).toBe("unevaluated");
    });

    // Regression coverage for the panel self-contradiction bug: a
    // model-assisted watch always reports `tier: null` (its detector never
    // computes an `ExtractionPlan`), but that is a known, expected state —
    // the model-assisted extraction disclosure renders directly below this
    // same badge on the panel, so the badge must not claim "no tier to
    // report yet" right above copy that names the tier as model-extracted.
    it("renders a model-assisted watch (tier=null, extraction_health='model_assisted') distinctly from 'no tier recorded' — must not contradict the disclosure below it", async () => {
      const MODEL_ASSISTED_NO_TIER: AssignmentWatchHealth = {
        has_evaluated: true,
        tier: null,
        extraction_path: "llm",
        degraded_reason: null,
        extraction_plan_degraded: false,
        extraction_plan_degraded_reason: null,
        extraction_health: "model_assisted",
        contract_status: BOUND_STATUS,
      };

      await renderBadge(MODEL_ASSISTED_NO_TIER);
      const label = badgeLabel();
      const detail = container.querySelector("[data-testid='watch-health-detail']")?.textContent ?? "";

      expect(label).not.toBe("No tier recorded");
      expect(detail).not.toContain("no extraction tier to report yet");
      // Never the error tone — a model-assisted watch is expected, not broken.
      expect(container.querySelector("[data-testid='watch-health-badge']")?.className).not.toContain("--error");
    });

    // Backward compatibility for older persisted assignments/responses
    // captured before `extraction_health` existed on the wire: with no
    // signal to distinguish "model-assisted" from "genuinely unknown," the
    // badge must keep the original neutral "no tier recorded" copy rather
    // than assume either health state.
    it("still renders the neutral 'no tier recorded' copy when tier=null and extraction_health is absent or pending", async () => {
      const NO_EXTRACTION_HEALTH_FIELD: AssignmentWatchHealth = {
        has_evaluated: true,
        tier: null,
        extraction_path: null,
        degraded_reason: null,
        extraction_plan_degraded: false,
        extraction_plan_degraded_reason: null,
        contract_status: BOUND_STATUS,
      };
      await renderBadge(NO_EXTRACTION_HEALTH_FIELD);
      expect(badgeLabel()).toBe("No tier recorded");
      expect(container.querySelector("[data-testid='watch-health-detail']")?.textContent).toBe(
        "This watch has evaluated, but has no extraction tier to report yet.",
      );

      const PENDING_NO_TIER: AssignmentWatchHealth = {
        ...NO_EXTRACTION_HEALTH_FIELD,
        extraction_health: "pending",
      };
      await renderBadge(PENDING_NO_TIER);
      expect(badgeLabel()).toBe("No tier recorded");
      expect(container.querySelector("[data-testid='watch-health-detail']")?.textContent).toBe(
        "This watch has evaluated, but has no extraction tier to report yet.",
      );
    });
  });

  it("renders the health badge inline when watchHealth is supplied, for both a populated and an empty contract", async () => {
    const health: AssignmentWatchHealth = {
      has_evaluated: true,
      tier: "deterministic",
      extraction_path: "deterministic",
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      contract_status: BOUND_STATUS,
    };

    await render(CONTRACT, health);
    expect(container.querySelector("[data-testid='watch-contract-panel'] [data-testid='watch-health-badge']")).toBeTruthy();

    await render(null, health);
    expect(container.querySelector("[data-testid='watch-contract-empty'] [data-testid='watch-health-badge']")).toBeTruthy();
  });

  it("omits the health badge entirely when watchHealth is not supplied at all", async () => {
    await render(CONTRACT);
    expect(container.querySelector("[data-testid='watch-health-badge']")).toBeNull();
  });

  describe("frozen contract disclosure", () => {
    const DETERMINISTIC_HEALTH: AssignmentWatchHealth = {
      has_evaluated: true,
      tier: "deterministic",
      extraction_path: "deterministic",
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      extraction_health: "deterministic",
      extraction_health_reason: null,
      model_calls_today: 0,
      last_extraction_path: "deterministic",
      contract_status: BOUND_STATUS,
    };

    it("renders the tool name and pretty-printed arguments when a tool is frozen and extraction is deterministic", async () => {
      await render(CONTRACT, DETERMINISTIC_HEALTH, {
        extractionTool: "list_finance_emails",
        extractionArgs: { folder: "finance", unread_only: true },
      });

      const block = container.querySelector("[data-testid='watch-frozen-contract']");
      expect(block).toBeTruthy();
      expect(container.querySelector("[data-testid='watch-frozen-contract-tool']")?.textContent).toBe(
        "list_finance_emails",
      );
      const argsText = container.querySelector("[data-testid='watch-frozen-contract-args']")?.textContent ?? "";
      expect(JSON.parse(argsText)).toEqual({ folder: "finance", unread_only: true });
      expect(block?.textContent).toContain("no model reviews this before it runs");
    });

    it("renders no frozen-contract block when no tool is frozen", async () => {
      await render(CONTRACT, DETERMINISTIC_HEALTH);
      expect(container.querySelector("[data-testid='watch-frozen-contract']")).toBeNull();
    });

    it("renders no frozen-contract block when extractionTool is explicitly null", async () => {
      await render(CONTRACT, DETERMINISTIC_HEALTH, { extractionTool: null, extractionArgs: null });
      expect(container.querySelector("[data-testid='watch-frozen-contract']")).toBeNull();
    });

    // Regression coverage for the provenance honesty gap: `extraction_health:
    // "deterministic"` alone can't tell a server-declared-schema plan apart
    // from a parsed-text one — both render the same frozen-contract block and
    // the same "no model reviews this" claim (a true cost claim either way).
    // `extraction_provenance` is what the sub-line below reads to tell them
    // apart on DRIFT RISK instead.
    describe("extraction provenance sub-line", () => {
      const FROZEN_TOOL = { extractionTool: "list_finance_emails", extractionArgs: { folder: "finance" } };

      it("renders a 'declared schema' sub-line, distinct from the parsed-text wording, when provenance is declared_schema", async () => {
        await render(CONTRACT, { ...DETERMINISTIC_HEALTH, extraction_provenance: "declared_schema" }, FROZEN_TOOL);

        const sub = container.querySelector("[data-testid='watch-extraction-provenance']");
        expect(sub).toBeTruthy();
        expect(sub?.textContent).toContain("declared");
        expect(sub?.textContent).toContain("schema");
        expect(sub?.textContent).not.toContain("parsing");
      });

      it("renders a distinct 'parsed from text' sub-line when provenance is parsed_from_text", async () => {
        await render(CONTRACT, { ...DETERMINISTIC_HEALTH, extraction_provenance: "parsed_from_text" }, FROZEN_TOOL);

        const sub = container.querySelector("[data-testid='watch-extraction-provenance']");
        expect(sub).toBeTruthy();
        expect(sub?.textContent).toContain("parsing");
        expect(sub?.textContent).not.toContain("contractual promise");
      });

      it("declared_schema and parsed_from_text never render identical copy", async () => {
        await render(CONTRACT, { ...DETERMINISTIC_HEALTH, extraction_provenance: "declared_schema" }, FROZEN_TOOL);
        const declared = container.querySelector("[data-testid='watch-extraction-provenance']")?.textContent;

        await render(CONTRACT, { ...DETERMINISTIC_HEALTH, extraction_provenance: "parsed_from_text" }, FROZEN_TOOL);
        const parsed = container.querySelector("[data-testid='watch-extraction-provenance']")?.textContent;

        expect(declared).toBeTruthy();
        expect(parsed).toBeTruthy();
        expect(declared).not.toBe(parsed);
      });

      it("renders a neutral sub-line — never a false 'declared schema' guarantee — when extraction_provenance is absent", async () => {
        await render(CONTRACT, DETERMINISTIC_HEALTH, FROZEN_TOOL);

        const sub = container.querySelector("[data-testid='watch-extraction-provenance']");
        expect(sub).toBeTruthy();
        expect(sub?.textContent).toContain("not recorded");
        expect(sub?.textContent).not.toContain("contractual promise");
      });

      it("renders the same neutral sub-line when extraction_provenance is explicitly null", async () => {
        await render(CONTRACT, { ...DETERMINISTIC_HEALTH, extraction_provenance: null }, FROZEN_TOOL);

        const sub = container.querySelector("[data-testid='watch-extraction-provenance']");
        expect(sub?.textContent).toContain("not recorded");
      });
    });
  });

  // Regression: this disclosure used to gate solely on `extractionTool`'s
  // truthiness, so a watch with a frozen tool name but no extraction plan —
  // meaning a model actually ran on every poll — still rendered "no model
  // reviews this before it runs." These tests key every rendering off
  // `extraction_health` instead, the only server-derived signal that can
  // tell the four states apart.
  describe("extraction health disclosure (deterministic / model_assisted / degraded / pending)", () => {
    const BASE_HEALTH: Pick<
      AssignmentWatchHealth,
      "has_evaluated" | "tier" | "extraction_path" | "degraded_reason" | "extraction_plan_degraded" | "extraction_plan_degraded_reason"
    > = {
      has_evaluated: true,
      tier: null,
      extraction_path: "llm",
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
    };

    const MODEL_ASSISTED_REASON = "This source returns no structured content, so a model reads it every poll.";
    const DEGRADED_REASON = "The extraction plan failed on the last poll and this watch fell back to the model.";

    const DETERMINISTIC_HEALTH: AssignmentWatchHealth = {
      ...BASE_HEALTH,
      tier: "deterministic",
      extraction_path: "deterministic",
      extraction_health: "deterministic",
      extraction_health_reason: null,
      model_calls_today: 0,
      last_extraction_path: "deterministic",
      contract_status: BOUND_STATUS,
    };

    const MODEL_ASSISTED_HEALTH: AssignmentWatchHealth = {
      ...BASE_HEALTH,
      extraction_health: "model_assisted",
      extraction_health_reason: MODEL_ASSISTED_REASON,
      model_calls_today: 3,
      last_extraction_path: "llm",
      contract_status: BOUND_STATUS,
    };

    const DEGRADED_HEALTH: AssignmentWatchHealth = {
      ...BASE_HEALTH,
      extraction_plan_degraded: true,
      extraction_plan_degraded_reason: "path \"items\" did not resolve against the tool's last response",
      extraction_health: "degraded",
      extraction_health_reason: DEGRADED_REASON,
      model_calls_today: 1,
      last_extraction_path: "llm",
      contract_status: BOUND_STATUS,
    };

    const PENDING_HEALTH: AssignmentWatchHealth = {
      has_evaluated: false,
      tier: null,
      extraction_path: null,
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      extraction_health: "pending",
      extraction_health_reason: null,
      model_calls_today: 0,
      last_extraction_path: null,
      contract_status: BOUND_STATUS,
    };

    const NO_MODEL_REVIEW_CLAIM = "no model reviews this before it runs";
    const FROZEN_TOOL = { extractionTool: "list_finance_emails", extractionArgs: { folder: "finance" } };

    it("deterministic: keeps the frozen-tool copy, and is the only state that claims no model reviews it", async () => {
      await render(CONTRACT, DETERMINISTIC_HEALTH, FROZEN_TOOL);

      const panel = container.querySelector("[data-testid='watch-contract-panel']");
      expect(container.querySelector("[data-testid='watch-frozen-contract']")).toBeTruthy();
      expect(panel?.textContent).toContain(NO_MODEL_REVIEW_CLAIM);
    });

    it("model_assisted: shows the frozen target as frozen, states a model extracts every poll, and renders the real model-call count plus the reason", async () => {
      await render(CONTRACT, MODEL_ASSISTED_HEALTH, FROZEN_TOOL);

      const panel = container.querySelector("[data-testid='watch-contract-panel']");
      const disclosure = container.querySelector("[data-testid='watch-model-assisted-disclosure']");
      expect(disclosure).toBeTruthy();
      expect(container.querySelector("[data-testid='watch-model-assisted-tool']")?.textContent).toBe(
        "list_finance_emails",
      );
      expect(disclosure?.textContent).toContain("3 model calls today");
      expect(disclosure?.textContent).toContain(MODEL_ASSISTED_REASON);
      expect(panel?.textContent).not.toContain(NO_MODEL_REVIEW_CLAIM);
    });

    it("degraded: shows a Degraded treatment with the reason rendered verbatim, and never the no-model-review claim", async () => {
      await render(CONTRACT, DEGRADED_HEALTH, FROZEN_TOOL);

      const panel = container.querySelector("[data-testid='watch-contract-panel']");
      const disclosure = container.querySelector("[data-testid='watch-extraction-degraded']");
      expect(disclosure).toBeTruthy();
      expect(disclosure?.textContent).toContain("Degraded");
      expect(disclosure?.textContent).toContain(DEGRADED_REASON);
      expect(panel?.textContent).not.toContain(NO_MODEL_REVIEW_CLAIM);
    });

    it("pending: shows a neutral not-yet-evaluated disclosure and claims no guarantee either way", async () => {
      await render(CONTRACT, PENDING_HEALTH, FROZEN_TOOL);

      const panel = container.querySelector("[data-testid='watch-contract-panel']");
      const disclosure = container.querySelector("[data-testid='watch-extraction-pending']");
      expect(disclosure).toBeTruthy();
      expect(panel?.textContent).not.toContain(NO_MODEL_REVIEW_CLAIM);
      expect(panel?.textContent).not.toContain("model-extracted");
    });

    it("a truthy frozen tool name alone never produces the no-model-review claim outside the deterministic state", async () => {
      await render(CONTRACT, MODEL_ASSISTED_HEALTH, FROZEN_TOOL);
      expect(container.querySelector("[data-testid='watch-contract-panel']")?.textContent).not.toContain(
        NO_MODEL_REVIEW_CLAIM,
      );

      await render(CONTRACT, DEGRADED_HEALTH, FROZEN_TOOL);
      expect(container.querySelector("[data-testid='watch-contract-panel']")?.textContent).not.toContain(
        NO_MODEL_REVIEW_CLAIM,
      );

      await render(CONTRACT, PENDING_HEALTH, FROZEN_TOOL);
      expect(container.querySelector("[data-testid='watch-contract-panel']")?.textContent).not.toContain(
        NO_MODEL_REVIEW_CLAIM,
      );
    });

    it("falls back to the neutral pending disclosure when watchHealth/extraction_health is entirely absent, never claiming the deterministic guarantee", async () => {
      await render(CONTRACT, undefined, FROZEN_TOOL);

      expect(container.querySelector("[data-testid='watch-extraction-pending']")).toBeTruthy();
      expect(container.querySelector("[data-testid='watch-contract-panel']")?.textContent).not.toContain(
        NO_MODEL_REVIEW_CLAIM,
      );
    });
  });

  describe("identity probe inconclusive disclosure", () => {
    const INCONCLUSIVE_REASON =
      "Only one poll has completed since this contract was authored, so the stability probe hasn't had a second observation to compare against yet.";

    const BASE_HEALTH: AssignmentWatchHealth = {
      has_evaluated: true,
      tier: "deterministic",
      extraction_path: "deterministic",
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      contract_status: BOUND_STATUS,
    };

    it("renders the caveat and the backend's reason verbatim when identity_probe_inconclusive is true", async () => {
      await render(CONTRACT, {
        ...BASE_HEALTH,
        identity_probe_inconclusive: true,
        identity_probe_inconclusive_reason: INCONCLUSIVE_REASON,
      });

      const disclosure = container.querySelector("[data-testid='watch-identity-probe-inconclusive']");
      expect(disclosure).toBeTruthy();
      expect(container.querySelector("[data-testid='watch-identity-probe-inconclusive-reason']")?.textContent).toBe(
        INCONCLUSIVE_REASON,
      );
    });

    it("renders no caveat when identity_probe_inconclusive is false", async () => {
      await render(CONTRACT, {
        ...BASE_HEALTH,
        identity_probe_inconclusive: false,
        identity_probe_inconclusive_reason: null,
      });

      expect(container.querySelector("[data-testid='watch-identity-probe-inconclusive']")).toBeNull();
    });

    it("renders no caveat when the field is absent entirely (a response captured before it existed)", async () => {
      await render(CONTRACT, BASE_HEALTH);

      expect(container.querySelector("[data-testid='watch-identity-probe-inconclusive']")).toBeNull();
    });

    it("coexists with the identity rationale's own 'verified' claim instead of being swallowed or contradicted by it", async () => {
      await render(CONTRACT, {
        ...BASE_HEALTH,
        identity_probe_inconclusive: true,
        identity_probe_inconclusive_reason: INCONCLUSIVE_REASON,
      });

      const panel = container.querySelector("[data-testid='watch-contract-panel']");
      // CONTRACT's own rationale text asserts the key was "verified unchanged
      // across the stability probe" — the caveat must sit alongside that
      // claim, not replace or hide it.
      expect(panel?.textContent).toContain("verified unchanged across the stability probe");
      expect(container.querySelector("[data-testid='watch-identity-probe-inconclusive']")).toBeTruthy();
      expect(panel?.textContent).toContain(INCONCLUSIVE_REASON);
    });
  });

  // Work items 1/1b/2's own acceptance surface: the three contract-authoring
  // states (`not_yet_attempted` / `authoring_rejected` / `bound`) must be
  // MUTUALLY EXCLUSIVE — never two rendered on the same panel at once — and
  // a `bound` contract that only converged after a rejection must read
  // differently from a rejection that's still outstanding.
  describe("WatchContractStatus (three mutually exclusive states)", () => {
    const NOT_YET_ATTEMPTED_HEALTH: AssignmentWatchHealth = {
      has_evaluated: false,
      tier: null,
      extraction_path: null,
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      contract_status: { state: "not_yet_attempted" },
    };

    const AUTHORING_REJECTED_HEALTH: AssignmentWatchHealth = {
      has_evaluated: true,
      tier: null,
      extraction_path: "llm",
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      extraction_health: "model_assisted",
      extraction_health_reason:
        "No deterministic extraction plan is bound for this watch yet, so a model reads its output on every poll.",
      contract_status: {
        state: "authoring_rejected",
        attempts: 2,
        ceiling_hit: false,
        last_rejection_reason: "proposal failed validation: no material fields declared",
      },
    };

    const AUTHORING_REJECTED_AT_CEILING_HEALTH: AssignmentWatchHealth = {
      ...AUTHORING_REJECTED_HEALTH,
      contract_status: {
        state: "authoring_rejected",
        attempts: 5,
        ceiling_hit: true,
        last_rejection_reason: "proposal failed validation: no material fields declared",
      },
    };

    const BOUND_CLEANLY_HEALTH: AssignmentWatchHealth = {
      has_evaluated: true,
      tier: "deterministic",
      extraction_path: "deterministic",
      degraded_reason: null,
      extraction_plan_degraded: false,
      extraction_plan_degraded_reason: null,
      contract_status: { state: "bound", bound_after_repairs: null },
    };

    const BOUND_AFTER_REPAIR_HEALTH: AssignmentWatchHealth = {
      ...BOUND_CLEANLY_HEALTH,
      contract_status: { state: "bound", bound_after_repairs: 2 },
    };

    const LEARNING_TEXT = "Learning this source — the contract will appear after the first poll.";

    it("not_yet_attempted: renders only the learning placeholder — never the rejection or bound-contract testids", async () => {
      await render(null, NOT_YET_ATTEMPTED_HEALTH);

      expect(container.querySelector("[data-testid='watch-contract-empty']")).toBeTruthy();
      expect(container.querySelector("[data-testid='watch-contract-authoring-rejected']")).toBeNull();
      expect(container.querySelector("[data-testid='watch-contract-panel']")).toBeNull();
      expect(container.textContent).toContain(LEARNING_TEXT);
    });

    it("authoring_rejected: renders only the rejection detail — never the learning placeholder or the bound-contract testids", async () => {
      await render(null, AUTHORING_REJECTED_HEALTH);

      expect(container.querySelector("[data-testid='watch-contract-authoring-rejected']")).toBeTruthy();
      expect(container.querySelector("[data-testid='watch-contract-empty']")).toBeNull();
      expect(container.querySelector("[data-testid='watch-contract-panel']")).toBeNull();
      expect(container.textContent).not.toContain(LEARNING_TEXT);

      const detail =
        container.querySelector("[data-testid='watch-contract-authoring-rejected-detail']")?.textContent ?? "";
      expect(detail).toContain("Attempt 2 was rejected");
      expect(
        container.querySelector("[data-testid='watch-contract-authoring-rejected-reason']")?.textContent,
      ).toContain("proposal failed validation: no material fields declared");
    });

    // The exact contradiction this feature fixes: before this change, a
    // rejected-but-not-bound poll rendered BOTH "Learning this source" (the
    // `!contract` placeholder, gated only on contract presence) AND the
    // health badge's "Model-assisted — no fixed tier" copy (gated only on
    // `extraction_health`, which never looked at contract-bound-ness) at
    // once — one claiming nothing has been tried yet, the other claiming a
    // permanent degraded state. Both are now driven by the SAME
    // `contract_status` field, so they can no longer disagree.
    it("never renders the learning placeholder alongside the old model-assisted badge copy for a rejected-but-unbound poll", async () => {
      await render(null, AUTHORING_REJECTED_HEALTH);
      const text = container.textContent ?? "";

      expect(text).not.toContain(LEARNING_TEXT);
      expect(text).not.toContain("Model-assisted — no fixed tier");
    });

    it("authoring_rejected at the ceiling: says the ceiling was hit and how many attempts, distinctly from below-ceiling copy", async () => {
      await render(null, AUTHORING_REJECTED_AT_CEILING_HEALTH);

      const detail =
        container.querySelector("[data-testid='watch-contract-authoring-rejected-detail']")?.textContent ?? "";
      expect(detail).toContain("Rejected 5 times in a row");
      expect(detail).toContain("stopped retrying");
      expect(detail).not.toContain("Attempt 5 was rejected — retrying automatically");

      expect(container.querySelector("[data-testid='watch-health-label']")?.textContent).toBe(
        "Contract proposal rejected — stopped retrying",
      );
    });

    it("bound (no repairs): renders the full contract panel with no rejection copy and no convergence banner", async () => {
      await render(CONTRACT, BOUND_CLEANLY_HEALTH);

      expect(container.querySelector("[data-testid='watch-contract-panel']")).toBeTruthy();
      expect(container.querySelector("[data-testid='watch-contract-empty']")).toBeNull();
      expect(container.querySelector("[data-testid='watch-contract-authoring-rejected']")).toBeNull();
      expect(container.querySelector("[data-testid='watch-contract-repaired']")).toBeNull();
      expect(container.textContent).not.toContain(LEARNING_TEXT);
    });

    // The convergence half of the fix (work item 1b): a contract that only
    // bound after some rejected proposals must say so explicitly, and that
    // notice must read differently from an outstanding rejection — never
    // silence, and never the same copy as a rejection that's still live.
    it("bound after repair: renders a convergence banner distinct from an outstanding rejection, and no rejection copy survives", async () => {
      await render(CONTRACT, BOUND_AFTER_REPAIR_HEALTH);

      const banner = container.querySelector("[data-testid='watch-contract-repaired']");
      expect(banner).toBeTruthy();
      expect(banner?.textContent).toContain("bound after repairing 2 rejected proposals");

      expect(container.querySelector("[data-testid='watch-contract-authoring-rejected']")).toBeNull();
      expect(container.textContent).not.toContain(LEARNING_TEXT);
      expect(container.textContent).not.toContain("Rejected 2 times in a row");
      expect(container.textContent).not.toContain("was rejected — retrying automatically");
    });

    it("the outstanding-rejection string and the repaired-convergence string are never identical", async () => {
      await render(null, AUTHORING_REJECTED_HEALTH);
      const outstanding = container.querySelector(
        "[data-testid='watch-contract-authoring-rejected-detail']",
      )?.textContent;

      await render(CONTRACT, BOUND_AFTER_REPAIR_HEALTH);
      const converged = container.querySelector("[data-testid='watch-contract-repaired']")?.textContent;

      expect(outstanding).toBeTruthy();
      expect(converged).toBeTruthy();
      expect(outstanding).not.toBe(converged);
    });

    it("exactly one of the three top-level state testids renders for each state — never zero, never two", async () => {
      const cases: Array<[WatchContract | null, AssignmentWatchHealth]> = [
        [null, NOT_YET_ATTEMPTED_HEALTH],
        [null, AUTHORING_REJECTED_HEALTH],
        [null, AUTHORING_REJECTED_AT_CEILING_HEALTH],
        [CONTRACT, BOUND_CLEANLY_HEALTH],
        [CONTRACT, BOUND_AFTER_REPAIR_HEALTH],
      ];
      const testIds = ["watch-contract-empty", "watch-contract-authoring-rejected", "watch-contract-panel"];

      for (const [contract, health] of cases) {
        await render(contract, health);
        const rendered = testIds.filter((id) => container.querySelector(`[data-testid='${id}']`) !== null);
        expect(rendered.length).toBe(1);
      }
    });
  });
});
