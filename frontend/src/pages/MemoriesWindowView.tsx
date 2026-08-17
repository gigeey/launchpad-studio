import { useThemeSync, useFontSync } from "../App";
import { MemoriesSettings } from "../components/settings/MemoriesSettings";

/**
 * Standalone root for the Learning panel (memories + skills review) popped
 * out into its own OS window (opened via `openMemoriesWindow()` from the
 * sidebar's Learning icon in `AppShell`).
 *
 * main.tsx mounts this directly instead of <App/> for that window's route,
 * so it only pulls in theme/font sync — not the main window's version gate,
 * pending-migration checks, dev panel, or network/update monitors, which
 * are singletons the main window already owns and shouldn't run twice.
 */
export function MemoriesWindowView() {
    useThemeSync();
    useFontSync();

    return (
        <div className="w-screen h-screen overflow-hidden bg-[var(--modal-bg)]">
            <MemoriesSettings />
        </div>
    );
}
