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
                               the two automatic systems: unplugging drops
                               to Balanced then refines towards Eco;
                               plugging in steps up to Performance
  power restore-on-start <on|off>

FANS
  fan get                      speed, temperature, mode, capabilities
  fan set <auto|max|manual|curve> [--pwm 0-255]
  fan curve <t:pct,...> [--interpolation smooth|discrete]
                               e.g. fan curve 40:20,60:50,85:100
  fan restore-on-start <on|off>
  fan diagnose [--write]       the fan-control self-test

OPTIONS
  --json                       print the daemon's reply verbatim
  -h, --help
  -V, --version

The socket is $PYREN_SOCKET, or /tmp/pyren-daemon.sock. Reaching a
daemon running as root means being in the 'pyren' group.
";

const USAGE_ERROR: u8 = 2;
const UNREACHABLE: u8 = 3;

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
            ExitCode::from(match e {
                ClientError::Daemon(_) => 1,
                _ => UNREACHABLE,
            })
        }
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
            show(command, client::call("fan", "setCurve", params)?, print_fan)
        }
        ["fan", "restore-on-start", value] => {
            let enabled = word_switch("restore-on-start", value)?;
            show(command, client::call("fan", "setRestoreOnStart", json!({ "enabled": enabled }))?, print_fan)
        }
        ["fan", "diagnose"] => {
            let allow_writes = command.options.contains_key("write");
            show(
                command,
                client::call("fan", "diagnose", json!({ "allowWrites": allow_writes }))?,
                print_diagnosis,
            )
        }

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

    client::call("power", "setAutoConfig", config)?;
    show(command, power_state()?, print_power)
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
fn row(label: &str, value: impl std::fmt::Display) {
    println!("  {label:<10} {value}");
}

fn text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "-".to_string(),
        Some(other) => other.to_string(),
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
    if let Some(last) = state.get("lastAutoSwitch").and_then(Value::as_str) {
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
            "{} rpm, cpu {} C{}",
            status.get("fanRpm").and_then(Value::as_i64).unwrap_or(0),
            status
                .get("cpuTempC")
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
    if let Some(error) = status.get("error").and_then(Value::as_str) {
        println!("  ! {error}");
    }
}

fn print_diagnosis(diagnosis: &Value) {
    row("verdict", text(diagnosis, "verdict"));
    println!("  {}", text(diagnosis, "summary"));
    for check in diagnosis.get("checks").and_then(Value::as_array).unwrap_or(&Vec::new()) {
        println!(
            "  [{:^6}] {:28} {}",
            text(check, "status"),
            text(check, "label"),
            text(check, "detail")
        );
    }
    if let Some(notice) = diagnosis.get("driverNotice").and_then(Value::as_str) {
        println!("\n  {notice}");
    }
}

/// The one-screen answer to "what is this machine doing".
fn status(command: &args::Command) -> Run {
    let info = client::call("system", "getInfo", Value::Null)?;
    let power = power_state()?;
    let fan = client::call("fan", "getStatus", Value::Null)?;

    if command.json {
        print_json(&json!({ "system": info, "power": power, "fan": fan }));
        return Ok(());
    }

    println!("machine");
    print_info(&info);
    println!("\npower");
    print_power(&power);
    println!("\nfans");
    print_fan(&fan);
    Ok(())
}
