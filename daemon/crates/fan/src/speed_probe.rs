//! Does writing `pwm1` actually **move the fans**?
//!
//! [`crate::control::Capabilities`] answers a narrower question than its
//! name suggests: it checks that the `pwm1` file exists. On board `8D2F`
//! with the patched driver that is true and the fans still run on the
//! firmware's own curve — the EC never honours the value written. Worse,
//! `pwm1` there reads back the *measured* speed scaled to 0-255 rather than
//! the setpoint, so every cheap test passes: write 57, read 57, conclude the
//! channel works. `dev/FINDINGS.md` §"`pwm1` exists on 8D2F and is ignored"
//! has the traces.
//!
//! The only thing that settles it is commanding a speed the fans are *not*
//! at and watching the tachometer. That costs a few seconds of fan noise,
//! which is why this is a deliberate probe rather than part of discovery:
//! nothing here runs unless someone asked for it.
//!
//! Kept in two halves for the same reason [`crate::calibration`] is: [`Run`]
//! decides what a trace means and can be tested without an HP laptop, while
//! [`run`] is the part that owns the hardware.

use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::calibration::Restore;
use crate::control::{self, Capabilities, FanMode};
use crate::{observed_mode, parse_hwmon_rpm, read_raw_rpm, FanPaths};

/// How long a probe may take when the fans never respond. A machine that
/// honours the write shows it in the first few seconds, so this is the
/// budget for proving a *negative*.
pub const DEFAULT_SECONDS: u64 = 15;

/// Shortest a run may be. Below this a fan that starts spinning up slowly
/// can look like one that is not spinning up at all.
pub const MIN_SECONDS: u64 = 8;

/// Longest a caller may ask for.
pub const MAX_SECONDS: u64 = 60;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// How far the fans must move toward the commanded speed before the write
/// counts as honoured.
///
/// The observed ramp on a machine that obeys is ~600 rpm every two seconds,
/// so this is under a second of it — and far enough above tachometer jitter
/// (~50 rpm, see [`crate::calibration`]) that fans sitting still cannot
/// drift into it.
const MIN_RESPONSE_RPM: i64 = 300;

/// What to command when the fans are idle, and when they are already fast.
///
/// Neither is `max`: the question is whether the *channel* works, and asking
/// it at full speed is louder than it has to be. Aiming down matters as much
/// as aiming up — a probe that only ever aims high would call a machine
/// under load "honoured" for a rise the heat produced anyway.
const AIM_HIGH_PWM: u8 = 200;
const AIM_LOW_PWM: u8 = 60;

/// Above this share of the measured ceiling the fans count as already fast,
/// and the probe aims down instead of up.
const ALREADY_FAST_NUM: i64 = 3;
const ALREADY_FAST_DEN: i64 = 5;

/// What a probe concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Verdict {
    /// The fans followed the commanded speed. `pwm1` is a real setpoint.
    Honoured,
    /// The write was accepted and the fans did not move. This is board
    /// `8D2F`, and the reason [`crate::FanConfig::speed_control`] exists.
    Ignored,
    /// No tachometer reading at all, so there was nothing to watch. Proves
    /// nothing either way.
    NoReading,
    /// There is no `pwm1` to write to. Proves nothing either way.
    NoChannel,
}

impl Verdict {
    /// Whether this run is worth remembering. The two inconclusive verdicts
    /// must not overwrite a previous run that did settle the question.
    pub fn is_conclusive(self) -> bool {
        matches!(self, Self::Honoured | Self::Ignored)
    }
}

/// What this module's answer is *stored* as, which is the conclusive half of
/// [`Verdict`] plus "nobody has asked yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpeedControl {
    /// No probe has run. Speed control is offered, because a machine whose
    /// driver exposes `pwm1` usually does honour it and refusing on
    /// suspicion would be worse than the occasional wasted slider.
    #[default]
    Untested,
    Honoured,
    Ignored,
}

impl SpeedControl {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untested => "untested",
            Self::Honoured => "honoured",
            Self::Ignored => "ignored",
        }
    }

    /// Whether a commanded speed is known to do nothing here.
    pub fn is_ignored(self) -> bool {
        matches!(self, Self::Ignored)
    }
}

impl From<Verdict> for Option<SpeedControl> {
    fn from(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Honoured => Some(SpeedControl::Honoured),
            Verdict::Ignored => Some(SpeedControl::Ignored),
            Verdict::NoReading | Verdict::NoChannel => None,
        }
    }
}

/// One reading. The trace is the evidence for the verdict, and on a machine
/// that is refused speed control afterwards it is the only thing that makes
/// the refusal arguable rather than an assertion.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub at_secs: u64,
    pub rpm: i64,
    pub is_reverse: bool,
}

/// What a run found, and enough of how it found it to argue with.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedProbe {
    pub verdict: Verdict,
    /// The reading before anything was written.
    pub baseline_rpm: i64,
    /// Whether the probe asked for more speed or less.
    pub aiming_up: bool,
    /// The 0-255 value that was commanded.
    pub target_pwm: u8,
    /// What that implies in rpm, where a calibration has measured the
    /// ceiling. `None` is not a failure — the verdict never needed it, it
    /// only makes the detail sentence concrete.
    pub expected_rpm: Option<i64>,
    /// The reading furthest in the commanded direction.
    pub reached_rpm: i64,
    /// How far the fans actually moved that way.
    pub response_rpm: i64,
    pub seconds: u64,
    pub samples: Vec<Sample>,
    /// The mode put back afterwards, and what went wrong if anything did.
    pub restored_mode: &'static str,
    pub restore_error: Option<String>,
    /// A sentence for a human, saying what the verdict means here.
    pub detail: String,
}

/// The decision half, separated from the hardware half so that what a trace
/// is allowed to conclude can be tested without an HP laptop.
#[derive(Debug, Clone)]
pub struct Run {
    baseline: i64,
    aiming_up: bool,
    target_pwm: u8,
    expected_rpm: Option<i64>,
    limit_secs: u64,
    samples: Vec<Sample>,
    /// The reading furthest in the commanded direction, seeded with the
    /// baseline so that "no movement" is a response of zero rather than a
    /// special case.
    best: i64,
    saw_reading: bool,
}

impl Run {
    pub fn new(
        baseline: i64,
        aiming_up: bool,
        target_pwm: u8,
        expected_rpm: Option<i64>,
        limit_secs: u64,
    ) -> Self {
        Self {
            baseline,
            aiming_up,
            target_pwm,
            expected_rpm,
            limit_secs: limit_secs.clamp(MIN_SECONDS, MAX_SECONDS),
            samples: Vec::new(),
            best: baseline,
            saw_reading: baseline > 0,
        }
    }

    pub fn push(&mut self, sample: Sample) {
        self.saw_reading |= sample.rpm > 0;
        self.best = if self.aiming_up {
            self.best.max(sample.rpm)
        } else {
            // A stopped fan reads 0, which is genuinely the slowest it goes,
            // so there is nothing to exclude here: aiming down and reaching
            // zero is the write being honoured.
            self.best.min(sample.rpm)
        };
        self.samples.push(sample);
    }

    /// How far the fans moved the way they were told to. Never negative:
    /// movement *against* the command is not evidence of anything, since
    /// the firmware curve is still reacting to temperature underneath.
    pub fn response(&self) -> i64 {
        let moved =
            if self.aiming_up { self.best - self.baseline } else { self.baseline - self.best };
        moved.max(0)
    }

    fn responded(&self) -> bool {
        self.response() >= MIN_RESPONSE_RPM
    }

    /// Whether the run has learned everything it is going to.
    ///
    /// A machine that obeys proves it as soon as the fans move, and there is
    /// no reason to keep them there for the rest of the budget — the full
    /// duration is only ever spent proving a negative.
    pub fn is_done(&self, elapsed_secs: u64) -> bool {
        self.responded() || elapsed_secs >= self.limit_secs
    }

    /// Turns the trace into a verdict. `elapsed_secs` is how long the run
    /// actually took, which is not the limit when the fans answered early.
    pub fn finish(self, elapsed_secs: u64) -> SpeedProbe {
        let response = self.response();
        let direction = if self.aiming_up { "up" } else { "down" };

        let (verdict, detail) = if !self.saw_reading {
            (
                Verdict::NoReading,
                "no fan reported a speed during the run, so there was nothing to watch \
                 and this settles nothing"
                    .to_string(),
            )
        } else if response >= MIN_RESPONSE_RPM {
            (
                Verdict::Honoured,
                format!(
                    "pwm1 = {} moved the fans {direction} from {} to {} rpm ({response} rpm) \
                     in {elapsed_secs}s, so a commanded speed reaches this hardware",
                    self.target_pwm, self.baseline, self.best
                ),
            )
        } else {
            (
                Verdict::Ignored,
                format!(
                    "pwm1 = {} was accepted and changed nothing: the fans sat at {} rpm for \
                     {elapsed_secs}s (furthest {direction} reading {} rpm, {response} rpm of \
                     movement). The driver takes the value and the embedded controller keeps \
                     the fans on its own curve, so manual and curve modes cannot work here — \
                     auto and max still do, because they are a different firmware call",
                    self.target_pwm, self.baseline, self.best
                ),
            )
        };

        SpeedProbe {
            verdict,
            baseline_rpm: self.baseline,
            aiming_up: self.aiming_up,
            target_pwm: self.target_pwm,
            expected_rpm: self.expected_rpm,
            reached_rpm: self.best,
            response_rpm: response,
            seconds: elapsed_secs,
            samples: self.samples,
            // Filled in by `run`, which is the half that owns the hardware.
            restored_mode: "auto",
            restore_error: None,
            detail,
        }
    }
}

/// A probe that never touched the hardware, for the two cases where there is
/// nothing to ask.
fn inconclusive(verdict: Verdict, detail: &str) -> SpeedProbe {
    SpeedProbe {
        verdict,
        baseline_rpm: 0,
        aiming_up: true,
        target_pwm: 0,
        expected_rpm: None,
        reached_rpm: 0,
        response_rpm: 0,
        seconds: 0,
        samples: Vec::new(),
        restored_mode: "none",
        restore_error: None,
        detail: detail.to_string(),
    }
}

/// Which way to aim, and at what.
///
/// Aiming down when the fans are already fast is what stops a machine under
/// load from passing on a rise its own temperature produced.
fn choose_target(baseline: i64, fan_max_rpm: Option<i64>) -> (bool, u8) {
    let already_fast = fan_max_rpm
        .filter(|max| *max > 0)
        .is_some_and(|max| baseline > max * ALREADY_FAST_NUM / ALREADY_FAST_DEN);
    if already_fast {
        (false, AIM_LOW_PWM)
    } else {
        (true, AIM_HIGH_PWM)
    }
}

/// Runs a probe against the hardware. Blocks for up to `seconds`.
///
/// The caller is responsible for keeping the control loop off the fans while
/// this runs; see `State::calibrating`, which this borrows.
pub(crate) fn run(
    paths: &FanPaths,
    caps: Capabilities,
    fan_max_rpm: Option<i64>,
    seconds: u64,
) -> Result<SpeedProbe, control::ControlError> {
    if !caps.supports(FanMode::Manual) {
        return Ok(inconclusive(
            Verdict::NoChannel,
            "this driver exposes no pwm1, so there is no commanded speed to test",
        ));
    }

    let limit = seconds.clamp(MIN_SECONDS, MAX_SECONDS);
    let before_mode = observed_mode(paths).unwrap_or(FanMode::Auto);
    let before_pwm = control::read_pwm(paths).unwrap_or(crate::curve::MIN_COMMANDED_PWM);
    let baseline = sample(paths, 0).rpm;

    let (aiming_up, target_pwm) = choose_target(baseline, fan_max_rpm);
    let expected_rpm = fan_max_rpm.map(|max| target_pwm as i64 * max / 255);

    control::apply(paths, caps, FanMode::Manual, target_pwm)?;
    let restore = Restore::new(paths, caps, before_mode, before_pwm);

    let mut measurement = Run::new(baseline, aiming_up, target_pwm, expected_rpm, limit);
    let started = Instant::now();
    let elapsed = loop {
        sleep(SAMPLE_INTERVAL);
        let elapsed = started.elapsed().as_secs();
        measurement.push(sample(paths, elapsed));
        if measurement.is_done(elapsed) {
            break elapsed;
        }
    };

    let mut probe = measurement.finish(elapsed);
    let (restored_mode, restore_error) = restore.finish();
    probe.restored_mode = restored_mode;
    probe.restore_error = restore_error;
    Ok(probe)
}

fn sample(paths: &FanPaths, at_secs: u64) -> Sample {
    let (fan1_rpm, rev1) = parse_hwmon_rpm(read_raw_rpm(paths.fan1_input.as_deref()));
    let (fan2_rpm, rev2) = parse_hwmon_rpm(read_raw_rpm(paths.fan2_input.as_deref()));
    Sample { at_secs, rpm: fan1_rpm.max(fan2_rpm), is_reverse: rev1 || rev2 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(run: &mut Run, readings: &[i64]) -> u64 {
        for (i, rpm) in readings.iter().enumerate() {
            let at = i as u64 + 1;
            run.push(Sample { at_secs: at, rpm: *rpm, is_reverse: false });
            if run.is_done(at) {
                return at;
            }
        }
        readings.len() as u64
    }

    /// The trace actually recorded on board 8D2F: manual pwm 200 commanded,
    /// and the fans go on doing what the firmware curve says.
    #[test]
    fn a_machine_that_takes_the_value_and_ignores_it_is_caught() {
        let mut run = Run::new(1200, true, 200, Some(4156), DEFAULT_SECONDS);
        let elapsed = feed(&mut run, &[900, 700, 300, 1300, 1100, 900, 1200, 1000]);
        let probe = run.finish(elapsed);

        assert_eq!(probe.verdict, Verdict::Ignored);
        assert!(probe.response_rpm < MIN_RESPONSE_RPM, "1300 from 1200 is not a response");
        assert!(probe.detail.contains("auto and max still do"), "{}", probe.detail);
    }

    /// A machine that obeys says so in the first few seconds, and the run
    /// stops there rather than holding the fans up for the full budget.
    #[test]
    fn a_machine_that_obeys_is_recognised_and_ends_early() {
        let mut run = Run::new(1200, true, 200, Some(4156), DEFAULT_SECONDS);
        let elapsed = feed(&mut run, &[2100, 3300, 4400, 4200, 4100, 4150, 4100, 4100]);
        assert_eq!(elapsed, 1, "2100 from 1200 is already 900 rpm of response");

        let probe = run.finish(elapsed);
        assert_eq!(probe.verdict, Verdict::Honoured);
        assert_eq!(probe.reached_rpm, 2100);
    }

    /// Aiming down is the other half, and it has to count a *fall* as the
    /// response rather than looking for a rise that will never come.
    #[test]
    fn aiming_down_counts_a_fall_as_the_response() {
        let mut run = Run::new(4800, false, AIM_LOW_PWM, Some(1247), DEFAULT_SECONDS);
        let elapsed = feed(&mut run, &[4000, 2600, 1300]);
        let probe = run.finish(elapsed);

        assert_eq!(probe.verdict, Verdict::Honoured);
        assert!(probe.response_rpm >= MIN_RESPONSE_RPM);
    }

    /// Movement *against* the command is the firmware curve reacting to
    /// temperature underneath, not evidence that the write landed.
    #[test]
    fn movement_the_wrong_way_is_not_a_response() {
        let mut run = Run::new(1000, true, 200, None, DEFAULT_SECONDS);
        let elapsed = feed(&mut run, &[600, 400, 200, 300, 500, 700, 800, 900]);
        let probe = run.finish(elapsed);

        assert_eq!(probe.verdict, Verdict::Ignored);
        assert_eq!(probe.response_rpm, 0, "a fall cannot answer a request to speed up");
    }

    /// A machine whose tachometer says nothing settles nothing, and must not
    /// be recorded as a machine that refused.
    #[test]
    fn no_tachometer_reading_is_inconclusive_rather_than_a_refusal() {
        let mut run = Run::new(0, true, 200, None, DEFAULT_SECONDS);
        let elapsed = feed(&mut run, &[0, 0, 0, 0, 0, 0, 0, 0]);
        let probe = run.finish(elapsed);

        assert_eq!(probe.verdict, Verdict::NoReading);
        assert!(!probe.verdict.is_conclusive());
        assert_eq!(Option::<SpeedControl>::from(probe.verdict), None);
    }

    /// The two inconclusive verdicts must never overwrite a stored answer.
    #[test]
    fn only_a_conclusive_verdict_becomes_a_stored_setting() {
        assert_eq!(Option::<SpeedControl>::from(Verdict::Honoured), Some(SpeedControl::Honoured));
        assert_eq!(Option::<SpeedControl>::from(Verdict::Ignored), Some(SpeedControl::Ignored));
        assert_eq!(Option::<SpeedControl>::from(Verdict::NoChannel), None);
        assert_eq!(Option::<SpeedControl>::from(Verdict::NoReading), None);
    }

    /// Idle fans get asked to speed up; fans already near the ceiling get
    /// asked to slow down, so a hot machine cannot pass on its own rise.
    #[test]
    fn the_direction_follows_where_the_fans_already_are() {
        assert_eq!(choose_target(1200, Some(5300)), (true, AIM_HIGH_PWM));
        assert_eq!(choose_target(4800, Some(5300)), (false, AIM_LOW_PWM));
        // With no calibration there is no "already fast" to know about.
        assert_eq!(choose_target(4800, None), (true, AIM_HIGH_PWM));
    }

    #[test]
    fn a_run_never_ends_before_the_floor_unless_the_fans_answered() {
        let run = Run::new(1200, true, 200, None, 1);
        assert!(!run.is_done(MIN_SECONDS - 1), "the limit is clamped up to the floor");
        assert!(run.is_done(MIN_SECONDS));
    }
}
