//! Tauri commands are the only bridge the frontend has to the outside
//! world. This process runs unprivileged; it never touches hardware
//! directly - every module call is forwarded to omen-hub-daemon over its
//! Unix domain socket. See docs/01-ipc-protocol.md at the repo root for
//! the wire format.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use serde_json::{json, Value};

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![fan_get_status, core_capabilities])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
