//! The arithmetic behind fan control: turning a temperature into a PWM
//! value, and deciding whether that value is worth writing.
//!
//! Ported from `calculate_target_pwm` and the `serve` loop's hysteresis
//! block in the Python original (`docs/04-fan-control-logic.md` in
//! `omen-fan-control`). Kept free of file I/O and clocks so it can be
//! tested exhaustively - this is the part that decides how loud the
//! machine is, and it is the part a port is most likely to get subtly
//! wrong.

use serde::{Deserialize, Serialize};

/// One point of the temperature → speed curve, in the same shape the
/// frontend already uses (`CurvePoint` in `hardware.svelte.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    #[serde(rename = "tempC")]
    pub temp_c: f64,
    pub percent: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Interpolation {
    /// Linear between the two bracketing points. Matches the frontend's
    /// `curveValueAt`, so the graph the user drew is the curve they get.
    #[default]
    Smooth,
    /// Hold the lower point's percentage until the next one is reached.
    Discrete,
}

/// `pwm1 = 0` does **not** mean "fans off" - the driver reads it as
/// `HP_FAN_SPEED_AUTOMATIC` and hands control back to the firmware
/// (`hp_wmi_fan_speed_set`, `HP_FAN_SPEED_AUTOMATIC 0x00`). A curve point
/// at 0 % would therefore silently stop being a curve. Anything that means
/// "spin this slowly" has to be at least 1.
pub const MIN_COMMANDED_PWM: u8 = 1;

/// Speed the curve asks for at `temp_c`, as a percentage.
///
/// Clamped at both ends rather than extrapolated: below the first point
/// the first point's value holds, above the last the last one does. An
/// empty curve has no opinion.
pub fn percent_at(curve: &[CurvePoint], temp_c: f64, interpolation: Interpolation) -> Option<f64> {
    let mut sorted: Vec<CurvePoint> = curve.to_vec();
    sorted.sort_by(|a, b| a.temp_c.total_cmp(&b.temp_c));

    let first = *sorted.first()?;
    let last = *sorted.last()?;
    if temp_c <= first.temp_c {
        return Some(first.percent);
    }
    if temp_c >= last.temp_c {
        return Some(last.percent);
    }

    for pair in sorted.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        // Half-open on purpose: a point's own temperature belongs to that
        // point, not to the segment below it. Under `Smooth` either
        // convention gives the same number, but under `Discrete` the
        // difference is a whole step - at exactly 60 C the user expects the
        // 60 C point's speed, not the one before it. It also guarantees
        // `a.temp_c < b.temp_c` here, so the ratio cannot divide by zero.
        if temp_c < a.temp_c || temp_c >= b.temp_c {
            continue;
        }
        return Some(match interpolation {
            Interpolation::Discrete => a.percent,
            Interpolation::Smooth => {
                let ratio = (temp_c - a.temp_c) / (b.temp_c - a.temp_c);
                a.percent + ratio * (b.percent - a.percent)
            }
        });
    }

    Some(last.percent)
}

/// Percentage → the 0-255 value `pwm1` takes.
///
/// Never returns 0 for a positive percentage, because 0 means "give up and
/// let the firmware decide" (see [`MIN_COMMANDED_PWM`]). A curve asking for
/// 0 % is asking for the slowest speed the hardware will hold, not for the
/// firmware curve back.
pub fn percent_to_pwm(percent: f64) -> u8 {
    let clamped = percent.clamp(0.0, 100.0);
    let raw = (clamped / 100.0 * 255.0).round() as i64;
    raw.clamp(MIN_COMMANDED_PWM as i64, 255) as u8
}

/// Target PWM for a temperature, or `None` if the curve is empty.
pub fn target_pwm(curve: &[CurvePoint], temp_c: f64, interpolation: Interpolation) -> Option<u8> {
    percent_at(curve, temp_c, interpolation).map(percent_to_pwm)
}

/// A fixed-size moving average over the last `window` temperatures.
///
/// The original calls this `ma_window` and defaults it to 5 samples at a
/// ~2 s tick, i.e. about ten seconds of smoothing. Its job is to stop a
/// single spike from stepping the fans up.
#[derive(Debug, Clone)]
pub struct TempSmoother {
    window: usize,
    samples: Vec<f64>,
}

impl TempSmoother {
    pub fn new(window: usize) -> Self {
        Self { window: window.max(1), samples: Vec::new() }
    }

    pub fn push(&mut self, temp_c: f64) -> f64 {
        self.samples.push(temp_c);
        if self.samples.len() > self.window {
            self.samples.remove(0);
        }
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// How far the target has to move before it is worth writing again, and how
/// long a write can be skipped for.
///
/// The original suppresses a rewrite while the measured RPM is within
/// 200 RPM of what the target implies, for at most 60 s. The reason is
/// audible: re-issuing near-identical PWM values makes the fan hunt.
const RPM_DEADBAND: i64 = 200;

/// Deadband when no calibrated maximum RPM is known, in PWM units.
///
/// The RPM form of this test needs `fan_max`, which `fan.calibrate`
/// measures and most machines have never been asked for. 8/255 is a hair
/// over 3 %, small enough to track a curve and large enough to swallow the
/// jitter of a smoothed temperature.
const PWM_DEADBAND: u8 = 8;

/// Longest a write may be suppressed for.
///
/// Also the re-assert interval for the static modes, and the reason it is
/// 60 rather than 90 or 120: the kernel driver refreshes its own fan
/// settings every 90 s (`KEEP_ALIVE_DELAY_SECS`) and the EC takes back
/// control roughly 120 s after being overridden. Anything under 90 leaves
/// the fans where the user put them even if the driver's keep-alive is
/// absent, as it is on older kernels.
pub const REASSERT_SECS: u64 = 60;

/// Decides when to actually touch `pwm1`, given what was last written.
#[derive(Debug, Clone, Default)]
pub struct Hysteresis {
    last: Option<(u8, u64)>,
}

impl Hysteresis {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `target` should be written now, `now_secs` being a monotonic
    /// clock in seconds.
    ///
    /// `measured_rpm` and `fan_max_rpm` are the calibrated form of the
    /// test: when both are known the question is "is the fan already going
    /// roughly this fast", which tolerates a fan that is still spinning up.
    /// Without calibration (see [`crate::calibration`]) it falls back to
    /// comparing PWM values.
    pub fn should_apply(
        &self,
        target: u8,
        measured_rpm: Option<i64>,
        fan_max_rpm: Option<i64>,
        now_secs: u64,
    ) -> bool {
        let Some((last_pwm, written_at)) = self.last else {
            return true;
        };
        if now_secs.saturating_sub(written_at) >= REASSERT_SECS {
            return true;
        }

        match (measured_rpm, fan_max_rpm) {
            (Some(measured), Some(max)) if max > 0 => {
                let expected = target as i64 * max / 255;
                (expected - measured).abs() > RPM_DEADBAND
            }
            _ => target.abs_diff(last_pwm) > PWM_DEADBAND,
        }
    }

    /// Record a write that happened.
    pub fn applied(&mut self, pwm: u8, now_secs: u64) {
        self.last = Some((pwm, now_secs));
    }

    pub fn last_written(&self) -> Option<u8> {
        self.last.map(|(pwm, _)| pwm)
    }

    /// Forget the last write, so the next tick applies unconditionally.
    /// Used when the mode changes under us.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve() -> Vec<CurvePoint> {
        vec![
            CurvePoint { temp_c: 40.0, percent: 20.0 },
            CurvePoint { temp_c: 60.0, percent: 50.0 },
            CurvePoint { temp_c: 80.0, percent: 100.0 },
        ]
    }

    #[test]
    fn an_empty_curve_has_no_opinion() {
        assert_eq!(target_pwm(&[], 50.0, Interpolation::Smooth), None);
    }

    #[test]
    fn the_ends_are_clamped_rather_than_extrapolated() {
        let c = curve();
        assert_eq!(percent_at(&c, 0.0, Interpolation::Smooth), Some(20.0));
        assert_eq!(percent_at(&c, 200.0, Interpolation::Smooth), Some(100.0));
    }

    #[test]
    fn smooth_interpolates_linearly_between_points() {
        let at = percent_at(&curve(), 50.0, Interpolation::Smooth).unwrap();
        assert!((at - 35.0).abs() < 1e-9, "midpoint of 20..50 is 35, got {at}");
    }

    #[test]
    fn discrete_holds_the_lower_point() {
        assert_eq!(percent_at(&curve(), 59.0, Interpolation::Discrete), Some(20.0));
        assert_eq!(percent_at(&curve(), 60.0, Interpolation::Discrete), Some(50.0));
    }

    /// The frontend sorts before interpolating; a curve arriving over IPC
    /// has been through a JSON round trip and need not be ordered.
    #[test]
    fn points_do_not_have_to_arrive_in_order() {
        let mut shuffled = curve();
        shuffled.reverse();
        assert_eq!(
            percent_at(&shuffled, 50.0, Interpolation::Smooth),
            percent_at(&curve(), 50.0, Interpolation::Smooth)
        );
    }

    /// A vertical step is a legal thing to draw, and must not divide by
    /// zero on the way through.
    #[test]
    fn two_points_at_the_same_temperature_are_a_step_not_a_panic() {
        let c = vec![
            CurvePoint { temp_c: 50.0, percent: 20.0 },
            CurvePoint { temp_c: 50.0, percent: 80.0 },
            CurvePoint { temp_c: 60.0, percent: 90.0 },
        ];
        // At the step itself the lower of the two holds (the sort is
        // stable, so it is the one the user drew first); above it, the
        // upper point is what the next segment interpolates from.
        assert_eq!(percent_at(&c, 50.0, Interpolation::Smooth), Some(20.0));
        assert_eq!(percent_at(&c, 55.0, Interpolation::Smooth), Some(85.0));
    }

    /// The whole reason `MIN_COMMANDED_PWM` exists.
    #[test]
    fn zero_percent_never_becomes_the_drivers_automatic_sentinel() {
        assert_eq!(percent_to_pwm(0.0), MIN_COMMANDED_PWM);
        assert_ne!(percent_to_pwm(0.0), 0);
    }

    #[test]
    fn a_hundred_percent_is_full_scale() {
        assert_eq!(percent_to_pwm(100.0), 255);
        assert_eq!(percent_to_pwm(1000.0), 255);
    }

    #[test]
    fn the_smoother_averages_only_its_window() {
        let mut s = TempSmoother::new(3);
        assert_eq!(s.push(10.0), 10.0);
        assert_eq!(s.push(20.0), 15.0);
        assert_eq!(s.push(30.0), 20.0);
        // 10 falls out of the window here.
        assert_eq!(s.push(40.0), 30.0);
    }

    #[test]
    fn a_zero_window_still_averages_something() {
        let mut s = TempSmoother::new(0);
        assert_eq!(s.push(42.0), 42.0);
    }

    #[test]
    fn the_first_target_is_always_written() {
        assert!(Hysteresis::new().should_apply(128, None, None, 0));
    }

    #[test]
    fn a_small_change_is_suppressed_and_a_large_one_is_not() {
        let mut h = Hysteresis::new();
        h.applied(128, 0);
        assert!(!h.should_apply(130, None, None, 1), "2/255 is noise");
        assert!(h.should_apply(200, None, None, 1), "a real step must land");
    }

    /// Suppression is bounded: the fans must not drift back to the
    /// firmware curve because the target happened to stay still.
    #[test]
    fn suppression_expires_so_the_setting_is_re_asserted() {
        let mut h = Hysteresis::new();
        h.applied(128, 0);
        assert!(!h.should_apply(128, None, None, REASSERT_SECS - 1));
        assert!(h.should_apply(128, None, None, REASSERT_SECS));
    }

    /// With a calibrated maximum the question becomes "is the fan already
    /// going this fast", which tolerates one that is still spinning up.
    #[test]
    fn a_fan_already_near_the_target_rpm_is_left_alone() {
        let mut h = Hysteresis::new();
        h.applied(0, 0);
        // Target 128/255 of 5800 rpm is ~2913.
        assert!(!h.should_apply(128, Some(2900), Some(5800), 1));
        assert!(h.should_apply(128, Some(1000), Some(5800), 1));
    }

    #[test]
    fn a_mode_change_forgets_the_last_write() {
        let mut h = Hysteresis::new();
        h.applied(128, 0);
        h.reset();
        assert!(h.should_apply(128, None, None, 1));
    }
}
