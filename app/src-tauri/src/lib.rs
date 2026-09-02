//! Tauri commands are the only bridge the frontend has to the outside
//! world. This process runs unprivileged; it never touches hardware
//! directly - every module call is forwarded to pyren-daemon over its
//! Unix domain socket. See docs/01-ipc-protocol.md at the repo root for
//! the wire format.

mod admin;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use pyren_config::{ConfigStore, LoadOutcome};
use serde_json::{json, Map, Value};
use tauri::Manager;

/// Config namespaces the frontend is allowed to read and write.
///
/// An allowlist rather than a free-form name: the namespace becomes a
/// filename, and nothing in the webview should be able to choose arbitrary
/// paths. (`ConfigStore` sanitises names too - this is the first line.)
const APP_CONFIG_NAMESPACES: &[&str] = &["app", "ui"];

/// Where the daemon might be, most-likely first.
///
/// There are two, and assuming either one alone is a bug: the installed
/// systemd unit listens on `/run/pyren/daemon.sock`, while an unprivileged
/// `cargo run` falls back to `/tmp/pyren-daemon.sock`. A client that knew
/// only the second would never find a properly installed daemon - which is
/// every real installation.
///
/// Keep in step with `daemon/crates/core/src/client.rs`, which resolves the
/// same two for `pyren-ctl`.
const SOCKET_CANDIDATES: &[&str] = &["/run/pyren/daemon.sock", "/tmp/pyren-daemon.sock"];

fn socket_candidates() -> Vec<String> {
    match std::env::var("PYREN_SOCKET") {
        // An explicit setting is a decision, not a hint: don't second-guess it.
        Ok(path) => vec![path],
        Err(_) => SOCKET_CANDIDATES.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// The socket to name in the UI: the one that answers, else the one that at
/// least exists, else the first we would try.
pub(crate) fn socket_path() -> String {
    let candidates = socket_candidates();
    candidates
        .iter()
        .find(|path| UnixStream::connect(path).is_ok())
        .or_else(|| candidates.iter().find(|path| std::path::Path::new(path).exists()))
        .cloned()
        .unwrap_or_else(|| candidates[0].clone())
}

/// Opens the first socket that answers.
fn connect_daemon() -> Result<UnixStream, String> {
    let candidates = socket_candidates();
    let mut failure: Option<(String, std::io::Error)> = None;

    for path in &candidates {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                // "You are not in the group" is a far more useful thing to
                // report than "no such file" from the path we tried next,
                // so it wins whatever order the failures arrive in.
                let more_useful = e.kind() == std::io::ErrorKind::PermissionDenied
                    || failure.as_ref().is_none_or(|(_, previous)| {
                        previous.kind() != std::io::ErrorKind::PermissionDenied
                    });
                if more_useful {
                    failure = Some((path.clone(), e));
                }
            }
        }
    }

    let (path, error) = failure.expect("there is always at least one candidate");
    Err(connect_error(&path, error))
}

/// Sends one JSON-RPC-ish request to pyren-daemon and returns its
/// `result`, or an `Err` built from the connection failure or the
/// daemon's own `error` field.
fn call_daemon(module: &str, method: &str, params: Value) -> Result<Value, String> {
    let stream = connect_daemon()?;

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let request = json!({ "id": 1, "module": module, "method": method, "params": params });
    let mut line = request.to_string();
    line.push('\n');
    writer.write_all(line.as_bytes()).map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).map_err(|e| e.to_string())?;

    let response: Value = serde_json::from_str(&response_line).map_err(|e| e.to_string())?;
    if let Some(err) = response.get("error") {
        return Err(daemon_error(err));
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

/// The daemon answers a refusal with `{ kind, message }` - `kind` being a
/// closed set the UI can branch on, `message` being prose for a person.
///
/// Only the message crosses into the frontend today, because a Tauri
/// command's error is a string and nothing on the other side branches yet.
/// The kind is where admin mode should look when it does: `permissionDenied`
/// is a daemon running unprivileged, and `notCapable` is hardware that will
/// never do it however it is asked - offering to elevate for the second is
/// the mistake this field exists to prevent.
///
/// A bare string is a daemon older than that format, and is passed through
/// rather than dropped: reading an unparseable error as *absent* would turn
/// a refusal into a silent success.
fn daemon_error(error: &Value) -> String {
    match error {
        Value::String(message) => message.clone(),
        _ => error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the daemon refused without saying why")
            .to_string(),
    }
}

/// The daemon's socket only admits root and members of the `pyren`
/// group (see `daemon/crates/core/src/socket.rs`), so "permission denied"
/// here is not a broken install - it is a user who has not been added to
/// the group yet, and saying so is the whole difference between a
/// two-minute fix and a bug report.
fn connect_error(path: &str, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return format!(
            "not allowed to reach pyren-daemon at {path}. \
             Add this user to the 'pyren' group \
             (sudo usermod -aG pyren $USER), then log out and back in."
        );
    }
    format!("cannot reach pyren-daemon at {path}: {e}")
}

#[tauri::command]
fn fan_get_status() -> Result<Value, String> {
    call_daemon("fan", "getStatus", Value::Null)
}

/// `pwm` is only meaningful for `manual`; the daemon ignores it otherwise
/// and refuses a mode this machine's driver cannot do, rather than
/// pretending it worked.
#[tauri::command]
fn fan_set_mode(mode: String, pwm: Option<u8>) -> Result<Value, String> {
    call_daemon("fan", "setMode", json!({ "mode": mode, "pwm": pwm }))
}

#[tauri::command]
fn fan_set_curve(curve: Value, interpolation: Option<String>) -> Result<Value, String> {
    call_daemon("fan", "setCurve", json!({ "curve": curve, "interpolation": interpolation }))
}

#[tauri::command]
fn fan_set_restore_on_start(enabled: bool) -> Result<Value, String> {
    call_daemon("fan", "setRestoreOnStart", json!({ "enabled": enabled }))
}

#[tauri::command]
fn power_set_apply_to_os_profile(enabled: bool) -> Result<Value, String> {
    call_daemon("power", "setApplyToOsProfile", json!({ "enabled": enabled }))
}

#[tauri::command]
fn power_set_tuning(tuning: Value) -> Result<Value, String> {
    call_daemon("power", "setTuning", tuning)
}

#[tauri::command]
fn core_capabilities() -> Result<Value, String> {
    call_daemon("core", "capabilities", Value::Null)
}

#[tauri::command]
fn fan_diagnose(allow_writes: bool) -> Result<Value, String> {
    call_daemon("fan", "diagnose", json!({ "allowWrites": allow_writes }))
}

#[tauri::command]
fn system_get_info() -> Result<Value, String> {
    call_daemon("system", "getInfo", Value::Null)
}

#[tauri::command]
fn system_get_metrics() -> Result<Value, String> {
    call_daemon("system", "getMetrics", Value::Null)
}

#[tauri::command]
fn power_get_state() -> Result<Value, String> {
    call_daemon("power", "getState", Value::Null)
}

#[tauri::command]
fn power_set_mode(mode: String) -> Result<Value, String> {
    call_daemon("power", "setMode", json!({ "mode": mode }))
}

#[tauri::command]
fn power_set_auto_config(config: Value) -> Result<Value, String> {
    call_daemon("power", "setAutoConfig", config)
}

#[tauri::command]
fn power_set_restore_on_start(enabled: bool) -> Result<Value, String> {
    call_daemon("power", "setRestoreOnStart", json!({ "enabled": enabled }))
}

/// Per-user settings, stored under `~/.config/pyren/`.
///
/// The frontend owns the shape of these documents, so they are persisted as
/// opaque JSON objects rather than mirrored into Rust structs that would
/// need updating every time a preference is added.
fn app_config_store(namespace: &str) -> Result<ConfigStore, String> {
    if !APP_CONFIG_NAMESPACES.contains(&namespace) {
        return Err(format!("unknown config namespace '{namespace}'"));
    }
    Ok(ConfigStore::user())
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct JsonDocument {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

/// What the app is allowed to do, and what is missing. Runs entirely in
/// this unprivileged process and needs no daemon - the daemon being
/// unreachable is one of the things it diagnoses.
#[tauri::command]
fn admin_status() -> Result<Value, String> {
    Ok(admin::status(&socket_path()))
}

/// Applies one of a closed set of fixes, authenticated through `pkexec`.
#[tauri::command]
fn admin_grant(action: String) -> Result<Value, String> {
    admin::grant(&action)
}

#[tauri::command]
fn app_config_load(namespace: String) -> Result<Value, String> {
    let store = app_config_store(&namespace)?;
    let loaded = store.load::<JsonDocument>(&namespace);

    // Report *why* there is no data, so the UI can tell "first run" apart
    // from "your settings file was corrupt and has been set aside".
    let status = match &loaded.outcome {
        LoadOutcome::Loaded => json!({ "status": "loaded" }),
        LoadOutcome::Missing => json!({ "status": "missing" }),
        LoadOutcome::Recovered { backup, reason } => json!({
            "status": "recovered",
            "reason": reason,
            "backup": backup.as_ref().map(|b| b.display().to_string()),
        }),
        LoadOutcome::TooNew { found } => json!({ "status": "tooNew", "found": found }),
    };

    Ok(json!({
        "values": loaded.value.fields,
        "path": store.path_for(&namespace).display().to_string(),
        "outcome": status,
    }))
}

#[tauri::command]
fn app_config_save(namespace: String, values: Map<String, Value>) -> Result<String, String> {
    let store = app_config_store(&namespace)?;
    store
        .save(&namespace, &JsonDocument { fields: values })
        .map(|()| store.path_for(&namespace).display().to_string())
        .map_err(|e| e.to_string())
}

/// Works around a WebKitGTK bug that leaves the window showing a stale frame.
///
/// WebKitGTK's accelerated compositor hands finished frames to GTK as
/// DMA-BUFs. On a hybrid machine whose EGL device is the NVIDIA
/// proprietary driver, that hand-off silently stops presenting: the web
/// process keeps running, the DOM keeps updating and nothing is logged,
/// but the compositor keeps showing the last frame it managed to import.
/// Resizing forces the buffers to be reallocated, which is why the window
/// looks frozen until it is dragged and then jumps to the current state.
///
/// `WEBKIT_DISABLE_DMABUF_RENDERER=1` makes WebKit copy frames through
/// shared memory instead. Rendering stays accelerated; only the zero-copy
/// hand-off is given up, which costs a little on a page this static.
///
/// Applied only where the bug lives - Linux, with the NVIDIA module
/// loaded - and never over a value the user set, so `…=0` in the
/// environment is still a way to test whether a newer WebKitGTK has fixed
/// it. Must run before the first window is built, since WebKit reads this
/// once when the web process starts.
fn workaround_webkit_dmabuf() {
    if !cfg!(target_os = "linux") {
        return;
    }
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some() {
        return;
    }
    if !std::path::Path::new("/sys/module/nvidia").exists() {
        return;
    }
    std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    workaround_webkit_dmabuf();

    tauri::Builder::default()
        // Must be the first plugin registered. A second `pyren` launch hands
        // its argv to the instance already running and exits; we answer by
        // bringing the existing window forward rather than opening a rival
        // one that would fight over the config files.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            fan_get_status,
            fan_diagnose,
            fan_set_mode,
            fan_set_curve,
            fan_set_restore_on_start,
            core_capabilities,
            system_get_info,
            system_get_metrics,
            power_get_state,
            power_set_mode,
            power_set_auto_config,
            power_set_restore_on_start,
            power_set_tuning,
            power_set_apply_to_os_profile,
            admin_status,
            admin_grant,
            app_config_load,
            app_config_save
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
