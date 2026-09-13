//! Applying a power mode to the machine.
//!
//! **Two halves, applied separately**, because they are two different
//! machines' worth of policy and a user may well want one without the
//! other:
//!
//! 1. **The laptop's own profile** -
//!    `/sys/firmware/acpi/platform_profile`, the switch behind Fn+P. This
//!    is the firmware's, and changing it changes things no userspace knob
//!    reaches: the EC's temperature-to-RPM fan curve (which is why Eco
//!    makes the fans start *later*, not just slower), PCIe and other
//!    internal power states. Always applied.
//! 2. **The OS profile** - whichever power manager this system runs.
//!    Applied only when `applyToOsPowerProfile` is on, so the firmware
//!    profile can be changed without touching what the desktop thinks.
//!
//! The OS half is deliberately **delegated rather than reimplemented**.
//! A power manager already knows how to drive EPP, the governor and the
//! platform driver for the running system, and it re-applies its own idea
//! of them whenever it sees fit - on a charger event, on resume, or (for
//! auto-cpufreq) every few seconds. Writing those knobs ourselves on top of
//! one would not even lose a fight: it would win for a moment and then be
//! silently undone. So the manager is asked instead, in its own terms:
//!
//! | manager | asked through | Eco / Balanced / Performance |
//! |---|---|---|
//! | power-profiles-daemon, tuned-ppd, tlp-pd | the `PowerProfiles` D-Bus API | `power-saver` / `balanced` / `performance` |
//! | TLP 1.8+ without tlp-pd | `tlp <profile>` | `power-saver` / `balanced` / `performance` |
//! | auto-cpufreq | `auto-cpufreq --force` | `powersave` / `reset` / `performance` |
//!
//! The first two are alternatives (tlp-pd *is* TLP, behind the D-Bus API),
//! while auto-cpufreq is asked whenever its daemon runs, because nothing
//! else it shares a machine with would outlast its next pass. The per-CPU
//! energy-performance hint is only a *fallback*, for a machine with none
//! of them.
//!
//! **Nothing here may start a power manager.** The D-Bus API is activatable,
//! and power-profiles-daemon's unit `Conflicts=` with TLP and auto-cpufreq:
//! merely *asking* it for the current profile on a machine where it is
//! installed but disabled would start it, and systemd would stop the TLP or
//! auto-cpufreq the user actually chose. Every bus call therefore goes out
//! with auto-start off, and "nobody owns the name" means "not here".
//!
//! Every mechanism is best-effort and reports back what actually happened,
//! so the UI can say "applied via platform_profile" rather than claiming
//! success it can't verify. Writes need root; running the daemon
//! unprivileged surfaces a permission error instead of failing silently.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use pyren_core::process;
use serde::Serialize;

use crate::watch::Knobs;
use crate::PowerMode;

const PLATFORM_PROFILE: &str = "/sys/firmware/acpi/platform_profile";
const CPU_ROOT: &str = "/sys/devices/system/cpu";

/// The two names the power-profiles API answers to, newest first.
/// power-profiles-daemon 0.20+ serves both; tuned-ppd and tlp-pd are
/// reimplementations of the same API, and older ones may only have one.
const PROFILE_APIS: [BusApi; 2] = [
    BusApi {
        name: "org.freedesktop.UPower.PowerProfiles",
        path: "/org/freedesktop/UPower/PowerProfiles",
    },
    BusApi {
        name: "net.hadess.PowerProfiles",
        path: "/net/hadess/PowerProfiles",
    },
];

/// One place the power-profiles API may live. The interface is named like
/// the service on both, so one field serves as both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BusApi {
    name: &'static str,
    path: &'static str,
}

/// The firmware profile file. `PYREN_PLATFORM_PROFILE` points it at a
/// fixture, which is the only way to exercise the *writing* half of this
/// module at all: a test that ran against the real path would change the
/// developer's own machine, and on a laptop with no such file it could
/// not run in the first place.
///
/// The choices file is taken as its sibling rather than as a second
/// variable, because that is how sysfs lays them out and a fixture that
/// had to name both could name two that disagree.
fn platform_profile_path() -> PathBuf {
    std::env::var_os("PYREN_PLATFORM_PROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(PLATFORM_PROFILE))
}

fn platform_profile_choices_path() -> PathBuf {
    let path = platform_profile_path();
    match path.parent() {
        Some(dir) => dir.join("platform_profile_choices"),
        None => PathBuf::from("platform_profile_choices"),
    }
}

/// `/sys/devices/system/cpu`, or a fixture standing in for it.
///
/// Shared with [`crate::limits`], whose turbo knobs live under the same
/// root: one fake machine, not two that could drift apart.
pub(crate) fn cpu_root() -> PathBuf {
    std::env::var_os("PYREN_CPU_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(CPU_ROOT))
}

/// An external program, looked up on `PATH` - or, when `PYREN_TOOLS_DIR`
/// is set, *only* in that directory.
///
/// One variable for every program rather than one each, because the
/// failure it prevents is a test that forgot one: the developer's own
/// `tlp` or `auto-cpufreq` answering a fixture, and under `sudo cargo
/// test` being told to change profile. With the directory set, a program
/// the fixture did not provide simply is not installed.
fn tool(name: &str) -> PathBuf {
    match std::env::var_os("PYREN_TOOLS_DIR") {
        Some(dir) => PathBuf::from(dir).join(name),
        None => PathBuf::from(name),
    }
}

/// What the machine offers and what it is currently set to.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendState {
    pub platform_profile: Option<String>,
    pub platform_profile_choices: Vec<String>,
    /// The active profile of whatever serves the power-profiles D-Bus API
    /// (power-profiles-daemon, tuned-ppd, tlp-pd), if something does.
    pub power_profiles_daemon: Option<String>,
    /// TLP's active profile, when TLP is in charge and new enough (1.8) to
    /// have profiles at all.
    pub tlp: Option<String>,
    /// Whether auto-cpufreq's daemon is running.
    pub auto_cpufreq: bool,
    pub energy_preference: Option<String>,
    pub governor: Option<String>,
    /// Power managers running here that this module does not drive -
    /// tuned without tuned-ppd, system76-power, TLP - found through
    /// systemd. Each rewrites the CPU hint on its own schedule, so while
    /// any is active the hint is left alone and the conflict is reported.
    pub other_managers: Vec<String>,
    /// Mechanisms that could be used here, best first.
    pub available: Vec<&'static str>,
}

/// Outcome of one `setMode`, listing what was actually changed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    pub applied: Vec<String>,
    pub failed: Vec<String>,
    /// What the knobs this daemon writes itself read right afterwards -
    /// the reference [`crate::watch`] compares the machine against.
    #[serde(skip)]
    pub(crate) expected: Knobs,
    /// The firmware refused its profile, so everything this apply had
    /// already changed was put back: the mode did not happen, and nothing
    /// after it (the envelope) may be applied as if it had.
    #[serde(skip)]
    pub(crate) rolled_back: bool,
}

impl ApplyReport {
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}

/// The firmware profile, straight from sysfs.
pub(crate) fn read_platform_profile() -> Option<String> {
    read_trimmed(platform_profile_path())
}

/// The first CPU's energy-performance hint, straight from sysfs.
pub(crate) fn read_energy_preference() -> Option<String> {
    read_trimmed(cpu_root().join("cpu0/cpufreq/energy_performance_preference"))
}

pub fn read_state() -> BackendState {
    let platform_profile = read_platform_profile();
    let platform_profile_choices = read_trimmed(platform_profile_choices_path())
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();

    let mut available = Vec::new();
    if platform_profile.is_some() {
        available.push("platform_profile");
    }
    let ppd = find_power_profiles().map(|(_, profile)| profile);
    if ppd.is_some() {
        available.push("power-profiles-daemon");
    }
    // TLP behind tlp-pd is already covered by the line above, and asking
    // it twice would only apply its whole profile twice.
    let tlp = if ppd.is_none() { read_tlp() } else { None };
    if tlp.is_some() {
        available.push("tlp");
    }
    let auto_cpufreq = auto_cpufreq_running();
    if auto_cpufreq {
        available.push("auto-cpufreq");
    }
    let energy_preference = read_energy_preference();
    if energy_preference.is_some() {
        available.push("energy_performance_preference");
    }

    BackendState {
        platform_profile,
        platform_profile_choices,
        power_profiles_daemon: ppd,
        tlp,
        auto_cpufreq,
        energy_preference,
        governor: read_trimmed(cpu_root().join("cpu0/cpufreq/scaling_governor")),
        other_managers: other_managers(),
        available,
    }
}

/// One mechanism the machine offers, and what this mode would say to it.
///
/// Deciding is separated from doing so the decision can be unit-tested:
/// calling `apply` in a test would write to real firmware on any machine
/// that has some.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// The laptop's own profile.
    PlatformProfile(String),
    /// The OS profile, delegated to whatever serves the power-profiles API.
    PowerProfilesDaemon(&'static str),
    /// The OS profile, delegated to TLP directly.
    Tlp(&'static str),
    /// auto-cpufreq's override: `powersave`, `performance` or `reset`.
    AutoCpufreq(&'static str),
    /// Only when there is no power manager to delegate to.
    EnergyPreference(&'static str),
}

/// What applying `mode` to this machine would do.
///
/// The two halves are independent - the firmware profile is always part
/// of the answer, the OS profile only when the user says so - **but they
/// are not independent writers.** power-profiles-daemon 0.30 and later
/// ships its own `platform_profile` driver, so asking it for a profile
/// can itself write `/sys/firmware/acpi/platform_profile` as a side
/// effect, in whatever this machine's ACPI choices happen to map to in
/// *ppd's* opinion - which measurably disagrees with this module's own
/// mapping (see `pick_platform_profile`) on the reference laptop: asking
/// ppd for `power-saver` here lands the firmware on `balanced`, not the
/// `cool` this module would have chosen for Eco.
///
/// So the OS step is planned **before** the platform step, and applied in
/// that order too (`apply` does not reorder what `plan` hands it): our
/// own explicit write is always the last thing touching the file, and
/// wins regardless of what ppd's driver decided to do on the way past.
/// TLP (`PLATFORM_PROFILE_ON_*`) and auto-cpufreq (`platform_profile`)
/// can both be configured to write the same file, so every OS step goes
/// first, not just ppd's.
/// Losing this ordering silently reintroduces a race that a fixed
/// interval and a `sleep` will not reliably catch - `tests/profiles.rs`
/// has a machine whose fake `powerprofilesctl` writes its own, wrong,
/// platform profile as a side effect, precisely so a future reordering
/// fails a test rather than a user's fan curve.
pub(crate) fn plan(
    state: &BackendState,
    mode: PowerMode,
    os_profile: bool,
) -> (Vec<Step>, Vec<String>) {
    let (mut steps, mut problems) = (Vec::new(), Vec::new());

    if os_profile {
        // The power-profiles API first, because it is what the desktop's
        // own battery menu talks to; TLP directly only where nothing serves
        // it, since tlp-pd is TLP behind that same API.
        if state.power_profiles_daemon.is_some() {
            steps.push(Step::PowerProfilesDaemon(power_profiles_daemon_name(mode)));
        } else if state.tlp.is_some() {
            steps.push(Step::Tlp(power_profiles_daemon_name(mode)));
        }
        // Whatever else is here, auto-cpufreq rewrites the governor and
        // EPP on its next pass, so it has to be told as well.
        if state.auto_cpufreq {
            steps.push(Step::AutoCpufreq(auto_cpufreq_name(mode)));
        }
        // Writing the hint ourselves only where no manager would undo it -
        // including the ones this module cannot ask, which would silently
        // put their own value back on the next charger event.
        if steps.is_empty() && state.energy_preference.is_some() {
            if state.other_managers.is_empty() {
                steps.push(Step::EnergyPreference(energy_preference_name(mode)));
            } else {
                problems.push(format!(
                    "energy_performance_preference: left to {}, which manages it",
                    state.other_managers.join(", ")
                ));
            }
        }
    }

    if !state.platform_profile_choices.is_empty() {
        match pick_platform_profile(mode, &state.platform_profile_choices) {
            Some(profile) => steps.push(Step::PlatformProfile(profile)),
            None => problems.push("platform_profile: no choice matches this mode".to_string()),
        }
    }

    (steps, problems)
}

/// Applies the laptop's own profile, and optionally the OS's.
///
/// `os_profile` is the user's answer to "should changing the machine's
/// performance mode also change what my desktop thinks the power policy
/// is?". Both answers are legitimate, which is why it is a question and
/// not a fixed order of preference.
///
/// `before` is the machine as read *before* the caller took its lock (the
/// read starts processes, and must not stall everyone else behind it). It
/// doubles as the snapshot a refused firmware profile is rolled back to.
pub fn apply(before: &BackendState, mode: PowerMode, os_profile: bool) -> ApplyReport {
    let (steps, problems) = plan(before, mode, os_profile);
    let mut report = ApplyReport {
        applied: Vec::new(),
        failed: problems,
        expected: Knobs::default(),
        rolled_back: false,
    };
    // auto-cpufreq's override is not part of `BackendState` (reading it is
    // a Python start-up), so it is only captured when it is about to move.
    let auto_cpufreq_before = steps
        .iter()
        .any(|s| matches!(s, Step::AutoCpufreq(_)))
        .then(read_auto_cpufreq)
        .flatten();
    let mut done = Vec::new();

    for step in steps {
        match &step {
            Step::PlatformProfile(profile) => match write_platform_profile(profile) {
                Ok(()) => report.applied.push(format!("platform_profile={profile}")),
                Err(e) => {
                    // The firmware profile is what a mode *is* (it moves
                    // the EC's fan curve). An OS profile left in the new
                    // mode over a firmware still in the old one is a
                    // machine in neither, so the OS half is put back too.
                    report.failed.push(format!("platform_profile: {e}"));
                    report
                        .failed
                        .extend(roll_back(before, auto_cpufreq_before.as_deref(), &done));
                    report.applied.clear();
                    report.rolled_back = true;
                    break;
                }
            },
            Step::PowerProfilesDaemon(profile) => match set_power_profiles(profile) {
                Ok(()) => report
                    .applied
                    .push(format!("power-profiles-daemon={profile}")),
                Err(e) => report.failed.push(format!("power-profiles-daemon: {e}")),
            },
            Step::Tlp(profile) => match set_tlp(profile) {
                Ok(()) => report.applied.push(format!("tlp={profile}")),
                Err(e) => report.failed.push(format!("tlp: {e}")),
            },
            Step::AutoCpufreq(force) => match set_auto_cpufreq(force) {
                Ok(()) => report.applied.push(format!("auto-cpufreq={force}")),
                Err(e) => report.failed.push(format!("auto-cpufreq: {e}")),
            },
            Step::EnergyPreference(preference) => match write_energy_preference(preference) {
                Ok(count) => {
                    report.expected.energy_preference = read_energy_preference();
                    report.applied.push(format!(
                        "energy_performance_preference={preference} ({count} cpus)"
                    ))
                }
                Err(e) => report
                    .failed
                    .push(format!("energy_performance_preference: {e}")),
            },
        }
        done.push(step);
    }

    // Read back whether or not this call wrote it: whatever it says now is
    // where the machine was left, and a change from here is someone else's.
    report.expected.platform_profile = read_platform_profile();
    report
}

/// Writes the firmware profile and reads it back.
///
/// Some firmware accepts the write and keeps its old profile - HP refuses
/// `performance` on battery on some boards - and the kernel still reports
/// success. The file afterwards is the only answer that means anything.
fn write_platform_profile(profile: &str) -> Result<(), String> {
    fs::write(platform_profile_path(), profile).map_err(|e| e.to_string())?;
    match read_platform_profile() {
        Some(now) if now == profile => Ok(()),
        Some(now) => Err(format!("asked for {profile}, the firmware kept {now}")),
        None => Err(format!(
            "asked for {profile}, the firmware profile is unreadable afterwards"
        )),
    }
}

/// Puts back what `done` changed, newest first, from the snapshot taken
/// before the apply. Returns what could not be put back.
///
/// Best-effort by nature - the machine just refused one write - so every
/// miss is reported rather than treated as a reason to stop undoing.
fn roll_back(
    before: &BackendState,
    auto_cpufreq_before: Option<&str>,
    done: &[Step],
) -> Vec<String> {
    let mut failed = Vec::new();
    for step in done.iter().rev() {
        let result = match step {
            Step::PlatformProfile(_) => continue,
            Step::PowerProfilesDaemon(_) => match &before.power_profiles_daemon {
                Some(previous) => set_power_profiles(previous),
                None => continue,
            },
            Step::Tlp(_) => match &before.tlp {
                Some(previous) => set_tlp(previous),
                None => continue,
            },
            Step::AutoCpufreq(_) => match auto_cpufreq_before {
                Some("default") | None => set_auto_cpufreq("reset"),
                Some(previous) => set_auto_cpufreq(previous),
            },
            Step::EnergyPreference(_) => match &before.energy_preference {
                Some(previous) => {
                    write_all_cpus("energy_performance_preference", previous).map(|_| ())
                }
                None => continue,
            },
        };
        if let Err(e) = result {
            failed.push(format!("rolling back {step:?}: {e}"));
        }
    }
    // An OS manager's own platform driver may have moved the firmware file
    // on the way past; the snapshot's value is the one to leave behind.
    if let Some(previous) = &before.platform_profile {
        if read_platform_profile().as_ref() != Some(previous) {
            if let Err(e) = fs::write(platform_profile_path(), previous) {
                failed.push(format!("rolling back platform_profile: {e}"));
            }
        }
    }
    failed
}

/// Maps a mode onto whichever profile names this firmware actually offers.
///
/// The ACPI ABI defines a fixed vocabulary but firmware exposes only a
/// subset (HP laptops typically `low-power`/`balanced`/`performance`), so
/// each mode has an ordered list of acceptable names.
fn pick_platform_profile(mode: PowerMode, choices: &[String]) -> Option<String> {
    let preferences: &[&str] = match mode {
        PowerMode::Eco => &["low-power", "quiet", "cool", "balanced"],
        PowerMode::Balanced => &["balanced", "balanced-performance", "quiet"],
        PowerMode::Performance => &["balanced-performance", "performance", "balanced"],
        // There is no firmware profile beyond "performance"; what makes
        // Unlimited different is the manual fan and power limits the fan
        // module applies on top, not a different platform profile.
        PowerMode::Unlimited => &["performance", "balanced-performance"],
    };
    preferences
        .iter()
        .find(|wanted| choices.iter().any(|c| c == *wanted))
        .map(|wanted| wanted.to_string())
}

fn power_profiles_daemon_name(mode: PowerMode) -> &'static str {
    match mode {
        PowerMode::Eco => "power-saver",
        PowerMode::Balanced => "balanced",
        PowerMode::Performance | PowerMode::Unlimited => "performance",
    }
}

fn energy_preference_name(mode: PowerMode) -> &'static str {
    match mode {
        PowerMode::Eco => "power",
        PowerMode::Balanced => "balance_performance",
        PowerMode::Performance | PowerMode::Unlimited => "performance",
    }
}

/// What auto-cpufreq's `--force` calls each mode. It has no middle
/// setting, so Balanced hands the choice back to auto-cpufreq's own
/// automatic behaviour - which is what someone running it chose it for.
fn auto_cpufreq_name(mode: PowerMode) -> &'static str {
    match mode {
        PowerMode::Eco => "powersave",
        PowerMode::Balanced => "reset",
        PowerMode::Performance | PowerMode::Unlimited => "performance",
    }
}

// --- the power-profiles D-Bus API ---

/// How the power-profiles API was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfilesEndpoint {
    /// On the system bus, through `busctl`, at one of [`PROFILE_APIS`].
    Bus(BusApi),
    /// Through `powerprofilesctl`, and only where there is no `busctl` -
    /// that is, no systemd. Without systemd, bus activation of the service
    /// runs its `Exec=/bin/false` and fails, so this cannot start anything
    /// either; with systemd it could, which is why it is not used there.
    Cli,
}

/// Finds whatever serves the power-profiles API right now, and its
/// profile - **without starting it**. See the module docs for why that
/// matters more than it looks.
fn find_power_profiles() -> Option<(ProfilesEndpoint, String)> {
    for api in PROFILE_APIS {
        match busctl(&[
            "call",
            api.name,
            api.path,
            PROPERTIES,
            "Get",
            "ss",
            api.name,
            "ActiveProfile",
        ]) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return read_powerprofilesctl().map(|profile| (ProfilesEndpoint::Cli, profile));
            }
            Err(_) => return None,
            Ok(output) if output.status.success() => {
                if let Some(profile) =
                    parse_variant_string(&String::from_utf8_lossy(&output.stdout))
                {
                    return Some((ProfilesEndpoint::Bus(api), profile));
                }
            }
            // Nobody owns this name; the older one may still be served.
            Ok(_) => {}
        }
    }
    None
}

const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// `busctl` on the system bus, never auto-starting the destination.
///
/// `--auto-start=no` is honoured by `call` but *not* by `get-property`
/// (checked against systemd 261, which still asks the unit to start),
/// which is why reads go through `Properties.Get` by hand.
fn busctl(args: &[&str]) -> std::io::Result<std::process::Output> {
    process::output(
        Command::new(tool("busctl"))
            .args(["--system", "--auto-start=no"])
            .args(args),
    )
}

/// `v s "balanced"` - busctl's rendering of a string variant - to `balanced`.
fn parse_variant_string(output: &str) -> Option<String> {
    let value = output.trim().strip_prefix("v s ")?.trim().trim_matches('"');
    (!value.is_empty()).then(|| value.to_string())
}

fn read_active_profile(endpoint: ProfilesEndpoint) -> Option<String> {
    match endpoint {
        ProfilesEndpoint::Bus(api) => {
            let output = busctl(&[
                "call",
                api.name,
                api.path,
                PROPERTIES,
                "Get",
                "ss",
                api.name,
                "ActiveProfile",
            ])
            .ok()?;
            if !output.status.success() {
                return None;
            }
            parse_variant_string(&String::from_utf8_lossy(&output.stdout))
        }
        ProfilesEndpoint::Cli => read_powerprofilesctl(),
    }
}

fn request_profile(endpoint: ProfilesEndpoint, profile: &str) -> Result<(), String> {
    let output = match endpoint {
        ProfilesEndpoint::Bus(api) => busctl(&[
            "call",
            api.name,
            api.path,
            PROPERTIES,
            "Set",
            "ssv",
            api.name,
            "ActiveProfile",
            "s",
            profile,
        ]),
        ProfilesEndpoint::Cli => {
            process::output(Command::new(tool("powerprofilesctl")).args(["set", profile]))
        }
    }
    .map_err(|e| e.to_string())?;
    succeeded(&output)
}

fn read_powerprofilesctl() -> Option<String> {
    let output = process::output(Command::new(tool("powerprofilesctl")).arg("get")).ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

// --- TLP ---

/// TLP's active profile, from `tlp-stat -m`: `balanced/BAT`, possibly
/// followed by `(manual)`.
///
/// Only the three profile names are accepted. TLP before 1.8 has no
/// profiles to switch - `-m` there prints the power source, or nothing -
/// and a TLP that has not run this boot has no saved profile to print;
/// neither is something `tlp <profile>` could be asked to change.
fn read_tlp() -> Option<String> {
    let output = process::output(Command::new(tool("tlp-stat")).arg("-m")).ok()?;
    if !output.status.success() {
        return None;
    }
    parse_tlp_mode(&String::from_utf8_lossy(&output.stdout))
}

fn parse_tlp_mode(output: &str) -> Option<String> {
    let profile = output.split_whitespace().next()?.split('/').next()?;
    matches!(profile, "performance" | "balanced" | "power-saver").then(|| profile.to_string())
}

/// `tlp <profile>`, confirmed through `tlp-stat -m`.
///
/// TLP applies a profile under its own lock, and a charger event landing
/// at the same moment holds it: the command then says so and changes
/// nothing, which is exactly the kind of miss the second attempt is for.
fn set_tlp(profile: &str) -> Result<(), String> {
    set_and_confirm("TLP", profile, read_tlp, || {
        let output =
            process::output(Command::new(tool("tlp")).arg(profile)).map_err(|e| e.to_string())?;
        succeeded(&output)
    })
}

// --- auto-cpufreq ---

/// Whether auto-cpufreq's daemon is running.
///
/// A process check rather than `auto-cpufreq --get-state`, which answers
/// the same question but is a Python start-up - most of a second, on every
/// status read, on every machine that merely has it installed.
fn auto_cpufreq_running() -> bool {
    cached(&AUTO_CPUFREQ_CACHE, || {
        process::output(Command::new(tool("pgrep")).args(["-f", "auto-cpufreq.* --daemon"]))
            .is_ok_and(|output| output.status.success())
    })
}

/// auto-cpufreq's current override as `--get-state` prints it
/// (`default`, `powersave`, `performance`).
fn read_auto_cpufreq() -> Option<String> {
    let output = process::output(Command::new(tool("auto-cpufreq")).arg("--get-state")).ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Takes auto-cpufreq's override off, if its daemon is running: the
/// override is auto-cpufreq's own persisted state and outlives both pyren
/// and the switch that put it there.
pub(crate) fn release_auto_cpufreq() -> Option<Result<(), String>> {
    if !auto_cpufreq_running() || read_auto_cpufreq().as_deref() == Some("default") {
        return None;
    }
    Some(set_auto_cpufreq("reset"))
}

/// Units that manage the CPU hint and are not asked through anything this
/// module speaks. TLP is listed even though it can be asked directly: a
/// TLP before 1.8 has no profiles, and still rewrites the hint.
const OTHER_MANAGER_UNITS: [&str; 3] = ["tlp.service", "tuned.service", "system76-power.service"];

/// The [`OTHER_MANAGER_UNITS`] systemd reports as active.
///
/// No systemd, or a `systemctl` that fails, is "none found": this only
/// ever *withholds* a write, so not knowing must not invent a reason to.
fn other_managers() -> Vec<String> {
    cached(&OTHER_MANAGERS_CACHE, || {
        let Ok(output) = process::output(
            Command::new(tool("systemctl"))
                .arg("is-active")
                .args(OTHER_MANAGER_UNITS),
        ) else {
            return Vec::new();
        };
        // One line per unit, in order; the exit status is non-zero as soon
        // as any one is inactive, so it says nothing here.
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .zip(OTHER_MANAGER_UNITS)
            .filter(|(state, _)| state.trim() == "active")
            .map(|(_, unit)| unit.trim_end_matches(".service").to_string())
            .collect()
    })
}

/// How long a manager's presence is trusted before it is looked for again.
///
/// Every status read and every apply asks, and each answer is a process;
/// whether a daemon is installed and running does not change several times
/// a second.
const MANAGER_CACHE_TTL: Duration = Duration::from_secs(5);

type Cache<T> = OnceLock<Mutex<Option<(Instant, Option<OsString>, T)>>>;
static AUTO_CPUFREQ_CACHE: Cache<bool> = OnceLock::new();
static OTHER_MANAGERS_CACHE: Cache<Vec<String>> = OnceLock::new();

/// `compute`, remembered for [`MANAGER_CACHE_TTL`].
///
/// Keyed by `PYREN_TOOLS_DIR`, so a test that swaps the fake machine's
/// programs is never answered from the previous machine's.
fn cached<T: Clone>(cache: &'static Cache<T>, compute: impl FnOnce() -> T) -> T {
    let key = std::env::var_os("PYREN_TOOLS_DIR");
    // A fixture installs and removes managers mid-test; only a real system
    // is stable enough to cache.
    if key.is_some() {
        return compute();
    }
    let slot = cache.get_or_init(|| Mutex::new(None));
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, for_key, value)) = slot.as_ref() {
        if at.elapsed() < MANAGER_CACHE_TTL && *for_key == key {
            return value.clone();
        }
    }
    let value = compute();
    *slot = Some((Instant::now(), key, value.clone()));
    value
}

/// `auto-cpufreq --force`, confirmed through `--get-state`, which reports
/// a reset as `default`.
///
/// The override is auto-cpufreq's, and persists in its own state until
/// something resets it - which Balanced does.
fn set_auto_cpufreq(force: &str) -> Result<(), String> {
    let expected = if force == "reset" { "default" } else { force };
    set_and_confirm("auto-cpufreq", expected, read_auto_cpufreq, || {
        let output =
            process::output(Command::new(tool("auto-cpufreq")).arg(format!("--force={force}")))
                .map_err(|e| e.to_string())?;
        succeeded(&output)
    })
}

/// A command's failure as its own words: stderr if it said anything
/// there, stdout otherwise (TLP and auto-cpufreq both report on stdout).
fn succeeded(output: &std::process::Output) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let said = [&output.stderr, &output.stdout]
        .into_iter()
        .map(|stream| String::from_utf8_lossy(stream).trim().to_string())
        .find(|text| !text.is_empty());
    Err(said.unwrap_or_else(|| format!("exited with {}", output.status)))
}

/// How many times `set` is asked to land on the profile before this gives
/// up on it.
///
/// Found on the reference laptop: power-profiles-daemon 0.30's own
/// platform driver does not always get there in one call - going straight
/// from `performance` to `power-saver` reproducibly settles on `balanced`
/// instead, with ppd itself entirely out of pyren's picture (`ps
/// power-profiles-daemon`'s own client hits the same thing). A second
/// `set` for the same profile, immediately after, was observed to
/// succeed every time it was tried - so this is not pyren compensating
/// for a bug it understands, only pyren declining to call a transient
/// miss a settled answer before asking once more.
const OS_PROFILE_ATTEMPTS: u32 = 2;

/// Asks whatever serves the power-profiles API for `profile`.
fn set_power_profiles(profile: &str) -> Result<(), String> {
    let Some((endpoint, _)) = find_power_profiles() else {
        return Err("the power-profiles service is no longer running".to_string());
    };
    set_and_confirm(
        "power-profiles-daemon",
        profile,
        || read_active_profile(endpoint),
        || request_profile(endpoint, profile),
    )
}

/// Asks a power manager for `wanted`, and reads back what it actually
/// landed on rather than trusting the command's exit status - which is
/// the same principle [`crate::limits::apply`] applies to the power
/// envelope, and for the same reason: a mechanism that can silently not do
/// what it was asked needs to be checked, not assumed.
fn set_and_confirm(
    who: &str,
    wanted: &str,
    read: impl Fn() -> Option<String>,
    request: impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    let mut settled_on = None;

    for attempt in 1..=OS_PROFILE_ATTEMPTS {
        request()?;

        let seen = read();
        if seen.as_deref() == Some(wanted) {
            return Ok(());
        }
        settled_on = seen;

        // Not the last attempt: a moment for the manager's own state to
        // catch up to the call that just returned, before asking again.
        if attempt < OS_PROFILE_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    Err(match settled_on {
        Some(seen) => format!(
            "asked for {wanted}, {who} settled on {seen} (tried {OS_PROFILE_ATTEMPTS} times)"
        ),
        None => format!(
            "asked for {wanted}, {who} did not answer afterwards (tried {OS_PROFILE_ATTEMPTS} times)"
        ),
    })
}

/// CPU frequency drivers whose `energy_performance_preference` is the
/// hint this module means. Passive intel_pstate, amd-pstate in guided or
/// passive mode and the generic drivers either lack the file or treat it
/// as something else.
const EPP_DRIVERS: [&str; 2] = ["intel_pstate", "amd-pstate-epp"];

/// The CPU hint, written only where it means what it says, and read back
/// on every CPU.
fn write_energy_preference(value: &str) -> Result<usize, String> {
    let cpu0 = cpu_root().join("cpu0/cpufreq");
    match read_trimmed(cpu0.join("scaling_driver")) {
        Some(driver) if EPP_DRIVERS.contains(&driver.as_str()) => {}
        Some(driver) => return Err(format!("the {driver} driver has no usable hint")),
        None => return Err("the CPU frequency driver is unknown".to_string()),
    }
    // The performance governor pins the hint: intel_pstate refuses writes
    // with EBUSY on some CPUs and not others, leaving a mixed machine.
    if read_trimmed(cpu0.join("scaling_governor")).as_deref() == Some("performance") {
        return Err("the performance governor fixes the hint".to_string());
    }
    let offered = read_trimmed(cpu0.join("energy_performance_available_preferences"))
        .ok_or_else(|| "this CPU lists no available hints".to_string())?;
    if !offered.split_whitespace().any(|choice| choice == value) {
        return Err(format!(
            "{value} is not one of this CPU's hints ({offered})"
        ));
    }

    let count = write_all_cpus("energy_performance_preference", value)?;
    let mismatched = cpu_dirs()
        .into_iter()
        .filter_map(|(name, dir)| {
            let path = dir.join("cpufreq/energy_performance_preference");
            (path.exists() && read_trimmed(&path).as_deref() != Some(value)).then_some(name)
        })
        .collect::<Vec<_>>();
    if mismatched.is_empty() {
        Ok(count)
    } else {
        Err(format!(
            "asked for {value}, {} kept their own hint",
            mismatched.join(", ")
        ))
    }
}

/// `cpuN` directories under the CPU root, with their names.
fn cpu_dirs() -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(cpu_root()) else {
        return Vec::new();
    };
    let mut cpus: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let digits = name.strip_prefix("cpu")?;
            (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
                .then(|| (name, entry.path()))
        })
        .collect();
    cpus.sort();
    cpus
}

/// Writes one cpufreq attribute on every CPU, returning how many took it.
///
/// Partial success is normal on hybrid CPUs where some cores are offline,
/// so only a total failure is reported as an error.
fn write_all_cpus(attribute: &str, value: &str) -> Result<usize, String> {
    let root = cpu_root();
    let Ok(entries) = fs::read_dir(&root) else {
        return Err(format!("{} is unreadable", root.display()));
    };

    let mut written = 0;
    let mut last_error = None;
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("cpu") || !name[3..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let path = entry.path().join("cpufreq").join(attribute);
        if !path.exists() {
            continue;
        }
        match fs::write(&path, value) {
            Ok(()) => written += 1,
            Err(e) => last_error = Some(e.to_string()),
        }
    }

    match (written, last_error) {
        (0, Some(e)) => Err(e),
        (0, None) => Err("no cpu exposes this attribute".to_string()),
        (count, _) => Ok(count),
    }
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn eco_prefers_low_power_when_offered() {
        let available = choices(&["low-power", "balanced", "performance"]);
        assert_eq!(
            pick_platform_profile(PowerMode::Eco, &available).unwrap(),
            "low-power"
        );
    }

    #[test]
    fn eco_falls_back_to_quiet_on_firmware_that_calls_it_that() {
        let available = choices(&["quiet", "balanced", "performance"]);
        assert_eq!(
            pick_platform_profile(PowerMode::Eco, &available).unwrap(),
            "quiet"
        );
    }

    #[test]
    fn unlimited_maps_onto_performance() {
        let available = choices(&["low-power", "balanced", "performance"]);
        assert_eq!(
            pick_platform_profile(PowerMode::Unlimited, &available).unwrap(),
            "performance"
        );
    }

    #[test]
    fn a_firmware_offering_nothing_usable_yields_none() {
        assert_eq!(
            pick_platform_profile(PowerMode::Unlimited, &choices(&["custom"])),
            None
        );
    }

    /// A laptop with both mechanisms, which is the case the split exists
    /// for: the firmware profile and the desktop's are different things.
    fn full_machine() -> BackendState {
        BackendState {
            platform_profile: Some("balanced".into()),
            platform_profile_choices: choices(&["low-power", "balanced", "performance"]),
            power_profiles_daemon: Some("balanced".into()),
            tlp: None,
            auto_cpufreq: false,
            energy_preference: Some("balance_performance".into()),
            governor: Some("powersave".into()),
            other_managers: Vec::new(),
            available: vec!["platform_profile", "power-profiles-daemon"],
        }
    }

    /// The OS step is planned - and applied - before the firmware step,
    /// on purpose: see the race documented on `plan` itself. This is the
    /// assertion that would catch a well-meaning reordering.
    #[test]
    fn both_halves_are_applied_when_the_os_profile_is_wanted() {
        let (steps, problems) = plan(&full_machine(), PowerMode::Eco, true);

        assert_eq!(
            steps,
            vec![
                Step::PowerProfilesDaemon("power-saver"),
                Step::PlatformProfile("low-power".into()),
            ],
            "the firmware write must come last, so it wins any race with ppd's own driver"
        );
        assert!(problems.is_empty());
    }

    /// Saying no to the OS profile still changes the laptop's own, which is
    /// the half that moves the fan curve.
    #[test]
    fn declining_the_os_profile_leaves_the_firmware_profile_alone() {
        let (steps, _) = plan(&full_machine(), PowerMode::Eco, false);
        assert_eq!(steps, vec![Step::PlatformProfile("low-power".into())]);
    }

    /// power-profiles-daemon already drives EPP; writing it ourselves on
    /// top would be two things fighting over the same files.
    #[test]
    fn the_cpu_hint_is_only_used_where_there_is_no_daemon_to_delegate_to() {
        let no_ppd = BackendState {
            power_profiles_daemon: None,
            ..full_machine()
        };
        let (steps, _) = plan(&no_ppd, PowerMode::Eco, true);

        assert_eq!(
            steps,
            vec![
                Step::EnergyPreference("power"),
                Step::PlatformProfile("low-power".into())
            ]
        );
    }

    /// tuned or system76-power running where nothing serves the profiles
    /// API: they rewrite the hint themselves, so it is not written under
    /// them - and the user is told why it did not move.
    #[test]
    fn the_cpu_hint_is_not_written_under_a_manager_this_module_cannot_ask() {
        let tuned = BackendState {
            power_profiles_daemon: None,
            other_managers: vec!["tuned".into()],
            ..full_machine()
        };
        let (steps, problems) = plan(&tuned, PowerMode::Eco, true);

        assert_eq!(steps, vec![Step::PlatformProfile("low-power".into())]);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("tuned"), "{problems:?}");
    }

    /// Board 8D2F: no firmware profile at all, so the OS half is the whole
    /// answer - and switching it off leaves nothing to do.
    #[test]
    fn a_machine_with_no_firmware_profile_has_only_the_os_half() {
        let no_firmware = BackendState {
            platform_profile: None,
            platform_profile_choices: Vec::new(),
            ..full_machine()
        };

        let (steps, _) = plan(&no_firmware, PowerMode::Eco, true);
        assert_eq!(steps, vec![Step::PowerProfilesDaemon("power-saver")]);

        let (steps, problems) = plan(&no_firmware, PowerMode::Eco, false);
        assert!(
            steps.is_empty() && problems.is_empty(),
            "nothing to do is not a failure"
        );
    }

    /// TLP without tlp-pd: no power-profiles API, so TLP is asked in its
    /// own terms - and the hint is left to it, since TLP would rewrite it
    /// on the next charger event anyway.
    #[test]
    fn tlp_is_asked_directly_where_nothing_serves_the_profiles_api() {
        let tlp = BackendState {
            power_profiles_daemon: None,
            tlp: Some("balanced".into()),
            ..full_machine()
        };
        let (steps, _) = plan(&tlp, PowerMode::Eco, true);
        assert_eq!(
            steps,
            vec![
                Step::Tlp("power-saver"),
                Step::PlatformProfile("low-power".into())
            ]
        );
    }

    /// tlp-pd is TLP behind the D-Bus API; asking both would apply TLP's
    /// whole profile twice.
    #[test]
    fn the_profiles_api_wins_over_tlp() {
        let both = BackendState {
            tlp: Some("balanced".into()),
            ..full_machine()
        };
        let (steps, _) = plan(&both, PowerMode::Performance, true);
        assert!(steps.contains(&Step::PowerProfilesDaemon("performance")));
        assert!(!steps.iter().any(|s| matches!(s, Step::Tlp(_))));
    }

    /// auto-cpufreq rewrites the governor and EPP every few seconds, so it
    /// is told alongside whatever else is here, and the hint is never
    /// written underneath it. Balanced hands control back to it.
    #[test]
    fn auto_cpufreq_is_told_whatever_else_is_running() {
        let auto = BackendState {
            power_profiles_daemon: None,
            tlp: Some("balanced".into()),
            auto_cpufreq: true,
            ..full_machine()
        };
        let (steps, _) = plan(&auto, PowerMode::Eco, true);
        assert_eq!(
            steps,
            vec![
                Step::Tlp("power-saver"),
                Step::AutoCpufreq("powersave"),
                Step::PlatformProfile("low-power".into()),
            ]
        );

        let alone = BackendState { tlp: None, ..auto };
        let (steps, _) = plan(&alone, PowerMode::Balanced, true);
        assert_eq!(
            steps,
            vec![
                Step::AutoCpufreq("reset"),
                Step::PlatformProfile("balanced".into())
            ]
        );
    }

    #[test]
    fn tlp_stat_mode_is_read_as_a_profile_or_not_at_all() {
        assert_eq!(
            parse_tlp_mode("balanced/BAT\n").as_deref(),
            Some("balanced")
        );
        assert_eq!(
            parse_tlp_mode("power-saver/SAV (manual)\n").as_deref(),
            Some("power-saver")
        );
        assert_eq!(
            parse_tlp_mode("performance/AC (default)").as_deref(),
            Some("performance")
        );
        // TLP before 1.8, and a TLP that has not run this boot.
        assert_eq!(parse_tlp_mode("AC\n"), None);
        assert_eq!(parse_tlp_mode("unknown\n"), None);
        assert_eq!(parse_tlp_mode(""), None);
    }

    #[test]
    fn a_busctl_string_variant_is_unwrapped() {
        assert_eq!(
            parse_variant_string("v s \"power-saver\"\n").as_deref(),
            Some("power-saver")
        );
        assert_eq!(parse_variant_string("b true"), None);
        assert_eq!(parse_variant_string("v s \"\""), None);
    }
}
