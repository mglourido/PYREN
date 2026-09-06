//! Tauri commands are the only bridge the frontend has to the outside
//! world. This process runs unprivileged; it never touches hardware
//! directly - every module call is forwarded to pyren-daemon over its
//! Unix domain socket. See docs/01-ipc-protocol.md at the repo root for
//! the wire format.

mod admin;
mod session;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use pyren_config::{ConfigStore, LoadOutcome};
use serde_json::{json, Map, Value};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};

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
/// One request, one response, with a refusal left as the daemon sent it.
///
/// Separate from [`call_daemon`] because a refusal is two different things
/// depending on who is asking. A command forwards the message to the user
/// and is done; the event watcher below has to branch on `kind`, because
/// "this daemon has no event stream" is a reason to stop asking and
/// everything else is a reason to try again.
/// # Why every command below is `#[tauri::command(async)]`
///
/// A **synchronous** Tauri command runs on the main thread, and this
/// function blocks on a socket with no timeout. That was invisible for the
/// calls that answer in milliseconds and catastrophic for the one that
/// does not: installing the driver is a single request that compiles a
/// kernel module and regenerates the initramfs, so the window froze solid
/// for the better part of a minute - no repaint, no progress, and the
/// install panel appearing all at once when it was already over.
///
/// `async` on a synchronous body tells Tauri to run it on a worker thread
/// instead. Marking *all* of them, not only the slow ones, is deliberate:
/// with no read timeout, any call to a wedged daemon would otherwise hang
/// the interface permanently, and "which of these can be slow" is not a
/// judgement worth re-making every time a command is added. None of them
/// take a `Window` or `AppHandle`, so none of them needs the main thread.
fn request_daemon(module: &str, method: &str, params: Value) -> Result<Value, String> {
    let stream = connect_daemon()?;

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let request = json!({ "id": 1, "module": module, "method": method, "params": params });
    let mut line = request.to_string();
    line.push('\n');
    writer.write_all(line.as_bytes()).map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).map_err(|e| e.to_string())?;

    serde_json::from_str(&response_line).map_err(|e| e.to_string())
}

fn call_daemon(module: &str, method: &str, params: Value) -> Result<Value, String> {
    let response = request_daemon(module, method, params)?;
    if let Some(err) = response.get("error") {
        return Err(daemon_error(err));
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

/// The daemon answers a refusal with `{ kind, message, key?, params? }` -
/// `kind` a closed set the UI can branch on, `message` the English prose,
/// and `key`/`params` present when the sentence is in the translation
/// catalog (`pyren_core::Msg`).
///
/// A Tauri command's error is a `String`, so a structured refusal is
/// forwarded as a **JSON string**: when `key` is set, the whole error
/// object is serialised and the frontend's `call()` parses it back into a
/// `DaemonRefusal` it can localise. A refusal with no `key`, and a
/// bare-string error from an older daemon, are passed through as the plain
/// message - reading an unparseable error as *absent* would turn a refusal
/// into a silent success.
fn daemon_error(error: &Value) -> String {
    match error {
        Value::String(message) => message.clone(),
        _ => {
            let has_key = error.get("key").and_then(Value::as_str).is_some_and(|k| !k.is_empty());
            if has_key {
                if let Ok(json) = serde_json::to_string(error) {
                    return json;
                }
            }
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the daemon refused without saying why")
                .to_string()
        }
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

#[tauri::command(async)]
fn fan_get_status() -> Result<Value, String> {
    call_daemon("fan", "getStatus", Value::Null)
}

/// `pwm` is only meaningful for `manual`; the daemon ignores it otherwise
/// and refuses a mode this machine's driver cannot do, rather than
/// pretending it worked.
#[tauri::command(async)]
fn fan_set_mode(mode: String, pwm: Option<u8>) -> Result<Value, String> {
    call_daemon("fan", "setMode", json!({ "mode": mode, "pwm": pwm }))
}

/// `profile` picks which power profile's curve to write: `None` writes the
/// one the machine is in, and `Some("")` the shared fallback.
#[tauri::command(async)]
fn fan_set_curve(
    curve: Value,
    interpolation: Option<String>,
    reference_sensor: Option<String>,
    profile: Option<String>,
) -> Result<Value, String> {
    call_daemon(
        "fan",
        "setCurve",
        json!({
            "curve": curve,
            "interpolation": interpolation,
            "referenceSensor": reference_sensor,
            "profile": profile,
        }),
    )
}

#[tauri::command(async)]
fn fan_set_restore_on_start(enabled: bool) -> Result<Value, String> {
    call_daemon("fan", "setRestoreOnStart", json!({ "enabled": enabled }))
}

/// The fan cleaner. `refresh` re-asks the firmware what it can do, which
/// costs two ACPI calls - the polling status read leaves it off.
#[tauri::command(async)]
fn fan_cleaner_status(refresh: bool) -> Result<Value, String> {
    call_daemon("fan", "cleanerStatus", json!({ "refresh": refresh }))
}

/// Starts a cycle. Blocks for a few seconds while the blades are braked,
/// then returns with a countdown running; the daemon ends it on its own.
#[tauri::command(async)]
fn fan_start_cleaning(
    speed: Option<u8>,
    seconds: Option<u64>,
    force: Option<bool>,
) -> Result<Value, String> {
    call_daemon(
        "fan",
        "startCleaning",
        json!({ "speed": speed, "seconds": seconds, "force": force.unwrap_or(false) }),
    )
}

#[tauri::command(async)]
fn fan_stop_cleaning() -> Result<Value, String> {
    call_daemon("fan", "stopCleaning", Value::Null)
}

/// The remembered duration and speed, which are a preference rather than
/// a parameter of one run.
#[tauri::command(async)]
fn fan_set_cleaner_config(seconds: Option<u64>, speed: Option<Value>) -> Result<Value, String> {
    let mut params = serde_json::Map::new();
    if let Some(seconds) = seconds {
        params.insert("seconds".into(), json!(seconds));
    }
    // `null` here means "back to the firmware's own speed", so it has to
    // reach the daemon as a value rather than be dropped as absent.
    if let Some(speed) = speed {
        params.insert("speed".into(), speed);
    }
    call_daemon("fan", "setCleanerConfig", Value::Object(params))
}

#[tauri::command(async)]
fn power_set_apply_to_os_profile(enabled: bool) -> Result<Value, String> {
    call_daemon("power", "setApplyToOsProfile", json!({ "enabled": enabled }))
}

#[tauri::command(async)]
fn power_set_tuning(tuning: Value) -> Result<Value, String> {
    call_daemon("power", "setTuning", tuning)
}

/// The 4-zone lightbar. Every write needs root *and* the `acpi_call`
/// kernel module, and the three ways it can be unavailable - no `hp-wmi`,
/// no `acpi_call`, a firmware that refuses - are different problems with
/// different fixes, so the page reads them off `getCapabilities` rather
/// than being handed one boolean (`docs/01-ipc-protocol.md` §"`rgb`
/// module").
#[tauri::command(async)]
fn rgb_get_status() -> Result<Value, String> {
    call_daemon("rgb", "getStatus", Value::Null)
}

/// Which GPU is driving the screen - iGPU only / hybrid / dGPU - through
/// the patched `hp-wmi` driver's `gpu_mux_mode`, not `supergfxctl`. See
/// `docs/01-ipc-protocol.md` §"`gpu` module".
#[tauri::command(async)]
fn gpu_get_status() -> Result<Value, String> {
    call_daemon("gpu", "getStatus", Value::Null)
}

/// Takes effect after a logout or reboot, which the daemon does not do
/// itself - the graphics page says so.
#[tauri::command(async)]
fn gpu_set_mode(mode: String) -> Result<Value, String> {
    call_daemon("gpu", "setMode", json!({ "mode": mode }))
}

/// System-wide smart queuing only - see `docs/01-ipc-protocol.md`
/// §"`network` module" for why there is no per-application field here.
#[tauri::command(async)]
fn network_get_status() -> Result<Value, String> {
    call_daemon("network", "getStatus", Value::Null)
}

#[tauri::command(async)]
fn network_set_mode(mode: String) -> Result<Value, String> {
    call_daemon("network", "setMode", json!({ "mode": mode }))
}

/// A **fresh** probe, unlike the one in `getStatus`. This is what makes
/// "install acpi_call, then ask again" a complete workflow without
/// restarting the daemon, so the page calls it after an install.
#[tauri::command(async)]
fn rgb_get_capabilities() -> Result<Value, String> {
    call_daemon("rgb", "getCapabilities", Value::Null)
}

/// `color` goes out as `"#rrggbb"`; `brightness` is a percentage, and
/// omitting it keeps whatever the daemon has stored.
#[tauri::command(async)]
fn rgb_set_static(color: String, brightness: Option<u8>) -> Result<Value, String> {
    call_daemon("rgb", "setStatic", json!({ "color": color, "brightness": brightness }))
}

#[tauri::command(async)]
fn rgb_set_zones(zones: Value, brightness: Option<u8>) -> Result<Value, String> {
    call_daemon("rgb", "setZones", json!({ "zones": zones, "brightness": brightness }))
}

#[tauri::command(async)]
fn rgb_off() -> Result<Value, String> {
    call_daemon("rgb", "off", Value::Null)
}

/// Asks the firmware what the zones are, which is four ACPI round trips -
/// hence a button and not a poll. It is also the only check that the
/// payload was *understood* rather than merely accepted.
#[tauri::command(async)]
fn rgb_read_zones() -> Result<Value, String> {
    call_daemon("rgb", "readZones", Value::Null)
}

/// Pins one of the lighting dialects, or `auto` to go back to picking the
/// first that answers. Exists because auto can only ever choose a dialect
/// this build can *read*, and the person at the keyboard can see whether
/// the lights actually changed.
#[tauri::command(async)]
fn rgb_set_dialect(dialect: String) -> Result<Value, String> {
    call_daemon("rgb", "setDialect", json!({ "dialect": dialect }))
}

#[tauri::command(async)]
fn rgb_set_restore_on_start(enabled: bool) -> Result<Value, String> {
    call_daemon("rgb", "setRestoreOnStart", json!({ "enabled": enabled }))
}

/// GPU overclocking. The one module whose calls can leave the machine
/// running outside what the firmware shipped, so three of its five
/// commands exist purely to make that hard to do by accident: the consent,
/// the confirmation, and the reset.
///
/// `request` is passed through as the daemon's params for the same reason
/// the installer's is - the shape is documented in
/// `docs/01-ipc-protocol.md`, and what makes an apply safe is the daemon's
/// own rules (consent, the ramp, the revert timer), not a re-typing of the
/// fields here.
#[tauri::command(async)]
fn overclock_get_state() -> Result<Value, String> {
    call_daemon("overclock", "getState", Value::Null)
}

/// `allowWrites` opts into the one question that costs a write: whether
/// the clock offsets can be *set*, as opposed to merely read.
#[tauri::command(async)]
fn overclock_probe(allow_writes: bool) -> Result<Value, String> {
    call_daemon("overclock", "probe", json!({ "allowWrites": allow_writes }))
}

#[tauri::command(async)]
fn overclock_set_consent(accepted: bool) -> Result<Value, String> {
    call_daemon("overclock", "setConsent", json!({ "accepted": accepted }))
}

#[tauri::command(async)]
fn overclock_apply(request: Value) -> Result<Value, String> {
    call_daemon("overclock", "apply", request)
}

#[tauri::command(async)]
fn overclock_confirm() -> Result<Value, String> {
    call_daemon("overclock", "confirm", Value::Null)
}

/// Undoes a pending change now instead of at the end of its timer.
#[tauri::command(async)]
fn overclock_cancel() -> Result<Value, String> {
    call_daemon("overclock", "cancel", Value::Null)
}

#[tauri::command(async)]
fn overclock_reset(gpu: Option<String>) -> Result<Value, String> {
    call_daemon("overclock", "reset", json!({ "gpu": gpu }))
}

#[tauri::command(async)]
fn overclock_set_restore_on_start(enabled: bool) -> Result<Value, String> {
    call_daemon("overclock", "setRestoreOnStart", json!({ "enabled": enabled }))
}

/// The driver installer, in the four parts the module is split into.
///
/// `request` is passed through as the daemon's params rather than being
/// re-typed here: the shape is documented in `docs/01-ipc-protocol.md` and
/// mirroring it in the shell would mean editing three places to add one
/// field. What keeps `apply` safe is not this layer but the daemon's own
/// rule that it is a dry run unless `confirm` is true.
#[tauri::command(async)]
fn installer_inspect() -> Result<Value, String> {
    call_daemon("installer", "inspect", Value::Null)
}

#[tauri::command(async)]
fn installer_autodetect(request: Value) -> Result<Value, String> {
    call_daemon("installer", "autodetect", request)
}

#[tauri::command(async)]
fn installer_plan(request: Value) -> Result<Value, String> {
    call_daemon("installer", "plan", request)
}

#[tauri::command(async)]
fn installer_apply(request: Value) -> Result<Value, String> {
    call_daemon("installer", "apply", request)
}

#[tauri::command(async)]
fn core_capabilities() -> Result<Value, String> {
    call_daemon("core", "capabilities", Value::Null)
}

#[tauri::command(async)]
fn fan_diagnose(allow_writes: bool) -> Result<Value, String> {
    call_daemon("fan", "diagnose", json!({ "allowWrites": allow_writes }))
}

/// Blocks for as long as the run takes - up to `seconds`, and the daemon
/// ends early once the reading settles. Tauri commands run off the UI
/// thread, so the window stays responsive; the install panel covers it
/// anyway, because the fans are at full speed while this is happening.
#[tauri::command(async)]
fn fan_calibrate(seconds: Option<u64>) -> Result<Value, String> {
    call_daemon("fan", "calibrate", json!({ "seconds": seconds }))
}

/// Holds the fans at a speed they are not at and watches whether they
/// follow. Blocks like `fan_calibrate` does, and for the same reason: the
/// caller is waiting on a physical process. The daemon ends early as soon
/// as the fans answer, so a machine that obeys is done in a few seconds.
#[tauri::command(async)]
fn fan_probe_speed_control(seconds: Option<u64>) -> Result<Value, String> {
    call_daemon("fan", "probeSpeedControl", json!({ "seconds": seconds }))
}

#[tauri::command(async)]
fn system_get_info() -> Result<Value, String> {
    call_daemon("system", "getInfo", Value::Null)
}

#[tauri::command(async)]
fn system_get_metrics() -> Result<Value, String> {
    call_daemon("system", "getMetrics", Value::Null)
}

#[tauri::command(async)]
fn power_get_state() -> Result<Value, String> {
    call_daemon("power", "getState", Value::Null)
}

#[tauri::command(async)]
fn power_set_mode(mode: String) -> Result<Value, String> {
    call_daemon("power", "setMode", json!({ "mode": mode }))
}

#[tauri::command(async)]
fn power_set_auto_config(config: Value) -> Result<Value, String> {
    call_daemon("power", "setAutoConfig", config)
}

#[tauri::command(async)]
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
#[tauri::command(async)]
fn admin_status() -> Result<Value, String> {
    Ok(admin::status(&socket_path()))
}

/// Applies one of a closed set of fixes, authenticated through `pkexec`.
#[tauri::command(async)]
fn admin_grant(action: String) -> Result<Value, String> {
    admin::grant(&action)
}

#[tauri::command(async)]
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

#[tauri::command(async)]
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

/// One of the frontend's own booleans, read back from the file it writes.
///
/// `~/.config/pyren/app.json` is the only thing the two halves share: the
/// settings page owns the toggles, this shell owns the window. Re-read on
/// every use rather than cached at launch, because a toggle flipped while
/// the app is up has to change what the window does *now* - a value read an
/// hour ago would leave the close button doing something the settings page
/// no longer says.
fn app_flag(name: &str) -> bool {
    ConfigStore::user()
        .load::<JsonDocument>("app")
        .value
        .fields
        .get(name)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Whether the window should never appear at launch.
fn starts_hidden() -> bool {
    app_flag("startMinimized")
}

/// Whether closing the window means "put Pyren away" rather than "quit".
///
/// Its own setting and not a consequence of [`starts_hidden`], because they
/// answer different questions. Pyren is what starts `pyren-osd` at launch,
/// so somebody who wants the widget there without a window open needs the
/// app to *keep running* after they close it - which is this - and that is
/// a want quite separate from whether the window shows up at login.
fn closes_to_tray() -> bool {
    app_flag("closeToTray")
}

/// The tray menu's two labels, in the language the settings file names.
///
/// Not the frontend's catalog: that is TypeScript, loaded by the webview,
/// and this menu is drawn by the desktop before any webview exists. Two
/// strings duplicated is a smaller price than a menu that is English on a
/// Spanish desktop, and if a third locale is ever added the fallback here
/// is the same English the rest of the app falls back to.
fn tray_labels() -> (&'static str, &'static str) {
    let language = ConfigStore::user()
        .load::<JsonDocument>("app")
        .value
        .fields
        .get("mainLanguage")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();

    match language.as_str() {
        "es" => ("Abrir Pyren", "Salir de Pyren"),
        _ => ("Open Pyren", "Quit Pyren"),
    }
}

/// Brings the window back from hidden, minimised, or merely buried.
///
/// All three, in that order, because "show me Pyren" is one gesture and the
/// window can be in any of those states: `show` on a minimised window
/// leaves it minimised, and `set_focus` on a hidden one does nothing at all.
fn reveal_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// The tray icon - what makes "start minimised" a state the user can get
/// back out of.
///
/// On Wayland the only tray is StatusNotifierItem over D-Bus, and whether
/// the icon is ever *drawn* depends on the desktop running a tray host
/// (waybar's `tray` module, ags, the GNOME extension). Nothing here can
/// check that, which is why [`run`] shows the window anyway when this
/// fails: an app hidden behind an icon nobody renders is unreachable.
fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let (open, quit) = tray_labels();
    let open = MenuItem::with_id(app, "open", open, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", quit, true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &PredefinedMenuItem::separator(app)?, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(tauri::include_image!("icons/32x32.png"))
        .tooltip("Pyren")
        .menu(&menu)
        // Right-click opens the menu; a left click just shows the window,
        // which is what clicking a tray icon means on every desktop.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => reveal_window(app),
            // The only way out when the window is hidden, so it really
            // exits rather than hiding again.
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

/// What is running in this session, and what starts at login.
#[tauri::command(async)]
fn session_status() -> Value {
    session::status()
}

/// Starts the widget now. The app does this at launch by itself; this is
/// the button for the case where it was stopped by hand.
#[tauri::command(async)]
fn session_start_osd() -> Result<Value, String> {
    session::start_osd()?;
    Ok(session::status())
}

/// Shows the widget without changing the power mode.
#[tauri::command(async)]
fn session_show_osd() -> Result<Value, String> {
    session::show_osd()?;
    Ok(session::status())
}

/// Stops the widget now, and stops it coming back at login.
///
/// Both, because they are one idea to the person switching it off: a
/// widget that vanishes and reappears at the next login has not been
/// turned off, it has been dismissed.
#[tauri::command(async)]
fn session_stop_osd() -> Result<Value, String> {
    session::stop_osd()?;
    Ok(session::status())
}

#[tauri::command(async)]
fn session_set_osd_at_login(enabled: bool) -> Result<Value, String> {
    session::set_osd_at_login(enabled)
}

/// What key is bound, whether the daemon can hear one at all, and whether
/// acting on it is switched on.
#[tauri::command(async)]
fn hotkey_get_status() -> Result<Value, String> {
    call_daemon("hotkey", "getStatus", Value::Null)
}

/// Opens a learn window and blocks until a key arrives or it times out.
///
/// Long-running on purpose: the reply *is* the key the user pressed, so
/// the settings page shows "press a key now" for exactly as long as this
/// call is outstanding.
#[tauri::command(async)]
fn hotkey_learn(timeout_ms: Option<u64>) -> Result<Value, String> {
    call_daemon("hotkey", "learn", json!({ "timeoutMs": timeout_ms, "bind": true }))
}

/// Forgets the bound key. The hotkey stays switched on, so the next
/// `learn` binds without a second trip.
#[tauri::command(async)]
fn hotkey_clear() -> Result<Value, String> {
    call_daemon("hotkey", "setTriggers", json!({ "triggers": [] }))
}

#[tauri::command(async)]
fn hotkey_set_enabled(enabled: bool) -> Result<Value, String> {
    call_daemon("hotkey", "setEnabled", json!({ "enabled": enabled }))
}

/// Does what the key does, without the key: the settings page's preview,
/// and the only way to see the widget on a laptop whose Fn+P never
/// reaches Linux.
#[tauri::command(async)]
fn hotkey_press() -> Result<Value, String> {
    call_daemon("hotkey", "press", Value::Null)
}

#[tauri::command(async)]
fn keymap_get_status() -> Result<Value, String> {
    call_daemon("keymap", "getStatus", Value::Null)
}

/// `mapping` and `from` are passed through as-is - `{ from: { device?, keycode }, to }`
/// and `{ device?, keycode }` respectively - rather than unpacked into
/// separate arguments, since both are already the daemon's own shape.
#[tauri::command(async)]
fn keymap_set_mapping(mapping: Value) -> Result<Value, String> {
    call_daemon("keymap", "setMapping", mapping)
}

#[tauri::command(async)]
fn keymap_remove_mapping(from: Value) -> Result<Value, String> {
    call_daemon("keymap", "removeMapping", from)
}

/// The one call here that can take a keyboard away from the rest of the
/// session if the mapping is wrong - `EVIOCGRAB` is exclusive, so `hotkey`
/// stops hearing the same device too. See `docs/01-ipc-protocol.md`
/// §"`keymap` module".
#[tauri::command(async)]
fn keymap_set_enabled(enabled: bool) -> Result<Value, String> {
    call_daemon("keymap", "setEnabled", json!({ "enabled": enabled }))
}

#[tauri::command(async)]
fn session_set_app_at_login(enabled: bool) -> Result<Value, String> {
    session::set_app_at_login(enabled)
}

/// How long one `core.nextEvent` waits before the daemon answers with
/// nothing. Long, because a poll that returns nothing costs a round trip
/// and this is not a timer - the answer arrives when something happens.
const EVENT_POLL_MS: u64 = 25_000;

/// After the daemon goes away. Long enough not to spin on a socket that
/// may be gone for the rest of the session.
const EVENT_RETRY: std::time::Duration = std::time::Duration::from_secs(2);

/// Forwards the daemon's event stream into the webview as `daemon-event`.
///
/// The app can change the power mode, and so can four other things: the
/// laptop's own performance key, `pyren-ctl`, the on-screen display, and
/// the daemon's own supervisor. Without this the window went on showing
/// whichever mode it had last read - the page was not wrong when it was
/// drawn, it just had no way to hear that the machine had moved.
///
/// One thread and one socket, held in a long poll. It is not a timer:
/// asking every two seconds would cost the same round trips whether or not
/// anything happened, and would still be up to two seconds late.
fn watch_daemon_events(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut since: Option<u64> = None;

        loop {
            let mut params = json!({ "timeoutMs": EVENT_POLL_MS });
            if let Some(seq) = since {
                params["since"] = json!(seq);
            }

            let response = match request_daemon("core", "nextEvent", params) {
                Ok(response) => response,
                Err(_) => {
                    // Unreachable. Not reported: the UI already says the
                    // daemon is down, from the telemetry poll that runs
                    // whether or not this thread exists.
                    since = None;
                    std::thread::sleep(EVENT_RETRY);
                    continue;
                }
            };

            if let Some(error) = response.get("error") {
                // A daemon older than the event stream will answer this
                // way forever, so stop asking. Everything else - busy,
                // internal, a kind this build has never heard of - is
                // worth another try in a moment.
                if error.get("kind").and_then(Value::as_str) == Some("unknownMethod") {
                    eprintln!(
                        "pyren: this daemon has no event stream, so the window will not \
                         follow mode changes made outside it"
                    );
                    return;
                }
                since = None;
                std::thread::sleep(EVENT_RETRY);
                continue;
            }

            let result = response.get("result").cloned().unwrap_or(Value::Null);
            let seq = result.get("seq").and_then(Value::as_u64);
            // A daemon that restarted counts from zero again, so a sequence
            // lower than the one held is a new daemon rather than an error.
            since = match (since, seq) {
                (Some(previous), Some(seq)) if seq < previous => Some(seq),
                (previous, seq) => seq.or(previous),
            };

            for event in result.get("events").and_then(Value::as_array).into_iter().flatten() {
                // Emitted verbatim: which topics are worth reacting to is
                // the frontend's business, and a topic this build has never
                // heard of has to reach it rather than be filtered out here.
                let _ = app.emit("daemon-event", event.clone());
            }
        }
    });
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
            // Also the way back in when the tray is not being drawn: running
            // `pyren` again reveals the window this copy started hidden.
            reveal_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        // Closing the window means quit, unless the user asked for it to
        // mean "put Pyren away" instead. Read from the file each time, so
        // flipping the toggle changes what the close button does straight
        // away rather than at the next launch.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" && closes_to_tray() {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .setup(|app| {
            watch_daemon_events(app.handle().clone());
            // On a thread: it may shell out to systemctl, and nothing about
            // starting the widget should hold up the window.
            std::thread::spawn(session::ensure_running);

            // The window is `"visible": false` in tauri.conf.json and is
            // shown here instead, so that starting minimised is a window
            // that never appears rather than one that flashes up and is
            // yanked away. Which means every path out of this block has to
            // end in either a shown window or a working tray icon.
            let tray = build_tray(app);
            if let Err(e) = &tray {
                eprintln!("pyren: could not create the tray icon: {e}");
            }
            if !starts_hidden() || tray.is_err() {
                reveal_window(&app.handle().clone());
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            fan_get_status,
            fan_diagnose,
            fan_calibrate,
            fan_probe_speed_control,
            fan_set_mode,
            fan_set_curve,
            fan_set_restore_on_start,
            fan_cleaner_status,
            fan_start_cleaning,
            fan_stop_cleaning,
            fan_set_cleaner_config,
            core_capabilities,
            system_get_info,
            system_get_metrics,
            power_get_state,
            power_set_mode,
            power_set_auto_config,
            power_set_restore_on_start,
            power_set_tuning,
            power_set_apply_to_os_profile,
            overclock_get_state,
            overclock_probe,
            overclock_set_consent,
            overclock_apply,
            overclock_confirm,
            overclock_cancel,
            overclock_reset,
            overclock_set_restore_on_start,
            rgb_get_status,
            gpu_get_status,
            gpu_set_mode,
            network_get_status,
            network_set_mode,
            rgb_get_capabilities,
            rgb_set_static,
            rgb_set_zones,
            rgb_off,
            rgb_read_zones,
            rgb_set_dialect,
            rgb_set_restore_on_start,
            installer_inspect,
            installer_autodetect,
            installer_plan,
            installer_apply,
            admin_status,
            admin_grant,
            session_status,
            session_start_osd,
            session_show_osd,
            session_stop_osd,
            session_set_osd_at_login,
            session_set_app_at_login,
            hotkey_get_status,
            hotkey_learn,
            hotkey_clear,
            hotkey_set_enabled,
            hotkey_press,
            keymap_get_status,
            keymap_set_mapping,
            keymap_remove_mapping,
            keymap_set_enabled,
            app_config_load,
            app_config_save
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
