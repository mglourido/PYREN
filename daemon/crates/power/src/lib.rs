//! Power mode module: the Eco / Balanced / Performance / Unlimited switch,
//! plus the background supervisor that can drive it automatically.
//!
//! On an HP laptop the firmware's ACPI platform profile is the real thing
//! this drives (the same switch as Fn+P). Elsewhere it falls back to
//! power-profiles-daemon and the CPU's energy-performance hint, which makes
//! the module useful - and testable - on ordinary Linux machines too.
//!
//! | method | params | result |
//! |---|---|---|
//! | `power.getState` | none | current mode, backend state, battery, auto-switch config |
//! | `power.setMode` | `{ "mode": "eco"\|"balanced"\|"performance"\|"unlimited" }` | what was applied |
//! | `power.setAutoConfig` | [`AutoConfig`] | the stored config, and whether it reached disk |
//! | `power.setRestoreOnStart` | `{ "enabled": bool }` | as above |
//! | `power.setApplyToOsProfile` | `{ "enabled": bool }` | as `getState` |
//! | `power.setTuning` | `{ "mode"?, "pl1W"?, "pl2W"?, "turbo"? }` | as `getState`; defaults to the current mode |
//!
//! A **mode is a profile**, and it has three parts that are applied
//! separately because they belong to different owners:
//!
//! | part | mechanism | optional? |
//! |---|---|---|
//! | the laptop's own profile | ACPI `platform_profile` | no |
//! | the OS profile | power-profiles-daemon | yes - `applyToOsProfile` |
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
//! Settings live in `power.json` (see `pyren-config`), so the
//! supervisor keeps running with the user's rules after a reboot - which
//! is the whole point of it being a daemon rather than part of the app.

mod auto;
mod backend;
mod limits;
mod supply;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pyren_config::{ConfigStore, LoadOutcome};
use pyren_core::{msg, ErrorKind, EventBus, Module, ModuleError, ModuleResult, Msg};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use auto::{AutoConfig, AutoInputs, AutoSwitcher};
pub use backend::{ApplyReport, BackendState};
pub use limits::{Limits, ModeTuning, Tuning};
pub use supply::PowerSupplyState;

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
    pub const ALL: &'static [Self] =
        &[Self::Eco, Self::Balanced, Self::Performance, Self::Unlimited];

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
}

/// What is persisted to `power.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// power profile (power-profiles-daemon), or only the laptop's own
    /// firmware profile.
    ///
    /// The two are separate on purpose: the firmware profile is what moves
    /// the EC's fan curve and its internal power states, while the OS
    /// profile is what the desktop's battery menu shows. Wanting the first
    /// without the second is a reasonable thing to want, and the app has
    /// had a switch for it since before the daemon honoured it.
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
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            auto: AutoConfig::default(),
            mode: None,
            restore_mode_on_start: false,
            // On by default: someone who picks "Eco" in this app almost
            // always means the whole machine, and the switch is there for
            // the case where they do not.
            apply_to_os_profile: true,
            stock_limits: None,
            tuning: ModeTuning::default(),
        }
    }
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
                println!("pyren-daemon: power config loaded from {}", store.path_for("power").display());
            }
            LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                eprintln!(
                    "pyren-daemon: power config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                eprintln!(
                    "pyren-daemon: power config is version {found}, newer than this \
                     build understands; using defaults and leaving the file alone"
                );
            }
        }
        let mut config = loaded.value;

        // Read the envelope before anything has had a chance to change it.
        let limit_paths = limits::LimitPaths::discover();
        let observed = limits::read(&limit_paths);
        config.stock_limits = Some(highest(config.stock_limits, observed));

        // Start from whatever the machine is already set to rather than
        // assuming Balanced, so the first supervisor tick compares against
        // reality.
        let mut mode = current_mode().unwrap_or(PowerMode::Balanced);

        if config.restore_mode_on_start {
            if let Some(saved) = config.mode {
                let report = apply_profile(saved, &config, &limit_paths);
                if report.is_empty() {
                    eprintln!(
                        "pyren-daemon: could not restore power mode {saved:?}: {}",
                        report.failed.join("; ")
                    );
                } else {
                    println!("pyren-daemon: restored power mode {saved:?}");
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
        }));

        let announce = Announcer::default();
        spawn_supervisor(
            Arc::clone(&state),
            store.clone(),
            limit_paths.clone(),
            announce.clone(),
        );
        Self { state, store, limits: limit_paths, announce }
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
        let report = self.set_mode(to, true, "hotkey");
        Cycled { from, to: self.mode(), asked_for: to, report }
    }

    /// `source` says who asked, and travels with the announcement: a UI
    /// that made the change itself can tell it apart from one made behind
    /// its back, which is the difference between a redundant re-read and a
    /// necessary one.
    fn set_mode(&self, mode: PowerMode, manual: bool, source: &str) -> ApplyReport {
        let report = {
            let state = lock(&self.state);
            apply_profile(mode, &state.config, &self.limits)
        };
        let mut state = lock(&self.state);
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
        let state = lock(&self.state);
        let supply = PowerSupplyState::read();
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
            "backend": backend::read_state(),
            "limits": {
                "available": self.limits.has_limits(),
                "turboAvailable": self.limits.has_turbo(),
                "stock": state.config.stock_limits,
                "current": limits::read(&self.limits),
                "turbo": limits::read_turbo(&self.limits),
                "tuning": state.config.tuning,
            },
            "supply": supply,
            "auto": state.config.auto,
            "restoreModeOnStart": state.config.restore_mode_on_start,
            "applyToOsProfile": state.config.apply_to_os_profile,
            "autoOverrideSecondsLeft": override_remaining,
            "lastAutoSwitch": state.last_auto_switch,
            "configPath": self.store.path_for("power"),
            "configSaveError": state.last_save_error,
        })
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

                let report = self.set_mode(mode, true, "request");
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
                let mut state = lock(&self.state);
                state.switcher.reset();
                state.config.auto = auto;
                persist(&self.store, &mut state);
                Ok(saved_response(&state))
            }

            "setTuning" => {
                let mode = params
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(PowerMode::parse)
                    .unwrap_or_else(|| lock(&self.state).mode);

                let mut state = lock(&self.state);
                let mut tuning = state.config.tuning.get(mode);
                let stock = state.config.stock_limits.unwrap_or_default();

                // Watts on the wire, because that is what the user is
                // shown; percentages on disk, because that is what
                // survives being restored onto different hardware.
                if let Some(watts) = params.get("pl1W").and_then(Value::as_f64) {
                    tuning.pl1_percent = percent_of(watts, stock.pl1_uw)?;
                }
                if let Some(watts) = params.get("pl2W").and_then(Value::as_f64) {
                    tuning.pl2_percent = percent_of(watts, stock.pl2_uw)?;
                }
                if let Some(turbo) = params.get("turbo").and_then(Value::as_bool) {
                    tuning.turbo = turbo;
                }
                state.config.tuning.set(mode, tuning);
                let applies_now = state.mode == mode;
                persist(&self.store, &mut state);
                drop(state);

                // Tuning the mode the machine is in should be audible
                // straight away, not after the next mode switch.
                if applies_now {
                    self.set_mode(mode, true, "tuning");
                }
                Ok(self.state_json())
            }

            "setApplyToOsProfile" => {
                let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!("power.err.enabledBool", "params.enabled must be a boolean"),
                    )
                })?;
                let mode = {
                    let mut state = lock(&self.state);
                    state.config.apply_to_os_profile = enabled;
                    persist(&self.store, &mut state);
                    state.mode
                };
                // Re-apply so the answer takes effect now rather than at
                // the next mode change - turning it on and seeing nothing
                // happen would look broken.
                self.set_mode(mode, true, "osProfile");
                Ok(self.state_json())
            }

            "setRestoreOnStart" => {
                let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
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
) {
    std::thread::spawn(move || loop {
        let (interval, decision) = {
            let mut guard = lock(&state);
            let interval = Duration::from_secs(guard.config.auto.interval_secs.max(1));

            if !guard.config.auto.enabled {
                (interval, None)
            } else {
                let supply = PowerSupplyState::read();
                let inputs = AutoInputs::sample(supply.on_battery, supply.battery_percent);
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
            let report = {
                let guard = lock(&state);
                apply_profile(mode, &guard.config, &paths)
            };
            let mut guard = lock(&state);
            if !report.is_empty() {
                guard.mode = mode;
                println!("pyren-daemon: power auto-switch -> {mode:?} ({})", decision.reason);
                guard.last_auto_switch = Some(decision.reason);
                // Only worth a disk write when the mode is meant to survive
                // a reboot; otherwise the supervisor would rewrite the file
                // every time conditions change.
                if guard.config.restore_mode_on_start {
                    guard.config.mode = Some(mode);
                    persist(&store, &mut guard);
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
                eprintln!(
                    "pyren-daemon: power auto-switch to {mode:?} failed: {}",
                    report.failed.join("; ")
                );
            }
        }

        std::thread::sleep(interval);
    });
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
fn apply_profile(mode: PowerMode, config: &PowerConfig, paths: &limits::LimitPaths) -> ApplyReport {
    let mut report = backend::apply(mode, config.apply_to_os_profile);

    let stock = config.stock_limits.unwrap_or_default();
    let tuning = config.tuning.get(mode);
    let target = tuning.target(stock).clamp_to_stock(stock);

    if !target.is_empty() {
        let (applied, failed) = limits::apply(paths, target);
        report.applied.extend(applied);
        report.failed.extend(failed);
    }

    match limits::apply_turbo(paths, tuning.turbo) {
        Some(Ok(message)) => report.applied.push(message),
        Some(Err(e)) => report.failed.push(e),
        None => {}
    }

    report
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
            eprintln!("pyren-daemon: could not save power config: {e}");
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
    let name = state.platform_profile.or(state.power_profiles_daemon)?;
    match name.as_str() {
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
            msg!("power.err.wattsPositive", "power limits must be a positive number of watts"),
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

    /// The ratchet this guards against: reading the envelope at startup
    /// while Eco is in force would otherwise record Eco's reduced limit as
    /// the machine's ceiling, and every boot would shave a little more off.
    #[test]
    fn a_capped_machine_does_not_become_its_own_new_ceiling() {
        let stored = Limits { pl1_uw: Some(77 * W), pl2_uw: Some(77 * W), pl4_uw: None };
        let while_capped = Limits { pl1_uw: Some(34 * W), pl2_uw: Some(42 * W), pl4_uw: None };

        assert_eq!(highest(Some(stored), while_capped), stored);
    }

    /// A value above what is on file can only have come from the firmware.
    #[test]
    fn a_higher_reading_replaces_the_recorded_stock() {
        let stored = Limits { pl1_uw: Some(45 * W), ..Default::default() };
        let observed = Limits { pl1_uw: Some(77 * W), pl4_uw: Some(168 * W) , ..Default::default() };

        let merged = highest(Some(stored), observed);
        assert_eq!(merged.pl1_uw, Some(77 * W));
        assert_eq!(merged.pl4_uw, Some(168 * W), "a limit seen for the first time is recorded");
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
    #[test]
    fn a_profile_on_a_machine_without_powercap_still_applies_the_os_half() {
        let config = PowerConfig::default();
        let report = apply_profile(PowerMode::Eco, &config, &limits::LimitPaths::default());

        assert!(!report.applied.iter().any(|a| a.starts_with("PL")));
        assert!(!report.applied.iter().any(|a| a.starts_with("turbo")));
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
        announce.0.set(Arc::clone(&bus)).expect("a fresh announcer is empty");

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
        assert_eq!(seen, PowerMode::ALL, "every mode is reachable by pressing the key");
    }

    #[test]
    fn modes_serialize_as_the_names_the_frontend_sends() {
        assert_eq!(serde_json::to_string(&PowerMode::Performance).unwrap(), "\"performance\"");
    }

    /// A store under the temp dir, so tests never touch the real /etc.
    fn test_store(tag: &str) -> ConfigStore {
        let root = std::env::temp_dir()
            .join(format!("pyren-power-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        ConfigStore::at(root)
    }

    #[test]
    fn auto_config_survives_a_daemon_restart() {
        let store = test_store("restart");

        let module = PowerModule::with_store(store.clone());
        let wanted = AutoConfig { enabled: true, load_high: 0.42, ..AutoConfig::default() };
        module
            .call("setAutoConfig", serde_json::to_value(&wanted).unwrap())
            .expect("setAutoConfig should succeed");

        // A second module over the same store stands in for a restart.
        let restarted = PowerModule::with_store(store);
        let state = lock(&restarted.state);
        assert!(state.config.auto.enabled);
        assert_eq!(state.config.auto.load_high, 0.42);
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
        assert!(module.call("setRestoreOnStart", json!({ "enabled": "yes" })).is_err());
    }
}
