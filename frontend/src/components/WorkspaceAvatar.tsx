import type { CSSProperties } from "react";

export interface WorkspaceAvatarProps {
  /** Workspace/profile display name. Preferred letter source when non-empty
   *  after trimming. */
  name?: string | null;
  /** Workspace's on-disk path — the letter fallback when `name` is unset.
   *  Derived from the last non-empty path segment with any leading dots
   *  stripped, so a dotfile-style data root (e.g.
   *  `/Users/x/.launchpad_studio-tools`) still yields a real letter ("L"),
   *  never ".". */
  path?: string | null;
  /** User-chosen emoji. Absent, null, or empty-after-trim all mean "unset" —
   *  the avatar falls back to the letter tile. A non-empty string is a
   *  deliberate opt-in and is rendered alone, full-bleed, with no
   *  background. */
  emoji?: string | null;
  /** Background color for the letter tile. Ignored when `emoji` is set —
   *  the emoji state has no background at all. */
  color: string;
  /** Box side length in pixels. The avatar is always square. */
  size: number;
  className?: string;
}

// Emoji glyph roughly fills the box (tuned so a 36px box yields a ~31px
// glyph — big enough to draw the eye without clipping/overflowing).
const EMOJI_SIZE_RATIO = 0.86;
// Letter glyph is deliberately much smaller than the box it sits in — it's
// a monogram, not a full-bleed mark.
const LETTER_SIZE_RATIO = 0.44;
// Rounded-rect corner radius for the letter tile (10px at size 36).
const LETTER_RADIUS_RATIO = 0.28;

/**
 * Shared square avatar for a workspace/profile: a rounded, colored letter
 * tile by default, or the user's chosen emoji alone (no background) once
 * they opt in. This is the only place avatar appearance is decided — route
 * every consumer through it so a future profile-picture state has exactly
 * one insertion point to add.
 *
 * Both states center their glyph by box geometry (the glyph gets its own
 * `display:flex` box sized to 100%/100% of the tile) rather than by
 * font/line-height metrics. Emoji and letter glyphs have different baseline
 * metrics, so text-metric centering (e.g. a bare `<span>` with
 * `leading-none` on a flex parent) visibly drifts for one or the other —
 * geometric centering doesn't.
 */
export function WorkspaceAvatar({ name, path, emoji, color, size, className }: WorkspaceAvatarProps) {
  const trimmedEmoji = emoji?.trim();

  const glyphBoxStyle: CSSProperties = {
    width: "100%",
    height: "100%",
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    lineHeight: 1,
  };

  if (trimmedEmoji) {
    return (
      <div className={className} style={{ width: size, height: size }} aria-hidden="true">
        <span style={{ ...glyphBoxStyle, fontSize: Math.round(size * EMOJI_SIZE_RATIO) }}>{trimmedEmoji}</span>
      </div>
    );
  }

  return (
    <div
      className={className}
      style={{ width: size, height: size, backgroundColor: color, borderRadius: Math.round(size * LETTER_RADIUS_RATIO) }}
      aria-hidden="true"
    >
      <span
        style={{
          ...glyphBoxStyle,
          fontWeight: 600,
          color: "#fff",
          fontSize: Math.round(size * LETTER_SIZE_RATIO),
        }}
      >
        {deriveLetter(name, path)}
      </span>
    </div>
  );
}

/**
 * First-character letter fallback for the no-emoji state — never "/" or
 * ".". Prefers `name`; falls back to the last non-empty segment of `path`
 * with leading dots stripped (so a dotfile-style root like
 * `/Users/x/.launchpad_studio-tools` yields "L", not "."); falls back to
 * "?" if neither source yields anything usable. Always uppercased.
 *
 * Reads with `Array.from` rather than string indexing so an astral-plane
 * first character (outside the BMP — many emoji and some rare scripts) is
 * taken as one full code point instead of being split into a broken
 * surrogate half.
 */
function deriveLetter(name: string | null | undefined, path: string | null | undefined): string {
  const trimmedName = name?.trim();
  if (trimmedName) {
    return Array.from(trimmedName)[0].toUpperCase();
  }

  const segments = (path ?? "").split("/").filter((segment) => segment.length > 0);
  const lastSegment = segments[segments.length - 1];
  const stripped = lastSegment?.replace(/^\.+/, "");
  if (stripped) {
    return Array.from(stripped)[0].toUpperCase();
  }

  return "?";
}

export default WorkspaceAvatar;
