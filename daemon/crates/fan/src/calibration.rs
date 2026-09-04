//! Calibration: measuring what "full speed" actually is on this machine.
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

/// Puts back the mode the machine was found in, whatever happens to the
/// run in between - including a panic, which is why this is a guard and
/// not a line at the end.
struct Restore<'a> {
    paths: &'a FanPaths,
    caps: Capabilities,
    mode: FanMode,
    pwm: u8,
    done: bool,
}

impl Restore<'_> {
    /// Restores explicitly, so the outcome can be reported rather than
    /// swallowed. Falling back to `auto` is deliberate: it is the mode
    /// where the firmware owns the fans, and leaving a machine at full
    /// speed because the restore failed would be the worse failure.
    fn finish(mut self) -> (&'static str, Option<String>) {
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

/// Runs a calibration against the hardware. Blocks for up to `seconds`.
///
/// The caller is responsible for keeping the control loop off the fans
/// while this runs; see `State::calibrating`.
pub(crate) fn run(
    paths: &FanPaths,
    caps: Capabilities,
    seconds: u64,
) -> Result<Calibration, control::ControlError> {
    let limit = seconds.clamp(MIN_SECONDS, MAX_SECONDS);
    let before_mode = observed_mode(paths).unwrap_or(FanMode::Auto);
    let before_pwm = control::read_pwm(paths).unwrap_or(crate::curve::MIN_COMMANDED_PWM);
    let baseline = sample(paths, 0).faster();

    control::apply(paths, caps, FanMode::Max, 0)?;
    let restore = Restore { paths, caps, mode: before_mode, pwm: before_pwm, done: false };

    let mut measurement = Run::new(baseline, before_mode == FanMode::Max, limit);
    let started = Instant::now();
    let elapsed = loop {
        sleep(SAMPLE_INTERVAL);
        let elapsed = started.elapsed().as_secs();
        measurement.push(sample(paths, elapsed));
        if measurement.is_done(elapsed) {
            break elapsed;
        }
    };

    let mut calibration = measurement.finish(elapsed);
    let (restored_mode, restore_error) = restore.finish();
    calibration.restored_mode = restored_mode;
    calibration.restore_error = restore_error;
    Ok(calibration)
}

fn sample(paths: &FanPaths, at_secs: u64) -> Sample {
    let (fan1_rpm, rev1) = parse_hwmon_rpm(read_raw_rpm(paths.fan1_input.as_deref()));
    let (fan2_rpm, rev2) = parse_hwmon_rpm(read_raw_rpm(paths.fan2_input.as_deref()));
    Sample { at_secs, fan1_rpm, fan2_rpm, is_reverse: rev1 || rev2 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_at(at_secs: u64, fan1: i64, fan2: i64) -> Sample {
        Sample { at_secs, fan1_rpm: fan1, fan2_rpm: fan2, is_reverse: false }
    }

    /// The ramp actually measured on board 8D2F (`dev/FINDINGS.md`), one
    /// sample a second rather than every two.
    fn ramp() -> Vec<Sample> {
        let readings =
            [2400, 2700, 3000, 3300, 3600, 3900, 3910, 3900, 3915, 3905, 3900, 3910, 3905];
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

        assert!(elapsed < DEFAULT_SECONDS, "should not have run the full {DEFAULT_SECONDS}s");
        assert!(run.finish(elapsed).settled);
    }

    /// A fan that ramps in steps is flat between them; believing the first
    /// plateau would record a ceiling the machine goes well past.
    #[test]
    fn a_plateau_before_the_minimum_duration_is_not_settled() {
        let mut run = Run::new(2000, false, DEFAULT_SECONDS);
        let mut samples: Vec<Sample> =
            (1..=8).map(|i| sample_at(i, 3000, 2800)).collect();
        samples.extend((9..=20).map(|i| sample_at(i, 4200, 4000)));
        let elapsed = feed(&mut run, &samples);
        let result = run.finish(elapsed);

        assert!(elapsed >= MIN_SECONDS);
        assert_eq!(result.fan_max_rpm, Some(4200), "the second step must be seen");
    }

    /// The failure this whole verdict exists for: max is accepted, nothing
    /// spins up, and the idle speed must not be recorded as the ceiling.
    #[test]
    fn fans_that_never_moved_store_nothing() {
        let mut run = Run::new(2100, false, DEFAULT_SECONDS);
        let elapsed = feed(&mut run, &(1..=30).map(|i| sample_at(i, 2100, 1950)).collect::<Vec<_>>());
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
        let elapsed = feed(&mut run, &(1..=30).map(|i| sample_at(i, 3900, 3700)).collect::<Vec<_>>());
        let result = run.finish(elapsed);

        assert_eq!(result.verdict, Verdict::Measured);
        assert_eq!(result.fan_max_rpm, Some(3900));
    }

    #[test]
    fn a_machine_with_no_tachometer_says_so_rather_than_reporting_zero() {
        let mut run = Run::new(0, false, DEFAULT_SECONDS);
        let elapsed = feed(&mut run, &(1..=30).map(|i| sample_at(i, 0, 0)).collect::<Vec<_>>());
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
        let dir = std::env::temp_dir()
            .join(format!("pyren-fan-calibration-{tag}-{}", std::process::id()));
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
            pwm1_enable: Some(dir.join("pwm1_enable")),
            fan1_input: Some(dir.join("fan1_input")),
            fan2_input: Some(dir.join("fan2_input")),
            cpu_temp: None,
            gpu_temp: None,
        }
    }

    fn enable(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("pwm1_enable")).unwrap().trim().to_string()
    }

    #[test]
    fn the_mode_the_machine_was_found_in_is_put_back() {
        let dir = fixture("restore", &["pwm1_enable", "pwm1"]);
        let p = paths(&dir);
        let caps = Capabilities::detect(&p);
        control::apply(&p, caps, FanMode::Max, 0).unwrap();

        let restore =
            Restore { paths: &p, caps, mode: FanMode::Auto, pwm: 128, done: false };
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
        let restore =
            Restore { paths: &p, caps, mode: FanMode::Manual, pwm: 200, done: false };
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

        drop(Restore { paths: &p, caps, mode: FanMode::Auto, pwm: 128, done: false });

        assert_eq!(enable(&dir), "2", "a dropped run must not leave the fans at max");
    }
}
