//! Hotkey module: the laptop's own performance key, heard by the daemon.
//!
//! On an OMEN laptop Fn+P steps through the performance modes and Windows
//! draws a widget in the middle of the screen. Nothing on Linux does either
//! half. The daemon is the only process that *can* do the first: the key
//! reaches userspace through `/dev/input/event*`, which is `root:input`, and
//! a desktop app is neither.
//!
//! | method | params | result |
//! |---|---|---|
//! | `hotkey.getStatus` | none | what is bound, what is being watched, and why not if not |
//! | `hotkey.learn` | `{ "timeoutMs"?, "bind"? }` | the next key pressed, and whether it was saved |
//! | `hotkey.setTriggers` | `{ "triggers": [{ "device"?, "keycode"?, "scancode"?, "modifiers"? }] }` | as `getStatus` |
//! | `hotkey.setEnabled` | `{ "enabled": bool }` | as `getStatus` |
//! | `hotkey.press` | none | pretends the key was pressed - what a UI is developed against |
//!
//! ## Nothing is bound by default, and that is deliberate
//!
//! There is no table of "the OMEN key is keycode N" in here. Which key a
//! laptop sends, whether the kernel has a keycode for it at all, and which
//! device it arrives on are all things that vary between machines of the
//! *same model*, and a guessed table is the mistake this project already
//! made once with board ids (see `docs/01-ipc-protocol.md`, "measured, not
//! looked up"). So the machine is asked instead: `hotkey.learn` opens a few
//! seconds, the user presses their key, and whatever arrives is what gets
//! bound - a key with no keycode included.
//!
//! ## A shortcut can be a combination
//!
//! [`Modifiers`] rides along with every press and every trigger, and is
//! matched exactly - `Ctrl+P` does not fire on `Ctrl+Shift+P`. A modifier
//! is never a trigger on its own: the watcher reports it as state rather
//! than as a press, which is what makes *learning* a combination possible
//! at all, since the modifier necessarily goes down before the key.
//!
//! This matters more than it looks. On the laptop this was written for,
//! Fn+P never reaches Linux - the firmware keeps it - so choosing an
//! ordinary combination instead is the normal path, not the fallback. It
//! is heard here rather than through a desktop keybinding so that one
//! shortcut works on every compositor and at the login screen.
//!
//! ## What this module does not decide
//!
//! It does not know what a hotkey *does*. The daemon binary hands it an
//! action when it calls [`HotkeyModule::watch`], because deciding that the
//! shortcut shows the power modes is coordination between two modules,
//! and modules here never call each other.

mod devices;

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use pyren_config::{ConfigStore, LoadOutcome};
use pyren_core::{msg, ErrorKind, Module, ModuleError, ModuleResult, Msg};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use devices::{KeyPress, Modifiers};
use devices::{is_button, is_modifier, Unavailable};

/// Longest a `learn` call will hold the socket waiting for a key press.
const MAX_LEARN: Duration = Duration::from_secs(30);
const DEFAULT_LEARN: Duration = Duration::from_secs(10);

/// How often the watcher looks for keyboards that appeared after it
/// started. A plugged-in keyboard is not urgent; a busy loop is a cost.
const RESCAN_EVERY: Duration = Duration::from_secs(5);

/// One key, as this module recognises it again later.
///
/// Every field that is set has to match. In practice `learn` fills in all
/// of what the key actually reported, which is what makes a binding
/// specific to one physical key on one device.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Trigger {
    /// Device name as the kernel reports it, e.g. `HP WMI hotkeys`.
    pub device: Option<String>,
    pub keycode: Option<u16>,
    pub scancode: Option<u32>,
    /// Which modifiers must be held. Matched exactly, so `Ctrl+P` does not
    /// fire on `Ctrl+Shift+P` - a shortcut that swallowed its own
    /// supersets would collide with whatever the user had bound there.
    ///
    /// Absent from a config file written before this existed, and `serde`
    /// fills in "none held", which is the right reading of it: the vendor
    /// key those files bind is pressed on its own.
    pub modifiers: Modifiers,
}

impl Trigger {
    /// A binding that matches nothing is a binding that silently never
    /// fires, so it is rejected at the door rather than saved.
    fn is_specific(&self) -> bool {
        self.keycode.is_some() || self.scancode.is_some()
    }

    /// Whether this names a button on a pointing device rather than a key.
    ///
    /// The second half of the fix for a real accident: a learn window
    /// caught `BTN_TOOL_FINGER` from the touchpad and bound the power-mode
    /// cycle to *resting a finger on the trackpad*, which then cycled the
    /// machine through all four modes as fast as it could be touched. The
    /// watcher no longer opens pointing devices at all, and this refuses
    /// the binding even if one arrives some other way - a keyboard with
    /// mouse buttons on it, or a config file written by hand.
    fn is_button(&self) -> bool {
        self.keycode.is_some_and(is_button)
    }

    /// Whether this binds a modifier on its own - `Ctrl`, and nothing
    /// else. The watcher never reports one, so such a trigger could only
    /// come from a hand-written config, and it would never fire.
    fn is_modifier_alone(&self) -> bool {
        self.keycode.is_some_and(is_modifier)
    }

    /// The shortcut as somebody would write it down, for a UI to show.
    pub fn label(&self) -> String {
        KeyPress {
            device: self.device.clone().unwrap_or_default(),
            keycode: self.keycode,
            scancode: self.scancode,
            modifiers: self.modifiers,
        }
        .label()
    }

    fn matches(&self, press: &KeyPress) -> bool {
        if let Some(device) = &self.device {
            if device != &press.device {
                return false;
            }
        }
        if let Some(keycode) = self.keycode {
            if Some(keycode) != press.keycode {
                return false;
            }
        }
        if let Some(scancode) = self.scancode {
            if Some(scancode) != press.scancode {
                return false;
            }
        }
        self.modifiers == press.modifiers
    }

    fn from_press(press: &KeyPress) -> Self {
        Self {
            device: Some(press.device.clone()),
            keycode: press.keycode,
            scancode: press.scancode,
            modifiers: press.modifiers,
        }
    }
}

/// What is persisted to `hotkey.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HotkeyConfig {
    /// Off here means "heard and ignored"; the watcher keeps running so
    /// `learn` still works, which is what a settings page needs.
    pub enabled: bool,
    pub triggers: Vec<Trigger>,
    /// How long after firing an identical press is treated as the same
    /// press rather than a new one.
    ///
    /// This is not cosmetic debouncing. A key the kernel has no keycode for
    /// reports the *same* bare scancode when it goes down and when it comes
    /// back up, with nothing to tell the two apart - so without this window
    /// every press of Fn+P would advance two modes.
    pub repeat_guard_ms: u64,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self { enabled: true, triggers: Vec::new(), repeat_guard_ms: 300 }
    }
}

/// A learn window somebody opened, waiting for the key that answers it.
#[derive(Debug, Default)]
struct Learning {
    open: bool,
    caught: Option<KeyPress>,
}

struct State {
    config: HotkeyConfig,
    learning: Learning,
    /// Set once [`HotkeyModule::watch`] has been called. Held as an `Arc`
    /// so the reader thread and `hotkey.press` share one action.
    action: Option<Action>,
    watching: bool,
    /// Why the watcher could not start, when it could not.
    unavailable: Option<Unavailable>,
    devices: Vec<String>,
    fired: u64,
    last_fired: Option<Instant>,
    last_save_error: Option<String>,
}

/// What a hotkey press does. Supplied by the daemon binary.
pub type Action = Arc<dyn Fn(&KeyPress) + Send + Sync>;

/// Cloning shares one module: every clone talks to the same state, the same
/// watcher thread and the same config file. The daemon keeps one to wire up
/// the action and registers another.
#[derive(Clone)]
pub struct HotkeyModule {
    state: Arc<Mutex<State>>,
    /// Notified when a learn window catches a key, and when one opens.
    caught: Arc<Condvar>,
    store: ConfigStore,
    /// Whether there is a keyboard here at all, decided once at startup.
    present: bool,
}

impl Default for HotkeyModule {
    fn default() -> Self {
        Self::new()
    }
}

impl HotkeyModule {
    pub fn new() -> Self {
        Self::with_store(ConfigStore::system())
    }

    pub fn with_store(store: ConfigStore) -> Self {
        let loaded = store.load::<HotkeyConfig>("hotkey");
        match &loaded.outcome {
            LoadOutcome::Loaded | LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                eprintln!(
                    "pyren-daemon: hotkey config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                eprintln!(
                    "pyren-daemon: hotkey config is version {found}, newer than this build \
                     understands; using defaults and leaving the file alone"
                );
            }
        }

        // Asked once, before anything is watched, so `core.capabilities`
        // can tell "this machine has no keyboard" apart from "this daemon
        // is not root" - which are a hidden feature and a fixable error.
        let probe = devices::open_all();
        let present = !matches!(probe, Err(Unavailable::NoDevices));
        let unavailable = probe.as_ref().err().cloned();

        let state = State {
            config: loaded.value,
            learning: Learning::default(),
            action: None,
            watching: false,
            unavailable,
            devices: probe.map(|d| d.iter().map(|d| d.name.clone()).collect()).unwrap_or_default(),
            fired: 0,
            last_fired: None,
            last_save_error: None,
        };

        Self {
            state: Arc::new(Mutex::new(state)),
            caught: Arc::new(Condvar::new()),
            store,
            present,
        }
    }

    /// Starts the watcher thread with the action a matched key runs.
    ///
    /// Returns whether it could start. It is not an error if it could not:
    /// an unprivileged development daemon cannot read `/dev/input`, and
    /// that is a line at startup rather than a refusal to run.
    pub fn watch(&self, action: Action) -> bool {
        {
            let mut state = self.lock();
            state.action = Some(action);
            if state.watching {
                return true;
            }
            if state.unavailable.is_some() {
                return false;
            }
            state.watching = true;
        }

        let module = self.clone();
        std::thread::Builder::new()
            .name("pyren-hotkey".into())
            .spawn(move || module.run())
            .is_ok()
    }

    /// The watcher. Opens the keyboards, waits, and hands on what arrives.
    fn run(self) {
        let mut devices = match devices::open_all() {
            Ok(devices) => devices,
            Err(reason) => {
                let mut state = self.lock();
                state.watching = false;
                state.unavailable = Some(reason);
                return;
            }
        };
        {
            let mut state = self.lock();
            state.devices = devices.iter().map(|d| d.name.clone()).collect();
        }

        let mut last_scan = Instant::now();
        let mut last_fire: Option<(Instant, KeyPress)> = None;

        loop {
            for press in devices::wait_for_presses(&mut devices, Duration::from_secs(1)) {
                // A learn window swallows the key: somebody is in the
                // middle of rebinding, and firing the old binding while
                // they do it would be a surprise every time.
                if self.deliver_to_learner(&press) {
                    continue;
                }

                let action = {
                    let state = self.lock();
                    if !state.config.enabled {
                        continue;
                    }
                    if !state.config.triggers.iter().any(|t| t.matches(&press)) {
                        continue;
                    }
                    let guard = Duration::from_millis(state.config.repeat_guard_ms);
                    if let Some((at, previous)) = &last_fire {
                        if previous == &press && at.elapsed() < guard {
                            continue;
                        }
                    }
                    state.action.clone()
                };

                last_fire = Some((Instant::now(), press.clone()));
                {
                    let mut state = self.lock();
                    state.fired += 1;
                    state.last_fired = Some(Instant::now());
                }
                if let Some(action) = action {
                    action(&press);
                }
            }

            if last_scan.elapsed() >= RESCAN_EVERY {
                last_scan = Instant::now();
                if let Ok(found) = devices::open_all() {
                    // Only devices that are new: the open ones carry
                    // half-read packets that must not be thrown away.
                    let known: Vec<_> = devices.iter().map(|d| d.path.clone()).collect();
                    for device in found {
                        if !known.contains(&device.path) {
                            devices.push(device);
                        }
                    }
                    let mut state = self.lock();
                    state.devices = devices.iter().map(|d| d.name.clone()).collect();
                }
            }
        }
    }

    /// Hands a press to an open learn window. `true` if one took it.
    ///
    /// A button never closes the window. Somebody who rests a thumb on the
    /// trackpad while reaching for Fn+P has not answered the question, and
    /// treating that as the answer is how the touchpad ended up bound to
    /// the power mode once already.
    fn deliver_to_learner(&self, press: &KeyPress) -> bool {
        let mut state = self.lock();
        if !state.learning.open {
            return false;
        }
        if press.keycode.is_some_and(is_button) {
            return true; // swallowed: not a key, and not the trigger either
        }
        state.learning.open = false;
        state.learning.caught = Some(press.clone());
        drop(state);
        self.caught.notify_all();
        true
    }

    /// Waits for one key press, for `hotkey.learn`.
    fn learn(&self, timeout: Duration, bind: bool) -> ModuleResult {
        {
            let mut state = self.lock();
            if let Some(reason) = &state.unavailable {
                return Err(match reason {
                    Unavailable::NeedsRoot => {
                        ModuleError::localised(ErrorKind::PermissionDenied, reason.to_msg())
                    }
                    Unavailable::NoDevices => ModuleError::Unsupported,
                });
            }
            if !state.watching {
                return Err(ModuleError::localised(
                    ErrorKind::Failed,
                    msg!(
                        "hotkey.err.watcherDown",
                        "the hotkey watcher is not running, so no key can be heard"
                    ),
                ));
            }
            if state.learning.open {
                return Err(ModuleError::localised(
                    ErrorKind::Busy,
                    msg!(
                        "hotkey.err.learnBusy",
                        "something else is already waiting for a key press"
                    ),
                ));
            }
            state.learning = Learning { open: true, caught: None };
        }

        let deadline = Instant::now() + timeout.min(MAX_LEARN);
        let mut state = self.lock();
        while state.learning.caught.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (guard, _) = self
                .caught
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }

        state.learning.open = false;
        let caught = state.learning.caught.take();

        let Some(press) = caught else {
            // Nobody pressed anything. That is an answer, not a failure:
            // a UI shows "no key detected - try again", and a key that
            // never arrives is exactly what a laptop whose Fn+P is handled
            // entirely inside the EC looks like.
            return Ok(json!({ "press": null, "timedOut": true, "bound": false }));
        };

        let bound = if bind {
            let trigger = Trigger::from_press(&press);
            state.config.triggers = vec![trigger];
            self.persist(&mut state);
            true
        } else {
            false
        };

        Ok(json!({
            "press": press_json(&press),
            "timedOut": false,
            "bound": bound,
            "triggers": state.config.triggers,
        }))
    }

    fn status(&self) -> Value {
        let state = self.lock();
        json!({
            "enabled": state.config.enabled,
            "watching": state.watching,
            "detail": self.detail(&state),
            "devices": state.devices,
            "triggers": state.config.triggers,
            "label": state.config.triggers.first().map(Trigger::label),
            "repeatGuardMs": state.config.repeat_guard_ms,
            "learning": state.learning.open,
            "fired": state.fired,
            "lastFiredAgoMs": state.last_fired.map(|at| at.elapsed().as_millis() as u64),
            "configPath": self.store.path_for("hotkey"),
            "configSaveError": state.last_save_error,
        })
    }

    /// One sentence a CLI or a settings page can show without composing it
    /// from four booleans - and, when the answer is "nothing happens", the
    /// reason it is.
    fn detail(&self, state: &State) -> Msg {
        if let Some(reason) = &state.unavailable {
            return reason.to_msg();
        }
        if state.config.triggers.is_empty() {
            return msg!(
                "hotkey.detail.noKeyBound",
                "no key bound yet; press yours during 'hotkey learn' to bind it"
            );
        }
        if !state.config.enabled {
            return msg!("hotkey.detail.disabled", "a key is bound and the hotkey is switched off");
        }
        msg!(
            "hotkey.detail.watching",
            { "count" => state.devices.len() },
            "watching {count} keyboards for the bound key"
        )
    }

    fn set_triggers(&self, triggers: Vec<Trigger>) -> ModuleResult {
        if let Some(vague) = triggers.iter().find(|t| !t.is_specific()) {
            return Err(ModuleError::localised(
                ErrorKind::InvalidParams,
                msg!(
                    "hotkey.err.vagueTrigger",
                    { "trigger" => format!("{vague:?}") },
                    "a trigger needs a keycode or a scancode; got {trigger}"
                ),
            ));
        }
        if let Some(button) = triggers.iter().find(|t| t.is_button()) {
            return Err(ModuleError::localised(
                ErrorKind::InvalidParams,
                msg!(
                    "hotkey.err.buttonTrigger",
                    { "keycode" => button.keycode.unwrap_or_default() },
                    "keycode {keycode} is a mouse or touchpad button, not a key; binding it \
                     would throw the widget on screen on every click"
                ),
            ));
        }
        if let Some(modifier) = triggers.iter().find(|t| t.is_modifier_alone()) {
            return Err(ModuleError::localised(
                ErrorKind::InvalidParams,
                msg!(
                    "hotkey.err.modifierTrigger",
                    { "keycode" => modifier.keycode.unwrap_or_default() },
                    "keycode {keycode} is a modifier, which is never a shortcut on its own; \
                     bind it together with another key"
                ),
            ));
        }
        let mut state = self.lock();
        state.config.triggers = triggers;
        self.persist(&mut state);
        drop(state);
        Ok(self.status())
    }

    fn set_enabled(&self, enabled: bool) -> ModuleResult {
        let mut state = self.lock();
        state.config.enabled = enabled;
        self.persist(&mut state);
        drop(state);
        Ok(self.status())
    }

    /// `hotkey.press`: run the action as though the key had been pressed.
    ///
    /// This is how the OSD is developed on a machine whose Fn+P does not
    /// reach Linux at all, and how a settings page offers "preview". It
    /// deliberately does not check `enabled` or the bindings - the caller
    /// is asking for the effect, not simulating the hardware.
    fn press(&self) -> ModuleResult {
        let action = self.lock().action.clone();
        let press = KeyPress {
            device: "pyren-ctl".into(),
            keycode: None,
            scancode: None,
            modifiers: Modifiers::default(),
        };
        match action {
            Some(action) => {
                action(&press);
                Ok(json!({ "fired": true }))
            }
            None => Err(ModuleError::localised(
                ErrorKind::Failed,
                msg!("hotkey.err.noAction", "this daemon has no hotkey action wired up"),
            )),
        }
    }

    fn persist(&self, state: &mut State) {
        match self.store.save("hotkey", &state.config) {
            Ok(()) => state.last_save_error = None,
            Err(e) => {
                let message = e.to_string();
                eprintln!("pyren-daemon: could not save hotkey config: {message}");
                state.last_save_error = Some(message);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn press_json(press: &KeyPress) -> Value {
    json!({
        "device": press.device,
        "keycode": press.keycode,
        "scancode": press.scancode,
        "modifiers": press.modifiers,
        "describe": press.describe(),
        "label": press.label(),
    })
}

impl Module for HotkeyModule {
    fn id(&self) -> &'static str {
        "hotkey"
    }

    /// True whenever this machine has a keyboard, even if this daemon may
    /// not read it. Being unable to *reach* the hardware is a
    /// `permissionDenied` with a fix in it, and hiding the feature instead
    /// would hide the fix as well.
    fn is_supported(&self) -> bool {
        self.present
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getStatus" => Ok(self.status()),

            "learn" => {
                let timeout = params
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .map(Duration::from_millis)
                    .unwrap_or(DEFAULT_LEARN);
                let bind = params.get("bind").and_then(Value::as_bool).unwrap_or(false);
                self.learn(timeout, bind)
            }

            "setTriggers" => {
                let triggers = params.get("triggers").cloned().ok_or_else(|| {
                    ModuleError::InvalidParams(
                        "params.triggers is required: a list of { device?, keycode?, scancode? }"
                            .into(),
                    )
                })?;
                let triggers: Vec<Trigger> = serde_json::from_value(triggers)
                    .map_err(|e| ModuleError::InvalidParams(format!("invalid triggers: {e}")))?;
                self.set_triggers(triggers)
            }

            "setEnabled" => {
                let enabled = params.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
                    ModuleError::InvalidParams("params.enabled is required: true or false".into())
                })?;
                self.set_enabled(enabled)
            }

            "press" => self.press(),

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(device: &str, keycode: Option<u16>, scancode: Option<u32>) -> KeyPress {
        KeyPress { device: device.into(), keycode, scancode, modifiers: Modifiers::default() }
    }

    fn store(tag: &str) -> ConfigStore {
        let dir = std::env::temp_dir()
            .join(format!("pyren-hotkey-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ConfigStore::at(dir)
    }

    #[test]
    fn a_trigger_matches_only_the_key_it_was_learned_from() {
        let trigger = Trigger::from_press(&press("HP WMI hotkeys", Some(148), None));

        assert!(trigger.matches(&press("HP WMI hotkeys", Some(148), None)));
        assert!(!trigger.matches(&press("HP WMI hotkeys", Some(149), None)));
        assert!(
            !trigger.matches(&press("AT Translated Set 2 keyboard", Some(148), None)),
            "the same keycode on another keyboard is another key"
        );
    }

    /// A combination is matched on its modifiers as well as its key, and
    /// *exactly*: the settings page lets people choose Ctrl+P, and firing
    /// that on Ctrl+Shift+P would hijack whatever they had bound there.
    #[test]
    fn a_combination_matches_only_the_modifiers_it_was_learned_with() {
        let ctrl = Modifiers { ctrl: true, ..Default::default() };
        let ctrl_shift = Modifiers { ctrl: true, shift: true, ..Default::default() };

        let mut learned = press("AT Translated Set 2 keyboard", Some(25), Some(0x19));
        learned.modifiers = ctrl;
        let trigger = Trigger::from_press(&learned);

        assert!(trigger.matches(&learned));
        assert_eq!(trigger.label(), "Ctrl+P");

        let mut plain = learned.clone();
        plain.modifiers = Modifiers::default();
        assert!(!trigger.matches(&plain), "P on its own is not Ctrl+P");

        let mut more = learned.clone();
        more.modifiers = ctrl_shift;
        assert!(!trigger.matches(&more), "Ctrl+Shift+P is a different shortcut");
    }

    /// A binding on a modifier alone can only come from a hand-written
    /// config - the watcher never reports one - and it would fire on the
    /// first half of every copy-paste, so it is refused at the door.
    #[test]
    fn a_modifier_on_its_own_is_refused_as_a_shortcut() {
        let module = HotkeyModule::with_store(store("modifier-alone"));
        let refused = module.call(
            "setTriggers",
            json!({ "triggers": [{ "keycode": 29 }] }),
        );

        match refused {
            Err(e) => {
                assert_eq!(e.kind(), ErrorKind::InvalidParams);
                assert!(e.to_string().contains("modifier"), "{e}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The case this is all built around: a key with no keycode at all is
    /// bindable, by its scancode.
    #[test]
    fn a_key_with_no_keycode_is_still_bindable() {
        let trigger = Trigger::from_press(&press("AT Translated Set 2 keyboard", None, Some(0xe02b)));

        assert!(trigger.is_specific());
        assert!(trigger.matches(&press("AT Translated Set 2 keyboard", None, Some(0xe02b))));
        assert!(!trigger.matches(&press("AT Translated Set 2 keyboard", None, Some(0xe02c))));
    }

    /// A trigger with nothing in it would match every key on the machine -
    /// every keystroke would cycle the power mode.
    #[test]
    fn a_trigger_that_names_no_key_is_refused() {
        let module = HotkeyModule::with_store(store("vague"));
        let error = module
            .call("setTriggers", json!({ "triggers": [{ "device": "some keyboard" }] }))
            .expect_err("a device on its own is not a key");

        assert_eq!(error.kind(), pyren_core::ErrorKind::InvalidParams);
        assert!(error.to_string().contains("keycode or a scancode"));
    }

    /// The accident this pair of guards exists for: `BTN_TOOL_FINGER` is
    /// "a finger is resting on the trackpad", and bound as a hotkey it
    /// cycled the machine through all four power modes on every touch.
    #[test]
    fn a_touchpad_button_is_refused_as_a_hotkey() {
        let module = HotkeyModule::with_store(store("button"));
        let error = module
            .call(
                "setTriggers",
                json!({ "triggers": [{ "device": "SYNA32FF:00 06CB:CFC5 Touchpad", "keycode": 325 }] }),
            )
            .expect_err("a touchpad button is not a key");

        assert_eq!(error.kind(), pyren_core::ErrorKind::InvalidParams);
        assert!(error.to_string().contains("button"), "{error}");
        assert!(
            module.call("getStatus", Value::Null).unwrap()["triggers"]
                .as_array()
                .unwrap()
                .is_empty(),
            "and nothing was saved"
        );
    }

    /// The keys either side of the button block still bind: the fix must
    /// not cost a real hotkey.
    #[test]
    fn a_real_key_is_still_bindable_on_either_side_of_the_button_block() {
        let module = HotkeyModule::with_store(store("keys"));
        for keycode in [148u16, 0x160, 0x264] {
            module
                .call("setTriggers", json!({ "triggers": [{ "keycode": keycode }] }))
                .unwrap_or_else(|e| panic!("keycode {keycode} should bind: {e}"));
        }
    }

    #[test]
    fn a_learned_key_survives_a_restart_of_the_daemon() {
        let store = store("persist");
        let module = HotkeyModule::with_store(store.clone());
        module
            .call("setTriggers", json!({ "triggers": [{ "keycode": 148, "device": "HP WMI hotkeys" }] }))
            .expect("a specific trigger is accepted");

        let restarted = HotkeyModule::with_store(store);
        let status = restarted.call("getStatus", Value::Null).unwrap();
        assert_eq!(status["triggers"][0]["keycode"], 148);
        assert_eq!(status["triggers"][0]["device"], "HP WMI hotkeys");
    }

    #[test]
    fn the_status_says_what_is_wrong_rather_than_only_that_something_is() {
        let module = HotkeyModule::with_store(store("detail"));
        let status = module.call("getStatus", Value::Null).unwrap();

        let detail = status["detail"]["text"].as_str().unwrap().to_string();
        // Either no key is bound yet, or this machine will not let a test
        // process read /dev/input. Both are sentences with a fix in them.
        assert!(
            detail.contains("no key bound") || detail.contains("root") || detail.contains("/dev/input"),
            "unhelpful detail: {detail}"
        );
        // ...and it is shown in the user's language: the sentence carries a
        // catalog key beside its English text.
        assert!(
            status["detail"]["key"].as_str().unwrap().starts_with("hotkey."),
            "detail must be a translatable Msg: {}",
            status["detail"]
        );
        assert_eq!(status["fired"], 0);
    }

    /// `press` exists so a UI can be built before the key is known; with no
    /// action wired up it has to say so rather than claim it fired.
    #[test]
    fn a_simulated_press_with_no_action_is_an_honest_failure() {
        let module = HotkeyModule::with_store(store("nopress"));
        let error = module.call("press", Value::Null).expect_err("nothing is wired up");
        assert!(error.to_string().contains("no hotkey action"));
    }

    #[test]
    fn a_simulated_press_runs_the_action_the_daemon_wired_up() {
        let module = HotkeyModule::with_store(store("press"));
        let count = Arc::new(Mutex::new(0));
        let counter = Arc::clone(&count);
        module.lock().action = Some(Arc::new(move |_press: &KeyPress| {
            *counter.lock().unwrap() += 1;
        }));

        assert_eq!(module.call("press", Value::Null).unwrap()["fired"], true);
        assert_eq!(*count.lock().unwrap(), 1);
    }

    #[test]
    fn switching_the_hotkey_off_is_remembered() {
        let store = store("enabled");
        let module = HotkeyModule::with_store(store.clone());
        let status = module.call("setEnabled", json!({ "enabled": false })).unwrap();
        assert_eq!(status["enabled"], false);

        let restarted = HotkeyModule::with_store(store);
        assert_eq!(restarted.call("getStatus", Value::Null).unwrap()["enabled"], false);
    }

    #[test]
    fn an_unknown_method_is_told_apart_from_a_bad_parameter() {
        let module = HotkeyModule::with_store(store("methods"));
        assert_eq!(
            module.call("nope", Value::Null).unwrap_err().kind(),
            pyren_core::ErrorKind::UnknownMethod
        );
        assert_eq!(
            module.call("setEnabled", json!({})).unwrap_err().kind(),
            pyren_core::ErrorKind::InvalidParams
        );
    }
}
