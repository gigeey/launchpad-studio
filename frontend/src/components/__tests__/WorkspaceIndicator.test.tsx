// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

const getActiveWorkspace = vi.fn();
const getWorkspacesMock = vi.fn();
const activateWorkspaceMock = vi.fn();

vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return {
    ...actual,
    BASE_URL: "http://localhost:3001",
    getActiveWorkspace: (...a: unknown[]) => getActiveWorkspace(...a),
    getWorkspaces: (...a: unknown[]) => getWorkspacesMock(...a),
    activateWorkspace: (...a: unknown[]) => activateWorkspaceMock(...a),
  };
});

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...a: unknown[]) => invokeMock(...a),
}));

// Tooltip only mounts its `label` into the DOM after a 700ms hover warm-up
// (see ui/Tooltip.tsx), which this suite doesn't need to re-verify — it's
// covered by whatever exercises Tooltip directly. Rendering `label`
// unconditionally lets these tests assert the *content* WorkspaceIndicator
// hands to it without depending on hover timing.
vi.mock("../ui/Tooltip", () => ({
  Tooltip: ({ children, label }: { children: React.ReactNode; label: React.ReactNode }) =>
    React.createElement(React.Fragment, null, children, React.createElement("div", { "data-testid": "tooltip-label" }, label)),
}));

import { WorkspaceIndicator } from "../WorkspaceIndicator";
import { useBannerStore } from "../../stores/bannerStore";
import { WORKSPACE_API_MESSAGES } from "./workspaceApiMessages.fixtures";

// "default" has no emoji — the new default state — so it exercises the
// letter tile; "other" has opted into an emoji, exercising the full-bleed
// state.
const WORKSPACES = [
  { id: "default", name: "Default", path: "/Users/x/.launchpad_studio", color: "#3B82F6", emoji: null },
  { id: "other", name: "Client Project", path: "/Users/x/client", color: "#22C55E", emoji: "🌱" },
];

describe("WorkspaceIndicator", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    getActiveWorkspace.mockReset();
    getWorkspacesMock.mockReset().mockResolvedValue({ workspaces: WORKSPACES, active: "default" });
    activateWorkspaceMock.mockReset();
    invokeMock.mockReset();
    useBannerStore.setState({ banners: [], dismissed: new Set() });
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

  async function render() {
    await act(async () => {
      root.render(React.createElement(WorkspaceIndicator));
    });
  }

  /** Let pending promise chains (mock resolutions, the state updates they
   *  trigger) settle, with React tracking every resulting update. */
  async function flush() {
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
  }

  function tile(): HTMLElement {
    const el = container.querySelector('[data-testid="workspace-tile"]');
    if (!el) throw new Error("workspace tile not rendered");
    return el as HTMLElement;
  }

  async function clickTile() {
    await act(async () => {
      tile().dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
  }

  function row(id: string): HTMLButtonElement {
    const el = document.body.querySelector(`[data-testid="workspace-row-${id}"]`);
    if (!el) throw new Error(`workspace row "${id}" not rendered`);
    return el as HTMLButtonElement;
  }

  function findButton(text: string): HTMLButtonElement {
    const button = Array.from(document.body.querySelectorAll("button")).find((b) => b.textContent?.trim() === text);
    if (!button) throw new Error(`no button with text "${text}" found`);
    return button as HTMLButtonElement;
  }

  async function click(button: HTMLElement) {
    await act(async () => {
      button.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
  }

  it("renders nothing while the fetch is in flight", async () => {
    getActiveWorkspace.mockReturnValue(new Promise(() => {})); // never resolves
    await render();
    expect(container.textContent).toBe("");
  });

  it("renders nothing if the fetch fails", async () => {
    getActiveWorkspace.mockRejectedValue(new Error("network error"));
    await render();
    expect(container.textContent).toBe("");
  });

  describe("provenance: registry", () => {
    it("shows the registry workspace name", async () => {
      getActiveWorkspace.mockResolvedValue({
        path: "/Users/x/.launchpad_studio",
        provenance: "registry",
        name: "Client Project",
      });
      await render();
      const tooltip = container.querySelector('[data-testid="tooltip-label"]')?.textContent ?? "";
      expect(tooltip).toContain("Client Project");
    });
  });

  describe("provenance: home_default", () => {
    it("reads as the default profile instead of showing a null name as an empty gap", async () => {
      getActiveWorkspace.mockResolvedValue({
        path: "/Users/x/.launchpad_studio",
        provenance: "home_default",
        name: null,
      });
      await render();
      const tooltip = container.querySelector('[data-testid="tooltip-label"]')?.textContent ?? "";
      expect(tooltip).toContain("Default profile");
    });
  });

  describe("provenance: env_override", () => {
    const envOverrideResponse = {
      path: "/Users/x/.launchpad_studio-tools",
      provenance: "env_override",
      name: null,
    };

    it("shows the resolved path rather than a registry name", async () => {
      getActiveWorkspace.mockResolvedValue(envOverrideResponse);
      await render();
      const tooltip = container.querySelector('[data-testid="tooltip-label"]')?.textContent ?? "";
      expect(tooltip).toContain("/Users/x/.launchpad_studio-tools");
    });

    it("demotes the warning to a small corner badge rather than replacing the whole tile", async () => {
      getActiveWorkspace.mockResolvedValue(envOverrideResponse);
      await render();
      // The tile itself still renders (clickable, opens the switcher) —
      // the amber affordance is only the corner badge layered on top of it.
      expect(tile()).toBeTruthy();
      expect(container.querySelector('[data-testid="workspace-tile-env-badge"]')).toBeTruthy();
    });

    it("names LAUNCHPAD_STUDIO_DATA_DIR literally and explains the switch requirement in the tooltip", async () => {
      getActiveWorkspace.mockResolvedValue(envOverrideResponse);
      await render();
      const tooltipText = container.querySelector('[data-testid="tooltip-label"]')?.textContent ?? "";
      expect(tooltipText).toContain("LAUNCHPAD_STUDIO_DATA_DIR");
      expect(tooltipText).toContain(envOverrideResponse.path);
      expect(tooltipText).toMatch(/unsetting.*relaunch/i);
    });
  });

  // A startup crash-recovery pin looks identical to `env_override` on the
  // wire shape (both are a resolved default-root path with no registry
  // name) but MUST NOT trip any of the env-override disabling — that's the
  // exact regression this task exists to prevent. See
  // `ao_protocol::data_root::RootProvenance::Fallback`'s doc comment for the
  // backend half of this contract.
  describe("provenance: fallback", () => {
    const failedRoot = "/Users/x/client";
    const fallbackResponse = {
      path: "/Users/x/.launchpad_studio",
      provenance: "fallback",
      name: null,
      startup_fallback: {
        failed_root: failedRoot,
        fallback_root: "/Users/x/.launchpad_studio",
        error: "permission denied",
      },
    };

    it("does not show the env-override badge, but does show a distinct fallback badge on the tile", async () => {
      getActiveWorkspace.mockResolvedValue(fallbackResponse);
      await render();
      expect(container.querySelector('[data-testid="workspace-tile-env-badge"]')).toBeFalsy();
      expect(container.querySelector('[data-testid="workspace-tile-fallback-badge"]')).toBeTruthy();
    });

    it("explains the recovery path in the tooltip rather than the env-pin copy", async () => {
      getActiveWorkspace.mockResolvedValue(fallbackResponse);
      await render();
      const tooltipText = container.querySelector('[data-testid="tooltip-label"]')?.textContent ?? "";
      expect(tooltipText).toMatch(/could not be opened/i);
      expect(tooltipText).not.toMatch(/pinned by/i);
    });

    it("keeps every switcher control enabled — this is the recovery path, not a locked state", async () => {
      getActiveWorkspace.mockResolvedValue(fallbackResponse);
      await render();
      await clickTile();

      // "default" is the active row here (fallback resolved to the same
      // path as the "default" fixture workspace), so it's disabled for the
      // ordinary reason every active row is — not because of fallback mode.
      expect(row("default").disabled).toBe(true);
      expect(row("default").title).toBe("Active profile");
      // "other" is NOT the active row and must stay fully clickable — an
      // env override would have disabled this.
      const otherRow = row("other");
      expect(otherRow.disabled).toBe(false);
      expect(otherRow.title.toLowerCase()).not.toContain("pinned by");

      const createAction = document.body.querySelector('[data-testid="workspace-create-action"]') as HTMLButtonElement;
      expect(createAction.disabled).toBe(false);
    });

    it("shows a distinct recovery banner naming the failed workspace, never the env-override copy", async () => {
      getActiveWorkspace.mockResolvedValue(fallbackResponse);
      await render();
      await clickTile();

      const banner = document.body.querySelector('[data-testid="workspace-fallback-banner"]');
      expect(banner).toBeTruthy();
      const bannerText = banner?.textContent ?? "";
      expect(bannerText).toContain(
        "Your selected workspace could not be opened, so the app started on the default one. Pick a workspace below to recover.",
      );
      expect(bannerText).toContain(failedRoot);
      expect(bannerText).not.toMatch(/unset it and relaunch/i);

      // The env-override banner must never render alongside the fallback
      // one — the two states are mutually exclusive.
      expect(document.body.textContent).not.toContain("Unset it and relaunch to switch profiles.");
    });

    it("omits the raw error string when it isn't short and human-legible", async () => {
      getActiveWorkspace.mockResolvedValue({
        ...fallbackResponse,
        startup_fallback: {
          ...fallbackResponse.startup_fallback,
          error: "A".repeat(200), // far past the human-legible length cutoff
        },
      });
      await render();
      await clickTile();

      const bannerText = document.body.querySelector('[data-testid="workspace-fallback-banner"]')?.textContent ?? "";
      expect(bannerText).toContain(failedRoot);
      expect(bannerText).not.toContain("A".repeat(200));
    });

    it("still allows activating a different workspace to actually recover", async () => {
      getActiveWorkspace.mockResolvedValue(fallbackResponse);
      activateWorkspaceMock.mockResolvedValue({ workspaces: WORKSPACES, active: "other" });
      invokeMock.mockResolvedValue("restarting");
      await render();
      await clickTile();

      await click(row("other"));
      expect(document.body.textContent).toContain("Restart to switch profile?");

      await click(findButton("Restart & switch"));
      await flush();

      expect(activateWorkspaceMock).toHaveBeenCalledWith("other");
      expect(invokeMock).toHaveBeenCalledWith("restart_app");
    });
  });

  describe("rail tile", () => {
    it("renders as a clickable rail tile sized like the other rail rows, not the old header pill", async () => {
      getActiveWorkspace.mockResolvedValue({ path: "/Users/x/.launchpad_studio", provenance: "registry", name: "Default" });
      await render();
      const wrapper = tile().querySelector("div");
      expect(wrapper?.className).toContain("w-[36px]");
      expect(wrapper?.className).toContain("h-[36px]");
      expect(tile().getAttribute("role")).toBe("button");
    });

    it("colors the tile from the matching workspace entry and shows its letter when no emoji is set", async () => {
      getActiveWorkspace.mockResolvedValue({ path: "/Users/x/.launchpad_studio", provenance: "registry", name: "Default" });
      await render();
      // tile > wrapper (relative w-[36px] h-[36px]) > WorkspaceAvatar's own root.
      const avatarBox = tile().firstElementChild?.firstElementChild as HTMLDivElement;
      expect(avatarBox.style.backgroundColor).toBe("rgb(59, 130, 246)"); // #3B82F6
      expect(avatarBox.style.borderRadius).toBe("10px"); // size(36) * 0.28, rounded
      expect(tile().textContent).toBe("D");
    });

    it("shows the emoji full-bleed with no background when the matching entry has one set", async () => {
      getActiveWorkspace.mockResolvedValue({ path: "/Users/x/client", provenance: "registry", name: "Client Project" });
      await render();
      const avatarBox = tile().firstElementChild?.firstElementChild as HTMLDivElement;
      expect(avatarBox.style.backgroundColor).toBe("");
      expect(avatarBox.style.borderRadius).toBe("");
      expect(tile().textContent).toBe("🌱");
    });
  });

  describe("switcher popover", () => {
    beforeEach(() => {
      getActiveWorkspace.mockResolvedValue({ path: "/Users/x/.launchpad_studio", provenance: "registry", name: "Default" });
    });

    it("opens on tile click and lists every workspace", async () => {
      await render();
      expect(document.body.textContent).not.toContain("Client Project");
      await clickTile();
      expect(document.body.textContent).toContain("Default");
      expect(document.body.textContent).toContain("Client Project");
      expect(document.body.textContent).toContain("Create workspace");
    });

    it("marks the active workspace and disables switching to itself", async () => {
      await render();
      await clickTile();
      const activeRow = row("default");
      expect(activeRow.disabled).toBe(true);
      expect(activeRow.title).toBe("Active profile");
      const otherRow = row("other");
      expect(otherRow.disabled).toBe(false);
    });

    it("activates then restarts, in that order, when a non-active row is confirmed (release-equivalent: restart_app reports it actually restarted)", async () => {
      activateWorkspaceMock.mockResolvedValue({ workspaces: WORKSPACES, active: "other" });
      // "restarting" is what the Rust `restart_app` command reports outside
      // `cfg!(debug_assertions)` — i.e. a packaged/release build. See
      // frontend/src-tauri/src/lib.rs's `RestartOutcome`.
      invokeMock.mockResolvedValue("restarting");
      await render();
      await clickTile();

      await click(row("other"));
      expect(document.body.textContent).toContain("restarts Launchpad Studio");
      expect(activateWorkspaceMock).not.toHaveBeenCalled();

      await click(findButton("Restart & switch"));
      await flush();

      expect(activateWorkspaceMock).toHaveBeenCalledWith("other");
      expect(invokeMock).toHaveBeenCalledWith("restart_app");
      const activateOrder = activateWorkspaceMock.mock.invocationCallOrder[0];
      const restartOrder = invokeMock.mock.invocationCallOrder[0];
      expect(activateOrder).toBeLessThan(restartOrder);
      expect(useBannerStore.getState().banners).toHaveLength(0);
    });

    it("activates but does not restart itself when restart_app reports a dev build, and explains why in a banner", async () => {
      activateWorkspaceMock.mockResolvedValue({ workspaces: WORKSPACES, active: "other" });
      // What the Rust `restart_app` command reports under
      // `cfg!(debug_assertions)` (`npm run tauri dev`) instead of actually
      // restarting — see that command's doc comment for why restarting for
      // real there would just reconnect to a torn-down dev server.
      invokeMock.mockResolvedValue("dev_restart_required");
      await render();
      await clickTile();

      await click(row("other"));
      await click(findButton("Restart & switch"));
      await flush();

      expect(activateWorkspaceMock).toHaveBeenCalledWith("other");
      expect(invokeMock).toHaveBeenCalledWith("restart_app");

      // Reads as expected-and-intentional, not as a failure — distinct
      // wording and severity from the "restart ipc unavailable" banner
      // below.
      const banners = useBannerStore.getState().banners;
      expect(banners).toHaveLength(1);
      expect(banners[0].variant).toBe("info");
      expect(String(banners[0].message)).toMatch(/restart your dev server/i);
      expect(String(banners[0].message)).not.toMatch(/quit and reopen/i);
    });

    it("raises a persistent banner if the pointer switched but the restart call failed", async () => {
      activateWorkspaceMock.mockResolvedValue({ workspaces: WORKSPACES, active: "other" });
      invokeMock.mockRejectedValue(new Error("restart ipc unavailable"));
      await render();
      await clickTile();

      await click(row("other"));
      await click(findButton("Restart & switch"));
      await flush();

      const banners = useBannerStore.getState().banners;
      expect(banners).toHaveLength(1);
      expect(banners[0].dismissible).toBe(false);
      expect(banners[0].variant).toBe("error");
      expect(String(banners[0].message)).toMatch(/quit and reopen/i);
    });

    it("does not call activate before the restart warning is confirmed", async () => {
      await render();
      await clickTile();

      await click(row("other"));
      await click(findButton("Cancel"));

      expect(activateWorkspaceMock).not.toHaveBeenCalled();
      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("states the restart on the row itself, before the confirm dialog ever opens", async () => {
      await render();
      await clickTile();

      // The row-level control's own tooltip names the restart — the user
      // isn't only warned after already committing to the confirm dialog.
      expect(row("other").title.toLowerCase()).toContain("restart");
    });

    it("renders the backend's exact 409 message, pid included, when another running instance has the profile open", async () => {
      activateWorkspaceMock.mockRejectedValue(new Error(WORKSPACE_API_MESSAGES.activeElsewhere(4242)));
      await render();
      await clickTile();

      await click(row("other"));
      await click(findButton("Restart & switch"));
      await flush();

      expect(document.body.textContent).toContain(WORKSPACE_API_MESSAGES.activeElsewhere(4242));
      // The confirm dialog must still be open on failure — the pointer
      // wasn't moved, so there's nothing to restart into and no reason to
      // close it.
      expect(document.body.textContent).toContain("Restart to switch profile?");
      expect(invokeMock).not.toHaveBeenCalled();
    });

    it("shows the pre-flight probe's failure and does not restart when the target data root can't be opened", async () => {
      // Mirrors `AoError::WorkspaceActivationTargetUnopenable` (400) — the
      // server's pre-flight probe in `activate_workspace` refused before
      // writing the registry pointer.
      activateWorkspaceMock.mockRejectedValue(
        new Error(WORKSPACE_API_MESSAGES.activationTargetUnopenable("/Users/x/client", "permission denied")),
      );
      await render();
      await clickTile();

      await click(row("other"));
      await click(findButton("Restart & switch"));
      await flush();

      expect(document.body.textContent).toContain(
        WORKSPACE_API_MESSAGES.activationTargetUnopenable("/Users/x/client", "permission denied"),
      );
      expect(document.body.textContent).toContain("Restart to switch profile?");
      expect(invokeMock).not.toHaveBeenCalled();
      expect(useBannerStore.getState().banners).toHaveLength(0);

      // Dismissible (the confirm dialog's own Cancel affordance is still
      // present), and the previous workspace is still the one marked
      // active — nothing about this state looks as though the switch
      // happened.
      expect(() => findButton("Cancel")).not.toThrow();
      expect(row("default").title).toBe("Active profile");
    });

    it("fails closed without restarting when activation rejects with an unrecognized error shape", async () => {
      // Not an `Error` instance and not the `{error: string}` shape
      // `throwApiError` normally produces — e.g. a thrown plain object, or
      // some other unexpected rejection value. `handleActivate` must still
      // treat this as a terminal failure rather than falling through to a
      // restart.
      activateWorkspaceMock.mockRejectedValue({ unexpected: "shape" });
      await render();
      await clickTile();

      await click(row("other"));
      await click(findButton("Restart & switch"));
      await flush();

      expect(document.body.textContent).toContain("Failed to switch profile");
      expect(document.body.textContent).toContain("Restart to switch profile?");
      expect(invokeMock).not.toHaveBeenCalled();
      expect(useBannerStore.getState().banners).toHaveLength(0);
    });
  });

  describe("under an env-override data root", () => {
    beforeEach(() => {
      getActiveWorkspace.mockResolvedValue({
        path: "/Users/x/.launchpad_studio-tools",
        provenance: "env_override",
        name: null,
      });
    });

    it("still renders the popover and lists workspaces, but disables activate and create", async () => {
      await render();
      await clickTile();

      expect(document.body.textContent).toContain("Default");
      expect(document.body.textContent).toContain("Client Project");

      const defaultRow = row("default");
      const otherRow = row("other");
      const createAction = document.body.querySelector('[data-testid="workspace-create-action"]') as HTMLButtonElement;

      expect(defaultRow.disabled).toBe(true);
      expect(otherRow.disabled).toBe(true);
      expect(createAction.disabled).toBe(true);
      expect(defaultRow.title.toLowerCase()).toContain("pinned by");

      // Clicking a disabled row must not open the confirm dialog or call
      // activateWorkspace.
      await click(otherRow);
      expect(document.body.textContent).not.toContain("Restart to switch profile?");
      expect(activateWorkspaceMock).not.toHaveBeenCalled();
    });
  });
});
