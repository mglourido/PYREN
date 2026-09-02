//! `omen-hub-check` - standalone fan-control verifier.
//!
//! Runs the same self-test as the app's `fan.diagnose`, but as a small
//! binary that needs no daemon, no socket and no GUI. That matters because
//! this is the thing you want to run *first* on an unfamiliar laptop, and
//! the thing to paste into a bug report.
//!
//! ```text
//! omen-hub-check              # read-only, safe anywhere
//! omen-hub-check --write      # also test that the PWM accepts writes (needs root)
//! omen-hub-check --json       # machine-readable, for bug reports
//! ```

use std::process::ExitCode;

use omen_hub_fan::{
    diagnostics::{CheckStatus, Verdict},
    FanModule,
};
use omen_hub_system::SystemIdentity;

const HELP: &str = "\
omen-hub-check - verify that fan control works on this machine

USAGE:
    omen-hub-check [OPTIONS]

OPTIONS:
    --write    Also verify that the PWM channel accepts writes. Rewrites the
               value that is already set and restores the previous mode, so
               no fan changes speed. Needs root.
    --json     Print the full report as JSON.
    -h, --help Show this help.

EXIT STATUS:
    0  fan control works
    1  fan speeds can be read but not set
    2  no HP fan-control interface on this machine
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if let Some(unknown) = args.iter().find(|a| !matches!(a.as_str(), "--write" | "--json")) {
        eprintln!("omen-hub-check: unknown argument '{unknown}'\n\n{HELP}");
        return ExitCode::from(64);
    }

    let allow_writes = args.iter().any(|a| a == "--write");
    let json = args.iter().any(|a| a == "--json");

    let identity = SystemIdentity::detect();
    let diagnosis = FanModule::new().diagnose(allow_writes);

    if json {
        let report = serde_json::json!({ "system": identity, "fan": diagnosis });
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    } else {
        print_report(&identity, &diagnosis, allow_writes);
    }

    match diagnosis.verdict {
        Verdict::FullControl => ExitCode::SUCCESS,
        Verdict::MonitoringOnly => ExitCode::from(1),
        Verdict::Unsupported => ExitCode::from(2),
    }
}

fn print_report(
    identity: &SystemIdentity,
    diagnosis: &omen_hub_fan::diagnostics::Diagnosis,
    allow_writes: bool,
) {
    println!("omen-hub-check\n");
    println!("  machine  {}", identity.summary());
    if let Some(kernel) = &identity.kernel {
        println!("  kernel   {kernel}");
    }
    println!();

    for check in &diagnosis.checks {
        println!("  {}  {:<28} {}", marker(check.status), check.title, check.detail);
        if let Some(remedy) = &check.remedy {
            for line in wrap(remedy, 68) {
                println!("        {line}");
            }
        }
    }

    println!();
    println!("  {} passed, {} failed", diagnosis.passed(), diagnosis.failed());
    println!();
    for line in wrap(&diagnosis.summary, 72) {
        println!("  {line}");
    }

    if let Some(notice) = &diagnosis.driver_notice {
        println!();
        for line in wrap(notice, 72) {
            println!("  ! {line}");
        }
    }

    if !allow_writes && diagnosis.verdict == Verdict::FullControl {
        println!("\n  Re-run with --write (as root) to confirm the hardware accepts writes.");
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
