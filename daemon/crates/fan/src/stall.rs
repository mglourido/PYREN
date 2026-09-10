//! Noticing when the fans stall near Pyren's floor, and nudging the floor
//! up when it keeps happening.
//!
//! Pyren's floor ([`crate::calibration::sweep_floor`]) sits one step above
//! the slowest speed the fans were seen to hold when it was measured. That
//! edge is not fixed: dust in the intake, a warmer bearing, a colder start
//! all move it. When it moves up past the floor the fans stall at the
//! commanded speed and the controller kicks them back into motion - a
//! control tick that reads near zero, or one that reads a sudden jump back
//! up. One of those is tachometer noise. A handful inside half an hour is
//! the floor being wrong, and the cheap fix is to raise it one 100 rpm
//! step and record that it happened; a full `fan.calibrate` re-measures it
//! properly when the user gets to it.
//!
//! This only ever *raises* the floor, and never past the driver's own -
//! at that point Pyren's floor is the driver's and there is nothing left
//! to gain. It runs solely while Pyren's floor is the one in force and a
//! speed near it is being commanded; every other tick calls [`StallWatch::idle`].

use std::collections::VecDeque;

/// Faults older than this do not count toward "keeps happening". Half an
/// hour: long enough that a real, slow-moving problem accumulates, short
/// enough that a bad afternoon does not haunt next week.
pub const WINDOW_SECS: u64 = 1800;

/// Faults inside the window that mean the floor is wrong rather than the
/// tachometer being briefly confused.
pub const TRIGGER: usize = 3;

/// After a raise, this long before another can happen. The fans need to
/// settle at the new floor, and a burst of stalls while they do must not
/// stack three raises in ten seconds.
pub const RAISE_COOLDOWN_SECS: u64 = 300;

/// A measured speed below this fraction of the commanded one is a stall,
/// as a percentage. Half: a fan holding a speed reads within a ~150 rpm
/// tachometer step of it, never at less than half.
pub const STALL_PERCENT: i64 = 50;

/// A jump this big above the commanded speed between two ticks, while a
/// steady low speed is being commanded, is the controller restarting a
/// stalled fan. Matches [`crate::calibration::SWEEP_KICK_RPM`], which the
/// floor sweep looks for the same way.
pub const KICK_RPM: i64 = 300;

/// How far above the floor a commanded speed can be and still be worth
/// watching. Above this the fans do not stall and a low reading is
/// something else.
pub const NEAR_FLOOR_MARGIN_RPM: i64 = 300;

/// What a tick's observation came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// The fans are holding the commanded speed, or it is too soon to tell.
    Quiet,
    /// A stall or a kick, but not enough of them yet to act on.
    Fault { count: usize },
    /// Enough faults in the window: raise the floor a step.
    RaiseFloor { faults: usize },
}

/// The rolling record of recent stalls. In memory only, like the
/// hysteresis: a restart forgets it, and a persistent problem re-earns it.
#[derive(Debug, Default)]
pub struct StallWatch {
    faults: VecDeque<u64>,
    /// The measured speed on the previous watched tick, for spotting a kick.
    last_rpm: Option<i64>,
    /// When the floor was last raised, for the cooldown.
    last_raise: Option<u64>,
}

impl StallWatch {
    /// One control tick, while Pyren's floor is in force and a speed near
    /// it is being commanded. `expected_rpm` is what that commanded PWM
    /// implies; `measured_rpm` is the tachometer (the faster of the two
    /// fans); `steady` is false right after the target moved, when the
    /// fans are legitimately not there yet.
    pub fn observe(
        &mut self,
        now_secs: u64,
        expected_rpm: i64,
        measured_rpm: i64,
        steady: bool,
    ) -> Tick {
        self.prune(now_secs);
        let previous = self.last_rpm.replace(measured_rpm);

        if !steady || expected_rpm <= 0 {
            return Tick::Quiet;
        }

        let stalled = measured_rpm * 100 < expected_rpm * STALL_PERCENT;
        let kicked = previous.is_some_and(|prev| measured_rpm - prev > KICK_RPM)
            && measured_rpm > expected_rpm + KICK_RPM;
        if !stalled && !kicked {
            return Tick::Quiet;
        }

        self.faults.push_back(now_secs);
        let count = self.faults.len();
        if count < TRIGGER {
            return Tick::Fault { count };
        }
        // Enough faults, but a raise just happened and the fans are still
        // settling at the new floor. Spend them: a fresh three after the
        // cooldown is what earns the next raise, so it cannot over-shoot
        // on the settling wobble.
        self.faults.clear();
        if self.cooling(now_secs) {
            return Tick::Fault { count };
        }
        Tick::RaiseFloor { faults: count }
    }

    /// Not watching this tick - released, on the driver's floor, or well
    /// clear of it. The kick detector's memory is dropped so a fault from
    /// before a spell at speed is not paired with one after it; the fault
    /// trail itself ages out by time rather than being cleared, because a
    /// stall now and one twenty minutes ago are the same problem.
    pub fn idle(&mut self) {
        self.last_rpm = None;
    }

    /// Records that the caller acted on a [`Tick::RaiseFloor`], starting
    /// the cooldown.
    pub fn note_raised(&mut self, now_secs: u64) {
        self.last_raise = Some(now_secs);
    }

    /// Faults still inside the window.
    pub fn recent_faults(&self) -> usize {
        self.faults.len()
    }

    fn cooling(&self, now_secs: u64) -> bool {
        self.last_raise
            .is_some_and(|raised| now_secs.saturating_sub(raised) < RAISE_COOLDOWN_SECS)
    }

    fn prune(&mut self, now_secs: u64) {
        while self.faults.front().is_some_and(|&t| now_secs.saturating_sub(t) > WINDOW_SECS) {
            self.faults.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The commanded speed on board 8D2F's floor: 700 rpm asked, ~5300
    /// ceiling, so about 34/255.
    const EXPECTED: i64 = 700;

    fn watch() -> StallWatch {
        StallWatch::default()
    }

    #[test]
    fn a_fan_holding_its_speed_is_quiet() {
        let mut w = watch();
        for t in 0..10 {
            assert_eq!(w.observe(t, EXPECTED, 700, true), Tick::Quiet);
        }
        assert_eq!(w.recent_faults(), 0);
    }

    #[test]
    fn a_single_stall_is_noted_but_not_acted_on() {
        let mut w = watch();
        assert_eq!(w.observe(0, EXPECTED, 0, true), Tick::Fault { count: 1 });
    }

    /// Three stalls inside the window: the floor was measured too low.
    #[test]
    fn repeated_stalls_ask_for_a_raise() {
        let mut w = watch();
        assert_eq!(w.observe(0, EXPECTED, 0, true), Tick::Fault { count: 1 });
        assert_eq!(w.observe(60, EXPECTED, 200, true), Tick::Fault { count: 2 });
        assert_eq!(w.observe(120, EXPECTED, 0, true), Tick::RaiseFloor { faults: 3 });
        // And the count is cleared, so the next raise needs three fresh ones.
        assert_eq!(w.observe(121, EXPECTED, 0, true), Tick::Fault { count: 1 });
    }

    /// The kick the floor sweep also looks for: fan restarts itself and the
    /// tachometer jumps well past the commanded speed.
    #[test]
    fn a_kick_back_up_counts_as_a_fault() {
        let mut w = watch();
        assert_eq!(w.observe(0, EXPECTED, 700, true), Tick::Quiet);
        assert_eq!(w.observe(2, EXPECTED, 1600, true), Tick::Fault { count: 1 });
    }

    /// Rising toward a freshly raised target is not a kick.
    #[test]
    fn a_climb_that_is_not_steady_is_ignored() {
        let mut w = watch();
        assert_eq!(w.observe(0, 2000, 700, false), Tick::Quiet);
        assert_eq!(w.observe(2, 2000, 1600, false), Tick::Quiet);
        assert_eq!(w.recent_faults(), 0);
    }

    #[test]
    fn faults_age_out_of_the_window() {
        let mut w = watch();
        w.observe(0, EXPECTED, 0, true);
        w.observe(10, EXPECTED, 0, true);
        // Both are older than the window now.
        assert_eq!(w.observe(WINDOW_SECS + 20, EXPECTED, 0, true), Tick::Fault { count: 1 });
    }

    /// A burst of stalls right after a raise, while the fans settle at the
    /// new floor, must not stack a second and third raise.
    #[test]
    fn the_cooldown_stops_raises_stacking() {
        let mut w = watch();
        w.observe(0, EXPECTED, 0, true);
        w.observe(2, EXPECTED, 0, true);
        assert_eq!(w.observe(4, EXPECTED, 0, true), Tick::RaiseFloor { faults: 3 });
        w.note_raised(4);

        w.observe(6, EXPECTED, 0, true);
        w.observe(8, EXPECTED, 0, true);
        assert_eq!(
            w.observe(10, EXPECTED, 0, true),
            Tick::Fault { count: 3 },
            "still cooling down from the first raise"
        );

        // Past the cooldown, a fresh three do raise again.
        w.observe(RAISE_COOLDOWN_SECS + 10, EXPECTED, 0, true);
        w.observe(RAISE_COOLDOWN_SECS + 12, EXPECTED, 0, true);
        assert_eq!(
            w.observe(RAISE_COOLDOWN_SECS + 14, EXPECTED, 0, true),
            Tick::RaiseFloor { faults: 3 }
        );
    }

    /// `idle` forgets the previous reading so a fault before a spell at
    /// speed is not paired with the first reading after it as a "kick".
    #[test]
    fn idle_breaks_the_kick_pairing_but_keeps_the_trail() {
        let mut w = watch();
        w.observe(0, EXPECTED, 0, true);
        assert_eq!(w.recent_faults(), 1);
        w.idle();
        // A high reading right after coming back is not a kick from 0.
        assert_eq!(w.observe(30, EXPECTED, 700, true), Tick::Quiet);
        assert_eq!(w.recent_faults(), 1, "the earlier fault still stands");
    }
}
