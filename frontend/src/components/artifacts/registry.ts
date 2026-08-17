import type { ArtifactKind } from "../../types/api";
import {
  BoardArtifactBody,
  CardsArtifactBody,
  ChartArtifactBody,
  ListArtifactBody,
  MetricArtifactBody,
  TableArtifactBody,
} from "./TypedArtifactBodies";
import { HtmlArtifactBody } from "./HtmlArtifactBody";
import { UnsupportedArtifactBody } from "./UnsupportedArtifactBody";
import type { ArtifactBodyComponent } from "./types";

export type { ArtifactBodyProps, ArtifactBodyComponent } from "./types";

/** Renderer registry keyed by `ArtifactKind`. The six typed
 *  renderers each draw structured JSON; `html` is just another entry in the
 *  same registry drawing through the sandboxed iframe — there is no
 *  separate "HTML product," one registry covers both. `unknown` is the
 *  forward-compat catch-all and always resolves to the inert
 *  placeholder, never a throw or a blank pane. */
export const ARTIFACT_KIND_RENDERERS: Record<ArtifactKind, ArtifactBodyComponent> = {
  list: ListArtifactBody,
  cards: CardsArtifactBody,
  table: TableArtifactBody,
  board: BoardArtifactBody,
  metric: MetricArtifactBody,
  chart: ChartArtifactBody,
  html: HtmlArtifactBody,
  unknown: UnsupportedArtifactBody,
};

/** Resolve the renderer for a given kind. Falls back to the unsupported
 *  placeholder for anything outside the known registry — belt-and-suspenders
 *  beyond the `"unknown"` tag itself, so a surprising runtime value (e.g. a
 *  hand-rolled payload bypassing the wire types) still degrades safely
 *  instead of throwing on an undefined lookup. */
export function resolveArtifactRenderer(kind: ArtifactKind): ArtifactBodyComponent {
  return ARTIFACT_KIND_RENDERERS[kind] ?? UnsupportedArtifactBody;
}
