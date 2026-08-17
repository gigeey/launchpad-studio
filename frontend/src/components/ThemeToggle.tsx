import { Sun, Moon, Monitor } from "lucide-react";
import { useUserPreferencesStore, type ThemePreference } from "../stores/userPreferencesStore";

const nextTheme: Record<ThemePreference, ThemePreference> = {
  system: "light",
  light: "dark",
  dark: "system",
};

const themeIcon: Record<ThemePreference, typeof Sun> = {
  system: Monitor,
  light: Sun,
  dark: Moon,
};

const themeLabel: Record<ThemePreference, string> = {
  system: "System theme",
  light: "Light theme",
  dark: "Dark theme",
};

export function ThemeToggle() {
  const theme = useUserPreferencesStore((s) => s.theme);
  const setTheme = useUserPreferencesStore((s) => s.setTheme);
  const Icon = themeIcon[theme];

  return (
    <button
      type="button"
      title={themeLabel[theme]}
      className="w-[28px] h-[28px] flex items-center justify-center rounded-[6px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] active:opacity-80 transition-colors duration-150 cursor-pointer"
      onClick={() => setTheme(nextTheme[theme])}
    >
      <Icon size={16} />
    </button>
  );
}
