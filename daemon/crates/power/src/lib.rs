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
//! | `power.setAutoConfig` | partial [`AutoConfig`] | the stored config |

mod auto;
mod backend;
mod supply;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Shared between the IPC handlers and the supervisor thread.
#[derive(Debug)]
struct State {
    mode: PowerMode,
    config: AutoConfig,
    switcher: AutoSwitcher,
    /// When the user last set a mode by hand; the supervisor stays out of
    /// the way for `manual_override_secs` after that.
    manual_override_at: Option<Instant>,
    last_auto_switch: Option<String>,
}

pub struct PowerModule {
    state: Arc<Mutex<State>>,
}

impl PowerModule {
    pub fn new() -> Self {
        Self::with_config(AutoConfig::default())
    }

    pub fn with_config(config: AutoConfig) -> Self {
        let state = Arc::new(Mutex::new(State {
            // Start from whatever the machine is already set to rather than
            // assuming Balanced, so the first supervisor tick compares
            // against reality.
            mode: current_mode().unwrap_or(PowerMode::Balanced),
            config,
            switcher: AutoSwitcher::default(),
            manual_override_at: None,
            last_auto_switch: None,
        }));

        spawn_supervisor(Arc::clone(&state));
        Self { state }
    }

    fn set_mode(&self, mode: PowerMode, manual: bool) -> ApplyReport {
        let report = backend::apply(mode);
        let mut state = lock(&self.state);
        // Only record the mode if something actually took effect; otherwise
        // the UI would show a mode the machine isn't in.
        if !report.is_empty() {
            state.mode = mode;
        }
        if manual {
            state.manual_override_at = Some(Instant::now());
            state.switcher.reset();
        }
        report
    }

    fn state_json(&self) -> Value {
        let state = lock(&self.state);
        let supply = PowerSupplyState::read();
        let override_remaining = state
            .manual_override_at
            .map(|at| {
                Duration::from_secs(state.config.manual_override_secs)
                    .saturating_sub(at.elapsed())
                    .as_secs()
            })
            .filter(|remaining| *remaining > 0);

        json!({
            "mode": state.mode,
            "backend": backend::read_state(),
            "supply": supply,
            "auto": state.config,
            "autoOverrideSecondsLeft": override_remaining,
            "lastAutoSwitch": state.last_auto_switch,
        })
    }
}

impl Default for PowerModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for PowerModule {
    fn id(&self) -> &'static str {
        "power"
    }

    /// True when the machine offers any way at all to change its power
    /// behaviour. On hardware with none of them the UI should show the
    /// modes as unavailable rather than pretending they work.
    fn is_supported(&self) -> bool {
        !backend::read_state().available.is_empty()
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
                let config: AutoConfig = serde_json::from_value(params)
                    .map_err(|e| ModuleError::Other(format!("invalid auto config: {e}")))?;
                let mut state = lock(&self.state);
                state.switcher.reset();
                state.config = config;
                serde_json::to_value(&state.config).map_err(|e| ModuleError::Other(e.to_string()))
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
fn spawn_supervisor(state: Arc<Mutex<State>>) {
    std::thread::spawn(move || loop {
        let (interval, decision) = {
            let mut guard = lock(&state);
            let interval = Duration::from_secs(guard.config.interval_secs.max(1));

            if !guard.config.enabled {
                (interval, None)
            } else if manual_override_active(&guard) {
                guard.switcher.reset();
                (interval, None)
            } else {
                let inputs = AutoInputs::sample(PowerSupplyState::read().on_battery);
                let current = guard.mode;
                let config = guard.config.clone();
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
    at.elapsed() < Duration::from_secs(state.config.manual_override_secs)
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
}
