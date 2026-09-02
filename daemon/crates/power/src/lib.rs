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
//!
//! Settings live in `power.json` (see `omen-hub-config`), so the
//! supervisor keeps running with the user's rules after a reboot - which
//! is the whole point of it being a daemon rather than part of the app.

mod auto;
mod backend;
mod supply;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omen_hub_config::{ConfigStore, LoadOutcome};
use omen_hub_core::{Module, ModuleError, ModuleResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use auto::{AutoConfig, AutoInputs, AutoSwitcher};
pub use backend::{ApplyReport, BackendState};
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
    last_auto_switch: Option<String>,
    /// Set when the last write to disk failed, so the UI can say the
    /// setting will not survive a restart instead of quietly losing it.
    last_save_error: Option<String>,
}

pub struct PowerModule {
    state: Arc<Mutex<State>>,
    store: ConfigStore,
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
                println!("omen-hub-daemon: power config loaded from {}", store.path_for("power").display());
            }
            LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                eprintln!(
                    "omen-hub-daemon: power config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                eprintln!(
                    "omen-hub-daemon: power config is version {found}, newer than this build                      understands; using defaults and leaving the file alone"
                );
            }
        }
        let config = loaded.value;

        // Start from whatever the machine is already set to rather than
        // assuming Balanced, so the first supervisor tick compares against
        // reality.
        let mut mode = current_mode().unwrap_or(PowerMode::Balanced);

        if config.restore_mode_on_start {
            if let Some(saved) = config.mode {
                let report = backend::apply(saved);
                if report.is_empty() {
                    eprintln!(
                        "omen-hub-daemon: could not restore power mode {saved:?}: {}",
                        report.failed.join("; ")
                    );
                } else {
                    println!("omen-hub-daemon: restored power mode {saved:?}");
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

        spawn_supervisor(Arc::clone(&state), store.clone());
        Self { state, store }
    }

    /// Path of the file this module reads and writes, for diagnostics.
    pub fn config_path(&self) -> std::path::PathBuf {
        self.store.path_for("power")
    }

    fn set_mode(&self, mode: PowerMode, manual: bool) -> ApplyReport {
        let report = backend::apply(mode);
        let mut state = lock(&self.state);
        // Only record the mode if something actually took effect; otherwise
        // the UI would show a mode the machine isn't in.
        if !report.is_empty() {
            state.mode = mode;
            state.config.mode = Some(mode);
        }
        if manual {
            state.manual_override_at = Some(Instant::now());
            state.switcher.reset();
        }
        persist(&self.store, &mut state);
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
            "supply": supply,
            "auto": state.config.auto,
            "restoreModeOnStart": state.config.restore_mode_on_start,
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
/// answer - the compatibility verdict, `omen-hub-check` - must not have to
/// build a `PowerModule` to get it: constructing one loads config and
/// starts the supervisor, which is a thread that can change the machine's
/// power mode on its own. A question should not have side effects.
pub fn power_mode_available() -> bool {
    !backend::read_state().available.is_empty()
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
                        ModuleError::Other(
                            "params.mode must be one of eco, balanced, performance, unlimited"
                                .to_string(),
                        )
                    })?;

                let report = self.set_mode(mode, true);
                if report.is_empty() {
                    // Every mechanism needs root; an unprivileged daemon
                    // failing here is the expected case in development, so
                    // say which one and why rather than a bare "failed".
                    return Err(ModuleError::PermissionDenied(report.failed.join("; ")));
                }
                serde_json::to_value(report).map_err(|e| ModuleError::Other(e.to_string()))
            }

            "setAutoConfig" => {
                let auto: AutoConfig = serde_json::from_value(params)
                    .map_err(|e| ModuleError::Other(format!("invalid auto config: {e}")))?;
                let mut state = lock(&self.state);
                state.switcher.reset();
                state.config.auto = auto;
                persist(&self.store, &mut state);
                Ok(saved_response(&state))
            }

            "setRestoreOnStart" => {
                let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
                    ModuleError::Other("params.enabled must be a boolean".to_string())
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
fn spawn_supervisor(state: Arc<Mutex<State>>, store: ConfigStore) {
    std::thread::spawn(move || loop {
        let (interval, decision) = {
            let mut guard = lock(&state);
            let interval = Duration::from_secs(guard.config.auto.interval_secs.max(1));

            if !guard.config.auto.enabled {
                (interval, None)
            } else if manual_override_active(&guard) {
                guard.switcher.reset();
                (interval, None)
            } else {
                let inputs = AutoInputs::sample(PowerSupplyState::read().on_battery);
                let current = guard.mode;
                let config = guard.config.auto.clone();
                (interval, guard.switcher.observe(inputs, &config, current))
            }
        };

        if let Some(mode) = decision {
            let report = backend::apply(mode);
            let mut guard = lock(&state);
            if !report.is_empty() {
                guard.mode = mode;
                guard.last_auto_switch = Some(format!("{mode:?} via {}", report.applied.join(", ")));
                println!("omen-hub-daemon: power auto-switch -> {mode:?}");
                // Only worth a disk write when the mode is meant to survive
                // a reboot; otherwise the supervisor would rewrite the file
                // every time conditions change.
                if guard.config.restore_mode_on_start {
                    guard.config.mode = Some(mode);
                    persist(&store, &mut guard);
                }
            } else {
                guard.last_auto_switch = Some(format!("{mode:?} failed: {}", report.failed.join("; ")));
                eprintln!(
                    "omen-hub-daemon: power auto-switch to {mode:?} failed: {}",
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
fn persist(store: &ConfigStore, state: &mut State) {
    match store.save("power", &state.config) {
        Ok(()) => state.last_save_error = None,
        Err(e) => {
            eprintln!("omen-hub-daemon: could not save power config: {e}");
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
fn lock(state: &Arc<Mutex<State>>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_case_insensitively_and_reject_junk() {
        assert_eq!(PowerMode::parse("ECO"), Some(PowerMode::Eco));
        assert_eq!(PowerMode::parse("unlimited"), Some(PowerMode::Unlimited));
        assert_eq!(PowerMode::parse("turbo"), None);
    }

    #[test]
    fn modes_serialize_as_the_names_the_frontend_sends() {
        assert_eq!(serde_json::to_string(&PowerMode::Performance).unwrap(), "\"performance\"");
    }

    /// A store under the temp dir, so tests never touch the real /etc.
    fn test_store(tag: &str) -> ConfigStore {
        let root = std::env::temp_dir()
            .join(format!("omen-hub-power-test-{tag}-{}", std::process::id()));
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
