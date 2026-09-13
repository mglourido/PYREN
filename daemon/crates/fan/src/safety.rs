//! The guards that stand between a fan setting and an overheating machine.
//!
//! Everything else in this crate does what it was asked. This is the part
//! that decides when what was asked has stopped being safe, and takes the
//! fans back until it is again. Four watches, each answering one question:
//!
//! - [`SensorWatch`]: is the temperature the curve follows still a real
//!   reading? A curve fed nothing keeps the last speed it wrote, forever.
//! - [`CriticalLatch`]: is the machine at a temperature where no curve and
//!   no manual speed is allowed to have an opinion?
//! - [`ThermalChecker`]: the machine is hot - are the fans answering? If
//!   not, the firmware gets them, and if *it* does not answer either, full
//!   speed until the heat is gone.
//! - [`ZeroRpmWatch`]: a real speed is commanded - is anything turning?
//!
//! Kept free of file I/O and clocks, like [`crate::curve`], so the decisions
//! can be tested exhaustively: these are the ones that must not be subtly
//! wrong.

use crate::FanMode;

/// At or above this, on the hotter of CPU and GPU, the fans go to full
/// speed in `manual` and `curve` whatever the setting says. Below the
/// CPU's own 100 C throttle on purpose: the point is never to reach it.
pub const CRITICAL_C: f64 = 90.0;
/// ...and they stay there until the machine is back under this.
pub const CRITICAL_CLEAR_C: f64 = 80.0;

/// Consecutive unusable readings before a curve stops being followed. Three
/// ticks is six seconds: long enough to ride out a hwmon renumbering, short
/// enough that a machine under load has not heated meaningfully.
pub const SENSOR_FAILURE_TICKS: u32 = 3;
/// A reading that has not moved by a single degree for this long, while
/// the fans are turning, is a sensor that stopped updating rather than a
/// machine at a perfectly constant temperature.
pub const STALE_SENSOR_SECS: u64 = 300;

/// How long the fans get to answer heat before the next step is taken -
/// first from the user's setting to the firmware, then from the firmware to
/// full speed.
pub const ANSWER_SECS: u64 = 10;
/// A rise this big from where the fans were when the heat began counts as
/// an answer. The tachometer reports hundreds.
pub const RISE_RPM: i64 = 300;
/// With a calibrated maximum, fans already this close to it have nothing
/// left to rise by and count as answering.
pub const NEAR_MAX_FRACTION: f64 = 0.85;
/// A commanded speed at or above this is full speed in all but name.
pub const NEAR_FULL_PWM: u8 = 230;

/// A commanded speed at or above this with the fans at 0 rpm is a stall.
/// Half scale: below it, a stopped fan can be a floor, not a fault.
pub const STALL_MIN_PWM: u8 = 128;
/// A manual speed under this with no usable temperature reading gets the
/// same full-speed fallback as a curve whose sensor failed: the critical
/// override is what makes a slow manual speed safe, and it needs a reading.
/// Half scale, like the stall threshold.
pub const MANUAL_BLIND_BELOW_PWM: u8 = 128;
/// How long 0 rpm has to last under such a command.
pub const STALL_SECS: u64 = 10;

/// Calibration and the speed probe hold the fans somewhere other than where
/// the machine needs them, for up to two minutes. Neither starts above this,
/// and both stop the moment it is crossed.
pub const MEASUREMENT_MAX_C: f64 = 60.0;
/// How far under [`MEASUREMENT_MAX_C`] the machine has to come before the
/// safety sequence an aborted measurement starts hands the fans back.
pub const MEASUREMENT_COOL_MARGIN_C: f64 = 5.0;

/// The fan cleaner reverses the fans, which is cooling switched off. It
/// already refuses to start above `cleaner::MAX_START_TEMP_C`; this is the
/// temperature at which a cycle already running is ended.
pub const CLEANER_ABORT_C: i64 = 80;

/// Whether a whole-degree reading is a temperature at all. 0 or below is a
/// part that is asleep or a driver that failed; above 125 no laptop part
/// survives, so it is a garbage register rather than a reading.
pub fn plausible_c(temp_c: i64) -> bool {
    temp_c > 0 && temp_c <= pyren_core::sensors::MAX_PLAUSIBLE_C
}

/// The hotter of two readings, leaving out any that is not a temperature.
pub fn hottest_c(cpu: Option<i64>, gpu: Option<i64>) -> Option<f64> {
    [cpu, gpu]
        .into_iter()
        .flatten()
        .filter(|t| plausible_c(*t))
        .max()
        .map(|t| t as f64)
}

/// Counts reasons to stop believing the curve's reference sensor.
#[derive(Debug, Clone, Default)]
pub struct SensorWatch {
    bad_ticks: u32,
    last: Option<i64>,
    unchanged_since: u64,
}

impl SensorWatch {
    /// Feeds one tick's reading in; true once the sensor counts as failed.
    ///
    /// Failed means [`SENSOR_FAILURE_TICKS`] consecutive readings that were
    /// missing or implausible, or one that has sat on the same value for
    /// [`STALE_SENSOR_SECS`] while the fans were turning. A single good,
    /// *changed* reading clears it.
    pub fn observe(&mut self, reading: Option<i64>, now_secs: u64, fans_turning: bool) -> bool {
        match reading.filter(|t| plausible_c(*t)) {
            None => {
                self.bad_ticks = self.bad_ticks.saturating_add(1);
                self.last = None;
            }
            Some(temp) => {
                if self.last != Some(temp) || !fans_turning {
                    self.last = Some(temp);
                    self.unchanged_since = now_secs;
                }
                let stale = now_secs.saturating_sub(self.unchanged_since) >= STALE_SENSOR_SECS;
                if stale {
                    self.bad_ticks = self.bad_ticks.saturating_add(1);
                } else {
                    self.bad_ticks = 0;
                }
            }
        }
        self.failed()
    }

    pub fn failed(&self) -> bool {
        self.bad_ticks >= SENSOR_FAILURE_TICKS
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Latched so the fans do not flap on the threshold: on at
/// [`CRITICAL_C`], off only under [`CRITICAL_CLEAR_C`]. A lost reading
/// changes nothing - losing a sensor is not evidence of cooling.
#[derive(Debug, Clone, Copy, Default)]
pub struct CriticalLatch {
    active: bool,
}

impl CriticalLatch {
    pub fn observe(&mut self, hottest_c: Option<f64>) -> bool {
        if let Some(temp) = hottest_c {
            if temp >= CRITICAL_C {
                self.active = true;
            } else if temp < CRITICAL_CLEAR_C {
                self.active = false;
            }
        }
        self.active
    }

    pub fn is_active(self) -> bool {
        self.active
    }
}

/// The hot/cooled pair the checker works between, taken from the power
/// supervisor's own "hot at" / "cooled below" settings so the machine has
/// one idea of what hot means.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeatThresholds {
    pub hot_c: f64,
    pub cool_c: f64,
}

impl Default for HeatThresholds {
    fn default() -> Self {
        Self {
            hot_c: 85.0,
            cool_c: 75.0,
        }
    }
}

impl HeatThresholds {
    /// Refuses a pair that could never latch off, or that would call a
    /// sleeping machine hot, and keeps the defaults instead.
    pub fn sanitised(hot_c: f64, cool_c: f64) -> Self {
        let valid = hot_c.is_finite()
            && cool_c.is_finite()
            && cool_c < hot_c
            && (40.0..=CRITICAL_C).contains(&hot_c)
            && cool_c >= 30.0;
        if valid {
            Self { hot_c, cool_c }
        } else {
            Self::default()
        }
    }
}

/// Where an episode of heat is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Hot, and the user's setting still has the fans. Watching for them
    /// to answer.
    Watching { since: u64, baseline: Option<i64> },
    /// They answered. Nothing more to do until the heat is gone.
    Answered,
    /// They did not; the firmware has the fans now. `answered` once it
    /// shows it is doing something about the heat.
    Firmware {
        since: u64,
        baseline: Option<i64>,
        answered: bool,
    },
    /// Neither answered. Full speed until the heat is gone.
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Episode {
    phase: Phase,
    /// Under this the episode ends and the user's setting comes back.
    cool_below: f64,
    /// Started by something other than the "hot" threshold - an aborted
    /// measurement - and so runs whether or not the setting is on.
    tripped: bool,
}

/// What one observation changed, for logging and the event bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    None,
    /// Heat began; the fans are being watched.
    Watching,
    /// The fans did not answer; handed to the firmware.
    HandedToFirmware,
    /// The firmware did not answer either; full speed.
    ForcedMax,
    /// The heat is gone; the user's setting has the fans again.
    Restored,
}

impl Transition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Watching => "watching",
            Self::HandedToFirmware => "firmware",
            Self::ForcedMax => "max",
            Self::Restored => "restored",
        }
    }
}

/// One tick's worth of evidence.
#[derive(Debug, Clone, Copy)]
pub struct Evidence {
    pub now_secs: u64,
    pub hottest_c: Option<f64>,
    /// `None` when no tachometer could be read, which is never an answer.
    pub rpm: Option<i64>,
    pub fan_max_rpm: Option<i64>,
    /// The fans are already commanded full speed (max, or a curve/manual
    /// speed near it) and turning, so there is nothing to rise to.
    pub commanded_full: bool,
}

/// The "are the fans answering the heat" sequence.
///
/// Hot (per [`HeatThresholds`]) and the fans have not risen within
/// [`ANSWER_SECS`]: the firmware gets them. The firmware has not raised
/// them within another [`ANSWER_SECS`]: full speed. Cooled: whatever the
/// user had is put back, exactly - the setting itself is never touched,
/// only what the hardware is told while this holds it.
#[derive(Debug, Clone, Default)]
pub struct ThermalChecker {
    episode: Option<Episode>,
}

impl ThermalChecker {
    /// What the fans have to be while this holds them; `None` when it does
    /// not.
    pub fn command(&self) -> Option<FanMode> {
        match self.episode?.phase {
            Phase::Firmware { .. } => Some(FanMode::Auto),
            Phase::Max => Some(FanMode::Max),
            Phase::Watching { .. } | Phase::Answered => None,
        }
    }

    pub fn phase_name(&self) -> &'static str {
        match self.episode.map(|e| e.phase) {
            None => "idle",
            Some(Phase::Watching { .. }) => "watching",
            Some(Phase::Answered) => "answered",
            Some(Phase::Firmware { .. }) => "firmware",
            Some(Phase::Max) => "max",
        }
    }

    /// Starts the sequence at its second step, whatever the temperature
    /// threshold says: an aborted measurement has already shown the fans
    /// were somewhere the machine could not afford.
    pub fn trip(&mut self, now_secs: u64, rpm: Option<i64>, cool_below: f64) -> Transition {
        let already_holding = self.command().is_some();
        self.episode = Some(Episode {
            phase: Phase::Firmware {
                since: now_secs,
                baseline: rpm,
                answered: false,
            },
            cool_below,
            tripped: true,
        });
        if already_holding {
            Transition::None
        } else {
            Transition::HandedToFirmware
        }
    }

    pub fn observe(
        &mut self,
        evidence: Evidence,
        thresholds: HeatThresholds,
        enabled: bool,
    ) -> Transition {
        let Some(mut episode) = self.episode else {
            if !enabled {
                return Transition::None;
            }
            return match evidence.hottest_c {
                Some(temp) if temp >= thresholds.hot_c => {
                    self.episode = Some(Episode {
                        phase: Phase::Watching {
                            since: evidence.now_secs,
                            baseline: evidence.rpm,
                        },
                        cool_below: thresholds.cool_c,
                        tripped: false,
                    });
                    Transition::Watching
                }
                _ => Transition::None,
            };
        };

        let holding = matches!(episode.phase, Phase::Firmware { .. } | Phase::Max);
        let cooled = evidence
            .hottest_c
            .is_some_and(|temp| temp <= episode.cool_below);
        // Switching the setting off ends an episode it started; one an
        // aborted measurement started runs to the end regardless.
        if cooled || (!enabled && !episode.tripped) {
            self.episode = None;
            return if holding {
                Transition::Restored
            } else {
                Transition::None
            };
        }

        let answered = |baseline: Option<i64>| responded(evidence, baseline);
        let elapsed = |since: u64| evidence.now_secs.saturating_sub(since) >= ANSWER_SECS;
        let transition = match episode.phase {
            Phase::Watching { since, baseline } => {
                if answered(baseline) {
                    episode.phase = Phase::Answered;
                    Transition::None
                } else if elapsed(since) {
                    episode.phase = Phase::Firmware {
                        since: evidence.now_secs,
                        baseline: evidence.rpm,
                        answered: false,
                    };
                    Transition::HandedToFirmware
                } else {
                    Transition::None
                }
            }
            Phase::Answered | Phase::Max => Transition::None,
            Phase::Firmware {
                since,
                baseline,
                answered: done,
            } => {
                if done {
                    Transition::None
                } else if responded(
                    Evidence {
                        // Whatever the user's command was, the firmware
                        // has the fans now: full-speed-by-command no
                        // longer applies.
                        commanded_full: false,
                        ..evidence
                    },
                    baseline,
                ) {
                    episode.phase = Phase::Firmware {
                        since,
                        baseline,
                        answered: true,
                    };
                    Transition::None
                } else if elapsed(since) {
                    episode.phase = Phase::Max;
                    Transition::ForcedMax
                } else {
                    Transition::None
                }
            }
        };
        self.episode = Some(episode);
        transition
    }

    pub fn reset(&mut self) {
        self.episode = None;
    }
}

/// Whether the fans are doing something about the heat, measured against
/// where they were when the current step began.
fn responded(evidence: Evidence, baseline: Option<i64>) -> bool {
    let Some(rpm) = evidence.rpm.filter(|r| *r > 0) else {
        return false;
    };
    if evidence.commanded_full {
        return true;
    }
    if let Some(max) = evidence.fan_max_rpm.filter(|m| *m > 0) {
        if rpm as f64 >= max as f64 * NEAR_MAX_FRACTION {
            return true;
        }
    }
    rpm >= baseline.unwrap_or(0) + RISE_RPM
}

/// Watches for a real speed commanded and nothing turning.
#[derive(Debug, Clone, Default)]
pub struct ZeroRpmWatch {
    since: Option<u64>,
}

impl ZeroRpmWatch {
    /// True once `pwm` of at least [`STALL_MIN_PWM`] has met a readable
    /// 0 rpm continuously for [`STALL_SECS`].
    pub fn observe(&mut self, now_secs: u64, pwm: Option<u8>, rpm: Option<i64>) -> bool {
        let stalled_now = pwm.is_some_and(|p| p >= STALL_MIN_PWM) && rpm == Some(0);
        if !stalled_now {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now_secs);
        now_secs.saturating_sub(since) >= STALL_SECS
    }

    pub fn reset(&mut self) {
        self.since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(now_secs: u64, temp: f64, rpm: i64) -> Evidence {
        Evidence {
            now_secs,
            hottest_c: Some(temp),
            rpm: Some(rpm),
            fan_max_rpm: None,
            commanded_full: false,
        }
    }

    fn thresholds() -> HeatThresholds {
        HeatThresholds::default()
    }

    #[test]
    fn implausible_readings_are_not_temperatures() {
        assert!(!plausible_c(0));
        assert!(!plausible_c(-40));
        assert!(!plausible_c(255));
        assert!(plausible_c(1));
        assert!(plausible_c(125));
        assert_eq!(hottest_c(Some(255), Some(70)), Some(70.0));
        assert_eq!(hottest_c(Some(0), None), None);
    }

    #[test]
    fn a_sensor_fails_after_three_bad_ticks_and_recovers_on_a_good_one() {
        let mut watch = SensorWatch::default();
        assert!(!watch.observe(None, 0, true));
        assert!(!watch.observe(Some(0), 2, true));
        assert!(watch.observe(Some(300), 4, true), "third bad tick fails");
        assert!(
            !watch.observe(Some(55), 6, true),
            "a real reading clears it"
        );
    }

    #[test]
    fn a_reading_frozen_while_the_fans_turn_is_a_failed_sensor() {
        let mut watch = SensorWatch::default();
        assert!(!watch.observe(Some(50), 0, true));
        assert!(!watch.observe(Some(50), STALE_SENSOR_SECS - 1, true));
        let mut now = STALE_SENSOR_SECS;
        for _ in 0..SENSOR_FAILURE_TICKS - 1 {
            assert!(!watch.observe(Some(50), now, true));
            now += 2;
        }
        assert!(watch.observe(Some(50), now, true));
        assert!(!watch.observe(Some(51), now + 2, true), "it moved again");
    }

    #[test]
    fn a_constant_reading_with_the_fans_stopped_is_not_stale() {
        let mut watch = SensorWatch::default();
        for tick in 0..400 {
            assert!(!watch.observe(Some(40), tick * 2, false));
        }
    }

    #[test]
    fn the_critical_latch_holds_until_well_below_the_threshold() {
        let mut latch = CriticalLatch::default();
        assert!(!latch.observe(Some(89.0)));
        assert!(latch.observe(Some(90.0)));
        assert!(latch.observe(Some(85.0)), "still hot enough to hold");
        assert!(latch.observe(None), "losing the sensor is not cooling");
        assert!(!latch.observe(Some(79.0)));
    }

    #[test]
    fn nonsense_heat_thresholds_fall_back_to_the_defaults() {
        assert_eq!(HeatThresholds::sanitised(70.0, 80.0), thresholds());
        assert_eq!(HeatThresholds::sanitised(f64::NAN, 75.0), thresholds());
        assert_eq!(HeatThresholds::sanitised(120.0, 75.0), thresholds());
        assert_eq!(
            HeatThresholds::sanitised(80.0, 70.0),
            HeatThresholds {
                hot_c: 80.0,
                cool_c: 70.0
            }
        );
    }

    #[test]
    fn fans_that_answer_the_heat_are_left_alone() {
        let mut checker = ThermalChecker::default();
        let t = thresholds();
        assert_eq!(
            checker.observe(evidence(0, 86.0, 2000), t, true),
            Transition::Watching
        );
        assert_eq!(
            checker.observe(evidence(4, 87.0, 2600), t, true),
            Transition::None
        );
        assert_eq!(checker.command(), None);
        assert_eq!(
            checker.observe(evidence(60, 87.0, 2600), t, true),
            Transition::None
        );
        assert_eq!(checker.command(), None, "answered stays answered");
        assert_eq!(
            checker.observe(evidence(62, 74.0, 2600), t, true),
            Transition::None,
            "nothing was taken, so nothing is restored"
        );
    }

    #[test]
    fn fans_that_do_not_answer_go_to_the_firmware_then_to_max_then_back() {
        let mut checker = ThermalChecker::default();
        let t = thresholds();
        checker.observe(evidence(0, 86.0, 2000), t, true);
        assert_eq!(
            checker.observe(evidence(ANSWER_SECS - 1, 88.0, 2000), t, true),
            Transition::None
        );
        assert_eq!(
            checker.observe(evidence(ANSWER_SECS, 88.0, 2000), t, true),
            Transition::HandedToFirmware
        );
        assert_eq!(checker.command(), Some(FanMode::Auto));
        assert_eq!(
            checker.observe(evidence(2 * ANSWER_SECS, 89.0, 2100), t, true),
            Transition::ForcedMax
        );
        assert_eq!(checker.command(), Some(FanMode::Max));
        assert_eq!(
            checker.observe(evidence(3 * ANSWER_SECS, 80.0, 5000), t, true),
            Transition::None,
            "not yet under the cooled threshold"
        );
        assert_eq!(
            checker.observe(evidence(4 * ANSWER_SECS, 75.0, 5000), t, true),
            Transition::Restored
        );
        assert_eq!(checker.command(), None);
    }

    #[test]
    fn a_firmware_that_answers_keeps_the_fans_until_cool() {
        let mut checker = ThermalChecker::default();
        let t = thresholds();
        checker.observe(evidence(0, 86.0, 2000), t, true);
        checker.observe(evidence(ANSWER_SECS, 86.0, 2000), t, true);
        checker.observe(evidence(ANSWER_SECS + 2, 86.0, 3500), t, true);
        assert_eq!(
            checker.observe(evidence(5 * ANSWER_SECS, 86.0, 3500), t, true),
            Transition::None
        );
        assert_eq!(checker.command(), Some(FanMode::Auto));
        assert_eq!(
            checker.observe(evidence(6 * ANSWER_SECS, 70.0, 3000), t, true),
            Transition::Restored
        );
    }

    #[test]
    fn fans_already_at_full_command_count_as_answering() {
        let mut checker = ThermalChecker::default();
        let t = thresholds();
        let full = Evidence {
            commanded_full: true,
            ..evidence(0, 88.0, 4800)
        };
        checker.observe(full, t, true);
        checker.observe(
            Evidence {
                now_secs: ANSWER_SECS * 3,
                ..full
            },
            t,
            true,
        );
        assert_eq!(checker.command(), None);
    }

    #[test]
    fn no_tachometer_is_never_an_answer() {
        let mut checker = ThermalChecker::default();
        let t = thresholds();
        let blind = Evidence {
            rpm: None,
            ..evidence(0, 88.0, 0)
        };
        checker.observe(blind, t, true);
        checker.observe(
            Evidence {
                now_secs: ANSWER_SECS,
                ..blind
            },
            t,
            true,
        );
        checker.observe(
            Evidence {
                now_secs: 2 * ANSWER_SECS,
                ..blind
            },
            t,
            true,
        );
        assert_eq!(checker.command(), Some(FanMode::Max));
    }

    #[test]
    fn switching_the_checker_off_hands_back_what_it_took() {
        let mut checker = ThermalChecker::default();
        let t = thresholds();
        assert_eq!(
            checker.observe(evidence(0, 95.0, 1000), t, false),
            Transition::None
        );
        checker.observe(evidence(0, 95.0, 1000), t, true);
        checker.observe(evidence(ANSWER_SECS, 95.0, 1000), t, true);
        assert_eq!(
            checker.observe(evidence(ANSWER_SECS + 2, 95.0, 1000), t, false),
            Transition::Restored
        );
    }

    #[test]
    fn a_tripped_sequence_runs_whatever_the_setting_and_ends_at_its_own_threshold() {
        let mut checker = ThermalChecker::default();
        let t = thresholds();
        assert_eq!(
            checker.trip(0, Some(1500), 55.0),
            Transition::HandedToFirmware
        );
        assert_eq!(checker.command(), Some(FanMode::Auto));
        assert_eq!(
            checker.observe(evidence(ANSWER_SECS, 62.0, 1500), t, false),
            Transition::ForcedMax
        );
        assert_eq!(
            checker.observe(evidence(ANSWER_SECS + 2, 56.0, 5000), t, false),
            Transition::None
        );
        assert_eq!(
            checker.observe(evidence(ANSWER_SECS + 4, 55.0, 5000), t, false),
            Transition::Restored
        );
    }

    #[test]
    fn zero_rpm_under_a_real_command_is_a_stall_only_after_a_while() {
        let mut watch = ZeroRpmWatch::default();
        assert!(!watch.observe(0, Some(200), Some(0)));
        assert!(!watch.observe(STALL_SECS - 1, Some(200), Some(0)));
        assert!(watch.observe(STALL_SECS, Some(200), Some(0)));
        assert!(!watch.observe(STALL_SECS + 2, Some(200), Some(1800)));
        assert!(
            !watch.observe(100, Some(60), Some(0)),
            "a low command may stop"
        );
        assert!(
            !watch.observe(200, Some(200), None),
            "unreadable is not zero"
        );
    }
}
