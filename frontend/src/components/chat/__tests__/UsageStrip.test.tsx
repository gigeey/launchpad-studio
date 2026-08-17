// @vitest-environment jsdom
/**
 * Renders the UsageStrip component in isolation to pin the human-facing
 * formatting:
 *
 * - Input (ArrowUp icon) and output (ArrowDown icon) always render — they're
 *   the headline cost signals users care about regardless of build mode.
 * - The cache breakdown (Zap = cache_read, Sparkles = cache_creation) is
 *   gated behind `showCacheFields` (defaults to `import.meta.env.DEV`). Dev
 *   builds get the full four-field readout for caching telemetry; prod
 *   builds get the lean two-field strip without the noisy cache details.
 * - Cache-hit ratio renders as a percentage of total *input* (input + cache_read
 *   + cache_creation), since that's the share of what was sent on the wire
 *   that came back from cache. Hidden when cache fields are hidden.
 * - `cache_creation` is hidden when 0 (avoids visual noise on cache-only
 *   turns), shown otherwise.
 * - Large numbers get locale-formatted thousands separators for legibility.
 *
 * Tests target fields via their `data-field` attribute rather than glyph text
 * — icons render as SVG nodes so substring matching on the strip's textContent
 * would skip them. The data-field hook also survives icon-library swaps.
 *
 * Note on animation: the AnimatedNumber sub-component initializes its display
 * state to the incoming `value` synchronously, so first-render text equals the
 * final value. Tests don't need rAF mocks — only subsequent value *changes*
 * trigger the rolling tween, which these tests don't exercise.
 */

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { UsageStrip } from "../StreamingMessage";

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

function renderStrip(
  usage: Parameters<typeof UsageStrip>[0]["usage"],
  showCacheFields?: boolean,
) {
  act(() => {
    root.render(
      <UsageStrip
        usage={usage}
        accentColor="#3478f6"
        {...(showCacheFields !== undefined ? { showCacheFields } : {})}
      />,
    );
  });
  const strip = container.querySelector('[data-testid="usage-strip"]');
  if (!strip) throw new Error("usage-strip not rendered");
  return strip;
}

function field(strip: Element, name: string): HTMLElement | null {
  return strip.querySelector(`[data-field="${name}"]`);
}

describe("UsageStrip — dev mode (cache fields visible)", () => {
  it("renders all four fields with cache ratio when cache_creation > 0", () => {
    // Numbers lifted from a real dogfood log line so the test doubles as a
    // documented example of the expected shape.
    const strip = renderStrip(
      {
        input: 1,
        output: 162,
        cacheRead: 145467,
        cacheCreation: 2157,
        total: 147625,
      },
      true,
    );
    expect(field(strip, "input")?.textContent).toContain("1");
    expect(field(strip, "output")?.textContent).toContain("162");
    expect(field(strip, "cache-read")?.textContent).toContain("145,467");
    expect(field(strip, "cache-creation")?.textContent).toContain("2,157");
    // ratio = round(145467 / (1 + 145467 + 2157) * 100) = round(98.54...) = 99
    expect(field(strip, "cache-read")?.textContent).toContain("(99%)");
  });

  it("omits cache_creation field when zero", () => {
    const strip = renderStrip(
      {
        input: 5,
        output: 30,
        cacheRead: 150,
        cacheCreation: 0,
        total: 185,
      },
      true,
    );
    expect(field(strip, "cache-creation")).toBeNull();
  });

  it("omits cache ratio when there is no input at all", () => {
    // Edge case — shouldn't happen in practice but the ratio div-by-zero
    // guard should keep the strip clean rather than rendering "(NaN%)".
    const strip = renderStrip(
      {
        input: 0,
        output: 0,
        cacheRead: 0,
        cacheCreation: 0,
        total: 0,
      },
      true,
    );
    expect(strip.textContent ?? "").not.toContain("%");
  });
});

describe("UsageStrip — production mode (cache fields hidden)", () => {
  it("renders only input + output when showCacheFields is false", () => {
    // Production build behavior: keep the data flowing on the wire (TurnUsage
    // still carries cacheRead/cacheCreation) but render only the lean two-field
    // strip so end users don't see cache-internal jargon.
    const strip = renderStrip(
      {
        input: 1,
        output: 162,
        cacheRead: 145467,
        cacheCreation: 2157,
        total: 147625,
      },
      false,
    );
    expect(field(strip, "input")?.textContent).toContain("1");
    expect(field(strip, "output")?.textContent).toContain("162");
    // Cache fields must NOT leak into the prod render — neither the wrapping
    // spans nor the raw numbers they would have displayed.
    expect(field(strip, "cache-read")).toBeNull();
    expect(field(strip, "cache-creation")).toBeNull();
    expect(strip.textContent ?? "").not.toContain("%");
    expect(strip.textContent ?? "").not.toContain("145,467");
    expect(strip.textContent ?? "").not.toContain("2,157");
  });
});

describe("UsageStrip — icon rendering", () => {
  it("renders lucide SVG icons (not emoji glyphs) for each visible field", () => {
    // Guards against regressing back to emoji — verifies the SVG nodes are
    // actually present and no stray emoji characters slipped through.
    const strip = renderStrip(
      { input: 1, output: 2, cacheRead: 3, cacheCreation: 4, total: 10 },
      true,
    );
    expect(field(strip, "input")?.querySelector("svg")).not.toBeNull();
    expect(field(strip, "output")?.querySelector("svg")).not.toBeNull();
    expect(field(strip, "cache-read")?.querySelector("svg")).not.toBeNull();
    expect(field(strip, "cache-creation")?.querySelector("svg")).not.toBeNull();
    const text = strip.textContent ?? "";
    for (const glyph of ["↑", "↓", "⚡", "✦"]) {
      expect(text).not.toContain(glyph);
    }
  });
});
