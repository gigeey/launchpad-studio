interface CoordinatorBadgeProps {
  /** Coordinator level: 0 renders as "L" (leaf), positive integers as "C{n}". */
  level: number;
  /** Outer pixel size of the seal. Defaults to 24. */
  size?: number;
  className?: string;
}

/** Renders a scalloped "seal"-style coordinator badge with the level centered on it.
 *
 *  The seal silhouette is built from two identical rounded squares — one axis-aligned,
 *  one rotated 45° — stacked on the same centre so they overlap into an eight-point
 *  rounded star. A `drop-shadow` filter hugs that combined outline (not a box), and a
 *  soft white sheen across the top-left gives the badge a glossy, minted look.
 *
 *  Level 0 = leaf agent ("L"), level ≥1 = coordinator ("C1", "C2", …). */
export function CoordinatorBadge({ level, size = 24, className = "" }: CoordinatorBadgeProps) {
  const label = level === 0 ? "L" : `C${level}`;

  // Each square spans ~72% of the outer box so the rotated square's diagonal points
  // land just inside the bounding box instead of spilling onto neighbouring elements.
  const squareSize = Math.round(size * 0.72);
  const radius = Math.max(2, Math.round(size * 0.15));
  const fontSize = Math.max(8, Math.round(size * 0.38));

  // Shared geometry for the two stacked squares. They are centred on the same point;
  // only the rotation differs, which is what produces the eight-point star.
  const squareBase: React.CSSProperties = {
    position: "absolute",
    top: "50%",
    left: "50%",
    width: squareSize,
    height: squareSize,
    borderRadius: radius,
    backgroundColor: "var(--accent)",
    backgroundImage:
      "linear-gradient(135deg, rgba(255,255,255,0.28) 0%, rgba(255,255,255,0) 60%)",
  };

  return (
    <span
      className={`relative inline-flex flex-shrink-0 items-center justify-center ${className}`}
      style={{ width: size, height: size }}
    >
      {/* Seal silhouette — two overlapping rounded squares (one rotated 45°). The
          drop-shadow filter follows the combined star outline rather than a box. */}
      <span
        aria-hidden="true"
        className="absolute inset-0"
        style={{ filter: "drop-shadow(0 1px 1.5px rgba(0,0,0,0.22))" }}
      >
        <span style={{ ...squareBase, transform: "translate(-50%, -50%)" }} />
        <span style={{ ...squareBase, transform: "translate(-50%, -50%) rotate(45deg)" }} />
      </span>

      {/* Level label, centred above the seal. */}
      <span
        className="relative z-[1] font-bold leading-none text-white select-none cursor-default"
        style={{ fontSize }}
        aria-label={`Coordinator badge: ${label}`}
        data-testid="coordinator-badge"
      >
        {label}
      </span>

    </span>
  );
}
