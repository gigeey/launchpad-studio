import { useCallback, useEffect, useMemo, useRef } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { useThemeSync, useFontSync } from "../App";
import { ArtifactPreview } from "../components/artifacts/ArtifactRenderer";
import { ARTIFACT_PRINT_EVENT } from "../lib/windows";

const ARTIFACT_WINDOW_PREFIX = "#/artifact-window/";

/** Parse `#/artifact-window/{agentId}/{artifactId}[?print=1]` out of the
 *  location hash. Both ids ride the hash — see `lib/windows.ts#openArtifactWindow`
 *  for why the artifact id alone isn't enough (the fetch route is per-agent).
 *  `print=1` is the marker `printArtifactWindow` sets when it opens the window
 *  specifically to print: the window self-prints once its artifact has
 *  rendered (an already-open window is asked via {@link ARTIFACT_PRINT_EVENT}
 *  instead). The query rides *inside* the hash, so it's stripped here before
 *  the id split rather than read from `location.search`. */
function parseArtifactWindowHash(
    hash: string,
): { agentId: string; artifactId: string; print: boolean } | null {
    if (!hash.startsWith(ARTIFACT_WINDOW_PREFIX)) return null;
    let rest = hash.slice(ARTIFACT_WINDOW_PREFIX.length);
    let print = false;
    const q = rest.indexOf("?");
    if (q >= 0) {
        print = new URLSearchParams(rest.slice(q + 1)).get("print") === "1";
        rest = rest.slice(0, q);
    }
    const [rawAgentId, rawArtifactId] = rest.split("/");
    if (!rawAgentId || !rawArtifactId) return null;
    try {
        return {
            agentId: decodeURIComponent(rawAgentId),
            artifactId: decodeURIComponent(rawArtifactId),
            print,
        };
    } catch {
        return null;
    }
}

/**
 * Standalone root for an artifact popped out into its own OS window (opened
 * via `openArtifactWindow()`). Mirrors `MemoriesWindowView` exactly:
 * main.tsx mounts this directly instead of <App/> for the `#/artifact-window`
 * route, so it only pulls in theme/font sync — not the main window's version
 * gate, pending-migration checks, dev panel, or network/update monitors,
 * which are singletons the main window already owns and must not run twice.
 *
 * Renders the exact same `ArtifactPreview` used inline and in the Assets
 * panel — same registry, same sandboxed-HTML renderer, same sandbox flags.
 * Relocating the surface into its own window does not
 * grant it a host bridge; a popped-out interactive artifact is still Tier 2
 * because the renderer itself is unchanged.
 */
export function ArtifactWindowView() {
    useThemeSync();
    useFontSync();

    const ids = useMemo(() => parseArtifactWindowHash(window.location.hash), []);

    // Guards `window.print()` to firing at most once per intent: the fresh
    // "opened to print" case can also receive an `ARTIFACT_PRINT_EVENT` if the
    // opener raced, and the body's `onReady` can fire more than once (e.g. an
    // artifact whose iframe reloads), so both print paths funnel through this.
    const printedRef = useRef(false);
    const wantsPrintOnReadyRef = useRef(ids?.print ?? false);

    // Already-open window path: `printArtifactWindow` focuses this window and
    // emits `ARTIFACT_PRINT_EVENT`; its artifact is already rendered, so print
    // straight away. (The fresh-window path prints from `onBodyReady` below.)
    useEffect(() => {
        let unlisten: (() => void) | undefined;
        let disposed = false;
        listen(ARTIFACT_PRINT_EVENT, () => {
            printedRef.current = true;
            window.print();
        }).then((un) => {
            if (disposed) un();
            else unlisten = un;
        });
        return () => {
            disposed = true;
            unlisten?.();
        };
    }, []);

    // Fresh-window path: the window was opened with `print=1`, so print once
    // the artifact has actually rendered (the iframe's `load`), not before.
    const handleBodyReady = useCallback(() => {
        if (!wantsPrintOnReadyRef.current || printedRef.current) return;
        printedRef.current = true;
        wantsPrintOnReadyRef.current = false;
        window.print();
    }, []);

    return (
        // `--bg-secondary`, not `--modal-bg`: this window IS the artifact's
        // own surface (not a floating modal over other app content), so its
        // background must match `ArtifactPreview`'s panel color exactly —
        // otherwise the panel's rounded corners (in "overlay" chrome) or its
        // edges (in "window" chrome) reveal a mismatched color underneath.
        // `--modal-bg` is intentionally allowed to diverge from
        // `--bg-secondary` for colorful chrome themes (see App.css), which is
        // correct for an actual modal but wrong here.
        <div className="w-screen h-screen overflow-hidden relative bg-[var(--bg-secondary)]">
            {ids ? (
                <ArtifactPreview
                    agentId={ids.agentId}
                    artifactId={ids.artifactId}
                    chrome="window"
                    onBodyReady={handleBodyReady}
                    // The window itself IS the "opened elsewhere" surface, so the
                    // header's close (X) control closes the OS window rather than
                    // clearing some parent selection state.
                    onClose={() => {
                        getCurrentWindow().close();
                    }}
                />
            ) : (
                <div className="w-full h-full flex items-center justify-center text-[13px] text-[var(--text-secondary)]">
                    Artifact not found.
                </div>
            )}
        </div>
    );
}
