import { AlertTriangle } from "lucide-react";
import type { ArtifactBodyProps } from "./types";

/** Inert fallback for an artifact `kind` this build doesn't recognize (the FE
 *  mirror of the backend's `#[serde(other)]` catch-all on `ArtifactKind`).
 *  A newer app version can emit a kind an older build has never
 *  heard of; this renders a calm placeholder instead of throwing or leaving a
 *  blank pane. Never fetches, never scripts, never assumes a payload shape. */
export function UnsupportedArtifactBody({ artifact }: ArtifactBodyProps) {
  return (
    <div
      data-testid="artifact-body-unsupported"
      className="flex-1 min-h-0 flex flex-col items-center justify-center gap-2 text-center px-6 py-10"
    >
      <AlertTriangle size={20} style={{ color: "var(--text-secondary)" }} />
      <div className="text-[13px] font-medium" style={{ color: "var(--text-primary)" }}>
        Unsupported artifact type
      </div>
      <div className="text-[12px] max-w-[280px]" style={{ color: "var(--text-secondary)" }}>
        {`This artifact's type ("${artifact.kind}") isn't supported by this version of the app yet. Update the app to view it.`}
      </div>
    </div>
  );
}
