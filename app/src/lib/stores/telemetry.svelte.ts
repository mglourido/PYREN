/**
 * Live hardware readings for the whole UI.
 *
 * One poller feeds every page, so switching tabs doesn't restart sampling
 * and the history graphs stay continuous.
 *
 * Two sources, deliberately independent:
 *
 * - `system.getMetrics` is generic Linux and works on any machine, so CPU,
 *   memory, disks, network, GPU and process data appear even on hardware
 *   the OMEN features don't support.
 * - `fan.getStatus` is HP-only and is allowed to fail on its own without
 *   taking the rest of the readings down with it.
 *
 * When the daemon itself can't be reached the store keeps the last readings
 * and flags itself `demo`, and the pages draw a "daemon unreachable" notice.
 */

import {
  DaemonUnavailable,
  daemon,
  type DiskUsage,
  type GpuMetrics,
  type ProcessUsage,
  type SystemInfo,
  type TempReading,
} from "$lib/api/daemon";
import { hardware } from "./hardware.svelte";
import { notifications } from "./notifications.svelte";
import { settings } from "./settings.svelte";

/** Number of samples kept for the sparkline graphs (~2 min at 2s). */
const HISTORY = 60;

export type Series = { label: string; color: string; values: number[] };

function pushCapped(values: number[], value: number): number[] {
  const next = values.length >= HISTORY ? values.slice(1) : values.slice();
  next.push(value);
  return next;
}

export class Telemetry {
  /** True while the daemon is unreachable. */
  demo = $state(true);
  daemonError = $state<string | null>(null);
  /** Whether the patched hp-wmi driver is present (HP machines only). */
  driverInstalled = $state(false);
  systemInfo = $state<SystemInfo | null>(null);

  cpuTempC = $state(52);
  gpuTempC = $state<number | null>(null);
  chassisTempC = $state<number | null>(39);
  cpuUsage = $state(12);
  gpuUsage = $state<number | null>(null);
  perCoreUsage = $state<number[]>([]);
  coreClocksMhz = $state<number[]>([]);
  ramUsedGb = $state(8.6);
  ramTotalGb = $state(31.4);
  swapUsedGb = $state(0);
  swapTotalGb = $state(0);
  fanRpm = $state(0);
  fanReverse = $state(false);
  netUpMbps = $state(0);
  netDownMbps = $state(0);

  gpu = $state<GpuMetrics | null>(null);
  gpus = $state<GpuMetrics[]>([]);
  disks = $state<DiskUsage[]>([]);
  processes = $state<ProcessUsage[]>([]);
  temperatures = $state<TempReading[]>([]);

  cpuTempHistory = $state<number[]>([]);
  cpuUsageHistory = $state<number[]>([]);
  gpuUsageHistory = $state<number[]>([]);
  /** One history per GPU, keyed by name, so a hybrid machine's two chips
   *  each get their own graph instead of sharing the primary one's. */
  gpuHistories = $state<Record<string, number[]>>({});
  gpuTempHistory = $state<number[]>([]);
  ramHistory = $state<number[]>([]);
  fanHistory = $state<number[]>([]);

  private timer: ReturnType<typeof setInterval> | null = null;
  private subscribers = 0;
  /** True while a poll is in flight; see `poll()`. */
  private polling = false;
  /**
   * The previous poll's reachability, so the console line below fires on
   * the transition rather than once every interval - a machine with no
   * daemon would otherwise print a line every two seconds forever.
   * `null` until the first poll, so the first failure still logs.
   */
  private lastReachable: boolean | null = null;

  /**
   * The configured interval, floored at 250 ms. A settings file holding 0
   * (or anything unparseable) would otherwise schedule a timer that fires
   * as fast as the browser will allow and never lets the page paint.
   */
  private get intervalMs(): number {
    const value = settings.current.pollIntervalMs;
    return Number.isFinite(value) ? Math.max(250, value) : 2000;
  }

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
    this.timer = setInterval(() => void this.poll(), this.intervalMs);
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
    this.timer = setInterval(() => void this.poll(), this.intervalMs);
  }

  async loadSystemInfo() {
    try {
      this.systemInfo = await daemon.systemInfo();
    } catch {
      // Expected whenever the daemon isn't running; the UI shows "unknown"
      // rather than an error, since it already says the daemon is down.
      this.systemInfo = null;
    }
  }

  private async poll() {
    // A poll that outlives its interval would otherwise start a second one
    // on top of it, and two in flight write the same fields and both call
    // `record()` - which puts two samples in the history for one tick and
    // makes the graphs run at the wrong speed. Skipping the tick is the
    // right answer: the next one is already scheduled.
    if (this.polling) return;
    this.polling = true;
    try {
      await this.pollOnce();
    } finally {
      this.polling = false;
    }
  }

  private async pollOnce() {
    let reachable = false;

    // Started together, applied in order. These are two independent round
    // trips to the daemon and awaiting them one after the other made every
    // tick cost the sum of the two for no reason.
    //
    // `allSettled` rather than `all`, because either is allowed to fail on
    // its own: the fan module is HP-only and its absence says nothing about
    // whether the daemon is up.
    const [metrics, fan] = await Promise.allSettled([
      daemon.systemMetrics(),
      daemon.fanStatus(),
    ]);

    if (metrics.status === "fulfilled") {
      reachable = true;
      this.applyMetrics(metrics.value);
    } else {
      const e = metrics.reason;
      this.daemonError = e instanceof DaemonUnavailable ? e.message : String(e);
    }

    // Applied after the metrics and not before: `applyMetrics` reads
    // `driverInstalled` to decide whether to believe the generic hwmon fan,
    // and it has to see the value from the previous poll, the way it did
    // when these two calls ran in series.
    if (fan.status === "fulfilled") {
      const status = fan.value;
      reachable = true;
      hardware.observeFan(status);
      // The daemon persists its fan-floor-raise log, so a raise that
      // happened with the app closed still reaches the bell on this poll.
      notifications.observeFanStatus(status);
      this.driverInstalled = status.driverInstalled;
      this.fanReverse = status.isReverse;
      if (status.driverInstalled) {
        this.fanRpm = status.fanRpm;
        this.cpuTempC = status.cpuTempC ?? this.cpuTempC;
      }
    } else {
      this.driverInstalled = false;
    }

    // A daemon that goes away leaves the last readings frozen on screen; the
    // pages show a notice, and the console spells it out too.
    if (reachable !== this.lastReachable) {
      if (!reachable) {
        console.warn(`pyren: daemon unreachable (${this.daemonError ?? "no reason given"});`, "vitals are stale");
      } else if (this.lastReachable === false) {
        console.info("pyren: daemon reachable again; vitals are live");
      }
      this.lastReachable = reachable;
    }

    this.demo = !reachable;
    if (reachable) this.daemonError = null;
    this.record();
  }

  private applyMetrics(metrics: Awaited<ReturnType<typeof daemon.systemMetrics>>) {
    this.cpuUsage = metrics.cpu.usagePercent;
    this.perCoreUsage = metrics.cpu.perCorePercent;
    this.coreClocksMhz = metrics.cpu.clocksMhz;
    this.cpuTempC = metrics.cpu.tempC ?? this.cpuTempC;

    this.ramUsedGb = metrics.memory.usedGb;
    this.ramTotalGb = metrics.memory.totalGb;
    this.swapUsedGb = metrics.memory.swapUsedGb;
    this.swapTotalGb = metrics.memory.swapTotalGb;

    this.temperatures = metrics.temperatures;
    this.chassisTempC = chassisTemperature(metrics.temperatures);

    this.disks = metrics.disks;
    this.processes = metrics.processes;

    this.netUpMbps = metrics.network.upMbps;
    this.netDownMbps = metrics.network.downMbps;

    this.gpus = metrics.gpus;
    // The first GPU with real telemetry is the one the dashboard shows;
    // an iGPU that reports nothing shouldn't hide a discrete card that does.
    this.gpu = metrics.gpus.find((g) => g.usagePercent !== null) ?? metrics.gpus[0] ?? null;
    this.gpuUsage = this.gpu?.usagePercent ?? null;
    this.gpuTempC = this.gpu?.tempC ?? null;

    // Without the hp-wmi driver, report whatever fan the board does expose
    // (a super-I/O chip on desktops) rather than a flat zero.
    if (!this.driverInstalled) {
      this.fanRpm = metrics.fans.reduce((max, fan) => Math.max(max, fan.rpm), 0);
      this.fanReverse = false;
    }
  }

  private record() {
    this.cpuTempHistory = pushCapped(this.cpuTempHistory, this.cpuTempC);
    this.cpuUsageHistory = pushCapped(this.cpuUsageHistory, this.cpuUsage);
    this.gpuUsageHistory = pushCapped(this.gpuUsageHistory, this.gpuUsage ?? 0);
    // Rebuilt rather than updated in place, so a card that goes away does
    // not leave its history behind for the next one to inherit.
    this.gpuHistories = Object.fromEntries(
      this.gpus.map((gpu) => [
        gpu.name,
        pushCapped(this.gpuHistories[gpu.name] ?? [], gpu.usagePercent ?? 0),
      ]),
    );
    this.gpuTempHistory = pushCapped(this.gpuTempHistory, this.gpuTempC ?? 0);
    this.ramHistory = pushCapped(this.ramHistory, this.ramPercent);
    this.fanHistory = pushCapped(this.fanHistory, this.fanRpm);
  }
}

export const telemetry = new Telemetry();

/**
 * Best guess at a "chassis" temperature: the ACPI thermal zone, or any
 * sensor a board labels as ambient/system. Returns null rather than a
 * misleading substitute when neither exists.
 */
function chassisTemperature(readings: TempReading[]): number | null {
  const labelled = readings.find((r) => {
    const label = r.label.toLowerCase();
    return label.includes("chassis") || label.includes("systin") || label.includes("ambient");
  });
  if (labelled) return labelled.celsius;
  return readings.find((r) => r.chip === "acpitz")?.celsius ?? null;
}

/** Colour band for a temperature readout, matching the legend in the app. */
export function tempColor(celsius: number | null): string {
  if (celsius === null) return "var(--text-mute)";
  if (celsius >= 90) return "var(--danger)";
  if (celsius >= 75) return "var(--warn)";
  return "var(--ok)";
}
