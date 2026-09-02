//! The power envelope: how much the CPU package is allowed to draw, and
//! whether it may turbo.
//!
//! This is the half of a power profile that the fans actually feel. The
//! ACPI platform profile and power-profiles-daemon change *scheduling*
//! preferences; the package power limit changes how many watts end up as
//! heat, which is what decides whether the fans have to spin up at all. On
//! a machine with no firmware platform profile - board 8D2F has none - it
//! is the only lever of the two that exists.
//!
//! Limits are read and written through the kernel's powercap interface
//! (`/sys/class/powercap`), which exposes Intel RAPL as three constraints:
//!
//! | constraint | name | what it is |
//! |---|---|---|
//! | 0 | `long_term` | PL1, the sustained ceiling |
//! | 1 | `short_term` | PL2, the boost ceiling |
//! | 2 | `peak_power` | PL4, the instantaneous ceiling |
//!
//! **Nothing here ever writes a value above the one the firmware shipped**
//! (see [`Limits::clamp_to_stock`]). Raising a limit past its stock value
//! is overclocking, which is a separate feature with separate consent.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::PowerMode;

const POWERCAP: &str = "/sys/class/powercap";
const NO_TURBO: &str = "/sys/devices/system/cpu/intel_pstate/no_turbo";
const BOOST: &str = "/sys/devices/system/cpu/cpufreq/boost";

/// Never cap the package below this, whatever a percentage works out to.
/// A CPU that cannot draw a few watts is a machine that does not respond.
const FLOOR_UW: u64 = 5_000_000;

/// The knob for turbo, whose polarity depends on which one exists.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TurboKnob {
    /// `intel_pstate/no_turbo`: 1 means *off*.
    NoTurbo(PathBuf),
    /// `cpufreq/boost`: 1 means *on*.
    Boost(PathBuf),
}

/// Where this machine's knobs are, or `None` for the ones it lacks.
#[derive(Debug, Clone, Default)]
pub struct LimitPaths {
    /// The package RAPL zone, e.g. `/sys/class/powercap/intel-rapl:0`.
    zone: Option<PathBuf>,
    turbo: Option<TurboKnob>,
}

impl LimitPaths {
    pub fn discover() -> Self {
        Self { zone: find_package_zone(), turbo: find_turbo_knob() }
    }

    pub fn has_limits(&self) -> bool {
        self.zone.is_some()
    }

    pub fn has_turbo(&self) -> bool {
        self.turbo.is_some()
    }
}

/// Package power limits in microwatts. `None` for a constraint this
/// machine does not expose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub pl1_uw: Option<u64>,
    pub pl2_uw: Option<u64>,
    pub pl4_uw: Option<u64>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        self.pl1_uw.is_none() && self.pl2_uw.is_none() && self.pl4_uw.is_none()
    }

    /// Caps every field at the machine's stock value and floors it at
    /// something survivable.
    ///
    /// The ceiling is stock rather than `constraint_*_max_power_uw`,
    /// because that attribute is not a ceiling: on the test laptop it reads
    /// 28 W while the firmware's own PL1 is 77 W. Believing it would cap a
    /// machine at a third of its designed power.
    pub fn clamp_to_stock(self, stock: Limits) -> Limits {
        fn clamp(value: Option<u64>, stock: Option<u64>) -> Option<u64> {
            let value = value?;
            // No stock value recorded means no idea what is safe, so
            // nothing is commanded.
            stock.map(|stock| value.clamp(FLOOR_UW.min(stock), stock))
        }
        Limits {
            pl1_uw: clamp(self.pl1_uw, stock.pl1_uw),
            pl2_uw: clamp(self.pl2_uw, stock.pl2_uw),
            pl4_uw: clamp(self.pl4_uw, stock.pl4_uw),
        }
    }
}

/// A mode's share of the machine's stock power envelope.
///
/// Percentages rather than watts so the same defaults are sensible on a
/// 15 W ultrabook and a 77 W gaming laptop. The user's own numbers, when
/// they set them, are stored the same way for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tuning {
    pub pl1_percent: u8,
    pub pl2_percent: u8,
    /// Whether the CPU may boost above its base frequency.
    pub turbo: bool,
}

impl Tuning {
    /// Where the four modes sit by default.
    ///
    /// Eco gives up turbo, which is the single biggest thing one can do for
    /// fan noise and battery: the short bursts it enables are what make a
    /// laptop audible while idling on a web page. The other three keep it.
    pub fn default_for(mode: PowerMode) -> Self {
        match mode {
            PowerMode::Eco => Self { pl1_percent: 45, pl2_percent: 55, turbo: false },
            PowerMode::Balanced => Self { pl1_percent: 75, pl2_percent: 90, turbo: true },
            PowerMode::Performance => Self { pl1_percent: 100, pl2_percent: 100, turbo: true },
            // Not "beyond stock" - there is no software way to exceed the
            // firmware's envelope that is not overclocking. Unlimited means
            // this daemon imposes no limit of its own.
            PowerMode::Unlimited => Self { pl1_percent: 100, pl2_percent: 100, turbo: true },
        }
    }

    /// The absolute limits this tuning asks for, given the machine's stock.
    pub fn target(&self, stock: Limits) -> Limits {
        fn scale(stock: Option<u64>, percent: u8) -> Option<u64> {
            Some(stock? / 100 * percent as u64)
        }
        Limits {
            pl1_uw: scale(stock.pl1_uw, self.pl1_percent),
            pl2_uw: scale(stock.pl2_uw, self.pl2_percent),
            // PL4 is the instantaneous ceiling and exists to keep the VRM
            // inside spec. Scaling it down with the others buys nothing a
            // lower PL1 has not already bought, so it is left at stock.
            pl4_uw: stock.pl4_uw,
        }
    }
}

/// Every mode's tuning, as persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeTuning {
    pub eco: Tuning,
    pub balanced: Tuning,
    pub performance: Tuning,
    pub unlimited: Tuning,
}

impl Default for ModeTuning {
    fn default() -> Self {
        Self {
            eco: Tuning::default_for(PowerMode::Eco),
            balanced: Tuning::default_for(PowerMode::Balanced),
            performance: Tuning::default_for(PowerMode::Performance),
            unlimited: Tuning::default_for(PowerMode::Unlimited),
        }
    }
}

impl ModeTuning {
    pub fn get(&self, mode: PowerMode) -> Tuning {
        match mode {
            PowerMode::Eco => self.eco,
            PowerMode::Balanced => self.balanced,
            PowerMode::Performance => self.performance,
            PowerMode::Unlimited => self.unlimited,
        }
    }

    pub fn set(&mut self, mode: PowerMode, tuning: Tuning) {
        match mode {
            PowerMode::Eco => self.eco = tuning,
            PowerMode::Balanced => self.balanced = tuning,
            PowerMode::Performance => self.performance = tuning,
            PowerMode::Unlimited => self.unlimited = tuning,
        }
    }
}

/// The package zone: the one whose `name` is `package-*`.
///
/// Sub-zones (`core`, `uncore`) and the mmio mirror of the same hardware
/// are deliberately skipped - writing the same limit twice through two
/// interfaces is how one ends up with a machine whose limit depends on the
/// order sysfs was enumerated in.
fn find_package_zone() -> Option<PathBuf> {
    let entries = fs::read_dir(POWERCAP).ok()?;
    let mut zones: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            // The mmio interface addresses the same package; one is enough.
            name.starts_with("intel-rapl:") && !name.contains("mmio")
        })
        .filter(|p| {
            fs::read_to_string(p.join("name"))
                .map(|n| n.trim().starts_with("package-"))
                .unwrap_or(false)
        })
        .collect();
    zones.sort();
    zones.into_iter().next()
}

fn find_turbo_knob() -> Option<TurboKnob> {
    let no_turbo = Path::new(NO_TURBO);
    if no_turbo.exists() {
        return Some(TurboKnob::NoTurbo(no_turbo.to_path_buf()));
    }
    let boost = Path::new(BOOST);
    boost.exists().then(|| TurboKnob::Boost(boost.to_path_buf()))
}

fn read_uw(zone: &Path, constraint: u8) -> Option<u64> {
    fs::read_to_string(zone.join(format!("constraint_{constraint}_power_limit_uw")))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn read(paths: &LimitPaths) -> Limits {
    let Some(zone) = paths.zone.as_deref() else {
        return Limits::default();
    };
    Limits { pl1_uw: read_uw(zone, 0), pl2_uw: read_uw(zone, 1), pl4_uw: read_uw(zone, 2) }
}

/// Whether turbo is currently allowed, or `None` when the machine has no
/// say in it.
pub fn read_turbo(paths: &LimitPaths) -> Option<bool> {
    match paths.turbo.as_ref()? {
        TurboKnob::NoTurbo(path) => Some(fs::read_to_string(path).ok()?.trim() == "0"),
        TurboKnob::Boost(path) => Some(fs::read_to_string(path).ok()?.trim() == "1"),
    }
}

/// Applies `target`, already clamped by the caller, and reports each write.
///
/// Values are read back rather than assumed: the powercap driver silently
/// clamps what it will not accept, so the only honest report of what a
/// machine is now set to is what it says afterwards.
pub fn apply(paths: &LimitPaths, target: Limits) -> (Vec<String>, Vec<String>) {
    let (mut applied, mut failed) = (Vec::new(), Vec::new());
    let Some(zone) = paths.zone.as_deref() else {
        return (applied, failed);
    };

    for (constraint, wanted, label) in
        [(0u8, target.pl1_uw, "PL1"), (1, target.pl2_uw, "PL2"), (2, target.pl4_uw, "PL4")]
    {
        let Some(wanted) = wanted else { continue };
        let path = zone.join(format!("constraint_{constraint}_power_limit_uw"));
        if !path.exists() {
            continue;
        }
        // Writing a value the hardware already holds achieves nothing and,
        // on an unprivileged daemon, turns a no-op into a reported failure.
        // PL4 hits this every time, since no profile scales it.
        if read_uw(zone, constraint) == Some(wanted) {
            continue;
        }
        match fs::write(&path, wanted.to_string()) {
            Ok(()) => {
                let got = read_uw(zone, constraint).unwrap_or(wanted);
                applied.push(format!("{label}={}W", got / 1_000_000));
            }
            Err(e) => failed.push(format!("{label}: {e}")),
        }
    }

    (applied, failed)
}

pub fn apply_turbo(paths: &LimitPaths, enabled: bool) -> Option<Result<String, String>> {
    let knob = paths.turbo.as_ref()?;
    // As with the limits: writing the state it is already in turns a no-op
    // into a reported permission failure on an unprivileged daemon.
    if read_turbo(paths) == Some(enabled) {
        return None;
    }
    let (path, value) = match knob {
        TurboKnob::NoTurbo(path) => (path, if enabled { "0" } else { "1" }),
        TurboKnob::Boost(path) => (path, if enabled { "1" } else { "0" }),
    };
    Some(match fs::write(path, value) {
        Ok(()) => Ok(format!("turbo={}", if enabled { "on" } else { "off" })),
        Err(e) => Err(format!("turbo: {e}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u64 = 1_000_000;

    fn stock() -> Limits {
        Limits { pl1_uw: Some(77 * W), pl2_uw: Some(77 * W), pl4_uw: Some(168 * W) }
    }

    #[test]
    fn eco_asks_for_a_fraction_of_the_machines_own_envelope() {
        let target = Tuning::default_for(PowerMode::Eco).target(stock());
        assert_eq!(target.pl1_uw, Some(34 * W + 650_000));
        assert_eq!(target.pl4_uw, stock().pl4_uw, "PL4 stays at stock");
    }

    #[test]
    fn performance_and_unlimited_ask_for_exactly_stock() {
        for mode in [PowerMode::Performance, PowerMode::Unlimited] {
            assert_eq!(Tuning::default_for(mode).target(stock()), stock());
        }
    }

    /// The whole point of the ceiling: raising a limit past what the
    /// firmware shipped is overclocking, and is not something a mode does.
    #[test]
    fn nothing_may_ask_for_more_than_stock() {
        let greedy = Limits { pl1_uw: Some(200 * W), pl2_uw: Some(200 * W), pl4_uw: Some(500 * W) };
        assert_eq!(greedy.clamp_to_stock(stock()), stock());
    }

    #[test]
    fn a_percentage_that_works_out_to_nothing_is_floored() {
        let tiny = Tuning { pl1_percent: 1, pl2_percent: 1, turbo: false };
        let clamped = tiny.target(stock()).clamp_to_stock(stock());
        assert_eq!(clamped.pl1_uw, Some(FLOOR_UW));
    }

    /// A machine whose stock was never captured must not be written to at
    /// all - there is nothing to be sure we are staying under.
    #[test]
    fn without_a_recorded_stock_value_nothing_is_commanded() {
        let target = Limits { pl1_uw: Some(30 * W), ..Default::default() };
        assert!(target.clamp_to_stock(Limits::default()).is_empty());
    }

    #[test]
    fn only_eco_gives_up_turbo_by_default() {
        assert!(!Tuning::default_for(PowerMode::Eco).turbo);
        for mode in [PowerMode::Balanced, PowerMode::Performance, PowerMode::Unlimited] {
            assert!(Tuning::default_for(mode).turbo);
        }
    }

    #[test]
    fn tuning_round_trips_through_the_mode_table() {
        let mut table = ModeTuning::default();
        let custom = Tuning { pl1_percent: 60, pl2_percent: 70, turbo: false };
        table.set(PowerMode::Balanced, custom);

        assert_eq!(table.get(PowerMode::Balanced), custom);
        assert_eq!(table.get(PowerMode::Eco), Tuning::default_for(PowerMode::Eco));
    }

    /// Writing a limit the machine already holds is a no-op that, on an
    /// unprivileged daemon, would be reported as a permission failure.
    #[test]
    fn a_limit_that_is_already_set_is_not_written_again() {
        let dir = std::env::temp_dir().join(format!("omen-hub-rapl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("constraint_0_power_limit_uw"), (45 * W).to_string()).unwrap();
        let paths = LimitPaths { zone: Some(dir.clone()), turbo: None };

        let (applied, failed) = apply(&paths, Limits { pl1_uw: Some(45 * W), ..Default::default() });
        assert!(applied.is_empty() && failed.is_empty(), "nothing to do");

        let (applied, _) = apply(&paths, Limits { pl1_uw: Some(30 * W), ..Default::default() });
        assert_eq!(applied, vec!["PL1=30W".to_string()]);
    }

    #[test]
    fn a_machine_with_no_powercap_reads_nothing_and_writes_nothing() {
        let paths = LimitPaths::default();
        assert!(read(&paths).is_empty());
        assert_eq!(apply(&paths, stock()), (Vec::new(), Vec::new()));
        assert!(apply_turbo(&paths, true).is_none());
        assert_eq!(read_turbo(&paths), None);
    }
}
