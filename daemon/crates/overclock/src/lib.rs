//! GPU overclocking - the one module that leaves the envelope the firmware
//! shipped, and the only one whose failure mode is not an error message.
//!
//! | method | params | result |
//! |---|---|---|
//! | `overclock.getState` | none | the probe, the consent, what is applied, what is pending |
//! | `overclock.probe` | `{ "allowWrites"?: bool }` | a fresh look at every GPU |
//! | `overclock.setConsent` | `{ "accepted": bool }` | the new state |
//! | `overclock.apply` | `{ "gpu"?, "coreOffsetMhz"?, "memOffsetMhz"?, "clockLock"?, "holdSecs"? }` | the new state |
//! | `overclock.confirm` | none | the new state |
//! | `overclock.cancel` | none | the new state |
//! | `overclock.reset` | `{ "gpu"? }` | the new state |
//! | `overclock.setRestoreOnStart` | `{ "enabled": bool }` | the new state |
//!
//! ## Why this is not just two sliders and a write
//!
//! Everything else in this daemon stays inside what the firmware allows,
//! and the worst a mistake there can do is make the machine slow or loud.
//! An offset is different: the normal case is a value that is stable in a
//! benchmark and not in a game, and what happens then is a hang or
//! corrupted VRAM rather than a refusal. So `dev/TODO.md` set four
//! conditions before any of this could ship, and they are the four things
//! this file is mostly made of:
//!
//! 1. **Default to zero offset.** [`plan::Target::default`] is stock, an
//!    empty config is stock, and a machine that has never been told
//!    otherwise is left exactly as the firmware left it.
//! 2. **Never restore an offset at boot without an explicit opt-in.**
//!    `restoreOnStart` defaults to false like every other module's
//!    equivalent - and unlike them, it also refuses to act when the last
//!    apply was never confirmed, because that is what a machine that hung
//!    looks like from here (see [`State::unconfirmed_at_start`]).
//! 3. **Apply in small steps with a revert-on-failure timer.** The climb is
//!    [`plan::ramp`]; the timer is [`Pending`], armed by `apply` and
//!    disarmed by `confirm`. Nothing stays applied because a user *stopped
//!    answering* - the desktop having died is the case it is there for.
//! 4. **Say plainly that this one can cost a session's work.** The consent
//!    text is [`CONSENT`], it is served with the state so the app cannot
//!    quietly reword it, and no offset is written before it is accepted.
//!
//! ## What it will not do
//!
//! - **Raise the CPU's power limits.** That belongs behind this same
//!   consent (`dev/TODO.md`), and it is not here because the power module
//!   already owns those knobs and re-applies them, clamped to stock, on
//!   every mode change. Two owners on one register is a worse bug than a
//!   missing feature.
//! - **Undervolt.** Same interface on AMD, no interface at all on NVIDIA
//!   under Linux, and nothing to test it on.
//! - **Set a fan curve of its own.** More heat is the fan module's problem
//!   and it is already good at it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pyren_config::{ConfigStore, LoadOutcome};
use pyren_core::{log_error, log_info, log_warn};
use pyren_core::{msg, ErrorKind, Module, ModuleError, ModuleResult, Msg};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod nvidia;
pub mod nvml;
pub mod plan;
pub mod probe;

pub use plan::{ClockLock, Target};
pub use probe::{GpuProbe, Probe, Vendor};

use nvidia::{Nvidia, NvidiaError};

/// Shown before anything can be applied, and served with the state so the
/// warning a user agreed to is the daemon's words rather than whatever the
/// client felt like putting in a dialog.
pub const CONSENT: &str = "\
Overclocking runs this GPU outside the settings it was shipped with. An \
offset that survives a benchmark can still hang the machine in a game, and \
when it does there is no error message: the screen stops, and anything \
unsaved is gone. Nothing here is covered by the manufacturer's warranty. \
Every change is reverted automatically unless you confirm it, and none is \
restored after a reboot unless you ask for that separately.";

/// Bumped if [`CONSENT`] ever says something materially different, so a
/// stored acceptance of the old wording stops counting.
const CONSENT_VERSION: u32 = 1;

/// How long an applied offset waits to be confirmed before it is undone.
///
/// Long enough to see a desktop come back and click a button, short enough
/// that a machine which locked up is at stock again before its owner has
/// finished reaching for the power button.
const DEFAULT_HOLD_SECS: u64 = 20;
const MIN_HOLD_SECS: u64 = 5;
const MAX_HOLD_SECS: u64 = 300;

/// How often the watchdog looks at the clock. Fine enough that the
/// countdown a UI shows is honest, coarse enough to be free.
const WATCHDOG_TICK: Duration = Duration::from_millis(500);

/// Let each step settle before it is read back. A driver takes the write
/// asynchronously, and reading immediately reports the previous value.
const STEP_SETTLE: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Consent {
    /// Seconds since the epoch. Stored so the UI can say when, and so a
    /// consent given to different words can be told apart from this one.
    pub accepted_at: u64,
    pub version: u32,
}

impl Consent {
    fn current(&self) -> bool {
        self.version == CONSENT_VERSION
    }
}

/// What is persisted to `overclock.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OverclockConfig {
    pub consent: Option<Consent>,
    /// Per GPU id, the last target a human confirmed. Never what was merely
    /// applied: an unconfirmed target is by definition one nobody has said
    /// worked.
    pub targets: BTreeMap<String, Target>,
    /// Off by default, and the one flag in this daemon whose default has a
    /// worse consequence if flipped carelessly than the fan module's: a bad
    /// offset restored at boot is a machine that may not reach a desktop.
    pub restore_on_start: bool,
    /// Set while an apply is waiting to be confirmed, cleared when it is.
    /// Persisted on purpose - see [`State::unconfirmed_at_start`].
    pub armed_gpu: Option<String>,
    pub hold_secs: Option<u64>,
}

/// An applied change that has not been confirmed yet, and what it goes back
/// to when the clock runs out.
#[derive(Debug, Clone)]
struct Pending {
    gpu: String,
    revert_to: Target,
    deadline: Instant,
}

struct State {
    config: OverclockConfig,
    /// What this daemon believes is on each card *now*. Only ever what it
    /// wrote itself: a card nobody has asked it to touch is absent, not
    /// zero, because zero would be a claim about hardware we never read.
    applied: BTreeMap<String, Target>,
    pending: Option<Pending>,
    /// Open for exactly as long as `pending` is: the driver's own
    /// "this GPU just faulted" signal, asked once per watchdog tick so a
    /// card that starts failing at second 2 of the hold is not left on a
    /// bad clock until second 20. `None` wherever the signal does not
    /// exist - no NVML, an older driver, a card that does not advertise
    /// it - which is the behaviour this module had before it, unchanged.
    fault_watch: Option<nvml::EventWatch>,
    /// True when the daemon started and found the config still armed - the
    /// signature of a machine that was overclocked and never came back to
    /// confirm it. Nothing is restored on such a boot, and the state says
    /// so rather than silently doing less than `restoreOnStart` promised.
    unconfirmed_at_start: bool,
    last_error: Option<Msg>,
    /// The last thing that happened by itself, in words: a revert that
    /// nobody asked for is the one event a user has to be able to see an
    /// explanation of after the fact.
    last_note: Option<Msg>,
    last_save_error: Option<String>,
}

pub struct OverclockModule {
    /// Taken once at startup, because `is_supported` is asked for every
    /// `core.capabilities` and probing shells out to nvidia-smi.
    /// `overclock.probe` replaces it, so plugging in a driver and asking
    /// again works without a restart.
    probe: Arc<Mutex<Probe>>,
    store: ConfigStore,
    state: Arc<Mutex<State>>,
}

impl OverclockModule {
    pub fn new() -> Self {
        Self::with_store(ConfigStore::system())
    }

    pub fn with_store(store: ConfigStore) -> Self {
        let probe = probe::probe(false);
        let loaded = store.load::<OverclockConfig>("overclock");
        match &loaded.outcome {
            LoadOutcome::Loaded => {
                log_info!(
                    "overclock config loaded from {}",
                    store.path_for("overclock").display()
                );
            }
            LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                log_warn!(
                    "overclock config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                log_warn!(
                    "overclock config is version {found}, newer than this build \
                     understands; using defaults and leaving the file alone"
                );
            }
        }

        let mut config = loaded.value;
        // An apply that was never confirmed means the machine stopped
        // answering while it was overclocked. Whatever else happens this
        // boot, it does not include putting an offset back on that card.
        let unconfirmed_at_start = config.armed_gpu.take().is_some();

        let module = Self {
            probe: Arc::new(Mutex::new(probe)),
            store,
            state: Arc::new(Mutex::new(State {
                config,
                applied: BTreeMap::new(),
                pending: None,
                fault_watch: None,
                unconfirmed_at_start,
                last_error: None,
                last_note: unconfirmed_at_start.then(|| {
                    msg!(
                        "overclock.note.unconfirmedAtStart",
                        "the last overclock was never confirmed, so this boot starts at \
                         stock and nothing was restored"
                    )
                }),
                last_save_error: None,
            })),
        };

        module.restore_on_start();
        module.spawn_watchdog();
        module
    }

    /// The probe taken at startup. The daemon prints it, which is the
    /// fastest way to answer "why is the overclocking page empty".
    pub fn probe(&self) -> Probe {
        lock(&self.probe).clone()
    }

    /// Re-applies the confirmed targets, if and only if everything says so.
    fn restore_on_start(&self) {
        let state = lock(&self.state);
        let unconfirmed = state.unconfirmed_at_start;
        let opted_in = state.config.restore_on_start;
        let consented = state.config.consent.is_some_and(|c| c.current());
        let targets = state.config.targets.clone();
        drop(state);

        if !opted_in || targets.values().all(Target::is_stock) {
            return;
        }
        if unconfirmed {
            log_warn!(
                "not restoring the saved GPU offsets - the last one was applied \
                 and never confirmed, which is what a machine that hung looks like from here"
            );
            return;
        }
        if !consented {
            log_warn!("not restoring GPU offsets - the warning was never accepted");
            return;
        }

        for (id, target) in targets {
            if target.is_stock() {
                continue;
            }
            let Some(gpu) = lock(&self.probe).gpu(&id).cloned() else {
                log_warn!("saved overclock is for {id}, which is not on this machine");
                continue;
            };
            log_info!("restoring the saved overclock on {} ({id})", gpu.name);
            match self.climb(&gpu, Target::default(), target) {
                Ok(()) => {
                    lock(&self.state).applied.insert(id, target);
                }
                Err(e) => {
                    log_warn!("could not restore the overclock on {id}: {e}");
                    lock(&self.state).last_error = Some(e.as_msg());
                }
            }
        }
    }

    /// The revert-on-failure timer, as a thread of its own.
    ///
    /// It has to be a thread rather than something the next IPC call
    /// notices, because the case it exists for is precisely the one where
    /// no next IPC call arrives: the desktop is gone, the app is gone, and
    /// the only thing left running is this daemon.
    fn spawn_watchdog(&self) {
        let state = Arc::clone(&self.state);
        let probe = Arc::clone(&self.probe);
        let store = self.store.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(WATCHDOG_TICK);

            let due = {
                let guard = lock(&state);
                // Asked only while something is actually armed, and while
                // the lock is held, because the watch belongs to the same
                // `pending` it is watching for. The poll itself returns in
                // about a millisecond - see `nvml::POLL_TIMEOUT_MS`.
                let fault = guard.pending.is_some()
                    && guard.fault_watch.as_ref().is_some_and(nvml::EventWatch::poll);
                watchdog_tick(guard.pending.as_ref(), fault, Instant::now())
                    .map(|reason| (guard.pending.clone().expect("a reason implies a pending"), reason))
            };
            let Some((pending, reason)) = due else { continue };

            let gpu = pending.gpu.clone();
            match revert(&state, &probe, &store, pending, reason) {
                Ok(()) => match reason {
                    RevertReason::FaultReported => {
                        log_error!("the driver reported a fault on {gpu}; reverted the overclock")
                    }
                    _ => log_warn!("overclock on {gpu} was never confirmed; reverted"),
                },
                // The one failure with nothing left to try. Say it loudly:
                // the card is running something nobody confirmed and this
                // daemon could not take it back.
                Err(e) => log_error!("could NOT undo the overclock on {gpu}: {e}"),
            }
        });
    }

    // --- the state, as it goes over the socket --------------------------

    fn status(&self) -> Value {
        let probe = lock(&self.probe).clone();
        let state = lock(&self.state);
        let now = Instant::now();

        let gpus: Vec<Value> = probe
            .gpus
            .iter()
            .map(|gpu| {
                let confirmed = state.config.targets.get(&gpu.id).copied().unwrap_or_default();
                json!({
                    "id": gpu.id,
                    "name": gpu.name,
                    "vendor": gpu.vendor,
                    "driver": gpu.driver,
                    "drivable": gpu.drivable(),
                    "coreOffset": gpu.core_offset,
                    "memOffset": gpu.mem_offset,
                    "clockLock": gpu.clock_lock,
                    "offsetsWritable": gpu.offsets_writable,
                    "detail": gpu.detail,
                    // What a human agreed to keep, as opposed to what is on
                    // the card this second - which may be a step of a climb
                    // or a target still waiting to be confirmed.
                    "confirmed": confirmed,
                    "applied": state.applied.get(&gpu.id).copied(),
                })
            })
            .collect();

        json!({
            "supported": probe.supported,
            "detail": probe.detail,
            "gpus": gpus,
            "defaultGpu": probe.default_gpu().map(|gpu| gpu.id.clone()),
            "consent": {
                "text": CONSENT,
                "version": CONSENT_VERSION,
                "accepted": state.config.consent.is_some_and(|c| c.current()),
                "acceptedAt": state.config.consent.map(|c| c.accepted_at),
            },
            "pending": state.pending.as_ref().map(|pending| json!({
                "gpu": pending.gpu,
                "secondsLeft": pending.deadline.saturating_duration_since(now).as_secs_f64(),
                "revertsTo": pending.revert_to,
            })),
            "holdSecs": hold_secs(&state.config),
            "restoreOnStart": state.config.restore_on_start,
            "restoredOnStart": !state.unconfirmed_at_start
                && state.config.restore_on_start
                && !state.applied.is_empty(),
            "unconfirmedAtStart": state.unconfirmed_at_start,
            "note": state.last_note,
            "error": state.last_error,
            "configPath": self.store.path_for("overclock").display().to_string(),
            "saved": state.last_save_error.is_none(),
            "saveError": state.last_save_error,
        })
    }

    // --- the methods ----------------------------------------------------

    fn set_consent(&self, accepted: bool) -> ModuleResult {
        {
            let mut state = lock(&self.state);
            state.config.consent = accepted.then(|| Consent {
                accepted_at: now_secs(),
                version: CONSENT_VERSION,
            });
            persist(&self.store, &mut state);
        }
        // Withdrawing consent is not a preference change, it is a request
        // to stop: whatever is on the cards goes back to stock.
        if !accepted {
            self.reset(None)?;
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

    /// Applies a target, in steps, and arms the timer that will undo it.
    fn apply(&self, params: &Value) -> ModuleResult {
        let gpu = self.requested_gpu(params)?;
        let hold = self.hold_from(params)?;

        {
            let state = lock(&self.state);
            if !state.config.consent.is_some_and(|c| c.current()) {
                return Err(ModuleError::localised(
                    ErrorKind::InvalidParams,
                    msg!(
                        "overclock.err.notConsented",
                        "overclocking has not been consented to on this machine; call \
                         overclock.setConsent with the text from overclock.getState first"
                    ),
                ));
            }
            if let Some(pending) = &state.pending {
                return Err(ModuleError::localised(
                    ErrorKind::Busy,
                    msg!(
                        "overclock.err.pending",
                        { "gpu" => pending.gpu.clone() },
                        "the last change to {gpu} is still waiting to be confirmed or undone"
                    ),
                ));
            }
        }

        if !gpu.drivable() {
            return Err(ModuleError::localised(ErrorKind::NotCapable, gpu.detail.clone()));
        }

        let from = self.current(&gpu.id);
        let requested = self.merge(params, from)?;
        let clamped = plan::clamp(requested, &gpu.ceiling());

        // Asking for exactly what is already on the card is not a change,
        // and arming a watchdog over it would demand a confirmation for
        // nothing.
        if clamped.target == from {
            let mut state = lock(&self.state);
            state.last_note = Msg::join(clamped.notes.clone(), "; ");
            drop(state);
            return Ok(self.status());
        }

        match self.climb(&gpu, from, clamped.target) {
            Ok(()) => {
                let mut state = lock(&self.state);
                state.applied.insert(gpu.id.clone(), clamped.target);
                state.last_error = None;
                state.last_note = Msg::join(clamped.notes.clone(), "; ");
                // Stock needs no confirming: it is where a revert would put
                // the card anyway.
                if clamped.target.is_stock() {
                    state.config.targets.insert(gpu.id.clone(), clamped.target);
                    state.config.armed_gpu = None;
                } else {
                    state.pending = Some(Pending {
                        gpu: gpu.id.clone(),
                        revert_to: self.confirmed(&state, &gpu.id),
                        deadline: Instant::now() + Duration::from_secs(hold),
                    });
                    state.fault_watch = fault_watch(&gpu);
                    state.config.armed_gpu = Some(gpu.id.clone());
                    state.config.hold_secs = Some(hold);
                }
                persist(&self.store, &mut state);
                drop(state);
                Ok(self.status())
            }
            Err(e) => {
                // The climb undoes itself before reporting, so a failure
                // leaves the card where it was rather than at whichever
                // step stopped answering.
                let mut state = lock(&self.state);
                state.last_error = Some(e.as_msg());
                drop(state);
                Err(e)
            }
        }
    }

    fn confirm(&self) -> ModuleResult {
        let mut state = lock(&self.state);
        let Some(pending) = state.pending.take() else {
            return Err(ModuleError::localised(
                ErrorKind::InvalidParams,
                msg!("overclock.err.nothingPending", "there is nothing waiting to be confirmed"),
            ));
        };
        state.fault_watch = None;
        let applied = state.applied.get(&pending.gpu).copied().unwrap_or_default();
        state.config.targets.insert(pending.gpu.clone(), applied);
        state.config.armed_gpu = None;
        state.last_note = Some(msg!(
            "overclock.note.kept",
            { "gpu" => pending.gpu.clone() },
            "kept the change to {gpu}"
        ));
        persist(&self.store, &mut state);
        drop(state);
        Ok(self.status())
    }

    /// Undoes the pending change now rather than when the timer says so.
    ///
    /// The same revert the watchdog would do, which is the point: a user
    /// pressing "undo" and a desktop that never came back must leave the
    /// card in the same place.
    fn cancel(&self) -> ModuleResult {
        let pending = lock(&self.state).pending.clone();
        let Some(pending) = pending else {
            return Err(ModuleError::localised(
                ErrorKind::InvalidParams,
                msg!("overclock.err.nothingPending", "there is nothing waiting to be confirmed"),
            ));
        };
        revert(&self.state, &self.probe, &self.store, pending, RevertReason::Undone)?;
        Ok(self.status())
    }

    /// Back to stock, on one card or on all of them.
    ///
    /// Deliberately not gated on consent, and deliberately allowed while a
    /// confirmation is pending: there must be no state of this module in
    /// which "put it back" is refused.
    ///
    /// A card this daemon has never moved is *cleared* rather than
    /// written: there is nothing of ours on it to undo, and writing stock
    /// over whatever somebody else set would be this module touching
    /// hardware it was not asked about - the same rule the fan module
    /// follows at startup (`dev/TODO.md` §3).
    fn reset(&self, gpu_id: Option<String>) -> ModuleResult {
        let probe = lock(&self.probe).clone();
        let ids: Vec<String> = match gpu_id {
            Some(id) => vec![id],
            None => probe.gpus.iter().filter(|g| g.drivable()).map(|g| g.id.clone()).collect(),
        };

        let mut failures: Vec<(String, ModuleError)> = Vec::new();
        let mut written = 0;
        for id in ids {
            let Some(gpu) = probe.gpu(&id) else {
                return Err(ModuleError::InvalidParams(format!("no GPU with id '{id}'")));
            };

            if needs_undoing(lock(&self.state).applied.get(&id).copied()) {
                match write_target(gpu, Target::default()) {
                    Ok(()) => written += 1,
                    Err(e) => {
                        failures.push((id, e));
                        continue;
                    }
                }
            }

            let mut state = lock(&self.state);
            state.applied.insert(id.clone(), Target::default());
            state.config.targets.insert(id.clone(), Target::default());
            if state.pending.as_ref().is_some_and(|p| p.gpu == id) {
                state.pending = None;
                state.fault_watch = None;
                state.config.armed_gpu = None;
            }
            persist(&self.store, &mut state);
        }

        if !failures.is_empty() {
            let notes: Vec<Msg> = failures
                .iter()
                .map(|(id, e)| {
                    msg!(
                        "overclock.err.perGpu",
                        { "id" => id.clone(), "error" => e.to_string() },
                        "{id}: {error}"
                    )
                })
                .collect();
            let mut state = lock(&self.state);
            state.last_error = Msg::join(notes, "; ");
            drop(state);
            let (_, first) = failures.remove(0);
            return Err(keep_kind(first, "could not go back to stock"));
        }

        let mut state = lock(&self.state);
        state.last_error = None;
        state.last_note = Some(if written > 0 {
            msg!(
                "overclock.note.backToStock",
                "back to the clocks the firmware shipped"
            )
        } else {
            msg!(
                "overclock.note.nothingToUndo",
                "nothing to undo: this daemon has not moved these clocks"
            )
        });
        drop(state);
        Ok(self.status())
    }

    // --- the climb ------------------------------------------------------

    /// Walks from `from` to `to` one step at a time, checking after each
    /// one, and puts the card back if any step fails.
    ///
    /// No lock is held while this runs: each step talks to a driver, and a
    /// `getState` that blocked for the length of a climb would make the UI
    /// look like the thing that had hung.
    fn climb(&self, gpu: &GpuProbe, from: Target, to: Target) -> Result<(), ModuleError> {
        let steps = plan::ramp(from, to);
        for step in steps {
            if let Err(e) = write_target(gpu, step) {
                let _ = write_target(gpu, from);
                return Err(step_error(step, from, e));
            }
            std::thread::sleep(STEP_SETTLE);
            if let Err(e) = verify(gpu, step) {
                let _ = write_target(gpu, from);
                return Err(step_error(step, from, e));
            }
        }
        Ok(())
    }

    // --- reading the request --------------------------------------------

    fn requested_gpu(&self, params: &Value) -> Result<GpuProbe, ModuleError> {
        let probe = lock(&self.probe);
        match params.get("gpu").and_then(Value::as_str) {
            Some(id) => probe
                .gpu(id)
                .cloned()
                .ok_or_else(|| ModuleError::InvalidParams(format!("no GPU with id '{id}'"))),
            None => probe.default_gpu().cloned().ok_or_else(|| {
                ModuleError::localised(
                    ErrorKind::NotCapable,
                    msg!(
                        "overclock.err.nothingToTune",
                        { "detail" => probe.detail.text.clone() },
                        "nothing here can be tuned: {detail}"
                    ),
                )
            }),
        }
    }

    /// What is on the card now, falling back to what was confirmed for it,
    /// and to stock for a card this daemon has never written.
    fn current(&self, id: &str) -> Target {
        let state = lock(&self.state);
        state
            .applied
            .get(id)
            .copied()
            .unwrap_or_else(|| state.config.targets.get(id).copied().unwrap_or_default())
    }

    fn confirmed(&self, state: &State, id: &str) -> Target {
        state.config.targets.get(id).copied().unwrap_or_default()
    }

    /// A request is a change to what is already there, not a whole new
    /// state: leaving `memOffsetMhz` out means "and don't touch the
    /// memory", which is what a UI with one slider on screen means too.
    fn merge(&self, params: &Value, current: Target) -> Result<Target, ModuleError> {
        let mut target = current;

        if let Some(value) = params.get("coreOffsetMhz") {
            target.core_offset_mhz = as_mhz(value, "coreOffsetMhz")?;
        }
        if let Some(value) = params.get("memOffsetMhz") {
            target.mem_offset_mhz = as_mhz(value, "memOffsetMhz")?;
        }
        // An explicit null is how a lock is taken off, which is a different
        // request from not mentioning it.
        match params.get("clockLock") {
            None => {}
            Some(Value::Null) => target.core_clock = None,
            Some(value) => {
                let min = value.get("minMhz").ok_or_else(|| {
                    ModuleError::InvalidParams("clockLock needs minMhz and maxMhz".into())
                })?;
                let max = value.get("maxMhz").ok_or_else(|| {
                    ModuleError::InvalidParams("clockLock needs minMhz and maxMhz".into())
                })?;
                target.core_clock = Some(ClockLock {
                    min_mhz: as_mhz(min, "clockLock.minMhz")?,
                    max_mhz: as_mhz(max, "clockLock.maxMhz")?,
                });
            }
        }
        Ok(target)
    }

    fn hold_from(&self, params: &Value) -> Result<u64, ModuleError> {
        match params.get("holdSecs") {
            None => Ok(hold_secs(&lock(&self.state).config)),
            Some(value) => {
                let secs = value.as_u64().ok_or_else(|| {
                    ModuleError::InvalidParams("holdSecs must be a whole number of seconds".into())
                })?;
                Ok(secs.clamp(MIN_HOLD_SECS, MAX_HOLD_SECS))
            }
        }
    }
}

impl Default for OverclockModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for OverclockModule {
    fn id(&self) -> &'static str {
        "overclock"
    }

    fn is_supported(&self) -> bool {
        lock(&self.probe).supported
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getState" => Ok(self.status()),

            "probe" => {
                let allow_writes =
                    params.get("allowWrites").and_then(Value::as_bool).unwrap_or(false);
                let fresh = probe::probe(allow_writes);
                *lock(&self.probe) = fresh;
                Ok(self.status())
            }

            "setConsent" => {
                let accepted = params.get("accepted").and_then(Value::as_bool).ok_or_else(|| {
                    ModuleError::InvalidParams("params.accepted must be a boolean".into())
                })?;
                self.set_consent(accepted)
            }

            "apply" => self.apply(&params),
            "confirm" => self.confirm(),
            "cancel" => self.cancel(),

            "reset" => {
                let gpu = params.get("gpu").and_then(Value::as_str).map(str::to_string);
                self.reset(gpu)
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

/// What the watchdog should do this tick, with no thread, no clock of its
/// own and no GPU in it.
///
/// The whole of the loop's judgement lives here so it can be tested: the
/// thread above supplies "what is pending", "has the card complained" and
/// "what time is it", and this decides - once - which of the two reasons
/// applies, or neither.
///
/// The deadline is checked first. Both answers end in the same `revert()`
/// to the same target, so when a hold expires *and* a fault is queued in
/// the same 500 ms the difference is only which sentence the user reads,
/// and "you never confirmed this" is the older, surer fact.
fn watchdog_tick(pending: Option<&Pending>, fault: bool, now: Instant) -> Option<RevertReason> {
    let pending = pending?;
    if now >= pending.deadline {
        Some(RevertReason::NotConfirmed)
    } else if fault {
        Some(RevertReason::FaultReported)
    } else {
        None
    }
}

/// The fault signal for a card about to be left on an unconfirmed offset,
/// where there is one.
///
/// Only NVIDIA, because NVML is the only thing that reports this and the
/// only vendor this module writes to at all. Everything else - a card
/// whose id is not an index, a driver without the event API - is `None`,
/// which is exactly today's behaviour: the timer alone.
fn fault_watch(gpu: &GpuProbe) -> Option<nvml::EventWatch> {
    if gpu.vendor != Vendor::Nvidia {
        return None;
    }
    nvml::EventWatch::create(nvidia_index(gpu).ok()?)
}

/// Undoes a pending change, putting the card back where it was before the
/// apply. Shared by the watchdog and by `overclock.cancel`, because "the
/// timer ran out" and "the user pressed undo" must not be two code paths
/// that can disagree about what going back means.
/// Why a pending change is being undone - decides the note the state shows.
#[derive(Clone, Copy)]
enum RevertReason {
    /// The confirm window ran out.
    NotConfirmed,
    /// The user pressed "undo".
    Undone,
    /// The driver said the card faulted while the change was still
    /// waiting to be confirmed. Distinct from `NotConfirmed` on purpose:
    /// a user reading `note` afterwards has to be able to tell "you took
    /// too long to click" apart from "the card actually complained".
    FaultReported,
}

impl RevertReason {
    fn note(self, gpu: &str) -> Msg {
        match self {
            Self::NotConfirmed => msg!(
                "overclock.note.revertedUnconfirmed",
                { "gpu" => gpu },
                "the change to {gpu} was not confirmed, so it was undone"
            ),
            Self::Undone => msg!(
                "overclock.note.revertedUndone",
                { "gpu" => gpu },
                "the change to {gpu} was undone"
            ),
            Self::FaultReported => msg!(
                "overclock.note.revertedFault",
                { "gpu" => gpu },
                "the change to {gpu} was reverted: the driver reported a fault"
            ),
        }
    }
}

fn revert(
    state: &Arc<Mutex<State>>,
    probe: &Arc<Mutex<Probe>>,
    store: &ConfigStore,
    pending: Pending,
    reason: RevertReason,
) -> Result<(), ModuleError> {
    let gpu = lock(probe).gpu(&pending.gpu).cloned();
    let outcome = match &gpu {
        Some(gpu) => write_target(gpu, pending.revert_to),
        None => Err(ModuleError::localised(
            ErrorKind::Failed,
            msg!("overclock.err.gpuGone", { "gpu" => pending.gpu.clone() }, "{gpu} is gone"),
        )),
    };

    let mut guard = lock(state);
    guard.pending = None;
    guard.fault_watch = None;
    guard.config.armed_gpu = None;
    match &outcome {
        Ok(()) => {
            guard.applied.insert(pending.gpu.clone(), pending.revert_to);
            guard.last_note = Some(reason.note(&pending.gpu));
        }
        Err(e) => {
            guard.last_error = Some(msg!(
                "overclock.err.couldNotUndo",
                { "error" => e.to_string() },
                "could not undo the overclock: {error}"
            ))
        }
    }
    persist(store, &mut guard);
    outcome
}

// --- talking to the hardware -------------------------------------------

/// Writes one whole target to one card. The only place in this crate that
/// changes anything, so it is the only place a new vendor has to be taught
/// about.
fn write_target(gpu: &GpuProbe, target: Target) -> Result<(), ModuleError> {
    match gpu.vendor {
        Vendor::Nvidia => write_nvidia(gpu, target),
        // Detected and deliberately not driven; `probe` says why in words.
        _ => Err(ModuleError::localised(ErrorKind::NotCapable, gpu.detail.clone())),
    }
}

fn write_nvidia(gpu: &GpuProbe, target: Target) -> Result<(), ModuleError> {
    let index = nvidia_index(gpu)?;
    let nvidia = Nvidia::detect();

    // A knob is written only where it is actually being changed. Putting an
    // offset back at the value it already has is not free: on a screen with
    // no Coolbits it is *refused*, which would fail an apply that only ever
    // asked for a clock lock - and the clock lock is the mechanism that
    // works on the machine this was written on.
    if gpu.core_offset.is_some() && differs(nvidia.core_offset(index), target.core_offset_mhz) {
        nvidia.set_core_offset(index, target.core_offset_mhz).map_err(nvidia_error)?;
    }
    if gpu.mem_offset.is_some() && differs(nvidia.mem_offset(index), target.mem_offset_mhz) {
        nvidia.set_mem_offset(index, target.mem_offset_mhz).map_err(nvidia_error)?;
    }
    if gpu.clock_lock.is_some() {
        match target.core_clock {
            Some(lock) => nvidia.lock_clocks(index, lock).map_err(nvidia_error)?,
            None => nvidia.reset_clocks(index).map_err(nvidia_error)?,
        }
    }
    Ok(())
}

/// Whether a knob has to be written at all.
///
/// A value that could not be *read* counts as different: an offset we
/// cannot see is not one we can claim already matches, and skipping the
/// write on that basis would report a change that never happened.
fn differs(current: Result<(i32, Option<plan::Range>), NvidiaError>, wanted: i32) -> bool {
    match current {
        Ok((value, _)) => value != wanted,
        Err(_) => true,
    }
}

/// Reads back what was just written.
///
/// The point is not to catch a driver that lies - it is to catch a card
/// that has stopped answering at all, which is the failure this module is
/// built around and which looks, from here, exactly like a query that never
/// comes back with the value we set.
fn verify(gpu: &GpuProbe, expected: Target) -> Result<(), ModuleError> {
    if gpu.vendor != Vendor::Nvidia {
        return Ok(());
    }
    let index = nvidia_index(gpu)?;
    let nvidia = Nvidia::detect();

    if gpu.core_offset.is_some() {
        let (value, _) = nvidia.core_offset(index).map_err(nvidia_error)?;
        if value != expected.core_offset_mhz {
            return Err(ModuleError::localised(
                ErrorKind::Failed,
                msg!(
                    "overclock.err.verifyCore",
                    { "got" => value, "asked" => expected.core_offset_mhz },
                    "the driver reports a core offset of {got} MHz after being asked for {asked} MHz"
                ),
            ));
        }
    }
    if gpu.mem_offset.is_some() {
        let (value, _) = nvidia.mem_offset(index).map_err(nvidia_error)?;
        if value != expected.mem_offset_mhz {
            return Err(ModuleError::localised(
                ErrorKind::Failed,
                msg!(
                    "overclock.err.verifyMem",
                    { "got" => value, "asked" => expected.mem_offset_mhz },
                    "the driver reports a memory offset of {got} MHz after being asked for {asked} MHz"
                ),
            ));
        }
    }
    Ok(())
}

fn nvidia_index(gpu: &GpuProbe) -> Result<u32, ModuleError> {
    gpu.id
        .strip_prefix("nvidia:")
        .and_then(|index| index.parse().ok())
        .ok_or_else(|| ModuleError::Internal(format!("'{}' is not an nvidia-smi index", gpu.id)))
}

/// Hardware failures, translated for the socket.
///
/// The distinction that matters is between "this machine will never do it"
/// and "this machine is not set up to do it": a missing `Coolbits` is a
/// line in an X configuration file, and reporting it as `notCapable` would
/// tell a UI to hide a page that one edit would light up.
fn nvidia_error(e: NvidiaError) -> ModuleError {
    let kind = match e {
        NvidiaError::NotInstalled(_) => ErrorKind::NotCapable,
        NvidiaError::NoDisplay(_) | NvidiaError::Refused(_) => ErrorKind::Failed,
        NvidiaError::NeedsRoot(_) => ErrorKind::PermissionDenied,
        NvidiaError::Unreadable { .. } => ErrorKind::Io,
    };
    ModuleError::localised(kind, e.to_msg())
}

/// Names the step that failed, because "the apply failed" is useless to
/// somebody deciding whether their card is one offset away from stable or
/// nowhere near it.
///
/// The *kind* is carried through unchanged. A UI is entitled to branch on
/// it - `permissionDenied` is the one it should offer to elevate for - and
/// flattening everything to `failed` on the way out would take that away
/// at exactly the point where it is most useful.
fn step_error(step: Target, from: Target, e: ModuleError) -> ModuleError {
    keep_kind(
        e,
        &format!(
            "stopped at core {:+} MHz / memory {:+} MHz and went back to core {:+} / memory {:+}",
            step.core_offset_mhz, step.mem_offset_mhz, from.core_offset_mhz, from.mem_offset_mhz
        ),
    )
}

/// Re-wraps an error with something said in front of it, in the same kind
/// it arrived as. Built from the inner message rather than from
/// `to_string`, so a `permissionDenied` does not end up announcing its
/// prefix twice.
fn keep_kind(e: ModuleError, context: &str) -> ModuleError {
    match e {
        ModuleError::PermissionDenied(m) => {
            ModuleError::PermissionDenied(format!("{context}: {m}"))
        }
        ModuleError::NotCapable(m) => ModuleError::NotCapable(format!("{context}: {m}")),
        ModuleError::Io(m) => ModuleError::Io(format!("{context}: {m}")),
        ModuleError::Busy(m) => ModuleError::Busy(format!("{context}: {m}")),
        ModuleError::Unsupported => ModuleError::Unsupported,
        ModuleError::Localised { kind, msg } => ModuleError::localised(
            kind,
            msg!(
                "overclock.err.context",
                { "context" => context, "detail" => msg.text },
                "{context}: {detail}"
            ),
        ),
        other => ModuleError::Failed(format!("{context}: {other}")),
    }
}

/// Whether going back to stock means writing anything.
///
/// `None` is a card this daemon has never written, which is not the same as
/// a card at stock - it is a card whose state is somebody else's business.
fn needs_undoing(applied: Option<Target>) -> bool {
    applied.is_some_and(|target| !target.is_stock())
}

// --- odds and ends ------------------------------------------------------

fn hold_secs(config: &OverclockConfig) -> u64 {
    config.hold_secs.unwrap_or(DEFAULT_HOLD_SECS).clamp(MIN_HOLD_SECS, MAX_HOLD_SECS)
}

fn as_mhz(value: &Value, field: &str) -> Result<i32, ModuleError> {
    value
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| ModuleError::InvalidParams(format!("params.{field} must be a whole number of MHz")))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn persist(store: &ConfigStore, state: &mut State) {
    match store.save("overclock", &state.config) {
        Ok(()) => state.last_save_error = None,
        Err(e) => {
            log_warn!("could not save overclock config: {e}");
            state.last_save_error = Some(e.to_string());
        }
    }
}

/// A poisoned lock here would mean a panic while an offset was half
/// applied. Recovering the guard keeps the watchdog alive, which is the
/// thing that puts the card back.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One directory per test: they run in the same process, in parallel,
    /// and a shared config file would have them reading each other's.
    fn store(tag: &str) -> ConfigStore {
        let dir = std::env::temp_dir().join(format!("pyren-oc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp config dir");
        ConfigStore::at(dir)
    }

    /// A change armed now, for as long as the caller says. Only the
    /// deadline matters to `watchdog_tick`; the rest is what a real
    /// `apply()` would have put there.
    fn armed(hold: Duration) -> Pending {
        Pending {
            gpu: "nvidia:0".to_string(),
            revert_to: Target::default(),
            deadline: Instant::now() + hold,
        }
    }

    /// A card with no knobs, named and vendored as the caller needs. The
    /// fault watch is decided on the id and the vendor alone.
    fn gpu_probe(id: &str, vendor: Vendor) -> GpuProbe {
        GpuProbe {
            id: id.to_string(),
            name: "a card".to_string(),
            vendor,
            driver: "whatever".to_string(),
            core_offset: None,
            mem_offset: None,
            clock_lock: None,
            offsets_writable: None,
            detail: Msg::literal(""),
        }
    }

    /// The whole safety story starts here: a machine nobody has spoken to
    /// is at stock, has consented to nothing, and restores nothing.
    #[test]
    fn a_fresh_config_is_stock_and_unconsented() {
        let config = OverclockConfig::default();
        assert!(config.consent.is_none());
        assert!(!config.restore_on_start);
        assert!(config.targets.is_empty());
        assert!(config.armed_gpu.is_none());
    }

    #[test]
    fn an_offset_cannot_be_applied_before_the_warning_is_accepted() {
        let module = OverclockModule::with_store(store("unconsented"));
        let error = module
            .call("apply", json!({ "coreOffsetMhz": 50 }))
            .expect_err("an unconsented apply must be refused");
        // On a machine with no tunable GPU the refusal is about the
        // hardware instead, which is also a refusal - what must never
        // happen is an offset being written.
        assert!(matches!(
            error.kind(),
            ErrorKind::InvalidParams | ErrorKind::NotCapable
        ));
    }

    #[test]
    fn the_consent_text_travels_with_the_state() {
        let module = OverclockModule::with_store(store("consent-text"));
        let state = module.call("getState", Value::Null).expect("a state");
        assert_eq!(state["consent"]["text"], CONSENT);
        assert_eq!(state["consent"]["accepted"], false);
    }

    /// Consenting is not applying: saying yes to the warning must leave the
    /// card exactly where it was.
    #[test]
    fn consent_on_its_own_changes_no_clock() {
        let module = OverclockModule::with_store(store("consent-only"));
        let state = module.call("setConsent", json!({ "accepted": true })).expect("consent");
        assert_eq!(state["consent"]["accepted"], true);
        assert!(state["pending"].is_null());
        assert!(lock(&module.state).applied.is_empty());
    }

    #[test]
    fn confirming_nothing_is_a_refusal_rather_than_a_no_op() {
        let module = OverclockModule::with_store(store("confirm-nothing"));
        assert_eq!(
            module.call("confirm", Value::Null).unwrap_err().kind(),
            ErrorKind::InvalidParams
        );
    }

    /// Undo and confirm are the same shape: both refuse when there is
    /// nothing armed, rather than quietly succeeding at nothing.
    #[test]
    fn cancelling_nothing_is_a_refusal_too() {
        let module = OverclockModule::with_store(store("cancel-nothing"));
        assert_eq!(
            module.call("cancel", Value::Null).unwrap_err().kind(),
            ErrorKind::InvalidParams
        );
    }

    #[test]
    fn an_unknown_method_is_named_in_the_error() {
        let module = OverclockModule::with_store(store("unknown-method"));
        assert!(matches!(module.call("overvolt", Value::Null), Err(ModuleError::UnknownMethod(m)) if m == "overvolt"));
    }

    /// The crash signature: a config that is still armed means the machine
    /// went away while overclocked, and the next boot must not put the
    /// offset back however the flags are set.
    #[test]
    fn an_armed_config_stops_the_next_boot_restoring_anything() {
        let store = store("armed-boot");
        let config = OverclockConfig {
            consent: Some(Consent { accepted_at: now_secs(), version: CONSENT_VERSION }),
            targets: BTreeMap::from([(
                "nvidia:0".to_string(),
                Target { core_offset_mhz: 150, ..Target::default() },
            )]),
            restore_on_start: true,
            armed_gpu: Some("nvidia:0".to_string()),
            hold_secs: None,
        };
        store.save("overclock", &config).expect("a saved config");

        let module = OverclockModule::with_store(store);
        let state = module.call("getState", Value::Null).expect("a state");
        assert_eq!(state["unconfirmedAtStart"], true);
        assert!(lock(&module.state).applied.is_empty(), "nothing may be written on such a boot");
        assert!(state["note"]["text"].as_str().unwrap().contains("never confirmed"));
    }

    /// ...and the armed flag must not survive that boot, or the machine
    /// would refuse to restore for ever.
    #[test]
    fn the_armed_flag_is_cleared_once_it_has_been_acted_on() {
        let store = store("armed-cleared");
        store
            .save(
                "overclock",
                &OverclockConfig { armed_gpu: Some("nvidia:0".into()), ..Default::default() },
            )
            .expect("a saved config");
        let module = OverclockModule::with_store(store);
        assert!(lock(&module.state).config.armed_gpu.is_none());
    }

    #[test]
    fn the_hold_is_clamped_to_something_a_person_can_react_within() {
        let module = OverclockModule::with_store(store("hold"));
        assert_eq!(module.hold_from(&json!({ "holdSecs": 1 })).unwrap(), MIN_HOLD_SECS);
        assert_eq!(module.hold_from(&json!({ "holdSecs": 99999 })).unwrap(), MAX_HOLD_SECS);
        assert_eq!(module.hold_from(&json!({})).unwrap(), DEFAULT_HOLD_SECS);
    }

    #[test]
    fn a_request_that_mentions_one_offset_leaves_the_other_alone() {
        let module = OverclockModule::with_store(store("merge"));
        let current = Target { core_offset_mhz: 30, mem_offset_mhz: 200, core_clock: None };
        let merged = module.merge(&json!({ "coreOffsetMhz": 60 }), current).unwrap();
        assert_eq!(merged.core_offset_mhz, 60);
        assert_eq!(merged.mem_offset_mhz, 200);
    }

    /// An explicit null takes the lock off; leaving the field out keeps it.
    #[test]
    fn a_clock_lock_is_removed_only_when_it_is_named() {
        let module = OverclockModule::with_store(store("lock-merge"));
        let locked = Target {
            core_clock: Some(ClockLock { min_mhz: 1000, max_mhz: 2000 }),
            ..Target::default()
        };
        assert_eq!(
            module.merge(&json!({}), locked).unwrap().core_clock,
            locked.core_clock
        );
        assert_eq!(
            module.merge(&json!({ "clockLock": null }), locked).unwrap().core_clock,
            None
        );
    }

    /// The kind is what a client branches on, so it has to survive being
    /// given context - `permissionDenied` is the one a UI offers to
    /// elevate for, and it is useless as a `failed`.
    #[test]
    fn adding_context_to_an_error_keeps_its_kind() {
        let e = keep_kind(ModuleError::PermissionDenied("nvidia-smi said no".into()), "stopped");
        assert!(matches!(e, ModuleError::PermissionDenied(_)));
        let message = e.to_string();
        assert!(message.contains("stopped") && message.contains("nvidia-smi said no"));
        assert_eq!(
            message.matches("elevated privileges").count(),
            1,
            "the prefix must not be announced twice"
        );
    }

    /// A card this daemon never touched has nothing of ours on it, so
    /// "back to stock" writes nothing - writing anyway would overwrite
    /// whatever another tool had set.
    #[test]
    fn undoing_a_card_we_never_moved_writes_nothing() {
        assert!(!needs_undoing(None));
        assert!(!needs_undoing(Some(Target::default())));
        assert!(needs_undoing(Some(Target { core_offset_mhz: 100, ..Target::default() })));
    }

    /// The knob that is not changing is not written, so asking only for a
    /// clock lock never touches an offset - which matters because on a
    /// screen with no Coolbits that write is a refusal.
    #[test]
    fn a_knob_that_is_not_changing_is_not_written() {
        assert!(!differs(Ok((0, None)), 0));
        assert!(differs(Ok((0, None)), 15));
        assert!(
            differs(Err(NvidiaError::NotInstalled("nvidia-settings")), 0),
            "an offset we could not read is not one we may assume matches"
        );
    }

    #[test]
    fn a_wrong_type_is_a_refusal_and_not_a_zero() {
        let module = OverclockModule::with_store(store("bad-type"));
        assert!(matches!(
            module.merge(&json!({ "coreOffsetMhz": "lots" }), Target::default()),
            Err(ModuleError::InvalidParams(_))
        ));
    }

    /// A tick is only ever about a change that is waiting to be
    /// confirmed. With nothing armed there is nothing to undo, and a card
    /// that is complaining about somebody else's workload must not make
    /// this daemon write to hardware it did not arm.
    #[test]
    fn a_watchdog_tick_with_nothing_armed_undoes_nothing_however_the_card_behaves() {
        assert!(watchdog_tick(None, false, Instant::now()).is_none());
        assert!(watchdog_tick(None, true, Instant::now()).is_none());
    }

    /// The ordinary case, and the one that must stay silent: the whole
    /// point of the hold is that the user has those seconds to look at the
    /// screen and press confirm without the daemon interrupting.
    #[test]
    fn a_change_still_inside_its_hold_on_a_healthy_card_is_left_alone() {
        let pending = armed(Duration::from_secs(20));
        assert!(watchdog_tick(Some(&pending), false, Instant::now()).is_none());
    }

    /// The path that already had hardware confidence behind it, now
    /// through the shared decision: a hold that runs out with nobody there
    /// to confirm is still an unprompted revert, and still says so.
    #[test]
    fn a_hold_that_runs_out_undoes_the_change_without_anyone_asking() {
        let pending = armed(Duration::from_secs(20));
        let expired = pending.deadline + Duration::from_millis(1);
        assert!(matches!(
            watchdog_tick(Some(&pending), false, expired),
            Some(RevertReason::NotConfirmed)
        ));
    }

    /// The reason this feature exists. A card that faults at second 2 of a
    /// twenty second hold must not be left on the bad clock until second
    /// 20 just because the timer has not run out yet.
    #[test]
    fn a_card_that_reports_a_fault_is_not_left_waiting_for_the_rest_of_the_hold() {
        let pending = armed(Duration::from_secs(20));
        assert!(matches!(
            watchdog_tick(Some(&pending), true, Instant::now()),
            Some(RevertReason::FaultReported)
        ));
    }

    /// Both at once lands in the same `revert()` to the same target, so
    /// all that is being chosen is the sentence the user reads afterwards.
    /// "You never confirmed this" is the older and surer of the two facts,
    /// so it wins - and this pins that tie-break down rather than leaving
    /// it to whichever branch happens to be written first.
    #[test]
    fn a_hold_that_ran_out_is_reported_as_such_even_when_the_card_also_complained() {
        let pending = armed(Duration::from_secs(20));
        let expired = pending.deadline + Duration::from_millis(1);
        assert!(matches!(
            watchdog_tick(Some(&pending), true, expired),
            Some(RevertReason::NotConfirmed)
        ));
    }

    /// "Exactly one revert, not zero and not two" is two separate claims.
    /// One of them the type settles: a tick returns a single `Option`, so
    /// it cannot name two reasons no matter what it is given. The other is
    /// this one - that a revert takes the pending with it, so the next
    /// tick 500 ms later finds nothing left to undo and the card is not
    /// written to twice.
    ///
    /// The revert here is made to fail on purpose, with a card id no probe
    /// will match: even the failing path has to leave nothing armed, or a
    /// card that has gone away would be retried every half second forever.
    #[test]
    fn a_revert_disarms_what_it_undid_so_the_next_tick_finds_nothing() {
        let module = OverclockModule::with_store(store("revert-disarms"));
        let pending = Pending {
            gpu: "absent:0".to_string(),
            revert_to: Target::default(),
            deadline: Instant::now() + Duration::from_secs(20),
        };
        lock(&module.state).pending = Some(pending.clone());

        let outcome = revert(
            &module.state,
            &module.probe,
            &module.store,
            pending,
            RevertReason::FaultReported,
        );
        assert!(outcome.is_err(), "a card that is not there cannot be written to");

        let state = lock(&module.state);
        assert!(state.pending.is_none(), "a revert must disarm what it undid");
        assert!(state.fault_watch.is_none(), "the fault watch outlives nothing");
        assert!(state.config.armed_gpu.is_none());
        assert!(
            watchdog_tick(state.pending.as_ref(), true, Instant::now()).is_none(),
            "the tick after a revert must not ask for a second one"
        );
    }

    /// The whole reason `FaultReported` is a variant of its own and not a
    /// reuse of `NotConfirmed`: someone reading the note afterwards has to
    /// be able to tell "you took too long to click" apart from "the card
    /// actually complained", and to see which card it was.
    #[test]
    fn a_fault_note_reads_differently_from_a_timeout_note_and_names_the_card() {
        let fault = RevertReason::FaultReported.note("nvidia:0");
        let timeout = RevertReason::NotConfirmed.note("nvidia:0");

        assert_ne!(fault.key, timeout.key, "two reasons, two keys to translate");
        assert_ne!(fault.text, timeout.text);
        assert_eq!(fault.params["gpu"], "nvidia:0");
        assert!(fault.text.contains("nvidia:0"), "the note must say which card: {}", fault.text);
        assert!(!fault.text.contains('{'), "every placeholder must be filled: {}", fault.text);
        assert!(fault.text.contains("fault"), "a fault must read as one: {}", fault.text);
    }

    /// NVML is the only thing that reports this signal and NVIDIA is the
    /// only vendor this module writes to, so every other card falls back
    /// to exactly the behaviour that was there before: the timer alone,
    /// silently, with no watch and no message about not having one.
    #[test]
    fn a_card_that_is_not_an_nvidia_index_gets_no_fault_watch() {
        assert!(fault_watch(&gpu_probe("drm:card0", Vendor::Intel)).is_none());
        assert!(fault_watch(&gpu_probe("drm:card1", Vendor::Amd)).is_none());
        assert!(
            fault_watch(&gpu_probe("drm:card1", Vendor::Nvidia)).is_none(),
            "an NVIDIA card whose id is not an nvidia-smi index has no index to register with"
        );
    }
}
