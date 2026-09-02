//! Keeps `tools/omen-check.sh` and `omen-hub-check` from drifting apart.
//!
//! The shell script exists so the self-test can be run on a machine where
//! building this project isn't practical - which means it is the version
//! people will actually use in the field, and a script that quietly
//! disagrees with the app would be worse than no script at all.
//!
//! Both are run against the same fixture directories and must agree on the
//! verdict, the exit status and the per-check results.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/omen-check.sh")
        .canonicalize()
        .expect("tools/omen-check.sh should exist")
}

/// A fixture hwmon directory containing exactly the given files.
fn fixture(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omen-check-parity-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    for (name, contents) in files {
        std::fs::write(dir.join(name), contents).expect("fixture file");
    }
    dir
}

struct Run {
    exit_code: i32,
    report: Value,
}

fn run(program: &str, args: &[&str], hwmon: &Path) -> Run {
    let output = Command::new(program)
        .args(args)
        .env("OMEN_HUB_HWMON_DIR", hwmon)
        .output()
        .unwrap_or_else(|e| panic!("running {program}: {e}"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("{program} produced invalid JSON: {e}\n{stdout}"));

    Run { exit_code: output.status.code().unwrap_or(-1), report }
}

fn compare(tag: &str, files: &[(&str, &str)]) {
    let hwmon = fixture(tag, files);
    let rust = run(env!("CARGO_BIN_EXE_omen-hub-check"), &["--json"], &hwmon);
    let shell = run("sh", &[script_path().to_str().unwrap(), "--json"], &hwmon);

    assert_eq!(
        rust.exit_code, shell.exit_code,
        "{tag}: exit status differs (rust {} vs sh {})",
        rust.exit_code, shell.exit_code
    );

    let (rust_fan, shell_fan) = (&rust.report["fan"], &shell.report["fan"]);
    assert_eq!(rust_fan["verdict"], shell_fan["verdict"], "{tag}: verdict differs");

    // Compare the checks by id and status. Wording is allowed to differ in
    // punctuation; a status that disagrees is a real divergence.
    let statuses = |report: &Value| -> Vec<(String, String)> {
        report["checks"]
            .as_array()
            .expect("checks array")
            .iter()
            .map(|c| {
                (
                    c["id"].as_str().unwrap_or_default().to_string(),
                    c["status"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    };
    assert_eq!(statuses(rust_fan), statuses(shell_fan), "{tag}: per-check results differ");
}

#[test]
fn agree_on_a_machine_with_full_fan_control() {
    compare(
        "full",
        &[
            ("name", "hp\n"),
            ("fan1_input", "2400\n"),
            ("fan2_input", "2550\n"),
            ("pwm1", "128\n"),
            ("pwm1_enable", "2\n"),
        ],
    );
}

#[test]
fn agree_on_an_hwmon_node_with_no_pwm() {
    // The case the tool exists for: readable fans, no control channel.
    compare("nopwm", &[("name", "hp\n"), ("fan1_input", "2400\n"), ("fan2_input", "2300\n")]);
}

#[test]
fn agree_on_a_machine_with_no_interface_at_all() {
    compare("empty", &[]);
}

#[test]
fn agree_on_the_reverse_spin_encoding() {
    compare(
        "reverse",
        &[
            ("name", "hp\n"),
            ("fan1_input", "15200\n"),
            ("pwm1", "100\n"),
            ("pwm1_enable", "1\n"),
        ],
    );
}

#[test]
fn agree_on_an_unparseable_sysfs_value() {
    compare(
        "garbage",
        &[("name", "hp\n"), ("fan1_input", "not a number\n"), ("pwm1", "128\n"), ("pwm1_enable", "2\n")],
    );
}
