//! The parts of the compatibility report that are not about fans.
//!
//! `fan.diagnose` answers "can this machine's fans be driven" in detail,
//! and it lives in the `fan` module because the app asks it too. The other
//! two questions - **can the power envelope be moved**, and **is there any
//! lighting here** - have no such caller: nothing in the app needs a
//! self-test for them, and putting one in each module would be two more
//! diagnostic surfaces to keep in step with the shell script.
//!
//! So they live here, in the tool, built out of the same [`Check`] shape
//! the fan section uses. One check type, one renderer, one JSON schema.
//!
//! Everything in this file is **read-only**. The lighting section does put
//! one question to the firmware - an ACPI *read* command, the same one the
//! daemon uses at startup - and only when `/proc/acpi/call` is already
//! there. It never loads a kernel module and never writes a colour.

use pyren_fan::diagnostics::{Check, CheckStatus};
use pyren_power::PowerSurface;
use pyren_rgb::probe::Probe;
use serde::Serialize;

/// One group of checks with a sentence saying what they add up to.
///
/// No per-section verdict: the machine gets **one** verdict, and it is
/// `system.compatibility`, derived from what each module found it could
/// do. A second verdict per section is how a tool ends up disagreeing
/// with itself in the same output - which is the mistake that killed the
/// board list (`dev/FINDINGS.md`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub summary: String,
    pub checks: Vec<Check>,
}

// --- power -------------------------------------------------------------

pub fn power(surface: &PowerSurface) -> Section {
    let checks =
        vec![mechanisms_check(surface), envelope_check(surface), turbo_check(surface)];

    let summary = if surface.mechanisms.is_empty() {
        "No power-mode mechanism answered, so the modes would have nothing to \
         drive. The envelope, if there is one, can still be set directly."
            .to_string()
    } else {
        format!(
            "Power modes are available through {}.",
            join(&surface.mechanisms.iter().map(String::as_str).collect::<Vec<_>>())
        )
    };

    Section { summary, checks }
}

fn mechanisms_check(surface: &PowerSurface) -> Check {
    const ID: &str = "power-mechanisms";
    const TITLE: &str = "Power-mode mechanisms";

    if surface.mechanisms.is_empty() {
        return Check::new(
            ID,
            TITLE,
            CheckStatus::Warn,
            "none - no ACPI platform profile, no power-profiles-daemon, no EPP hint",
        )
        .with_remedy(
            "This is normal on a desktop. On a laptop, power-profiles-daemon is the \
             usual provider: install and enable it (systemctl enable --now \
             power-profiles-daemon).",
        );
    }

    let detail = match &surface.platform_profile {
        Some(active) if !surface.platform_profile_choices.is_empty() => format!(
            "{} (platform profile {active}, choices: {})",
            join(&surface.mechanisms.iter().map(String::as_str).collect::<Vec<_>>()),
            surface.platform_profile_choices.join(", ")
        ),
        _ => join(&surface.mechanisms.iter().map(String::as_str).collect::<Vec<_>>()),
    };
    Check::new(ID, TITLE, CheckStatus::Pass, detail)
}

fn envelope_check(surface: &PowerSurface) -> Check {
    const ID: &str = "power-envelope";
    const TITLE: &str = "Package power envelope";

    if surface.limits.is_empty() {
        return Check::new(
            ID,
            TITLE,
            CheckStatus::Warn,
            "no RAPL package zone, so PL1/PL2 cannot be read or set",
        );
    }
    Check::new(ID, TITLE, CheckStatus::Pass, format!("PL1 {}, PL2 {}", watts(surface.limits.pl1_uw), watts(surface.limits.pl2_uw)))
}

fn turbo_check(surface: &PowerSurface) -> Check {
    const ID: &str = "power-turbo";
    const TITLE: &str = "Turbo / boost switch";

    if surface.has_turbo {
        Check::new(ID, TITLE, CheckStatus::Pass, "exposed, so turbo can be switched per mode")
    } else {
        Check::new(ID, TITLE, CheckStatus::Warn, "not exposed; modes leave turbo alone")
    }
}

// --- lighting ----------------------------------------------------------

pub fn lighting(probe: &Probe) -> Section {
    // Both paths, always, even when neither is here. Which one a machine
    // has is not decided by its model name, so "no lighting" as a single
    // line answers the question for neither of them.
    let mut checks = vec![per_key_check(probe)];
    // And one check per dialect, because there is no single OMEN lighting
    // protocol: a machine that refuses all three is a different report
    // from one where two were never asked.
    checks.extend(probe.lighting.dialects.iter().map(dialect_check));

    let driven: Vec<&str> =
        probe.lighting.dialects.iter().filter(|d| d.available).map(|d| d.id).collect();
    let summary = match (driven.as_slice(), probe.per_key.present) {
        ([first, ..], _) => format!("The lights answered on '{first}' and can be driven."),
        ([], true) => {
            "A per-key RGB keyboard is attached; this build does not drive it.".to_string()
        }
        ([], false) => {
            "No lighting this project can drive was found. See the per-dialect checks for \
             whether that was established or merely not asked."
                .to_string()
        }
    };

    Section { summary, checks }
}

fn dialect_check(dialect: &pyren_rgb::dialect::DialectProbe) -> Check {
    let title = format!("Lighting dialect: {}", dialect.id);
    let id = format!("lighting-{}", dialect.id);

    if dialect.available {
        return Check::new(&id, title, CheckStatus::Pass, dialect.detail.clone());
    }
    if !dialect.asked {
        // Never asked. Reporting this as "no lighting" would be claiming
        // something nobody established.
        return Check::new(&id, title, CheckStatus::Skip, dialect.detail.clone());
    }
    // Asked, and told no. A fact about this dialect, not about the
    // machine - which is the whole reason there is more than one.
    Check::new(&id, title, CheckStatus::Warn, dialect.detail.clone()).with_remedy(
        "This is one of several ways of talking to these lights; the others are checked \
         separately. 'pyren-ctl rgb dialect <id>' forces one by hand.",
    )
}

fn per_key_check(probe: &Probe) -> Check {
    const ID: &str = "lighting-per-key";
    const TITLE: &str = "Per-key RGB keyboard";

    if probe.per_key.present {
        Check::new(
            ID,
            TITLE,
            CheckStatus::Warn,
            format!("{} is attached, but this build does not drive it", probe.per_key.usb_id),
        )
        .with_remedy(
            "The per-key path is deliberately unported until the key map's backspace \
             entry can be checked on real hardware; see docs/04-rgb-porting-review.md.",
        )
    } else {
        Check::new(
            ID,
            TITLE,
            CheckStatus::Skip,
            format!("no {} on this machine", probe.per_key.usb_id),
        )
    }
}


// --- shared ------------------------------------------------------------

fn watts(uw: Option<u64>) -> String {
    match uw {
        Some(uw) => format!("{} W", uw / 1_000_000),
        None => "-".to_string(),
    }
}

fn join(items: &[&str]) -> String {
    items.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_of(section: &Section, id: &str) -> CheckStatus {
        section.checks.iter().find(|c| c.id == id).expect("check should exist").status
    }

    /// The distinction the whole lighting section exists for: not being
    /// able to ask is not the same as having been told no, and a tool that
    /// prints "no lighting" for both is telling the user something nobody
    /// established.
    #[test]
    fn a_dialect_that_was_never_asked_about_is_not_reported_as_absent() {
        // Skipped for want of an interface: a `Skip`, and the detail must
        // not say the firmware refused anything.
        let not_asked = lighting(&probe_with([("fourZone", false, false)]));
        assert_eq!(status_of(&not_asked, "lighting-fourZone"), CheckStatus::Skip);
        assert!(!not_asked.checks[1].detail.contains("refused"));
        assert!(not_asked.checks[1].remedy.is_none(), "there is nothing to do about a skip");

        // Asked, and told no. A fact about *this dialect* - so it carries
        // the remedy that matters, which is to try another.
        let refused = lighting(&probe_with([("fourZone", true, false)]));
        assert_eq!(status_of(&refused, "lighting-fourZone"), CheckStatus::Warn);
        assert!(refused.checks[1].remedy.is_some(), "a refusal names the other dialects");

        let answered = lighting(&probe_with([("fourZone", true, true)]));
        assert_eq!(status_of(&answered, "lighting-fourZone"), CheckStatus::Pass);
        assert!(answered.summary.contains("fourZone"), "got: {}", answered.summary);
    }

    /// Every dialect appears whatever the machine has, because *which* one
    /// it speaks is exactly the question - and because the ids in this
    /// list are what `pyren-ctl rgb dialect <id>` takes.
    #[test]
    fn every_dialect_is_always_reported() {
        let section = lighting(&probe_with([
            ("kernelZones", false, false),
            ("fourZone", true, false),
            ("lightbar", true, false),
        ]));
        let ids: Vec<&str> = section.checks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            ["lighting-per-key", "lighting-kernelZones", "lighting-fourZone", "lighting-lightbar"]
        );
    }

    #[test]
    fn a_machine_with_no_power_mechanism_says_what_to_install() {
        let section = power(&PowerSurface {
            mechanisms: Vec::new(),
            platform_profile: None,
            platform_profile_choices: Vec::new(),
            limits: Default::default(),
            has_turbo: false,
        });
        assert_eq!(status_of(&section, "power-mechanisms"), CheckStatus::Warn);
        assert!(section.checks[0].remedy.is_some());
        assert_eq!(status_of(&section, "power-envelope"), CheckStatus::Warn);
    }

    #[test]
    fn an_envelope_is_reported_in_watts_not_microwatts() {
        let section = power(&PowerSurface {
            mechanisms: vec!["platform_profile".into()],
            platform_profile: Some("balanced".into()),
            platform_profile_choices: vec!["low-power".into(), "balanced".into()],
            limits: pyren_power::Limits {
                pl1_uw: Some(45_000_000),
                pl2_uw: Some(65_000_000),
                pl4_uw: None,
            },
            has_turbo: true,
        });
        assert_eq!(status_of(&section, "power-envelope"), CheckStatus::Pass);
        let detail = &section.checks[1].detail;
        assert!(detail.contains("45 W") && detail.contains("65 W"), "got: {detail}");
        assert!(section.checks[0].detail.contains("balanced"));
    }

    /// `(id, asked, available)` per dialect.
    fn probe_with<const N: usize>(dialects: [(&'static str, bool, bool); N]) -> Probe {
        let dialects: Vec<pyren_rgb::dialect::DialectProbe> = dialects
            .into_iter()
            .map(|(id, asked, available)| pyren_rgb::dialect::DialectProbe {
                id,
                transport: "fixture".into(),
                available,
                asked,
                detail: if available {
                    "answered a read of all four zones".into()
                } else if asked {
                    "the firmware refused".into()
                } else {
                    "skipped: nothing to ask through".into()
                },
            })
            .collect();
        Probe {
            supported: dialects.iter().any(|d| d.available),
            per_key: pyren_rgb::probe::PerKey {
                present: false,
                usb_id: "0d62:54bf",
                ported: false,
                detail: "none".into(),
            },
            lighting: pyren_rgb::probe::Lighting {
                present: dialects.iter().any(|d| d.available),
                hp_wmi: true,
                acpi_call: true,
                acpi_call_installed: true,
                dialects,
                command_answers: None,
                unreachable: None,
                detail: "fixture".into(),
            },
        }
    }
}
