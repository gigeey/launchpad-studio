// @vitest-environment jsdom
//
// Coverage for the ArtifactKind → renderer registry: every kind
// must route to its own renderer, and an unrecognized kind must land on the
// inert unsupported placeholder rather than throwing or blank-screening.
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { ARTIFACT_KIND_RENDERERS, resolveArtifactRenderer } from "../registry";
import type { ArtifactKind, ArtifactWithPayload } from "../../../types/api";

function makeArtifact(kind: ArtifactKind, payload: unknown): ArtifactWithPayload {
  return {
    id: `artifact-${kind}`,
    title: `Test ${kind} artifact`,
    kind,
    format: kind === "html" ? "html" : "json",
    stored_filename: kind === "html" ? "blob.html" : "blob.json",
    size_bytes: 0,
    checksum_sha256: "deadbeef",
    refresh_intent: "none",
    origin_intent: null,
    capabilities: [],
    source_message_id: null,
    created_at: "2026-07-11T00:00:00Z",
    updated_at: "2026-07-11T00:00:00Z",
    last_refreshed_at: null,
    refresh_count: 0,
    pinned: false,
    pinned_at: null,
    group_id: null,
    payload,
  };
}

// One well-formed payload per typed kind, matching what each body's guard
// expects (the exact payload shape is the renderer's business, not a
// backend-enforced schema).
const TYPED_PAYLOADS: Record<Exclude<ArtifactKind, "unknown">, unknown> = {
  list: { items: [{ title: "Item one", subtitle: "sub" }] },
  cards: { items: [{ title: "Card one" }] },
  table: { columns: ["name", "value"], rows: [{ name: "a", value: 1 }] },
  board: { columns: [{ title: "To do", items: [{ title: "Task one" }] }] },
  metric: { metrics: [{ label: "Revenue", value: 42 }] },
  chart: { labels: ["Mon", "Tue"], series: [{ name: "Visits", values: [3, 5] }] },
  html: "<p>hello</p>",
};

const ALL_KINDS = Object.keys(TYPED_PAYLOADS) as Array<keyof typeof TYPED_PAYLOADS>;

describe("ARTIFACT_KIND_RENDERERS", () => {
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

  it("has exactly the 7 known kinds plus the unknown fallback registered", () => {
    expect(Object.keys(ARTIFACT_KIND_RENDERERS).sort()).toEqual(
      [...ALL_KINDS, "unknown"].sort(),
    );
  });

  for (const kind of ALL_KINDS) {
    it(`routes "${kind}" to its own renderer body`, async () => {
      const Body = ARTIFACT_KIND_RENDERERS[kind];
      const artifact = makeArtifact(kind, TYPED_PAYLOADS[kind]);
      await act(async () => {
        root.render(React.createElement(Body, { artifact }));
      });
      const el = container.querySelector(`[data-testid="artifact-body-${kind}"]`);
      expect(el).not.toBeNull();
    });
  }

  it('routes an "unknown" kind to the inert unsupported placeholder, never throwing', async () => {
    const artifact = makeArtifact("unknown", { whatever: true });
    const Body = resolveArtifactRenderer(artifact.kind);
    expect(() => {
      act(() => {
        root.render(React.createElement(Body, { artifact }));
      });
    }).not.toThrow();
    expect(container.querySelector('[data-testid="artifact-body-unsupported"]')).not.toBeNull();
    expect(container.textContent).toContain("Unsupported artifact type");
  });

  it("resolveArtifactRenderer falls back to the placeholder for a value outside the map", () => {
    // Cast past the type system the way a stale/forward-compat payload could
    // arrive at runtime despite the TS union claiming exhaustiveness.
    const surprising = "timeline" as unknown as ArtifactKind;
    expect(resolveArtifactRenderer(surprising)).toBe(ARTIFACT_KIND_RENDERERS.unknown);
  });
});
