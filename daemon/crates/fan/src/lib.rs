//! Fan control module - ported from the `omen-fan-control` Python project
//! (see `../omen-fan-control-main/docs/` in the workspace for the full
//! behavioral spec this is ported from).
//!
//! | method | params | result |
//! |---|---|---|
//! | `fan.getStatus` | none | temperature, RPM, mode, what this machine can do |
//! | `fan.diagnose` | `{ "allowWrites": bool }` | the self-test, see [`diagnostics`] |
//! | `fan.setMode` | `{ "mode": "auto"\|"max"\|"manual"\|"curve", "pwm"?: 0-255 }` | the new status |
//! | `fan.setCurve` | `{ "curve": [{ "tempC": n, "percent": n }], "interpolation"?: "smooth"\|"discrete", "referenceSensor"?: "cpu"\|"gpu" }` | the new status |
//! | `fan.setRestoreOnStart` | `{ "enabled": bool }` | the new status |
//! | `fan.setKeepDriverFloor` | `{ "enabled": bool }` | the new status |
//! | `fan.setThermalSafetyChecker` | `{ "enabled": bool }` | the new status |
//! | `fan.setSensorFailureAction` | `{ "action": "max"\|"auto" }` | the new status |
//! | `fan.clearFloorNotices` | none | the new status |
//! | `fan.calibrate` | `{ "seconds"?: 10-120 }` | what full speed measured, see [`calibration`] |
//! | `fan.cleanerStatus` | `{ "refresh"?: bool }` | what the fan cleaner can do here, see [`cleaner`] |
//! | `fan.startCleaning` | `{ "speed"?: 10-39, "seconds"?: 5-60, "force"?: bool }` | the cleaner status |
//! | `fan.stopCleaning` | none | the cleaner status |
//!
//! What a given machine will accept is not the same everywhere, and the
//! difference is not cosmetic - see [`control`] for the `pwm1` /
//! `pwm1_enable` split. `getStatus` reports it as `capabilities` so the UI
//! can hide a slider that would do nothing.
//!
//! The fan cleaner ([`cleaner`]) lives here rather than in a module of its
//! own for one reason: it and the control loop drive the same fans. A
//! cycle has to be able to stop the loop writing `pwm1` underneath it, and
//! putting the two in different modules would mean one calling the other -
//! which this project's modules never do.

// The status object is one `json!` literal, past the macro's default depth.
#![recursion_limit = "256"]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pyren_config::{ConfigStore, LoadOutcome};
use pyren_core::{acpi, msg, ErrorKind, Module, ModuleError, ModuleResult, Msg};
use pyren_core::{log_info, log_warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod calibration;
pub mod cleaner;
// Private because its functions take the crate-private `FanPaths`; the
// types callers need are re-exported below.
mod control;
pub mod curve;
pub mod diagnostics;
pub mod safety;
pub mod speed_probe;
pub mod stall;

pub use calibration::Calibration;
pub use cleaner::Cycle;
pub use control::{Capabilities, FanMode};
pub use curve::{CurvePoint, Interpolation};
pub use speed_probe::{SpeedControl, SpeedProbe};

const HWMON_ROOT: &str = "/sys/devices/platform/hp-wmi/hwmon";

/// How often the control loop looks at the temperature. The original uses
/// two seconds; nothing here is cheaper for being slower, and a curve that
/// reacts a tick late is a curve the user can hear lagging.
const TICK: Duration = Duration::from_secs(2);

/// Bounds on `ma_window`. Fifteen samples is half a minute of smoothing;
/// a larger window from a hand-edited file would have the curve answer a
/// load that started minutes ago.
pub const MIN_MA_WINDOW: usize = 1;
pub const MAX_MA_WINDOW: usize = 15;

/// Highest rpm a stored measurement may claim. No laptop fan turns this
/// fast, and the products the curve arithmetic takes of these values must
/// stay far from overflowing.
pub const MAX_PLAUSIBLE_RPM: i64 = 10_000;

/// How often the "hot" thresholds are re-read from their owner. They are a
/// setting, not a sensor: half a minute behind a change is fine.
const HEAT_REFRESH_SECS: u64 = 30;

/// A pass this many ticks after the last one means the machine was asleep
/// or stalled, and what was last written can no longer be trusted.
const LATE_TICKS: u32 = 3;

/// Consecutive panicking passes after which the loop stops driving the fans.
const PANICS_BEFORE_STANDING_DOWN: u32 = 3;

/// Where the fan module learns what "hot" means: the power supervisor's
/// "hot at" and "cooled below" settings, as `(hot_c, cool_c)`. Handed over
/// by the daemon binary, the one place allowed to know both modules - see
/// [`FanModule::set_heat_source`]. `None` from it keeps the defaults.
pub type HeatSource = Box<dyn Fn() -> Option<(f64, f64)> + Send + Sync>;

/// Sysfs paths discovered for this machine. Any of these can be `None` if
/// the patched hp-wmi driver isn't installed, or if no supported CPU temp
/// sensor was found - callers must handle that, not assume presence.
#[derive(Debug, Clone, Default)]
pub(crate) struct FanPaths {
    pub(crate) hwmon_dir: Option<PathBuf>,
    pub(crate) pwm1: Option<PathBuf>,
    /// The second fan's setpoint. Optional even with the driver loaded:
    /// only `pwm1` is required for speed control, so capabilities are
    /// never derived from this one.
    pub(crate) pwm2: Option<PathBuf>,
    pub(crate) pwm1_enable: Option<PathBuf>,
    pub(crate) fan1_input: Option<PathBuf>,
    pub(crate) fan2_input: Option<PathBuf>,
    pub(crate) cpu_temp: Option<PathBuf>,
    /// The discrete GPU's own sensor, when it has one that hwmon
    /// publishes. `None` is the common case rather than a fault: an
    /// integrated-only machine has nothing here, and so does one whose
    /// card is powered down at discovery time.
    pub(crate) gpu_temp: Option<PathBuf>,
    /// `hp_wmi`'s module parameters, where Pyren's driver reports the fan
    /// table's floor and takes a replacement for it. See `control`.
    pub(crate) driver_params: Option<PathBuf>,
}

/// Which temperature the curve follows.
///
/// The original supports both, and the reason is not preference: on a
/// laptop the GPU is the part that gets hot first under a game, and a
/// CPU-only curve spins up after the heat has already spread. The reason
/// it is not simply "the hotter of the two" is the fallback below - a
/// sleeping card reads 0, and taking the maximum of a real number and a
/// meaningless one is fine, but taking a *curve point* from a sleeping
/// card is not, and the user should be able to see which sensor they are
/// driving from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReferenceSensor {
    #[default]
    Cpu,
    Gpu,
}

impl ReferenceSensor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cpu" => Some(Self::Cpu),
            "gpu" => Some(Self::Gpu),
            _ => None,
        }
    }
}

/// A record that the stall watch raised Pyren's floor because the fans
/// kept stalling at it. Persisted so the app can surface it whenever it
/// gains a way to - the daemon does not have one yet - and kept to a
/// handful ([`FLOOR_NOTICE_CAP`]), newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorNotice {
    /// Wall-clock seconds. This outlives the process, so a monotonic value
    /// would mean nothing after a restart; the reader turns it into an age.
    pub at_unix_secs: u64,
    /// Pyren's floor before and after the raise, in rpm.
    pub raised_from_rpm: i64,
    pub raised_to_rpm: i64,
    /// How many stalls in the half-hour window triggered it.
    pub stalls: usize,
    /// True when the raise brought Pyren's floor up to the driver's own,
    /// so there is nothing lower to fall back to and a real recalibration
    /// is the next step.
    pub reached_driver_floor: bool,
}

/// How many [`FloorNotice`]s are kept.
pub const FLOOR_NOTICE_CAP: usize = 5;

/// What is persisted to `fan.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FanConfig {
    pub mode: FanMode,
    /// 0-255, used when `mode` is `manual`. 128 is the driver's own
    /// default and a sane half-speed to land on.
    pub manual_pwm: u8,
    /// The curve used when no profile-specific one applies: a daemon that
    /// has never heard a `power.mode`, an unknown profile name, or a
    /// profile the user has not drawn yet. Also what
    /// [`FanConfig::migrate_profile_curves`] seeds the per-profile ones
    /// from, so an upgrade keeps the shape that was already there.
    pub curve: Vec<CurvePoint>,
    /// One curve per power profile, keyed by the profile's own name.
    ///
    /// **The key is opaque here on purpose.** The fan module must not learn
    /// what profiles exist - `eco`/`balanced`/... is the power module's
    /// vocabulary, and a table of them in this crate would be the two
    /// modules knowing about each other by another route. What arrives is
    /// whatever `power.mode` announced; an unrecognised name simply has no
    /// curve and falls back to [`FanConfig::curve`].
    pub profile_curves: BTreeMap<String, Vec<CurvePoint>>,
    pub interpolation: Interpolation,
    /// Which sensor the curve reads. See [`ReferenceSensor`]; a `gpu`
    /// setting falls back to the CPU rather than failing, because the
    /// card being asleep is normal and stopping the fan curve while it is
    /// would be worse than following the other sensor.
    pub reference_sensor: ReferenceSensor,
    /// Samples of temperature smoothing, ~2 s apart.
    pub ma_window: usize,
    /// Full-speed RPM as measured by `fan.calibrate`, of whichever fan
    /// reads faster - the form the hysteresis compares against. Only
    /// sharpens it; everything works without it.
    pub fan_max_rpm: Option<i64>,
    /// The same measurement per fan, which is the form the driver's
    /// `OMEN_CPU_MAX_RPM` / `OMEN_GPU_MAX_RPM` constants want. Kept
    /// because the installer can patch them and nothing else produces
    /// the numbers.
    pub fan1_max_rpm: Option<i64>,
    pub fan2_max_rpm: Option<i64>,
    /// The slowest the fans hold when commanded the minimum, measured by
    /// the same `fan.calibrate` - which, with the upstream driver's clamp
    /// in place, is that clamp. Below the floor in force a curve or a
    /// manual speed hands the fans to the firmware, which is the only thing
    /// that stops them - see [`curve::stop_below_pwm`]. A driver that
    /// reports its floor (`control::read_driver_floor`) is believed over
    /// this; it is kept for one that does not.
    pub fan_min_rpm: Option<i64>,
    /// The slowest speed `fan.calibrate` found the fans holding - no
    /// stall, no kick - with the driver's clamp lifted, the step below it
    /// having failed (see `calibration::sweep_floor`). Pyren's floor is a
    /// step above it ([`FLOOR_MARGIN_RPM`]): 600 held on board 8D2F, whose
    /// fan table says 1800, so 700.
    pub fan_stable_min_rpm: Option<i64>,
    /// Keep the upstream driver's floor rather than Pyren's. On by default:
    /// the table's floor is the vendor's choice, and running below it is
    /// something to opt into.
    pub keep_driver_floor: bool,
    /// Times the stall watch has raised Pyren's floor because the fans kept
    /// stalling at it, newest first. The app reads these from `getStatus`
    /// and shows them in its notification history (the `fan.floorRaised`
    /// event carries the same thing live). See [`FloorNotice`], [`stall`].
    pub fan_floor_notices: Vec<FloorNotice>,
    /// Whether a commanded speed was ever found to reach the fans.
    ///
    /// `pwm1` existing does not mean the embedded controller honours it —
    /// board `8D2F` takes the write, reports it back as the *measured*
    /// speed, and goes on running its own curve. Only
    /// [`speed_probe`] can tell the two apart, and only by spinning the
    /// fans, so this remembers the answer rather than re-asking. Defaults
    /// to `Untested`, which offers speed control: most machines that expose
    /// `pwm1` do honour it, and refusing on suspicion would be worse than
    /// an occasional slider that turns out to do nothing.
    pub speed_control: SpeedControl,
    /// Off by default, like the power module's equivalent: putting a
    /// machine's fans somewhere the user last left them, at boot, before
    /// they have asked for anything, is not a decision this should make on
    /// its own.
    pub restore_mode_on_start: bool,
    /// How long a fan-cleaning cycle runs, in seconds. Remembered because
    /// it is a preference, not a parameter of one run.
    pub cleaner_duration_secs: u64,
    /// The reverse speed to command, in hundreds of RPM. `None` - the
    /// default - uses whatever the firmware has configured for itself,
    /// which is the number the vendor's own tool would send.
    pub cleaner_speed: Option<u8>,
    /// The thermal safety checker. When the machine is hot - by the power
    /// supervisor's own "hot at" setting - and the fans do not speed up to
    /// meet it, they are handed to the firmware; if the firmware does not
    /// speed them up either, they run at full speed until the machine has
    /// cooled, and then whatever was in force is put back. On by default:
    /// turning it off is choosing to trust a setting over the temperature.
    /// See [`safety::ThermalChecker`].
    pub thermal_safety_checker: bool,
    /// Where the fans go while a curve, or a slow manual speed, has lost its
    /// temperature readings. See [`SensorFailureAction`].
    pub sensor_failure_action: SensorFailureAction,
}

impl Default for FanConfig {
    fn default() -> Self {
        Self {
            mode: FanMode::Auto,
            manual_pwm: 128,
            curve: Vec::new(),
            profile_curves: BTreeMap::new(),
            interpolation: Interpolation::default(),
            reference_sensor: ReferenceSensor::default(),
            ma_window: 5,
            fan_max_rpm: None,
            fan1_max_rpm: None,
            fan2_max_rpm: None,
            fan_min_rpm: None,
            fan_stable_min_rpm: None,
            keep_driver_floor: true,
            fan_floor_notices: Vec::new(),
            speed_control: SpeedControl::default(),
            restore_mode_on_start: false,
            cleaner_duration_secs: cleaner::DEFAULT_DURATION_SECS,
            cleaner_speed: None,
            thermal_safety_checker: true,
            sensor_failure_action: SensorFailureAction::default(),
        }
    }
}

/// What a lost temperature reading hands the fans to.
///
/// Full speed is the default because it does not depend on anything still
/// working: loud, but never hot. Auto hands them to the firmware, which
/// reads its own sensors and is quiet - and trusts that the firmware's
/// thermal control is sound on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SensorFailureAction {
    #[default]
    Max,
    Auto,
}

impl SensorFailureAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Max => "max",
            Self::Auto => "auto",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "max" => Some(Self::Max),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    fn mode(self) -> FanMode {
        match self {
            Self::Max => FanMode::Max,
            Self::Auto => FanMode::Auto,
        }
    }
}

impl FanConfig {
    /// Brings a config read from disk inside the bounds a request would be
    /// held to, and says what it changed.
    ///
    /// A file is edited by hand, written by an older build, or damaged, and
    /// it reaches the fans by the same road as the app - so it gets the same
    /// checks. Curves are repaired rather than dropped where that is
    /// possible (see [`curve::repair`]): a curve somebody tuned before the
    /// rules existed should come back as the nearest valid curve, not vanish.
    pub fn sanitise(&mut self) -> Vec<String> {
        let mut changed = Vec::new();

        let window = self.ma_window.clamp(MIN_MA_WINDOW, MAX_MA_WINDOW);
        if window != self.ma_window {
            changed.push(format!("maWindow {} -> {window}", self.ma_window));
            self.ma_window = window;
        }

        for (name, value) in [
            ("fanMaxRpm", &mut self.fan_max_rpm),
            ("fan1MaxRpm", &mut self.fan1_max_rpm),
            ("fan2MaxRpm", &mut self.fan2_max_rpm),
            ("fanMinRpm", &mut self.fan_min_rpm),
            ("fanStableMinRpm", &mut self.fan_stable_min_rpm),
        ] {
            if let Some(rpm) = value.filter(|rpm| !(0..=MAX_PLAUSIBLE_RPM).contains(rpm)) {
                changed.push(format!("{name} {rpm} is not a plausible speed; forgotten"));
                *value = None;
            }
        }

        let mut check = |name: &str, points: &mut Vec<CurvePoint>| -> bool {
            if points.is_empty() || curve::validate(points).is_ok() {
                return true;
            }
            match curve::repair(points) {
                Some(repaired) => {
                    changed.push(format!("the {name} curve was repaired to a safe shape"));
                    *points = repaired;
                    true
                }
                None => {
                    changed.push(format!("the {name} curve could not be repaired; dropped"));
                    false
                }
            }
        };
        if !check("shared", &mut self.curve) {
            self.curve.clear();
        }
        self.profile_curves
            .retain(|profile, points| check(&format!("'{profile}'"), points));

        changed
    }

    /// The curve that should be driving the fans, given the profile the
    /// machine is in.
    ///
    /// Falls back to the shared [`FanConfig::curve`] for a profile with no
    /// curve of its own, and for a daemon that has never been told a
    /// profile at all. Falling back rather than refusing matters: a machine
    /// whose `power` module is unsupported still gets a working curve.
    pub fn curve_for(&self, profile: Option<&str>) -> &[CurvePoint] {
        profile
            .and_then(|p| self.profile_curves.get(p))
            .filter(|points| !points.is_empty())
            .map(Vec::as_slice)
            .unwrap_or(&self.curve)
    }

    /// Gives every profile a curve of its own the first time per-profile
    /// curves are used, copying the one shared shape the user already drew.
    ///
    /// Without this, turning four profiles loose on an empty map would
    /// silently drop the curve someone had tuned: they would all fall back
    /// to `curve`, look identical, and then diverge one edit at a time from
    /// a shape nobody chose. Seeded only for profiles that have no entry,
    /// so it can never overwrite one.
    pub fn migrate_profile_curves(&mut self, profiles: &[&str]) -> bool {
        if self.curve.is_empty() {
            return false;
        }
        let mut seeded = false;
        for profile in profiles {
            if !self.profile_curves.contains_key(*profile) {
                self.profile_curves
                    .insert((*profile).to_string(), self.curve.clone());
                seeded = true;
            }
        }
        seeded
    }
}

/// Where a fan-cleaning cycle is, from this module's point of view.
///
/// A bool would not do: **the two transitional states are the ones that
/// matter.** Starting means the blades are being braked and nothing is
/// reversed yet; stopping means the ramp down is underway and the fans are
/// still backwards. A caller that reads either as "idle" would offer a
/// second cycle in the middle of the first, and the control loop would
/// take the fans back mid-ramp.
#[derive(Debug, Clone, Default)]
enum Cleaning {
    #[default]
    Idle,
    Starting,
    Running(cleaner::Cycle),
    Stopping,
}

impl Cleaning {
    fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    /// Whether the fans are the cleaner's rather than the control loop's.
    /// True through both transitions, which is the point.
    fn holds_the_fans(&self) -> bool {
        !self.is_idle()
    }

    fn cycle(&self) -> Option<&cleaner::Cycle> {
        match self {
            Self::Running(cycle) => Some(cycle),
            _ => None,
        }
    }
}

struct State {
    config: FanConfig,
    /// Mode actually in force, which is only the configured one once it has
    /// been applied - and on a machine that cannot do it, never.
    mode: FanMode,
    /// Whether *this daemon* put the fans where they are.
    ///
    /// False at startup, when `mode` is only what the hardware was found
    /// in. The distinction matters: adopting an observed `manual` and then
    /// "re-asserting" it would write our own idea of the speed over
    /// whatever the user had actually set, seconds after boot, without
    /// anyone asking. Nothing is written until someone asks - or until
    /// `restoreModeOnStart` says they already did.
    owned: bool,
    /// The power profile the machine is in, as last announced on the event
    /// bus - the key `config.profile_curves` is looked up by.
    ///
    /// Deliberately **not** persisted: it is an observation about the
    /// machine right now, and the power module is its owner. A daemon that
    /// restarts has not heard an announcement yet and uses the shared
    /// curve until it does, which is the same rule as `mode` not being
    /// restored unless `restoreModeOnStart` says so. `None` also covers
    /// every machine whose `power` module found nothing to control.
    active_profile: Option<String>,
    hysteresis: curve::Hysteresis,
    /// In `manual` or `curve`, the speed asked for is below the fans' floor
    /// and they have been handed to the firmware so it can stop them. The
    /// mode is still the user's; only the hardware is in auto.
    released: bool,
    /// Watches for the fans stalling at Pyren's floor, and nudges it up
    /// when it keeps happening. See [`stall`]. In memory only.
    stall: stall::StallWatch,
    smoother: curve::TempSmoother,
    /// A calibration run has the fans, and the control loop must not take
    /// them back mid-measurement - it would drop them out of max and the
    /// run would measure the ramp back down.
    calibrating: bool,
    /// Where a fan-cleaning cycle is. Like `calibrating`, this stops the
    /// control loop writing - see [`Cleaning`].
    cleaning: Cleaning,
    /// The last answer the firmware gave about the cleaner. Cached because
    /// asking costs two ACPI calls and the answer is a property of the
    /// machine, not of the moment; `cleanerStatus { refresh: true }` and
    /// every `startCleaning` re-ask.
    cleaner_probe: Option<cleaner::Probe>,
    /// Why the last cycle failed, kept for the status read - a start that
    /// failed in a background ramp has nobody left to return an error to.
    last_cleaner_error: Option<Msg>,
    last_target_pwm: Option<u8>,
    last_control_error: Option<Msg>,
    last_save_error: Option<String>,
    /// The thermal guards - see [`safety`]. In memory only: every one of
    /// them is about the machine right now.
    sensor_watch: safety::SensorWatch,
    critical: safety::CriticalLatch,
    checker: safety::ThermalChecker,
    zero_rpm: safety::ZeroRpmWatch,
    /// A real speed was commanded and the fans sat at 0 rpm, so the
    /// firmware was given them. Cleared by the next mode or curve the user
    /// sets - that is somebody deciding to try again.
    stalled: bool,
    /// The mode a guard is holding the hardware in, and when it was last
    /// written. `None` while the user's setting has the fans. The setting
    /// itself (`mode`, `config`) is never changed by a guard, which is what
    /// makes "put back exactly what was there" possible.
    safety_hold: Option<(FanMode, u64)>,
    /// What "hot" means for the checker, and when it was last asked.
    heat: safety::HeatThresholds,
    heat_read_at: Option<u64>,
    /// The daemon is on its way out and has handed the fans back; nothing
    /// may take them again in the moments before the process ends.
    exiting: bool,
}

impl State {
    fn new(config: FanConfig, mode: FanMode, owned: bool) -> Self {
        Self {
            smoother: curve::TempSmoother::new(config.ma_window),
            config,
            mode,
            active_profile: None,
            owned,
            hysteresis: curve::Hysteresis::new(),
            released: false,
            stall: stall::StallWatch::default(),
            calibrating: false,
            cleaning: Cleaning::Idle,
            cleaner_probe: None,
            last_cleaner_error: None,
            last_target_pwm: None,
            last_control_error: None,
            last_save_error: None,
            sensor_watch: safety::SensorWatch::default(),
            critical: safety::CriticalLatch::default(),
            checker: safety::ThermalChecker::default(),
            zero_rpm: safety::ZeroRpmWatch::default(),
            stalled: false,
            safety_hold: None,
            heat: safety::HeatThresholds::default(),
            heat_read_at: None,
            exiting: false,
        }
    }

    /// Forget what was last written, so the next tick applies whatever it
    /// decides unconditionally - including handing the fans to the
    /// firmware again, which the hardware may no longer be in.
    fn forget_writes(&mut self) {
        self.hysteresis.reset();
        self.released = false;
        // The last measured speed is about to stop meaning anything; the
        // fault trail ages out by time and stays.
        self.stall.idle();
        self.zero_rpm.reset();
    }
}

/// The fans, claimed for a measurement or a write check that must not have
/// the control loop writing underneath it. Dropping it gives them back -
/// on every path out, a panic included, which is why it is a guard: a
/// `calibrating` flag left set by a panic stopped the curve for good.
struct FanClaim {
    state: Arc<Mutex<State>>,
}

impl Drop for FanClaim {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        state.calibrating = false;
        // The fans were moved out from under the hysteresis, so what it
        // last wrote says nothing about where they are now.
        state.forget_writes();
    }
}

/// What this module knows about the hardware it is driving: where the
/// sysfs files are, and what they will accept.
///
/// Shared and replaceable rather than fixed at construction, because both
/// answers change under a running daemon. Installing the driver unloads
/// and reloads `hp-wmi`, which destroys and recreates the whole hwmon
/// directory - `hwmon9` becomes `hwmon8` - so every path found at startup
/// then names a file that no longer exists. Fan control went on reporting
/// itself as working and quietly read nothing until somebody restarted the
/// daemon, which is what the driver wizard used to have to tell people to
/// do. See [`FanModule::rediscover`].
#[derive(Clone)]
struct Hardware {
    paths: FanPaths,
    caps: Capabilities,
}

/// The daemon binary's event bus, once it has handed one over. Empty
/// everywhere else - `pyren-check` and the tests build a `FanModule` with
/// nobody listening, and publishing has to be a no-op there rather than a
/// reason to require a bus. Mirrors `pyren_power::Announcer`.
#[derive(Clone, Default)]
pub struct Announcer(Arc<std::sync::OnceLock<Arc<pyren_core::EventBus>>>);

impl Announcer {
    fn publish(&self, topic: &str, payload: Value) {
        if let Some(bus) = self.0.get() {
            bus.publish(topic, payload);
        }
    }
}

#[derive(Clone)]
pub struct FanModule {
    hardware: Arc<Mutex<Hardware>>,
    store: ConfigStore,
    state: Arc<Mutex<State>>,
    announcer: Announcer,
    heat_source: Arc<std::sync::OnceLock<HeatSource>>,
}

impl FanModule {
    /// Where the fan files are *right now*.
    ///
    /// Cloned out rather than borrowed: holding the lock across a sysfs
    /// read would serialise every status call behind whichever one is
    /// waiting on the kernel.
    fn paths(&self) -> FanPaths {
        lock_hw(&self.hardware).paths.clone()
    }

    fn caps(&self) -> Capabilities {
        lock_hw(&self.hardware).caps
    }

    /// The floor in force for `config` on the driver loaded right now. A
    /// driver that reports its own floor is believed over a measurement of
    /// it, which is only a fallback for one that does not.
    fn floor(&self, config: &FanConfig) -> Floor {
        let paths = self.paths();
        let driver = control::read_driver_floor(&paths).or(config.fan_min_rpm);
        floor_in_force(
            driver,
            pyren_floor(config.fan_stable_min_rpm, driver),
            config.keep_driver_floor,
            control::floor_override_supported(&paths),
        )
    }

    /// Tells the driver the floor in force. A read when it already knows,
    /// which is every tick but the first after a driver reload - that
    /// resets the parameter, and this is what puts it back.
    fn sync_floor_override(&self, floor: Floor) -> Result<(), control::ControlError> {
        let Some(want) = floor.override_hundreds else {
            return Ok(());
        };
        let paths = self.paths();
        if control::read_floor_override(&paths) == Some(want) {
            return Ok(());
        }
        control::set_floor_override(&paths, want)
    }

    /// What this machine can be *told*, as opposed to what files it has.
    ///
    /// The difference is board `8D2F`: `pwm1` is there, the driver accepts
    /// writes to it, and the fans never move. Once [`speed_probe`] has
    /// caught that, `setSpeed` has to go false or every client goes on
    /// offering a slider that does nothing - which is the exact failure
    /// [`control`] was written to avoid, one layer further in.
    fn effective_caps(&self) -> Capabilities {
        let caps = self.caps();
        let ignored = lock(&self.state).config.speed_control.is_ignored();
        Capabilities {
            set_speed: caps.set_speed && !ignored,
            ..caps
        }
    }

    /// Look at the hardware again, because the driver under it changed.
    ///
    /// Called after a driver install or restore, which reload `hp-wmi` and
    /// so renumber the hwmon directory. Without it the daemon keeps the
    /// paths it found at startup and reads a directory that no longer
    /// exists; with it, nobody has to be told to restart anything.
    ///
    /// Returns whether anything actually moved, which is worth reporting:
    /// "the driver was replaced and fan control is still there" is a
    /// different sentence from "nothing changed".
    pub fn rediscover(&self) -> bool {
        self.adopt(discover_paths())
    }

    /// Takes on a freshly found set of paths, and says whether it was a
    /// different machine from the one it had.
    ///
    /// Split from [`rediscover`](Self::rediscover) so the decision can be
    /// tested against fabricated paths: the discovery itself reads real
    /// sysfs (or `PYREN_HWMON_DIR`, which is process-wide and so unusable
    /// from a test running beside others), while this - what counts as a
    /// change, and that the swap is actually stored - is the part that
    /// was written today.
    fn adopt(&self, fresh: FanPaths) -> bool {
        let caps = Capabilities::detect(&fresh);
        let mut hardware = lock_hw(&self.hardware);
        // The hwmon directory is the thing a module reload renumbers, and
        // capabilities are what changes when the driver under it is a
        // different one. Comparing every path would report a move twice.
        let changed = hardware.paths.hwmon_dir != fresh.hwmon_dir || hardware.caps != caps;
        hardware.paths = fresh;
        hardware.caps = caps;
        drop(hardware);

        if changed {
            log_info!(
                "re-read the fan hardware after a driver change: {}",
                describe(caps).text
            );
        }
        changed
    }

    pub fn new() -> Self {
        Self::with_store(ConfigStore::system())
    }

    /// Builds the module against an explicit config store. Tests use this
    /// to keep out of the real `/etc`.
    pub fn with_store(store: ConfigStore) -> Self {
        let paths = discover_paths();
        let caps = Capabilities::detect(&paths);

        let loaded = store.load::<FanConfig>("fan");
        match &loaded.outcome {
            LoadOutcome::Loaded => {
                log_info!("fan config loaded from {}", store.path_for("fan").display());
            }
            LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                log_warn!(
                    "fan config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                log_warn!(
                    "fan config is version {found}, newer than this build \
                     understands; using defaults and leaving the file alone"
                );
            }
        }
        let mut config = loaded.value;
        for change in config.sanitise() {
            log_warn!("fan config: {change}");
        }

        // Believe the hardware over the file: the machine may have been
        // rebooted, or something else may have moved the fans since.
        let observed = observed_mode(&paths);
        let restoring = config.restore_mode_on_start && caps.supports(config.mode);
        let mode = if restoring {
            config.mode
        } else {
            observed.unwrap_or(FanMode::Auto)
        };

        // Adopting a manual mode we did not set means adopting its speed
        // too, or the app would show a number nobody chose.
        if !restoring && mode == FanMode::Manual {
            if let Some(pwm) = control::read_pwm(&paths) {
                config.manual_pwm = pwm;
            }
        }

        let state = Arc::new(Mutex::new(State::new(config, mode, restoring)));

        let module = Self {
            hardware: Arc::new(Mutex::new(Hardware { paths, caps })),
            store,
            state,
            announcer: Announcer::default(),
            heat_source: Arc::default(),
        };
        module.recover_interrupted_cycle();
        if caps.switch_mode {
            module.spawn_control_loop();
        }
        module
    }

    /// Fans found spinning **backwards** at startup, by a daemon that did
    /// not put them there.
    ///
    /// This is the one place the module touches the hardware without being
    /// asked, and it is a deliberate exception to "the daemon does not
    /// touch the fans until asked" (`dev/TODO.md`). The rule is about not
    /// imposing a remembered setting on a machine at boot. Reverse spin is
    /// not a setting: it is cooling switched off, by a cycle that was
    /// supposed to end thirty seconds after it began and whose daemon died
    /// first. Leaving it for whenever somebody next opens the app is not a
    /// plan, it is a thermal event with a UI.
    ///
    /// Narrow on purpose. It runs only when the tachometers themselves say
    /// reverse - the driver's own bit, not a config file this process
    /// wrote - and only when `acpi_call` is *already* loaded, because
    /// loading a kernel module at startup to undo something that might not
    /// be ours is exactly the change this project does not make.
    fn recover_interrupted_cycle(&self) {
        let (_, reversed) = read_fan_rpm(
            self.paths().fan1_input.as_deref(),
            self.paths().fan2_input.as_deref(),
        );
        if !reversed || !acpi::is_loaded() {
            return;
        }

        log_info!(
            "the fans are spinning in reverse and no cycle was started here; \
             ending it and handing them back"
        );

        // On a thread: the ramp down takes seconds, and nothing about
        // serving the socket should wait for it.
        let module = self.clone();
        std::thread::spawn(move || {
            lock(&module.state).cleaning = Cleaning::Stopping;
            let generation = cleaner::probe()
                .generation
                .unwrap_or(cleaner::Generation::Modern);
            let result = cleaner::emergency_stop(generation);
            if let Err(e) = &result {
                log_warn!("could not end the interrupted cleaning cycle: {e}");
            }
            module.finish_cycle(result);
        });
    }

    /// A module for tools that only want to *look*: no config is read or
    /// written and no control loop is started.
    ///
    /// `pyren-check` uses this. A self-test that created files and
    /// started a thread capable of driving fans would be a self-test nobody
    /// should run on a machine they care about.
    pub fn inspector() -> Self {
        let paths = discover_paths();
        let caps = Capabilities::detect(&paths);
        let config = FanConfig::default();

        let mode = observed_mode(&paths).unwrap_or(FanMode::Auto);
        Self {
            state: Arc::new(Mutex::new(State::new(config, mode, false))),
            store: ConfigStore::system(),
            hardware: Arc::new(Mutex::new(Hardware { paths, caps })),
            announcer: Announcer::default(),
            heat_source: Arc::default(),
        }
    }

    /// Hands the module the daemon's event bus, so it can announce what it
    /// does: `fan.mode` when the mode changes (see `set_mode`), and
    /// `fan.floorRaised` when the stall watch nudges the fans' minimum up.
    /// Call once, from the binary. A no-op second call is tolerated rather
    /// than panicking - the binary is the only caller and calls it once.
    pub fn publish_to(&self, events: Arc<pyren_core::EventBus>) {
        let _ = self.announcer.0.set(events);
    }

    /// Tells the module where the "hot" thresholds live. Called once, from
    /// the binary; without it the checker uses [`safety::HeatThresholds`]'
    /// defaults, which are the power supervisor's defaults too.
    pub fn set_heat_source(&self, source: HeatSource) {
        let _ = self.heat_source.set(source);
    }

    /// The daemon is stopping: give the fans back to the firmware.
    ///
    /// Called from the termination handler, which ends the process straight
    /// after - no destructor runs, so without this a curve's last low speed,
    /// a calibration sweep's near-stall floor, or a cleaning cycle's reverse
    /// spin would stay on the hardware, held there by the driver's own
    /// keep-alive, with nothing left to change it. In the order that makes
    /// each step safe: reverse spin ended first (auto on reversed blades is
    /// not auto), then the mode, then the driver's floor put back to its
    /// own table's.
    pub fn on_exit(&self) {
        let paths = self.paths();
        let caps = self.caps();
        let running = {
            let mut state = lock(&self.state);
            state.exiting = true;
            match &state.cleaning {
                Cleaning::Running(cycle) => Some(Some(cycle.generation)),
                Cleaning::Starting | Cleaning::Stopping => Some(None),
                Cleaning::Idle => None,
            }
        };

        let (_, reversed) = read_fan_rpm(paths.fan1_input.as_deref(), paths.fan2_input.as_deref());
        if (running.is_some() || reversed) && acpi::is_loaded() {
            let generation = running.flatten().unwrap_or_else(|| {
                cleaner::probe()
                    .generation
                    .unwrap_or(cleaner::Generation::Modern)
            });
            if let Err(e) = cleaner::emergency_stop(generation) {
                log_warn!("on exit: could not end the fan-cleaning cycle: {e}");
            }
        }
        if caps.switch_mode {
            match control::apply(&paths, caps, FanMode::Auto, 0) {
                Ok(()) => log_info!("on exit: fans handed back to the firmware"),
                Err(e) => log_warn!("on exit: could not hand the fans back to the firmware: {e}"),
            }
        }
        if control::floor_override_supported(&paths) {
            if let Err(e) = control::set_floor_override(&paths, 0) {
                log_warn!("on exit: could not put the driver's fan floor back: {e}");
            }
        }
    }

    /// Claims the fans for something that drives them directly. Refused
    /// while anything else has them.
    fn claim_fans(&self) -> Result<FanClaim, ModuleError> {
        let mut state = lock(&self.state);
        if state.calibrating {
            return Err(ModuleError::localised(
                ErrorKind::Busy,
                msg!(
                    "fan.err.calibrating",
                    "a calibration run is already in progress"
                ),
            ));
        }
        if !state.cleaning.is_idle() {
            return Err(ModuleError::localised(
                ErrorKind::Busy,
                msg!(
                    "fan.err.cleaningHoldsFans",
                    "a fan-cleaning cycle has the fans; wait for it to finish"
                ),
            ));
        }
        state.calibrating = true;
        Ok(FanClaim {
            state: Arc::clone(&self.state),
        })
    }

    /// The question a measurement asks before it starts and at every sample
    /// while it holds the fans: is the machine still cool enough for the
    /// fans to be somewhere other than where it needs them? No reading at
    /// all is a no as well - there would be nothing to stop it.
    fn measurement_abort(&self) -> impl Fn() -> Option<control::ControlError> {
        let paths = self.paths();
        move || {
            let cpu = paths.cpu_temp.as_deref().and_then(read_millideg_c);
            let gpu = paths.gpu_temp.as_deref().and_then(read_millideg_c);
            match safety::hottest_c(cpu, gpu) {
                None => Some(control::ControlError::NoTemperature),
                Some(temp) if temp > safety::MEASUREMENT_MAX_C => Some(
                    control::ControlError::TooHot(temp as i64, safety::MEASUREMENT_MAX_C as i64),
                ),
                Some(_) => None,
            }
        }
    }

    /// A measurement was stopped by the heat: the fans had been held away
    /// from where the machine needed them, so the safety sequence starts at
    /// once, whether or not the checker setting is on - firmware first, full
    /// speed if that does not answer, and the user's setting back once the
    /// machine is [`safety::MEASUREMENT_COOL_MARGIN_C`] under the limit.
    fn trip_after_measurement(&self, what: &str, error: &control::ControlError) {
        if !matches!(error, control::ControlError::TooHot(_, _)) {
            return;
        }
        let paths = self.paths();
        let rpm = fan_rpm_reading(&paths);
        let transition = lock(&self.state).checker.trip(
            monotonic_secs(),
            rpm,
            safety::MEASUREMENT_MAX_C - safety::MEASUREMENT_COOL_MARGIN_C,
        );
        log_warn!("{what} stopped: {error}; the thermal safety sequence has the fans");
        self.announcer.publish(
            "fan.safety",
            json!({
                "guard": "checker",
                "state": transition.as_str(),
                "reason": "measurementAborted",
            }),
        );
        let _ = self.tick_once();
    }

    /// The "hot" / "cooled" pair, re-read from its owner at most every
    /// [`HEAT_REFRESH_SECS`]. Asked outside the state lock: the source reads
    /// a file.
    fn heat_thresholds(&self, now_secs: u64) -> safety::HeatThresholds {
        {
            let state = lock(&self.state);
            if state
                .heat_read_at
                .is_some_and(|at| now_secs.saturating_sub(at) < HEAT_REFRESH_SECS)
            {
                return state.heat;
            }
        }
        let heat = self
            .heat_source
            .get()
            .and_then(|source| source())
            .map(|(hot_c, cool_c)| safety::HeatThresholds::sanitised(hot_c, cool_c))
            .unwrap_or_default();
        let mut state = lock(&self.state);
        state.heat = heat;
        state.heat_read_at = Some(now_secs);
        heat
    }

    /// Runs the fan-control self-test against this machine.
    ///
    /// `allow_writes` opts into the two checks that touch hardware: whether
    /// `pwm1` stores what it is given, and - the only question that actually
    /// settles anything - whether the fans then move. The second one spins
    /// them for a few seconds and puts back what it found. Both are off
    /// without `allow_writes`, and the report says they were not attempted
    /// rather than passing them by default.
    pub fn diagnose(&self, allow_writes: bool) -> diagnostics::Diagnosis {
        // A failed probe is not a failed diagnosis: the check reports it as
        // untested and the rest of the report is still worth having.
        let probe = if allow_writes {
            self.run_speed_probe(speed_probe::DEFAULT_SECONDS).ok()
        } else {
            None
        };
        // The write check puts pwm1 somewhere and back, so it needs the
        // fans the way a measurement does. Busy - a calibration, a cycle -
        // means it is reported as not attempted rather than run underneath.
        let claim = if allow_writes {
            self.claim_fans().ok()
        } else {
            None
        };
        let diagnosis = diagnostics::diagnose(&self.paths(), claim.is_some(), probe.as_ref());
        drop(claim);
        diagnosis
    }

    /// What this machine accepts, with a measured refusal taken into
    /// account. `pyren-check`'s verdict reads this, and it should say what
    /// was found to work rather than what exists.
    pub fn capabilities(&self) -> Capabilities {
        self.effective_caps()
    }

    /// Tells this module which power profile the machine is now in, so a
    /// curve drawn for that profile is the one that drives the fans.
    ///
    /// Called from the daemon binary's subscription to `power.mode`, which
    /// is the only place that knows about both modules - the `fan` and
    /// `power` crates still have no idea the other exists, and `profile` is
    /// an opaque string here for exactly that reason (see
    /// [`FanConfig::profile_curves`]).
    ///
    /// Seeds the per-profile curves from the shared one the first time a
    /// profile is heard of, so an upgrade does not silently drop a curve
    /// somebody tuned.
    ///
    /// Takes effect immediately rather than on the next tick: a mode change
    /// is exactly when someone is listening to the fans.
    pub fn set_active_profile(&self, profile: &str) {
        {
            let mut state = lock(&self.state);
            if state.active_profile.as_deref() == Some(profile) {
                return;
            }
            state.active_profile = Some(profile.to_string());
            if state.config.migrate_profile_curves(&[profile]) {
                persist(&self.store, &mut state);
            }
            // A different curve is in force, so what the hysteresis last
            // wrote says nothing about whether the new target is close.
            state.forget_writes();
        }
        // Only does anything in `curve` mode, and only when this daemon
        // owns the fans - the other modes do not read a curve at all.
        let _ = self.tick_once();
    }

    /// The profile whose curve is in force. `getStatus` reads the state
    /// directly; this exists for the tests that assert the switch happened.
    #[cfg(test)]
    fn active_profile(&self) -> Option<String> {
        lock(&self.state).active_profile.clone()
    }

    fn status(&self) -> Value {
        let cpu_temp_c = self.paths().cpu_temp.as_deref().and_then(read_millideg_c);
        let gpu_temp_c = self.paths().gpu_temp.as_deref().and_then(read_millideg_c);
        let (fan_rpm, is_reverse) = read_fan_rpm(
            self.paths().fan1_input.as_deref(),
            self.paths().fan2_input.as_deref(),
        );
        let state = lock(&self.state);
        let floor = self.floor(&state.config);

        let now = now_unix_secs();
        let floor_notices: Vec<Value> = state
            .config
            .fan_floor_notices
            .iter()
            .map(|n| {
                json!({
                    "atUnixSecs": n.at_unix_secs,
                    "ageSecs": now.saturating_sub(n.at_unix_secs),
                    "raisedFromRpm": n.raised_from_rpm,
                    "raisedToRpm": n.raised_to_rpm,
                    "stalls": n.stalls,
                    "reachedDriverFloor": n.reached_driver_floor,
                })
            })
            .collect();

        // Built apart: one more nested object and the status literal
        // outgrows `json!`'s recursion limit.
        let safety_status = json!({
            "checker": state.checker.phase_name(),
            "critical": state.critical.is_active(),
            "sensorFailed": state.sensor_watch.failed(),
            "stalled": state.stalled,
            "holding": state.safety_hold.map(|(mode, _)| mode.as_str()),
            "hotC": state.heat.hot_c,
            "coolC": state.heat.cool_c,
            "criticalC": safety::CRITICAL_C,
        });

        json!({
            "driverInstalled": self.paths().hwmon_dir.is_some(),
            // The *effective* capabilities: `pwm1` existing is not the same
            // as the fans obeying it. See `effective_caps`.
            "capabilities": Capabilities {
                set_speed: self.caps().set_speed && !state.config.speed_control.is_ignored(),
                ..self.caps()
            },
            // Why `capabilities.setSpeed` is what it is, so a client can say
            // "measured, and it does nothing" rather than only hiding the
            // control. `untested` until somebody runs `fan.probeSpeedControl`.
            "speedControl": state.config.speed_control.as_str(),
            "cpuTempC": cpu_temp_c,
            "gpuTempC": gpu_temp_c,
            "fanRpm": fan_rpm,
            // The same reading broken out per cooler, so a UI can show
            // "CPU 2400 / GPU 3100" beneath the headline number.
            "fans": read_labelled_fans(
                self.paths().fan1_input.as_deref(),
                self.paths().fan2_input.as_deref(),
            ),
            "isReverse": is_reverse,
            "mode": state.mode.as_str(),
            "pwm": control::read_pwm(&self.paths()),
            "targetPwm": state.last_target_pwm,
            "manualPwm": state.config.manual_pwm,
            // The curve actually in force. Still called `curve` and still
            // the first thing a client should read: a caller that knows
            // nothing about profiles goes on getting the right shape.
            "curve": state.config.curve_for(state.active_profile.as_deref()),
            // Every profile's curve, for an editor that wants to show them
            // all, plus the shared fallback under its own name.
            "profileCurves": state.config.profile_curves,
            "sharedCurve": state.config.curve,
            // Which of them `curve` came from; null when nothing has
            // announced a profile and the shared one is in force.
            "activeProfile": state.active_profile,
            "interpolation": state.config.interpolation,
            "referenceSensor": state.config.reference_sensor.as_str(),
            // What the curve is *actually* reading right now, which is not
            // the setting whenever the card is asleep. A UI that showed
            // only the setting would be telling the user their curve
            // follows a sensor it has fallen back from.
            "referenceSensorInUse": reference_temp(
                cpu_temp_c,
                gpu_temp_c,
                state.config.reference_sensor,
            )
            .map(|(_, sensor)| sensor.as_str()),
            "gpuSensorAvailable": self.paths().gpu_temp.is_some(),
            "restoreModeOnStart": state.config.restore_mode_on_start,
            "fanMaxRpm": state.config.fan_max_rpm,
            "fan1MaxRpm": state.config.fan1_max_rpm,
            "fan2MaxRpm": state.config.fan2_max_rpm,
            // The floor in force: the driver's or Pyren's, per
            // `keepDriverFloor`. Below it the firmware gets the fans.
            "fanMinRpm": floor.rpm,
            // The two it is chosen from. The driver's is what it reports
            // (or, on a driver that does not, what calibration measured);
            // Pyren's is null until a calibration has swept for it.
            "driverMinRpm": control::read_driver_floor(&self.paths()).or(state.config.fan_min_rpm),
            "pyrenMinRpm": pyren_floor(
                state.config.fan_stable_min_rpm,
                control::read_driver_floor(&self.paths()).or(state.config.fan_min_rpm),
            ),
            // What the sweep measured, before the margin.
            "slowestHeldRpm": state.config.fan_stable_min_rpm,
            "keepDriverFloor": state.config.keep_driver_floor,
            // Whether the driver can be told a floor other than its own at
            // all; false on a driver installed before Pyren's patch did it.
            "floorOverrideSupported": control::floor_override_supported(&self.paths()),
            // Targets below this PWM hand the fans to the firmware; 0 when
            // there is no known floor and nothing is handed over.
            "stopBelowPwm": curve::stop_below_pwm(floor.rpm, state.config.fan_max_rpm),
            // True while that is the case right now: the mode is still
            // manual or curve, but the fans are the firmware's.
            "fansReleased": state.released,
            // Times the stall watch raised Pyren's floor because the fans
            // kept stalling at it, newest first, for the app to surface.
            // `clearFloorNotices` empties it. `ageSecs` is derived above so
            // a reader never has to trust the daemon's clock against its
            // own.
            "floorNotices": floor_notices,
            // Stalls seen in the last half hour but not yet enough to act;
            // 0 almost always, and a hint to the app that something is off
            // before a floor is actually raised.
            "recentFanStalls": state.stall.recent_faults(),
            "calibrating": state.calibrating,
            "thermalSafetyChecker": state.config.thermal_safety_checker,
            "sensorFailureAction": state.config.sensor_failure_action.as_str(),
            // What the thermal guards are doing right now. `holding` is the
            // mode a guard has put the hardware in over the user's setting,
            // null while the setting has the fans; `mode` above is always the
            // setting.
            "safety": safety_status,
            // Enough for a caller that only wants to know the fans are not
            // its to command; `cleanerStatus` is the detail.
            "cleaning": state.cleaning.holds_the_fans(),
            "error": state.last_control_error,
            "saved": state.last_save_error.is_none(),
            "saveError": state.last_save_error,
        })
    }

    fn set_mode(&self, mode: FanMode, pwm: Option<u8>) -> ModuleResult {
        if !self.caps().supports(mode) {
            return Err(ModuleError::localised(
                ErrorKind::NotCapable,
                msg!(
                    "fan.err.cannotDoMode",
                    { "mode" => mode.as_str(), "exposes" => describe(self.caps()).text },
                    "this machine cannot do '{mode}': the hp-wmi driver exposes {exposes}. \
                     Run fan.diagnose for the details."
                ),
            ));
        }

        // A separate refusal from the one above, and deliberately so: that
        // one is "there is no pwm1", this one is "there is, and a probe
        // watched the fans ignore it". Conflating them would send someone
        // to install a driver they already have.
        if mode.needs_pwm() && lock(&self.state).config.speed_control.is_ignored() {
            return Err(ModuleError::localised(
                ErrorKind::NotCapable,
                msg!(
                    "fan.err.speedIgnored",
                    { "mode" => mode.as_str() },
                    "'{mode}' needs a commanded fan speed, and a probe found that this \
                     machine's embedded controller accepts pwm1 and ignores it - the fans \
                     stay on the firmware's own curve. Auto and max still work. Re-run \
                     fan.probeSpeedControl to test again."
                ),
            ));
        }

        {
            let mut state = lock(&self.state);
            if let Some(pwm) = pwm {
                state.config.manual_pwm = pwm.max(curve::MIN_COMMANDED_PWM);
            }
            state.config.mode = mode;
            state.mode = mode;
            state.owned = true;
            // Somebody chose again: a stall that handed the fans to the
            // firmware is worth one more try.
            state.stalled = false;
            // A mode change must land now, whatever the last write was.
            state.forget_writes();
            state.smoother = curve::TempSmoother::new(state.config.ma_window);
        }

        self.tick_once()?;
        let mut state = lock(&self.state);
        persist(&self.store, &mut state);
        let manual_pwm = state.config.manual_pwm;
        drop(state);

        // Announced the way `power.mode` is: anything watching the daemon -
        // the app's fan page, the widget, `pyren-ctl` - hears that the fan
        // mode moved, however it moved. `manualPwm` rides along so a widget
        // that cannot read a curve can still place its manual slider.
        self.announcer.publish(
            "fan.mode",
            json!({ "mode": mode.as_str(), "manualPwm": manual_pwm, "source": "request" }),
        );

        Ok(self.status())
    }

    /// Stores a curve.
    ///
    /// `profile` picks which one: a name writes that profile's curve, and
    /// `None` writes the profile the machine is *in* - so a client that
    /// knows nothing about profiles goes on editing the curve it can see,
    /// which is the one running. `Some("")` is the escape hatch for the
    /// shared fallback, used by `pyren-ctl --profile shared`.
    fn set_curve(
        &self,
        curve: Vec<CurvePoint>,
        interpolation: Option<Interpolation>,
        reference_sensor: Option<ReferenceSensor>,
        profile: Option<&str>,
    ) -> ModuleResult {
        if let Err(problem) = curve::validate(&curve) {
            return Err(ModuleError::localised(
                ErrorKind::InvalidParams,
                curve_problem(problem),
            ));
        }

        {
            let mut state = lock(&self.state);
            // Which curve this call is editing. An explicit name wins; then
            // the profile in force; and with neither, the shared one - the
            // case for a machine whose power module controls nothing.
            let target = match profile {
                Some("") => None,
                Some(name) => Some(name.to_string()),
                None => state.active_profile.clone(),
            };
            match target {
                Some(name) => {
                    state.config.profile_curves.insert(name, curve);
                }
                // Kept in step so a client that never mentions profiles
                // sees the shape it drew come back as the fallback too.
                None => state.config.curve = curve,
            }
            if let Some(interpolation) = interpolation {
                state.config.interpolation = interpolation;
            }
            if let Some(sensor) = reference_sensor {
                if sensor != state.config.reference_sensor {
                    state.config.reference_sensor = sensor;
                    // The smoothing window holds ten seconds of the *other*
                    // sensor's readings, and the two are not the same
                    // number. Averaging across the change would drive the
                    // fans from a temperature neither part is at.
                    state.smoother = curve::TempSmoother::new(state.config.ma_window);
                }
            }
            state.stalled = false;
            // The shape changed under the current target; re-evaluate.
            state.forget_writes();
        }

        // Only touches hardware if the curve is the mode in force.
        let _ = self.tick_once();
        let mut state = lock(&self.state);
        persist(&self.store, &mut state);
        drop(state);
        Ok(self.status())
    }

    /// Asks whether a commanded speed reaches the fans, and remembers the
    /// answer.
    ///
    /// Blocks for up to `seconds` while the fans are held at a speed they
    /// were not at. Same shape as [`Self::calibrate`], and it borrows the
    /// same `calibrating` flag to keep the control loop off the fans -
    /// there is one set of fans and only one of these may have them.
    fn probe_speed_control(&self, seconds: u64) -> ModuleResult {
        let probe = self.run_speed_probe(seconds)?;
        let mut result =
            serde_json::to_value(&probe).map_err(|e| ModuleError::Internal(e.to_string()))?;
        // The same shape every other fan write returns, so a caller never
        // has to follow one with a read.
        result["status"] = self.status();
        Ok(result)
    }

    /// The probe itself: drives the fans, stores what it learned, and hands
    /// back the trace. Shared with [`Self::diagnose`], which asks the same
    /// question as one check among many.
    fn run_speed_probe(&self, seconds: u64) -> Result<SpeedProbe, ModuleError> {
        let abort = self.measurement_abort();
        // Refused before anything is claimed or moved: too hot to start is
        // not a measurement that went wrong.
        if let Some(e) = abort() {
            return Err(control_error(e));
        }
        let claim = self.claim_fans()?;

        let fan_max_rpm = lock(&self.state).config.fan_max_rpm;
        let outcome = speed_probe::run(&self.paths(), self.caps(), fan_max_rpm, seconds, &abort);
        drop(claim);

        let mut state = lock(&self.state);
        let probe = match outcome {
            Ok(probe) => probe,
            Err(e) => {
                state.last_control_error = Some(e.to_msg());
                drop(state);
                self.trip_after_measurement("the speed probe", &e);
                return Err(control_error(e));
            }
        };

        // A run that settled nothing must not erase one that did.
        if let Some(answer) = Option::<SpeedControl>::from(probe.verdict) {
            state.config.speed_control = answer;
            // A mode that needs a speed is not doing anything on a machine
            // that ignores one. Leaving it selected would leave the UI
            // showing a curve nothing is following - the very thing this
            // probe exists to stop - so it goes back to the mode where the
            // firmware openly owns the fans.
            if answer.is_ignored() && state.mode.needs_pwm() {
                state.mode = FanMode::Auto;
                state.config.mode = FanMode::Auto;
            }
            persist(&self.store, &mut state);
        }
        drop(state);

        // Re-assert whatever the mode in force is, now rather than up to a
        // TICK later. Only does anything when this daemon owns the fans.
        let _ = self.tick_once();
        Ok(probe)
    }

    /// Measures what full speed is on this machine and remembers it.
    ///
    /// Blocks for up to `seconds` while the fans are at max - the caller
    /// is waiting on a physical process, and there is nothing to return
    /// until it finishes. Holding the state lock for that long would
    /// block `getStatus` too, so the flag is set, the lock dropped, and
    /// the run happens outside it.
    fn calibrate(&self, seconds: u64) -> ModuleResult {
        if !self.caps().supports(FanMode::Max) {
            return Err(ModuleError::localised(
                ErrorKind::NotCapable,
                msg!(
                    "fan.err.cannotCalibrate",
                    { "exposes" => describe(self.caps()).text },
                    "calibration puts the fans at max and watches them, which this machine \
                     cannot do: the hp-wmi driver exposes {exposes}. Run fan.diagnose for \
                     the details."
                ),
            ));
        }

        let abort = self.measurement_abort();
        if let Some(e) = abort() {
            return Err(control_error(e));
        }
        let claim = self.claim_fans()?;

        let outcome = calibration::run(&self.paths(), self.caps(), seconds, &abort);
        drop(claim);

        let mut state = lock(&self.state);
        let calibration = match outcome {
            Ok(calibration) => calibration,
            Err(e) => {
                state.last_control_error = Some(e.to_msg());
                drop(state);
                self.trip_after_measurement("calibration", &e);
                return Err(control_error(e));
            }
        };

        // A run that measured nothing must not erase a run that did.
        let mut pinned = None;
        if calibration.verdict.worth_storing() {
            state.config.fan_max_rpm = calibration.fan_max_rpm;
            state.config.fan1_max_rpm = calibration.fan1_max_rpm;
            state.config.fan2_max_rpm = calibration.fan2_max_rpm;
            // Same rule one level down: a floor that could not be measured
            // this time does not erase one that was.
            if calibration.fan_min_rpm.is_some() {
                state.config.fan_min_rpm = calibration.fan_min_rpm;
            }
            if calibration.fan_stable_min_rpm.is_some() {
                state.config.fan_stable_min_rpm = calibration.fan_stable_min_rpm;
            }
            persist(&self.store, &mut state);
            // The sweep put back the override it found; the floor in force
            // may be a different one now that Pyren's has been measured.
            let floor = self.floor(&state.config);
            if let Err(e) = self.sync_floor_override(floor) {
                log_warn!("could not set the driver's fan floor after calibrating: {e}");
            }
            pinned = Some(pin_ceiling(&calibration));
        }
        drop(state);

        let mut result =
            serde_json::to_value(&calibration).map_err(|e| ModuleError::Internal(e.to_string()))?;
        // What became of the measurement beyond this daemon's own config.
        // Reported rather than silently attempted: it is the difference
        // between a number the curve uses and a number the *driver* uses.
        result["pinned"] = match pinned {
            Some(Ok(detail)) => json!({ "ok": true, "detail": detail }),
            Some(Err(detail)) => json!({ "ok": false, "detail": detail }),
            None => Value::Null,
        };
        // The same shape every other fan write returns, so a caller never
        // has to follow one with a read.
        result["status"] = self.status();
        // Re-assert whatever the mode in force is, now rather than up to a
        // TICK later. Only does anything when this daemon owns the fans.
        let _ = self.tick_once();
        Ok(result)
    }

    /// Chooses between the upstream driver's floor and Pyren's.
    ///
    /// Takes effect now: the driver's clamp is set straight away - this is
    /// the user asking, so it is written whether or not this daemon owns
    /// the fans - and the curve re-decides against the new threshold.
    fn set_keep_driver_floor(&self, keep: bool) -> ModuleResult {
        let floor = {
            let mut state = lock(&self.state);
            // A calibration sweep is stepping the same parameter, and its
            // guard would put its own value back over this one.
            if state.calibrating {
                return Err(ModuleError::localised(
                    ErrorKind::Busy,
                    msg!(
                        "fan.err.calibrating",
                        "a calibration run is already in progress"
                    ),
                ));
            }
            state.config.keep_driver_floor = keep;
            persist(&self.store, &mut state);
            // The threshold for handing the fans over has moved.
            state.forget_writes();
            self.floor(&state.config)
        };
        self.sync_floor_override(floor).map_err(control_error)?;
        let _ = self.tick_once();
        Ok(self.status())
    }

    fn set_restore_on_start(&self, enabled: bool) -> ModuleResult {
        let mut state = lock(&self.state);
        state.config.restore_mode_on_start = enabled;
        if enabled {
            // Remember what is running now, so enabling this and rebooting
            // restores what the user can currently see.
            state.config.mode = state.mode;
        }
        persist(&self.store, &mut state);
        drop(state);
        Ok(self.status())
    }

    /// Turns the thermal safety checker on or off. Off lets go of anything
    /// it was holding on the next tick - taken now rather than then.
    fn set_thermal_safety_checker(&self, enabled: bool) -> ModuleResult {
        {
            let mut state = lock(&self.state);
            state.config.thermal_safety_checker = enabled;
            persist(&self.store, &mut state);
        }
        let _ = self.tick_once();
        Ok(self.status())
    }

    /// Picks where the fans go when their temperature readings are lost.
    /// Applied on the next tick, so a failure already in progress moves
    /// over straight away.
    fn set_sensor_failure_action(&self, action: SensorFailureAction) -> ModuleResult {
        {
            let mut state = lock(&self.state);
            state.config.sensor_failure_action = action;
            persist(&self.store, &mut state);
        }
        let _ = self.tick_once();
        Ok(self.status())
    }

    // --- the fan cleaner -----------------------------------------------

    /// What the cleaner can do here, and what it is doing right now.
    ///
    /// `refresh` re-asks the firmware; without it a cached answer is
    /// reused, because two ACPI calls per poll would put the app's status
    /// loop on the same file the lightbar writes through.
    ///
    /// Reading the status also **enforces the timeout**. That is not a
    /// side effect worth hiding: a cycle whose watchdog thread died would
    /// otherwise run until the daemon did, and this is the cheapest place
    /// left to notice.
    fn cleaner_status(&self, refresh: bool) -> Value {
        self.stop_if_expired();

        // Probed outside the lock, always: it is file I/O against an
        // interface the lightbar also writes, and holding the state lock
        // across it would block `getStatus` behind it.
        let need_probe = refresh || lock(&self.state).cleaner_probe.is_none();
        let probe = if need_probe {
            let probe = cleaner::probe();
            lock(&self.state).cleaner_probe = Some(probe.clone());
            probe
        } else {
            // Cloned out of the guard first, so the fallback - which does
            // file I/O - cannot end up running while the lock is held.
            let cached = lock(&self.state).cleaner_probe.clone();
            match cached {
                Some(probe) => probe,
                // Unreachable while `need_probe` is what decides this, and
                // cheap enough not to be worth an unwrap that could.
                None => cleaner::probe(),
            }
        };

        let state = lock(&self.state);
        let cycle = state.cleaning.cycle();
        let (_, is_reverse) = read_fan_rpm(
            self.paths().fan1_input.as_deref(),
            self.paths().fan2_input.as_deref(),
        );

        json!({
            "supported": probe.supported,
            "generation": probe.generation.map(cleaner::Generation::as_str),
            "capabilities": probe.capabilities,
            "answered": probe.answered,
            "unreachable": probe.unreachable,
            "acpiCallLoaded": probe.acpi_call_loaded,
            "acpiCallInstalled": probe.acpi_call_installed,
            "detail": probe.detail,
            "running": cycle.is_some(),
            // True through both transitions. A client shows a spinner and
            // offers neither button while this is set.
            "transitioning": matches!(state.cleaning, Cleaning::Starting | Cleaning::Stopping),
            "secondsRemaining": cycle.map(|c| c.remaining().as_secs()),
            "secondsTotal": cycle.map(|c| c.duration.as_secs()),
            "speed": cycle.map(|c| c.cpu_speed),
            // What the *hardware* says, which is the one reading that does
            // not depend on this daemon having been the one to start it.
            "fansReversed": is_reverse,
            "durationSecs": state.config.cleaner_duration_secs,
            "configuredSpeed": state.config.cleaner_speed,
            "maxStartTempC": cleaner::MAX_START_TEMP_C,
            "cpuTempC": self.paths().cpu_temp.as_deref().and_then(read_millideg_c),
            "error": state.last_cleaner_error,
        })
    }

    /// Starts a cycle and arms the watchdog that ends it.
    ///
    /// Blocks only for the braking step (a few seconds); the cycle itself
    /// runs in the background, so the caller gets a status back with a
    /// countdown rather than a connection held open for half a minute.
    fn start_cleaning(&self, speed: Option<u8>, seconds: Option<u64>, force: bool) -> ModuleResult {
        {
            let mut state = lock(&self.state);
            if !state.cleaning.is_idle() {
                return Err(cleaner_error(cleaner::CleanerError::Busy));
            }
            if state.calibrating {
                return Err(ModuleError::localised(
                    ErrorKind::Busy,
                    msg!(
                        "fan.err.calibrating",
                        "a calibration run is already in progress"
                    ),
                ));
            }
            // Claimed before the lock is dropped, so a second caller in
            // the braking window is refused rather than joining in.
            state.cleaning = Cleaning::Starting;
            state.last_cleaner_error = None;
        }

        let outcome = self.begin_cycle(speed, seconds, force);

        match outcome {
            Ok(cycle) => {
                let id = cycle.id;
                let wait = cycle.remaining();
                let generation = cycle.generation;
                lock(&self.state).cleaning = Cleaning::Running(cycle);
                self.arm_watchdog(id, wait, generation);
                Ok(self.cleaner_status(false))
            }
            Err(e) => {
                let mut state = lock(&self.state);
                state.cleaning = Cleaning::Idle;
                state.last_cleaner_error = Some(e.to_msg());
                drop(state);
                Err(cleaner_error(e))
            }
        }
    }

    /// The part that talks to the firmware, with the state already claimed.
    fn begin_cycle(
        &self,
        speed: Option<u8>,
        seconds: Option<u64>,
        force: bool,
    ) -> Result<cleaner::Cycle, cleaner::CleanerError> {
        let mut probe = cleaner::probe();

        if probe.unreachable.is_some() {
            // The interface could not be written. `ensure_loaded` both
            // tries the `modprobe` - which the probe deliberately does not,
            // being a question - and names which of the two reasons it was:
            // "install a package" or "run as root" are different errors
            // with different fixes.
            acpi::ensure_loaded()?;
            // Loading it changes the answer, so the answer is asked for
            // again rather than the stale "could not ask" being read as
            // "this machine cannot".
            probe = cleaner::probe();
        }
        lock(&self.state).cleaner_probe = Some(probe.clone());
        // `force` exists because none of the capability decoding in
        // `cleaner` has been confirmed against real firmware: a machine
        // that has the feature and answers a query this build reads wrongly
        // would otherwise have no way to try it. It skips the refusal and
        // the need for a sensor, not the temperature guard itself.
        if !probe.supported && !force {
            return Err(cleaner::CleanerError::NotCapable);
        }
        // The hotter of the two parts: reversing the fans takes the cooling
        // off both, and a card at 75 C is as good a reason to wait as a CPU.
        let paths = self.paths();
        let temp_c = safety::hottest_c(
            paths.cpu_temp.as_deref().and_then(read_millideg_c),
            paths.gpu_temp.as_deref().and_then(read_millideg_c),
        )
        .map(|t| t as i64);
        if temp_c.is_none() && !force {
            return Err(cleaner::CleanerError::NoTemperature);
        }

        let (duration, speed) = {
            let state = lock(&self.state);
            let secs = seconds.unwrap_or(state.config.cleaner_duration_secs);
            (
                Duration::from_secs(secs),
                speed.or(state.config.cleaner_speed),
            )
        };

        let request = cleaner::Request {
            speed,
            duration,
            temp_c,
        };

        let fan1 = self.paths().fan1_input.clone();
        let fan2 = self.paths().fan2_input.clone();
        cleaner::start(&probe, &request, || {
            let (rpm1, _) = parse_hwmon_rpm(read_raw_rpm(fan1.as_deref()));
            let (rpm2, _) = parse_hwmon_rpm(read_raw_rpm(fan2.as_deref()));
            (rpm1, rpm2)
        })
    }

    /// Ends the cycle now, ramps the fans back down out of reverse and
    /// puts the mode that was in force back.
    ///
    /// Idempotent: stopping when nothing is running is not an error, it is
    /// the state the caller asked for. That matters because the button
    /// that calls this is the one somebody reaches for when they are not
    /// sure what is happening.
    fn stop_cleaning(&self) -> ModuleResult {
        // The decision is made under the lock and acted on outside it:
        // the ramp down takes seconds, and holding the state lock across
        // it would block `getStatus` for the whole cycle.
        let generation = {
            let mut state = lock(&self.state);
            match std::mem::take(&mut state.cleaning) {
                Cleaning::Running(cycle) => {
                    state.cleaning = Cleaning::Stopping;
                    Some(cycle.generation)
                }
                // Somebody else owns the sequence. Putting a second ramp
                // down the same ACPI file would interleave two sets of
                // speed commands.
                transitional @ (Cleaning::Starting | Cleaning::Stopping) => {
                    state.cleaning = transitional;
                    return Err(cleaner_error(cleaner::CleanerError::Busy));
                }
                Cleaning::Idle => None,
            }
        };

        if let Some(generation) = generation {
            let result = cleaner::stop(generation);
            self.finish_cycle(result);
        }
        Ok(self.cleaner_status(false))
    }

    /// Puts the module back to idle after a stop, whichever way it went,
    /// and hands the fans back to whatever mode was in force.
    fn finish_cycle(&self, result: Result<(), cleaner::CleanerError>) {
        {
            let mut state = lock(&self.state);
            state.cleaning = Cleaning::Idle;
            state.last_cleaner_error = result.as_ref().err().map(cleaner::CleanerError::to_msg);
            // The fans were moved out from under the hysteresis by
            // something that does not speak PWM at all, so what it last
            // wrote says nothing about where they are.
            state.forget_writes();
        }
        // Re-asserts the configured mode now rather than up to a TICK
        // later. Only does anything when this daemon owns the fans.
        let _ = self.tick_once();
    }

    /// The timer that ends a cycle. One thread per cycle, tagged with its
    /// id so a watchdog left over from a cycle somebody stopped by hand
    /// cannot end the next one.
    fn arm_watchdog(&self, id: u64, wait: Duration, generation: cleaner::Generation) {
        let module = self.clone();
        let state = Arc::clone(&self.state);

        std::thread::spawn(move || {
            std::thread::sleep(wait);

            {
                let mut guard = lock(&state);
                match guard.cleaning.cycle() {
                    Some(cycle) if cycle.id == id => guard.cleaning = Cleaning::Stopping,
                    // Already stopped, or this is a later cycle. Either
                    // way it is not ours to end.
                    _ => return,
                }
            }

            let result = cleaner::stop(generation);
            module.finish_cycle(result);
        });
    }

    /// The second of the three places the timeout is enforced (see the
    /// [`cleaner`] module docs). Called from every status read and every
    /// control tick, so a cycle outlives its watchdog by a tick at most.
    ///
    /// Also where a cycle is ended early by the heat: it runs with the
    /// cooling effectively off, and [`safety::CLEANER_ABORT_C`] on either
    /// part ends it now rather than at its countdown.
    fn stop_if_expired(&self) {
        let paths = self.paths();
        let hottest = safety::hottest_c(
            paths.cpu_temp.as_deref().and_then(read_millideg_c),
            paths.gpu_temp.as_deref().and_then(read_millideg_c),
        )
        .map(|t| t as i64);
        let too_hot = hottest.filter(|t| *t > safety::CLEANER_ABORT_C);
        let generation = {
            let mut state = lock(&self.state);
            match state.cleaning.cycle() {
                Some(cycle) if cycle.expired() || too_hot.is_some() => {
                    let generation = cycle.generation;
                    state.cleaning = Cleaning::Stopping;
                    generation
                }
                _ => return,
            }
        };
        if let Some(temp) = too_hot {
            log_warn!(
                "{temp} °C during a fan-cleaning cycle; ending it now (the limit is {} °C)",
                safety::CLEANER_ABORT_C
            );
        }
        let result = cleaner::stop(generation);
        self.finish_cycle(result);
        if let Some(temp) = too_hot {
            lock(&self.state).last_cleaner_error = Some(msg!(
                "fan.cleaner.err.endedHot",
                { "temp" => temp, "limit" => safety::CLEANER_ABORT_C },
                "the cycle was ended early: {temp} °C is over the {limit} °C limit"
            ));
            self.announcer.publish(
                "fan.safety",
                json!({ "guard": "cleaner", "state": "stopped", "tempC": temp }),
            );
        }
    }

    /// One pass of the control loop. Also used by `setMode`/`setCurve` so a
    /// call takes effect immediately rather than up to [`TICK`] later.
    ///
    /// Events are collected during the pass and published after the state
    /// lock is gone, so a listener that reaches back into this module cannot
    /// deadlock against it.
    fn tick_once(&self) -> Result<(), ModuleError> {
        let mut events = Vec::new();
        let outcome = self.tick(&mut events);
        for (topic, payload) in events {
            self.announcer.publish(topic, payload);
        }
        outcome
    }

    fn tick(&self, events: &mut Vec<(&'static str, Value)>) -> Result<(), ModuleError> {
        let now_secs = monotonic_secs();
        let paths = self.paths();
        let cpu_temp_c = paths.cpu_temp.as_deref().and_then(read_millideg_c);
        let gpu_temp_c = paths.gpu_temp.as_deref().and_then(read_millideg_c);
        let rpm_reading = fan_rpm_reading(&paths);
        let rpm = rpm_reading.unwrap_or(0);
        let heat = self.heat_thresholds(now_secs);

        let mut state = lock(&self.state);
        if state.exiting {
            // The fans were handed back on the way out. See `on_exit`.
            return Ok(());
        }
        if state.calibrating {
            // Somebody else is driving, on purpose. See `State::calibrating`.
            // A measurement watches the temperature itself and stops.
            return Ok(());
        }
        if state.cleaning.holds_the_fans() {
            // A cleaning cycle owns the fans, and it is not driving them
            // through `pwm1` at all - writing a speed here would fight the
            // firmware override mid-cycle. See `Cleaning`. The cycle's own
            // heat limit is enforced in `stop_if_expired`.
            return Ok(());
        }
        let mode = state.mode;

        // --- the guards ----------------------------------------------------
        //
        // Before `owned`: a manual speed this daemon found at startup, and
        // did not set, is exactly as able to cook the machine as one it did.

        let hottest = safety::hottest_c(cpu_temp_c, gpu_temp_c);
        let was_critical = state.critical.is_active();
        let critical_now = state.critical.observe(hottest);
        if critical_now != was_critical {
            if critical_now {
                log_warn!(
                    "{} °C: at or over {} °C, fans to full speed until under {} °C",
                    hottest.unwrap_or_default(),
                    safety::CRITICAL_C,
                    safety::CRITICAL_CLEAR_C
                );
            } else {
                log_info!(
                    "cooled under {} °C; the critical override let go",
                    safety::CRITICAL_CLEAR_C
                );
            }
            events.push((
                "fan.safety",
                json!({ "guard": "critical", "active": critical_now, "tempC": hottest }),
            ));
        }
        // Only where a speed is being commanded. In auto the firmware is
        // already answering the heat, and in max there is nothing to add.
        let critical = critical_now && mode.needs_pwm();

        // A curve is blind without its own sensor. A manual speed does not
        // read one, but a low one leans on the critical override above, and
        // that is blind without *any* reading - so a slow manual speed with
        // no usable temperature gets the same fallback.
        let watched = match mode {
            FanMode::Curve => Some(
                reference_temp(cpu_temp_c, gpu_temp_c, state.config.reference_sensor)
                    .map(|(temp, _)| temp),
            ),
            FanMode::Manual if state.config.manual_pwm < safety::MANUAL_BLIND_BELOW_PWM => {
                Some(hottest.map(|temp| temp as i64))
            }
            _ => None,
        };
        let sensor_failed = if let Some(reading) = watched {
            let was = state.sensor_watch.failed();
            let failed = state.sensor_watch.observe(reading, now_secs, rpm > 0);
            if failed != was {
                if failed {
                    log_warn!(
                        "the fan {} lost its temperature readings; \
                         fans to {} until they return",
                        mode.as_str(),
                        state.config.sensor_failure_action.as_str()
                    );
                } else {
                    log_info!("the fan {}'s temperature readings are back", mode.as_str());
                }
                events.push(("fan.safety", json!({ "guard": "sensor", "active": failed })));
            }
            failed
        } else {
            state.sensor_watch.reset();
            false
        };

        // Fans already commanded to full, and turning, have nothing to
        // rise to - which must not be read as not answering.
        let commanded_full = rpm > 0
            && (mode == FanMode::Max
                || (mode.needs_pwm()
                    && !state.released
                    && state
                        .hysteresis
                        .last_written()
                        .is_some_and(|pwm| pwm >= safety::NEAR_FULL_PWM)));
        let evidence = safety::Evidence {
            now_secs,
            hottest_c: hottest,
            rpm: rpm_reading,
            fan_max_rpm: state.config.fan_max_rpm,
            commanded_full,
        };
        let enabled = state.config.thermal_safety_checker;
        let transition = state.checker.observe(evidence, heat, enabled);
        if transition != safety::Transition::None {
            match transition {
                safety::Transition::Watching => log_info!(
                    "{} °C is hot; watching whether the fans answer",
                    hottest.unwrap_or_default()
                ),
                safety::Transition::HandedToFirmware => log_warn!(
                    "the fans did not speed up for the heat in {} s; handing them to the firmware",
                    safety::ANSWER_SECS
                ),
                safety::Transition::ForcedMax => log_warn!(
                    "the firmware did not speed the fans up either; full speed until the machine cools"
                ),
                safety::Transition::Restored => {
                    log_info!("the machine cooled; the fan setting in force has the fans again")
                }
                safety::Transition::None => {}
            }
            events.push((
                "fan.safety",
                json!({
                    "guard": "checker",
                    "state": transition.as_str(),
                    "tempC": hottest,
                    "rpm": rpm_reading,
                }),
            ));
        }

        // Most urgent first. A stall hands over to the firmware, and if the
        // firmware does not answer the heat that follows, the checker above
        // outranks it.
        let hold = if critical {
            Some(FanMode::Max)
        } else if sensor_failed {
            Some(state.config.sensor_failure_action.mode())
        } else if let Some(mode) = state.checker.command() {
            Some(mode)
        } else if state.stalled && mode.needs_pwm() {
            Some(FanMode::Auto)
        } else {
            None
        };

        if let Some(forced) = hold {
            // Written straight away, not through the hysteresis, and
            // re-asserted on the same interval as any other setting.
            let due = match state.safety_hold {
                Some((held, at)) => {
                    held != forced || now_secs.saturating_sub(at) >= curve::REASSERT_SECS
                }
                None => true,
            };
            if !due {
                return Ok(());
            }
            state.hysteresis.reset();
            state.released = false;
            let result = control::apply(&paths, self.caps(), forced, 0);
            // Only a write that landed counts as held; one that failed is
            // tried again next tick rather than a minute from now.
            if result.is_ok() {
                state.safety_hold = Some((forced, now_secs));
            }
            return record_write(&mut state, result);
        }
        if state.safety_hold.take().is_some() {
            // The guard let go: what was in force goes back now, exactly.
            state.forget_writes();
            if !state.owned {
                // A mode this daemon adopted rather than set is put back as
                // it was found, once, and then left alone again.
                let pwm = state.config.manual_pwm;
                let result = control::apply(&paths, self.caps(), mode, pwm);
                return record_write(&mut state, result);
            }
        }

        if !state.owned {
            // Watching, not driving. See `State::owned`.
            return Ok(());
        }

        let target = match mode {
            // The firmware owns the fans in this mode; re-asserting it
            // would be a WMI call that changes nothing. It is written once,
            // when the mode is selected.
            FanMode::Auto => None,
            FanMode::Max => Some(0),
            FanMode::Manual => Some(state.config.manual_pwm),
            FanMode::Curve => {
                let sensor = state.config.reference_sensor;
                let Some((temp_c, _used)) = reference_temp(cpu_temp_c, gpu_temp_c, sensor)
                    .filter(|(temp, _)| safety::plausible_c(*temp))
                else {
                    // Held for the few ticks the sensor watch gives a
                    // renumbered hwmon to come back; after that it takes
                    // the fans to full speed above.
                    state.last_control_error = Some(msg!(
                        "fan.err.noCpuTemp",
                        "no CPU temperature sensor, so a curve cannot be followed"
                    ));
                    return Ok(());
                };
                let avg = state.smoother.push(temp_c as f64);
                let interpolation = state.config.interpolation;
                // The curve for the profile the machine is in, which is the
                // whole point of `profile_curves`. Cloned rather than
                // borrowed: `state` is mutably borrowed for the smoother
                // above and the error below.
                let points = state
                    .config
                    .curve_for(state.active_profile.as_deref())
                    .to_vec();
                match curve::target_pwm(&points, avg, interpolation) {
                    Some(pwm) => Some(pwm),
                    None => {
                        // Nothing to follow: the firmware, not whatever
                        // speed happened to be written last.
                        state.last_control_error =
                            Some(msg!("fan.err.curveEmpty", "the curve has no points"));
                        if state.released {
                            return Ok(());
                        }
                        state.released = true;
                        state.hysteresis.reset();
                        let result = control::apply(&paths, self.caps(), FanMode::Auto, 0);
                        return record_write(&mut state, result);
                    }
                }
            }
        };

        if mode == FanMode::Curve {
            state.last_target_pwm = target;
        }

        // A speed below the floor is one the fans cannot hold, so they go
        // to the firmware, which stops them when the machine is cool. Once:
        // auto needs no re-asserting, and the next write after this -
        // whenever the target climbs back - switches to manual first.
        if let Some(target) = target.filter(|_| mode.needs_pwm()) {
            let floor = self.floor(&state.config);
            let stop_below = curve::stop_below_pwm(floor.rpm, state.config.fan_max_rpm);
            let release = curve::release_fans(target, stop_below, state.released);
            if release && state.released {
                return Ok(());
            }
            if release {
                state.released = true;
                state.hysteresis.reset();
                state.zero_rpm.reset();
                let result = control::apply(&self.paths(), self.caps(), FanMode::Auto, 0);
                return record_write(&mut state, result);
            }
            if state.released {
                state.released = false;
                state.hysteresis.reset();
            }

            // A real speed commanded and nothing turning: whatever is wrong,
            // the firmware is likelier to cool the machine than a setpoint
            // the fans are not following.
            if state.zero_rpm.observe(now_secs, Some(target), rpm_reading) {
                state.stalled = true;
                state.zero_rpm.reset();
                state.hysteresis.reset();
                log_warn!(
                    "the fans read 0 rpm for {} s with pwm {target} commanded; \
                     handing them to the firmware",
                    safety::STALL_SECS
                );
                events.push((
                    "fan.fault",
                    json!({ "kind": "stalled", "pwm": target, "seconds": safety::STALL_SECS }),
                ));
                let result = control::apply(&paths, self.caps(), FanMode::Auto, 0);
                if result.is_ok() {
                    state.safety_hold = Some((FanMode::Auto, now_secs));
                }
                return record_write(&mut state, result);
            }

            // A speed is about to be commanded, so the driver has to be
            // clamping at the same floor this just decided against. Failing
            // leaves it at its own, higher one: the fans run a little
            // faster than asked, never slower than they can hold.
            // Warned once: this runs every tick, and a daemon without the
            // right to write it will not gain one between ticks.
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if let Err(e) = self.sync_floor_override(floor) {
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    log_warn!("could not set the driver's fan floor: {e}");
                }
            }

            // Watch for the fans stalling, but only where they could: on
            // Pyren's floor, with a speed near it commanded. `driver` is
            // the clamp the fans would have had without the override, so
            // `floor.rpm < driver` is "we lifted it".
            let driver = control::read_driver_floor(&self.paths());
            let on_pyrens_floor = matches!((floor.rpm, driver), (Some(f), Some(d)) if f < d);
            let expected = state
                .config
                .fan_max_rpm
                .map(|max| i64::from(target) * max / 255);
            match (on_pyrens_floor, floor.rpm, expected) {
                (true, Some(floor_rpm), Some(expected))
                    if expected <= floor_rpm + stall::NEAR_FLOOR_MARGIN_RPM =>
                {
                    // Steady: the hysteresis has a last write and it is
                    // close to this target, so the fans have had time to
                    // reach it rather than still be climbing.
                    let steady = state
                        .hysteresis
                        .last_written()
                        .is_some_and(|last| last.abs_diff(target) <= curve::PWM_DEADBAND);
                    if let stall::Tick::RaiseFloor { faults } =
                        state.stall.observe(now_secs, expected, rpm, steady)
                    {
                        if let Some(payload) =
                            self.raise_floor_after_stalls(&mut state, faults, driver)
                        {
                            events.push(("fan.floorRaised", payload));
                        }
                    }
                }
                _ => state.stall.idle(),
            }
        } else {
            state.zero_rpm.reset();
        }

        let should = match (mode, target) {
            (FanMode::Auto, _) => state.hysteresis.last_written().is_none(),
            (_, Some(target)) => {
                let fan_max = state.config.fan_max_rpm;
                let measured = (rpm > 0).then_some(rpm);
                state
                    .hysteresis
                    .should_apply(target, measured, fan_max, now_secs)
            }
            (_, None) => false,
        };
        if !should {
            return Ok(());
        }

        let pwm = target.unwrap_or(0);
        let result = control::apply(&self.paths(), self.caps(), mode, pwm);
        // Recorded even when the write failed, so a machine that cannot be
        // written to is retried once a minute rather than every tick.
        state.hysteresis.applied(pwm, now_secs);
        record_write(&mut state, result)
    }

    /// The stall watch has seen the fans give out at Pyren's floor enough
    /// times to act. Raise the stored slowest-held speed one step - which
    /// lifts Pyren's floor with it - persist it, and hand back the event
    /// payload for the caller to publish once the lock is clear.
    ///
    /// Only ever upward, and never past the driver's own floor: once
    /// Pyren's reaches the driver's there is nothing lower to keep, and a
    /// real `fan.calibrate` is what re-measures it. `None` when nothing
    /// changed (already at that ceiling and already recorded).
    fn raise_floor_after_stalls(
        &self,
        state: &mut State,
        faults: usize,
        driver: Option<i64>,
    ) -> Option<Value> {
        let held = state.config.fan_stable_min_rpm.unwrap_or(0);
        let from = pyren_floor(Some(held), driver).unwrap_or(held + FLOOR_MARGIN_RPM);

        let raised_held = held + calibration::SWEEP_FINE_STEP_RPM;
        let to = pyren_floor(Some(raised_held), driver);
        let reached_driver_floor = matches!((to, driver), (Some(t), Some(d)) if t >= d);

        // Once a notice already says the floor is the driver's, the
        // override is cleared and the watch stops firing - but a race
        // could get here once more. Note the raise so the cooldown holds,
        // and do nothing else.
        let already_capped = reached_driver_floor
            && state
                .config
                .fan_floor_notices
                .first()
                .is_some_and(|n| n.reached_driver_floor);
        if already_capped {
            state.stall.note_raised(monotonic_secs());
            return None;
        }

        state.config.fan_stable_min_rpm = Some(raised_held);
        let to = to.unwrap_or(raised_held);

        let notice = FloorNotice {
            at_unix_secs: now_unix_secs(),
            raised_from_rpm: from,
            raised_to_rpm: to,
            stalls: faults,
            reached_driver_floor,
        };
        state.config.fan_floor_notices.insert(0, notice);
        state.config.fan_floor_notices.truncate(FLOOR_NOTICE_CAP);
        state.forget_writes();
        persist(&self.store, state);
        state.stall.note_raised(monotonic_secs());

        let floor = self.floor(&state.config);
        if let Err(e) = self.sync_floor_override(floor) {
            log_warn!("could not raise the driver's fan floor after stalls: {e}");
        }
        log_info!(
            "fans stalled {faults} times near {from} rpm; raised Pyren's floor to {to} rpm{}",
            if reached_driver_floor {
                " - the driver's own; recalibrate to re-measure"
            } else {
                ""
            }
        );

        Some(json!({
            "fromRpm": from,
            "toRpm": to,
            "stalls": faults,
            "reachedDriverFloor": reached_driver_floor,
        }))
    }

    /// The loop that keeps a curve tracking, and keeps a chosen mode from
    /// quietly expiring.
    ///
    /// Runs on its own thread rather than being driven by IPC calls: a
    /// curve has to be followed while the app is closed, which is the whole
    /// reason there is a daemon.
    fn spawn_control_loop(&self) {
        // A clone, not a snapshot: it shares the hardware handle, so a
        // `rediscover` after a driver install reaches the loop that is
        // actually driving the fans, not only the next IPC call.
        let worker = self.clone();

        std::thread::spawn(move || {
            // Wall clock on purpose: the monotonic clock stops across a
            // suspend, and a resume is exactly the gap worth noticing.
            let mut last_pass = std::time::SystemTime::now();
            let mut panics = 0u32;
            loop {
                let now = std::time::SystemTime::now();
                let late = now
                    .duration_since(last_pass)
                    .map_or(true, |gap| gap > TICK * LATE_TICKS);
                last_pass = now;

                let pass = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if late {
                        // Back from a suspend, or the machine stalled: the
                        // firmware may have reset the fans in the meantime,
                        // and the hysteresis would otherwise sit on its last
                        // write for up to a minute.
                        lock(&worker.state).forget_writes();
                    }
                    // The third enforcement point for a cycle's timeout, so
                    // that a cleaner left running by a lost watchdog is ended
                    // by the loop that is running anyway.
                    worker.stop_if_expired();
                    // A transient sysfs failure must not take the loop down;
                    // the error is already recorded in the state for getStatus.
                    let _ = worker.tick_once();
                }));
                match pass {
                    Ok(()) => panics = 0,
                    Err(_) => {
                        panics += 1;
                        worker.after_panic(panics);
                    }
                }
                std::thread::sleep(TICK);
            }
        });
    }

    /// A pass of the control loop panicked. The fans go to the firmware -
    /// the one owner that does not depend on this code being right - and
    /// the loop carries on. A panic that keeps coming back stops this
    /// daemon driving the fans at all rather than flapping them between
    /// the firmware and a setting every two seconds.
    fn after_panic(&self, in_a_row: u32) {
        log_warn!(
            "the fan control loop panicked ({in_a_row} in a row); fans handed to the firmware"
        );
        let paths = self.paths();
        let caps = self.caps();
        if caps.switch_mode {
            if let Err(e) = control::apply(&paths, caps, FanMode::Auto, 0) {
                log_warn!("could not hand the fans to the firmware after a panic: {e}");
            }
        }
        let mut state = lock(&self.state);
        state.forget_writes();
        state.safety_hold = None;
        if in_a_row >= PANICS_BEFORE_STANDING_DOWN {
            state.owned = false;
            state.last_control_error = Some(msg!(
                "fan.err.loopPanicked",
                "the fan control loop kept failing, so the fans were left to the firmware; \
                 set a fan mode to try again"
            ));
        }
    }
}

impl Default for FanModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for FanModule {
    fn id(&self) -> &'static str {
        "fan"
    }

    fn is_supported(&self) -> bool {
        self.paths().hwmon_dir.is_some()
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getStatus" => Ok(self.status()),

            "diagnose" => {
                // Writing is opt-in and off by default: a diagnostic that
                // silently drives the fans would be a surprising thing for
                // a "check my hardware" button to do.
                let allow_writes = params
                    .get("allowWrites")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                serde_json::to_value(self.diagnose(allow_writes))
                    .map_err(|e| ModuleError::Internal(e.to_string()))
            }

            "setMode" => {
                let mode = params
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(FanMode::parse)
                    .ok_or_else(|| {
                        ModuleError::InvalidParams(
                            "params.mode must be one of auto, max, manual, curve".into(),
                        )
                    })?;
                let pwm = params
                    .get("pwm")
                    .and_then(Value::as_u64)
                    .map(|v| v.min(255) as u8);
                if mode == FanMode::Manual && pwm.is_none() {
                    return Err(ModuleError::InvalidParams(
                        "params.pwm (0-255) is required for manual mode".into(),
                    ));
                }
                self.set_mode(mode, pwm)
            }

            "setCurve" => {
                let points = params
                    .get("curve")
                    .cloned()
                    .ok_or_else(|| ModuleError::InvalidParams("params.curve is required".into()))?;
                let points: Vec<CurvePoint> = serde_json::from_value(points)
                    .map_err(|e| ModuleError::InvalidParams(format!("invalid curve: {e}")))?;
                let interpolation = match params.get("interpolation") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(serde_json::from_value(v.clone()).map_err(|e| {
                        ModuleError::InvalidParams(format!("invalid interpolation: {e}"))
                    })?),
                };
                let reference_sensor = match params.get("referenceSensor") {
                    None | Some(Value::Null) => None,
                    Some(v) => {
                        Some(v.as_str().and_then(ReferenceSensor::parse).ok_or_else(|| {
                            ModuleError::InvalidParams(
                                "params.referenceSensor must be \"cpu\" or \"gpu\"".into(),
                            )
                        })?)
                    }
                };
                // Absent edits whichever profile is running; a name edits
                // that one; "" edits the shared fallback. Anything that is
                // not a string is a mistake worth refusing rather than
                // silently writing the running profile's curve.
                let profile = match params.get("profile") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(name)) => Some(name.clone()),
                    Some(_) => {
                        return Err(ModuleError::InvalidParams(
                            "params.profile must be a string".into(),
                        ))
                    }
                };
                self.set_curve(points, interpolation, reference_sensor, profile.as_deref())
            }

            "setRestoreOnStart" => {
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        ModuleError::InvalidParams("params.enabled must be a boolean".into())
                    })?;
                self.set_restore_on_start(enabled)
            }

            "setKeepDriverFloor" => {
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        ModuleError::InvalidParams("params.enabled must be a boolean".into())
                    })?;
                self.set_keep_driver_floor(enabled)
            }

            "setThermalSafetyChecker" => {
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        ModuleError::InvalidParams("params.enabled must be a boolean".into())
                    })?;
                self.set_thermal_safety_checker(enabled)
            }

            "setSensorFailureAction" => {
                let action = params
                    .get("action")
                    .and_then(Value::as_str)
                    .and_then(SensorFailureAction::parse)
                    .ok_or_else(|| {
                        ModuleError::InvalidParams(
                            "params.action must be \"max\" or \"auto\"".into(),
                        )
                    })?;
                self.set_sensor_failure_action(action)
            }

            "clearFloorNotices" => {
                let mut state = lock(&self.state);
                state.config.fan_floor_notices.clear();
                persist(&self.store, &mut state);
                drop(state);
                Ok(self.status())
            }

            "calibrate" => {
                // Unlike `diagnose`, there is no read-only version of this
                // to default to: measuring full speed means reaching it.
                // The method name is the consent - it does exactly what it
                // says, and puts back what it found.
                let seconds = params
                    .get("seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(calibration::DEFAULT_SECONDS);
                self.calibrate(seconds)
            }

            "probeSpeedControl" => {
                // Like `calibrate`, the method name is the consent: it holds
                // the fans at a speed they were not at for a few seconds,
                // and puts back what it found.
                let seconds = params
                    .get("seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(speed_probe::DEFAULT_SECONDS);
                self.probe_speed_control(seconds)
            }

            "cleanerStatus" => {
                let refresh = params
                    .get("refresh")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Ok(self.cleaner_status(refresh))
            }

            "startCleaning" => {
                // Both are optional and both are clamped rather than
                // refused: a number outside the range is a slider that
                // went too far, not a caller that misunderstood the API.
                let speed = params
                    .get("speed")
                    .and_then(Value::as_u64)
                    .map(|v| v.clamp(cleaner::MIN_SPEED as u64, cleaner::MAX_SPEED as u64) as u8);
                let seconds = params
                    .get("seconds")
                    .and_then(Value::as_u64)
                    .map(|v| v.clamp(cleaner::MIN_DURATION_SECS, cleaner::MAX_DURATION_SECS));
                let force = params
                    .get("force")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.start_cleaning(speed, seconds, force)
            }

            "stopCleaning" => self.stop_cleaning(),

            "setCleanerConfig" => {
                let mut state = lock(&self.state);
                if let Some(secs) = params.get("seconds").and_then(Value::as_u64) {
                    state.config.cleaner_duration_secs =
                        secs.clamp(cleaner::MIN_DURATION_SECS, cleaner::MAX_DURATION_SECS);
                }
                // `null` is a value here, not an omission: it is how a
                // client goes back to the firmware's own speeds.
                match params.get("speed") {
                    None => {}
                    Some(Value::Null) => state.config.cleaner_speed = None,
                    Some(v) => {
                        let speed = v.as_u64().ok_or_else(|| {
                            ModuleError::InvalidParams(
                                "params.speed must be a number or null".into(),
                            )
                        })?;
                        state.config.cleaner_speed = Some(
                            speed.clamp(cleaner::MIN_SPEED as u64, cleaner::MAX_SPEED as u64) as u8,
                        );
                    }
                }
                persist(&self.store, &mut state);
                drop(state);
                Ok(self.cleaner_status(false))
            }

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

/// Hardware failures, translated for the socket.
///
/// The distinction that earns its keep is `notCapable` against
/// `permissionDenied`: the first will never work on this board however it
/// is asked, and the second works fine as root. A UI that cannot tell them
/// apart either hides a control that would work or offers one that never
/// will.
fn control_error(e: control::ControlError) -> ModuleError {
    let kind = match e {
        control::ControlError::Unsupported(_, _) => ErrorKind::NotCapable,
        control::ControlError::PermissionDenied(_, _) => ErrorKind::PermissionDenied,
        control::ControlError::Io(_, _) => ErrorKind::Io,
        // Like the cleaner's: a reasonable request, a machine in no state
        // for it right now.
        control::ControlError::TooHot(_, _) | control::ControlError::NoTemperature => {
            ErrorKind::Failed
        }
    };
    ModuleError::localised(kind, e.to_msg())
}

/// Which floor is in force, and what the driver has to be told for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Floor {
    /// The slowest speed that is commanded rather than handed to the
    /// firmware, in rpm; `None` when no floor is known at all.
    rpm: Option<i64>,
    /// What the driver's `min_rpm_override` should hold, in hundreds of
    /// rpm; `None` on a driver that has no such parameter.
    override_hundreds: Option<u8>,
}

/// One step above the slowest speed the sweep saw the fans hold.
///
/// That speed is the edge, and the edge moves: board 8D2F held 600 rpm
/// cleanly on one sweep and kicked back up from it on the one before. A
/// floor on the edge is a fan that stalls and restarts itself now and
/// then, so the floor is a fine step clear of it - 700 there. Applied here
/// rather than in the stored measurement, so that is what it says it is.
pub const FLOOR_MARGIN_RPM: i64 = calibration::SWEEP_FINE_STEP_RPM;

/// Pyren's floor from the slowest speed held; never above the driver's,
/// since a floor that high is the driver's already.
fn pyren_floor(slowest_held: Option<i64>, driver: Option<i64>) -> Option<i64> {
    slowest_held.map(|held| {
        let floor = held + FLOOR_MARGIN_RPM;
        driver.map_or(floor, |driver| floor.min(driver))
    })
}

/// Pyren's floor only when asked for, measured, and possible; otherwise the
/// driver's, with its override cleared so the driver enforces its own.
fn floor_in_force(
    driver: Option<i64>,
    pyren: Option<i64>,
    keep_driver: bool,
    override_supported: bool,
) -> Floor {
    if !override_supported {
        return Floor {
            rpm: driver,
            override_hundreds: None,
        };
    }
    match (keep_driver, pyren) {
        // Pyren's floor, but only while it is actually lower than the
        // driver's. Once the stall watch has raised it to meet the
        // driver's there is nothing to override, and the parameter goes
        // back to 0 so the driver enforces its own.
        (false, Some(rpm)) if driver.is_none_or(|d| rpm < d) => Floor {
            rpm: Some(rpm),
            // Rounded up, so the driver's clamp is never below the speed
            // the daemon stops commanding at.
            override_hundreds: Some(((rpm + 99) / 100).clamp(1, 255) as u8),
        },
        _ => Floor {
            rpm: driver,
            override_hundreds: Some(0),
        },
    }
}

/// Keeps the outcome of a control write for `getStatus`, and hands it on.
fn record_write(
    state: &mut State,
    result: Result<(), control::ControlError>,
) -> Result<(), ModuleError> {
    match result {
        Ok(()) => {
            state.last_control_error = None;
            Ok(())
        }
        Err(e) => {
            state.last_control_error = Some(e.to_msg());
            Err(control_error(e))
        }
    }
}

/// Cleaner failures, translated for the socket.
///
/// `notCapable` against `failed` is the distinction that matters here, and
/// it is the one `docs/01-ipc-protocol.md` singles out for `acpi_call`: a
/// missing kernel module is **not** a verdict on the hardware, so it stays
/// `failed` (it names a package to install) while a firmware that answered
/// and said no is `notCapable`.
fn cleaner_error(e: cleaner::CleanerError) -> ModuleError {
    use cleaner::CleanerError as E;
    let kind = match &e {
        E::Acpi(acpi::AcpiError::PermissionDenied) => ErrorKind::PermissionDenied,
        E::Acpi(_) => ErrorKind::Failed,
        E::NotCapable => ErrorKind::NotCapable,
        E::Busy => ErrorKind::Busy,
        // Not `invalidParams`: the caller asked for something reasonable
        // and the machine is in no state for it *right now*.
        E::TooHot(_) | E::NoTemperature => ErrorKind::Failed,
        E::Refused(_) => ErrorKind::Failed,
    };
    ModuleError::localised(kind, e.to_msg())
}

/// Why a curve was refused, as a sentence.
fn curve_problem(problem: curve::CurveProblem) -> Msg {
    use curve::CurveProblem as P;
    match problem {
        P::PointCount(count) => msg!(
            "fan.err.curvePointCount",
            { "count" => count, "min" => curve::MIN_CURVE_POINTS, "max" => curve::MAX_CURVE_POINTS },
            "a curve needs {min} to {max} points; this one has {count}"
        ),
        P::NotFinite { temp_c, percent } => msg!(
            "fan.err.curvePointNotFinite",
            { "temp" => temp_c, "percent" => percent },
            "curve point ({temp}, {percent}) is not a finite number"
        ),
        P::TempOutOfRange(temp) => msg!(
            "fan.err.curveTempRange",
            { "temp" => temp, "max" => curve::MAX_CURVE_TEMP_C },
            "a curve point at {temp} °C is outside 0-{max} °C"
        ),
        P::PercentOutOfRange(percent) => msg!(
            "fan.err.curvePercentRange",
            { "percent" => percent },
            "a curve point at {percent} % is outside 0-100 %"
        ),
        P::Decreasing(temp) => msg!(
            "fan.err.curveDecreasing",
            { "temp" => temp },
            "the curve slows the fans down as the temperature rises, at {temp} °C"
        ),
    }
}

/// The faster fan's speed, or `None` when neither tachometer could be read
/// - which a guard must never take for a fan standing still.
fn fan_rpm_reading(paths: &FanPaths) -> Option<i64> {
    let fan1 = read_raw_rpm(paths.fan1_input.as_deref());
    let fan2 = read_raw_rpm(paths.fan2_input.as_deref());
    if fan1.is_none() && fan2.is_none() {
        return None;
    }
    let (rpm1, _) = parse_hwmon_rpm(fan1);
    let (rpm2, _) = parse_hwmon_rpm(fan2);
    Some(rpm1.max(rpm2))
}

/// Human-readable version of what the driver offers, for an error the user
/// will actually read.
fn describe(caps: Capabilities) -> Msg {
    match (caps.switch_mode, caps.set_speed) {
        (true, true) => msg!("fan.caps.both", "both pwm1 and pwm1_enable"),
        (true, false) => msg!(
            "fan.caps.switchOnly",
            "pwm1_enable but no pwm1, so only auto and max are possible"
        ),
        (false, true) => msg!("fan.caps.speedOnly", "pwm1 but no pwm1_enable"),
        (false, false) => msg!("fan.caps.none", "no fan control interface at all"),
    }
}

/// The mode the hardware is in, translated into ours. The driver cannot
/// distinguish manual from curve - a curve is manual values that keep
/// changing - so a machine found in manual reports manual.
/// Hands a fresh measurement to the driver, permanently.
///
/// Without this a calibration only ever reached *this* daemon's config:
/// the driver went on converting between pwm and rpm against a ceiling it
/// had guessed, or one the firmware claimed, and there was no way to
/// correct it short of rebuilding the kernel module. The measurement now
/// goes into `/etc/modprobe.d`, which the driver reads on every load - so
/// it survives a reboot, a DKMS rebuild and a kernel upgrade alike.
///
/// Deliberately does **not** reload `hp-wmi` to make it take effect at
/// once. That would recreate the hwmon directory under this process's
/// feet, leaving every cached sysfs path here pointing at a file that no
/// longer exists - fan control silently broken until the daemon is
/// restarted. The value takes effect at the next load instead, and the
/// driver installer, which is reloading anyway, is where "now" happens.
fn pin_ceiling(calibration: &calibration::Calibration) -> Result<String, String> {
    let max_rpm = pyren_installer::MaxRpm {
        cpu: calibration
            .fan1_max_rpm
            .and_then(|rpm| u32::try_from(rpm).ok()),
        gpu: calibration
            .fan2_max_rpm
            .and_then(|rpm| u32::try_from(rpm).ok()),
    };
    match pyren_installer::pin_measured_ceiling(max_rpm) {
        Ok(detail) => {
            log_info!("pinned the measured fan ceiling for the driver: {detail}");
            Ok(detail)
        }
        Err(e) => {
            // Never fails the calibration itself: the measurement is
            // taken, stored and usable by the curve either way. Running
            // unprivileged is the ordinary reason to land here.
            log_warn!("could not pin the measured fan ceiling for the driver: {e}");
            Err(e)
        }
    }
}

/// Same reasoning as [`lock`]: a poisoned hardware lock means a thread
/// panicked while swapping paths, and refusing to read them afterwards
/// would turn a recoverable mishap into a dead fan module.
fn lock_hw(hardware: &Arc<Mutex<Hardware>>) -> std::sync::MutexGuard<'_, Hardware> {
    hardware.lock().unwrap_or_else(|e| e.into_inner())
}

fn observed_mode(paths: &FanPaths) -> Option<FanMode> {
    match control::read_hardware_mode(paths)? {
        0 => Some(FanMode::Max),
        1 => Some(FanMode::Manual),
        2 => Some(FanMode::Auto),
        _ => None,
    }
}

fn persist(store: &ConfigStore, state: &mut State) {
    match store.save("fan", &state.config) {
        Ok(()) => state.last_save_error = None,
        Err(e) => {
            log_warn!("could not save fan config: {e}");
            state.last_save_error = Some(e.to_string());
        }
    }
}

fn lock(state: &Arc<Mutex<State>>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Seconds since the process started. The hysteresis only ever compares
/// two of these, so the origin does not matter - only that it cannot jump
/// when the wall clock does.
fn monotonic_secs() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs()
}

/// Wall-clock seconds since the epoch, for a record that outlives the
/// process. 0 if the clock is somehow before 1970, which a reader will
/// render as a very old age rather than crash on.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn discover_paths() -> FanPaths {
    let mut paths = FanPaths::default();

    if let Some(hwmon_dir) = find_hp_wmi_hwmon_dir() {
        paths.pwm1 = Some(hwmon_dir.join("pwm1"));
        paths.pwm2 = Some(hwmon_dir.join("pwm2"));
        paths.pwm1_enable = Some(hwmon_dir.join("pwm1_enable"));
        paths.fan1_input = Some(hwmon_dir.join("fan1_input"));
        paths.fan2_input = Some(hwmon_dir.join("fan2_input"));
        paths.hwmon_dir = Some(hwmon_dir);
    }

    paths.cpu_temp = find_cpu_temp_path();
    paths.gpu_temp = find_gpu_temp_path();
    paths.driver_params = find_driver_params();
    paths
}

/// `hp_wmi`'s module parameters. Under `PYREN_HWMON_DIR` they are looked
/// for beside the fixture instead: a self-test pointed at a fixture must
/// not read - let alone write - the real driver's floor.
fn find_driver_params() -> Option<PathBuf> {
    let dir = match std::env::var("PYREN_HWMON_DIR") {
        Ok(fixture) => PathBuf::from(fixture).join("parameters"),
        Err(_) => PathBuf::from("/sys/module/hp_wmi/parameters"),
    };
    dir.is_dir().then_some(dir)
}

/// Mirrors `FanController._find_paths` (`glob.glob(HWMON_PATH_PATTERN)`
/// taking the first match) in the Python original.
///
/// `PYREN_HWMON_DIR` overrides the search, which is how the self-test
/// can be exercised against a fixture directory on hardware that has no
/// hp-wmi - including in CI.
fn find_hp_wmi_hwmon_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("PYREN_HWMON_DIR") {
        let dir = PathBuf::from(dir);
        return dir.is_dir().then_some(dir);
    }

    fs::read_dir(HWMON_ROOT)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
}

/// Mirrors `FanController._find_cpu_temp_path`: prefer `coretemp`/`k10temp`
/// hwmon drivers, fall back to `thermal_zone0`. Lives in
/// [`pyren_core::sensors`] because the power supervisor wants the same
/// sensor for its own reason.
fn find_cpu_temp_path() -> Option<PathBuf> {
    pyren_core::sensors::cpu_temp_path()
}

/// The GPU's temperature sensor, when hwmon publishes one - see
/// [`pyren_core::sensors::gpu_temp_path`] for which drivers count and why
/// it is a named list rather than "the first temperature I find".
fn find_gpu_temp_path() -> Option<PathBuf> {
    pyren_core::sensors::gpu_temp_path()
}

/// Which temperature a curve should follow, and which sensor it came from.
///
/// The fallback is one-directional on purpose. A `gpu` setting means "the
/// GPU when it has something to say", and a card at 0 C is a card that is
/// powered down, not a cold one - so the CPU stands in. A `cpu` setting
/// never falls back to the GPU: someone who picked the CPU and silently
/// got a curve driven by the other sensor would be looking at a machine
/// nobody asked for.
fn reference_temp(
    cpu_temp_c: Option<i64>,
    gpu_temp_c: Option<i64>,
    sensor: ReferenceSensor,
) -> Option<(i64, ReferenceSensor)> {
    if sensor == ReferenceSensor::Gpu {
        if let Some(gpu) = gpu_temp_c.filter(|t| *t > 0) {
            return Some((gpu, ReferenceSensor::Gpu));
        }
    }
    cpu_temp_c.map(|cpu| (cpu, ReferenceSensor::Cpu))
}

/// sysfs temperature files report millidegrees C.
fn read_millideg_c(path: &Path) -> Option<i64> {
    pyren_core::sensors::read_millideg_c(path)
}

fn read_raw_rpm(path: Option<&Path>) -> Option<i64> {
    fs::read_to_string(path?).ok()?.trim().parse::<i64>().ok()
}

/// Mirrors `FanController.parse_hwmon_rpm`: hp-wmi encodes fan-cleaner
/// reverse-spin state in the fan?_input value itself (bit 7 of a
/// hundred-RPM-unit byte, i.e. raw value >= 12800). See
/// docs/02-kernel-driver.md ("The reverse-bit / fan-cleaner RPM encoding")
/// in the source repo for why this looks the way it does.
fn parse_hwmon_rpm(raw: Option<i64>) -> (i64, bool) {
    match raw {
        None => (0, false),
        Some(raw_rpm) if raw_rpm >= 12800 => {
            let reverse_bit_speed = raw_rpm / 100;
            let actual_speed = (reverse_bit_speed & 0x7F) * 100;
            (actual_speed, true)
        }
        Some(raw_rpm) => (raw_rpm, false),
    }
}

/// Mirrors `FanController.get_fan_speed_info`: report whichever fan reads
/// faster, and whether either fan is currently in reverse.
fn read_fan_rpm(fan1: Option<&Path>, fan2: Option<&Path>) -> (i64, bool) {
    let (rpm1, rev1) = parse_hwmon_rpm(read_raw_rpm(fan1));
    let (rpm2, rev2) = parse_hwmon_rpm(read_raw_rpm(fan2));
    (rpm1.max(rpm2), rev1 || rev2)
}

/// Per-fan tachometer readings, labelled by the cooler each drives. OMEN
/// boards wire `fan1` to the CPU and `fan2` to the GPU — the same mapping
/// `fan1_max_rpm` / `fan2_max_rpm` (`OMEN_CPU_MAX_RPM` / `OMEN_GPU_MAX_RPM`)
/// already assume. A fan whose input file is absent (single-fan machines)
/// is left out rather than reported as 0. `fanRpm` above stays the summary;
/// this is the breakdown for a UI that wants to name each one.
fn read_labelled_fans(fan1: Option<&Path>, fan2: Option<&Path>) -> Vec<Value> {
    [("cpu", fan1), ("gpu", fan2)]
        .into_iter()
        .filter_map(|(key, path)| {
            let (rpm, is_reverse) = parse_hwmon_rpm(Some(read_raw_rpm(path)?));
            Some(json!({ "key": key, "rpm": rpm, "isReverse": is_reverse }))
        })
        .collect()
}

/// Test-only redirection of `PYREN_ACPI_CALL`.
///
/// The variable is process-global and every test binary runs its tests in
/// parallel threads, so four tests each setting it and then *removing* it
/// were unsetting it under one another. That was invisible for as long as
/// the development machine had no `/proc/acpi/call`: the fallback the
/// removal exposed was a path that did not exist either, which is what
/// those tests wanted anyway. On a machine where `acpi_call` is loaded the
/// same race reaches the real firmware interface and the assertions
/// invert. Holding one lock for the whole of each such test, and putting
/// the variable back to whatever it was rather than deleting it, makes the
/// redirection mean the same thing on both machines.
#[cfg(test)]
pub(crate) mod testenv {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct NoAcpiCall {
        // Poisoning is not interesting here - a test that panicked while
        // holding the lock has already failed, and the next test still
        // needs the redirection.
        _guard: MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
        redirected: bool,
    }

    /// Points `acpi_call` at a path that cannot exist, for as long as the
    /// returned guard lives. `dir` is the test's own temp directory, so
    /// two tests never share the name.
    pub(crate) fn without_acpi_call(dir: &Path) -> NoAcpiCall {
        let mut env = real();
        std::env::set_var("PYREN_ACPI_CALL", dir.join("definitely-not-here"));
        env.redirected = true;
        env
    }

    /// Takes the lock without changing anything.
    ///
    /// Every test that *reads* the interface needs this, not only the ones
    /// that redirect it. Two reasons, and the second is the one that bit:
    /// a test reading the real machine while another has the variable
    /// pointed at a missing file gets the wrong answer, and `setenv`
    /// racing a `getenv` on another thread is a data race in the C library
    /// underneath - which is why setting an environment variable is
    /// `unsafe` in the 2024 edition. Serialising both sides is what makes
    /// the redirection sound rather than usually-fine.
    pub(crate) fn real() -> NoAcpiCall {
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        NoAcpiCall {
            _guard: guard,
            previous: std::env::var_os("PYREN_ACPI_CALL"),
            redirected: false,
        }
    }

    impl Drop for NoAcpiCall {
        fn drop(&mut self) {
            if !self.redirected {
                return;
            }
            match self.previous.take() {
                Some(previous) => std::env::set_var("PYREN_ACPI_CALL", previous),
                None => std::env::remove_var("PYREN_ACPI_CALL"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module with no hardware behind it, which is what CI has, and a
    /// config store in a temp directory - `inspector()` would reach for
    /// the real one, and a test that saves a setting into the developer's
    /// home is a test that changes their machine.
    fn module(tag: &str) -> FanModule {
        let root = std::env::temp_dir().join(format!("pyren-fan-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut module = FanModule::inspector();
        module.store = ConfigStore::at(root);
        module
    }

    /// Installing the driver reloads `hp-wmi`, and the hwmon directory
    /// comes back under a different number. Everything this module had
    /// found at startup then names a file that is gone - it went on
    /// answering `getStatus` with a rpm it could no longer read, and the
    /// only fix was restarting the daemon, which the wizard had to ask
    /// for in so many words.
    #[test]
    fn a_renumbered_hwmon_directory_is_picked_up() {
        let module = module("rediscover");
        let moved = FanPaths {
            hwmon_dir: Some(PathBuf::from("/sys/devices/platform/hp-wmi/hwmon/hwmon8")),
            pwm1: Some(PathBuf::from(
                "/sys/devices/platform/hp-wmi/hwmon/hwmon8/pwm1",
            )),
            ..Default::default()
        };

        assert!(
            module.adopt(moved.clone()),
            "a different directory is a change"
        );
        assert_eq!(
            module.paths().pwm1,
            moved.pwm1,
            "and the new paths are the ones kept"
        );
        assert!(!module.adopt(moved), "the same directory twice is not");
    }

    /// The other half of what a driver change can do: same directory, but
    /// the module behind it now accepts something it did not before.
    /// Reporting "nothing changed" there would leave the fan page saying
    /// no speed can be set on a machine where it now can.
    #[test]
    fn gaining_a_control_counts_as_a_change() {
        let module = module("rediscover-caps");
        // Real files, because capabilities are detected by looking: a
        // path that names a pwm1 which is not there is exactly the state
        // this whole mechanism exists to get out of.
        let dir = std::env::temp_dir().join(format!("pyren-hwmon-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let bare = FanPaths {
            hwmon_dir: Some(dir.clone()),
            ..Default::default()
        };
        module.adopt(bare);
        assert!(!module.caps().switch_mode, "nothing to drive yet");

        fs::write(dir.join("pwm1"), b"0").unwrap();
        fs::write(dir.join("pwm1_enable"), b"2").unwrap();
        let with_pwm = FanPaths {
            hwmon_dir: Some(dir.clone()),
            pwm1: Some(dir.join("pwm1")),
            pwm1_enable: Some(dir.join("pwm1_enable")),
            ..Default::default()
        };
        assert!(
            module.adopt(with_pwm),
            "the same directory can still be a different driver"
        );
        assert!(module.caps().switch_mode);

        let _ = fs::remove_dir_all(&dir);
    }

    /// The GPU is what gets hot first under a game, so a curve that can
    /// follow it is the point of the setting.
    #[test]
    fn a_gpu_curve_follows_the_gpu() {
        assert_eq!(
            reference_temp(Some(45), Some(78), ReferenceSensor::Gpu),
            Some((78, ReferenceSensor::Gpu))
        );
    }

    /// A card that is powered down reads 0, and 0 C is not a temperature -
    /// following it would idle the fans on a machine that is working.
    #[test]
    fn a_sleeping_card_hands_the_curve_back_to_the_cpu() {
        assert_eq!(
            reference_temp(Some(45), Some(0), ReferenceSensor::Gpu),
            Some((45, ReferenceSensor::Cpu))
        );
        assert_eq!(
            reference_temp(Some(45), None, ReferenceSensor::Gpu),
            Some((45, ReferenceSensor::Cpu)),
            "a machine with no GPU sensor at all is the same case"
        );
    }

    /// The fallback is one-directional. Someone who picked the CPU and got
    /// a curve driven by the GPU would be looking at a machine nobody
    /// asked for.
    #[test]
    fn choosing_the_cpu_never_silently_becomes_the_gpu() {
        assert_eq!(reference_temp(None, Some(78), ReferenceSensor::Cpu), None);
        assert_eq!(
            reference_temp(Some(45), Some(78), ReferenceSensor::Cpu),
            Some((45, ReferenceSensor::Cpu))
        );
    }

    /// With neither sensor there is nothing to follow, and the caller has
    /// to say so rather than picking a number.
    #[test]
    fn no_sensor_at_all_is_not_a_temperature_of_zero() {
        assert_eq!(reference_temp(None, None, ReferenceSensor::Gpu), None);
    }

    /// The setting survives a round trip through `fan.json` in the shape
    /// the IPC uses, which is the one the frontend sends.
    #[test]
    fn the_reference_sensor_is_persisted_as_a_word() {
        let config = FanConfig {
            reference_sensor: ReferenceSensor::Gpu,
            ..FanConfig::default()
        };
        let text = serde_json::to_string(&config).unwrap();
        assert!(text.contains("\"referenceSensor\":\"gpu\""), "{text}");
        let back: FanConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.reference_sensor, ReferenceSensor::Gpu);
    }

    /// A `fan.json` written before the field existed still loads - the
    /// daemon and the app are separate binaries and are not always
    /// updated together.
    #[test]
    fn a_config_without_the_field_defaults_to_the_cpu() {
        let back: FanConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(back.reference_sensor, ReferenceSensor::Cpu);
    }

    #[test]
    fn setting_the_curve_can_change_the_sensor_and_refuses_a_word_it_does_not_know() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module("sensor");
        let curve = json!([
            { "tempC": 40.0, "percent": 20.0 },
            { "tempC": 85.0, "percent": 100.0 },
        ]);

        let status = module
            .call(
                "setCurve",
                json!({ "curve": curve, "referenceSensor": "gpu" }),
            )
            .expect("the curve is legal and so is the sensor");
        assert_eq!(status["referenceSensor"], json!("gpu"));

        let error = module
            .call(
                "setCurve",
                json!({ "curve": curve, "referenceSensor": "chassis" }),
            )
            .expect_err("there is no chassis sensor");
        assert_eq!(error.kind(), ErrorKind::InvalidParams);
        // ...and the refused call changed nothing.
        assert_eq!(module.status()["referenceSensor"], json!("gpu"));
    }

    /// Every one of these has to be answerable without `acpi_call`,
    /// because that is the machine most people run this on - and a status
    /// call that failed there would take the whole page down with it.
    #[test]
    fn the_cleaner_status_answers_on_a_machine_with_no_acpi_call() {
        let dir = std::env::temp_dir().join(format!("pyren-fan-cleaner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _no_acpi = crate::testenv::without_acpi_call(&dir);

        let module = module("status");
        let status = module.cleaner_status(true);
        assert_eq!(status["supported"], json!(false));
        assert_eq!(status["running"], json!(false));
        assert_eq!(status["acpiCallLoaded"], json!(false));
        assert!(
            status["unreachable"].is_object(),
            "it says why, and the sentence is translatable"
        );
        // Reported even when nothing can be driven: the page shows the
        // limit next to the temperature, so the two arrive together.
        assert_eq!(status["maxStartTempC"], json!(cleaner::MAX_START_TEMP_C));

        // Nothing to stop is the state the caller asked for, not an error.
        assert!(module.stop_cleaning().is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The refusal a machine without the kernel module gets. It must not
    /// be `notCapable`: that would tell someone their laptop cannot do
    /// this when what it needs is a package.
    #[test]
    fn a_missing_acpi_call_is_a_failure_with_a_remedy_not_a_verdict() {
        let dir = std::env::temp_dir().join(format!("pyren-fan-start-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _no_acpi = crate::testenv::without_acpi_call(&dir);

        let module = module("start");
        let error = module
            .start_cleaning(None, None, false)
            .expect_err("nothing to talk to");
        assert_ne!(
            error.kind(),
            ErrorKind::NotCapable,
            "a package to install is not a verdict on the hardware"
        );
        assert!(
            error.as_msg().contains("acpi_call"),
            "the message names the module: {}",
            error.as_msg()
        );

        // A failed start leaves nothing claimed - the next attempt must
        // not be refused as busy by the one that never began.
        assert!(lock(&module.state).cleaning.is_idle());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both transitional states hold the fans. This is the invariant the
    /// control loop reads, and getting it wrong means `pwm1` writes
    /// landing in the middle of a reverse ramp.
    #[test]
    fn the_control_loop_stands_off_through_both_transitions() {
        for state in [Cleaning::Starting, Cleaning::Stopping] {
            assert!(
                state.holds_the_fans(),
                "{state:?} must stop the control loop writing"
            );
            assert!(state.cycle().is_none(), "{state:?} is not a running cycle");
        }
        assert!(!Cleaning::Idle.holds_the_fans());
    }

    /// The duration is a stored preference, and it is clamped where it is
    /// stored rather than where it is used - so a bad value cannot sit in
    /// the config file waiting for the next start.
    #[test]
    fn a_stored_cleaner_duration_is_clamped_on_the_way_in() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module("config");
        let status = module
            .call("setCleanerConfig", json!({ "seconds": 6000, "speed": 99 }))
            .expect("setting the config never touches hardware");
        assert_eq!(status["durationSecs"], json!(cleaner::MAX_DURATION_SECS));
        assert_eq!(status["configuredSpeed"], json!(cleaner::MAX_SPEED));

        // Null is how a client goes back to the firmware's own speeds,
        // and it has to be distinguishable from "did not say".
        let status = module
            .call("setCleanerConfig", json!({ "speed": Value::Null }))
            .expect("null is a value here");
        assert_eq!(status["configuredSpeed"], json!(null));
        assert_eq!(
            status["durationSecs"],
            json!(cleaner::MAX_DURATION_SECS),
            "an omitted field is left alone rather than reset"
        );
    }

    #[test]
    fn a_fan_status_says_whether_the_fans_are_the_cleaners() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module("owns");
        assert_eq!(module.status()["cleaning"], json!(false));
        lock(&module.state).cleaning = Cleaning::Running(cleaner::Cycle {
            generation: cleaner::Generation::Modern,
            started: Instant::now(),
            duration: Duration::from_secs(30),
            id: 1,
            cpu_speed: 37,
            gpu_speed: 39,
        });
        assert_eq!(module.status()["cleaning"], json!(true));
    }

    /// Board 8D2F: `pwm1` is there, the driver takes the write, the fans
    /// never move. Once a probe has watched that happen, every client has
    /// to stop being told a speed can be set - otherwise the app goes on
    /// drawing a curve editor for a curve nothing follows, which is the
    /// bug that started this.
    #[test]
    fn a_measured_refusal_takes_speed_control_out_of_the_capabilities() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module("ignored-speed");
        let dir = std::env::temp_dir().join(format!("pyren-fan-caps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["pwm1", "pwm1_enable", "fan1_input"] {
            std::fs::write(dir.join(f), "2\n").unwrap();
        }
        let paths = FanPaths {
            hwmon_dir: Some(dir.clone()),
            pwm1: Some(dir.join("pwm1")),
            pwm2: None,
            pwm1_enable: Some(dir.join("pwm1_enable")),
            fan1_input: Some(dir.join("fan1_input")),
            fan2_input: None,
            cpu_temp: None,
            gpu_temp: None,
            driver_params: None,
        };
        *lock_hw(&module.hardware) = Hardware {
            caps: Capabilities::detect(&paths),
            paths,
        };

        // Untested is the default, and it offers speed control: most boards
        // that expose pwm1 do honour it.
        assert!(module.capabilities().set_speed);
        assert_eq!(module.status()["speedControl"], json!("untested"));
        assert_eq!(module.status()["capabilities"]["setSpeed"], json!(true));

        lock(&module.state).config.speed_control = SpeedControl::Ignored;

        assert!(
            !module.capabilities().set_speed,
            "a watched refusal must reach pyren-check too"
        );
        assert_eq!(module.status()["speedControl"], json!("ignored"));
        assert_eq!(module.status()["capabilities"]["setSpeed"], json!(false));
        // ...and the modes that need a speed are refused, with a reason that
        // is not "install a driver" - the driver is already there.
        let err = module
            .set_mode(FanMode::Curve, None)
            .expect_err("curve must be refused");
        assert!(format!("{err:?}").contains("speedIgnored"), "got {err:?}");
        // The two that go through a different firmware call still work.
        assert!(module.caps().supports(FanMode::Max));
        assert!(module.set_mode(FanMode::Auto, None).is_ok());
    }

    /// The widget and the app's fan page stay in step with a mode changed
    /// behind their back only because `set_mode` says so on the bus. Its
    /// shape - `mode`, and the `manualPwm` a speedless widget needs to
    /// place its slider - is part of the contract.
    #[test]
    fn setting_the_fan_mode_is_announced_on_the_bus() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module("announce-mode");

        let dir = std::env::temp_dir().join(format!("pyren-fan-announce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["pwm1", "pwm1_enable", "fan1_input"] {
            std::fs::write(dir.join(f), "2\n").unwrap();
        }
        let paths = FanPaths {
            hwmon_dir: Some(dir.clone()),
            pwm1: Some(dir.join("pwm1")),
            pwm2: None,
            pwm1_enable: Some(dir.join("pwm1_enable")),
            fan1_input: Some(dir.join("fan1_input")),
            fan2_input: None,
            cpu_temp: None,
            gpu_temp: None,
            driver_params: None,
        };
        *lock_hw(&module.hardware) = Hardware {
            caps: Capabilities::detect(&paths),
            paths,
        };

        let bus = Arc::new(pyren_core::EventBus::new());
        module.publish_to(Arc::clone(&bus));

        module
            .set_mode(FanMode::Manual, Some(120))
            .expect("manual is commandable here");

        let batch = bus.read_since(0, Duration::from_millis(0));
        let mode_events: Vec<_> = batch
            .events
            .iter()
            .filter(|e| e.topic == "fan.mode")
            .collect();
        assert_eq!(mode_events.len(), 1, "exactly one fan.mode per change");
        assert_eq!(mode_events[0].payload["mode"], "manual");
        assert_eq!(mode_events[0].payload["manualPwm"], 120);
    }

    /// The inconclusive verdicts exist so that a probe which learned nothing
    /// cannot erase one that did.
    #[test]
    fn an_inconclusive_probe_does_not_overwrite_a_stored_answer() {
        use crate::speed_probe::Verdict;
        assert_eq!(Option::<SpeedControl>::from(Verdict::NoReading), None);
        assert_eq!(Option::<SpeedControl>::from(Verdict::NoChannel), None);
        assert_eq!(
            Option::<SpeedControl>::from(Verdict::Ignored),
            Some(SpeedControl::Ignored)
        );
    }

    /// A curve whose distinguishing point is `pairs`, finished with a
    /// full-speed point so every shape rises and has at least two points.
    fn points(pairs: &[(f64, f64)]) -> Vec<CurvePoint> {
        pairs
            .iter()
            .chain(&[(85.0, 100.0)])
            .map(|(t, p)| CurvePoint {
                temp_c: *t,
                percent: *p,
            })
            .collect()
    }

    /// The point of the whole feature: two profiles, two shapes, and the
    /// one that drives the fans follows the machine.
    #[test]
    fn each_profile_keeps_its_own_curve_and_the_active_one_drives() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module("per-profile");

        module.set_active_profile("eco");
        module
            .set_curve(points(&[(40.0, 10.0)]), None, None, None)
            .unwrap();
        module.set_active_profile("performance");
        module
            .set_curve(points(&[(40.0, 90.0)]), None, None, None)
            .unwrap();

        // Each was stored where it belongs, not over the other.
        let state = lock(&module.state);
        assert_eq!(state.config.profile_curves["eco"], points(&[(40.0, 10.0)]));
        assert_eq!(
            state.config.profile_curves["performance"],
            points(&[(40.0, 90.0)])
        );
        drop(state);

        // ...and `curve` in the status is whichever one is in force.
        assert_eq!(module.active_profile().as_deref(), Some("performance"));
        assert_eq!(module.status()["curve"], json!(points(&[(40.0, 90.0)])));
        module.set_active_profile("eco");
        assert_eq!(module.status()["curve"], json!(points(&[(40.0, 10.0)])));
    }

    /// A client that has never heard of profiles edits the one running,
    /// which is the only one it can see.
    #[test]
    fn a_curve_written_without_a_profile_edits_the_one_in_force() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module("implicit-profile");
        module.set_active_profile("balanced");
        module
            .set_curve(points(&[(50.0, 42.0)]), None, None, None)
            .unwrap();

        let state = lock(&module.state);
        assert_eq!(
            state.config.profile_curves["balanced"],
            points(&[(50.0, 42.0)])
        );
        assert!(
            state.config.curve.is_empty(),
            "the shared fallback is not the one being edited"
        );
    }

    /// A machine whose power module controls nothing never announces a
    /// profile, and must still have a working curve rather than none.
    #[test]
    fn with_no_profile_announced_the_shared_curve_is_used_and_edited() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module("no-profile");
        assert_eq!(module.active_profile(), None);

        module
            .set_curve(points(&[(60.0, 55.0)]), None, None, None)
            .unwrap();
        let state = lock(&module.state);
        assert_eq!(state.config.curve, points(&[(60.0, 55.0)]));
        assert!(state.config.profile_curves.is_empty());
        assert_eq!(state.config.curve_for(None), points(&[(60.0, 55.0)]));
    }

    /// Upgrading must not quietly discard the curve someone already tuned:
    /// every profile starts as a copy of it, then diverges by editing.
    #[test]
    fn the_existing_curve_seeds_every_profile_rather_than_vanishing() {
        let mut config = FanConfig {
            curve: points(&[(45.0, 30.0)]),
            ..FanConfig::default()
        };
        assert!(config.migrate_profile_curves(&["eco", "balanced"]));
        assert_eq!(config.profile_curves["eco"], points(&[(45.0, 30.0)]));
        assert_eq!(config.profile_curves["balanced"], points(&[(45.0, 30.0)]));

        // Seeding runs once per profile and never overwrites a real curve.
        config
            .profile_curves
            .insert("eco".into(), points(&[(70.0, 100.0)]));
        assert!(!config.migrate_profile_curves(&["eco", "balanced"]));
        assert_eq!(config.profile_curves["eco"], points(&[(70.0, 100.0)]));
    }

    /// A profile nobody has drawn for - and any name this build has never
    /// heard of - falls back rather than leaving the fans with no curve.
    #[test]
    fn an_undrawn_or_unknown_profile_falls_back_to_the_shared_curve() {
        let mut config = FanConfig {
            curve: points(&[(50.0, 40.0)]),
            ..FanConfig::default()
        };
        config
            .profile_curves
            .insert("eco".into(), points(&[(50.0, 10.0)]));

        assert_eq!(config.curve_for(Some("eco")), points(&[(50.0, 10.0)]));
        assert_eq!(config.curve_for(Some("balanced")), points(&[(50.0, 40.0)]));
        assert_eq!(
            config.curve_for(Some("turbo-plus")),
            points(&[(50.0, 40.0)])
        );
        assert_eq!(config.curve_for(None), points(&[(50.0, 40.0)]));

        // An empty stored curve is not a curve of "no fan at all".
        config.profile_curves.insert("balanced".into(), Vec::new());
        assert_eq!(config.curve_for(Some("balanced")), points(&[(50.0, 40.0)]));
    }

    /// `profile: ""` is the only way to reach the fallback explicitly, and
    /// `pyren-ctl --profile shared` depends on it.
    #[test]
    fn an_empty_profile_name_edits_the_shared_curve() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module("shared-curve");
        module.set_active_profile("eco");
        module
            .set_curve(points(&[(55.0, 65.0)]), None, None, Some(""))
            .unwrap();

        let state = lock(&module.state);
        assert_eq!(state.config.curve, points(&[(55.0, 65.0)]));
        assert!(!state.config.profile_curves.contains_key("eco"));
    }

    /// Old `fan.json` files predate the field entirely, and must not come
    /// back as "this machine refuses speeds".
    #[test]
    fn a_config_without_the_speed_control_field_is_untested_not_ignored() {
        let config: FanConfig = serde_json::from_value(json!({ "mode": "auto" })).unwrap();
        assert_eq!(config.speed_control, SpeedControl::Untested);
        assert!(!config.speed_control.is_ignored());
    }

    /// The per-cooler breakdown: fan1 is the CPU, fan2 the GPU, each
    /// decoded through the reverse-bit encoding, and a fan whose input
    /// file is absent is left out rather than reported as a stopped one.
    #[test]
    fn labelled_fans_name_each_cooler_and_skip_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("pyren-labelled-fans-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("fan1_input"), "2400").unwrap();
        // Reverse-bit encoded: raw >= 12800 means 3100 rpm, spinning backwards.
        fs::write(dir.join("fan2_input"), format!("{}", 12800 + 3100)).unwrap();

        let both = read_labelled_fans(Some(&dir.join("fan1_input")), Some(&dir.join("fan2_input")));
        assert_eq!(
            both,
            vec![
                json!({ "key": "cpu", "rpm": 2400, "isReverse": false }),
                json!({ "key": "gpu", "rpm": 3100, "isReverse": true }),
            ],
        );

        // A single-fan machine: fan2's path is discovered but the file is
        // not there, so only the CPU entry comes back.
        let one = read_labelled_fans(
            Some(&dir.join("fan1_input")),
            Some(&dir.join("fan2_input.missing")),
        );
        assert_eq!(
            one,
            vec![json!({ "key": "cpu", "rpm": 2400, "isReverse": false })]
        );

        // No hp-wmi hwmon at all: an empty list, never a fake reading.
        assert_eq!(read_labelled_fans(None, None), Vec::<Value>::new());

        let _ = fs::remove_dir_all(&dir);
    }

    /// Fans on a fixture directory, driven by this module in curve mode,
    /// with the floor board 8D2F measures: 1800 of 5300 rpm.
    fn driven_on_a_fixture(tag: &str, temp_c: i64) -> (FanModule, PathBuf) {
        let module = module(tag);
        let dir =
            std::env::temp_dir().join(format!("pyren-fan-release-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (file, value) in [
            ("pwm1", "0"),
            ("pwm2", "0"),
            ("pwm1_enable", "2"),
            ("fan1_input", "0"),
        ] {
            fs::write(dir.join(file), value).unwrap();
        }
        fs::write(dir.join("temp1_input"), format!("{}", temp_c * 1000)).unwrap();
        // Pyren's driver: the table's floor reported, the override cleared.
        fs::create_dir_all(dir.join("parameters")).unwrap();
        fs::write(dir.join("parameters/min_rpm_table"), "18").unwrap();
        fs::write(dir.join("parameters/min_rpm_override"), "0").unwrap();
        let paths = FanPaths {
            hwmon_dir: Some(dir.clone()),
            pwm1: Some(dir.join("pwm1")),
            pwm2: Some(dir.join("pwm2")),
            pwm1_enable: Some(dir.join("pwm1_enable")),
            fan1_input: Some(dir.join("fan1_input")),
            cpu_temp: Some(dir.join("temp1_input")),
            driver_params: Some(dir.join("parameters")),
            ..Default::default()
        };
        *lock_hw(&module.hardware) = Hardware {
            caps: Capabilities::detect(&paths),
            paths,
        };

        let mut state = lock(&module.state);
        state.owned = true;
        state.mode = FanMode::Curve;
        state.config.fan_min_rpm = Some(1800);
        state.config.fan_max_rpm = Some(5300);
        state.config.curve = points(&[(40.0, 0.0), (60.0, 50.0), (80.0, 100.0)]);
        drop(state);
        (module, dir)
    }

    fn read_file(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name))
            .unwrap()
            .trim()
            .to_string()
    }

    /// A curve asking for less than the fans can hold hands them to the
    /// firmware, which is what stops them - and says so in the status.
    #[test]
    fn a_curve_below_the_floor_hands_the_fans_to_the_firmware() {
        let (module, dir) = driven_on_a_fixture("below", 40);
        fs::write(dir.join("pwm1_enable"), "1").unwrap();

        module.tick_once().unwrap();

        assert_eq!(read_file(&dir, "pwm1_enable"), "2");
        assert_eq!(module.status()["fansReleased"], json!(true));
        assert_eq!(
            module.status()["mode"],
            json!("curve"),
            "the mode is still the user's"
        );
    }

    /// And takes them back, manual first, once the curve climbs clear.
    #[test]
    fn a_curve_that_climbs_back_takes_the_fans_again() {
        let (module, dir) = driven_on_a_fixture("climb", 40);
        module.tick_once().unwrap();
        assert!(lock(&module.state).released);

        fs::write(dir.join("temp1_input"), "60000").unwrap();
        // A fresh window, so the average is the new temperature rather
        // than a blend with the old one.
        lock(&module.state).smoother = curve::TempSmoother::new(1);
        module.tick_once().unwrap();

        assert_eq!(read_file(&dir, "pwm1_enable"), "1");
        assert_eq!(read_file(&dir, "pwm1"), "128");
        assert_eq!(read_file(&dir, "pwm2"), "128");
        assert_eq!(module.status()["fansReleased"], json!(false));
    }

    /// Without a measured floor 0 % stays the slowest commanded speed.
    #[test]
    fn without_a_floor_a_low_curve_still_commands_a_speed() {
        let (module, dir) = driven_on_a_fixture("nofloor", 40);
        lock(&module.state).config.fan_min_rpm = None;
        // A driver that read no fan table reports no floor either.
        fs::write(dir.join("parameters/min_rpm_table"), "0").unwrap();

        module.tick_once().unwrap();

        assert_eq!(read_file(&dir, "pwm1_enable"), "1");
        assert_eq!(
            read_file(&dir, "pwm1"),
            curve::MIN_COMMANDED_PWM.to_string()
        );
    }

    #[test]
    fn the_drivers_floor_is_the_default() {
        let floor = floor_in_force(Some(1800), Some(700), true, true);
        assert_eq!(
            floor,
            Floor {
                rpm: Some(1800),
                override_hundreds: Some(0)
            }
        );
    }

    #[test]
    fn pyrens_floor_is_used_when_asked_for_and_measured() {
        let floor = floor_in_force(Some(1800), Some(700), false, true);
        assert_eq!(
            floor,
            Floor {
                rpm: Some(700),
                override_hundreds: Some(7)
            }
        );
    }

    /// Asked for, but never measured: there is no lower floor to use.
    #[test]
    fn an_unmeasured_pyren_floor_falls_back_to_the_drivers() {
        assert_eq!(
            floor_in_force(Some(1800), None, false, true).rpm,
            Some(1800)
        );
    }

    /// A driver that cannot be told another floor enforces its own, and
    /// the daemon must not pretend otherwise.
    #[test]
    fn a_driver_without_the_override_keeps_its_floor_whatever_is_chosen() {
        let floor = floor_in_force(Some(1800), Some(700), false, false);
        assert_eq!(
            floor,
            Floor {
                rpm: Some(1800),
                override_hundreds: None
            }
        );
    }

    /// The clamp is rounded up, never below the speed the daemon commands.
    #[test]
    fn the_override_rounds_up_to_the_next_hundred() {
        assert_eq!(
            floor_in_force(Some(1800), Some(750), false, true).override_hundreds,
            Some(8)
        );
    }

    /// With Pyren's floor, a speed the driver's would have handed to the
    /// firmware is commanded - and the driver is told to allow it.
    #[test]
    fn pyrens_floor_commands_what_the_drivers_would_have_released() {
        // 50 C on 40:0 -> 60:50 is 25 %: pwm 64, 1300 rpm of 5300.
        let (module, dir) = driven_on_a_fixture("pyren-floor", 50);
        {
            let mut state = lock(&module.state);
            state.config.fan_stable_min_rpm = Some(600);
            state.config.keep_driver_floor = false;
        }

        module.tick_once().unwrap();

        assert_eq!(read_file(&dir, "parameters/min_rpm_override"), "7");
        assert_eq!(read_file(&dir, "pwm1_enable"), "1");
        assert_eq!(read_file(&dir, "pwm1"), "64");
        assert_eq!(module.status()["fanMinRpm"], json!(700));
    }

    /// The same speed with the driver's floor kept is below it.
    #[test]
    fn the_drivers_floor_releases_that_same_speed() {
        let (module, dir) = driven_on_a_fixture("driver-floor", 50);
        lock(&module.state).config.fan_stable_min_rpm = Some(600);

        module.tick_once().unwrap();

        assert_eq!(read_file(&dir, "pwm1_enable"), "2");
        assert_eq!(read_file(&dir, "parameters/min_rpm_override"), "0");
        assert_eq!(module.status()["fanMinRpm"], json!(1800));
    }

    /// Flipping the setting reaches the driver now, and re-decides.
    #[test]
    fn choosing_the_floor_takes_effect_at_once() {
        let (module, dir) = driven_on_a_fixture("toggle-floor", 50);
        lock(&module.state).config.fan_stable_min_rpm = Some(600);
        module.tick_once().unwrap();
        assert_eq!(
            read_file(&dir, "pwm1_enable"),
            "2",
            "released under the driver's floor"
        );

        let status = module.set_keep_driver_floor(false).unwrap();

        assert_eq!(read_file(&dir, "parameters/min_rpm_override"), "7");
        assert_eq!(
            read_file(&dir, "pwm1_enable"),
            "1",
            "commanded under Pyren's"
        );
        assert_eq!(status["keepDriverFloor"], json!(false));
        assert_eq!(status["pyrenMinRpm"], json!(700));
        assert_eq!(status["driverMinRpm"], json!(1800));

        module.set_keep_driver_floor(true).unwrap();
        assert_eq!(read_file(&dir, "parameters/min_rpm_override"), "0");
        assert_eq!(read_file(&dir, "pwm1_enable"), "2");
    }

    /// A reload resets the driver's parameter; the next tick restores it.
    #[test]
    fn a_reset_override_is_put_back_before_the_next_speed() {
        let (module, dir) = driven_on_a_fixture("override-reset", 60);
        {
            let mut state = lock(&module.state);
            state.config.fan_stable_min_rpm = Some(600);
            state.config.keep_driver_floor = false;
        }
        module.tick_once().unwrap();
        fs::write(dir.join("parameters/min_rpm_override"), "0").unwrap();

        module.tick_once().unwrap();

        assert_eq!(read_file(&dir, "parameters/min_rpm_override"), "7");
    }

    /// 600 held, so 700: the edge plus one fine step.
    #[test]
    fn pyrens_floor_is_a_step_above_the_slowest_speed_held() {
        assert_eq!(pyren_floor(Some(600), Some(1800)), Some(700));
        assert_eq!(pyren_floor(None, Some(1800)), None);
    }

    /// Held only just below the driver's floor: the margin cannot lift it
    /// past the driver's.
    #[test]
    fn pyrens_floor_never_rises_above_the_drivers() {
        assert_eq!(pyren_floor(Some(1750), Some(1800)), Some(1800));
    }

    /// The fans keep giving out at Pyren's floor: three stalls in the
    /// window and the daemon raises the floor a step, records it, and
    /// tells the driver.
    #[test]
    fn repeated_stalls_raise_pyrens_floor_and_leave_a_notice() {
        let (module, dir) = driven_on_a_fixture("stall-raise", 40);
        {
            let mut state = lock(&module.state);
            state.mode = FanMode::Manual;
            state.config.keep_driver_floor = false;
            state.config.fan_stable_min_rpm = Some(600); // Pyren's floor: 700
                                                         // A commanded speed sitting on that floor: ~34/255 of 5300.
            state.config.manual_pwm = 34;
        }

        // First tick commands the speed; the fans are allowed to be
        // catching up, so it is not a fault.
        fs::write(dir.join("fan1_input"), "700").unwrap();
        module.tick_once().unwrap();
        assert_eq!(module.status()["floorNotices"].as_array().unwrap().len(), 0);

        // Now they stall. Three ticks reading zero while a steady speed is
        // commanded.
        fs::write(dir.join("fan1_input"), "0").unwrap();
        module.tick_once().unwrap();
        module.tick_once().unwrap();
        assert_eq!(
            module.status()["recentFanStalls"],
            json!(2),
            "counted, not yet acted on"
        );
        module.tick_once().unwrap();

        let status = module.status();
        let notices = status["floorNotices"].as_array().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["raisedFromRpm"], json!(700));
        assert_eq!(notices[0]["raisedToRpm"], json!(800));
        assert_eq!(notices[0]["stalls"], json!(3));
        assert_eq!(notices[0]["reachedDriverFloor"], json!(false));
        assert_eq!(
            status["fanMinRpm"],
            json!(800),
            "Pyren's floor moved up with it"
        );
        assert_eq!(
            read_file(&dir, "parameters/min_rpm_override"),
            "8",
            "and the driver was told"
        );
        assert_eq!(lock(&module.state).config.fan_stable_min_rpm, Some(700));
    }

    /// On the driver's floor, the watch does not run at all - the driver
    /// clamps there and the firmware curve starts there, so our commands
    /// cannot stall the fans.
    #[test]
    fn the_watch_is_quiet_on_the_drivers_floor() {
        let (module, dir) = driven_on_a_fixture("stall-driver-floor", 40);
        {
            let mut state = lock(&module.state);
            state.mode = FanMode::Manual;
            state.config.keep_driver_floor = true;
            state.config.fan_stable_min_rpm = Some(600);
            state.config.manual_pwm = 34;
        }
        fs::write(dir.join("fan1_input"), "0").unwrap();
        for _ in 0..5 {
            module.tick_once().unwrap();
        }
        assert_eq!(module.status()["recentFanStalls"], json!(0));
        assert_eq!(module.status()["floorNotices"].as_array().unwrap().len(), 0);
    }

    /// A raise that brings Pyren's floor up to the driver's says so, and is
    /// the last one - there is nothing lower left to keep.
    #[test]
    fn a_raise_that_reaches_the_drivers_floor_is_marked_and_final() {
        let (module, dir) = driven_on_a_fixture("stall-cap", 40);
        {
            let mut state = lock(&module.state);
            state.mode = FanMode::Manual;
            state.config.keep_driver_floor = false;
            // Held 1600 -> Pyren's floor 1700, one step below the driver's.
            state.config.fan_stable_min_rpm = Some(1600);
            // A speed on that floor: ~82/255 of 5300 is ~1700.
            state.config.manual_pwm = 82;
        }
        fs::write(dir.join("fan1_input"), "1700").unwrap();
        module.tick_once().unwrap();
        fs::write(dir.join("fan1_input"), "0").unwrap();
        module.tick_once().unwrap();
        module.tick_once().unwrap();
        module.tick_once().unwrap();

        let status = module.status();
        let notices = status["floorNotices"].as_array().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["raisedToRpm"], json!(1800));
        assert_eq!(notices[0]["reachedDriverFloor"], json!(true));
        assert_eq!(status["fanMinRpm"], json!(1800));
        // The override is cleared: Pyren's floor is the driver's now.
        assert_eq!(read_file(&dir, "parameters/min_rpm_override"), "0");

        // And further stalls change nothing - the watch no longer runs.
        for _ in 0..4 {
            module.tick_once().unwrap();
        }
        assert_eq!(module.status()["floorNotices"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn clearing_the_notices_empties_them() {
        let (module, _dir) = driven_on_a_fixture("stall-clear", 40);
        {
            let mut state = lock(&module.state);
            state.config.fan_floor_notices.push(FloorNotice {
                at_unix_secs: 1,
                raised_from_rpm: 700,
                raised_to_rpm: 800,
                stalls: 3,
                reached_driver_floor: false,
            });
        }
        assert_eq!(module.status()["floorNotices"].as_array().unwrap().len(), 1);

        module.call("clearFloorNotices", json!({})).unwrap();
        assert_eq!(module.status()["floorNotices"].as_array().unwrap().len(), 0);
    }

    fn set_temp(dir: &Path, temp_c: &str) {
        fs::write(dir.join("temp1_input"), temp_c).unwrap();
    }

    fn in_manual(module: &FanModule, pwm: u8) {
        let mut state = lock(&module.state);
        state.mode = FanMode::Manual;
        state.config.manual_pwm = pwm;
    }

    /// No manual speed and no curve has a say at 90 C: full speed, held
    /// through the band, and the user's own speed back once it is cool.
    #[test]
    fn a_critical_temperature_overrides_a_manual_speed_until_well_cooled() {
        let (module, dir) = driven_on_a_fixture("critical", 95);
        in_manual(&module, 200);

        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "0", "full speed");
        assert_eq!(module.status()["safety"]["holding"], json!("max"));
        assert_eq!(
            module.status()["mode"],
            json!("manual"),
            "the setting is untouched"
        );

        set_temp(&dir, "85000");
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "0", "still inside the band");

        set_temp(&dir, "70000");
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1");
        assert_eq!(read_file(&dir, "pwm1"), "200", "exactly what was there");
        assert_eq!(module.status()["safety"]["holding"], json!(null));
    }

    /// The case the startup hand-over used to be for: a manual speed this
    /// daemon found rather than set is guarded just the same, and put back
    /// as found.
    #[test]
    fn an_adopted_manual_speed_is_guarded_and_put_back_as_found() {
        let (module, dir) = driven_on_a_fixture("adopted", 95);
        in_manual(&module, 90);
        lock(&module.state).owned = false;

        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "0");

        set_temp(&dir, "60000");
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1");
        assert_eq!(read_file(&dir, "pwm1"), "90");

        // ...and then left alone again, as an adopted mode always was.
        fs::write(dir.join("pwm1"), "33").unwrap();
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1"), "33");
    }

    /// A curve that cannot read its sensor does not keep its last speed:
    /// three ticks, then full speed, then the curve again once it reads.
    #[test]
    fn a_curve_whose_sensor_fails_goes_to_full_speed_and_comes_back() {
        let (module, dir) = driven_on_a_fixture("sensor-fail", 60);
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1", "following the curve");

        set_temp(&dir, "garbage");
        module.tick_once().unwrap();
        module.tick_once().unwrap();
        assert_eq!(
            read_file(&dir, "pwm1_enable"),
            "1",
            "a renumbering gets a moment"
        );
        set_temp(&dir, "255000");
        module.tick_once().unwrap();
        assert_eq!(
            read_file(&dir, "pwm1_enable"),
            "0",
            "third bad reading: full speed"
        );
        assert_eq!(module.status()["safety"]["sensorFailed"], json!(true));

        set_temp(&dir, "61000");
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1");
    }

    /// A slow manual speed with no temperature to read is blind to the
    /// critical override, so it gets the curve's fallback; a fast one does
    /// not need it.
    #[test]
    fn a_slow_manual_speed_with_no_readings_goes_to_full_speed_and_comes_back() {
        let (module, dir) = driven_on_a_fixture("manual-blind", 50);
        in_manual(&module, 100);
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1");

        set_temp(&dir, "garbage");
        module.tick_once().unwrap();
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1");
        set_temp(&dir, "0");
        module.tick_once().unwrap();
        assert_eq!(
            read_file(&dir, "pwm1_enable"),
            "0",
            "third blind tick: full speed"
        );
        assert_eq!(module.status()["safety"]["sensorFailed"], json!(true));

        set_temp(&dir, "52000");
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1");
        assert_eq!(read_file(&dir, "pwm1"), "100");

        let (fast, fast_dir) = driven_on_a_fixture("manual-fast", 50);
        in_manual(&fast, 200);
        set_temp(&fast_dir, "garbage");
        for _ in 0..4 {
            fast.tick_once().unwrap();
        }
        assert_eq!(
            read_file(&fast_dir, "pwm1_enable"),
            "1",
            "a fast speed is left alone"
        );
        assert_eq!(read_file(&fast_dir, "pwm1"), "200");
    }

    /// A measurement stopped by the heat starts the safety sequence at its
    /// firmware step, whatever the checker setting says.
    #[test]
    fn an_aborted_measurement_hands_the_fans_to_the_firmware() {
        let (module, dir) = driven_on_a_fixture("tripped", 70);
        in_manual(&module, 200);
        module
            .call("setThermalSafetyChecker", json!({ "enabled": false }))
            .unwrap();
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1");

        module.trip_after_measurement("a test", &control::ControlError::TooHot(70, 60));
        assert_eq!(read_file(&dir, "pwm1_enable"), "2");
        assert_eq!(module.status()["safety"]["checker"], json!("firmware"));

        set_temp(&dir, "50000");
        module.tick_once().unwrap();
        assert_eq!(
            read_file(&dir, "pwm1_enable"),
            "1",
            "cooled: the setting is back"
        );
        assert_eq!(read_file(&dir, "pwm1"), "200");
    }

    #[test]
    fn a_measurement_refuses_to_start_on_a_warm_machine() {
        let (module, dir) = driven_on_a_fixture("warm-probe", 61);
        let error = module.run_speed_probe(8).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Failed);
        assert_eq!(read_file(&dir, "pwm1_enable"), "2", "nothing was moved");
        assert!(!lock(&module.state).calibrating, "nothing was claimed");
    }

    #[test]
    fn the_checker_setting_is_on_by_default_and_persisted() {
        let _acpi = crate::testenv::real();
        let module = module("checker-setting");
        assert_eq!(module.status()["thermalSafetyChecker"], json!(true));
        let status = module
            .call("setThermalSafetyChecker", json!({ "enabled": false }))
            .unwrap();
        assert_eq!(status["thermalSafetyChecker"], json!(false));
        let stored = module.store.load::<FanConfig>("fan").value;
        assert!(!stored.thermal_safety_checker);
        let old: FanConfig = serde_json::from_str("{}").unwrap();
        assert!(old.thermal_safety_checker, "a file from before it existed");
    }

    #[test]
    fn the_sensor_failure_action_defaults_to_max_and_refuses_other_words() {
        let _acpi = crate::testenv::real();
        let module = module("sensor-action");
        assert_eq!(module.status()["sensorFailureAction"], json!("max"));
        let status = module
            .call("setSensorFailureAction", json!({ "action": "auto" }))
            .unwrap();
        assert_eq!(status["sensorFailureAction"], json!("auto"));
        let stored = module.store.load::<FanConfig>("fan").value;
        assert_eq!(stored.sensor_failure_action, SensorFailureAction::Auto);
        let error = module
            .call("setSensorFailureAction", json!({ "action": "off" }))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidParams);
        let old: FanConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(old.sensor_failure_action, SensorFailureAction::Max);
    }

    /// With auto chosen, a curve that loses its sensor goes to the firmware
    /// instead of full speed - and still comes back once it reads again.
    #[test]
    fn a_failed_sensor_can_hand_the_fans_to_the_firmware_instead() {
        let (module, dir) = driven_on_a_fixture("sensor-fail-auto", 60);
        module
            .call("setSensorFailureAction", json!({ "action": "auto" }))
            .unwrap();
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1", "following the curve");

        set_temp(&dir, "garbage");
        for _ in 0..3 {
            module.tick_once().unwrap();
        }
        assert_eq!(
            read_file(&dir, "pwm1_enable"),
            "2",
            "third bad reading: the firmware"
        );
        assert_eq!(module.status()["safety"]["sensorFailed"], json!(true));
        assert_eq!(module.status()["safety"]["holding"], json!("auto"));

        set_temp(&dir, "61000");
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1");
    }

    #[test]
    fn an_unsafe_curve_is_refused_over_the_socket() {
        let _acpi = crate::testenv::real();
        let module = module("unsafe-curve");
        let error = module
            .call(
                "setCurve",
                json!({ "curve": [{ "tempC": 40.0, "percent": 50.0 }, { "tempC": 100.0, "percent": 5.0 }] }),
            )
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidParams);
    }

    #[test]
    fn a_stored_config_is_brought_inside_the_bounds() {
        let mut config = FanConfig {
            ma_window: 500,
            fan_max_rpm: Some(99_999),
            fan_min_rpm: Some(-5),
            curve: vec![
                CurvePoint {
                    temp_c: 40.0,
                    percent: 10.0,
                },
                CurvePoint {
                    temp_c: 100.0,
                    percent: 30.0,
                },
            ],
            ..Default::default()
        };
        config.profile_curves.insert(
            "eco".into(),
            vec![CurvePoint {
                temp_c: f64::NAN,
                percent: 1.0,
            }],
        );
        let changes = config.sanitise();

        assert_eq!(config.ma_window, MAX_MA_WINDOW);
        assert_eq!(config.fan_max_rpm, None);
        assert_eq!(config.fan_min_rpm, None);
        assert_eq!(curve::validate(&config.curve), Ok(()));
        assert_eq!(
            config.curve[0],
            CurvePoint {
                temp_c: 40.0,
                percent: 10.0
            }
        );
        assert!(!config.profile_curves.contains_key("eco"));
        assert_eq!(changes.len(), 4, "{changes:?}");
    }

    /// The daemon's way out: firmware control, the driver's own floor, and
    /// nothing written after it however the loop is timed.
    #[test]
    fn on_exit_hands_the_fans_back_and_stays_out() {
        let _acpi = crate::testenv::real();
        let (module, dir) = driven_on_a_fixture("exit", 60);
        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "1");
        fs::write(dir.join("parameters/min_rpm_override"), "7").unwrap();

        module.on_exit();
        assert_eq!(read_file(&dir, "pwm1_enable"), "2");
        assert_eq!(read_file(&dir, "parameters/min_rpm_override"), "0");

        module.tick_once().unwrap();
        assert_eq!(read_file(&dir, "pwm1_enable"), "2");
    }
}
