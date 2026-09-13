//! Calibration: measuring what "full speed" actually is on this machine -
//! and, where a speed can be commanded, what the slowest one is.
//!
//! The hysteresis in [`crate::curve`] wants one number it has never had -
//! the RPM the fans reach at full speed - so that "is the fan already going
//! roughly this fast" can be asked of the tachometer rather than of the PWM
//! value we last wrote. Without it the deadband is [`crate::curve`]'s
//! `PWM_DEADBAND`, which is a guess about a linear relationship that no fan
//! actually has.
//!
//! The routine is the one the source project specifies
//! (`docs/04-fan-control-logic.md` §Calibration): put the fans at **max**,
//! watch them, keep the peak, put back what was there. Two deliberate
//! refinements:
//!
//! - **It stops as soon as the reading settles**, after
//!   [`MIN_SECONDS`]. On the test laptop the fans go from ~2000 to ~3900
//!   rpm in six seconds (`dev/FINDINGS.md`), so a fixed thirty is
//!   twenty-four seconds of noise that measures nothing. The full duration
//!   is still the ceiling, not the target.
//! - **A run that did not move the fans stores nothing.** A machine where
//!   `max` is accepted and ignored would otherwise record its idle speed as
//!   its ceiling, which is worse than having no calibration at all: the
//!   hysteresis would then believe every target above idle was already
//!   reached. The one case where no rise is expected - the fans were
//!   *already* at max when the run started - is recognised rather than
//!   guessed at.
//!
//! Only `pwm1_enable` is needed, since `max` is a mode and not a speed. So
//! this runs on a board like `8D2F`, which cannot be given a percentage at
//! all - and on such a board it is the only way to learn the number the
//! driver's own `OMEN_CPU_MAX_RPM` fallback is standing in for.

use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::control::{self, Capabilities, FanMode};
use crate::{observed_mode, parse_hwmon_rpm, read_raw_rpm, FanPaths};

/// How long a run may take when the reading never settles.
pub const DEFAULT_SECONDS: u64 = 30;

/// Longest a caller may ask for. Half-speed fans for two minutes is
/// already an odd thing to want; there is no reading past it.
pub const MAX_SECONDS: u64 = 120;

/// Shortest a run may be, and the earliest a settled reading is believed.
/// Below this a fan that ramps in steps can look settled between steps.
pub const MIN_SECONDS: u64 = 10;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Consecutive samples without a meaningful rise that mean "settled".
const SETTLED_SAMPLES: usize = 5;

/// A rise smaller than this is tachometer jitter, not a fan still climbing.
const SETTLED_RISE_RPM: i64 = 50;

/// How much the fans must gain over the baseline for the run to have
/// measured a ceiling rather than an idle speed. The observed ramp is
/// ~600 rpm every two seconds, so this is under a second of it.
const MIN_RISE_RPM: i64 = 300;

/// One reading, kept so the reply can show the ramp rather than assert a
/// number. The trace is the evidence for the verdict.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub at_secs: u64,
    pub fan1_rpm: i64,
    pub fan2_rpm: i64,
    pub is_reverse: bool,
}

impl Sample {
    fn faster(&self) -> i64 {
        self.fan1_rpm.max(self.fan2_rpm)
    }
}

/// Why a run did or did not produce a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Verdict {
    /// A ceiling was measured and is worth storing.
    Measured,
    /// No fan reported a speed at all, so there is nothing to measure.
    NoReading,
    /// The fans never sped up, and they were not already at max. Either
    /// the firmware ignored the request or the tachometer is not moving.
    DidNotRespond,
    /// A fan was spinning backwards (the fan cleaner's encoding), so the
    /// reading is not a speed this machine reaches in normal use.
    Reverse,
}

impl Verdict {
    pub fn worth_storing(self) -> bool {
        matches!(self, Self::Measured)
    }
}

/// What a run found, and enough of how it found it to argue with.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Calibration {
    pub verdict: Verdict,
    /// The peak of whichever fan reads faster - the form the hysteresis
    /// compares against, since [`crate::read_fan_rpm`] reports the same.
    pub fan_max_rpm: Option<i64>,
    /// Per fan, because the driver has a constant for each
    /// (`OMEN_CPU_MAX_RPM` / `OMEN_GPU_MAX_RPM`) and the installer can
    /// patch them separately. Which fan cools what is not asserted here:
    /// the driver numbers them and this module does the same.
    pub fan1_max_rpm: Option<i64>,
    pub fan2_max_rpm: Option<i64>,
    /// The slowest the fans turn when commanded the least a speed can be,
    /// measured after the ceiling (see [`FloorRun`]) - or, on a driver that
    /// reports it, the floor that driver enforces. `None` when the machine
    /// cannot be given a speed, or the fans did not come down.
    pub fan_min_rpm: Option<i64>,
    /// Pyren's floor: the slowest speed the fans held with the driver's
    /// clamp lifted, the next step down having failed. Only on a driver
    /// that lets the clamp be lifted; see [`sweep_floor`].
    pub fan_stable_min_rpm: Option<i64>,
    /// The reading before anything was written.
    pub baseline_rpm: i64,
    /// Whether the fans were already at max, in which case no rise is
    /// expected and its absence is not a failure.
    pub started_at_max: bool,
    pub seconds: u64,
    /// True when the run ended because the reading stopped climbing.
    pub settled: bool,
    pub samples: Vec<Sample>,
    /// The mode put back afterwards, and what went wrong if anything did.
    pub restored_mode: &'static str,
    pub restore_error: Option<String>,
    /// A sentence for a human, saying what the verdict means here.
    pub detail: String,
}

/// The decision half, separated from the hardware half so the thing worth
/// testing - when a run is finished, and what it is allowed to conclude -
/// can be tested without an HP laptop.
#[derive(Debug, Clone)]
pub struct Run {
    baseline: i64,
    started_at_max: bool,
    limit_secs: u64,
    samples: Vec<Sample>,
    peak: i64,
    flat_for: usize,
    saw_reverse: bool,
}

impl Run {
    pub fn new(baseline: i64, started_at_max: bool, limit_secs: u64) -> Self {
        Self {
            baseline,
            started_at_max,
            limit_secs: limit_secs.clamp(MIN_SECONDS, MAX_SECONDS),
            samples: Vec::new(),
            peak: 0,
            flat_for: 0,
            saw_reverse: false,
        }
    }

    pub fn push(&mut self, sample: Sample) {
        let faster = sample.faster();
        if faster > self.peak + SETTLED_RISE_RPM {
            self.flat_for = 0;
        } else {
            self.flat_for += 1;
        }
        self.peak = self.peak.max(faster);
        self.saw_reverse |= sample.is_reverse;
        self.samples.push(sample);
    }

    /// Whether the run has learned everything it is going to.
    pub fn is_done(&self, elapsed_secs: u64) -> bool {
        elapsed_secs >= self.limit_secs || self.is_settled(elapsed_secs)
    }

    fn is_settled(&self, elapsed_secs: u64) -> bool {
        elapsed_secs >= MIN_SECONDS && self.flat_for >= SETTLED_SAMPLES
    }

    fn peak_of(&self, of: impl Fn(&Sample) -> i64) -> Option<i64> {
        let peak = self.samples.iter().map(of).max()?;
        (peak > 0).then_some(peak)
    }

    /// Turns the trace into a verdict. `elapsed_secs` is how long the run
    /// actually took, which is not the limit when it settled early.
    pub fn finish(self, elapsed_secs: u64) -> Calibration {
        let settled = self.is_settled(elapsed_secs);
        let rise = self.peak - self.baseline;

        let (verdict, detail) = if self.peak <= 0 {
            (
                Verdict::NoReading,
                "no fan reported a speed during the run, so there is nothing to \
                 calibrate against"
                    .to_string(),
            )
        } else if self.saw_reverse {
            (
                Verdict::Reverse,
                "a fan was spinning in reverse during the run; that is the fan \
                 cleaner's speed, not this machine's ceiling"
                    .to_string(),
            )
        } else if !self.started_at_max && rise < MIN_RISE_RPM {
            (
                Verdict::DidNotRespond,
                format!(
                    "the fans went from {} to {} rpm, a rise of {rise}. Max was \
                     accepted and changed nothing, so {} rpm is this machine's \
                     idle speed rather than its ceiling, and storing it would \
                     make the curve worse rather than better",
                    self.baseline, self.peak, self.peak
                ),
            )
        } else if self.started_at_max {
            (
                Verdict::Measured,
                format!(
                    "{} rpm, measured over {elapsed_secs}s. The fans were already \
                     at max when the run started, so there was no ramp to watch",
                    self.peak
                ),
            )
        } else {
            (
                Verdict::Measured,
                format!(
                    "{} rpm, up from {} at idle, {}",
                    self.peak,
                    self.baseline,
                    if settled {
                        format!("settled after {elapsed_secs}s")
                    } else {
                        format!("still climbing when the {elapsed_secs}s ran out")
                    }
                ),
            )
        };

        let measured = verdict.worth_storing();
        Calibration {
            verdict,
            fan_max_rpm: measured.then_some(self.peak),
            fan1_max_rpm: measured.then(|| self.peak_of(|s| s.fan1_rpm)).flatten(),
            fan2_max_rpm: measured.then(|| self.peak_of(|s| s.fan2_rpm)).flatten(),
            // Filled in by `run`, from the second half of the run.
            fan_min_rpm: None,
            fan_stable_min_rpm: None,
            baseline_rpm: self.baseline,
            started_at_max: self.started_at_max,
            seconds: elapsed_secs,
            settled,
            samples: self.samples,
            // Filled in by `run`, which is the half that owns the hardware.
            restored_mode: "auto",
            restore_error: None,
            detail,
        }
    }
}

/// The second half of a run: from full speed, command the slowest speed
/// there is and watch where the fans come to rest.
///
/// That resting point is the floor [`crate::curve::stop_below_pwm`] needs.
/// It is not zero on board 8D2F: the driver clamps every manual speed to
/// the slowest entry of the firmware's fan table (1800 rpm), which is why
/// 0 % in a curve used to sound exactly like a third.
///
/// Settles the same way the ceiling does, mirrored: five samples without a
/// meaningful *fall*. A controller that undershoots on the way down and
/// climbs back counts the climb as settled, and the last reading - not the
/// lowest - is the answer, so the undershoot is not mistaken for the floor.
#[derive(Debug, Clone)]
pub struct FloorRun {
    limit_secs: u64,
    samples: Vec<Sample>,
    low: Option<i64>,
    flat_for: usize,
    saw_reverse: bool,
}

impl FloorRun {
    pub fn new(limit_secs: u64) -> Self {
        Self {
            limit_secs: limit_secs.clamp(MIN_SECONDS, MAX_SECONDS),
            samples: Vec::new(),
            low: None,
            flat_for: 0,
            saw_reverse: false,
        }
    }

    pub fn push(&mut self, sample: Sample) {
        let faster = sample.faster();
        match self.low {
            Some(low) if faster >= low - SETTLED_RISE_RPM => self.flat_for += 1,
            _ => self.flat_for = 0,
        }
        self.low = Some(self.low.map_or(faster, |low| low.min(faster)));
        self.saw_reverse |= sample.is_reverse;
        self.samples.push(sample);
    }

    pub fn is_done(&self, elapsed_secs: u64) -> bool {
        elapsed_secs >= self.limit_secs
            || (elapsed_secs >= MIN_SECONDS && self.flat_for >= SETTLED_SAMPLES)
    }

    /// The floor, given the ceiling the first half measured. `None` unless
    /// the fans came well down from it: a machine that ignores the command
    /// would otherwise report its full speed as its slowest.
    pub fn finish(&self, peak: i64) -> Option<i64> {
        if self.saw_reverse {
            return None;
        }
        let rest = self.samples.last()?.faster();
        (peak - rest >= MIN_RISE_RPM).then_some(rest)
    }
}

/// Puts back the mode the machine was found in, whatever happens to the
/// run in between - including a panic, which is why this is a guard and
/// not a line at the end.
///
/// Shared with [`crate::speed_probe`], which drives the fans for the same
/// kind of reason and must put them back with the same certainty.
pub(crate) struct Restore<'a> {
    paths: &'a FanPaths,
    caps: Capabilities,
    mode: FanMode,
    pwm: u8,
    done: bool,
}

impl<'a> Restore<'a> {
    pub(crate) fn new(paths: &'a FanPaths, caps: Capabilities, mode: FanMode, pwm: u8) -> Self {
        Self {
            paths,
            caps,
            mode,
            pwm,
            done: false,
        }
    }

    /// Restores explicitly, so the outcome can be reported rather than
    /// swallowed. Falling back to `auto` is deliberate: it is the mode
    /// where the firmware owns the fans, and leaving a machine at full
    /// speed because the restore failed would be the worse failure.
    pub(crate) fn finish(mut self) -> (&'static str, Option<String>) {
        self.done = true;
        match control::apply(self.paths, self.caps, self.mode, self.pwm) {
            Ok(()) => (self.mode.as_str(), None),
            Err(e) => {
                let first = e.to_string();
                match control::apply(self.paths, self.caps, FanMode::Auto, 0) {
                    Ok(()) => ("auto", Some(format!("{first}; fell back to auto"))),
                    Err(second) => ("none", Some(format!("{first}; auto also failed: {second}"))),
                }
            }
        }
    }
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if control::apply(self.paths, self.caps, self.mode, self.pwm).is_err() {
            let _ = control::apply(self.paths, self.caps, FanMode::Auto, 0);
        }
    }
}

/// Asked once per sample while a measurement holds the fans: `Some` ends
/// the run at once - the [`Restore`] guard puts the fans back on the way
/// out - and becomes the run's error. The caller decides what counts;
/// today that is the machine getting too hot to be measuring anything.
pub(crate) type Abort<'a> = &'a dyn Fn() -> Option<control::ControlError>;

/// Runs a calibration against the hardware. Blocks for up to `seconds`.
///
/// The caller is responsible for keeping the control loop off the fans
/// while this runs; see `State::calibrating`.
pub(crate) fn run(
    paths: &FanPaths,
    caps: Capabilities,
    seconds: u64,
    abort: Abort,
) -> Result<Calibration, control::ControlError> {
    if let Some(e) = abort() {
        return Err(e);
    }
    let limit = seconds.clamp(MIN_SECONDS, MAX_SECONDS);
    let before_mode = observed_mode(paths).unwrap_or(FanMode::Auto);
    let before_pwm = control::read_pwm(paths).unwrap_or(crate::curve::MIN_COMMANDED_PWM);
    let baseline = sample(paths, 0).faster();

    control::apply(paths, caps, FanMode::Max, 0)?;
    let restore = Restore::new(paths, caps, before_mode, before_pwm);

    let mut measurement = Run::new(baseline, before_mode == FanMode::Max, limit);
    let started = Instant::now();
    let elapsed = loop {
        sleep(SAMPLE_INTERVAL);
        if let Some(e) = abort() {
            return Err(e);
        }
        let elapsed = started.elapsed().as_secs();
        measurement.push(sample(paths, elapsed));
        if measurement.is_done(elapsed) {
            break elapsed;
        }
    };

    let mut calibration = measurement.finish(elapsed);
    if calibration.verdict.worth_storing() && caps.supports(FanMode::Manual) {
        // A driver that reports its floor needs it measured no more; what
        // is worth measuring there is how far below it the fans can go.
        match control::read_driver_floor(paths).filter(|_| control::floor_override_supported(paths))
        {
            Some(driver_floor) => {
                calibration.fan_min_rpm = Some(driver_floor);
                sweep_floor(paths, caps, driver_floor, &mut calibration, abort);
            }
            None => measure_floor(paths, caps, limit, &mut calibration, abort),
        }
        // The floor steps are the part that holds the fans near stall, so
        // they are where an abort is most likely to land. Asked again here
        // because both steps swallow their own failures.
        if let Some(e) = abort() {
            return Err(e);
        }
    }
    let (restored_mode, restore_error) = restore.finish();
    calibration.restored_mode = restored_mode;
    calibration.restore_error = restore_error;
    Ok(calibration)
}

/// Runs a [`FloorRun`] straight after the ceiling, while the fans are
/// still at it. Never fails the calibration: the ceiling is measured and
/// worth keeping whatever happens here.
fn measure_floor(
    paths: &FanPaths,
    caps: Capabilities,
    limit: u64,
    calibration: &mut Calibration,
    abort: Abort,
) {
    let Some(peak) = calibration.fan_max_rpm else {
        return;
    };
    if control::apply(
        paths,
        caps,
        FanMode::Manual,
        crate::curve::MIN_COMMANDED_PWM,
    )
    .is_err()
    {
        return;
    }

    let offset = calibration.seconds;
    let mut floor = FloorRun::new(limit);
    let started = Instant::now();
    loop {
        sleep(SAMPLE_INTERVAL);
        if abort().is_some() {
            return;
        }
        let elapsed = started.elapsed().as_secs();
        floor.push(sample(paths, offset + elapsed));
        if floor.is_done(elapsed) {
            break;
        }
    }

    calibration.samples.extend(floor.samples.iter().copied());
    calibration.fan_min_rpm = floor.finish(peak);
    if let Some(min) = calibration.fan_min_rpm {
        calibration.detail.push_str(&format!(
            "; told the slowest speed, the fans settle at {min} rpm"
        ));
    }
}

// --- Pyren's floor ------------------------------------------------------
//
// The driver's floor is the fan table's slowest entry: the bottom of the
// firmware's *automatic* curve, not the slowest the fans can turn. With the
// clamp lifted on board 8D2F they held every speed down to 600 rpm on one
// run; asked for 400 they went 900 -> 1400, and at 200 they stalled and
// were kicked back to 500. On another run 600 itself kicked once - 700,
// 1600, 800 - so the edge is not a fixed number, and a floor sitting on it
// is a fan that restarts itself every so often. That kick, a motor that has
// stalled being restarted by its controller, is what the sweep looks for.
//
// Each step is commanded through the driver's own floor rather than through
// pwm: the override is set to the step and pwm to 1, which the driver clamps
// up to exactly the override on both fans. Converting rpm to pwm here would
// need the ceiling the *loaded* driver scales by, which a calibration that
// has just measured a new one and pinned it for the next load does not
// have - that mismatch put every step of the first sweep 100 rpm low.

/// Size of each step down while well clear of any stall.
pub const SWEEP_COARSE_STEP_RPM: i64 = 200;
/// Below this the steps are fine, since this is where the floor is.
pub const SWEEP_FINE_BELOW_RPM: i64 = 1000;
/// Size of each step down near the floor. The floor is the last one held,
/// so the next step down - which failed - is its margin.
pub const SWEEP_FINE_STEP_RPM: i64 = 100;
/// Lowest speed tried. Below this nothing is a speed.
pub const SWEEP_LOWEST_RPM: i64 = 200;
/// How far from the commanded speed a settled reading may be. The
/// tachometer reports hundreds.
pub const SWEEP_TOLERANCE_RPM: i64 = 150;
/// A rise this big between two readings while stepping *down* is a kick:
/// the fan stalled and its controller restarted it.
pub const SWEEP_KICK_RPM: i64 = 300;
/// Readings at the end of a step that have to be at the speed.
pub const SWEEP_HOLD_SAMPLES: usize = 3;
/// How long each step is watched, whole, so a kick anywhere in it counts.
/// Short where no fan stalls, longer near the floor, where one kicked
/// within two seconds on board 8D2F.
pub const SWEEP_COARSE_SECS: u64 = 4;
pub const SWEEP_FINE_SECS: u64 = 6;
/// Longest the coast from full speed down to the driver's floor may take.
/// Measured at twelve to fifteen seconds from 5500 on board 8D2F.
pub const SWEEP_SETTLE_SECS: u64 = 25;

/// Which fans the sweep watches: the ones the ceiling saw turning. A
/// machine with one fan must not fail every step on the one it lacks.
#[derive(Debug, Clone, Copy)]
pub struct Fans {
    pub fan1: bool,
    pub fan2: bool,
}

impl Fans {
    fn readings(self, sample: &Sample) -> impl Iterator<Item = i64> {
        [(self.fan1, sample.fan1_rpm), (self.fan2, sample.fan2_rpm)]
            .into_iter()
            .filter_map(|(watched, rpm)| watched.then_some(rpm))
    }
}

/// Whether the last few readings show every watched fan at `expected`.
/// Enough to know a coast has arrived; not enough to call a step held.
pub fn holding(samples: &[Sample], expected: i64, fans: Fans) -> bool {
    samples.len() >= SWEEP_HOLD_SAMPLES
        && samples[samples.len() - SWEEP_HOLD_SAMPLES..]
            .iter()
            .all(|s| {
                fans.readings(s)
                    .all(|rpm| rpm > 0 && (rpm - expected).abs() <= SWEEP_TOLERANCE_RPM)
            })
}

/// Whether a whole step shows the fans turning at `expected`: arrived and
/// steady at the end, and at no point in it stopped or kicked back up.
pub fn step_held(samples: &[Sample], expected: i64, fans: Fans) -> bool {
    let stopped = samples.iter().any(|s| fans.readings(s).any(|rpm| rpm <= 0));
    let kicked = samples.windows(2).any(|pair| {
        fans.readings(&pair[0])
            .zip(fans.readings(&pair[1]))
            .any(|(before, after)| after - before > SWEEP_KICK_RPM)
    });
    !stopped && !kicked && holding(samples, expected, fans)
}

/// The steps a sweep from `driver_floor` tries, in order: coarse while
/// clear of [`SWEEP_FINE_BELOW_RPM`], fine below it.
pub fn next_step(rpm: i64) -> Option<(i64, bool)> {
    let coarse = rpm - SWEEP_COARSE_STEP_RPM >= SWEEP_FINE_BELOW_RPM;
    let next = rpm
        - if coarse {
            SWEEP_COARSE_STEP_RPM
        } else {
            SWEEP_FINE_STEP_RPM
        };
    (next >= SWEEP_LOWEST_RPM).then_some((next, coarse))
}

/// Puts the driver's floor override back however the sweep ends. Left at
/// a step's value, it would be the floor for every manual write after -
/// ours or anyone's.
struct OverrideGuard<'a> {
    paths: &'a FanPaths,
    value: u8,
}

impl Drop for OverrideGuard<'_> {
    fn drop(&mut self) {
        let _ = control::set_floor_override(self.paths, self.value);
    }
}

/// Runs the sweep straight after the ceiling. Never fails the calibration.
fn sweep_floor(
    paths: &FanPaths,
    caps: Capabilities,
    driver_floor: i64,
    calibration: &mut Calibration,
    abort: Abort,
) {
    let fans = Fans {
        fan1: calibration.fan1_max_rpm.is_some(),
        fan2: calibration.fan2_max_rpm.is_some(),
    };
    if !fans.fan1 && !fans.fan2 {
        return;
    }
    let before = control::read_floor_override(paths).unwrap_or(0);
    let _guard = OverrideGuard {
        paths,
        value: before,
    };

    let mut clock = calibration.seconds;
    let mut trace = Vec::new();
    // Commands exactly `rpm` on both fans: the driver clamps pwm 1 up to
    // its floor, and the floor is set to `rpm`. `settle` ends as soon as
    // the fans arrive; a step is watched whole.
    let mut run = |rpm: i64, secs: u64, settle: bool| -> bool {
        if abort().is_some() {
            return false;
        }
        let Ok(hundreds) = u8::try_from(rpm / 100) else {
            return false;
        };
        if control::set_floor_override(paths, hundreds).is_err()
            || control::apply(paths, caps, FanMode::Manual, 1).is_err()
        {
            return false;
        }
        let mut step = Vec::new();
        for _ in 0..secs {
            sleep(SAMPLE_INTERVAL);
            if abort().is_some() {
                return false;
            }
            clock += 1;
            step.push(sample(paths, clock));
            if settle && holding(&step, rpm, fans) {
                break;
            }
        }
        let held = if settle {
            holding(&step, rpm, fans)
        } else {
            step_held(&step, rpm, fans)
        };
        trace.extend(step);
        held
    };

    // From full speed to the driver's floor first, so no step is judged
    // on a coast that takes longer than a step is watched.
    if !run(driver_floor, SWEEP_SETTLE_SECS, true) {
        return;
    }
    let mut last_held = None;
    let mut rpm = driver_floor;
    while let Some((next, coarse)) = next_step(rpm) {
        if run(
            next,
            if coarse {
                SWEEP_COARSE_SECS
            } else {
                SWEEP_FINE_SECS
            },
            false,
        ) {
            last_held = Some(next);
            rpm = next;
            continue;
        }
        // A coarse step can jump past the floor; the speed it skipped is
        // worth one fine try before settling for the one above.
        let skipped = next + SWEEP_FINE_STEP_RPM;
        if coarse && skipped < rpm && run(skipped, SWEEP_FINE_SECS, false) {
            last_held = Some(skipped);
        }
        break;
    }

    let floor = last_held.unwrap_or(driver_floor);
    calibration.samples.extend(trace);
    calibration.fan_stable_min_rpm = Some(floor);
    calibration.detail.push_str(&match last_held {
        Some(held) => format!(
            "; with the driver's {driver_floor} rpm floor lifted the fans held {held} rpm \
             and not a step below; Pyren's floor keeps a step clear of that"
        ),
        None => format!("; the fans would not hold anything below the driver's {driver_floor} rpm"),
    });
}

fn sample(paths: &FanPaths, at_secs: u64) -> Sample {
    let (fan1_rpm, rev1) = parse_hwmon_rpm(read_raw_rpm(paths.fan1_input.as_deref()));
    let (fan2_rpm, rev2) = parse_hwmon_rpm(read_raw_rpm(paths.fan2_input.as_deref()));
    Sample {
        at_secs,
        fan1_rpm,
        fan2_rpm,
        is_reverse: rev1 || rev2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_at(at_secs: u64, fan1: i64, fan2: i64) -> Sample {
        Sample {
            at_secs,
            fan1_rpm: fan1,
            fan2_rpm: fan2,
            is_reverse: false,
        }
    }

    /// The ramp actually measured on board 8D2F (`dev/FINDINGS.md`), one
    /// sample a second rather than every two.
    fn ramp() -> Vec<Sample> {
        let readings = [
            2400, 2700, 3000, 3300, 3600, 3900, 3910, 3900, 3915, 3905, 3900, 3910, 3905,
        ];
        readings
            .iter()
            .enumerate()
            .map(|(i, rpm)| sample_at(i as u64 + 1, *rpm, rpm - 170))
            .collect()
    }

    fn feed(run: &mut Run, samples: &[Sample]) -> u64 {
        let mut elapsed = 0;
        for sample in samples {
            elapsed = sample.at_secs;
            run.push(*sample);
            if run.is_done(elapsed) {
                break;
            }
        }
        elapsed
    }

    #[test]
    fn the_ceiling_is_the_peak_of_the_faster_fan() {
        let mut run = Run::new(2093, false, DEFAULT_SECONDS);
        let elapsed = feed(&mut run, &ramp());
        let result = run.finish(elapsed);

        assert_eq!(result.verdict, Verdict::Measured);
        assert_eq!(result.fan_max_rpm, Some(3915));
        assert_eq!(result.fan1_max_rpm, Some(3915));
        assert_eq!(result.fan2_max_rpm, Some(3745));
    }

    /// Thirty seconds of full-speed fans to learn something that stopped
    /// changing at six is noise, not rigour.
    #[test]
    fn a_reading_that_stops_climbing_ends_the_run_early() {
        let mut run = Run::new(2093, false, DEFAULT_SECONDS);
        let elapsed = feed(&mut run, &ramp());

        assert!(
            elapsed < DEFAULT_SECONDS,
            "should not have run the full {DEFAULT_SECONDS}s"
        );
        assert!(run.finish(elapsed).settled);
    }

    /// A fan that ramps in steps is flat between them; believing the first
    /// plateau would record a ceiling the machine goes well past.
    #[test]
    fn a_plateau_before_the_minimum_duration_is_not_settled() {
        let mut run = Run::new(2000, false, DEFAULT_SECONDS);
        let mut samples: Vec<Sample> = (1..=8).map(|i| sample_at(i, 3000, 2800)).collect();
        samples.extend((9..=20).map(|i| sample_at(i, 4200, 4000)));
        let elapsed = feed(&mut run, &samples);
        let result = run.finish(elapsed);

        assert!(elapsed >= MIN_SECONDS);
        assert_eq!(
            result.fan_max_rpm,
            Some(4200),
            "the second step must be seen"
        );
    }

    fn feed_floor(run: &mut FloorRun, readings: &[i64]) -> u64 {
        let mut elapsed = 0;
        for (i, rpm) in readings.iter().enumerate() {
            elapsed = i as u64 + 1;
            run.push(sample_at(elapsed, *rpm, rpm - 100));
            if run.is_done(elapsed) {
                break;
            }
        }
        elapsed
    }

    /// The coast-down measured on board 8D2F: from 5300 to the fan table's
    /// 1800 in about ten seconds, then flat.
    #[test]
    fn the_floor_is_where_the_fans_come_to_rest() {
        let mut floor = FloorRun::new(DEFAULT_SECONDS);
        let readings = [
            4600, 3800, 2900, 2300, 2000, 1800, 1800, 1800, 1800, 1800, 1800, 1800,
        ];
        let elapsed = feed_floor(&mut floor, &readings);

        assert!(
            elapsed < DEFAULT_SECONDS,
            "a settled floor should end the run"
        );
        assert_eq!(floor.finish(5300), Some(1800));
    }

    /// Undershoot and recover: the resting point is the answer, not the dip.
    #[test]
    fn an_undershoot_on_the_way_down_is_not_the_floor() {
        let mut floor = FloorRun::new(DEFAULT_SECONDS);
        let readings = [
            4000, 2600, 1500, 1700, 1800, 1800, 1800, 1800, 1800, 1800, 1800,
        ];
        feed_floor(&mut floor, &readings);

        assert_eq!(floor.finish(5300), Some(1800));
    }

    /// Told the slowest speed and still at full: the command was ignored,
    /// and full speed is no one's floor.
    #[test]
    fn fans_that_did_not_come_down_have_no_floor() {
        let mut floor = FloorRun::new(DEFAULT_SECONDS);
        feed_floor(&mut floor, &[5300; 30]);

        assert_eq!(floor.finish(5300), None);
    }

    /// A fan that stops when told the minimum has a floor of zero, which
    /// is an answer rather than a missing reading.
    #[test]
    fn fans_that_stop_on_command_have_a_floor_of_zero() {
        let mut floor = FloorRun::new(DEFAULT_SECONDS);
        feed_floor(&mut floor, &[3000, 1200, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        assert_eq!(floor.finish(5300), Some(0));
    }

    const BOTH: Fans = Fans {
        fan1: true,
        fan2: true,
    };

    fn readings(pairs: &[(i64, i64)]) -> Vec<Sample> {
        pairs
            .iter()
            .enumerate()
            .map(|(i, (a, b))| sample_at(i as u64 + 1, *a, *b))
            .collect()
    }

    /// The EC experiment on board 8D2F, step by step: 600 held exactly.
    #[test]
    fn a_speed_read_back_steadily_is_held() {
        assert!(holding(
            &readings(&[(1000, 1000), (600, 600), (600, 600), (600, 600)]),
            600,
            BOTH
        ));
    }

    /// ...400 did not: 900 at six seconds, 1400 at ten.
    #[test]
    fn a_fan_hunting_around_a_speed_does_not_hold_it() {
        assert!(!holding(
            &readings(&[(900, 800), (1100, 1000), (1400, 1400)]),
            400,
            BOTH
        ));
    }

    /// ...and 200 stalled outright before being kicked back up.
    #[test]
    fn a_stalled_fan_does_not_hold_even_a_slow_speed() {
        assert!(!holding(
            &readings(&[(200, 200), (0, 0), (200, 200)]),
            200,
            BOTH
        ));
    }

    /// Both fans have to hold it: one stalled fan is a stalled speed.
    #[test]
    fn every_watched_fan_has_to_hold() {
        assert!(!holding(
            &readings(&[(600, 0), (600, 0), (600, 0)]),
            600,
            BOTH
        ));
    }

    /// A machine with one fan is judged on that fan.
    #[test]
    fn a_missing_fan_is_not_held_against_the_speed() {
        let one = Fans {
            fan1: true,
            fan2: false,
        };
        assert!(holding(
            &readings(&[(600, 0), (600, 0), (600, 0)]),
            600,
            one
        ));
    }

    #[test]
    fn too_few_readings_prove_nothing() {
        assert!(!holding(&readings(&[(600, 600), (600, 600)]), 600, BOTH));
    }

    /// The installer log from board 8D2F's second sweep, at 600: arrived,
    /// then kicked to 1600 and back. The last three readings are fine, and
    /// the step still did not hold.
    #[test]
    fn a_kick_anywhere_in_a_step_fails_it() {
        let step = readings(&[
            (700, 700),
            (1600, 1600),
            (800, 800),
            (600, 600),
            (600, 600),
            (600, 600),
        ]);
        assert!(holding(&step, 600, BOTH), "the tail alone looks settled");
        assert!(!step_held(&step, 600, BOTH));
    }

    #[test]
    fn a_clean_step_down_holds() {
        let step = readings(&[
            (850, 850),
            (700, 700),
            (700, 700),
            (700, 700),
            (700, 700),
            (700, 700),
        ]);
        assert!(step_held(&step, 700, BOTH));
    }

    /// Falling on the way to a step is the step arriving, not a kick.
    #[test]
    fn falling_readings_are_not_kicks() {
        let step = readings(&[
            (1600, 1600),
            (1200, 1200),
            (1000, 1000),
            (1000, 1000),
            (1000, 1000),
        ]);
        assert!(step_held(&step, 1000, BOTH));
    }

    /// A fan that reads 0 for one second has stalled, whatever it does next.
    #[test]
    fn a_zero_anywhere_in_a_step_fails_it() {
        let step = readings(&[
            (600, 600),
            (0, 600),
            (500, 600),
            (600, 600),
            (600, 600),
            (600, 600),
        ]);
        assert!(!step_held(&step, 600, BOTH));
    }

    /// 1800 down in 200s to 1000, then 100s, stopping at 200.
    #[test]
    fn the_steps_are_coarse_then_fine() {
        let mut steps = Vec::new();
        let mut rpm = 1800;
        while let Some((next, coarse)) = next_step(rpm) {
            steps.push((next, coarse));
            rpm = next;
        }
        assert_eq!(
            steps,
            vec![
                (1600, true),
                (1400, true),
                (1200, true),
                (1000, true),
                (900, false),
                (800, false),
                (700, false),
                (600, false),
                (500, false),
                (400, false),
                (300, false),
                (200, false),
            ]
        );
    }

    /// The failure this whole verdict exists for: max is accepted, nothing
    /// spins up, and the idle speed must not be recorded as the ceiling.
    #[test]
    fn fans_that_never_moved_store_nothing() {
        let mut run = Run::new(2100, false, DEFAULT_SECONDS);
        let elapsed = feed(
            &mut run,
            &(1..=30)
                .map(|i| sample_at(i, 2100, 1950))
                .collect::<Vec<_>>(),
        );
        let result = run.finish(elapsed);

        assert_eq!(result.verdict, Verdict::DidNotRespond);
        assert_eq!(result.fan_max_rpm, None);
        assert!(result.detail.contains("idle speed"));
    }

    /// ...unless they were already at max, where no rise is the expected
    /// result rather than a failed one.
    #[test]
    fn fans_already_at_max_measure_fine_without_a_rise() {
        let mut run = Run::new(3900, true, DEFAULT_SECONDS);
        let elapsed = feed(
            &mut run,
            &(1..=30)
                .map(|i| sample_at(i, 3900, 3700))
                .collect::<Vec<_>>(),
        );
        let result = run.finish(elapsed);

        assert_eq!(result.verdict, Verdict::Measured);
        assert_eq!(result.fan_max_rpm, Some(3900));
    }

    #[test]
    fn a_machine_with_no_tachometer_says_so_rather_than_reporting_zero() {
        let mut run = Run::new(0, false, DEFAULT_SECONDS);
        let elapsed = feed(
            &mut run,
            &(1..=30).map(|i| sample_at(i, 0, 0)).collect::<Vec<_>>(),
        );
        let result = run.finish(elapsed);

        assert_eq!(result.verdict, Verdict::NoReading);
        assert_eq!(result.fan_max_rpm, None);
    }

    #[test]
    fn a_reverse_reading_is_not_a_ceiling() {
        let mut run = Run::new(2000, false, DEFAULT_SECONDS);
        let mut samples = ramp();
        samples[3].is_reverse = true;
        let elapsed = feed(&mut run, &samples);
        let result = run.finish(elapsed);

        assert_eq!(result.verdict, Verdict::Reverse);
        assert_eq!(result.fan_max_rpm, None);
    }

    /// A one-fan machine is normal; the absent fan must not become a zero
    /// ceiling that something later divides by.
    #[test]
    fn a_second_fan_that_reads_nothing_is_left_unset() {
        let mut run = Run::new(2000, false, DEFAULT_SECONDS);
        let samples: Vec<Sample> = (1..=15).map(|i| sample_at(i, 3800, 0)).collect();
        let elapsed = feed(&mut run, &samples);
        let result = run.finish(elapsed);

        assert_eq!(result.fan1_max_rpm, Some(3800));
        assert_eq!(result.fan2_max_rpm, None);
    }

    #[test]
    fn a_duration_outside_the_allowed_range_is_clamped_rather_than_refused() {
        assert_eq!(Run::new(0, false, 1).limit_secs, MIN_SECONDS);
        assert_eq!(Run::new(0, false, 9999).limit_secs, MAX_SECONDS);
    }

    // The hardware half. `run` itself sleeps for at least MIN_SECONDS, so
    // what is tested here is the part that has to be right when it does
    // not finish normally: putting the fans back.

    fn fixture(tag: &str, files: &[&str]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pyren-fan-calibration-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), "0\n").unwrap();
        }
        dir
    }

    fn paths(dir: &std::path::Path) -> FanPaths {
        FanPaths {
            hwmon_dir: Some(dir.to_path_buf()),
            pwm1: dir.join("pwm1").exists().then(|| dir.join("pwm1")),
            pwm2: None,
            pwm1_enable: Some(dir.join("pwm1_enable")),
            fan1_input: Some(dir.join("fan1_input")),
            fan2_input: Some(dir.join("fan2_input")),
            cpu_temp: None,
            gpu_temp: None,
            driver_params: None,
        }
    }

    fn enable(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("pwm1_enable"))
            .unwrap()
            .trim()
            .to_string()
    }

    #[test]
    fn the_mode_the_machine_was_found_in_is_put_back() {
        let dir = fixture("restore", &["pwm1_enable", "pwm1"]);
        let p = paths(&dir);
        let caps = Capabilities::detect(&p);
        control::apply(&p, caps, FanMode::Max, 0).unwrap();

        let restore = Restore {
            paths: &p,
            caps,
            mode: FanMode::Auto,
            pwm: 128,
            done: false,
        };
        let (mode, error) = restore.finish();

        assert_eq!((mode, error), ("auto", None));
        assert_eq!(enable(&dir), "2");
    }

    /// Leaving a machine at full speed because the restore failed would be
    /// the worse failure, so an impossible mode becomes auto rather than
    /// nothing.
    #[test]
    fn a_restore_that_cannot_happen_falls_back_to_auto() {
        let dir = fixture("restore-fallback", &["pwm1_enable"]);
        let p = paths(&dir);
        let caps = Capabilities::detect(&p);
        control::apply(&p, caps, FanMode::Max, 0).unwrap();

        // Manual needs pwm1, which this machine does not have - the 8D2F
        // case, where the driver can still report mode 1.
        let restore = Restore {
            paths: &p,
            caps,
            mode: FanMode::Manual,
            pwm: 200,
            done: false,
        };
        let (mode, error) = restore.finish();

        assert_eq!(mode, "auto");
        assert!(error.expect("should say what went wrong").contains("pwm1"));
        assert_eq!(enable(&dir), "2");
    }

    /// The reason this is a guard and not a line at the end of `run`.
    #[test]
    fn fans_are_put_back_even_when_the_run_never_finishes() {
        let dir = fixture("restore-drop", &["pwm1_enable", "pwm1"]);
        let p = paths(&dir);
        let caps = Capabilities::detect(&p);
        control::apply(&p, caps, FanMode::Max, 0).unwrap();

        drop(Restore {
            paths: &p,
            caps,
            mode: FanMode::Auto,
            pwm: 128,
            done: false,
        });

        assert_eq!(
            enable(&dir),
            "2",
            "a dropped run must not leave the fans at max"
        );
    }
}
