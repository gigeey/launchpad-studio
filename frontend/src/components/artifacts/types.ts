import type { ComponentType, Ref } from "react";
import type { ArtifactWithPayload } from "../../types/api";

/** Props every kind-specific renderer body receives. Just the fetched
 *  artifact (record + current payload) — bodies are pure functions of that,
 *  no store/context assumptions, so the same body works inline, in the
 *  Assets panel, and in a popped-out standalone window.
 *
 *  `iframeRef` is an optional escape hatch solely for `HtmlArtifactBody`,
 *  forwarding the mounted iframe node up to `ArtifactRenderer`'s shared
 *  header. Every other renderer ignores it.
 *
 *  `roundedBottom` is another `HtmlArtifactBody`-only escape hatch: whether
 *  the card chrome around the body is the rounded "overlay" card
 *  (`ArtifactRenderer`'s `chrome === "overlay"`) or the flush/square
 *  pop-out window. See the comment on the iframe's className in
 *  `HtmlArtifactBody` for why the iframe needs to know this directly instead
 *  of trusting a wrapper's clip. Every other renderer ignores it.
 *
 *  `onReady` fires once the body's content has finished loading (the iframe's
 *  `load` event for `HtmlArtifactBody`). The popped-out artifact window uses
 *  it to know when the artifact has rendered so it can print itself; every
 *  other renderer, and every inline mount, ignores it. */
export interface ArtifactBodyProps {
  artifact: ArtifactWithPayload;
  iframeRef?: Ref<HTMLIFrameElement>;
  roundedBottom?: boolean;
  onReady?: () => void;
}

export type ArtifactBodyComponent = ComponentType<ArtifactBodyProps>;
