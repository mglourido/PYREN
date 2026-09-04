//! What is asked for, what the machine will take, and how to get there.
//!
//! Everything in this file is a pure function over numbers, which is the
//! point: the interesting behaviour of an overclock is *how it is
//! approached*, and that has to be testable without a graphics card whose
//! failure mode is a frozen desktop.
//!
//! Three rules live here, and all three come from `dev/TODO.md`
//! §"GPU overclocking - last on purpose":
//!
//! - **Nothing is asked for that the driver has not advertised.** The
//!   ranges are read from the hardware ([`Ceiling`]), never guessed, and a
//!   request outside them is clamped with a note saying so rather than
//!   passed through to see what happens.
//! - **The climb is made in steps** ([`ramp`]), so a card that stops
//!   answering does so at a known offset and the daemon can go back to the
//!   last one that worked instead of guessing how far it had got.
//! - **Going back is one move.** Down is the safe direction; ramping a
//!   revert would spend seconds being careful about the one change that is
//!   never the risky one.

use pyren_core::{msg, Msg};
use serde::{Deserialize, Serialize};

/// How much the core offset may move in one step of the climb.
///
/// This does not *find* a stable offset - only a workload can do that, and
/// the normal case is an offset that survives a benchmark and dies in a
/// game. What the step buys is a bounded distance between "the card was
/// answering" and "the card stopped": each one is written, read back and
/// re-queried, so a failure names the value that caused it.
pub const CORE_STEP_MHZ: i32 = 15;

/// The same for memory, which moves in bigger numbers - a memory offset is
/// a transfer-rate offset, so its useful range is several times the core's.
pub const MEM_STEP_MHZ: i32 = 50;

/// An inclusive range the driver itself advertised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Range {
    pub min: i32,
    pub max: i32,
}

impl Range {
    pub fn new(min: i32, max: i32) -> Self {
        Self { min, max }
    }

    pub fn clamp(&self, value: i32) -> i32 {
        value.clamp(self.min, self.max)
    }

    pub fn contains(&self, value: i32) -> bool {
        value >= self.min && value <= self.max
    }
}

/// Clocks pinned to a range **inside** the stock one.
///
/// Not an overclock in the sense the offsets are: the card never runs a
/// frequency it was not shipped able to run. What it changes is how long it
/// is willing to stay there, which is why it belongs on this page rather
/// than in the power module - it is asked for by somebody who wants the
/// machine louder and faster than its own governor would choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockLock {
    pub min_mhz: i32,
    pub max_mhz: i32,
}

/// What the user asked this GPU to run at. Every field's zero value is
/// stock, so [`Target::default`] is "leave the card alone".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Target {
    pub core_offset_mhz: i32,
    pub mem_offset_mhz: i32,
    /// `None` means the driver picks the clock, which is the default and
    /// what a reset goes back to.
    pub core_clock: Option<ClockLock>,
}

impl Target {
    /// Whether this is the card as the firmware left it. The one state that
    /// needs no consent, no confirmation and no watchdog.
    pub fn is_stock(&self) -> bool {
        *self == Target::default()
    }
}

/// What this particular card advertised it will accept. A `None` is a knob
/// this card does not have - not a knob with a range of zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ceiling {
    pub core_offset: Option<Range>,
    pub mem_offset: Option<Range>,
    /// The frequencies the card lists as supported, ends inclusive.
    pub clock: Option<Range>,
}

/// A request, made safe: what will actually be written, and every way in
/// which that differs from what was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clamped {
    pub target: Target,
    /// One translatable line per difference. Empty when the request survived
    /// untouched - which is the case a UI should not decorate.
    pub notes: Vec<Msg>,
}

/// Cuts a request down to what the driver said it would take.
///
/// A knob the card does not have is *dropped* rather than refused: asking
/// for a memory offset on a card with none is a UI that has not caught up
/// with a probe, and losing the whole apply over it would also lose the
/// core offset that was perfectly askable.
pub fn clamp(request: Target, ceiling: &Ceiling) -> Clamped {
    let mut notes = Vec::new();
    let mut target = Target::default();

    match ceiling.core_offset {
        Some(range) => {
            target.core_offset_mhz = range.clamp(request.core_offset_mhz);
            if target.core_offset_mhz != request.core_offset_mhz {
                notes.push(msg!(
                    "overclock.clamp.coreOutOfRange",
                    {
                        "requested" => request.core_offset_mhz,
                        "min" => range.min,
                        "max" => range.max,
                        "used" => target.core_offset_mhz,
                    },
                    "core offset {requested} MHz is outside what the driver advertises \
                     ({min} to {max} MHz); using {used} MHz"
                ));
            }
        }
        None if request.core_offset_mhz != 0 => {
            notes.push(msg!(
                "overclock.clamp.noCoreOffset",
                "this GPU exposes no core-clock offset, so that part was ignored"
            ));
        }
        None => {}
    }

    match ceiling.mem_offset {
        Some(range) => {
            target.mem_offset_mhz = range.clamp(request.mem_offset_mhz);
            if target.mem_offset_mhz != request.mem_offset_mhz {
                notes.push(msg!(
                    "overclock.clamp.memOutOfRange",
                    {
                        "requested" => request.mem_offset_mhz,
                        "min" => range.min,
                        "max" => range.max,
                        "used" => target.mem_offset_mhz,
                    },
                    "memory offset {requested} MHz is outside what the driver advertises \
                     ({min} to {max} MHz); using {used} MHz"
                ));
            }
        }
        None if request.mem_offset_mhz != 0 => {
            notes.push(msg!(
                "overclock.clamp.noMemOffset",
                "this GPU exposes no memory offset, so that part was ignored"
            ));
        }
        None => {}
    }

    match (request.core_clock, ceiling.clock) {
        (Some(lock), Some(range)) => {
            let min = range.clamp(lock.min_mhz);
            // An upside-down pair is a slider that was dragged past its
            // partner, not an attack; the two are swapped rather than
            // refused, and only the clamp is worth a note.
            let max = range.clamp(lock.max_mhz).max(min);
            let clamped = ClockLock { min_mhz: min, max_mhz: max };
            if clamped != lock {
                notes.push(msg!(
                    "overclock.clamp.lockOutOfRange",
                    {
                        "reqMin" => lock.min_mhz,
                        "reqMax" => lock.max_mhz,
                        "min" => range.min,
                        "max" => range.max,
                        "usedMin" => min,
                        "usedMax" => max,
                    },
                    "clock lock {reqMin}-{reqMax} MHz is outside what this GPU supports \
                     ({min} to {max} MHz); using {usedMin}-{usedMax} MHz"
                ));
            }
            target.core_clock = Some(clamped);
        }
        (Some(_), None) => {
            notes.push(msg!(
                "overclock.clamp.noClockLock",
                "this GPU cannot have its clocks pinned, so that part was ignored"
            ));
        }
        (None, _) => {}
    }

    Clamped { target, notes }
}

/// The intermediate targets between where the card is and where it is being
/// asked to go, `to` included and `from` not.
///
/// Only the offsets are approached gradually. The clock lock is set once,
/// on the final step: it cannot ask for a frequency the card does not
/// support, so walking up to it would be ceremony rather than caution.
///
/// A revert does not go through here - see the module docs.
pub fn ramp(from: Target, to: Target) -> Vec<Target> {
    let mut steps = Vec::new();
    let mut current = Target { core_clock: from.core_clock, ..from };

    while current.core_offset_mhz != to.core_offset_mhz
        || current.mem_offset_mhz != to.mem_offset_mhz
    {
        current.core_offset_mhz = step_towards(current.core_offset_mhz, to.core_offset_mhz, CORE_STEP_MHZ);
        current.mem_offset_mhz = step_towards(current.mem_offset_mhz, to.mem_offset_mhz, MEM_STEP_MHZ);
        steps.push(current);
    }

    // Either the offsets were already where they should be, or the last
    // step landed on them; either way the clock lock still has to be said.
    match steps.last_mut() {
        Some(last) => last.core_clock = to.core_clock,
        None => steps.push(to),
    }
    steps
}

/// One step of at most `step` towards `to`, in whichever direction that is.
fn step_towards(from: i32, to: i32, step: i32) -> i32 {
    let distance = to - from;
    if distance.abs() <= step {
        to
    } else {
        from + step * distance.signum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ceiling() -> Ceiling {
        Ceiling {
            core_offset: Some(Range::new(-1000, 1000)),
            mem_offset: Some(Range::new(-2000, 6000)),
            clock: Some(Range::new(210, 3090)),
        }
    }

    #[test]
    fn a_request_inside_the_advertised_range_is_left_alone() {
        let request = Target { core_offset_mhz: 120, mem_offset_mhz: 400, core_clock: None };
        let clamped = clamp(request, &ceiling());
        assert_eq!(clamped.target, request);
        assert!(clamped.notes.is_empty(), "an untouched request must not be decorated");
    }

    /// The driver's own number is the ceiling. Passing a bigger one through
    /// to see what happens is the thing this whole module exists to avoid.
    #[test]
    fn a_request_past_the_advertised_range_is_cut_down_and_said_so() {
        let clamped = clamp(
            Target { core_offset_mhz: 5000, mem_offset_mhz: 0, core_clock: None },
            &ceiling(),
        );
        assert_eq!(clamped.target.core_offset_mhz, 1000);
        assert_eq!(clamped.notes.len(), 1);
        assert!(clamped.notes[0].contains("1000"));
    }

    /// Half a request is better than none: the knob that exists still gets
    /// written, and the one that does not is reported rather than refused.
    #[test]
    fn a_knob_this_card_lacks_is_dropped_not_refused() {
        let ceiling = Ceiling { mem_offset: None, ..ceiling() };
        let clamped = clamp(
            Target { core_offset_mhz: 90, mem_offset_mhz: 300, core_clock: None },
            &ceiling,
        );
        assert_eq!(clamped.target.core_offset_mhz, 90);
        assert_eq!(clamped.target.mem_offset_mhz, 0);
        assert_eq!(clamped.notes.len(), 1);
        assert!(clamped.notes[0].contains("memory offset"));
    }

    #[test]
    fn a_clock_lock_is_clamped_to_the_clocks_the_card_lists() {
        let clamped = clamp(
            Target { core_clock: Some(ClockLock { min_mhz: 100, max_mhz: 9000 }), ..Target::default() },
            &ceiling(),
        );
        assert_eq!(clamped.target.core_clock, Some(ClockLock { min_mhz: 210, max_mhz: 3090 }));
        assert_eq!(clamped.notes.len(), 1);
    }

    #[test]
    fn the_climb_moves_by_one_step_at_a_time() {
        let steps = ramp(Target::default(), Target { core_offset_mhz: 40, ..Target::default() });
        let offsets: Vec<i32> = steps.iter().map(|t| t.core_offset_mhz).collect();
        assert_eq!(offsets, vec![15, 30, 40]);
    }

    /// Both offsets are walked at once, each at its own pace, so the number
    /// of steps is the longer of the two climbs rather than their sum.
    #[test]
    fn the_two_offsets_climb_together() {
        let steps = ramp(
            Target::default(),
            Target { core_offset_mhz: 30, mem_offset_mhz: 200, core_clock: None },
        );
        assert_eq!(steps.len(), 4);
        assert_eq!(steps.last().unwrap().core_offset_mhz, 30);
        assert_eq!(steps.last().unwrap().mem_offset_mhz, 200);
    }

    #[test]
    fn the_climb_works_downwards_too() {
        let from = Target { core_offset_mhz: 45, ..Target::default() };
        let steps = ramp(from, Target::default());
        let offsets: Vec<i32> = steps.iter().map(|t| t.core_offset_mhz).collect();
        assert_eq!(offsets, vec![30, 15, 0]);
    }

    /// A lock with no offset change still has to be written once, or
    /// "pin the clocks and leave the offsets alone" would do nothing.
    #[test]
    fn a_clock_lock_on_its_own_is_still_one_step() {
        let lock = Some(ClockLock { min_mhz: 2000, max_mhz: 2500 });
        let steps = ramp(Target::default(), Target { core_clock: lock, ..Target::default() });
        assert_eq!(steps, vec![Target { core_clock: lock, ..Target::default() }]);
    }

    /// The lock is part of the destination, so it must not be written on
    /// the way up: pinning clocks high while the offset is still climbing
    /// would run the card at a frequency the final offset has not been
    /// tested at.
    #[test]
    fn the_clock_lock_arrives_with_the_last_step() {
        let lock = Some(ClockLock { min_mhz: 2000, max_mhz: 2500 });
        let steps = ramp(
            Target::default(),
            Target { core_offset_mhz: 30, core_clock: lock, ..Target::default() },
        );
        assert!(steps[..steps.len() - 1].iter().all(|s| s.core_clock.is_none()));
        assert_eq!(steps.last().unwrap().core_clock, lock);
    }

    #[test]
    fn stock_is_the_default_and_knows_it() {
        assert!(Target::default().is_stock());
        assert!(!Target { core_offset_mhz: 1, ..Target::default() }.is_stock());
    }
}
