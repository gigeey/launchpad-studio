import { useEffect, useState } from "react";
import { Routes, Route, Navigate } from "react-router-dom";
import { getVersion } from "@tauri-apps/api/app";
import { AppShell } from "./layouts";
import { viewConfigs } from "./config/navigation";
import { ViewRedirect } from "./components/ViewRedirect";
import { ChatView } from "./pages/ChatView";
import { TasksView } from "./pages/TasksView";
import { ProjectsView, ProjectsIndex } from "./pages/ProjectsView";
import { AssignmentsView } from "./pages/AssignmentsView";
import { AssetsView } from "./pages/AssetsView";
import { ProjectDetailView } from "./pages/ProjectDetailView";
import { TaskDetailView } from "./pages/TaskDetailView";
import { NewTaskView } from "./pages/NewTaskView";
import { PlaceholderView } from "./pages/PlaceholderView";
import { useUserPreferencesStore } from "./stores/userPreferencesStore";
import { MediaPreview } from "./components/chat/MediaPreview";
import { DevPanel } from "./components/DevPanel";
import { ForceUpdateGate } from "./components/ForceUpdateGate";
import { fetchLatestVersion, isVersionTooOld } from "./utils/versionCheck";
import { deriveCustomThemeVars, deriveCustomDarkContentVars, themeKind } from "./lib/customTheme";
import { PRESET_THEME_MAP } from "./lib/presetThemes";
import { ensureNotificationPermission } from "./lib/notifications/notify";
import "./App.css";

// Every CSS custom property deriveCustomThemeVars() can set — kept as a
// standalone list so we can always clear a stale value when the user leaves
// the custom theme (or its palette resets), rather than only ever setting
// properties and letting old ones linger on the root element.
const CUSTOM_THEME_VAR_NAMES = [
  "--app-bg-image", "--app-backdrop-filter",
  "--bg-primary", "--bg-secondary", "--bg-tertiary", "--bg-sidebar", "--bg-input", "--chat-input-bg",
  "--sidebar-active-bg", "--sidebar-active-text-primary", "--sidebar-active-text-secondary", "--sidebar-text-primary",
  "--bg-hover", "--bg-user-message", "--bg-agent-message",
  "--text-primary", "--text-secondary", "--text-tertiary", "--text-on-accent",
  "--border-primary", "--border-secondary", "--checkbox-border",
  "--accent", "--accent-hover", "--error", "--error-bg", "--error-border", "--success",
  "--input-focus-border", "--unread-badge-bg", "--presence-indicator", "--search-border",
];

// deriveCustomDarkContentVars() key -> the root-level custom property that
// carries its raw computed value. Named "--custom-dark-content-*" (not
// "--surface-*" directly) so the value can live on <html> unconditionally —
// App.css's `[data-app-theme='custom'][data-theme='dark']` block is the only
// thing that aliases it into --surface-*, so an inline style set here never
// fights the light-mode CSS rule for the same --surface-* names (inline
// styles always beat stylesheet rules for a shared property on the same
// element, which is exactly the DOM-position bug this whole contract exists
// to avoid).
const CUSTOM_DARK_CONTENT_VAR_MAP: Record<string, string> = {
  "--bg-secondary": "--custom-dark-content-bg",
  "--bg-tertiary": "--custom-dark-content-bg-tertiary",
  "--bg-input": "--custom-dark-content-bg-input",
  "--chat-input-bg": "--custom-dark-content-chat-input-bg",
  "--bg-hover": "--custom-dark-content-bg-hover",
  "--bg-agent-message": "--custom-dark-content-bg-agent-message",
  "--bg-primary": "--custom-dark-content-bg-primary",
  "--border-primary": "--custom-dark-content-border-primary",
  "--border-secondary": "--custom-dark-content-border-secondary",
  "--text-primary": "--custom-dark-content-text-primary",
  "--text-secondary": "--custom-dark-content-text-secondary",
  "--text-tertiary": "--custom-dark-content-text-tertiary",
};

export function useThemeSync() {
  const theme = useUserPreferencesStore((s) => s.theme);
  const appTheme = useUserPreferencesStore((s) => s.appTheme);
  const customThemeColors = useUserPreferencesStore((s) => s.customThemeColors);

  useEffect(() => {
    const resolvedAppTheme = appTheme || "default";
    document.documentElement.setAttribute("data-app-theme", resolvedAppTheme);

    const palette = appTheme === "custom" ? customThemeColors : (PRESET_THEME_MAP.get(appTheme)?.palette ?? null);

    const apply = (resolved: "light" | "dark") => {
      document.documentElement.setAttribute("data-theme", resolved);

      // The default theme (the former "Blue Lagoon" preset, which took over
      // the `default` id) shares the Deep Space dark CSS block verbatim
      // instead of deriving its own inline vars below (see the shared
      // [data-app-theme='deep-space'/'default'][data-theme='dark'] rule in
      // App.css), so it must also report data-theme-kind="adaptive" in dark,
      // exactly like Deep Space — otherwise the chrome-only surface alias
      // ([data-theme-kind='chrome'][data-theme='dark']) would point
      // --surface-* at --custom-dark-content-* vars nobody sets for it
      // once the inline derivation below is skipped.
      const isDefaultDark = appTheme === "default" && resolved === "dark";
      document.documentElement.setAttribute("data-theme-kind", isDefaultDark ? "adaptive" : themeKind(resolvedAppTheme));

      // Neither the "custom" theme's colors (pasted by the user) nor a
      // preset theme's palette are read through a static App.css block —
      // unlike the old hand-authored chrome themes, both are computed and
      // applied directly to the root element here. Inline style properties
      // beat any stylesheet rule for the same element, so this cleanly
      // overrides the :root/[data-theme=dark] defaults exactly like a real
      // [data-app-theme='...'] block would.
      CUSTOM_THEME_VAR_NAMES.forEach((name) => document.documentElement.style.removeProperty(name));
      Object.values(CUSTOM_DARK_CONTENT_VAR_MAP).forEach((name) => document.documentElement.style.removeProperty(name));

      // Default dark has no inline styles at all — same as Deep Space (an
      // adaptive theme) — so the shared App.css dark rule can win cleanly.
      if (isDefaultDark) return;

      if (palette && palette.length === 10) {
        const vars = deriveCustomThemeVars(palette);
        Object.entries(vars).forEach(([name, value]) => {
          document.documentElement.style.setProperty(name, value);
        });

        // Set unconditionally (not gated on the resolved light/dark mode) —
        // App.css only ever reads these through the [data-theme='dark']-scoped
        // alias above, so there's nothing to fight in light mode.
        const darkContentVars = deriveCustomDarkContentVars(palette);
        Object.entries(darkContentVars).forEach(([name, value]) => {
          const rootName = CUSTOM_DARK_CONTENT_VAR_MAP[name];
          if (rootName) document.documentElement.style.setProperty(rootName, value);
        });
      }
    };

    if (theme !== "system") {
      apply(theme);
      return;
    }

    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    apply(mq.matches ? "dark" : "light");

    const handler = (e: MediaQueryListEvent) => apply(e.matches ? "dark" : "light");
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, [theme, appTheme, customThemeColors]);
}

export function useFontSync() {
  const font = useUserPreferencesStore((s) => s.font);

  useEffect(() => {
    let fontFamily: string;
    switch (font) {
      case "Arial":
        fontFamily = "Arial, sans-serif";
        break;
      case "Comic Sans":
        fontFamily = "'Comic Sans MS', 'Comic Sans', cursive";
        break;
      case "Georgia":
        fontFamily = "Georgia, serif";
        break;
      case "Inter":
        fontFamily = "'Inter', sans-serif";
        break;
      case "Noto Sans":
        fontFamily = "'Noto Sans', sans-serif";
        break;
      case "OpenDyslexic":
        fontFamily = "OpenDyslexic, sans-serif";
        break;
      case "Roboto Mono":
        fontFamily = "'Roboto Mono', monospace";
        break;
      case "System (San Francisco Pro)":
      case "System Default": // Fallback for old preferences
        fontFamily = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif";
        break;
      case "Verdana":
        fontFamily = "Verdana, sans-serif";
        break;
      case "Lato (Default)":
      case "Inter Lata (Default)":
      case "Inter (Default)": // Fallback for old preferences
      default:
        fontFamily = "'Lato', sans-serif";
        break;
    }
    document.documentElement.style.fontFamily = fontFamily;
  }, [font]);
}

// Module-level guard so React StrictMode's dev-only double-mount can't
// prime (and double-prompt for) OS notification permission twice.
let notificationPrimingStarted = false;

function useNotificationPermissionPriming() {
  const notificationsEnabled = useUserPreferencesStore((s) => s.notificationsEnabled);

  useEffect(() => {
    if (notificationPrimingStarted || !notificationsEnabled) return;
    notificationPrimingStarted = true;
    ensureNotificationPermission().catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}

type VersionGateState =
  | { status: "checking" }
  | { status: "allowed" }
  | { status: "blocked"; currentVersion: string; latestVersion: string };

function useVersionGate(): VersionGateState {
  const [state, setState] = useState<VersionGateState>({ status: "checking" });

  useEffect(() => {
    let cancelled = false;

    async function check() {
      try {
        const [currentVersion, latestVersion] = await Promise.all([
          getVersion().catch(() => null),
          fetchLatestVersion(),
        ]);

        if (cancelled) return;

        if (!currentVersion || !latestVersion) {
          setState({ status: "allowed" });
          return;
        }

        if (isVersionTooOld(currentVersion, latestVersion)) {
          setState({ status: "blocked", currentVersion, latestVersion });
        } else {
          setState({ status: "allowed" });
        }
      } catch {
        if (!cancelled) setState({ status: "allowed" });
      }
    }

    check();
    return () => { cancelled = true; };
  }, []);

  return state;
}

function App() {
  useThemeSync();
  useFontSync();
  useNotificationPermissionPriming();
  const versionGate = useVersionGate();

  if (versionGate.status === "checking") {
    return null;
  }

  if (versionGate.status === "blocked") {
    return (
      <ForceUpdateGate
        currentVersion={versionGate.currentVersion}
        latestVersion={versionGate.latestVersion}
      />
    );
  }

  return (
    <>
      <MediaPreview />
      <DevPanel />
      <Routes>
        <Route element={<AppShell />}>
          <Route path="/" element={<Navigate to="/chat" replace />} />
          {viewConfigs
            .filter((view) => view.id !== "settings")
            .map((view) => (
              <Route key={view.id} path={view.path}>
                <Route
                  index
                  element={
                    view.id === "scheduled"
                      ? <AssignmentsView />
                      : view.id === "projects"
                        ? <ProjectsIndex />
                        : view.id === "assets"
                          ? <AssetsView />
                          : <ViewRedirect viewId={view.id} />
                  }
                />
                {view.id === "tasks" && (
                  <>
                    <Route path="new" element={<NewTaskView />} />
                    <Route path=":taskId/detail" element={<TaskDetailView />} />
                  </>
                )}
                {view.id === "projects" && (
                  <>
                    <Route path="new" element={<ProjectsView />} />
                    <Route path=":projectId" element={<ProjectDetailView />} />
                  </>
                )}
                {/* Agent new/edit is handled via a modal overlay in AppShell, no route needed */}
                {view.id !== "projects" && view.id !== "scheduled" && view.id !== "assets" && (
                  <Route
                    path=":subMenuSlug"
                    element={
                      view.id === "chat" || view.id === "home"
                        ? <ChatView />
                        : view.id === "tasks"
                          ? <TasksView />
                          : <PlaceholderView />
                    }
                  />
                )}
              </Route>
            ))}
        </Route>
      </Routes>
    </>
  );
}

export default App;
