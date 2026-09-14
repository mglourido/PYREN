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

/// Fewest points a curve may have: one point is a constant speed, which is
/// what `manual` is for, and it cannot rise with the heat.
pub const MIN_CURVE_POINTS: usize = 2;
/// Most points a curve may have. An editor draws a handful; a thousand is a
/// mistake, and the curve is re-sorted every tick.
pub const MAX_CURVE_POINTS: usize = 16;
/// Highest temperature a point may sit at. Past this a part is being
/// damaged, and a point there is one the curve can never usefully reach.
pub const MAX_CURVE_TEMP_C: f64 = 110.0;

/// Why a curve was refused. Carries the offending numbers so the message
/// can name them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CurveProblem {
    PointCount(usize),
    NotFinite {
        temp_c: f64,
        percent: f64,
    },
    TempOutOfRange(f64),
    PercentOutOfRange(f64),
    /// Speed drops as the temperature rises, at this temperature.
    Decreasing(f64),
}

/// Whether a curve is one the fans may follow.
///
/// Checked when a curve arrives and again when one is loaded from disk,
/// because a hand-edited file reaches the fans by the same road as the
/// app. How fast the fans run when hot is the user's call: the critical
/// override and the thermal safety checker (see `safety`) are what step in
/// when a quiet curve is not enough.
pub fn validate(curve: &[CurvePoint]) -> Result<(), CurveProblem> {
    if !(MIN_CURVE_POINTS..=MAX_CURVE_POINTS).contains(&curve.len()) {
        return Err(CurveProblem::PointCount(curve.len()));
    }
    for point in curve {
        if !point.temp_c.is_finite() || !point.percent.is_finite() {
            return Err(CurveProblem::NotFinite {
                temp_c: point.temp_c,
                percent: point.percent,
            });
        }
        if !(0.0..=MAX_CURVE_TEMP_C).contains(&point.temp_c) {
            return Err(CurveProblem::TempOutOfRange(point.temp_c));
        }
        if !(0.0..=100.0).contains(&point.percent) {
            return Err(CurveProblem::PercentOutOfRange(point.percent));
        }
    }
    let mut sorted = curve.to_vec();
    sorted.sort_by(|a, b| a.temp_c.total_cmp(&b.temp_c));
    if let Some(pair) = sorted.windows(2).find(|w| w[1].percent < w[0].percent) {
        return Err(CurveProblem::Decreasing(pair[1].temp_c));
    }
    Ok(())
}

/// The nearest safe curve to one [`validate`] refused, or `None` when
/// there is not enough of it left to be a curve.
///
/// For a curve read from disk, where refusing means silently losing a shape
/// somebody tuned. Every change only ever makes the fans faster or the
/// numbers saner: non-finite points are dropped, the rest clamped into
/// range, and speeds raised so they never fall as the heat rises.
pub fn repair(curve: &[CurvePoint]) -> Option<Vec<CurvePoint>> {
    let mut points: Vec<CurvePoint> = curve
        .iter()
        .filter(|p| p.temp_c.is_finite() && p.percent.is_finite())
        .map(|p| CurvePoint {
            temp_c: p.temp_c.clamp(0.0, MAX_CURVE_TEMP_C),
            percent: p.percent.clamp(0.0, 100.0),
        })
        .collect();
    points.sort_by(|a, b| a.temp_c.total_cmp(&b.temp_c));

    points.truncate(MAX_CURVE_POINTS);
    let mut highest = 0.0_f64;
    for point in &mut points {
        highest = highest.max(point.percent);
        point.percent = highest;
    }

    validate(&points).ok().map(|()| points)
}

/// Percentage → the 0-255 value `pwm1` takes.
///
/// Never returns 0 for a positive percentage, because 0 means "give up and
/// let the firmware decide" (see [`MIN_COMMANDED_PWM`]). Whether a speed
/// this low is one the fans can hold at all is a separate question, and
/// [`stop_below_pwm`] is where it is answered.
pub fn percent_to_pwm(percent: f64) -> u8 {
    let clamped = percent.clamp(0.0, 100.0);
    let raw = (clamped / 100.0 * 255.0).round() as i64;
    raw.clamp(MIN_COMMANDED_PWM as i64, 255) as u8
}

/// Target PWM for a temperature, or `None` if the curve is empty.
pub fn target_pwm(curve: &[CurvePoint], temp_c: f64, interpolation: Interpolation) -> Option<u8> {
    percent_at(curve, temp_c, interpolation).map(percent_to_pwm)
}

/// The PWM below which a commanded speed is not one the fans can hold, so
/// the fans are handed to the firmware instead. `0` means never.
///
/// Board 8D2F is why: its fan table starts at 1800 rpm and the driver
/// clamps every manual speed up to that, so everything from 0 to a third
/// of the scale came out as the same audible 1800. The firmware, meanwhile,
/// does stop the fans when the machine is cool - measured at 0 rpm within
/// fifteen seconds of `pwm1_enable = 2` at 38 C. Nothing a speed command
/// can say reaches 0 rpm here (0 is the driver's "automatic"), so a curve
/// that asks for less than the floor gets the one thing that does.
///
/// Only with a measured floor (`fan.calibrate`): without one there is no
/// telling a board with a floor from one that will turn at 1/255, and the
/// latter should get the slow speed it was asked for. A measured floor of
/// 0 is a fan that stops when told the minimum, and needs no hand-over.
pub fn stop_below_pwm(fan_min_rpm: Option<i64>, fan_max_rpm: Option<i64>) -> u8 {
    match (fan_min_rpm, fan_max_rpm) {
        (Some(min), Some(max)) if min > 0 && max > 0 => {
            // Rounded up: a target exactly at the floor is a speed the fans
            // hold, and must not be read as one below it.
            let pwm = (min * 255 + max - 1) / max;
            pwm.clamp(MIN_COMMANDED_PWM as i64 + 1, 255) as u8
        }
        _ => 0,
    }
}

/// Whether the fans should be with the firmware rather than at `target`.
///
/// Banded so a temperature sitting on the threshold does not start and
/// stop the fans every tick, which is louder than either state: once
/// stopped they restart only [`PWM_DEADBAND`] above the floor.
pub fn release_fans(target: u8, stop_below: u8, released: bool) -> bool {
    if stop_below == 0 {
        return false;
    }
    let threshold = if released {
        stop_below.saturating_add(PWM_DEADBAND)
    } else {
        stop_below
    };
    target < threshold
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
        Self {
            window: window.max(1),
            samples: Vec::new(),
        }
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
pub const PWM_DEADBAND: u8 = 8;

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
            CurvePoint {
                temp_c: 40.0,
                percent: 20.0,
            },
            CurvePoint {
                temp_c: 60.0,
                percent: 50.0,
            },
            CurvePoint {
                temp_c: 80.0,
                percent: 100.0,
            },
        ]
    }

    fn pts(pairs: &[(f64, f64)]) -> Vec<CurvePoint> {
        pairs
            .iter()
            .map(|&(temp_c, percent)| CurvePoint { temp_c, percent })
            .collect()
    }

    #[test]
    fn a_sensible_curve_is_accepted() {
        assert_eq!(validate(&curve()), Ok(()));
        assert_eq!(validate(&pts(&[(40.0, 0.0), (85.0, 100.0)])), Ok(()));
    }

    #[test]
    fn a_curve_needs_between_two_and_sixteen_points() {
        assert_eq!(
            validate(&pts(&[(80.0, 100.0)])),
            Err(CurveProblem::PointCount(1))
        );
        let many: Vec<(f64, f64)> = (0..17).map(|i| (i as f64 * 5.0, 100.0)).collect();
        assert_eq!(validate(&pts(&many)), Err(CurveProblem::PointCount(17)));
    }

    #[test]
    fn out_of_range_numbers_are_refused() {
        assert_eq!(
            validate(&pts(&[(-1e9, 10.0), (80.0, 100.0)])),
            Err(CurveProblem::TempOutOfRange(-1e9))
        );
        assert_eq!(
            validate(&pts(&[(40.0, 10.0), (111.0, 100.0)])),
            Err(CurveProblem::TempOutOfRange(111.0))
        );
        assert_eq!(
            validate(&pts(&[(40.0, -5.0), (80.0, 100.0)])),
            Err(CurveProblem::PercentOutOfRange(-5.0))
        );
        assert!(matches!(
            validate(&pts(&[(f64::NAN, 5.0), (80.0, 100.0)])),
            Err(CurveProblem::NotFinite { .. })
        ));
    }

    #[test]
    fn speed_may_not_fall_as_the_heat_rises() {
        assert_eq!(
            validate(&pts(&[(40.0, 50.0), (60.0, 30.0), (80.0, 100.0)])),
            Err(CurveProblem::Decreasing(60.0))
        );
    }

    /// How quiet a curve is when hot is the user's choice; the critical
    /// override is what covers it.
    #[test]
    fn a_quiet_curve_is_the_users_call() {
        assert_eq!(validate(&pts(&[(40.0, 5.0), (100.0, 5.0)])), Ok(()));
    }

    #[test]
    fn a_stored_curve_is_repaired_into_a_safe_one_keeping_its_quiet_end() {
        let repaired = repair(&pts(&[
            (40.0, 10.0),
            (60.0, 5.0),
            (100.0, 40.0),
            (150.0, 90.0),
        ]))
        .expect("repairable");
        assert_eq!(validate(&repaired), Ok(()));
        assert_eq!(
            repaired[0],
            CurvePoint {
                temp_c: 40.0,
                percent: 10.0
            }
        );
        assert_eq!(repaired[1].percent, 10.0, "raised, never lowered");
        assert_eq!(repaired[3].temp_c, MAX_CURVE_TEMP_C, "clamped into range");
    }

    #[test]
    fn a_single_bad_point_is_nothing_to_repair() {
        assert_eq!(repair(&pts(&[(f64::NAN, 1.0)])), None);
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
        assert!(
            (at - 35.0).abs() < 1e-9,
            "midpoint of 20..50 is 35, got {at}"
        );
    }

    #[test]
    fn discrete_holds_the_lower_point() {
        assert_eq!(
            percent_at(&curve(), 59.0, Interpolation::Discrete),
            Some(20.0)
        );
        assert_eq!(
            percent_at(&curve(), 60.0, Interpolation::Discrete),
            Some(50.0)
        );
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
            CurvePoint {
                temp_c: 50.0,
                percent: 20.0,
            },
            CurvePoint {
                temp_c: 50.0,
                percent: 80.0,
            },
            CurvePoint {
                temp_c: 60.0,
                percent: 90.0,
            },
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

    /// Board 8D2F: 1800 of 5300 rpm is 86.6/255, so 87 is the first PWM
    /// the fans can hold.
    #[test]
    fn the_floor_is_the_measured_minimum_on_the_pwm_scale() {
        assert_eq!(stop_below_pwm(Some(1800), Some(5300)), 87);
    }

    #[test]
    fn without_a_measured_floor_nothing_is_handed_over() {
        assert_eq!(stop_below_pwm(None, Some(5300)), 0);
        assert_eq!(stop_below_pwm(Some(1800), None), 0);
        assert!(!release_fans(MIN_COMMANDED_PWM, 0, false));
    }

    /// A fan that stops when told the minimum needs no firmware to stop it.
    #[test]
    fn a_floor_of_zero_needs_no_hand_over() {
        assert_eq!(stop_below_pwm(Some(0), Some(5300)), 0);
    }

    #[test]
    fn below_the_floor_the_fans_go_to_the_firmware() {
        assert!(release_fans(percent_to_pwm(0.0), 87, false));
        assert!(release_fans(86, 87, false));
        assert!(!release_fans(87, 87, false), "the floor itself is a speed");
    }

    /// Stopped fans restart a deadband above the floor, not at it.
    #[test]
    fn stopped_fans_restart_only_clear_of_the_floor() {
        assert!(release_fans(87, 87, true));
        assert!(release_fans(87 + PWM_DEADBAND - 1, 87, true));
        assert!(!release_fans(87 + PWM_DEADBAND, 87, true));
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
