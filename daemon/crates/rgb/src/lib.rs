//! RGB lighting module - ported from the `omen-rgb-linux` Python project
//! (arfelious, GPL-3.0), following the review in
//! `docs/04-rgb-porting-review.md`.
//!
//! | method | params | result |
//! |---|---|---|
//! | `rgb.getCapabilities` | none | a fresh probe of both hardware paths |
//! | `rgb.getStatus` | none | the probe, plus what this daemon last set |
//! | `rgb.setZones` | `{ "zones": [c, c, c, c], "brightness"?: 0-100 }` | the new status |
//! | `rgb.setStatic` | `{ "color": c, "brightness"?: 0-100 }` | the new status |
//! | `rgb.off` | none | the new status |
//! | `rgb.readZones` | none | the four colours the firmware reports |
//! | `rgb.setRestoreOnStart` | `{ "enabled": bool }` | the new status |
//!
//! A colour `c` is `"#rrggbb"` or `[r, g, b]`; see [`color`].
//!
//! ## Only one of the two paths is here
//!
//! The source project drives two unrelated things, and which one a laptop
//! has is not decided by its model name: **per-key RGB** over USB HID
//! (`0d62:54bf`) and a **4-zone light strip** over ACPI-WMI. They share no
//! transport, no privileges and no detection. Both are *probed* - see
//! [`probe`] - and only the light strip is *driven*, because on the one
//! OMEN this project has run on, `lsusb` finds no `0d62` device at all
//! (`dev/FINDINGS.md` §"The test laptop has no per-key RGB keyboard"). The
//! per-key path is step 3 of the porting order, and blocked on hardware
//! rather than on time.
//!
//! ## Nothing here has been confirmed against a light strip
//!
//! The development laptop has no `acpi_call` installed, so every constant
//! in [`lightbar`] is upstream's reverse engineering, carried across and
//! unit-tested for *shape* only. What that buys: when someone installs
//! `acpi_call-dkms`, the only untested thing left is the firmware's answer,
//! and `rgb.getCapabilities` says in words which of the three ways it can
//! be unavailable this machine is in.

use std::sync::{Arc, Mutex};

use pyren_config::{ConfigStore, LoadOutcome};
use pyren_core::{Module, ModuleError, ModuleResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod color;
pub mod lightbar;
pub mod probe;

pub use color::Rgb;
pub use probe::Probe;

/// What is persisted to `rgb.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RgbConfig {
    pub zones: Vec<Rgb>,
    /// A percentage, not a 0-255 level: that is what the protocol takes.
    pub brightness: u8,
    /// Off by default, like every other module's equivalent. Turning a
    /// machine's lights on at boot because they were on last week is a
    /// decision for the user, not for the daemon (`dev/TODO.md` §4).
    pub restore_on_start: bool,
}

impl Default for RgbConfig {
    fn default() -> Self {
        Self {
            zones: vec![Rgb::BLACK; lightbar::ZONES],
            brightness: 100,
            restore_on_start: false,
        }
    }
}

struct State {
    config: RgbConfig,
    /// Whether *this daemon* put the lights where they are. False until
    /// something is written, so `getStatus` never claims a colour the
    /// firmware chose as one we set.
    owned: bool,
    last_error: Option<String>,
    last_save_error: Option<String>,
}

pub struct RgbModule {
    /// Taken once at startup, because `is_supported` is called for every
    /// `core.capabilities` and each probe is an ACPI round trip.
    /// `getCapabilities` re-probes, so installing `acpi_call` and asking
    /// again works without restarting the daemon.
    probe: Probe,
    store: ConfigStore,
    state: Arc<Mutex<State>>,
}

impl RgbModule {
    pub fn new() -> Self {
        Self::with_store(ConfigStore::system())
    }

    pub fn with_store(store: ConfigStore) -> Self {
        let probe = probe::probe();

        let loaded = store.load::<RgbConfig>("rgb");
        match &loaded.outcome {
            LoadOutcome::Loaded => {
                println!("pyren-daemon: rgb config loaded from {}", store.path_for("rgb").display());
            }
            LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                eprintln!(
                    "pyren-daemon: rgb config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                eprintln!(
                    "pyren-daemon: rgb config is version {found}, newer than this build \
                     understands; using defaults and leaving the file alone"
                );
            }
        }
        let mut config = loaded.value;
        // A file edited by hand, or written before ZONES was what it is,
        // must not reach the payload builder the wrong length.
        config.zones.resize(lightbar::ZONES, Rgb::BLACK);
        config.brightness = config.brightness.min(100);

        let restoring = config.restore_on_start && probe.lightbar.present;
        let module = Self {
            probe,
            store,
            state: Arc::new(Mutex::new(State {
                config,
                owned: restoring,
                last_error: None,
                last_save_error: None,
            })),
        };

        if restoring {
            let state = lock(&module.state);
            let (zones, brightness) = (state.config.zones.clone(), state.config.brightness);
            drop(state);
            if let Err(e) = lightbar::set_colors(&zones, brightness) {
                eprintln!("pyren-daemon: could not restore the lightbar: {e}");
                lock(&module.state).last_error = Some(e.to_string());
            }
        }

        module
    }

    /// The probe taken at startup. The daemon prints it, which is the
    /// fastest way to answer "why is there no lighting page".
    pub fn probe(&self) -> &Probe {
        &self.probe
    }

    fn status(&self) -> Value {
        let state = lock(&self.state);
        json!({
            "capabilities": self.probe,
            "zones": state.config.zones,
            "brightness": state.config.brightness,
            "restoreOnStart": state.config.restore_on_start,
            // What is reported is what we wrote, and only if we wrote it.
            // Reading the hardware back is `rgb.readZones`, which is a
            // separate call because it is four ACPI round trips.
            "owned": state.owned,
            "error": state.last_error,
            "saved": state.last_save_error.is_none(),
            "saveError": state.last_save_error,
        })
    }

    fn apply(&self, zones: Vec<Rgb>, brightness: u8) -> ModuleResult {
        // A fresh probe rather than the startup one: the interesting case
        // is exactly the machine where `acpi_call` has just been installed.
        if !lightbar::hp_wmi_present() {
            return Err(ModuleError::Unsupported);
        }

        match lightbar::set_colors(&zones, brightness) {
            Ok(()) => {
                let mut state = lock(&self.state);
                state.config.zones = zones;
                state.config.brightness = brightness;
                state.owned = true;
                state.last_error = None;
                persist(&self.store, &mut state);
            }
            Err(e) => {
                let message = e.to_string();
                lock(&self.state).last_error = Some(message);
                return Err(lightbar_error(e));
            }
        }
        Ok(self.status())
    }

    fn set_restore_on_start(&self, enabled: bool) -> ModuleResult {
        let mut state = lock(&self.state);
        state.config.restore_on_start = enabled;
        persist(&self.store, &mut state);
        drop(state);
        Ok(self.status())
    }

    fn brightness_from(&self, params: &Value) -> u8 {
        match params.get("brightness").and_then(Value::as_i64) {
            Some(value) => lightbar::clamp_brightness(value),
            None => lock(&self.state).config.brightness,
        }
    }
}

impl Default for RgbModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for RgbModule {
    fn id(&self) -> &'static str {
        "rgb"
    }

    fn is_supported(&self) -> bool {
        self.probe.supported
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            // Re-probed rather than cached, so that installing acpi_call
            // and asking again is a complete workflow.
            "getCapabilities" => serde_json::to_value(probe::probe())
                .map_err(|e| ModuleError::Internal(e.to_string())),

            "getStatus" => Ok(self.status()),

            "setZones" => {
                let zones = params.get("zones").cloned().ok_or_else(|| {
                    ModuleError::InvalidParams(
                        "params.zones is required: four colours, '#rrggbb' or [r, g, b]".into(),
                    )
                })?;
                let zones: Vec<Rgb> = serde_json::from_value(zones)
                    .map_err(|e| ModuleError::InvalidParams(format!("invalid zones: {e}")))?;
                if zones.is_empty() || zones.len() > lightbar::ZONES {
                    return Err(ModuleError::InvalidParams(format!(
                        "params.zones takes 1 to {} colours, not {}",
                        lightbar::ZONES,
                        zones.len()
                    )));
                }
                let brightness = self.brightness_from(&params);
                // Short is padded here rather than in the payload builder,
                // so the stored config and the buffer say the same thing.
                let mut zones = zones;
                zones.resize(lightbar::ZONES, Rgb::BLACK);
                self.apply(zones, brightness)
            }

            "setStatic" => {
                let color = params.get("color").cloned().ok_or_else(|| {
                    ModuleError::InvalidParams(
                        "params.color is required: '#rrggbb' or [r, g, b]".into(),
                    )
                })?;
                let color: Rgb = serde_json::from_value(color)
                    .map_err(|e| ModuleError::InvalidParams(format!("invalid colour: {e}")))?;
                let brightness = self.brightness_from(&params);
                self.apply(vec![color; lightbar::ZONES], brightness)
            }

            // Brightness 0 as well as black: on some firmwares one alone
            // leaves a dim glow, and "off" should mean off.
            "off" => self.apply(vec![Rgb::BLACK; lightbar::ZONES], 0),

            "readZones" => {
                let colors = lightbar::read_colors().map_err(lightbar_error)?;
                Ok(json!({ "zones": colors }))
            }

            "setRestoreOnStart" => {
                let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
                    ModuleError::InvalidParams("params.enabled must be a boolean".into())
                })?;
                self.set_restore_on_start(enabled)
            }

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

/// Hardware failures, translated for the socket.
///
/// The one worth spelling out is `NotLoaded`: it is **not** `notCapable`,
/// which a client is entitled to read as "this machine will never do it".
/// A missing `acpi_call` is one `pacman -S` away, so it goes out as a
/// plain failure whose message names the package.
fn lightbar_error(e: lightbar::LightbarError) -> ModuleError {
    use pyren_core::acpi::AcpiError;
    let message = e.to_string();
    match e {
        lightbar::LightbarError::Acpi(AcpiError::NotLoaded) => ModuleError::Failed(message),
        lightbar::LightbarError::Acpi(AcpiError::PermissionDenied) => {
            ModuleError::PermissionDenied(message)
        }
        lightbar::LightbarError::Acpi(AcpiError::Io(_)) => ModuleError::Io(message),
        // The call worked and the firmware said no. That is the machine
        // saying it has no light strip, which no privilege changes.
        lightbar::LightbarError::Refused(_) => ModuleError::NotCapable(message),
        lightbar::LightbarError::Unreadable(_) => ModuleError::Failed(message),
    }
}

fn persist(store: &ConfigStore, state: &mut State) {
    match store.save("rgb", &state.config) {
        Ok(()) => state.last_save_error = None,
        Err(e) => {
            eprintln!("pyren-daemon: could not save rgb config: {e}");
            state.last_save_error = Some(e.to_string());
        }
    }
}

fn lock(state: &Arc<Mutex<State>>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module() -> RgbModule {
        let dir = std::env::temp_dir()
            .join(format!("pyren-rgb-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        RgbModule::with_store(ConfigStore::at(dir))
    }

    #[test]
    fn capabilities_report_both_paths_whether_or_not_either_is_here() {
        let reply = module().call("getCapabilities", Value::Null).expect("probing cannot fail");
        assert!(reply.get("perKey").is_some(), "both paths are reported, not just the driven one");
        assert!(reply.get("lightbar").is_some());
        assert_eq!(reply["perKey"]["ported"], false);
    }

    /// Bad arguments must be rejected before anything reaches
    /// `/proc/acpi/call`, so that a typo cannot become a hardware write.
    #[test]
    fn malformed_colours_are_refused_as_invalid_params() {
        let module = module();
        for (method, params) in [
            ("setZones", json!({})),
            ("setZones", json!({ "zones": [] })),
            ("setZones", json!({ "zones": ["#ff0000", "#00ff00", "#0000ff", "#fff", "#000"] })),
            ("setZones", json!({ "zones": ["not a colour"] })),
            ("setStatic", json!({})),
            ("setStatic", json!({ "color": "#zzz" })),
            ("setRestoreOnStart", json!({ "enabled": "yes" })),
        ] {
            let error = module.call(method, params.clone()).expect_err("should be refused");
            assert_eq!(
                error.kind(),
                pyren_core::ErrorKind::InvalidParams,
                "{method} {params}: {error}"
            );
        }
    }

    #[test]
    fn an_unknown_method_is_its_own_kind() {
        let error = module().call("rainbow", Value::Null).unwrap_err();
        assert_eq!(error.kind(), pyren_core::ErrorKind::UnknownMethod);
    }

    /// The config is what a hand-edited file could get wrong, and a
    /// three-zone list reaching the payload builder would silently leave
    /// zone four black rather than saying anything.
    #[test]
    fn a_config_with_the_wrong_number_of_zones_is_squared_up_on_load() {
        let dir = std::env::temp_dir().join(format!("pyren-rgb-resize-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::at(&dir);
        store
            .save("rgb", &json!({ "zones": ["#ff0000"], "brightness": 240 }))
            .expect("a temp dir is writable");

        let module = RgbModule::with_store(store);
        let status = module.status();
        assert_eq!(status["zones"].as_array().unwrap().len(), lightbar::ZONES);
        assert_eq!(status["zones"][0], "#ff0000");
        assert_eq!(status["zones"][3], "#000000");
        assert_eq!(status["brightness"], 100, "brightness is a percentage");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing is written to the lights until someone asks - the same rule
    /// the fan module follows about the fans.
    #[test]
    fn a_fresh_module_does_not_claim_to_own_the_lights() {
        let status = module().status();
        assert_eq!(status["owned"], false);
        assert_eq!(status["restoreOnStart"], false);
    }
}
