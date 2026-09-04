//! pyren-ctl: talk to a running pyren-daemon from a shell.
//!
//! The app is the ordinary way to drive this; the CLI exists for the two
//! cases the app is bad at. One is scripting - a keybinding, a systemd
//! unit, a hook that drops the machine into Eco. The other is the reason it
//! was asked for: **putting a measured number in**. The daemon ships no
//! opinion about what Eco should be worth in watts on any given laptop
//! (see the `power` module's `Tuning::default_for`), so somebody has to
//! measure their own machine and say so, and
//!
//!     pyren-ctl power tune --mode eco --pl1 35 --turbo off
//!
//! is a better way to record that than a slider.
//!
//! Exit status: 0 success, 1 the daemon refused, 2 bad arguments, 3 the
//! daemon could not be reached.

mod args;

use std::process::ExitCode;

use pyren_core::client::{self, ClientError};
use pyren_core::ErrorKind;
use serde_json::{json, Value};

const HELP: &str = "\
pyren-ctl - control a running pyren-daemon

USAGE
  pyren-ctl [--json] <command>

MACHINE
  status                       one screen: what is on, and what this
                               machine can actually do
  info                         identity and the controls that were found

POWER
  power get                    current mode, mechanisms, envelope
  power set <mode>             eco | balanced | performance | unlimited
  power tune [--mode M] [--pl1 W] [--pl2 W] [--turbo on|off]
                               a mode's power envelope, in watts. Defaults
                               to the mode in force. Nothing may exceed
                               what the firmware shipped.
  power os-profile <on|off>    whether changing the mode also changes the
                               OS power profile (power-profiles-daemon),
                               or only the laptop's own firmware profile
  power auto <on|off> [--eco on|off] [--performance on|off]
              [--thermal on|off] [--temp-high C] [--temp-low C]
                               the two automatic systems: unplugging drops
                               to Balanced then refines towards Eco;
                               plugging in steps up to Performance.
                               --thermal is the third rule and outranks
                               both: a machine over --temp-high steps down
                               until it is back under --temp-low
  power restore-on-start <on|off>

FANS
  fan get                      speed, temperature, mode, capabilities
  fan set <auto|max|manual|curve> [--pwm 0-255]
  fan curve <t:pct,...> [--interpolation smooth|discrete] [--sensor cpu|gpu]
                               e.g. fan curve 40:20,60:50,85:100
                               --sensor gpu follows the graphics card and
                               falls back to the CPU while it is asleep
  fan restore-on-start <on|off>
  fan diagnose [--write]       the fan-control self-test
  fan calibrate [--seconds N]  run the fans at max and measure what full
                               speed is on this machine, then put back the
                               mode it found. Loud, and the only way to
                               give the curve's hysteresis a real ceiling
  fan cleaner                  what the dust-removal fan cleaner can do here
  fan clean [--speed 10-39] [--seconds 5-60] [--force]
                               spin the fans backwards to shake dust out.
                               Ends on its own; 'fan clean-stop' ends it
                               early. Loud, and the machine has no working
                               cooling while it runs
  fan clean-stop               end a cleaning cycle now

LIGHTS
  rgb probe                    which ways of talking to the lights this
                               machine answers, and where none do, why
  rgb get                      the probe plus what this daemon last set
  rgb read                     ask the firmware what the four zones are
  rgb set <colour>             all four zones, e.g. rgb set '#ff9900'
  rgb zones <c,c,c,c>          one colour per zone
  rgb off
  rgb brightness <0-100>       keeps the colours, changes the level
  rgb dialect                  which ways of talking to the lights
                               this machine answers
  rgb dialect <auto|id>        pin one by hand, e.g. rgb dialect fourZone
  rgb restore-on-start <on|off>
                               'set' and 'zones' also take --brightness 0-100

HOTKEY
  hotkey get                   which key is bound, and whether this daemon
                               can hear a key at all
  hotkey learn [--seconds N]   press the laptop's performance key when
                               asked; whatever arrives is bound to it.
                               Nothing is bound by default, because which
                               key a laptop sends is not something that
                               can be looked up
  hotkey <on|off>              act on the bound key, or hear and ignore it
  hotkey clear                 forget the bound key
  hotkey press                 do what the key does, without the key -
                               how the on-screen display is tested on a
                               machine whose Fn+P never reaches Linux

GPU
  gpu get                       which GPU is driving the screen (needs the
                               patched hp-wmi driver's gpu_mux_mode)
  gpu set <integrated|hybrid|discrete|optimus>
                               switch, and log out or reboot for it to
                               take effect - the firmware does not do that
                               itself
  network get                   the default-route interface, this daemon's
                               remembered mode, and the qdisc actually
                               active right now
  network set <off|auto>       off deletes the root qdisc; auto hands the
                               interface cake, or fq_codel on a kernel with
                               no sch_cake - system-wide, not per-app (see
                               dev/TODO.md §2.1 for why there is no
                               per-process priority here)
  oc get                       what can be tuned on each GPU, what is set,
                               and - where nothing can be - why not
  oc probe [--write]           ask the machine again. --write finds out
                               whether the offsets can actually be *set*,
                               by writing one back at the value it has
  oc consent <on|off>          accept the warning 'oc get' prints. Nothing
                               can be applied until it is, and turning it
                               off puts every card back to stock
  oc set [--gpu id] [--core MHz] [--memory MHz] [--lock min-max|off]
         [--hold SECS]         apply, in steps, and wait to be confirmed.
                               Unconfirmed changes are undone by the daemon
  oc confirm                   keep what was just applied
  oc cancel                    undo it now, without waiting for the timer
  oc reset [--gpu id]          back to the clocks the firmware shipped
  oc restore-on-start <on|off> re-apply the confirmed offsets at boot. Off
                               by default, and skipped after a boot that
                               followed an unconfirmed change

EVENTS
  events [--seconds N]         print what the daemon publishes as it
                               happens: key presses, mode changes

OPTIONS
  --json                       print the daemon's reply verbatim
  -h, --help
  -V, --version

The socket is $PYREN_SOCKET, or /tmp/pyren-daemon.sock. Reaching a
daemon running as root means being in the 'pyren' group.
";

// Exit codes a script can branch on, which is the point of the daemon
// answering with a `kind` rather than a sentence. "Refused" on its own
// tells a caller nothing about whether to fix the command, wait, or stop.
const USAGE_ERROR: u8 = 2;
const UNREACHABLE: u8 = 3;
/// This machine will not do it however it is asked. Stop retrying.
const NOT_CAPABLE: u8 = 4;
/// The daemon was reached and is running unprivileged.
const NEEDS_ROOT: u8 = 5;
/// Something else has the hardware. Asking again later is the fix.
const BUSY: u8 = 6;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if argv.iter().any(|a| a == "-V" || a == "--version") {
        println!("pyren-ctl {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let command = match args::parse(&argv) {
        Ok(command) => command,
        Err(e) => return fail(&e, USAGE_ERROR),
    };

    match run(&command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(message)) => fail(&message, USAGE_ERROR),
        Err(Failure::Client(e)) => {
            eprintln!("pyren-ctl: {e}");
            if e.is_permission_denied() {
                eprintln!(
                    "  the daemon is running as root and this user is not in its group.\n\
                     \x20 sudo usermod -aG pyren $USER, then log out and back in\n\
                     \x20 (or run this once through: newgrp pyren)"
                );
            }
            if e.needs_root() {
                // The opposite problem to the one above, and it is worth
                // saying which: there the caller lacks a group, here the
                // daemon lacks root, and swapping the two fixes wastes an
                // evening.
                eprintln!(
                    "  the daemon was reached but is running unprivileged, so it\n\
                     \x20 cannot write to the hardware. Restart it as root:\n\
                     \x20 sudo -E cargo run -p pyren-daemon"
                );
            }
            ExitCode::from(exit_code(&e))
        }
    }
}

/// Turns a refusal into something a shell script can act on.
fn exit_code(e: &ClientError) -> u8 {
    let Some(kind) = e.kind() else {
        // Either not a refusal at all, or a kind this build does not know.
        // Both mean "the daemon is not where the answer is".
        return match e {
            ClientError::Daemon { .. } => 1,
            _ => UNREACHABLE,
        };
    };
    match kind {
        // The daemon rejecting the arguments and this binary rejecting
        // them are the same mistake, so they get the same code.
        ErrorKind::InvalidParams | ErrorKind::UnknownMethod | ErrorKind::UnknownModule => {
            USAGE_ERROR
        }
        ErrorKind::NotCapable | ErrorKind::Unsupported => NOT_CAPABLE,
        ErrorKind::PermissionDenied => NEEDS_ROOT,
        ErrorKind::Busy => BUSY,
        _ => 1,
    }
}

fn fail(message: &str, code: u8) -> ExitCode {
    eprintln!("pyren-ctl: {message}\n\nTry 'pyren-ctl --help'.");
    ExitCode::from(code)
}

enum Failure {
    Usage(String),
    Client(ClientError),
}

impl From<ClientError> for Failure {
    fn from(e: ClientError) -> Self {
        Self::Client(e)
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Usage(message)
    }
}

type Run = Result<(), Failure>;

fn run(command: &args::Command) -> Run {
    let path: Vec<&str> = command.path.iter().map(String::as_str).collect();

    match path.as_slice() {
        ["status"] => status(command),
        ["info"] => show(command, client::call("system", "getInfo", Value::Null)?, print_info),

        ["power", "get"] => show(command, power_state()?, print_power),
        ["power", "set", mode] => {
            let reply = client::call("power", "setMode", json!({ "mode": mode }))?;
            if command.json {
                print_json(&reply);
            } else {
                print_apply(mode, &reply);
            }
            Ok(())
        }
        ["power", "tune"] => power_tune(command),
        ["power", "os-profile", value] => {
            let enabled = word_switch("os-profile", value)?;
            show(command, client::call("power", "setApplyToOsProfile", json!({ "enabled": enabled }))?, print_power)
        }
        ["power", "auto", value] => power_auto(command, value),
        ["power", "restore-on-start", value] => {
            let enabled = word_switch("restore-on-start", value)?;
            show(command, client::call("power", "setRestoreOnStart", json!({ "enabled": enabled }))?, print_power)
        }

        ["fan", "get"] => show(command, client::call("fan", "getStatus", Value::Null)?, print_fan),
        ["fan", "set", mode] => {
            let pwm = command.number("pwm")?;
            let params = match pwm {
                Some(pwm) => json!({ "mode": mode, "pwm": pwm.round().clamp(0.0, 255.0) as u64 }),
                None => json!({ "mode": mode }),
            };
            show(command, client::call("fan", "setMode", params)?, print_fan)
        }
        ["fan", "curve", spec] => {
            let points: Vec<Value> = args::parse_curve(spec)?
                .into_iter()
                .map(|(temp_c, percent)| json!({ "tempC": temp_c, "percent": percent }))
                .collect();
            let mut params = json!({ "curve": points });
            if let Some(interpolation) = command.option("interpolation") {
                params["interpolation"] = json!(interpolation);
            }
            if let Some(sensor) = command.option("sensor") {
                params["referenceSensor"] = json!(sensor);
            }
            show(command, client::call("fan", "setCurve", params)?, print_fan)
        }
        ["fan", "restore-on-start", value] => {
            let enabled = word_switch("restore-on-start", value)?;
            show(command, client::call("fan", "setRestoreOnStart", json!({ "enabled": enabled }))?, print_fan)
        }
        ["fan", "calibrate"] => {
            let seconds = command.number("seconds")?;
            let params = match seconds {
                Some(seconds) => json!({ "seconds": seconds.round().max(0.0) as u64 }),
                None => Value::Null,
            };
            show(command, client::call("fan", "calibrate", params)?, print_calibration)
        }
        ["fan", "cleaner"] => show(
            command,
            client::call("fan", "cleanerStatus", json!({ "refresh": true }))?,
            print_cleaner,
        ),
        ["fan", "clean"] => {
            let mut params = serde_json::Map::new();
            if let Some(speed) = command.number("speed")? {
                params.insert("speed".into(), json!(speed.round().max(0.0) as u64));
            }
            if let Some(seconds) = command.number("seconds")? {
                params.insert("seconds".into(), json!(seconds.round().max(0.0) as u64));
            }
            if command.options.contains_key("force") {
                params.insert("force".into(), json!(true));
            }
            show(command, client::call("fan", "startCleaning", Value::Object(params))?, print_cleaner)
        }
        ["fan", "clean-stop"] => show(
            command,
            client::call("fan", "stopCleaning", Value::Null)?,
            print_cleaner,
        ),
        ["fan", "diagnose"] => {
            let allow_writes = command.options.contains_key("write");
            show(
                command,
                client::call("fan", "diagnose", json!({ "allowWrites": allow_writes }))?,
                print_diagnosis,
            )
        }

        ["rgb", "probe"] => {
            show(command, client::call("rgb", "getCapabilities", Value::Null)?, print_rgb_probe)
        }
        ["rgb", "get"] => show(command, client::call("rgb", "getStatus", Value::Null)?, print_rgb),
        ["rgb", "read"] => show(command, client::call("rgb", "readZones", Value::Null)?, print_zones),
        ["rgb", "off"] => show(command, client::call("rgb", "off", Value::Null)?, print_rgb),
        ["rgb", "set", colour] => {
            let mut params = json!({ "color": colour });
            add_brightness(command, &mut params)?;
            show(command, client::call("rgb", "setStatic", params)?, print_rgb)
        }
        ["rgb", "zones", spec] => {
            let zones: Vec<&str> = spec.split(',').map(str::trim).filter(|z| !z.is_empty()).collect();
            if zones.is_empty() {
                return Err(Failure::Usage(
                    "rgb zones takes up to four comma-separated colours, e.g. \
                     '#f00,#0f0,#00f,#ff0'"
                        .into(),
                ));
            }
            let mut params = json!({ "zones": zones });
            add_brightness(command, &mut params)?;
            show(command, client::call("rgb", "setZones", params)?, print_rgb)
        }
        // Without this there is no way back from `rgb off`, which leaves
        // the brightness at 0 - and every later `rgb set` then writes a
        // colour that is scaled to black.
        //
        // Two calls rather than a `setBrightness` method: brightness is
        // not a thing the daemon holds apart from the colours, and adding
        // a method that only re-sends them would be a third way to say
        // what `setZones` already says.
        ["rgb", "brightness", value] => {
            let percent: i64 = value.trim_end_matches('%').parse().map_err(|_| {
                Failure::Usage(format!("rgb brightness takes 0-100, not '{value}'"))
            })?;
            let status = client::call("rgb", "getStatus", Value::Null)?;
            let zones = status.get("zones").cloned().unwrap_or(Value::Null);
            show(
                command,
                client::call("rgb", "setZones", json!({ "zones": zones, "brightness": percent }))?,
                print_rgb,
            )
        }
        ["rgb", "dialect"] => {
            show(command, client::call("rgb", "getCapabilities", Value::Null)?, print_rgb_probe)
        }
        ["rgb", "dialect", id] => show(
            command,
            client::call("rgb", "setDialect", json!({ "dialect": id }))?,
            print_rgb,
        ),
        ["rgb", "restore-on-start", value] => {
            let enabled = word_switch("restore-on-start", value)?;
            show(
                command,
                client::call("rgb", "setRestoreOnStart", json!({ "enabled": enabled }))?,
                print_rgb,
            )
        }

        ["gpu", "get"] => show(command, client::call("gpu", "getStatus", Value::Null)?, print_gpu),
        ["gpu", "set", mode] => {
            show(command, client::call("gpu", "setMode", json!({ "mode": mode }))?, print_gpu)
        }

        ["network", "get"] => {
            show(command, client::call("network", "getStatus", Value::Null)?, print_network)
        }
        ["network", "set", mode] => {
            show(command, client::call("network", "setMode", json!({ "mode": mode }))?, print_network)
        }

        ["hotkey", "get"] => {
            show(command, client::call("hotkey", "getStatus", Value::Null)?, print_hotkey)
        }
        ["hotkey", "learn"] => hotkey_learn(command),
        ["hotkey", "clear"] => show(
            command,
            client::call("hotkey", "setTriggers", json!({ "triggers": [] }))?,
            print_hotkey,
        ),
        ["hotkey", "press"] => {
            let reply = client::call("hotkey", "press", Value::Null)?;
            if command.json {
                print_json(&reply);
            } else {
                println!("pressed");
            }
            Ok(())
        }
        // Last of the hotkey arms: everything else here is a subcommand,
        // and this one is a value.
        ["hotkey", value] => {
            let enabled = word_switch("hotkey", value)?;
            show(
                command,
                client::call("hotkey", "setEnabled", json!({ "enabled": enabled }))?,
                print_hotkey,
            )
        }

        ["oc", "get"] => show(command, client::call("overclock", "getState", Value::Null)?, print_oc),
        ["oc", "probe"] => {
            let allow_writes = command.options.contains_key("write");
            show(
                command,
                client::call("overclock", "probe", json!({ "allowWrites": allow_writes }))?,
                print_oc,
            )
        }
        ["oc", "consent", value] => {
            let accepted = word_switch("consent", value)?;
            show(
                command,
                client::call("overclock", "setConsent", json!({ "accepted": accepted }))?,
                print_oc,
            )
        }
        ["oc", "set"] => oc_set(command),
        ["oc", "confirm"] => {
            show(command, client::call("overclock", "confirm", Value::Null)?, print_oc)
        }
        ["oc", "cancel"] => {
            show(command, client::call("overclock", "cancel", Value::Null)?, print_oc)
        }
        ["oc", "reset"] => {
            let params = match command.option("gpu") {
                Some(gpu) => json!({ "gpu": gpu }),
                None => Value::Null,
            };
            show(command, client::call("overclock", "reset", params)?, print_oc)
        }
        ["oc", "restore-on-start", value] => {
            let enabled = word_switch("restore-on-start", value)?;
            show(
                command,
                client::call("overclock", "setRestoreOnStart", json!({ "enabled": enabled }))?,
                print_oc,
            )
        }

        ["events"] => events(command),

        [] => Err(Failure::Usage("no command given".into())),
        other => Err(Failure::Usage(format!("unknown command '{}'", other.join(" ")))),
    }
}

/// Prints a reply either verbatim or through a human-readable printer.
fn show(command: &args::Command, reply: Value, printer: fn(&Value)) -> Run {
    if command.json {
        print_json(&reply);
    } else {
        printer(&reply);
    }
    Ok(())
}

fn print_json(value: &Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

fn power_state() -> Result<Value, ClientError> {
    client::call("power", "getState", Value::Null)
}

fn power_tune(command: &args::Command) -> Run {
    let mut params = serde_json::Map::new();
    if let Some(mode) = command.option("mode") {
        params.insert("mode".into(), json!(mode));
    }
    if let Some(watts) = command.number("pl1")? {
        params.insert("pl1W".into(), json!(watts));
    }
    if let Some(watts) = command.number("pl2")? {
        params.insert("pl2W".into(), json!(watts));
    }
    if let Some(turbo) = command.switch("turbo")? {
        params.insert("turbo".into(), json!(turbo));
    }
    if params.is_empty() || (params.len() == 1 && params.contains_key("mode")) {
        return Err(Failure::Usage(
            "nothing to tune: pass --pl1, --pl2 or --turbo".to_string(),
        ));
    }

    show(command, client::call("power", "setTuning", Value::Object(params))?, print_power)
}

fn power_auto(command: &args::Command, value: &str) -> Run {
    let enabled = word_switch("auto", value)?;
    // Read the current config first: setAutoConfig takes the whole object,
    // and clobbering thresholds someone tuned would be a nasty surprise.
    let state = power_state()?;
    let mut config = state.get("auto").cloned().unwrap_or_else(|| json!({}));
    config["enabled"] = json!(enabled);
    if let Some(eco) = command.switch("eco")? {
        config["ecoOnBattery"] = json!(eco);
    }
    if let Some(performance) = command.switch("performance")? {
        config["performanceOnLoad"] = json!(performance);
    }
    if let Some(hot) = command.switch("thermal")? {
        config["backOffWhenHot"] = json!(hot);
    }
    if let Some(high) = command.number("temp-high")? {
        config["tempHighC"] = json!(high);
    }
    if let Some(low) = command.number("temp-low")? {
        config["tempLowC"] = json!(low);
    }
    // Two thresholds that have crossed would latch on and never let go.
    let high = config.get("tempHighC").and_then(Value::as_f64).unwrap_or(f64::MAX);
    let low = config.get("tempLowC").and_then(Value::as_f64).unwrap_or(f64::MIN);
    if low >= high {
        return Err(Failure::Usage(format!(
            "--temp-low ({low}) has to be below --temp-high ({high}): they are the two \
             ends of a dead band, and a machine that crossed them would never cool down again"
        )));
    }

    client::call("power", "setAutoConfig", config)?;
    show(command, power_state()?, print_power)
}

fn add_brightness(command: &args::Command, params: &mut Value) -> Result<(), Failure> {
    if let Some(percent) = command.number("brightness")? {
        if !(0.0..=100.0).contains(&percent) {
            return Err(Failure::Usage(format!(
                "--brightness is a percentage between 0 and 100, not {percent}"
            )));
        }
        params["brightness"] = json!(percent.round() as i64);
    }
    Ok(())
}

fn word_switch(name: &str, value: &str) -> Result<bool, String> {
    match value {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        other => Err(format!("{name} takes on or off, not '{other}'")),
    }
}

// --- printing ---------------------------------------------------------

/// Every line in the human-readable output is a label and a value, so
/// they line up whatever the command was.
/// `hotkey learn`: hold the socket open while the user presses their key.
///
/// The prompt is printed before the call, and flushed, because the daemon
/// answers nothing at all until a key arrives - a silent terminal for ten
/// seconds is indistinguishable from a hang.
fn hotkey_learn(command: &args::Command) -> Run {
    let seconds = command.number("seconds")?.unwrap_or(10.0).clamp(1.0, 30.0);

    if !command.json {
        println!("Press the key you want on the laptop - Fn+P on an OMEN.");
        println!("Waiting {seconds:.0}s...");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    let reply = client::call(
        "hotkey",
        "learn",
        json!({ "timeoutMs": (seconds * 1000.0) as u64, "bind": true }),
    )?;

    if command.json {
        print_json(&reply);
        return Ok(());
    }

    if reply.get("timedOut").and_then(Value::as_bool).unwrap_or(false) {
        // Not an error, and the difference matters: a key that never
        // arrives is what a laptop whose Fn+P is handled entirely inside
        // the embedded controller looks like from here.
        println!("No key arrived. Either none was pressed, or this laptop keeps");
        println!("that key to itself and never tells Linux about it.");
        return Ok(());
    }

    let press = reply.get("press").cloned().unwrap_or(Value::Null);
    row("device", text(&press, "device"));
    row("key", text(&press, "describe"));
    row("bound", yes_no(reply.get("bound")));
    // Whatever was pressed is now bound, and the device name above is how
    // somebody notices they bound the wrong thing. Say the undo out loud
    // rather than leaving it in --help: this once caught a touchpad, and
    // every touch then cycled the power mode.
    println!("\nNot the key you meant? 'pyren-ctl hotkey clear' unbinds it.");
    Ok(())
}

/// `events`: the long poll, printed as it arrives.
///
/// Runs until interrupted, or until `--seconds` have passed with nothing
/// left to wait for - which is what makes it usable in a script that wants
/// to see whether a key press produces anything at all.
fn events(command: &args::Command) -> Run {
    let deadline = command
        .number("seconds")?
        .map(|s| std::time::Instant::now() + std::time::Duration::from_secs_f64(s.max(0.0)));
    let mut since: Option<u64> = None;

    loop {
        let wait = match deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return Ok(());
                }
                left.as_millis().min(25_000) as u64
            }
            None => 25_000,
        };

        let mut params = json!({ "timeoutMs": wait });
        if let Some(since) = since {
            params["since"] = json!(since);
        }
        let reply = client::call("core", "nextEvent", params)?;
        since = reply.get("seq").and_then(Value::as_u64).or(since);

        if command.json {
            // One JSON object per event, so this pipes into jq line by
            // line rather than producing one document at the end.
            for event in reply.get("events").and_then(Value::as_array).into_iter().flatten() {
                println!("{}", serde_json::to_string(event).unwrap_or_default());
            }
        } else {
            for event in reply.get("events").and_then(Value::as_array).into_iter().flatten() {
                println!(
                    "{:<16} {}",
                    text(event, "topic"),
                    serde_json::to_string(event.get("payload").unwrap_or(&Value::Null))
                        .unwrap_or_default()
                );
            }
        }

        let missed = reply.get("missed").and_then(Value::as_u64).unwrap_or(0);
        if missed > 0 {
            eprintln!("pyren-ctl: {missed} events were dropped before this caught up");
        }
    }
}

fn print_hotkey(status: &Value) {
    // The first line is the one that answers "why does my key do nothing":
    // not bound, not heard, or switched off - each with its own fix.
    row("state", text(status, "detail"));

    let triggers = status.get("triggers").and_then(Value::as_array).cloned().unwrap_or_default();
    match triggers.first() {
        None => row("key", "none bound (pyren-ctl hotkey learn)"),
        Some(trigger) => {
            let keycode = trigger.get("keycode").and_then(Value::as_u64);
            let scancode = trigger.get("scancode").and_then(Value::as_u64);
            let key = match (keycode, scancode) {
                (Some(key), Some(scan)) => format!("keycode {key}, scancode 0x{scan:x}"),
                (Some(key), None) => format!("keycode {key}"),
                (None, Some(scan)) => format!("scancode 0x{scan:x} (no keycode assigned)"),
                (None, None) => "-".to_string(),
            };
            // The shortcut as somebody would write it down, and the raw
            // numbers underneath: the name is what a person recognises,
            // and the numbers are what a bug report needs.
            if let Some(label) = status.get("label").and_then(Value::as_str) {
                row("shortcut", label);
            }
            row("key", key);
            row("on", text(trigger, "device"));
        }
    }

    row("enabled", yes_no(status.get("enabled")));
    row("heard", yes_no(status.get("watching")));
    if let Some(fired) = status.get("fired").and_then(Value::as_u64) {
        row("presses", fired);
    }
    if let Some(error) = status.get("configSaveError").and_then(Value::as_str) {
        row("not saved", error);
    }
}

fn row(label: &str, value: impl std::fmt::Display) {
    println!("  {label:<10} {value}");
}

fn text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(s)) => s.clone(),
        // A translatable `Msg` (`{ key, params, text }`) - the CLI shows the
        // English `text` it always carries.
        Some(Value::Object(o)) if o.contains_key("text") => {
            o.get("text").and_then(Value::as_str).unwrap_or("-").to_string()
        }
        Some(Value::Null) | None => "-".to_string(),
        Some(other) => other.to_string(),
    }
}

/// The English text of a `Msg` field, or `None` when the field is absent
/// or null. For the optional lines (`note`, `error`) that used to be
/// `Option<String>`.
fn msg_line(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => v.get("text").and_then(Value::as_str).map(str::to_string),
    }
}

fn watts(value: Option<&Value>) -> String {
    match value.and_then(Value::as_f64) {
        Some(uw) => format!("{:.0}W", uw / 1_000_000.0),
        None => "-".to_string(),
    }
}

fn print_info(info: &Value) {
    row(
        "machine",
        format!(
            "{} {} (board {})",
            text(info, "vendor"),
            text(info, "model"),
            text(info, "boardName")
        ),
    );
    row("kernel", text(info, "kernel"));
    row("verdict", format!("{} - {}", text(info, "compatibility"), text(info, "reason")));
    if let Some(controls) = info.get("controls") {
        row(
            "controls",
            format!(
                "fan mode {} | fan speed {} | power modes {}",
                yes_no(controls.get("fanMode")),
                yes_no(controls.get("fanSpeed")),
                yes_no(controls.get("powerMode")),
            ),
        );
    }
}

fn yes_no(value: Option<&Value>) -> &'static str {
    match value.and_then(Value::as_bool) {
        Some(true) => "yes",
        Some(false) => "no",
        None => "?",
    }
}

fn print_power(state: &Value) {
    row("mode", text(state, "mode"));
    row(
        "os profile",
        match state.get("applyToOsProfile").and_then(Value::as_bool) {
            Some(true) => "changed along with the mode",
            Some(false) => "left alone - only the laptop's own profile changes",
            None => "?",
        },
    );

    if let Some(backend) = state.get("backend") {
        let available = backend
            .get("available")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
            })
            .unwrap_or_default();
        row(
            "mechanisms",
            if available.is_empty() {
                "none - this machine offers no power-mode control".to_string()
            } else {
                available
            },
        );
    }

    if let Some(limits) = state.get("limits") {
        if limits.get("available").and_then(Value::as_bool) == Some(true) {
            let stock = limits.get("stock");
            let current = limits.get("current");
            row(
                "envelope",
                format!(
                    "PL1 {} / PL2 {}   (stock {} / {})",
                    watts(current.and_then(|c| c.get("pl1Uw"))),
                    watts(current.and_then(|c| c.get("pl2Uw"))),
                    watts(stock.and_then(|c| c.get("pl1Uw"))),
                    watts(stock.and_then(|c| c.get("pl2Uw"))),
                ),
            );
            if let Some(turbo) = limits.get("turbo").and_then(Value::as_bool) {
                row("turbo", if turbo { "on" } else { "off" });
            }
            print_tuning(limits.get("tuning"));
        } else {
            row("envelope", "not exposed by this machine");
        }
    }

    if let Some(auto) = state.get("auto") {
        let on = auto.get("enabled").and_then(Value::as_bool) == Some(true);
        row(
            "auto",
            if !on {
                "off".to_string()
            } else {
                format!(
                    "on - eco system {}, performance system {}",
                    yes_no(auto.get("ecoOnBattery")),
                    yes_no(auto.get("performanceOnLoad"))
                )
            },
        );
    }
    if let Some(thermal) = state.get("thermal") {
        row(
            "temp",
            match (
                thermal.get("available").and_then(Value::as_bool),
                thermal.get("tempC").and_then(Value::as_f64),
            ) {
                (Some(true), Some(temp)) => format!(
                    "{temp:.0} C{}",
                    if thermal.get("hot").and_then(Value::as_bool) == Some(true) {
                        " - running hot, the supervisor is holding it down"
                    } else {
                        ""
                    }
                ),
                (Some(true), None) => "a sensor was found and read nothing".to_string(),
                _ => "no CPU or GPU sensor on this machine".to_string(),
            },
        );
    }
    if let Some(last) = msg_line(state, "lastAutoSwitch") {
        row("last auto", last);
    }
}

/// Only prints modes someone has actually tuned: the defaults are "leave
/// the machine alone", and a table of four identical 100 % rows would be
/// noise pretending to be information.
fn print_tuning(tuning: Option<&Value>) {
    let Some(Value::Object(modes)) = tuning else { return };
    let mut tuned = Vec::new();
    for (mode, values) in modes {
        let pl1 = values.get("pl1Percent").and_then(Value::as_u64).unwrap_or(100);
        let pl2 = values.get("pl2Percent").and_then(Value::as_u64).unwrap_or(100);
        let turbo = values.get("turbo").and_then(Value::as_bool).unwrap_or(true);
        if pl1 != 100 || pl2 != 100 || !turbo {
            tuned.push(format!(
                "{mode} {pl1}%/{pl2}%{}",
                if turbo { "" } else { " no turbo" }
            ));
        }
    }
    if tuned.is_empty() {
        row("tuning", "none - every mode leaves the envelope where the firmware set it");
    } else {
        row("tuning", tuned.join(", "));
    }
}

fn print_apply(mode: &str, report: &Value) {
    let list = |key: &str| {
        report
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default()
    };

    let applied = list("applied");
    if applied.is_empty() {
        println!("  {mode}: nothing was applied");
    } else {
        println!("  {mode}: {}", applied.join(", "));
    }
    for failure in list("failed") {
        println!("  ! {failure}");
    }
}

fn print_fan(status: &Value) {
    row("mode", text(status, "mode"));
    row(
        "reading",
        format!(
            "{} rpm, cpu {} C, gpu {} C{}",
            status.get("fanRpm").and_then(Value::as_i64).unwrap_or(0),
            status
                .get("cpuTempC")
                .and_then(Value::as_i64)
                .map(|t| t.to_string())
                .unwrap_or("-".into()),
            status
                .get("gpuTempC")
                .and_then(Value::as_i64)
                .map(|t| t.to_string())
                .unwrap_or("-".into()),
            if status.get("isReverse").and_then(Value::as_bool) == Some(true) {
                " (reverse)"
            } else {
                ""
            }
        ),
    );

    if let Some(caps) = status.get("capabilities") {
        let speed = caps.get("setSpeed").and_then(Value::as_bool) == Some(true);
        row(
            "can do",
            if caps.get("switchMode").and_then(Value::as_bool) != Some(true) {
                "nothing - no fan control interface on this machine".to_string()
            } else if speed {
                "auto, max, manual, curve".to_string()
            } else {
                "auto and max only - this driver exposes no pwm1, so a speed \
                 cannot be commanded"
                    .to_string()
            },
        );
    }

    if let Some(points) = status.get("curve").and_then(Value::as_array) {
        if !points.is_empty() {
            let drawn: Vec<String> = points
                .iter()
                .map(|p| {
                    format!(
                        "{}:{}",
                        p.get("tempC").and_then(Value::as_f64).unwrap_or(0.0),
                        p.get("percent").and_then(Value::as_f64).unwrap_or(0.0)
                    )
                })
                .collect();
            row("curve", drawn.join(","));
        }
    }

    // The setting and what is being read are two lines' worth of one fact
    // only when they disagree - which is exactly when it matters, because
    // the card being asleep is why the curve is on the other sensor.
    if let Some(sensor) = status.get("referenceSensor").and_then(Value::as_str) {
        let in_use = status.get("referenceSensorInUse").and_then(Value::as_str);
        row(
            "curve from",
            match in_use {
                Some(in_use) if in_use != sensor => {
                    format!("{sensor} (reading {in_use} - the {sensor} has nothing to report)")
                }
                _ => sensor.to_string(),
            },
        );
    }

    // Only worth a line once it exists: an absent ceiling is the normal
    // state, not a missing setting, and `fan calibrate` is what fills it.
    row(
        "full speed",
        match status.get("fanMaxRpm").and_then(Value::as_i64) {
            Some(rpm) => format!("{rpm} rpm, measured"),
            None => "not calibrated - run 'fan calibrate'".to_string(),
        },
    );
    if let Some(error) = msg_line(status, "error") {
        println!("  ! {error}");
    }
}

/// The fan cleaner, in the two states worth telling apart at a terminal:
/// a cycle running with a countdown, and a machine being told what it can
/// or cannot do.
fn print_cleaner(status: &Value) {
    let flag = |key: &str| status.get(key).and_then(Value::as_bool).unwrap_or(false);

    if flag("running") {
        let left = status.get("secondsRemaining").and_then(Value::as_u64).unwrap_or(0);
        let total = status.get("secondsTotal").and_then(Value::as_u64).unwrap_or(0);
        row("cleaning", format!("{left}s left of {total}s - 'fan clean-stop' ends it now"));
    } else if flag("transitioning") {
        row("cleaning", "in progress - the fans are being braked or ramped back");
    } else {
        row("cleaning", "not running");
    }

    row(
        "cleaner",
        match (flag("supported"), status.get("generation").and_then(Value::as_str)) {
            (true, Some(generation)) => format!("available ({generation})"),
            (true, None) => "available".to_string(),
            (false, _) => "not available".to_string(),
        },
    );
    println!("  {}", text(status, "detail"));

    // The guard people hit, and the reading it is compared against, on one
    // line - a refusal that names neither is a refusal nobody can act on.
    if let Some(temp) = status.get("cpuTempC").and_then(Value::as_i64) {
        let limit = status.get("maxStartTempC").and_then(Value::as_i64).unwrap_or(0);
        row("cpu", format!("{temp} °C (a cycle will not start above {limit} °C)"));
    }
    row(
        "settings",
        format!(
            "{}s at {}",
            status.get("durationSecs").and_then(Value::as_u64).unwrap_or(0),
            match status.get("configuredSpeed").and_then(Value::as_u64) {
                Some(speed) => format!("{}00 rpm", speed),
                None => "the firmware's own speed".to_string(),
            }
        ),
    );
    // The hardware's own answer, which is the one that does not depend on
    // this daemon having been the one to start anything.
    if flag("fansReversed") {
        row("fans", "spinning in reverse right now");
    }
    if let Some(error) = msg_line(status, "error") {
        println!("  ! {error}");
    }
}

fn print_calibration(result: &Value) {
    row("verdict", text(result, "verdict"));
    println!("  {}", text(result, "detail"));

    let rpm = |key: &str| match result.get(key).and_then(Value::as_i64) {
        Some(rpm) => format!("{rpm} rpm"),
        None => "-".to_string(),
    };
    row("stored", format!("fan1 {}, fan2 {}", rpm("fan1MaxRpm"), rpm("fan2MaxRpm")));
    row(
        "run",
        format!(
            "{}s from {} rpm, restored to {}",
            result.get("seconds").and_then(Value::as_u64).unwrap_or(0),
            result.get("baselineRpm").and_then(Value::as_i64).unwrap_or(0),
            text(result, "restoredMode"),
        ),
    );
    if let Some(error) = result.get("restoreError").and_then(Value::as_str) {
        println!("  ! could not put the fans back: {error}");
    }

    // The trace is the evidence for the verdict, so it is worth seeing
    // when a run concludes something surprising.
    for sample in result.get("samples").and_then(Value::as_array).unwrap_or(&Vec::new()) {
        println!(
            "  +{:>3}s      fan1 {:>5}  fan2 {:>5}{}",
            sample.get("atSecs").and_then(Value::as_u64).unwrap_or(0),
            sample.get("fan1Rpm").and_then(Value::as_i64).unwrap_or(0),
            sample.get("fan2Rpm").and_then(Value::as_i64).unwrap_or(0),
            if sample.get("isReverse").and_then(Value::as_bool) == Some(true) {
                "  (reverse)"
            } else {
                ""
            }
        );
    }
}

fn print_diagnosis(diagnosis: &Value) {
    row("verdict", text(diagnosis, "verdict"));
    println!("  {}", text(diagnosis, "summary"));
    for check in diagnosis.get("checks").and_then(Value::as_array).unwrap_or(&Vec::new()) {
        println!(
            "  [{:^6}] {:28} {}",
            text(check, "status"),
            // "title", not "label": the field has never been called that,
            // so every check printed its name as "-".
            text(check, "title"),
            text(check, "detail")
        );
    }
    if let Some(notice) = msg_line(diagnosis, "driverNotice") {
        println!("\n  {notice}");
    }
}

/// `oc set`, which is the one command here that changes a clock.
///
/// Every option is optional and what is left out is left alone, so
/// `oc set --core 90` means "and don't touch the memory" rather than "and
/// put the memory back to stock" - the same rule the daemon applies to the
/// request it receives.
fn oc_set(command: &args::Command) -> Run {
    let mut params = serde_json::Map::new();
    if let Some(gpu) = command.option("gpu") {
        params.insert("gpu".into(), json!(gpu));
    }
    if let Some(core) = command.number("core")? {
        params.insert("coreOffsetMhz".into(), json!(core.round() as i64));
    }
    if let Some(memory) = command.number("memory")? {
        params.insert("memOffsetMhz".into(), json!(memory.round() as i64));
    }
    if let Some(hold) = command.number("hold")? {
        params.insert("holdSecs".into(), json!(hold.round().max(0.0) as u64));
    }
    if let Some(lock) = command.option("lock") {
        params.insert("clockLock".into(), parse_clock_lock(lock)?);
    }
    if params.is_empty() {
        return Err(Failure::Usage(
            "oc set needs at least one of --core, --memory or --lock".into(),
        ));
    }

    let reply = client::call("overclock", "apply", Value::Object(params))?;
    if command.json {
        print_json(&reply);
        return Ok(());
    }
    print_oc(&reply);
    // The daemon has already armed its timer; saying so is the difference
    // between a CLI that applied an overclock and one that applied an
    // overclock the user has 20 seconds to keep.
    if let Some(pending) = reply.get("pending").filter(|p| !p.is_null()) {
        println!(
            "  run 'pyren-ctl oc confirm' within {:.0}s to keep this; \
             otherwise the daemon undoes it",
            pending.get("secondsLeft").and_then(Value::as_f64).unwrap_or(0.0)
        );
    }
    Ok(())
}

/// `--lock 2000-2500`, or `--lock off` to hand the clocks back to the
/// driver. A JSON `null` is what "off" is on the wire, and it is a
/// different request from not passing `--lock` at all.
fn parse_clock_lock(spec: &str) -> Result<Value, Failure> {
    if matches!(spec, "off" | "none" | "auto") {
        return Ok(Value::Null);
    }
    let (min, max) = spec
        .split_once('-')
        .ok_or_else(|| Failure::Usage(format!("--lock takes MIN-MAX or off, not '{spec}'")))?;
    let parse = |value: &str, end: &str| {
        value
            .trim()
            .parse::<i64>()
            .map_err(|_| Failure::Usage(format!("--lock's {end} is not a number: '{value}'")))
    };
    Ok(json!({ "minMhz": parse(min, "minimum")?, "maxMhz": parse(max, "maximum")? }))
}

fn print_oc(state: &Value) {
    let empty = Vec::new();
    let gpus = state.get("gpus").and_then(Value::as_array).unwrap_or(&empty);
    if gpus.is_empty() {
        row("gpus", "none found");
    }
    for gpu in gpus {
        // The name first, then the sentence that says what can be done to
        // it, because on a hybrid laptop the two cards answer differently
        // and a single verdict for the machine would be wrong about one.
        row("gpu", format!("{} ({})", text(gpu, "name"), text(gpu, "id")));
        row("", text(gpu, "detail"));
        let confirmed = gpu.get("confirmed").cloned().unwrap_or(Value::Null);
        let core = confirmed.get("coreOffsetMhz").and_then(Value::as_i64).unwrap_or(0);
        let memory = confirmed.get("memOffsetMhz").and_then(Value::as_i64).unwrap_or(0);
        row("", format!("kept: core {core:+} MHz, memory {memory:+} MHz"));
    }

    let consent = state.get("consent").cloned().unwrap_or(Value::Null);
    row("consent", yes_no(consent.get("accepted")));
    if consent.get("accepted").and_then(Value::as_bool) != Some(true) {
        println!("  {}", text(&consent, "text"));
    }
    row("restore", yes_no(state.get("restoreOnStart")));

    if let Some(pending) = state.get("pending").filter(|p| !p.is_null()) {
        row(
            "pending",
            format!(
                "{} - undone in {:.0}s unless confirmed",
                text(pending, "gpu"),
                pending.get("secondsLeft").and_then(Value::as_f64).unwrap_or(0.0)
            ),
        );
    }
    if let Some(note) = msg_line(state, "note") {
        println!("  - {note}");
    }
    if let Some(error) = msg_line(state, "error") {
        println!("  ! {error}");
    }
}

fn print_rgb_probe(probe: &Value) {
    let lighting = probe.get("lighting").cloned().unwrap_or(Value::Null);
    let per_key = probe.get("perKey").cloned().unwrap_or(Value::Null);

    // Both paths, always, even when neither is here: which one a machine
    // has is the question this command exists to answer, and a single
    // "no lighting" line answers it for neither.
    row("lighting", format!("{} - {}", yes_no(lighting.get("present")), text(&lighting, "detail")));

    // One line per dialect, because "no lighting" is three different
    // findings with three different next steps, and the manual override
    // needs the ids spelled out somewhere.
    if let Some(dialects) = lighting.get("dialects").and_then(Value::as_array) {
        for dialect in dialects {
            let mark = match dialect.get("available").and_then(Value::as_bool) {
                Some(true) => "yes",
                _ if dialect.get("asked").and_then(Value::as_bool) == Some(true) => "no",
                _ => "not asked",
            };
            println!(
                "    {:<12} {mark} - {}",
                text(dialect, "id"),
                text(dialect, "detail")
            );
        }
    }
    row(
        "per-key",
        format!("{} - {}", yes_no(per_key.get("present")), text(&per_key, "detail")),
    );
}

fn print_rgb(status: &Value) {
    if let Some(caps) = status.get("capabilities") {
        print_rgb_probe(caps);
    }
    print_zones(status);
    row("brightness", format!("{}%", status.get("brightness").and_then(Value::as_i64).unwrap_or(0)));
    row(
        "dialect",
        match (status.get("dialect").and_then(Value::as_str), status.get("activeDialect").and_then(Value::as_str)) {
            (Some("auto"), Some(active)) => format!("{active} (chosen automatically)"),
            (Some("auto"), None) => "auto - and nothing answered".to_string(),
            (Some(pinned), _) => format!("{pinned} (pinned by hand)"),
            _ => "unknown".to_string(),
        },
    );
    row(
        "set by",
        match status.get("owned").and_then(Value::as_bool) {
            Some(true) => "this daemon",
            _ => "nothing yet - these are the stored colours, not the hardware's",
        },
    );
    if let Some(error) = msg_line(status, "error") {
        println!("  ! {error}");
    }
}

fn print_gpu(status: &Value) {
    if status.get("supported").and_then(Value::as_bool) != Some(true) {
        println!("  no GPU MUX switch on this machine (no gpu_mux_mode)");
        return;
    }
    row(
        "mode",
        status.get("mode").and_then(Value::as_str).unwrap_or("unknown - firmware answered a mode this build does not recognise"),
    );
}

fn print_network(status: &Value) {
    if status.get("supported").and_then(Value::as_bool) != Some(true) {
        println!("  no default-route interface found (or 'tc' is not on PATH)");
        return;
    }
    row("interface", text(status, "interface"));
    row("mode", text(status, "mode"));
    row(
        "active qdisc",
        status.get("activeQdisc").and_then(Value::as_str).unwrap_or("unknown"),
    );
}

fn print_zones(value: &Value) {
    let zones = value
        .get("zones")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    if !zones.is_empty() {
        row("zones", zones);
    }
}

/// The one-screen answer to "what is this machine doing".
fn status(command: &args::Command) -> Run {
    let info = client::call("system", "getInfo", Value::Null)?;
    let power = power_state()?;
    let fan = client::call("fan", "getStatus", Value::Null)?;
    // Optional on purpose: a daemon built before the hotkey module answers
    // `unknownModule`, and one missing section must not take the screen
    // down with it.
    let hotkey = client::call("hotkey", "getStatus", Value::Null).ok();

    if command.json {
        print_json(&json!({ "system": info, "power": power, "fan": fan, "hotkey": hotkey }));
        return Ok(());
    }

    println!("machine");
    print_info(&info);
    println!("\npower");
    print_power(&power);
    println!("\nfans");
    print_fan(&fan);
    if let Some(hotkey) = &hotkey {
        println!("\nhotkey");
        print_hotkey(hotkey);
    }
    Ok(())
}
