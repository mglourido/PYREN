//! Do the two fans take **different** speeds?
//!
//! `pwm2` existing says the driver keeps a setpoint per fan. It does not say
//! the embedded controller runs them apart: the driver sends both speeds in
//! one firmware call, and a board is free to apply one of them to both
//! fans. A GPU curve offered on such a machine would be a second editor
//! that drives nothing.
//!
//! The only thing that settles it is the same thing that settles
//! [`crate::speed_probe`]: command it and watch the tachometers. One fan is
//! told slow and the other fast, and then the two are swapped. The swap is
//! the proof - a pair of fans that merely differ, because one is always the
//! faster of the two, passes the first half and cannot pass the second.
//!
//! Measured on board `8D2F`: `pwm1` 147 / `pwm2` 216 settled at 3000 / 4500
//! rpm in ten seconds, and the reverse at 4500 / 3000 in six.
//!
//! Kept in two halves like the other measurements: [`Run`] decides what a
//! trace means and can be tested without an HP laptop, while [`run`] is the
//! part that owns the hardware.

use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::calibration::Restore;
use crate::control::{self, Capabilities, FanMode};
use crate::{observed_mode, parse_hwmon_rpm, read_raw_rpm, FanPaths};

/// How long each half may take when the fans never come apart. Twice what
/// the slower half took on the board above; the budget for proving a
/// negative, since a machine that obeys ends each half early.
pub const HALF_SECONDS: u64 = 20;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// The two speeds. Far enough apart that the gap is unmistakable - about
/// 1500 rpm on a 5200 rpm fan - and neither of them full speed, which is
/// louder than the question needs.
const SLOW_PWM: u8 = 140;
const FAST_PWM: u8 = 215;

/// How far ahead the fan told fast must be of the one told slow. A third of
/// the gap asked for, and ten times tachometer jitter.
const MIN_GAP_RPM: i64 = 500;

/// What a probe concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Verdict {
    /// Each fan went to its own speed, both ways round.
    Separate,
    /// Both orders were accepted and the fans ran together.
    Together,
    /// A fan reported no speed, so there was nothing to compare.
    NoReading,
    /// No `pwm2`, or no speed control at all. Proves nothing either way.
    NoChannel,
}

/// What this module's answer is *stored* as: the conclusive half of
/// [`Verdict`] plus "nobody has asked yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SplitControl {
    /// No probe has run. A separate GPU fan is offered wherever there is a
    /// `pwm2`, on the same reasoning as [`crate::SpeedControl::Untested`].
    #[default]
    Untested,
    Separate,
    Together,
}

impl SplitControl {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untested => "untested",
            Self::Separate => "separate",
            Self::Together => "together",
        }
    }

    /// Whether two different orders are known to come out as one here.
    pub fn is_together(self) -> bool {
        matches!(self, Self::Together)
    }
}

impl From<Verdict> for Option<SplitControl> {
    fn from(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Separate => Some(SplitControl::Separate),
            Verdict::Together => Some(SplitControl::Together),
            Verdict::NoReading | Verdict::NoChannel => None,
        }
    }
}

/// One reading of both tachometers.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub at_secs: u64,
    pub fan1_rpm: i64,
    pub fan2_rpm: i64,
}

/// One half of the run: which fan was told fast, and what came of it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Half {
    pub fan1_pwm: u8,
    pub fan2_pwm: u8,
    /// The last reading, which is the one the verdict is made from.
    pub fan1_rpm: i64,
    pub fan2_rpm: i64,
    /// How far the fan told fast ended ahead of the other. Negative when it
    /// ended behind.
    pub gap_rpm: i64,
    pub seconds: u64,
    pub samples: Vec<Sample>,
}

/// What a run found, and enough of how it found it to argue with.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitProbe {
    pub verdict: Verdict,
    pub halves: Vec<Half>,
    /// The mode put back afterwards, and what went wrong if anything did.
    pub restored_mode: &'static str,
    pub restore_error: Option<String>,
    /// A sentence for a human, saying what the verdict means here.
    pub detail: String,
}

/// The decision half for one half of the run.
#[derive(Debug, Clone)]
pub struct Run {
    fan1_fast: bool,
    limit_secs: u64,
    samples: Vec<Sample>,
}

impl Run {
    pub fn new(fan1_fast: bool, limit_secs: u64) -> Self {
        Self {
            fan1_fast,
            limit_secs,
            samples: Vec::new(),
        }
    }

    pub fn push(&mut self, sample: Sample) {
        self.samples.push(sample);
    }

    /// How far the fan told fast is ahead, by the latest reading. The
    /// latest and not the best: fans that crossed on their way to the same
    /// speed were apart for a moment and are not apart.
    pub fn gap(&self) -> i64 {
        self.samples.last().map_or(0, |last| {
            if self.fan1_fast {
                last.fan1_rpm - last.fan2_rpm
            } else {
                last.fan2_rpm - last.fan1_rpm
            }
        })
    }

    /// A machine that obeys shows it as soon as the gap opens, and there is
    /// no reason to hold the fans there for the rest of the budget.
    pub fn is_done(&self, elapsed_secs: u64) -> bool {
        self.gap() >= MIN_GAP_RPM || elapsed_secs >= self.limit_secs
    }

    pub fn finish(self, elapsed_secs: u64) -> Half {
        let gap = self.gap();
        let last = self.samples.last().copied();
        let (fan1_pwm, fan2_pwm) = orders(self.fan1_fast);
        Half {
            fan1_pwm,
            fan2_pwm,
            fan1_rpm: last.map_or(0, |s| s.fan1_rpm),
            fan2_rpm: last.map_or(0, |s| s.fan2_rpm),
            gap_rpm: gap,
            seconds: elapsed_secs,
            samples: self.samples,
        }
    }
}

fn orders(fan1_fast: bool) -> (u8, u8) {
    if fan1_fast {
        (FAST_PWM, SLOW_PWM)
    } else {
        (SLOW_PWM, FAST_PWM)
    }
}

/// Turns the two halves into a verdict.
pub fn conclude(halves: Vec<Half>) -> SplitProbe {
    let read = |fan: fn(&Sample) -> i64| {
        halves
            .iter()
            .flat_map(|half| half.samples.iter())
            .any(|sample| fan(sample) > 0)
    };
    let both_read = read(|s| s.fan1_rpm) && read(|s| s.fan2_rpm);
    let apart = halves.len() == 2 && halves.iter().all(|half| half.gap_rpm >= MIN_GAP_RPM);

    let (verdict, detail) = if !both_read {
        (
            Verdict::NoReading,
            "a fan reported no speed during the run, so there was nothing to compare".to_string(),
        )
    } else if apart {
        (
            Verdict::Separate,
            format!(
                "fan 1 / fan 2 ran at {} / {} rpm and then at {} / {} rpm, each following \
                 its own order",
                halves[0].fan1_rpm, halves[0].fan2_rpm, halves[1].fan1_rpm, halves[1].fan2_rpm
            ),
        )
    } else {
        let last = halves.last();
        (
            Verdict::Together,
            format!(
                "two different speeds were accepted and the fans ran together ({} / {} rpm), \
                 so one curve drives both here",
                last.map_or(0, |half| half.fan1_rpm),
                last.map_or(0, |half| half.fan2_rpm)
            ),
        )
    };

    SplitProbe {
        verdict,
        halves,
        // Filled in by `run`, which is the half that owns the hardware.
        restored_mode: "auto",
        restore_error: None,
        detail,
    }
}

/// A probe that never touched the hardware.
fn inconclusive(verdict: Verdict, detail: &str) -> SplitProbe {
    SplitProbe {
        verdict,
        halves: Vec::new(),
        restored_mode: "none",
        restore_error: None,
        detail: detail.to_string(),
    }
}

/// Runs a probe against the hardware. Blocks for up to twice
/// [`HALF_SECONDS`].
///
/// The caller is responsible for keeping the control loop off the fans while
/// this runs; see `State::calibrating`, which this borrows.
pub(crate) fn run(
    paths: &FanPaths,
    caps: Capabilities,
    abort: crate::calibration::Abort,
) -> Result<SplitProbe, control::ControlError> {
    let has_pwm2 = paths.pwm2.as_deref().is_some_and(|path| path.exists());
    if !caps.supports(FanMode::Manual) || !has_pwm2 {
        return Ok(inconclusive(
            Verdict::NoChannel,
            "this driver exposes no pwm2, so there is no second speed to command",
        ));
    }

    if let Some(e) = abort() {
        return Err(e);
    }
    let before_mode = observed_mode(paths).unwrap_or(FanMode::Auto);
    let before_pwm = control::read_pwm(paths).unwrap_or(crate::curve::MIN_COMMANDED_PWM);

    let mut restore = None;
    let mut halves = Vec::with_capacity(2);
    for fan1_fast in [false, true] {
        let (fan1_pwm, fan2_pwm) = orders(fan1_fast);
        control::apply_pair(paths, caps, FanMode::Manual, fan1_pwm, fan2_pwm)?;
        // Armed after the first write that landed: there is nothing to put
        // back before it.
        restore.get_or_insert_with(|| Restore::new(paths, caps, before_mode, before_pwm));

        let mut measurement = Run::new(fan1_fast, HALF_SECONDS);
        let started = Instant::now();
        let elapsed = loop {
            sleep(SAMPLE_INTERVAL);
            // The `restore` guard puts the fans back on the way out.
            if let Some(e) = abort() {
                return Err(e);
            }
            let elapsed = started.elapsed().as_secs();
            measurement.push(sample(paths, elapsed));
            if measurement.is_done(elapsed) {
                break elapsed;
            }
        };
        let half = measurement.finish(elapsed);
        let apart = half.gap_rpm >= MIN_GAP_RPM;
        halves.push(half);
        if !apart {
            // The swap proves nothing about a pair that never came apart.
            break;
        }
    }

    let mut probe = conclude(halves);
    if let Some(restore) = restore {
        let (restored_mode, restore_error) = restore.finish();
        probe.restored_mode = restored_mode;
        probe.restore_error = restore_error;
    }
    Ok(probe)
}

fn sample(paths: &FanPaths, at_secs: u64) -> Sample {
    let (fan1_rpm, _) = parse_hwmon_rpm(read_raw_rpm(paths.fan1_input.as_deref()));
    let (fan2_rpm, _) = parse_hwmon_rpm(read_raw_rpm(paths.fan2_input.as_deref()));
    Sample {
        at_secs,
        fan1_rpm,
        fan2_rpm,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn half(fan1_fast: bool, readings: &[(i64, i64)]) -> Half {
        let mut run = Run::new(fan1_fast, HALF_SECONDS);
        let mut elapsed = 0;
        for (i, &(fan1_rpm, fan2_rpm)) in readings.iter().enumerate() {
            elapsed = i as u64 + 1;
            run.push(Sample {
                at_secs: elapsed,
                fan1_rpm,
                fan2_rpm,
            });
            if run.is_done(elapsed) {
                break;
            }
        }
        run.finish(elapsed)
    }

    /// The trace recorded on board 8D2F, both ways round.
    #[test]
    fn fans_that_follow_their_own_order_are_recognised() {
        let first = half(false, &[(2600, 2400), (3000, 3000), (3000, 3600)]);
        assert_eq!(first.seconds, 3, "the half ends when the gap opens");
        let second = half(true, &[(3500, 4100), (4100, 3500), (4500, 3000)]);

        let probe = conclude(vec![first, second]);

        assert_eq!(probe.verdict, Verdict::Separate);
        assert_eq!(
            Option::<SplitControl>::from(probe.verdict),
            Some(SplitControl::Separate)
        );
    }

    /// A board that applies one of the two speeds to both fans.
    #[test]
    fn fans_that_run_together_are_caught() {
        let readings = vec![(3000, 3050); HALF_SECONDS as usize];
        let first = half(false, &readings);
        assert_eq!(first.seconds, HALF_SECONDS, "a negative takes the budget");

        let probe = conclude(vec![first]);

        assert_eq!(probe.verdict, Verdict::Together);
        assert!(probe.detail.contains("one curve"), "{}", probe.detail);
    }

    /// One fan always faster than the other passes the first half on its
    /// build alone. The swap is what tells it from a fan that obeys.
    #[test]
    fn a_fan_that_is_merely_faster_does_not_pass_the_swap() {
        let first = half(false, &[(3000, 3600)]);
        let second = half(true, &vec![(3000, 3600); HALF_SECONDS as usize]);

        assert_eq!(conclude(vec![first, second]).verdict, Verdict::Together);
    }

    /// Fans crossing on their way to one shared speed are apart for a
    /// moment. Only where they end counts.
    #[test]
    fn the_gap_is_read_where_the_fans_end() {
        let mut run = Run::new(true, HALF_SECONDS);
        for (at_secs, fan1_rpm, fan2_rpm) in [(1, 3400, 3000), (2, 3100, 3050)] {
            run.push(Sample {
                at_secs,
                fan1_rpm,
                fan2_rpm,
            });
        }
        assert_eq!(run.gap(), 50);
    }

    /// A fan with no tachometer settles nothing, and must not be stored as
    /// a board that cannot do it.
    #[test]
    fn a_silent_tachometer_is_not_an_answer() {
        let probe = conclude(vec![half(false, &vec![(3000, 0); HALF_SECONDS as usize])]);

        assert_eq!(probe.verdict, Verdict::NoReading);
        assert_eq!(Option::<SplitControl>::from(probe.verdict), None);
    }
}
