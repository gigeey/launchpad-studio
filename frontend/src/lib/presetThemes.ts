// Preset chrome themes. Each is a curated 10-color palette that flows through the
// same runtime derivation engine as the user's "Custom" theme (see customTheme.ts) —
// so presets need NO hardcoded App.css color block; their CSS variables are computed
// and applied inline on <html> by useThemeSync (App.tsx). Palette role order matches
// the custom-theme 10-role contract (see CustomThemeRoles/colorsToRoles in
// customTheme.ts): Column BG, Menu BG Hover, Active Item BG, Active Item Text,
// Hover Item BG, Text Color, Active Presence, Mention Badge, Top Nav BG, Top Nav Text.

export interface PresetTheme {
    id: string;
    displayName: string;
    palette: string[]; // exactly 10 hex colors, in the 10-role order above
}

export const PRESET_THEMES: readonly PresetTheme[] = [
    { id: "grape-soda", displayName: "Grape Soda", palette: ["#4A3A55","#3A2A45","#8A7A95","#FFFFFF","#2A1A35","#FFFFFF","#22CC88","#CC2288","#3A2A45","#FFFFFF"] },
    { id: "zen-garden", displayName: "Zen Garden", palette: ["#333333","#1E2320","#709080","#FFFFFF","#1E2320","#FFFFFF","#F0DFAF","#CC9393","#1E2320","#FFFFFF"] },
    { id: "licorice", displayName: "Licorice", palette: ["#222222","#111111","#111111","#66AADD","#555555","#EEEEEE","#66DD66","#DD666D","#111111","#EEEEEE"] },
    { id: "firewatch", displayName: "Firewatch", palette: ["#292D36","#000000","#DA5647","#FFFFFF","#333644","#FFFFFF","#57AFBD","#DA5647","#000000","#FFFFFF"] },
    { id: "red-velvet", displayName: "Red Velvet", palette: ["#282828","#551127","#79092B","#F5F5F5","#111111","#CCCCCC","#F04B6E","#B40F42","#551127","#CCCCCC"] },
    { id: "matcha-latte", displayName: "Matcha Latte", palette: ["#353535","#2F2F2F","#8FA876","#FFFFFF","#8FA876","#FFFFFF","#818181","#8FA876","#2F2F2F","#FFFFFF"] },
    { id: "blue-raspberry", displayName: "Blue Raspberry", palette: ["#3F51B5","#303F9F","#303F9F","#FFFFFF","#303F9F","#FFFFFF","#B2FF59","#9FA8DA","#303F9F","#FFFFFF"] },
    { id: "coral-reef", displayName: "Coral Reef", palette: ["#27364E","#1A2535","#F55D54","#F2F2F4","#1A2535","#E6E6E9","#F6756D","#F55D54","#1A2535","#E6E6E9"] },
    { id: "nebula", displayName: "Nebula", palette: ["#352E59","#2D274F","#0076BF","#FFFFFF","#2D274F","#FFFFFF","#94E864","#78AF8F","#2D274F","#FFFFFF"] },
    { id: "harbor", displayName: "Harbor", palette: ["#324050","#283542","#4B9AD9","#FFFFFF","#283542","#CFD8E5","#3BB594","#EB4D5C","#283542","#CFD8E5"] },
    { id: "amethyst", displayName: "Amethyst", palette: ["#373352","#1A123B","#A347B7","#FFFFFF","#1A123B","#D6E0FF","#D15FEA","#A347B7","#1A123B","#D6E0FF"] },
    { id: "blue-steel", displayName: "Blue Steel", palette: ["#2E4F7E","#2C3849","#203251","#FFFFFF","#456494","#FFFFFF","#A6C056","#E9AE4C","#2C3849","#FFFFFF"] },
    { id: "dune", displayName: "Dune", palette: ["#363C74","#000000","#E8D3A2","#363C74","#000000","#FFFFFF","#E8D3A2","#B7A57A","#000000","#FFFFFF"] },
    { id: "newsroom", displayName: "Newsroom", palette: ["#23282D","#191E23","#0073AA","#FFFFFF","#111111","#EEEEEE","#46B450","#D54E21","#191E23","#EEEEEE"] },
    { id: "honey-mustard", displayName: "Honey Mustard", palette: ["#4D5250","#444A47","#D39B46","#FFFFFF","#434745","#FFFFFF","#99D04A","#DB6668","#444A47","#FFFFFF"] },
    { id: "default", displayName: "Default", palette: ["#F2F2F4","#E6E6E9","#1FBAD6","#FFFFFF","#C0C0C8","#151525","#1FBAD6","#4CC8DE","#E6E6E9","#151525"] },
];

export const PRESET_THEME_MAP: ReadonlyMap<string, PresetTheme> = new Map(
    PRESET_THEMES.map((t) => [t.id, t]),
);

export const PRESET_THEME_IDS: ReadonlySet<string> = new Set(PRESET_THEMES.map((t) => t.id));
