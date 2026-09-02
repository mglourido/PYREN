/**
 * Thin wrapper around the Tauri commands that proxy to pyren-daemon
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

/** What this machine's hp-wmi driver actually exposes. */
export type FanCapabilities = {
  /** `pwm1_enable`: auto and max can be commanded. */
  switchMode: boolean;
  /** `pwm1`: a specific speed can be commanded. */
  setSpeed: boolean;
};

export type FanDaemonMode = "auto" | "max" | "manual" | "curve";

export type FanCurvePoint = { tempC: number; percent: number };

export type FanStatus = {
  driverInstalled: boolean;
  capabilities: FanCapabilities;
  cpuTempC: number | null;
  fanRpm: number;
  isReverse: boolean;
  mode: FanDaemonMode;
  /** Raw 0-255 the driver reports, or null where `pwm1` does not exist. */
  pwm: number | null;
  targetPwm: number | null;
  manualPwm: number;
  curve: FanCurvePoint[];
  interpolation: "smooth" | "discrete";
  restoreModeOnStart: boolean;
  fanMaxRpm: number | null;
  /** Last failure from the control loop, e.g. a write that needed root. */
  error: string | null;
  saved: boolean;
  saveError: string | null;
};

export type ModuleCapability = { id: string; supported: boolean };

/**
 * What this machine was found able to control. An observation the daemon
 * makes by asking each hardware module what it could actually do - not a
 * lookup of the board id, which used to call a machine "supported" on the
 * strength of a copied list while its fans refused to be set.
 */
export type Compatibility = "controllable" | "monitoringOnly" | "unsupported";

/** The itemised version of `Compatibility`. Gate UI on these, not on the summary. */
export type Controls = {
  /** Fan mode switching (auto/max). */
  fanMode: boolean;
  /** A specific fan speed, i.e. manual and curve. */
  fanSpeed: boolean;
  powerMode: boolean;
};

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
  controls: Controls;
  supported: boolean;
  reason: string;
  /** What the *daemon* was started with, as opposed to what the machine
   *  can do. Some readings are gated on privilege, not on hardware. */
  privileges: { root: boolean; perfEvents: boolean };
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
  /** The chip inside the CPU package, as opposed to a card of its own. */
  integrated: boolean | null;
};

export type ProcessUsage = {
  pid: number;
  name: string;
  cpuPercent: number;
  memMb: number;
  /** `null` where the driver publishes no per-process accounting, which is
   *  not the same as an idle process - the table shows the two apart. */
  gpuPercent: number | null;
};

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
  /** The "switch to Eco automatically" system: unplugging drops to
   *  Balanced, and a machine that stays idle - or whose battery gets low -
   *  goes on to Eco. */
  ecoOnBattery: boolean;
  /** The "switch to Performance automatically" system: plugging in steps up
   *  to Performance, and an idle machine on mains comes back to Balanced. */
  performanceOnLoad: boolean;
  loadHigh: number;
  loadLow: number;
  /** Battery percentage at or below which Eco is preferred whatever the
   *  load is doing. */
  batteryLowPercent: number;
  samplesToSwitch: number;
  intervalSecs: number;
  manualOverrideSecs: number;
};

/** Package power limits in microwatts; `null` for one this machine lacks. */
export type PowerLimits = { pl1Uw: number | null; pl2Uw: number | null; pl4Uw: number | null };

/** One mode's share of the machine's stock envelope. */
export type PowerTuning = { pl1Percent: number; pl2Percent: number; turbo: boolean };

export type PowerLimitState = {
  available: boolean;
  turboAvailable: boolean;
  /** What the firmware shipped, captured before the daemon wrote anything.
   *  Everything else is a percentage of this, and nothing exceeds it. */
  stock: PowerLimits | null;
  current: PowerLimits;
  turbo: boolean | null;
  tuning: Record<PowerMode, PowerTuning>;
};

export type PowerState = {
  mode: PowerMode;
  limits: PowerLimitState;
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
  /** Re-apply the saved mode when the daemon starts. */
  restoreModeOnStart: boolean;
  /** Whether a mode change also changes the OS power profile
   *  (power-profiles-daemon), or only the laptop's own firmware profile. */
  applyToOsProfile: boolean;
  autoOverrideSecondsLeft: number | null;
  lastAutoSwitch: string | null;
  /** Where the daemon keeps this module's settings. */
  configPath: string;
  /** Set when the daemon could not write its config file. */
  configSaveError: string | null;
};

/** Reply from the calls that change stored power settings. */
export type PowerConfigReply = {
  auto: AutoConfig;
  restoreModeOnStart: boolean;
  saved: boolean;
  saveError: string | null;
};

/** What `power.setMode` actually managed to change. */
export type ApplyReport = { applied: string[]; failed: string[] };

export type CheckStatus = "pass" | "fail" | "warn" | "skip";

export type FanCheck = {
  id: string;
  title: string;
  status: CheckStatus;
  detail: string;
  remedy: string | null;
};

/** Overall conclusion of the fan-control self-test. */
export type FanVerdict = "fullControl" | "monitoringOnly" | "unsupported";

export type FanDiagnosis = {
  verdict: FanVerdict;
  summary: string;
  /** Set when a driver that might help exists but isn't in use. */
  driverNotice: string | null;
  checks: FanCheck[];
  wroteToHardware: boolean;
};

/* --- installer ---------------------------------------------------------
 *
 * Mirrors `daemon/crates/installer/src/{detect,plan,execute,patch}.rs`.
 * The wire shapes are documented in `docs/01-ipc-protocol.md`.
 */

export type InstallerAction =
  | "installDriver"
  | "restoreDriver"
  | "installService"
  | "removeService";

/** How a permanent install survives a kernel upgrade. */
export type InstallStrategy = "dkms" | "hooks";

export type HookFlavour = "pacman" | "kernelPostinst" | "kernelInstall" | "none";

export type InstallerEnvironment = {
  kernel: {
    release: string;
    major: number;
    minor: number;
    /** The running kernel already has manual fan control of its own. */
    hasUpstreamFanControl: boolean;
  };
  distroId: string;
  hookFlavour: HookFlavour;
  headers: {
    buildDir: string | null;
    hasAutoconf: boolean;
    hasKbuildScripts: boolean;
    usable: boolean;
    fixHint: string | null;
  };
  hasDkms: boolean;
  dkmsInstalled: boolean;
  dkmsStatus: string | null;
  hasMake: boolean;
  hasCompiler: boolean;
  initramfsTool: string | null;
  /** `pwm1` exists right now, whatever driver is providing it. */
  fanControlAvailable: boolean;
  hpWmiLoaded: boolean;
  acpiCallAvailable: boolean;
  driverSource: string | null;
  serviceInstalled: boolean;
};

export type InstallerInspection = {
  environment: InstallerEnvironment;
  patchNeeded: boolean;
};

/** An empty `command` is a step the daemon carries out itself. */
export type InstallStep = {
  id: string;
  description: string;
  command: string[];
  optional: boolean;
};

export type InstallBlocker = { id: string; message: string; fix: string | null };

export type InstallPlan = {
  action: InstallerAction;
  strategy: InstallStrategy | null;
  steps: InstallStep[];
  blockers: InstallBlocker[];
  warnings: string[];
  needsRoot: boolean;
};

export type StepStatus = "ok" | "warned" | "failed" | "skipped" | "planned";

export type StepResult = {
  id: string;
  description: string;
  status: StepStatus;
  detail: string;
};

export type ExecutionReport = {
  dryRun: boolean;
  succeeded: boolean;
  results: StepResult[];
};

/** Which board-params variant an untested board should be driven with. */
export type BoardParams = "victusS" | "omenV1" | "omenV1Legacy" | "omenV1NoEc";

/**
 * Which of the driver's tables an untested board id goes into. Serde
 * writes this adjacently tagged, so the unit tables carry no `params`.
 */
export type BoardTable =
  | { table: "omenThermalProfile" }
  | { table: "omenForceV0" }
  | { table: "omenTimed" }
  | { table: "victusThermalProfile" }
  | { table: "features"; params: BoardParams };

export type InstallerRequest = {
  action: InstallerAction;
  preferHooks?: boolean;
  force?: boolean;
  /** Anything but `true` leaves `apply` a dry run. */
  confirm?: boolean;
  cpuMaxRpm?: number | null;
  gpuMaxRpm?: number | null;
  experimentalBoard?: string | null;
  boardTable?: BoardTable | null;
};

export type ApplyResult = { plan: InstallPlan; report: ExecutionReport };

export class DaemonUnavailable extends Error {}

/**
 * The daemon method behind each Tauri command, so `vite dev` in a browser
 * can reach the daemon through the dev-server bridge instead of falling
 * back to synthetic numbers (see `dev-daemon-bridge.js`).
 *
 * `params` is the argument object as passed here; two commands hand one of
 * their arguments straight through as the daemon's params, which is what
 * the Rust side does too - keep this table in step with
 * `app/src-tauri/src/lib.rs`.
 */
const DAEMON_ROUTES: Record<
  string,
  { module: string; method: string; params?: (args: Record<string, unknown>) => unknown }
> = {
  core_capabilities: { module: "core", method: "capabilities" },
  system_get_info: { module: "system", method: "getInfo" },
  system_get_metrics: { module: "system", method: "getMetrics" },
  fan_get_status: { module: "fan", method: "getStatus" },
  fan_diagnose: { module: "fan", method: "diagnose" },
  fan_set_mode: { module: "fan", method: "setMode" },
  fan_set_curve: { module: "fan", method: "setCurve" },
  fan_set_restore_on_start: { module: "fan", method: "setRestoreOnStart" },
  power_get_state: { module: "power", method: "getState" },
  power_set_mode: { module: "power", method: "setMode" },
  power_set_auto_config: { module: "power", method: "setAutoConfig", params: (a) => a.config },
  power_set_restore_on_start: { module: "power", method: "setRestoreOnStart" },
  power_set_tuning: { module: "power", method: "setTuning", params: (a) => a.tuning },
  power_set_apply_to_os_profile: { module: "power", method: "setApplyToOsProfile" },
  installer_inspect: { module: "installer", method: "inspect" },
  installer_plan: { module: "installer", method: "plan", params: (a) => a.request },
  installer_apply: { module: "installer", method: "apply", params: (a) => a.request },
};

/** Reaches the daemon through the Vite dev server. Development only. */
async function callViaDevBridge<T>(
  command: string,
  args: Record<string, unknown> | undefined,
): Promise<T> {
  const route = DAEMON_ROUTES[command];
  if (!route) throw new DaemonUnavailable(`no daemon route for '${command}'`);

  // Tauri turns a missing optional argument into an explicit null; match
  // that so the daemon sees the same params either way.
  const named = Object.fromEntries(
    Object.entries(args ?? {}).map(([k, v]) => [k, v === undefined ? null : v]),
  );
  const params = route.params ? route.params(named) : (args ? named : null);

  let response: Response;
  try {
    response = await fetch("/__daemon", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ id: 1, module: route.module, method: route.method, params }),
    });
  } catch (e) {
    throw new DaemonUnavailable(String(e));
  }

  // Two shapes arrive here. The daemon's own refusals are
  // `{ kind, message }` - see `docs/01-ipc-protocol.md` - while the bridge
  // reports its own failures (no daemon, timeout) as a plain string.
  const reply = (await response.json().catch(() => null)) as
    | { result?: T; error?: string | { kind?: string; message?: string } }
    | null;
  if (!reply) throw new DaemonUnavailable("malformed reply from the dev bridge");
  if (reply.error !== undefined && reply.error !== null) {
    const message =
      typeof reply.error === "string"
        ? reply.error
        : (reply.error.message ?? "the daemon refused without saying why");
    throw new DaemonUnavailable(message);
  }
  return reply.result as T;
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!inTauri) return callViaDevBridge<T>(command, args);
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
  /** `allowWrites` opts into the one check that touches hardware. */
  fanDiagnose: (allowWrites = false) => call<FanDiagnosis>("fan_diagnose", { allowWrites }),
  /** `pwm` (0-255) is required for `manual` and ignored otherwise. */
  setFanMode: (mode: FanDaemonMode, pwm?: number) =>
    call<FanStatus>("fan_set_mode", { mode, pwm }),
  /** Stores the curve; it only drives the fans while the mode is `curve`. */
  setFanCurve: (curve: FanCurvePoint[], interpolation?: "smooth" | "discrete") =>
    call<FanStatus>("fan_set_curve", { curve, interpolation }),
  setFanRestoreOnStart: (enabled: boolean) =>
    call<FanStatus>("fan_set_restore_on_start", { enabled }),
  powerState: () => call<PowerState>("power_get_state"),
  setPowerMode: (mode: PowerMode) => call<ApplyReport>("power_set_mode", { mode }),
  setAutoConfig: (config: AutoConfig) =>
    call<PowerConfigReply>("power_set_auto_config", { config }),
  setRestoreOnStart: (enabled: boolean) =>
    call<PowerConfigReply>("power_set_restore_on_start", { enabled }),
  /** Tunes one mode's profile. Watts on the wire; the daemon stores them as
   *  a percentage of this machine's own limits. Defaults to the mode in
   *  force, and re-applies immediately when that is the one changed. */
  /** Whether a mode change also moves the OS power profile. */
  setApplyToOsProfile: (enabled: boolean) =>
    call<PowerState>("power_set_apply_to_os_profile", { enabled }),
  /** What this machine has, and whether the patched driver is needed. */
  installerInspect: () => call<InstallerInspection>("installer_inspect"),
  /** Pure: works out the steps without touching anything. */
  installerPlan: (request: InstallerRequest) => call<InstallPlan>("installer_plan", { request }),
  /** A dry run unless the request carries `confirm: true`. */
  installerApply: (request: InstallerRequest) => call<ApplyResult>("installer_apply", { request }),
  setPowerTuning: (tuning: {
    mode?: PowerMode;
    pl1W?: number;
    pl2W?: number;
    turbo?: boolean;
  }) => call<PowerState>("power_set_tuning", { tuning }),
};
