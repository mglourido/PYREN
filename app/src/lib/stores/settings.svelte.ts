/**
 * User settings, stored in `~/.config/pyren/app.json`.
 *
 * Loading is two-stage on purpose (see `DiskBacked`): a synchronous cache
 * read for the first paint, so the app doesn't render in English before
 * switching to the user's language, then the file as the authority.
 */

import { DEFAULT_LOCALE, detectLocale, i18n } from "$lib/i18n/index.svelte";
import { DiskBacked } from "./persistence";
import type { ConfigOutcome } from "$lib/api/config";

export type TempUnit = "c" | "f";

export type Settings = {
  mainLanguage: string;
  fallbackLanguage: string;
  tempUnit: TempUnit;
  pollIntervalMs: number;
  startMinimized: boolean;
  /** Closing the window puts Pyren in the tray instead of quitting it.
   *  Read by the Tauri shell straight out of this file - see `closes_to_tray`. */
  closeToTray: boolean;
  autostart: boolean;
  demoData: boolean;
  /** TODO item: the "driver missing" notice has a don't-show-again box. */
  hideDriverNotice: boolean;
  vitalsAdvancedView: boolean;
};

function defaults(): Settings {
  return {
    mainLanguage: detectLocale(),
    fallbackLanguage: DEFAULT_LOCALE,
    tempUnit: "c",
    pollIntervalMs: 2000,
    startMinimized: false,
    // Off by default: the close button quitting is what every user already
    // expects, and a tray icon nobody's desktop draws would make an app that
    // silently refuses to close.
    closeToTray: false,
    autostart: false,
    demoData: true,
    hideDriverNotice: false,
    vitalsAdvancedView: false,
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
  }

  set<K extends keyof Settings>(key: K, value: Settings[K]) {
    this.current = { ...this.current, [key]: value };
    if (key === "mainLanguage" || key === "fallbackLanguage") this.applyLocales();
    this.disk.save(this.current);
  }

  reset() {
    this.current = defaults();
    this.applyLocales();
    this.disk.save(this.current);
  }

  /** Writes immediately, e.g. before the window closes. */
  flush() {
    return this.disk.flush();
  }

  private applyLocales() {
    i18n.setLocales(this.current.mainLanguage, this.current.fallbackLanguage);
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
