import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { emitTo } from "@tauri-apps/api/event";

/** Event a pop-out artifact window listens for to print itself. Emitted by
 *  `printArtifactWindow` to an *already-open* artifact window (a freshly
 *  opened one instead carries a `print` marker in its URL and self-prints
 *  once its content has rendered — see `ArtifactWindowView`). Printing has to
 *  happen inside the pop-out's own top-level webview because that is the only
 *  frame Tauri patches `window.print()` on to reach the native print panel;
 *  the main window's artifact is a sandboxed child frame the patch never
 *  reaches, and printing the main window's top frame would capture the whole
 *  app (sidebar/chat), not just the artifact. */
export const ARTIFACT_PRINT_EVENT = "artifact:print";

/**
 * Utility windows the main window can pop content out into. The label
 * doubles as the hash-route marker main.tsx checks at boot to decide
 * whether to mount the full app shell or a window's lean standalone root
 * (see main.tsx + the matching `*WindowView` component for each entry).
 *
 * `openPopoutWindow` takes a full spec rather than a lookup key into a
 * compile-time table, so it works for both a fixed singleton window
 * (Memories) and per-instance windows keyed by an id only known at runtime
 * (artifacts) — the same open-or-focus mechanics apply to either.
 */
interface PopoutWindowSpec {
    /** Unique per window instance. `WebviewWindow.getByLabel` keys the
     *  focus-if-exists check on this — a fixed label means one singleton
     *  window; a label carrying an instance id means one window per
     *  instance. */
    label: string;
    /** Hash route mounted by main.tsx's boot-time check, without the
     *  leading `/`. */
    hash: string;
    title: string;
    /** Optional query string (including the leading `?`) appended to the URL,
     *  e.g. `"?print=1"`. Rides inside the hash, so the window's standalone
     *  root parses it out of `window.location.hash`, not `location.search`. */
    query?: string;
}

/** Open the pop-out (or focus it if it already exists). Resolves to whether an
 *  existing window was focused rather than a new one created — callers that
 *  need to act differently on an already-rendered window (e.g. emit a print
 *  request to it) branch on this. */
async function openPopoutWindow(spec: PopoutWindowSpec): Promise<{ focusedExisting: boolean }> {
    const { label, hash, title, query } = spec;

    // Already open somewhere (maybe behind the main window) — bring it
    // forward instead of spawning a duplicate.
    const existing = await WebviewWindow.getByLabel(label);
    if (existing) {
        await existing.setFocus();
        return { focusedExisting: true };
    }

    const win = new WebviewWindow(label, {
        url: `/${hash}${query ?? ""}`,
        title,
        width: 900,
        height: 640,
        minWidth: 640,
        minHeight: 420,
        center: true,
    });
    win.once("tauri://error", (e) => {
        console.error(`[windows] failed to open "${label}" window:`, e);
    });
    return { focusedExisting: false };
}

const MEMORIES_WINDOW: PopoutWindowSpec = {
    // Label/hash are internal identifiers (route keys, not user-facing text)
    // and stay as `memories` for backward compatibility with any window
    // state Tauri persists across app restarts — only `title` (the OS
    // window's actual chrome) needs to track the panel's user-facing name.
    label: "memories",
    hash: "#/memories-window",
    // "Learning" matches the in-panel header (`MemoriesSettings`'s `<h2>`) —
    // this pop-out covers both self-improving memories AND skills, so a
    // title of just "Memories" undersold half of what's in it.
    title: "Learning",
};

/**
 * Opens the Learning panel (memories + skills review) in its own OS window
 * (or focuses it if it's already open), instead of rendering it inline in
 * the Settings modal. Triggered from the sidebar's Learning icon.
 */
export function openMemoriesWindow(): Promise<void> {
    return openPopoutWindow(MEMORIES_WINDOW).then(() => undefined);
}

/**
 * Opens an artifact in its own OS window (or focuses it if already open),
 * generalizing the Memories pop-out from a fixed singleton window to one
 * window per artifact. The label is keyed by `artifactId` alone
 * (artifact ids are globally-unique uuids) so re-opening the same artifact
 * always focuses its existing window while a different artifact opens a
 * distinct one. `agentId` still has to ride along in the hash — artifacts
 * are fetched through a per-agent-scoped route
 * (`/agents/{agent_id}/artifacts/{artifact_id}`), so `ArtifactWindowView`
 * needs both ids to load anything, not just the artifact id.
 */
export function openArtifactWindow(agentId: string, artifactId: string): Promise<{ focusedExisting: boolean }> {
    return openPopoutWindow(artifactWindowSpec(agentId, artifactId));
}

function artifactWindowSpec(agentId: string, artifactId: string): PopoutWindowSpec {
    return {
        label: `artifact:${artifactId}`,
        hash: `#/artifact-window/${encodeURIComponent(agentId)}/${encodeURIComponent(artifactId)}`,
        title: "Artifact",
    };
}

/**
 * Print an artifact through its own pop-out window — the in-app replacement
 * for "open in a browser tab and ⌘P". The artifact is normally shown inside a
 * sandboxed (opaque-origin) child frame, and Tauri only patches
 * `window.print()` to route to the native macOS print panel on a webview's
 * *top* frame, never a nested one; printing the main window's top frame would
 * also sweep in the whole app (sidebar, chat), not just the artifact. Its
 * own pop-out window is a genuine separate top-level webview containing only
 * the artifact, so its top-frame `window.print()` both reaches the native
 * panel and captures artifact-only content (`ArtifactWindowView` hides its
 * lightweight header for print).
 *
 * If the window is already open it's already rendered, so we just focus it and
 * emit {@link ARTIFACT_PRINT_EVENT} to trigger the print immediately. If it
 * isn't, we open it carrying a `print=1` marker in its URL and let it
 * self-print once its content has loaded — avoiding a race where a print event
 * could arrive before the fresh window has mounted its listener or rendered
 * the artifact.
 */
export async function printArtifactWindow(agentId: string, artifactId: string): Promise<void> {
    const spec = artifactWindowSpec(agentId, artifactId);
    const { focusedExisting } = await openPopoutWindow({ ...spec, query: "?print=1" });
    if (focusedExisting) {
        await emitTo(spec.label, ARTIFACT_PRINT_EVENT);
    }
}
