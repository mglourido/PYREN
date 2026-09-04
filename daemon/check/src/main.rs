//! `pyren-check` - standalone compatibility verifier.
//!
//! Answers, on an unfamiliar laptop and without a daemon, a socket or a
//! GUI: **what can this machine actually be told to do?** Three surfaces,
//! one verdict.
//!
//! - **Fans** - the self-test the app runs as `fan.diagnose`, in detail.
//! - **Power** - what would drive the modes, and whether there is an
//!   envelope to move.
//! - **Lighting** - the two unrelated RGB paths, probed rather than
//!   guessed from the model name.
//!
//! The verdict at the end is `system.compatibility`, the same one the
//! daemon prints at startup and the app shows on its Hardware page,
//! derived from the same probes. Somebody pasting this output into a bug
//! report must not then be told something different by the app.
//!
//! ```text
//! pyren-check              # read-only, safe anywhere
//! pyren-check --write      # also test that the PWM accepts writes (needs root)
//! pyren-check --json       # machine-readable, for bug reports
//! ```

mod compat;

use std::process::ExitCode;

use pyren_fan::{
    diagnostics::{Check, CheckStatus, Verdict},
    FanModule,
};
use pyren_system::{Controls, SystemIdentity};

const HELP: &str = "\
pyren-check - what this machine can actually be told to do

USAGE:
    pyren-check [OPTIONS]

Checks three surfaces - fans, power modes and lighting - and prints one
verdict: the same one the daemon prints at startup.

OPTIONS:
    --write    Also verify that the PWM channel accepts writes. Rewrites the
               value that is already set and restores the previous mode, so
               no fan changes speed. Needs root.
    --json     Print the full report as JSON.
    -h, --help Show this help.

EXIT STATUS is about fan control, which is what scripts branch on:
    0  fan control works
    1  fan speeds can be read but not set
    2  no HP fan-control interface on this machine

The overall verdict is wider than that - a machine with no fan control can
still have power modes and a lightbar - so read the last line, not $?, for
the compatibility answer.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if let Some(unknown) = args.iter().find(|a| !matches!(a.as_str(), "--write" | "--json")) {
        eprintln!("pyren-check: unknown argument '{unknown}'\n\n{HELP}");
        return ExitCode::from(64);
    }

    let allow_writes = args.iter().any(|a| a == "--write");
    let json = args.iter().any(|a| a == "--json");

    // Every surface is probed before anything is classified, because the
    // verdict is a *summary of what was found* and nothing else. This is
    // the same order the daemon uses in `main.rs`, for the same reason.
    let fan = FanModule::inspector();
    let power = pyren_power::surface();
    let lighting = pyren_rgb::probe::probe();

    let identity = SystemIdentity::detect(Controls {
        fan_mode: fan.capabilities().switch_mode,
        fan_speed: fan.capabilities().set_speed,
        power_mode: !power.mechanisms.is_empty(),
        lightbar: lighting.lighting.present,
        // Not probed here: GPU MUX switching and network QoS are
        // `pyren-daemon`'s own modules, and this tool's checks
        // (`compat.rs`) stop at fans, power and lighting, same as they
        // already do for overclock and hotkey - see the daemon/rgb docs
        // for why.
        gpu_mux: false,
        network_qos: false,
    });
    let diagnosis = fan.diagnose(allow_writes);
    let power_section = compat::power(&power);
    let lighting_section = compat::lighting(&lighting);

    if json {
        let report = serde_json::json!({
            "system": identity,
            "fan": diagnosis,
            "power": power_section,
            "lighting": lighting_section,
        });
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    } else {
        print_report(&identity, &diagnosis, &power_section, &lighting_section, allow_writes);
    }

    match diagnosis.verdict {
        Verdict::FullControl => ExitCode::SUCCESS,
        Verdict::MonitoringOnly => ExitCode::from(1),
        Verdict::Unsupported => ExitCode::from(2),
    }
}

fn print_report(
    identity: &SystemIdentity,
    diagnosis: &pyren_fan::diagnostics::Diagnosis,
    power: &compat::Section,
    lighting: &compat::Section,
    allow_writes: bool,
) {
    println!("pyren-check\n");
    println!("  machine  {}", identity.summary());
    if let Some(kernel) = &identity.kernel {
        println!("  kernel   {kernel}");
    }

    println!("\nfans");
    for check in &diagnosis.checks {
        print_check(check);
    }
    println!("  {} passed, {} failed", diagnosis.passed(), diagnosis.failed());
    for line in wrap(&diagnosis.summary.text, 72) {
        println!("  {line}");
    }
    if let Some(notice) = &diagnosis.driver_notice {
        for line in wrap(&notice.text, 72) {
            println!("  ! {line}");
        }
    }

    print_section("power", power);
    print_section("lighting", lighting);

    // The one line this whole tool exists to print, so it goes last and
    // says what it is rather than being another summary among four.
    println!("\ncompatibility");
    for line in wrap(&identity.reason.text, 72) {
        println!("  {line}");
    }

    if !allow_writes && diagnosis.verdict == Verdict::FullControl {
        println!("\n  Re-run with --write (as root) to confirm the hardware accepts writes.");
    }
}

fn print_section(title: &str, section: &compat::Section) {
    println!("\n{title}");
    for check in &section.checks {
        print_check(check);
    }
    for line in wrap(&section.summary, 72) {
        println!("  {line}");
    }
}

fn print_check(check: &Check) {
    println!("  {}  {:<28} {}", marker(check.status), check.title.text, check.detail.text);
    if let Some(remedy) = &check.remedy {
        for line in wrap(&remedy.text, 68) {
            println!("        {line}");
        }
    }
}

/// Plain ASCII markers rather than colour: this output gets pasted into
/// issue trackers, where escape codes turn into noise.
fn marker(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Pass => "[ ok ]",
        CheckStatus::Fail => "[FAIL]",
        CheckStatus::Warn => "[warn]",
        CheckStatus::Skip => "[skip]",
    }
}

/// Wraps on word boundaries so long remedies stay readable in a terminal.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        if !current.is_empty() && current.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}
