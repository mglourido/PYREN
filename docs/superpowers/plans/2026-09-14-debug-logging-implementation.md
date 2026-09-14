# Debug Logging System Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in, user-facing debug-logging system — one switch in
Settings — that keeps rolling JSONL histories of driver/kernel identity,
power+fan mode changes, RGB commands, calibration/diagnostic runs, the fan
cleaner, driver installs, the daemon's own lifecycle, the full IPC
transcript, the OSD widget, and the desktop app's frontend.

**Architecture:** A single shared module, `pyren_core::debuglog`, owns file
writing and 5 MB rotation. Every call site that feeds it is exactly one
line, added after existing logic — never a branch woven into `Registry::
dispatch`'s match, the `EventBus` wiring, or the OSD's message loop —
so the feature can be deleted later by deleting those lines. The daemon
writes to `/var/cache/pyren/depuration` (falling back to `~/.cache/…`
unprivileged, same probe-by-trying as `ConfigStore::system()`); the OSD
widget and the desktop app, which already run as the user, always write to
`~/.cache/pyren/depuration`. A new tiny `debug` IPC module persists the one
boolean and publishes `debug.changed` so every process picks up a toggle
live.

**Tech Stack:** Rust (daemon, OSD, Tauri backend) — no new crate
dependencies beyond `pyren-core`/`pyren-config` path deps already used
elsewhere in the workspace. Svelte 5 + TypeScript (app frontend), using the
existing `@tauri-apps/plugin-opener` (already installed, already permitted
via `opener:default`) for "open logs folder".

**Spec:** `docs/superpowers/specs/2026-09-14-debug-logging-design.md`

## Global Constraints

- Every hook point is a **single additive line** (or a small, obviously
  self-contained, easily-deletable block) placed after existing logic.
  Never restructure an existing `match`, `if`, or closure to accommodate
  logging.
- No new external crate dependencies. `pyren_core::debuglog` uses only
  `std`, `serde`, `serde_json` — already present everywhere it's added.
- One master toggle. No per-category switches, anywhere.
- 5 MB per file, one rotation generation (`<name>.jsonl` → `<name>.jsonl.1`).
- `core.*` IPC calls (`capabilities`, `nextEvent`) are **not** logged to
  `ipc.jsonl` — `nextEvent` is a long poll and would otherwise fill the
  transcript with 25-second "calls" that carry no information.
- Follow existing project conventions exactly: `ConfigStore`'s
  load/save/namespace pattern for `debug.json`, the `Announcer`/`publish_to`
  pattern for publishing to the `EventBus`, `#[tauri::command(async)]` for
  every new Tauri command, and the `admin.ts`/`config.ts` shape for new
  frontend API wrappers.
- This project's existing test convention: business logic gets unit tests
  with an explicit temp directory (see `pyren_config`'s and `pyren_fan`'s
  tests); process wiring in `main.rs`, the OSD's GTK loop, and the Tauri
  shell has **no existing automated tests** and this plan does not invent
  any — those get a manual verification checklist instead, matching how
  the rest of `daemon/daemon/src/main.rs` is exercised today.

---

## Task 1: `pyren_core::debuglog` — the shared writer

**Files:**
- Create: `daemon/crates/core/src/debuglog.rs`
- Modify: `daemon/crates/core/src/lib.rs:20-24` (add `pub mod debuglog;`
  next to the other `pub mod` lines)

**Interfaces:**
- Produces (used by every later task):
  - `pub enum Category { DriverKernel, Performance, Lighting, Calibration, Cleaner, Installer, Daemon, Ipc, Widget, Frontend }`
  - `pub fn init(root: PathBuf)`
  - `pub fn daemon_root() -> PathBuf`
  - `pub fn user_root() -> PathBuf`
  - `pub fn is_writable(dir: &Path) -> bool`
  - `pub fn enabled() -> bool`
  - `pub fn set_enabled(value: bool)`
  - `pub fn record(category: Category, value: impl Serialize)`
  - `pub fn record_if_changed(category: Category, value: impl Serialize)`
  - `pub fn on_ipc(module: &str, method: &str, params: Option<Value>, response: &crate::Response, duration: std::time::Duration)` (built in Task 2, declared here as a stub that panics with `unimplemented!()` so Task 1 compiles standalone — replaced for real in Task 2)

- [ ] **Step 1: Write the failing tests**

Create `daemon/crates/core/src/debuglog.rs` with just the test module first
(everything it calls doesn't exist yet, so this won't compile — that's the
point):

```rust
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

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

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
        // No global ENABLED/ROOT mutation here on purpose - other tests in
        // this binary run in parallel and share those statics. This test
        // only proves the guard exists by construction: `record`/
        // `record_if_changed` both check `enabled()` before touching the
        // filesystem, which the source below shows directly. The
        // filesystem-touching behaviour itself is covered through
        // `write_entry`/`rotate_if_needed`/`last_line_matches` above,
        // which take an explicit root and never read the global state.
        assert!(!enabled(), "must default to off");
    }
}
```

- [ ] **Step 2: Run the tests to see them fail to compile**

Run: `cd daemon && cargo test -p pyren-core debuglog`
Expected: compile errors — `write_entry`, `with_ts_mut`, `rotate_if_needed`,
`last_line_matches`, `enabled` don't exist yet.

- [ ] **Step 3: Implement `debuglog.rs`**

Add the implementation above the `#[cfg(test)] mod tests` block (after the
`static ROOT: OnceLock<PathBuf> = OnceLock::new();` line):

```rust
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
    if fs::create_dir_all(SYSTEM_ROOT).is_ok() && is_writable(Path::new(SYSTEM_ROOT)) {
        return PathBuf::from(SYSTEM_ROOT);
    }
    user_root()
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
pub fn is_writable(dir: &Path) -> bool {
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
```

Note: `on_ipc` is deliberately **not** added in this task - it needs the
crate's `Request`/`Response` types and belongs with the `Registry::
dispatch` change in Task 2, so the two are reviewed together.

- [ ] **Step 4: Register the module**

In `daemon/crates/core/src/lib.rs`, add one line to the module list near
the top (currently `pub mod acpi;` … `pub mod signals;` then `mod socket;`):

```rust
pub mod debuglog;
```

(Alphabetically it sits between `pub mod client;` and `pub mod events;` -
put it there.)

- [ ] **Step 5: Run the tests to see them pass**

Run: `cd daemon && cargo test -p pyren-core debuglog`
Expected: all 8 tests pass.

- [ ] **Step 6: Commit**

```bash
git add daemon/crates/core/src/debuglog.rs daemon/crates/core/src/lib.rs
git commit -m "$(cat <<'EOF'
core: add pyren_core::debuglog, the shared debug-log writer

One append-only JSONL file per category under a resolved root
(/var/cache/pyren/depuration for the privileged daemon, ~/.cache/pyren/
depuration otherwise), 5 MB rotation, off by default. Every call site
that will use this is a single `record`/`record_if_changed` call added
in a later task - this task only adds the writer itself.
EOF
)"
```

---

## Task 2: Wire `Registry::dispatch` to the IPC transcript

**Files:**
- Modify: `daemon/crates/core/src/lib.rs:1-11` (imports), `:329-345`
  (`Registry::dispatch`)
- Modify: `daemon/crates/core/src/debuglog.rs` (add `on_ipc` and its private
  routing helper, plus tests)

**Interfaces:**
- Consumes: `Category`, `record` from Task 1.
- Produces: `pub fn on_ipc(module: &str, method: &str, params: Option<Value>, response: &crate::Response, duration: std::time::Duration)`, used by `Registry::dispatch` only.

- [ ] **Step 1: Write the failing unit tests for the routing table**

Add to `daemon/crates/core/src/debuglog.rs`'s existing `mod tests` block:

```rust
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
```

- [ ] **Step 2: Run to see it fail**

Run: `cd daemon && cargo test -p pyren-core debuglog::tests::category_for`
Expected: compile error, `category_for` does not exist.

- [ ] **Step 3: Implement `category_for` and `on_ipc`**

Append to `daemon/crates/core/src/debuglog.rs`, above the test module:

```rust
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
    record(
        Category::Ipc,
        json!({
            "module": module,
            "method": method,
            "params": params.clone(),
            "ok": ok,
            "durationMs": duration.as_millis() as u64,
            "error": error_message.clone(),
        }),
    );

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
```

- [ ] **Step 4: Run to see the routing tests pass**

Run: `cd daemon && cargo test -p pyren-core debuglog`
Expected: all pass, including the two new ones.

- [ ] **Step 5: Write the failing integration test for `dispatch`**

Add to the existing `#[cfg(test)] mod tests` block in
`daemon/crates/core/src/lib.rs` (it already has the `Stub` module and
`reply`/`kind_of` helpers - see lines 411-449):

```rust
    /// The one integration point this task adds: a real `dispatch()` call,
    /// with logging on, produces a real line in `ipc.jsonl`. `ROOT` is a
    /// `OnceLock` and can only be set once per process, so this is the
    /// only test in the crate allowed to call `debuglog::init` - every
    /// other debug-log test (in `debuglog.rs`) works against an explicit
    /// directory instead, precisely to avoid needing a second `init`.
    #[test]
    fn a_dispatched_call_is_recorded_to_the_ipc_transcript_when_enabled() {
        let dir = std::env::temp_dir()
            .join(format!("pyren-core-dispatch-debuglog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::debuglog::init(dir.clone());
        crate::debuglog::set_enabled(true);

        let mut registry = Registry::new();
        registry.register(Box::new(Stub(|| ModuleError::Failed("unused".into()))));
        registry.dispatch(Request {
            id: 1,
            module: "stub".to_string(),
            method: "ok".to_string(),
            params: Value::Null,
        });

        crate::debuglog::set_enabled(false);

        let text = std::fs::read_to_string(dir.join("ipc.jsonl")).expect("ipc.jsonl written");
        let line: Value = serde_json::from_str(text.lines().next().expect("one line")).unwrap();
        assert_eq!(line["module"], "stub");
        assert_eq!(line["method"], "ok");
        assert_eq!(line["ok"], true);
    }

    /// The other half: with the toggle off (the default), nothing is
    /// written at all - not even the directory is created.
    #[test]
    fn a_dispatched_call_writes_nothing_when_debug_logging_is_off() {
        let dir = std::env::temp_dir()
            .join(format!("pyren-core-dispatch-nodebuglog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // `debuglog::init` was already called by the previous test in this
        // binary (or will be by a later one) - `ROOT` sticks to whichever
        // directory won that race, which is fine here: this test never
        // enables logging, so nothing should be written to *any* root.
        crate::debuglog::set_enabled(false);

        let mut registry = Registry::new();
        registry.register(Box::new(Stub(|| ModuleError::Failed("unused".into()))));
        registry.dispatch(Request {
            id: 2,
            module: "stub".to_string(),
            method: "ok".to_string(),
            params: Value::Null,
        });

        assert!(!dir.join("ipc.jsonl").exists());
    }
```

- [ ] **Step 6: Run to see it fail**

Run: `cd daemon && cargo test -p pyren-core dispatch`
Expected: compiles (it links against `crate::debuglog::init` etc. from Task
1), but `a_dispatched_call_is_recorded_to_the_ipc_transcript_when_enabled`
fails - `dispatch` doesn't call `on_ipc` yet, so `ipc.jsonl` is never
created.

- [ ] **Step 7: Wire `on_ipc` into `dispatch`**

In `daemon/crates/core/src/lib.rs`, add `use std::time::Instant;` next to
the existing `use std::time::Duration;` at the top of the file, then
change `dispatch` (currently lines 329-345):

```rust
    pub fn dispatch(&self, req: Request) -> Response {
        if req.module == "core" {
            return self.dispatch_core(&req);
        }

        match self.modules.iter().find(|m| m.id() == req.module) {
            None => Response::err(
                req.id,
                ErrorKind::UnknownModule,
                format!("unknown module '{}'", req.module),
            ),
            Some(m) => match m.call(&req.method, req.params) {
                Ok(v) => Response::ok(req.id, v),
                Err(e) => Response::err_msg(req.id, e.kind(), e.into_msg()),
            },
        }
    }
```

to:

```rust
    pub fn dispatch(&self, req: Request) -> Response {
        if req.module == "core" {
            return self.dispatch_core(&req);
        }

        let started = Instant::now();
        let logged_params = debuglog::enabled().then(|| req.params.clone());
        let response = match self.modules.iter().find(|m| m.id() == req.module) {
            None => Response::err(
                req.id,
                ErrorKind::UnknownModule,
                format!("unknown module '{}'", req.module),
            ),
            Some(m) => match m.call(&req.method, req.params) {
                Ok(v) => Response::ok(req.id, v),
                Err(e) => Response::err_msg(req.id, e.kind(), e.into_msg()),
            },
        };
        debuglog::on_ipc(&req.module, &req.method, logged_params, &response, started.elapsed());
        response
    }
```

Nothing inside the `match` changed - both arms are byte-for-byte what they
were. The three new lines wrap the existing match without touching it, and
deleting them (plus the `use std::time::Instant;`) returns `dispatch` to
exactly its current shape.

- [ ] **Step 8: Run to see both tests pass**

Run: `cd daemon && cargo test -p pyren-core dispatch`
Expected: both new tests pass, and the pre-existing `dispatch`/`nextEvent`
tests in the same module are unaffected.

- [ ] **Step 9: Run the whole crate's test suite**

Run: `cd daemon && cargo test -p pyren-core`
Expected: all pass (this confirms the `on_ipc` change didn't disturb the
`core.*` tests, which take the early-return branch and never reach it).

- [ ] **Step 10: Commit**

```bash
git add daemon/crates/core/src/debuglog.rs daemon/crates/core/src/lib.rs
git commit -m "$(cat <<'EOF'
core: log every dispatched IPC call to ipc.jsonl when debug logging is on

Registry::dispatch gets one wrapping block (timing + a debuglog::on_ipc
call) around its existing match, which is otherwise untouched. Rich
methods (fan.calibrate/diagnose/probeSpeedControl, the fan cleaner,
rgb.*, installer.apply/plan/inspect) additionally get a full copy in
their own category file, since the daemon already computes that detail
to answer the call.
EOF
)"
```

---

## Task 3: The `debug` IPC module (the toggle itself)

**Files:**
- Create: `daemon/crates/core/src/debug_module.rs`
- Modify: `daemon/crates/core/Cargo.toml` (add `pyren-config` dependency)
- Modify: `daemon/crates/core/src/lib.rs` (register the module, re-export
  `DebugModule`)

**Interfaces:**
- Consumes: `pyren_config::ConfigStore` (existing crate), `crate::events::EventBus`, `crate::{Module, ModuleError, ModuleResult}` (existing), `crate::debuglog::{enabled, set_enabled}` (Task 1).
- Produces: `pub struct DebugModule` with `pub fn new() -> Self`, `pub fn with_store(store: ConfigStore) -> Self`, `pub fn publish_to(&self, events: Arc<EventBus>)`, and `impl Module for DebugModule` (`id() == "debug"`, methods `getStatus`/`setEnabled`).

- [ ] **Step 1: Add the dependency**

In `daemon/crates/core/Cargo.toml`, add under `[dependencies]` (it currently
has `serde`, `serde_json`, `thiserror`, `libc`):

```toml
pyren-config = { path = "../config" }
```

No cycle: `pyren-config`'s own `Cargo.toml` depends on nothing in this
workspace.

- [ ] **Step 2: Write the failing tests**

Create `daemon/crates/core/src/debug_module.rs`:

```rust
//! The `debug` IPC module: the one switch behind "Registros de
//! depuración", and the file it's persisted in.
//!
//! Deliberately tiny. It owns exactly one boolean - whether the files in
//! [`crate::debuglog`] get written - and nothing about what goes into
//! them; every category file is written by a one-line call from wherever
//! the thing being logged already happens. See
//! `docs/superpowers/specs/2026-09-14-debug-logging-design.md`.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use pyren_config::ConfigStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::events::EventBus;
use crate::{debuglog, Module, ModuleError, ModuleResult};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase", default)]
struct DebugConfig {
    enabled: bool,
}

#[derive(Clone)]
pub struct DebugModule {
    store: ConfigStore,
    events: Arc<OnceLock<Arc<EventBus>>>,
    daemon_dir: PathBuf,
    user_dir: PathBuf,
}

impl DebugModule {
    /// Loads the persisted setting and puts `pyren_core::debuglog` in the
    /// same state, so the two never disagree about whether logging is on.
    pub fn new() -> Self {
        Self::with_store(ConfigStore::system())
    }

    pub fn with_store(store: ConfigStore) -> Self {
        let loaded = store.load::<DebugConfig>("debug");
        debuglog::set_enabled(loaded.value.enabled);
        Self {
            store,
            events: Arc::new(OnceLock::new()),
            daemon_dir: debuglog::daemon_root(),
            user_dir: debuglog::user_root(),
        }
    }

    /// Hands the module the bus to announce `debug.changed` on. Called
    /// once, by the daemon binary, after the registry exists - same shape
    /// as `PowerModule::publish_to`/`FanModule::publish_to`.
    pub fn publish_to(&self, events: Arc<EventBus>) {
        let _ = self.events.set(events);
    }

    fn status(&self) -> Value {
        json!({
            "enabled": debuglog::enabled(),
            "daemonDir": self.daemon_dir.display().to_string(),
            "daemonDirWritable": debuglog::is_writable(&self.daemon_dir),
            "userDir": self.user_dir.display().to_string(),
        })
    }
}

impl Default for DebugModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for DebugModule {
    fn id(&self) -> &'static str {
        "debug"
    }

    fn is_supported(&self) -> bool {
        true
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getStatus" => Ok(self.status()),
            "setEnabled" => {
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| ModuleError::InvalidParams("enabled must be a bool".into()))?;
                debuglog::set_enabled(enabled);
                let _ = self.store.save("debug", &DebugConfig { enabled });
                if let Some(bus) = self.events.get() {
                    bus.publish("debug.changed", json!({ "enabled": enabled }));
                }
                Ok(self.status())
            }
            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn store(tag: &str) -> ConfigStore {
        let root = std::env::temp_dir()
            .join(format!("pyren-debug-module-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        ConfigStore::at(root)
    }

    #[test]
    fn a_fresh_module_starts_disabled() {
        let module = DebugModule::with_store(store("fresh"));
        let status = module.status();
        assert_eq!(status["enabled"], false);
    }

    #[test]
    fn set_enabled_flips_the_flag_and_persists_it() {
        let store = store("persist");
        let module = DebugModule::with_store(store.clone());

        let result = module.call("setEnabled", json!({ "enabled": true })).unwrap();
        assert_eq!(result["enabled"], true);
        assert!(debuglog::enabled());

        // A fresh module reading the same store picks up the persisted value.
        let reloaded = DebugModule::with_store(store);
        assert_eq!(reloaded.status()["enabled"], true);

        // Leave global state as found for whichever test runs next in this
        // binary.
        debuglog::set_enabled(false);
    }

    #[test]
    fn set_enabled_without_a_bool_is_invalid_params() {
        let module = DebugModule::with_store(store("bad-params"));
        let err = module.call("setEnabled", json!({})).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::InvalidParams);
    }

    #[test]
    fn an_unknown_method_is_refused() {
        let module = DebugModule::with_store(store("unknown-method"));
        let err = module.call("nope", Value::Null).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::UnknownMethod);
    }

    #[test]
    fn set_enabled_publishes_debug_changed_once_wired_to_a_bus() {
        let module = DebugModule::with_store(store("publish"));
        let events = Arc::new(EventBus::new());
        module.publish_to(Arc::clone(&events));

        module.call("setEnabled", json!({ "enabled": true })).unwrap();

        let batch = events.read_since(0, std::time::Duration::from_millis(0));
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].topic, "debug.changed");
        assert_eq!(batch.events[0].payload["enabled"], true);

        debuglog::set_enabled(false);
    }
}
```

- [ ] **Step 3: Run to see it fail**

Run: `cd daemon && cargo test -p pyren-core debug_module`
Expected: compile error - `debug_module` isn't declared as a module of the
crate yet.

- [ ] **Step 4: Register the module and re-export `DebugModule`**

In `daemon/crates/core/src/lib.rs`, add near the other `pub mod` lines:

```rust
pub mod debug_module;
```

and add to the existing re-export line (currently
`pub use events::{Batch, Event, EventBus};`), as its own line right after
it:

```rust
pub use debug_module::DebugModule;
```

- [ ] **Step 5: Run to see it pass**

Run: `cd daemon && cargo test -p pyren-core debug_module`
Expected: all 5 tests pass.

- [ ] **Step 6: Run the whole workspace to catch any dependency-graph issue**

Run: `cd daemon && cargo build --workspace`
Expected: builds clean (confirms the new `pyren-config` dependency on
`pyren-core` doesn't create a cycle anywhere else in the graph).

- [ ] **Step 7: Commit**

```bash
git add daemon/crates/core/Cargo.toml daemon/crates/core/src/debug_module.rs daemon/crates/core/src/lib.rs
git commit -m "$(cat <<'EOF'
core: add the debug IPC module (debug.getStatus / debug.setEnabled)

The one switch behind "Registros de depuración": persisted in
debug.json via the same ConfigStore every other module uses, mirrored
into pyren_core::debuglog's in-memory flag, and announced as
debug.changed on the EventBus so every connected process picks up a
toggle live.
EOF
)"
```

---

## Task 4: Wire the daemon binary

**Files:**
- Modify: `daemon/daemon/src/main.rs`

**Interfaces:**
- Consumes: `pyren_core::debuglog::{init, daemon_root, record, record_if_changed, Category}`, `pyren_core::DebugModule` (Tasks 1-3), and the already-in-scope `fan`, `system`, `identity`, `events`, `registry`, `on_termination` locals.

This task has no automated test: `main.rs` itself has none today (its
existing wiring - the fan/power `EventBus` subscription, the termination
handler - is exercised by running the daemon, not by a test file). This
plan follows that convention rather than inventing a new one; Step 6 below
is a manual verification checklist instead.

- [ ] **Step 1: Initialize the debug-log root**

Near the top of `fn main()`, right after the existing
`pyren_core::signals::block_termination();` line, add:

```rust
    pyren_core::debuglog::init(pyren_core::debuglog::daemon_root());
```

(One line. `daemon_root()` decides `/var/cache/pyren/depuration` vs the
`~/.cache` fallback; nothing else in this block changes.)

- [ ] **Step 2: Construct and wire the `debug` module**

Find where the other modules are constructed (`let fan = FanModule::new();`
… `let system = SystemModule::new(controls);`). Add, alongside them (order
doesn't matter - it depends on nothing else):

```rust
    let debug = pyren_core::DebugModule::new();
```

Then, right beside the existing:

```rust
    let events = Arc::clone(registry.events());
```

add:

```rust
    debug.publish_to(Arc::clone(&events));
```

And alongside the other `registry.register(...)` calls:

```rust
    registry.register(Box::new(debug));
```

- [ ] **Step 3: Log driver/kernel identity at startup, only on change**

Right after the existing block that prints the identity summary (the one
ending with the `perf_events` note - look for
`if !privileges.perf_events {`), add:

```rust
    pyren_core::debuglog::record_if_changed(
        pyren_core::debuglog::Category::DriverKernel,
        serde_json::json!({
            "vendor": identity.vendor,
            "model": identity.model,
            "boardName": identity.board_name,
            "boardVendor": identity.board_vendor,
            "biosVersion": identity.bios_version,
            "kernel": identity.kernel,
            "cpu": identity.cpu,
            "gpus": identity.gpus,
            "driverInstalled": fan.is_supported(),
        }),
    );
```

This runs once per daemon start, and writes a line only when it differs
from the last one already on disk - a normal restart with nothing changed
adds nothing. (Deliberately not repeated after `installer.apply`: every
driver-changing install action already requires a daemon restart to take
effect, so the next startup's compare-and-log catches it - see the design
spec's "Driver/kernel identity" section for why a second hook here would
either fire too early or need machinery this daemon doesn't have.)

- [ ] **Step 4: Log the combined power+fan history from the `EventBus`**

Right after the existing subscriber that wires `power.mode` into
`fan.set_active_profile` (the block starting `{ let fan = fan.clone();
events.subscribe(...) }`, just above `fan.set_active_profile(power.mode
().as_str());`), add a second, independent subscriber:

```rust
    events.subscribe(|topic, payload| {
        if matches!(topic, "power.mode" | "fan.mode" | "fan.floorRaised") {
            pyren_core::debuglog::record(
                pyren_core::debuglog::Category::Performance,
                serde_json::json!({ "topic": topic, "payload": payload }),
            );
        }
    });
```

This is a whole, independent `events.subscribe(...)` call - deleting it
removes the performance history with no effect on the fan/power
coordination subscriber next to it.

- [ ] **Step 5: Log daemon startup and shutdown**

Right after the `for cap in registry.capabilities() { ... }` loop (still
before `let socket_path = socket_path();`), add:

```rust
    pyren_core::debuglog::record(
        pyren_core::debuglog::Category::Daemon,
        serde_json::json!({
            "event": "startup",
            "version": env!("CARGO_PKG_VERSION"),
            "privileged": is_root(),
        }),
    );
```

Then, inside the existing `pyren_core::signals::on_termination(move |signal|
{ ... })` closure, right after its `log_info!(...)` call and before
`rgb.on_exit(stopping);`, add:

```rust
        pyren_core::debuglog::record(
            pyren_core::debuglog::Category::Daemon,
            serde_json::json!({
                "event": "shutdown",
                "signal": pyren_core::signals::name(signal),
                "systemStopping": stopping,
            }),
        );
```

- [ ] **Step 6: Build, then verify manually**

Run: `cd daemon && cargo build --workspace`
Expected: builds clean.

Manual verification (no daemon test harness exists for `main.rs` today):

```bash
export PYREN_DEPURATION_DIR=/tmp/pyren-depuration-check
export PYREN_SOCKET=/tmp/pyren-daemon.sock
rm -rf "$PYREN_DEPURATION_DIR"
cargo run -p pyren-daemon &
sleep 1
# Turn logging on:
printf '{"id":1,"module":"debug","method":"setEnabled","params":{"enabled":true}}\n' \
  | socat - UNIX-CONNECT:/tmp/pyren-daemon.sock
# Drive a couple of things:
printf '{"id":2,"module":"power","method":"getState","params":null}\n' \
  | socat - UNIX-CONNECT:/tmp/pyren-daemon.sock
ls "$PYREN_DEPURATION_DIR"
cat "$PYREN_DEPURATION_DIR/daemon.jsonl"       # one "startup" line
cat "$PYREN_DEPURATION_DIR/driver-kernel.jsonl" # one identity line
cat "$PYREN_DEPURATION_DIR/ipc.jsonl"           # debug.setEnabled + power.getState
kill %1
cat "$PYREN_DEPURATION_DIR/daemon.jsonl"        # now also a "shutdown" line
```

Expected: `debug.json` at `$PYREN_CONFIG_DIR` (or the default) reads
`{"version":1,"enabled":true}`; `ipc.jsonl` has one line per call above
(and none for `core.*` if you also try `core.capabilities`); restarting the
daemon a second time with nothing changed does **not** add a second line to
`driver-kernel.jsonl`.

- [ ] **Step 7: Commit**

```bash
git add daemon/daemon/src/main.rs
git commit -m "$(cat <<'EOF'
daemon: wire debug logging into the daemon binary

Registers the debug module, initializes pyren_core::debuglog's root,
and adds five independent one-line/one-block hooks: driver/kernel
identity at startup (only on change), the combined power+fan history
via a second EventBus subscriber, and daemon startup/shutdown. Every
hook is additive next to existing wiring, not woven into it.
EOF
)"
```

---

## Task 5: Document the `debug` module in the IPC protocol

**Files:**
- Modify: `docs/01-ipc-protocol.md`

**Interfaces:** None (documentation only).

- [ ] **Step 1: Add the `debug` module section**

Add a new section after the `## installer module` section (or after
whichever module section precedes it in the file - keep the existing
module ordering) with:

```markdown
## `debug` module

The one switch behind the app's "Registros de depuración" setting. See
`docs/superpowers/specs/2026-09-14-debug-logging-design.md` for the full
design of what gets written and where.

| method | params | result |
|---|---|---|
| `debug.getStatus` | none | `{ "enabled": bool, "daemonDir": string, "daemonDirWritable": bool, "userDir": string }` |
| `debug.setEnabled` | `{ "enabled": bool }` | same shape as `getStatus` |

`daemonDir` is where the daemon itself writes (`/var/cache/pyren/
depuration` when it can, `~/.cache/pyren/depuration` when it's running
unprivileged); `userDir` is always `~/.cache/pyren/depuration`, the
directory the OSD widget and the desktop app write to directly, since both
already run as the desktop user. `daemonDirWritable: false` means the
switch is on but the daemon has nowhere to write - the UI should say so
rather than imply the files exist.

Enabling or disabling this publishes `debug.changed` (see the event topics
table above): `{ "enabled": bool }`.
```

- [ ] **Step 2: Add the `debug.changed` row to the topics table**

Find the topics table (the one with `hotkey.pressed`, `power.mode`,
`power.overridden`, `fan.mode`, `fan.floorRaised`) and add a row:

```markdown
| `debug.changed` | `debug.setEnabled` took effect | `{ enabled }` |
```

- [ ] **Step 3: Commit**

```bash
git add docs/01-ipc-protocol.md
git commit -m "docs: document the debug module and debug.changed event"
```

---

## Task 6: OSD widget logging

**Files:**
- Modify: `osd/src/main.rs`
- Modify: `osd/src/daemon.rs`

**Interfaces:**
- Consumes: `pyren_core::debuglog::{init, user_root, set_enabled, record, Category}` (already available - `osd` already depends on `pyren-core`), `pyren_core::client::call` (existing).

No automated test: `osd` has no test suite today.

- [ ] **Step 1: Initialize the user-cache root**

In `osd/src/main.rs`, at the very top of `fn main() -> glib::ExitCode {`,
before the existing `let mut show_now = false;`, add:

```rust
    pyren_core::debuglog::init(pyren_core::debuglog::user_root());
```

- [ ] **Step 2: Fetch the toggle once at connect, and track live changes**

In `osd/src/daemon.rs`, at the top of `fn poll_until_closed`, before the
existing `let mut since: Option<u64> = None;`, add:

```rust
    if let Ok(status) = client::call("debug", "getStatus", Value::Null) {
        if let Some(enabled) = status.get("enabled").and_then(Value::as_bool) {
            pyren_core::debuglog::set_enabled(enabled);
        }
    }
```

Then, inside the existing `for event in reply.get("events")...` loop
(right before the existing `if let Some(message) = interpret(event) {`
line), add:

```rust
                    if event.get("topic").and_then(Value::as_str) == Some("debug.changed") {
                        if let Some(enabled) = event
                            .get("payload")
                            .and_then(|p| p.get("enabled"))
                            .and_then(Value::as_bool)
                        {
                            pyren_core::debuglog::set_enabled(enabled);
                        }
                    }
```

Both blocks are self-contained `if`s that only ever call
`debuglog::set_enabled` - deleting either has no effect on the surrounding
poll loop.

- [ ] **Step 3: Log every message the widget acts on**

In `osd/src/main.rs`, inside the `glib::spawn_future_local(async move { ...
})` block, right after `while let Ok(message) = receiver.recv().await {`
and before the existing `match message { ... }`, add:

```rust
                pyren_core::debuglog::record(
                    pyren_core::debuglog::Category::Widget,
                    serde_json::json!({ "message": format!("{message:?}") }),
                );
```

(`Message` already derives `Debug` - see `osd/src/daemon.rs`'s `#[derive
(Debug, Clone)] pub enum Message`. No new mapping function needed.) The
`match` immediately below is untouched.

- [ ] **Step 4: Build and verify manually**

Run: `cd osd && cargo build`
Expected: builds clean.

Manual verification:

```bash
export PYREN_DEPURATION_DIR=/tmp/pyren-depuration-check
# with the daemon from Task 4's manual check still running and logging on:
cargo run -p pyren-osd -- --show &
sleep 1
cat "$PYREN_DEPURATION_DIR/widget.jsonl"   # at least one Show/Mode/FanState line
```

- [ ] **Step 5: Commit**

```bash
git add osd/src/main.rs osd/src/daemon.rs
git commit -m "$(cat <<'EOF'
osd: log widget messages when debug logging is on

Three additive hooks: the debug-log root is initialized as the user's
own ~/.cache (the widget already runs as the desktop user), the toggle
is fetched once at connect and kept live via debug.changed, and every
Message the widget acts on is recorded at the one place they already
all cross into the GTK thread.
EOF
)"
```

---

## Task 7: Tauri backend — passthrough commands and frontend logging

**Files:**
- Modify: `app/src-tauri/Cargo.toml`
- Modify: `app/src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: `pyren_core::debuglog::{init, user_root, record, Category}` (new dependency), the existing `call_daemon` helper.
- Produces: Tauri commands `debug_get_status() -> Result<Value, String>`, `debug_set_enabled(enabled: bool) -> Result<Value, String>`, `debug_log_frontend(category: String, entry: Value) -> Result<(), String>`.

No automated test: `app/src-tauri` has no test suite today.

- [ ] **Step 1: Add the dependency**

In `app/src-tauri/Cargo.toml`, alongside the existing
`pyren-config = { path = "../../daemon/crates/config" }`, add:

```toml
pyren-core = { path = "../../daemon/crates/core" }
```

- [ ] **Step 2: Initialize the user-cache root at startup**

In `app/src-tauri/src/lib.rs`, inside the `.setup(|app| { ... })` closure,
right before the existing `watch_daemon_events(app.handle().clone());`,
add:

```rust
            pyren_core::debuglog::init(pyren_core::debuglog::user_root());
```

- [ ] **Step 3: Add the passthrough commands**

Alongside the other `#[tauri::command(async)] fn ..._get_status`/`..._set_
...` functions (e.g. right after `fan_get_status`), add:

```rust
#[tauri::command(async)]
fn debug_get_status() -> Result<Value, String> {
    call_daemon("debug", "getStatus", Value::Null)
}

#[tauri::command(async)]
fn debug_set_enabled(enabled: bool) -> Result<Value, String> {
    call_daemon("debug", "setEnabled", json!({ "enabled": enabled }))
}

/// Writes one line to `frontend.jsonl` from the webview - uncaught errors
/// and a short list of user-action breadcrumbs (see `$lib/api/debug.ts`).
/// Does not go through the daemon: this file is only ever the app's own,
/// under the user's `~/.cache/pyren/depuration`.
#[tauri::command(async)]
fn debug_log_frontend(category: String, entry: Value) -> Result<(), String> {
    let mut payload = entry;
    if let Value::Object(map) = &mut payload {
        map.insert("category".to_string(), Value::String(category));
    }
    pyren_core::debuglog::record(pyren_core::debuglog::Category::Frontend, payload);
    Ok(())
}
```

- [ ] **Step 4: Register the three commands**

In the `tauri::generate_handler![...]` list, add them alongside the other
entries (e.g. right after `session_status,`):

```rust
            debug_get_status,
            debug_set_enabled,
            debug_log_frontend,
```

- [ ] **Step 5: Build and verify manually**

Run: `cd app/src-tauri && cargo build`
Expected: builds clean.

- [ ] **Step 6: Commit**

```bash
git add app/src-tauri/Cargo.toml app/src-tauri/src/lib.rs
git commit -m "$(cat <<'EOF'
app: add Tauri commands for the debug-log toggle and frontend logging

debug_get_status/debug_set_enabled forward to the daemon like every
other command here; debug_log_frontend writes straight to the app's
own ~/.cache/pyren/depuration/frontend.jsonl via the same
pyren_core::debuglog writer the daemon and the OSD use.
EOF
)"
```

---

## Task 8: Frontend API wrapper and error capture

**Files:**
- Create: `app/src/lib/api/debug.ts`
- Modify: `app/src/routes/+layout.svelte`

**Interfaces:**
- Consumes: `invoke` (`@tauri-apps/api/core`), `inTauri` (`$lib/api/daemon`).
- Produces:
  - `export type DebugLogStatus = { enabled: boolean; daemonDir: string; daemonDirWritable: boolean; userDir: string }`
  - `export const debugLog = { status, setEnabled, available, installErrorCapture }`

No automated test: this project has no frontend test runner configured
today (no vitest/jest in `app/package.json`).

- [ ] **Step 1: Write `app/src/lib/api/debug.ts`**

```typescript
/**
 * The debug-logging toggle ("Registros de depuración") and frontend error
 * capture, mirroring the shape of `$lib/api/admin.ts`.
 *
 * `status`/`setEnabled` forward to the daemon's `debug` module (see
 * docs/01-ipc-protocol.md). `installErrorCapture` is local to this
 * process: it writes straight to `~/.cache/pyren/depuration/frontend.jsonl`
 * through the Tauri shell and never touches the daemon.
 */

import { invoke } from "@tauri-apps/api/core";
import { inTauri } from "./daemon";

export type DebugLogStatus = {
  enabled: boolean;
  daemonDir: string;
  daemonDirWritable: boolean;
  userDir: string;
};

export const debugLog = {
  status: () => invoke<DebugLogStatus>("debug_get_status"),
  setEnabled: (enabled: boolean) => invoke<DebugLogStatus>("debug_set_enabled", { enabled }),
  /** False in a plain browser tab, where there is no shell to ask. */
  available: () => inTauri,

  /**
   * Attaches `window.onerror`/`unhandledrejection` handlers that forward
   * to `frontend.jsonl`. A no-op outside Tauri. Returns the function that
   * removes the handlers, same shape as `onDaemonEvent`.
   *
   * Every call is fire-and-forget: a failed write here must never itself
   * throw, or an error handler would become a new source of errors.
   */
  installErrorCapture(): () => void {
    if (!inTauri) return () => {};

    const onError = (event: ErrorEvent) => {
      void invoke("debug_log_frontend", {
        category: "error",
        entry: { message: event.message, source: event.filename, line: event.lineno },
      }).catch(() => {});
    };
    const onRejection = (event: PromiseRejectionEvent) => {
      void invoke("debug_log_frontend", {
        category: "error",
        entry: { message: String(event.reason) },
      }).catch(() => {});
    };

    window.addEventListener("error", onError);
    window.addEventListener("unhandledrejection", onRejection);
    return () => {
      window.removeEventListener("error", onError);
      window.removeEventListener("unhandledrejection", onRejection);
    };
  },

  /** A short, explicit breadcrumb - not every click. Call this from the
   *  handful of places worth remembering when reading a bug report. */
  action(name: string, detail?: Record<string, unknown>) {
    if (!inTauri) return;
    void invoke("debug_log_frontend", { category: "action", entry: { name, ...detail } }).catch(
      () => {},
    );
  },
};
```

- [ ] **Step 2: Wire error capture into the root layout**

In `app/src/routes/+layout.svelte`, add the import alongside the others:

```typescript
  import { debugLog } from "$lib/api/debug";
```

Then, inside the existing `onMount(() => { ... return () => { ... }; })`
block, add one line right after `const stopNotifications =
notifications.start();`:

```typescript
    const stopErrorCapture = debugLog.installErrorCapture();
```

and add its cleanup to the existing returned function, alongside
`telemetry.stop();`:

```typescript
      stopErrorCapture();
```

- [ ] **Step 3: Verify manually**

Run: `cd app && bun run dev` (or the project's existing dev command), then
`bun run tauri dev` if that's how this project runs the Tauri shell locally
(check `app/README.md`/`package.json` scripts - do not guess a command that
isn't already defined there).

With the daemon's debug logging on, open the app, trigger a UI error (e.g.
temporarily throw in a `$effect` and revert), and confirm a line appears in
`~/.cache/pyren/depuration/frontend.jsonl`.

- [ ] **Step 4: Commit**

```bash
git add app/src/lib/api/debug.ts app/src/routes/+layout.svelte
git commit -m "$(cat <<'EOF'
app: add debug.ts API wrapper and frontend error capture

Mirrors admin.ts's shape. installErrorCapture() attaches window-level
error/rejection handlers that forward to frontend.jsonl through the
Tauri shell; wired once in the root layout, alongside the other
onMount subscriptions, with its own cleanup.
EOF
)"
```

---

## Task 9: Settings UI

**Files:**
- Modify: `app/src/routes/settings/+page.svelte`
- Modify: `app/src/lib/i18n/locales/en.json`
- Modify: `app/src/lib/i18n/locales/es.json`

**Interfaces:**
- Consumes: `debugLog` (Task 8), `onDaemonEvent` (`$lib/api/daemon`, existing), `revealItemInDir` (`@tauri-apps/plugin-opener`, already installed and already permitted via the `opener:default` capability - no `capabilities/default.json` change needed).

- [ ] **Step 1: Add state and lifecycle wiring**

In `app/src/routes/settings/+page.svelte`'s `<script>` block, add the
imports alongside the existing ones:

```typescript
  import { debugLog, type DebugLogStatus } from "$lib/api/debug";
  import { revealItemInDir } from "@tauri-apps/plugin-opener";
```

and extend the existing `$lib/api/daemon` import (currently `import {
daemon, errorText, type FanSensorFailureAction, type HotkeyStatus } from
"$lib/api/daemon";`) to also pull in `onDaemonEvent`:

```typescript
  import {
    daemon,
    errorText,
    onDaemonEvent,
    type FanSensorFailureAction,
    type HotkeyStatus,
  } from "$lib/api/daemon";
```

Add state next to the existing `privileges`/`elevating` declarations:

```typescript
  /** The debug-logging toggle, as the daemon has it. */
  let debugStatus = $state<DebugLogStatus | null>(null);
  let debugStatusError = $state<string | null>(null);
```

Add a refresh function next to `refreshPrivileges`:

```typescript
  async function refreshDebugStatus() {
    try {
      debugStatus = await debugLog.status();
      debugStatusError = null;
    } catch (e) {
      debugStatus = null;
      debugStatusError = errorText(e);
    }
  }

  async function setDebugLogging(enabled: boolean) {
    try {
      debugStatus = await debugLog.setEnabled(enabled);
      debugStatusError = null;
    } catch (e) {
      debugStatusError = errorText(e);
    }
  }
```

In the existing `onMount(() => { ... })` block, add the initial fetch next
to `void refreshHotkey();`:

```typescript
    void refreshDebugStatus();
    const stopDebugWatch = onDaemonEvent((event) => {
      if (event.topic === "debug.changed") void refreshDebugStatus();
    });
    return stopDebugWatch;
```

(This `onMount` currently has an early `if (!session.available()) return;`
guard and no return value otherwise - adding a `return stopDebugWatch;` as
the block's final statement is consistent with Svelte's `onMount` cleanup
convention and does not change what runs when `session.available()` is
`false`, since that branch already returns before reaching this point.)

- [ ] **Step 2: Add the Settings panel**

Add a new `<Panel>` in the markup, after the existing fan-control panel
(the one gated on `hardware.fan?.capabilities.switchMode`) so it always
renders regardless of hardware:

```svelte
  <Panel title={t("settings.debugLogs")}>
    <div class="row">
      <span>
        {t("settings.debugLogsEnable")}
        <small class="hint-inline"><RichText text={t("settings.debugLogsEnableHint")} /></small>
      </span>
      <Toggle
        checked={debugStatus?.enabled ?? false}
        onchange={(v) => void setDebugLogging(v)}
        ariaLabel={t("settings.debugLogsEnable")}
      />
    </div>
    {#if debugStatusError}
      <p class="notice warn">{debugStatusError}</p>
    {:else if debugStatus && !debugStatus.daemonDirWritable}
      <p class="notice warn">
        {t("settings.debugLogsNotWritable", { path: debugStatus.daemonDir })}
      </p>
    {/if}
    {#if debugStatus}
      <div class="row">
        <span>{t("settings.debugLogsFolder")}</span>
        <button
          type="button"
          class="btn-secondary"
          onclick={() => void revealItemInDir(debugStatus!.userDir)}
        >
          {t("settings.debugLogsOpenFolder")}
        </button>
      </div>
    {/if}
  </Panel>
```

(Match the exact class names `btn-secondary`/`notice warn` to whatever this
file already uses elsewhere - grep the file for `class="btn-secondary"` and
`class="notice warn"` before finalizing; both already appear in this same
file per the admin/privileges section above, so no new CSS is needed.)

- [ ] **Step 3: Add i18n keys**

In `app/src/lib/i18n/locales/en.json`, inside the `"settings"` object, add:

```json
    "debugLogs": "Debug logs",
    "debugLogsEnable": "Debug logging",
    "debugLogsEnableHint": "Keeps a detailed history of driver/kernel info, power and fan mode changes, lighting commands, calibration runs, the fan cleaner, driver installs and the daemon's own activity, in plain text files under your cache folder. Off by default so it costs nothing when you don't need it.",
    "debugLogsNotWritable": "The switch is on, but {path} isn't writable, so nothing is being recorded there yet.",
    "debugLogsFolder": "Log files",
    "debugLogsOpenFolder": "Open logs folder",
```

In `app/src/lib/i18n/locales/es.json`, inside the `"settings"` object, add:

```json
    "debugLogs": "Registros de depuración",
    "debugLogsEnable": "Registros de depuración",
    "debugLogsEnableHint": "Guarda un historial detallado de la información de driver/kernel, cambios de modo de rendimiento y ventiladores, comandos de iluminación, calibraciones, el modo de limpieza inverso, instalaciones del driver y la actividad del propio daemon, en archivos de texto plano dentro de tu carpeta de caché. Apagado por defecto, así no consume nada si no lo necesitas.",
    "debugLogsNotWritable": "El interruptor está activado, pero {path} no se puede escribir, así que de momento no se está guardando nada ahí.",
    "debugLogsFolder": "Archivos de registro",
    "debugLogsOpenFolder": "Abrir carpeta de registros",
```

- [ ] **Step 4: Verify manually**

Run the app (`bun run tauri dev` or this project's equivalent), open
Settings, toggle "Registros de depuración" on, confirm `debug.json` and the
JSONL files appear under `~/.cache/pyren/depuration`, and that "Abrir
carpeta de registros" opens the file manager there.

- [ ] **Step 5: Commit**

```bash
git add app/src/routes/settings/+page.svelte app/src/lib/i18n/locales/en.json app/src/lib/i18n/locales/es.json
git commit -m "$(cat <<'EOF'
app: add the "Registros de depuración" panel to Settings

One toggle wired to debug.getStatus/setEnabled, live-refreshed on
debug.changed, a not-writable notice when the daemon has nowhere to
write, and an "open logs folder" button via the already-installed
opener plugin.
EOF
)"
```

---

## Task 10: Changelog and final smoke test

**Files:**
- Modify: `CHANGELOG.md`

**Interfaces:** None.

- [ ] **Step 1: Add an Unreleased entry**

In `CHANGELOG.md`, under `## [Unreleased]` → `### Added`, add a bullet
matching the style and level of detail of the existing entries there:

```markdown
- **Opt-in debug logging.** A new "Registros de depuración" switch in
  Settings (`debug.getStatus`/`debug.setEnabled`) keeps a rolling,
  human-readable history under `~/.cache/pyren/depuration` (or
  `/var/cache/pyren/depuration` for the installed daemon): driver/kernel
  identity whenever it changes, combined power+fan mode history, every
  RGB command sent, full calibration and diagnostic runs, the fan
  cleaner, driver installs, the daemon's own startup/shutdown, the full
  IPC transcript, and what the OSD widget and the app itself did. Off by
  default; five 5 MB files per category, one rotation generation each.
```

- [ ] **Step 2: Full workspace build and test run**

Run: `cd daemon && cargo build --workspace && cargo test --workspace`
Expected: everything from Tasks 1-6 passes.

Run: `cd osd && cargo build`
Run: `cd app/src-tauri && cargo build`
Expected: both build clean.

- [ ] **Step 3: End-to-end manual smoke test**

Repeat the manual checks from Tasks 4, 6 and 9 in one sitting, with the
daemon, the OSD, and the app all running against the same
`PYREN_DEPURATION_DIR`/`~/.cache/pyren/depuration`, and confirm:

- Toggling the switch in the app's Settings turns logging on for the
  daemon *and* is reflected by the OSD (check `widget.jsonl` starts
  filling in without restarting `pyren-osd`).
- Turning it back off stops new lines from appearing anywhere (existing
  files are left alone - nothing deletes them on disable, only the
  toggle itself changes).
- Restarting the daemon with nothing about the machine changed does not
  add a duplicate line to `driver-kernel.jsonl`.

- [ ] **Step 4: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs: add changelog entry for the debug logging system"
```
