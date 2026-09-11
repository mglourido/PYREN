/**
 * Colour themes.
 *
 * `theme.css` holds the *structure* of the token system (radii, fonts,
 * sizes) and a full set of dark values on `:root`, so the very first frame
 * is already styled before any script runs. This file is the source of
 * truth for the *colours*: one palette per theme, applied as inline custom
 * properties on `<html>` by `applyTheme`, overriding the CSS defaults.
 *
 * Adding a theme is "add an entry here" - it shows up in the switcher and
 * in Settings on its own (see `THEME_CODES`).
 */

export type ThemeCode =
  | "dark"
  | "light"
  | "dracula"
  | "tokyo-night"
  | "zero-two"
  | "doki"
  | "cobalt2"
  | "ayu";

export const DEFAULT_THEME: ThemeCode = "dark";

/** Every custom property a theme is allowed to set. Anything not listed
 *  here is the same in every theme and stays in `theme.css`. */
type ThemeTokens = {
  "--omen-black": string;
  "--bg-window": string;
  "--bg-chrome": string;
  "--bg-panel": string;
  "--bg-card": string;
  "--bg-card-hover": string;
  "--bg-inset": string;
  "--line": string;
  "--line-soft": string;
  "--border": string;
  "--text": string;
  "--text-dim": string;
  "--text-mute": string;
  "--text-disabled": string;
  "--accent-1": string;
  "--accent-2": string;
  "--accent-3": string;
  "--ok": string;
  "--warn": string;
  "--danger": string;
  "--info": string;
  /** The "on / ticked" affordance (filled Toggle rail, native checkboxes). */
  "--check": string;
  "--shadow": string;
  "--scroll-thumb": string;
  "--scroll-thumb-hover": string;
  /** Inert grey rail of a slider / toggle, and the knob that rides it. */
  "--track": string;
  "--knob": string;
  /** "Inverted" selection chip (Segmented's active option): a surface the
   *  opposite lightness of the theme, with matching text. */
  "--invert-bg": string;
  "--invert-text": string;
};

export type Theme = {
  code: ThemeCode;
  /** Drives the UA `color-scheme` so native widgets (scrollbars, form
   *  controls, the caret) match. */
  scheme: "dark" | "light";
  tokens: ThemeTokens;
};

/** Dark: the original OMEN-reference palette. Kept in step with the
 *  fallback values in `theme.css`. */
const dark: Theme = {
  code: "dark",
  scheme: "dark",
  tokens: {
    "--omen-black": "#000000",
    "--bg-window": "#0b0b0d",
    "--bg-chrome": "#131316",
    "--bg-panel": "#1b1b1e",
    "--bg-card": "#232326",
    "--bg-card-hover": "#2b2b2f",
    "--bg-inset": "#101012",
    "--line": "#303035",
    "--line-soft": "#232327",
    "--border": "#303035",
    "--text": "#ffffff",
    "--text-dim": "#b6b6bd",
    "--text-mute": "#7c7c86",
    "--text-disabled": "#5a5a62",
    "--accent-1": "#e5178c",
    "--accent-2": "#f2374b",
    "--accent-3": "#ff8a00",
    "--ok": "#21e065",
    "--warn": "#ffb020",
    "--danger": "#ff4747",
    "--info": "#2f8fff",
    "--check": "#2f8fff",
    "--shadow": "0 8px 24px rgba(0, 0, 0, 0.55)",
    "--scroll-thumb": "#3a3a41",
    "--scroll-thumb-hover": "#4d4d56",
    "--track": "#4a4a52",
    "--knob": "#ffffff",
    "--invert-bg": "#f2f2f4",
    "--invert-text": "#17171a",
  },
};

/** Light: near-white and grey as the base, the same way dark builds on
 *  near-black greys. The OMEN magenta -> orange accent carries over, a
 *  shade deeper so it holds contrast on white. */
const light: Theme = {
  code: "light",
  scheme: "light",
  tokens: {
    "--omen-black": "#e6e6e9",
    "--bg-window": "#f3f3f5",
    "--bg-chrome": "#ffffff",
    "--bg-panel": "#ffffff",
    "--bg-card": "#f6f6f8",
    "--bg-card-hover": "#ececef",
    "--bg-inset": "#ececed",
    "--line": "#d5d5da",
    "--line-soft": "#e5e5e9",
    "--border": "#d5d5da",
    "--text": "#1a1a1f",
    "--text-dim": "#4a4a54",
    "--text-mute": "#6e6e78",
    "--text-disabled": "#a6a6b0",
    "--accent-1": "#c9137f",
    "--accent-2": "#d92b3e",
    "--accent-3": "#e07600",
    "--ok": "#12a150",
    "--warn": "#b26a00",
    "--danger": "#d32f2f",
    "--info": "#1667d6",
    "--check": "#1667d6",
    "--shadow": "0 8px 24px rgba(0, 0, 0, 0.14)",
    "--scroll-thumb": "#c6c6ce",
    "--scroll-thumb-hover": "#a9a9b3",
    "--track": "#cfcfd6",
    "--knob": "#ffffff",
    "--invert-bg": "#1f1f24",
    "--invert-text": "#f4f4f6",
  },
};

/** Dracula: the well-known dark-and-pink palette
 *  (https://draculatheme.com). Same near-black-grey build as `dark`, with
 *  Dracula's own surfaces, and its pink -> purple -> orange in place of the
 *  OMEN magenta -> orange accent. */
const dracula: Theme = {
  code: "dracula",
  scheme: "dark",
  tokens: {
    "--omen-black": "#191a21",
    "--bg-window": "#1e1f29",
    "--bg-chrome": "#21222c",
    "--bg-panel": "#282a36",
    "--bg-card": "#2d2f3d",
    "--bg-card-hover": "#343746",
    "--bg-inset": "#191a21",
    "--line": "#44475a",
    "--line-soft": "#343746",
    "--border": "#44475a",
    "--text": "#f8f8f2",
    "--text-dim": "#bcc0d8",
    "--text-mute": "#6272a4",
    "--text-disabled": "#4d5779",
    "--accent-1": "#ff79c6",
    "--accent-2": "#bd93f9",
    "--accent-3": "#ffb86c",
    "--ok": "#50fa7b",
    "--warn": "#f1fa8c",
    "--danger": "#ff5555",
    "--info": "#8be9fd",
    // Dracula leans on its green for anything switched on; the cyan stays
    // reserved for genuine "info".
    "--check": "#50fa7b",
    "--shadow": "0 8px 24px rgba(0, 0, 0, 0.55)",
    "--scroll-thumb": "#44475a",
    "--scroll-thumb-hover": "#565a72",
    "--track": "#44475a",
    "--knob": "#f8f8f2",
    "--invert-bg": "#f8f8f2",
    "--invert-text": "#282a36",
  },
};

/** Tokyo Night: the well-known deep-blue palette
 *  (https://github.com/enkia/tokyo-night-vscode-theme). Same near-black-grey
 *  build as `dark`, with Tokyo Night's own navy surfaces and its
 *  blue -> purple -> cyan in place of the OMEN magenta -> orange accent. */
const tokyoNight: Theme = {
  code: "tokyo-night",
  scheme: "dark",
  tokens: {
    "--omen-black": "#13141c",
    "--bg-window": "#1a1b26",
    "--bg-chrome": "#1f2335",
    "--bg-panel": "#24283b",
    "--bg-card": "#292e42",
    "--bg-card-hover": "#333a56",
    "--bg-inset": "#16161e",
    "--line": "#3b4261",
    "--line-soft": "#292e42",
    "--border": "#3b4261",
    "--text": "#c0caf5",
    "--text-dim": "#a9b1d6",
    "--text-mute": "#565f89",
    "--text-disabled": "#414868",
    "--accent-1": "#7aa2f7",
    "--accent-2": "#bb9af7",
    "--accent-3": "#7dcfff",
    "--ok": "#9ece6a",
    "--warn": "#e0af68",
    "--danger": "#f7768e",
    "--info": "#7dcfff",
    "--check": "#9ece6a",
    "--shadow": "0 8px 24px rgba(0, 0, 0, 0.55)",
    "--scroll-thumb": "#3b4261",
    "--scroll-thumb-hover": "#414868",
    "--track": "#3b4261",
    "--knob": "#c0caf5",
    "--invert-bg": "#c0caf5",
    "--invert-text": "#1a1b26",
  },
};

/** Zero Two: the "Zero Two - Rose" character theme from the Doki Theme
 *  collection (https://github.com/doki-theme, Darling in the Franxx group) -
 *  a deep crimson base with her signature rose/orange accents in place of
 *  the OMEN magenta -> orange gradient. */
const zeroTwo: Theme = {
  code: "zero-two",
  scheme: "dark",
  tokens: {
    "--omen-black": "#200a0a",
    "--bg-window": "#2a0e0e",
    "--bg-chrome": "#300d0e",
    "--bg-panel": "#330f10",
    "--bg-card": "#3c1616",
    "--bg-card-hover": "#481818",
    "--bg-inset": "#310f0f",
    "--line": "#481818",
    "--line-soft": "#3c1515",
    "--border": "#481818",
    "--text": "#efefef",
    "--text-dim": "#cbb9b0",
    "--text-mute": "#a87975",
    "--text-disabled": "#6f4f4a",
    "--accent-1": "#e356a7",
    "--accent-2": "#af2636",
    "--accent-3": "#ffb86c",
    "--ok": "#39e6a6",
    "--warn": "#e5c374",
    "--danger": "#ec081e",
    "--info": "#34a7d1",
    // Zero Two's palette leans on mint for keywords/"go"; kept distinct
    // from --info the same way Dracula and Tokyo Night split green/cyan.
    "--check": "#39e6a6",
    "--shadow": "0 8px 24px rgba(0, 0, 0, 0.55)",
    "--scroll-thumb": "#481818",
    "--scroll-thumb-hover": "#5a1f1f",
    "--track": "#481818",
    "--knob": "#efefef",
    "--invert-bg": "#efefef",
    "--invert-text": "#330f10",
  },
};

/** Doki Theme: AzurLane: Essex - a naval navy-blue base with the ship
 *  girl's red accent and cyan/lavender syntax colours, in place of the OMEN
 *  magenta -> orange gradient. Sourced from the official Doki Theme
 *  collection (https://github.com/doki-theme/doki-master-theme,
 *  definitions/azurLane/essex). */
const doki: Theme = {
  code: "doki",
  scheme: "dark",
  tokens: {
    "--omen-black": "#001527",
    "--bg-window": "#001d39",
    "--bg-chrome": "#00203e",
    "--bg-panel": "#002446",
    "--bg-card": "#002f5d",
    "--bg-card-hover": "#163756",
    "--bg-inset": "#00203e",
    "--line": "#002c55",
    "--line-soft": "#00274e",
    "--border": "#002c55",
    "--text": "#d7d7d7",
    "--text-dim": "#d0d0d0",
    "--text-mute": "#6387af",
    "--text-disabled": "#5b646f",
    "--accent-1": "#d53232",
    "--accent-2": "#2d96ea",
    "--accent-3": "#f3b085",
    // Essex's palette has no dedicated green; this teal keeps "success"
    // legible against the navy without clashing with the cyan --info.
    "--ok": "#4cd9a0",
    "--warn": "#eec45e",
    "--danger": "#e51515",
    "--info": "#78dbef",
    "--check": "#4cd9a0",
    "--shadow": "0 8px 24px rgba(0, 0, 0, 0.55)",
    "--scroll-thumb": "#002c55",
    "--scroll-thumb-hover": "#163756",
    "--track": "#002c55",
    "--knob": "#d7d7d7",
    "--invert-bg": "#d7d7d7",
    "--invert-text": "#002446",
  },
};

/** Cobalt2: Wes Bos's well-known blue-and-yellow theme
 *  (https://github.com/wesbos/cobalt2-vscode). Deep blue surfaces with its
 *  signature yellow highlight and blue -> cyan accent, in place of the OMEN
 *  magenta -> orange gradient. */
const cobalt2: Theme = {
  code: "cobalt2",
  scheme: "dark",
  tokens: {
    "--omen-black": "#0d1f2c",
    "--bg-window": "#193549",
    "--bg-chrome": "#122738",
    "--bg-panel": "#122738",
    "--bg-card": "#1f4662",
    "--bg-card-hover": "#234e6d",
    "--bg-inset": "#101f2b",
    "--line": "#0d3a58",
    "--line-soft": "#15232d",
    "--border": "#0d3a58",
    "--text": "#ffffff",
    "--text-dim": "#d6dee6",
    "--text-mute": "#aaaaaa",
    "--text-disabled": "#6b7c8c",
    "--accent-1": "#0088ff",
    "--accent-2": "#00ffff",
    "--accent-3": "#ffc600",
    "--ok": "#3ad900",
    "--warn": "#ff9d00",
    "--danger": "#ff5630",
    "--info": "#0088ff",
    "--check": "#3ad900",
    "--shadow": "0 8px 24px rgba(0, 0, 0, 0.55)",
    "--scroll-thumb": "#0d3a58",
    "--scroll-thumb-hover": "#1f4662",
    "--track": "#0d3a58",
    "--knob": "#ffffff",
    "--invert-bg": "#ffffff",
    "--invert-text": "#193549",
  },
};

/** Ayu Dark: the well-known dark theme with a strong orange accent
 *  (https://github.com/ayu-theme/vscode-ayu). Near-black slate surfaces with
 *  its signature orange -> amber -> gold in place of the OMEN magenta ->
 *  orange gradient. */
const ayu: Theme = {
  code: "ayu",
  scheme: "dark",
  tokens: {
    "--omen-black": "#0a0d13",
    "--bg-window": "#0d1017",
    "--bg-chrome": "#0d1017",
    "--bg-panel": "#10141c",
    "--bg-card": "#141821",
    "--bg-card-hover": "#1f2430",
    "--bg-inset": "#0a0d13",
    "--line": "#1b1f29",
    "--line-soft": "#141821",
    "--border": "#1b1f29",
    "--text": "#bfbdb6",
    "--text-dim": "#b3b1ab",
    "--text-mute": "#5a6378",
    "--text-disabled": "#3d4552",
    "--accent-1": "#ff8f40",
    "--accent-2": "#ffb454",
    "--accent-3": "#e6b450",
    "--ok": "#aad94c",
    "--warn": "#ffb454",
    "--danger": "#d95757",
    "--info": "#39bae6",
    "--check": "#aad94c",
    "--shadow": "0 8px 24px rgba(0, 0, 0, 0.55)",
    "--scroll-thumb": "#1b1f29",
    "--scroll-thumb-hover": "#475266",
    "--track": "#1b1f29",
    "--knob": "#bfbdb6",
    "--invert-bg": "#bfbdb6",
    "--invert-text": "#0d1017",
  },
};

export const THEMES: Record<ThemeCode, Theme> = {
  dark,
  light,
  dracula,
  "tokyo-night": tokyoNight,
  "zero-two": zeroTwo,
  doki,
  cobalt2,
  ayu,
};

/** Order shown in the switcher. */
export const THEME_CODES: ThemeCode[] = [
  "dark",
  "light",
  "dracula",
  "tokyo-night",
  "zero-two",
  "doki",
  "cobalt2",
  "ayu",
];

export function isThemeCode(value: unknown): value is ThemeCode {
  return (
    value === "dark" ||
    value === "light" ||
    value === "dracula" ||
    value === "tokyo-night" ||
    value === "zero-two" ||
    value === "doki" ||
    value === "cobalt2" ||
    value === "ayu"
  );
}

/** Writes the palette onto `<html>` as inline custom properties. Cheap
 *  enough to call on every change; the browser only repaints what moved. */
export function applyTheme(code: ThemeCode): void {
  const theme = THEMES[code] ?? THEMES[DEFAULT_THEME];
  const root = document.documentElement;
  for (const [name, value] of Object.entries(theme.tokens)) {
    root.style.setProperty(name, value);
  }
  root.style.colorScheme = theme.scheme;
  root.dataset.theme = theme.code;
}
