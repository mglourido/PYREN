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
//!
//! What a given machine will accept is not the same everywhere, and the
//! difference is not cosmetic - see [`control`] for the `pwm1` /
//! `pwm1_enable` split. `getStatus` reports it as `capabilities` so the UI
//! can hide a slider that would do nothing.
//!
//! Not ported yet: calibration (`fanMaxRpm` is therefore usually unknown,
//! which only costs some hysteresis precision) and the fan cleaner.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omen_hub_config::{ConfigStore, LoadOutcome};
use omen_hub_core::{Module, ModuleError, ModuleResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// Private because its functions take the crate-private `FanPaths`; the
// types callers need are re-exported below.
mod control;
pub mod curve;
pub mod diagnostics;

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
    /// Measured full-speed RPM, once calibration exists. Only sharpens the
    /// hysteresis; everything works without it.
    pub fan_max_rpm: Option<i64>,
    /// Off by default, like the power module's equivalent: putting a
    /// machine's fans somewhere the user last left them, at boot, before
    /// they have asked for anything, is not a decision this should make on
    /// its own.
    pub restore_mode_on_start: bool,
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
            restore_mode_on_start: false,
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
    last_target_pwm: Option<u8>,
    last_control_error: Option<String>,
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
                println!("omen-hub-daemon: fan config loaded from {}", store.path_for("fan").display());
            }
            LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                eprintln!(
                    "omen-hub-daemon: fan config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                eprintln!(
                    "omen-hub-daemon: fan config is version {found}, newer than this build \
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
            last_target_pwm: None,
            last_control_error: None,
            last_save_error: None,
        }));

        let module = Self { paths, caps, store, state };
        if caps.switch_mode {
            module.spawn_control_loop();
        }
        module
    }

    /// A module for tools that only want to *look*: no config is read or
    /// written and no control loop is started.
    ///
    /// `omen-hub-check` uses this. A self-test that created files and
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
            "error": state.last_control_error,
            "saved": state.last_save_error.is_none(),
            "saveError": state.last_save_error,
        })
    }

    fn set_mode(&self, mode: FanMode, pwm: Option<u8>) -> ModuleResult {
        if !self.caps.supports(mode) {
            return Err(ModuleError::Other(format!(
                "this machine cannot do '{}': the hp-wmi driver exposes {}. \
                 Run fan.diagnose for the details.",
                mode.as_str(),
                describe(self.caps)
            )));
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
            return Err(ModuleError::Other("params.curve must have at least one point".into()));
        }
        if let Some(bad) = curve.iter().find(|p| !p.temp_c.is_finite() || !p.percent.is_finite()) {
            return Err(ModuleError::Other(format!(
                "curve point ({}, {}) is not a finite number",
                bad.temp_c, bad.percent
            )));
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

    /// One pass of the control loop. Also used by `setMode`/`setCurve` so a
    /// call takes effect immediately rather than up to [`TICK`] later.
    fn tick_once(&self) -> Result<(), ModuleError> {
        let now_secs = monotonic_secs();
        let temp_c = self.paths.cpu_temp.as_deref().and_then(read_millideg_c);
        let (rpm, _) =
            read_fan_rpm(self.paths.fan1_input.as_deref(), self.paths.fan2_input.as_deref());

        let mut state = lock(&self.state);
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
                    state.last_control_error =
                        Some("no CPU temperature sensor, so a curve cannot be followed".into());
                    return Ok(());
                };
                let avg = state.smoother.push(temp_c as f64);
                let interpolation = state.config.interpolation;
                match curve::target_pwm(&state.config.curve, avg, interpolation) {
                    Some(pwm) => Some(pwm),
                    None => {
                        state.last_control_error = Some("the curve has no points".into());
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
                let message = e.to_string();
                state.last_control_error = Some(message.clone());
                Err(match e {
                    control::ControlError::PermissionDenied(_, _) => {
                        ModuleError::PermissionDenied(message)
                    }
                    _ => ModuleError::Other(message),
                })
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
                    .map_err(|e| ModuleError::Other(e.to_string()))
            }

            "setMode" => {
                let mode = params
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(FanMode::parse)
                    .ok_or_else(|| {
                        ModuleError::Other(
                            "params.mode must be one of auto, max, manual, curve".into(),
                        )
                    })?;
                let pwm = params
                    .get("pwm")
                    .and_then(Value::as_u64)
                    .map(|v| v.min(255) as u8);
                if mode == FanMode::Manual && pwm.is_none() {
                    return Err(ModuleError::Other(
                        "params.pwm (0-255) is required for manual mode".into(),
                    ));
                }
                self.set_mode(mode, pwm)
            }

            "setCurve" => {
                let points = params
                    .get("curve")
                    .cloned()
                    .ok_or_else(|| ModuleError::Other("params.curve is required".into()))?;
                let points: Vec<CurvePoint> = serde_json::from_value(points)
                    .map_err(|e| ModuleError::Other(format!("invalid curve: {e}")))?;
                let interpolation = match params.get("interpolation") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(
                        serde_json::from_value(v.clone())
                            .map_err(|e| ModuleError::Other(format!("invalid interpolation: {e}")))?,
                    ),
                };
                self.set_curve(points, interpolation)
            }

            "setRestoreOnStart" => {
                let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
                    ModuleError::Other("params.enabled must be a boolean".into())
                })?;
                self.set_restore_on_start(enabled)
            }

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

/// Human-readable version of what the driver offers, for an error the user
/// will actually read.
fn describe(caps: Capabilities) -> &'static str {
    match (caps.switch_mode, caps.set_speed) {
        (true, true) => "both pwm1 and pwm1_enable",
        (true, false) => "pwm1_enable but no pwm1, so only auto and max are possible",
        (false, true) => "pwm1 but no pwm1_enable",
        (false, false) => "no fan control interface at all",
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
            eprintln!("omen-hub-daemon: could not save fan config: {e}");
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
/// `OMEN_HUB_HWMON_DIR` overrides the search, which is how the self-test
/// can be exercised against a fixture directory on hardware that has no
/// hp-wmi - including in CI.
fn find_hp_wmi_hwmon_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("OMEN_HUB_HWMON_DIR") {
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
