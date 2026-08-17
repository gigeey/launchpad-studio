// @vitest-environment jsdom
//
// These drive the real store functions rather than setting store state by hand,
// because the thing worth asserting is that the live path arrives at the right
// UI — a unit test of the reducer would have passed just as happily while
// Linux users were shown a phantom "Update available" banner.
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import React from "react";
import { createRoot } from "react-dom/client";
import { act } from "react";

const check = vi.fn();
const relaunch = vi.fn();

vi.mock("@tauri-apps/plugin-updater", () => ({ check: (...a: unknown[]) => check(...a) }));
vi.mock("@tauri-apps/plugin-process", () => ({ relaunch: (...a: unknown[]) => relaunch(...a) }));

vi.mock("framer-motion", () => ({
  motion: {
    div: ({ children, ...rest }: React.HTMLAttributes<HTMLDivElement>) =>
      React.createElement("div", rest, children),
  },
  AnimatePresence: ({ children }: { children: React.ReactNode }) =>
    React.createElement(React.Fragment, null, children),
}));

import { UpdateNotification } from "../UpdateNotification";
import {
  useUpdateStore,
  checkForUpdates,
  downloadAndInstallUpdate,
  relaunchApp,
} from "../../stores/updateStore";

/** Minimal stand-in for the plugin's Update object. */
function fakeUpdate(downloadAndInstall: () => Promise<void>) {
  return {
    version: "1.2.0",
    currentVersion: "1.1.0",
    body: "notes",
    downloadAndInstall,
  };
}

describe("UpdateNotification", () => {
  let container: HTMLDivElement;
  let root: ReturnType<typeof createRoot>;
  let consoleError: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    check.mockReset();
    relaunch.mockReset();
    consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
    useUpdateStore.setState({
      status: "none",
      update: null,
      newVersion: null,
      currentVersion: null,
      releaseNotes: null,
      error: null,
      downloadProgress: null,
      dismissed: false,
    });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => { root.unmount(); });
    document.body.removeChild(container);
    consoleError.mockRestore();
  });

  async function mount() {
    await act(async () => { root.render(React.createElement(UpdateNotification)); });
  }

  // The Linux/Windows case: the manifest lists no artifact for the running
  // target, so check() throws on every call whether or not an update exists.
  it("renders nothing when the update check itself fails", async () => {
    check.mockRejectedValue(new Error('TargetsNotFound(["linux-x86_64-appimage", "linux-x86_64"])'));
    await mount();
    await act(async () => { await checkForUpdates(); });

    expect(useUpdateStore.getState().status).toBe("checkFailed");
    expect(container.textContent).toBe("");
    expect(container.querySelector("button")).toBeNull();
    expect(consoleError).toHaveBeenCalled(); // logged, not silently dropped
  });

  it("renders the banner when an update is genuinely available", async () => {
    check.mockResolvedValue(fakeUpdate(async () => {}));
    await mount();
    await act(async () => { await checkForUpdates(); });

    expect(container.textContent).toContain("Update available");
    expect(container.textContent).toContain("1.1.0");
  });

  it("stays silent when no update is available", async () => {
    check.mockResolvedValue(null);
    await mount();
    await act(async () => { await checkForUpdates(); });

    expect(useUpdateStore.getState().status).toBe("none");
    expect(container.textContent).toBe("");
  });

  // A failed install is the one failure the user can act on, so it does show —
  // but it must not claim an update is "available" when the attempt just failed.
  it("names an install failure as a failure, and offers a retry that has an update to retry", async () => {
    check.mockResolvedValue(fakeUpdate(async () => { throw new Error("disk full"); }));
    await mount();
    await act(async () => { await checkForUpdates(); });
    await act(async () => { await downloadAndInstallUpdate(); });

    expect(useUpdateStore.getState().status).toBe("error");
    expect(container.textContent).toContain("Update failed to install");
    expect(container.textContent).not.toContain("Update available");
    expect(container.textContent).toContain("disk full");
    // The retry button calls downloadAndInstallUpdate, which no-ops when there
    // is no pending update. Assert there is one, so the button does something.
    expect(useUpdateStore.getState().update).not.toBeNull();
    expect(container.textContent).toContain("Retry");
  });

  it("keeps the installed state when only the relaunch fails", async () => {
    check.mockResolvedValue(fakeUpdate(async () => {}));
    relaunch.mockRejectedValue(new Error("relaunch blocked"));
    await mount();
    await act(async () => { await checkForUpdates(); });
    await act(async () => { await downloadAndInstallUpdate(); });
    await act(async () => { await relaunchApp(); });

    expect(useUpdateStore.getState().status).toBe("installed");
    expect(container.textContent).toContain("Update installed — restart to apply");
    expect(container.textContent).toContain("relaunch blocked");
    expect(container.textContent).toContain("Relaunch Now");
  });
});
