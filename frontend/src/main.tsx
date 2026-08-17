import React from "react";
import ReactDOM from "react-dom/client";
import { HashRouter } from "react-router-dom";
import { openUrl } from "@tauri-apps/plugin-opener";
import { invoke } from "@tauri-apps/api/core";
import App from "./App";
import { MemoriesWindowView } from "./pages/MemoriesWindowView";
import { ArtifactWindowView } from "./pages/ArtifactWindowView";
import { startNetworkMonitor } from "./stores/networkStore";
import { startUpdateMonitor } from "./stores/updateStore";
import { isDebugUnlocked } from "./lib/debugUnlock";

// Popped-out utility windows (e.g. Memories, or a per-artifact window opened
// via lib/windows.ts) load this same bundle with a dedicated hash marker.
// They render a lean standalone root instead of the full <App/> shell — see
// MemoriesWindowView / ArtifactWindowView for what that skips and why.
const isMemoriesWindow = window.location.hash.startsWith("#/memories-window");
// Artifact windows are per-instance (`#/artifact-window/{agentId}/{artifactId}`),
// so this is a prefix match rather than an exact one — the id suffix is
// parsed by ArtifactWindowView itself, not here.
const isArtifactWindow = window.location.hash.startsWith("#/artifact-window");

// Disable right-click context menu in production builds.
if (!import.meta.env.DEV) {
  document.addEventListener("contextmenu", (e) => e.preventDefault());
}

// Hidden dev tools shortcut: Cmd+Option+W (macOS) / Ctrl+Alt+W (other)
// Only toggles the DevPanel if the session has been unlocked via the debug code.
document.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.altKey && e.key.toLowerCase() === "w") {
    e.preventDefault();
    if (!isDebugUnlocked()) return;
    invoke("open_devtools").catch(() => {});
    // Dispatch custom event to toggle dev panel
    window.dispatchEvent(new CustomEvent("toggle-dev-panel"));
  }
});

// Open all external links in the system browser instead of the webview.
document.addEventListener("click", (e) => {
  const anchor = (e.target as HTMLElement).closest("a");
  if (!anchor) return;
  const href = anchor.getAttribute("href");
  if (href && /^https?:\/\//.test(href)) {
    e.preventDefault();
    openUrl(href);
  }
});

if (!isMemoriesWindow && !isArtifactWindow) {
  // Connectivity monitor: pings the local backend and probes the public
  // internet, both every 10s. See stores/networkStore.ts.
  startNetworkMonitor();

  // Auto-update monitor: first check 5s after launch, then every 4 hours.
  // See stores/updateStore.ts.
  //
  // Production only, unlike the connectivity monitor above. Under `tauri dev`
  // the update path is wrong by construction: it would offer to download and
  // install a signed release bundle over the build being edited, and the
  // correct fix for a stale checkout is `git pull`, never a download. The timer
  // is recurring, so left on it fires for the life of a long dev session rather
  // than once.
  //
  // A dev build therefore never reaches the updater plugin at all. `check()`
  // has exactly two callers — this monitor, and ForceUpdateGate's on-demand
  // check which is gated on the same condition — and the `Update` object that
  // downloadAndInstallUpdate needs can only come from `check()`, so the install
  // path is unreachable in dev rather than merely unused. What does still run
  // is the startup version gate (utils/versionCheck.ts), which fetches the
  // release manifest directly instead of through the plugin, and stays on in
  // dev deliberately — the reasoning is at the top of that file.
  if (!import.meta.env.DEV) {
    startUpdateMonitor();
  }
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    {isMemoriesWindow ? (
      <MemoriesWindowView />
    ) : isArtifactWindow ? (
      <ArtifactWindowView />
    ) : (
      <HashRouter>
        <App />
      </HashRouter>
    )}
  </React.StrictMode>,
);
