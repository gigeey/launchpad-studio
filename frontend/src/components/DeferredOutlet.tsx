import { useEffect, useState } from "react";
import { useOutlet } from "react-router-dom";

/**
 * Splits a top-level nav switch into two paints so the shell renders first and
 * the (often heavy) route content mounts a frame later — "UI first, then the
 * dynamic content."
 *
 * Why this exists: the app frame (icon rail, sidebar, the empty content panel)
 * lives in AppShell and its markup doesn't change when you switch tabs. But
 * React commits a route change and the freshly-mounted view in a *single*
 * synchronous pass, so the browser can't paint the shell until the whole new
 * subtree is built. Some views are expensive to mount — ChatView alone spins
 * up a rich-text editor, a virtualized message list, and a handful of
 * fetch-on-mount effects — which is why clicking e.g. Assets -> Chat feels like
 * it stalls before anything appears.
 *
 * By withholding the new route element for exactly one animation frame, the
 * shell (unchanged markup) paints immediately and the heavy view mounts on the
 * next frame. This only decouples the frame paint from the view mount; the
 * data-driven fade-in is still owned per-view by ContentGate/useReadyLatch.
 *
 * `viewKey` should identify the top-level view (not the full path), so the
 * deferral fires on nav-rail switches but stays out of the way of in-view
 * sub-navigation (switching agents inside Chat, opening a project's detail),
 * which earlier work deliberately kept mount-stable and smooth.
 */
export function DeferredOutlet({ viewKey }: { viewKey: string }) {
  const outlet = useOutlet();
  const [committedKey, setCommittedKey] = useState(viewKey);
  const [holding, setHolding] = useState(false);

  // Top-level view changed: withhold the new content for this commit. Setting
  // state during render (guarded by `!holding` so it runs exactly once per
  // change) re-renders immediately into the "blank shell" state — the
  // React-sanctioned "adjust state when a prop changes" pattern, and safe under
  // StrictMode's double render.
  if (viewKey !== committedKey && !holding) {
    setHolding(true);
  }

  useEffect(() => {
    if (!holding) return;
    // requestAnimationFrame fires just before the next repaint — i.e. after the
    // blank-shell commit has already painted — so the heavy view mounts one
    // frame behind the frame. Re-runs if viewKey changes again mid-hold (rapid
    // switching), so we always land on the latest view rather than a stale one.
    const id = requestAnimationFrame(() => {
      setCommittedKey(viewKey);
      setHolding(false);
    });
    return () => cancelAnimationFrame(id);
  }, [holding, viewKey]);

  // One frame with an empty content panel while the shell paints; the route
  // content mounts on the next frame.
  return holding ? null : outlet;
}

export default DeferredOutlet;
