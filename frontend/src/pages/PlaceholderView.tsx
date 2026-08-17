import { useParams, useLocation } from "react-router-dom";
import { viewConfigs } from "../config/navigation";

export function PlaceholderView() {
  const { subMenuSlug } = useParams<{ subMenuSlug: string }>();
  const location = useLocation();

  // Derive the view from the current URL path
  const pathSegment = location.pathname.split("/").filter(Boolean)[0];
  const viewConfig = viewConfigs.find(
    (v) => v.path === `/${pathSegment}`
  );
  const viewLabel = viewConfig?.label ?? "View";

  // Find the sub-menu item label
  const subMenuItem = viewConfig?.subMenuItems.find(
    (item) => item.id === subMenuSlug
  );
  const subMenuLabel = subMenuItem?.label ?? subMenuSlug ?? "";

  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-4 text-center p-8">
      <div className="w-16 h-16 rounded-2xl bg-[var(--bg-tertiary)] flex items-center justify-center border border-[var(--border-secondary)] text-[32px]">
        {subMenuItem?.emoji ? (
          subMenuItem.emoji
        ) : (
          viewConfig && <viewConfig.icon size={28} className="text-[var(--text-secondary)]" />
        )}
      </div>
      <h1 className="text-[22px] font-bold text-[var(--text-primary)]">{viewLabel}</h1>
      <p className="text-[14px] text-[var(--text-secondary)]">
        Content for {viewLabel} &gt; {subMenuLabel} coming soon
      </p>
    </div>
  );
}
