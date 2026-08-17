import { create } from "zustand";

interface WindowFocusState {
    isFocused: boolean;
    setFocused: (focused: boolean) => void;
}

export const useWindowFocusStore = create<WindowFocusState>()((set) => ({
    isFocused: typeof document !== "undefined" ? document.visibilityState === "visible" : true,
    setFocused: (focused) => set({ isFocused: focused }),
}));

// Module-level guard so re-importing this module (HMR, multiple bundles,
// etc.) never binds the same listeners twice — double-binding would still
// be functionally correct (setFocused is idempotent) but would leak
// listeners and double-fire the Tauri dynamic import below.
let attached = false;

function attachWindowFocusListeners(): void {
    if (attached) return;
    if (typeof window === "undefined" || typeof document === "undefined") return;
    attached = true;

    const setFocused = useWindowFocusStore.getState().setFocused;

    document.addEventListener("visibilitychange", () => {
        setFocused(document.visibilityState === "visible");
    });
    window.addEventListener("focus", () => setFocused(true));
    window.addEventListener("blur", () => setFocused(false));

    // Tauri's webview-level `focus`/`blur` events don't always fire the way
    // browser ones do inside a native window frame, so also listen to the
    // Tauri window's own focus-changed event when running under Tauri.
    // Dynamic import + try/catch: this module is imported in plain
    // browser/vitest contexts too, where `@tauri-apps/api/window` has no
    // backing IPC and must be a silent no-op rather than a hard failure.
    try {
        import("@tauri-apps/api/window")
            .then(({ getCurrentWindow }) => {
                getCurrentWindow().onFocusChanged(({ payload }) => {
                    setFocused(payload);
                });
            })
            .catch(() => {
                // Not running under Tauri (or the IPC call failed) — the
                // browser-level listeners above already cover focus tracking.
            });
    } catch {
        // Synchronous failure importing the module — same no-op fallback.
    }
}

attachWindowFocusListeners();
