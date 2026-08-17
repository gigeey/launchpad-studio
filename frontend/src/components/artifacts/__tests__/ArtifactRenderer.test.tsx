// @vitest-environment jsdom
//
// Integration coverage for `ArtifactPreview`: fetches an artifact by id via
// `api.getArtifact` and dispatches to the registry-selected
// renderer, with loading/error states and a never-throws unknown-kind path.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { ArtifactPreview } from "../ArtifactRenderer";
import type { ArtifactWithPayload } from "../../../types/api";

const getArtifactMock = vi.fn();
const deleteArtifactMock = vi.fn();
const regenerateArtifactMock = vi.fn();
const undoArtifactMock = vi.fn();

const { MockApiError, printArtifactWindowMock } = vi.hoisted(() => ({
  MockApiError: class MockApiError extends Error {
    status: number;
    constructor(status: number, message: string) {
      super(message);
      this.name = "ApiError";
      this.status = status;
    }
  },
  printArtifactWindowMock: vi.fn(),
}));

// Print routes through the artifact's pop-out window (a real top-level webview
// whose top frame Tauri patches `window.print()` on) rather than the sandboxed
// child frame — see `printArtifactWindow`/`handlePrint`. Mock the module so
// this unit test asserts the routing without touching real Tauri windows.
vi.mock("../../../lib/windows", () => ({
  printArtifactWindow: (...args: unknown[]) => printArtifactWindowMock(...args),
}));

vi.mock("../../../lib/api", () => ({
  getArtifact: (...args: unknown[]) => getArtifactMock(...args),
  deleteArtifact: (...args: unknown[]) => deleteArtifactMock(...args),
  regenerateArtifact: (...args: unknown[]) => regenerateArtifactMock(...args),
  undoArtifact: (...args: unknown[]) => undoArtifactMock(...args),
  getAttachmentUrl: (agentId: string, id: string) => `mock://attachment/${agentId}/${id}`,
  ApiError: MockApiError,
}));

// The chat-to-adjust panel (opened by one test below) is lazy-loaded by
// `ArtifactRenderer` and renders through the real `MessageBubble` + real
// `ChatInput`. `ChatInput` (a TipTap rich editor with no test hooks) is
// stubbed here the same way `ProjectDetailView.transition.test.tsx` and
// `ArtifactChatPanel.serverHydrate.test.tsx` do, since this file doesn't
// exercise the composer directly.
vi.mock("../../chat/ChatInput", () => ({ ChatInput: () => null }));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  save: vi.fn().mockResolvedValue(null),
}));

vi.mock("@tauri-apps/plugin-fs", () => ({
  writeFile: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("framer-motion", () => ({
  motion: {
    div: ({ children, ...rest }: React.HTMLAttributes<HTMLDivElement>) =>
      React.createElement("div", rest, children),
  },
  AnimatePresence: ({ children }: { children: React.ReactNode }) =>
    React.createElement(React.Fragment, null, children),
}));

function makeArtifact(overrides: Partial<ArtifactWithPayload> = {}): ArtifactWithPayload {
  return {
    id: "artifact-1",
    title: "Weekly metrics",
    kind: "metric",
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
    payload: { metrics: [{ label: "Revenue", value: 100 }] },
    ...overrides,
  };
}

describe("ArtifactPreview", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
    getArtifactMock.mockReset();
    deleteArtifactMock.mockReset();
    regenerateArtifactMock.mockReset();
    undoArtifactMock.mockReset();
    printArtifactWindowMock.mockReset();
    printArtifactWindowMock.mockResolvedValue(undefined);
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  it("renders nothing when agentId/artifactId are null", async () => {
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: null, artifactId: null, onClose: vi.fn() }),
      );
    });
    expect(getArtifactMock).not.toHaveBeenCalled();
    expect(container.querySelector('[data-testid^="artifact-body-"]')).toBeNull();
  });

  it("fetches by id via api.getArtifact and renders the matching kind body", async () => {
    getArtifactMock.mockResolvedValue(makeArtifact());
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, {
          agentId: "agent-1",
          artifactId: "artifact-1",
          onClose: vi.fn(),
        }),
      );
    });
    // Flush the resolved fetch promise.
    await act(async () => {
      await Promise.resolve();
    });

    expect(getArtifactMock).toHaveBeenCalledWith("agent-1", "artifact-1");
    expect(container.querySelector('[data-testid="artifact-body-metric"]')).not.toBeNull();
    expect(container.textContent).toContain("Weekly metrics");
  });

  it("shows an error message when the fetch rejects, without throwing", async () => {
    getArtifactMock.mockRejectedValue(new Error("API 404: not found"));
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, {
          agentId: "agent-1",
          artifactId: "missing",
          onClose: vi.fn(),
        }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    expect(container.textContent).toContain("API 404: not found");
  });

  it("renders the inert unsupported placeholder for an unknown kind end-to-end", async () => {
    getArtifactMock.mockResolvedValue(
      makeArtifact({ kind: "unknown", payload: { some: "future-shape" } }),
    );
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, {
          agentId: "agent-1",
          artifactId: "artifact-future",
          onClose: vi.fn(),
        }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    expect(container.querySelector('[data-testid="artifact-body-unsupported"]')).not.toBeNull();
    expect(container.textContent).toContain("Unsupported artifact type");
  });

  it("renders a 'cards' artifact whose payload is a top-level array as cards, not raw JSON", async () => {
    getArtifactMock.mockResolvedValue(
      makeArtifact({
        kind: "cards",
        format: "json",
        payload: [
          { name: "First card", body: "First body" },
          { name: "Second card", body: "Second body" },
        ],
      }),
    );
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, {
          agentId: "agent-1",
          artifactId: "artifact-cards-array",
          onClose: vi.fn(),
        }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    expect(container.querySelector('[data-testid="artifact-body-cards"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="artifact-raw-fallback"]')).toBeNull();
    expect(container.textContent).toContain("First card");
    expect(container.textContent).toContain("Second card");
  });

  it("routes Print through the artifact's pop-out window", async () => {
    // The artifact is a sandboxed opaque-origin child frame whose
    // `window.print()` Tauri never patches (its native-print patch is
    // main-frame-only), so print is routed to the artifact's own pop-out
    // window — a real top-level webview containing only the artifact — via
    // `printArtifactWindow(agentId, artifactId)`. This asserts that routing
    // rather than any in-frame `postMessage`/`.print()` call.
    getArtifactMock.mockResolvedValue(
      makeArtifact({ kind: "html", format: "html", payload: "<p>hello</p>" }),
    );
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, {
          agentId: "agent-1",
          artifactId: "artifact-1",
          onClose: vi.fn(),
        }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    const printBtn = container.querySelector('[aria-label="Print"]') as HTMLButtonElement;
    expect(printBtn).not.toBeNull();
    await act(async () => {
      printBtn.click();
    });

    expect(printArtifactWindowMock).toHaveBeenCalledTimes(1);
    expect(printArtifactWindowMock).toHaveBeenCalledWith("agent-1", "artifact-1");
  });

  it("does not throw when the pop-out print routing fails", async () => {
    // A failed pop-out open/focus must not crash the header — `handlePrint`
    // swallows the rejection.
    printArtifactWindowMock.mockRejectedValueOnce(new Error("no window"));
    getArtifactMock.mockResolvedValue(
      makeArtifact({ kind: "html", format: "html", payload: "<p>hello</p>" }),
    );
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, {
          agentId: "agent-1",
          artifactId: "artifact-1",
          onClose: vi.fn(),
        }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    const printBtn = container.querySelector('[aria-label="Print"]') as HTMLButtonElement;
    expect(() => {
      act(() => {
        printBtn.click();
      });
    }).not.toThrow();
  });

  it("does not render a Print button for non-html artifact kinds", async () => {
    getArtifactMock.mockResolvedValue(makeArtifact()); // kind: "metric"
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, {
          agentId: "agent-1",
          artifactId: "artifact-1",
          onClose: vi.fn(),
        }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    expect(container.querySelector('[aria-label="Print"]')).toBeNull();
  });

  it("calls onClose when the close button is clicked", async () => {
    const onClose = vi.fn();
    getArtifactMock.mockResolvedValue(makeArtifact());
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: "agent-1", artifactId: "artifact-1", onClose }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    const closeBtn = container.querySelector('[aria-label="Close artifact"]') as HTMLButtonElement;
    expect(closeBtn).not.toBeNull();
    await act(async () => {
      closeBtn.click();
    });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("asks for confirmation, then deletes via api.deleteArtifact and closes on confirm", async () => {
    const onClose = vi.fn();
    getArtifactMock.mockResolvedValue(makeArtifact());
    deleteArtifactMock.mockResolvedValue(undefined);
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: "agent-1", artifactId: "artifact-1", onClose }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    const deleteBtn = container.querySelector('[aria-label="Delete artifact"]') as HTMLButtonElement;
    expect(deleteBtn).not.toBeNull();
    await act(async () => {
      deleteBtn.click();
    });
    expect(deleteArtifactMock).not.toHaveBeenCalled();

    const confirmBtn = Array.from(document.querySelectorAll('[role="dialog"] button')).find(
      (b) => b.textContent?.trim() === "Delete",
    ) as HTMLButtonElement;
    expect(confirmBtn).not.toBeNull();
    expect(confirmBtn.textContent).toContain("Delete");
    await act(async () => {
      confirmBtn.click();
      await Promise.resolve();
    });

    expect(deleteArtifactMock).toHaveBeenCalledWith("agent-1", "artifact-1");
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("opens and closes the chat-to-adjust mini-thread panel keyed by artifactId, next to Refresh", async () => {
    getArtifactMock.mockResolvedValue(makeArtifact());
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, {
          agentId: "agent-1",
          artifactId: "artifact-1",
          onClose: vi.fn(),
        }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    expect(container.querySelector('[data-testid="artifact-chat-panel"]')).toBeNull();

    const chatBtn = container.querySelector('[aria-label="Toggle chat panel"]') as HTMLButtonElement;
    expect(chatBtn).not.toBeNull();
    expect(chatBtn.getAttribute("aria-pressed")).toBe("false");
    await act(async () => {
      chatBtn.click();
    });
    // `ArtifactChatPanel` is lazy-loaded (see `ArtifactRenderer.tsx`'s
    // comment on why) — the dynamic `import()` resolves via Vitest's module
    // runner, which takes real (if tiny) async work, not just a microtask —
    // a plain `Promise.resolve()` flush isn't enough, unlike other awaits in
    // this file. Poll briefly until `<Suspense>` swaps its `null` fallback
    // for the resolved panel. The old 20×5ms (100ms) budget was measured
    // right at the edge of what this resolves in on a real machine —
    // consistently ~110-125ms here — so it failed deterministically rather
    // than flakily; 100×5ms (500ms) leaves real headroom without slowing a
    // passing run, since the loop still exits the instant the panel appears.
    for (let i = 0; i < 100 && !container.querySelector('[data-testid="artifact-chat-panel"]'); i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 5));
      });
    }

    const panel = container.querySelector('[data-testid="artifact-chat-panel"]');
    expect(panel).not.toBeNull();
    expect(panel!.getAttribute("data-artifact-id")).toBe("artifact-1");
    expect(chatBtn.getAttribute("aria-pressed")).toBe("true");

    // Closing via the panel's own X button (distinct from the header toggle).
    const closeBtn = container.querySelector('[aria-label="Close chat panel"]') as HTMLButtonElement;
    await act(async () => {
      closeBtn.click();
    });
    expect(container.querySelector('[data-testid="artifact-chat-panel"]')).toBeNull();
  });

  it("shows an inline error and leaves the view open when delete fails", async () => {
    const onClose = vi.fn();
    getArtifactMock.mockResolvedValue(makeArtifact());
    deleteArtifactMock.mockRejectedValue(new Error("API 500: boom"));
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: "agent-1", artifactId: "artifact-1", onClose }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    const deleteBtn = container.querySelector('[aria-label="Delete artifact"]') as HTMLButtonElement;
    await act(async () => {
      deleteBtn.click();
    });

    const confirmBtn = Array.from(document.querySelectorAll('[role="dialog"] button')).find(
      (b) => b.textContent?.trim() === "Delete",
    ) as HTMLButtonElement;
    await act(async () => {
      confirmBtn.click();
      await Promise.resolve();
    });

    expect(onClose).not.toHaveBeenCalled();
    expect(container.textContent).toContain("API 500: boom");
  });

  it("resumes the Regenerating… spinner on mount when getArtifact reports a running_task_id", async () => {
    // Regression coverage for the "navigate away mid-run, come back, spinner
    // is gone" bug: the mount-time getArtifact fetch this component already
    // makes is also where a still-running background task surfaces
    // (`running_task_id`), so a fresh mount whose fetch reports one must
    // restore the spinner via `regen.resume()` instead of sitting idle.
    getArtifactMock.mockResolvedValue(
      makeArtifact({
        refresh_intent: "whole_artifact",
        origin_intent: { refresh_prompt: "Summarize this week's metrics" },
        updated_at: "t0",
        checksum_sha256: "c0",
        running_task_id: "bg-resumed-1",
      }),
    );
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: "agent-1", artifactId: "artifact-1", onClose: vi.fn() }),
      );
    });
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    // No new POST — this is a resume of an already-in-flight run, not a
    // freshly triggered one.
    expect(regenerateArtifactMock).not.toHaveBeenCalled();
    expect(container.querySelector('[aria-label="Regenerating artifact"]')).not.toBeNull();
    expect(container.querySelector('[aria-label="Refresh artifact"]')).toBeNull();
  });

  it("does not resume the spinner when getArtifact reports no running task", async () => {
    getArtifactMock.mockResolvedValue(
      makeArtifact({
        refresh_intent: "whole_artifact",
        origin_intent: { refresh_prompt: "Summarize this week's metrics" },
        running_task_id: null,
      }),
    );
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: "agent-1", artifactId: "artifact-1", onClose: vi.fn() }),
      );
    });
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(container.querySelector('[aria-label="Regenerating artifact"]')).toBeNull();
    expect(container.querySelector('[aria-label="Refresh artifact"]')).not.toBeNull();
  });

  it("shows an error toast and clears the regenerating state when a regenerate run fails", async () => {
    getArtifactMock.mockResolvedValue(
      makeArtifact({
        refresh_intent: "whole_artifact",
        origin_intent: { refresh_prompt: "Summarize this week's metrics" },
      }),
    );
    regenerateArtifactMock.mockRejectedValue(new Error("API 409: not refreshable"));
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: "agent-1", artifactId: "artifact-1", onClose: vi.fn() }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    const refreshBtn = container.querySelector('[aria-label="Refresh artifact"]') as HTMLButtonElement;
    expect(refreshBtn).not.toBeNull();

    await act(async () => {
      refreshBtn.click();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(regenerateArtifactMock).toHaveBeenCalledWith("agent-1", "artifact-1");
    // A toast surfaces the failure regardless of whether the chat panel is
    // open (it isn't, in this test) — see the `regen.status === "error"`
    // effect in ArtifactRenderer.
    expect(container.querySelector('[data-testid="artifact-regen-error-toast"]')?.textContent).toBe(
      "API 409: not refreshable",
    );
    // The "Regenerating…" busy/disabled state must not hang after the error.
    expect(container.querySelector('[aria-label="Refresh artifact"]')).not.toBeNull();
    expect(container.querySelector('[aria-label="Regenerating artifact"]')).toBeNull();
    expect((container.querySelector('[aria-label="Refresh artifact"]') as HTMLButtonElement).disabled).toBe(false);
    // The last-good body must NOT be blanked by the error — only replaced by
    // an error state when there was never a successful render to begin with.
    expect(container.querySelector('[data-testid="artifact-body-metric"]')).not.toBeNull();
  });

  it("disables the Undo button when undo_available is false, enables it when true", async () => {
    getArtifactMock.mockResolvedValue(makeArtifact({ undo_available: false }));
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: "agent-1", artifactId: "artifact-1", onClose: vi.fn() }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    let undoBtn = container.querySelector('[aria-label="Undo last edit"]') as HTMLButtonElement;
    expect(undoBtn).not.toBeNull();
    expect(undoBtn.disabled).toBe(true);

    getArtifactMock.mockResolvedValue(makeArtifact({ undo_available: true }));
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: "agent-1", artifactId: "artifact-2", onClose: vi.fn() }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    undoBtn = container.querySelector('[aria-label="Undo last edit"]') as HTMLButtonElement;
    expect(undoBtn.disabled).toBe(false);
  });

  it("clicking Undo posts to the undo endpoint, re-renders via getArtifact, and updates enabled state", async () => {
    getArtifactMock.mockResolvedValueOnce(makeArtifact({ undo_available: true, title: "Weekly metrics" }));
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: "agent-1", artifactId: "artifact-1", onClose: vi.fn() }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    undoArtifactMock.mockResolvedValue({});
    getArtifactMock.mockResolvedValueOnce(
      makeArtifact({
        undo_available: false,
        title: "Weekly metrics (reverted)",
        payload: { metrics: [{ label: "Revenue", value: 42 }] },
      }),
    );

    const undoBtn = container.querySelector('[aria-label="Undo last edit"]') as HTMLButtonElement;
    expect(undoBtn.disabled).toBe(false);
    await act(async () => {
      undoBtn.click();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(undoArtifactMock).toHaveBeenCalledWith("agent-1", "artifact-1");
    // Re-render goes through the same getArtifact fetch path as Refresh —
    // no separate render path, no duplicate card.
    expect(getArtifactMock).toHaveBeenCalledTimes(2);
    expect(container.textContent).toContain("Weekly metrics (reverted)");
    expect(container.querySelectorAll('[data-testid="artifact-body-metric"]').length).toBe(1);
    // Enabled state now reflects the freshly re-fetched undo_available.
    expect((container.querySelector('[aria-label="Undo last edit"]') as HTMLButtonElement).disabled).toBe(true);
  });

  it("disables the Undo button on a 409 (nothing to undo) instead of surfacing an error", async () => {
    getArtifactMock.mockResolvedValueOnce(makeArtifact({ undo_available: true }));
    await act(async () => {
      root.render(
        React.createElement(ArtifactPreview, { agentId: "agent-1", artifactId: "artifact-1", onClose: vi.fn() }),
      );
    });
    await act(async () => {
      await Promise.resolve();
    });

    undoArtifactMock.mockRejectedValue(new MockApiError(409, "nothing to undo"));

    const undoBtn = container.querySelector('[aria-label="Undo last edit"]') as HTMLButtonElement;
    await act(async () => {
      undoBtn.click();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(undoArtifactMock).toHaveBeenCalledWith("agent-1", "artifact-1");
    // Only the trigger POST ran — no follow-up getArtifact re-render on 409.
    expect(getArtifactMock).toHaveBeenCalledTimes(1);
    expect((container.querySelector('[aria-label="Undo last edit"]') as HTMLButtonElement).disabled).toBe(true);
    expect(container.textContent).not.toContain("nothing to undo");
  });
});
