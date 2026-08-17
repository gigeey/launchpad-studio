import { Box } from "lucide-react";
import { useArtifactStore } from "../stores/artifactStore";
import { useArtifactViewStore } from "../stores/artifactViewStore";
import { ArtifactPreview } from "../components/artifacts/ArtifactRenderer";
import { openArtifactWindow } from "../lib/windows";

// ---------------------------------------------------------------------------
// Global Assets page — the cross-agent home for pinned
// artifacts. `AssetsSidebar` (the collapsible sub-menu column, mounted by
// AppShell) lists every pinned artifact and drives selection through the
// shared `artifactViewStore`; this pane renders whichever one is selected via
// `ArtifactPreview`, reusing the exact same renderer the inline chat card and
// the per-agent Assets panel already use — pin/copy/download/refresh/pop-out
// all work identically here.
// ---------------------------------------------------------------------------

export function AssetsView() {
  const agentId = useArtifactViewStore((s) => s.agentId);
  const artifactId = useArtifactViewStore((s) => s.artifactId);
  const close = useArtifactViewStore((s) => s.close);
  const pinnedCount = useArtifactStore((s) => s.pinned.length);

  const hasSelection = agentId !== null && artifactId !== null;

  return (
    <div className="relative flex flex-col flex-1 min-h-0 bg-[var(--bg-secondary)]">
      {!hasSelection && (
        <div className="flex-1 flex flex-col items-center justify-center gap-3 text-center px-8">
          <Box className="w-[40px] h-[40px] text-[var(--text-tertiary)]" />
          <span className="text-[13px] text-[var(--text-secondary)] leading-relaxed max-w-[320px]">
            {pinnedCount > 0
              ? "Select a pinned artifact from the sidebar to view it."
              : "Pin an artifact from any agent's chat to save it here — pinned artifacts show up across every agent."}
          </span>
        </div>
      )}

      <ArtifactPreview
        agentId={agentId}
        artifactId={artifactId}
        onClose={close}
        onPopOut={(aId, artId) => {
          openArtifactWindow(aId, artId);
        }}
      />
    </div>
  );
}
