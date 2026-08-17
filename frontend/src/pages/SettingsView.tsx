import { useState, useEffect, useCallback, useRef } from "react";
import { useUserPreferencesStore, isNotificationSnoozed, type ThemePreference } from "../stores/userPreferencesStore";
import { Sun, Moon, Laptop, ChevronDown, User, Brush, Globe, Info, BookOpen, Copy, CheckCircle2, Sparkles, BedDouble, Bell } from "lucide-react";
import { twMerge } from "tailwind-merge";
import { getPreferences, putPreferences, cloneExampleWorkflow, type UserPreferences } from "../lib/api";
import { useBannerStore } from "../stores/bannerStore";
import { useWorkflowStore } from "../stores/workflowStore";
import { useUpdateStore } from "../stores/updateStore";
import { RichMarkdown } from "../components/shared/RichMarkdown";
import { validateDebugCode, setDebugUnlocked, isDebugExpired } from "../lib/debugUnlock";
import { getVersion } from "@tauri-apps/api/app";
import { parseCustomThemePalette, CUSTOM_THEME_ROLE_LABELS, customSwatchColor, themeKind } from "../lib/customTheme";
import { PRESET_THEMES } from "../lib/presetThemes";
import { isPermissionGranted } from "@tauri-apps/plugin-notification";
import { ensureNotificationPermission } from "../lib/notifications/notify";


function ProfileSettings() {
    const [fullName, setFullName] = useState("");
    const [preferredName, setPreferredName] = useState("");
    const [loading, setLoading] = useState(true);
    const [saveError, setSaveError] = useState<string | null>(null);
    const prefsRef = useRef<UserPreferences | null>(null);
    const removeBanner = useBannerStore((s) => s.removeBanner);

    useEffect(() => {
        getPreferences()
            .then((prefs) => {
                prefsRef.current = prefs;
                setFullName(prefs.full_name ?? "");
                setPreferredName(prefs.preferred_name ?? "");
            })
            .catch((err) => console.error("[ProfileSettings] failed to load preferences:", err))
            .finally(() => setLoading(false));
    }, []);

    const savePrefs = useCallback(async (updates: Partial<UserPreferences>) => {
        setSaveError(null);
        const current = prefsRef.current ?? { full_name: null, preferred_name: null, timezone: null, language: null, locale: null };
        const merged = { ...current, ...updates };
        try {
            const saved = await putPreferences(merged);
            prefsRef.current = saved;
            // Dismiss the profile setup banner once both name fields are set
            if (saved.full_name && saved.preferred_name) {
                removeBanner("preferences");
                sessionStorage.setItem("preferencesAlertDismissed", "1");
            }
        } catch (err) {
            console.error("[ProfileSettings] save failed:", err);
            setSaveError("Failed to save. Please try again.");
        }
    }, [removeBanner]);

    if (loading) {
        return (
            <div className="flex flex-col flex-1 w-full max-w-3xl pt-2">
                <p className="text-[14px] text-[var(--modal-text-secondary)]">Loading...</p>
            </div>
        );
    }

    return (
        <div className="flex flex-col flex-1 w-full max-w-3xl pt-2">
            {saveError && (
                <div className="mb-4 px-3 py-2 bg-red-500/10 border border-red-500/30 rounded-[8px] text-[13px] text-red-600 dark:text-red-400">
                    {saveError}
                </div>
            )}
            <div className="flex flex-col gap-6">
                <div className="flex flex-col gap-2">
                    <label className="text-[14px] font-medium text-[var(--modal-text-primary)]">Full Name</label>
                    <input
                        type="text"
                        value={fullName}
                        onChange={(e) => setFullName(e.target.value)}
                        onBlur={() => savePrefs({ full_name: fullName || null })}
                        placeholder="First Last"
                        className="px-3 py-2 bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors"
                    />
                </div>
                <div className="flex flex-col gap-2">
                    <label className="text-[14px] font-medium text-[var(--modal-text-primary)]">Preferred Name</label>
                    <input
                        type="text"
                        value={preferredName}
                        onChange={(e) => setPreferredName(e.target.value)}
                        onBlur={() => savePrefs({ preferred_name: preferredName || null })}
                        placeholder="How you'd like to be called"
                        className="px-3 py-2 bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors"
                    />
                </div>
            </div>
        </div>
    );
}

const BUBBLE_COLORS = [
    { id: "aubergine", hex: "#4A154B", label: "Aubergine" },
    { id: "blue", hex: "#1164A3", label: "Blue" },
    { id: "sky", hex: "#007AFF", label: "Sky Blue" },
    { id: "green", hex: "#007A5A", label: "Green" },
    { id: "red", hex: "#E01E5A", label: "Red" },
    { id: "yellow", hex: "#ECB22E", label: "Yellow" },
    { id: "purple", hex: "#611f69", label: "Purple" },
];

function AppearanceSettings() {
    const theme = useUserPreferencesStore((s) => s.theme);
    const setTheme = useUserPreferencesStore((s) => s.setTheme);
    const font = useUserPreferencesStore((s) => s.font);
    const setFont = useUserPreferencesStore((s) => s.setFont);
    const appTheme = useUserPreferencesStore((s) => s.appTheme);
    const setAppTheme = useUserPreferencesStore((s) => s.setAppTheme);
    const bubbleColor = useUserPreferencesStore((s) => s.bubbleColor);
    const setBubbleColor = useUserPreferencesStore((s) => s.setBubbleColor);
    const customThemeColors = useUserPreferencesStore((s) => s.customThemeColors);
    const setCustomThemeColors = useUserPreferencesStore((s) => s.setCustomThemeColors);
    const [customThemeInput, setCustomThemeInput] = useState(() => customThemeColors?.join(", ") ?? "");
    const [customThemeError, setCustomThemeError] = useState<string | null>(null);

    const handleApplyCustomTheme = useCallback(() => {
        const result = parseCustomThemePalette(customThemeInput);
        if ("error" in result) {
            setCustomThemeError(result.error);
            return;
        }
        setCustomThemeError(null);
        setCustomThemeColors(result.colors);
        setCustomThemeInput(result.colors.join(", "));
        setAppTheme("custom");
    }, [customThemeInput, setCustomThemeColors, setAppTheme]);

    const themeOptions: { id: ThemePreference; label: string; icon: React.ReactNode }[] = [
        { id: "light", label: "Light", icon: <Sun size={18} /> },
        { id: "dark", label: "Dark", icon: <Moon size={18} /> },
        { id: "system", label: "System", icon: <Laptop size={18} /> },
    ];

    // `kind` (chrome vs adaptive) drives App.css's Tier-B content neutralization
    // via the `data-theme-kind` root attribute — see themeKind()'s doc comment.
    // Sourced from the same lookup useThemeSync uses so this list can't drift
    // out of sync with the CSS contract when a new theme is added here.
    // Default (a preset under the hood, see presetThemes.ts) is pulled to the
    // front of the grid post-hoc rather than reordered in PRESET_THEMES —
    // .sort() is stable, so this only moves "default" and leaves every other
    // preset's relative order untouched.
    const appThemeOptions = [
        { id: "deep-space", label: "Deep Space", swatch: "#1E1E1E", kind: themeKind("deep-space") },
        { id: "daybreak", label: "Daybreak", swatch: "#1FBAD6", kind: themeKind("daybreak") },
        ...PRESET_THEMES.map((preset) => ({
            id: preset.id,
            label: preset.displayName,
            swatch: customSwatchColor(preset.palette),
            kind: themeKind(preset.id),
        })),
        { id: "custom", label: "Custom", swatch: customSwatchColor(customThemeColors), kind: themeKind("custom") },
    ].sort((a, b) => (a.id === "default" ? -1 : b.id === "default" ? 1 : 0));

    const fonts = [
        "Arial",
        "Comic Sans",
        "Georgia",
        "Inter",
        "Noto Sans",
        "OpenDyslexic",
        "Roboto Mono",
        "System (San Francisco Pro)",
        "Verdana",
        "Lato (Default)",
    ];

    return (
        <div className="flex flex-col flex-1 w-full max-w-3xl pb-8 gap-8 pt-2">
            {/* Font Dropdown */}
            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-4">Font</label>
                <div className="relative inline-block w-full max-w-sm">
                    <select
                        value={font}
                        onChange={(e) => setFont(e.target.value)}
                        className="appearance-none w-full px-4 py-2 bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors cursor-pointer"
                    >
                        {fonts.map((f) => (
                            <option key={f} value={f}>{f}</option>
                        ))}
                    </select>
                    <div className="absolute right-3 top-1/2 -translate-y-1/2 pointer-events-none text-[var(--modal-text-secondary)]">
                        <ChevronDown size={16} />
                    </div>
                </div>
            </div>

            <div className="w-full border-t border-[var(--modal-border-secondary)] my-2" />

            {/* Color Mode */}
            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Color Mode</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Choose if Launchpad's appearance should be light or dark, or follow your computer's settings.
                </p>
                <div className="flex gap-4">
                    {themeOptions.map((opt) => {
                        const isActive = theme === opt.id;
                        return (
                            <div
                                key={opt.id}
                                onClick={() => setTheme(opt.id)}
                                className={twMerge(
                                    "flex items-center justify-center gap-2 px-6 py-3 border rounded-[8px] cursor-pointer transition-all w-[140px]",
                                    isActive
                                        ? "border-[var(--modal-accent)] shadow-[0_0_0_1px_var(--modal-accent)_inset]"
                                        : "border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)] hover:bg-[var(--modal-bg-hover)]"
                                )}
                            >
                                <span className={isActive ? "text-[var(--modal-accent)]" : "text-[var(--modal-text-secondary)]"}>
                                    {opt.icon}
                                </span>
                                <span className={twMerge("text-[14px] font-medium", isActive ? "text-[var(--modal-accent)]" : "text-[var(--modal-text-primary)]")}>
                                    {opt.label}
                                </span>
                            </div>
                        );
                    })}
                </div>
            </div>
            <div className="w-full border-t border-[var(--modal-border-secondary)] my-2" />

            {/* App Theme */}
            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">App Theme (Color Scheme)</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Choose an overarching color scheme. Works with both Light and Dark modes.
                </p>
                <div className="grid grid-cols-2 lg:grid-cols-3 gap-4 w-full">
                    {appThemeOptions.map((opt) => {
                        const isActive = appTheme === opt.id;
                        const isCustom = opt.id === "custom";
                        return (
                            <div
                                key={opt.id}
                                onClick={() => setAppTheme(opt.id)}
                                className={twMerge(
                                    "flex items-center gap-3 border rounded-[8px] cursor-pointer transition-all overflow-hidden",
                                    isCustom ? "col-span-full px-4 py-3" : "px-3 py-2",
                                    isActive
                                        ? "border-[var(--modal-accent)] shadow-[0_0_0_1px_var(--modal-accent)_inset]"
                                        : "border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)] hover:bg-[var(--modal-bg-hover)]"
                                )}
                            >
                                <div
                                    className={twMerge(
                                        "rounded-[6px] flex-shrink-0 border border-[var(--modal-border-secondary)]",
                                        isCustom ? "w-[36px] h-[36px]" : "w-[32px] h-[32px]"
                                    )}
                                    style={{ backgroundColor: opt.swatch }}
                                />
                                <div className="flex flex-col min-w-0">
                                    <span className={twMerge("text-[14px] font-medium whitespace-nowrap overflow-hidden text-ellipsis min-w-0", isActive ? "text-[var(--modal-accent)]" : "text-[var(--modal-text-primary)]")}>
                                        {opt.label}
                                    </span>
                                    {isCustom && (
                                        <span className="text-[12px] text-[var(--modal-text-secondary)]">
                                            Design your own palette
                                        </span>
                                    )}
                                </div>
                            </div>
                        );
                    })}
                </div>
                {appTheme === "custom" && (
                    <div className="mt-4 p-4 rounded-[8px] border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]">
                        <p className="text-[14px] text-[var(--modal-text-primary)] font-medium mb-1">
                            Paste 10 hex colors, comma-separated, in this order:
                        </p>
                        <p className="text-[13px] text-[var(--modal-text-secondary)] mb-3 leading-relaxed">
                            {CUSTOM_THEME_ROLE_LABELS.map((r, i) => `${i + 1}. ${r.label}`).join("  ·  ")}
                        </p>
                        <textarea
                            value={customThemeInput}
                            onChange={(e) => setCustomThemeInput(e.target.value)}
                            placeholder="#222222, #2F2F2F, #F92772, #FFFFFF, #A6E22D, #FFFFFF, #66D9EF, #BE84F2, #2F2F2F, #FFFFFF"
                            rows={3}
                            className="w-full px-3 py-2 bg-[var(--modal-bg-input)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[13px] font-mono text-[var(--modal-text-primary)] focus:outline-none focus:border-[var(--modal-accent)] transition-colors resize-none"
                        />
                        {customThemeError && (
                            <p className="text-[13px] text-[var(--error)] mt-2">{customThemeError}</p>
                        )}
                        <div className="flex items-center gap-3 mt-3">
                            <button
                                type="button"
                                onClick={handleApplyCustomTheme}
                                className="px-4 py-1.5 rounded-[6px] text-[13px] font-medium bg-[var(--modal-accent)] text-white hover:opacity-90 transition-opacity"
                            >
                                Apply
                            </button>
                            {customThemeColors && (
                                <div className="flex items-center gap-1.5">
                                    {customThemeColors.map((c, i) => (
                                        <div key={i} className="w-[16px] h-[16px] rounded-[3px] border border-[var(--modal-border-secondary)]" style={{ backgroundColor: c }} title={`${i + 1}. ${CUSTOM_THEME_ROLE_LABELS[i].label}`} />
                                    ))}
                                </div>
                            )}
                        </div>
                    </div>
                )}
            </div>

            <div className="w-full border-t border-[var(--modal-border-secondary)] my-2" />

            {/* Bubble Colors */}
            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Text Bubble Color</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Choose the color for your text bubbles.
                </p>
                <div className="grid grid-cols-2 lg:grid-cols-3 gap-4 w-full">
                    {BUBBLE_COLORS.map((bc) => {
                        const isActive = bubbleColor === bc.hex;
                        return (
                            <div
                                key={bc.id}
                                onClick={() => setBubbleColor(bc.hex)}
                                className={twMerge(
                                    "flex items-center gap-3 px-4 py-3 border rounded-[8px] cursor-pointer transition-all h-[50px]",
                                    isActive
                                        ? "border-[var(--modal-accent)] shadow-[0_0_0_1px_var(--modal-accent)_inset]"
                                        : "border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)] hover:bg-[var(--modal-bg-hover)]"
                                )}
                            >
                                <div
                                    className="w-[36px] h-[36px] rounded-full flex-shrink-0 relative flex items-center justify-center border shadow-sm border-[var(--modal-border-secondary)]"
                                    style={{ backgroundColor: bc.hex }}
                                >
                                </div>
                                <span className="text-[14px] font-medium text-[var(--modal-text-primary)]">{bc.label}</span>
                            </div>
                        );
                    })}
                </div>
            </div>

            <div className="w-full border-t border-[var(--modal-border-secondary)] my-2" />

            {/* Avatar Style */}
            <AvatarStyleSetting />

            <div className="w-full border-t border-[var(--modal-border-secondary)] my-2" />

            {/* Show Recent Tasks */}
            <RecentTasksToggle />
        </div>
    );
}

function AvatarStyleSetting() {
    const circularAvatars = useUserPreferencesStore((s) => s.circularAvatars);
    const setCircularAvatars = useUserPreferencesStore((s) => s.setCircularAvatars);

    const options = [
        { id: "rounded", label: "Rounded", circular: false },
        { id: "circular", label: "Circular", circular: true },
    ];

    return (
        <div>
            <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Agent Avatar Style</label>
            <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                Choose the shape for agent chat profile avatars.
            </p>
            <div className="flex gap-4">
                {options.map((opt) => {
                    const isActive = circularAvatars === opt.circular;
                    return (
                        <div
                            key={opt.id}
                            onClick={() => setCircularAvatars(opt.circular)}
                            className={twMerge(
                                "flex items-center gap-3 px-5 py-3 border rounded-[8px] cursor-pointer transition-all",
                                isActive
                                    ? "border-[var(--modal-accent)] shadow-[0_0_0_1px_var(--modal-accent)_inset]"
                                    : "border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)] hover:bg-[var(--modal-bg-hover)]"
                            )}
                        >
                            {/* Preview avatar */}
                            <div
                                className={twMerge(
                                    "w-[36px] h-[36px] flex items-center justify-center text-[18px] bg-[#DFD6FE]",
                                    opt.circular ? "rounded-full" : "rounded-[10px]"
                                )}
                            >
                                🤖
                            </div>
                            <span className={twMerge("text-[14px] font-medium", isActive ? "text-[var(--modal-accent)]" : "text-[var(--modal-text-primary)]")}>
                                {opt.label}
                            </span>
                        </div>
                    );
                })}
            </div>
        </div>
    );
}

function RecentTasksToggle() {
    const showRecentTasks = useUserPreferencesStore((s) => s.showRecentTasks);
    const setShowRecentTasks = useUserPreferencesStore((s) => s.setShowRecentTasks);

    return (
        <div className="flex items-center justify-between">
            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)]">Show Recent Tasks</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mt-0.5">
                    Display recent task progress rings in the chat sidebar.
                </p>
            </div>
            <button
                onClick={() => setShowRecentTasks(!showRecentTasks)}
                className={twMerge(
                    "relative w-[44px] h-[24px] rounded-full transition-colors flex-shrink-0 cursor-pointer",
                    showRecentTasks ? "bg-[var(--modal-accent)]" : "bg-[var(--modal-border-secondary)]"
                )}
            >
                <div
                    className={twMerge(
                        "absolute top-[2px] w-[20px] h-[20px] rounded-full bg-white transition-transform shadow-sm",
                        showRecentTasks ? "left-[22px]" : "left-[2px]"
                    )}
                />
            </button>
        </div>
    );
}

function LanguageRegionSettings() {
    const timezoneAuto = useUserPreferencesStore((s) => s.timezoneAuto);
    const setTimezoneAuto = useUserPreferencesStore((s) => s.setTimezoneAuto);
    const timezone = useUserPreferencesStore((s) => s.timezone);
    const setTimezone = useUserPreferencesStore((s) => s.setTimezone);
    const [saveError, setSaveError] = useState<string | null>(null);

    const syncTimezoneToBackend = useCallback(async (tz: string) => {
        setSaveError(null);
        try {
            const prefs = await getPreferences();
            await putPreferences({ ...prefs, timezone: tz });
        } catch (err) {
            console.error("[LanguageRegionSettings] save timezone failed:", err);
            setSaveError("Failed to save timezone. Please try again.");
        }
    }, []);

    const handleTimezoneChange = (tz: string) => {
        setTimezone(tz);
        syncTimezoneToBackend(tz);
    };

    const clearTimezoneOnBackend = useCallback(async () => {
        setSaveError(null);
        try {
            const prefs = await getPreferences();
            await putPreferences({ ...prefs, timezone: null });
        } catch (err) {
            console.error("[LanguageRegionSettings] clear timezone failed:", err);
            setSaveError("Failed to save timezone. Please try again.");
        }
    }, []);

    const handleTimezoneAutoChange = (auto: boolean) => {
        setTimezoneAuto(auto);
        if (auto) {
            // Set local display to detected timezone, but send null to backend
            // so it uses system detection at evaluation time (stays current if user travels)
            const detectedTz = Intl.DateTimeFormat().resolvedOptions().timeZone;
            setTimezone(detectedTz);
            clearTimezoneOnBackend();
        }
    };

    return (
        <div className="flex flex-col flex-1 w-full max-w-3xl gap-10 pt-2">
            {saveError && (
                <div className="px-3 py-2 bg-red-500/10 border border-red-500/30 rounded-[8px] text-[13px] text-red-600 dark:text-red-400">
                    {saveError}
                </div>
            )}
            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Language</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Choose the language you’d like to use.
                </p>
                <div className="relative inline-block w-full max-w-md">
                    <select
                        disabled
                        className="appearance-none opacity-80 w-full px-4 py-2 bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none cursor-not-allowed"
                    >
                        <option>English</option>
                    </select>
                    <div className="absolute right-3 top-1/2 -translate-y-1/2 pointer-events-none text-[var(--modal-text-secondary)]">
                        <ChevronDown size={16} />
                    </div>
                </div>
                <p className="text-[12px] text-[var(--modal-text-secondary)] mt-2 italic">
                    Only English is supported at this time
                </p>
            </div>

            <div className="w-full border-t border-[var(--modal-border-secondary)]" />

            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Locale</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Choose the locale for date and number formatting.
                </p>
                <div className="relative inline-block w-full max-w-md">
                    <select
                        disabled
                        className="appearance-none opacity-80 w-full px-4 py-2 bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none cursor-not-allowed"
                    >
                        <option>en-US</option>
                    </select>
                    <div className="absolute right-3 top-1/2 -translate-y-1/2 pointer-events-none text-[var(--modal-text-secondary)]">
                        <ChevronDown size={16} />
                    </div>
                </div>
                <p className="text-[12px] text-[var(--modal-text-secondary)] mt-2 italic">
                    Only English is supported at this time
                </p>
            </div>

            <div className="w-full border-t border-[var(--modal-border-secondary)]" />

            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Time Zone</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    We use your time zone to send summaries and notify you of events at the right time.
                </p>

                <label className="flex items-center gap-2 mb-4 cursor-pointer">
                    <input
                        type="checkbox"
                        checked={timezoneAuto}
                        onChange={(e) => handleTimezoneAutoChange(e.target.checked)}
                        className="w-4 h-4 text-[var(--modal-accent)] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded focus:ring-[var(--modal-accent)] cursor-pointer"
                    />
                    <span className="text-[14px] text-[var(--modal-text-primary)]">Set time zone automatically</span>
                </label>

                <div className="relative inline-block w-full max-w-md">
                    <select
                        value={timezone}
                        onChange={(e) => handleTimezoneChange(e.target.value)}
                        disabled={timezoneAuto}
                        className={twMerge(
                            "appearance-none w-full px-4 py-2 bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none transition-all",
                            timezoneAuto ? "opacity-50 cursor-not-allowed" : "cursor-pointer focus:border-[var(--modal-accent)]"
                        )}
                    >
                        <option value="America/Los_Angeles">Pacific Time (US & Canada)</option>
                        <option value="America/Denver">Mountain Time (US & Canada)</option>
                        <option value="America/Chicago">Central Time (US & Canada)</option>
                        <option value="America/New_York">Eastern Time (US & Canada)</option>
                        <option value="Europe/London">London</option>
                        <option value="Asia/Tokyo">Tokyo</option>
                    </select>
                    <div className="absolute right-3 top-1/2 -translate-y-1/2 pointer-events-none text-[var(--modal-text-secondary)]">
                        <ChevronDown size={16} />
                    </div>
                </div>
            </div>

        </div>
    );
}

function DocsView({ title, content }: { title: string, content: string }) {
    return (
        <div className="flex flex-col flex-1 w-full max-w-3xl">
            <h2 className="text-[18px] font-bold text-[var(--modal-text-primary)] mb-6">{title}</h2>
            <div className="p-6 border border-[var(--modal-border-secondary)] rounded-xl bg-[var(--modal-bg-tertiary)] text-[var(--modal-text-primary)] shadow-sm">
                {content}
            </div>
        </div>
    );
}

function WhatsNewView() {
    const releaseNotes = useUpdateStore((s) => s.releaseNotes);
    const newVersion = useUpdateStore((s) => s.newVersion);
    const currentVersion = useUpdateStore((s) => s.currentVersion);

    const version = newVersion ?? currentVersion ?? "current";

    // Real app version for debug code validation (not dependent on update store)
    const [appVersion, setAppVersion] = useState<string | null>(null);
    useEffect(() => {
        getVersion().then(setAppVersion).catch(() => setAppVersion(null));
    }, []);

    // Hidden debug panel activation: 7 taps within 3 seconds
    const [showDebugInput, setShowDebugInput] = useState(false);
    const [debugCode, setDebugCode] = useState("");
    const [debugError, setDebugError] = useState(false);
    const tapCountRef = useRef(0);
    const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

    const handleVersionTap = useCallback(() => {
        // Silently ignore if build has expired (100-day EOL)
        if (isDebugExpired()) return;

        tapCountRef.current += 1;

        // Clear any existing reset timer
        if (timerRef.current) clearTimeout(timerRef.current);

        if (tapCountRef.current >= 7) {
            setShowDebugInput(true);
            tapCountRef.current = 0;
            return;
        }

        // Reset tap count after 3 seconds of no taps
        timerRef.current = setTimeout(() => {
            tapCountRef.current = 0;
        }, 3000);
    }, []);

    // Clean up timer on unmount and reset debug input on remount
    useEffect(() => {
        setShowDebugInput(false);
        tapCountRef.current = 0;
        return () => {
            if (timerRef.current) clearTimeout(timerRef.current);
        };
    }, []);

    const handleDebugSubmit = useCallback(async (value: string) => {
        if (value.length !== 6) return;
        const valid = await validateDebugCode(value, appVersion ?? version);
        if (valid) {
            setDebugUnlocked(true);
            setShowDebugInput(false);
            setDebugCode("");
        } else {
            setDebugError(true);
            setTimeout(() => setDebugError(false), 1000);
        }
    }, [appVersion, version]);

    return (
        <div className="flex flex-col flex-1 w-full max-w-3xl">
            <h2 className="text-[18px] font-bold text-[var(--modal-text-primary)] mb-1">What's New</h2>
            <p
                className="text-[13px] text-[var(--modal-text-secondary)] mb-6 select-none cursor-default"
                onClick={handleVersionTap}
            >
                Version {appVersion ?? version}
            </p>
            {showDebugInput && (
                <input
                    type="text"
                    inputMode="numeric"
                    maxLength={6}
                    placeholder="Enter debug code"
                    value={debugCode}
                    onChange={(e) => {
                        const val = e.target.value.replace(/\D/g, "").slice(0, 6);
                        setDebugCode(val);
                        setDebugError(false);
                        if (val.length === 6) handleDebugSubmit(val);
                    }}
                    className={`mb-4 px-3 py-1.5 w-48 text-[13px] rounded-md border bg-[var(--modal-bg-tertiary)] text-[var(--modal-text-primary)] placeholder:text-[var(--modal-text-secondary)] outline-none transition-colors ${
                        debugError
                            ? "border-red-500 animate-[shake_0.3s_ease-in-out]"
                            : "border-[var(--modal-border-secondary)] focus:border-[var(--modal-accent)]"
                    }`}
                />
            )}
            <div className="p-6 border border-[var(--modal-border-secondary)] rounded-xl bg-[var(--modal-bg-tertiary)] text-[var(--modal-text-primary)] shadow-sm prose prose-sm max-w-none">
                {releaseNotes ? (
                    <RichMarkdown>{releaseNotes}</RichMarkdown>
                ) : (
                    <p className="text-[var(--modal-text-secondary)]">You're up to date! Release notes will appear here when a new version is available.</p>
                )}
            </div>
        </div>
    );
}

// ---------------------------------------------------------------------------
// Workflow documentation with example templates and clone-to-disk
// ---------------------------------------------------------------------------

const WORKFLOW_DOCS_MARKDOWN = `# Creating Workflows

This is a complete reference for creating workflows in LaunchPad Studio. Give this to any AI agent alongside a description of the workflow you want, and the agent will be able to create it.

---

## What is a Workflow?

A workflow is a reusable, multi-phase template that defines a sequence of steps — each executed by an AI agent, a script, or the user. Workflows are stored as folders on disk and executed as **tasks** (individual runs of a workflow).

**Key concepts:**
- **Workflow** = reusable template (definition)
- **Task** = a single execution of a workflow
- **Phase** = one step in the workflow

---

## Directory Structure

Workflows live in the LaunchPad Studio data directory:

\`\`\`
~/.launchpad_studio/workflows/
\`\`\`

Or \`$LAUNCHPAD_STUDIO_DATA_DIR/workflows/\` if the env var is set.

Each workflow is a directory containing a \`workflow.yaml\` and all phase assets:

\`\`\`
~/.launchpad_studio/workflows/
└── my-workflow/                    # Directory name = workflow ID
    ├── workflow.yaml               # Required: workflow definition
    │
    ├── interview/
    │   └── prompt.md               # Prompt phase: markdown instructions for the agent
    │
    ├── generate-prd/
    │   ├── prompt.md               # Prompt phase with schema
    │   └── schema.json             # Optional: JSON schema for structured output
    │
    ├── build/
    │   ├── run.sh                  # Folder phase: executable script
    │   └── helpers/                # Any supporting files the script needs
    │       └── template.hbs
    │
    └── (input and pause phases don't need directories or files)
\`\`\`

When a task runs, outputs are written to a separate task directory:

\`\`\`
~/.launchpad_studio/tasks/
└── my-workflow_2026-04-09_abc123/  # Task instance
    ├── task.yaml                   # Task state/snapshot
    └── output/                     # All phase outputs (flat)
        ├── inputs.yaml             # Shared input phase values
        ├── interview.md            # Output from interview phase
        ├── prd.json                # Output from PRD phase
        └── report.md               # Output from another phase
\`\`\`

**Important:** Output files are always flat — no nested directories on the output side.

---

## workflow.yaml Reference

This is the main definition file. Here is the complete schema:

\`\`\`yaml
# ─── Workflow metadata ───────────────────────────────────────
id: "my-workflow"                   # Required. Unique identifier (must match directory name)
name: "My Workflow"                 # Required. Human-readable display name
version: "1.0.0"                    # Optional. Semantic version string
description: "What this workflow does" # Optional. Shown in the UI and system prompt

# ─── Phases ──────────────────────────────────────────────────
phases:                             # Required. Ordered list of phase definitions
  - id: "phase-id"                  # Required. Unique within this workflow
    name: "Phase Name"              # Required. Human-readable
    intent: "What this phase does"  # Optional. Guides the agent's approach
    path: "phase-id/prompt.md"      # Required. File or directory path (relative to workflow dir)
    phase_type: "prompt"            # Optional. One of: prompt, folder, input, pause
    auto_advance: true              # Optional. Default: true. Set false to pause before next phase
    schema: "phase-id/schema.json"  # Optional. Path to JSON schema for output validation
    inputs: []                      # Optional. References to prior phase outputs
    outputs: []                     # Optional. Declared output artifacts
    fields: []                      # Optional. Form fields (input phases only)
\`\`\`

### Workflow Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| \`id\` | string | Yes | — | Unique workflow identifier. Must match the directory name. |
| \`name\` | string | Yes | — | Human-readable name shown in UI |
| \`version\` | string | No | — | Semantic version (e.g., \`"1.0.0"\`) |
| \`description\` | string | No | — | Detailed description of the workflow |
| \`phases\` | array | Yes | — | Ordered list of phase definitions |

### Phase Definition Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| \`id\` | string | Yes | — | Unique phase ID within this workflow |
| \`name\` | string | Yes | — | Human-readable phase name |
| \`intent\` | string | No | — | Purpose description — guides the agent's decision-making |
| \`path\` | string | Yes | — | Relative path to prompt file or phase directory |
| \`phase_type\` | enum | No | Auto-inferred | \`prompt\`, \`folder\`, \`input\`, or \`pause\` |
| \`auto_advance\` | bool | No | \`true\` | Whether to automatically advance to the next phase on completion |
| \`schema\` | string | No | — | Relative path to a JSON schema file |
| \`inputs\` | array | No | \`[]\` | Input references from prior phases |
| \`outputs\` | array | No | \`[]\` | Declared output artifacts |
| \`fields\` | array | No | \`[]\` | Form field definitions (only used by \`input\` phases) |

### Phase Type Auto-Detection

If \`phase_type\` is omitted, the engine infers it from the filesystem:
- If \`path\` resolves to a **directory** → \`folder\`
- If \`path\` resolves to a **file** → \`prompt\`

**Exception:** \`input\` and \`pause\` types are **never auto-detected** — you must declare them explicitly.

---

## Phase Types

### Prompt Phase

An AI agent executes instructions from a markdown file. This is the most common phase type.

\`\`\`yaml
- id: "interview"
  name: "User Interview"
  intent: "Gather project requirements through structured conversation"
  path: "interview/prompt.md"        # Points to a .md file
  phase_type: "prompt"               # Optional if path is a file
  outputs:
    - id: "findings"
      filename: "interview.md"
      description: "Summary of gathered requirements"
\`\`\`

The \`prompt.md\` file contains the agent's instructions. The engine reads the markdown, resolves any \`{{placeholders}}\`, injects prior phase outputs as context, and sends it all to the agent.

### Folder Phase

A self-contained script that runs independently of the agent. Use this for deterministic operations like builds, deployments, git commands, API calls, or any task where you want full shell access without permission prompts.

\`\`\`yaml
- id: "implementation"
  name: "Implementation"
  intent: "Implement the approved user stories"
  path: "implementation/"            # Points to a directory
  phase_type: "folder"              # Optional if path is a directory
  auto_advance: false               # Pause after completion for review
  inputs:
    - id: "prd_input"
      from_phase: "prd"
      from_output: "document"
  outputs:
    - id: "progress"
      filename: "progress.json"
      description: "Implementation progress log"
\`\`\`

The directory must contain a \`run.sh\` file. See the Folder Phases section below for details.

### Input Phase

Displays a form in the UI to collect user input. All values are written to a shared \`inputs.yaml\` file and become available as \`{{placeholders}}\` in downstream phases.

\`\`\`yaml
- id: "config"
  name: "Project Configuration"
  intent: "Collect project settings from the user"
  path: "config"                     # Can be any value; not used for file lookup
  phase_type: "input"               # MUST be declared explicitly
  fields:
    - name: "repo_url"
      label: "Repository URL"
      placeholder: "https://github.com/org/repo"
      description: "The Git repository to work with"
      required: true
    - name: "branch_name"
      label: "Branch Name"
      placeholder: "feature/my-feature"
      required: true
    - name: "notes"
      label: "Additional Notes"
      placeholder: "Any extra context..."
      required: false
\`\`\`

When the user submits, values are appended to \`outputs/inputs.yaml\` as YAML key-value pairs:

\`\`\`yaml
repo_url: https://github.com/org/repo
branch_name: feature/my-feature
notes: Deploy to staging first
\`\`\`

These values are then available as \`{{repo_url}}\`, \`{{branch_name}}\`, and \`{{notes}}\` in any downstream prompt phase.

### Pause Phase

Halts workflow execution until the user explicitly resumes. Use this as a review gate between phases.

\`\`\`yaml
- id: "review"
  name: "Review Gate"
  intent: "Pause for stakeholder review before proceeding"
  path: "review"                     # Not used for file lookup
  phase_type: "pause"               # MUST be declared explicitly
  auto_advance: false
  inputs:
    - id: "plan_input"
      from_phase: "planning"
      from_output: "plan"
\`\`\`

---

## Inputs and Outputs — Wiring Phases Together

The input/output system is how phases pass data to each other.

### Declaring Outputs

\`\`\`yaml
outputs:
  - id: "findings"                   # Unique ID, used as reference
    filename: "interview.md"         # Filename in the output/ directory
    description: "Interview summary" # Optional description
\`\`\`

If \`filename\` is omitted, it defaults to \`{id}.txt\`.

### Declaring Inputs (Referencing Prior Outputs)

\`\`\`yaml
inputs:
  - id: "interview_data"            # Unique ID for this input reference
    from_phase: "interview"          # ID of the source phase
    from_output: "findings"          # ID of the output on the source phase
\`\`\`

At runtime, the engine reads the output file from the source phase and injects its contents into the current phase's context. The agent sees it like this:

\`\`\`
## Inputs

### interview_data (from phase: interview)
[contents of interview.md]
\`\`\`

### Input Availability Check

Before starting a phase, the engine verifies all declared inputs exist as output files. If any are missing, the phase is **paused** with a message indicating which inputs are unavailable.

---

## Placeholders and Template Resolution

Placeholders use the \`{{name}}\` syntax in prompt markdown files and are resolved at runtime before the agent sees the prompt.

### Placeholder Sources (in priority order)

1. **Task context** — Key-value pairs set when the task was created (e.g., \`{{user_context}}\`)
2. **Input phase values** — Parsed from the shared \`inputs.yaml\` file (e.g., \`{{repo_url}}\`, \`{{branch_name}}\`)

### Example

Given this \`inputs.yaml\`:
\`\`\`yaml
repo_url: https://github.com/org/repo
branch_name: feature/release-notes
\`\`\`

And this \`prompt.md\`:
\`\`\`markdown
# Create Branch

Clone the repository at {{repo_url}} and create a new branch called {{branch_name}}.

Then set up the development environment.
\`\`\`

The agent receives:
\`\`\`markdown
# Create Branch

Clone the repository at https://github.com/org/repo and create a new branch called feature/release-notes.

Then set up the development environment.
\`\`\`

### Resolution Rules

- Pattern: \`{{key}}\` — double curly braces
- If a placeholder has no matching value, it remains as literal text \`{{key}}\` in the prompt
- Keys are matched case-sensitively
- Values from \`inputs.yaml\` are trimmed and have surrounding quotes stripped

---

## Input Phase Fields

Input phases display a form in the UI. Each field is defined in the \`fields\` array.

### Field Schema

\`\`\`yaml
fields:
  - name: "field_name"              # Required. Programmatic name (becomes YAML key and placeholder name)
    label: "Display Label"          # Required. Label shown in the form UI
    placeholder: "hint text"        # Optional. Placeholder text in the input
    description: "Help text"        # Optional. Description shown below the field
    required: true                  # Optional. Default: true
\`\`\`

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| \`name\` | string | Yes | — | Programmatic name. Becomes the key in \`inputs.yaml\` and the placeholder name \`{{name}}\` |
| \`label\` | string | Yes | — | Display label shown to the user |
| \`placeholder\` | string | No | — | Placeholder/hint text in the form input |
| \`description\` | string | No | — | Help text displayed below the input field |
| \`required\` | bool | No | \`true\` | Whether the field must be filled before submission |

### How Input Values Flow

\`\`\`
User fills form → values written to outputs/inputs.yaml → available as {{placeholders}} in prompts
\`\`\`

The \`name\` field is the bridge: it becomes both the YAML key and the placeholder name.

---

## Folder Phases and run.sh

Folder phases execute a \`run.sh\` bash script with full shell access. Use for git operations, build processes, API calls, spawning CLI tools, or any operation needing shell access without permission prompts.

### Required File

The phase directory **must** contain a \`run.sh\` file. The engine runs it as \`bash run.sh\` from the phase directory.

### Environment Variables

The engine provides these environment variables to \`run.sh\`:

| Variable | Description |
|----------|-------------|
| \`WORKFLOW_TASK_ID\` | Current task ID |
| \`WORKFLOW_PHASE_ID\` | Current phase ID |
| \`WORKFLOW_OUTPUT_DIR\` | Directory to write output files |
| \`WORKFLOW_STATUS_FILE\` | Path to write progress updates |
| \`WORKFLOW_WORKING_DIR\` | Task working directory (if set) |
| \`WORKFLOW_INPUT_{ID}\` | Path to each input file (uppercased input ID) |

The \`WORKFLOW_INPUT_*\` variables are derived from the phase's \`inputs\` array. The input \`id\` is uppercased. For example, an input with \`id: "prd_input"\` becomes \`WORKFLOW_INPUT_PRD_INPUT\`.

### Progress Reporting

Scripts can report progress by writing to \`$WORKFLOW_STATUS_FILE\`. The engine polls this file every 5 seconds and emits progress events to the UI.

\`\`\`json
{
  "status": "running",
  "message": "Implementing user story 3 of 7...",
  "percent": 42,
  "input_tokens": 1500,
  "output_tokens": 800
}
\`\`\`

| Field | Type | Description |
|-------|------|-------------|
| \`status\` | string | Current status (freeform, e.g., \`"started"\`, \`"running"\`, \`"completed"\`) |
| \`message\` | string | Human-readable progress message shown in UI |
| \`percent\` | number | Progress percentage (0-100) |
| \`input_tokens\` | number | Token usage tracking (optional) |
| \`output_tokens\` | number | Token usage tracking (optional) |

### Example run.sh

\`\`\`bash
#!/bin/bash
set -euo pipefail

# Report starting
echo '{"status": "started", "message": "Starting implementation...", "percent": 0}' > "$WORKFLOW_STATUS_FILE"

# Read input from prior phase
PRD=$(cat "$WORKFLOW_INPUT_PRD_INPUT")

# Do work...
echo '{"status": "running", "message": "Building feature...", "percent": 50}' > "$WORKFLOW_STATUS_FILE"

# Write output file(s) to the output directory
echo '{"completed_stories": 7, "status": "done"}' > "$WORKFLOW_OUTPUT_DIR/progress.json"

# Report completion
echo '{"status": "completed", "message": "All stories implemented", "percent": 100}' > "$WORKFLOW_STATUS_FILE"
\`\`\`

### Success/Failure

- **Exit code 0** → phase marked as completed (after validating declared outputs exist)
- **Non-zero exit code** → phase marked as failed
- If a declared output file is missing after script exits successfully → phase marked as failed

---

## Schemas for Structured Output

Prompt phases can include a JSON schema that defines the expected shape of the output. The schema is injected into the agent's context so it produces correctly structured output.

\`\`\`yaml
- id: "prd"
  name: "Generate PRD"
  path: "prd/prompt.md"
  schema: "prd/schema.json"          # Relative to workflow directory
  outputs:
    - id: "document"
      filename: "prd.json"
\`\`\`

The agent sees the schema in its context under \`## Output Schema\` and uses it to structure its output.

---

## Phase Context — What the Agent Sees

When a prompt phase executes, the engine constructs a full context block and sends it to the agent. The agent receives (in this order):

1. **Workflow & Task info** — workflow name, task/project name, phase number
2. **Intent** — the phase's intent field
3. **Inputs** — contents of all declared input files
4. **Expected Outputs** — what files to write and where
5. **Output Schema** — JSON schema if provided
6. **Instructions** — contents of prompt.md with \`{{placeholders}}\` resolved
7. **Project Context** — user-provided context from task creation
8. **Working Directory** — where the agent runs
9. **Guidance** — instructions on how to write outputs and signal completion

### Key Takeaways for Prompt Authors

1. **Don't repeat context in your prompt** — The engine already injects the intent, inputs, schema, and output expectations. Your \`prompt.md\` should focus on *how* to do the work, not *what* inputs are available.
2. **Use \`{{placeholders}}\`** — They get resolved before the agent sees the prompt.
3. **Reference inputs by reading them** — The agent sees input file contents in the Inputs section automatically.
4. **The agent knows where to write** — The guidance section tells it the exact XML tags to use.

---

## Task Lifecycle

### Task Statuses

| Status | Meaning |
|--------|---------|
| \`pending\` | Created but not started |
| \`running\` | Executing phases sequentially |
| \`completed\` | All phases finished successfully |
| \`failed\` | A phase failed |
| \`stopped\` | User manually stopped the task |
| \`archived\` | Manually archived by user |

### Phase Statuses

| Status | Meaning |
|--------|---------|
| \`running\` | Phase currently executing |
| \`paused\` | Waiting for user action (input submission, resume, missing inputs) |
| \`completed\` | Phase finished, outputs validated |
| \`skipped\` | Phase was explicitly skipped |
| \`failed\` | Phase execution failed |
| \`stopped\` | Parent task was stopped |

---

## Importing Workflows

To import a workflow, use the **Import** button in the Tasks sidebar. Select a folder containing a \`workflow.yaml\` file. The folder will be copied into your workflows directory. You can also ask an LLM to create a workflow folder for you and then import it.

---

## Checklist for Workflow Authors

- Workflow directory matches the \`id\` in \`workflow.yaml\`
- Every phase has a unique \`id\` and descriptive \`name\`
- \`phase_type: input\` and \`phase_type: pause\` are declared explicitly (never auto-detected)
- Prompt phases have a \`prompt.md\` file at the specified \`path\`
- Folder phases have a \`run.sh\` file in the specified directory
- \`run.sh\` is executable and uses \`set -euo pipefail\`
- \`run.sh\` writes declared output files to \`$WORKFLOW_OUTPUT_DIR\`
- All \`from_phase\` and \`from_output\` references point to real phase/output IDs
- Output \`filename\` values are unique across all phases (flat output directory)
- Input field \`name\` values don't collide with each other or with task context keys
- Schemas are valid JSON Schema if provided
- \`auto_advance: false\` is set on phases that need human review
- Placeholders in prompts (\`{{key}}\`) match actual input field names or context keys
- Phase order makes sense — a phase's inputs must come from phases that run before it

---

## Quick Reference

\`\`\`
Phase Types:     prompt | folder | input | pause
                 (auto)   (auto)  (manual) (manual)

Placeholders:    {{field_name}} in prompt.md → resolved from inputs.yaml + task context

Input → Output:  inputs[].from_phase + from_output → reads source phase's output file

Input Phase:     fields[] → form UI → outputs/inputs.yaml → {{placeholders}}

Folder Phase:    run.sh + env vars (WORKFLOW_OUTPUT_DIR, WORKFLOW_STATUS_FILE, etc.)

Progress:        echo '{"status":"running","message":"...","percent":50}' > $WORKFLOW_STATUS_FILE

Output Files:    All written to {task_dir}/output/ (flat, no nesting)

Output Default:  If filename omitted → {output_id}.txt
\`\`\`
`;

interface WorkflowExample {
    id: string;
    name: string;
    description: string;
    files: Record<string, string>;
}

const WORKFLOW_EXAMPLES: WorkflowExample[] = [
    {
        id: "ralph",
        name: "Ralph - Product Development",
        description: "End-to-end product workflow: stakeholder interview, PRD generation with user stories, review gate, then iterative implementation.",
        files: {
            "workflow.yaml": `id: ralph
            name: Ralph - Product Development
            version: "1.0.0"
description: >
            End-to-end product development workflow: gather requirements through
            stakeholder interview, generate a structured PRD with user stories,
            then iteratively implement each story using an agent CLI.

            phases:
            - id: interview
            name: Interview
    intent: >
            Conduct a requirements gathering session. Ask the user targeted questions
            to capture functional requirements, design considerations, technical
            constraints, and success criteria.
            path: interview_prompt.md
            outputs:
            - id: interview
            filename: interview.md
            description: Comprehensive interview findings in markdown

            - id: prd
            name: PRD
    intent: >
            Convert interview findings into a structured PRD (prd.json) with
            right-sized, dependency-ordered user stories.
            path: prd_prompt.md
            schema: prd_schema.json
            inputs:
            - id: interview_findings
            from_phase: interview
            from_output: interview
            outputs:
            - id: prd
            filename: prd.json
            description: Structured PRD with ordered user stories

            - id: review
            name: Review
            phase_type: pause
            path: ""

            - id: implementation
            name: Implementation
    intent: >
            Iteratively implement user stories from the PRD. Each iteration picks
            the highest-priority unfinished story, implements it, runs quality
            checks, commits, and marks it done. Loops until all stories pass.
            path: implementation
            inputs:
            - id: prd_document
            from_phase: prd
            from_output: prd
            outputs:
            - id: progress
            filename: progress.txt
            description: Implementation progress log`,
            "interview_prompt.md": `# Interview Phase Prompt

            ## Objective
            Conduct a comprehensive requirements gathering session to capture ALL information needed for PRD generation.

            **CRITICAL CONTEXT**:
            - The next phase (PRD) will NOT have user input - it can only research existing files
            - This interview is your ONLY opportunity to ask the user clarifying questions
            - Capture enough detail so the PRD can be written without further user consultation
            - Be purposeful: only ask questions that genuinely need user input

            **CRITICAL INSTRUCTIONS**:
            - Ask questions **ONE AT A TIME** - wait for response before next question
            - Do NOT ask multiple questions in a single message
            - Do NOT read project files or implement code in this phase
            - Focus ONLY on understanding and documenting requirements

            ## Information to Capture

            ### 1. Project Vision & Context
            - What problem are we solving?
            - Who are the target users?
            - What is the expected outcome?

            ### 2. Functional Requirements (CRITICAL)
            - What specific actions must users be able to perform?
            - What should happen when users interact with each feature?
            - What are the expected inputs and outputs?
            - What are the edge cases and error conditions?

            ### 3. Non-Goals (Scope Management)
            - What will this feature NOT include?
            - What related functionality is explicitly out of scope?

            ### 4. Design Considerations (if applicable)
            - Are there UI/UX requirements or preferences?
            - Are there existing components or patterns to reuse?

            ### 5. Technical Considerations (if applicable)
            - Are there known technical constraints or dependencies?
            - Are there integration points with existing systems?

            ### 6. Success Criteria
            - How will we measure success?
            - What does "done" look like?

            ## Output Format

            Document all findings in a single markdown file: interview.md

            ## Completion

            When you have gathered sufficient requirements:
            1. Write the interview findings to interview.md using the write_output action
            2. Mark the phase as complete using the complete_phase action`,
            "prd_prompt.md": `# PRD (Product Requirements Document) Phase Prompt

            ## Objective
            Convert the interview findings into a detailed, executable PRD (prd.json) with properly sized, ordered user stories.

            **CRITICAL CONTEXT**:
            - You do NOT have user input in this phase - read the Interview findings for requirements
            - The interview.md file contains ALL user requirements and context you need
            - You CAN research the codebase in the working directory to understand existing patterns
            - Your output (prd.json) will be used directly by the Implementation phase
            - User stories MUST be sized for one iteration (one context window)

            ## Critical Rules

            ### 1. Story Size: The Number One Rule
            Each story must be completable in ONE iteration (one context window). If a story is too big, the LLM runs out of context before finishing.

            **Right-sized stories**:
            - Add a database column and migration
            - Add a UI component to an existing page
            - Update a server action with new logic
            - Create one API endpoint with tests

            **Too big** (MUST split):
- "Build the entire dashboard" -> Split into: schema, queries, UI components, filters
- "Add authentication" -> Split into: schema, middleware, login UI, session handling

            **Rule of thumb**: If you cannot describe the change in 2-3 sentences, it is too big. SPLIT IT.

            ### 2. Story Ordering: Dependencies First
            Stories execute in priority order. Earlier stories must not depend on later ones.

            ### 3. Acceptance Criteria: Must Be Verifiable
            Each criterion must be something that can be CHECKED.
            - ALWAYS include: "Typecheck passes"
            - For stories with testable logic: "Tests pass"

            ## Output Format

            Create prd.json with the following structure:

            \\\`\\\`\\\`json
            {
                "project": "ProjectName",
            "branchName": "ralph/feature-name",
            "description": "Brief description",
            "userStories": [
            {
                "id": "US-001",
            "title": "Brief, specific title",
            "description": "As a [role], I want [goal] so that [benefit].",
            "acceptanceCriteria": ["Specific criterion 1", "Typecheck passes"],
            "priority": 1,
            "passes": false,
            "notes": ""
    }
            ]
}
            \\\`\\\`\\\`

            ## Completion

            When you have generated the PRD:
            1. Write prd.json using the write_output action
            2. Mark the phase as complete using the complete_phase action`,
            "prd_schema.json": `{
                "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "PRD Schema",
            "type": "object",
            "required": ["project", "branchName", "description", "userStories"],
            "properties": {
                "project": {
                "type": "string",
            "minLength": 1
    },
            "branchName": {
                "type": "string",
            "pattern": "^[a-zA-Z0-9/_-]+$"
    },
            "description": {
                "type": "string",
            "minLength": 1
    },
            "userStories": {
                "type": "array",
            "minItems": 1,
            "items": {
                "type": "object",
            "required": ["id", "title", "description", "acceptanceCriteria", "priority", "passes", "notes"],
            "properties": {
                "id": {"type": "string", "pattern": "^US-\\\\d{3}$" },
            "title": {"type": "string", "minLength": 1 },
            "description": {"type": "string", "minLength": 1 },
            "acceptanceCriteria": {"type": "array", "minItems": 1, "items": {"type": "string" } },
            "priority": {"type": "integer", "minimum": 1 },
            "passes": {"type": "boolean" },
            "notes": {"type": "string" }
        }
      }
    }
  }
}`,
            "implementation/prompt.md": `# Implementation Phase Prompt

            ## Objective
            You are an autonomous coding agent implementing user stories from the PRD.

            **CRITICAL - YOU MUST ONLY IMPLEMENT ONE SINGLE STORY THEN STOP**:
            - You are called in a loop by an external script. Each invocation = one story.
            - Pick the SINGLE highest priority story where \`passes: false\`
            - Implement ONLY that one story, commit it, update progress
            - Then STOP IMMEDIATELY. Do NOT continue to the next story.
            - The external loop will call you again for the next story automatically.

            ## Input Files

            - **PRD file**: Read from \`$PRD_FILE\` environment variable
            - **Progress file**: \`$PROGRESS_FILE\` — append learnings here
            - **Working directory**: \`$WORKING_DIR\` — make ALL code changes here
            - **Output directory**: \`$OUTPUT_DIR\` — write final progress.txt here when done

            ## Per-Invocation Steps

            1. **Read the PRD** from \`$PRD_FILE\`
            2. **Read progress log** from \`$PROGRESS_FILE\` (check Codebase Patterns section first)
            3. **Check branch** - Verify you're on the correct branch from PRD \`branchName\`
            4. **Pick highest priority story** where \`passes: false\`
            5. **Implement that single story** in \`$WORKING_DIR\`
            6. **Run quality checks** (typecheck, lint, test)
            7. **Commit changes**:
            \`\`\`bash
            git add .
            git commit -m "feat: [Story ID] - [Story Title]"
            \`\`\`
            8. **Update PRD** - Set \`passes: true\` for the completed story
            9. **Append progress** to \`$PROGRESS_FILE\`

            ## Progress Report Format

            **APPEND** to the progress file (never replace):

            \`\`\`
            ## [Date/Time] - [Story ID]
            - What was implemented
            - Files changed
            - **Learnings for future iterations:**
            - Patterns discovered
            - Gotchas encountered
            - Useful context
            ---
            \`\`\`

            ## Quality Requirements

            - ALL commits must pass quality checks (typecheck, lint, test)
            - Do NOT commit broken code
            - Keep changes focused and minimal
            - Follow existing code patterns

            ## Stop Condition

            After implementing your ONE story, check if ALL stories have \`passes: true\`:
            - **If ALL complete**: Write final progress to \`$OUTPUT_DIR/progress.txt\`, then stop
            - **If stories remain**: STOP NOW. The loop will call you again.

            ## Remember

            - EXACTLY ONE story per invocation
            - Read Codebase Patterns first
            - Work in \`$WORKING_DIR\`
            - Commit frequently
            - Keep CI green`,
            "implementation/run.sh": `#!/bin/bash
            # Implementation Phase - Folder Phase Runner
            # Iteratively works through user stories in prd.json until complete.
            #
            # Uses the \`claude\` CLI to implement each story.

            set -e

            SCRIPT_DIR="$(cd "$(dirname "\${BASH_SOURCE[0]}")" && pwd)"

            # ── Resolve env vars (set by workflow runner) ────────────────────────
            PRD_FILE="\${WORKFLOW_INPUT_PRD_DOCUMENT:-}"
            OUTPUT_DIR="\${WORKFLOW_OUTPUT_DIR:-}"
            MAX_ITERATIONS=32

            if [[ -z "$PRD_FILE" ]]; then
            echo "ERROR: WORKFLOW_INPUT_PRD_DOCUMENT env var is required"
            exit 1
            fi

            if [[ -z "$OUTPUT_DIR" ]]; then
            echo "ERROR: WORKFLOW_OUTPUT_DIR env var is required"
            exit 1
            fi

            if [[ ! -f "$PRD_FILE" ]]; then
            echo "ERROR: PRD file not found at $PRD_FILE"
            exit 1
            fi

            # ── Token tracking helpers ───────────────────────────────────────────
            TOTAL_INPUT_TOKENS=0
            TOTAL_OUTPUT_TOKENS=0

            update_status() {
                local status="$1"
            local message="$2"
            local percent="\${3:-0}"
            if [[ -n "\${WORKFLOW_STATUS_FILE:-}" ]]; then
            echo "{\\"status\\":\\"$status\\",\\"message\\":\\"$message\\",\\"percent\\":$percent,\\"input_tokens\\":$TOTAL_INPUT_TOKENS,\\"output_tokens\\":$TOTAL_OUTPUT_TOKENS}" > "$WORKFLOW_STATUS_FILE"
            fi
}

            run_claude_with_usage() {
                local tmpfile
            tmpfile=$(mktemp)

            echo "Begin implementing the next story." | claude \\
            --dangerously-skip-permissions \\
            --print \\
            --output-format stream-json \\
            --verbose \\
            --append-system-prompt "$(cat "$PROMPT_FILE")" \\
            -p "$WORKING_DIR" \\
    > "$tmpfile" 2>&1 || true

  if command -v jq &> /dev/null; then
            local in_tok out_tok
            in_tok=$(grep -o '"input_tokens":[0-9]*' "$tmpfile" | tail -1 | grep -o '[0-9]*' || echo "0")
            out_tok=$(grep -o '"output_tokens":[0-9]*' "$tmpfile" | tail -1 | grep -o '[0-9]*' || echo "0")
            TOTAL_INPUT_TOKENS=$((TOTAL_INPUT_TOKENS + in_tok))
            TOTAL_OUTPUT_TOKENS=$((TOTAL_OUTPUT_TOKENS + out_tok))
            if [[ "$in_tok" -gt 0 || "$out_tok" -gt 0 ]]; then
            echo "  Tokens this iteration: in=\${in_tok} out=\${out_tok} (total: in=\${TOTAL_INPUT_TOKENS} out=\${TOTAL_OUTPUT_TOKENS})"
            fi
            fi

  if command -v jq &> /dev/null; then
    jq -r 'select(.type == "content_block_delta") | .delta.text // empty' "$tmpfile" 2>/dev/null || cat "$tmpfile"
            else
            cat "$tmpfile"
            fi

            rm -f "$tmpfile"
}

            # ── Setup ────────────────────────────────────────────────────────────
            PROGRESS_FILE="$OUTPUT_DIR/progress.txt"
            touch "$PROGRESS_FILE"

            echo "========================================="
            echo "  Implementation Phase - Build Features"
            echo "========================================="
            echo ""
            echo "PRD:        $PRD_FILE"
            echo "Output:     $OUTPUT_DIR"
            echo "Progress:   $PROGRESS_FILE"

if command -v jq &> /dev/null; then
            PROJECT_NAME=$(jq -r '.project' "$PRD_FILE")
            DESCRIPTION=$(jq -r '.description' "$PRD_FILE")
            STORY_COUNT=$(jq '.userStories | length' "$PRD_FILE")
            BRANCH_NAME=$(jq -r '.branchName' "$PRD_FILE")
            echo "Project:    $PROJECT_NAME"
            echo "Branch:     $BRANCH_NAME"
            echo "Stories:    $STORY_COUNT"
            echo "Description: $DESCRIPTION"
            fi

            echo ""

            # ── Resolve working directory ────────────────────────────────────────
            WORKING_DIR="\${WORKFLOW_WORKING_DIR:-$(pwd)}"
            echo "Working dir: $WORKING_DIR"
            echo ""

            if grep -q "<promise>COMPLETE</promise>" "$PROGRESS_FILE" 2>/dev/null; then
            echo "Implementation phase already marked as COMPLETE"
            exit 0
            fi

            PROMPT_FILE="$SCRIPT_DIR/prompt.md"
            if [[ ! -f "$PROMPT_FILE" ]]; then
            echo "ERROR: prompt.md not found at $PROMPT_FILE"
            exit 1
            fi

            echo "Starting implementation (max $MAX_ITERATIONS iterations)..."
            echo "---"

            # ── Iteration loop ───────────────────────────────────────────────────
            for i in $(seq 1 $MAX_ITERATIONS); do
            echo ""
            echo "==============================================================="
            echo "  Iteration $i of $MAX_ITERATIONS"
            echo "==============================================================="
            echo ""

  if command -v jq &> /dev/null; then
            TOTAL=$(jq '.userStories | length' "$PRD_FILE")
            DONE=$(jq '[.userStories[] | select(.passes == true)] | length' "$PRD_FILE")
            echo "Progress: $DONE / $TOTAL stories completed"
            update_status "running" "Iteration $i: $DONE of $TOTAL stories done" "$((DONE * 100 / TOTAL))"
            fi

            export PRD_FILE PROGRESS_FILE WORKING_DIR OUTPUT_DIR
            run_claude_with_usage

            echo ""
            echo "---"

            if grep -q "<promise>COMPLETE</promise>" "$PROGRESS_FILE" 2>/dev/null; then
            echo ""
            echo "Phase marked as COMPLETE in progress file"
            break
            fi

  if command -v jq &> /dev/null; then
            TOTAL=$(jq '.userStories | length' "$PRD_FILE")
            DONE=$(jq '[.userStories[] | select(.passes == true)] | length' "$PRD_FILE")
            if [[ "$DONE" == "$TOTAL" ]]; then
            echo "All $TOTAL stories completed!"
            echo "<promise>COMPLETE</promise>" >> "$PROGRESS_FILE"
            break
            fi
            echo "Stories remaining: $(($TOTAL - $DONE))"
            fi

            sleep 2
            done

            # ── Final summary ────────────────────────────────────────────────────
            echo ""
            echo "---"

            if grep -q "<promise>COMPLETE</promise>" "$PROGRESS_FILE" 2>/dev/null; then
            echo "Implementation phase completed successfully!"
            update_status "complete" "All stories implemented" 100
            else
            echo "WARNING: Phase not marked as complete"
            update_status "incomplete" "Max iterations reached" 100
            fi

if command -v jq &> /dev/null; then
            TOTAL=$(jq '.userStories | length' "$PRD_FILE")
            DONE=$(jq '[.userStories[] | select(.passes == true)] | length' "$PRD_FILE")
            echo "Final: $DONE / $TOTAL stories completed"
            fi

            echo "Total tokens: input=\${TOTAL_INPUT_TOKENS} output=\${TOTAL_OUTPUT_TOKENS}"

cp "$PROGRESS_FILE" "$OUTPUT_DIR/progress.txt" 2>/dev/null || true

            echo ""
            echo "Implementation phase execution finished!"`,
        },
    },
    {
        id: "content-pipeline",
        name: "Content Pipeline",
        description: "A simple 3-phase workflow: research a topic, write a draft, then review and polish. Demonstrates phase inputs and outputs.",
        files: {
            "workflow.yaml": `id: content-pipeline
            name: Content Pipeline
            version: "1.0"
            description: "Research, write, and polish content in three phases"

            phases:
            - id: research
            name: Research
            intent: Research the topic and gather key facts and sources
            path: research_prompt.md
            outputs:
            - id: notes
            filename: research_notes.md
            description: Research notes with key facts and sources

            - id: draft
            name: Write Draft
            intent: Write a first draft based on research findings
            path: draft_prompt.md
            inputs:
            - id: research_notes
            from_phase: research
            from_output: notes
            outputs:
            - id: draft
            filename: draft.md
            description: First draft of the content

            - id: polish
            name: Review & Polish
            intent: Review the draft for clarity, accuracy, and tone
            path: polish_prompt.md
            inputs:
            - id: original_research
            from_phase: research
            from_output: notes
            - id: first_draft
            from_phase: draft
            from_output: draft
            outputs:
            - id: final
            filename: final.md
            description: Polished final version`,
            "research_prompt.md": `# Research Phase

            Research the given topic thoroughly. Gather:
            - Key facts and statistics
            - Relevant sources and references
            - Different perspectives on the topic
            - Current trends or recent developments

            Compile your findings into structured research notes.`,
            "draft_prompt.md": `# Draft Writing

            Using the research notes provided, write a clear and engaging first draft.

            Focus on:
            - Logical structure and flow
            - Clear explanations of complex topics
            - Engaging opening and strong conclusion`,
            "polish_prompt.md": `# Review & Polish

            Review the draft against the original research notes.

            Check for:
            - Factual accuracy
            - Clarity and readability
            - Consistent tone and style
            - Grammar and punctuation

            Produce the final polished version.`,
        },
    },
    {
        id: "deep-research",
        name: "Deep Research Report",
        description: "User provides a topic and focus areas via an input form, then the agent researches, refines, pauses for review, and generates a rich markdown report with Mermaid diagrams and KaTeX formulas.",
        files: {
            "workflow.yaml": `id: deep-research
            name: Deep Research Report
            version: "1.0"
description: >
            Guided research workflow: collect a topic and focus areas from the user,
            conduct deep research, refine findings, pause for human review, then
            produce a polished markdown report with Mermaid diagrams and KaTeX math.

            phases:
            - id: intake
            name: Topic Intake
            phase_type: input
            path: ""
            fields:
            - name: topic
            label: Research Topic
            required: true
            placeholder: "e.g. Transformer architectures in NLP"
            - name: focus_areas
            label: Focus Areas
            required: false
            placeholder: "e.g. attention mechanisms, scaling laws, inference optimization"

            - id: research
            name: Deep Research
    intent: >
            Conduct thorough research on the topic, focusing on the areas
            specified by the user. Gather key findings, data points, relevant
            formulas, and relationships between concepts.
            path: research_prompt.md
            inputs:
            - id: user_intake
            from_phase: intake
            from_output: values
            outputs:
            - id: research_notes
            filename: research_notes.md
            description: Comprehensive research notes with sources

            - id: refinement
            name: Refinement
    intent: >
            Analyze and refine the raw research. Identify gaps, resolve
            contradictions, prioritize the most important findings, and
            outline the structure for the final report.
            path: refinement_prompt.md
            inputs:
            - id: raw_research
            from_phase: research
            from_output: research_notes
            outputs:
            - id: refined
            filename: refined_outline.md
            description: Refined outline with prioritized findings

            - id: review
            name: Human Review
            phase_type: pause
            path: ""

            - id: report
            name: Report Generation
    intent: >
            Produce a polished markdown report. Use Mermaid diagrams to
            visualize relationships, architectures, and flows. Use KaTeX
            for any mathematical formulas or equations. The report should
            be publication-ready.
            path: report_prompt.md
            inputs:
            - id: refined_outline
            from_phase: refinement
            from_output: refined
            - id: original_research
            from_phase: research
            from_output: research_notes
            outputs:
            - id: final_report
            filename: report.md
            description: Final markdown report with Mermaid and KaTeX`,
            "research_prompt.md": `# Deep Research Phase

            ## Topic
            {{ topic }}

            ## Focus Areas
            {{ focus_areas }}

            ## Your task

            1. **Understand the scope** — Research the topic above, concentrating on the specified focus areas
            2. **Research thoroughly** — Cover each focus area with:
            - Key concepts and definitions
            - Current state of the art
            - Important data points and statistics
            - Relevant mathematical formulas or relationships
            - How concepts connect to each other
            3. **Cite sources** — Note where information comes from
            4. **Identify visual opportunities** — Flag concepts that would benefit from diagrams (architectures, flows, comparisons, hierarchies)

            ## Output format

            Write structured research notes in markdown. Use clear headings for each focus area. Include a "Diagram Opportunities" section listing concepts that should be visualized.`,
            "refinement_prompt.md": `# Refinement Phase

            You have raw research notes from the previous phase. Your job is to refine them into a clear outline for the final report.

            ## Your task

            1. **Identify gaps** — Are any focus areas underexplored? Note what's missing
            2. **Resolve contradictions** — If sources disagree, note the consensus or explain the debate
            3. **Prioritize** — Rank findings by importance and relevance to the user's topic
            4. **Structure the report** — Create a detailed outline with:
            - Proposed sections and subsections
            - Which Mermaid diagram type to use for each visual (flowchart, sequence, class, etc.)
            - Which formulas need KaTeX rendering
            - Key takeaways for each section

            ## Output format

            Write the refined outline in markdown with clear section markers and annotations for diagrams and formulas.`,
            "report_prompt.md": `# Report Generation Phase

            You have a refined outline and the original research notes. Produce the final report.

            ## Requirements

            1. **Rich markdown** — Use headings, lists, tables, blockquotes, and emphasis effectively
            2. **Mermaid diagrams** — Include diagrams using fenced code blocks:
            \`\`\`mermaid
            graph TD
       A[Concept] --> B[Related]
            \`\`\`
            Use appropriate diagram types: flowchart for processes, sequence for interactions, class for hierarchies, pie for distributions
            3. **KaTeX formulas** — Include mathematical notation using:
            - Inline: \`$E = mc^2$\`
            - Display: \`$$\\sum_{i = 1}^{n} x_i$$\`
            4. **Structure** — Follow the refined outline. Each section should have:
            - Clear explanation
            - Supporting data or examples
            - Visuals (Mermaid/KaTeX) where they add clarity
            5. **Executive summary** — Start with a concise summary of key findings
            6. **References** — End with a references section

            ## Output

            Write the complete report as a single markdown file.`,
        },
    },
];

function WorkflowDocsView() {
    const [clonedIds, setClonedIds] = useState<Set<string>>(new Set());
    const [cloning, setCloning] = useState<string | null>(null);
    const [error, setError] = useState<string | null>(null);
    const fetchWorkflows = useWorkflowStore((s) => s.fetchWorkflows);

    const handleClone = async (example: WorkflowExample) => {
        setError(null);
        setCloning(example.id);
        try {
            await cloneExampleWorkflow(example.id, example.files);
            await fetchWorkflows();
            setClonedIds((prev) => new Set(prev).add(example.id));
        } catch (err: unknown) {
            const msg = err instanceof Error ? err.message : String(err);
            if (msg.includes("already exists")) {
                setError(`Workflow "${example.id}" already exists. Remove it first or rename.`);
            } else {
                setError(`Failed to clone: ${msg}`);
            }
        } finally {
            setCloning(null);
        }
    };

    return (
        <div className="flex flex-col flex-1 w-full max-w-3xl gap-8 overflow-y-auto">
            <div className="prose prose-sm max-w-none text-[var(--modal-text-primary)]">
                <RichMarkdown>{WORKFLOW_DOCS_MARKDOWN}</RichMarkdown>
            </div>

            {/* Example templates */}
            {error && (
                <div className="px-3 py-2 bg-red-500/10 border border-red-500/30 rounded-[8px] text-[13px] text-red-600 dark:text-red-400">
                    {error}
                </div>
            )}

            <div>
                <h3 className="text-[15px] font-semibold text-[var(--modal-text-primary)] mb-4">Example Templates</h3>
                <p className="text-[14px] text-[var(--modal-text-secondary)] leading-relaxed mb-4">
                    Clone these examples to your workflows directory to get started. You can modify them or use them as a reference
                    when asking an LLM to create new workflows for you.
                </p>
                <div className="flex flex-col gap-4">
                    {WORKFLOW_EXAMPLES.map((example) => {
                        const isCloned = clonedIds.has(example.id);
                        const isCloning = cloning === example.id;
                        return (
                            <div
                                key={example.id}
                                className="p-4 rounded-xl border border-[var(--modal-border-secondary)] bg-[var(--modal-bg-tertiary)]"
                            >
                                <div className="flex items-start justify-between gap-4">
                                    <div className="flex-1 min-w-0">
                                        <div className="text-[15px] font-semibold text-[var(--modal-text-primary)]">
                                            {example.name}
                                        </div>
                                        <p className="text-[13px] text-[var(--modal-text-secondary)] mt-1 leading-relaxed">
                                            {example.description}
                                        </p>
                                        <div className="flex flex-wrap gap-1.5 mt-2">
                                            {Object.keys(example.files).map((f) => (
                                                <span
                                                    key={f}
                                                    className="px-2 py-0.5 rounded bg-[var(--modal-bg)] text-[11px] font-mono text-[var(--modal-text-tertiary)]"
                                                >
                                                    {f}
                                                </span>
                                            ))}
                                        </div>
                                    </div>
                                    <button
                                        type="button"
                                        disabled={isCloning || isCloned}
                                        onClick={() => handleClone(example)}
                                        className={`flex-shrink-0 flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-[13px] font-medium transition-colors cursor-pointer ${isCloned
                                            ? "bg-green-500/10 text-green-600 dark:text-green-400"
                                            : "bg-[var(--modal-accent)] text-white hover:opacity-90"
                                            } disabled:opacity-50`}
                                    >
                                        {isCloned ? (
                                            <>
                                                <CheckCircle2 size={14} />
                                                Cloned
                                            </>
                                        ) : isCloning ? (
                                            "Cloning..."
                                        ) : (
                                            <>
                                                <Copy size={14} />
                                                Clone
                                            </>
                                        )}
                                    </button>
                                </div>
                            </div>
                        );
                    })}
                </div>
            </div>
        </div>
    );
}

function SleepGuardSettings() {
    const [enabled, setEnabled] = useState(true);
    const [hours, setHours] = useState(4);
    const [workflowGuardEnabled, setWorkflowGuardEnabled] = useState(true);
    const [agentRunGuardEnabled, setAgentRunGuardEnabled] = useState(true);
    const [tasklistGuardEnabled, setTasklistGuardEnabled] = useState(true);
    const [keepDisplayAwake, setKeepDisplayAwake] = useState(false);
    const [saveError, setSaveError] = useState<string | null>(null);
    const loadedRef = useRef(false);

    useEffect(() => {
        if (loadedRef.current) return;
        loadedRef.current = true;
        getPreferences().then((prefs) => {
            if (prefs.max_sleep_guard_hours === null || prefs.max_sleep_guard_hours === undefined) {
                setEnabled(false);
            } else {
                setEnabled(true);
                setHours(prefs.max_sleep_guard_hours);
            }
            setWorkflowGuardEnabled(prefs.prevent_sleep_during_workflows ?? true);
            setAgentRunGuardEnabled(prefs.prevent_sleep_during_agent_runs ?? true);
            setTasklistGuardEnabled(prefs.prevent_sleep_during_tasklists ?? true);
            setKeepDisplayAwake(prefs.keep_display_awake ?? false);
        }).catch(() => {});
    }, []);

    const savePrefs = useCallback(async (patch: Partial<UserPreferences>) => {
        setSaveError(null);
        try {
            const prefs = await getPreferences();
            await putPreferences({
                ...prefs,
                ...patch,
            });
        } catch (err) {
            console.error("[SleepGuardSettings] save failed:", err);
            setSaveError("Failed to save sleep guard preference. Please try again.");
        }
    }, []);

    const handleToggle = (checked: boolean) => {
        setEnabled(checked);
        savePrefs({ max_sleep_guard_hours: checked ? hours : null });
    };

    const handleHoursBlur = () => {
        const clamped = Math.max(0.5, Math.min(24, hours));
        setHours(clamped);
        if (enabled) savePrefs({ max_sleep_guard_hours: clamped });
    };

    const handleWorkflowToggle = (checked: boolean) => {
        setWorkflowGuardEnabled(checked);
        savePrefs({ prevent_sleep_during_workflows: checked });
    };

    const handleAgentRunToggle = (checked: boolean) => {
        setAgentRunGuardEnabled(checked);
        savePrefs({ prevent_sleep_during_agent_runs: checked });
    };

    const handleTasklistToggle = (checked: boolean) => {
        setTasklistGuardEnabled(checked);
        savePrefs({ prevent_sleep_during_tasklists: checked });
    };

    const handleKeepDisplayAwakeToggle = (checked: boolean) => {
        setKeepDisplayAwake(checked);
        savePrefs({ keep_display_awake: checked });
    };

    return (
        <div className="flex flex-col flex-1 w-full max-w-3xl gap-10 pt-2">
            {saveError && (
                <div className="px-3 py-2 bg-red-500/10 border border-red-500/30 rounded-[8px] text-[13px] text-red-600 dark:text-red-400">
                    {saveError}
                </div>
            )}
            <p className="text-[13px] text-[var(--modal-text-secondary)]">
                These toggles only prevent idle sleep — they can't keep your Mac awake if the lid is closed or the system is set to sleep on battery.
            </p>
            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Display</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    By default your Mac's screen can turn off while a task runs — the task still completes. Turn this on to keep the screen lit while the sleep guard is active.
                </p>

                <label className="flex items-center gap-2 cursor-pointer">
                    <input
                        type="checkbox"
                        checked={keepDisplayAwake}
                        onChange={(e) => handleKeepDisplayAwakeToggle(e.target.checked)}
                        className="w-4 h-4 text-[var(--modal-accent)] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded focus:ring-[var(--modal-accent)] cursor-pointer"
                    />
                    <span className="text-[14px] text-[var(--modal-text-primary)]">Keep the display on</span>
                </label>
            </div>

            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Scheduled items</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Keep your computer awake when a scheduled item is coming up soon so that it fires on time.
                </p>

                <label className="flex items-center gap-2 mb-4 cursor-pointer">
                    <input
                        type="checkbox"
                        checked={enabled}
                        onChange={(e) => handleToggle(e.target.checked)}
                        className="w-4 h-4 text-[var(--modal-accent)] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded focus:ring-[var(--modal-accent)] cursor-pointer"
                    />
                    <span className="text-[14px] text-[var(--modal-text-primary)]">Enable sleep guard for scheduled items</span>
                </label>

                <div>
                    <label className="block text-[13px] text-[var(--modal-text-secondary)] mb-1">Hours in advance</label>
                    <input
                        type="number"
                        min={0.5}
                        max={24}
                        step={0.5}
                        value={hours}
                        onChange={(e) => setHours(parseFloat(e.target.value) || 4)}
                        onBlur={handleHoursBlur}
                        disabled={!enabled}
                        className={twMerge(
                            "w-32 px-3 py-2 bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded-[8px] text-[14px] text-[var(--modal-text-primary)] focus:outline-none transition-colors",
                            !enabled ? "opacity-50 cursor-not-allowed" : "focus:border-[var(--modal-accent)]"
                        )}
                    />
                    <p className="text-[12px] text-[var(--modal-text-secondary)] mt-2">
                        Your computer will stay awake this many hours before a scheduled item is due to fire.
                    </p>
                </div>
            </div>

            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Workflows</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Keep your computer awake while any workflow task is running so it can finish without being interrupted by sleep.
                </p>

                <label className="flex items-center gap-2 cursor-pointer">
                    <input
                        type="checkbox"
                        checked={workflowGuardEnabled}
                        onChange={(e) => handleWorkflowToggle(e.target.checked)}
                        className="w-4 h-4 text-[var(--modal-accent)] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded focus:ring-[var(--modal-accent)] cursor-pointer"
                    />
                    <span className="text-[14px] text-[var(--modal-text-primary)]">Prevent sleep while workflows are running</span>
                </label>
            </div>

            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Agent runs</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Keep your computer awake while any agent is actively responding — including background and delegated subagents — so its run isn’t cut short by display sleep.
                </p>

                <label className="flex items-center gap-2 cursor-pointer">
                    <input
                        type="checkbox"
                        checked={agentRunGuardEnabled}
                        onChange={(e) => handleAgentRunToggle(e.target.checked)}
                        className="w-4 h-4 text-[var(--modal-accent)] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded focus:ring-[var(--modal-accent)] cursor-pointer"
                    />
                    <span className="text-[14px] text-[var(--modal-text-primary)]">Prevent sleep while an agent run is in flight</span>
                </label>
            </div>

            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Tasklists</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Keep your computer awake while any tasklist task is queued or running so the queue can drain without sleep interruptions.
                </p>

                <label className="flex items-center gap-2 cursor-pointer">
                    <input
                        type="checkbox"
                        checked={tasklistGuardEnabled}
                        onChange={(e) => handleTasklistToggle(e.target.checked)}
                        className="w-4 h-4 text-[var(--modal-accent)] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded focus:ring-[var(--modal-accent)] cursor-pointer"
                    />
                    <span className="text-[14px] text-[var(--modal-text-primary)]">Prevent sleep while a tasklist is running</span>
                </label>
            </div>
        </div>
    );
}

const NOTIFICATION_SNOOZE_OPTIONS: { label: string; ms: number }[] = [
    { label: "30 min", ms: 30 * 60 * 1000 },
    { label: "1 hour", ms: 60 * 60 * 1000 },
    { label: "2 hours", ms: 2 * 60 * 60 * 1000 },
];

function msUntilTomorrow8AM(): number {
    const now = new Date();
    const tomorrow8am = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1, 8, 0, 0, 0);
    return tomorrow8am.getTime() - now.getTime();
}

function NotificationSettings() {
    const notificationsEnabled = useUserPreferencesStore((s) => s.notificationsEnabled);
    const setNotificationsEnabled = useUserPreferencesStore((s) => s.setNotificationsEnabled);
    const notifyBanner = useUserPreferencesStore((s) => s.notifyBanner);
    const setNotifyBanner = useUserPreferencesStore((s) => s.setNotifyBanner);
    const notifySound = useUserPreferencesStore((s) => s.notifySound);
    const setNotifySound = useUserPreferencesStore((s) => s.setNotifySound);
    const notifyAgentReplies = useUserPreferencesStore((s) => s.notifyAgentReplies);
    const setNotifyAgentReplies = useUserPreferencesStore((s) => s.setNotifyAgentReplies);
    const notifySnoozedUntil = useUserPreferencesStore((s) => s.notifySnoozedUntil);
    const snoozeNotifications = useUserPreferencesStore((s) => s.snoozeNotifications);
    const clearNotificationSnooze = useUserPreferencesStore((s) => s.clearNotificationSnooze);

    const [permissionStatus, setPermissionStatus] = useState<"unknown" | "granted" | "denied">("unknown");

    useEffect(() => {
        let cancelled = false;
        isPermissionGranted()
            .then((granted) => {
                if (!cancelled) setPermissionStatus(granted ? "granted" : "denied");
            })
            .catch(() => {});
        return () => {
            cancelled = true;
        };
    }, []);

    const handleMasterToggle = async (checked: boolean) => {
        setNotificationsEnabled(checked);
        if (checked) {
            const granted = await ensureNotificationPermission();
            setPermissionStatus(granted ? "granted" : "denied");
        }
    };

    const isSnoozed = isNotificationSnoozed({ notifySnoozedUntil }, Date.now());

    return (
        <div className="flex flex-col flex-1 w-full max-w-3xl gap-10 pt-2">
            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Desktop notifications</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Get notified when a scheduled item, agent run, or workflow needs your attention.
                </p>

                <label className="flex items-center gap-2 mb-2 cursor-pointer">
                    <input
                        type="checkbox"
                        checked={notificationsEnabled}
                        onChange={(e) => handleMasterToggle(e.target.checked)}
                        className="w-4 h-4 text-[var(--modal-accent)] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded focus:ring-[var(--modal-accent)] cursor-pointer"
                    />
                    <span className="text-[14px] text-[var(--modal-text-primary)]">Enable desktop notifications</span>
                </label>

                {notificationsEnabled && permissionStatus === "denied" && (
                    <p className="text-[12px] text-amber-600 dark:text-amber-400 mb-2">
                        Notifications are blocked for Launchpad Studio at the OS level. Enable them in System Settings → Notifications to actually receive alerts.
                    </p>
                )}

                <label className="flex items-center gap-2 mb-2 cursor-pointer">
                    <input
                        type="checkbox"
                        checked={notifyBanner}
                        disabled={!notificationsEnabled}
                        onChange={(e) => setNotifyBanner(e.target.checked)}
                        className="w-4 h-4 text-[var(--modal-accent)] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded focus:ring-[var(--modal-accent)] cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                    />
                    <span className={twMerge("text-[14px] text-[var(--modal-text-primary)]", !notificationsEnabled && "opacity-50")}>Show banners</span>
                </label>

                <label className="flex items-center gap-2 cursor-pointer">
                    <input
                        type="checkbox"
                        checked={notifySound}
                        disabled={!notificationsEnabled}
                        onChange={(e) => setNotifySound(e.target.checked)}
                        className="w-4 h-4 text-[var(--modal-accent)] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded focus:ring-[var(--modal-accent)] cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                    />
                    <span className={twMerge("text-[14px] text-[var(--modal-text-primary)]", !notificationsEnabled && "opacity-50")}>Play sound</span>
                </label>

                <label className="flex items-start gap-2 mt-3 cursor-pointer">
                    <input
                        type="checkbox"
                        checked={notifyAgentReplies}
                        disabled={!notificationsEnabled}
                        onChange={(e) => setNotifyAgentReplies(e.target.checked)}
                        className="w-4 h-4 mt-0.5 text-[var(--modal-accent)] bg-[var(--modal-bg-tertiary)] border border-[var(--modal-border-secondary)] rounded focus:ring-[var(--modal-accent)] cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
                    />
                    <span className="flex flex-col">
                        <span className={twMerge("text-[14px] text-[var(--modal-text-primary)]", !notificationsEnabled && "opacity-50")}>Notify on agent replies</span>
                        <span className={twMerge("text-[12px] text-[var(--modal-text-secondary)]", !notificationsEnabled && "opacity-50")}>
                            Play a sound and show a banner when an agent replies while you're in another app or viewing a different thread.
                        </span>
                    </span>
                </label>
            </div>

            <div>
                <label className="block text-[15px] font-semibold text-[var(--modal-text-primary)] mb-2">Snooze</label>
                <p className="text-[14px] text-[var(--modal-text-secondary)] mb-4">
                    Temporarily pause notifications without turning them off entirely.
                </p>

                <div className="flex flex-wrap gap-2 mb-3">
                    {NOTIFICATION_SNOOZE_OPTIONS.map((opt) => (
                        <button
                            key={opt.label}
                            type="button"
                            onClick={() => snoozeNotifications(opt.ms)}
                            className="px-3 py-1.5 rounded-[6px] text-[13px] font-medium border border-[var(--modal-border-secondary)] text-[var(--modal-text-primary)] bg-[var(--modal-bg-tertiary)] hover:border-[var(--modal-accent)] transition-colors cursor-pointer"
                        >
                            {opt.label}
                        </button>
                    ))}
                    <button
                        type="button"
                        onClick={() => snoozeNotifications(msUntilTomorrow8AM())}
                        className="px-3 py-1.5 rounded-[6px] text-[13px] font-medium border border-[var(--modal-border-secondary)] text-[var(--modal-text-primary)] bg-[var(--modal-bg-tertiary)] hover:border-[var(--modal-accent)] transition-colors cursor-pointer"
                    >
                        Until tomorrow
                    </button>
                    {isSnoozed && (
                        <button
                            type="button"
                            onClick={() => clearNotificationSnooze()}
                            className="px-3 py-1.5 rounded-[6px] text-[13px] font-medium border border-[var(--modal-accent)] text-[var(--modal-accent)] hover:opacity-80 transition-opacity cursor-pointer"
                        >
                            Resume notifications
                        </button>
                    )}
                </div>

                {isSnoozed && notifySnoozedUntil !== null && (
                    <p className="text-[13px] text-[var(--modal-text-secondary)]">
                        Snoozed until {new Date(notifySnoozedUntil).toLocaleString(undefined, { hour: "numeric", minute: "2-digit", month: "short", day: "numeric" })}.
                    </p>
                )}
            </div>

            <p className="text-[12px] text-[var(--modal-text-tertiary)]">
                These notification settings apply only to this device.
            </p>
        </div>
    );
}

const GENERAL_SECTIONS = [
    { id: "profile", label: "Profile", icon: <User size={18} strokeWidth={2} /> },
    { id: "appearance", label: "Appearance", icon: <Brush size={18} strokeWidth={2} /> },
    { id: "notifications", label: "Notifications", icon: <Bell size={18} strokeWidth={2} /> },
    { id: "language-region", label: "Language & region", icon: <Globe size={18} strokeWidth={2} /> },
    { id: "sleep-guard", label: "Sleep guard", icon: <BedDouble size={18} strokeWidth={2} /> },
];

const DOCS_SECTIONS = [
    { id: "workflows-guide", label: "Creating Workflows", icon: <BookOpen size={18} strokeWidth={2} /> },
    { id: "whats-new", label: "What's New", icon: <Sparkles size={18} strokeWidth={2} /> },
    { id: "about", label: "About", icon: <Info size={18} strokeWidth={2} /> },
    // { id: "how-to", label: "How to", icon: <HelpCircle size={18} strokeWidth={2} /> },
];

export function SettingsPanel({ isDocs }: { isDocs: boolean }) {
    const sections = isDocs ? DOCS_SECTIONS : GENERAL_SECTIONS;

    const [activeSectionId, setActiveSectionId] = useState(sections[0].id);

    // Reset active section when mode changes
    useEffect(() => {
        setActiveSectionId(sections[0].id);
    }, [isDocs, sections]);

    const renderContent = () => {
        if (activeSectionId === "profile") return <ProfileSettings />;
        if (activeSectionId === "appearance") return <AppearanceSettings />;
        if (activeSectionId === "notifications") return <NotificationSettings />;
        if (activeSectionId === "language-region") return <LanguageRegionSettings />;
        if (activeSectionId === "sleep-guard") return <SleepGuardSettings />;
        if (activeSectionId === "workflows-guide") return <WorkflowDocsView />;
        if (activeSectionId === "whats-new") return <WhatsNewView />;
        if (activeSectionId === "about") return <DocsView title="About" content="Manage your very own organization of agents." />;
        if (activeSectionId === "how-to") return <DocsView title="How To" content="Here you will find tutorials and guides on how to use Launchpad Studio." />;
        return null;
    };

    return (
        <div className="flex flex-col w-full h-full bg-[var(--modal-bg)] overflow-hidden">
            {/* Top Modal Header */}
            <div className="flex-shrink-0 w-full px-10 py-6 border-b border-[var(--modal-border-secondary)] flex items-center">
                <h1 className="text-[26px] font-bold text-[var(--modal-text-primary)] tracking-tight">
                    {isDocs ? "Docs" : "Preferences"}
                </h1>
            </div>

            {/* Content Area */}
            <div className="flex flex-1 min-h-0">
                {/* Left Sub-Sub-Menu */}
                <div className="w-[240px] flex-shrink-0 flex flex-col pt-6 pb-6 px-2 gap-[2px] overflow-y-auto bg-[var(--modal-bg)]">
                    {sections.map(sec => {
                        const isActive = activeSectionId === sec.id;
                        return (
                            <div
                                key={sec.id}
                                onClick={() => setActiveSectionId(sec.id)}
                                className={twMerge(
                                    "flex items-center gap-[10px] px-3 py-[6px] mx-2 rounded-[10px] cursor-pointer text-[15px] transition-colors duration-150 select-none",
                                    isActive
                                        ? "bg-[var(--modal-accent)] text-white font-medium"
                                        : "text-[var(--modal-text-primary)] hover:bg-[var(--modal-bg-hover)]"
                                )}
                            >
                                <span className={twMerge(isActive ? "text-white" : "text-[var(--modal-text-secondary)]")}>
                                    {sec.icon}
                                </span>
                                <div>{sec.label}</div>
                            </div>
                        );
                    })}
                </div>

                {/* Right Content Area */}
                <div className="flex-1 flex flex-col overflow-y-auto p-10 pt-8 w-full">
                    {renderContent()}
                </div>
            </div>
        </div>
    );
}
