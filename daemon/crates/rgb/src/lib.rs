//! RGB lighting module - ported from the `omen-rgb-linux` Python project
//! (arfelious, GPL-3.0). The wire protocol and its three dialects are in
//! `docs/01-ipc-protocol.md` §"There is no single OMEN lighting protocol".
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
//! | `rgb.setDialect` | `{ "dialect": "auto" \| id }` | the new status |
//! | `rgb.listEffects` | none | the effects, and the speed and fps ranges |
//! | `rgb.setEffect` | `{ "effect": e, "brightness"?: 0-100, "fps"?: 5-60 }` | the new status |
//! | `rgb.stopEffect` | none | the new status, the zones back on |
//! | `rgb.setBrightness` | `{ "brightness": 0-100 }` | the new status |
//! | `rgb.powerOn` | `{ "ifEnabled"?: bool }` | the new status, after the sweep in |
//! | `rgb.powerOff` | `{ "ifEnabled"?: bool }` | the new status, after the sweep out |
//! | `rgb.setPowerAnimation` | `{ "enabled": bool }` | the new status |
//! | `rgb.setBatteryFps` | `{ "fps": 0-60 }` | the new status; 0 pauses effects on battery |
//!
//! An effect `e` is `{ "kind", "colors"?, "speed"?: 1-10, "direction"? }`;
//! see [`effects`]. `setZones`, `setStatic` and `off` stop a running effect.
//!
//! A colour `c` is `"#rrggbb"` or `[r, g, b]`; see [`color`].
//!
//! ## There is no single OMEN lighting protocol
//!
//! There are three ways a machine of this family can be lit, they share
//! nothing but the vendor, and which one a laptop speaks is not decided by
//! its model name. All three are implemented in [`dialect`], all three are
//! probed, and the first that answers a **read** is used - with a setting
//! to pin one by hand when the automatic choice is wrong. That setting is
//! not decoration: auto can only pick a dialect this build can read, and
//! the person at the keyboard can see whether the lights actually changed.
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
//! ## Confirmed against the hardware (2026-09-04)
//!
//! On the one OMEN this has run on, the firmware speaks the [`fourzone`]
//! dialect: `rgb.readZones` reads all four back and a write is visible on
//! the keyboard. Two caveats, both in `dev/FINDINGS.md`: the [`lightbar`]
//! dialect this project ported *first* answers `PASS` and does nothing
//! here, and `acpi_call` truncates the reply so `fourZone`'s read reports
//! the fourth zone black (the colour written to it is real). The dialect
//! without either problem is [`kernel_zones`], which needs the out-of-tree
//! `omen-rgb-keyboard` module. `rgb.getCapabilities` says in words which
//! dialects this machine answered.

use std::sync::{Arc, Mutex};

use pyren_config::{ConfigStore, LoadOutcome};
use pyren_core::{log_info, log_warn};
use pyren_core::{msg, ErrorKind, Module, ModuleError, ModuleResult, Msg};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod color;
pub mod dialect;
pub mod effects;
pub mod fourzone;
pub mod kernel_zones;
pub mod lightbar;
pub mod probe;

pub use color::Rgb;
pub use dialect::{Dialect, DialectError, Selection};
pub use effects::{Animator, Effect, EffectKind, Transition};
pub use probe::Probe;

/// Four zones. Not a configurable number: every dialect's buffer has the
/// count and the twelve colour bytes baked into its layout.
pub const ZONES: usize = 4;

/// Brightness applied to the colours themselves.
///
/// Two of the three dialects have no brightness field - their reference
/// drivers scale in software and so does this - so scaling here is what
/// makes one brightness slider mean the same thing on all three. The
/// dialect that *does* have a field ([`Dialect::Lightbar`]) gets the
/// unscaled colours and the percentage, and lets the firmware do it.
pub fn scale(colors: &[Rgb], brightness: u8) -> Vec<Rgb> {
    let percent = u16::from(brightness.min(100));
    colors
        .iter()
        .map(|c| {
            let at = |v: u8| ((u16::from(v) * percent) / 100) as u8;
            Rgb::new(at(c.r), at(c.g), at(c.b))
        })
        .collect()
}

/// Brightness is a percentage in these protocols, not a 0-255 level.
pub fn clamp_brightness(value: i64) -> u8 {
    value.clamp(0, 100) as u8
}

/// What is persisted to `rgb.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RgbConfig {
    pub zones: Vec<Rgb>,
    /// A percentage, not a 0-255 level: that is what the protocol takes.
    pub brightness: u8,
    /// Which dialect to speak. `Auto` unless somebody has picked.
    pub dialect: Selection,
    /// Off by default, like every other module's equivalent. Turning a
    /// machine's lights on at boot because they were on last week is a
    /// decision for the user, not for the daemon (`dev/TODO.md` §3).
    pub restore_on_start: bool,
    /// The running effect, or none for the static `zones`. Restored with
    /// the zones when `restore_on_start` is on.
    pub effect: Option<Effect>,
    /// Frames a second for an effect.
    pub fps: u8,
    /// Sweep the lights in when the daemon restores them and out when the
    /// machine shuts down or suspends. Off by default for the same reason
    /// `restore_on_start` is.
    pub power_animation: bool,
    /// The most frames a second an effect gets on battery; 0 pauses it
    /// there. Each frame is an EC transaction, and the cost scales with
    /// the rate (`dev/FINDINGS.md` §"Lighting effects").
    pub battery_fps: u8,
}

impl Default for RgbConfig {
    fn default() -> Self {
        Self {
            zones: vec![Rgb::BLACK; ZONES],
            brightness: 100,
            dialect: Selection::Auto,
            restore_on_start: false,
            effect: None,
            fps: effects::FPS_DEFAULT,
            power_animation: false,
            battery_fps: DEFAULT_BATTERY_FPS,
        }
    }
}

/// Half the default rate: still smooth on four zones, half the cost.
pub const DEFAULT_BATTERY_FPS: u8 = 15;

/// What the machine is doing that bears on an animation. Told by the
/// daemon, which watches the charger and the lid - the power module owns
/// the first and neither is this module's business to poll.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Conditions {
    pub on_battery: bool,
    pub lid_closed: bool,
}

struct State {
    config: RgbConfig,
    conditions: Conditions,
    /// Whether *this daemon* put the lights where they are. False until
    /// something is written, so `getStatus` never claims a colour the
    /// firmware chose as one we set.
    owned: bool,
    /// Put out by `powerOff`. What `powerOn` with `ifEnabled` checks, so a
    /// resume only lights a keyboard that a suspend darkened.
    dark: bool,
    last_error: Option<Msg>,
    last_save_error: Option<String>,
}

/// Cheap to clone: every field is shared. The daemon keeps a clone for
/// its SIGTERM handler, which is the one caller outside the registry.
#[derive(Clone)]
pub struct RgbModule {
    /// The last probe taken, not a fresh one: `is_supported` is called for
    /// every `core.capabilities` and asking the firmware is an ACPI round
    /// trip on the file the fan cleaner shares.
    ///
    /// It is not frozen at startup either, because that made `getStatus`
    /// say "acpi_call is not installed" on a machine where it had been
    /// installed ten minutes ago. [`RgbModule::current_probe`] re-takes it
    /// when - and only when - the three interface facts have changed, all
    /// three of which are a `stat` rather than a call.
    probe: Arc<Mutex<Probe>>,
    store: ConfigStore,
    state: Arc<Mutex<State>>,
    /// Never locked while holding `state`: a failing animation takes the
    /// state lock from its own thread, and `stop` waits for that thread.
    animator: Arc<Mutex<Animator>>,
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
                log_info!("rgb config loaded from {}", store.path_for("rgb").display());
            }
            LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                log_warn!(
                    "rgb config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                log_warn!(
                    "rgb config is version {found}, newer than this build \
                     understands; using defaults and leaving the file alone"
                );
            }
        }
        let mut config = loaded.value;
        // A file edited by hand, or written before ZONES was what it is,
        // must not reach the payload builder the wrong length.
        config.zones.resize(ZONES, Rgb::BLACK);
        config.brightness = config.brightness.min(100);
        config.fps = config.fps.clamp(effects::FPS_MIN, effects::FPS_MAX);
        config.battery_fps = config.battery_fps.min(effects::FPS_MAX);
        config.effect = config.effect.map(Effect::normalised);

        let restoring = config.restore_on_start && probe.lighting.present;
        let module = Self {
            probe: Arc::new(Mutex::new(probe)),
            store,
            state: Arc::new(Mutex::new(State {
                config,
                conditions: Conditions::default(),
                owned: restoring,
                dark: false,
                last_error: None,
                last_save_error: None,
            })),
            animator: Arc::new(Mutex::new(Animator::new())),
        };

        if restoring {
            let state = lock(&module.state);
            let (zones, brightness) = (state.config.zones.clone(), state.config.brightness);
            let effect = state.config.effect.clone();
            let animate = state.config.power_animation;
            drop(state);
            let chosen = module.chosen_dialect(&module.current_probe());
            match (chosen, effect) {
                // Blocks startup for the length of the sweep, a second
                // and a bit. Worth it: the alternative is the socket
                // answering while the lights are still coming up, and a
                // `setZones` in that second landing under the sweep.
                (Some(_), _) if animate => {
                    if let Err(e) = module.power_on() {
                        log_warn!("could not bring the lights up: {e}");
                    }
                }
                (Some(_), Some(effect)) => {
                    if let Err(e) = module.start_effect(effect) {
                        log_warn!("could not restore the lighting effect: {e}");
                    }
                }
                (Some(dialect), None) => {
                    if let Err(e) = dialect.write_colors(&zones, brightness) {
                        log_warn!("could not restore the lights: {e}");
                        lock(&module.state).last_error = Some(e.to_msg());
                    }
                }
                (None, _) => log_warn!(
                    "not restoring the lights: no lighting dialect answered"
                ),
            }
        }

        module
    }

    /// What lighting this machine has, re-asked only if something about
    /// the interfaces has moved since the last time. The daemon prints it
    /// at startup, which is the fastest way to answer "why is there no
    /// lighting page".
    ///
    /// The cheap half - is `hp-wmi` there, is `/proc/acpi/call` there, is
    /// the module installed - costs three `stat`s and is exactly what
    /// changes while the daemon runs. The expensive half is asking the
    /// firmware, and that only becomes a *different* question when the
    /// cheap half has changed. So this is a poll-safe accessor, and
    /// `getCapabilities` stays the one that always asks.
    pub fn probe(&self) -> Probe {
        self.current_probe()
    }

    fn current_probe(&self) -> Probe {
        let mut cached = lock_probe(&self.probe);
        if probe::interfaces() != cached.lighting.interfaces() {
            *cached = probe::probe();
        }
        cached.clone()
    }

    /// A fresh probe, always, replacing the cached one. What
    /// `rgb.getCapabilities` is.
    fn reprobe(&self) -> Probe {
        let fresh = probe::probe();
        *lock_probe(&self.probe) = fresh.clone();
        fresh
    }

    fn status(&self) -> Value {
        // Before the state lock, not inside it: this can take the probe
        // lock and there is no reason to hold both.
        let probe = self.current_probe();
        // Both of these take the state lock, and this one is not
        // reentrant: resolving the dialect before the guard rather than
        // under it is the difference between a status read and a hang.
        let active = self.chosen_dialect(&probe);
        let animating = lock_animator(&self.animator).is_running();
        let state = lock(&self.state);
        json!({
            "capabilities": probe,
            // Which of the three ways of talking to the lights is in use,
            // and whether that was worked out or chosen. A UI that shows
            // only the first would leave a user unable to tell an
            // automatic pick from their own.
            "dialect": state.config.dialect.id(),
            "activeDialect": active.map(|d| d.id()),
            "zones": state.config.zones,
            "brightness": state.config.brightness,
            "restoreOnStart": state.config.restore_on_start,
            // What was asked for, and whether it is actually moving: an
            // effect whose writes kept failing stops itself and leaves
            // the reason in `error`.
            "effect": state.config.effect,
            "effectRunning": animating,
            "fps": state.config.fps,
            "powerAnimation": state.config.power_animation,
            // Put out by `powerOff` and not yet back.
            "dark": state.dark,
            "batteryFps": state.config.battery_fps,
            // Why an effect is running slower than `fps`, or not at all:
            // "lid" (paused), "battery" (capped or paused), or null.
            "throttled": throttle_reason(&state),
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
        // is exactly the machine where `acpi_call` has just been installed
        // - or where the user has just pinned a different dialect.
        let probe = self.current_probe();
        let Some(dialect) = self.chosen_dialect(&probe) else {
            return Err(ModuleError::Unsupported);
        };
        // Before the write, or the effect's next frame lands on top of it.
        lock_animator(&self.animator).stop();

        match dialect.write_colors(&zones, brightness) {
            Ok(()) => {
                let mut state = lock(&self.state);
                state.config.zones = zones;
                state.config.brightness = brightness;
                state.config.effect = None;
                state.dark = false;
                state.owned = true;
                state.last_error = None;
                persist(&self.store, &mut state);
            }
            Err(e) => {
                lock(&self.state).last_error = Some(e.to_msg());
                return Err(dialect_error(e));
            }
        }
        Ok(self.status())
    }

    /// Starts `effect` on the chosen dialect. Does not persist it; the
    /// callers that should, do.
    fn start_effect(&self, effect: Effect) -> Result<(), ModuleError> {
        let probe = self.current_probe();
        let dialect = self.chosen_dialect(&probe).ok_or(ModuleError::Unsupported)?;
        let (brightness, fps) = {
            let state = lock(&self.state);
            (state.config.brightness, state.config.fps)
        };

        let mut animator = lock_animator(&self.animator);
        // Stopped before the sink is made: making the four-zone one is a
        // read, and a frame of the old effect must not be what it reads.
        animator.stop();
        let sink = dialect.frames().map_err(|e| {
            lock(&self.state).last_error = Some(e.to_msg());
            dialect_error(e)
        })?;

        let state = Arc::clone(&self.state);
        animator.start(effect, brightness, fps, sink, move |e| {
            log_warn!("the lighting effect stopped: {e}");
            lock(&state).last_error = Some(e.to_msg());
        });
        drop(animator);

        let mut state = lock(&self.state);
        state.owned = true;
        state.dark = false;
        state.last_error = None;
        Ok(())
    }

    /// Sweeps the lights in, from black to what the config says - the
    /// static zones, or the first frame of the effect, which then starts
    /// from exactly there.
    fn power_on(&self) -> Result<(), ModuleError> {
        let probe = self.current_probe();
        let dialect = self.chosen_dialect(&probe).ok_or(ModuleError::Unsupported)?;
        let (zones, brightness, fps, effect) = {
            let state = lock(&self.state);
            let c = &state.config;
            (c.zones.clone(), c.brightness, c.fps, c.effect.clone())
        };
        lock_animator(&self.animator).stop();

        let target = match &effect {
            Some(e) => effects::frame(e, 0.0),
            None => zones_array(&zones),
        };
        let mut sink = dialect.frames().map_err(|e| self.failed(e))?;
        effects::play(&mut sink, brightness, fps, Transition::DURATION, |k, _| {
            Transition::PowerOn.apply(&target, k)
        })
        .map_err(|e| self.failed(e))?;
        drop(sink);

        match effect {
            Some(e) => self.start_effect(e)?,
            None => {
                let mut state = lock(&self.state);
                state.owned = true;
                state.dark = false;
                state.last_error = None;
            }
        }
        Ok(())
    }

    /// Sweeps the lights out to black. A running effect carries on moving
    /// under the sweep rather than freezing on the frame it was showing.
    ///
    /// The effect stays in the config: this is the lights going off, not
    /// somebody choosing a different colour, and `powerOn` brings it back.
    fn power_off(&self) -> Result<(), ModuleError> {
        let probe = self.current_probe();
        let dialect = self.chosen_dialect(&probe).ok_or(ModuleError::Unsupported)?;
        let (zones, brightness, fps) = {
            let state = lock(&self.state);
            (zones_array(&state.config.zones), state.config.brightness, state.config.fps)
        };
        let running = {
            let mut animator = lock_animator(&self.animator);
            let running = animator.current();
            animator.stop();
            running
        };

        let mut sink = dialect.frames().map_err(|e| self.failed(e))?;
        effects::play(&mut sink, brightness, fps, Transition::DURATION, |k, t| {
            let base = match &running {
                Some((effect, into)) => effects::frame(effect, into + t),
                None => zones,
            };
            Transition::PowerOff.apply(&base, k)
        })
        .map_err(|e| self.failed(e))?;

        let mut state = lock(&self.state);
        state.owned = true;
        state.dark = true;
        state.last_error = None;
        Ok(())
    }

    /// Records a hardware failure for `getStatus` and turns it into the
    /// socket's error.
    fn failed(&self, e: DialectError) -> ModuleError {
        lock(&self.state).last_error = Some(e.to_msg());
        dialect_error(e)
    }

    /// What the daemon does on its way out, called from its SIGTERM
    /// handler. Never fails: there is nobody left to tell.
    ///
    /// - The machine shutting down, with the power animation on: the
    ///   sweep out, which is the one the user asked for.
    /// - Anything else with an effect running - the service being stopped
    ///   or restarted: the static zones, so the keyboard is not left on
    ///   whichever frame the thread was killed in.
    /// - Otherwise nothing, like before there was a handler at all.
    pub fn on_exit(&self, machine_stopping: bool) {
        let animate = lock(&self.state).config.power_animation;
        if machine_stopping && animate {
            if let Err(e) = self.power_off() {
                log_warn!("could not play the power-off sweep: {e}");
            }
            return;
        }
        let mut animator = lock_animator(&self.animator);
        if !animator.is_running() {
            return;
        }
        animator.stop();
        drop(animator);
        let (zones, brightness) = {
            let state = lock(&self.state);
            (state.config.zones.clone(), state.config.brightness)
        };
        if let Some(dialect) = self.chosen_dialect(&self.current_probe()) {
            if let Err(e) = dialect.write_colors(&zones, brightness) {
                log_warn!("could not put the static colours back: {e}");
            }
        }
    }

    /// `powerOn` / `powerOff`. With `ifEnabled` they are what the suspend
    /// hook calls: nothing unless the power animation is on, and a power
    /// on only after a power off.
    fn power(&self, on: bool, if_enabled: bool) -> ModuleResult {
        if if_enabled {
            let state = lock(&self.state);
            let wanted = state.config.power_animation && (!on || state.dark);
            drop(state);
            if !wanted {
                return Ok(self.status());
            }
        }
        if on {
            self.power_on()?;
        } else {
            self.power_off()?;
        }
        Ok(self.status())
    }

    /// The daemon's watcher reports the charger and the lid here. Cheap
    /// enough to call every few seconds; only a change does anything.
    pub fn set_conditions(&self, conditions: Conditions) {
        let mut state = lock(&self.state);
        if state.conditions == conditions {
            return;
        }
        state.conditions = conditions;
        drop(state);
        self.apply_throttle();
    }

    /// Hands the animator the limits that follow from the conditions and
    /// the battery setting.
    fn apply_throttle(&self) {
        let (conditions, battery_fps) = {
            let state = lock(&self.state);
            (state.conditions, state.config.battery_fps)
        };
        let paused = conditions.lid_closed || (conditions.on_battery && battery_fps == 0);
        let limit = (conditions.on_battery && battery_fps > 0).then_some(battery_fps);
        let animator = lock_animator(&self.animator);
        animator.set_paused(paused);
        animator.set_limit(limit);
    }

    fn set_battery_fps(&self, fps: u8) -> ModuleResult {
        let mut state = lock(&self.state);
        state.config.battery_fps = fps.min(effects::FPS_MAX);
        persist(&self.store, &mut state);
        drop(state);
        self.apply_throttle();
        Ok(self.status())
    }

    fn set_power_animation(&self, enabled: bool) -> ModuleResult {
        let mut state = lock(&self.state);
        state.config.power_animation = enabled;
        persist(&self.store, &mut state);
        drop(state);
        Ok(self.status())
    }

    fn set_effect(&self, effect: Effect, brightness: Option<u8>, fps: Option<u8>) -> ModuleResult {
        let effect = effect.normalised();
        {
            let mut state = lock(&self.state);
            if let Some(b) = brightness {
                state.config.brightness = b;
            }
            if let Some(f) = fps {
                state.config.fps = f;
            }
        }
        self.start_effect(effect.clone())?;
        let mut state = lock(&self.state);
        state.config.effect = Some(effect);
        persist(&self.store, &mut state);
        drop(state);
        Ok(self.status())
    }

    /// Back to the static zones, which are written again: the last frame
    /// of an effect is not a colour anybody chose.
    fn stop_effect(&self) -> ModuleResult {
        let (zones, brightness) = {
            let state = lock(&self.state);
            (state.config.zones.clone(), state.config.brightness)
        };
        self.apply(zones, brightness)
    }

    fn set_brightness(&self, brightness: u8) -> ModuleResult {
        let (running, zones) = {
            let state = lock(&self.state);
            (state.config.effect.is_some(), state.config.zones.clone())
        };
        let animator = lock_animator(&self.animator);
        if running && animator.is_running() {
            animator.set_brightness(brightness);
            drop(animator);
            let mut state = lock(&self.state);
            state.config.brightness = brightness;
            persist(&self.store, &mut state);
            drop(state);
            return Ok(self.status());
        }
        drop(animator);
        self.apply(zones, brightness)
    }

    /// The dialect a call would go through: the user's if they pinned one,
    /// otherwise the first that answered.
    fn chosen_dialect(&self, probe: &Probe) -> Option<Dialect> {
        lock(&self.state).config.dialect.resolve(&probe.lighting.dialects)
    }

    fn set_dialect(&self, choice: Selection) -> ModuleResult {
        let mut state = lock(&self.state);
        state.config.dialect = choice;
        persist(&self.store, &mut state);
        drop(state);
        // Re-probed, because pinning a dialect is exactly the moment
        // somebody wants to know whether it answers.
        self.reprobe();
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
            Some(value) => clamp_brightness(value),
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
        self.current_probe().supported
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            // Re-probed rather than cached, so that installing acpi_call
            // and asking again is a complete workflow.
            "getCapabilities" => serde_json::to_value(self.reprobe())
                .map_err(|e| ModuleError::Internal(e.to_string())),

            "getStatus" => Ok(self.status()),

            "setZones" => {
                let zones = params.get("zones").cloned().ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "rgb.err.zonesRequired",
                            "params.zones is required: four colours, '#rrggbb' or [r, g, b]"
                        ),
                    )
                })?;
                let zones: Vec<Rgb> = serde_json::from_value(zones)
                    .map_err(|e| ModuleError::InvalidParams(format!("invalid zones: {e}")))?;
                if zones.is_empty() || zones.len() > ZONES {
                    return Err(ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "rgb.err.zonesCount",
                            { "max" => ZONES, "got" => zones.len() },
                            "params.zones takes 1 to {max} colours, not {got}"
                        ),
                    ));
                }
                let brightness = self.brightness_from(&params);
                // Short is padded here rather than in the payload builder,
                // so the stored config and the buffer say the same thing.
                let mut zones = zones;
                zones.resize(ZONES, Rgb::BLACK);
                self.apply(zones, brightness)
            }

            "setStatic" => {
                let color = params.get("color").cloned().ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "rgb.err.colorRequired",
                            "params.color is required: '#rrggbb' or [r, g, b]"
                        ),
                    )
                })?;
                let color: Rgb = serde_json::from_value(color)
                    .map_err(|e| ModuleError::InvalidParams(format!("invalid colour: {e}")))?;
                let brightness = self.brightness_from(&params);
                self.apply(vec![color; ZONES], brightness)
            }

            // Brightness 0 as well as black: on some firmwares one alone
            // leaves a dim glow, and "off" should mean off.
            "off" => self.apply(vec![Rgb::BLACK; ZONES], 0),

            "readZones" => {
                let probe = self.current_probe();
                let dialect = self.chosen_dialect(&probe).ok_or(ModuleError::Unsupported)?;
                let colors = dialect.read_colors().map_err(dialect_error)?;
                Ok(json!({ "zones": colors, "dialect": dialect.id() }))
            }

            // Pinning one by hand. `auto` puts it back.
            "setDialect" => {
                let id = params.get("dialect").and_then(Value::as_str).ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "rgb.err.dialectRequired",
                            "params.dialect is required: 'auto', or one of the ids in \
                             getCapabilities"
                        ),
                    )
                })?;
                let choice = Selection::from_id(id).ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "rgb.err.dialectUnknown",
                            { "got" => id.to_string() },
                            "there is no lighting dialect called {got}"
                        ),
                    )
                })?;
                self.set_dialect(choice)
            }

            "setRestoreOnStart" => {
                let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
                    ModuleError::InvalidParams("params.enabled must be a boolean".into())
                })?;
                self.set_restore_on_start(enabled)
            }

            "listEffects" => Ok(json!({
                "effects": EffectKind::ALL.iter().map(|k| json!({
                    "id": k.id(),
                    "usesColors": k.uses_colors(),
                    "defaults": Effect::new(*k).normalised(),
                })).collect::<Vec<_>>(),
                "maxColors": effects::MAX_COLORS,
                "speed": { "min": effects::SPEED_MIN, "max": effects::SPEED_MAX, "default": effects::SPEED_DEFAULT },
                "fps": { "min": effects::FPS_MIN, "max": effects::FPS_MAX, "default": effects::FPS_DEFAULT },
            })),

            "setEffect" => {
                let effect = params.get("effect").cloned().ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "rgb.err.effectRequired",
                            "params.effect is required: { kind, colors?, speed?, direction? }"
                        ),
                    )
                })?;
                let effect: Effect = serde_json::from_value(effect)
                    .map_err(|e| ModuleError::InvalidParams(format!("invalid effect: {e}")))?;
                let brightness = params.get("brightness").and_then(Value::as_i64).map(clamp_brightness);
                let fps = params.get("fps").and_then(Value::as_i64).map(effects::clamp_fps);
                self.set_effect(effect, brightness, fps)
            }

            "stopEffect" => self.stop_effect(),

            "powerOn" | "powerOff" => {
                let if_enabled = params.get("ifEnabled").and_then(Value::as_bool).unwrap_or(false);
                self.power(method == "powerOn", if_enabled)
            }

            "setBatteryFps" => {
                let fps = params.get("fps").and_then(Value::as_i64).ok_or_else(|| {
                    ModuleError::InvalidParams("params.fps must be a number, 0-60".into())
                })?;
                self.set_battery_fps(fps.clamp(0, i64::from(effects::FPS_MAX)) as u8)
            }

            "setPowerAnimation" => {
                let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
                    ModuleError::InvalidParams("params.enabled must be a boolean".into())
                })?;
                self.set_power_animation(enabled)
            }

            "setBrightness" => {
                let brightness = params.get("brightness").and_then(Value::as_i64).ok_or_else(|| {
                    ModuleError::InvalidParams("params.brightness must be a number, 0-100".into())
                })?;
                self.set_brightness(clamp_brightness(brightness))
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
fn dialect_error(e: DialectError) -> ModuleError {
    use pyren_core::acpi::AcpiError;
    let kind = match &e {
        DialectError::Acpi(AcpiError::NotLoaded) => ErrorKind::Failed,
        DialectError::Acpi(AcpiError::PermissionDenied) => ErrorKind::PermissionDenied,
        DialectError::Acpi(AcpiError::Io(_)) => ErrorKind::Io,
        DialectError::NeedsRoot => ErrorKind::PermissionDenied,
        DialectError::Io(_) => ErrorKind::Io,
        // The call worked and the firmware said no. That is this machine
        // saying it does not speak *this dialect* - which is why it is
        // worth trying another before believing it about the hardware.
        DialectError::Refused(_) | DialectError::ReturnCode(_) => ErrorKind::NotCapable,
        DialectError::Unreadable(_) => ErrorKind::Failed,
    };
    ModuleError::localised(kind, e.to_msg())
}

fn persist(store: &ConfigStore, state: &mut State) {
    match store.save("rgb", &state.config) {
        Ok(()) => state.last_save_error = None,
        Err(e) => {
            log_warn!("could not save rgb config: {e}");
            state.last_save_error = Some(e.to_string());
        }
    }
}

fn lock(state: &Arc<Mutex<State>>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn throttle_reason(state: &State) -> Option<&'static str> {
    if state.conditions.lid_closed {
        Some("lid")
    } else if state.conditions.on_battery && state.config.battery_fps < state.config.fps {
        Some("battery")
    } else {
        None
    }
}

fn zones_array(zones: &[Rgb]) -> [Rgb; ZONES] {
    let mut out = [Rgb::BLACK; ZONES];
    for (slot, c) in out.iter_mut().zip(zones) {
        *slot = *c;
    }
    out
}

fn lock_animator(animator: &Mutex<Animator>) -> std::sync::MutexGuard<'_, Animator> {
    animator.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_probe(probe: &Mutex<Probe>) -> std::sync::MutexGuard<'_, Probe> {
    probe.lock().unwrap_or_else(|e| e.into_inner())
}

/// Test-only serialisation of `PYREN_ACPI_CALL`.
///
/// One test redirects the interface at a writable temp file to read back
/// exactly what went down the wire; several others ask what this machine
/// answers. The variable is process-global and the harness runs tests in
/// parallel threads, so on a machine with `acpi_call` loaded the second
/// group can run *while* the first has the interface pointed at a plain
/// file - and a plain file accepts everything, so "nobody was asked" and
/// "the firmware refused" swap places. Both groups take this lock: one to
/// redirect, one to be sure nothing is redirected under it.
#[cfg(test)]
pub(crate) mod testenv {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    /// Holds the lock, and puts the variable back the way it was.
    pub(crate) struct AcpiEnv {
        // A test that panicked while holding the lock has already failed;
        // the next one still needs the redirection to work.
        _guard: MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
        redirected: bool,
    }

    /// Points `acpi_call` at `path` for as long as the guard lives.
    pub(crate) fn redirect(path: &Path) -> AcpiEnv {
        let mut env = real();
        std::env::set_var("PYREN_ACPI_CALL", path);
        env.redirected = true;
        env
    }

    /// Takes the lock without changing anything - what a test that asks
    /// the *real* machine needs, so no redirection is in force while it
    /// runs.
    pub(crate) fn real() -> AcpiEnv {
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        AcpiEnv {
            _guard: guard,
            previous: std::env::var_os("PYREN_ACPI_CALL"),
            redirected: false,
        }
    }

    impl Drop for AcpiEnv {
        fn drop(&mut self) {
            if !self.redirected {
                return;
            }
            match self.previous.take() {
                Some(previous) => std::env::set_var("PYREN_ACPI_CALL", previous),
                None => std::env::remove_var("PYREN_ACPI_CALL"),
            }
        }
    }
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
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let reply = module().call("getCapabilities", Value::Null).expect("probing cannot fail");
        assert!(reply.get("perKey").is_some(), "both paths are reported, not just the driven one");
        assert!(reply.get("lighting").is_some());
        assert_eq!(reply["perKey"]["ported"], false);
        // Every dialect is listed even where none of them answered: a UI
        // offering the manual override has to be able to name the choices.
        let dialects = reply["lighting"]["dialects"].as_array().expect("a list of dialects");
        assert_eq!(dialects.len(), dialect::ORDER.len());
    }

    /// Bad arguments must be rejected before anything reaches
    /// `/proc/acpi/call`, so that a typo cannot become a hardware write.
    #[test]
    fn malformed_colours_are_refused_as_invalid_params() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module();
        for (method, params) in [
            ("setZones", json!({})),
            ("setZones", json!({ "zones": [] })),
            ("setZones", json!({ "zones": ["#ff0000", "#00ff00", "#0000ff", "#fff", "#000"] })),
            ("setZones", json!({ "zones": ["not a colour"] })),
            ("setStatic", json!({})),
            ("setStatic", json!({ "color": "#zzz" })),
            ("setRestoreOnStart", json!({ "enabled": "yes" })),
            ("setEffect", json!({})),
            ("setEffect", json!({ "effect": { "kind": "disco" } })),
            ("setEffect", json!({ "effect": { "kind": "wave", "colors": ["nope"] } })),
            ("setBrightness", json!({})),
            ("setPowerAnimation", json!({ "enabled": "yes" })),
            ("setBatteryFps", json!({ "fps": "fast" })),
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
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
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
        assert_eq!(status["zones"].as_array().unwrap().len(), ZONES);
        assert_eq!(status["zones"][0], "#ff0000");
        assert_eq!(status["zones"][3], "#000000");
        assert_eq!(status["brightness"], 100, "brightness is a percentage");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bug this caught on the development laptop: `getStatus` handed
    /// back the probe taken when the daemon started, so it went on saying
    /// "acpi_call is not installed" for as long as the daemon lived after
    /// somebody installed it. A status read and a capabilities read must
    /// describe the same machine.
    #[test]
    fn a_status_read_does_not_describe_the_machine_as_it_was_at_startup() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let module = module();
        let fresh = module.call("getCapabilities", Value::Null).expect("probing cannot fail");
        let status = module.call("getStatus", Value::Null).expect("status cannot fail");
        assert_eq!(status["capabilities"], fresh);
    }

    /// The suspend hook runs on every suspend of every machine with the
    /// service installed. With the animation off it must not touch the
    /// lights at all - not even to read them.
    #[test]
    fn the_suspend_hook_does_nothing_while_the_power_animation_is_off() {
        let dir = std::env::temp_dir().join(format!("pyren-rgb-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("acpi_call");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "").unwrap();
        let _acpi = crate::testenv::redirect(&file);

        let module = RgbModule::with_store(ConfigStore::at(dir.join("config")));
        std::fs::write(&file, "").unwrap();
        for method in ["powerOff", "powerOn"] {
            let status = module.call(method, json!({ "ifEnabled": true })).expect("a no-op cannot fail");
            assert_eq!(status["dark"], false, "{method}");
            assert_eq!(status["owned"], false, "{method}");
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "", "nothing reached acpi_call");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_status_says_why_an_effect_is_held_back() {
        let _acpi = crate::testenv::real();
        let module = module();
        assert_eq!(module.status()["throttled"], Value::Null);
        module.set_conditions(Conditions { on_battery: true, lid_closed: false });
        assert_eq!(module.status()["throttled"], "battery", "15 on battery is below the 30 asked for");
        module.set_conditions(Conditions { on_battery: true, lid_closed: true });
        assert_eq!(module.status()["throttled"], "lid", "the lid wins: nothing is written");
        module.set_conditions(Conditions::default());
        assert_eq!(module.status()["throttled"], Value::Null);
    }

    /// Nothing is written to the lights until someone asks - the same rule
    /// the fan module follows about the fans.
    #[test]
    fn a_fresh_module_does_not_claim_to_own_the_lights() {
        // Reads the ACPI interface: no redirection may run under it.
        let _acpi = crate::testenv::real();
        let status = module().status();
        assert_eq!(status["owned"], false);
        assert_eq!(status["restoreOnStart"], false);
    }
}
