//! Fan control module - ported from the `omen-fan-control` Python project
//! (see `../omen-fan-control-main/docs/` in the workspace for the full
//! behavioral spec this is ported from).
//!
//! | method | params | result |
//! |---|---|---|
//! | `fan.getStatus` | none | temperature, RPM, mode, what this machine can do |
//! | `fan.diagnose` | `{ "allowWrites": bool }` | the self-test, see [`diagnostics`] |
//! | `fan.setMode` | `{ "mode": "auto"\|"max"\|"manual"\|"curve", "pwm"?: 0-255 }` | the new status |
//! | `fan.setCurve` | `{ "curve": [{ "tempC": n, "percent": n }], "interpolation"?: "smooth"\|"discrete" }` | the new status |
//! | `fan.setRestoreOnStart` | `{ "enabled": bool }` | the new status |
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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pyren_config::{ConfigStore, LoadOutcome};
use pyren_core::{acpi, msg, ErrorKind, Module, ModuleError, ModuleResult, Msg};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod calibration;
pub mod cleaner;
// Private because its functions take the crate-private `FanPaths`; the
// types callers need are re-exported below.
mod control;
pub mod curve;
pub mod diagnostics;

pub use calibration::Calibration;
pub use cleaner::Cycle;
pub use control::{Capabilities, FanMode};
pub use curve::{CurvePoint, Interpolation};

const HWMON_ROOT: &str = "/sys/devices/platform/hp-wmi/hwmon";
const HWMON_CLASS_ROOT: &str = "/sys/class/hwmon";
const THERMAL_ZONE0: &str = "/sys/class/thermal/thermal_zone0/temp";

/// How often the control loop looks at the temperature. The original uses
/// two seconds; nothing here is cheaper for being slower, and a curve that
/// reacts a tick late is a curve the user can hear lagging.
const TICK: Duration = Duration::from_secs(2);

/// Sysfs paths discovered for this machine. Any of these can be `None` if
/// the patched hp-wmi driver isn't installed, or if no supported CPU temp
/// sensor was found - callers must handle that, not assume presence.
#[derive(Debug, Clone, Default)]
pub(crate) struct FanPaths {
    pub(crate) hwmon_dir: Option<PathBuf>,
    pub(crate) pwm1: Option<PathBuf>,
    pub(crate) pwm1_enable: Option<PathBuf>,
    pub(crate) fan1_input: Option<PathBuf>,
    pub(crate) fan2_input: Option<PathBuf>,
    pub(crate) cpu_temp: Option<PathBuf>,
}

/// What is persisted to `fan.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FanConfig {
    pub mode: FanMode,
    /// 0-255, used when `mode` is `manual`. 128 is the driver's own
    /// default and a sane half-speed to land on.
    pub manual_pwm: u8,
    pub curve: Vec<CurvePoint>,
    pub interpolation: Interpolation,
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
}

impl Default for FanConfig {
    fn default() -> Self {
        Self {
            mode: FanMode::Auto,
            manual_pwm: 128,
            curve: Vec::new(),
            interpolation: Interpolation::default(),
            ma_window: 5,
            fan_max_rpm: None,
            fan1_max_rpm: None,
            fan2_max_rpm: None,
            restore_mode_on_start: false,
            cleaner_duration_secs: cleaner::DEFAULT_DURATION_SECS,
            cleaner_speed: None,
        }
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
    hysteresis: curve::Hysteresis,
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
}

pub struct FanModule {
    paths: FanPaths,
    caps: Capabilities,
    store: ConfigStore,
    state: Arc<Mutex<State>>,
}

impl FanModule {
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
                println!("pyren-daemon: fan config loaded from {}", store.path_for("fan").display());
            }
            LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                eprintln!(
                    "pyren-daemon: fan config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                eprintln!(
                    "pyren-daemon: fan config is version {found}, newer than this build \
                     understands; using defaults and leaving the file alone"
                );
            }
        }
        let config = loaded.value;

        // Believe the hardware over the file: the machine may have been
        // rebooted, or something else may have moved the fans since.
        let observed = observed_mode(&paths);
        let restoring = config.restore_mode_on_start && caps.supports(config.mode);
        let mode = if restoring { config.mode } else { observed.unwrap_or(FanMode::Auto) };

        let mut config = config;
        // Adopting a manual mode we did not set means adopting its speed
        // too, or the app would show a number nobody chose.
        if !restoring && mode == FanMode::Manual {
            if let Some(pwm) = control::read_pwm(&paths) {
                config.manual_pwm = pwm;
            }
        }

        let state = Arc::new(Mutex::new(State {
            smoother: curve::TempSmoother::new(config.ma_window),
            config,
            mode,
            owned: restoring,
            hysteresis: curve::Hysteresis::new(),
            calibrating: false,
            cleaning: Cleaning::Idle,
            cleaner_probe: None,
            last_cleaner_error: None,
            last_target_pwm: None,
            last_control_error: None,
            last_save_error: None,
        }));

        let module = Self { paths, caps, store, state };
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
        let (_, reversed) =
            read_fan_rpm(self.paths.fan1_input.as_deref(), self.paths.fan2_input.as_deref());
        if !reversed || !acpi::is_loaded() {
            return;
        }

        println!(
            "pyren-daemon: the fans are spinning in reverse and no cycle was started here; \
             ending it and handing them back"
        );

        // On a thread: the ramp down takes seconds, and nothing about
        // serving the socket should wait for it.
        let module = FanModule {
            paths: self.paths.clone(),
            caps: self.caps,
            store: self.store.clone(),
            state: Arc::clone(&self.state),
        };
        std::thread::spawn(move || {
            lock(&module.state).cleaning = Cleaning::Stopping;
            let generation = cleaner::probe().generation.unwrap_or(cleaner::Generation::Modern);
            let result = cleaner::emergency_stop(generation);
            if let Err(e) = &result {
                eprintln!("pyren-daemon: could not end the interrupted cleaning cycle: {e}");
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

        Self {
            state: Arc::new(Mutex::new(State {
                smoother: curve::TempSmoother::new(config.ma_window),
                mode: observed_mode(&paths).unwrap_or(FanMode::Auto),
                config,
                owned: false,
                hysteresis: curve::Hysteresis::new(),
                calibrating: false,
                cleaning: Cleaning::Idle,
                cleaner_probe: None,
                last_cleaner_error: None,
                last_target_pwm: None,
                last_control_error: None,
                last_save_error: None,
            })),
            store: ConfigStore::system(),
            paths,
            caps,
        }
    }

    /// Runs the fan-control self-test against this machine.
    ///
    /// `allow_writes` opts into the one check that touches hardware; it
    /// rewrites the value already set and restores the previous mode, so no
    /// fan changes speed.
    pub fn diagnose(&self, allow_writes: bool) -> diagnostics::Diagnosis {
        diagnostics::diagnose(&self.paths, allow_writes)
    }

    pub fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn status(&self) -> Value {
        let cpu_temp_c = self.paths.cpu_temp.as_deref().and_then(read_millideg_c);
        let (fan_rpm, is_reverse) =
            read_fan_rpm(self.paths.fan1_input.as_deref(), self.paths.fan2_input.as_deref());
        let state = lock(&self.state);

        json!({
            "driverInstalled": self.paths.hwmon_dir.is_some(),
            "capabilities": self.caps,
            "cpuTempC": cpu_temp_c,
            "fanRpm": fan_rpm,
            "isReverse": is_reverse,
            "mode": state.mode.as_str(),
            "pwm": control::read_pwm(&self.paths),
            "targetPwm": state.last_target_pwm,
            "manualPwm": state.config.manual_pwm,
            "curve": state.config.curve,
            "interpolation": state.config.interpolation,
            "restoreModeOnStart": state.config.restore_mode_on_start,
            "fanMaxRpm": state.config.fan_max_rpm,
            "fan1MaxRpm": state.config.fan1_max_rpm,
            "fan2MaxRpm": state.config.fan2_max_rpm,
            "calibrating": state.calibrating,
            // Enough for a caller that only wants to know the fans are not
            // its to command; `cleanerStatus` is the detail.
            "cleaning": state.cleaning.holds_the_fans(),
            "error": state.last_control_error,
            "saved": state.last_save_error.is_none(),
            "saveError": state.last_save_error,
        })
    }

    fn set_mode(&self, mode: FanMode, pwm: Option<u8>) -> ModuleResult {
        if !self.caps.supports(mode) {
            return Err(ModuleError::localised(
                ErrorKind::NotCapable,
                msg!(
                    "fan.err.cannotDoMode",
                    { "mode" => mode.as_str(), "exposes" => describe(self.caps).text },
                    "this machine cannot do '{mode}': the hp-wmi driver exposes {exposes}. \
                     Run fan.diagnose for the details."
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
            // A mode change must land now, whatever the last write was.
            state.hysteresis.reset();
            state.smoother = curve::TempSmoother::new(state.config.ma_window);
        }

        self.tick_once()?;
        let mut state = lock(&self.state);
        persist(&self.store, &mut state);
        drop(state);
        Ok(self.status())
    }

    fn set_curve(&self, curve: Vec<CurvePoint>, interpolation: Option<Interpolation>) -> ModuleResult {
        if curve.is_empty() {
            return Err(ModuleError::localised(
                ErrorKind::InvalidParams,
                msg!("fan.err.curveNoPoints", "params.curve must have at least one point"),
            ));
        }
        if let Some(bad) = curve.iter().find(|p| !p.temp_c.is_finite() || !p.percent.is_finite()) {
            return Err(ModuleError::localised(
                ErrorKind::InvalidParams,
                msg!(
                    "fan.err.curvePointNotFinite",
                    { "temp" => bad.temp_c, "percent" => bad.percent },
                    "curve point ({temp}, {percent}) is not a finite number"
                ),
            ));
        }

        {
            let mut state = lock(&self.state);
            state.config.curve = curve;
            if let Some(interpolation) = interpolation {
                state.config.interpolation = interpolation;
            }
            // The shape changed under the current target; re-evaluate.
            state.hysteresis.reset();
        }

        // Only touches hardware if the curve is the mode in force.
        let _ = self.tick_once();
        let mut state = lock(&self.state);
        persist(&self.store, &mut state);
        drop(state);
        Ok(self.status())
    }

    /// Measures what full speed is on this machine and remembers it.
    ///
    /// Blocks for up to `seconds` while the fans are at max - the caller
    /// is waiting on a physical process, and there is nothing to return
    /// until it finishes. Holding the state lock for that long would
    /// block `getStatus` too, so the flag is set, the lock dropped, and
    /// the run happens outside it.
    fn calibrate(&self, seconds: u64) -> ModuleResult {
        if !self.caps.supports(FanMode::Max) {
            return Err(ModuleError::localised(
                ErrorKind::NotCapable,
                msg!(
                    "fan.err.cannotCalibrate",
                    { "exposes" => describe(self.caps).text },
                    "calibration puts the fans at max and watches them, which this machine \
                     cannot do: the hp-wmi driver exposes {exposes}. Run fan.diagnose for \
                     the details."
                ),
            ));
        }

        {
            let mut state = lock(&self.state);
            if state.calibrating {
                return Err(ModuleError::localised(
                    ErrorKind::Busy,
                    msg!("fan.err.calibrating", "a calibration run is already in progress"),
                ));
            }
            state.calibrating = true;
        }

        let outcome = calibration::run(&self.paths, self.caps, seconds);

        let mut state = lock(&self.state);
        state.calibrating = false;
        // The fans were moved out from under the hysteresis, so what it
        // last wrote says nothing about where they are now.
        state.hysteresis.reset();

        let calibration = match outcome {
            Ok(calibration) => calibration,
            Err(e) => {
                state.last_control_error = Some(e.to_msg());
                return Err(control_error(e));
            }
        };

        // A run that measured nothing must not erase a run that did.
        if calibration.verdict.worth_storing() {
            state.config.fan_max_rpm = calibration.fan_max_rpm;
            state.config.fan1_max_rpm = calibration.fan1_max_rpm;
            state.config.fan2_max_rpm = calibration.fan2_max_rpm;
            persist(&self.store, &mut state);
        }
        drop(state);

        let mut result = serde_json::to_value(&calibration)
            .map_err(|e| ModuleError::Internal(e.to_string()))?;
        // The same shape every other fan write returns, so a caller never
        // has to follow one with a read.
        result["status"] = self.status();
        // Re-assert whatever the mode in force is, now rather than up to a
        // TICK later. Only does anything when this daemon owns the fans.
        let _ = self.tick_once();
        Ok(result)
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
        let (_, is_reverse) =
            read_fan_rpm(self.paths.fan1_input.as_deref(), self.paths.fan2_input.as_deref());

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
            "cpuTempC": self.paths.cpu_temp.as_deref().and_then(read_millideg_c),
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
        // would otherwise have no way to try it. It skips the refusal, not
        // the temperature guard.
        if !probe.supported && !force {
            return Err(cleaner::CleanerError::NotCapable);
        }

        let (duration, speed) = {
            let state = lock(&self.state);
            let secs = seconds.unwrap_or(state.config.cleaner_duration_secs);
            (Duration::from_secs(secs), speed.or(state.config.cleaner_speed))
        };

        let request = cleaner::Request {
            speed,
            duration,
            temp_c: self.paths.cpu_temp.as_deref().and_then(read_millideg_c),
        };

        let fan1 = self.paths.fan1_input.clone();
        let fan2 = self.paths.fan2_input.clone();
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
            state.hysteresis.reset();
        }
        // Re-asserts the configured mode now rather than up to a TICK
        // later. Only does anything when this daemon owns the fans.
        let _ = self.tick_once();
    }

    /// The timer that ends a cycle. One thread per cycle, tagged with its
    /// id so a watchdog left over from a cycle somebody stopped by hand
    /// cannot end the next one.
    fn arm_watchdog(&self, id: u64, wait: Duration, generation: cleaner::Generation) {
        let state = Arc::clone(&self.state);
        let paths = self.paths.clone();
        let caps = self.caps;
        let store = self.store.clone();

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
            let module = FanModule { paths, caps, store, state };
            module.finish_cycle(result);
        });
    }

    /// The second of the three places the timeout is enforced (see the
    /// [`cleaner`] module docs). Called from every status read and every
    /// control tick, so a cycle outlives its watchdog by a tick at most.
    fn stop_if_expired(&self) {
        let generation = {
            let mut state = lock(&self.state);
            match state.cleaning.cycle() {
                Some(cycle) if cycle.expired() => {
                    let generation = cycle.generation;
                    state.cleaning = Cleaning::Stopping;
                    generation
                }
                _ => return,
            }
        };
        let result = cleaner::stop(generation);
        self.finish_cycle(result);
    }

    /// One pass of the control loop. Also used by `setMode`/`setCurve` so a
    /// call takes effect immediately rather than up to [`TICK`] later.
    fn tick_once(&self) -> Result<(), ModuleError> {
        let now_secs = monotonic_secs();
        let temp_c = self.paths.cpu_temp.as_deref().and_then(read_millideg_c);
        let (rpm, _) =
            read_fan_rpm(self.paths.fan1_input.as_deref(), self.paths.fan2_input.as_deref());

        let mut state = lock(&self.state);
        if state.calibrating {
            // Somebody else is driving, on purpose. See `State::calibrating`.
            return Ok(());
        }
        if state.cleaning.holds_the_fans() {
            // A cleaning cycle owns the fans, and it is not driving them
            // through `pwm1` at all - writing a speed here would fight the
            // firmware override mid-cycle. See `Cleaning`.
            return Ok(());
        }
        if !state.owned {
            // Watching, not driving. See `State::owned`.
            return Ok(());
        }
        let mode = state.mode;

        let target = match mode {
            // The firmware owns the fans in this mode; re-asserting it
            // would be a WMI call that changes nothing. It is written once,
            // when the mode is selected.
            FanMode::Auto => None,
            FanMode::Max => Some(0),
            FanMode::Manual => Some(state.config.manual_pwm),
            FanMode::Curve => {
                let Some(temp_c) = temp_c else {
                    state.last_control_error = Some(msg!(
                        "fan.err.noCpuTemp",
                        "no CPU temperature sensor, so a curve cannot be followed"
                    ));
                    return Ok(());
                };
                let avg = state.smoother.push(temp_c as f64);
                let interpolation = state.config.interpolation;
                match curve::target_pwm(&state.config.curve, avg, interpolation) {
                    Some(pwm) => Some(pwm),
                    None => {
                        state.last_control_error =
                            Some(msg!("fan.err.curveEmpty", "the curve has no points"));
                        return Ok(());
                    }
                }
            }
        };

        let should = match (mode, target) {
            (FanMode::Auto, _) => state.hysteresis.last_written().is_none(),
            (_, Some(target)) => {
                let fan_max = state.config.fan_max_rpm;
                let measured = (rpm > 0).then_some(rpm);
                state.hysteresis.should_apply(target, measured, fan_max, now_secs)
            }
            (_, None) => false,
        };

        if mode == FanMode::Curve {
            state.last_target_pwm = target;
        }
        if !should {
            return Ok(());
        }

        let pwm = target.unwrap_or(0);
        let result = control::apply(&self.paths, self.caps, mode, pwm);
        // Recorded even when the write failed, so a machine that cannot be
        // written to is retried once a minute rather than every tick.
        state.hysteresis.applied(pwm, now_secs);

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

    /// The loop that keeps a curve tracking, and keeps a chosen mode from
    /// quietly expiring.
    ///
    /// Runs on its own thread rather than being driven by IPC calls: a
    /// curve has to be followed while the app is closed, which is the whole
    /// reason there is a daemon.
    fn spawn_control_loop(&self) {
        let paths = self.paths.clone();
        let caps = self.caps;
        let state = Arc::clone(&self.state);
        let store = self.store.clone();

        std::thread::spawn(move || {
            let worker = FanModule { paths, caps, store, state };
            loop {
                // The third enforcement point for a cycle's timeout, so
                // that a cleaner left running by a lost watchdog is ended
                // by the loop that is running anyway.
                worker.stop_if_expired();
                // A transient sysfs failure must not take the loop down;
                // the error is already recorded in the state for getStatus.
                let _ = worker.tick_once();
                std::thread::sleep(TICK);
            }
        });
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
        self.paths.hwmon_dir.is_some()
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getStatus" => Ok(self.status()),

            "diagnose" => {
                // Writing is opt-in and off by default: a diagnostic that
                // silently drives the fans would be a surprising thing for
                // a "check my hardware" button to do.
                let allow_writes =
                    params.get("allowWrites").and_then(Value::as_bool).unwrap_or(false);
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
                    Some(v) => Some(
                        serde_json::from_value(v.clone())
                            .map_err(|e| ModuleError::InvalidParams(format!("invalid interpolation: {e}")))?,
                    ),
                };
                self.set_curve(points, interpolation)
            }

            "setRestoreOnStart" => {
                let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
                    ModuleError::InvalidParams("params.enabled must be a boolean".into())
                })?;
                self.set_restore_on_start(enabled)
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

            "cleanerStatus" => {
                let refresh = params.get("refresh").and_then(Value::as_bool).unwrap_or(false);
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
                let seconds = params.get("seconds").and_then(Value::as_u64).map(|v| {
                    v.clamp(cleaner::MIN_DURATION_SECS, cleaner::MAX_DURATION_SECS)
                });
                let force = params.get("force").and_then(Value::as_bool).unwrap_or(false);
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
    };
    ModuleError::localised(kind, e.to_msg())
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
        E::TooHot(_) => ErrorKind::Failed,
        E::Refused(_) => ErrorKind::Failed,
    };
    ModuleError::localised(kind, e.to_msg())
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
            eprintln!("pyren-daemon: could not save fan config: {e}");
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

fn discover_paths() -> FanPaths {
    let mut paths = FanPaths::default();

    if let Some(hwmon_dir) = find_hp_wmi_hwmon_dir() {
        paths.pwm1 = Some(hwmon_dir.join("pwm1"));
        paths.pwm1_enable = Some(hwmon_dir.join("pwm1_enable"));
        paths.fan1_input = Some(hwmon_dir.join("fan1_input"));
        paths.fan2_input = Some(hwmon_dir.join("fan2_input"));
        paths.hwmon_dir = Some(hwmon_dir);
    }

    paths.cpu_temp = find_cpu_temp_path();
    paths
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
/// hwmon drivers, fall back to `thermal_zone0`.
fn find_cpu_temp_path() -> Option<PathBuf> {
    if let Ok(entries) = fs::read_dir(HWMON_CLASS_ROOT) {
        for entry in entries.filter_map(|e| e.ok()) {
            let dir = entry.path();
            let Ok(name) = fs::read_to_string(dir.join("name")) else {
                continue;
            };
            if matches!(name.trim(), "coretemp" | "k10temp") {
                let temp_path = dir.join("temp1_input");
                if temp_path.exists() {
                    return Some(temp_path);
                }
            }
        }
    }

    let fallback = Path::new(THERMAL_ZONE0);
    fallback.exists().then(|| fallback.to_path_buf())
}

/// sysfs temperature files report millidegrees C.
fn read_millideg_c(path: &Path) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse::<i64>().ok().map(|v| v / 1000)
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
    }

    /// Points `acpi_call` at a path that cannot exist, for as long as the
    /// returned guard lives. `dir` is the test's own temp directory, so
    /// two tests never share the name.
    pub(crate) fn without_acpi_call(dir: &Path) -> NoAcpiCall {
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("PYREN_ACPI_CALL");
        std::env::set_var("PYREN_ACPI_CALL", dir.join("definitely-not-here"));
        NoAcpiCall { _guard: guard, previous }
    }

    impl Drop for NoAcpiCall {
        fn drop(&mut self) {
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
        let root = std::env::temp_dir()
            .join(format!("pyren-fan-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut module = FanModule::inspector();
        module.store = ConfigStore::at(root);
        module
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
        assert!(status["unreachable"].is_object(), "it says why, and the sentence is translatable");
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
        let error = module.start_cleaning(None, None, false).expect_err("nothing to talk to");
        assert_ne!(
            error.kind(),
            ErrorKind::NotCapable,
            "a package to install is not a verdict on the hardware"
        );
        assert!(error.as_msg().contains("acpi_call"), "the message names the module: {}", error.as_msg());

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
            assert!(state.holds_the_fans(), "{state:?} must stop the control loop writing");
            assert!(state.cycle().is_none(), "{state:?} is not a running cycle");
        }
        assert!(!Cleaning::Idle.holds_the_fans());
    }

    /// The duration is a stored preference, and it is clamped where it is
    /// stored rather than where it is used - so a bad value cannot sit in
    /// the config file waiting for the next start.
    #[test]
    fn a_stored_cleaner_duration_is_clamped_on_the_way_in() {
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
}
