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
 * When the daemon itself can't be reached the store falls back to a
 * synthetic signal and flags itself `demo`, so pages show real layouts with
 * plausible numbers instead of a wall of "--".
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
import { settings } from "./settings.svelte";

/** Number of samples kept for the sparkline graphs (~2 min at 2s). */
const HISTORY = 60;

export type Series = { label: string; color: string; values: number[] };

function pushCapped(values: number[], value: number): number[] {
  const next = values.length >= HISTORY ? values.slice(1) : values.slice();
  next.push(value);
  return next;
}

/** Shape of the machine the demo signal pretends to be. */
const DEMO_CORES = Array.from({ length: 8 });

const DEMO_DISKS: DiskUsage[] = [
  {
    mount: "/",
    device: "/dev/nvme0n1p2",
    fstype: "ext4",
    totalBytes: 1_000_204_886_016,
    freeBytes: 412_316_860_416,
  },
  {
    mount: "/home",
    device: "/dev/nvme0n1p3",
    fstype: "ext4",
    totalBytes: 2_000_398_934_016,
    freeBytes: 1_331_439_861_760,
  },
];

const DEMO_PROCESSES = [
  "chrome",
  "code",
  "pyren",
  "gnome-shell",
  "steam",
  "node",
  "Xorg",
  "pipewire",
];

/** Smooth pseudo-random walk, kept inside [min, max]. */
function drift(previous: number, min: number, max: number, step: number): number {
  const next = previous + (Math.random() - 0.5) * step * 2;
  return Math.min(max, Math.max(min, next));
}

export class Telemetry {
  /** True while the daemon is unreachable and the numbers are synthetic. */
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
    let reachable = false;

    try {
      const metrics = await daemon.systemMetrics();
      reachable = true;
      this.applyMetrics(metrics);
    } catch (e) {
      this.daemonError = e instanceof DaemonUnavailable ? e.message : String(e);
      if (settings.current.demoData) this.simulate();
    }

    // The fan module is HP-only: its absence says nothing about whether the
    // daemon is up, so it must not flip the demo flag.
    try {
      const status = await daemon.fanStatus();
      reachable = true;
      hardware.observeFan(status);
      this.driverInstalled = status.driverInstalled;
      this.fanReverse = status.isReverse;
      if (status.driverInstalled) {
        this.fanRpm = status.fanRpm;
        this.cpuTempC = status.cpuTempC ?? this.cpuTempC;
      }
    } catch {
      this.driverInstalled = false;
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

  private simulate() {
    this.cpuUsage = drift(this.cpuUsage, 2, 100, 9);
    this.cpuTempC = drift(this.cpuTempC, 42, 92, 2.5);
    this.gpuUsage = drift(this.gpuUsage ?? 4, 0, 100, 7);
    this.gpuTempC = drift(this.gpuTempC ?? 46, 38, 84, 2);
    this.chassisTempC = drift(this.chassisTempC ?? 39, 34, 52, 0.6);
    this.ramUsedGb = drift(this.ramUsedGb, 6, this.ramTotalGb - 2, 0.4);
    // Fans idle below ~55 C on these machines, then ramp with temperature.
    this.fanRpm =
      this.cpuTempC < 55 ? 0 : Math.round((1200 + (this.cpuTempC - 55) * 90) / 100) * 100;
    this.netDownMbps = drift(this.netDownMbps, 0, 90, 12);
    this.netUpMbps = drift(this.netUpMbps, 0, 20, 3);

    // Everything below used to be left untouched, so a machine with no
    // daemon showed drifting gauges next to an empty storage panel, an
    // empty process table and no GPU section at all - which reads as a
    // half-finished page rather than as "there is no daemon". A demo has to
    // fill every panel it is standing in for.
    this.perCoreUsage = DEMO_CORES.map((_, i) =>
      drift(this.perCoreUsage[i] ?? this.cpuUsage, 0, 100, 14),
    );
    this.coreClocksMhz = DEMO_CORES.map((_, i) =>
      drift(this.coreClocksMhz[i] ?? 2400, 800, 4800, 300),
    );
    this.temperatures = [
      { chip: "coretemp", label: "Package id 0", celsius: this.cpuTempC },
      ...DEMO_CORES.map((_, i) => ({
        chip: "coretemp",
        label: `Core ${i}`,
        celsius: drift(this.cpuTempC, this.cpuTempC - 6, this.cpuTempC + 4, 3),
      })),
      { chip: "acpitz", label: "temp1", celsius: this.chassisTempC ?? 39 },
    ];
    // A hybrid pair, because that is what these laptops are: the demo
    // should stand in for the layout a real machine produces, not a
    // simpler one.
    this.gpus = [
      {
        name: "Demo discrete GPU",
        driver: "demo",
        integrated: false,
        usagePercent: this.gpuUsage,
        tempC: this.gpuTempC,
        memUsedMb: drift(this.gpus[0]?.memUsedMb ?? 1200, 400, 7800, 180),
        memTotalMb: 8192,
        powerW: drift(this.gpus[0]?.powerW ?? 40, 8, 120, 9),
        clockMhz: drift(this.gpus[0]?.clockMhz ?? 1600, 300, 2600, 180),
      },
      {
        name: "Demo integrated GPU",
        driver: "demo",
        integrated: true,
        usagePercent: drift(this.gpus[1]?.usagePercent ?? 8, 0, 70, 6),
        tempC: this.cpuTempC,
        memUsedMb: null,
        memTotalMb: null,
        powerW: null,
        clockMhz: drift(this.gpus[1]?.clockMhz ?? 900, 150, 2200, 140),
      },
    ];
    this.disks = DEMO_DISKS;
    this.processes = DEMO_PROCESSES.map((name, i) => ({
      pid: 1000 + i,
      name,
      cpuPercent: drift(this.processes[i]?.cpuPercent ?? 4, 0, 60, 6),
      memMb: drift(this.processes[i]?.memMb ?? 260, 40, 2600, 60),
      // Only some processes hold a GPU; the rest report null, which is what
      // the table draws as "--".
      gpuPercent: i % 3 === 0 ? drift(this.processes[i]?.gpuPercent ?? 3, 0, 40, 5) : null,
    })).sort((a, b) => b.cpuPercent - a.cpuPercent);
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
