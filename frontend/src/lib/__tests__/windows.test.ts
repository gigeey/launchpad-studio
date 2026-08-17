// @vitest-environment jsdom
//
// `openArtifactWindow` generalizes the Memories pop-out from a
// fixed singleton label/hash to one label/hash per artifact instance. The
// load-bearing assertion is that two distinct artifact ids produce two
// distinct labels (so the focus-if-exists check in `openPopoutWindow` keys
// on the specific artifact, not on some shared "artifact window" bucket),
// while `openMemoriesWindow` keeps working unchanged.
import { describe, it, expect, beforeEach, vi } from "vitest";

const { getByLabelMock, constructed, emitToMock } = vi.hoisted(() => ({
  getByLabelMock: vi.fn(),
  constructed: [] as Array<{ label: string; options: Record<string, unknown> }>,
  emitToMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/webviewWindow", () => {
  class MockWebviewWindow {
    label: string;
    constructor(label: string, options: Record<string, unknown>) {
      this.label = label;
      constructed.push({ label, options });
    }
    once() {
      // no-op — production code registers a "tauri://error" listener we
      // don't need to exercise here.
    }
    async setFocus() {}
    static getByLabel(...args: unknown[]) {
      return getByLabelMock(...args);
    }
  }
  return { WebviewWindow: MockWebviewWindow };
});

vi.mock("@tauri-apps/api/event", () => ({
  emitTo: (...args: unknown[]) => emitToMock(...args),
}));

import {
  openArtifactWindow,
  openMemoriesWindow,
  printArtifactWindow,
  ARTIFACT_PRINT_EVENT,
} from "../windows";

describe("lib/windows", () => {
  beforeEach(() => {
    constructed.length = 0;
    getByLabelMock.mockReset();
    getByLabelMock.mockResolvedValue(null);
    emitToMock.mockReset();
    emitToMock.mockResolvedValue(undefined);
  });

  it("openMemoriesWindow still opens the fixed memories label/hash", async () => {
    await openMemoriesWindow();

    expect(getByLabelMock).toHaveBeenCalledWith("memories");
    expect(constructed).toHaveLength(1);
    expect(constructed[0].label).toBe("memories");
    expect(constructed[0].options.url).toBe("/#/memories-window");
  });

  it("openArtifactWindow produces a distinct label and hash per artifact id", async () => {
    await openArtifactWindow("agent-1", "artifact-aaa");
    await openArtifactWindow("agent-1", "artifact-bbb");

    expect(constructed).toHaveLength(2);
    const [a, b] = constructed;

    // Distinct ids -> distinct labels (the focus-if-exists check keys on this).
    expect(a.label).toBe("artifact:artifact-aaa");
    expect(b.label).toBe("artifact:artifact-bbb");
    expect(a.label).not.toBe(b.label);

    // Distinct ids -> distinct hash routes too, both carrying the agent id
    // the pop-out window needs to fetch the artifact (per-agent-scoped route).
    expect(a.options.url).toBe("/#/artifact-window/agent-1/artifact-aaa");
    expect(b.options.url).toBe("/#/artifact-window/agent-1/artifact-bbb");
    expect(a.options.url).not.toBe(b.options.url);
  });

  it("re-opening the same artifact id focuses the existing window instead of spawning a duplicate", async () => {
    const setFocusMock = vi.fn().mockResolvedValue(undefined);
    getByLabelMock.mockResolvedValueOnce({ setFocus: setFocusMock });

    await openArtifactWindow("agent-1", "artifact-aaa");

    expect(setFocusMock).toHaveBeenCalledTimes(1);
    expect(constructed).toHaveLength(0);
  });

  it("a different artifact id under the same agent opens a distinct window", async () => {
    await openArtifactWindow("agent-1", "artifact-aaa");
    await openArtifactWindow("agent-1", "artifact-ccc");

    const labels = constructed.map((w) => w.label);
    expect(new Set(labels).size).toBe(2);
  });

  it("printArtifactWindow opens a fresh window carrying a print marker and does not emit", async () => {
    // No existing window -> open one whose URL tells the standalone root to
    // self-print once the artifact has rendered (avoids racing a print event
    // against a window that hasn't mounted its listener yet).
    await printArtifactWindow("agent-1", "artifact-aaa");

    expect(constructed).toHaveLength(1);
    expect(constructed[0].label).toBe("artifact:artifact-aaa");
    expect(constructed[0].options.url).toBe("/#/artifact-window/agent-1/artifact-aaa?print=1");
    // A fresh window self-prints from its URL marker, so no cross-window event.
    expect(emitToMock).not.toHaveBeenCalled();
  });

  it("printArtifactWindow focuses an already-open window and asks it to print via an event", async () => {
    const setFocusMock = vi.fn().mockResolvedValue(undefined);
    getByLabelMock.mockResolvedValueOnce({ setFocus: setFocusMock });

    await printArtifactWindow("agent-1", "artifact-aaa");

    // Already rendered -> focus it and emit the print request to its label
    // rather than spawning a duplicate.
    expect(setFocusMock).toHaveBeenCalledTimes(1);
    expect(constructed).toHaveLength(0);
    expect(emitToMock).toHaveBeenCalledTimes(1);
    expect(emitToMock).toHaveBeenCalledWith("artifact:artifact-aaa", ARTIFACT_PRINT_EVENT);
  });
});
