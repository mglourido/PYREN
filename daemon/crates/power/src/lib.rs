//! Power mode module: the Eco / Balanced / Performance / Unlimited switch,
//! plus the background supervisor that can drive it automatically.
//!
//! On an HP laptop the firmware's ACPI platform profile is the real thing
//! this drives (the same switch as Fn+P). Elsewhere it falls back to the
//! OS's power manager (power-profiles-daemon, TLP, auto-cpufreq) and the
//! CPU's energy-performance hint, which makes the module useful - and
//! testable - on ordinary Linux machines too.
//!
//! | method | params | result |
//! |---|---|---|
//! | `power.getState` | none | current mode, backend state, battery, auto-switch config |
//! | `power.setMode` | `{ "mode": "eco"\|"balanced"\|"performance"\|"unlimited" }` | what was applied |
//! | `power.setAutoConfig` | [`AutoConfig`] | the stored config, and whether it reached disk |
//! | `power.setRestoreOnStart` | `{ "enabled": bool }` | as above |
//! | `power.setApplyToOsProfile` | `{ "enabled": bool }` | as `getState` |
//! | `power.setTuning` | `{ "mode"?, "pl1W"?, "pl2W"?, "turbo"? }` | as `getState`; defaults to the current mode, and refuses a mode it does not know, a value of the wrong type or PL1 above PL2 |
//!
//! A **mode is a profile**, and it has three parts that are applied
//! separately because they belong to different owners:
//!
//! | part | mechanism | optional? |
//! |---|---|---|
//! | the laptop's own profile | ACPI `platform_profile` | no |
//! | the OS profile | power-profiles-daemon / TLP / auto-cpufreq | yes - `applyToOsProfile` |
//! | the power envelope | powercap PL1/PL2 + turbo | only if someone set it |
//!
//! The first is the one that matters most and the one this project cannot
//! replicate: changing it changes the EC's own temperature-to-RPM curve,
//! so Eco makes the fans start *later* rather than merely turn slower, and
//! it moves internal power states (PCIe and friends) that no userspace
//! knob reaches.
//!
//! The envelope ships untouched. See [`Tuning::default_for`] for why
//! guessing at it would be worse than leaving it alone, and nothing ever
//! asks for more than stock - raising a limit past what the firmware
//! shipped is overclocking, and is a separate feature with separate
//! consent.
//!
//! Whatever else writes those knobs is watched for rather than fought -
//! see [`watch`]: a firmware profile moved by Fn+P, the desktop or a power
//! manager is followed, and anything else overwritten is reported.
//!
//! Settings live in `power.json` (see `pyren-config`), so the
//! supervisor keeps running with the user's rules after a reboot - which
//! is the whole point of it being a daemon rather than part of the app.

mod auto;
mod backend;
mod limits;
mod supply;
mod watch;

use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use pyren_config::{ConfigStore, LoadOutcome};
use pyren_core::{log_info, log_warn};
use pyren_core::{msg, ErrorKind, EventBus, Module, ModuleError, ModuleResult, Msg};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use auto::{AutoConfig, AutoInputs, AutoSwitcher, HeatLatch, Sensors};
pub use backend::{ApplyReport, BackendState};
pub use limits::{Limits, LockSource, ModeTuning, Tuning};
pub use supply::PowerSupplyState;
pub use watch::Override;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerMode {
    Eco,
    Balanced,
    Performance,
    Unlimited,
}

impl PowerMode {
    /// Least to most, and the order the performance key steps through.
    ///
    /// The same order the app lists them in - a widget that highlights the
    /// current one has to agree with the key that moves the highlight, so
    /// there is one list and this is it.
    pub const ALL: &'static [Self] = &[
        Self::Eco,
        Self::Balanced,
        Self::Performance,
        Self::Unlimited,
    ];

    /// The next mode round the loop, wrapping back to Eco.
    pub fn next(self) -> Self {
        let at = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(at + 1) % Self::ALL.len()]
    }

    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "eco" => Some(Self::Eco),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            "unlimited" => Some(Self::Unlimited),
            _ => None,
        }
    }

    /// The name this mode goes out under, and the same string serde writes
    /// for it.
    ///
    /// The two must not drift: the `power.mode` event carries the serde
    /// form, and the fan module keys its per-profile curves off whatever
    /// arrives there. A daemon that seeded a curve under `Eco` and then
    /// looked it up under `eco` would lose it on the first mode change -
    /// `the_wire_name_is_the_serde_name` is the test that pins this.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eco => "eco",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
            Self::Unlimited => "unlimited",
        }
    }
}

/// What is persisted to `power.json`. Every default is "leave the machine
/// alone": no restore at boot, no OS profile, no envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PowerConfig {
    pub auto: AutoConfig,
    /// Last mode that took effect, remembered for `restore_mode_on_start`.
    pub mode: Option<PowerMode>,
    /// Re-apply `mode` when the daemon starts. Off by default: silently
    /// changing the machine's power behaviour at boot should be something
    /// the user opted into.
    pub restore_mode_on_start: bool,
    /// Whether changing the performance mode should also change the OS
    /// power profile (power-profiles-daemon, TLP, auto-cpufreq - see
    /// `backend`), or only the laptop's own
    /// firmware profile.
    ///
    /// The two are separate on purpose: the firmware profile is what moves
    /// the EC's fan curve and its internal power states, while the OS
    /// profile is what the desktop's battery menu shows. Wanting the first
    /// without the second is a reasonable thing to want, and the app has
    /// had a switch for it since before the daemon honoured it.
    ///
    /// Off unless the user turns it on: the OS profile belongs to whatever
    /// power manager the user installed, and a fresh install should not
    /// start overriding it.
    pub apply_to_os_profile: bool,
    /// The machine's own power limits, captured before this daemon ever
    /// wrote one.
    ///
    /// Persisted, and never lowered once recorded, because it is the
    /// ceiling everything else is measured against: re-reading it at
    /// startup while Eco was in force would make Eco's reduced limit the
    /// new "stock", and the machine would ratchet down a little on every
    /// boot. It *is* raised if the hardware ever reports more than what is
    /// stored, since a higher value can only have come from the firmware.
    pub stock_limits: Option<Limits>,
    /// Each mode's share of that envelope.
    pub tuning: ModeTuning,
    /// Whether the limits and turbo on the machine are this daemon's -
    /// written for a mode someone tuned - rather than the firmware's.
    ///
    /// A mode whose tuning is untouched writes nothing (see
    /// [`Tuning::is_default`]), with one exception this remembers: leaving
    /// a tuned mode has to put the stock envelope back, or its cap would
    /// outlive it. Persisted, because that cap outlives a daemon restart.
    pub envelope_owned: bool,
}

/// Where a mode change is announced, so that anything watching the daemon
/// hears about it however it happened.
///
/// The mode is the one piece of this module's state that moves *without*
/// the app asking: the performance key cycles it, the supervisor switches
/// it, `pyren-ctl` sets it, and the widget clicks it. Every one of those
/// used to leave an open app showing a mode the machine was no longer in
/// until something else made it re-read.
///
/// It is a bus rather than a call into another module: this announces what
/// happened and does not know or care who is listening, which is the one
/// shape that does not turn into modules calling each other.
///
/// Empty until the daemon binary fills it in - `pyren-check` and the tests
/// build a `PowerModule` with nobody listening, and publishing has to be a
/// no-op there rather than a reason to require a bus.
#[derive(Clone, Default)]
pub struct Announcer(Arc<OnceLock<Arc<EventBus>>>);

impl Announcer {
    fn publish(&self, mode: PowerMode, source: &str) {
        if let Some(bus) = self.0.get() {
            bus.publish("power.mode", json!({ "mode": mode, "source": source }));
        }
    }

    /// Something another program changed that did not move the mode, so
    /// `power.mode` will not carry it to an open app.
    fn overridden(&self, finding: &Override) {
        if let Some(bus) = self.0.get() {
            bus.publish("power.overridden", json!(finding));
        }
    }
}

/// What one press of the performance key did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cycled {
    pub from: PowerMode,
    /// The mode the machine ended up in - the same as `asked_for` when the
    /// change took, and unchanged from `from` when nothing did.
    pub to: PowerMode,
    pub asked_for: PowerMode,
    pub report: ApplyReport,
}

impl Cycled {
    pub fn changed(&self) -> bool {
        !self.report.is_empty()
    }
}

/// Shared between the IPC handlers and the supervisor thread.
#[derive(Debug)]
struct State {
    mode: PowerMode,
    config: PowerConfig,
    switcher: AutoSwitcher,
    /// When the user last set a mode by hand; the supervisor stays out of
    /// the way for `manual_override_secs` after that.
    manual_override_at: Option<Instant>,
    last_auto_switch: Option<Msg>,
    /// Set when the last write to disk failed, so the UI can say the
    /// setting will not survive a restart instead of quietly losing it.
    last_save_error: Option<String>,
    /// What the knobs this daemon writes itself were left at, and when -
    /// the reference the watcher holds the machine to.
    expected: watch::Knobs,
    expected_at: Instant,
    /// Knobs another program changed since this daemon last applied a
    /// mode, one entry per knob, latest finding wins.
    overrides: Vec<Override>,
    /// The last mode the machine was found in without this daemon putting
    /// it there.
    last_external: Option<Msg>,
}

impl State {
    /// Takes a finished apply as the new reference. Everything found
    /// overridden until now was overridden relative to the *previous*
    /// mode, so it is forgotten with it.
    fn record_apply(&mut self, report: &ApplyReport) {
        self.expected = report.expected.clone();
        self.expected_at = Instant::now();
        self.overrides.clear();
        // News from before the mode this daemon just applied.
        self.last_external = None;
    }
}

/// Cloning shares one module - the same state, the same supervisor thread
/// and the same config file - so the daemon binary can hold on to a handle
/// after registering it. Constructing a *second* one would start a second
/// supervisor, which is why this is a clone and not a `new`.
#[derive(Clone)]
pub struct PowerModule {
    state: Arc<Mutex<State>>,
    store: ConfigStore,
    limits: limits::LimitPaths,
    announce: Announcer,
    /// Discovered once - see [`Sensors`] - and shared with the supervisor
    /// thread, which is the only reason a status read has a temperature to
    /// show at all.
    sensors: Sensors,
    /// Held by every clone and by nothing else: the watcher stops once the
    /// last one is gone. It writes on its own - the envelope of a mode it
    /// follows - so a module a test has dropped must not keep doing that to
    /// whichever fixture the next test put in place.
    _alive: Arc<()>,
}

impl PowerModule {
    pub fn new() -> Self {
        Self::with_store(ConfigStore::system())
    }

    /// Builds the module against an explicit config store. Tests use this
    /// to keep out of the real `/etc`.
    pub fn with_store(store: ConfigStore) -> Self {
        let loaded = store.load::<PowerConfig>("power");
        match &loaded.outcome {
            LoadOutcome::Loaded => {
                log_info!(
                    "power config loaded from {}",
                    store.path_for("power").display()
                );
            }
            LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                log_warn!(
                    "power config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                log_warn!(
                    "power config is version {found}, newer than this \
                     build understands; using defaults and leaving the file alone"
                );
            }
        }
        let mut config = loaded.value;

        // `setAutoConfig` refuses crossed thresholds, but a file from an
        // older build or edited by hand never went through it - and a
        // supervisor running on `loadLow >= loadHigh` flips modes forever.
        if let Some(problem) = config.auto.problem() {
            if config.auto.enabled {
                log_warn!("power auto-switch config is invalid ({problem}); auto-switching is off until it is fixed");
                config.auto.enabled = false;
            }
        }

        // Read the envelope before anything has had a chance to change it.
        let limit_paths = limits::LimitPaths::discover();
        let observed = limits::read(&limit_paths);
        config.stock_limits = Some(sane_stock(config.stock_limits, observed));

        // Start from whatever the machine is already set to rather than
        // assuming Balanced, so the first supervisor tick compares against
        // reality.
        let mut mode = current_mode().unwrap_or(PowerMode::Balanced);
        // Nothing written yet, so only the firmware profile is anyone's to
        // follow: the machine is in it, whoever chose it.
        let mut expected = watch::Knobs {
            platform_profile: backend::read_platform_profile(),
            ..watch::Knobs::default()
        };

        if config.restore_mode_on_start {
            if let Some(saved) = config.mode {
                let saved = boot_mode(saved, &config, PowerSupplyState::read().on_battery);
                let before = backend::read_state();
                let report = apply_profile(&before, saved, &mut config, &limit_paths);
                expected = report.expected.clone();
                if report.is_empty() {
                    log_warn!(
                        "could not restore power mode {saved:?}: {}",
                        report.failed.join("; ")
                    );
                } else {
                    log_info!("restored power mode {saved:?}");
                    mode = saved;
                }
            }
        }

        let state = Arc::new(Mutex::new(State {
            mode,
            config,
            switcher: AutoSwitcher::default(),
            manual_override_at: None,
            last_auto_switch: None,
            last_save_error: None,
            expected,
            expected_at: Instant::now(),
            overrides: Vec::new(),
            last_external: None,
        }));

        let announce = Announcer::default();
        let sensors = Sensors::discover();
        spawn_supervisor(
            Arc::clone(&state),
            store.clone(),
            limit_paths.clone(),
            announce.clone(),
            sensors.clone(),
        );
        let alive = Arc::new(());
        spawn_watcher(
            Arc::clone(&state),
            store.clone(),
            limit_paths.clone(),
            announce.clone(),
            Arc::downgrade(&alive),
        );
        Self {
            state,
            store,
            limits: limit_paths,
            announce,
            sensors,
            _alive: alive,
        }
    }

    /// One look at the machine, as the watcher thread takes every
    /// [`watch::INTERVAL`]: follows a firmware profile something else
    /// moved, and records whatever else was overwritten. Returns the mode
    /// it followed the machine into, if it did.
    ///
    /// Public so a test can take the look itself instead of sleeping until
    /// the thread does.
    pub fn check_external(&self) -> Option<PowerMode> {
        watch_once(&self.state, &self.store, &self.limits, &self.announce)
    }

    /// Hands the module the bus to announce mode changes on. Called once,
    /// by the daemon binary, after the registry exists.
    pub fn publish_to(&self, events: Arc<EventBus>) {
        let _ = self.announce.0.set(events);
    }

    /// Path of the file this module reads and writes, for diagnostics.
    pub fn config_path(&self) -> std::path::PathBuf {
        self.store.path_for("power")
    }

    /// The mode the machine is in.
    pub fn mode(&self) -> PowerMode {
        lock(&self.state).mode
    }

    /// Steps to the next mode, as the laptop's performance key does.
    ///
    /// Counts as a manual change - the supervisor stays out of the way
    /// afterwards, exactly as it does when someone clicks a mode in the
    /// app. A key press *is* the user choosing.
    ///
    /// The report is returned rather than logged because the caller has to
    /// say what happened: on a machine with no mechanism at all nothing is
    /// applied, the mode does not move, and a widget that had already
    /// slid its highlight across would be lying.
    pub fn cycle(&self) -> Cycled {
        let from = self.mode();
        let to = from.next();
        let report = self.choose(to, "hotkey");
        Cycled {
            from,
            to: self.mode(),
            asked_for: to,
            report,
        }
    }

    /// A mode the user picked - in the app, from `pyren-ctl` or with the
    /// performance key. Beyond pausing the supervisor, it becomes what the
    /// supervisor works around once the pause is over (see
    /// [`AutoSwitcher::adopt`]).
    ///
    /// Separate from `set_mode(.., true, ..)` because the other manual
    /// callers only *re-apply* the mode in force - a tuning edit, the OS
    /// profile switch - and must not turn a mode the supervisor chose into
    /// one the user did.
    fn choose(&self, mode: PowerMode, source: &str) -> ApplyReport {
        let report = self.set_mode(mode, true, source);
        if !report.is_empty() {
            lock(&self.state).switcher.adopt(mode);
        }
        report
    }

    /// `source` says who asked, and travels with the announcement: a UI
    /// that made the change itself can tell it apart from one made behind
    /// its back, which is the difference between a redundant re-read and a
    /// necessary one.
    fn set_mode(&self, mode: PowerMode, manual: bool, source: &str) -> ApplyReport {
        // Read before the lock: it starts processes, and even bounded by
        // their timeouts that is not something to hold everyone up for.
        let before = backend::read_state();
        // Applied and recorded under one lock, so the watcher can never see
        // the machine already in the new mode while the daemon still
        // expects the old one - which it would take for someone else's
        // change, and follow.
        let mut state = lock(&self.state);
        let report = apply_profile(&before, mode, &mut state.config, &self.limits);
        state.record_apply(&report);
        // Only record the mode if something actually took effect; otherwise
        // the UI would show a mode the machine isn't in.
        let took_effect = !report.is_empty();
        if took_effect {
            state.mode = mode;
            state.config.mode = Some(mode);
        }
        if manual {
            state.manual_override_at = Some(Instant::now());
            state.switcher.reset();
        }
        persist(&self.store, &mut state);
        drop(state);

        // Announced only when the machine actually moved, and after the
        // state is recorded: a listener's first reaction is to ask for the
        // state, and it must not race with this.
        if took_effect {
            self.announce.publish(mode, source);
        }
        report
    }

    fn state_json(&self) -> Value {
        // Everything that reads the machine happens before the lock: the
        // backend read starts processes, and a status poll must never make
        // a mode change wait on it.
        let backend = backend::read_state();
        let supply = PowerSupplyState::read();
        let current_limits = limits::read(&self.limits);
        let turbo = limits::read_turbo(&self.limits);
        let locked = limits::locked(&self.limits);
        let state = lock(&self.state);
        let override_remaining = state
            .manual_override_at
            .map(|at| {
                Duration::from_secs(state.config.auto.manual_override_secs)
                    .saturating_sub(at.elapsed())
                    .as_secs()
            })
            .filter(|remaining| *remaining > 0);

        json!({
            "mode": state.mode,
            "backend": backend,
            "limits": {
                "available": self.limits.has_limits(),
                "turboAvailable": self.limits.has_turbo(),
                "stock": state.config.stock_limits,
                "current": current_limits,
                // `"msr"` or `"sysfs"` when the firmware locked PL1/PL2 -
                // the limits cannot change until reboot, whatever is tuned.
                "locked": locked,
                "turbo": turbo,
                "tuning": state.config.tuning,
            },
            "supply": supply,
            "auto": state.config.auto,
            "restoreModeOnStart": state.config.restore_mode_on_start,
            "applyToOsProfile": state.config.apply_to_os_profile,
            "autoOverrideSecondsLeft": override_remaining,
            // The mode the user picked by hand that the supervisor is
            // working around instead of the configured preference, until
            // the power source next changes. `null` when it is following
            // the preference.
            "autoManualBaseline": state.switcher.manual_baseline(
                &state.config.auto,
                state.switcher.on_battery().or(supply.on_battery).unwrap_or(false),
            ),
            // What the supervisor's thermal rule can see and what it
            // currently thinks. `hot` is latched, so it is not a
            // comparison a client could redo from `tempC` - which is the
            // reason it is reported rather than left to be inferred.
            "thermal": {
                "available": self.sensors.any(),
                "tempC": self.sensors.hottest_c_now(),
                "hot": state.switcher.is_hot(),
            },
            "lastAutoSwitch": state.last_auto_switch,
            // What another program changed since this daemon last applied
            // a mode - see `watch`. Not rewritten, only reported.
            "overrides": state.overrides,
            "lastExternal": state.last_external,
            "configPath": self.store.path_for("power"),
            "configSaveError": state.last_save_error,
        })
    }
}

impl PowerModule {
    /// What the daemon undoes on its way out.
    ///
    /// auto-cpufreq keeps a `--force` override in its own state, so a mode
    /// pyren put there would outlive pyren and quietly keep the machine in
    /// it. The firmware profile and the limits are left alone: they are
    /// where the user put the machine, and the firmware resets the limits
    /// at the next boot anyway.
    pub fn on_exit(&self) {
        if !lock(&self.state).config.apply_to_os_profile {
            return;
        }
        match backend::release_auto_cpufreq() {
            Some(Ok(())) => log_info!("power: handed auto-cpufreq back its own control"),
            Some(Err(e)) => log_warn!("power: could not reset auto-cpufreq's override: {e}"),
            None => {}
        }
    }
}

impl Default for PowerModule {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the machine offers any way at all to change its power behaviour.
///
/// A free function rather than a method because callers that only want the
/// answer - the compatibility verdict, `pyren-check` - must not have to
/// build a `PowerModule` to get it: constructing one loads config and
/// starts the supervisor, which is a thread that can change the machine's
/// power mode on its own. A question should not have side effects.
pub fn power_mode_available() -> bool {
    !backend::read_state().available.is_empty()
}

/// What power surface this machine offers, for a compatibility report.
///
/// A narrow accessor rather than making `backend` and `limits` public:
/// `pyren-check` needs to *describe* what is here, not drive it, and a
/// reporting tool with write access to the internals is a tool that will
/// eventually write.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerSurface {
    /// Mechanisms that answered, e.g. `platform_profile`.
    pub mechanisms: Vec<String>,
    pub platform_profile: Option<String>,
    pub platform_profile_choices: Vec<String>,
    /// The firmware's package limits, empty when there is no RAPL zone.
    pub limits: Limits,
    pub has_turbo: bool,
}

pub fn surface() -> PowerSurface {
    let state = backend::read_state();
    let paths = limits::LimitPaths::discover();
    PowerSurface {
        mechanisms: state.available.iter().map(|m| m.to_string()).collect(),
        platform_profile: state.platform_profile,
        platform_profile_choices: state.platform_profile_choices,
        limits: limits::read(&paths),
        has_turbo: paths.has_turbo(),
    }
}

impl Module for PowerModule {
    fn id(&self) -> &'static str {
        "power"
    }

    /// On hardware with no mechanism at all the UI should show the modes as
    /// unavailable rather than pretending they work.
    fn is_supported(&self) -> bool {
        power_mode_available()
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getState" => Ok(self.state_json()),

            "setMode" => {
                let mode = params
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(PowerMode::parse)
                    .ok_or_else(|| {
                        ModuleError::localised(
                            ErrorKind::InvalidParams,
                            msg!(
                                "power.err.badMode",
                                "params.mode must be one of eco, balanced, performance, unlimited"
                            ),
                        )
                    })?;

                let report = self.choose(mode, "request");
                if report.is_empty() {
                    // Every mechanism needs root; an unprivileged daemon
                    // failing here is the expected case in development, so
                    // say which one and why rather than a bare "failed".
                    return Err(ModuleError::localised(
                        ErrorKind::PermissionDenied,
                        msg!(
                            "power.err.applyFailed",
                            { "detail" => report.failed.join("; ") },
                            "no power mechanism could be applied: {detail}"
                        ),
                    ));
                }
                serde_json::to_value(report).map_err(|e| ModuleError::Internal(e.to_string()))
            }

            "setAutoConfig" => {
                let auto: AutoConfig = serde_json::from_value(params)
                    .map_err(|e| ModuleError::InvalidParams(format!("invalid auto config: {e}")))?;
                if let Some(problem) = auto.problem() {
                    return Err(ModuleError::localised(ErrorKind::InvalidParams, problem));
                }
                let mut state = lock(&self.state);
                if auto.enabled && !state.config.auto.enabled {
                    // Switched back on: start from nothing, as at startup.
                    // The supervisor was not sampling while it was off, so
                    // its idea of the power source is however old the
                    // switch-off is - a cable moved in the meantime would
                    // read as one moved just now and switch the mode on the
                    // first tick. Same for a hand-picked baseline, which
                    // belonged to a power source it can no longer vouch
                    // for, and a heat latch nobody has fed since.
                    state.switcher = AutoSwitcher::default();
                } else {
                    state.switcher.reset();
                }
                state.config.auto = auto;
                persist(&self.store, &mut state);
                Ok(saved_response(&state))
            }

            "setTuning" => {
                let mode = match params.get("mode") {
                    None | Some(Value::Null) => lock(&self.state).mode,
                    // A mode that is named and not understood is refused
                    // rather than read as "the current one": tuning the
                    // wrong mode is a change nobody can see happen.
                    Some(value) => value.as_str().and_then(PowerMode::parse).ok_or_else(|| {
                        ModuleError::localised(
                            ErrorKind::InvalidParams,
                            msg!(
                                "power.err.badMode",
                                "params.mode must be one of eco, balanced, performance, unlimited"
                            ),
                        )
                    })?,
                };
                let watts = |key: &str| match params.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(value) => value.as_f64().map(Some).ok_or_else(|| {
                        ModuleError::localised(
                            ErrorKind::InvalidParams,
                            msg!(
                                "power.err.wattsNumber",
                                { "key" => key },
                                "params.{key} must be a number of watts"
                            ),
                        )
                    }),
                };
                let pl1_watts = watts("pl1W")?;
                let pl2_watts = watts("pl2W")?;
                // A limit the firmware locked would be stored, applied,
                // accepted by the kernel and then not happen. Said now, not
                // discovered as a read-back failure on every mode change.
                if pl1_watts.is_some() || pl2_watts.is_some() {
                    if let Some(source) = limits::locked(&self.limits) {
                        return Err(ModuleError::localised(
                            ErrorKind::NotCapable,
                            msg!(
                                "power.err.limitsLocked",
                                { "source" => match source {
                                    LockSource::Msr => "msr",
                                    LockSource::Sysfs => "sysfs",
                                } },
                                "the firmware has locked this machine's power limits ({source}); they cannot be changed until it reboots"
                            ),
                        ));
                    }
                }
                let turbo = match params.get("turbo") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(value.as_bool().ok_or_else(|| {
                        ModuleError::localised(
                            ErrorKind::InvalidParams,
                            msg!("power.err.turboBool", "params.turbo must be a boolean"),
                        )
                    })?),
                };

                let mut state = lock(&self.state);
                let previous = state.config.tuning.get(mode);
                let mut tuning = previous;
                let stock = state.config.stock_limits.unwrap_or_default();

                // Watts on the wire, because that is what the user is
                // shown; percentages on disk, because that is what
                // survives being restored onto different hardware.
                if let Some(watts) = pl1_watts {
                    tuning.pl1_percent = percent_of(watts, stock.pl1_uw)?;
                }
                if let Some(watts) = pl2_watts {
                    tuning.pl2_percent = percent_of(watts, stock.pl2_uw)?;
                }
                if let Some(turbo) = turbo {
                    tuning.turbo = turbo;
                }
                let asked = tuning.target(stock);
                if let (Some(pl1), Some(pl2)) = (asked.pl1_uw, asked.pl2_uw) {
                    if pl1 > pl2 {
                        return Err(ModuleError::localised(
                            ErrorKind::InvalidParams,
                            msg!(
                                "power.err.limitsCrossed",
                                {
                                    "pl1" => (pl1 / 1_000_000).to_string(),
                                    "pl2" => (pl2 / 1_000_000).to_string()
                                },
                                "the sustained limit ({pl1} W) cannot be above the boost limit ({pl2} W)"
                            ),
                        ));
                    }
                }
                if tuning == previous {
                    drop(state);
                    return Ok(self.state_json());
                }
                state.config.tuning.set(mode, tuning);
                let applies_now = state.mode == mode;
                persist(&self.store, &mut state);
                drop(state);

                // Tuning the mode the machine is in should be audible
                // straight away, not after the next mode switch. Not a
                // manual choice of mode, though: editing a number must not
                // pause the supervisor as if the user had picked one.
                if applies_now {
                    self.set_mode(mode, false, "tuning");
                }
                Ok(self.state_json())
            }

            "setApplyToOsProfile" => {
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        ModuleError::localised(
                            ErrorKind::InvalidParams,
                            msg!("power.err.enabledBool", "params.enabled must be a boolean"),
                        )
                    })?;
                let (mode, was) = {
                    let mut state = lock(&self.state);
                    let was = state.config.apply_to_os_profile;
                    state.config.apply_to_os_profile = enabled;
                    persist(&self.store, &mut state);
                    (state.mode, was)
                };
                // Switched off: the OS profile is the power manager's again,
                // and auto-cpufreq's override is the one piece of pyren's
                // choice that would otherwise stay in its state.
                if was && !enabled {
                    if let Some(Err(e)) = backend::release_auto_cpufreq() {
                        log_warn!("power: could not reset auto-cpufreq's override: {e}");
                    }
                }
                // Re-apply so the answer takes effect now rather than at
                // the next mode change - turning it on and seeing nothing
                // happen would look broken.
                self.set_mode(mode, true, "osProfile");
                Ok(self.state_json())
            }

            "setRestoreOnStart" => {
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| {
                        ModuleError::localised(
                            ErrorKind::InvalidParams,
                            msg!("power.err.enabledBool", "params.enabled must be a boolean"),
                        )
                    })?;
                let mut state = lock(&self.state);
                state.config.restore_mode_on_start = enabled;
                // Remember the current mode straight away, so enabling this
                // and rebooting restores what the user can see right now.
                if enabled {
                    state.config.mode = Some(state.mode);
                }
                persist(&self.store, &mut state);
                Ok(saved_response(&state))
            }

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

/// The supervisor loop.
///
/// Runs on its own thread rather than being driven by IPC calls, because it
/// has to keep working when nothing is connected - the whole point is that
/// it manages the machine while the app is closed.
fn spawn_supervisor(
    state: Arc<Mutex<State>>,
    store: ConfigStore,
    paths: limits::LimitPaths,
    announce: Announcer,
    sensors: Sensors,
) {
    std::thread::spawn(move || loop {
        // One pass at a time behind `catch_unwind`: a panic in a sensor
        // read or a parse must cost one tick, not auto-switching for the
        // rest of the daemon's life. The lock recovers from poisoning, so
        // the state a panicking pass held is still usable.
        let pass = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            supervise_once(&state, &store, &paths, &announce, &sensors)
        }));
        let interval = pass.unwrap_or_else(|_| {
            log_warn!("power supervisor pass panicked; carrying on at the next tick");
            Duration::from_secs(lock(&state).config.auto.interval_secs.max(1))
        });
        std::thread::sleep(interval);
    });
}

/// One supervisor pass; returns how long to wait before the next.
fn supervise_once(
    state: &Arc<Mutex<State>>,
    store: &ConfigStore,
    paths: &limits::LimitPaths,
    announce: &Announcer,
    sensors: &Sensors,
) -> Duration {
    let (interval, decision) = {
        let mut guard = lock(state);
        let interval = Duration::from_secs(guard.config.auto.interval_secs.max(1));

        if !guard.config.auto.enabled {
            (interval, None)
        } else {
            let supply = PowerSupplyState::read();
            let inputs = AutoInputs::sample(supply.on_battery, supply.battery_percent, sensors);
            let current = guard.mode;
            let config = guard.config.auto.clone();
            let decision = guard.switcher.observe(inputs, &config, current);

            // A manual choice suspends *refinement*, but not the answer
            // to the power source changing: plugging the machine in is
            // the user speaking too, and more recently.
            match decision {
                Some(d) if d.from_transition => (interval, Some(d)),
                other if manual_override_active(&guard) => {
                    guard.switcher.reset();
                    let _ = other;
                    (interval, None)
                }
                other => (interval, other),
            }
        }
    };

    if let Some(decision) = decision {
        let mode = decision.mode;
        // The whole profile, not just its OS half: a mode has to mean
        // the same thing whether the user picked it or the supervisor
        // did, or "Eco" would quietly be two different settings.
        let before = backend::read_state();
        let mut guard = lock(state);
        let report = apply_profile(&before, mode, &mut guard.config, paths);
        guard.record_apply(&report);
        if !report.is_empty() {
            guard.mode = mode;
            log_info!("power auto-switch -> {mode:?} ({})", decision.reason);
            guard.last_auto_switch = Some(decision.reason);
            // Only worth a disk write when the mode is meant to survive
            // a reboot; otherwise the supervisor would rewrite the file
            // every time conditions change.
            if guard.config.restore_mode_on_start {
                guard.config.mode = Some(mode);
                persist(store, &mut guard);
            }
            // The one mode change nobody asked for. An open app has no
            // other way to learn about it, and this is the case where
            // it is most likely to be sitting there showing the wrong
            // one - the supervisor switches while the user watches.
            drop(guard);
            announce.publish(mode, "auto");
        } else {
            guard.last_auto_switch = Some(msg!(
                "power.autoSwitch.failed",
                { "mode" => format!("{mode:?}"), "failed" => report.failed.join("; ") },
                "{mode} failed: {failed}"
            ));
            log_warn!(
                "power auto-switch to {mode:?} failed: {}",
                report.failed.join("; ")
            );
        }
    }

    interval
}

/// The watcher loop: [`watch_once`] every [`watch::INTERVAL`], for as long
/// as any handle on the module exists.
///
/// A thread of the daemon rather than of the app for the same reason the
/// supervisor is: the other writers do not wait for a window to be open,
/// and the fan curve has to follow Fn+P with nobody looking.
fn spawn_watcher(
    state: Arc<Mutex<State>>,
    store: ConfigStore,
    paths: limits::LimitPaths,
    announce: Announcer,
    alive: Weak<()>,
) {
    std::thread::spawn(move || loop {
        std::thread::sleep(watch::INTERVAL);
        // Held for the look, so a module dropped mid-way finishes it first.
        let Some(_alive) = alive.upgrade() else {
            return;
        };
        watch_once(&state, &store, &paths, &announce);
    });
}

fn watch_once(
    state: &Arc<Mutex<State>>,
    store: &ConfigStore,
    paths: &limits::LimitPaths,
    announce: &Announcer,
) -> Option<PowerMode> {
    let mut guard = lock(state);
    let now = watch::Knobs::read(paths);
    let found = watch::examine(
        guard.mode,
        &guard.expected,
        &now,
        guard.expected_at.elapsed(),
    );

    if let Some(profile) = found.same_mode_profile {
        guard.expected.platform_profile = Some(profile);
    }

    // Announced once the lock is released: a listener's first move is to
    // ask for the state, and it must not have to wait on this.
    let mut news = Vec::new();
    for finding in found.overrides {
        let known = guard.overrides.iter().position(|o| o.knob == finding.knob);
        if known.is_some_and(|at| guard.overrides[at] == finding) {
            continue;
        }
        log_warn!(
            "power: {} was changed by another program ({} -> {}{})",
            finding.knob,
            finding.expected,
            finding.found,
            if finding.reverted {
                ", straight after pyren set it"
            } else {
                ""
            }
        );
        news.push(finding.clone());
        match known {
            Some(at) => guard.overrides[at] = finding,
            None => guard.overrides.push(finding),
        }
    }

    let Some((mode, profile)) = found.adopt else {
        drop(guard);
        news.iter().for_each(|finding| announce.overridden(finding));
        return None;
    };
    let from = guard.mode;
    // The envelope belongs to the mode, whoever picked it; the OS profile
    // is left alone - see `watch` for why pushing it back is a loop.
    let envelope = apply_envelope(mode, &mut guard.config, paths);
    guard.mode = mode;
    guard.config.mode = Some(mode);
    guard.expected = watch::Knobs {
        platform_profile: Some(profile.clone()),
        ..envelope.expected
    };
    guard.expected_at = Instant::now();
    // Someone other than the supervisor picked this, which is what a
    // manual choice is: the supervisor stands back, then works around it.
    guard.manual_override_at = Some(Instant::now());
    guard.switcher.reset();
    guard.switcher.adopt(mode);
    guard.last_external = Some(msg!(
        "power.external.followed",
        { "profile" => profile.clone(), "mode" => mode.as_str() },
        "another program set the firmware profile to {profile}; pyren followed it into {mode}"
    ));
    persist(store, &mut guard);
    drop(guard);

    log_info!(
        "power: firmware profile changed to {profile} elsewhere, following {from:?} -> {mode:?}"
    );
    news.iter().for_each(|finding| announce.overridden(finding));
    announce.publish(mode, "external");
    Some(mode)
}

fn manual_override_active(state: &State) -> bool {
    let Some(at) = state.manual_override_at else {
        return false;
    };
    at.elapsed() < Duration::from_secs(state.config.auto.manual_override_secs)
}

/// Writes the current config, recording rather than propagating a failure:
/// a setting that could not be saved has still been applied, and the user
/// needs to be told it won't survive a restart - not to have the call fail.
/// Applies a whole profile: the OS-level preference, then the power
/// envelope the fans actually feel.
///
/// Both halves are best-effort and both are reported, because on any given
/// machine either can be missing - board 8D2F has no firmware platform
/// profile at all, and its profiles are entirely the envelope half.
///
/// Deliberately does **not** touch the fans. A lower power limit makes the
/// fans spin less because there is less heat, which is the honest way to
/// get there; reaching across into the fan module to also command a fan
/// mode would put two owners on one piece of hardware.
fn apply_profile(
    before: &BackendState,
    mode: PowerMode,
    config: &mut PowerConfig,
    paths: &limits::LimitPaths,
) -> ApplyReport {
    let mut report = backend::apply(before, mode, config.apply_to_os_profile);
    // The firmware refused the mode and the OS half was put back: capping
    // the CPU for a mode the machine is not in would be a third state.
    if report.rolled_back {
        return report;
    }
    let envelope = apply_envelope(mode, config, paths);
    report.applied.extend(envelope.applied);
    report.failed.extend(envelope.failed);
    report.expected.turbo = envelope.expected.turbo;
    report.expected.limits = envelope.expected.limits;
    report
}

/// The half of a profile that is this daemon's alone - the package limits
/// and turbo - and the one a mode followed from outside still gets.
///
/// What it leaves in `expected` is what the machine reads afterwards, for
/// each knob it was meant to set and did not fail to: a value the kernel
/// clamped is the value to watch, and one this daemon could not write is
/// not its to watch at all.
///
/// A mode nobody tuned writes nothing at all, unless the envelope on the
/// machine is still one this daemon wrote for a tuned mode - then the stock
/// envelope is put back once, and the knobs are the firmware's again (see
/// `PowerConfig::envelope_owned`).
fn apply_envelope(
    mode: PowerMode,
    config: &mut PowerConfig,
    paths: &limits::LimitPaths,
) -> ApplyReport {
    let mut report = ApplyReport {
        applied: Vec::new(),
        failed: Vec::new(),
        expected: watch::Knobs::default(),
        rolled_back: false,
    };

    let tuning = config.tuning.get(mode);
    if tuning.is_default() && !config.envelope_owned {
        return report;
    }

    let stock = config.stock_limits.unwrap_or_default();
    let target = tuning.target(stock).clamp_to_stock(stock);

    if !target.is_empty() {
        let (applied, failed) = limits::apply(paths, target);
        let now = limits::read(paths);
        let refused = |label: &str| failed.iter().any(|f: &String| f.starts_with(label));
        let keep = |wanted: Option<u64>, now: Option<u64>, label| {
            wanted.and(now).filter(|_| !refused(label))
        };
        report.expected.limits = Limits {
            pl1_uw: keep(target.pl1_uw, now.pl1_uw, "PL1"),
            pl2_uw: keep(target.pl2_uw, now.pl2_uw, "PL2"),
            pl4_uw: keep(target.pl4_uw, now.pl4_uw, "PL4"),
        };
        report.applied.extend(applied);
        report.failed.extend(failed);
    }

    match limits::apply_turbo(paths, tuning.turbo) {
        Some(Ok(message)) => {
            report.expected.turbo = limits::read_turbo(paths);
            report.applied.push(message);
        }
        Some(Err(e)) => report.failed.push(e),
        // Already where it was asked to be - which is still this daemon's
        // setting to watch.
        None => report.expected.turbo = limits::read_turbo(paths),
    }

    // Owned while a tuned mode is in force; handed back once the stock
    // envelope is really back, and kept otherwise so the next mode change
    // tries again rather than leaving a cap nobody will ever lift.
    config.envelope_owned = !tuning.is_default() || !report.failed.is_empty();
    report
}

/// The stock envelope to trust: [`highest`] of what is on file and what the
/// machine reads, after throwing out what cannot be a real ceiling.
///
/// - Values no laptop has (see [`Limits::without_absurd`]) are dropped from
///   both, so a hand-edited `pl1Uw: 200000000000` is not a licence.
/// - A stored PL4 above the one read now is not believed: nothing here
///   ever lowers PL4, so the firmware's own value is the reading.
/// - PL1 and PL2 are held under PL4, and PL1 under PL2
///   ([`Limits::ordered`]): a sustained limit above the instantaneous one
///   is a corrupt file or another tool's leftovers, not what shipped.
///
/// `constraint_*_max_power_uw` is deliberately not a ceiling here either;
/// see [`Limits::clamp_to_stock`] for the machine where it reads a third of
/// the real limit.
fn sane_stock(stored: Option<Limits>, observed: Limits) -> Limits {
    let observed = observed.without_absurd();
    let mut stored = stored.unwrap_or_default().without_absurd();
    if let (Some(on_file), Some(now)) = (stored.pl4_uw, observed.pl4_uw) {
        stored.pl4_uw = Some(on_file.min(now));
    }
    highest(Some(stored), observed).ordered()
}

/// The mode a restore at boot actually applies.
///
/// A machine that booted on battery does not get Performance or Unlimited
/// back just because that was the last mode on mains: with nobody at the
/// keyboard yet, the battery preference is the honest guess, and the
/// supervisor or the user can raise it within seconds.
fn boot_mode(saved: PowerMode, config: &PowerConfig, on_battery: Option<bool>) -> PowerMode {
    match (saved, on_battery) {
        (PowerMode::Performance | PowerMode::Unlimited, Some(true)) => {
            let battery = config.auto.preferred(true);
            log_info!("power: booted on battery, restoring {battery:?} instead of {saved:?}");
            battery
        }
        _ => saved,
    }
}

/// Keeps the larger of each recorded limit.
///
/// See `PowerConfig::stock_limits`: a value higher than the one on file can
/// only have come from the firmware, so it replaces ours; a lower one is
/// most likely our own cap still in force from the last session, and must
/// not be mistaken for the machine's ceiling.
fn highest(stored: Option<Limits>, observed: Limits) -> Limits {
    let stored = stored.unwrap_or_default();
    fn pick(a: Option<u64>, b: Option<u64>) -> Option<u64> {
        match (a, b) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (some, None) | (None, some) => some,
        }
    }
    Limits {
        pl1_uw: pick(stored.pl1_uw, observed.pl1_uw),
        pl2_uw: pick(stored.pl2_uw, observed.pl2_uw),
        pl4_uw: pick(stored.pl4_uw, observed.pl4_uw),
    }
}

fn persist(store: &ConfigStore, state: &mut State) {
    match store.save("power", &state.config) {
        Ok(()) => state.last_save_error = None,
        Err(e) => {
            log_warn!("could not save power config: {e}");
            state.last_save_error = Some(e.to_string());
        }
    }
}

fn saved_response(state: &State) -> Value {
    json!({
        "auto": state.config.auto,
        "restoreModeOnStart": state.config.restore_mode_on_start,
        "saved": state.last_save_error.is_none(),
        "saveError": state.last_save_error,
    })
}

/// Best guess at the mode the machine is already in, from whichever
/// mechanism is present.
fn current_mode() -> Option<PowerMode> {
    let state = backend::read_state();
    let name = state
        .platform_profile
        .or(state.power_profiles_daemon)
        .or(state.tlp)?;
    mode_for_profile(&name)
}

/// The mode a firmware or OS profile name belongs to.
pub(crate) fn mode_for_profile(name: &str) -> Option<PowerMode> {
    match name {
        "low-power" | "quiet" | "cool" | "power-saver" => Some(PowerMode::Eco),
        "balanced" => Some(PowerMode::Balanced),
        "balanced-performance" | "performance" => Some(PowerMode::Performance),
        _ => None,
    }
}

/// A panicking supervisor must not take the whole daemon down with it.
/// Watts, as a percentage of a stock limit in microwatts.
///
/// Refused rather than guessed when the machine's stock is unknown: a
/// percentage of nothing would be applied as a limit of nothing.
fn percent_of(watts: f64, stock_uw: Option<u64>) -> Result<u8, ModuleError> {
    let stock_uw = stock_uw.ok_or_else(|| {
        ModuleError::localised(
            ErrorKind::NotCapable,
            msg!(
                "power.err.noPackageLimit",
                "this machine exposes no package power limit, so there is nothing to tune"
            ),
        )
    })?;
    if !watts.is_finite() || watts <= 0.0 {
        return Err(ModuleError::localised(
            ErrorKind::InvalidParams,
            msg!(
                "power.err.wattsPositive",
                "power limits must be a positive number of watts"
            ),
        ));
    }
    let percent = (watts * 1_000_000.0 / stock_uw as f64 * 100.0).round();
    Ok(percent.clamp(1.0, 100.0) as u8)
}

fn lock(state: &Arc<Mutex<State>>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u64 = 1_000_000;

    /// `as_str` is what seeds a fan curve's profile key at startup and
    /// serde is what the `power.mode` event carries. If they ever disagree,
    /// a curve drawn for a profile stops being found the moment the mode
    /// changes - silently, and looking exactly like "the curve reset
    /// itself". Cheap to pin, miserable to debug.
    #[test]
    fn the_wire_name_is_the_serde_name() {
        for mode in PowerMode::ALL {
            let serde_name = serde_json::to_value(mode).unwrap();
            assert_eq!(serde_name, json!(mode.as_str()), "{mode:?}");
            assert_eq!(PowerMode::parse(mode.as_str()), Some(*mode));
        }
    }

    /// The ratchet this guards against: reading the envelope at startup
    /// while Eco is in force would otherwise record Eco's reduced limit as
    /// the machine's ceiling, and every boot would shave a little more off.
    #[test]
    fn a_capped_machine_does_not_become_its_own_new_ceiling() {
        let stored = Limits {
            pl1_uw: Some(77 * W),
            pl2_uw: Some(77 * W),
            pl4_uw: None,
        };
        let while_capped = Limits {
            pl1_uw: Some(34 * W),
            pl2_uw: Some(42 * W),
            pl4_uw: None,
        };

        assert_eq!(highest(Some(stored), while_capped), stored);
    }

    /// A value above what is on file can only have come from the firmware.
    #[test]
    fn a_higher_reading_replaces_the_recorded_stock() {
        let stored = Limits {
            pl1_uw: Some(45 * W),
            ..Default::default()
        };
        let observed = Limits {
            pl1_uw: Some(77 * W),
            pl4_uw: Some(168 * W),
            ..Default::default()
        };

        let merged = highest(Some(stored), observed);
        assert_eq!(merged.pl1_uw, Some(77 * W));
        assert_eq!(
            merged.pl4_uw,
            Some(168 * W),
            "a limit seen for the first time is recorded"
        );
    }

    /// A hand-edited or corrupt `stockLimits` is not a licence: an absurd
    /// value is dropped, a PL4 above the machine's own is not believed, and
    /// PL1/PL2 are held under it.
    #[test]
    fn a_stored_ceiling_no_machine_could_have_is_not_trusted() {
        let observed = Limits {
            pl1_uw: Some(45 * W),
            pl2_uw: Some(60 * W),
            pl4_uw: Some(168 * W),
        };
        let edited = Limits {
            pl1_uw: Some(200 * W),
            pl2_uw: Some(90_000 * W),
            pl4_uw: Some(400 * W),
        };
        assert_eq!(
            sane_stock(Some(edited), observed),
            Limits {
                // The absurd PL2 on file is dropped for the 60 W read now,
                // and the stored 200 W PL1 is lowered under it.
                pl1_uw: Some(60 * W),
                pl2_uw: Some(60 * W),
                pl4_uw: Some(168 * W),
            }
        );

        let crossed = Limits {
            pl1_uw: Some(90 * W),
            pl2_uw: Some(77 * W),
            pl4_uw: None,
        };
        assert_eq!(
            sane_stock(Some(crossed), Limits::default()).pl1_uw,
            Some(77 * W),
            "PL1 above PL2 is lowered to it"
        );
    }

    #[test]
    fn booting_on_battery_does_not_restore_a_mains_only_mode() {
        let config = PowerConfig::default();
        let battery = config.auto.preferred(true);
        assert_eq!(
            boot_mode(PowerMode::Unlimited, &config, Some(true)),
            battery
        );
        assert_eq!(
            boot_mode(PowerMode::Performance, &config, Some(true)),
            battery
        );
        assert_eq!(
            boot_mode(PowerMode::Performance, &config, Some(false)),
            PowerMode::Performance
        );
        assert_eq!(
            boot_mode(PowerMode::Eco, &config, Some(true)),
            PowerMode::Eco
        );
        assert_eq!(
            boot_mode(PowerMode::Unlimited, &config, None),
            PowerMode::Unlimited,
            "a desktop has no battery to protect"
        );
    }

    #[test]
    fn the_os_profile_is_left_to_its_manager_unless_asked() {
        assert!(!PowerConfig::default().apply_to_os_profile);
        let old: PowerConfig = serde_json::from_value(json!({ "applyToOsProfile": true })).unwrap();
        assert!(old.apply_to_os_profile, "a stored choice is kept");
    }

    #[test]
    fn watts_become_a_percentage_of_this_machines_own_limit() {
        assert_eq!(percent_of(38.5, Some(77 * W)).unwrap(), 50);
        assert_eq!(percent_of(77.0, Some(77 * W)).unwrap(), 100);
    }

    #[test]
    fn a_request_above_stock_is_capped_rather_than_refused() {
        assert_eq!(percent_of(200.0, Some(77 * W)).unwrap(), 100);
    }

    #[test]
    fn tuning_a_machine_with_no_power_limit_is_an_error_not_a_no_op() {
        assert!(percent_of(30.0, None).is_err());
        assert!(percent_of(-5.0, Some(77 * W)).is_err());
        assert!(percent_of(f64::NAN, Some(77 * W)).is_err());
    }

    /// A machine with no powercap still gets the half of the profile it
    /// does have, and says so.
    ///
    /// `apply_profile` is the one function here that writes, so it is
    /// pointed at a directory that contains nothing first. Without that
    /// this test changes the machine it is running on - under `sudo cargo
    /// test` it would leave the developer's laptop in Eco - and what it
    /// asserts would depend on what that laptop happens to offer.
    /// `tests/profiles.rs` is where the writing half is exercised against
    /// a machine it is allowed to change.
    #[test]
    fn a_profile_on_a_machine_without_powercap_still_applies_the_os_half() {
        let nowhere =
            std::env::temp_dir().join(format!("pyren-power-nowhere-{}", std::process::id()));
        std::env::set_var("PYREN_PLATFORM_PROFILE", nowhere.join("platform_profile"));
        std::env::set_var("PYREN_CPU_ROOT", nowhere.join("cpu"));
        std::env::set_var("PYREN_TOOLS_DIR", nowhere.join("bin"));

        let mut config = PowerConfig::default();
        let before = backend::read_state();
        let report = apply_profile(
            &before,
            PowerMode::Eco,
            &mut config,
            &limits::LimitPaths::default(),
        );

        assert!(!report.applied.iter().any(|a| a.starts_with("PL")));
        assert!(!report.applied.iter().any(|a| a.starts_with("turbo")));

        for name in [
            "PYREN_PLATFORM_PROFILE",
            "PYREN_CPU_ROOT",
            "PYREN_TOOLS_DIR",
        ] {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn modes_parse_case_insensitively_and_reject_junk() {
        assert_eq!(PowerMode::parse("ECO"), Some(PowerMode::Eco));
        assert_eq!(PowerMode::parse("unlimited"), Some(PowerMode::Unlimited));
        assert_eq!(PowerMode::parse("turbo"), None);
    }

    /// The announcement is what keeps an open app in step with a mode
    /// changed behind its back, so its shape is part of the contract.
    #[test]
    fn a_mode_change_is_announced_with_who_asked_for_it() {
        let bus = Arc::new(EventBus::new());
        let announce = Announcer::default();
        announce
            .0
            .set(Arc::clone(&bus))
            .expect("a fresh announcer is empty");

        announce.publish(PowerMode::Performance, "hotkey");

        let batch = bus.read_since(0, Duration::from_millis(0));
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].topic, "power.mode");
        assert_eq!(batch.events[0].payload["mode"], "performance");
        assert_eq!(batch.events[0].payload["source"], "hotkey");
    }

    /// `pyren-check` and every test here build a module with nobody
    /// listening. Publishing into that must be a no-op, not a panic.
    #[test]
    fn announcing_with_nobody_listening_does_nothing_at_all() {
        Announcer::default().publish(PowerMode::Eco, "request");
    }

    /// The performance key steps through every mode and comes back round.
    /// Unlimited is in the loop because a key press is the user choosing -
    /// what the supervisor may not pick on its own is a different rule.
    #[test]
    fn the_modes_cycle_in_the_order_the_app_shows_them() {
        assert_eq!(PowerMode::Eco.next(), PowerMode::Balanced);
        assert_eq!(PowerMode::Balanced.next(), PowerMode::Performance);
        assert_eq!(PowerMode::Performance.next(), PowerMode::Unlimited);
        assert_eq!(PowerMode::Unlimited.next(), PowerMode::Eco);

        let mut seen = vec![PowerMode::Eco];
        while seen.len() < PowerMode::ALL.len() {
            seen.push(seen[seen.len() - 1].next());
        }
        assert_eq!(
            seen,
            PowerMode::ALL,
            "every mode is reachable by pressing the key"
        );
    }

    #[test]
    fn modes_serialize_as_the_names_the_frontend_sends() {
        assert_eq!(
            serde_json::to_string(&PowerMode::Performance).unwrap(),
            "\"performance\""
        );
    }

    /// A store under the temp dir, so tests never touch the real /etc.
    fn test_store(tag: &str) -> ConfigStore {
        let root =
            std::env::temp_dir().join(format!("pyren-power-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        ConfigStore::at(root)
    }

    #[test]
    fn auto_config_survives_a_daemon_restart() {
        let store = test_store("restart");

        let module = PowerModule::with_store(store.clone());
        let wanted = AutoConfig {
            enabled: true,
            load_high: 0.42,
            ..AutoConfig::default()
        };
        module
            .call("setAutoConfig", serde_json::to_value(&wanted).unwrap())
            .expect("setAutoConfig should succeed");

        // A second module over the same store stands in for a restart.
        let restarted = PowerModule::with_store(store);
        let state = lock(&restarted.state);
        assert!(state.config.auto.enabled);
        assert_eq!(state.config.auto.load_high, 0.42);
    }

    /// Turning the supervisor back on must not act on anything that
    /// happened while it was off - above all a hand-picked baseline or a
    /// power source it last saw hours ago.
    #[test]
    fn switching_auto_back_on_starts_the_supervisor_from_scratch() {
        let module = PowerModule::with_store(test_store("re-enable"));
        let off = AutoConfig {
            enabled: false,
            ..AutoConfig::default()
        };
        module
            .call("setAutoConfig", serde_json::to_value(&off).unwrap())
            .unwrap();
        lock(&module.state).switcher.adopt(PowerMode::Unlimited);

        let on = AutoConfig {
            enabled: true,
            ..off
        };
        module
            .call("setAutoConfig", serde_json::to_value(&on).unwrap())
            .unwrap();

        let state = lock(&module.state);
        // Only the baseline is asserted: the supervisor thread is live, and
        // may already have taken a real sample of the power source - which
        // is a first sample, and so cannot bring a baseline back.
        assert_eq!(
            state.switcher.manual_baseline(&state.config.auto, false),
            None
        );
        assert_eq!(
            state.switcher.manual_baseline(&state.config.auto, true),
            None
        );
    }

    /// A config the supervisor cannot run on is refused, and the stored
    /// one is left as it was.
    #[test]
    fn an_auto_config_with_crossed_thresholds_is_refused() {
        let module = PowerModule::with_store(test_store("crossed"));
        let crossed = AutoConfig {
            load_low: 0.9,
            load_high: 0.5,
            ..AutoConfig::default()
        };
        assert!(module
            .call("setAutoConfig", serde_json::to_value(&crossed).unwrap())
            .is_err());
        assert_eq!(
            lock(&module.state).config.auto.load_low,
            AutoConfig::default().load_low
        );
    }

    #[test]
    fn restore_on_start_records_the_current_mode() {
        let store = test_store("restore-flag");
        let module = PowerModule::with_store(store.clone());

        module
            .call("setRestoreOnStart", json!({ "enabled": true }))
            .expect("setRestoreOnStart should succeed");

        let saved = store.load::<PowerConfig>("power");
        assert!(saved.is_from_disk());
        assert!(saved.value.restore_mode_on_start);
        // Enabling it should capture a mode straight away, so a reboot
        // restores what the user could see when they ticked the box.
        assert!(saved.value.mode.is_some());
    }

    #[test]
    fn a_bad_enabled_parameter_is_rejected() {
        let module = PowerModule::with_store(test_store("bad-param"));
        assert!(module
            .call("setRestoreOnStart", json!({ "enabled": "yes" }))
            .is_err());
    }
}
