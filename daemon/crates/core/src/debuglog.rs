//! Opt-in, user-facing debug logs: `~/.cache/pyren/depuration` (or the
//! daemon's own `/var/cache/pyren/depuration`), one JSONL file per topic,
//! written only while "Registros de depuración" is on.
//!
//! Distinct from [`crate::log`], which is the developer-facing
//! `PYREN_LOG` logger that always goes to stdout/stderr for the systemd
//! journal. This one is for a user to hand back with a bug report, is off
//! by default, and every category is a call to [`record`] from wherever
//! the thing being logged already happens - see
//! `docs/superpowers/specs/2026-09-14-debug-logging-design.md`.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use serde_json::{json, Value};

/// One append-only history file this system writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    DriverKernel,
    Performance,
    Lighting,
    Calibration,
    Cleaner,
    Installer,
    Daemon,
    Ipc,
    Widget,
    Frontend,
}

impl Category {
    fn filename(self) -> &'static str {
        match self {
            Self::DriverKernel => "driver-kernel.jsonl",
            Self::Performance => "performance.jsonl",
            Self::Lighting => "lighting.jsonl",
            Self::Calibration => "calibration.jsonl",
            Self::Cleaner => "cleaner.jsonl",
            Self::Installer => "installer.jsonl",
            Self::Daemon => "daemon.jsonl",
            Self::Ipc => "ipc.jsonl",
            Self::Widget => "widget.jsonl",
            Self::Frontend => "frontend.jsonl",
        }
    }
}

/// Cap before a file is rotated to `<name>.jsonl.1`. One generation, no
/// more - this is a diagnostic a user turns on for a session, not an
/// audit log.
const MAX_BYTES: u64 = 5 * 1024 * 1024;

static ENABLED: AtomicBool = AtomicBool::new(false);
static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Guards `ENABLED`/`ROOT` for the duration of any test that mutates or
/// asserts on them - both are process-global, and Cargo's default test
/// runner shares one process across threads, so two tests touching either
/// static race exactly like `energy_profiles.rs`'s `PYREN_*` overrides do
/// (see that file's `machine_lock`). Not a `#[test]` itself: it is
/// infrastructure other test modules (`debug_module.rs`, `lib.rs`) reach
/// via `crate::debuglog::test_lock()`.
#[cfg(test)]
pub(crate) fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Sets the directory every `record`/`record_if_changed` call writes
/// into. Call once, near the top of `main`, before anything might log.
/// A second call is silently ignored - the same shape as
/// `pyren_core::log`'s `LEVEL`.
pub fn init(root: PathBuf) {
    let _ = ROOT.set(root);
}

/// `/var/cache/pyren/depuration` when writable, else `~/.cache/pyren/
/// depuration` - the same probe-by-trying fallback `pyren_config::
/// ConfigStore::system()` uses for `/etc`. For the daemon binary, which
/// normally runs as root under systemd and has no desktop `$HOME`.
pub fn daemon_root() -> PathBuf {
    if let Ok(dir) = std::env::var("PYREN_DEPURATION_DIR") {
        return PathBuf::from(dir);
    }
    const SYSTEM_ROOT: &str = "/var/cache/pyren/depuration";
    let path = Path::new(SYSTEM_ROOT);
    if fs::create_dir_all(SYSTEM_ROOT).is_ok() && is_writable(path) {
        restrict_system_root(path);
        return PathBuf::from(SYSTEM_ROOT);
    }
    user_root()
}

/// Matches the socket's own trust boundary (`socket.rs`'s `bind_restricted`):
/// `0750`, group `pyren`, so this system-wide, potentially-multi-user
/// directory is not left at `create_dir_all`'s default `0755 root:root`
/// under systemd. Idempotent - safe to call on a directory a prior run
/// already created at the old, wider mode - and degrades the same way
/// `socket.rs` does when the `pyren` group does not exist: the directory is
/// simply left as it is rather than failing daemon startup.
fn restrict_system_root(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o750));
    if let Some(gid) = crate::socket::lookup_gid(&crate::socket::socket_group()) {
        let _ = crate::socket::chown_group(path, gid);
    }
}

/// `~/.cache/pyren/depuration`, always - for a process that already runs
/// as the desktop user (the OSD widget, the Tauri app), or the daemon run
/// unprivileged for development.
pub fn user_root() -> PathBuf {
    if let Ok(dir) = std::env::var("PYREN_DEPURATION_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".cache"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("pyren").join("depuration")
}

/// Whether `dir` can actually be written to, checked by trying rather than
/// by looking at the effective uid - being root is not the same as the
/// path being writable (read-only `/var`, containers, immutable distros).
///
/// Creates `dir` first if it does not exist yet, so this reports what a
/// real [`record`] call would actually achieve (which also does
/// `create_dir_all` internally) rather than false-negativing with `ENOENT`
/// on a directory nothing has written to yet - e.g. the `user_root()`
/// fallback, or a `PYREN_DEPURATION_DIR` override, neither of which is
/// created just by computing the path.
pub fn is_writable(dir: &Path) -> bool {
    let _ = fs::create_dir_all(dir);
    let probe = dir.join(".pyren-write-test");
    match File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Flips the in-memory flag only. Persisting it to `debug.json` is the
/// `debug` module's job (`debug_module.rs`) - a process that only mirrors
/// another one's setting (the OSD, the app) calls this with no config file
/// of its own to write.
pub fn set_enabled(value: bool) {
    ENABLED.store(value, Ordering::Relaxed);
}

/// Appends one JSON line (plus an injected `ts`) to `category`'s file. A
/// no-op, with no formatting cost paid, while disabled. Never surfaces a
/// failure to the caller: a write error is reported once via `log_warn!`
/// and otherwise swallowed, because a diagnostic aid must never be a
/// reason the daemon refuses to do the thing it is trying to log.
pub fn record(category: Category, value: impl Serialize) {
    if !enabled() {
        return;
    }
    let Some(root) = ROOT.get() else { return };
    let mut payload = serde_json::to_value(value).unwrap_or(Value::Null);
    with_ts_mut(&mut payload);
    if let Err(e) = write_entry(root, category, &payload) {
        crate::log_warn!("debug log: could not write {}: {e}", category.filename());
    }
}

/// Like [`record`], but only appends when `value` differs from the last
/// line already in the file (ignoring the injected `ts`). For a snapshot
/// that only matters when it changes - driver/kernel identity, not "the
/// daemon looked and it's still what it was."
pub fn record_if_changed(category: Category, value: impl Serialize) {
    if !enabled() {
        return;
    }
    let Some(root) = ROOT.get() else { return };
    let payload = serde_json::to_value(value).unwrap_or(Value::Null);
    let path = root.join(category.filename());
    if last_line_matches(&path, &payload) {
        return;
    }
    let mut payload = payload;
    with_ts_mut(&mut payload);
    if let Err(e) = write_entry(root, category, &payload) {
        crate::log_warn!("debug log: could not write {}: {e}", category.filename());
    }
}

fn with_ts_mut(payload: &mut Value) {
    if let Value::Object(map) = payload {
        map.insert("ts".to_string(), Value::from(now_ms()));
    }
}

fn write_entry(root: &Path, category: Category, payload: &Value) -> std::io::Result<()> {
    fs::create_dir_all(root)?;
    let path = root.join(category.filename());
    rotate_if_needed(&path)?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut line = serde_json::to_string(payload).unwrap_or_default();
    line.push('\n');
    file.write_all(line.as_bytes())
}

fn rotate_if_needed(path: &Path) -> std::io::Result<()> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok(());
    };
    if meta.len() < MAX_BYTES {
        return Ok(());
    }
    let backup = path.with_extension("jsonl.1");
    fs::rename(path, backup)
}

/// Whether the last line already in `path` is the same as `value`, field
/// for field except the timestamp. `false` for a missing or unreadable
/// file, which is exactly "nothing to compare against yet" - go ahead and
/// write the first line.
fn last_line_matches(path: &Path, value: &Value) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Some(last) = text.lines().last() else {
        return false;
    };
    let Ok(mut parsed) = serde_json::from_str::<Value>(last) else {
        return false;
    };
    if let Value::Object(map) = &mut parsed {
        map.remove("ts");
    }
    &parsed == value
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How long a run of identical `ipc.jsonl` calls (same module, method,
/// params, ok and error - everything but `durationMs`/`ts`) is allowed to
/// stay silently suppressed before it gets an anchor line anyway. Keeps a
/// steady poller (e.g. a widget calling `fan.getStatus` once a second)
/// visible in the log without one near-identical line per tick.
const IPC_STREAK_FLUSH_MS: u64 = 15_000;

/// One run of identical `ipc.jsonl` calls in progress: the shared
/// signature (everything but `durationMs`/`ts`), how many calls have
/// happened since the run's opening line was written, and the spread of
/// `durationMs` seen across them.
struct IpcStreak {
    signature: Value,
    count: u64,
    first_ts: u64,
    dur_min: u64,
    dur_max: u64,
    dur_sum: u64,
}

/// In-flight streaks, keyed by `(module, method)`. A crash or restart
/// loses whatever streak was open - acceptable for an opt-in diagnostic
/// log: the opening line for that value is already on disk, only the
/// trailing repeat count is missing.
fn ipc_streaks() -> &'static Mutex<HashMap<(String, String), IpcStreak>> {
    static STREAKS: OnceLock<Mutex<HashMap<(String, String), IpcStreak>>> = OnceLock::new();
    STREAKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Writes `ipc.jsonl`'s general (per-request) line, collapsing a run of
/// calls that all share the same module/method/params/ok/error into a
/// single opening line plus, only once the run actually ends or
/// [`IPC_STREAK_FLUSH_MS`] has passed, one `"repeat"` summary carrying the
/// suppressed count and the `durationMs` range seen across the run. A call
/// that never repeats costs exactly the one line it always did.
fn record_ipc(
    root: &Path,
    module: &str,
    method: &str,
    params: Option<Value>,
    ok: bool,
    error_message: Option<String>,
    duration_ms: u64,
) {
    let signature = json!({
        "module": module,
        "method": method,
        "params": params,
        "ok": ok,
        "error": error_message,
    });
    let now = now_ms();
    let key = (module.to_string(), method.to_string());

    let mut streaks = ipc_streaks().lock().unwrap_or_else(|e| e.into_inner());
    match streaks.get_mut(&key) {
        Some(streak) if streak.signature == signature => {
            streak.count += 1;
            streak.dur_min = streak.dur_min.min(duration_ms);
            streak.dur_max = streak.dur_max.max(duration_ms);
            streak.dur_sum += duration_ms;
            if now.saturating_sub(streak.first_ts) >= IPC_STREAK_FLUSH_MS {
                let streak = streaks.remove(&key).expect("just matched above");
                flush_ipc_streak(root, &streak, now);
            }
        }
        _ => {
            if let Some(prev) = streaks.remove(&key) {
                flush_ipc_streak(root, &prev, now);
            }
            let mut opening = signature.clone();
            if let Value::Object(map) = &mut opening {
                map.insert("durationMs".to_string(), Value::from(duration_ms));
            }
            with_ts_mut(&mut opening);
            if let Err(e) = write_entry(root, Category::Ipc, &opening) {
                crate::log_warn!("debug log: could not write {}: {e}", Category::Ipc.filename());
            }
            streaks.insert(
                key,
                IpcStreak {
                    signature,
                    count: 1,
                    first_ts: now,
                    dur_min: duration_ms,
                    dur_max: duration_ms,
                    dur_sum: duration_ms,
                },
            );
        }
    }
}

/// Appends the `"repeat"` summary for a finished streak - a no-op when the
/// opening line was the only call in it (`count == 1`), so a call that
/// never repeated costs nothing extra.
fn flush_ipc_streak(root: &Path, streak: &IpcStreak, now: u64) {
    if streak.count <= 1 {
        return;
    }
    let mut summary = streak.signature.clone();
    if let Value::Object(map) = &mut summary {
        map.insert("repeat".to_string(), Value::from(streak.count - 1));
        map.insert("durationMsMin".to_string(), Value::from(streak.dur_min));
        map.insert("durationMsMax".to_string(), Value::from(streak.dur_max));
        map.insert(
            "durationMsAvg".to_string(),
            Value::from(streak.dur_sum / streak.count),
        );
        map.insert("ts".to_string(), Value::from(now));
    }
    if let Err(e) = write_entry(root, Category::Ipc, &summary) {
        crate::log_warn!("debug log: could not write {}: {e}", Category::Ipc.filename());
    }
}

/// Whether `(module, method)` gets a richer copy of its result alongside
/// the general `ipc.jsonl` line - the calls whose value is in the full
/// response, not a one-line summary. Everything else still reaches
/// `ipc.jsonl`; this only decides the *extra* copy.
fn category_for(module: &str, method: &str) -> Option<Category> {
    match (module, method) {
        ("fan", "calibrate" | "diagnose" | "probeSpeedControl") => Some(Category::Calibration),
        ("fan", "startCleaning" | "stopCleaning" | "cleanerStatus") => Some(Category::Cleaner),
        ("rgb", _) => Some(Category::Lighting),
        ("installer", "apply" | "plan" | "inspect") => Some(Category::Installer),
        _ => None,
    }
}

/// Called once per IPC request from [`crate::Registry::dispatch`], after
/// the response already exists. `core.*` calls never reach here -
/// `dispatch` answers those before this would be called, which is also
/// why `nextEvent`'s long poll never pollutes the transcript.
pub fn on_ipc(
    module: &str,
    method: &str,
    params: Option<Value>,
    response: &crate::Response,
    duration: std::time::Duration,
) {
    if !enabled() {
        return;
    }
    let ok = response.error.is_none();
    let error_message = response.error.as_ref().map(|e| e.message.clone());
    if let Some(root) = ROOT.get() {
        record_ipc(
            root,
            module,
            method,
            params.clone(),
            ok,
            error_message.clone(),
            duration.as_millis() as u64,
        );
    }

    let Some(category) = category_for(module, method) else {
        return;
    };
    record(
        category,
        json!({
            "method": method,
            "params": params,
            "ok": ok,
            "result": response.result.clone(),
            "error": error_message,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pyren-debuglog-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn every_category_has_a_distinct_stable_filename() {
        let all = [
            Category::DriverKernel,
            Category::Performance,
            Category::Lighting,
            Category::Calibration,
            Category::Cleaner,
            Category::Installer,
            Category::Daemon,
            Category::Ipc,
            Category::Widget,
            Category::Frontend,
        ];
        let names: Vec<&str> = all.iter().map(|c| c.filename()).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "two categories share a filename");
        assert_eq!(Category::DriverKernel.filename(), "driver-kernel.jsonl");
        assert_eq!(Category::Ipc.filename(), "ipc.jsonl");
    }

    #[test]
    fn write_entry_appends_one_line_per_call() {
        let dir = tmp("append");
        write_entry(&dir, Category::Daemon, &json!({ "event": "startup" })).unwrap();
        write_entry(&dir, Category::Daemon, &json!({ "event": "shutdown" })).unwrap();

        let text = fs::read_to_string(dir.join("daemon.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<Value>(lines[0]).unwrap()["event"],
            "startup"
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap()["event"],
            "shutdown"
        );
    }

    #[test]
    fn with_ts_mut_adds_a_timestamp_to_an_object() {
        let mut payload = json!({ "a": 1 });
        with_ts_mut(&mut payload);
        assert!(payload["ts"].is_u64());
        assert_eq!(payload["a"], 1);
    }

    #[test]
    fn rotate_if_needed_moves_the_file_aside_once_it_crosses_the_cap() {
        let dir = tmp("rotate");
        let path = dir.join("ipc.jsonl");
        fs::write(&path, "x".repeat((MAX_BYTES + 1) as usize)).unwrap();

        rotate_if_needed(&path).unwrap();

        assert!(!path.exists(), "the oversized file should have been moved aside");
        let backup = dir.join("ipc.jsonl.1");
        assert!(backup.exists());
        assert_eq!(fs::metadata(backup).unwrap().len(), MAX_BYTES + 1);
    }

    #[test]
    fn rotate_if_needed_leaves_a_small_file_alone() {
        let dir = tmp("no-rotate");
        let path = dir.join("ipc.jsonl");
        fs::write(&path, "small").unwrap();

        rotate_if_needed(&path).unwrap();

        assert!(path.exists());
        assert!(!dir.join("ipc.jsonl.1").exists());
    }

    #[test]
    fn last_line_matches_ignores_the_injected_timestamp() {
        let dir = tmp("last-line");
        let path = dir.join("driver-kernel.jsonl");
        fs::write(&path, r#"{"kernel":"6.12","ts":123}"#.to_string() + "\n").unwrap();

        assert!(last_line_matches(&path, &json!({ "kernel": "6.12" })));
        assert!(!last_line_matches(&path, &json!({ "kernel": "6.13" })));
    }

    #[test]
    fn last_line_matches_is_false_for_a_file_that_does_not_exist_yet() {
        let dir = tmp("missing");
        assert!(!last_line_matches(&dir.join("driver-kernel.jsonl"), &json!({ "a": 1 })));
    }

    #[test]
    fn record_and_record_if_changed_are_a_no_op_while_disabled() {
        // Other tests in this binary flip ENABLED under `test_lock()`, so
        // take the same lock before reading it here - otherwise this can
        // observe another thread's `true` mid-flight. `record`/
        // `record_if_changed` both check `enabled()` before touching the
        // filesystem, which the source above shows directly. The
        // filesystem-touching behaviour itself is covered through
        // `write_entry`/`rotate_if_needed`/`last_line_matches` above,
        // which take an explicit root and never read the global state.
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        assert!(!enabled(), "must default to off");
    }

    #[test]
    fn category_for_routes_the_rich_methods_to_their_own_file() {
        assert_eq!(category_for("fan", "calibrate"), Some(Category::Calibration));
        assert_eq!(category_for("fan", "diagnose"), Some(Category::Calibration));
        assert_eq!(
            category_for("fan", "probeSpeedControl"),
            Some(Category::Calibration)
        );
        assert_eq!(category_for("fan", "startCleaning"), Some(Category::Cleaner));
        assert_eq!(category_for("fan", "stopCleaning"), Some(Category::Cleaner));
        assert_eq!(category_for("fan", "cleanerStatus"), Some(Category::Cleaner));
        assert_eq!(category_for("rgb", "setStatic"), Some(Category::Lighting));
        assert_eq!(category_for("rgb", "setBrightness"), Some(Category::Lighting));
        assert_eq!(category_for("installer", "apply"), Some(Category::Installer));
        assert_eq!(category_for("installer", "plan"), Some(Category::Installer));
        assert_eq!(category_for("installer", "inspect"), Some(Category::Installer));
    }

    #[test]
    fn category_for_is_none_for_everything_else() {
        assert_eq!(category_for("power", "setMode"), None);
        assert_eq!(category_for("fan", "getStatus"), None);
        assert_eq!(category_for("fan", "setMode"), None);
        assert_eq!(category_for("installer", "autodetect"), None);
    }

    // `record_ipc`'s streak state (`ipc_streaks()`) is one process-global
    // map, shared by every test in this binary - unlike `write_entry` and
    // friends, which take an explicit root and so need no isolation beyond
    // their own `tmp()` directory. Each test below uses a module name
    // found nowhere else, so streaks never cross between tests even when
    // the test runner runs them concurrently.

    #[test]
    fn record_ipc_writes_one_opening_line_for_a_run_of_identical_calls() {
        let dir = tmp("record-ipc-collapse");
        let module = "test-record-ipc-collapse";
        record_ipc(&dir, module, "getStatus", None, true, None, 34);
        record_ipc(&dir, module, "getStatus", None, true, None, 35);
        record_ipc(&dir, module, "getStatus", None, true, None, 36);

        let text = fs::read_to_string(dir.join("ipc.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "the two repeats should be suppressed, not appended");
        let line: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(line["module"], module);
        assert_eq!(line["durationMs"], 34);
        assert!(line.get("repeat").is_none());
    }

    #[test]
    fn record_ipc_writes_no_summary_when_a_call_never_repeats() {
        let dir = tmp("record-ipc-no-repeat");
        let module = "test-record-ipc-no-repeat";
        record_ipc(&dir, module, "getStatus", None, true, None, 30);
        record_ipc(&dir, module, "getStatus", Some(json!({"x": 1})), true, None, 40);

        let text = fs::read_to_string(dir.join("ipc.jsonl")).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2, "just the two distinct opening lines, no summary between them");
        assert!(lines.iter().all(|l| l.get("repeat").is_none()));
    }

    #[test]
    fn record_ipc_flushes_a_repeat_summary_when_the_signature_changes() {
        let dir = tmp("record-ipc-flush-on-change");
        let module = "test-record-ipc-flush-on-change";
        record_ipc(&dir, module, "getStatus", None, true, None, 30); // opening
        record_ipc(&dir, module, "getStatus", None, true, None, 32); // suppressed repeat
        record_ipc(&dir, module, "getStatus", Some(json!({"x": 1})), true, None, 40); // new value: flush + opening

        let text = fs::read_to_string(dir.join("ipc.jsonl")).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 3, "opening, summary, new opening");

        assert_eq!(lines[0]["durationMs"], 30);
        assert!(lines[0].get("repeat").is_none());

        assert_eq!(lines[1]["repeat"], 1, "one call was suppressed before the run ended");
        assert_eq!(lines[1]["durationMsMin"], 30);
        assert_eq!(lines[1]["durationMsMax"], 32);
        assert_eq!(lines[1]["module"], module);

        assert_eq!(lines[2]["params"], json!({"x": 1}));
        assert!(lines[2].get("repeat").is_none());
    }

    #[test]
    fn flush_ipc_streak_reports_the_suppressed_count_and_duration_spread() {
        let dir = tmp("flush-streak-repeated");
        let streak = IpcStreak {
            signature: json!({"module": "fan", "method": "getStatus", "params": null, "ok": true, "error": null}),
            count: 5,
            first_ts: 1_000,
            dur_min: 30,
            dur_max: 50,
            dur_sum: 190,
        };
        flush_ipc_streak(&dir, &streak, 9_999);

        let text = fs::read_to_string(dir.join("ipc.jsonl")).unwrap();
        let line: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(line["repeat"], 4, "count includes the already-written opening line");
        assert_eq!(line["durationMsMin"], 30);
        assert_eq!(line["durationMsMax"], 50);
        assert_eq!(line["durationMsAvg"], 38);
        assert_eq!(line["ts"], 9_999);
    }

    #[test]
    fn flush_ipc_streak_is_a_no_op_for_a_streak_that_never_repeated() {
        let dir = tmp("flush-streak-single");
        let streak = IpcStreak {
            signature: json!({"a": 1}),
            count: 1,
            first_ts: 0,
            dur_min: 10,
            dur_max: 10,
            dur_sum: 10,
        };
        flush_ipc_streak(&dir, &streak, 100);

        assert!(!dir.join("ipc.jsonl").exists());
    }
}
