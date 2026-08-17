import { describe, it, expect } from "vitest";
import {
    parseCustomThemePalette,
    deriveCustomThemeVars,
    deriveCustomDarkContentVars,
    deriveCustomAccentPair,
    customSwatchColor,
} from "./customTheme";

// A "Monokai-ish" example palette, representative of a real pasted color list.
const MONOKAI_PALETTE = [
    "#222222", "#2F2F2F", "#F92772", "#FFFFFF", "#A6E22D",
    "#FFFFFF", "#66D9EF", "#BE84F2", "#2F2F2F", "#FFFFFF",
];

const HEX6_RE = /^#[0-9A-F]{6}$/;

describe("parseCustomThemePalette", () => {
    it("parses a comma-separated palette of 10 hex colors", () => {
        const result = parseCustomThemePalette(MONOKAI_PALETTE.join(", "));
        expect("colors" in result).toBe(true);
        if ("colors" in result) {
            expect(result.colors).toEqual(MONOKAI_PALETTE);
        }
    });

    it("normalizes 3-digit shorthand and missing '#'", () => {
        const raw = "fff, #000, " + MONOKAI_PALETTE.slice(2).join(", ");
        const result = parseCustomThemePalette(raw);
        expect("colors" in result).toBe(true);
        if ("colors" in result) {
            expect(result.colors[0]).toBe("#FFFFFF");
            expect(result.colors[1]).toBe("#000000");
        }
    });

    it("rejects a palette without exactly 10 colors", () => {
        const result = parseCustomThemePalette("#111111, #222222");
        expect("error" in result).toBe(true);
    });

    it("rejects an invalid hex token", () => {
        const raw = ["not-a-color", ...MONOKAI_PALETTE.slice(1)].join(", ");
        const result = parseCustomThemePalette(raw);
        expect("error" in result).toBe(true);
        if ("error" in result) {
            expect(result.error).toMatch(/position 1/);
        }
    });
});

describe("deriveCustomThemeVars", () => {
    const vars = deriveCustomThemeVars(MONOKAI_PALETTE);

    it("maps the unambiguous surface/badge roles straight through", () => {
        expect(vars["--bg-primary"]).toBe("#2F2F2F"); // 2: Menu BG Hover
        expect(vars["--bg-secondary"]).toBe("#222222"); // 1: Column BG
        expect(vars["--bg-sidebar"]).toBe("#222222");
        expect(vars["--chat-input-bg"]).toBe("#222222");
        expect(vars["--accent"]).toBe("#F92772"); // 3: Active Item BG
        expect(vars["--sidebar-active-bg"]).toBe("#F92772");
        expect(vars["--sidebar-active-text-primary"]).toBe("#FFFFFF"); // 4: Active Item Text
        expect(vars["--text-on-accent"]).toBe("#FFFFFF");
        expect(vars["--bg-hover"]).toBe("#A6E22D"); // 5: Hover Item BG
        expect(vars["--text-primary"]).toBe("#FFFFFF"); // 6: Text Color
        expect(vars["--sidebar-text-primary"]).toBe("#FFFFFF"); // 6: Text Color (reused)
        expect(vars["--presence-indicator"]).toBe("#66D9EF"); // 7: Active Presence
        expect(vars["--unread-badge-bg"]).toBe("#BE84F2"); // 8: Mention Badge
        expect(vars["--bg-user-message"]).toBe("#BE84F2");
    });

    it("derives text-secondary/tertiary as legible blends, never a copy of raw input", () => {
        // This is the specific bug class that made Goodstuff FM's sidebar text
        // (#383838 on a #292D36 bg) unreadable — text-secondary/tertiary must
        // never just be handed a raw, unvalidated "muted" input value.
        expect(vars["--text-secondary"]).not.toBe(vars["--text-primary"]);
        expect(vars["--text-tertiary"]).not.toBe(vars["--text-primary"]);
        expect(vars["--text-secondary"]).toMatch(HEX6_RE);
        expect(vars["--text-tertiary"]).toMatch(HEX6_RE);
    });

    it("produces valid 6-digit hex for every derived (non-rgba) surface", () => {
        for (const [key, value] of Object.entries(vars)) {
            if (key.endsWith("-text-secondary") || key === "--app-bg-image" || key === "--app-backdrop-filter") continue;
            expect(value, key).toMatch(HEX6_RE);
        }
    });

    it("gives --bg-tertiary the RGB-average-of-primary-and-secondary treatment", () => {
        // primary=#2F2F2F, secondary=#222222 -> average per channel
        expect(vars["--bg-tertiary"]).toBe("#292929");
    });
});

describe("deriveCustomDarkContentVars", () => {
    it("produces a full, valid neutral dark-panel palette", () => {
        const dark = deriveCustomDarkContentVars(MONOKAI_PALETTE);
        const expectedKeys = [
            "--bg-secondary", "--bg-tertiary", "--bg-input", "--chat-input-bg",
            "--bg-hover", "--bg-agent-message", "--bg-primary", "--border-primary",
            "--border-secondary", "--text-primary", "--text-secondary", "--text-tertiary",
        ];
        for (const key of expectedKeys) {
            expect(dark[key], key).toMatch(HEX6_RE);
        }
        // text-primary should read as light against this near-black panel
        expect(dark["--text-primary"]).not.toBe(dark["--bg-primary"]);
    });
});

describe("deriveCustomAccentPair", () => {
    it("returns [accent, a lightness-shifted hover variant]", () => {
        const [accent, hover] = deriveCustomAccentPair(MONOKAI_PALETTE);
        expect(accent).toBe("#F92772");
        expect(hover).not.toBe(accent);
        expect(hover).toMatch(HEX6_RE);
    });
});

describe("customSwatchColor", () => {
    it("falls back to a neutral gray when no palette is set", () => {
        expect(customSwatchColor(null)).toMatch(HEX6_RE);
    });

    it("mirrors --bg-primary (role 2, Menu BG Hover), matching every other theme's swatch convention", () => {
        expect(customSwatchColor(MONOKAI_PALETTE)).toBe("#2F2F2F");
    });
});
