/**
 * Desired hardware state (what the user picked in the UI).
 *
 * Kept separate from `telemetry` on purpose: telemetry is what the machine
 * reports, this is what the user asked for. Writes are forwarded to the
 * daemon when it's reachable and always kept locally, so the UI stays
 * consistent while the daemon-side write paths are still being ported
 * (`fan.setMode` / `fan.setCurve` are not implemented yet - see
 * docs/01-ipc-protocol.md).
 */

import { daemon, type AutoConfig, type ApplyReport, type PowerState } from "$lib/api/daemon";

const STORAGE_KEY = "omen-hub.hardware.v1";

export type PowerMode = "eco" | "balanced" | "performance" | "unlimited";
export type FanMode = "auto" | "max" | "manual";
export type GpuMode = "integrated" | "hybrid" | "discrete";
export type LightingMode = "static" | "breathing" | "wave" | "off";
export type NetworkMode = "off" | "auto" | "custom";

/** One point of the temperature -> fan speed curve. */
export type CurvePoint = { tempC: number; percent: number };

export type HardwareState = {
  powerMode: PowerMode;
  applyToOsPowerProfile: boolean;
  autoEco: boolean;
  autoPerformance: boolean;
  fanMode: FanMode;
  fanPercent: number;
  fanCurve: CurvePoint[];
  smartBoostEnabled: boolean;
  smartBoostW: number;
  maxBatteryDrain: number;
  chassisTempLimit: number;
  pl1: number;
  pl2: number;
  pl4: number;
  gpuCoreOffset: number;
  gpuMemOffset: number;
  gpuMode: GpuMode;
  lightingMode: LightingMode;
  brightness: number;
  zoneColors: string[];
  networkMode: NetworkMode;
};

/** Ranges are the ones the reference app exposes on an OMEN 16. */
export const LIMITS = {
  smartBoostW: { min: 0, max: 30 },
  maxBatteryDrain: { min: 10, max: 40 },
  chassisTempLimit: { min: 45, max: 55 },
  pl1: { min: 25, max: 77 },
  pl2: { min: 25, max: 77 },
  pl4: { min: 135, max: 168 },
  gpuCoreOffset: { min: -200, max: 300 },
  gpuMemOffset: { min: -500, max: 1500 },
};

function defaults(): HardwareState {
  return {
    powerMode: "balanced",
    applyToOsPowerProfile: true,
    autoEco: true,
    autoPerformance: true,
    fanMode: "auto",
    fanPercent: 50,
    fanCurve: [
      { tempC: 40, percent: 0 },
      { tempC: 55, percent: 25 },
      { tempC: 70, percent: 55 },
      { tempC: 80, percent: 80 },
      { tempC: 90, percent: 100 },
    ],
    smartBoostEnabled: true,
    smartBoostW: 30,
    maxBatteryDrain: 40,
    chassisTempLimit: 55,
    pl1: 77,
    pl2: 77,
    pl4: 168,
    gpuCoreOffset: 0,
    gpuMemOffset: 0,
    gpuMode: "hybrid",
    lightingMode: "static",
    brightness: 100,
    zoneColors: ["#e5178c", "#f2374b", "#ff8a00", "#7b2ff7"],
    networkMode: "off",
  };
}

class HardwareStore {
  state = $state<HardwareState>(defaults());
  /** Last write error, shown inline instead of swallowed. */
  lastError = $state<string | null>(null);
  /** What the daemon actually changed on the last power-mode write. */
  lastApply = $state<ApplyReport | null>(null);
  /** Live power state from the daemon; null while it is unreachable. */
  power = $state<PowerState | null>(null);

  load() {
    try {
      const raw = localStorage.getItem(STORAGE_KEY);
      if (raw) this.state = { ...defaults(), ...JSON.parse(raw) };
    } catch {
      /* keep defaults */
    }
  }

  set<K extends keyof HardwareState>(key: K, value: HardwareState[K]) {
    this.state = { ...this.state, [key]: value };
    this.save();
  }

  reset() {
    this.state = defaults();
    this.save();
  }

  /**
   * Reads the machine's actual power state, so the UI opens showing the
   * mode the machine is really in rather than the last one this app chose.
   */
  async syncFromDaemon() {
    try {
      const power = await daemon.powerState();
      this.power = power;
      this.state = {
        ...this.state,
        powerMode: power.mode,
        autoEco: power.auto.enabled && power.auto.ecoOnBattery,
        autoPerformance: power.auto.enabled && power.auto.performanceOnLoad,
      };
      this.lastError = null;
    } catch {
      // Daemon down: keep whatever was persisted locally. The layout
      // already tells the user the daemon is unreachable.
      this.power = null;
    }
  }

  async setPowerMode(mode: PowerMode) {
    this.set("powerMode", mode);
    // Unlimited is the only mode that hands fan control to the user; every
    // other mode is the firmware's own curve, mirroring the reference app.
    if (mode !== "unlimited" && this.state.fanMode === "manual") this.set("fanMode", "auto");
    try {
      this.lastApply = await daemon.setPowerMode(mode);
      this.lastError = null;
      // A manual change suspends the daemon's supervisor, so re-read to
      // pick up the override countdown it now reports.
      void this.syncFromDaemon();
    } catch (e) {
      this.lastError = String(e);
      this.lastApply = null;
    }
  }

  /**
   * Pushes the two home-screen auto-switch toggles to the daemon's
   * supervisor. The supervisor runs whenever either rule is on.
   */
  async setAutoSwitch(eco: boolean, performance: boolean) {
    this.set("autoEco", eco);
    this.set("autoPerformance", performance);

    const base: AutoConfig = this.power?.auto ?? {
      enabled: false,
      ecoOnBattery: true,
      performanceOnLoad: true,
      loadHigh: 0.7,
      loadLow: 0.3,
      samplesToSwitch: 3,
      intervalSecs: 10,
      manualOverrideSecs: 600,
    };

    try {
      const auto = await daemon.setAutoConfig({
        ...base,
        enabled: eco || performance,
        ecoOnBattery: eco,
        performanceOnLoad: performance,
      });
      if (this.power) this.power = { ...this.power, auto };
      this.lastError = null;
    } catch (e) {
      this.lastError = String(e);
    }
  }

  async setFanMode(mode: FanMode) {
    this.set("fanMode", mode);
    try {
      await daemon.setFanMode(mode, Math.round((this.state.fanPercent / 100) * 255));
      this.lastError = null;
    } catch (e) {
      this.lastError = String(e);
    }
  }

  private save() {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(this.state));
    } catch {
      /* storage disabled: settings just don't persist */
    }
  }
}

export const hardware = new HardwareStore();

/** Fan percentage the current curve asks for at `tempC` (linear between points). */
export function curveValueAt(curve: CurvePoint[], tempC: number): number {
  if (curve.length === 0) return 0;
  const sorted = [...curve].sort((a, b) => a.tempC - b.tempC);
  if (tempC <= sorted[0].tempC) return sorted[0].percent;
  const last = sorted[sorted.length - 1];
  if (tempC >= last.tempC) return last.percent;

  for (let i = 0; i < sorted.length - 1; i++) {
    const a = sorted[i];
    const b = sorted[i + 1];
    if (tempC >= a.tempC && tempC <= b.tempC) {
      const ratio = (tempC - a.tempC) / (b.tempC - a.tempC);
      return a.percent + ratio * (b.percent - a.percent);
    }
  }
  return last.percent;
}
