//! Key mapping: remap a physical key to another key, system-wide, no
//! compositor keybinding involved.
//!
//! | method | params | result |
//! |---|---|---|
//! | `keymap.getStatus` | none | mappings, whether the remapper is running, and why not if not |
//! | `keymap.setMapping` | `{ "from": { "device"?, "keycode" }, "to": keycode }` | as `getStatus` |
//! | `keymap.removeMapping` | `{ "device"?, "keycode" }` | as `getStatus` |
//! | `keymap.setEnabled` | `{ "enabled": bool }` | as `getStatus` |
//!
//! ## The backend decision `dev/TODO.md` §2 was waiting on
//!
//! Three ways exist to remap a key on Linux: `keyd` (a second daemon and
//! its own config file), a `udev` hwdb entry (build-time, one keycode for
//! another, no notion of "current mapping" a settings page could read
//! back), or doing it here. `pyren_hotkey` already answered the question
//! that used to make the third option expensive: this daemon runs as root
//! and already opens `/dev/input/event*` directly, with no evdev crate, to
//! read the vendor performance key. An evdev-level remapper needs exactly
//! that access plus a `/dev/uinput` virtual device to reinject the
//! substituted keys - no new privilege, and one fewer moving part than
//! shelling out to a second daemon's config format.
//!
//! ## Grabbing a keyboard and hearing its hotkey are mutually exclusive
//!
//! `EVIOCGRAB` makes this module's file descriptor the *only* one the
//! kernel delivers a device's events to - `pyren_hotkey`'s own reader on
//! the same device included. So a keymap enabled on the keyboard the
//! vendor key lives on silences `hotkey` there for as long as it runs. This
//! is not a bug to route around; it is why the module defaults to
//! `enabled: false` rather than watching to be turned on the moment it is
//! installed, unlike `hotkey` itself.
//!
//! ## What is not remapped
//!
//! Only a bare key, matched by keycode (optionally scoped to one named
//! device). Modifiers, chords and macros stay app-side "coming soon" - the
//! reference feature this port is measured against - because those are a
//! sequence of synthetic events this module has no reason to own; a plain
//! substitution is what a virtual `uinput` keyboard forwarding real
//! keycodes is for.

mod raw;

use std::collections::HashMap;
use std::io::ErrorKind as IoErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pyren_config::{ConfigStore, LoadOutcome};
use pyren_core::{log_warn, msg, ErrorKind, Module, ModuleError, ModuleResult, Msg};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One physical key, optionally scoped to a named device - `None` matches
/// that keycode on any keyboard, which is the common case: most laptops
/// have exactly one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeySpec {
    pub device: Option<String>,
    pub keycode: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyMapping {
    pub from: KeySpec,
    pub to: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct KeymapConfig {
    pub enabled: bool,
    pub mappings: Vec<KeyMapping>,
}

/// Why the remapper is not running, in the words a settings page should
/// show.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unavailable {
    NoDevices,
    NeedsRoot,
    NoUinput,
    Io(String),
}

impl Unavailable {
    fn to_msg(&self) -> Msg {
        match self {
            Self::NoDevices => msg!(
                "keymap.unavailable.noDevices",
                "no keyboard devices in /dev/input, so there is nothing to remap"
            ),
            Self::NeedsRoot => msg!(
                "keymap.unavailable.needsRoot",
                "grabbing /dev/input needs root; the systemd unit runs the daemon as root"
            ),
            Self::NoUinput => msg!(
                "keymap.unavailable.noUinput",
                "/dev/uinput is missing; load the 'uinput' kernel module"
            ),
            Self::Io(detail) => Msg::literal(detail.clone()),
        }
    }
}

struct State {
    config: KeymapConfig,
    running: bool,
    unavailable: Option<Unavailable>,
    devices: Vec<String>,
    last_save_error: Option<String>,
}

/// Cloning shares one module: the running thread and every clone of the
/// handle read the same table and the same config file.
#[derive(Clone)]
pub struct KeymapModule {
    state: Arc<Mutex<State>>,
    store: ConfigStore,
    /// Set to ask the running thread to stop; it clears this and `running`
    /// itself once it has ungrabbed everything.
    stop: Arc<AtomicBool>,
}

impl Default for KeymapModule {
    fn default() -> Self {
        Self::new()
    }
}

impl KeymapModule {
    pub fn new() -> Self {
        Self::with_store(ConfigStore::system())
    }

    pub fn with_store(store: ConfigStore) -> Self {
        let loaded = store.load::<KeymapConfig>("keymap");
        match &loaded.outcome {
            LoadOutcome::Loaded | LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                log_warn!(
                    "keymap config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                log_warn!(
                    "keymap config is version {found}, newer than this build understands; \
                     using defaults and leaving the file alone"
                );
            }
        }

        let state = State {
            config: loaded.value,
            running: false,
            unavailable: None,
            devices: Vec::new(),
            last_save_error: None,
        };

        let module = Self { state: Arc::new(Mutex::new(state)), store, stop: Arc::new(AtomicBool::new(false)) };
        if module.lock().config.enabled {
            module.start();
        }
        module
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn persist(&self, state: &mut State) {
        match self.store.save("keymap", &state.config) {
            Ok(()) => state.last_save_error = None,
            Err(e) => {
                let message = e.to_string();
                log_warn!("could not save keymap config: {message}");
                state.last_save_error = Some(message);
            }
        }
    }

    fn status(&self) -> Value {
        let state = self.lock();
        json!({
            "enabled": state.config.enabled,
            "running": state.running,
            "detail": self.detail(&state),
            "devices": state.devices,
            "mappings": state.config.mappings,
            "configPath": self.store.path_for("keymap"),
            "configSaveError": state.last_save_error,
        })
    }

    fn detail(&self, state: &State) -> Msg {
        if let Some(reason) = &state.unavailable {
            return reason.to_msg();
        }
        if !state.config.enabled {
            return msg!(
                "keymap.detail.disabled",
                "no key is being remapped; turn this on once a mapping is set"
            );
        }
        if state.config.mappings.is_empty() {
            return msg!("keymap.detail.noMappings", "enabled, with nothing mapped yet");
        }
        msg!(
            "keymap.detail.running",
            { "count" => state.config.mappings.len() },
            "remapping {count} keys"
        )
    }

    /// Starts the grab/uinput thread if it is not already running. Not an
    /// error if it cannot: an unprivileged development daemon cannot open
    /// `/dev/input`, and that belongs in `getStatus`, not a refusal to run.
    fn start(&self) {
        if self.lock().running {
            return;
        }
        self.stop.store(false, Ordering::SeqCst);
        let module = self.clone();
        let spawned = std::thread::Builder::new().name("pyren-keymap".into()).spawn(move || module.run()).is_ok();
        if !spawned {
            let mut state = self.lock();
            state.unavailable = Some(Unavailable::Io("could not start the remapper thread".into()));
        }
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // The thread notices on its next poll timeout (see `run`) and
        // clears `running` itself after ungrabbing everything - there is
        // no join here because the caller is holding the IPC socket open
        // and must not block on a device that has gone away.
    }

    /// Opens every keyboard, grabs it, and forwards its events - substituted
    /// per the live mapping table - through one virtual device, until asked
    /// to stop.
    fn run(self) {
        let paths = raw::device_paths();
        let mut grabbed = Vec::new();
        let mut names = Vec::new();
        let mut saw_permission_denied = false;

        for path in &paths {
            let name = raw::device_name(path).unwrap_or_else(|| path.display().to_string());
            match raw::open_nonblocking(path) {
                Ok(file) => {
                    if raw::grab(&file, true).is_ok() {
                        names.push(name);
                        grabbed.push(file);
                    }
                    // A device that refuses the grab (a mouse under a
                    // generic /dev/input entry, say) is left alone rather
                    // than aborting the whole run over one unrelated node.
                }
                Err(e) if e.kind() == IoErrorKind::PermissionDenied => saw_permission_denied = true,
                Err(_) => {}
            }
        }

        if grabbed.is_empty() {
            let mut state = self.lock();
            state.unavailable =
                Some(if saw_permission_denied { Unavailable::NeedsRoot } else { Unavailable::NoDevices });
            return;
        }

        let keys: Vec<u16> = (0..=raw::KEY_MAX).collect();
        let uinput = match raw::create_uinput("pyren-keymap", &keys) {
            Ok(file) => file,
            Err(e) => {
                for file in &grabbed {
                    let _ = raw::grab(file, false);
                }
                let mut state = self.lock();
                state.unavailable = Some(if e.kind() == IoErrorKind::NotFound {
                    Unavailable::NoUinput
                } else if e.kind() == IoErrorKind::PermissionDenied {
                    Unavailable::NeedsRoot
                } else {
                    Unavailable::Io(e.to_string())
                });
                return;
            }
        };
        let mut uinput = uinput;

        {
            let mut state = self.lock();
            state.running = true;
            state.unavailable = None;
            state.devices = names;
        }

        let mut buffer = [0u8; raw::EVENT_SIZE * 32];
        while !self.stop.load(Ordering::SeqCst) {
            let mut fds: Vec<libc::pollfd> = grabbed
                .iter()
                .map(|f| libc::pollfd {
                    fd: std::os::unix::io::AsRawFd::as_raw_fd(f),
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            // SAFETY: `fds` is a valid slice for the length passed.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 500) };
            if ready <= 0 {
                continue;
            }

            let table = self.table();
            for (index, fd) in fds.iter().enumerate() {
                if fd.revents & libc::POLLIN == 0 {
                    continue;
                }
                let Ok(events) = raw::read_events(&mut grabbed[index], &mut buffer) else { continue };
                for mut event in events {
                    if event.kind == raw::EV_KEY {
                        if let Some(&to) = table.get(&event.code) {
                            event.code = to;
                        }
                    }
                    let _ = raw::write_event(&mut uinput, event);
                }
            }
        }

        for file in &grabbed {
            let _ = raw::grab(file, false);
        }
        raw::destroy_uinput(&uinput);
        let mut state = self.lock();
        state.running = false;
        state.devices = Vec::new();
    }

    /// The live substitution table: device-scoped mappings are not
    /// distinguished here, because every grabbed keyboard shares one
    /// virtual output and `hotkey`'s own experience is that a laptop with
    /// two keyboards is the exception - a device-specific entry still
    /// round-trips through config and `getStatus`, for when it is not.
    fn table(&self) -> HashMap<u16, u16> {
        self.lock().config.mappings.iter().map(|m| (m.from.keycode, m.to)).collect()
    }

    fn set_mapping(&self, mapping: KeyMapping) -> ModuleResult {
        let mut state = self.lock();
        state.config.mappings.retain(|m| m.from.keycode != mapping.from.keycode || m.from.device != mapping.from.device);
        state.config.mappings.push(mapping);
        self.persist(&mut state);
        drop(state);
        Ok(self.status())
    }

    fn remove_mapping(&self, spec: KeySpec) -> ModuleResult {
        let mut state = self.lock();
        state.config.mappings.retain(|m| m.from != spec);
        self.persist(&mut state);
        drop(state);
        Ok(self.status())
    }

    fn set_enabled(&self, enabled: bool) -> ModuleResult {
        {
            let mut state = self.lock();
            state.config.enabled = enabled;
            self.persist(&mut state);
        }
        if enabled {
            self.start();
        } else {
            self.stop();
        }
        Ok(self.status())
    }
}

impl Module for KeymapModule {
    fn id(&self) -> &'static str {
        "keymap"
    }

    /// True whenever this machine has a keyboard at all - the same
    /// reasoning as `pyren_hotkey::is_supported`: being unable to *reach*
    /// it is a fixable `permissionDenied`, not a reason to hide the page.
    fn is_supported(&self) -> bool {
        !raw::device_paths().is_empty()
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getStatus" => Ok(self.status()),

            "setMapping" => {
                let mapping: KeyMapping = serde_json::from_value(params).map_err(|e| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "keymap.err.invalidMapping",
                            { "detail" => e.to_string() },
                            "invalid mapping: {detail}"
                        ),
                    )
                })?;
                self.set_mapping(mapping)
            }

            "removeMapping" => {
                let spec: KeySpec = serde_json::from_value(params).map_err(|e| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "keymap.err.invalidSpec",
                            { "detail" => e.to_string() },
                            "invalid key: {detail}"
                        ),
                    )
                })?;
                self.remove_mapping(spec)
            }

            "setEnabled" => {
                let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!("keymap.err.enabledRequired", "params.enabled is required: true or false"),
                    )
                })?;
                self.set_enabled(enabled)
            }

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn store() -> ConfigStore {
        let dir = std::env::temp_dir()
            .join(format!("pyren-keymap-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        ConfigStore::at(dir)
    }

    fn spec(keycode: u16) -> KeySpec {
        KeySpec { device: None, keycode }
    }

    #[test]
    fn a_fresh_module_is_disabled_and_empty() {
        let module = KeymapModule::with_store(store());
        let status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(status["enabled"], json!(false));
        assert_eq!(status["running"], json!(false));
        assert_eq!(status["mappings"], json!([]));
    }

    #[test]
    fn setting_a_mapping_persists_and_replaces_the_same_key() {
        let module = KeymapModule::with_store(store());
        module.call("setMapping", json!({ "from": { "keycode": 1 }, "to": 2 })).unwrap();
        let status = module.call("setMapping", json!({ "from": { "keycode": 1 }, "to": 3 })).unwrap();
        let mappings = status["mappings"].as_array().unwrap();
        assert_eq!(mappings.len(), 1, "the second call replaces the first, not adds to it");
        assert_eq!(mappings[0]["to"], json!(3));
    }

    #[test]
    fn two_devices_can_map_the_same_keycode_independently() {
        let module = KeymapModule::with_store(store());
        module
            .call("setMapping", json!({ "from": { "device": "kbd A", "keycode": 1 }, "to": 2 }))
            .unwrap();
        let status = module
            .call("setMapping", json!({ "from": { "device": "kbd B", "keycode": 1 }, "to": 3 }))
            .unwrap();
        assert_eq!(status["mappings"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn removing_a_mapping_drops_only_that_key() {
        let module = KeymapModule::with_store(store());
        module.call("setMapping", json!({ "from": { "keycode": 1 }, "to": 2 })).unwrap();
        module.call("setMapping", json!({ "from": { "keycode": 5 }, "to": 6 })).unwrap();
        let status = module.call("removeMapping", json!({ "keycode": 1 })).unwrap();
        let mappings = status["mappings"].as_array().unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0]["from"]["keycode"], json!(5));
    }

    #[test]
    fn the_substitution_table_is_built_from_current_mappings() {
        let module = KeymapModule::with_store(store());
        module.call("setMapping", json!({ "from": { "keycode": 1 }, "to": 2 })).unwrap();
        module.call("setMapping", json!({ "from": { "keycode": 3 }, "to": 4 })).unwrap();
        let table = module.table();
        assert_eq!(table.get(&1), Some(&2));
        assert_eq!(table.get(&3), Some(&4));
        assert_eq!(table.get(&99), None);
    }

    #[test]
    fn an_invalid_mapping_is_rejected_before_touching_the_config() {
        let module = KeymapModule::with_store(store());
        let err = module.call("setMapping", json!({ "from": {} })).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidParams);
    }

    #[test]
    fn set_enabled_without_the_param_is_invalid_params() {
        let module = KeymapModule::with_store(store());
        let err = module.call("setEnabled", json!({})).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidParams);
    }

    #[test]
    fn unknown_method_is_reported_by_name() {
        let module = KeymapModule::with_store(store());
        let err = module.call("frobnicate", Value::Null).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownMethod);
    }

    #[test]
    fn key_spec_equality_ignores_nothing() {
        assert_ne!(spec(1), KeySpec { device: Some("x".into()), keycode: 1 });
        assert_eq!(spec(1), spec(1));
    }

    #[test]
    fn a_path_outside_dev_input_is_not_a_device() {
        assert!(!raw::device_paths().contains(&PathBuf::from("/dev/null")));
    }
}
