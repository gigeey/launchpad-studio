import { useCallback, useEffect, useRef } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { ArtifactBodyProps } from "./types";

// ---------------------------------------------------------------------------
// The sandbox invariant — the #1 correctness item in this file.
//
// `allow-scripts` WITHOUT `allow-same-origin` is the only sandbox posture this
// renderer is ever allowed to use. A `srcDoc` iframe with both flags set runs
// scripted content that can reach back into the parent document — the two
// together defeat the sandbox entirely. Granting only `allow-scripts` keeps
// the frame at a null (opaque) origin: scripts can run *inside* the frame
// (needed for Tier-2 self-contained interactive artifacts — expand a row,
// filter, reveal a detail panel over data already embedded in the payload),
// but the frame can never read/write the parent's DOM, cookies, or storage.
//
// This same single posture also covers Tier-1 inert HTML: an artifact with
// no <script> tags simply has nothing for `allow-scripts` to run, so the
// flag is a no-op for it. There is deliberately no second "stricter" sandbox
// string for Tier 1 — one constant, enforced here, covers both tiers.
//
// `allow-modals` is the one addition beyond that original posture: it lets
// author HTML open browser-native modal dialogs (`alert`/`confirm`/`print`)
// on its *own* frame — without it the browser silently refuses them, even
// for a same-frame call. It does NOT let the frame reach the parent
// document, so it doesn't weaken the origin isolation above. Note that
// printing an artifact is NOT driven through this flag anymore: the print
// button routes through the artifact's own pop-out window (a real top-level
// webview whose top frame Tauri patches `window.print()` on), because
// `Window.print` is not a cross-origin-accessible member of a `WindowProxy`
// — from the parent it reads as `undefined` on this opaque-origin frame — and
// Tauri's native-print patch only reaches a webview's *top* frame, never a
// nested one like this. See `printArtifactWindow` in `lib/windows.ts` and
// `ArtifactWindowView`. `allow-same-origin` must still never be added here.
//
// `allow-popups` lets an artifact open external links (`<a target="_blank">`
// or a user-gesture `window.open`) in a new browser tab instead of being
// silently popup-blocked. `allow-popups-to-escape-sandbox` ensures that new
// tab does NOT inherit this sandbox, so the destination page loads as a
// normal, fully-functional page rather than being stuck at a null origin
// itself. Neither flag reaches back into the parent document, so they don't
// weaken the isolation above. `allow-top-navigation` and
// `allow-top-navigation-by-user-activation` remain DELIBERATELY excluded —
// an artifact must never be able to navigate the top frame (the whole app)
// away; external links only ever open in a new tab.
//
// In the desktop app there is no browser chrome for that new tab to land
// in — this is a Tauri webview with no window/tab manager — so a bare
// `target="_blank"` open is silently dropped. `withLinkBridge` below works
// around that: it intercepts link clicks and `window.open` calls *inside*
// the opaque-origin frame and relays the target URL to the parent via
// `postMessage`, and the parent hands it to the system browser through
// `openUrl`. That relay is one-way (URL string only) and the parent only
// trusts messages whose `event.source` is this exact iframe's
// `contentWindow` — it does not check `event.origin`, since an opaque-origin
// frame reports it as the literal string `"null"`, which is not a usable
// trust boundary. `allow-popups`/`allow-popups-to-escape-sandbox` stay as a
// harmless fallback for contexts (e.g. a plain browser tab, not the Tauri
// shell) where the bridge script doesn't run. `allow-same-origin` is still
// never added — the opaque-origin invariant this whole comment block
// describes is unaffected by the bridge.
//
// This is a hardcoded module-level constant, not a prop — no caller of this
// component can widen it. Do not lift it into `ArtifactBodyProps` or thread
// it from a parent; the whole point is that nothing upstream can override it.
const ARTIFACT_HTML_SANDBOX =
  "allow-scripts allow-modals allow-popups allow-popups-to-escape-sandbox";

// Printing an artifact iframe only picks up its own background colors if the
// browser is told to keep them (the manual "Background graphics" toggle in
// an external print dialog). Rather than rely on the user finding that
// toggle, force it on for this frame's content via a print-only style. This
// is the "smallest safe injection point" for it: the artifact HTML is
// mounted verbatim via `srcDoc` with no wrapper/template to hook into, so
// the tag is spliced into whatever head/html/doctype structure (if any) the
// author HTML already has, falling back to a plain prepend for a bare
// fragment.
const PRINT_COLOR_STYLE =
  "<style>@media print { html { -webkit-print-color-adjust: exact; print-color-adjust: exact; } }</style>";

function withPrintColorStyle(html: string): string {
  const headOpenTag = html.match(/<head[^>]*>/i);
  if (headOpenTag) {
    const at = headOpenTag.index! + headOpenTag[0].length;
    return html.slice(0, at) + PRINT_COLOR_STYLE + html.slice(at);
  }
  const htmlOpenTag = html.match(/<html[^>]*>/i);
  if (htmlOpenTag) {
    const at = htmlOpenTag.index! + htmlOpenTag[0].length;
    return html.slice(0, at) + PRINT_COLOR_STYLE + html.slice(at);
  }
  // A leading `<!DOCTYPE ...>` must stay the very first thing in the
  // document for standards mode, so splice after it rather than prepending.
  const doctype = html.match(/^\s*<!doctype[^>]*>/i);
  if (doctype) {
    return html.slice(0, doctype[0].length) + PRINT_COLOR_STYLE + html.slice(doctype[0].length);
  }
  return PRINT_COLOR_STYLE + html;
}

// Runs inside the opaque-origin artifact frame (see the sandbox comment
// block above). Deliberately reads no cookies/storage/parent state — it
// only rewrites where a link click / `window.open` call goes, using
// `postMessage` (the one channel an opaque-origin frame is still allowed to
// use) to hand the target URL to the parent. Never touches `event.origin`
// coming back, since that's this frame's own job, not this script's.
const LINK_BRIDGE_SCRIPT = `<script>
(function () {
  try {
    document.addEventListener(
      "click",
      function (e) {
        var a = e.target && e.target.closest && e.target.closest("a[href]");
        if (!a) return;
        var url = a.href;
        if (/^https?:\\/\\//i.test(url)) {
          e.preventDefault();
          window.parent.postMessage({ __artifactLinkOpen: true, url: url }, "*");
        }
      },
      true
    );
    var _open = window.open;
    window.open = function (u) {
      try {
        if (u && /^https?:\\/\\//i.test(String(u))) {
          window.parent.postMessage({ __artifactLinkOpen: true, url: String(u) }, "*");
          return null;
        }
      } catch (_) {}
      return _open ? _open.apply(this, arguments) : null;
    };
  } catch (_) {}
})();
</script>`;

function withLinkBridge(html: string): string {
  const bodyCloseTag = html.match(/<\/body>/i);
  if (bodyCloseTag) {
    return html.slice(0, bodyCloseTag.index!) + LINK_BRIDGE_SCRIPT + html.slice(bodyCloseTag.index!);
  }
  return html + LINK_BRIDGE_SCRIPT;
}

/** The sandboxed-HTML renderer ("html" is a `kind` in the same
 *  registry as the typed renderers, not a separate product). Draws freeform
 *  markup the agent authored directly, at a null origin.
 *
 *  Reserved-seam note: the envelope (`v`/`nonce`/`type`/
 *  `id`/`payload`) + per-artifact nonce injection + `ready`/`init`/`resize`/
 *  `theme` handshake are specced but NOT built here. `withLinkBridge` (above)
 *  does inject a bootstrap `<script>` into `srcDoc`, but it's a narrow,
 *  single-purpose exception: one fixed `{ __artifactLinkOpen, url }` message
 *  frame-to-parent, solely to route external link opens through the parent's
 *  `openUrl` plumbing (this Tauri shell has no tab/window manager to catch a
 *  dropped `target="_blank"`). It is not the general envelope/handshake
 *  protocol — do not grow it into one. When the real envelope lands, it can
 *  either subsume this message shape or run alongside it; either way, the
 *  wire shape for that broader protocol is not settled yet. (Printing used
 *  to inject a second `{ __artifactPrint: true }` bridge script here; that's
 *  been removed — print now routes through the artifact's pop-out window, see
 *  `printArtifactWindow` in `lib/windows.ts`.) */
export function HtmlArtifactBody({ artifact, iframeRef, roundedBottom, onReady }: ArtifactBodyProps) {
  const html = typeof artifact.payload === "string" ? artifact.payload : "";

  // `iframeRef` (the caller-supplied prop) may be a callback ref, an object
  // ref, or absent. The link-bridge listener below needs its own reliable
  // handle on the mounted iframe regardless of what the caller passed in, so
  // it keeps a private ref and forwards the node to `iframeRef` too, whatever
  // shape that is.
  const internalIframeRef = useRef<HTMLIFrameElement | null>(null);
  const setIframeRef = useCallback(
    (node: HTMLIFrameElement | null) => {
      internalIframeRef.current = node;
      if (typeof iframeRef === "function") {
        iframeRef(node);
      } else if (iframeRef) {
        iframeRef.current = node;
      }
    },
    [iframeRef]
  );

  useEffect(() => {
    async function handleMessage(event: MessageEvent) {
      // The trust boundary is identity, not `event.origin`: an opaque-origin
      // (sandboxed, no allow-same-origin) frame reports its origin as the
      // literal string "null", which every other opaque frame also reports —
      // it can't distinguish this artifact's frame from any other. Only
      // `event.source` pointing at this exact iframe's `contentWindow` is
      // meaningful here.
      if (event.source !== internalIframeRef.current?.contentWindow) return;
      if (!event.data || event.data.__artifactLinkOpen !== true) return;
      const url = event.data.url;
      if (typeof url !== "string" || !/^https?:\/\//i.test(url)) return;
      try {
        await openUrl(url);
      } catch {
        // Falls back for contexts without the Tauri opener plugin (e.g. a
        // plain browser tab during dev).
        window.open(url, "_blank", "noopener");
      }
    }
    window.addEventListener("message", handleMessage);
    return () => window.removeEventListener("message", handleMessage);
  }, []);

  return (
    <iframe
      ref={setIframeRef}
      data-testid="artifact-body-html"
      srcDoc={html ? withLinkBridge(withPrintColorStyle(html)) : html}
      sandbox={ARTIFACT_HTML_SANDBOX}
      title={artifact.title}
      // Fires once the frame's document has loaded. The popped-out artifact
      // window listens for this to know the artifact has rendered before it
      // prints itself (see `ArtifactWindowView`); inline mounts pass no
      // `onReady` and this is a no-op for them.
      onLoad={onReady}
      // Its immediate wrapper in ArtifactRenderer.tsx already owns
      // `overflow-hidden` + a matching bottom radius, and that clip is
      // usually enough on its own. But this iframe's true ancestry also
      // includes the animated `motion.div` card two levels up (framer-motion
      // `scale`), and WebKit doesn't reliably clip an iframe through *any*
      // transformed ancestor in its compositing-layer chain — not just the
      // direct parent. Left unmitigated, that lets the iframe's own white
      // background (`bg-white` below — sensible default for arbitrary
      // author HTML that assumes a page background) bleed a 1px fringe past
      // the rounded mask, which reads as a stray light corner against dark
      // artifact content/dark themes. Repeating the same radius here, on
      // the iframe's own box, clips the iframe's *own* paint instead of
      // depending on an ancestor's mask to reach through it — belt-and-
      // suspenders with the wrapper's clip, not a replacement for it.
      // `roundedBottom` mirrors the wrapper's overlay-vs-window chrome
      // check (`ArtifactRenderer.tsx`); window chrome stays flush/square.
      className={`flex-1 min-h-0 w-full border-0 bg-white ${roundedBottom ? "rounded-b-[16px]" : ""}`}
    />
  );
}
