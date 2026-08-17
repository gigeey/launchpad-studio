import { Navigate } from "react-router-dom";
import { Calendar, type LucideIcon } from "lucide-react";
import { getViewConfig, type ViewId } from "../config/navigation";
import { useNavigationStore } from "../stores/navigationStore";
import messageBubble from "../assets/messageBubbleNoBG.png";
import workflowIcon from "../assets/workflowsNoBG.png";

type EmptyStateConfig =
  | { kind: "icon"; icon: LucideIcon; label: string }
  | { kind: "image"; src: string; label: string };

const emptyStates: Partial<Record<ViewId, EmptyStateConfig>> = {
  home: { kind: "image", src: messageBubble, label: "Select a chat to start" },
  tasks: { kind: "image", src: workflowIcon, label: "Select a workflow to start" },
  chat: { kind: "image", src: messageBubble, label: "Select an agent to start chatting" },
  scheduled: { kind: "icon", icon: Calendar, label: "Select a category to view scheduled items" },
};

function EmptyState({ config }: { config: EmptyStateConfig }) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center text-[var(--text-secondary)] text-[13px] gap-2">
      {config.kind === "image" ? (
        <img src={config.src} alt="" className="w-[326px] h-[326px] object-contain select-none" draggable={false} />
      ) : (
        <div className="w-24 h-24 rounded-3xl bg-[var(--text-secondary)] flex items-center justify-center border-0 border-[var(--border-secondary)]">
          <config.icon size={48} className="text-[var(--bg-tertiary)]" fill="currentColor" strokeWidth={0} />
        </div>
      )}
      {/* {config.label} */}
    </div>
  );
}

export function ViewRedirect({ viewId }: { viewId: ViewId }) {
  const selectedSubMenuByView = useNavigationStore(
    (state) => state.selectedSubMenuByView
  );
  const storedSubMenu = selectedSubMenuByView[viewId] ?? null;
  const viewConfig = getViewConfig(viewId);

  if (!viewConfig) return null;

  const targetSubMenu = storedSubMenu ?? viewConfig.subMenuItems[0]?.id;
  if (!targetSubMenu) {
    const empty = emptyStates[viewId];
    if (empty) return <EmptyState config={empty} />;
    return null;
  }

  return <Navigate to={`${viewConfig.path}/${targetSubMenu}`} replace />;
}
