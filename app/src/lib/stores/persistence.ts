/**
 * Disk persistence for the UI's own settings.
 *
 * Disk is the source of truth, but reading it is asynchronous, and the very
 * first paint needs a language and a theme *now* - hydrating a moment later
 * would flash English before switching to the user's language. So each
 * store also mirrors its values into `localStorage` and reads that
 * synchronously for the first render, then reconciles with the file.
 *
 * localStorage is a cache here, never the record: it is written only
 * alongside a disk write, and the file wins on any disagreement.
 */

import { appConfig, type ConfigNamespace, type ConfigOutcome } from "$lib/api/config";

/** How long to coalesce rapid changes (dragging a slider) into one write. */
const SAVE_DEBOUNCE_MS = 300;

export type HydrateResult<T> = {
  values: T;
  outcome: ConfigOutcome | null;
  /** Absolute path of the file, once known. */
  path: string | null;
};

export class DiskBacked<T extends object> {
  private saveTimer: ReturnType<typeof setTimeout> | null = null;
  private pending: T | null = null;
  /** Set once the real values are known; guards against writing defaults
   *  over a file that simply hadn't been read yet. */
  private ready = false;

  constructor(
    private readonly namespace: ConfigNamespace,
    private readonly defaults: () => T,
  ) {}

  private get cacheKey(): string {
    return `omen-hub.${this.namespace}.cache`;
  }

  /**
   * Values for the first paint, from the local cache. Synchronous and
   * always succeeds - worst case it returns defaults.
   */
  readCache(): T {
    try {
      const raw = localStorage.getItem(this.cacheKey);
      // Merge over defaults so a cache written by an older build, missing
      // keys added since, still loads instead of blanking the app.
      if (raw) return { ...this.defaults(), ...JSON.parse(raw) };
    } catch {
      /* storage unavailable or corrupt: defaults are fine */
    }
    return this.defaults();
  }

  /**
   * Authoritative values from disk. Falls back to the cache when the file
   * has no data yet, or when running outside Tauri (`vite dev`), where
   * there is no filesystem to read.
   */
  async hydrate(): Promise<HydrateResult<T>> {
    if (!appConfig.available()) {
      this.ready = true;
      return { values: this.readCache(), outcome: null, path: null };
    }

    try {
      const loaded = await appConfig.load(this.namespace);
      this.ready = true;
      // "missing" is a first run, and "recovered"/"tooNew" mean the file
      // holds nothing usable - in all three cases the cache is the better
      // guess than bare defaults, and the caller is told which it was.
      const values =
        loaded.outcome.status === "loaded"
          ? ({ ...this.defaults(), ...loaded.values } as T)
          : this.readCache();
      return { values, outcome: loaded.outcome, path: loaded.path };
    } catch {
      this.ready = true;
      return { values: this.readCache(), outcome: null, path: null };
    }
  }

  /** Queues a write. Rapid successive calls collapse into one. */
  save(values: T) {
    this.writeCache(values);
    if (!this.ready) return;

    this.pending = values;
    if (this.saveTimer !== null) clearTimeout(this.saveTimer);
    this.saveTimer = setTimeout(() => void this.flush(), SAVE_DEBOUNCE_MS);
  }

  /** Writes any queued values immediately. */
  async flush(): Promise<void> {
    if (this.saveTimer !== null) {
      clearTimeout(this.saveTimer);
      this.saveTimer = null;
    }
    const values = this.pending;
    this.pending = null;
    if (!values || !appConfig.available()) return;

    try {
      await appConfig.save(this.namespace, values as Record<string, unknown>);
    } catch (e) {
      // Reported rather than thrown: the setting has been applied in the
      // running app, it just won't survive a restart.
      console.error(`omen-hub: could not save ${this.namespace} settings`, e);
    }
  }

  private writeCache(values: T) {
    try {
      localStorage.setItem(this.cacheKey, JSON.stringify(values));
    } catch {
      /* private mode / storage disabled: the disk write still happens */
    }
  }
}
