// @vitest-environment jsdom
//
// The load-bearing security test for the artifact renderer: the
// sandboxed-HTML renderer must NEVER set `allow-same-origin` alongside
// `allow-scripts`. The precedent this component's shape is copied from
// (`TasklistOutputPortal.tsx`'s html branch) uses the *inverse* posture
// (`allow-same-origin` only, no `allow-scripts`) — this test pins down that
// `HtmlArtifactBody` did not inherit that flag by copy-paste.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { HtmlArtifactBody } from "../HtmlArtifactBody";
import type { ArtifactWithPayload } from "../../../types/api";

const openUrlMock = vi.fn();

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: (...args: unknown[]) => openUrlMock(...args),
}));

function makeHtmlArtifact(payload: string): ArtifactWithPayload {
  return {
    id: "artifact-html-1",
    title: "Test HTML Artifact",
    kind: "html",
    format: "html",
    stored_filename: "blob.html",
    size_bytes: payload.length,
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

describe("HtmlArtifactBody — sandbox invariant", () => {
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

  it("never includes allow-same-origin in the rendered sandbox attribute", async () => {
    const artifact = makeHtmlArtifact("<script>window.parent.postMessage('escape', '*')</script>");
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact }));
    });

    const iframe = container.querySelector('iframe[data-testid="artifact-body-html"]');
    expect(iframe).not.toBeNull();

    const sandbox = iframe!.getAttribute("sandbox");
    expect(sandbox).not.toBeNull();
    expect(sandbox).not.toContain("allow-same-origin");
  });

  it("grants allow-scripts, allow-modals, and allow-popups — but never same-origin or top-navigation", async () => {
    const artifact = makeHtmlArtifact("<p>static</p>");
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact }));
    });

    const iframe = container.querySelector('iframe[data-testid="artifact-body-html"]');
    const sandbox = iframe!.getAttribute("sandbox")!;
    const flags = sandbox.split(/\s+/).filter(Boolean);
    expect(flags).toEqual([
      "allow-scripts",
      "allow-modals",
      "allow-popups",
      "allow-popups-to-escape-sandbox",
    ]);
    expect(flags).not.toContain("allow-same-origin");
    expect(flags).not.toContain("allow-top-navigation");
    expect(flags).not.toContain("allow-top-navigation-by-user-activation");
  });

  it("renders the payload via srcdoc rather than a src URL", async () => {
    const html = "<p>hello from the artifact</p>";
    const artifact = makeHtmlArtifact(html);
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact }));
    });

    const iframe = container.querySelector("iframe") as HTMLIFrameElement;
    expect(iframe.hasAttribute("src")).toBe(false);
    // The rendered payload, not a byte-for-byte match — the print
    // color-adjust style is spliced in (see the srcdoc test below).
    expect(iframe.srcdoc).toContain(html);
  });

  it("injects a print-color-adjust style into srcdoc so backgrounds print by default", async () => {
    const html = "<p>hello from the artifact</p>";
    const artifact = makeHtmlArtifact(html);
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact }));
    });

    const iframe = container.querySelector("iframe") as HTMLIFrameElement;
    expect(iframe.srcdoc).toContain("@media print");
    expect(iframe.srcdoc).toContain("print-color-adjust: exact");
  });

  it("splices the print style after <head> when the artifact HTML has one", async () => {
    const html = "<html><head><title>t</title></head><body><p>hi</p></body></html>";
    const artifact = makeHtmlArtifact(html);
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact }));
    });

    const iframe = container.querySelector("iframe") as HTMLIFrameElement;
    expect(iframe.srcdoc.indexOf("@media print")).toBeGreaterThan(iframe.srcdoc.indexOf("<head>"));
    expect(iframe.srcdoc.indexOf("@media print")).toBeLessThan(iframe.srcdoc.indexOf("<title>"));
  });

  it("forwards iframeRef to the underlying iframe element", async () => {
    const artifact = makeHtmlArtifact("<p>static</p>");
    const ref = React.createRef<HTMLIFrameElement>();
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact, iframeRef: ref }));
    });

    expect(ref.current).not.toBeNull();
    expect(ref.current?.tagName).toBe("IFRAME");
  });

  it("renders an empty frame rather than throwing when payload isn't a string", async () => {
    const artifact = makeHtmlArtifact("");
    // `payload` is typed `unknown` on ArtifactWithPayload — deliberately
    // assign a non-string shape here to exercise the runtime guard.
    artifact.payload = { not: "a string" };
    expect(() => {
      act(() => {
        root.render(React.createElement(HtmlArtifactBody, { artifact }));
      });
    }).not.toThrow();
  });
});

// The desktop shell has no tab/window manager, so a `target="_blank"` link
// click inside the artifact iframe is otherwise silently dropped (PRD link-
// bridge fix). `withLinkBridge` injects a script that relays the click to
// the parent via `postMessage`; these tests cover the parent-side listener
// that turns that message into an `openUrl` call — and, just as important,
// that it refuses to act on a message that isn't really from this iframe.
describe("HtmlArtifactBody — link-open bridge", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    openUrlMock.mockReset();
    openUrlMock.mockResolvedValue(undefined);
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function flushMicrotasks() {
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
  }

  function dispatchArtifactMessage(source: MessageEventSource | null, data: unknown) {
    const event = new MessageEvent("message", { data });
    // jsdom's `MessageEvent` doesn't honor a `source` passed via the
    // constructor init dict (browsers treat it as effectively read-only
    // outside of real cross-window dispatch), so force it directly — this
    // is the documented escape hatch for exercising the identity guard.
    Object.defineProperty(event, "source", { value: source });
    window.dispatchEvent(event);
  }

  it("calls openUrl when the message comes from the artifact iframe itself", async () => {
    const artifact = makeHtmlArtifact("<a href='https://example.com'>read more</a>");
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact }));
    });
    const iframe = container.querySelector(
      'iframe[data-testid="artifact-body-html"]'
    ) as HTMLIFrameElement;

    dispatchArtifactMessage(iframe.contentWindow, {
      __artifactLinkOpen: true,
      url: "https://example.com/x",
    });
    await flushMicrotasks();

    expect(openUrlMock).toHaveBeenCalledTimes(1);
    expect(openUrlMock).toHaveBeenCalledWith("https://example.com/x");
  });

  it("ignores a message whose source is not the artifact iframe", async () => {
    const artifact = makeHtmlArtifact("<a href='https://example.com'>read more</a>");
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact }));
    });

    // `window` (the top-level test window) is never the iframe's
    // `contentWindow` — a spoofed/foreign source for the identity guard.
    dispatchArtifactMessage(window, {
      __artifactLinkOpen: true,
      url: "https://example.com/x",
    });
    await flushMicrotasks();

    expect(openUrlMock).not.toHaveBeenCalled();
  });

  it("ignores a same-source message whose url isn't http(s)", async () => {
    const artifact = makeHtmlArtifact("<a href='https://example.com'>read more</a>");
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact }));
    });
    const iframe = container.querySelector(
      'iframe[data-testid="artifact-body-html"]'
    ) as HTMLIFrameElement;

    dispatchArtifactMessage(iframe.contentWindow, {
      __artifactLinkOpen: true,
      url: "javascript:alert(1)",
    });
    dispatchArtifactMessage(iframe.contentWindow, {
      __artifactLinkOpen: true,
      url: "file:///etc/passwd",
    });
    await flushMicrotasks();

    expect(openUrlMock).not.toHaveBeenCalled();
  });

  it("injects the link-bridge script (with the __artifactLinkOpen marker) into srcdoc", async () => {
    const html = "<p>hello from the artifact</p>";
    const artifact = makeHtmlArtifact(html);
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact }));
    });

    const iframe = container.querySelector("iframe") as HTMLIFrameElement;
    expect(iframe.srcdoc).toContain("__artifactLinkOpen");
    expect(iframe.srcdoc).toContain("<script>");
  });
});

// Printing no longer injects an in-frame bridge script: `Window.print` isn't
// reachable across the opaque-origin boundary from the parent, AND Tauri's
// native-print patch only reaches a webview's top frame (never this nested
// one), so a same-frame `window.print()` here would hit WebKit's no-op print
// anyway. Print is now routed through the artifact's own pop-out window
// instead — see `printArtifactWindow` in `lib/windows.ts` and
// `ArtifactWindowView`. This locks in the removal: no print-bridge script /
// `__artifactPrint` message shape should leak back into the srcdoc.
describe("HtmlArtifactBody — no in-frame print bridge", () => {
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

  it("does not inject a print-bridge script or __artifactPrint listener into srcdoc", async () => {
    const html = "<p>hello from the artifact</p>";
    const artifact = makeHtmlArtifact(html);
    await act(async () => {
      root.render(React.createElement(HtmlArtifactBody, { artifact }));
    });

    const iframe = container.querySelector("iframe") as HTMLIFrameElement;
    // The link bridge (and its message listener) stays; the print bridge is gone.
    expect(iframe.srcdoc).toContain("__artifactLinkOpen");
    expect(iframe.srcdoc).not.toContain("__artifactPrint");
    expect(iframe.srcdoc).not.toContain("window.print()");
  });
});
