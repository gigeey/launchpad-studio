// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";
import { ConnectorsTab } from "../ConnectorsTab";
import type { McpServerStatus } from "../../types/connectors";

vi.mock("../../lib/api", () => ({
  listMcpServers: vi.fn(),
  addMcpServer: vi.fn(),
  deleteMcpServer: vi.fn(),
  authorizeMcpServer: vi.fn(),
}));

vi.mock("framer-motion", () => ({
  motion: {
    div: ({ children, ...rest }: React.HTMLAttributes<HTMLDivElement>) =>
      React.createElement("div", rest, children),
  },
  AnimatePresence: ({ children }: { children: React.ReactNode }) =>
    React.createElement(React.Fragment, null, children),
}));

vi.mock("../ui/ConfirmDialog", () => ({
  default: () => null,
}));

const CONNECTED: McpServerStatus = {
  name: "github",
  transport: "stdio",
  endpoint: "npx -y @mcp/github",
  state: "connected",
  tool_names: ["create_issue", "list_prs", "merge_pr"],
  source: "config",
};

const NEEDS_AUTH: McpServerStatus = {
  name: "gdrive",
  transport: "http",
  endpoint: "https://mcp.gdrive.example.com",
  state: "needs_auth",
  tool_names: [],
  source: "config",
};

const ERROR_SERVER: McpServerStatus = {
  name: "broken",
  transport: "stdio",
  endpoint: "bad-cmd",
  state: "error",
  error: "spawn failed: No such file or directory",
  tool_names: [],
  source: "config",
};

const DISABLED: McpServerStatus = {
  name: "slack",
  transport: "http",
  endpoint: "https://mcp.slack.example.com",
  state: "disabled",
  tool_names: [],
  source: "config",
};

const PLUGIN_SOURCE: McpServerStatus = {
  name: "notion",
  transport: "http",
  endpoint: "https://mcp.notion.example.com",
  state: "connected",
  tool_names: ["search_pages"],
  source: "plugin:notion-integration",
};

// A "connected" server whose tools/list happened to succeed unauthenticated —
// the notion-shaped bug this feature exists to recover from. Reauthorize must
// stay reachable even though the badge says Connected.
const CONNECTED_HTTP: McpServerStatus = {
  name: "linear",
  transport: "http",
  endpoint: "https://mcp.linear.app/mcp",
  state: "connected",
  tool_names: ["create_issue"],
  source: "config",
};

const ERROR_HTTP: McpServerStatus = {
  name: "flaky",
  transport: "http",
  endpoint: "https://mcp.flaky.example.com",
  state: "error",
  error: "connection reset",
  tool_names: [],
  source: "config",
};

describe("ConnectorsTab", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => { root.unmount(); });
    document.body.removeChild(container);
  });

  it("shows loading pane while loading", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "loading" },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    expect(container.textContent).toContain("Loading");
  });

  it("shows error message on error state", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "error", message: "Network failed" },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    expect(container.textContent).toContain("Network failed");
  });

  it("shows empty state when no servers", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    expect(container.textContent).toContain("No connectors");
  });

  it("renders a connected server with green badge text", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [CONNECTED] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    expect(container.textContent).toContain("github");
    expect(container.textContent).toContain("Connected");
    expect(container.textContent).toContain("3 tools");
  });

  it("renders a needs_auth server with Authorize button", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [NEEDS_AUTH] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    expect(container.textContent).toContain("gdrive");
    expect(container.textContent).toContain("Needs auth");
    expect(container.textContent).toContain("Authorize");
  });

  it("renders an error server with error badge", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [ERROR_SERVER] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    expect(container.textContent).toContain("broken");
    expect(container.textContent).toContain("Error");
    expect(container.textContent).toContain("Show error");
  });

  it("renders a disabled server with disabled badge", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [DISABLED] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    expect(container.textContent).toContain("slack");
    expect(container.textContent).toContain("Disabled");
  });

  it("hides delete button for plugin-sourced servers", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [PLUGIN_SOURCE] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    expect(container.textContent).toContain("notion");
    expect(container.textContent).toContain("via notion-integration");
    // No delete button for plugin sources
    const deleteBtn = container.querySelector(`[aria-label="Remove notion"]`);
    expect(deleteBtn).toBeNull();
  });

  it("shows delete button for config-sourced servers", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [CONNECTED] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    const deleteBtn = container.querySelector(`[aria-label="Remove github"]`);
    expect(deleteBtn).not.toBeNull();
  });

  // ── Reauthorize affordance ────────────────────────────────────────────────
  //
  // A recovery action must stay reachable even when the app's own health
  // signal is wrong (e.g. a server whose tools/list succeeds unauthenticated
  // and only 401s on real calls, so it never shows as "needs auth").

  it("shows a reauthorize icon for a connected http server", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [CONNECTED_HTTP] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    const reauthBtn = container.querySelector(`[aria-label="Reauthorize linear"]`);
    expect(reauthBtn).not.toBeNull();
  });

  it("shows a reauthorize icon for an error-state http server", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [ERROR_HTTP] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    const reauthBtn = container.querySelector(`[aria-label="Reauthorize flaky"]`);
    expect(reauthBtn).not.toBeNull();
  });

  it("hides the reauthorize icon for a stdio server even when connected", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [CONNECTED] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    const reauthBtn = container.querySelector(`[aria-label="Reauthorize github"]`);
    expect(reauthBtn).toBeNull();
  });

  it("hides the reauthorize icon for a needs_auth server (primary button covers it)", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [NEEDS_AUTH] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    const reauthBtn = container.querySelector(`[aria-label="Reauthorize gdrive"]`);
    expect(reauthBtn).toBeNull();
    // The primary Authorize button must still be present and unaffected.
    expect(container.textContent).toContain("Authorize");
  });

  it("hides the reauthorize icon for a disabled http server", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [DISABLED] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    const reauthBtn = container.querySelector(`[aria-label="Reauthorize slack"]`);
    expect(reauthBtn).toBeNull();
  });

  it("shows the reauthorize icon for a connected plugin-sourced http server", async () => {
    await act(async () => {
      root.render(
        React.createElement(ConnectorsTab, {
          state: { status: "ready", data: [PLUGIN_SOURCE] },
          onRefresh: vi.fn(),
          onRequestAdd: vi.fn(),
        }),
      );
    });
    const reauthBtn = container.querySelector(`[aria-label="Reauthorize notion"]`);
    expect(reauthBtn).not.toBeNull();
  });
});
