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

export type ThemeCode = "dark" | "light" | "dracula";

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

export const THEMES: Record<ThemeCode, Theme> = { dark, light, dracula };

/** Order shown in the switcher. */
export const THEME_CODES: ThemeCode[] = ["dark", "light", "dracula"];

export function isThemeCode(value: unknown): value is ThemeCode {
  return value === "dark" || value === "light" || value === "dracula";
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
