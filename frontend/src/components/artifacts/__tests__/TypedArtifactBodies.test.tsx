// @vitest-environment jsdom
//
// Payload-shape tolerance for the typed renderer bodies. LLM-
// authored payloads for a given `kind` routinely deviate from the one exact
// shape each renderer was originally written against (`{ items: [...] }`,
// `{ columns: [...], rows: [...] }`, etc.) — these tests pin down that a
// handful of common synonym shapes still render as their intended body
// instead of falling through to `RawPayloadFallback`, while the original
// exact shape keeps producing byte-identical markup.
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import {
  BoardArtifactBody,
  CardsArtifactBody,
  ListArtifactBody,
  MetricArtifactBody,
  TableArtifactBody,
} from "../TypedArtifactBodies";
import type { ArtifactKind, ArtifactWithPayload } from "../../../types/api";

function makeArtifact(kind: ArtifactKind, payload: unknown): ArtifactWithPayload {
  return {
    id: `artifact-${kind}`,
    title: `Test ${kind} artifact`,
    kind,
    format: "json",
    stored_filename: "blob.json",
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

describe("TypedArtifactBodies — payload-shape tolerance", () => {
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

  async function render(el: React.ReactElement) {
    await act(async () => {
      root.render(el);
    });
  }

  describe("CardsArtifactBody", () => {
    it("keeps the exact-shape happy path for { items: [...] } unchanged", async () => {
      const artifact = makeArtifact("cards", { items: [{ title: "Card one", subtitle: "sub" }] });
      await render(React.createElement(CardsArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-body-cards"]')).not.toBeNull();
      expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).toBeNull();
      expect(container.textContent).toContain("Card one");
      expect(container.textContent).toContain("sub");
    });

    it("renders cards from a bare top-level array payload", async () => {
      const artifact = makeArtifact("cards", [
        { name: "First", body: "First body" },
        { name: "Second", body: "Second body" },
      ]);
      await render(React.createElement(CardsArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-body-cards"]')).not.toBeNull();
      expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).toBeNull();
      expect(container.textContent).toContain("First");
      expect(container.textContent).toContain("First body");
      expect(container.textContent).toContain("Second");
    });

    it('renders cards from a { cards: [...] } container', async () => {
      const artifact = makeArtifact("cards", { cards: [{ title: "From cards key" }] });
      await render(React.createElement(CardsArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-body-cards"]')).not.toBeNull();
      expect(container.textContent).toContain("From cards key");
    });

    it('renders cards from a { data: [...] } container with synonym fields', async () => {
      const artifact = makeArtifact("cards", {
        data: [{ heading: "Heading field", text: "Text field", tag: "Tag field" }],
      });
      await render(React.createElement(CardsArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-body-cards"]')).not.toBeNull();
      expect(container.textContent).toContain("Heading field");
      expect(container.textContent).toContain("Text field");
      expect(container.textContent).toContain("Tag field");
    });

    it("falls back to the formatted view when nothing in the payload is renderable as a card", async () => {
      const artifact = makeArtifact("cards", { foo: "bar", nested: { baz: 1 } });
      await render(React.createElement(CardsArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).not.toBeNull();
    });

    it("falls back to the formatted view for an array of unrecognizable items", async () => {
      const artifact = makeArtifact("cards", [{ unrelated: true }, { alsoUnrelated: 1 }]);
      await render(React.createElement(CardsArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).not.toBeNull();
    });
  });

  describe("ListArtifactBody", () => {
    it("still renders string items via { items: [...] }", async () => {
      const artifact = makeArtifact("list", { items: ["Item one"] });
      await render(React.createElement(ListArtifactBody, { artifact }));
      expect(container.textContent).toContain("Item one");
    });

    it("accepts a { entries: [...] } container", async () => {
      const artifact = makeArtifact("list", { entries: [{ title: "Entry title" }] });
      await render(React.createElement(ListArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).toBeNull();
      expect(container.textContent).toContain("Entry title");
    });
  });

  describe("TableArtifactBody", () => {
    it("keeps the exact-shape happy path unchanged", async () => {
      const artifact = makeArtifact("table", { columns: ["name"], rows: [{ name: "a" }] });
      await render(React.createElement(TableArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).toBeNull();
      expect(container.textContent).toContain("a");
    });

    it("accepts { headers: [...] } and { data: [...] } as synonyms", async () => {
      const artifact = makeArtifact("table", { headers: ["name"], data: [{ name: "synonym-row" }] });
      await render(React.createElement(TableArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).toBeNull();
      expect(container.textContent).toContain("synonym-row");
    });
  });

  describe("BoardArtifactBody", () => {
    it("accepts a { lanes: [...] } container as a synonym for columns", async () => {
      const artifact = makeArtifact("board", { lanes: [{ title: "To do", items: [{ title: "Task" }] }] });
      await render(React.createElement(BoardArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).toBeNull();
      expect(container.textContent).toContain("To do");
      expect(container.textContent).toContain("Task");
    });
  });

  describe("MetricArtifactBody", () => {
    it("keeps the single flat { value } happy path unchanged", async () => {
      const artifact = makeArtifact("metric", { label: "Revenue", value: 42 });
      await render(React.createElement(MetricArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).toBeNull();
      expect(container.textContent).toContain("42");
    });

    it("accepts a { data: [...] } container as a synonym for metrics", async () => {
      const artifact = makeArtifact("metric", { data: [{ label: "Signups", value: 7 }] });
      await render(React.createElement(MetricArtifactBody, { artifact }));
      expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).toBeNull();
      expect(container.textContent).toContain("Signups");
      expect(container.textContent).toContain("7");
    });
  });
});
