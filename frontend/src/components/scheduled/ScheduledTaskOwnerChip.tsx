import { agentAvatarColor, agentAvatarColorVibrant, teamTintColor } from "../../lib/agentColors";
import { useIsDark } from "../../stores/userPreferencesStore";
import type { ScheduledTaskOwner } from "../../lib/scheduledTaskShared";

// ---------------------------------------------------------------------------
// Owner chip for the aggregate Scheduled page — resolves an agent/team's
// color from ../../lib/agentColors and renders a small swatch+label pill
// (ScheduledTaskOwnerChip) or a bare swatch (ScheduledTaskOwnerDot).
//
// Consumed by the calendar-grid and list-view pieces of the Scheduled page
// built in later steps: "sm" fits a compact calendar day cell, "md" fits a
// list row, and the dot fits an overflow popover row.
//
// `ownerColor` is also exported directly so ScheduledCalendar.tsx can tint an
// entire occurrence tile's background to match its owner's avatar color, not
// just a small swatch — keeps both call sites deriving from one place.
//
// `ownerColorVibrant` is the saturated counterpart: team tints are already
// vivid at both index arrays, so it passes through unchanged there, but for
// individual agents it swaps the (fairly pastel) base avatar color for a
// more vivid variant. ScheduledCalendar uses the pastel `ownerColor` for
// disabled occurrence tiles and this vibrant version for enabled ones, so
// on/off reads as a real color distinction rather than just opacity.
// ---------------------------------------------------------------------------

export function ownerColor(owner: ScheduledTaskOwner, isDark: boolean): string {
  return owner.isTeam ? teamTintColor(owner.name, isDark) : agentAvatarColor(owner.name, isDark);
}

export function ownerColorVibrant(owner: ScheduledTaskOwner, isDark: boolean): string {
  return owner.isTeam ? teamTintColor(owner.name, isDark) : agentAvatarColorVibrant(owner.name, isDark);
}

function swatchContent(owner: ScheduledTaskOwner): string {
  return owner.emoji ?? owner.name.charAt(0).toUpperCase();
}

interface ScheduledTaskOwnerChipProps {
  owner: ScheduledTaskOwner;
  size?: "sm" | "md";
  /** Overrides the default name text sizing/weight classes — e.g. the
   *  list view's group-by-agent section heading wants a bigger, bolder
   *  label than the compact per-tile chip this component defaults to. */
  nameClassName?: string;
}

export function ScheduledTaskOwnerChip({ owner, size = "md", nameClassName }: ScheduledTaskOwnerChipProps) {
  const isDark = useIsDark();
  const color = ownerColor(owner, isDark);
  const isSm = size === "sm";
  const swatchPx = isSm ? 15 : 24;

  return (
    <span
      className="inline-flex items-center min-w-0 max-w-full"
      style={{ gap: isSm ? 4 : 6 }}
      title={owner.name}
    >
      <span
        className="rounded-full flex items-center justify-center flex-shrink-0 select-none leading-none"
        style={{
          width: swatchPx,
          height: swatchPx,
          backgroundColor: color,
          fontSize: isSm ? 9 : 12,
        }}
        aria-hidden
      >
        {swatchContent(owner)}
      </span>
      <span
        className={`truncate min-w-0 ${nameClassName ?? (isSm ? "text-[10px] leading-[13px]" : "text-[13px] leading-[16px]")}`}
        style={{ color: "var(--text-primary)" }}
      >
        {owner.name}
      </span>
    </span>
  );
}

interface ScheduledTaskOwnerDotProps {
  owner: ScheduledTaskOwner;
  sizePx?: number;
}

export function ScheduledTaskOwnerDot({ owner, sizePx = 10 }: ScheduledTaskOwnerDotProps) {
  const isDark = useIsDark();
  const color = ownerColor(owner, isDark);

  return (
    <span
      className="inline-block rounded-full flex-shrink-0 select-none"
      style={{
        width: sizePx,
        height: sizePx,
        backgroundColor: color,
      }}
      title={owner.name}
      aria-hidden
    />
  );
}
