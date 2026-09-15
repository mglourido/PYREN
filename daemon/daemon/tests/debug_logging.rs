//! Closes the gap the final whole-branch review found: unit tests cover
//! `category_for`'s routing table and one stub-module `dispatch → ipc.jsonl`
//! test, but nothing exercises a *real* module end to end through the
//! logging path. This enables the toggle, drives real IPC calls against
//! real (fake-hardware) `fan`/`power` modules through a real `Registry`,
//! and asserts real lines landed in `ipc.jsonl` - the same kind of thing
//! that would have caught Critical 1 (the app process never mirroring the
//! toggle) had it existed on the app side too.
//!
//! Modeled on `energy_profiles.rs`'s style: fake sysfs paths through
//! `PYREN_*` env overrides, and a lock guarding the process-global state
//! this test also has to touch. `pyren_core::debuglog::test_lock` is
//! `pub(crate)` inside `pyren-core`, and this file is a separate
//! compilation unit (an integration test, not `#[cfg(test)]` inside the
//! crate), so it cannot reach that lock - hence its own local `Mutex`
//! below, kept deliberately parallel to `energy_profiles.rs`'s
//! `machine_lock()` rather than trying to share one across the crate
//! boundary.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use pyren_core::{Registry, Request};
use pyren_fan::FanModule;
use pyren_power::PowerModule;
use serde_json::{json, Value};

/// `pyren_core::debuglog`'s `ENABLED`/`ROOT` and the `PYREN_*` hardware
/// overrides below are all process-global, so only one of these fixtures
/// may be live at a time - same reasoning as `energy_profiles.rs`'s
/// `machine_lock`.
fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Where `pyren_core::debuglog` writes for every test in this binary.
///
/// `debuglog::init` sets a process-global `OnceLock` and "a second call is
/// silently ignored" (its own doc comment) - so, unlike the per-test
/// `root` below, this path cannot be a fresh temp directory per test.
/// Instead it is fixed once for the whole binary, and each test clears it
/// under `test_lock()` before dispatching so it only ever sees its own
/// lines.
fn shared_log_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("pyren-debuglog-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        pyren_core::debuglog::init(dir.clone());
        dir
    })
}

/// A laptop with fake fan/power sysfs, and debug logging enabled and
/// pointed at [`shared_log_dir`] for the fixture's lifetime.
struct Machine {
    root: PathBuf,
    log_dir: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl Machine {
    fn new(tag: &str) -> Self {
        let guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let root =
            std::env::temp_dir().join(format!("pyren-debuglog-it-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let log_dir = shared_log_dir().clone();
        // A clean slate for this test - held under the same lock, so
        // nothing else in this binary can be mid-write to it.
        let _ = std::fs::remove_dir_all(&log_dir);

        let machine = Self {
            root,
            log_dir,
            _guard: guard,
        };

        // --- enough fan/power sysfs for both modules to answer a read ---
        machine.write("acpi/platform_profile", "balanced");
        machine.write("acpi/platform_profile_choices", "cool balanced performance");
        machine.write("cpu/intel_pstate/no_turbo", "0");
        machine.write("powercap/intel-rapl:0/name", "package-0");
        machine.write("hwmon/pwm1", "0");
        machine.write("hwmon/pwm1_enable", "2");
        machine.write("hwmon/fan1_input", "0");
        machine.write("hwmon/fan2_input", "0");
        machine.write("cpu_temp", "45000");

        machine.apply_env();

        // Fresh state for every test in this binary: `ENABLED` is a
        // process-global `AtomicBool`, held under `test_lock()` for the
        // fixture's lifetime.
        pyren_core::debuglog::set_enabled(true);

        machine
    }

    fn write(&self, name: &str, contents: &str) {
        let path = self.root.join(name);
        std::fs::create_dir_all(path.parent().expect("nested")).expect("fixture dir");
        std::fs::write(path, contents).expect("fixture file");
    }

    fn apply_env(&self) {
        std::env::set_var(
            "PYREN_PLATFORM_PROFILE",
            self.root.join("acpi/platform_profile"),
        );
        std::env::set_var("PYREN_CPU_ROOT", self.root.join("cpu"));
        std::env::set_var("PYREN_POWERCAP", self.root.join("powercap"));
        std::env::set_var("PYREN_TOOLS_DIR", self.root.join("bin"));
        std::env::set_var("PYREN_POWER_SUPPLY", self.root.join("power_supply"));
        std::env::set_var("PYREN_MSR_ROOT", self.root.join("msr"));
        std::env::set_var("PYREN_HWMON_DIR", self.root.join("hwmon"));
        std::env::set_var("PYREN_CPU_TEMP_PATH", self.root.join("cpu_temp"));
    }

    fn store(&self, name: &str) -> pyren_config::ConfigStore {
        pyren_config::ConfigStore::at(self.root.join("config").join(name))
    }

    fn power(&self) -> PowerModule {
        PowerModule::with_store(self.store("power"))
    }

    fn fan(&self) -> FanModule {
        FanModule::with_store(self.store("fan"))
    }

    /// Every line already written to `ipc.jsonl`, parsed.
    fn ipc_lines(&self) -> Vec<Value> {
        let path = self.log_dir.join("ipc.jsonl");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        // Leave the process-global toggle off for whichever test runs next
        // in this binary - matches `debug_module.rs`'s own tests' habit of
        // resetting `debuglog::set_enabled(false)` before releasing the
        // lock.
        pyren_core::debuglog::set_enabled(false);
        for name in [
            "PYREN_PLATFORM_PROFILE",
            "PYREN_CPU_ROOT",
            "PYREN_POWERCAP",
            "PYREN_TOOLS_DIR",
            "PYREN_POWER_SUPPLY",
            "PYREN_MSR_ROOT",
            "PYREN_HWMON_DIR",
            "PYREN_CPU_TEMP_PATH",
        ] {
            std::env::remove_var(name);
        }
    }
}

fn request(id: u64, module: &str, method: &str, params: Value) -> Request {
    // `Request` only derives `Deserialize`, so it is built the way a real
    // connection would produce one: parsed off the wire, not constructed
    // as a struct literal.
    serde_json::from_value(json!({
        "id": id,
        "module": module,
        "method": method,
        "params": params,
    }))
    .expect("well-formed request")
}

/// A real trigger through a real `Registry::dispatch` produces a real
/// `ipc.jsonl` line, for two unrelated modules - the specific integration
/// gap the final whole-branch review found (Critical 1: the toggle was
/// unreachable from the app process, and nothing end-to-end had ever
/// proven the daemon side actually writes when it is on).
#[test]
fn dispatching_real_module_calls_writes_real_ipc_log_lines() {
    let machine = Machine::new("ipc-routing");

    let mut registry = Registry::new();
    registry.register(Box::new(machine.power()));
    registry.register(Box::new(machine.fan()));

    let power_response = registry.dispatch(request(1, "power", "getState", Value::Null));
    assert!(power_response.error.is_none(), "{:?}", power_response.error);

    let fan_response = registry.dispatch(request(2, "fan", "getStatus", Value::Null));
    assert!(fan_response.error.is_none(), "{:?}", fan_response.error);

    let lines = machine.ipc_lines();
    let has_call = |module: &str, method: &str| {
        lines.iter().any(|l| {
            l.get("module").and_then(Value::as_str) == Some(module)
                && l.get("method").and_then(Value::as_str) == Some(method)
                && l.get("ok") == Some(&Value::Bool(true))
        })
    };

    assert!(
        has_call("power", "getState"),
        "expected a power.getState line in ipc.jsonl, got {lines:#?}"
    );
    assert!(
        has_call("fan", "getStatus"),
        "expected a fan.getStatus line in ipc.jsonl, got {lines:#?}"
    );
}

/// The other half of the same gap: with the toggle off (the default for
/// every process that has not turned it on), the same calls must write
/// nothing at all.
#[test]
fn dispatching_with_logging_disabled_writes_nothing() {
    let machine = Machine::new("ipc-disabled");
    pyren_core::debuglog::set_enabled(false);

    let mut registry = Registry::new();
    registry.register(Box::new(machine.power()));

    let response = registry.dispatch(request(1, "power", "getState", Value::Null));
    assert!(response.error.is_none(), "{:?}", response.error);

    assert!(
        machine.ipc_lines().is_empty(),
        "logging is off, nothing should have been written"
    );
}
