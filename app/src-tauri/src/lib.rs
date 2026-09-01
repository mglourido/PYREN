//! Tauri commands are the only bridge the frontend has to the outside
//! world. This process runs unprivileged; it never touches hardware
//! directly - every module call is forwarded to omen-hub-daemon over its
//! Unix domain socket. See docs/01-ipc-protocol.md at the repo root for
//! the wire format.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use omen_hub_config::{ConfigStore, LoadOutcome};
use serde_json::{json, Map, Value};

/// Config namespaces the frontend is allowed to read and write.
///
/// An allowlist rather than a free-form name: the namespace becomes a
/// filename, and nothing in the webview should be able to choose arbitrary
/// paths. (`ConfigStore` sanitises names too - this is the first line.)
const APP_CONFIG_NAMESPACES: &[&str] = &["app", "ui"];

/// Must match the daemon's own default in `daemon/daemon/src/main.rs` -
/// keep both in sync until this moves into a shared config/env convention.
fn socket_path() -> String {
    std::env::var("OMEN_HUB_SOCKET").unwrap_or_else(|_| "/tmp/omen-hub-daemon.sock".to_string())
}

/// Sends one JSON-RPC-ish request to omen-hub-daemon and returns its
/// `result`, or an `Err` built from the connection failure or the
/// daemon's own `error` field.
fn call_daemon(module: &str, method: &str, params: Value) -> Result<Value, String> {
    let stream = UnixStream::connect(socket_path())
        .map_err(|e| format!("cannot reach omen-hub-daemon at {}: {e}", socket_path()))?;

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let request = json!({ "id": 1, "module": module, "method": method, "params": params });
    let mut line = request.to_string();
    line.push('\n');
    writer.write_all(line.as_bytes()).map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).map_err(|e| e.to_string())?;

    let response: Value = serde_json::from_str(&response_line).map_err(|e| e.to_string())?;
    if let Some(err) = response.get("error").and_then(Value::as_str) {
        return Err(err.to_string());
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

#[tauri::command]
fn fan_get_status() -> Result<Value, String> {
    call_daemon("fan", "getStatus", Value::Null)
}

#[tauri::command]
fn core_capabilities() -> Result<Value, String> {
    call_daemon("core", "capabilities", Value::Null)
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

/// Per-user settings, stored under `~/.config/omen-hub/`.
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            fan_get_status,
            core_capabilities,
            system_get_info,
            system_get_metrics,
            power_get_state,
            power_set_mode,
            power_set_auto_config,
            power_set_restore_on_start,
            app_config_load,
            app_config_save
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
