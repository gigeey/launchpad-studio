// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

const createWorkspaceMock = vi.fn();
const renameWorkspaceMock = vi.fn();

vi.mock("../../lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/api")>();
  return {
    ...actual,
    createWorkspace: (...a: unknown[]) => createWorkspaceMock(...a),
    renameWorkspace: (...a: unknown[]) => renameWorkspaceMock(...a),
  };
});

const openDialogMock = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...a: unknown[]) => openDialogMock(...a),
}));

import { WorkspaceEditModal } from "../WorkspaceEditModal";
import { useBannerStore } from "../../stores/bannerStore";
import { ApiError, WORKSPACE_COLOR_PALETTE, type WorkspaceEntry } from "../../lib/api";
import { WORKSPACE_API_MESSAGES } from "./workspaceApiMessages.fixtures";

const WORKSPACE: WorkspaceEntry = {
  id: "other",
  name: "Client Project",
  path: "/Users/x/client",
  color: "#22C55E",
  emoji: "🌱",
};

// React attaches a value tracker to input elements; setting `.value`
// directly (bypassing the native setter it hooks) leaves the tracker
// thinking nothing changed, so `onChange` never fires. Going through the
// native setter first, as React's own testing utilities do, is required.
function setInputValue(input: HTMLInputElement, value: string) {
  const nativeSetter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value")!.set!;
  nativeSetter.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

function findButton(text: string): HTMLButtonElement {
  const button = Array.from(document.body.querySelectorAll("button")).find((b) => b.textContent?.trim() === text);
  if (!button) throw new Error(`no button with text "${text}" found`);
  return button as HTMLButtonElement;
}

function byTestId(id: string): HTMLElement {
  const el = document.body.querySelector(`[data-testid="${id}"]`);
  if (!el) throw new Error(`no element with data-testid "${id}" found`);
  return el as HTMLElement;
}

async function click(el: HTMLElement) {
  await act(async () => {
    el.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

/** Let pending promise chains (mock resolutions, the state updates they
 *  trigger) settle, with React tracking every resulting update. */
async function flush() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

describe("WorkspaceEditModal", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;

  beforeEach(() => {
    createWorkspaceMock.mockReset();
    renameWorkspaceMock.mockReset();
    openDialogMock.mockReset();
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

  async function renderModal(overrides: Partial<React.ComponentProps<typeof WorkspaceEditModal>> = {}) {
    const onClose = vi.fn();
    const onSaved = vi.fn();
    await act(async () => {
      root.render(
        React.createElement(WorkspaceEditModal, {
          open: true,
          mode: "create",
          workspace: null,
          onClose,
          onSaved,
          ...overrides,
        }),
      );
    });
    return { onClose, onSaved };
  }

  describe("create mode", () => {
    it("opens with nothing preselected — the live preview shows the letter avatar, not an emoji", async () => {
      await renderModal();
      await act(async () => setInputValue(byTestId("workspace-edit-name") as HTMLInputElement, "New One"));
      expect(byTestId("workspace-edit-avatar-preview").textContent).toBe("N");
      // Nothing to clear yet — the affordance is disabled, not hidden.
      expect((byTestId("workspace-edit-clear-emoji") as HTMLButtonElement).disabled).toBe(true);
    });

    it("submits the typed name, the picked path, the chosen color, and no emoji by default", async () => {
      openDialogMock.mockResolvedValue("/Users/x/new-workspace");
      createWorkspaceMock.mockResolvedValue({
        id: "new",
        name: "New One",
        path: "/Users/x/new-workspace",
        color: WORKSPACE_COLOR_PALETTE[3],
        emoji: null,
        adopted: false,
      });
      const { onSaved, onClose } = await renderModal();

      await act(async () => setInputValue(byTestId("workspace-edit-name") as HTMLInputElement, "New One"));

      const pathInput = byTestId("workspace-edit-path") as HTMLInputElement;
      expect(pathInput.readOnly).toBe(true);
      await click(pathInput);
      await flush();
      expect(openDialogMock).toHaveBeenCalledWith({ directory: true, multiple: false });
      expect((byTestId("workspace-edit-path") as HTMLInputElement).value).toBe("/Users/x/new-workspace");

      // Pick a non-default swatch so the test actually exercises selection
      // wiring rather than just re-asserting the create-mode default.
      await click(byTestId(`workspace-color-${WORKSPACE_COLOR_PALETTE[3]}`));

      await click(findButton("Create"));
      await flush();

      expect(createWorkspaceMock).toHaveBeenCalledWith(
        "New One",
        "/Users/x/new-workspace",
        WORKSPACE_COLOR_PALETTE[3],
        null,
      );
      expect(onSaved).toHaveBeenCalled();
      expect(onClose).toHaveBeenCalled();
    });

    it("won't submit without a name or a chosen path", async () => {
      await renderModal();

      await click(findButton("Create"));
      expect(createWorkspaceMock).not.toHaveBeenCalled();
      expect(document.body.textContent).toContain("Name is required");

      await act(async () => setInputValue(byTestId("workspace-edit-name") as HTMLInputElement, "New One"));
      await click(findButton("Create"));
      expect(createWorkspaceMock).not.toHaveBeenCalled();
      expect(document.body.textContent).toContain("Choose a folder");
    });
  });

  describe("rename mode", () => {
    it("pre-fills from the workspace and submits the new name/color/emoji without ever touching the path", async () => {
      renameWorkspaceMock.mockResolvedValue({ ...WORKSPACE, name: "Renamed" });
      const { onSaved, onClose } = await renderModal({ mode: "rename", workspace: WORKSPACE });

      const pathInput = byTestId("workspace-edit-path") as HTMLInputElement;
      expect(pathInput.value).toBe(WORKSPACE.path);
      expect(pathInput.disabled).toBe(true);
      // Clicking the (disabled) path field must never open the folder picker.
      await click(pathInput);
      expect(openDialogMock).not.toHaveBeenCalled();

      const nameInput = byTestId("workspace-edit-name") as HTMLInputElement;
      expect(nameInput.value).toBe(WORKSPACE.name);
      await act(async () => setInputValue(nameInput, "Renamed"));

      await click(findButton("Save"));
      await flush();

      expect(renameWorkspaceMock).toHaveBeenCalledWith(WORKSPACE.id, "Renamed", WORKSPACE.color, WORKSPACE.emoji);
      expect(renameWorkspaceMock.mock.calls[0]).toHaveLength(4);
      expect(onSaved).toHaveBeenCalled();
      expect(onClose).toHaveBeenCalled();
    });

    it("pre-fills the picker/preview from the workspace's existing emoji, full-bleed with no background", async () => {
      await renderModal({ mode: "rename", workspace: WORKSPACE });
      expect(byTestId("workspace-edit-avatar-preview").textContent).toBe(WORKSPACE.emoji);
      expect((byTestId("workspace-edit-clear-emoji") as HTMLButtonElement).disabled).toBe(false);
    });

    it("lets the user clear a set emoji back to the letter avatar — a one-way door without this", async () => {
      renameWorkspaceMock.mockResolvedValue({ ...WORKSPACE, emoji: null });
      const { onSaved } = await renderModal({ mode: "rename", workspace: WORKSPACE });

      await click(byTestId("workspace-edit-clear-emoji"));
      // Cleared — the preview now shows the letter tile, and the affordance
      // disables itself since there's nothing left to clear.
      expect(byTestId("workspace-edit-avatar-preview").textContent).toBe(WORKSPACE.name[0].toUpperCase());
      expect((byTestId("workspace-edit-clear-emoji") as HTMLButtonElement).disabled).toBe(true);

      await click(findButton("Save"));
      await flush();

      // Explicit null reaches the API — the backend distinguishes this from
      // "leave unchanged" (an omitted/undefined argument).
      expect(renameWorkspaceMock).toHaveBeenCalledWith(WORKSPACE.id, WORKSPACE.name, WORKSPACE.color, null);
      expect(onSaved).toHaveBeenCalled();
    });
  });

  describe("errors", () => {
    it("surfaces a 409 pinned-data-root message verbatim, inline and via the banner store, and keeps the modal open", async () => {
      const pinnedMessage =
        "Launchpad Studio is running with a pinned data directory, so it can't change the shared workspace list. Nothing was modified.";
      openDialogMock.mockResolvedValue("/Users/x/new-workspace");
      createWorkspaceMock.mockRejectedValue(new ApiError(409, pinnedMessage));
      const { onSaved, onClose } = await renderModal();

      await act(async () => setInputValue(byTestId("workspace-edit-name") as HTMLInputElement, "New One"));
      await click(byTestId("workspace-edit-path"));
      await flush();

      await click(findButton("Create"));
      await flush();

      expect(byTestId("workspace-edit-error").textContent).toBe(pinnedMessage);
      const banners = useBannerStore.getState().banners;
      expect(banners).toHaveLength(1);
      expect(banners[0].variant).toBe("error");
      expect(String(banners[0].message)).toBe(pinnedMessage);
      // Nothing was actually saved — the modal must stay open (not call
      // onClose/onSaved) so the user can see the message and retry.
      expect(onSaved).not.toHaveBeenCalled();
      expect(onClose).not.toHaveBeenCalled();
    });

    it("surfaces a rename rejection's server message the same way", async () => {
      const message = "cannot rename: workspace not found";
      renameWorkspaceMock.mockRejectedValue(new ApiError(404, message));
      await renderModal({ mode: "rename", workspace: WORKSPACE });

      await click(findButton("Save"));
      await flush();

      expect(byTestId("workspace-edit-error").textContent).toBe(message);
      expect(String(useBannerStore.getState().banners[0]?.message)).toBe(message);
    });

    it("renders the backend's exact 400 message verbatim when the folder isn't empty and isn't a Launchpad workspace", async () => {
      openDialogMock.mockResolvedValue("/Users/x/messy-folder");
      createWorkspaceMock.mockRejectedValue(new ApiError(400, WORKSPACE_API_MESSAGES.NOT_EMPTY_NOT_LAUNCHPAD));
      await renderModal();

      await act(async () => setInputValue(byTestId("workspace-edit-name") as HTMLInputElement, "New One"));
      await click(byTestId("workspace-edit-path"));
      await flush();

      await click(findButton("Create"));
      await flush();

      expect(byTestId("workspace-edit-error").textContent).toBe(WORKSPACE_API_MESSAGES.NOT_EMPTY_NOT_LAUNCHPAD);
    });

    it("renders the pre-existing registry-collision 400 message verbatim", async () => {
      openDialogMock.mockResolvedValue("/Users/x/client");
      createWorkspaceMock.mockRejectedValue(
        new ApiError(400, WORKSPACE_API_MESSAGES.registryCollision("/Users/x/client")),
      );
      await renderModal();

      await act(async () => setInputValue(byTestId("workspace-edit-name") as HTMLInputElement, "Dupe"));
      await click(byTestId("workspace-edit-path"));
      await flush();

      await click(findButton("Create"));
      await flush();

      expect(byTestId("workspace-edit-error").textContent).toBe(
        WORKSPACE_API_MESSAGES.registryCollision("/Users/x/client"),
      );
    });
  });
});
