// @vitest-environment jsdom
//
// The Assets panel gains a new, distinct "Artifacts"
// section alongside the existing Images/Files sections, fetched via
// `listArtifacts(agentId)` and rendered through the shared
// `ArtifactPreview` renderer on click — this pins that extension without
// touching the pre-existing attachment behavior.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import type { Artifact, ArtifactWithPayload } from "../../../types/api";

const listAttachmentsMock = vi.fn();
const getStorageInfoMock = vi.fn();
const listArtifactsMock = vi.fn();
const getArtifactMock = vi.fn();

vi.mock("../../../lib/api", () => ({
  listAttachments: (...a: unknown[]) => listAttachmentsMock(...a),
  getStorageInfo: (...a: unknown[]) => getStorageInfoMock(...a),
  listArtifacts: (...a: unknown[]) => listArtifactsMock(...a),
  getArtifact: (...a: unknown[]) => getArtifactMock(...a),
  deleteAttachment: vi.fn(),
  triggerCleanup: vi.fn(),
  getAttachmentUrl: (agentId: string, id: string) => `mock://attachment/${agentId}/${id}`,
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({ save: vi.fn().mockResolvedValue(null) }));
vi.mock("@tauri-apps/plugin-fs", () => ({ writeFile: vi.fn().mockResolvedValue(undefined) }));

vi.mock("framer-motion", () => ({
  motion: {
    div: ({ children, ...rest }: React.HTMLAttributes<HTMLDivElement>) =>
      React.createElement("div", rest, children),
  },
  AnimatePresence: ({ children }: { children: React.ReactNode }) =>
    React.createElement(React.Fragment, null, children),
}));

// jsdom reports a zero-size scroll container, which would otherwise make
// @tanstack/react-virtual render nothing. Replacing it with a pass-through
// that renders every row keeps this a DOM-content assertion, not a
// virtualization/layout test (virtualization itself is exercised by the
// pre-existing image/file sections this panel already ships).
vi.mock("@tanstack/react-virtual", () => ({
  useVirtualizer: (opts: { count: number }) => ({
    getTotalSize: () => opts.count * 50,
    getVirtualItems: () =>
      Array.from({ length: opts.count }, (_, i) => ({ index: i, start: i * 50, key: i })),
    measureElement: () => {},
  }),
}));

import { AssetsPanel } from "../AssetsPanel";
import { useArtifactStore } from "../../../stores/artifactStore";

function makeArtifact(overrides: Partial<Artifact> = {}): Artifact {
  return {
    id: "artifact-1",
    title: "Weekly report",
    kind: "table",
    format: "json",
    stored_filename: "blob.json",
    size_bytes: 1024,
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
    ...overrides,
  };
}

describe("AssetsPanel — artifacts section", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    listAttachmentsMock.mockReset().mockResolvedValue([]);
    getStorageInfoMock.mockReset().mockResolvedValue({ total_size_bytes: 0 });
    listArtifactsMock.mockReset().mockResolvedValue([]);
    getArtifactMock.mockReset();

    // The artifact list is cached per-agent in a shared store — reset it
    // between tests so nothing leaks across cases.
    useArtifactStore.setState({ byAgent: new Map() });
  });

  afterEach(async () => {
    await act(async () => {
      root.unmount();
    });
    document.body.removeChild(container);
  });

  async function flush() {
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
  }

  it("fetches artifacts via listArtifacts(agentId) and renders an Artifacts section", async () => {
    listArtifactsMock.mockResolvedValue([
      makeArtifact({ id: "artifact-1", title: "Weekly report", kind: "table" }),
      makeArtifact({ id: "artifact-2", title: "Landing page", kind: "html" }),
    ]);

    await act(async () => {
      root.render(React.createElement(AssetsPanel, { agentId: "agent-1" }));
    });
    await flush();

    expect(listArtifactsMock).toHaveBeenCalledWith("agent-1");
    expect(container.textContent).toContain("Artifacts");
    expect(container.textContent).toContain("Weekly report");
    expect(container.textContent).toContain("Landing page");
    expect(container.querySelectorAll('[data-testid="artifact-asset-item"]')).toHaveLength(2);
  });

  it("does not render an Artifacts section when the agent has none", async () => {
    listArtifactsMock.mockResolvedValue([]);

    await act(async () => {
      root.render(React.createElement(AssetsPanel, { agentId: "agent-empty" }));
    });
    await flush();

    expect(container.querySelectorAll('[data-testid="artifact-asset-item"]')).toHaveLength(0);
  });

  it("clicking an artifact row opens it through the shared renderer (fetches by id)", async () => {
    listArtifactsMock.mockResolvedValue([makeArtifact({ id: "artifact-1", title: "Weekly report" })]);
    const withPayload: ArtifactWithPayload = { ...makeArtifact({ id: "artifact-1" }), payload: { rows: [] } };
    getArtifactMock.mockResolvedValue(withPayload);

    await act(async () => {
      root.render(React.createElement(AssetsPanel, { agentId: "agent-1" }));
    });
    await flush();

    const row = container.querySelector('[data-testid="artifact-asset-item"]') as HTMLElement;
    expect(row).not.toBeNull();

    await act(async () => {
      row.click();
    });
    await flush();

    expect(getArtifactMock).toHaveBeenCalledWith("agent-1", "artifact-1");
  });
});
