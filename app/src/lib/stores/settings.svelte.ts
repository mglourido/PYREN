/**
 * User settings, stored in `~/.config/pyren/app.json`.
 *
 * Loading is two-stage on purpose (see `DiskBacked`): a synchronous cache
 * read for the first paint, so the app doesn't render in English before
 * switching to the user's language, then the file as the authority.
 */

import { DEFAULT_LOCALE, detectLocale, i18n } from "$lib/i18n/index.svelte";
import { applyTheme, DEFAULT_THEME, isThemeCode, type ThemeCode } from "$lib/styles/themes";
import { DiskBacked } from "./persistence";
import type { ConfigOutcome } from "$lib/api/config";

export type TempUnit = "c" | "f";

export type Settings = {
  mainLanguage: string;
  fallbackLanguage: string;
  /** Colour theme; see `$lib/styles/themes`. */
  theme: ThemeCode;
  tempUnit: TempUnit;
  pollIntervalMs: number;
  /** How often the process table refreshes, independent of `pollIntervalMs`
   *  - it's the slowest part of a sample (a full `/proc` walk plus an
   *  `nvidia-smi` spawn), so it is allowed to run slower on its own. */
  processPollIntervalMs: number;
  startMinimized: boolean;
  /** Closing the window puts Pyren in the tray instead of quitting it.
   *  Read by the Tauri shell straight out of this file - see `closes_to_tray`. */
  closeToTray: boolean;
  autostart: boolean;
  /** TODO item: the "driver missing" notice has a don't-show-again box. */
  hideDriverNotice: boolean;
  vitalsAdvancedView: boolean;
  /** Animate the vitals gauges toward each new reading instead of snapping. */
  gaugeAnimations: boolean;
  /** In Performance control, list each fan's own RPM under the headline
   *  figure (the speed sent to the controller), labelled by cooler. */
  perFanRpm: boolean;
  /** Show the fan-control modes as a second row in the pyren-osd widget.
   *  Read straight from this file by the widget, like `mainLanguage`. */
  widgetFanModes: boolean;
  /** Show the power-mode row in the widget. On by default; only turned off
   *  while `widgetFanModes` is on, so the widget is never empty. */
  widgetPowerModes: boolean;
  /** Checks GitHub for a new release on startup, at most once every 6h.
   *  Off leaves updates to the manual button in Help. */
  autoCheckUpdates: boolean;
  /** Once a version has been pushed as a notification, don't push it again
   *  on the next auto-check that finds the same tag still latest. */
  notifyUpdateOnce: boolean;
  /** Unix ms of the last update check, manual or automatic - shared by
   *  both so a manual check also postpones the next automatic one. */
  lastUpdateCheckAt: number | null;
  /** The last version an update notification was raised for, so
   *  `notifyUpdateOnce` has something to compare against. */
  lastNotifiedUpdateVersion: string | null;
};

function defaults(): Settings {
  return {
    mainLanguage: detectLocale(),
    fallbackLanguage: DEFAULT_LOCALE,
    theme: DEFAULT_THEME,
    tempUnit: "c",
    pollIntervalMs: 2000,
    // Slower than `pollIntervalMs`: the process list is the heaviest part
    // of a sample (a full /proc sweep plus an `nvidia-smi` spawn on the
    // daemon side), so it refreshes on its own longer-spaced timer instead
    // of paying that cost on every CPU/RAM/GPU tick.
    processPollIntervalMs: 8000,
    startMinimized: false,
    // Off by default: the close button quitting is what every user already
    // expects, and a tray icon nobody's desktop draws would make an app that
    // silently refuses to close.
    closeToTray: false,
    autostart: false,
    hideDriverNotice: false,
    vitalsAdvancedView: false,
    gaugeAnimations: true,
    // On by default: it is a read-only detail, costs nothing when the
    // machine has a single fan, and is what people come to this page for.
    perFanRpm: true,
    // Off by default: the widget's job is the power key, and the fan row
    // is an extra someone opts into.
    widgetFanModes: false,
    widgetPowerModes: true,
    // On by default: most people want to know a new release exists without
    // having to remember to press the button in Help.
    autoCheckUpdates: true,
    notifyUpdateOnce: true,
    lastUpdateCheckAt: null,
    lastNotifiedUpdateVersion: null,
  };
}

class SettingsStore {
  current = $state<Settings>(defaults());
  loaded = $state(false);
  /** Where the settings file lives, and how the last read of it went.
   *  Surfaced in Settings so a reset-to-defaults is never a mystery. */
  outcome = $state<ConfigOutcome | null>(null);
  /** Absolute path of the settings file, once a load has reported it. */
  configPath = $state<string | null>(null);

  private disk = new DiskBacked<Settings>("app", defaults);

  /** Synchronous, for the first render. Safe to call more than once. */
  loadCache() {
    if (this.loaded) return;
    this.current = this.disk.readCache();
    this.applyLocales();
    this.applyTheme();
    this.loaded = true;
  }

  /** Reads the file and takes it as authoritative. */
  async hydrate() {
    this.loadCache();
    const { values, outcome, path } = await this.disk.hydrate();
    this.current = values;
    this.outcome = outcome;
    this.configPath = path;
    this.applyLocales();
    this.applyTheme();
  }

  set<K extends keyof Settings>(key: K, value: Settings[K]) {
    this.current = { ...this.current, [key]: value };
    if (key === "mainLanguage" || key === "fallbackLanguage") this.applyLocales();
    if (key === "theme") this.applyTheme();
    this.disk.save(this.current);
  }

  reset() {
    this.current = defaults();
    this.applyLocales();
    this.applyTheme();
    this.disk.save(this.current);
  }

  /** Writes immediately, e.g. before the window closes. */
  flush() {
    return this.disk.flush();
  }

  private applyLocales() {
    i18n.setLocales(this.current.mainLanguage, this.current.fallbackLanguage);
  }

  private applyTheme() {
    applyTheme(isThemeCode(this.current.theme) ? this.current.theme : DEFAULT_THEME);
  }
}

export const settings = new SettingsStore();

/** Formats a Celsius reading according to the user's unit preference. */
export function formatTemp(celsius: number | null | undefined, withUnit = true): string {
  if (celsius === null || celsius === undefined || Number.isNaN(celsius)) return "--";
  const value =
    settings.current.tempUnit === "f" ? Math.round(celsius * 1.8 + 32) : Math.round(celsius);
  return withUnit ? `${value}°${settings.current.tempUnit === "f" ? "F" : "C"}` : String(value);
}
