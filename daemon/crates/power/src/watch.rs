//! Noticing when something other than this daemon moves what it set.
//!
//! There is no arbiter of power policy on Linux. The firmware profile,
//! the CPU's energy-performance hint, turbo and the package limits are
//! plain files, and anything running as root may write them: a power
//! manager re-applying its own policy on a charger event, the kernel
//! cycling the firmware profile on Fn+P, the desktop's battery menu going
//! through power-profiles-daemon, a script. Recognising every such writer
//! by name cannot be done, so this module does not try. It watches the
//! *files*, against what this daemon last left them at, and answers two
//! questions that do not depend on who the writer was:
//!
//! - **Did the firmware profile move to another mode?** Then the machine
//!   is in that mode, whoever put it there, and the daemon follows: its
//!   mode, the envelope that belongs to it and the fan curve drawn for it
//!   (through the `power.mode` announcement, source `external`). The OS
//!   power manager is deliberately *not* told - it is the likeliest writer,
//!   and answering a manager's change by pushing it back to that manager
//!   is how two programs end up in a loop.
//! - **Was anything else this daemon wrote changed afterwards?** Then it is
//!   recorded as overridden and reported, and *not* written again. A
//!   second writer that re-applies its policy will win in the end however
//!   often it is fought; the useful thing is to say so, so the user can
//!   decide which of the two should own the machine.
//!
//! A change within [`FIGHT_WINDOW`] of this daemon's own write is marked as
//! a revert: that is a program undoing what was just asked for, not a
//! person choosing something new.
//!
//! Only sysfs is read - no subprocess, no bus call - so looking every
//! [`INTERVAL`] costs next to nothing. `platform_profile` does support
//! `poll()`, but the reaction a second later is indistinguishable in use,
//! and a plain read works identically against a test fixture.

use std::time::Duration;

use serde::Serialize;

use crate::backend;
use crate::limits::{self, LimitPaths, Limits};
use crate::PowerMode;

/// How often the machine is looked at.
pub(crate) const INTERVAL: Duration = Duration::from_secs(1);

/// A change this soon after the daemon's own write reads as another
/// program undoing it. Long enough to cover auto-cpufreq's pass and a
/// charger event handled by udev; short enough that a person picking a
/// profile in the desktop's menu a while later is not called a revert.
pub(crate) const FIGHT_WINDOW: Duration = Duration::from_secs(15);

/// The knobs this daemon writes itself, as last read.
///
/// `None` means "not this daemon's to watch": never written, written and
/// refused, or absent from this machine. For the limits the same holds per
/// constraint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Knobs {
    pub platform_profile: Option<String>,
    pub energy_preference: Option<String>,
    pub turbo: Option<bool>,
    pub limits: Limits,
}

impl Knobs {
    /// Everything, as the machine has it now.
    pub(crate) fn read(paths: &LimitPaths) -> Self {
        Self {
            platform_profile: backend::read_platform_profile(),
            energy_preference: backend::read_energy_preference(),
            turbo: limits::read_turbo(paths),
            limits: limits::read(paths),
        }
    }
}

/// Something this daemon set that has since been changed by another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Override {
    /// `platform_profile`, `energy_performance_preference`, `turbo`, `PL1`...
    pub knob: &'static str,
    pub expected: String,
    pub found: String,
    /// Changed within [`FIGHT_WINDOW`] of being set.
    pub reverted: bool,
}

/// What one look at the machine found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Examined {
    /// The firmware profile now belongs to another mode: follow it.
    pub adopt: Option<(PowerMode, String)>,
    /// The firmware profile changed to another name for the *same* mode
    /// (`low-power` to `quiet`, say): take it as the new reference, and
    /// nothing else happened.
    pub same_mode_profile: Option<String>,
    pub overrides: Vec<Override>,
}

/// Compares the machine against what the daemon last left it at.
///
/// `mode` is the mode the daemon is in and `since` how long ago `expected`
/// was recorded. Pure, so every case is a unit test.
pub(crate) fn examine(mode: PowerMode, expected: &Knobs, now: &Knobs, since: Duration) -> Examined {
    let mut found = Examined::default();
    let reverted = since < FIGHT_WINDOW;
    let mut overridden = |knob: &'static str, expected: String, now: Option<String>| {
        found.overrides.push(Override {
            knob,
            expected,
            found: now.unwrap_or_else(|| "unreadable".to_string()),
            reverted,
        });
    };

    if let (Some(want), Some(seen)) = (&expected.platform_profile, &now.platform_profile) {
        if want != seen {
            match crate::mode_for_profile(seen) {
                Some(other) if same_family(other, mode) => {
                    found.same_mode_profile = Some(seen.clone());
                }
                Some(other) => {
                    found.adopt = Some((other, seen.clone()));
                    // Followed either way - it is where the machine is -
                    // but a revert is also worth saying out loud.
                    if reverted {
                        overridden("platform_profile", want.clone(), Some(seen.clone()));
                    }
                }
                // A name no mode maps onto: nothing to follow, but it is
                // no longer what this daemon set.
                None => overridden("platform_profile", want.clone(), Some(seen.clone())),
            }
        }
    }

    if let Some(want) = &expected.energy_preference {
        if now.energy_preference.as_ref() != Some(want) {
            overridden("energy_performance_preference", want.clone(), now.energy_preference.clone());
        }
    }

    if let Some(want) = expected.turbo {
        if now.turbo != Some(want) {
            overridden("turbo", on_off(want), now.turbo.map(on_off));
        }
    }

    for (label, want, seen) in [
        ("PL1", expected.limits.pl1_uw, now.limits.pl1_uw),
        ("PL2", expected.limits.pl2_uw, now.limits.pl2_uw),
        ("PL4", expected.limits.pl4_uw, now.limits.pl4_uw),
    ] {
        if let Some(want) = want {
            if seen != Some(want) {
                overridden(label, watts(want), seen.map(watts));
            }
        }
    }

    found
}

/// Performance and Unlimited share every firmware profile (see
/// `backend::pick_platform_profile`), so seeing `performance` while in
/// Unlimited is not a change of mode.
fn same_family(seen: PowerMode, mode: PowerMode) -> bool {
    let family = |m| if m == PowerMode::Unlimited { PowerMode::Performance } else { m };
    family(seen) == family(mode)
}

fn on_off(enabled: bool) -> String {
    if enabled { "on" } else { "off" }.to_string()
}

fn watts(uw: u64) -> String {
    if uw.is_multiple_of(1_000_000) {
        format!("{} W", uw / 1_000_000)
    } else {
        format!("{:.1} W", uw as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u64 = 1_000_000;
    const LONG_AGO: Duration = Duration::from_secs(600);
    const JUST_NOW: Duration = Duration::from_secs(2);

    fn eco() -> Knobs {
        Knobs {
            platform_profile: Some("low-power".into()),
            energy_preference: Some("power".into()),
            turbo: Some(false),
            limits: Limits { pl1_uw: Some(40 * W), pl2_uw: Some(50 * W), pl4_uw: None },
        }
    }

    #[test]
    fn a_machine_left_alone_has_nothing_to_report() {
        assert_eq!(examine(PowerMode::Eco, &eco(), &eco(), LONG_AGO), Examined::default());
    }

    /// Fn+P, the desktop's menu, TLP on a charger event: the machine is in
    /// another mode now, and the daemon follows rather than calling it an
    /// override - it happened long after the daemon's own write.
    #[test]
    fn a_firmware_profile_of_another_mode_is_followed() {
        let now = Knobs { platform_profile: Some("performance".into()), ..eco() };
        let found = examine(PowerMode::Eco, &eco(), &now, LONG_AGO);
        assert_eq!(found.adopt, Some((PowerMode::Performance, "performance".into())));
        assert!(found.overrides.is_empty(), "{:?}", found.overrides);
    }

    /// The same change seconds after the daemon's write is followed too -
    /// it is where the machine is - but reported as a revert.
    #[test]
    fn a_firmware_profile_undone_straight_away_is_followed_and_reported() {
        let now = Knobs { platform_profile: Some("balanced".into()), ..eco() };
        let found = examine(PowerMode::Eco, &eco(), &now, JUST_NOW);
        assert_eq!(found.adopt, Some((PowerMode::Balanced, "balanced".into())));
        assert_eq!(found.overrides.len(), 1);
        assert!(found.overrides[0].reverted);
        assert_eq!(found.overrides[0].knob, "platform_profile");
    }

    #[test]
    fn another_name_for_the_same_mode_is_not_a_change_of_mode() {
        let now = Knobs { platform_profile: Some("quiet".into()), ..eco() };
        let found = examine(PowerMode::Eco, &eco(), &now, JUST_NOW);
        assert_eq!(found.adopt, None);
        assert_eq!(found.same_mode_profile.as_deref(), Some("quiet"));
        assert!(found.overrides.is_empty());
    }

    #[test]
    fn performance_while_in_unlimited_is_not_a_change_of_mode() {
        let expected = Knobs { platform_profile: Some("balanced-performance".into()), ..Knobs::default() };
        let now = Knobs { platform_profile: Some("performance".into()), ..Knobs::default() };
        let found = examine(PowerMode::Unlimited, &expected, &now, LONG_AGO);
        assert_eq!(found.adopt, None);
    }

    #[test]
    fn a_firmware_profile_no_mode_maps_onto_is_reported_not_followed() {
        let now = Knobs { platform_profile: Some("custom".into()), ..eco() };
        let found = examine(PowerMode::Eco, &eco(), &now, LONG_AGO);
        assert_eq!(found.adopt, None);
        assert_eq!(found.overrides[0].found, "custom");
        assert!(!found.overrides[0].reverted);
    }

    /// The hint, turbo and the limits are reported and never followed:
    /// they do not say which mode the machine is in.
    #[test]
    fn every_other_knob_changed_is_reported() {
        let now = Knobs {
            platform_profile: Some("low-power".into()),
            energy_preference: Some("balance_power".into()),
            turbo: Some(true),
            limits: Limits { pl1_uw: Some(40 * W), pl2_uw: Some(64_500_000), pl4_uw: Some(9 * W) },
        };
        let found = examine(PowerMode::Eco, &eco(), &now, JUST_NOW);
        assert_eq!(found.adopt, None);
        let knobs: Vec<_> = found.overrides.iter().map(|o| (o.knob, o.found.as_str())).collect();
        assert_eq!(
            knobs,
            vec![("energy_performance_preference", "balance_power"), ("turbo", "on"), ("PL2", "64.5 W")],
            "PL4 was never the daemon's to watch"
        );
        assert!(found.overrides.iter().all(|o| o.reverted));
    }

    /// What the daemon never set is never watched - on a machine where TLP
    /// owns the hint, TLP changing it is nobody's business here.
    #[test]
    fn knobs_the_daemon_did_not_set_are_not_watched() {
        let expected = Knobs { platform_profile: Some("balanced".into()), ..Knobs::default() };
        let now = Knobs {
            platform_profile: Some("balanced".into()),
            energy_preference: Some("power".into()),
            turbo: Some(false),
            limits: Limits { pl1_uw: Some(W), pl2_uw: Some(W), pl4_uw: Some(W) },
        };
        assert_eq!(examine(PowerMode::Balanced, &expected, &now, JUST_NOW), Examined::default());
    }
}
