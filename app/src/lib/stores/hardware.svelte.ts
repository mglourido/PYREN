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

import {
  daemon,
  errorText,
  onDaemonEvent,
  type ApplyReport,
  type AutoConfig,
  type FanStatus,
  type PowerConfigReply,
  type PowerState,
} from "$lib/api/daemon";
import { tm } from "$lib/i18n/index.svelte";
import { DiskBacked } from "./persistence";

/** Coalesces a slider drag or a dragged curve point into one daemon call. */
const FAN_PUSH_DEBOUNCE_MS = 200;

/**
 * The 0-255 the driver takes. Never 0 for a positive percentage: `pwm1 = 0`
 * is the driver's "automatic" sentinel, not "off" - the daemon clamps this
 * too (`MIN_COMMANDED_PWM`), and the two must agree.
 */
function percentToPwm(percent: number): number {
  return Math.max(1, Math.min(255, Math.round((percent / 100) * 255)));
}

export type PowerMode = "eco" | "balanced" | "performance" | "unlimited";
export type FanMode = "auto" | "max" | "manual" | "curve";
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
  /**
   * `Date.now()` at the last successful power-state read. The daemon's
   * `autoOverrideSecondsLeft` is a snapshot, not a ticking value - the UI
   * ages it against this so the countdown counts down instead of sitting
   * frozen at ~599 until the next mode change re-reads it.
   */
  powerReadAt = $state(0);
  /** Live fan state from the daemon; null while it is unreachable. */
  fan = $state<FanStatus | null>(null);

  private disk = new DiskBacked<HardwareState>("ui", defaults);
  private fanPushTimer: ReturnType<typeof setTimeout> | null = null;
  private powerTuningTimer: ReturnType<typeof setTimeout> | null = null;

  /** Synchronous, for the first render. */
  loadCache() {
    this.state = this.disk.readCache();
  }

  /** Reads `~/.config/pyren/ui.json` and takes it as authoritative. */
  async hydrate() {
    const { values } = await this.disk.hydrate();
    this.state = values;
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
      this.powerReadAt = Date.now();
      this.state = {
        ...this.state,
        powerMode: power.mode,
        applyToOsPowerProfile: power.applyToOsProfile,
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

  /**
   * Follows the machine when something else moves it, and returns the
   * function that stops following.
   *
   * The mode is not this app's to know: the performance key cycles it, the
   * on-screen display sets it, `pyren-ctl` sets it, and the daemon's
   * supervisor switches it on battery. Before this, the page went on
   * showing whichever mode it had last read - it was not wrong when it was
   * drawn, it simply had no way to hear that the machine had left.
   *
   * The re-read happens even for a change this app made itself. It costs
   * one local socket call, and the alternative - guessing which
   * announcements are our own - would be wrong the first time two windows
   * were open.
   */
  watchDaemon(): () => void {
    return onDaemonEvent((event) => {
      if (event.topic === "power.mode") void this.syncFromDaemon();
    });
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
      this.lastError = errorText(e);
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
      batteryLowPercent: 25,
      samplesToSwitch: 3,
      intervalSecs: 10,
      manualOverrideSecs: 600,
    };

    try {
      const reply = await daemon.setAutoConfig({
        ...base,
        enabled: eco || performance,
        ecoOnBattery: eco,
        performanceOnLoad: performance,
      });
      this.applyConfigReply(reply);
    } catch (e) {
      this.lastError = errorText(e);
    }
  }

  /** Asks the daemon to re-apply the current mode after a reboot. */
  async setRestoreOnStart(enabled: boolean) {
    try {
      this.applyConfigReply(await daemon.setRestoreOnStart(enabled));
    } catch (e) {
      this.lastError = errorText(e);
    }
  }

  private applyConfigReply(reply: PowerConfigReply) {
    if (this.power) {
      this.power = {
        ...this.power,
        auto: reply.auto,
        restoreModeOnStart: reply.restoreModeOnStart,
        configSaveError: reply.saveError,
      };
    }
    // A setting that was applied but not written is worth saying out loud:
    // it silently reverts on the next daemon restart.
    this.lastError = reply.saved ? null : reply.saveError;
  }

  async setFanMode(mode: FanMode) {
    this.set("fanMode", mode);
    await this.pushFan(() => daemon.setFanMode(mode, percentToPwm(this.state.fanPercent)));
  }

  /**
   * The manual speed. Only reaches the daemon while manual is the mode in
   * force - moving the slider in any other mode is choosing a value for
   * later, not commanding the fans now.
   */
  setFanPercent(percent: number) {
    this.set("fanPercent", percent);
    if (this.state.fanMode !== "manual") return;
    this.pushFanSoon(() => daemon.setFanMode("manual", percentToPwm(percent)));
  }

  /**
   * The curve is stored daemon-side whatever the current mode is, so that
   * switching to `curve` later follows the shape the user drew rather than
   * an empty one.
   */
  setFanCurve(curve: CurvePoint[]) {
    this.set("fanCurve", curve);
    this.pushFanSoon(() => daemon.setFanCurve(curve));
  }

  /**
   * Whether picking a mode should also move the OS power profile, or only
   * the laptop's own. They are separate things - the firmware profile is
   * what moves the fan curve, the OS one is what the desktop's battery
   * menu shows - and wanting one without the other is reasonable.
   */
  async setApplyToOsProfile(enabled: boolean) {
    this.set("applyToOsPowerProfile", enabled);
    try {
      this.power = await daemon.setApplyToOsProfile(enabled);
      this.lastError = null;
    } catch (e) {
      this.lastError = errorText(e);
    }
  }

  /**
   * Tunes the current mode's power envelope. Debounced like the fan
   * controls - these are sliders, and each change is a socket round trip
   * that re-applies the profile to the CPU.
   */
  setPowerTuning(tuning: { pl1W?: number; pl2W?: number; turbo?: boolean }) {
    if (tuning.pl1W !== undefined) this.set("pl1", Math.round(tuning.pl1W));
    if (tuning.pl2W !== undefined) this.set("pl2", Math.round(tuning.pl2W));
    if (this.powerTuningTimer !== null) clearTimeout(this.powerTuningTimer);
    this.powerTuningTimer = setTimeout(() => {
      this.powerTuningTimer = null;
      void (async () => {
        try {
          this.power = await daemon.setPowerTuning(tuning);
          this.lastError = null;
        } catch (e) {
          this.lastError = errorText(e);
        }
      })();
    }, FAN_PUSH_DEBOUNCE_MS);
  }

  /** Asks the daemon to put the fans back where they were after a reboot. */
  async setFanRestoreOnStart(enabled: boolean) {
    await this.pushFan(() => daemon.setFanRestoreOnStart(enabled));
  }

  /**
   * The daemon's own view of the fans, from the telemetry poll. It is the
   * authority on what is actually in force - a machine that cannot do the
   * mode the UI is showing corrects it here rather than leaving a button
   * lit that nothing is honouring. Deliberately not saved to disk: this is
   * an observation, and the daemon persists its own mode.
   */
  observeFan(status: FanStatus) {
    this.fan = status;
    if (status.driverInstalled && status.mode !== this.state.fanMode) {
      this.state = { ...this.state, fanMode: status.mode };
    }
  }

  private async pushFan(send: () => Promise<FanStatus>) {
    try {
      this.fan = await send();
      // The daemon is the authority on which mode is actually in force: a
      // machine that cannot do manual says so here rather than leaving the
      // UI showing a mode nothing is honouring.
      this.state = { ...this.state, fanMode: this.fan.mode };
      this.lastError = this.fan.error ? tm(this.fan.error) : null;
    } catch (e) {
      this.lastError = errorText(e);
    }
  }

  /**
   * Same, but coalesced: a slider drag or a dragged curve point fires on
   * every pixel, and each of these is a socket round trip.
   */
  private pushFanSoon(send: () => Promise<FanStatus>) {
    if (this.fanPushTimer !== null) clearTimeout(this.fanPushTimer);
    this.fanPushTimer = setTimeout(() => {
      this.fanPushTimer = null;
      void this.pushFan(send);
    }, FAN_PUSH_DEBOUNCE_MS);
  }

  /** Writes immediately, e.g. before the window closes. */
  flush() {
    return this.disk.flush();
  }

  private save() {
    this.disk.save(this.state);
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
