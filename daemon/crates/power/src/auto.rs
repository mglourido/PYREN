//! Background supervisor that picks a power mode on its own.
//!
//! Two systems, matching the two switches on the app's home screen and the
//! behaviour of the app this clones:
//!
//! | | when it acts | what it does |
//! |---|---|---|
//! | **auto Eco** | the machine is unplugged | drops to Balanced at once, then to Eco if it stays idle or the battery gets low |
//! | **auto Performance** | the machine is plugged in | steps up to Performance at once, then back to Balanced if it sits idle |
//!
//! So a *change of power source* is a discrete event with an immediate
//! answer, and everything after it is a slow refinement inside the range
//! that source allows:
//!
//! ```text
//!   on battery:   Eco  <--->  Balanced
//!   on mains:          Balanced  <--->  Performance
//! ```
//!
//! **Unlimited is never chosen automatically.** It is the one mode that
//! removes this daemon's own limits, so it is the one mode the user has to
//! ask for. The supervisor will move *out* of it when the power source
//! changes - unplugging is a deliberate physical act and a laptop running
//! unlimited off a battery is not what anyone meant - but it will never
//! refine its way into it.
//!
//! Three properties matter more than cleverness here:
//!
//! - **It must not oscillate.** A mode switch spins fans up or down and is
//!   very visible, so a refinement has to hold for several consecutive
//!   samples, and the load thresholds have a dead band between them.
//! - **It must not fight the user.** Setting a mode by hand pauses the
//!   refinement for a while; whoever is at the keyboard wins. Plugging or
//!   unplugging the machine is also the user speaking, though, so *that*
//!   answer is not suppressed.
//! - **The decision is a pure function** over sampled inputs, so the
//!   behaviour is unit-tested rather than only observable by leaving a
//!   laptop running for an hour.

use std::fs;

use pyren_core::{msg, Msg};
use serde::{Deserialize, Serialize};

use crate::PowerMode;

/// `serde(default)` so a client that predates a field still parses: the
/// app and the daemon are separate binaries and are not always updated
/// together.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AutoConfig {
    pub enabled: bool,
    /// The "switch to Eco automatically" system: unplugging drops to
    /// Balanced, and a machine that stays idle - or whose battery gets low
    /// - goes on to Eco.
    pub eco_on_battery: bool,
    /// The "switch to Performance automatically" system: plugging in steps
    /// up to Performance, and a machine that sits idle on mains comes back
    /// down to Balanced.
    pub performance_on_load: bool,
    /// Load average per core at or above which load counts as "high".
    pub load_high: f64,
    /// ...and at or below which it counts as "low" again. The gap between
    /// the two is the dead band that stops the mode flapping.
    pub load_low: f64,
    /// Battery percentage at or below which Eco is preferred whatever the
    /// load is doing. A nearly flat battery is its own argument.
    pub battery_low_percent: f64,
    /// Whether a hot machine is a reason to step down. On by default: the
    /// case it exists for is a laptop on a duvet, and the answer to that
    /// is not "keep asking for Performance".
    pub back_off_when_hot: bool,
    /// Temperature at or above which the machine counts as hot...
    pub temp_high_c: f64,
    /// ...and the one it has to come back below before it stops counting.
    /// A second dead band, and a wider one than the load's on purpose: a
    /// chassis that has just been throttled is still full of heat, and a
    /// single threshold would step back up into the same wall.
    pub temp_low_c: f64,
    /// Consecutive agreeing samples required before a refinement happens.
    /// Does not apply to a change of power source, which is immediate.
    pub samples_to_switch: u32,
    pub interval_secs: u64,
    /// How long a manual mode change suspends refinement.
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
            battery_low_percent: 25.0,
            back_off_when_hot: true,
            // Not a shutdown temperature and not meant to be: the CPU's own
            // throttle is at 100 C, and the point of backing off at 85 is
            // to arrive there less often. Below 75 the machine has actually
            // cooled rather than paused.
            temp_high_c: 85.0,
            temp_low_c: 75.0,
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
    pub battery_percent: Option<f64>,
    /// The hottest of the CPU and GPU, or `None` on a machine that
    /// publishes neither - which is not the same as a cold one, and is why
    /// this is an option rather than a zero.
    pub temp_c: Option<f64>,
}

impl AutoInputs {
    pub fn sample(
        on_battery: Option<bool>,
        battery_percent: Option<f64>,
        sensors: &Sensors,
    ) -> Self {
        Self {
            on_battery,
            load_ratio: load_ratio(),
            battery_percent,
            temp_c: sensors.hottest_c(),
        }
    }
}

/// The temperature sensors this machine turned out to have.
///
/// Found once and held, rather than searched for on every tick: the
/// supervisor samples on a timer for as long as the daemon runs, and
/// walking `/sys/class/hwmon` ten times a minute to re-derive an answer
/// that does not change is a waste. The cost is that a card which appears
/// later - an eGPU, or a driver loaded after boot - is not picked up until
/// the daemon restarts, which is the same trade the fan module makes.
#[derive(Debug, Clone, Default)]
pub struct Sensors {
    cpu: Option<std::path::PathBuf>,
    gpu: Option<std::path::PathBuf>,
}

impl Sensors {
    pub fn discover() -> Self {
        Self { cpu: pyren_core::sensors::cpu_temp_path(), gpu: pyren_core::sensors::gpu_temp_path() }
    }

    /// True when this machine can answer the question at all. A supervisor
    /// on a machine with no sensors must not behave as though it were
    /// permanently cool.
    pub fn any(&self) -> bool {
        self.cpu.is_some() || self.gpu.is_some()
    }

    /// The reading right now, for a status call rather than for the
    /// supervisor's own tick.
    pub fn hottest_c_now(&self) -> Option<f64> {
        self.hottest_c()
    }

    fn hottest_c(&self) -> Option<f64> {
        pyren_core::sensors::hottest_c(self.cpu.as_deref(), self.gpu.as_deref())
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

/// One thing the supervisor decided to do, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoDecision {
    pub mode: PowerMode,
    /// Shown in the UI and the log, so an unexplained mode change never
    /// looks like the daemon acting on its own. Translatable.
    pub reason: Msg,
    /// True for an answer to the power source changing. Those are immediate
    /// and are not suppressed by a manual override, because plugging the
    /// machine in is the user speaking too.
    pub from_transition: bool,
}

/// The two modes a given power source may move between.
///
/// Neither range contains Unlimited, which is the point: refinement can
/// never arrive there.
fn range(on_battery: bool) -> (PowerMode, PowerMode) {
    if on_battery {
        (PowerMode::Eco, PowerMode::Balanced)
    } else {
        (PowerMode::Balanced, PowerMode::Performance)
    }
}

/// Whether the system responsible for this power source is switched on.
fn system_enabled(on_battery: bool, config: &AutoConfig) -> bool {
    if on_battery {
        config.eco_on_battery
    } else {
        config.performance_on_load
    }
}

/// Whether the machine counts as hot, with the dead band latched.
///
/// A latch rather than a comparison because the two thresholds are the
/// whole design: crossing 85 C makes it hot, and only coming back below
/// 75 C makes it cool again. Comparing against one number would step down,
/// see the temperature fall by a degree because the fans caught up, step
/// back up into the same wall, and do it again for as long as the load
/// lasted.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeatLatch {
    hot: bool,
}

impl HeatLatch {
    /// Feeds one reading in and returns what it now means. A machine with
    /// no sensor never becomes hot, and - this is the half worth saying
    /// out loud - never stops being hot either if it somehow got there:
    /// losing a sensor mid-run is not evidence of cooling.
    pub fn observe(&mut self, temp_c: Option<f64>, config: &AutoConfig) -> bool {
        if let Some(temp_c) = temp_c {
            if temp_c >= config.temp_high_c {
                self.hot = true;
            } else if temp_c <= config.temp_low_c {
                self.hot = false;
            }
        }
        self.hot
    }

    pub fn is_hot(self) -> bool {
        self.hot
    }
}

/// Where inside the source's range the current conditions point, or `None`
/// while they are inside the dead band and nothing should move.
///
/// `hot` comes from a [`HeatLatch`] rather than from `inputs` because it
/// is the one input with memory; everything else here is a comparison
/// against the moment.
pub fn refine(
    inputs: AutoInputs,
    config: &AutoConfig,
    on_battery: bool,
    hot: bool,
) -> Option<PowerMode> {
    let (quiet, fast) = range(on_battery);

    // Heat outranks load, and it has to: a machine is hot *because* it is
    // busy, so the two arguments arrive together and the load one would
    // otherwise win every time.
    if hot && config.back_off_when_hot {
        return Some(quiet);
    }

    // A battery this low is its own argument, whatever the CPU is doing.
    if on_battery {
        if let Some(percent) = inputs.battery_percent {
            if percent <= config.battery_low_percent {
                return Some(quiet);
            }
        }
    }

    if inputs.load_ratio >= config.load_high {
        return Some(fast);
    }
    if inputs.load_ratio <= config.load_low {
        return Some(quiet);
    }
    None
}

/// Tracks the power source and how long a refinement has been the answer,
/// so a switch only happens once conditions have held.
#[derive(Debug, Default)]
pub struct AutoSwitcher {
    /// `None` until the first sample: the first tick after startup must not
    /// look like the user just plugged the machine in.
    last_on_battery: Option<bool>,
    pending: Option<(PowerMode, u32)>,
    heat: HeatLatch,
}

impl AutoSwitcher {
    /// Feeds one sample in. Returns what to switch to, or `None`.
    pub fn observe(
        &mut self,
        inputs: AutoInputs,
        config: &AutoConfig,
        current: PowerMode,
    ) -> Option<AutoDecision> {
        // Updated on every tick, including the ones that return early:
        // the latch is about the machine, not about which branch this
        // sample took, and a transition must not leave it stale.
        self.heat.observe(inputs.temp_c, config);

        let Some(on_battery) = inputs.on_battery else {
            // No battery at all: nothing to transition between, and the
            // mains system is the only one that could apply.
            return self.refinement(inputs, config, false, current);
        };

        let previous = self.last_on_battery.replace(on_battery);
        let source_changed = previous.is_some_and(|was| was != on_battery);

        if source_changed && system_enabled(on_battery, config) {
            self.pending = None;
            let (_, fast) = range(on_battery);
            // Unplugging lands on the *quiet* end of the mains range, which
            // is Balanced; plugging in lands on the fast end, Performance.
            let mode = if on_battery { PowerMode::Balanced } else { fast };
            if mode != current {
                return Some(AutoDecision {
                    mode,
                    reason: if on_battery {
                        msg!("power.autoReason.toBattery", "switched to battery")
                    } else {
                        msg!("power.autoReason.pluggedIn", "plugged in")
                    },
                    from_transition: true,
                });
            }
            return None;
        }

        self.refinement(inputs, config, on_battery, current)
    }

    fn refinement(
        &mut self,
        inputs: AutoInputs,
        config: &AutoConfig,
        on_battery: bool,
        current: PowerMode,
    ) -> Option<AutoDecision> {
        // Unlimited is the user's own choice; refinement leaves it alone.
        if current == PowerMode::Unlimited || !system_enabled(on_battery, config) {
            self.pending = None;
            return None;
        }

        let Some(target) = refine(inputs, config, on_battery, self.heat.is_hot()) else {
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
            let reason = if self.heat.is_hot() && config.back_off_when_hot {
                msg!(
                    "power.autoReason.hot",
                    { "temp" => format!("{:.0}", inputs.temp_c.unwrap_or_default()) },
                    "running hot ({temp} C)"
                )
            } else if inputs.load_ratio >= config.load_high {
                msg!(
                    "power.autoReason.sustainedLoad",
                    { "percent" => format!("{:.0}", inputs.load_ratio * 100.0) },
                    "sustained load ({percent}% per core)"
                )
            } else if on_battery
                && inputs
                    .battery_percent
                    .is_some_and(|p| p <= config.battery_low_percent)
            {
                msg!(
                    "power.autoReason.batteryLow",
                    { "percent" => format!("{:.0}", inputs.battery_percent.unwrap_or_default()) },
                    "battery at {percent}%"
                )
            } else {
                msg!("power.autoReason.idle", "idle")
            };
            return Some(AutoDecision { mode: target, reason, from_transition: false });
        }

        self.pending = Some((target, count));
        None
    }

    /// Forgets any in-progress refinement - used when the user takes over.
    ///
    /// The heat latch is deliberately *not* reset: how hot the machine is
    /// is not an opinion the user overrode, and clearing it would let the
    /// next tick treat a chassis at 90 C as freshly cool.
    pub fn reset(&mut self) {
        self.pending = None;
    }

    pub fn is_hot(&self) -> bool {
        self.heat.is_hot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AutoConfig {
        AutoConfig { enabled: true, samples_to_switch: 3, ..AutoConfig::default() }
    }

    /// A comfortable machine: 60 C is well below `temp_low_c`, so the
    /// thermal rule is inert in every test that does not opt into it.
    fn inputs(on_battery: Option<bool>, load_ratio: f64) -> AutoInputs {
        AutoInputs { on_battery, load_ratio, battery_percent: Some(80.0), temp_c: Some(60.0) }
    }

    fn at(temp_c: f64, on_battery: bool, load_ratio: f64) -> AutoInputs {
        AutoInputs { temp_c: Some(temp_c), ..inputs(Some(on_battery), load_ratio) }
    }

    /// Settles the switcher's idea of the power source without producing a
    /// transition, the way the first tick after startup does.
    fn settled(on_battery: bool) -> AutoSwitcher {
        let mut switcher = AutoSwitcher::default();
        switcher.observe(inputs(Some(on_battery), 0.5), &config(), PowerMode::Balanced);
        switcher
    }

    #[test]
    fn unplugging_drops_to_balanced_immediately() {
        let mut switcher = settled(false);
        let decision = switcher
            .observe(inputs(Some(true), 0.5), &config(), PowerMode::Performance)
            .expect("a source change is answered at once");

        assert_eq!(decision.mode, PowerMode::Balanced);
        assert!(decision.from_transition);
    }

    #[test]
    fn plugging_in_steps_up_to_performance_immediately() {
        let mut switcher = settled(true);
        let decision = switcher
            .observe(inputs(Some(false), 0.1), &config(), PowerMode::Eco)
            .expect("a source change is answered at once");

        assert_eq!(decision.mode, PowerMode::Performance);
        assert!(decision.from_transition);
    }

    /// The first sample must not look like the user just plugged in.
    #[test]
    fn the_first_sample_after_startup_is_not_a_transition() {
        let mut switcher = AutoSwitcher::default();
        let decision = switcher.observe(inputs(Some(false), 0.5), &config(), PowerMode::Balanced);
        assert_eq!(decision, None);
    }

    #[test]
    fn an_idle_machine_on_battery_refines_from_balanced_down_to_eco() {
        let mut switcher = settled(true);
        let idle = inputs(Some(true), 0.05);

        assert_eq!(switcher.observe(idle, &config(), PowerMode::Balanced), None);
        assert_eq!(switcher.observe(idle, &config(), PowerMode::Balanced), None);
        let decision = switcher.observe(idle, &config(), PowerMode::Balanced).unwrap();

        assert_eq!(decision.mode, PowerMode::Eco);
        assert!(!decision.from_transition, "a refinement, not an event");
    }

    #[test]
    fn an_idle_machine_on_mains_comes_back_down_to_balanced() {
        let mut switcher = settled(false);
        let idle = inputs(Some(false), 0.05);
        for _ in 0..2 {
            switcher.observe(idle, &config(), PowerMode::Performance);
        }
        let decision = switcher.observe(idle, &config(), PowerMode::Performance).unwrap();

        assert_eq!(decision.mode, PowerMode::Balanced);
    }

    /// The ranges do not overlap at the top on battery: no amount of load
    /// justifies Performance off a battery.
    #[test]
    fn load_on_battery_never_reaches_performance() {
        let mut switcher = settled(true);
        let busy = inputs(Some(true), 0.99);
        for _ in 0..10 {
            if let Some(decision) = switcher.observe(busy, &config(), PowerMode::Eco) {
                assert_eq!(decision.mode, PowerMode::Balanced);
                return;
            }
        }
        panic!("a busy machine on battery should still climb to Balanced");
    }

    #[test]
    fn a_low_battery_asks_for_eco_however_busy_the_machine_is() {
        let flat = AutoInputs { battery_percent: Some(12.0), ..inputs(Some(true), 0.99) };
        assert_eq!(refine(flat, &config(), true, false), Some(PowerMode::Eco));
    }

    /// The rule this exists for: a machine that is busy *and* hot gets the
    /// quiet end, not the fast one. Heat and load always arrive together -
    /// the machine is hot because it is working - so if load won here the
    /// thermal rule would never fire at all.
    #[test]
    fn heat_outranks_load() {
        let busy_and_hot = at(90.0, false, 0.99);
        assert_eq!(refine(busy_and_hot, &config(), false, true), Some(PowerMode::Balanced));
        assert_eq!(
            refine(busy_and_hot, &config(), false, false),
            Some(PowerMode::Performance),
            "the same sample with the latch cool is an ordinary busy machine"
        );
    }

    /// Two thresholds, not one. A single 85 C line would step down, watch
    /// the fans win back one degree, step up into the same wall, and do it
    /// again for as long as the load lasted.
    #[test]
    fn the_heat_latch_holds_until_the_machine_has_actually_cooled() {
        let mut latch = HeatLatch::default();
        assert!(!latch.observe(Some(84.0), &config()));
        assert!(latch.observe(Some(85.0), &config()), "at the threshold, not past it");
        assert!(latch.observe(Some(80.0), &config()), "still hot inside the dead band");
        assert!(latch.observe(Some(76.0), &config()), "and at the bottom of it");
        assert!(!latch.observe(Some(75.0), &config()), "cool again only below temp_low_c");
    }

    /// A machine with no sensor is not a cold machine - but it is not a hot
    /// one either, and the rule simply never fires there.
    #[test]
    fn a_machine_with_no_sensor_never_becomes_hot() {
        let mut latch = HeatLatch::default();
        for _ in 0..5 {
            assert!(!latch.observe(None, &config()));
        }
    }

    /// ...and losing the sensor mid-run is not evidence of cooling. The
    /// last thing this machine said was 90 C; a driver unloading does not
    /// change that.
    #[test]
    fn losing_the_sensor_does_not_cool_a_hot_machine() {
        let mut latch = HeatLatch::default();
        assert!(latch.observe(Some(90.0), &config()));
        assert!(latch.observe(None, &config()), "no reading is no news");
    }

    /// The whole rule is switchable, and off it changes nothing at all.
    #[test]
    fn a_user_who_turned_the_thermal_rule_off_keeps_their_performance_mode() {
        let cool_headed = AutoConfig { back_off_when_hot: false, ..config() };
        assert_eq!(
            refine(at(95.0, false, 0.99), &cool_headed, false, true),
            Some(PowerMode::Performance)
        );
    }

    /// End to end through the switcher, which is where the latch actually
    /// lives: three agreeing samples and the reason names the temperature.
    #[test]
    fn a_hot_machine_steps_down_and_says_why() {
        let mut switcher = settled(false);
        let hot = at(92.0, false, 0.99);

        assert_eq!(switcher.observe(hot, &config(), PowerMode::Performance), None);
        assert_eq!(switcher.observe(hot, &config(), PowerMode::Performance), None);
        let decision = switcher.observe(hot, &config(), PowerMode::Performance).unwrap();

        assert_eq!(decision.mode, PowerMode::Balanced);
        assert!(!decision.from_transition);
        assert_eq!(decision.reason.key, "power.autoReason.hot");
        assert!(decision.reason.to_string().contains("92"), "{}", decision.reason);
    }

    /// The latch is updated on every tick, including the ones that return
    /// early. A machine that got hot while unplugged must not come back
    /// from the transition believing it is cool.
    #[test]
    fn the_latch_is_updated_even_on_a_tick_that_answers_a_transition() {
        let mut switcher = settled(false);
        let decision = switcher.observe(at(92.0, true, 0.99), &config(), PowerMode::Performance);
        assert!(decision.is_some_and(|d| d.from_transition));
        assert!(switcher.is_hot(), "the transition tick still read the sensor");
    }

    /// Taking over by hand is an opinion about the mode, not about the
    /// temperature.
    #[test]
    fn a_manual_override_does_not_clear_the_heat_latch() {
        let mut switcher = settled(false);
        switcher.observe(at(92.0, false, 0.99), &config(), PowerMode::Performance);
        switcher.reset();
        assert!(switcher.is_hot());
    }

    /// Nothing the supervisor does may arrive at Unlimited.
    #[test]
    fn refinement_never_selects_unlimited() {
        for on_battery in [true, false] {
            for load in [0.0, 0.5, 1.0, 4.0] {
                for hot in [false, true] {
                    let target =
                        refine(inputs(Some(on_battery), load), &config(), on_battery, hot);
                    assert_ne!(target, Some(PowerMode::Unlimited));
                }
                let target = refine(inputs(Some(on_battery), load), &config(), on_battery, false);
                assert_ne!(target, Some(PowerMode::Unlimited));
            }
        }
    }

    /// ...and a user who chose Unlimited keeps it while nothing physical
    /// changes.
    #[test]
    fn a_machine_left_in_unlimited_is_not_refined_out_of_it() {
        let mut switcher = settled(false);
        let idle = inputs(Some(false), 0.0);
        for _ in 0..10 {
            assert_eq!(switcher.observe(idle, &config(), PowerMode::Unlimited), None);
        }
    }

    /// ...but unplugging still moves it, because that is a deliberate act
    /// and a laptop running unlimited off a battery is not what was meant.
    #[test]
    fn unplugging_does_move_a_machine_out_of_unlimited() {
        let mut switcher = settled(false);
        let decision =
            switcher.observe(inputs(Some(true), 0.5), &config(), PowerMode::Unlimited).unwrap();

        assert_eq!(decision.mode, PowerMode::Balanced);
    }

    #[test]
    fn the_dead_band_between_the_thresholds_produces_no_opinion() {
        let middling = inputs(Some(false), 0.5);
        assert_eq!(refine(middling, &config(), false, false), None);
    }

    #[test]
    fn a_disabled_system_does_nothing_for_its_own_power_source() {
        let off = AutoConfig { eco_on_battery: false, ..config() };
        let mut switcher = settled(false);

        // Unplugging with the Eco system off is not answered...
        assert_eq!(switcher.observe(inputs(Some(true), 0.5), &off, PowerMode::Performance), None);
        // ...and neither is idling on battery.
        let idle = inputs(Some(true), 0.0);
        for _ in 0..5 {
            assert_eq!(switcher.observe(idle, &off, PowerMode::Balanced), None);
        }
    }

    #[test]
    fn a_desktop_with_no_battery_is_treated_as_being_on_mains() {
        let mut switcher = AutoSwitcher::default();
        let busy = AutoInputs {
            on_battery: None,
            load_ratio: 0.9,
            battery_percent: None,
            temp_c: Some(60.0),
        };
        for _ in 0..2 {
            switcher.observe(busy, &config(), PowerMode::Balanced);
        }
        let decision = switcher.observe(busy, &config(), PowerMode::Balanced).unwrap();

        assert_eq!(decision.mode, PowerMode::Performance);
    }

    #[test]
    fn a_refinement_that_is_already_in_force_is_not_re_applied() {
        let mut switcher = settled(true);
        let idle = inputs(Some(true), 0.0);
        for _ in 0..5 {
            assert_eq!(switcher.observe(idle, &config(), PowerMode::Eco), None);
        }
    }
}
