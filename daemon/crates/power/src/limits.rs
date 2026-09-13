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

/// The powercap tree, or a fixture standing in for it
/// (`PYREN_POWERCAP`). Writing a real RAPL zone from a test would cap the
/// developer's own CPU, and reverting it afterwards is not something a
/// failed assertion can be relied on to do.
fn powercap_root() -> PathBuf {
    std::env::var_os("PYREN_POWERCAP")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(POWERCAP))
}

/// The two knobs for turbo, whichever this CPU has. Both live under the
/// CPU root the backend already resolves, so one `PYREN_CPU_ROOT` moves
/// the whole fake machine rather than half of it.
fn no_turbo_path() -> PathBuf {
    crate::backend::cpu_root().join("intel_pstate/no_turbo")
}

fn boost_path() -> PathBuf {
    crate::backend::cpu_root().join("cpufreq/boost")
}

/// Never cap the package below this, whatever a percentage works out to.
/// A CPU that cannot draw a few watts is a machine that does not respond.
const FLOOR_UW: u64 = 5_000_000;

/// No laptop package limit is anywhere near this. A stored or reported
/// value above it is a corrupt file or a misread, never a ceiling to trust.
const MAX_SANE_UW: u64 = 500_000_000;

/// How far a limit read back may sit from the one written and still count
/// as taken. RAPL stores limits in fixed power units (1/8 W on Intel), so
/// the kernel rounds; a watt covers that and nothing a firmware lock does.
const READ_BACK_TOLERANCE_UW: u64 = 1_000_000;

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
    /// `/dev/cpu`, or the fixture standing in for it - where the package
    /// power-limit MSR can be read to see whether the firmware locked it.
    /// `None` means "do not look", which is what a test's hand-built paths
    /// want: they must not answer from the developer's own CPU.
    msr_root: Option<PathBuf>,
}

/// How the firmware's lock on the package limits was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LockSource {
    /// Bit 63 of `MSR_PKG_POWER_LIMIT` (0x610), read through `/dev/cpu/0/msr`.
    Msr,
    /// A `locked` attribute on the RAPL zone, where a kernel provides one.
    Sysfs,
}

/// `MSR_PKG_POWER_LIMIT`, and its lock bit: once set by the firmware, PL1
/// and PL2 cannot change until the next reset, and the kernel still accepts
/// a write to the powercap file - it just does nothing.
const MSR_PKG_POWER_LIMIT: u64 = 0x610;
const MSR_LOCK_BIT: u32 = 63;

const MSR_ROOT: &str = "/dev/cpu";

fn msr_root() -> PathBuf {
    std::env::var_os("PYREN_MSR_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(MSR_ROOT))
}

impl LimitPaths {
    pub fn discover() -> Self {
        Self {
            zone: find_package_zone(),
            turbo: find_turbo_knob(),
            msr_root: Some(msr_root()),
        }
    }

    pub fn has_limits(&self) -> bool {
        self.zone.is_some()
    }

    pub fn has_turbo(&self) -> bool {
        self.turbo.is_some()
    }
}

/// Whether the firmware has locked the package limits, and how that was
/// found - `None` when they are not locked *or* nothing could say.
///
/// Two places are asked, and neither is made to exist:
///
/// - a `locked` attribute on the zone, if this kernel has one;
/// - the MSR, only if `/dev/cpu/0/msr` is already there and readable. The
///   `msr` module is never loaded for this - loading a module to answer a
///   status question is a side effect nobody asked for.
///
/// When neither answers, a locked limit still shows up afterwards, as a
/// write that did not read back.
pub fn locked(paths: &LimitPaths) -> Option<LockSource> {
    let zone = paths.zone.as_deref()?;
    if let Ok(value) = fs::read_to_string(zone.join("locked")) {
        return (value.trim() == "1").then_some(LockSource::Sysfs);
    }
    let root = paths.msr_root.as_deref()?;
    let file = fs::File::open(root.join("0/msr")).ok()?;
    let mut raw = [0u8; 8];
    std::os::unix::fs::FileExt::read_exact_at(&file, &mut raw, MSR_PKG_POWER_LIMIT).ok()?;
    ((u64::from_le_bytes(raw) >> MSR_LOCK_BIT) & 1 == 1).then_some(LockSource::Msr)
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
        .ordered()
    }

    /// PL1 <= PL2 <= PL4, lowering the higher tier where they cross.
    ///
    /// A sustained limit above the boost limit is not a configuration any
    /// firmware ships, and what the CPU does with one is up to the
    /// microcode. Lowering rather than raising keeps the guarantee that
    /// nothing asks for more than it was given.
    pub fn ordered(self) -> Limits {
        let pl2 = match (self.pl2_uw, self.pl4_uw) {
            (Some(pl2), Some(pl4)) => Some(pl2.min(pl4)),
            (pl2, _) => pl2,
        };
        let ceiling = pl2.or(self.pl4_uw);
        let pl1 = match (self.pl1_uw, ceiling) {
            (Some(pl1), Some(ceiling)) => Some(pl1.min(ceiling)),
            (pl1, _) => pl1,
        };
        Limits {
            pl1_uw: pl1,
            pl2_uw: pl2,
            pl4_uw: self.pl4_uw,
        }
    }

    /// Values no machine could really have, dropped: nothing, or more than
    /// any laptop package draws. The floor is a watt rather than
    /// [`FLOOR_UW`], since a fanless part really can ship a lower PL1.
    pub fn without_absurd(self) -> Limits {
        let sane = |v: Option<u64>| v.filter(|v| (1_000_000..=MAX_SANE_UW).contains(v));
        Limits {
            pl1_uw: sane(self.pl1_uw),
            pl2_uw: sane(self.pl2_uw),
            pl4_uw: sane(self.pl4_uw),
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
    /// Every mode starts at the machine's own envelope, untouched.
    ///
    /// It is tempting to ship an opinion here - Eco at 45 %, Balanced at
    /// 75 %, and so on - and it would be wrong. **Every laptop has its own
    /// internal profiles, and their curves are not each other's.** A number
    /// that is a sensible Eco on one chassis is a thermally-throttled mess
    /// on the next, and this daemon has no way to know which it is looking
    /// at. Inventing one and applying it everywhere would be worse than
    /// doing nothing, because it would look deliberate.
    ///
    /// So out of the box a mode drives only the mechanisms the machine
    /// itself provides - its ACPI platform profile, power-profiles-daemon,
    /// the CPU's energy-performance hint - and the envelope is left where
    /// the firmware set it. The knobs are all here, and `power.setTuning`
    /// is how a value that someone actually measured on *this* machine gets
    /// in. Importing the Windows OMEN profile would be another (see
    /// `dev/TODO.md`); guessing is not.
    pub fn default_for(_mode: PowerMode) -> Self {
        Self {
            pl1_percent: 100,
            pl2_percent: 100,
            turbo: true,
        }
    }

    /// Whether this is still the untouched envelope - nothing anyone set.
    ///
    /// A mode whose tuning is default has no opinion about the limits or
    /// turbo, and must not write them: the firmware, power-profiles-daemon,
    /// TLP and auto-cpufreq all move them on their own, and writing "stock"
    /// back over each of those would be this daemon overriding a choice it
    /// was never asked to make.
    pub fn is_default(&self) -> bool {
        *self == Self::default_for(PowerMode::Balanced)
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
    let entries = fs::read_dir(powercap_root()).ok()?;
    let mut zones: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
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
    let no_turbo = no_turbo_path();
    if no_turbo.exists() {
        return Some(TurboKnob::NoTurbo(no_turbo));
    }
    let boost = boost_path();
    boost.exists().then_some(TurboKnob::Boost(boost))
}

fn read_uw(zone: &Path, constraint: u8) -> Option<u64> {
    fs::read_to_string(zone.join(format!("constraint_{constraint}_power_limit_uw")))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The three limits, the constraint index each is at, and its kernel name.
const CONSTRAINTS: [(&str, u8, &str); 3] = [
    ("PL1", 0, "long_term"),
    ("PL2", 1, "short_term"),
    ("PL4", 2, "peak_power"),
];

/// Where the constraint called `name` actually is in this zone.
///
/// The order in the table above is the usual one, not a promise: a zone
/// without a peak-power constraint, or a driver that lists them
/// differently, would otherwise have "PL4" written into whatever sits at
/// index 2. So the names are read, and a zone that names its constraints
/// but not this one has no such limit. Only a zone naming none of them
/// (an old kernel, or a test fixture) is taken at the usual order.
fn constraint_index(zone: &Path, name: &str, usual: u8) -> Option<u8> {
    let mut any_named = false;
    for index in 0..8u8 {
        let Ok(found) = fs::read_to_string(zone.join(format!("constraint_{index}_name"))) else {
            continue;
        };
        any_named = true;
        if found.trim() == name {
            return Some(index);
        }
    }
    (!any_named).then_some(usual)
}

/// `false` only when the zone says so: a disabled zone enforces nothing,
/// so a limit written there is a report of success that changed nothing.
fn zone_enabled(zone: &Path) -> bool {
    fs::read_to_string(zone.join("enabled")).map_or(true, |v| v.trim() != "0")
}

pub fn read(paths: &LimitPaths) -> Limits {
    let Some(zone) = paths.zone.as_deref() else {
        return Limits::default();
    };
    let at = |label: &str| {
        let (_, usual, name) = CONSTRAINTS.iter().find(|(l, _, _)| *l == label)?;
        read_uw(zone, constraint_index(zone, name, *usual)?)
    };
    Limits {
        pl1_uw: at("PL1"),
        pl2_uw: at("PL2"),
        pl4_uw: at("PL4"),
    }
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
/// `target`, with each limit it does not set taken from the machine - what
/// the machine would read if every write in `target` took.
fn target_with_current(paths: &LimitPaths, target: Limits) -> Limits {
    let now = read(paths);
    Limits {
        pl1_uw: target.pl1_uw.or(now.pl1_uw),
        pl2_uw: target.pl2_uw.or(now.pl2_uw),
        pl4_uw: target.pl4_uw.or(now.pl4_uw),
    }
}

pub fn apply(paths: &LimitPaths, target: Limits) -> (Vec<String>, Vec<String>) {
    let (mut applied, mut failed) = (Vec::new(), Vec::new());
    let Some(zone) = paths.zone.as_deref() else {
        return (applied, failed);
    };
    if target.is_empty() {
        return (applied, failed);
    }
    if !zone_enabled(zone) {
        failed.push("PL: the RAPL package zone is disabled, so no limit would be enforced".into());
        return (applied, failed);
    }
    // Checked before writing, and only when a write is actually needed: a
    // locked machine already at the target has nothing to report.
    if let Some(source) = locked(paths) {
        if read(paths) != target_with_current(paths, target) {
            failed.push(format!(
                "PL: locked by the firmware ({}), so the limits cannot change until reboot",
                match source {
                    LockSource::Msr => "MSR_PKG_POWER_LIMIT lock bit",
                    LockSource::Sysfs => "the RAPL zone reports locked",
                }
            ));
        }
        return (applied, failed);
    }

    for ((label, usual, name), wanted) in
        CONSTRAINTS
            .into_iter()
            .zip([target.pl1_uw, target.pl2_uw, target.pl4_uw])
    {
        let Some(wanted) = wanted else { continue };
        let Some(constraint) = constraint_index(zone, name, usual) else {
            continue;
        };
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
            // A limit the firmware locked, or clamped to its own range, is
            // accepted by the kernel and then reads back as something else.
            Ok(()) => match read_uw(zone, constraint) {
                Some(got) if got.abs_diff(wanted) <= READ_BACK_TOLERANCE_UW => {
                    applied.push(format!("{label}={}W", got / 1_000_000));
                }
                Some(got) => failed.push(format!(
                    "{label}: asked for {}W, the firmware kept {}W",
                    wanted / 1_000_000,
                    got / 1_000_000
                )),
                None => failed.push(format!("{label}: unreadable after writing it")),
            },
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
        Ok(()) if read_turbo(paths) == Some(enabled) => {
            Ok(format!("turbo={}", if enabled { "on" } else { "off" }))
        }
        Ok(()) => Err(format!(
            "turbo: asked for {}, the CPU kept it {}",
            if enabled { "on" } else { "off" },
            if enabled { "off" } else { "on" }
        )),
        Err(e) => Err(format!("turbo: {e}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u64 = 1_000_000;

    fn stock() -> Limits {
        Limits {
            pl1_uw: Some(77 * W),
            pl2_uw: Some(77 * W),
            pl4_uw: Some(168 * W),
        }
    }

    /// No mode ships an opinion about watts. Whose watts would they be?
    #[test]
    fn every_mode_starts_at_the_machines_own_envelope() {
        for mode in [
            PowerMode::Eco,
            PowerMode::Balanced,
            PowerMode::Performance,
            PowerMode::Unlimited,
        ] {
            assert_eq!(Tuning::default_for(mode).target(stock()), stock());
        }
    }

    #[test]
    fn a_tuning_someone_set_is_a_fraction_of_that_envelope() {
        let measured = Tuning {
            pl1_percent: 45,
            pl2_percent: 55,
            turbo: false,
        };
        let target = measured.target(stock());

        assert_eq!(target.pl1_uw, Some(34 * W + 650_000));
        assert_eq!(target.pl4_uw, stock().pl4_uw, "PL4 stays at stock");
    }

    /// The whole point of the ceiling: raising a limit past what the
    /// firmware shipped is overclocking, and is not something a mode does.
    #[test]
    fn nothing_may_ask_for_more_than_stock() {
        let greedy = Limits {
            pl1_uw: Some(200 * W),
            pl2_uw: Some(200 * W),
            pl4_uw: Some(500 * W),
        };
        assert_eq!(greedy.clamp_to_stock(stock()), stock());
    }

    #[test]
    fn a_percentage_that_works_out_to_nothing_is_floored() {
        let tiny = Tuning {
            pl1_percent: 1,
            pl2_percent: 1,
            turbo: false,
        };
        let clamped = tiny.target(stock()).clamp_to_stock(stock());
        assert_eq!(clamped.pl1_uw, Some(FLOOR_UW));
    }

    /// A machine whose stock was never captured must not be written to at
    /// all - there is nothing to be sure we are staying under.
    #[test]
    fn without_a_recorded_stock_value_nothing_is_commanded() {
        let target = Limits {
            pl1_uw: Some(30 * W),
            ..Default::default()
        };
        assert!(target.clamp_to_stock(Limits::default()).is_empty());
    }

    /// Turbo is a behaviour choice, not a measurement, so it is not one
    /// this daemon makes for anyone either.
    #[test]
    fn no_mode_gives_up_turbo_unless_someone_says_so() {
        for mode in [
            PowerMode::Eco,
            PowerMode::Balanced,
            PowerMode::Performance,
            PowerMode::Unlimited,
        ] {
            assert!(Tuning::default_for(mode).turbo);
        }
    }

    #[test]
    fn tuning_round_trips_through_the_mode_table() {
        let mut table = ModeTuning::default();
        let custom = Tuning {
            pl1_percent: 60,
            pl2_percent: 70,
            turbo: false,
        };
        table.set(PowerMode::Balanced, custom);

        assert_eq!(table.get(PowerMode::Balanced), custom);
        assert_eq!(
            table.get(PowerMode::Eco),
            Tuning::default_for(PowerMode::Eco)
        );
    }

    /// Writing a limit the machine already holds is a no-op that, on an
    /// unprivileged daemon, would be reported as a permission failure.
    #[test]
    fn a_limit_that_is_already_set_is_not_written_again() {
        let dir = std::env::temp_dir().join(format!("pyren-rapl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("constraint_0_power_limit_uw"),
            (45 * W).to_string(),
        )
        .unwrap();
        let paths = LimitPaths {
            zone: Some(dir.clone()),
            turbo: None,
            msr_root: None,
        };

        let (applied, failed) = apply(
            &paths,
            Limits {
                pl1_uw: Some(45 * W),
                ..Default::default()
            },
        );
        assert!(applied.is_empty() && failed.is_empty(), "nothing to do");

        let (applied, _) = apply(
            &paths,
            Limits {
                pl1_uw: Some(30 * W),
                ..Default::default()
            },
        );
        assert_eq!(applied, vec!["PL1=30W".to_string()]);
    }

    fn zone_fixture(tag: &str) -> (PathBuf, LimitPaths) {
        let dir = std::env::temp_dir().join(format!("pyren-rapl-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let paths = LimitPaths {
            zone: Some(dir.clone()),
            turbo: None,
            msr_root: None,
        };
        (dir, paths)
    }

    /// Constraints are found by name, so a zone that lists them in another
    /// order gets each limit in its own slot - and one that has no peak
    /// power constraint gets no PL4 written anywhere.
    #[test]
    fn limits_are_written_to_the_constraint_that_carries_their_name() {
        let (dir, paths) = zone_fixture("names");
        for (index, name, value) in [(0, "short_term", 90), (1, "long_term", 60)] {
            fs::write(dir.join(format!("constraint_{index}_name")), name).unwrap();
            fs::write(
                dir.join(format!("constraint_{index}_power_limit_uw")),
                (value * W).to_string(),
            )
            .unwrap();
        }

        assert_eq!(
            read(&paths),
            Limits {
                pl1_uw: Some(60 * W),
                pl2_uw: Some(90 * W),
                pl4_uw: None,
            }
        );

        let (applied, failed) = apply(
            &paths,
            Limits {
                pl1_uw: Some(40 * W),
                pl2_uw: Some(90 * W),
                pl4_uw: Some(150 * W),
            },
        );
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(applied, vec!["PL1=40W".to_string()]);
        assert_eq!(
            fs::read_to_string(dir.join("constraint_1_power_limit_uw")).unwrap(),
            (40 * W).to_string()
        );
        assert!(!dir.join("constraint_2_power_limit_uw").exists());
    }

    /// Firmware that set the MSR lock bit: the kernel would take the write
    /// and change nothing, so nothing is written and the reason is said.
    #[test]
    fn a_locked_package_limit_is_reported_and_not_written() {
        let (dir, mut paths) = zone_fixture("msr-locked");
        let msr = dir.join("msr-root");
        fs::create_dir_all(msr.join("0")).unwrap();
        let file = fs::File::create(msr.join("0/msr")).unwrap();
        let register: u64 = (1 << 63) | 0x00dd_8268;
        std::os::unix::fs::FileExt::write_all_at(&file, &register.to_le_bytes(), 0x610).unwrap();
        paths.msr_root = Some(msr.clone());
        fs::write(
            dir.join("constraint_0_power_limit_uw"),
            (45 * W).to_string(),
        )
        .unwrap();

        assert_eq!(locked(&paths), Some(LockSource::Msr));
        let ask = Limits {
            pl1_uw: Some(30 * W),
            ..Default::default()
        };
        let (applied, failed) = apply(&paths, ask);
        assert!(applied.is_empty());
        assert_eq!(failed.len(), 1);
        assert!(failed[0].contains("locked"), "{failed:?}");
        assert_eq!(
            fs::read_to_string(dir.join("constraint_0_power_limit_uw")).unwrap(),
            (45 * W).to_string()
        );

        // Already where it was asked to be: locked, but nothing to report.
        let (_, failed) = apply(
            &paths,
            Limits {
                pl1_uw: Some(45 * W),
                ..Default::default()
            },
        );
        assert!(failed.is_empty());

        // The lock bit clear is not a lock.
        std::os::unix::fs::FileExt::write_all_at(&file, &0x00dd_8268u64.to_le_bytes(), 0x610)
            .unwrap();
        assert_eq!(locked(&paths), None);
        // And a kernel attribute, where there is one, answers first.
        fs::write(dir.join("locked"), "1").unwrap();
        assert_eq!(locked(&paths), Some(LockSource::Sysfs));
    }

    #[test]
    fn a_missing_msr_device_is_not_a_lock() {
        let (dir, mut paths) = zone_fixture("msr-missing");
        paths.msr_root = Some(dir.join("no-msr-module"));
        assert_eq!(locked(&paths), None);
    }

    #[test]
    fn a_disabled_zone_is_reported_rather_than_written() {
        let (dir, paths) = zone_fixture("disabled");
        fs::write(dir.join("enabled"), "0").unwrap();
        fs::write(
            dir.join("constraint_0_power_limit_uw"),
            (45 * W).to_string(),
        )
        .unwrap();

        let (applied, failed) = apply(
            &paths,
            Limits {
                pl1_uw: Some(30 * W),
                ..Default::default()
            },
        );
        assert!(applied.is_empty());
        assert_eq!(failed.len(), 1);
        assert_eq!(
            fs::read_to_string(dir.join("constraint_0_power_limit_uw")).unwrap(),
            (45 * W).to_string()
        );
    }

    #[test]
    fn crossed_limits_are_lowered_into_order() {
        let crossed = Limits {
            pl1_uw: Some(90 * W),
            pl2_uw: Some(200 * W),
            pl4_uw: Some(150 * W),
        };
        assert_eq!(
            crossed.ordered(),
            Limits {
                pl1_uw: Some(90 * W),
                pl2_uw: Some(150 * W),
                pl4_uw: Some(150 * W),
            }
        );
        let sustained_above_boost = Limits {
            pl1_uw: Some(80 * W),
            pl2_uw: Some(60 * W),
            pl4_uw: None,
        };
        assert_eq!(sustained_above_boost.ordered().pl1_uw, Some(60 * W));
    }

    #[test]
    fn an_absurd_limit_is_not_a_limit() {
        let absurd = Limits {
            pl1_uw: Some(9_000 * W),
            pl2_uw: Some(0),
            pl4_uw: Some(168 * W),
        };
        assert_eq!(
            absurd.without_absurd(),
            Limits {
                pl1_uw: None,
                pl2_uw: None,
                pl4_uw: Some(168 * W),
            }
        );
    }

    #[test]
    fn only_the_shipped_tuning_counts_as_default() {
        assert!(Tuning::default_for(PowerMode::Eco).is_default());
        assert!(!Tuning {
            turbo: false,
            ..Tuning::default_for(PowerMode::Eco)
        }
        .is_default());
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
