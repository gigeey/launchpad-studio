// ---------------------------------------------------------------------------
// Shared agent avatar colour utilities.
// Used by ChatSidebar, ChatHeader, and the agent edit modal so every avatar
// chip that derives its background from the agent name is consistent.
// ---------------------------------------------------------------------------

export const AVATAR_COLORS = [
    "#DFD6FE",
    "#BCF1C4",
    "#FECEDC",
    "#CAF0FF",
    "#FFB9BA",
    "#FEDBAB",
];

const AVATAR_COLORS_DARK = [
    "#6D5DC7",
    "#3B8A4A",
    "#B5587A",
    "#3A8DAD",
    "#B5565A",
    "#B58A4A",
];

/**
 * Returns a deterministic pastel background colour for an agent avatar based
 * on the agent's name length.
 *
 * Pass `isDark` explicitly for reactive updates; omit to read from the DOM.
 */
export function agentAvatarColor(name: string, isDark?: boolean): string {
    const dark = isDark ?? document.documentElement.getAttribute("data-theme") === "dark";
    const colors = dark ? AVATAR_COLORS_DARK : AVATAR_COLORS;
    return colors[name.length % colors.length];
}

// Saturated counterparts of AVATAR_COLORS/AVATAR_COLORS_DARK, same hue
// families and same name-length index — used where the pastel/base swatch
// colour reads as too washed out for a larger surface (e.g. a whole tile
// background instead of a small avatar circle), and a muted-vs-vivid
// distinction is meaningful (e.g. disabled vs. enabled).
const AVATAR_COLORS_VIBRANT = [
    "#8B5CF6", // Purple
    "#22C55E", // Green
    "#EC4899", // Pink
    "#0EA5E9", // Blue
    "#F43F5E", // Red
    "#F59E0B", // Orange
];

const AVATAR_COLORS_VIBRANT_DARK = [
    "#9B8AFB",
    "#4ADE80",
    "#F472B6",
    "#38BDF8",
    "#FB7185",
    "#FBBF24",
];

/**
 * Returns a more saturated variant of an agent's avatar colour — same hue
 * family and index as `agentAvatarColor`, just more vivid. Intended for
 * "enabled/active" surfaces that pair with a muted `agentAvatarColor` used
 * for the "disabled" state, so the two read as a clear on/off pair rather
 * than just an opacity change.
 */
export function agentAvatarColorVibrant(name: string, isDark?: boolean): string {
    const dark = isDark ?? document.documentElement.getAttribute("data-theme") === "dark";
    const colors = dark ? AVATAR_COLORS_VIBRANT_DARK : AVATAR_COLORS_VIBRANT;
    return colors[name.length % colors.length];
}

const BANNER_GRADIENTS = [
    "from-indigo-600 via-purple-600 to-indigo-800", // Purple
    "from-emerald-600 via-green-600 to-teal-800",   // Green
    "from-rose-600 via-pink-600 to-rose-800",       // Pink
    "from-[#1E40AF] via-[#3B82F6] to-[#0D9488]",    // Teal/Blue (Original)
    "from-red-600 via-red-500 to-red-800",          // Red
    "from-orange-600 via-amber-600 to-orange-800",  // Orange/Yellow
];

const BANNER_GRADIENTS_DARK = [
    "from-indigo-950 via-purple-900 to-indigo-950", // Purple
    "from-emerald-950 via-green-900 to-teal-950",   // Green
    "from-rose-950 via-pink-900 to-rose-950",       // Pink
    "from-blue-950 via-indigo-900 to-cyan-950",     // Teal/Blue
    "from-red-950 via-red-900 to-red-950",          // Red
    "from-orange-950 via-amber-900 to-orange-950",  // Orange/Yellow
];

/**
 * Returns a Tailwind gradient class string for a team banner based on the name length.
 * Maps to the same indices as agentAvatarColor.
 */
export function teamBannerGradient(name: string, isDark?: boolean): string {
    const dark = isDark ?? document.documentElement.getAttribute("data-theme") === "dark";
    const gradients = dark ? BANNER_GRADIENTS_DARK : BANNER_GRADIENTS;
    return gradients[name.length % gradients.length];
}

// Hex colors that match each banner family (mid-stop of the gradient).
// Used to tint surfaces (e.g. the team home page) so they read as part
// of the same color family as the banner above them.
const BANNER_TINTS = [
    "#7C3AED", // Purple  (matches indigo/purple banner)
    "#10B981", // Green   (matches emerald/green/teal banner)
    "#EC4899", // Pink    (matches rose/pink banner)
    "#3B82F6", // Blue    (matches blue/teal banner)
    "#EF4444", // Red     (matches red banner)
    "#F59E0B", // Orange  (matches orange/amber banner)
];

const BANNER_TINTS_DARK = [
    "#A78BFA", // Purple
    "#34D399", // Green
    "#F472B6", // Pink
    "#60A5FA", // Blue
    "#F87171", // Red
    "#FBBF24", // Orange
];

/**
 * Returns a hex color matching a team's banner family. Use this to derive
 * tinted backgrounds, glows, or accents that should harmonize with the banner.
 */
export function teamTintColor(name: string, isDark?: boolean): string {
    const dark = isDark ?? document.documentElement.getAttribute("data-theme") === "dark";
    const tints = dark ? BANNER_TINTS_DARK : BANNER_TINTS;
    return tints[name.length % tints.length];
}

/**
 * Deterministic djb2-style string hash. The colour helpers above key off
 * `name.length % colors.length`, which is fine for the fairly short, varied
 * names it was designed for, but breaks down for fixed-length opaque ids
 * (e.g. UUIDs) — every id would collapse into the same bucket. This gives an
 * actual hash of the full string for callers (like frequentTaskColor below)
 * that need to key off an id rather than a name.
 */
function hashString(value: string): number {
    let hash = 5381;
    for (let i = 0; i < value.length; i++) {
        hash = (hash * 33) ^ value.charCodeAt(i);
    }
    return hash >>> 0; // coerce to an unsigned 32-bit int
}

/**
 * Deterministic vivid colour for a scheduled task's "frequent" marker —
 * the calendar's per-day corner squares, its top legend row, and the List
 * view's matching badge all resolve the same task id through this function,
 * so a given task reads as the same colour everywhere it appears.
 *
 * Hashed from `task.id`, not the task's name or cron expression: two
 * unrelated tasks that happen to share a schedule (or two tasks a user
 * renames to the same label) must not collide onto the same swatch, or the
 * legend stops meaning anything.
 */
export function frequentTaskColor(taskId: string, isDark?: boolean): string {
    const dark = isDark ?? document.documentElement.getAttribute("data-theme") === "dark";
    const colors = dark ? AVATAR_COLORS_VIBRANT_DARK : AVATAR_COLORS_VIBRANT;
    return colors[hashString(taskId) % colors.length];
}
