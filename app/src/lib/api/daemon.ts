/**
 * Thin wrapper around the Tauri commands that proxy to omen-hub-daemon
 * (see docs/01-ipc-protocol.md). Everything the UI needs from the outside
 * world goes through here, so the rest of the frontend never imports
 * `@tauri-apps/api` directly and stays runnable in a plain browser tab
 * (`vite dev`) for UI work.
 *
 * The types below mirror the daemon's serde output field for field; keep
 * them in sync with `daemon/crates/system/src/{identity,metrics}.rs` and
 * `daemon/crates/fan/src/lib.rs`.
 */

import { invoke } from "@tauri-apps/api/core";

/** False when the page is served by Vite in a normal browser, not Tauri. */
export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export type FanStatus = {
  driverInstalled: boolean;
  cpuTempC: number | null;
  fanRpm: number;
  isReverse: boolean;
};

export type ModuleCapability = { id: string; supported: boolean };

/** How far the OMEN-specific features can be trusted on this machine. */
export type Compatibility = "supported" | "untested" | "unsupported";

export type SystemInfo = {
  vendor: string | null;
  model: string | null;
  boardName: string | null;
  boardVendor: string | null;
  biosVersion: string | null;
  biosDate: string | null;
  kernel: string | null;
  cpu: string | null;
  cpuCores: number;
  gpus: string[];
  formFactor: "laptop" | "desktop" | "unknown";
  compatibility: Compatibility;
  supported: boolean;
  reason: string;
};

export type TempReading = { chip: string; label: string; celsius: number };
export type FanReading = { chip: string; label: string; rpm: number };

export type DiskUsage = {
  mount: string;
  device: string;
  fstype: string;
  totalBytes: number;
  freeBytes: number;
};

export type GpuMetrics = {
  name: string;
  driver: string;
  usagePercent: number | null;
  tempC: number | null;
  memUsedMb: number | null;
  memTotalMb: number | null;
  powerW: number | null;
  clockMhz: number | null;
};

export type ProcessUsage = { pid: number; name: string; cpuPercent: number; memMb: number };

export type SystemMetrics = {
  cpu: {
    usagePercent: number;
    perCorePercent: number[];
    clocksMhz: number[];
    tempC: number | null;
  };
  memory: {
    totalGb: number;
    usedGb: number;
    availableGb: number;
    percent: number;
    swapTotalGb: number;
    swapUsedGb: number;
  };
  temperatures: TempReading[];
  fans: FanReading[];
  disks: DiskUsage[];
  network: {
    upMbps: number;
    downMbps: number;
    interfaces: { name: string; upMbps: number; downMbps: number }[];
  };
  gpus: GpuMetrics[];
  processes: ProcessUsage[];
};

export type PowerMode = "eco" | "balanced" | "performance" | "unlimited";

/** Config for the daemon's background Eco/Performance supervisor. */
export type AutoConfig = {
  enabled: boolean;
  ecoOnBattery: boolean;
  performanceOnLoad: boolean;
  loadHigh: number;
  loadLow: number;
  samplesToSwitch: number;
  intervalSecs: number;
  manualOverrideSecs: number;
};

export type PowerState = {
  mode: PowerMode;
  backend: {
    platformProfile: string | null;
    platformProfileChoices: string[];
    powerProfilesDaemon: string | null;
    energyPreference: string | null;
    governor: string | null;
    /** Mechanisms this machine offers, best first. Empty means no control. */
    available: string[];
  };
  supply: {
    onBattery: boolean | null;
    batteryPercent: number | null;
    batteryStatus: string | null;
    hasBattery: boolean;
  };
  auto: AutoConfig;
  autoOverrideSecondsLeft: number | null;
  lastAutoSwitch: string | null;
};

/** What `power.setMode` actually managed to change. */
export type ApplyReport = { applied: string[]; failed: string[] };

export class DaemonUnavailable extends Error {}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!inTauri) throw new DaemonUnavailable("not running inside Tauri");
  try {
    return await invoke<T>(command, args);
  } catch (e) {
    throw new DaemonUnavailable(String(e));
  }
}

export const daemon = {
  capabilities: () => call<ModuleCapability[]>("core_capabilities"),
  systemInfo: () => call<SystemInfo>("system_get_info"),
  systemMetrics: () => call<SystemMetrics>("system_get_metrics"),
  fanStatus: () => call<FanStatus>("fan_get_status"),
  /** Not implemented daemon-side yet; the UI already calls it. */
  setFanMode: (mode: "auto" | "manual" | "max", pwm?: number) =>
    call<null>("fan_set_mode", { mode, pwm }),
  powerState: () => call<PowerState>("power_get_state"),
  setPowerMode: (mode: PowerMode) => call<ApplyReport>("power_set_mode", { mode }),
  setAutoConfig: (config: AutoConfig) => call<AutoConfig>("power_set_auto_config", { config }),
};
