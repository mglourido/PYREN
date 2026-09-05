//! Do the performance profiles' energy settings actually reach the
//! hardware - and in Unlimited, do the fan modes?
//!
//! This is the question `crates/power/tests/profiles.rs` deliberately does
//! not answer. That file is about *which* profile a mode selects; this one
//! is about the **envelope**: the package power limits and the turbo knob
//! that Performance and Unlimited are the only two modes to expose, and
//! which are the half of a profile the fans actually feel.
//!
//! It lives in the daemon rather than in a crate because the question is
//! cross-module and the daemon is the only place that can see all of it:
//!
//! | module | what it owns here |
//! |---|---|
//! | `pyren-power` | the package limits, the turbo knob, the profile |
//! | `pyren-fan` | `pwm1` and `pwm1_enable` - the fans themselves |
//! | `pyren-overclock` | the GPU's offsets, and nothing else |
//!
//! **Those three never call each other**, which is the design and is also
//! the thing most worth testing: the app presents Unlimited as the mode
//! that unlocks manual power limits *and* manual fan control, but that
//! grouping is the frontend's policy, not the daemon's. The daemon will
//! set a fan curve in Eco perfectly happily. So what is asserted here is
//! not "Unlimited enables the fans" - it is that each owner moves its own
//! hardware, that none of them moves anybody else's, and that the
//! combination the app calls Unlimited does in fact land on the machine.
//!
//! Nothing here touches a real GPU. The overclock module is exercised for
//! its boundary - that it does not raise the CPU's limits - and for its
//! consent gate, both of which are reachable without `nvidia-smi`. The
//! offsets themselves are covered by that crate's own tests, and running
//! them against real silicon is what its consent text is about.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use pyren_config::ConfigStore;
use pyren_core::Module;
use pyren_fan::FanModule;
use pyren_overclock::OverclockModule;
use pyren_power::{Limits, PowerMode, PowerModule};
use serde_json::{json, Value};

const W: u64 = 1_000_000;

/// The reference laptop's own envelope, read off it with `pyren-ctl power
/// get`: 77 W sustained and boost, 168 W instantaneous.
const STOCK_PL1: u64 = 77 * W;
const STOCK_PL2: u64 = 77 * W;
const STOCK_PL4: u64 = 168 * W;

/// `PYREN_*` overrides are process-global, so only one fake machine may
/// exist at a time.
fn machine_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A laptop with an envelope and a set of fans, neither of which is real.
struct Machine {
    root: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl Machine {
    fn new(tag: &str) -> Self {
        Self::with_fan_files(tag, &["pwm1", "pwm1_enable", "fan1_input", "fan2_input"])
    }

    /// `fan_files` is what this machine's hp-wmi driver exposes. Leaving
    /// `pwm1` out of it is board 8D2F, and is the case that decides
    /// whether curve and manual are offered at all.
    fn with_fan_files(tag: &str, fan_files: &[&str]) -> Self {
        let guard = machine_lock().lock().unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("pyren-energy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let machine = Self { root, _guard: guard };

        // --- the power half ---
        machine.write("acpi/platform_profile", "balanced");
        machine.write("acpi/platform_profile_choices", "cool balanced performance");
        machine.write("cpu/intel_pstate/no_turbo", "0");
        machine.write("powercap/intel-rapl:0/name", "package-0");
        machine.write_limits(stock());
        machine.write_os_profile("balanced");
        machine.install_powerprofilesctl();

        // --- the fan half ---
        for file in fan_files {
            // `pwm1_enable` starts at 2, the firmware's own curve, which is
            // what a machine nobody has touched is in.
            machine.write(&format!("hwmon/{file}"), if *file == "pwm1_enable" { "2" } else { "0" });
        }

        machine.apply_env();
        machine
    }

    fn write(&self, name: &str, contents: &str) {
        let path = self.root.join(name);
        std::fs::create_dir_all(path.parent().expect("nested")).expect("fixture dir");
        std::fs::write(path, contents).expect("fixture file");
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(name)).ok().map(|v| v.trim().to_string())
    }

    fn write_limits(&self, limits: Limits) {
        for (constraint, value) in [(0, limits.pl1_uw), (1, limits.pl2_uw), (2, limits.pl4_uw)] {
            if let Some(value) = value {
                self.write(
                    &format!("powercap/intel-rapl:0/constraint_{constraint}_power_limit_uw"),
                    &value.to_string(),
                );
            }
        }
    }

    fn write_os_profile(&self, profile: &str) {
        self.write("os_profile", profile);
    }

    fn install_powerprofilesctl(&self) {
        self.write(
            "bin/powerprofilesctl",
            "#!/bin/sh\n\
             root=$(dirname \"$0\")/..\n\
             case \"$1\" in\n\
             get) cat \"$root/os_profile\" ;;\n\
             set) printf '%s' \"$2\" > \"$root/os_profile\" ;;\n\
             *) exit 2 ;;\n\
             esac\n",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                self.root.join("bin/powerprofilesctl"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("runnable");
        }
    }

    fn apply_env(&self) {
        std::env::set_var("PYREN_PLATFORM_PROFILE", self.root.join("acpi/platform_profile"));
        std::env::set_var("PYREN_CPU_ROOT", self.root.join("cpu"));
        std::env::set_var("PYREN_POWERCAP", self.root.join("powercap"));
        std::env::set_var("PYREN_POWERPROFILESCTL", self.root.join("bin/powerprofilesctl"));
        std::env::set_var("PYREN_HWMON_DIR", self.root.join("hwmon"));
    }

    fn store(&self, name: &str) -> ConfigStore {
        ConfigStore::at(self.root.join("config").join(name))
    }

    fn power(&self) -> PowerModule {
        PowerModule::with_store(self.store("power"))
    }

    fn fan(&self) -> FanModule {
        FanModule::with_store(self.store("fan"))
    }

    fn overclock(&self) -> OverclockModule {
        OverclockModule::with_store(self.store("overclock"))
    }

    // --- what the machine says about itself ---

    /// The package limits, in microwatts, as the kernel would report them.
    fn limits(&self) -> Limits {
        let read = |c: u8| {
            self.read(&format!("powercap/intel-rapl:0/constraint_{c}_power_limit_uw"))
                .and_then(|v| v.parse().ok())
        };
        Limits { pl1_uw: read(0), pl2_uw: read(1), pl4_uw: read(2) }
    }

    /// `intel_pstate/no_turbo` is inverted: 1 means turbo is *off*.
    fn turbo(&self) -> bool {
        self.read("cpu/intel_pstate/no_turbo").as_deref() == Some("0")
    }

    /// What `pwm1_enable` means: 0 max, 1 manual, 2 the firmware's curve.
    fn fan_enable(&self) -> Option<String> {
        self.read("hwmon/pwm1_enable")
    }

    fn fan_pwm(&self) -> Option<u8> {
        self.read("hwmon/pwm1")?.parse().ok()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        for name in [
            "PYREN_PLATFORM_PROFILE",
            "PYREN_CPU_ROOT",
            "PYREN_POWERCAP",
            "PYREN_POWERPROFILESCTL",
            "PYREN_HWMON_DIR",
        ] {
            std::env::remove_var(name);
        }
    }
}

fn stock() -> Limits {
    Limits { pl1_uw: Some(STOCK_PL1), pl2_uw: Some(STOCK_PL2), pl4_uw: Some(STOCK_PL4) }
}

fn mode_name(mode: PowerMode) -> &'static str {
    match mode {
        PowerMode::Eco => "eco",
        PowerMode::Balanced => "balanced",
        PowerMode::Performance => "performance",
        PowerMode::Unlimited => "unlimited",
    }
}

fn set_mode(power: &PowerModule, mode: PowerMode) {
    power
        .call("setMode", json!({ "mode": mode_name(mode) }))
        .unwrap_or_else(|e| panic!("setMode {mode:?}: {e:?}"));
}

/// Watts, as the app sends them, onto the mode named.
fn tune(power: &PowerModule, mode: PowerMode, params: Value) {
    let mut params = params;
    params["mode"] = json!(mode_name(mode));
    power.call("setTuning", params).unwrap_or_else(|e| panic!("setTuning {mode:?}: {e:?}"));
}

/// Percentages are what is stored, so a request in watts comes back
/// quantised to one percent of this machine's own ceiling. Every
/// assertion about watts here goes through this rather than pretending
/// the number survives the round trip exactly.
fn as_stored(watts: f64, stock_uw: u64) -> u64 {
    let percent = (watts * 1_000_000.0 / stock_uw as f64 * 100.0).round() as u64;
    stock_uw / 100 * percent
}

// ---------------------------------------------------------------------
// The energy settings, and whether they take effect
// ---------------------------------------------------------------------

/// The headline claim: a number typed into Performance's power limits
/// reaches the powercap interface, and is gone again in Unlimited.
///
/// Performance and Unlimited are the only two modes the app offers these
/// for, and they are offered *per mode* - so the interesting assertion is
/// not that one write happened, but that switching between the two swaps
/// one envelope for the other on the hardware.
#[test]
fn performance_and_unlimited_carry_their_own_envelopes_onto_the_hardware() {
    let machine = Machine::new("envelopes");
    let power = machine.power();

    // Performance: measured on this machine, well under stock.
    tune(&power, PowerMode::Performance, json!({ "pl1W": 45.0, "pl2W": 60.0 }));
    // Unlimited: what the name says. Left at the firmware's own.
    tune(&power, PowerMode::Unlimited, json!({ "pl1W": 77.0, "pl2W": 77.0 }));

    set_mode(&power, PowerMode::Performance);
    assert_eq!(
        machine.limits().pl1_uw,
        Some(as_stored(45.0, STOCK_PL1)),
        "Performance's sustained limit reached the hardware"
    );
    assert_eq!(machine.limits().pl2_uw, Some(as_stored(60.0, STOCK_PL2)));
    assert_eq!(machine.limits().pl4_uw, Some(STOCK_PL4), "PL4 is never scaled by a profile");

    set_mode(&power, PowerMode::Unlimited);
    assert_eq!(machine.limits(), stock(), "Unlimited gives the whole envelope back");

    // ...and back again, because a one-way test would not catch a mode
    // that only applies its envelope the first time it is entered.
    set_mode(&power, PowerMode::Performance);
    assert_eq!(machine.limits().pl1_uw, Some(as_stored(45.0, STOCK_PL1)));
}

/// Turbo is the other half of the energy settings, and it is a knob with
/// an inverted polarity - `no_turbo=1` means off - so a mode that turned
/// it off and one that failed to turn it on look identical in a config
/// file and opposite on the machine.
#[test]
fn turning_turbo_off_for_one_mode_leaves_every_other_mode_boosting() {
    let machine = Machine::new("turbo");
    let power = machine.power();

    tune(&power, PowerMode::Performance, json!({ "turbo": false }));

    set_mode(&power, PowerMode::Performance);
    assert!(!machine.turbo(), "Performance was told not to boost");
    assert_eq!(machine.read("cpu/intel_pstate/no_turbo").as_deref(), Some("1"));

    set_mode(&power, PowerMode::Unlimited);
    assert!(machine.turbo(), "Unlimited never gave up turbo, so entering it restores boost");

    set_mode(&power, PowerMode::Eco);
    assert!(machine.turbo(), "and neither did Eco - no mode ships an opinion about it");
}

/// The ceiling, from the outside. Nothing a profile does may raise a
/// limit past what the firmware shipped: that is overclocking the CPU,
/// which is a separate feature behind separate consent and is not what
/// picking "Unlimited" asked for.
#[test]
fn no_mode_can_be_tuned_above_the_firmwares_own_ceiling() {
    let machine = Machine::new("ceiling");
    let power = machine.power();

    tune(&power, PowerMode::Unlimited, json!({ "pl1W": 250.0, "pl2W": 250.0 }));
    set_mode(&power, PowerMode::Unlimited);

    assert_eq!(machine.limits(), stock(), "asking for 250 W got the machine's own 77 W");
    assert!(
        machine.limits().pl1_uw.unwrap() <= STOCK_PL1,
        "the whole point: never above what the firmware shipped"
    );
}

/// A percentage that works out to almost nothing is floored rather than
/// applied. A CPU capped at half a watt is a machine that does not
/// respond, which is a worse outcome than ignoring the request.
#[test]
fn an_absurdly_low_limit_is_floored_not_obeyed() {
    let machine = Machine::new("floor");
    let power = machine.power();

    tune(&power, PowerMode::Performance, json!({ "pl1W": 0.5 }));
    set_mode(&power, PowerMode::Performance);

    let applied = machine.limits().pl1_uw.expect("something was applied");
    assert!(applied >= 5 * W, "floored at something survivable, got {applied} uW");
    assert!(applied < 10 * W, "but still recognisably the low limit that was asked for");
}

/// Each mode's envelope is its own. Tuning Performance must not quietly
/// become the setting Eco and Balanced use too - they are four separate
/// stored profiles and the app edits them one at a time.
#[test]
fn tuning_one_mode_does_not_touch_what_the_other_three_apply() {
    let machine = Machine::new("isolation");
    let power = machine.power();

    tune(&power, PowerMode::Performance, json!({ "pl1W": 40.0, "turbo": false }));

    for untouched in [PowerMode::Eco, PowerMode::Balanced, PowerMode::Unlimited] {
        set_mode(&power, untouched);
        assert_eq!(
            machine.limits(),
            stock(),
            "{untouched:?} was never tuned, so it applies the machine's own envelope"
        );
        assert!(machine.turbo(), "{untouched:?} was never told to give up turbo");
    }

    set_mode(&power, PowerMode::Performance);
    assert_eq!(machine.limits().pl1_uw, Some(as_stored(40.0, STOCK_PL1)));
    assert!(!machine.turbo());
}

/// Tuning the mode the machine is *already in* has to be audible now,
/// not at the next mode change - a slider that does nothing until you
/// click away and back is a slider that looks broken.
#[test]
fn editing_the_envelope_of_the_current_mode_applies_it_immediately() {
    let machine = Machine::new("live-edit");
    let power = machine.power();

    set_mode(&power, PowerMode::Unlimited);
    assert_eq!(machine.limits(), stock());

    // No setMode call after this: the tuning itself has to land.
    tune(&power, PowerMode::Unlimited, json!({ "pl1W": 35.0 }));

    assert_eq!(
        machine.limits().pl1_uw,
        Some(as_stored(35.0, STOCK_PL1)),
        "the envelope moved without the mode being re-selected"
    );
}

/// A tuned envelope is a setting, so it survives the daemon restarting -
/// and, because limits are stored as a percentage of the machine's own
/// ceiling, it comes back meaning the same watts.
#[test]
fn a_tuned_envelope_survives_a_daemon_restart() {
    let machine = Machine::new("envelope-restart");
    let first = machine.power();
    tune(&first, PowerMode::Performance, json!({ "pl1W": 45.0, "turbo": false }));
    first.call("setRestoreOnStart", json!({ "enabled": true })).expect("restore on");
    set_mode(&first, PowerMode::Performance);

    let applied = machine.limits();
    drop(first);

    // The machine comes back up at the firmware's defaults.
    machine.write_limits(stock());
    machine.write("cpu/intel_pstate/no_turbo", "0");

    let restarted = machine.power();

    assert_eq!(restarted.mode(), PowerMode::Performance);
    assert_eq!(machine.limits(), applied, "the same watts, to the microwatt");
    assert!(!machine.turbo(), "turbo is part of the profile and came back with it");
}

/// A machine with no powercap at all - the envelope is simply not one of
/// the things a profile can do there, and asking for it is an error
/// rather than a write that goes nowhere.
#[test]
fn a_machine_with_no_powercap_refuses_to_pretend_it_has_an_envelope() {
    let machine = Machine::new("no-powercap");
    std::fs::remove_dir_all(machine.root.join("powercap")).expect("take the zone away");
    let power = machine.power();

    let refused = power.call("setTuning", json!({ "mode": "unlimited", "pl1W": 45.0 }));
    assert!(refused.is_err(), "a percentage of nothing is not a limit");

    // The rest of the profile still works.
    set_mode(&power, PowerMode::Unlimited);
    assert_eq!(machine.read("acpi/platform_profile").as_deref(), Some("performance"));
}

// ---------------------------------------------------------------------
// The fan modes Unlimited unlocks
// ---------------------------------------------------------------------
//
// "Unlimited" is where the app offers max, auto, manual and curve, and
// the modes below are asserted in that context - but the daemon itself
// has no such gate, and these tests set the power mode to Unlimited only
// so the scenario is the real one. What is being tested is the fan
// module's own writes.
//
// `manual` gets one test and no more, on purpose: it is the one mode
// that pins the fans at a speed nobody is watching, and the reference
// laptop is not a machine to leave sitting there. What matters about it
// is that it is accepted where `pwm1` exists and refused where it does
// not, and that is one assertion each.

/// The safe state, and the one every other mode has to be able to get
/// back to: `pwm1_enable = 2` hands the fans to the firmware's own curve.
#[test]
fn auto_hands_the_fans_back_to_the_firmware() {
    let machine = Machine::new("fan-auto");
    let power = machine.power();
    let fan = machine.fan();
    set_mode(&power, PowerMode::Unlimited);

    // Away from auto first, so returning to it is a real change.
    fan.call("setMode", json!({ "mode": "max" })).expect("max");
    assert_eq!(machine.fan_enable().as_deref(), Some("0"), "max is pwm1_enable=0");

    fan.call("setMode", json!({ "mode": "auto" })).expect("auto");
    assert_eq!(machine.fan_enable().as_deref(), Some("2"), "auto is the firmware's curve");

    let status = fan.call("getStatus", Value::Null).expect("status");
    assert_eq!(status["mode"], json!("auto"));
}

/// Curve mode, end to end: the stored curve, this machine's temperature,
/// and the PWM that actually got written.
///
/// The temperature is whatever the machine running the test happens to
/// be at - there is no override for the CPU sensor - so the expected PWM
/// is computed from that same reading through the module's own public
/// `curve::target_pwm` rather than hard-coded. That keeps the assertion
/// exact without pretending to control the thermometer.
#[test]
fn curve_mode_writes_the_pwm_the_curve_asks_for_at_this_temperature() {
    let machine = Machine::new("fan-curve");
    let power = machine.power();
    let fan = machine.fan();
    set_mode(&power, PowerMode::Unlimited);

    let curve = json!([
        { "tempC": 30, "percent": 20 },
        { "tempC": 50, "percent": 45 },
        { "tempC": 70, "percent": 75 },
        { "tempC": 90, "percent": 100 },
    ]);
    fan.call("setCurve", json!({ "curve": curve, "interpolation": "smooth" })).expect("setCurve");
    fan.call("setMode", json!({ "mode": "curve" })).expect("curve");

    let status = fan.call("getStatus", Value::Null).expect("status");
    assert_eq!(status["mode"], json!("curve"));
    assert_eq!(machine.fan_enable().as_deref(), Some("1"), "a curve drives pwm1, so manual mode");

    let temp = status["cpuTempC"].as_f64().expect("this machine has a CPU sensor");
    let points: Vec<pyren_fan::CurvePoint> = serde_json::from_value(curve).expect("points");
    let wanted = pyren_fan::curve::target_pwm(&points, temp, pyren_fan::Interpolation::Smooth)
        .expect("the curve covers this temperature");

    assert_eq!(
        machine.fan_pwm(),
        Some(wanted),
        "at {temp} C this curve asks for pwm {wanted}, and that is what reached the hardware"
    );
}

/// A flat curve is the same assertion with the thermometer taken out of
/// it: every temperature maps to one percentage, so the PWM is knowable
/// without reading a sensor at all.
#[test]
fn a_flat_curve_pins_the_pwm_whatever_the_temperature_is() {
    let machine = Machine::new("fan-curve-flat");
    let fan = machine.fan();

    fan.call(
        "setCurve",
        json!({ "curve": [{ "tempC": 0, "percent": 60 }, { "tempC": 110, "percent": 60 }] }),
    )
    .expect("setCurve");
    fan.call("setMode", json!({ "mode": "curve" })).expect("curve");

    assert_eq!(
        machine.fan_pwm(),
        Some(pyren_fan::curve::percent_to_pwm(60.0)),
        "60 % of full speed, whatever this machine's CPU happens to be at"
    );
}

/// Changing the curve while curve mode is running has to move the fans
/// now. Waiting for the next tick would be a curve editor whose preview
/// lags behind the thing it is previewing.
#[test]
fn editing_the_curve_while_it_is_running_moves_the_fans_at_once() {
    let machine = Machine::new("fan-curve-live");
    let fan = machine.fan();

    let flat = |percent: u64| {
        json!({ "curve": [{ "tempC": 0, "percent": percent }, { "tempC": 110, "percent": percent }] })
    };

    fan.call("setCurve", flat(40)).expect("setCurve");
    fan.call("setMode", json!({ "mode": "curve" })).expect("curve");
    assert_eq!(machine.fan_pwm(), Some(pyren_fan::curve::percent_to_pwm(40.0)));

    fan.call("setCurve", flat(80)).expect("a second curve");
    assert_eq!(
        machine.fan_pwm(),
        Some(pyren_fan::curve::percent_to_pwm(80.0)),
        "the new curve took effect without the mode being re-selected"
    );
}

/// Manual, once: it is accepted where `pwm1` exists, and the speed asked
/// for is the speed written. Nothing is left running - the test returns
/// the machine to auto, which is what the module does for every other
/// caller too.
#[test]
fn manual_writes_the_speed_it_was_given_and_then_gives_the_fans_back() {
    let machine = Machine::new("fan-manual");
    let fan = machine.fan();

    fan.call("setMode", json!({ "mode": "manual", "pwm": 96 })).expect("manual");
    assert_eq!(machine.fan_enable().as_deref(), Some("1"));
    assert_eq!(machine.fan_pwm(), Some(96), "the speed asked for is the speed written");

    fan.call("setMode", json!({ "mode": "auto" })).expect("back to auto");
    assert_eq!(machine.fan_enable().as_deref(), Some("2"), "the firmware has the fans again");
}

/// Manual without a speed is a request that cannot be honoured, and
/// guessing one would be picking a fan speed on the user's behalf.
#[test]
fn manual_without_a_speed_is_refused_rather_than_guessed_at() {
    let machine = Machine::new("fan-manual-nopwm");
    let fan = machine.fan();

    assert!(fan.call("setMode", json!({ "mode": "manual" })).is_err());
    assert_eq!(machine.fan_enable().as_deref(), Some("2"), "and nothing was written");
}

/// Board 8D2F: `pwm1_enable` without `pwm1`. Max and auto are real
/// there; manual and curve are not, and have to be refused rather than
/// half-applied - which is also why the app asks `capabilities` before
/// it draws the slider.
#[test]
fn a_machine_that_cannot_set_a_speed_still_switches_between_auto_and_max() {
    let machine = Machine::with_fan_files("fan-nopwm", &["pwm1_enable", "fan1_input"]);
    let fan = machine.fan();

    let status = fan.call("getStatus", Value::Null).expect("status");
    assert_eq!(status["capabilities"]["switchMode"], json!(true));
    assert_eq!(status["capabilities"]["setSpeed"], json!(false));

    fan.call("setMode", json!({ "mode": "max" })).expect("max is a name, not a speed");
    assert_eq!(machine.fan_enable().as_deref(), Some("0"));
    fan.call("setMode", json!({ "mode": "auto" })).expect("and so is auto");
    assert_eq!(machine.fan_enable().as_deref(), Some("2"));

    for refused in ["manual", "curve"] {
        let attempt = fan.call("setMode", json!({ "mode": refused, "pwm": 128 }));
        assert!(attempt.is_err(), "{refused} needs pwm1, which this machine does not have");
    }
    assert_eq!(
        machine.fan_enable().as_deref(),
        Some("2"),
        "a refused mode leaves the fans where they were"
    );
}

// ---------------------------------------------------------------------
// The boundaries between the three owners
// ---------------------------------------------------------------------
//
// The power module owns the package limits, the fan module owns `pwm1`,
// and the overclock module owns the GPU's offsets. Each of those
// sentences is a decision with a comment next to it in the source, and
// the failure they guard against is the same one: two owners on one
// piece of hardware, where the bug is not a wrong value but a value that
// depends on which module wrote last.
//
// **No test here applies an offset to a GPU.** The reference laptop has
// a real card and a consent already on file, so an `apply` in a test
// would drive it - which is the one thing the overclock module's own
// warning is about. What is exercised is the path that stops before any
// write, and the direction that can be checked safely: that the other
// two modules leave it alone.

/// The comment on `apply_profile` says it deliberately does not touch the
/// fans: a lower power limit makes them spin less because there is less
/// heat, which is the honest way to get there. This is that sentence as
/// an assertion.
#[test]
fn changing_the_power_mode_never_writes_to_the_fans() {
    let machine = Machine::new("boundary-fans");
    let power = machine.power();
    let fan = machine.fan();

    // Put the fans somewhere deliberate and remember exactly where.
    fan.call("setMode", json!({ "mode": "manual", "pwm": 120 })).expect("manual");
    let enable_before = machine.fan_enable();
    let pwm_before = machine.fan_pwm();

    tune(&power, PowerMode::Performance, json!({ "pl1W": 40.0, "turbo": false }));
    for mode in PowerMode::ALL {
        set_mode(&power, *mode);
        assert_eq!(machine.fan_enable(), enable_before, "{mode:?} moved pwm1_enable");
        assert_eq!(machine.fan_pwm(), pwm_before, "{mode:?} moved pwm1");
    }

    assert_eq!(
        fan.call("getStatus", Value::Null).expect("status")["mode"],
        json!("manual"),
        "and the fan module still thinks it is in charge"
    );
}

/// The other direction: the fan module owns heat, not watts.
#[test]
fn changing_the_fan_mode_never_writes_to_the_power_envelope() {
    let machine = Machine::new("boundary-envelope");
    let power = machine.power();
    let fan = machine.fan();

    tune(&power, PowerMode::Performance, json!({ "pl1W": 45.0 }));
    set_mode(&power, PowerMode::Performance);
    let envelope = machine.limits();
    let turbo = machine.turbo();

    for mode in ["max", "auto", "curve"] {
        if mode == "curve" {
            fan.call("setCurve", json!({ "curve": [{ "tempC": 0, "percent": 50 }, { "tempC": 110, "percent": 50 }] }))
                .expect("a curve");
        }
        fan.call("setMode", json!({ "mode": mode })).unwrap_or_else(|e| panic!("{mode}: {e:?}"));
        assert_eq!(machine.limits(), envelope, "fan {mode} changed the package limits");
        assert_eq!(machine.turbo(), turbo, "fan {mode} changed the turbo knob");
    }
}

/// The overclock module's own documentation lists "raise the CPU's power
/// limits" under what it will not do, because the power module already
/// owns those and re-applies them on every mode change. An overclock
/// request that never gets past the consent gate is the safe way to
/// assert it: nothing is written to any GPU, and nothing is written to
/// the CPU either.
#[test]
fn an_overclock_request_does_not_reach_the_cpus_power_limits() {
    let machine = Machine::new("boundary-overclock");
    let power = machine.power();
    let overclock = machine.overclock();

    set_mode(&power, PowerMode::Unlimited);
    let envelope = machine.limits();

    // No consent on this fixture's store, so this is refused before
    // anything is driven - see the crate's own tests for that gate.
    let refused = overclock.call("apply", json!({ "coreOffsetMhz": 150 }));
    assert!(refused.is_err(), "an unconsented apply must be refused");

    assert_eq!(machine.limits(), envelope, "and the CPU's envelope is not the GPU module's to move");
    assert!(machine.turbo());
    assert_eq!(
        machine.read("acpi/platform_profile").as_deref(),
        Some("performance"),
        "nor is the platform profile"
    );
}

/// Consent is a stored setting of one module, and the other two moving
/// the machine around must not disturb it. The case this guards against
/// is a shared config file or a store path collision - both of which
/// would show up here as a consent that quietly went away.
#[test]
fn cycling_the_power_modes_leaves_the_overclock_consent_alone() {
    let machine = Machine::new("boundary-consent");
    let power = machine.power();
    let fan = machine.fan();
    let overclock = machine.overclock();

    overclock.call("setConsent", json!({ "accepted": true })).expect("consent");
    let consented = |state: &Value| state["consent"]["accepted"] == json!(true);
    assert!(consented(&overclock.call("getState", Value::Null).expect("state")));

    for _ in 0..3 {
        for mode in PowerMode::ALL {
            set_mode(&power, *mode);
        }
        fan.call("setMode", json!({ "mode": "max" })).expect("max");
        fan.call("setMode", json!({ "mode": "auto" })).expect("auto");
    }

    assert!(
        consented(&overclock.call("getState", Value::Null).expect("state")),
        "twelve mode changes and six fan changes later, the consent is still on file"
    );
}

/// Each module keeps its own file. Asserted directly, because "they
/// happen to work today" and "they cannot collide" are different claims
/// and only the second one survives someone adding a fourth module.
#[test]
fn the_three_modules_write_three_separate_config_files() {
    let machine = Machine::new("boundary-config");
    let power = machine.power();
    let fan = machine.fan();
    let overclock = machine.overclock();

    tune(&power, PowerMode::Performance, json!({ "pl1W": 45.0 }));
    fan.call("setRestoreOnStart", json!({ "enabled": true })).expect("fan setting");
    overclock.call("setConsent", json!({ "accepted": true })).expect("overclock setting");

    let paths = [
        power.config_path(),
        machine.root.join("config/fan/fan.json"),
        machine.root.join("config/overclock/overclock.json"),
    ];
    for path in &paths {
        assert!(path.exists(), "{} should have been written", path.display());
    }
    // Three distinct files, not one being rewritten three times.
    let mut unique: Vec<_> = paths.iter().collect();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 3);
}
