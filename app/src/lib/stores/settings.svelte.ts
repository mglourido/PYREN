/**
 * User settings.
 *
 * Persisted in localStorage for now: it survives restarts, needs no
 * privileges, and keeps the frontend independent of the daemon. Moving
 * these to `~/.config/omen-hub/app.json` through a Tauri command later is
 * a change to `load`/`save` only - nothing else reads storage directly.
 */

import { DEFAULT_LOCALE, detectLocale, i18n } from "$lib/i18n/index.svelte";

const STORAGE_KEY = "omen-hub.settings.v1";

export type TempUnit = "c" | "f";

export type Settings = {
  mainLanguage: string;
  fallbackLanguage: string;
  tempUnit: TempUnit;
  pollIntervalMs: number;
  startMinimized: boolean;
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
    autostart: false,
    demoData: true,
    hideDriverNotice: false,
    vitalsAdvancedView: false,
  };
}

class SettingsStore {
  current = $state<Settings>(defaults());
  loaded = $state(false);

  load() {
    if (this.loaded) return;
    try {
      const raw = localStorage.getItem(STORAGE_KEY);
      // Merge over defaults so a settings file written by an older version
      // (missing keys added since) still loads instead of blanking the app.
      if (raw) this.current = { ...defaults(), ...JSON.parse(raw) };
    } catch {
      /* corrupt or unavailable storage: keep defaults */
    }
    this.loaded = true;
    i18n.setLocales(this.current.mainLanguage, this.current.fallbackLanguage);
  }

  set<K extends keyof Settings>(key: K, value: Settings[K]) {
    this.current = { ...this.current, [key]: value };
    if (key === "mainLanguage" || key === "fallbackLanguage") {
      i18n.setLocales(this.current.mainLanguage, this.current.fallbackLanguage);
    }
    this.save();
  }

  reset() {
    this.current = defaults();
    i18n.setLocales(this.current.mainLanguage, this.current.fallbackLanguage);
    this.save();
  }

  private save() {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(this.current));
    } catch {
      /* private mode / storage disabled: settings just don't persist */
    }
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
