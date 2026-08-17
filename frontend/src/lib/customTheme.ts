import { PRESET_THEME_IDS } from "./presetThemes";

/**
 * Custom "paste 10 hex colors, get a full theme" generator.
 *
 * Every chrome theme so far (Midnight/Sapphire/Emerald/Plum/Tanuki/Denim/
 * Goodstuff FM) was hand-derived: a raw list of ~10 hex values was mapped
 * onto CSS custom properties, then the surfaces nobody specified (borders,
 * input fields, muted text, the dark-mode neutral content panel) were
 * eyeballed or measured by hand. This module automates that whole pipeline
 * so a user can paste 10 hex codes and get a complete, usable theme without
 * anyone hand-tuning numbers.
 *
 * Only ONE custom theme exists at a time — pasting a new palette overwrites
 * the last one (see `customThemeColors` in userPreferencesStore.ts). This is
 * a generator, not a per-theme registry like the hardcoded ones in App.css.
 *
 * Role mapping (1-indexed). The POSITION order MUST match the order the
 * imported palette export's own "create a custom theme" color list uses
 * (also what similar palette-export tools produce) — that's the order every
 * pasted palette actually arrives in, so we conform to it rather than
 * inventing our own and forcing manual reordering on every paste:
 *   1. Column BG         -> sidebar surface      (--bg-secondary/--bg-sidebar/--chat-input-bg)
 *   2. Menu BG Hover     -> overall app bg       (--bg-primary)
 *   3. Active Item BG    -> accent/highlight     (--accent, --sidebar-active-bg)
 *   4. Active Item Text  -> text on the accent   (--sidebar-active-text-primary, --text-on-accent)
 *   5. Hover Item BG     -> hover surface         (--bg-hover)
 *   6. Text Color        -> the app-wide default text (--text-primary), and also
 *      reused for prominent sidebar-only text (--sidebar-text-primary — channel/
 *      agent names, section headers). The imported palette's export only
 *      exposes one generic text color, so both surfaces share it rather than
 *      inventing a distinction the palette never specifies.
 *      --text-secondary/--text-tertiary have no direct
 *      source position and are derived from this by blending toward the sidebar
 *      bg (see deriveCustomThemeVars), not copied verbatim — copying a raw
 *      "muted" input verbatim is exactly what made Goodstuff FM's sidebar text
 *      unreadable (#383838 on a #292D36 bg) before that got hand-fixed; deriving
 *      it algorithmically avoids that bug class.
 *   7. Active Presence   -> presence dot          (--presence-indicator)
 *   8. Mention Badge     -> unread badge           (--unread-badge-bg, --bg-user-message)
 *   9. Top Nav BG        -> (informational only; the app has no distinct "top
 *      nav" element, so it isn't wired to a var)
 *   10. Top Nav Text     -> (informational only, same reason)
 *
 * Column BG -> --bg-secondary and Menu BG Hover -> --bg-primary (rather than
 * the literal reading of the role names, where "Column BG" sounds like it
 * should feed the main background) is intentional and empirically verified
 * twice: it's what the Denim chrome theme's bg-primary/bg-secondary flip
 * converged on (the color Denim's source list called "Menu BG Hover" is the
 * one that ended up as --bg-primary once the flip was confirmed to look
 * right), and it's what a live paste of a teal palette required too —
 * positions 1 and 2 of a real pasted list had to be swapped to get
 * correct-looking output under the *previous* (reversed) version of this
 * mapping. Both data points agree, so this mapping — not the reverse — is
 * the one that ships.
 *
 * Everything else (bg-tertiary, bg-input, borders, checkbox-border,
 * search-border, input-focus-border, text-secondary/tertiary, the dark-mode
 * neutral "content panel" a.k.a. darkContentMap, and accent-hover) is derived
 * algorithmically below. These are reasonable, documented deltas — not an
 * attempt to reproduce the exact bespoke numbers hand-picked for
 * Denim/Goodstuff/etc, which were measured against those specific palettes
 * and aren't universal constants.
 */

// Theme "kind" (see App.css's Tier-B/Tier-A token contract): a "chrome" theme
// keeps its sidebar/brand color constant across light and dark mode, so its
// content surfaces need neutralizing per-mode; an "adaptive" theme has a
// genuinely different, self-sufficient palette per mode and needs none. This
// is the single source every new theme must join to get correct content
// neutralization — SettingsView.tsx's appThemeOptions annotates each entry
// from the same lookup so the picker and the CSS contract can't drift apart.
export const CHROME_APP_THEMES: ReadonlySet<string> = new Set<string>([
    "custom",
    ...PRESET_THEME_IDS,
]);

export function themeKind(appTheme: string): "chrome" | "adaptive" {
    return CHROME_APP_THEMES.has(appTheme) ? "chrome" : "adaptive";
}

export interface CustomThemeRoles {
    columnBg: string;
    menuBgHover: string;
    activeItemBg: string;
    activeItemText: string;
    hoverItemBg: string;
    textColor: string;
    activePresence: string;
    mentionBadge: string;
    topNavBg: string;
    topNavText: string;
}

export const CUSTOM_THEME_ROLE_LABELS: { key: keyof CustomThemeRoles; label: string }[] = [
    { key: "columnBg", label: "Column BG" },
    { key: "menuBgHover", label: "Menu BG Hover" },
    { key: "activeItemBg", label: "Active Item BG" },
    { key: "activeItemText", label: "Active Item Text" },
    { key: "hoverItemBg", label: "Hover Item BG" },
    { key: "textColor", label: "Text Color" },
    { key: "activePresence", label: "Active Presence" },
    { key: "mentionBadge", label: "Mention Badge" },
    { key: "topNavBg", label: "Top Nav BG" },
    { key: "topNavText", label: "Top Nav Text" },
];

const HEX_RE = /^#?([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/;

function normalizeHex(raw: string): string | null {
    const m = HEX_RE.exec(raw.trim());
    if (!m) return null;
    let hex = m[1];
    if (hex.length === 3) {
        hex = hex.split("").map((c) => c + c).join("");
    }
    return `#${hex.toUpperCase()}`;
}

/** Parses a pasted 10-color palette (comma/whitespace/newline separated). */
export function parseCustomThemePalette(raw: string): { colors: string[] } | { error: string } {
    const tokens = raw.split(/[,\n\r\t]+/).map((t) => t.trim()).filter(Boolean);
    if (tokens.length !== 10) {
        return { error: `Expected 10 colors, got ${tokens.length}. Paste them comma-separated.` };
    }
    const colors: string[] = [];
    for (let i = 0; i < tokens.length; i++) {
        const hex = normalizeHex(tokens[i]);
        if (!hex) {
            return { error: `"${tokens[i]}" (position ${i + 1}) isn't a valid hex color.` };
        }
        colors.push(hex);
    }
    return { colors };
}

export function colorsToRoles(colors: string[]): CustomThemeRoles {
    return {
        columnBg: colors[0],
        menuBgHover: colors[1],
        activeItemBg: colors[2],
        activeItemText: colors[3],
        hoverItemBg: colors[4],
        textColor: colors[5],
        activePresence: colors[6],
        mentionBadge: colors[7],
        topNavBg: colors[8],
        topNavText: colors[9],
    };
}

// ---------------------------------------------------------------------------
// Color math

interface Rgb { r: number; g: number; b: number }
interface Hsl { h: number; s: number; l: number }

function hexToRgb(hex: string): Rgb {
    const n = hex.replace("#", "");
    return {
        r: parseInt(n.slice(0, 2), 16),
        g: parseInt(n.slice(2, 4), 16),
        b: parseInt(n.slice(4, 6), 16),
    };
}

function rgbToHex({ r, g, b }: Rgb): string {
    const c = (v: number) => Math.round(Math.min(255, Math.max(0, v))).toString(16).padStart(2, "0");
    return `#${c(r)}${c(g)}${c(b)}`.toUpperCase();
}

function rgbToHsl({ r, g, b }: Rgb): Hsl {
    const rn = r / 255, gn = g / 255, bn = b / 255;
    const max = Math.max(rn, gn, bn), min = Math.min(rn, gn, bn);
    const l = (max + min) / 2;
    if (max === min) return { h: 0, s: 0, l: l * 100 };
    const d = max - min;
    const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
    let h: number;
    switch (max) {
        case rn: h = ((gn - bn) / d + (gn < bn ? 6 : 0)); break;
        case gn: h = (bn - rn) / d + 2; break;
        default: h = (rn - gn) / d + 4; break;
    }
    return { h: h * 60, s: s * 100, l: l * 100 };
}

function hslToRgb({ h, s, l }: Hsl): Rgb {
    const sn = s / 100, ln = l / 100;
    if (sn === 0) {
        const v = ln * 255;
        return { r: v, g: v, b: v };
    }
    const q = ln < 0.5 ? ln * (1 + sn) : ln + sn - ln * sn;
    const p = 2 * ln - q;
    const hue2rgb = (t: number) => {
        let tt = t;
        if (tt < 0) tt += 1;
        if (tt > 1) tt -= 1;
        if (tt < 1 / 6) return p + (q - p) * 6 * tt;
        if (tt < 1 / 2) return q;
        if (tt < 2 / 3) return p + (q - p) * (2 / 3 - tt) * 6;
        return p;
    };
    const hn = h / 360;
    return {
        r: hue2rgb(hn + 1 / 3) * 255,
        g: hue2rgb(hn) * 255,
        b: hue2rgb(hn - 1 / 3) * 255,
    };
}

function hexToHsl(hex: string): Hsl {
    return rgbToHsl(hexToRgb(hex));
}

function hslToHex(hsl: Hsl): string {
    return rgbToHex(hslToRgb(hsl));
}

function clamp(v: number, min: number, max: number): number {
    return Math.min(max, Math.max(min, v));
}

/** True when a color reads as visually "light" (would want a dark overlay/text on it). */
function isLight(hex: string): boolean {
    return hexToHsl(hex).l >= 50;
}

/** Shifts a color's lightness by deltaL (positive = lighter), same hue/saturation. */
function adjustLightness(hex: string, deltaL: number): string {
    const hsl = hexToHsl(hex);
    return hslToHex({ ...hsl, l: clamp(hsl.l + deltaL, 0, 100) });
}

/** Nudges a color away from its current lightness extreme — lightens dark colors,
 *  darkens light ones — so the result always reads as a distinct-but-related surface. */
function stepAwayFromExtreme(hex: string, amount: number): string {
    return adjustLightness(hex, isLight(hex) ? -amount : amount);
}

/** Per-channel RGB blend of two colors; weightB=0 returns hexA, weightB=1
 *  returns hexB. Used both for --bg-tertiary (weight 0.5, the formula
 *  reverse-engineered from Plum/Tanuki: verified exact/near-exact there) and
 *  for muting text toward the background (smaller weights). */
function mixHex(hexA: string, hexB: string, weightB: number): string {
    const a = hexToRgb(hexA), b = hexToRgb(hexB);
    return rgbToHex({
        r: a.r + (b.r - a.r) * weightB,
        g: a.g + (b.g - a.g) * weightB,
        b: a.b + (b.b - a.b) * weightB,
    });
}

function rgbAverage(hexA: string, hexB: string): string {
    return mixHex(hexA, hexB, 0.5);
}

// ---------------------------------------------------------------------------
// Theme derivation

/** Fixed error/success convention shared verbatim by every chrome theme so far
 *  (Plum/Tanuki/Denim/Goodstuff all use these exact four values) — not derived,
 *  intentionally reused as-is. */
const FIXED_STATUS_VARS = {
    "--error": "#FF6B8A",
    "--error-bg": "#2B1525",
    "--error-border": "#5C2040",
    "--success": "#5FDFB0",
};

/** Produces the full set of base CSS custom properties for the custom theme —
 *  the runtime equivalent of one hardcoded [data-app-theme='x'] block in
 *  App.css. Applied identically regardless of light/dark mode, matching every
 *  other "chrome" theme (they intentionally look the same in both). */
export function deriveCustomThemeVars(colors: string[]): Record<string, string> {
    const r = colorsToRoles(colors);

    const bgPrimary = r.menuBgHover;
    const bgSecondary = r.columnBg;
    const bgTertiary = rgbAverage(bgPrimary, bgSecondary);
    const accent = r.activeItemBg;
    const accentHover = stepAwayFromExtreme(accent, 10);
    const activeTextIsLight = isLight(r.activeItemText);
    const textPrimary = r.textColor;

    return {
        "--app-bg-image": "none",
        "--app-backdrop-filter": "none",

        "--bg-primary": bgPrimary,
        "--bg-secondary": bgSecondary,
        "--bg-tertiary": bgTertiary,
        "--bg-sidebar": bgSecondary,
        "--bg-input": stepAwayFromExtreme(bgSecondary, 6),
        "--chat-input-bg": bgSecondary,

        "--sidebar-active-bg": accent,
        "--sidebar-active-text-primary": r.activeItemText,
        "--sidebar-active-text-secondary": activeTextIsLight ? "rgba(255, 255, 255, 0.65)" : "rgba(0, 0, 0, 0.65)",
        // Prominent sidebar-only text (channel/agent names, section headers) —
        // reuses the same Text Color role as --text-primary; see the role-6
        // note in this file's docstring.
        "--sidebar-text-primary": textPrimary,

        "--bg-hover": r.hoverItemBg,
        "--bg-user-message": r.mentionBadge,
        "--bg-agent-message": stepAwayFromExtreme(bgPrimary, 4),

        "--text-primary": textPrimary,
        // Muted toward the sidebar bg rather than copied from user input —
        // guarantees these stay legible instead of risking a near-invisible
        // color the way a raw pasted "muted" value could (see docstring).
        "--text-secondary": mixHex(textPrimary, bgSecondary, 0.35),
        "--text-tertiary": mixHex(textPrimary, bgSecondary, 0.55),
        "--text-on-accent": r.activeItemText,

        "--border-primary": stepAwayFromExtreme(bgSecondary, 10),
        "--border-secondary": stepAwayFromExtreme(bgSecondary, 8),
        "--checkbox-border": stepAwayFromExtreme(bgSecondary, 18),

        "--accent": accent,
        "--accent-hover": accentHover,
        ...FIXED_STATUS_VARS,
        "--input-focus-border": adjustLightness(accent, -12),

        "--unread-badge-bg": r.mentionBadge,
        "--presence-indicator": r.activePresence,
        // A theme-specific border here matters — the default (--border-secondary)
        // is close to --bg-secondary by design, and Goodstuff FM shipped with an
        // invisible search box for exactly that reason. Always give custom themes
        // a stronger, clearly-visible search border up front instead of waiting
        // for a bug report.
        "--search-border": stepAwayFromExtreme(bgSecondary, 14),
    };
}

/** The dark-mode "neutral content panel" override (AppShell.tsx's darkContentMap).
 *  Formula reverse-engineered from Tanuki's and Denim's actual shipped values
 *  (documented in project memory as reusable): every field is an absolute
 *  HSL target (same S/L across all themes), with only the hue tracking this
 *  theme's own --bg-primary hue. */
export function deriveCustomDarkContentVars(colors: string[]): Record<string, string> {
    const r = colorsToRoles(colors);
    const hue = hexToHsl(r.menuBgHover).h;
    const at = (s: number, l: number) => hslToHex({ h: hue, s, l });

    const bgTertiary = at(14.6, 20.2);
    return {
        "--bg-secondary": at(14.35, 14.2),
        "--bg-tertiary": bgTertiary,
        "--bg-input": bgTertiary,
        "--chat-input-bg": at(14.35, 14.2),
        "--bg-hover": at(14.7, 26.7),
        "--bg-agent-message": at(13.5, 16.8),
        "--bg-primary": at(13.3, 16.3),
        "--border-primary": at(11.3, 23.4),
        "--border-secondary": bgTertiary,
        "--text-primary": at(14.3, 90.4),
        "--text-secondary": at(8.5, 61.0),
        "--text-tertiary": at(7.9, 39.6),
    };
}

/** [accent, accent-hover] pair for AppShell.tsx's accentMap. */
export function deriveCustomAccentPair(colors: string[]): [string, string] {
    const r = colorsToRoles(colors);
    return [r.activeItemBg, stepAwayFromExtreme(r.activeItemBg, 10)];
}

/** The color the Settings swatch (and anything else wanting "the one color that
 *  represents this theme") should show — mirrors --bg-primary, same convention
 *  every hardcoded chrome theme's swatch follows. */
export function customSwatchColor(colors: string[] | null): string {
    if (!colors || colors.length !== 10) return "#4B5563";
    return colorsToRoles(colors).menuBgHover;
}
