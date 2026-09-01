/**
 * Live hardware readings for the whole UI.
 *
 * One poller feeds every page, so switching tabs doesn't restart sampling
 * and the history graphs stay continuous. When the daemon can't be reached
 * (browser dev, daemon not started, driver missing) the store falls back to
 * a synthetic signal and flags itself `demo` - pages show real layouts with
 * plausible numbers instead of a wall of "--", and the banner in the
 * layout tells the user the data isn't real.
 */

import { DaemonUnavailable, daemon, type FanStatus, type SystemInfo } from "$lib/api/daemon";
import { settings } from "./settings.svelte";

/** Number of samples kept for the sparkline graphs (~2 min at 2s). */
const HISTORY = 60;

export type Series = { label: string; color: string; values: number[] };

function pushCapped(values: number[], value: number): number[] {
  const next = values.length >= HISTORY ? values.slice(1) : values.slice();
  next.push(value);
  return next;
}

/** Smooth pseudo-random walk, kept inside [min, max]. */
function drift(previous: number, min: number, max: number, step: number): number {
  const next = previous + (Math.random() - 0.5) * step * 2;
  return Math.min(max, Math.max(min, next));
}

export class Telemetry {
  /** True while the last poll failed and the numbers below are synthetic. */
  demo = $state(true);
  daemonError = $state<string | null>(null);
  driverInstalled = $state(false);
  systemInfo = $state<SystemInfo | null>(null);

  cpuTempC = $state(52);
  gpuTempC = $state<number | null>(null);
  chassisTempC = $state(39);
  cpuUsage = $state(12);
  gpuUsage = $state<number | null>(null);
  ramUsedGb = $state(8.6);
  ramTotalGb = $state(31.4);
  fanRpm = $state(0);
  fanReverse = $state(false);
  netUpMbps = $state(0);
  netDownMbps = $state(0);

  cpuTempHistory = $state<number[]>([]);
  cpuUsageHistory = $state<number[]>([]);
  gpuUsageHistory = $state<number[]>([]);
  ramHistory = $state<number[]>([]);
  fanHistory = $state<number[]>([]);

  private timer: ReturnType<typeof setInterval> | null = null;
  private subscribers = 0;

  get ramPercent(): number {
    return this.ramTotalGb > 0 ? (this.ramUsedGb / this.ramTotalGb) * 100 : 0;
  }

  /**
   * Ref-counted so several mounted components can `start()` independently
   * without any of them stopping the poller the others still need.
   */
  start() {
    this.subscribers += 1;
    if (this.timer !== null) return;
    void this.poll();
    this.timer = setInterval(() => void this.poll(), settings.current.pollIntervalMs);
  }

  stop() {
    this.subscribers = Math.max(0, this.subscribers - 1);
    if (this.subscribers === 0 && this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /** Applies a changed refresh interval without losing history. */
  restart() {
    if (this.timer === null) return;
    clearInterval(this.timer);
    this.timer = setInterval(() => void this.poll(), settings.current.pollIntervalMs);
  }

  async loadSystemInfo() {
    try {
      this.systemInfo = await daemon.systemInfo();
    } catch {
      // The daemon command doesn't exist yet (see TODO: DMI reader), so
      // this is the expected path today rather than an error worth showing.
      this.systemInfo = null;
    }
  }

  private async poll() {
    try {
      const status: FanStatus = await daemon.fanStatus();
      this.demo = false;
      this.daemonError = null;
      this.driverInstalled = status.driverInstalled;
      this.cpuTempC = status.cpuTempC ?? this.cpuTempC;
      this.fanRpm = status.fanRpm;
      this.fanReverse = status.isReverse;
    } catch (e) {
      this.daemonError = e instanceof DaemonUnavailable ? e.message : String(e);
      this.demo = true;
      this.driverInstalled = false;
      if (settings.current.demoData) this.simulate();
    }
    this.record();
  }

  private simulate() {
    this.cpuUsage = drift(this.cpuUsage, 2, 100, 9);
    this.cpuTempC = drift(this.cpuTempC, 42, 92, 2.5);
    this.gpuUsage = drift(this.gpuUsage ?? 4, 0, 100, 7);
    this.gpuTempC = drift(this.gpuTempC ?? 46, 38, 84, 2);
    this.chassisTempC = drift(this.chassisTempC, 34, 52, 0.6);
    this.ramUsedGb = drift(this.ramUsedGb, 6, this.ramTotalGb - 2, 0.4);
    // Fans idle below ~55 C on these machines, then ramp with temperature.
    this.fanRpm =
      this.cpuTempC < 55 ? 0 : Math.round((1200 + (this.cpuTempC - 55) * 90) / 100) * 100;
    this.netDownMbps = drift(this.netDownMbps, 0, 90, 12);
    this.netUpMbps = drift(this.netUpMbps, 0, 20, 3);
  }

  private record() {
    this.cpuTempHistory = pushCapped(this.cpuTempHistory, this.cpuTempC);
    this.cpuUsageHistory = pushCapped(this.cpuUsageHistory, this.cpuUsage);
    this.gpuUsageHistory = pushCapped(this.gpuUsageHistory, this.gpuUsage ?? 0);
    this.ramHistory = pushCapped(this.ramHistory, this.ramPercent);
    this.fanHistory = pushCapped(this.fanHistory, this.fanRpm);
  }
}

export const telemetry = new Telemetry();

/** Colour band for a temperature readout, matching the legend in the app. */
export function tempColor(celsius: number | null): string {
  if (celsius === null) return "var(--text-mute)";
  if (celsius >= 90) return "var(--danger)";
  if (celsius >= 75) return "var(--warn)";
  return "var(--ok)";
}
