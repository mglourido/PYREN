//! Background supervisor that picks a power mode on its own.
//!
//! This is the "sistema en segundo plano" from the project's feature
//! notes: watch what the machine is doing and drop to Eco or step up to
//! Performance without the user having to think about it.
//!
//! Two properties matter more than cleverness here:
//!
//! - **It must not oscillate.** A mode switch spins fans up or down and is
//!   very visible, so a decision has to hold for several consecutive
//!   samples, and the load thresholds have a dead band between them.
//! - **It must not fight the user.** Setting a mode by hand pauses the
//!   supervisor for a while; whoever is at the keyboard wins.
//!
//! The decision itself is a pure function over sampled inputs, so the
//! behaviour is unit-tested rather than only observable by leaving a laptop
//! running for an hour.

use std::fs;

use serde::{Deserialize, Serialize};

use crate::PowerMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoConfig {
    pub enabled: bool,
    /// Drop to Eco whenever the machine is running on battery.
    pub eco_on_battery: bool,
    /// Step up to Performance under sustained load.
    pub performance_on_load: bool,
    /// Load average per core at or above which load counts as "high".
    pub load_high: f64,
    /// ...and at or below which it counts as "low" again. The gap between
    /// the two is the dead band that stops the mode flapping.
    pub load_low: f64,
    /// Consecutive agreeing samples required before actually switching.
    pub samples_to_switch: u32,
    pub interval_secs: u64,
    /// How long a manual mode change suspends the supervisor.
    pub manual_override_secs: u64,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            eco_on_battery: true,
            performance_on_load: true,
            // One busy core per core is "flat out"; 0.7 catches a game or a
            // build without reacting to background housekeeping.
            load_high: 0.70,
            load_low: 0.30,
            samples_to_switch: 3,
            interval_secs: 10,
            manual_override_secs: 600,
        }
    }
}

/// What the supervisor looks at on each tick.
#[derive(Debug, Clone, Copy)]
pub struct AutoInputs {
    /// `None` on machines without a battery - not the same as "on mains".
    pub on_battery: Option<bool>,
    /// 1-minute load average divided by CPU count.
    pub load_ratio: f64,
}

impl AutoInputs {
    pub fn sample(on_battery: Option<bool>) -> Self {
        Self { on_battery, load_ratio: load_ratio() }
    }
}

/// Load average per core.
///
/// The 1-minute average is used rather than instantaneous CPU usage
/// precisely because it is already smoothed: the supervisor is looking for
/// *sustained* load, and a 2-second spike should not move the fans.
fn load_ratio() -> f64 {
    let Ok(loadavg) = fs::read_to_string("/proc/loadavg") else {
        return 0.0;
    };
    let Some(one_minute) = loadavg.split_whitespace().next().and_then(|v| v.parse::<f64>().ok())
    else {
        return 0.0;
    };
    let cores = fs::read_to_string("/proc/cpuinfo")
        .map(|info| info.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(1)
        .max(1);
    one_minute / cores as f64
}

/// The mode the current conditions call for, or `None` when they call for
/// nothing in particular and whatever is set should be left alone.
///
/// Battery beats load: there is no point running Performance off a battery
/// the firmware will throttle anyway.
pub fn target_mode(inputs: AutoInputs, config: &AutoConfig) -> Option<PowerMode> {
    if config.eco_on_battery && inputs.on_battery == Some(true) {
        return Some(PowerMode::Eco);
    }
    if config.performance_on_load && inputs.load_ratio >= config.load_high {
        return Some(PowerMode::Performance);
    }
    if inputs.load_ratio <= config.load_low {
        return Some(PowerMode::Balanced);
    }
    // Inside the dead band: no opinion.
    None
}

/// Counts how long a candidate mode has been the answer, so a switch only
/// happens once the conditions have held.
#[derive(Debug, Default)]
pub struct AutoSwitcher {
    pending: Option<(PowerMode, u32)>,
}

impl AutoSwitcher {
    /// Feeds one sample in. Returns the mode to switch to, or `None`.
    pub fn observe(
        &mut self,
        inputs: AutoInputs,
        config: &AutoConfig,
        current: PowerMode,
    ) -> Option<PowerMode> {
        let Some(target) = target_mode(inputs, config) else {
            self.pending = None;
            return None;
        };
        if target == current {
            self.pending = None;
            return None;
        }

        let count = match self.pending {
            Some((pending, count)) if pending == target => count + 1,
            _ => 1,
        };

        if count >= config.samples_to_switch.max(1) {
            self.pending = None;
            return Some(target);
        }

        self.pending = Some((target, count));
        None
    }

    /// Forgets any in-progress decision - used when the user takes over.
    pub fn reset(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AutoConfig {
        AutoConfig { enabled: true, samples_to_switch: 3, ..AutoConfig::default() }
    }

    fn inputs(on_battery: Option<bool>, load_ratio: f64) -> AutoInputs {
        AutoInputs { on_battery, load_ratio }
    }

    #[test]
    fn battery_wins_over_load() {
        let heavy_but_unplugged = inputs(Some(true), 0.95);
        assert_eq!(target_mode(heavy_but_unplugged, &config()), Some(PowerMode::Eco));
    }

    #[test]
    fn sustained_load_on_mains_asks_for_performance() {
        assert_eq!(target_mode(inputs(Some(false), 0.8), &config()), Some(PowerMode::Performance));
    }

    #[test]
    fn an_idle_machine_asks_for_balanced() {
        assert_eq!(target_mode(inputs(Some(false), 0.05), &config()), Some(PowerMode::Balanced));
    }

    #[test]
    fn the_dead_band_expresses_no_opinion() {
        // Between load_low and load_high nothing should change, which is
        // what stops the mode flapping around a threshold.
        assert_eq!(target_mode(inputs(Some(false), 0.5), &config()), None);
    }

    #[test]
    fn a_machine_without_a_battery_is_never_treated_as_unplugged() {
        assert_eq!(target_mode(inputs(None, 0.05), &config()), Some(PowerMode::Balanced));
    }

    #[test]
    fn switching_needs_the_condition_to_hold() {
        let mut switcher = AutoSwitcher::default();
        let busy = inputs(Some(false), 0.9);

        assert_eq!(switcher.observe(busy, &config(), PowerMode::Balanced), None);
        assert_eq!(switcher.observe(busy, &config(), PowerMode::Balanced), None);
        assert_eq!(
            switcher.observe(busy, &config(), PowerMode::Balanced),
            Some(PowerMode::Performance)
        );
    }

    #[test]
    fn a_single_spike_does_not_switch_anything() {
        let mut switcher = AutoSwitcher::default();
        let config = config();

        switcher.observe(inputs(Some(false), 0.9), &config, PowerMode::Balanced);
        switcher.observe(inputs(Some(false), 0.9), &config, PowerMode::Balanced);
        // Load drops back before the third sample: the count restarts.
        switcher.observe(inputs(Some(false), 0.05), &config, PowerMode::Balanced);
        assert_eq!(switcher.observe(inputs(Some(false), 0.9), &config, PowerMode::Balanced), None);
    }

    #[test]
    fn no_switch_is_proposed_when_already_in_that_mode() {
        let mut switcher = AutoSwitcher::default();
        let config = config();
        for _ in 0..5 {
            assert_eq!(
                switcher.observe(inputs(Some(false), 0.9), &config, PowerMode::Performance),
                None
            );
        }
    }

    #[test]
    fn samples_to_switch_of_zero_still_requires_one_sample() {
        let mut switcher = AutoSwitcher::default();
        let config = AutoConfig { samples_to_switch: 0, ..config() };
        assert_eq!(
            switcher.observe(inputs(Some(true), 0.1), &config, PowerMode::Balanced),
            Some(PowerMode::Eco)
        );
    }
}
