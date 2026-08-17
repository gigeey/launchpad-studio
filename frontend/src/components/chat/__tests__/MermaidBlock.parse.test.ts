/**
 * Pins the two mermaid contracts `MermaidBlock` depends on before it will put a
 * diagram on screen.
 *
 * `MermaidBlock` calls `mermaid.parse(code)` and only calls `mermaid.render()`
 * if that resolves. The pre-flight parse is not redundant: on invalid input
 * `render()` injects its own error SVG directly into `document.body`, outside
 * the React tree, and the component shows a styled error card instead. Both
 * halves of that design rest on `parse()` resolving for valid diagrams and
 * rejecting for invalid ones.
 *
 * WHY THIS FILE EXISTS. mermaid splits parsing across two backends — the
 * built-in grammars, and `@mermaid-js/parser` for the newer `-beta` diagram
 * types. The `-beta` cases below are the ones routed through that separate
 * package, so they are the ones that move when it is upgraded.
 *
 * WHAT THIS FILE DOES NOT COVER. `mermaid.render()` is not exercised here.
 * Under jsdom it throws `text2.getBBox is not a function` — jsdom implements no
 * SVG text metrics, and mermaid measures every label to lay a diagram out. That
 * is a limit of the environment, not a defect in mermaid or in `MermaidBlock`.
 * Covering `render()` needs a real browser engine.
 *
 * TODO(mermaid-render): no automated coverage asserts that a diagram reaches
 * the screen as SVG. Add it if a browser-mode runner is introduced; until then
 * a mermaid upgrade needs the diagram path checked by hand.
 */

import { describe, it, expect, beforeAll } from "vitest";

/** Mirrors the options `MermaidBlock.getMermaid()` initializes with. */
async function loadMermaid() {
  const mermaid = (await import("mermaid")).default;
  mermaid.initialize({ startOnLoad: false, theme: "neutral", securityLevel: "strict" });
  return mermaid;
}

/**
 * `parser` marks which backend owns the grammar: the `-beta` types are parsed
 * by `@mermaid-js/parser`, the rest by mermaid's built-in grammars.
 */
const DIAGRAMS: Array<{ name: string; parser: "builtin" | "parser-pkg"; src: string; type: string }> = [
  { name: "flowchart", parser: "builtin", type: "flowchart-v2", src: "graph TD; A-->B;" },
  { name: "sequence", parser: "builtin", type: "sequence", src: "sequenceDiagram\n  Alice->>Bob: Hi" },
  { name: "state", parser: "builtin", type: "stateDiagram", src: "stateDiagram-v2\n  [*] --> Still" },
  { name: "class", parser: "builtin", type: "class", src: "classDiagram\n  Animal <|-- Duck" },
  { name: "pie", parser: "builtin", type: "pie", src: 'pie title Pets\n  "Dogs" : 40' },
  {
    name: "gantt",
    parser: "builtin",
    type: "gantt",
    src: "gantt\n  title T\n  section S\n  A task :a1, 2024-01-01, 30d",
  },
  {
    name: "architecture",
    parser: "parser-pkg",
    type: "architecture",
    src: "architecture-beta\n  group api(cloud)[API]\n  service db(database)[DB] in api",
  },
  {
    name: "radar",
    parser: "parser-pkg",
    type: "radar",
    src: 'radar-beta\n  axis a["A"], b["B"]\n  curve x["X"]{1,2}',
  },
  { name: "packet", parser: "parser-pkg", type: "packet", src: 'packet-beta\n  0-15: "Src Port"' },
  { name: "treemap", parser: "parser-pkg", type: "treemap", src: 'treemap-beta\n"Root"\n  "A": 10' },
];

describe("mermaid.parse — the gate MermaidBlock renders behind", () => {
  let mermaid: Awaited<ReturnType<typeof loadMermaid>>;

  beforeAll(async () => {
    mermaid = await loadMermaid();
  });

  it.each(DIAGRAMS)("accepts a $name diagram ($parser)", async ({ src, type }) => {
    const result = await mermaid.parse(src);
    expect(result).toBeTruthy();
    expect(result && result.diagramType).toBe(type);
  });

  it("rejects input that matches no diagram grammar", async () => {
    // MermaidBlock's error card is reachable only via this rejection. If parse
    // began resolving here, render() would run on invalid input and append its
    // own error SVG to document.body.
    await expect(mermaid.parse("this is definitely not a diagram {{{")).rejects.toThrow();
  });

  it("rejects a diagram whose header is valid but whose body is not", async () => {
    await expect(mermaid.parse("sequenceDiagram\n  ->>>> nonsense ->>>>")).rejects.toThrow();
  });
});
