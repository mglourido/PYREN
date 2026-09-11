/**
 * The lighting page's saved configurations, in their own file
 * (`~/.config/pyren/lighting.json`) rather than folded into `app.json` -
 * they are the user's own presets, not app chrome, and keeping them apart
 * means a reset of one never touches the other.
 */

import { DiskBacked } from "./persistence";
import type { RgbEffect } from "$lib/api/daemon";

export type LightingPreset = {
  id: string;
  name: string;
  mode: "static" | "zones" | "effect";
  zones: string[];
  brightness: number;
  effect: RgbEffect | null;
  fps: number | null;
};

type LightingPresetsFile = {
  presets: LightingPreset[];
};

function defaults(): LightingPresetsFile {
  return { presets: [] };
}

class LightingPresetsStore {
  current = $state<LightingPresetsFile>(defaults());
  loaded = $state(false);

  private disk = new DiskBacked<LightingPresetsFile>("lighting", defaults);

  /** Synchronous, for the first render. Safe to call more than once. */
  loadCache() {
    if (this.loaded) return;
    this.current = this.disk.readCache();
    this.loaded = true;
  }

  /** Reads the file and takes it as authoritative. */
  async hydrate() {
    this.loadCache();
    const { values } = await this.disk.hydrate();
    this.current = values;
  }

  set(presets: LightingPreset[]) {
    this.current = { presets };
    this.disk.save(this.current);
  }

  /** Writes immediately, e.g. before the window closes. */
  flush() {
    return this.disk.flush();
  }
}

export const lightingPresets = new LightingPresetsStore();
