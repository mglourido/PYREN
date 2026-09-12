//! Background supervisor that picks a power mode on its own.
//!
//! Two systems, matching the two switches on the app's home screen and the
//! behaviour of the app this clones:
//!
//! | | when it acts | what it does |
//! |---|---|---|
//! | **auto Eco** | the machine is unplugged | goes to the preferred battery mode at once, then moves between Eco and Balanced |
//! | **auto Performance** | the machine is plugged in | goes to the preferred mains mode at once, then moves between Balanced and Performance |
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
//! **Which end is home is the user's call, not a guess.** Each source has a
//! *preferred* mode (`preferred_on_battery`, `preferred_on_mains`): it is
//! where the machine lands when the cable moves and where it returns once
//! whatever moved it has passed. Leaving it takes a strong argument - a
//! sustained load to go up, an idle machine, heat or a low battery to go
//! down - and coming back only takes that argument going away. That
//! asymmetry is the whole difference between preferring Eco and preferring
//! Balanced: the same machine, doing the same moderate work, stays in
//! whichever one the user asked for.
//!
//! **A mode picked by hand becomes the preferred one** until the power
//! source next changes. The manual override only pauses the supervisor for
//! a few minutes; without this, the moment it ran out the supervisor would
//! go back to second-guessing a choice the user had just made. Refining
//! around a hand-picked mode only ever steps *down* from it - for heat, an
//! idle machine or a low battery - and back up to it when that passes: a
//! user who chose Eco for a quiet meeting did not ask to be overruled by
//! a compile. It is also the one way Performance can be in force on
//! battery, since the supervisor never picks it there itself.
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
//!   refinement for a while, and after that the hand-picked mode is what
//!   refinement works around; whoever is at the keyboard wins. Plugging or
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
    /// The "switch to Eco automatically" system: unplugging goes to
    /// `preferred_on_battery`, and refinement then moves between Eco and
    /// Balanced.
    pub eco_on_battery: bool,
    /// The "switch to Performance automatically" system: plugging in goes
    /// to `preferred_on_mains`, and refinement then moves between Balanced
    /// and Performance.
    pub performance_on_load: bool,
    /// Home on battery: Eco or Balanced. Anything else is clamped into that
    /// range - the supervisor never picks Performance off a battery.
    pub preferred_on_battery: PowerMode,
    /// Home on mains: Balanced or Performance. Eco has no business being
    /// chosen for a plugged-in machine, and Unlimited is never chosen.
    pub preferred_on_mains: PowerMode,
    /// Load average per core at or above which load counts as "high".
    pub load_high: f64,
    /// ...and at or below which it counts as "low" again. The gap between
    /// the two is the dead band that stops the mode flapping; its midpoint
    /// is where a machine that left its preferred mode heads back to it.
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
            // Frugal off the battery, generous on the wall: each side
            // starts from the obvious reading of its power source and
            // earns its way to the other end.
            preferred_on_battery: PowerMode::Eco,
            preferred_on_mains: PowerMode::Performance,
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
        Self {
            cpu: pyren_core::sensors::cpu_temp_path(),
            gpu: pyren_core::sensors::gpu_temp_path(),
        }
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
    let Some(one_minute) = loadavg
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
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

/// Position in [`PowerMode::ALL`], least to most. The enum has no `Ord` of
/// its own because nothing else should be comparing modes; this is the
/// one place "one step down" has to mean something.
fn rank(mode: PowerMode) -> usize {
    PowerMode::ALL.iter().position(|m| *m == mode).unwrap_or(0)
}

/// One step down from `mode`, but not past `floor`. A mode already at or
/// under the floor stays where it is: stepping *down* must never mean
/// climbing back up into the range.
fn step_down(mode: PowerMode, floor: PowerMode) -> PowerMode {
    if rank(mode) <= rank(floor) {
        mode
    } else {
        PowerMode::ALL[rank(mode) - 1]
    }
}

/// One step up from `mode`, but not past `ceiling` - the mirror image.
fn step_up(mode: PowerMode, ceiling: PowerMode) -> PowerMode {
    if rank(mode) >= rank(ceiling) {
        mode
    } else {
        PowerMode::ALL[rank(mode) + 1]
    }
}

fn lower_of(a: PowerMode, b: PowerMode) -> PowerMode {
    if rank(a) <= rank(b) {
        a
    } else {
        b
    }
}

impl AutoConfig {
    /// Why this config cannot be used, or `None` when it can.
    ///
    /// Each check is a way the supervisor would otherwise break silently:
    /// crossed load thresholds leave it with no dead band and it flaps
    /// every few samples; crossed temperatures latch "hot" and never let
    /// go; a battery threshold past 100 would hold every unplugged machine
    /// in Eco for good.
    pub fn problem(&self) -> Option<Msg> {
        let finite = [
            self.load_low,
            self.load_high,
            self.battery_low_percent,
            self.temp_low_c,
            self.temp_high_c,
        ]
        .iter()
        .all(|v| v.is_finite());
        if !finite || self.load_low < 0.0 || self.load_low >= self.load_high {
            return Some(msg!(
                "power.err.loadBand",
                { "low" => format!("{:.2}", self.load_low), "high" => format!("{:.2}", self.load_high) },
                "loadLow ({low}) has to be at least 0 and below loadHigh ({high})"
            ));
        }
        if self.temp_low_c >= self.temp_high_c {
            return Some(msg!(
                "power.err.tempBand",
                { "low" => format!("{:.0}", self.temp_low_c), "high" => format!("{:.0}", self.temp_high_c) },
                "tempLowC ({low}) has to be below tempHighC ({high})"
            ));
        }
        if !(0.0..=100.0).contains(&self.battery_low_percent) {
            return Some(msg!(
                "power.err.batteryPercent",
                { "percent" => format!("{:.0}", self.battery_low_percent) },
                "batteryLowPercent has to be between 0 and 100, not {percent}"
            ));
        }
        None
    }

    /// The configured home for a power source, clamped into its range. A
    /// hand-edited config asking for Performance on battery gets Balanced,
    /// not a supervisor that picks Performance off a battery.
    pub fn preferred(&self, on_battery: bool) -> PowerMode {
        let (floor, ceiling) = range(on_battery);
        let wanted = if on_battery {
            self.preferred_on_battery
        } else {
            self.preferred_on_mains
        };
        if rank(wanted) < rank(floor) {
            floor
        } else if rank(wanted) > rank(ceiling) {
            ceiling
        } else {
            wanted
        }
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

/// The mode refinement works around right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Baseline {
    pub mode: PowerMode,
    /// True when the user picked `mode` by hand. A hand-picked mode is only
    /// ever stepped *down* from, never up.
    pub manual: bool,
}

/// Why refinement wants to move - kept as a value rather than rebuilt from
/// the inputs afterwards, so the sentence the user reads is always the rule
/// that actually fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Hot,
    BatteryLow,
    SustainedLoad,
    Idle,
    /// Whatever moved the machine off its preferred mode has passed.
    BackToPreferred,
    /// The machine is in a mode the supervisor would never pick for this
    /// power source - Performance on battery at startup, say.
    OutOfRange,
}

impl Why {
    fn to_msg(self, inputs: AutoInputs) -> Msg {
        match self {
            Why::Hot => msg!(
                "power.autoReason.hot",
                { "temp" => format!("{:.0}", inputs.temp_c.unwrap_or_default()) },
                "running hot ({temp} C)"
            ),
            Why::BatteryLow => msg!(
                "power.autoReason.batteryLow",
                { "percent" => format!("{:.0}", inputs.battery_percent.unwrap_or_default()) },
                "battery at {percent}%"
            ),
            Why::SustainedLoad => msg!(
                "power.autoReason.sustainedLoad",
                { "percent" => format!("{:.0}", inputs.load_ratio * 100.0) },
                "sustained load ({percent}% per core)"
            ),
            Why::Idle => msg!("power.autoReason.idle", "idle"),
            Why::BackToPreferred => {
                msg!(
                    "power.autoReason.backToPreferred",
                    "back to the preferred mode"
                )
            }
            Why::OutOfRange => msg!(
                "power.autoReason.outOfRange",
                "not a mode chosen automatically on this power source"
            ),
        }
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

/// Where the current conditions point, and why, or `None` while nothing
/// should move.
///
/// In order, first match wins:
///
/// 1. **Heat** - one step below the baseline (never a step up, whatever
///    the baseline is).
/// 2. **A low battery** - Eco, whatever else is going on.
/// 3. **Out of range** - a mode the supervisor would never pick for this
///    source, and nobody picked by hand, is brought to the nearest end.
/// 4. **Sustained load** - one step above the baseline, within the range,
///    and never above a hand-picked mode.
/// 5. **Idle** - one step below the baseline, within the range.
/// 6. **Neither** - the dead band. A machine that is off its baseline goes
///    back once load has crossed the band's midpoint *towards* it; one
///    that is on it stays put.
///
/// Rule 6 is what makes the preferred mode sticky: leaving it takes the
/// far threshold, coming back only the middle. With the default band that
/// is 0.70 to leave Eco and 0.50 to come back, or 0.30 to leave
/// Performance and 0.50 to come back - two narrower dead bands, each still
/// wide enough that the machine does not flap across it.
///
/// `hot` comes from a [`HeatLatch`] rather than from `inputs` because it
/// is the one input with memory; everything else here is a comparison
/// against the moment.
pub fn refine(
    inputs: AutoInputs,
    config: &AutoConfig,
    on_battery: bool,
    hot: bool,
    baseline: Baseline,
    current: PowerMode,
) -> Option<(PowerMode, Why)> {
    let (floor, ceiling) = range(on_battery);
    let below = step_down(baseline.mode, floor);
    let above = if baseline.manual {
        baseline.mode
    } else {
        step_up(baseline.mode, ceiling)
    };

    // Heat outranks load, and it has to: a machine is hot *because* it is
    // busy, so the two arguments arrive together and the load one would
    // otherwise win every time.
    if hot && config.back_off_when_hot {
        return Some((lower_of(below, current), Why::Hot));
    }

    // A battery this low is its own argument, whatever the CPU is doing.
    if on_battery
        && inputs
            .battery_percent
            .is_some_and(|percent| percent <= config.battery_low_percent)
    {
        return Some((lower_of(floor, current), Why::BatteryLow));
    }

    // Only a mode the user did not choose is "out of range": a hand-picked
    // Performance on battery is exactly what rule 4 is careful around.
    if !baseline.manual && (rank(current) < rank(floor) || rank(current) > rank(ceiling)) {
        let nearest = if rank(current) < rank(floor) {
            floor
        } else {
            ceiling
        };
        return Some((nearest, Why::OutOfRange));
    }

    let load = inputs.load_ratio;
    if load >= config.load_high {
        return Some((above, Why::SustainedLoad));
    }
    if load <= config.load_low {
        return Some((below, Why::Idle));
    }

    let middle = (config.load_high + config.load_low) / 2.0;
    let (here, home) = (rank(current), rank(baseline.mode));
    if (here > home && load <= middle) || (here < home && load >= middle) {
        return Some((baseline.mode, Why::BackToPreferred));
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
    /// The mode the user last picked by hand, which refinement works around
    /// instead of the configured preference until the power source next
    /// changes.
    manual: Option<PowerMode>,
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

        if source_changed {
            // A choice made for the old power source is not one for the new
            // one - and this holds even when the new source's system is
            // off, or the old choice would outlive the cable it was about.
            self.manual = None;
            self.pending = None;
        }

        if source_changed && system_enabled(on_battery, config) {
            let mode = config.preferred(on_battery);
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

        let baseline = self.baseline(config, on_battery);
        let Some((target, why)) = refine(
            inputs,
            config,
            on_battery,
            self.heat.is_hot(),
            baseline,
            current,
        ) else {
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
            return Some(AutoDecision {
                mode: target,
                reason: why.to_msg(inputs),
                from_transition: false,
            });
        }

        self.pending = Some((target, count));
        None
    }

    /// What refinement works around for this power source: the mode the
    /// user picked by hand, or the configured preference.
    ///
    /// A hand-picked mode that *is* the preference is not treated as
    /// manual. Clicking the mode the supervisor would have chosen anyway is
    /// agreeing with it, and it must not quietly switch off the step up
    /// that the preference allows.
    pub fn baseline(&self, config: &AutoConfig, on_battery: bool) -> Baseline {
        let preferred = config.preferred(on_battery);
        match self.manual {
            Some(mode) if mode != preferred => Baseline { mode, manual: true },
            _ => Baseline {
                mode: preferred,
                manual: false,
            },
        }
    }

    /// Records a mode the user picked by hand, as the new baseline for as
    /// long as the machine stays on this power source.
    pub fn adopt(&mut self, mode: PowerMode) {
        self.manual = Some(mode);
        self.pending = None;
    }

    /// The hand-picked mode refinement is working around, if there is one
    /// that differs from the preference - for the UI to say so.
    pub fn manual_baseline(&self, config: &AutoConfig, on_battery: bool) -> Option<PowerMode> {
        let baseline = self.baseline(config, on_battery);
        baseline.manual.then_some(baseline.mode)
    }

    /// The power source as of the last sample, or `None` before the first
    /// one and on a machine with no battery.
    pub fn on_battery(&self) -> Option<bool> {
        self.last_on_battery
    }

    /// Forgets any in-progress refinement - used when the user takes over.
    ///
    /// The heat latch is deliberately *not* reset: how hot the machine is
    /// is not an opinion the user overrode, and clearing it would let the
    /// next tick treat a chassis at 90 C as freshly cool. Nor is the
    /// hand-picked baseline: that is the one thing the user *did* say.
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
        AutoConfig {
            enabled: true,
            samples_to_switch: 3,
            ..AutoConfig::default()
        }
    }

    /// A comfortable machine: 60 C is well below `temp_low_c`, so the
    /// thermal rule is inert in every test that does not opt into it.
    fn inputs(on_battery: Option<bool>, load_ratio: f64) -> AutoInputs {
        AutoInputs {
            on_battery,
            load_ratio,
            battery_percent: Some(80.0),
            temp_c: Some(60.0),
        }
    }

    fn at(temp_c: f64, on_battery: bool, load_ratio: f64) -> AutoInputs {
        AutoInputs {
            temp_c: Some(temp_c),
            ..inputs(Some(on_battery), load_ratio)
        }
    }

    /// What `refine` points at with the configured preference as the
    /// baseline, without the reason.
    fn target(
        inputs: AutoInputs,
        config: &AutoConfig,
        on_battery: bool,
        hot: bool,
        current: PowerMode,
    ) -> Option<PowerMode> {
        let baseline = Baseline {
            mode: config.preferred(on_battery),
            manual: false,
        };
        refine(inputs, config, on_battery, hot, baseline, current).map(|(mode, _)| mode)
    }

    /// Feeds the same sample until the switcher acts, and returns what it
    /// did - or `None` if `ticks` samples were not enough.
    fn run(
        switcher: &mut AutoSwitcher,
        sample: AutoInputs,
        config: &AutoConfig,
        current: PowerMode,
        ticks: usize,
    ) -> Option<AutoDecision> {
        (0..ticks).find_map(|_| switcher.observe(sample, config, current))
    }

    /// Settles the switcher's idea of the power source without producing a
    /// transition, the way the first tick after startup does.
    fn settled(on_battery: bool) -> AutoSwitcher {
        let mut switcher = AutoSwitcher::default();
        switcher.observe(
            inputs(Some(on_battery), 0.5),
            &config(),
            PowerMode::Balanced,
        );
        // Only the power source is being settled, not a half-counted
        // refinement that sample may have started.
        switcher.reset();
        switcher
    }

    #[test]
    fn unplugging_drops_to_the_preferred_battery_mode_immediately() {
        let mut switcher = settled(false);
        let decision = switcher
            .observe(inputs(Some(true), 0.5), &config(), PowerMode::Performance)
            .expect("a source change is answered at once");

        assert_eq!(
            decision.mode,
            PowerMode::Eco,
            "Eco is the default home on battery"
        );
        assert!(decision.from_transition);

        let balanced = AutoConfig {
            preferred_on_battery: PowerMode::Balanced,
            ..config()
        };
        let mut switcher = settled(false);
        let decision = switcher
            .observe(inputs(Some(true), 0.5), &balanced, PowerMode::Performance)
            .unwrap();
        assert_eq!(decision.mode, PowerMode::Balanced);
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
        let decision = switcher
            .observe(idle, &config(), PowerMode::Balanced)
            .unwrap();

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
        let decision = switcher
            .observe(idle, &config(), PowerMode::Performance)
            .unwrap();

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
        let flat = AutoInputs {
            battery_percent: Some(12.0),
            ..inputs(Some(true), 0.99)
        };
        assert_eq!(
            target(flat, &config(), true, false, PowerMode::Balanced),
            Some(PowerMode::Eco)
        );
    }

    /// The rule this exists for: a machine that is busy *and* hot gets the
    /// quiet end, not the fast one. Heat and load always arrive together -
    /// the machine is hot because it is working - so if load won here the
    /// thermal rule would never fire at all.
    #[test]
    fn heat_outranks_load() {
        let busy_and_hot = at(90.0, false, 0.99);
        let current = PowerMode::Performance;
        assert_eq!(
            target(busy_and_hot, &config(), false, true, current),
            Some(PowerMode::Balanced)
        );
        assert_eq!(
            target(busy_and_hot, &config(), false, false, PowerMode::Balanced),
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
        assert!(
            latch.observe(Some(85.0), &config()),
            "at the threshold, not past it"
        );
        assert!(
            latch.observe(Some(80.0), &config()),
            "still hot inside the dead band"
        );
        assert!(
            latch.observe(Some(76.0), &config()),
            "and at the bottom of it"
        );
        assert!(
            !latch.observe(Some(75.0), &config()),
            "cool again only below temp_low_c"
        );
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
        let cool_headed = AutoConfig {
            back_off_when_hot: false,
            ..config()
        };
        assert_eq!(
            target(
                at(95.0, false, 0.99),
                &cool_headed,
                false,
                true,
                PowerMode::Balanced
            ),
            Some(PowerMode::Performance)
        );
    }

    /// End to end through the switcher, which is where the latch actually
    /// lives: three agreeing samples and the reason names the temperature.
    #[test]
    fn a_hot_machine_steps_down_and_says_why() {
        let mut switcher = settled(false);
        let hot = at(92.0, false, 0.99);

        assert_eq!(
            switcher.observe(hot, &config(), PowerMode::Performance),
            None
        );
        assert_eq!(
            switcher.observe(hot, &config(), PowerMode::Performance),
            None
        );
        let decision = switcher
            .observe(hot, &config(), PowerMode::Performance)
            .unwrap();

        assert_eq!(decision.mode, PowerMode::Balanced);
        assert!(!decision.from_transition);
        assert_eq!(decision.reason.key, "power.autoReason.hot");
        assert!(
            decision.reason.to_string().contains("92"),
            "{}",
            decision.reason
        );
    }

    /// The latch is updated on every tick, including the ones that return
    /// early. A machine that got hot while unplugged must not come back
    /// from the transition believing it is cool.
    #[test]
    fn the_latch_is_updated_even_on_a_tick_that_answers_a_transition() {
        let mut switcher = settled(false);
        let decision = switcher.observe(at(92.0, true, 0.99), &config(), PowerMode::Performance);
        assert!(decision.is_some_and(|d| d.from_transition));
        assert!(
            switcher.is_hot(),
            "the transition tick still read the sensor"
        );
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
                    for current in [PowerMode::Eco, PowerMode::Balanced, PowerMode::Performance] {
                        for manual in [false, true] {
                            let baseline = Baseline {
                                mode: current,
                                manual,
                            };
                            let decided = refine(
                                inputs(Some(on_battery), load),
                                &config(),
                                on_battery,
                                hot,
                                baseline,
                                current,
                            );
                            assert_ne!(decided.map(|(m, _)| m), Some(PowerMode::Unlimited));
                        }
                    }
                }
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
            assert_eq!(
                switcher.observe(idle, &config(), PowerMode::Unlimited),
                None
            );
        }
    }

    /// ...but unplugging still moves it, because that is a deliberate act
    /// and a laptop running unlimited off a battery is not what was meant.
    #[test]
    fn unplugging_does_move_a_machine_out_of_unlimited() {
        let mut switcher = settled(false);
        let decision = switcher
            .observe(inputs(Some(true), 0.5), &config(), PowerMode::Unlimited)
            .unwrap();

        assert_eq!(decision.mode, PowerMode::Eco);
    }

    #[test]
    fn the_dead_band_between_the_thresholds_produces_no_opinion() {
        let middling = inputs(Some(false), 0.5);
        assert_eq!(
            target(middling, &config(), false, false, PowerMode::Performance),
            None
        );
    }

    #[test]
    fn a_disabled_system_does_nothing_for_its_own_power_source() {
        let off = AutoConfig {
            eco_on_battery: false,
            ..config()
        };
        let mut switcher = settled(false);

        // Unplugging with the Eco system off is not answered...
        assert_eq!(
            switcher.observe(inputs(Some(true), 0.5), &off, PowerMode::Performance),
            None
        );
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
        let decision = switcher
            .observe(busy, &config(), PowerMode::Balanced)
            .unwrap();

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

    // --- Preferred modes -------------------------------------------------

    fn preferring(battery: PowerMode, mains: PowerMode) -> AutoConfig {
        AutoConfig {
            preferred_on_battery: battery,
            preferred_on_mains: mains,
            ..config()
        }
    }

    /// Preferring Eco on battery: a busy machine earns Balanced, and gives
    /// it back once the load has eased past the middle of the band - not
    /// only once it is idle.
    #[test]
    fn preferring_eco_on_battery_climbs_under_load_and_comes_back_when_it_eases() {
        let config = preferring(PowerMode::Eco, PowerMode::Performance);
        let mut switcher = settled(true);

        let up = run(
            &mut switcher,
            inputs(Some(true), 0.9),
            &config,
            PowerMode::Eco,
            5,
        )
        .unwrap();
        assert_eq!(up.mode, PowerMode::Balanced);
        assert_eq!(up.reason.key, "power.autoReason.sustainedLoad");

        // 0.55 is above the middle (0.50): still working, so it holds.
        assert_eq!(
            run(
                &mut switcher,
                inputs(Some(true), 0.55),
                &config,
                PowerMode::Balanced,
                10
            ),
            None
        );

        let back = run(
            &mut switcher,
            inputs(Some(true), 0.45),
            &config,
            PowerMode::Balanced,
            5,
        )
        .unwrap();
        assert_eq!(back.mode, PowerMode::Eco);
        assert_eq!(back.reason.key, "power.autoReason.backToPreferred");
    }

    /// Preferring Balanced on battery: the mirror image. Idle earns Eco,
    /// and it is given back as soon as the machine is doing something -
    /// not only once it is flat out.
    #[test]
    fn preferring_balanced_on_battery_drops_when_idle_and_returns_when_work_resumes() {
        let config = preferring(PowerMode::Balanced, PowerMode::Performance);
        let mut switcher = settled(true);

        let down = run(
            &mut switcher,
            inputs(Some(true), 0.1),
            &config,
            PowerMode::Balanced,
            5,
        )
        .unwrap();
        assert_eq!(down.mode, PowerMode::Eco);
        assert_eq!(down.reason.key, "power.autoReason.idle");

        // 0.45 is below the middle: not enough to call it work.
        assert_eq!(
            run(
                &mut switcher,
                inputs(Some(true), 0.45),
                &config,
                PowerMode::Eco,
                10
            ),
            None
        );

        let back = run(
            &mut switcher,
            inputs(Some(true), 0.55),
            &config,
            PowerMode::Eco,
            5,
        )
        .unwrap();
        assert_eq!(back.mode, PowerMode::Balanced);
    }

    /// The two preferences differ exactly in the dead band: the same
    /// moderate load leaves each of them where the user asked.
    #[test]
    fn moderate_load_keeps_whichever_mode_is_preferred() {
        for preferred in [PowerMode::Eco, PowerMode::Balanced] {
            let config = preferring(preferred, PowerMode::Performance);
            for load in [0.35, 0.5, 0.65] {
                assert_eq!(
                    target(inputs(Some(true), load), &config, true, false, preferred),
                    None
                );
            }
        }
        for preferred in [PowerMode::Balanced, PowerMode::Performance] {
            let config = preferring(PowerMode::Eco, preferred);
            for load in [0.35, 0.5, 0.65] {
                assert_eq!(
                    target(inputs(Some(false), load), &config, false, false, preferred),
                    None
                );
            }
        }
    }

    /// Preferring Balanced on mains: plugging in lands there, sustained
    /// load earns Performance, and it never goes down to Eco.
    #[test]
    fn preferring_balanced_on_mains_earns_performance_and_never_reaches_eco() {
        let config = preferring(PowerMode::Eco, PowerMode::Balanced);
        let mut switcher = settled(true);

        let plugged = switcher
            .observe(inputs(Some(false), 0.9), &config, PowerMode::Eco)
            .unwrap();
        assert_eq!(plugged.mode, PowerMode::Balanced);
        assert!(plugged.from_transition);

        let up = run(
            &mut switcher,
            inputs(Some(false), 0.9),
            &config,
            PowerMode::Balanced,
            5,
        )
        .unwrap();
        assert_eq!(up.mode, PowerMode::Performance);

        let back = run(
            &mut switcher,
            inputs(Some(false), 0.0),
            &config,
            PowerMode::Performance,
            5,
        )
        .unwrap();
        assert_eq!(back.mode, PowerMode::Balanced);
        assert_eq!(
            run(
                &mut switcher,
                inputs(Some(false), 0.0),
                &config,
                PowerMode::Balanced,
                10
            ),
            None
        );
    }

    /// A hand-edited config cannot make the supervisor pick Performance on
    /// battery, Eco on mains, or Unlimited anywhere.
    #[test]
    fn a_preference_outside_the_range_is_clamped_into_it() {
        let config = preferring(PowerMode::Unlimited, PowerMode::Eco);
        assert_eq!(config.preferred(true), PowerMode::Balanced);
        assert_eq!(config.preferred(false), PowerMode::Balanced);
    }

    #[test]
    fn crossed_or_impossible_thresholds_are_reported() {
        assert_eq!(AutoConfig::default().problem(), None);
        let crossed_load = AutoConfig {
            load_low: 0.8,
            load_high: 0.7,
            ..config()
        };
        assert_eq!(crossed_load.problem().unwrap().key, "power.err.loadBand");
        let crossed_temp = AutoConfig {
            temp_low_c: 90.0,
            temp_high_c: 85.0,
            ..config()
        };
        assert_eq!(crossed_temp.problem().unwrap().key, "power.err.tempBand");
        let battery = AutoConfig {
            battery_low_percent: 150.0,
            ..config()
        };
        assert_eq!(battery.problem().unwrap().key, "power.err.batteryPercent");
        let nan = AutoConfig {
            load_high: f64::NAN,
            ..config()
        };
        assert!(nan.problem().is_some());
    }

    /// Serialised as the same lowercase names the rest of the wire uses,
    /// and a config written before the fields existed still loads.
    #[test]
    fn the_preferences_round_trip_and_old_configs_get_the_defaults() {
        let json =
            serde_json::to_value(preferring(PowerMode::Balanced, PowerMode::Balanced)).unwrap();
        assert_eq!(json["preferredOnBattery"], "balanced");
        assert_eq!(json["preferredOnMains"], "balanced");

        let old: AutoConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert_eq!(old.preferred_on_battery, PowerMode::Eco);
        assert_eq!(old.preferred_on_mains, PowerMode::Performance);
    }

    /// A machine found in Performance on battery that nobody chose - after
    /// a restart, say - is brought back into the battery range.
    #[test]
    fn performance_on_battery_that_nobody_chose_is_brought_down() {
        let mut switcher = settled(true);
        let decision = run(
            &mut switcher,
            inputs(Some(true), 0.5),
            &config(),
            PowerMode::Performance,
            5,
        )
        .unwrap();
        assert_eq!(decision.mode, PowerMode::Balanced);
        assert_eq!(decision.reason.key, "power.autoReason.outOfRange");
    }

    // --- A mode picked by hand --------------------------------------------

    /// The case this exists for: Performance picked by hand on battery.
    /// Once the override has run out, a busy machine keeps it - the old
    /// supervisor stepped it down to Balanced exactly when it was needed.
    #[test]
    fn performance_picked_by_hand_on_battery_survives_the_override_under_load() {
        let mut switcher = settled(true);
        switcher.adopt(PowerMode::Performance);

        for load in [0.99, 0.6, 0.4] {
            assert_eq!(
                run(
                    &mut switcher,
                    inputs(Some(true), load),
                    &config(),
                    PowerMode::Performance,
                    10
                ),
                None,
                "load {load} is no reason to take away what the user asked for"
            );
        }
    }

    /// ...but it is stepped down when it has stopped making sense, and
    /// given back when the reason has passed.
    #[test]
    fn performance_picked_by_hand_steps_down_for_idle_heat_and_a_low_battery() {
        let mut switcher = settled(true);
        switcher.adopt(PowerMode::Performance);

        let idle = run(
            &mut switcher,
            inputs(Some(true), 0.05),
            &config(),
            PowerMode::Performance,
            5,
        )
        .unwrap();
        assert_eq!(
            idle.mode,
            PowerMode::Balanced,
            "one step, not all the way to Eco"
        );

        let back = run(
            &mut switcher,
            inputs(Some(true), 0.6),
            &config(),
            PowerMode::Balanced,
            5,
        )
        .unwrap();
        assert_eq!(back.mode, PowerMode::Performance);
        assert_eq!(back.reason.key, "power.autoReason.backToPreferred");

        let hot = run(
            &mut switcher,
            at(90.0, true, 0.99),
            &config(),
            PowerMode::Performance,
            5,
        )
        .unwrap();
        assert_eq!(hot.mode, PowerMode::Balanced);
        assert_eq!(hot.reason.key, "power.autoReason.hot");
        // Still latched at 80 C: no climbing back into the same wall.
        assert_eq!(
            run(
                &mut switcher,
                at(80.0, true, 0.99),
                &config(),
                PowerMode::Balanced,
                10
            ),
            None
        );

        let flat = AutoInputs {
            battery_percent: Some(15.0),
            ..inputs(Some(true), 0.99)
        };
        let low = run(&mut switcher, flat, &config(), PowerMode::Balanced, 5).unwrap();
        assert_eq!(low.mode, PowerMode::Eco);
        assert_eq!(
            run(&mut switcher, flat, &config(), PowerMode::Eco, 10),
            None
        );
    }

    /// A hand-picked mode is only ever stepped down from: Eco chosen for a
    /// quiet room stays Eco under a compile.
    #[test]
    fn a_hand_picked_mode_is_never_stepped_up_from() {
        let mut switcher = settled(false);
        switcher.adopt(PowerMode::Eco);
        assert_eq!(
            run(
                &mut switcher,
                inputs(Some(false), 0.99),
                &config(),
                PowerMode::Eco,
                10
            ),
            None
        );
        assert_eq!(
            switcher.manual_baseline(&config(), false),
            Some(PowerMode::Eco)
        );
    }

    /// Picking the mode the supervisor would have chosen anyway is agreeing
    /// with it, and must not switch off the step up the preference allows.
    #[test]
    fn picking_the_preferred_mode_by_hand_changes_nothing() {
        let config = preferring(PowerMode::Eco, PowerMode::Performance);
        let mut switcher = settled(true);
        switcher.adopt(PowerMode::Eco);
        assert_eq!(switcher.manual_baseline(&config, true), None);

        let up = run(
            &mut switcher,
            inputs(Some(true), 0.9),
            &config,
            PowerMode::Eco,
            5,
        )
        .unwrap();
        assert_eq!(up.mode, PowerMode::Balanced);
    }

    /// A choice is about the power source it was made on. Moving the cable
    /// forgets it - even when the new source's system is switched off, or
    /// it would come back into force the next time the cable moved again.
    #[test]
    fn changing_the_power_source_forgets_the_hand_picked_mode() {
        let mut switcher = settled(true);
        switcher.adopt(PowerMode::Performance);

        let plugged = switcher.observe(inputs(Some(false), 0.5), &config(), PowerMode::Performance);
        assert_eq!(
            plugged, None,
            "already in Performance, the mains preference"
        );
        assert_eq!(switcher.manual_baseline(&config(), false), None);

        switcher.adopt(PowerMode::Performance);
        let no_eco_system = AutoConfig {
            eco_on_battery: false,
            ..config()
        };
        assert_eq!(
            switcher.observe(
                inputs(Some(true), 0.5),
                &no_eco_system,
                PowerMode::Performance
            ),
            None
        );
        assert_eq!(switcher.manual_baseline(&config(), true), None);
    }
}
