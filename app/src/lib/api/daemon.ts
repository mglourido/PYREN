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
import { listen } from "@tauri-apps/api/event";
import { tm, type Msg } from "$lib/i18n/index.svelte";

export type { Msg };

/** False when the page is served by Vite in a normal browser, not Tauri. */
export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/**
 * What this machine's hp-wmi driver actually accepts.
 *
 * `setSpeed` is the *effective* answer, not a file listing: a board whose
 * `pwm1` exists and whose embedded controller ignores it reports false
 * here once a probe has watched that happen. `FanStatus.speedControl` says
 * which of the two it is.
 */
export type FanCapabilities = {
  /** `pwm1_enable`: auto and max can be commanded. */
  switchMode: boolean;
  /** `pwm1` exists *and* a commanded speed is not known to be ignored. */
  setSpeed: boolean;
};

/**
 * Whether a commanded fan speed was ever found to reach the hardware.
 *
 * `pwm1` existing does not settle it. Board 8D2F takes the write, reports
 * it back as the *measured* speed, and goes on running the firmware's own
 * curve - so manual and curve modes do nothing there while every cheap
 * check passes. Only `fan.probeSpeedControl` can tell the two apart, and
 * only by spinning the fans, so the answer is remembered rather than
 * re-asked. `untested` is the default and offers speed control.
 */
export type FanSpeedControl = "untested" | "honoured" | "ignored";

export type FanDaemonMode = "auto" | "max" | "manual" | "curve";

export type FanCurvePoint = { tempC: number; percent: number };

/** Which temperature the curve follows. */
export type FanReferenceSensor = "cpu" | "gpu";

export type FanStatus = {
  driverInstalled: boolean;
  capabilities: FanCapabilities;
  cpuTempC: number | null;
  /** The discrete GPU's own sensor, where hwmon publishes one. Null is
   *  the common case rather than a fault - an integrated-only machine has
   *  no such sensor, and neither does one whose card is powered down. */
  gpuTempC: number | null;
  fanRpm: number;
  isReverse: boolean;
  mode: FanDaemonMode;
  /** Raw 0-255 the driver reports, or null where `pwm1` does not exist. */
  pwm: number | null;
  targetPwm: number | null;
  manualPwm: number;
  /** The curve actually in force — the active profile's, or the shared
   *  one where no profile applies. Read this to know what is running. */
  curve: FanCurvePoint[];
  /** Every profile's stored curve, keyed by the power mode's own name.
   *  A profile with no entry uses `sharedCurve`. */
  profileCurves: Partial<Record<PowerMode, FanCurvePoint[]>>;
  /** The fallback for a profile nobody has drawn, and for a machine whose
   *  power module never announces one. */
  sharedCurve: FanCurvePoint[];
  /** Which profile `curve` came from; `null` while nothing has announced
   *  one and the shared curve is in force. */
  activeProfile: PowerMode | null;
  interpolation: "smooth" | "discrete";
  /** The sensor the curve is *set* to follow. */
  referenceSensor: FanReferenceSensor;
  /** ...and the one it is actually reading, which differs whenever the
   *  card is asleep. Null when neither sensor answers at all. */
  referenceSensorInUse: FanReferenceSensor | null;
  /** Whether this machine has a GPU sensor to offer in the first place. */
  gpuSensorAvailable: boolean;
  /** Why `capabilities.setSpeed` is what it is - so a UI can say "measured,
   *  and it does nothing here" rather than only hiding the control. */
  speedControl: FanSpeedControl;
  restoreModeOnStart: boolean;
  /** Whether a cleaning cycle owns the fans - through both transitions,
   *  not only while they are actually reversed. `fanCleanerStatus` is the
   *  detail; this is here so any page can grey out a fan control. */
  cleaning: boolean;
  fanMaxRpm: number | null;
  /** Last failure from the control loop, e.g. a write that needed root.
   *  Translatable - render with `tm()`. */
  error: Msg | null;
  saved: boolean;
  saveError: string | null;
};

/** Which fans this machine's firmware will spin backwards, and how fast
 *  it has them configured (in hundreds of RPM). */
export type FanCleanerCapabilities = {
  cpu: boolean;
  gpu: boolean;
  fan3: boolean;
  cpuSpeed: number;
  gpuSpeed: number;
  fan3Speed: number;
};

/**
 * The fan cleaner: dust removal by spinning the fans backwards.
 *
 * Three of these fields exist to keep one distinction visible, and it is
 * the same one the lightbar probe makes: **not being able to ask is not
 * being told no.** `supported` false with `answered` true is a machine
 * that has no fan cleaner; `supported` false with `unreachable` set is a
 * missing kernel module or a missing root, and showing the first sentence
 * for the second sends someone shopping for a laptop.
 */
export type FanCleanerStatus = {
  supported: boolean;
  generation: "modern" | "legacy" | null;
  capabilities: FanCleanerCapabilities;
  /** The firmware answered, whichever way. */
  answered: boolean;
  /** Set when the question could not be put. Translatable - `tm()`. */
  unreachable: Msg | null;
  acpiCallLoaded: boolean;
  acpiCallInstalled: boolean;
  /** One sentence covering whichever of the above applies. Translatable. */
  detail: Msg;
  running: boolean;
  /** Braking or ramping back down: the fans are the cleaner's, and
   *  neither button should be offered. */
  transitioning: boolean;
  secondsRemaining: number | null;
  secondsTotal: number | null;
  /** The speed the running cycle was started at, in hundreds of RPM. */
  speed: number | null;
  /** What the tachometers say right now, which does not depend on this
   *  daemon having been the one to start anything. */
  fansReversed: boolean;
  durationSecs: number;
  /** null means "use whatever the firmware has configured for itself". */
  configuredSpeed: number | null;
  maxStartTempC: number;
  cpuTempC: number | null;
  /** Why the last cycle failed. Translatable - render with `tm()`. */
  error: Msg | null;
};

/**
 * Per-key RGB over USB HID. Probed and reported, deliberately not driven:
 * a `0d62:54bf` keyboard is a different device on a different bus from the
 * lightbar, and a machine can have either, both or neither.
 */
export type RgbPerKey = {
  present: boolean;
  /** The USB id that was looked for, so a bug report says what was searched. */
  usbId: string;
  /** Always false in this build - see dev/TODO.md, "The per-key RGB keyboard path is not ported". */
  ported: boolean;
  detail: string;
};

/** The stable ids of the three ways of talking to these lights. */
export type RgbDialectId = "kernelZones" | "fourZone" | "lightbar";

/** What one dialect answered when it was asked. */
export type RgbDialectProbe = {
  id: RgbDialectId;
  /** What it talks to, in one phrase: "WMI 0x20009, command types 2/3".
   *  Translatable - render with `tm()`. */
  transport: Msg;
  /** A read through it worked. The only field that means the lights can be
   *  driven this way. */
  available: boolean;
  /** Whether anything was actually asked. False means the dialect was
   *  skipped for want of `acpi_call` or the kernel's files — which is not
   *  a refusal, and must not be shown as one. */
  asked: boolean;
  /** Translatable - render with `tm()`. */
  detail: Msg;
};

/**
 * What lighting this machine answers, and how each dialect answered.
 *
 * There is no single OMEN lighting protocol: three unrelated ways exist,
 * and which one a laptop speaks is not decided by its model name. Hence a
 * list rather than a boolean.
 *
 * Fields that are easy to conflate and must not be:
 *
 * - `present` is a claim about **hardware**: at least one dialect answered
 *   a read.
 * - a dialect with `asked: false` is **not a refusal** — the question could
 *   not be put at all.
 * - `acpiCall` vs `acpiCallInstalled`: not installed needs a package,
 *   installed-but-not-loaded needs a `modprobe`.
 * - `commandAnswers: true` with nothing available is the interesting
 *   machine: the firmware *has* a lighting command and none of the three
 *   operations this build knows is the one it wants.
 */
export type RgbLighting = {
  present: boolean;
  hpWmi: boolean;
  acpiCall: boolean;
  acpiCallInstalled: boolean;
  dialects: RgbDialectProbe[];
  commandAnswers: boolean | null;
  /** Set when the interfaces are there and nothing could be asked anyway —
   *  almost always "the daemon is not root". Carries why. Translatable -
   *  render with `tm()`. */
  unreachable: Msg | null;
  detail: string;
};

export type RgbProbe = {
  perKey: RgbPerKey;
  lighting: RgbLighting;
  /** Whether anything here can be driven at all. */
  supported: boolean;
};

/** `gpu.getStatus` - see `docs/01-ipc-protocol.md` §"`gpu` module". */
export type GpuStatus = {
  /** False on a machine with no `gpu_mux_mode` at all - no HP MUX switch,
   *  or a driver older than the patch that adds it. */
  supported: boolean;
  /** The firmware's own name for the mode, or `null` when `supported` is
   *  false. Includes `"optimus"`, which the app's three-card UI does not
   *  offer as a destination but may see reported. */
  mode: "integrated" | "hybrid" | "discrete" | "optimus" | null;
  /** The raw index `mode` was decoded from, kept for whoever needs it
   *  when `mode` is `null` because this build does not recognise it. */
  raw: number | null;
};

/**
 * `network.getStatus` - see `docs/01-ipc-protocol.md` §"`network` module".
 * System-wide smart queuing only; there is no per-application field here
 * because the daemon has no per-process traffic data to report.
 */
export type NetworkStatus = {
  /** False when `tc` is missing or no default-route interface was found. */
  supported: boolean;
  /** The interface `mode` acts on, e.g. `"wlan0"`. */
  interface: string | null;
  /** The daemon's own memory of the last `setMode` call - resets to
   *  `"off"` on restart, it is not a read of the interface (see the
   *  daemon module doc for why `fq_codel` alone proves nothing). */
  mode: "off" | "auto";
  /** What `tc qdisc show` reports right now, ours or the kernel's own
   *  default - `null` when `interface` is `null` or the read failed. */
  activeQdisc: string | null;
};

export type RgbStatus = {
  /** The probe taken at daemon startup. `rgbCapabilities()` re-asks. */
  capabilities: RgbProbe;
  /** What the user picked: `"auto"` or one dialect id. */
  dialect: "auto" | RgbDialectId;
  /** What that resolved to — null when nothing answered. Shown apart from
   *  `dialect` so an automatic pick is distinguishable from a chosen one. */
  activeDialect: RgbDialectId | null;
  /** Four `"#rrggbb"` strings. */
  zones: string[];
  /** A percentage, not a 0-255 level. */
  brightness: number;
  restoreOnStart: boolean;
  /** Whether *this daemon* put the lights where they are. False until
   *  something is written, so the colours below are never presented as
   *  the hardware's when nobody asked the hardware. */
  owned: boolean;
  /** The last write failure. Translatable - render with `tm()`. */
  error: Msg | null;
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
  /** Some lighting dialect answered a read. Still named `lightbar` on the
   *  wire, which is what the daemon calls it; the per-key keyboard is a
   *  different device on a different bus and is not counted here. */
  lightbar: boolean;
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
  /** Why that compatibility verdict. Translatable - render with `tm()`. */
  reason: Msg;
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
  /** The third rule, and the one that outranks both others: a machine
   *  over `tempHighC` steps down until it is back under `tempLowC`. */
  backOffWhenHot: boolean;
  tempHighC: number;
  tempLowC: number;
};

/** What the supervisor's thermal rule can see, and what it thinks.
 *
 *  `hot` is latched between the two thresholds, so it is not something a
 *  client could work out from `tempC` - which is exactly why the daemon
 *  reports it rather than leaving it to be inferred. */
export type ThermalState = {
  available: boolean;
  tempC: number | null;
  hot: boolean;
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
  thermal: ThermalState;
  /** Why the supervisor last moved the mode. Translatable - render with `tm()`. */
  lastAutoSwitch: Msg | null;
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
  /** Translatable - render with `tm()`. */
  title: Msg;
  status: CheckStatus;
  /** Translatable - render with `tm()`. */
  detail: Msg;
  /** Translatable - render with `tm()`. */
  remedy: Msg | null;
};

/** What became of the measurement beyond the daemon's own config. */
export type FanPinned = { ok: boolean; detail: string };

export type FanCalibration = {
  verdict: "measured" | "noReading" | "didNotRespond" | "reverse";
  fanMaxRpm: number | null;
  fan1MaxRpm: number | null;
  fan2MaxRpm: number | null;
  baselineRpm: number | null;
  startedAtMax: boolean;
  seconds: number;
  settled: boolean;
  restoredMode: string | null;
  restoreError: Msg | null;
  detail: string;
  /** `null` when the run measured nothing worth storing. */
  pinned: FanPinned | null;
};

/** One reading from a speed probe; the trace is the evidence for the verdict. */
export type FanProbeSample = { atSecs: number; rpm: number; isReverse: boolean };

/**
 * What `fan.probeSpeedControl` found. `honoured` and `ignored` are the two
 * that settle anything; the other two mean the question could not be put
 * and must not be shown as a refusal.
 */
export type FanSpeedProbe = {
  verdict: "honoured" | "ignored" | "noReading" | "noChannel";
  baselineRpm: number;
  /** Whether the probe asked for more speed or less. It aims down when the
   *  fans are already fast, so a hot machine cannot pass on its own rise. */
  aimingUp: boolean;
  targetPwm: number;
  /** What that implies in rpm, where a calibration has measured the
   *  ceiling. `null` only makes the sentence vaguer, never the verdict. */
  expectedRpm: number | null;
  /** The reading furthest in the commanded direction. */
  reachedRpm: number;
  responseRpm: number;
  seconds: number;
  samples: FanProbeSample[];
  restoredMode: string;
  restoreError: string | null;
  detail: string;
  /** The same shape every other fan write returns. */
  status: FanStatus;
};

/** Overall conclusion of the fan-control self-test. */
export type FanVerdict = "fullControl" | "monitoringOnly" | "unsupported";

export type FanDiagnosis = {
  verdict: FanVerdict;
  /** Translatable - render with `tm()`. */
  summary: Msg;
  /** Set when a driver that might help exists but isn't in use. Translatable. */
  driverNotice: Msg | null;
  checks: FanCheck[];
  wroteToHardware: boolean;
};

/* --- overclock ----------------------------------------------------------
 *
 * Mirrors `daemon/crates/overclock/src/{lib,plan,probe}.rs`. The wire
 * shapes, and the reasons this module has a consent and a confirmation
 * when no other one does, are in `docs/01-ipc-protocol.md`.
 */

/** An inclusive range the driver itself advertised. */
export type OcRange = { min: number; max: number };

/** Clocks pinned to a range inside the stock one. */
export type OcClockLock = { minMhz: number; maxMhz: number };

/** What a card was asked to run at. All-zero is stock. */
export type OcTarget = {
  coreOffsetMhz: number;
  memOffsetMhz: number;
  coreClock: OcClockLock | null;
};

export type OcVendor = "nvidia" | "amd" | "intel" | "unknown";

/**
 * One GPU and what of it can be moved. A `null` range is a knob this card
 * does not have - never a knob with a range of zero - so a UI draws a
 * slider only where there is one to draw.
 */
export type OcGpu = {
  id: string;
  name: string;
  vendor: OcVendor;
  driver: string;
  /** Whether anything here can be driven at all. */
  drivable: boolean;
  coreOffset: OcRange | null;
  memOffset: OcRange | null;
  /** The frequencies this card lists as supported; a lock stays inside them. */
  clockLock: OcRange | null;
  /** Whether the offsets can be *set*, which reading them does not answer.
   *  `null` until a probe with `allowWrites` has asked. */
  offsetsWritable: boolean | null;
  /** What can be done to this card, or why nothing can, in a sentence.
   *  Translatable - render with `tm()`. */
  detail: Msg;
  /** What a human confirmed, as opposed to what is on the card this second. */
  confirmed: OcTarget;
  applied: OcTarget | null;
};

export type OverclockState = {
  supported: boolean;
  /** Translatable - render with `tm()`. */
  detail: Msg;
  gpus: OcGpu[];
  defaultGpu: string | null;
  /** The warning, in the daemon's words. Shown as it arrives: the app does
   *  not get to reword what somebody is agreeing to. */
  consent: { text: string; version: number; accepted: boolean; acceptedAt: number | null };
  /** An applied change waiting to be confirmed. The daemon undoes it when
   *  `secondsLeft` runs out, including - especially - when the desktop that
   *  should have confirmed it is gone. */
  pending: { gpu: string; secondsLeft: number; revertsTo: OcTarget } | null;
  holdSecs: number;
  restoreOnStart: boolean;
  restoredOnStart: boolean;
  /** The last change was never confirmed, so this boot restored nothing. */
  unconfirmedAtStart: boolean;
  /** Translatable - render with `tm()`. */
  note: Msg | null;
  /** Translatable - render with `tm()`. */
  error: Msg | null;
  configPath: string;
  saved: boolean;
  saveError: string | null;
};

/** Fields left out are left alone; `clockLock: null` takes a lock off. */
export type OverclockRequest = {
  gpu?: string;
  coreOffsetMhz?: number;
  memOffsetMhz?: number;
  clockLock?: OcClockLock | null;
  holdSecs?: number;
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
  | "removeService"
  /**
   * Hand the driver the fan ceiling a calibration measured, without
   * rebuilding it. `fan.calibrate` already writes the measurement where
   * the driver reads it, so this is only needed to make it take effect
   * *now* rather than at the next load - it reloads hp-wmi.
   */
  | "pinFanCeiling";

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
  /**
   * Whether *our* patched driver is what is installed. Without it,
   * `fanControlAvailable` conflates a stock driver that supports this
   * board with a previous install of ours already doing the work.
   */
  patchedDriverInstalled: boolean;
  /** Whether one of this installer's kernel-upgrade hooks is on disk. */
  hookInstalled: boolean;
  /**
   * Whether the loaded hp-wmi understands a measured fan ceiling. A driver
   * built before that parameter existed answers false, and the remedy is
   * installing it again rather than anything the fan page can do.
   */
  driverAcceptsMeasuredRpm: boolean;
};

export type InstallerInspection = {
  environment: InstallerEnvironment;
  patchNeeded: boolean;
};

/** An empty `command` is a step the daemon carries out itself. */
export type InstallStep = {
  id: string;
  /** Translatable - render with `tm()`. */
  description: Msg;
  command: string[];
  optional: boolean;
};

/** `message` is translatable (`tm()`); `fix` is a shell command, verbatim. */
export type InstallBlocker = { id: string; message: Msg; fix: string | null };

export type InstallPlan = {
  action: InstallerAction;
  strategy: InstallStrategy | null;
  steps: InstallStep[];
  blockers: InstallBlocker[];
  /** Translatable - render each with `tm()`. */
  warnings: Msg[];
  needsRoot: boolean;
};

export type StepStatus =
  | "ok"
  | "warned"
  | "failed"
  /** Not attempted, because an earlier required step failed. */
  | "skipped"
  /** Optional, and the caller asked for it not to run. */
  | "declined"
  | "planned";

export type StepResult = {
  id: string;
  /** Translatable - render with `tm()`. */
  description: Msg;
  status: StepStatus;
  /** Planned/skipped: translatable. Real run: command output, verbatim. */
  detail: Msg;
};

export type ExecutionReport = {
  dryRun: boolean;
  succeeded: boolean;
  results: StepResult[];
};

/**
 * One `installer.progress` event: a step starting, or the same step
 * finishing.
 *
 * An install is a single IPC call that takes most of a minute, so without
 * these a UI can only show a spinner or invent a progress bar. The daemon
 * emits one of these as each step *starts* — `status: null` — and another
 * with the outcome, which is what lets a bar advance at the moment the
 * long step begins rather than when the short one before it ended.
 */
export type InstallerProgress = {
  action: InstallerAction;
  dryRun: boolean;
  /** 0-based. */
  index: number;
  total: number;
  id: string;
  /** Translatable - render with `tm()`. */
  description: Msg;
  /** `null` while the step runs; its outcome once it is over. */
  status: StepStatus | null;
  /** Only on the second event: command output, or the error. */
  detail: Msg | null;
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
  /**
   * Fill in anything left unset below from what the machine says about
   * itself - the board id from DMI, the tables from the driver's own
   * source, the fan ceilings from the last calibration. Explicit values
   * still win, so the manual fields override the detection rather than
   * competing with it.
   */
  auto?: boolean;
  cpuMaxRpm?: number | null;
  gpuMaxRpm?: number | null;
  experimentalBoard?: string | null;
  boardTable?: BoardTable | null;
  /**
   * Ids of steps not to run. Only steps the plan marked `optional` may be
   * named — `apply` refuses anything else rather than ignoring it, so a
   * caller cannot believe it opted out of `depmod`.
   */
  skipSteps?: string[];
};

/** Which HP gaming family DMI says this is; decides the board params. */
export type BoardFamily = "omen" | "victus" | "unknown";

/** Where a suggested fan ceiling came from. */
export type RpmSource = "calibrated" | "driverFallback";

/**
 * Everything the install would otherwise have asked the user to type,
 * worked out from the machine - plus `notes`, the reasoning behind each
 * answer, which is the part that makes it reviewable rather than magic.
 *
 * Mirrors `daemon/crates/installer/src/autodetect.rs`.
 */
export type Autodetected = {
  dmi: {
    boardName: string | null;
    productName: string | null;
    productFamily: string | null;
    sysVendor: string | null;
  };
  family: BoardFamily;
  /** The driver already lists this board, so nothing needs injecting. */
  boardKnown: boolean;
  experimentalBoard: string | null;
  boardTable: BoardTable | null;
  /**
   * Whether the board-params variant changes anything on this board. All
   * four share one fan profile, and a board already on the OMEN or Victus
   * thermal-profile path never has its variant's EC offset read — so on
   * most boards the choice is inert, and saying so beats a caveat.
   */
  paramsEffect: "inertOmenPath" | "inertVictusPath" | "decidesReadback";
  /** What the embedded controller said, when it was asked. */
  ec:
    | { state: "read"; victusS: number; omen: number }
    | { state: "moduleNotLoaded" }
    | { state: "unavailable"; reason: string }
    | { state: "notPermitted" }
    | { state: "notProbed" };
  cpuMaxRpm: number | null;
  gpuMaxRpm: number | null;
  rpmSource: RpmSource;
  /** Translatable - render each with `tm()`. */
  notes: Msg[];
};

export type ApplyResult = {
  plan: InstallPlan;
  report: ExecutionReport;
  /** Only present for an `auto` request: what it decided to send. */
  autodetected?: Autodetected | null;
};

export class DaemonUnavailable extends Error {}

/**
 * A refusal the daemon sent back: `{ kind, message }`, and - when the
 * sentence is in the translation catalog - `key` / `params` so the UI can
 * show it in the user's language. `.message` is always the English text.
 * Use `errorText()` to render one (translated where possible).
 */
export class DaemonRefusal extends Error {
  kind?: string;
  key?: string;
  params?: Record<string, string | number | unknown>;
  constructor(fields: {
    message: string;
    kind?: string;
    key?: string;
    params?: Record<string, string | number | unknown>;
  }) {
    super(fields.message);
    this.name = "DaemonRefusal";
    this.kind = fields.kind;
    this.key = fields.key;
    this.params = fields.params;
  }
}

/**
 * Text for a caught error, in the user's language where the daemon gave us
 * a catalog key. Anything that is not a `DaemonRefusal` (a transport
 * failure, a thrown string) falls back to its own string form.
 */
export function errorText(e: unknown): string {
  if (e instanceof DaemonRefusal && e.key) {
    return tm({ key: e.key, params: e.params, text: e.message });
  }
  if (e instanceof Error) return e.message;
  return String(e);
}

/**
 * The Tauri bridge forwards a keyed refusal as a JSON string (see
 * `daemon_error` in `src-tauri`). Parse it back, or `null` if this is just
 * a plain message.
 */
function parseRefusal(raw: string): DaemonRefusal | null {
  if (!raw.startsWith("{")) return null;
  try {
    const o = JSON.parse(raw) as Record<string, unknown>;
    if (o && typeof o === "object" && typeof o.message === "string" && typeof o.key === "string") {
      return new DaemonRefusal({
        message: o.message,
        kind: typeof o.kind === "string" ? o.kind : undefined,
        key: o.key,
        params: (o.params as Record<string, unknown>) ?? undefined,
      });
    }
  } catch {
    /* not JSON - a plain message that happened to start with "{" */
  }
  return null;
}

/**
 * One thing that happened to the daemon, forwarded by the Tauri shell.
 *
 * Mirrors `core.nextEvent`'s event shape in docs/01-ipc-protocol.md.
 * `payload` is deliberately loose: an unknown `topic` has to reach the
 * handler rather than be filtered out, because a newer daemon may publish
 * one this build has never heard of.
 */
export type DaemonEvent = {
  seq: number;
  topic: string;
  payload: Record<string, unknown>;
  /** How long ago it happened, in milliseconds. */
  ageMs: number;
};

/**
 * Calls `handler` whenever the daemon publishes something, and returns the
 * function that stops listening.
 *
 * This is how the window learns about changes it did not make: the
 * laptop's performance key, the on-screen display, `pyren-ctl`, and the
 * daemon's own auto-switch supervisor all move the machine without asking
 * this app first.
 *
 * **A no-op outside Tauri.** The dev-server bridge is request/response
 * only, so `vite dev` in a browser tab still shows correct data on every
 * poll - it just does not react to a key press. Nothing else changes.
 */
export function onDaemonEvent(handler: (event: DaemonEvent) => void): () => void {
  if (!inTauri) return () => {};

  // Subscribing is async and callers unsubscribe synchronously (an
  // `onMount` cleanup), so the unlisten is captured when it arrives and
  // applied immediately if the caller has already gone.
  let unlisten: (() => void) | null = null;
  let stopped = false;

  void listen<DaemonEvent>("daemon-event", (message) => handler(message.payload))
    .then((stop) => {
      if (stopped) stop();
      else unlisten = stop;
    })
    .catch(() => {
      /* No event stream: the page still polls, it just will not react. */
    });

  return () => {
    stopped = true;
    unlisten?.();
  };
}

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
  fan_calibrate: { module: "fan", method: "calibrate" },
  fan_probe_speed_control: { module: "fan", method: "probeSpeedControl" },
  fan_set_mode: { module: "fan", method: "setMode" },
  fan_set_curve: { module: "fan", method: "setCurve" },
  fan_set_restore_on_start: { module: "fan", method: "setRestoreOnStart" },
  fan_cleaner_status: { module: "fan", method: "cleanerStatus" },
  fan_start_cleaning: { module: "fan", method: "startCleaning" },
  fan_stop_cleaning: { module: "fan", method: "stopCleaning" },
  fan_set_cleaner_config: { module: "fan", method: "setCleanerConfig" },
  power_get_state: { module: "power", method: "getState" },
  power_set_mode: { module: "power", method: "setMode" },
  power_set_auto_config: { module: "power", method: "setAutoConfig", params: (a) => a.config },
  power_set_restore_on_start: { module: "power", method: "setRestoreOnStart" },
  power_set_tuning: { module: "power", method: "setTuning", params: (a) => a.tuning },
  power_set_apply_to_os_profile: { module: "power", method: "setApplyToOsProfile" },
  overclock_get_state: { module: "overclock", method: "getState" },
  overclock_probe: { module: "overclock", method: "probe" },
  overclock_set_consent: { module: "overclock", method: "setConsent" },
  overclock_apply: { module: "overclock", method: "apply", params: (a) => a.request },
  overclock_confirm: { module: "overclock", method: "confirm" },
  overclock_cancel: { module: "overclock", method: "cancel" },
  overclock_reset: { module: "overclock", method: "reset" },
  overclock_set_restore_on_start: { module: "overclock", method: "setRestoreOnStart" },
  rgb_get_status: { module: "rgb", method: "getStatus" },
  rgb_get_capabilities: { module: "rgb", method: "getCapabilities" },
  gpu_get_status: { module: "gpu", method: "getStatus" },
  gpu_set_mode: { module: "gpu", method: "setMode" },
  network_get_status: { module: "network", method: "getStatus" },
  network_set_mode: { module: "network", method: "setMode" },
  keymap_get_status: { module: "keymap", method: "getStatus" },
  keymap_set_mapping: { module: "keymap", method: "setMapping", params: (a) => a.mapping },
  keymap_remove_mapping: { module: "keymap", method: "removeMapping", params: (a) => a.from },
  keymap_set_enabled: { module: "keymap", method: "setEnabled" },
  rgb_set_static: { module: "rgb", method: "setStatic" },
  rgb_set_zones: { module: "rgb", method: "setZones" },
  rgb_off: { module: "rgb", method: "off" },
  rgb_read_zones: { module: "rgb", method: "readZones" },
  rgb_set_dialect: { module: "rgb", method: "setDialect" },
  rgb_set_restore_on_start: { module: "rgb", method: "setRestoreOnStart" },
  installer_inspect: { module: "installer", method: "inspect" },
  installer_autodetect: {
    module: "installer",
    method: "autodetect",
    params: (a) => a.request,
  },
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
  // `{ kind, message, key?, params? }` - see `docs/01-ipc-protocol.md` -
  // while the bridge reports its own failures (no daemon, timeout) as a
  // plain string.
  const reply = (await response.json().catch(() => null)) as
    | {
        result?: T;
        error?:
          | string
          | { kind?: string; message?: string; key?: string; params?: Record<string, unknown> };
      }
    | null;
  if (!reply) throw new DaemonUnavailable("malformed reply from the dev bridge");
  if (reply.error !== undefined && reply.error !== null) {
    if (typeof reply.error === "string") throw new DaemonUnavailable(reply.error);
    const message = reply.error.message ?? "the daemon refused without saying why";
    if (reply.error.key) {
      throw new DaemonRefusal({
        message,
        kind: reply.error.kind,
        key: reply.error.key,
        params: reply.error.params,
      });
    }
    throw new DaemonUnavailable(message);
  }
  return reply.result as T;
}

/** Which modifiers a shortcut needs held. Matched exactly by the daemon. */
export type HotkeyModifiers = {
  ctrl: boolean;
  shift: boolean;
  alt: boolean;
  meta: boolean;
};

/**
 * One bound key. `keycode` and `scancode` are both optional and at least
 * one is set: a vendor key the kernel has no keycode for — the OMEN
 * performance key on the machines where it arrives at all — is bound by
 * its scancode alone.
 */
export type HotkeyTrigger = {
  /** Device name as the kernel reports it, e.g. `HP WMI hotkeys`. */
  device: string | null;
  keycode: number | null;
  scancode: number | null;
  modifiers: HotkeyModifiers;
};

export type HotkeyStatus = {
  /** Whether a matched key does anything. The watcher runs either way, so
   *  `learn` still works while this is off. */
  enabled: boolean;
  /** Whether the daemon can hear a key at all. False when it is not root. */
  watching: boolean;
  /** One sentence saying why nothing happens, when nothing does.
   *  Translatable - render with `tm()`. */
  detail: Msg;
  devices: string[];
  triggers: HotkeyTrigger[];
  /** The shortcut written the way a person would: `Ctrl+Alt+P`. Null when
   *  nothing is bound. */
  label: string | null;
  repeatGuardMs: number;
  learning: boolean;
  fired: number;
  lastFiredAgoMs: number | null;
  configPath: string;
  configSaveError: string | null;
};

/** What `hotkey.learn` answers with. `timedOut` is the answer that matters
 *  on a laptop whose key never reaches Linux: nothing was pressed, and
 *  that is a result rather than a failure. */
export type HotkeyLearned = {
  press: {
    device: string;
    keycode: number | null;
    scancode: number | null;
    modifiers: HotkeyModifiers;
    describe: string;
    label: string;
  } | null;
  timedOut: boolean;
  bound: boolean;
};

/** One key remapped to another. `device` scopes the source key to one
 *  named keyboard; omitted, it matches that keycode on any of them.
 *  Keycodes are Linux evdev codes, the same vocabulary `HotkeyTrigger`
 *  already uses - see `keycodeFromDomCode` / `domCodeFromKeycode`. */
export type KeyMapping = {
  from: { device: string | null; keycode: number };
  to: number;
};

export type KeymapStatus = {
  /** Whether the remapper is meant to be running. Off by default, unlike
   *  `hotkey.enabled`: turning this on grabs a keyboard exclusively, which
   *  silences `hotkey` on it for as long as it runs. */
  enabled: boolean;
  /** Whether it is actually grabbing devices right now. Can be false while
   *  `enabled` is true - not root, or no keyboard found. */
  running: boolean;
  /** Translatable - render with `tm()`. */
  detail: Msg;
  devices: string[];
  mappings: KeyMapping[];
  configPath: string;
  configSaveError: string | null;
};

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!inTauri) return callViaDevBridge<T>(command, args);
  try {
    return await invoke<T>(command, args);
  } catch (e) {
    // A Tauri command's error is a string; a keyed daemon refusal arrives
    // as JSON inside it (see `daemon_error` in src-tauri).
    const raw = String(e);
    throw parseRefusal(raw) ?? new DaemonUnavailable(raw);
  }
}

export const daemon = {
  capabilities: () => call<ModuleCapability[]>("core_capabilities"),
  systemInfo: () => call<SystemInfo>("system_get_info"),
  systemMetrics: () => call<SystemMetrics>("system_get_metrics"),
  fanStatus: () => call<FanStatus>("fan_get_status"),
  /** `allowWrites` opts into the one check that touches hardware. */
  fanDiagnose: (allowWrites = false) => call<FanDiagnosis>("fan_diagnose", { allowWrites }),
  /**
   * Runs the fans flat out and measures what full speed is here, then puts
   * the mode back. **Loud, and it blocks** for up to `seconds` - the
   * daemon stops early once the reading settles, so it is usually under
   * twenty. The measurement is also handed to the driver, which is the
   * only way a calibration reaches the pwm/rpm conversion at all; see
   * `pinned` in the reply.
   */
  calibrateFans: (seconds?: number) => call<FanCalibration>("fan_calibrate", { seconds }),
  /**
   * Holds the fans at a speed they are not at and watches whether they
   * follow — the only way to tell a driver that honours `pwm1` from one
   * that takes the value and ignores it. **Audible, and it blocks**, though
   * the daemon stops as soon as the fans answer, so a machine that obeys is
   * done in a few seconds. The answer is remembered: a machine that ignores
   * it reports `setSpeed: false` from then on.
   */
  probeFanSpeedControl: (seconds?: number) =>
    call<FanSpeedProbe>("fan_probe_speed_control", { seconds }),
  /** `pwm` (0-255) is required for `manual` and ignored otherwise. */
  setFanMode: (mode: FanDaemonMode, pwm?: number) =>
    call<FanStatus>("fan_set_mode", { mode, pwm }),
  /**
   * Stores a curve; it only drives the fans while the mode is `curve`.
   *
   * Each power profile has its own. `profile` names which one to write —
   * omitted, it writes whichever profile the machine is in, so a caller
   * that ignores profiles edits the curve it can see running. `""` writes
   * the shared fallback.
   */
  setFanCurve: (
    curve: FanCurvePoint[],
    interpolation?: "smooth" | "discrete",
    referenceSensor?: FanReferenceSensor,
    profile?: PowerMode | "",
  ) => call<FanStatus>("fan_set_curve", { curve, interpolation, referenceSensor, profile }),
  setFanRestoreOnStart: (enabled: boolean) =>
    call<FanStatus>("fan_set_restore_on_start", { enabled }),
  /** `refresh` re-asks the firmware what it can do (two ACPI calls); the
   *  polling read leaves it off and uses the daemon's cached answer. */
  fanCleanerStatus: (refresh = false) =>
    call<FanCleanerStatus>("fan_cleaner_status", { refresh }),
  /** Blocks for a few seconds while the blades are braked, then returns
   *  with the countdown running. The daemon ends the cycle on its own.
   *  `force` skips the "this machine has no fan cleaner" refusal, for
   *  firmware whose capability reply this build reads wrongly. */
  startFanCleaning: (options: { speed?: number; seconds?: number; force?: boolean } = {}) =>
    call<FanCleanerStatus>("fan_start_cleaning", {
      speed: options.speed ?? null,
      seconds: options.seconds ?? null,
      force: options.force ?? false,
    }),
  stopFanCleaning: () => call<FanCleanerStatus>("fan_stop_cleaning"),
  /** The remembered duration and speed. `speed: null` goes back to the
   *  firmware's own, which is not the same as leaving it unset. */
  setFanCleanerConfig: (config: { seconds?: number; speed?: number | null }) =>
    call<FanCleanerStatus>("fan_set_cleaner_config", {
      seconds: config.seconds ?? null,
      speed: config.speed === undefined ? undefined : config.speed,
    }),
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
  /** Every GPU, what can be tuned on it, and what is set right now. */
  overclockState: () => call<OverclockState>("overclock_get_state"),
  /** `allowWrites` opts into the one check that touches the hardware:
   *  whether an offset can be set, asked by writing back the current one. */
  overclockProbe: (allowWrites = false) =>
    call<OverclockState>("overclock_probe", { allowWrites }),
  /** Accepting the warning is what unlocks `applyOverclock`, and nothing
   *  else; withdrawing it puts every card back to stock. */
  setOverclockConsent: (accepted: boolean) =>
    call<OverclockState>("overclock_set_consent", { accepted }),
  /** Applies in steps and arms the daemon's revert timer. The reply's
   *  `pending` is the countdown a UI has to show - and act on, because
   *  saying nothing means the change is undone. */
  applyOverclock: (request: OverclockRequest) =>
    call<OverclockState>("overclock_apply", { request }),
  /** Keeps what was just applied. */
  confirmOverclock: () => call<OverclockState>("overclock_confirm"),
  /** Undoes it now rather than at the end of the countdown - the same
   *  revert the daemon would do on its own. */
  cancelOverclock: () => call<OverclockState>("overclock_cancel"),
  /** Back to the clocks the firmware shipped. Never refused. */
  resetOverclock: (gpu?: string) => call<OverclockState>("overclock_reset", { gpu }),
  setOverclockRestoreOnStart: (enabled: boolean) =>
    call<OverclockState>("overclock_set_restore_on_start", { enabled }),
  /** Which GPU is driving the screen right now. */
  gpuStatus: () => call<GpuStatus>("gpu_get_status"),
  /** Switches the MUX. Needs a logout or reboot to take effect - the
   *  daemon does not do that itself, and neither does this call. */
  setGpuMode: (mode: "integrated" | "hybrid" | "discrete" | "optimus") =>
    call<GpuStatus>("gpu_set_mode", { mode }),
  /** The default-route interface, this daemon's remembered mode, and the
   *  qdisc actually active right now. */
  networkStatus: () => call<NetworkStatus>("network_get_status"),
  /** `off` deletes the root qdisc; `auto` hands the interface `cake` (or
   *  `fq_codel` as a fallback) - system-wide, not per-application. */
  setNetworkMode: (mode: "off" | "auto") => call<NetworkStatus>("network_set_mode", { mode }),
  /** The lightbar: the startup probe plus what this daemon last set. */
  rgbStatus: () => call<RgbStatus>("rgb_get_status"),
  /** Re-probes both lighting paths. Costs an ACPI round trip, so it is
   *  the answer to "I just installed acpi_call", not a poll. */
  rgbCapabilities: () => call<RgbProbe>("rgb_get_capabilities"),
  /** All four zones one colour. `brightness` is a percentage; leaving it
   *  out keeps the stored one. */
  setRgbStatic: (color: string, brightness?: number) =>
    call<RgbStatus>("rgb_set_static", { color, brightness }),
  /** One colour per zone, in zone order. */
  setRgbZones: (zones: string[], brightness?: number) =>
    call<RgbStatus>("rgb_set_zones", { zones, brightness }),
  /** Black *and* brightness 0: on some firmwares either alone leaves a
   *  dim glow, and "off" should mean off. */
  rgbOff: () => call<RgbStatus>("rgb_off"),
  /** Asks the firmware what the zones are - and the only check that the
   *  payload was understood, not just accepted. Answers which dialect
   *  answered, because that is the interesting half. */
  rgbReadZones: () => call<{ zones: string[]; dialect: RgbDialectId }>("rgb_read_zones"),
  /** Pins a dialect, or `"auto"` to go back to picking the first that
   *  answers. A pinned dialect is used whether or not it probed: the user
   *  can see the lights and this build cannot. */
  setRgbDialect: (dialect: "auto" | RgbDialectId) =>
    call<RgbStatus>("rgb_set_dialect", { dialect }),
  setRgbRestoreOnStart: (enabled: boolean) =>
    call<RgbStatus>("rgb_set_restore_on_start", { enabled }),
  /** What this machine has, and whether the patched driver is needed. */
  installerInspect: () => call<InstallerInspection>("installer_inspect"),
  /** Reads DMI, the driver's tables and the fan config; changes nothing. */
  installerAutodetect: (probeEc = false) =>
    call<Autodetected>("installer_autodetect", { request: { probeEc } }),
  /** Pure: works out the steps without touching anything. */
  installerPlan: (request: InstallerRequest) => call<InstallPlan>("installer_plan", { request }),
  /** A dry run unless the request carries `confirm: true`. */
  installerApply: (request: InstallerRequest) => call<ApplyResult>("installer_apply", { request }),
  /** What key is bound, and whether the daemon can hear one at all. */
  hotkeyStatus: () => call<HotkeyStatus>("hotkey_get_status"),
  /** Opens a learn window and does not answer until a key arrives or the
   *  window closes. Whatever arrives is bound. */
  hotkeyLearn: (timeoutMs = 10_000) => call<HotkeyLearned>("hotkey_learn", { timeoutMs }),
  /** Forgets the bound key without switching the hotkey off. */
  hotkeyClear: () => call<HotkeyStatus>("hotkey_clear"),
  setHotkeyEnabled: (enabled: boolean) => call<HotkeyStatus>("hotkey_set_enabled", { enabled }),
  /** Does what the key does, without the key — the widget's preview. */
  hotkeyPress: () => call<{ fired: boolean }>("hotkey_press"),
  setPowerTuning: (tuning: {
    mode?: PowerMode;
    pl1W?: number;
    pl2W?: number;
    turbo?: boolean;
  }) => call<PowerState>("power_set_tuning", { tuning }),
  /** The mappings, and whether the remapper is actually grabbing devices. */
  keymapStatus: () => call<KeymapStatus>("keymap_get_status"),
  /** Replaces whatever this key (and device, if given) already mapped to. */
  setKeyMapping: (mapping: KeyMapping) => call<KeymapStatus>("keymap_set_mapping", { mapping }),
  removeKeyMapping: (from: { device?: string | null; keycode: number }) =>
    call<KeymapStatus>("keymap_remove_mapping", { from }),
  /** Grabs (or releases) the keyboards a mapping names. Turning this on is
   *  the one call here that can take a keyboard away from the rest of the
   *  session if the mapping is wrong - see `docs/01-ipc-protocol.md`
   *  §"`keymap` module". */
  setKeymapEnabled: (enabled: boolean) => call<KeymapStatus>("keymap_set_enabled", { enabled }),
};
