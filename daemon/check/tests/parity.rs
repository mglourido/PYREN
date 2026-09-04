//! Keeps `tools/pyren-check.sh` and `pyren-check` from drifting apart.
//!
//! The shell script exists so the self-test can be run on a machine where
//! building this project isn't practical - which means it is the version
//! people will actually use in the field, and a script that quietly
//! disagrees with the app would be worse than no script at all.
//!
//! Both are run against the same fixture directories and must agree on the
//! verdict, the exit status, the compatibility answer and the per-check
//! results - in every section, not only the fan one.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/pyren-check.sh")
        .canonicalize()
        .expect("tools/pyren-check.sh should exist")
}

/// A fixture directory containing exactly the given files. Names may
/// contain `/`, which is how a fake USB bus is built under `usb/`.
fn fixture(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pyren-check-parity-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    for (name, contents) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture subdir");
        }
        std::fs::write(path, contents).expect("fixture file");
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
        .env("PYREN_HWMON_DIR", hwmon)
        // The lighting checks must look at the same nothing on both sides.
        // Without this they would read the developer's real USB bus, which
        // is not a fixture and differs from CI's.
        .env("PYREN_USB_DEVICES", hwmon.join("usb"))
        .output()
        .unwrap_or_else(|e| panic!("running {program}: {e}"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("{program} produced invalid JSON: {e}\n{stdout}"));

    Run { exit_code: output.status.code().unwrap_or(-1), report }
}

fn compare(tag: &str, files: &[(&str, &str)]) {
    let hwmon = fixture(tag, files);
    let rust = run(env!("CARGO_BIN_EXE_pyren-check"), &["--json"], &hwmon);
    let shell = run("sh", &[script_path().to_str().unwrap(), "--json"], &hwmon);

    assert_eq!(
        rust.exit_code, shell.exit_code,
        "{tag}: exit status differs (rust {} vs sh {})",
        rust.exit_code, shell.exit_code
    );

    assert_eq!(
        rust.report["fan"]["verdict"], shell.report["fan"]["verdict"],
        "{tag}: fan verdict differs"
    );

    // The one verdict the whole tool exists to print. It is derived from
    // the controls, so a disagreement here means the two disagree about
    // what the machine can be told to do - the worst kind of drift, since
    // it is the line people read.
    assert_eq!(
        rust.report["system"]["compatibility"], shell.report["system"]["compatibility"],
        "{tag}: compatibility differs"
    );
    // `pyren-check`'s `reason` is a translatable `Msg` object (`{ key,
    // params, text }`); the shell script emits the plain English string. The
    // wording is what must not drift, so compare that.
    assert_eq!(
        rust.report["system"]["reason"]["text"], shell.report["system"]["reason"],
        "{tag}: the reason behind the verdict differs"
    );
    assert_eq!(
        rust.report["system"]["controls"], shell.report["system"]["controls"],
        "{tag}: controls differ"
    );

    // Every section, not just the fan one: a section present on one side
    // and absent on the other would silently compare as two empty lists.
    for section in ["fan", "power", "lighting"] {
        let (rust_section, shell_section) = (&rust.report[section], &shell.report[section]);
        assert!(rust_section.is_object(), "{tag}: rust has no '{section}' section");
        assert!(shell_section.is_object(), "{tag}: sh has no '{section}' section");
        assert_eq!(
            statuses(rust_section),
            statuses(shell_section),
            "{tag}: per-check results differ in '{section}'"
        );
    }
}

/// Checks by id and status. Wording is allowed to differ in punctuation; a
/// status that disagrees, or a check that only one side emits, is a real
/// divergence.
fn statuses(section: &Value) -> Vec<(String, String)> {
    section["checks"]
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

/// The per-key keyboard is the one piece of lighting hardware that can be
/// faked, so it is the one that can be checked for agreement rather than
/// only for "neither side found anything".
#[test]
fn agree_on_a_machine_with_a_per_key_keyboard_attached() {
    compare(
        "perkey",
        &[
            ("name", "hp\n"),
            ("fan1_input", "2400\n"),
            ("usb/1-2/idVendor", "0d62\n"),
            ("usb/1-2/idProduct", "54bf\n"),
            // A second device that is not it, so a match is a match on
            // both fields rather than on whichever was read first.
            ("usb/1-3/idVendor", "0d62\n"),
            ("usb/1-3/idProduct", "0001\n"),
        ],
    );

    let hwmon = fixture("perkey-detail", &[
        ("usb/2-1/idVendor", "0d62\n"),
        ("usb/2-1/idProduct", "54bf\n"),
    ]);
    let rust = run(env!("CARGO_BIN_EXE_pyren-check"), &["--json"], &hwmon);
    assert_eq!(
        statuses(&rust.report["lighting"])[0],
        ("lighting-per-key".to_string(), "warn".to_string()),
        "an attached keyboard this build cannot drive is a warning, not a pass"
    );
}

/// The one constant the shell script cannot derive from anything: the
/// 144-byte ACPI read it writes to `/proc/acpi/call`. If it drifts from
/// `read_request(0)` the script asks the firmware a different question
/// than the daemon does, and silently gets a different answer.
#[test]
fn the_shell_script_asks_the_firmware_the_same_question() {
    let request = pyren_rgb::lightbar::read_request(0);
    let script = std::fs::read_to_string(script_path()).expect("the script should be readable");

    // 'b' + a 16-byte header + a 128-byte payload, as hex.
    assert_eq!(request.len(), 1 + (16 + 128) * 2);

    let header = &request[..33];
    assert!(
        script.contains(header),
        "tools/pyren-check.sh no longer contains the ACPI read header {header}"
    );
    assert!(
        script.contains("%0256d"),
        "the script must still pad the request to 128 payload bytes"
    );
    assert!(
        request[33..].bytes().all(|b| b == b'0'),
        "zone 0 is an all-zero payload; if that changed, so must the script"
    );
}
