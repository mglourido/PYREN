//! The four performance profiles, end to end, against a machine that is
//! entirely made up.
//!
//! The unit tests next to the source cover the *decisions* - which
//! firmware profile a mode maps onto, what the supervisor would pick, how
//! a percentage becomes watts. What they cannot cover is the half that
//! writes: `backend::apply` and `limits::apply` reach into
//! `/sys/firmware/acpi`, `/sys/class/powercap` and the OS's power manager, and
//! a test that exercised those for real would change the machine it is
//! running on. So they were never exercised at all, and the questions a
//! user actually asks - *does switching profile move both the laptop's
//! profile and the OS's? does it survive closing the app? does it survive
//! the daemon restarting?* - had no answer here.
//!
//! [`Machine`] is that answer: a fixture sysfs tree plus stand-in power
//! managers that record every request, pointed at through the
//! `PYREN_*` variables the rest of the project already uses for this. What
//! the module writes to the fake machine is then simply readable back, so
//! "Eco set the firmware profile to low-power and asked the OS for
//! power-saver" is an assertion rather than something to take on trust.
//!
//! Four scenarios run through it, and they are the four in the title:
//!
//! | scenario | what it stands for |
//! |---|---|
//! | switching | clicking each of the four modes, and cycling them |
//! | minutes | the supervisor left running while conditions move |
//! | the app closes | the window goes away; the daemon does not |
//! | the daemon restarts | a reboot, or `systemctl restart` |

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use pyren_config::ConfigStore;
use pyren_core::Module;
use pyren_power::{
    AutoConfig, AutoInputs, AutoSwitcher, Limits, PowerConfig, PowerMode, PowerModule,
};
use serde_json::{json, Value};

const W: u64 = 1_000_000;

/// The test laptop's envelope, and the numbers every assertion here is
/// measured against.
const STOCK_PL1: u64 = 77 * W;
const STOCK_PL2: u64 = 77 * W;
const STOCK_PL4: u64 = 168 * W;

/// The `PYREN_*` overrides are process-global, so two fixtures alive at
/// once would be one machine wearing both their masks. Every [`Machine`]
/// holds this for its lifetime, which serialises the tests in this file -
/// the cost of a fake machine being a *machine* rather than an argument.
fn machine_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A laptop that does not exist: a firmware platform profile, a
/// power-profiles-daemon, four CPUs with an energy-performance hint, an
/// Intel RAPL package zone and a turbo knob.
///
/// Everything a [`PowerModule`] can reach is inside `root`, so an
/// assertion is a file read and a failed test leaves the evidence on disk.
struct Machine {
    root: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl Machine {
    /// The full machine: every mechanism present, at stock, in Balanced.
    fn new(tag: &str) -> Self {
        let guard = machine_lock().lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir()
            .join(format!("pyren-power-profiles-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let machine = Self { root, _guard: guard };
        machine.write("acpi/platform_profile", "balanced");
        machine.write("acpi/platform_profile_choices", "low-power balanced performance");

        for cpu in 0..4 {
            machine.write(&format!("cpu/cpu{cpu}/cpufreq/energy_performance_preference"), "balance_performance");
            machine.write(&format!("cpu/cpu{cpu}/cpufreq/scaling_governor"), "powersave");
        }
        // `intel_pstate/no_turbo`, whose polarity is inverted: 1 is off.
        machine.write("cpu/intel_pstate/no_turbo", "0");

        machine.write("powercap/intel-rapl:0/name", "package-0");
        machine.write_limits(Limits {
            pl1_uw: Some(STOCK_PL1),
            pl2_uw: Some(STOCK_PL2),
            pl4_uw: Some(STOCK_PL4),
        });
        // A sub-zone and the mmio mirror, which the module must ignore:
        // writing the same package twice through two interfaces is the bug
        // `find_package_zone` exists to avoid, and a fixture without them
        // could not catch a regression in it.
        machine.write("powercap/intel-rapl:0:0/name", "core");
        machine.write("powercap/intel-rapl-mmio:0/name", "package-0");
        machine.write("powercap/intel-rapl-mmio:0/constraint_0_power_limit_uw", "28000000");

        machine.write_os_profile("balanced");
        machine.install_profiles_service();
        machine.apply_env();
        machine
    }

    fn write(&self, name: &str, contents: &str) {
        let path = self.root.join(name);
        std::fs::create_dir_all(path.parent().expect("fixture files are nested")).expect("fixture dir");
        std::fs::write(path, contents).expect("fixture file");
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(name)).ok().map(|v| v.trim().to_string())
    }

    fn zone(&self) -> PathBuf {
        self.root.join("powercap/intel-rapl:0")
    }

    fn write_limits(&self, limits: Limits) {
        for (constraint, value) in
            [(0, limits.pl1_uw), (1, limits.pl2_uw), (2, limits.pl4_uw)]
        {
            let path = self.zone().join(format!("constraint_{constraint}_power_limit_uw"));
            match value {
                Some(value) => {
                    std::fs::create_dir_all(self.zone()).expect("zone dir");
                    std::fs::write(path, value.to_string()).expect("limit file");
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    /// What the fake power-profiles service will answer.
    fn write_os_profile(&self, profile: &str) {
        self.write("os_profile", profile);
    }

    /// A power-profiles service that keeps a diary, reached the way the
    /// module reaches the real one: `busctl` on the system bus.
    ///
    /// `Get` answers from a file and `Set` writes it, so the OS profile
    /// behaves like the stateful service it stands in for; the log is what
    /// makes "asked for it once" distinguishable from "asked for it on
    /// every tick for five minutes".
    fn install_profiles_service(&self) {
        self.install_profiles_service_script(
            "printf '%s' \"$profile\" > \"$root/os_profile\"\n\
             \x20    echo \"$profile\" >> \"$root/os_profile.log\"",
        );
    }

    /// The same stand-in, but with the misbehaviour found on the reference
    /// laptop: power-profiles-daemon 0.30 ships its own `platform_profile`
    /// driver, so `Set` here also writes the firmware file itself - to
    /// `wrong_hardware_profile`, standing in for whatever *ppd's* mapping
    /// landed on, which measurably disagreed with this module's own
    /// (`balanced` where this module chose `cool`). What made that a real
    /// incident rather than a one-off reading was the order `apply` used
    /// to run the two steps in: see `backend::plan`.
    fn install_racing_profiles_service(&self, wrong_hardware_profile: &str) {
        self.install_profiles_service_script(&format!(
            "printf '%s' \"$profile\" > \"$root/os_profile\"\n\
             \x20    echo \"$profile\" >> \"$root/os_profile.log\"\n\
             \x20    printf '%s' \"{wrong_hardware_profile}\" > \"$root/acpi/platform_profile\""
        ));
    }

    /// A service that misapplies exactly its first `Set` call ever, to a
    /// profile nobody asked for, and gets every one after that right - the
    /// shape of the mismatch found on the reference laptop, where a second
    /// attempt was observed to always succeed. Exercises the retry in
    /// `backend::set_and_confirm` rather than the platform-profile race,
    /// so `platform_profile` here is left alone.
    fn install_flaky_profiles_service(&self) {
        self.install_profiles_service_script(
            "count=\"$root/ppd_set_count\"\n\
             \x20    n=$(cat \"$count\" 2>/dev/null || echo 0); n=$((n + 1)); echo \"$n\" > \"$count\"\n\
             \x20    if [ \"$n\" -eq 1 ]; then printf '%s' nobody-asked-for-this > \"$root/os_profile\"\n\
             \x20    else printf '%s' \"$profile\" > \"$root/os_profile\"; fi\n\
             \x20    echo \"$profile\" >> \"$root/os_profile.log\"",
        );
    }

    /// A service that never lands on what it is asked for - the case the
    /// retry has to give up on rather than loop forever.
    fn install_permanently_wrong_profiles_service(&self, wrong: &str) {
        self.install_profiles_service_script(&format!(
            "printf '%s' \"{wrong}\" > \"$root/os_profile\"\n\
             \x20    echo \"$profile\" >> \"$root/os_profile.log\""
        ));
    }

    /// A fake `busctl` serving `ActiveProfile`, running `on_set` (with
    /// `$profile` and `$root` in scope) for a `Set`. Every invocation's
    /// arguments are logged, so a test can check how it was called - above
    /// all, that it was never allowed to auto-start anything.
    fn install_profiles_service_script(&self, on_set: &str) {
        self.install_tool(
            "busctl",
            &format!(
                "#!/bin/sh\n\
                 root=$(dirname \"$0\")/..\n\
                 echo \"$*\" >> \"$root/busctl.log\"\n\
                 for profile; do :; done\n\
                 case \" $* \" in\n\
                 *\" Get \"*) printf 'v s \"%s\"\\n' \"$(cat \"$root/os_profile\")\" ;;\n\
                 *\" Set \"*) {on_set} ;;\n\
                 *) exit 2 ;;\n\
                 esac\n"
            ),
        );
    }

    /// An executable in the fixture's tool directory - the only place the
    /// module looks for one while `PYREN_TOOLS_DIR` points here.
    fn install_tool(&self, name: &str, contents: &str) {
        let script = self.root.join("bin").join(name);
        self.write(&format!("bin/{name}"), contents);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("the stand-in has to be runnable");
        }
    }

    fn remove_tool(&self, name: &str) {
        let _ = std::fs::remove_file(self.root.join("bin").join(name));
    }

    /// TLP 1.8+ with no tlp-pd: `tlp-stat -m` answers from a file and
    /// `tlp <profile>` writes it, logging each request.
    fn install_tlp(&self, profile: &str) {
        self.remove_tool("busctl");
        self.write("tlp_profile", profile);
        self.install_tool(
            "tlp-stat",
            "#!/bin/sh\n\
             root=$(dirname \"$0\")/..\n\
             [ \"$1\" = -m ] || exit 2\n\
             printf '%s/BAT (manual)\\n' \"$(cat \"$root/tlp_profile\")\"\n",
        );
        self.install_tool(
            "tlp",
            "#!/bin/sh\n\
             root=$(dirname \"$0\")/..\n\
             case \"$1\" in\n\
             performance|balanced|power-saver) printf '%s' \"$1\" > \"$root/tlp_profile\"\n\
             \x20    echo \"$1\" >> \"$root/tlp.log\" ;;\n\
             *) exit 3 ;;\n\
             esac\n",
        );
    }

    /// A running auto-cpufreq: `pgrep` finds its daemon, `--force` writes
    /// the override (a reset removes it) and `--get-state` reads it back.
    fn install_auto_cpufreq(&self) {
        self.install_tool("pgrep", "#!/bin/sh\necho 1236\n");
        self.install_tool(
            "auto-cpufreq",
            "#!/bin/sh\n\
             root=$(dirname \"$0\")/..\n\
             case \"$1\" in\n\
             --get-state) cat \"$root/acf_override\" 2>/dev/null || echo default ;;\n\
             --force=reset) rm -f \"$root/acf_override\"; echo reset >> \"$root/acf.log\" ;;\n\
             --force=*) printf '%s\\n' \"${1#--force=}\" > \"$root/acf_override\"\n\
             \x20    echo \"${1#--force=}\" >> \"$root/acf.log\" ;;\n\
             *) exit 2 ;;\n\
             esac\n",
        );
    }

    /// Points the module at this machine. Called again after any change
    /// that removes a mechanism, since the variables are what say where -
    /// and what says a path is absent is the file not being there.
    fn apply_env(&self) {
        std::env::set_var("PYREN_PLATFORM_PROFILE", self.root.join("acpi/platform_profile"));
        std::env::set_var("PYREN_CPU_ROOT", self.root.join("cpu"));
        std::env::set_var("PYREN_POWERCAP", self.root.join("powercap"));
        std::env::set_var("PYREN_TOOLS_DIR", self.root.join("bin"));
    }

    /// A config store inside the fixture, so the daemon's memory dies with
    /// the machine it belongs to.
    fn store(&self) -> ConfigStore {
        ConfigStore::at(self.root.join("config"))
    }

    /// Starts "the daemon": a module over this machine's own config.
    fn boot(&self) -> PowerModule {
        PowerModule::with_store(self.store())
    }

    // --- what the machine now says about itself ---

    /// The laptop's own profile - the half that moves the EC's fan curve.
    fn hardware_profile(&self) -> String {
        self.read("acpi/platform_profile").expect("the fixture has a platform profile")
    }

    /// The OS profile - the half the desktop's battery menu shows.
    fn os_profile(&self) -> String {
        self.read("os_profile").expect("the fixture has an OS profile")
    }

    /// Every OS profile ever asked for, oldest first.
    fn os_profile_requests(&self) -> Vec<String> {
        self.read("os_profile.log")
            .map(|log| log.lines().map(|l| l.trim().to_string()).collect())
            .unwrap_or_default()
    }

    fn limits(&self) -> Limits {
        let read = |constraint: u8| {
            self.read(&format!("powercap/intel-rapl:0/constraint_{constraint}_power_limit_uw"))
                .and_then(|v| v.parse().ok())
        };
        Limits { pl1_uw: read(0), pl2_uw: read(1), pl4_uw: read(2) }
    }

    fn turbo(&self) -> bool {
        self.read("cpu/intel_pstate/no_turbo").as_deref() == Some("0")
    }

    /// What the daemon wrote down, read as a fresh reader would - which is
    /// what a restart is.
    fn saved_config(&self) -> PowerConfig {
        self.store().load::<PowerConfig>("power").value
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        // Left behind for whoever is reading a failure; the next run with
        // the same tag clears it.
        for name in
            ["PYREN_PLATFORM_PROFILE", "PYREN_CPU_ROOT", "PYREN_POWERCAP", "PYREN_TOOLS_DIR"]
        {
            std::env::remove_var(name);
        }
    }
}

/// Sets a mode the way the app's button does, and insists it worked.
fn set(module: &PowerModule, mode: PowerMode) -> Value {
    module
        .call("setMode", json!({ "mode": mode_name(mode) }))
        .unwrap_or_else(|e| panic!("setMode {mode:?} should have been applied: {e:?}"))
}

fn mode_name(mode: PowerMode) -> &'static str {
    match mode {
        PowerMode::Eco => "eco",
        PowerMode::Balanced => "balanced",
        PowerMode::Performance => "performance",
        PowerMode::Unlimited => "unlimited",
    }
}

/// What the machine's two profiles should read for each mode, on a laptop
/// offering `low-power balanced performance`.
///
/// Performance and Unlimited share both, which is not an oversight: there
/// is no firmware profile beyond `performance`, and what makes Unlimited
/// different is the envelope, not a fifth name nobody's firmware has.
fn expected(mode: PowerMode) -> (&'static str, &'static str) {
    match mode {
        PowerMode::Eco => ("low-power", "power-saver"),
        PowerMode::Balanced => ("balanced", "balanced"),
        PowerMode::Performance => ("performance", "performance"),
        PowerMode::Unlimited => ("performance", "performance"),
    }
}

// ---------------------------------------------------------------------
// Switching between the four
// ---------------------------------------------------------------------

/// The claim the feature makes: picking a mode moves the laptop's own
/// profile *and* the OS's, and each of the four means something specific.
#[test]
fn every_mode_moves_both_the_hardware_profile_and_the_os_profile() {
    let machine = Machine::new("four-modes");
    let daemon = machine.boot();

    for mode in PowerMode::ALL {
        let report = set(&daemon, *mode);
        let (hardware, os) = expected(*mode);

        assert_eq!(machine.hardware_profile(), hardware, "{mode:?}: the laptop's own profile");
        assert_eq!(machine.os_profile(), os, "{mode:?}: the OS profile");
        assert_eq!(daemon.mode(), *mode, "{mode:?}: the daemon agrees with the machine");

        let applied = report["applied"].as_array().expect("a report lists what it applied");
        assert!(
            applied.iter().any(|a| a == &json!(format!("platform_profile={hardware}"))),
            "{mode:?} should say it set the firmware profile, said {applied:?}"
        );
        assert!(
            applied.iter().any(|a| a == &json!(format!("power-profiles-daemon={os}"))),
            "{mode:?} should say it set the OS profile, said {applied:?}"
        );
        assert_eq!(report["failed"], json!([]), "{mode:?} should have nothing to complain about");
    }
}

/// Performance and Unlimited look identical to the firmware and to the
/// desktop, and differ only in the envelope. Asserted rather than left
/// implicit, because "the fourth mode does nothing" is exactly what it
/// looks like from outside until you know why.
#[test]
fn unlimited_differs_from_performance_only_in_the_envelope() {
    let machine = Machine::new("unlimited-vs-performance");
    let daemon = machine.boot();

    // Give Performance an envelope somebody measured, and leave Unlimited
    // at the machine's own.
    daemon
        .call("setTuning", json!({ "mode": "performance", "pl1W": 55.0, "pl2W": 65.0 }))
        .expect("tuning Performance");

    set(&daemon, PowerMode::Performance);
    // 71 % of 77 W, not 55 W exactly: the envelope is stored as a whole
    // percentage so that a restored config means the same thing on
    // different hardware, and one percent of this machine is 0.77 W.
    assert_eq!(machine.limits().pl1_uw, Some(54_670_000), "Performance is capped near where it was told");
    assert!(
        (machine.limits().pl1_uw.unwrap() as i64 - 55 * W as i64).abs() <= STOCK_PL1 as i64 / 100,
        "and never further out than the one percent that quantisation costs"
    );

    set(&daemon, PowerMode::Unlimited);
    assert_eq!(machine.hardware_profile(), "performance", "the firmware sees no difference");
    assert_eq!(machine.os_profile(), "performance", "nor does the desktop");
    assert_eq!(machine.limits().pl1_uw, Some(STOCK_PL1), "Unlimited is the whole envelope back");
}

/// Twenty times round the loop. A mode switch that is not idempotent
/// shows up as drift - a limit that creeps, a profile that lands
/// somewhere else the second time - and one pass would not see it.
#[test]
fn cycling_the_four_modes_for_twenty_rounds_lands_in_the_same_place_every_time() {
    let machine = Machine::new("cycle-soak");
    let daemon = machine.boot();

    let mut visited = Vec::new();
    for round in 0..20 {
        for mode in PowerMode::ALL {
            set(&daemon, *mode);
            let (hardware, os) = expected(*mode);
            assert_eq!(machine.hardware_profile(), hardware, "round {round}, {mode:?}");
            assert_eq!(machine.os_profile(), os, "round {round}, {mode:?}");
            assert_eq!(machine.limits(), stock(), "round {round}, {mode:?}: envelope untouched");
            visited.push(*mode);
        }
    }

    assert_eq!(visited.len(), 80);
    assert_eq!(daemon.mode(), PowerMode::Unlimited, "it ends where the last round left it");
    // The envelope was never tuned, so no mode ever had a limit to write:
    // the guard against writing a value the hardware already holds is what
    // keeps an unprivileged daemon from reporting eighty failures.
    assert_eq!(machine.saved_config().stock_limits, Some(stock()));
}

/// The performance key, which steps rather than picks. Four presses must
/// come back to where they started, having actually moved the machine
/// each time.
#[test]
fn four_presses_of_the_performance_key_return_to_the_starting_mode() {
    let machine = Machine::new("cycle-key");
    let daemon = machine.boot();
    set(&daemon, PowerMode::Eco);

    let mut seen = Vec::new();
    for _ in 0..4 {
        let cycled = daemon.cycle();
        assert!(cycled.changed(), "a press that applies nothing is a lie the widget would tell");
        assert_eq!(cycled.to, cycled.asked_for, "the machine went where the key asked");
        assert_eq!(machine.hardware_profile(), expected(cycled.to).0);
        seen.push(cycled.to);
    }

    assert_eq!(
        seen,
        vec![PowerMode::Balanced, PowerMode::Performance, PowerMode::Unlimited, PowerMode::Eco]
    );
    assert_eq!(machine.os_profile(), "power-saver", "back to Eco, and the OS knows");
}

/// The switch that exists because the two halves have different owners.
#[test]
fn declining_the_os_profile_still_moves_the_laptops_own() {
    let machine = Machine::new("os-profile-off");
    let daemon = machine.boot();
    set(&daemon, PowerMode::Balanced);

    daemon
        .call("setApplyToOsProfile", json!({ "enabled": false }))
        .expect("turning the OS half off");
    let before = machine.os_profile_requests().len();

    for mode in PowerMode::ALL {
        set(&daemon, *mode);
        assert_eq!(machine.hardware_profile(), expected(*mode).0, "{mode:?}: firmware still moves");
    }

    assert_eq!(
        machine.os_profile_requests().len(),
        before,
        "not one OS profile was asked for while the switch was off"
    );
    assert_eq!(machine.os_profile(), "balanced", "the desktop was left where it was");

    // ...and turning it back on catches the OS up at once, rather than at
    // the next mode change.
    daemon.call("setApplyToOsProfile", json!({ "enabled": true })).expect("turning it back on");
    assert_eq!(machine.os_profile(), "performance", "Unlimited's OS profile, applied on the spot");
}

/// A machine whose only mechanism is the OS profile - board 8D2F has no
/// firmware platform profile at all - and a user who then switches the OS
/// half off. Nothing can be applied, and `setMode` has to say so rather
/// than move the highlight over a machine that did not change.
#[test]
fn a_mode_that_could_not_be_applied_anywhere_is_an_error_not_a_silent_no_op() {
    let machine = Machine::new("no-mechanism");
    std::fs::remove_file(machine.root.join("acpi/platform_profile")).expect("remove the firmware profile");
    std::fs::remove_file(machine.root.join("acpi/platform_profile_choices")).expect("and its choices");

    let daemon = machine.boot();
    set(&daemon, PowerMode::Eco);
    assert_eq!(machine.os_profile(), "power-saver", "the OS half is the whole answer here");

    daemon.call("setApplyToOsProfile", json!({ "enabled": false })).ok();
    let before = daemon.mode();

    let refused = daemon.call("setMode", json!({ "mode": "performance" }));
    assert!(refused.is_err(), "nothing to apply must not report success");
    assert_eq!(daemon.mode(), before, "and the mode must not move on a machine that did not");
}

/// Ten threads clicking modes at once - the app, the widget, the hotkey
/// and `pyren-ctl` are four separate callers into one daemon, and nothing
/// stops them arriving together.
#[test]
fn concurrent_switches_leave_the_daemon_and_the_machine_agreeing() {
    let machine = Machine::new("concurrent");
    let daemon = machine.boot();

    std::thread::scope(|scope| {
        for thread in 0..10 {
            let daemon = daemon.clone();
            scope.spawn(move || {
                for step in 0..20 {
                    let mode = PowerMode::ALL[(thread + step) % PowerMode::ALL.len()];
                    let _ = daemon.call("setMode", json!({ "mode": mode_name(mode) }));
                }
            });
        }
    });

    // Whoever wrote last won, but the daemon's idea of the mode and the
    // machine's must be the same idea.
    let settled = daemon.mode();
    let state = daemon.call("getState", Value::Null).expect("getState");
    assert_eq!(state["mode"], json!(mode_name(settled)));
    assert_eq!(
        state["backend"]["platformProfile"],
        json!(expected(settled).0),
        "the daemon reports the mode the machine is actually in"
    );
}

fn stock() -> Limits {
    Limits { pl1_uw: Some(STOCK_PL1), pl2_uw: Some(STOCK_PL2), pl4_uw: Some(STOCK_PL4) }
}

// ---------------------------------------------------------------------
// Closing the app, and restarting the daemon
// ---------------------------------------------------------------------
//
// These two are not the same event, and the whole design of this module
// turns on the difference.
//
// **The app is a client.** It opens a socket, asks the daemon for a mode,
// and draws the answer. Closing its window - whether it was in front, in
// the background or minimised into the tray, which are the same thing to
// everything below the window manager - closes a socket and nothing else.
// The mode is the machine's, held by a process that is still running.
//
// **The daemon restarting is the event that can lose something**, because
// that is when the in-memory mode goes away and only what reached
// `power.json` comes back - and only then if the user asked for it.

/// Closing the app must be invisible to the machine.
///
/// Modelled the way it actually happens: a second handle on the same
/// daemon is what the app's connection is, and it goes away.
#[test]
fn closing_the_app_leaves_the_machine_in_the_mode_it_was_put_in() {
    let machine = Machine::new("app-closes");
    let daemon = machine.boot();

    let app = daemon.clone();
    set(&app, PowerMode::Eco);
    assert_eq!(machine.hardware_profile(), "low-power");
    drop(app);

    assert_eq!(machine.hardware_profile(), "low-power", "the firmware profile is still Eco's");
    assert_eq!(machine.os_profile(), "power-saver", "and so is the OS's");
    assert_eq!(daemon.mode(), PowerMode::Eco, "the daemon did not notice and had no reason to");

    // ...and re-opening it finds the same machine rather than a default.
    let reopened = daemon.clone();
    let state = reopened.call("getState", Value::Null).expect("getState");
    assert_eq!(state["mode"], json!("eco"));
    assert_eq!(state["backend"]["platformProfile"], json!("low-power"));
}

/// The supervisor is the reason this is a daemon rather than a thread
/// inside the window. It has to keep deciding with nobody watching.
#[test]
fn the_supervisor_keeps_working_after_the_app_is_gone() {
    let machine = Machine::new("app-closes-supervisor");
    let daemon = machine.boot();

    let app = daemon.clone();
    set(&app, PowerMode::Balanced);
    drop(app);

    // A quarter of an hour on mains under load, sampled every ten seconds
    // with nothing connected. See `run_minutes` - the supervisor's own
    // thread is on a real clock, so the timeline is driven here instead.
    let switches = run_minutes(&daemon, &machine, 15, &busy_on_mains());

    assert!(!switches.is_empty(), "a machine under sustained load should have been stepped up");
    assert_eq!(daemon.mode(), PowerMode::Performance);
    assert_eq!(machine.hardware_profile(), "performance", "with nobody watching, on its own");
}

/// A restart with the box unticked changes nothing, which is the default
/// and is the conservative answer: silently moving the machine's power
/// behaviour at boot should be opted into.
#[test]
fn a_daemon_restart_leaves_the_machine_alone_unless_asked_to_restore() {
    let machine = Machine::new("restart-default");
    let first = machine.boot();
    set(&first, PowerMode::Eco);
    drop(first);

    // Somebody's firmware, or a Windows session, moved it while we were
    // not running.
    machine.write("acpi/platform_profile", "performance");
    machine.write_os_profile("performance");

    let restarted = machine.boot();

    assert_eq!(machine.hardware_profile(), "performance", "the daemon did not overrule the machine");
    assert_eq!(
        restarted.mode(),
        PowerMode::Performance,
        "and it reports what it found, not what it remembered"
    );
    assert_eq!(machine.saved_config().mode, Some(PowerMode::Eco), "the memory is still there");
}

/// With the box ticked, the mode comes back - both halves of it.
#[test]
fn restore_on_start_puts_the_whole_profile_back() {
    let machine = Machine::new("restart-restore");
    let first = machine.boot();

    first.call("setTuning", json!({ "mode": "eco", "pl1W": 34.0, "turbo": false })).expect("tune Eco");
    first.call("setRestoreOnStart", json!({ "enabled": true })).expect("restore on");
    set(&first, PowerMode::Eco);

    let capped = machine.limits().pl1_uw.expect("Eco capped the package");
    assert!(capped < STOCK_PL1, "Eco's envelope is smaller than the machine's");
    assert!(!machine.turbo(), "and it turned turbo off");
    drop(first);

    // The machine comes up in whatever the firmware defaults to.
    machine.write("acpi/platform_profile", "balanced");
    machine.write_os_profile("balanced");
    machine.write_limits(stock());
    machine.write("cpu/intel_pstate/no_turbo", "0");

    let restarted = machine.boot();

    assert_eq!(restarted.mode(), PowerMode::Eco);
    assert_eq!(machine.hardware_profile(), "low-power", "the laptop's own profile is back");
    assert_eq!(machine.os_profile(), "power-saver", "the OS profile is back");
    assert_eq!(machine.limits().pl1_uw, Some(capped), "and so is the envelope, to the watt");
    assert!(!machine.turbo(), "turbo included");
}

/// The ratchet, end to end and across five restarts.
///
/// Reading the envelope at startup while Eco is in force would record
/// Eco's reduced limit as the machine's ceiling, and each boot would shave
/// a little more off until the laptop was permanently slow. `highest` is
/// unit-tested; this is the scenario it exists for.
#[test]
fn restarting_while_capped_never_lowers_the_machines_recorded_ceiling() {
    let machine = Machine::new("ratchet");
    let first = machine.boot();
    first.call("setTuning", json!({ "mode": "eco", "pl1W": 30.0, "pl2W": 30.0 })).expect("tune Eco");
    first.call("setRestoreOnStart", json!({ "enabled": true })).expect("restore on");
    set(&first, PowerMode::Eco);
    drop(first);

    let capped = machine.limits();
    assert!(capped.pl1_uw.unwrap() < STOCK_PL1);

    for boot in 0..5 {
        let daemon = machine.boot();
        assert_eq!(
            machine.saved_config().stock_limits,
            Some(stock()),
            "boot {boot}: the ceiling is still the firmware's"
        );
        assert_eq!(daemon.mode(), PowerMode::Eco, "boot {boot}");
        assert_eq!(machine.limits(), capped, "boot {boot}: and Eco still means the same watts");
        drop(daemon);
    }

    // Coming back out of Eco returns the whole envelope, five boots later.
    let daemon = machine.boot();
    set(&daemon, PowerMode::Unlimited);
    assert_eq!(machine.limits(), stock(), "nothing was lost along the way");
}

/// A ceiling higher than the one on file can only have come from the
/// firmware - a BIOS update, or a different machine restored onto.
#[test]
fn a_firmware_that_raises_its_limits_raises_the_recorded_ceiling() {
    let machine = Machine::new("ratchet-up");
    let first = machine.boot();
    assert_eq!(first.call("getState", Value::Null).unwrap()["limits"]["stock"], json!(stock()));
    // A mode change is what commits the reading to disk; see
    // `the_recorded_ceiling_only_reaches_disk_once_something_is_saved`.
    set(&first, PowerMode::Balanced);
    drop(first);
    assert_eq!(machine.saved_config().stock_limits, Some(stock()));

    machine.write_limits(Limits { pl1_uw: Some(90 * W), pl2_uw: Some(90 * W), pl4_uw: Some(200 * W) });
    let second = machine.boot();
    set(&second, PowerMode::Balanced);

    let recorded = machine.saved_config().stock_limits.expect("a ceiling");
    assert_eq!(recorded.pl1_uw, Some(90 * W), "the firmware's word is final upwards");
    assert_eq!(recorded.pl4_uw, Some(200 * W));
}

/// Pins the one seam in the ratchet, because it is not obvious and a
/// future change could close it without noticing it was ever open.
///
/// The ceiling is worked out at startup - before anything has had a
/// chance to cap the machine - but it is only *written* when something
/// else causes a save. A daemon that comes up, is never asked for
/// anything and is restarted therefore has no ceiling on file, and the
/// next boot works one out from scratch. That is harmless while the
/// machine is at stock, which it is in that case; it is worth an
/// assertion so that it stays that way deliberately.
#[test]
fn the_recorded_ceiling_only_reaches_disk_once_something_is_saved() {
    let machine = Machine::new("ceiling-persistence");

    let quiet = machine.boot();
    assert_eq!(quiet.call("getState", Value::Null).unwrap()["limits"]["stock"], json!(stock()));
    drop(quiet);
    assert_eq!(machine.saved_config().stock_limits, None, "nothing asked, nothing written");

    let asked = machine.boot();
    set(&asked, PowerMode::Balanced);
    assert_eq!(machine.saved_config().stock_limits, Some(stock()), "one mode change commits it");
}

/// A daemon that cannot write its config has still changed the machine.
/// The user needs to be told the setting will not survive a restart - not
/// to have the change refused.
#[test]
fn a_mode_that_could_not_be_saved_is_still_applied_and_says_so() {
    if unsafe { libc_getuid() } == 0 {
        // Root writes into a read-only directory regardless, so the
        // scenario cannot be built. Skipping is honest; asserting the
        // opposite would not be.
        return;
    }
    let machine = Machine::new("unwritable-config");
    let daemon = machine.boot();
    set(&daemon, PowerMode::Balanced);

    // The config directory goes read-only underneath a running daemon.
    let config = machine.root.join("config");
    let mode = std::fs::metadata(&config).expect("config dir").permissions();
    let mut readonly = mode.clone();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        readonly.set_mode(0o500);
    }
    std::fs::set_permissions(&config, readonly).expect("make it read-only");

    set(&daemon, PowerMode::Eco);
    assert_eq!(machine.hardware_profile(), "low-power", "the machine changed anyway");

    let state = daemon.call("getState", Value::Null).expect("getState");
    assert!(
        !state["configSaveError"].is_null(),
        "and the daemon admits the setting will not survive a restart"
    );

    std::fs::set_permissions(&config, mode).expect("put it back");
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

// ---------------------------------------------------------------------
// How it evolves over minutes
// ---------------------------------------------------------------------
//
// The supervisor samples on a timer for as long as the daemon runs, so
// "what happens after a few minutes" is its question and not the
// switching code's. Its thread is on a real clock, which makes a
// quarter-hour of it an awkward thing to assert about.
//
// So the timeline is driven here instead, on a simulated clock: the same
// `AutoSwitcher` the supervisor owns, fed the same ten-second ticks, with
// each decision applied through the module's own public interface so the
// fake machine really moves. What is reproduced is the loop's
// *scheduling*; the decisions are the real ones. The `#[ignore]`d soak at
// the end of this section then runs the actual thread, on the actual
// clock, as the cross-check that the two agree.

/// Ten-second ticks, as `AutoConfig::interval_secs` defaults to.
const TICKS_PER_MINUTE: u64 = 6;

fn supervising() -> AutoConfig {
    AutoConfig { enabled: true, ..AutoConfig::default() }
}

/// Conditions at a given minute.
type Conditions = dyn Fn(f64) -> AutoInputs;

fn conditions(on_battery: Option<bool>, load: f64, battery: Option<f64>, temp: f64) -> AutoInputs {
    AutoInputs { on_battery, load_ratio: load, battery_percent: battery, temp_c: Some(temp) }
}

fn busy_on_mains() -> Box<Conditions> {
    Box::new(|_minute| conditions(Some(false), 0.95, None, 62.0))
}

/// Runs `minutes` of supervision and returns every switch it made, with
/// the minute it happened at.
///
/// Asserts as it goes that the daemon and the machine never disagree:
/// a supervisor that moved its own idea of the mode without moving the
/// hardware is the failure that would otherwise only show up as a user
/// saying the app lies.
fn run_minutes(
    daemon: &PowerModule,
    machine: &Machine,
    minutes: u64,
    weather: &Conditions,
) -> Vec<(f64, PowerMode)> {
    run_minutes_with(&supervising(), daemon, machine, minutes, weather)
}

fn run_minutes_with(
    config: &AutoConfig,
    daemon: &PowerModule,
    machine: &Machine,
    minutes: u64,
    weather: &Conditions,
) -> Vec<(f64, PowerMode)> {
    let mut switcher = AutoSwitcher::default();
    let mut switches = Vec::new();

    for tick in 0..minutes * TICKS_PER_MINUTE {
        let minute = tick as f64 / TICKS_PER_MINUTE as f64;
        let decision = switcher.observe(weather(minute), config, daemon.mode());

        if let Some(decision) = decision {
            let mode = decision.mode;
            set(daemon, mode);
            switches.push((minute, mode));
        }

        let mode = daemon.mode();
        assert_eq!(
            machine.hardware_profile(),
            expected(mode).0,
            "minute {minute}: the machine is not in the mode the daemon thinks it is"
        );
    }

    switches
}

/// Half an hour of a machine that is plugged in and busy. It should step
/// up once, early, and then leave well alone - a mode switch spins the
/// fans, so a supervisor that kept making them is worse than one that
/// never acted.
#[test]
fn half_an_hour_under_sustained_load_produces_exactly_one_switch() {
    let machine = Machine::new("minutes-load");
    let daemon = machine.boot();
    set(&daemon, PowerMode::Balanced);

    let switches = run_minutes(&daemon, &machine, 30, &busy_on_mains());

    assert_eq!(switches.len(), 1, "it settled and stayed settled, made {switches:?}");
    assert_eq!(switches[0].1, PowerMode::Performance);
    assert!(
        switches[0].0 <= 1.0,
        "and it did not take a minute to notice: switched at minute {}",
        switches[0].0
    );
    assert_eq!(machine.os_profile(), "performance");
}

/// A load that hovers in the dead band between the thresholds must not
/// make a machine that is already in its preferred mode flap for twenty
/// minutes.
#[test]
fn twenty_minutes_of_borderline_load_never_moves_the_machine() {
    let machine = Machine::new("minutes-deadband");
    let daemon = machine.boot();
    set(&daemon, PowerMode::Balanced);
    let prefers_balanced =
        AutoConfig { preferred_on_mains: PowerMode::Balanced, ..supervising() };

    let switches = run_minutes_with(&prefers_balanced, &daemon, &machine, 20, &borderline());

    assert!(switches.is_empty(), "the dead band exists for exactly this, made {switches:?}");
    assert_eq!(machine.hardware_profile(), "balanced");
    assert_eq!(machine.os_profile_requests().last().map(String::as_str), Some("balanced"));
}

/// Straddling the middle of the dead band (0.50) once a minute.
fn borderline() -> Box<Conditions> {
    Box::new(|minute| {
        conditions(Some(false), if (minute as u64).is_multiple_of(2) { 0.45 } else { 0.55 }, None, 60.0)
    })
}

/// The same load on a machine that is *off* its preferred mode: it goes
/// home once, the first time the load crosses the middle towards it, and
/// then the dead band holds it there however the load wobbles.
#[test]
fn borderline_load_off_the_preferred_mode_goes_home_once_and_stays() {
    let machine = Machine::new("minutes-deadband-home");
    let daemon = machine.boot();
    set(&daemon, PowerMode::Balanced);

    let switches = run_minutes(&daemon, &machine, 20, &borderline());

    assert_eq!(
        switches.iter().map(|(_, mode)| *mode).collect::<Vec<_>>(),
        vec![PowerMode::Performance],
        "one move to the mains preference and no flapping after it: {switches:?}"
    );
}

/// An afternoon: idle on mains, then a long build, then the chassis heats
/// up, then somebody unplugs it and the battery runs down.
///
/// The point is the *sequence*. Each of these rules is unit-tested on its
/// own; what they have never been asked is whether they compose into
/// something a person would recognise as sensible over forty minutes.
#[test]
fn forty_minutes_of_a_realistic_afternoon_ends_in_eco_without_thrashing() {
    let machine = Machine::new("minutes-afternoon");
    let daemon = machine.boot();
    set(&daemon, PowerMode::Balanced);

    let afternoon: Box<Conditions> = Box::new(|minute| match minute as u64 {
        // Idle, plugged in.
        0..=4 => conditions(Some(false), 0.10, None, 45.0),
        // A build starts.
        5..=14 => conditions(Some(false), 0.95, None, 68.0),
        // ...and the chassis catches up with it. Past temp_high_c (85).
        15..=24 => conditions(Some(false), 0.95, None, 89.0),
        // Unplugged, still warm - the latch does not clear until 75.
        25..=29 => conditions(Some(true), 0.90, Some(80.0), 82.0),
        // Cooled off, and the battery is going.
        _ => conditions(Some(true), 0.40, Some(18.0), 60.0),
    });

    let switches = run_minutes(&daemon, &machine, 40, &afternoon);
    let modes: Vec<PowerMode> = switches.iter().map(|(_, mode)| *mode).collect();

    assert_eq!(daemon.mode(), PowerMode::Eco, "a warm laptop on a low battery ends in Eco");
    assert_eq!(machine.hardware_profile(), "low-power");
    assert_eq!(machine.os_profile(), "power-saver");

    assert!(
        modes.contains(&PowerMode::Performance),
        "the build should have been given the machine: {switches:?}"
    );
    assert!(
        switches.len() <= 4,
        "forty minutes and four distinct events should not need more than four switches: {switches:?}"
    );
    // Never above Performance: the supervisor may not refine its way into
    // Unlimited, whatever the afternoon looked like.
    assert!(!modes.contains(&PowerMode::Unlimited), "Unlimited is the user's own choice");
}

/// Unplugging is the user speaking, and it is answered at once - even
/// while a manual choice is otherwise keeping the supervisor quiet.
#[test]
fn unplugging_mid_run_is_answered_immediately_and_the_machine_follows() {
    let machine = Machine::new("minutes-unplug");
    let daemon = machine.boot();
    set(&daemon, PowerMode::Performance);

    let unplug_at_five: Box<Conditions> = Box::new(|minute| {
        conditions(Some(minute >= 5.0), 0.95, Some(90.0), 60.0)
    });

    let switches = run_minutes(&daemon, &machine, 10, &unplug_at_five);

    let unplug = switches
        .iter()
        .find(|(_, mode)| *mode == PowerMode::Eco)
        .expect("unplugging a machine in Performance should drop it to the battery preference");
    assert!(
        (unplug.0 - 5.0).abs() < 0.2,
        "within a tick of the cable coming out, not three samples later: minute {}",
        unplug.0
    );
    // Still busy, so it earns Balanced back - but on battery load can
    // never reach Performance again.
    assert_eq!(daemon.mode(), PowerMode::Balanced, "the battery range tops out at Balanced");
    assert_eq!(machine.hardware_profile(), "balanced");
    assert!(!switches.iter().any(|(minute, mode)| *minute > 5.0 && *mode == PowerMode::Performance));
}

/// A mode picked in the app or with the performance key is what the
/// supervisor works around once its pause is over, and `getState` says so.
///
/// Balanced and Unlimited are used because neither is the default
/// preference on either power source, so the answer does not depend on
/// whether the machine running the test is plugged in.
#[test]
fn a_mode_picked_by_hand_is_reported_as_the_supervisors_baseline() {
    let machine = Machine::new("manual-baseline");
    let daemon = machine.boot();

    set(&daemon, PowerMode::Balanced);
    let state = daemon.call("getState", Value::Null).expect("getState");
    assert_eq!(state["autoManualBaseline"], "balanced");

    // Balanced -> Performance -> Unlimited, one key press at a time.
    daemon.cycle();
    let pressed = daemon.cycle();
    assert_eq!(pressed.to, PowerMode::Unlimited);
    let state = daemon.call("getState", Value::Null).expect("getState");
    assert_eq!(state["autoManualBaseline"], "unlimited", "a key press is the user choosing");
}

/// The real thread, the real clock, nobody connected. Ignored by default
/// because it takes minutes; run it with
///
/// ```text
/// PYREN_SOAK_SECS=300 cargo test -p pyren-power --test profiles -- --ignored --nocapture
/// ```
///
/// It is the cross-check on everything above: the simulated timelines
/// assert what the supervisor *decides*, and this asserts that the loop
/// which actually runs stays in step with the machine while doing it.
#[test]
#[ignore = "runs for minutes on the real clock"]
fn the_real_supervisor_stays_in_step_with_the_machine_for_minutes() {
    let seconds: u64 = std::env::var("PYREN_SOAK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);

    let machine = Machine::new("soak");
    let daemon = machine.boot();

    // One-second ticks so a two-minute soak is a couple of hundred of
    // them rather than a dozen, and a short override so the run is not
    // spent inside one.
    daemon
        .call(
            "setAutoConfig",
            serde_json::to_value(AutoConfig {
                enabled: true,
                interval_secs: 1,
                manual_override_secs: 5,
                ..AutoConfig::default()
            })
            .unwrap(),
        )
        .expect("setAutoConfig");
    set(&daemon, PowerMode::Balanced);

    let started = std::time::Instant::now();
    let mut observed = Vec::new();
    let mut last = daemon.mode();

    while started.elapsed().as_secs() < seconds {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mode = daemon.mode();
        if mode != last {
            observed.push((started.elapsed().as_secs(), mode));
            last = mode;
        }

        assert_eq!(
            machine.hardware_profile(),
            expected(mode).0,
            "after {}s the daemon and the machine disagree",
            started.elapsed().as_secs()
        );

        let state = daemon.call("getState", Value::Null).expect("getState");
        assert!(state["configSaveError"].is_null(), "the daemon should keep saving cleanly");
        assert_ne!(state["mode"], Value::Null);
    }

    println!("soak: {seconds}s, {} switches: {observed:?}", observed.len());
    assert!(
        observed.len() as u64 <= seconds / 20,
        "a supervisor switching more than once every twenty seconds is flapping: {observed:?}"
    );
}

/// The firmware vocabulary this project is actually developed against.
///
/// The ACPI ABI defines a fixed set of names but every firmware exposes
/// its own subset, and the test laptop's Eco is spelled `cool` rather
/// than `low-power`. A suite that only ever fed the module the spelling
/// in the ABI documentation would pass on a machine nobody owns.
#[test]
fn a_firmware_that_spells_eco_cool_gets_the_same_four_modes() {
    let machine = Machine::new("firmware-cool");
    machine.write("acpi/platform_profile_choices", "cool balanced performance");
    machine.write("acpi/platform_profile", "cool");
    let daemon = machine.boot();

    let on_this_firmware = |mode: PowerMode| match mode {
        PowerMode::Eco => "cool",
        PowerMode::Balanced => "balanced",
        PowerMode::Performance | PowerMode::Unlimited => "performance",
    };

    for mode in PowerMode::ALL {
        set(&daemon, *mode);
        assert_eq!(machine.hardware_profile(), on_this_firmware(*mode), "{mode:?}");
        assert_eq!(machine.os_profile(), expected(*mode).1, "{mode:?}: the OS half is unaffected");
    }

    // Performance has no `balanced-performance` to prefer here, so it and
    // Unlimited both land on `performance` - as they do on the ABI's own
    // spelling. Worth pinning: it means this firmware cannot tell the
    // project's top two modes apart either.
    set(&daemon, PowerMode::Performance);
    let performance = machine.hardware_profile();
    set(&daemon, PowerMode::Unlimited);
    assert_eq!(machine.hardware_profile(), performance);
}

/// A firmware offering names this module has never heard of. The mode
/// cannot be applied to the firmware, and that must be reported rather
/// than silently skipped - but the OS half still works, so the call as a
/// whole succeeds.
#[test]
fn a_firmware_with_unknown_names_is_reported_not_ignored() {
    let machine = Machine::new("firmware-unknown");
    machine.write("acpi/platform_profile_choices", "custom vendor-tuned");
    machine.write("acpi/platform_profile", "custom");
    let daemon = machine.boot();

    let report = set(&daemon, PowerMode::Eco);

    assert_eq!(machine.hardware_profile(), "custom", "nothing was written blindly");
    assert_eq!(machine.os_profile(), "power-saver", "the OS half still happened");
    let failed = report["failed"].as_array().expect("a report lists what it could not do");
    assert!(
        failed.iter().any(|f| f.as_str().unwrap_or_default().contains("platform_profile")),
        "the firmware half should say it had no name to use, said {failed:?}"
    );
}

// ---------------------------------------------------------------------
// The power-profiles-daemon race
// ---------------------------------------------------------------------

/// Found running `tools/power-soak.sh` against the reference laptop:
/// power-profiles-daemon 0.30 has its own `platform_profile` driver, so
/// `powerprofilesctl set power-saver` wrote the firmware profile itself,
/// to `balanced` - not the `cool` this module had just chosen for Eco a
/// moment earlier. Two writers on one file, and whichever ran last won.
///
/// `backend::plan` now puts the OS step first and the firmware step last
/// for exactly this reason: this module's own explicit write is the one
/// thing that gets to be final. This machine has a power-profiles service
/// that misbehaves exactly as the real one did, so a reordering that
/// reintroduces the race fails here rather than on someone's laptop.
#[test]
fn pyrens_own_firmware_write_wins_even_when_ppd_writes_it_too() {
    let machine = Machine::new("ppd-race");
    // Standing in for ppd's own mapping disagreeing with ours: whatever
    // mode is asked for, its driver decides `balanced` is close enough.
    machine.install_racing_profiles_service("balanced");
    machine.apply_env();
    let daemon = machine.boot();

    for mode in PowerMode::ALL {
        set(&daemon, *mode);
        let (hardware, os) = expected(*mode);
        assert_eq!(
            machine.hardware_profile(),
            hardware,
            "{mode:?}: pyren's own choice must survive ppd's side effect"
        );
        assert_eq!(machine.os_profile(), os, "{mode:?}: the OS profile is still what was asked");
    }
}

/// The retry that recovers from the mismatch, in one call: a caller of
/// `setMode` never has to know it happened.
#[test]
fn a_ppd_that_misses_once_is_retried_and_the_call_still_succeeds() {
    let machine = Machine::new("ppd-flaky");
    machine.install_flaky_profiles_service();
    machine.apply_env();
    let daemon = machine.boot();

    let report = set(&daemon, PowerMode::Eco);

    assert_eq!(machine.os_profile(), "power-saver", "the retry got there in the end");
    let applied = report["applied"].as_array().expect("applied is a list");
    assert!(
        applied.iter().any(|a| a == "power-profiles-daemon=power-saver"),
        "a recovered attempt is a success, not a footnote in failed: {applied:?}"
    );
    assert_eq!(report["failed"], json!([]), "the caller never sees the first miss");
    assert_eq!(
        machine.os_profile_requests().len(),
        2,
        "one miss and one retry, not a loop and not a single silent attempt"
    );
}

/// A ppd that is wrong every time is not retried forever: two attempts,
/// then it is reported as what it is - a mechanism that could not be
/// applied - rather than as a success it never reached.
#[test]
fn a_permanently_wrong_ppd_is_given_up_on_and_reported_failed() {
    let machine = Machine::new("ppd-stuck");
    machine.install_permanently_wrong_profiles_service("balanced");
    machine.apply_env();
    let daemon = machine.boot();

    let report = set(&daemon, PowerMode::Eco);

    assert_eq!(machine.hardware_profile(), "low-power", "the firmware half is unrelated and still works");
    let failed = report["failed"].as_array().expect("failed is a list");
    assert!(
        failed.iter().any(|f| {
            let f = f.as_str().unwrap_or_default();
            f.contains("power-saver") && f.contains("balanced")
        }),
        "should say what was asked for and what it got instead, said {failed:?}"
    );
    assert!(
        !report["applied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a.as_str().unwrap_or_default().starts_with("power-profiles-daemon")),
        "never claimed as applied: {:?}",
        report["applied"]
    );
    assert_eq!(
        machine.os_profile_requests().len(),
        2,
        "exactly OS_PROFILE_ATTEMPTS tries, not an unbounded retry loop"
    );
}

// ---------------------------------------------------------------------
// Whichever power manager the OS runs
// ---------------------------------------------------------------------

/// The failure this module must never cause: a power-profiles-daemon that
/// is installed but disabled - because the user runs TLP or auto-cpufreq -
/// is still bus-activatable, and its unit `Conflicts=` with both. A client
/// that merely *asks* it for the profile starts it, and systemd stops the
/// power manager the user chose.
///
/// This machine has nothing serving the API, a `busctl` that logs how it
/// was called, and a `powerprofilesctl` that would have been the
/// activation. Nothing may reach the latter, and every bus call has to
/// have gone out with auto-start off.
#[test]
fn asking_about_the_os_profile_never_starts_a_power_manager() {
    let machine = Machine::new("no-activation");
    machine.install_tool(
        "busctl",
        "#!/bin/sh\n\
         root=$(dirname \"$0\")/..\n\
         echo \"$*\" >> \"$root/busctl.log\"\n\
         echo 'Call failed: Destination does not exist' >&2\n\
         exit 1\n",
    );
    machine.install_tool(
        "powerprofilesctl",
        "#!/bin/sh\n\
         root=$(dirname \"$0\")/..\n\
         echo activated >> \"$root/activated.log\"\n",
    );
    machine.apply_env();
    let daemon = machine.boot();

    daemon.call("getState", json!(null)).expect("getState");
    let report = set(&daemon, PowerMode::Eco);

    assert_eq!(machine.read("activated.log"), None, "powerprofilesctl would have started the service");
    let calls = machine.read("busctl.log").expect("the bus was asked");
    assert!(
        calls.lines().all(|call| call.contains("--auto-start=no")),
        "every bus call must refuse to auto-start its destination: {calls}"
    );
    // With no manager at all, the hint is the OS half again.
    assert!(
        report["applied"].as_array().unwrap().iter().any(|a| a
            .as_str()
            .unwrap_or_default()
            .starts_with("energy_performance_preference=power")),
        "{report}"
    );
}

/// No systemd, so no `busctl`: `powerprofilesctl` is how the API is
/// reached, and there it cannot start anything (see `ProfilesEndpoint`).
#[test]
fn without_busctl_the_profiles_api_is_reached_through_powerprofilesctl() {
    let machine = Machine::new("ppd-cli");
    machine.remove_tool("busctl");
    machine.install_tool(
        "powerprofilesctl",
        "#!/bin/sh\n\
         root=$(dirname \"$0\")/..\n\
         case \"$1\" in\n\
         get) cat \"$root/os_profile\" ;;\n\
         set) printf '%s' \"$2\" > \"$root/os_profile\" ;;\n\
         *) exit 2 ;;\n\
         esac\n",
    );
    machine.apply_env();
    let daemon = machine.boot();

    for mode in PowerMode::ALL {
        set(&daemon, *mode);
        assert_eq!(machine.os_profile(), expected(*mode).1, "{mode:?}");
    }
}

/// TLP 1.8+ without tlp-pd: every mode is a TLP profile, the firmware
/// profile is still pyren's own, and the CPU hint is left to TLP - which
/// would rewrite it on the next charger event anyway.
#[test]
fn on_a_tlp_machine_every_mode_is_a_tlp_profile() {
    let machine = Machine::new("tlp");
    machine.install_tlp("balanced");
    machine.apply_env();
    let daemon = machine.boot();

    let state = daemon.call("getState", json!(null)).expect("getState");
    assert_eq!(state["backend"]["tlp"], json!("balanced"));
    assert!(state["backend"]["available"].as_array().unwrap().contains(&json!("tlp")));

    for mode in PowerMode::ALL {
        let report = set(&daemon, *mode);
        let (hardware, os) = expected(*mode);
        assert_eq!(machine.read("tlp_profile").as_deref(), Some(os), "{mode:?}: TLP's profile");
        assert_eq!(machine.hardware_profile(), hardware, "{mode:?}: the laptop's own");
        assert!(
            report["applied"].as_array().unwrap().contains(&json!(format!("tlp={os}"))),
            "{mode:?}: {report}"
        );
        assert_eq!(report["failed"], json!([]), "{mode:?}");
    }
    assert_eq!(
        machine.read("cpu/cpu0/cpufreq/energy_performance_preference").as_deref(),
        Some("balance_performance"),
        "the hint is TLP's to set"
    );
}

/// A TLP too old for profiles is not a manager this can ask anything, so
/// the machine is treated as having none.
#[test]
fn a_tlp_without_profiles_is_not_mistaken_for_one() {
    let machine = Machine::new("tlp-old");
    machine.install_tlp("balanced");
    machine.install_tool("tlp-stat", "#!/bin/sh\necho AC\n");
    machine.apply_env();
    let daemon = machine.boot();

    let report = set(&daemon, PowerMode::Eco);
    assert_eq!(machine.read("tlp.log"), None, "an old TLP is never sent a profile");
    assert!(report["applied"].as_array().unwrap().iter().any(|a| a
        .as_str()
        .unwrap_or_default()
        .starts_with("energy_performance_preference")));
}

/// auto-cpufreq rewrites the governor and EPP every few seconds, so it is
/// forced for Eco and Performance and handed back its own judgement for
/// Balanced - alongside TLP, on a machine that runs both.
#[test]
fn a_running_auto_cpufreq_is_forced_and_reset_with_the_modes() {
    let machine = Machine::new("auto-cpufreq");
    machine.install_tlp("balanced");
    machine.install_auto_cpufreq();
    machine.apply_env();
    let daemon = machine.boot();

    let state = daemon.call("getState", json!(null)).expect("getState");
    assert_eq!(state["backend"]["autoCpufreq"], json!(true));

    for (mode, force, state) in [
        (PowerMode::Eco, "powersave", "powersave"),
        (PowerMode::Balanced, "reset", "default"),
        (PowerMode::Performance, "performance", "performance"),
        (PowerMode::Unlimited, "performance", "performance"),
    ] {
        let report = set(&daemon, mode);
        let applied = report["applied"].as_array().unwrap();
        assert!(applied.contains(&json!(format!("auto-cpufreq={force}"))), "{mode:?}: {report}");
        assert!(applied.contains(&json!(format!("tlp={}", expected(mode).1))), "{mode:?}: {report}");
        assert_eq!(
            machine.read("acf_override").unwrap_or_else(|| "default".into()),
            state,
            "{mode:?}"
        );
    }
    assert_eq!(
        machine.read("cpu/cpu0/cpufreq/energy_performance_preference").as_deref(),
        Some("balance_performance"),
        "never written underneath auto-cpufreq"
    );
}

/// An auto-cpufreq that is installed but not running is not asked: its
/// `--force` refuses without the daemon, and would be undone by nothing.
#[test]
fn an_auto_cpufreq_that_is_not_running_is_left_alone() {
    let machine = Machine::new("auto-cpufreq-stopped");
    machine.install_auto_cpufreq();
    machine.install_tool("pgrep", "#!/bin/sh\nexit 1\n");
    machine.apply_env();
    let daemon = machine.boot();

    set(&daemon, PowerMode::Eco);
    assert_eq!(machine.read("acf.log"), None);
    assert_eq!(machine.os_profile(), "power-saver", "the profiles service is still asked");
}

// ---------------------------------------------------------------------
// Something else moves what the daemon set
// ---------------------------------------------------------------------

/// Fn+P (the kernel's `platform_profile_cycle`), the desktop's menu or a
/// power manager on a charger event: the firmware profile is now another
/// mode's, and the daemon follows - its mode and that mode's envelope -
/// without pushing the change back to the OS profile, which is the
/// likeliest writer.
#[test]
fn a_firmware_profile_moved_from_outside_is_followed() {
    let machine = Machine::new("external-follow");
    let daemon = machine.boot();
    daemon
        .call("setTuning", json!({ "mode": "performance", "pl1W": 55.0 }))
        .expect("tuning Performance");
    set(&daemon, PowerMode::Eco);
    let requests = machine.os_profile_requests().len();

    machine.write("acpi/platform_profile", "performance");
    // The module's own watcher thread may have got there first, so what
    // is asserted is the outcome, not which of the two looks found it.
    daemon.check_external();

    assert_eq!(daemon.mode(), PowerMode::Performance);
    assert_eq!(machine.limits().pl1_uw, Some(54_670_000), "Performance's envelope came with it");
    assert_eq!(machine.hardware_profile(), "performance", "the outside choice is left standing");
    assert_eq!(machine.os_profile_requests().len(), requests, "the OS profile is not pushed back");

    let state = daemon.call("getState", json!(null)).expect("getState");
    assert_eq!(state["mode"], json!("performance"));
    assert!(state["lastExternal"]["text"].as_str().unwrap_or_default().contains("performance"));
    // Seconds after the daemon's own write, so it also reads as a revert.
    assert_eq!(state["overrides"][0]["knob"], json!("platform_profile"));
    assert_eq!(state["overrides"][0]["reverted"], json!(true));

    assert_eq!(daemon.check_external(), None, "followed once, not on every look");
    assert_eq!(machine.saved_config().mode, Some(PowerMode::Performance), "and remembered");

    // The daemon's own next choice starts from a clean slate.
    set(&daemon, PowerMode::Balanced);
    let state = daemon.call("getState", json!(null)).expect("getState");
    assert_eq!(state["overrides"], json!([]));
    assert_eq!(state["lastExternal"], json!(null));
}

/// The daemon's own writes - every mode, twenty rounds, with a ppd whose
/// driver writes the firmware profile too - are never mistaken for
/// someone else's.
#[test]
fn the_daemons_own_writes_are_never_taken_for_someone_elses() {
    let machine = Machine::new("external-own");
    machine.install_racing_profiles_service("balanced");
    machine.apply_env();
    let daemon = machine.boot();

    for _ in 0..20 {
        for mode in PowerMode::ALL {
            set(&daemon, *mode);
            assert_eq!(daemon.check_external(), None, "{mode:?}");
        }
    }
    let state = daemon.call("getState", json!(null)).expect("getState");
    assert_eq!(state["overrides"], json!([]));
}

/// With no power manager the daemon writes the CPU hint itself; an
/// unrecognised program changing it afterwards is reported, and the
/// daemon does not fight it.
#[test]
fn a_hint_changed_by_an_unknown_program_is_reported_and_not_rewritten() {
    let machine = Machine::new("external-epp");
    machine.remove_tool("busctl");
    machine.apply_env();
    let daemon = machine.boot();
    set(&daemon, PowerMode::Eco);
    assert_eq!(machine.read("cpu/cpu0/cpufreq/energy_performance_preference").as_deref(), Some("power"));

    machine.write("cpu/cpu0/cpufreq/energy_performance_preference", "balance_performance");
    assert_eq!(daemon.check_external(), None, "the hint says nothing about the mode");
    daemon.check_external();

    assert_eq!(daemon.mode(), PowerMode::Eco);
    assert_eq!(
        machine.read("cpu/cpu0/cpufreq/energy_performance_preference").as_deref(),
        Some("balance_performance"),
        "not fought over"
    );
    let state = daemon.call("getState", json!(null)).expect("getState");
    assert_eq!(
        state["overrides"],
        json!([{
            "knob": "energy_performance_preference",
            "expected": "power",
            "found": "balance_performance",
            "reverted": true,
        }]),
        "one entry however many looks"
    );
}

/// The thread, not the method: nothing calls `check_external` here, as
/// nothing does on a machine whose app is closed.
#[test]
fn the_watcher_follows_the_machine_with_nobody_asking() {
    let machine = Machine::new("external-thread");
    let daemon = machine.boot();
    set(&daemon, PowerMode::Eco);

    machine.write("acpi/platform_profile", "balanced");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while daemon.mode() != PowerMode::Balanced && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(daemon.mode(), PowerMode::Balanced, "followed within a few seconds");
}
