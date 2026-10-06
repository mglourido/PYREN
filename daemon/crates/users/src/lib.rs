//! Whose settings the daemon is running, on a machine more than one person
//! logs in to.
//!
//! The daemon is one root process started at boot, before anybody has
//! logged in, and until this crate it had one set of settings: whoever
//! changed them last changed them for everyone. Three things are decided
//! here, and nothing else:
//!
//! 1. **The settings in use are always somebody's.** The files under
//!    `/etc/pyren` stay where every module reads them - which is also what
//!    makes the daemon come up at boot with the most recently used ones -
//!    and this crate remembers whose they are (`owner`).
//! 2. **When another of Pyren's users becomes the active one, the daemon
//!    switches to theirs** - if they have left any. If they have not, the
//!    settings in use carry on and simply become theirs. Either way the
//!    previous owner's are put aside first, for when they come back. See
//!    [`profiles`].
//! 3. **When somebody who is not one of Pyren's users becomes the active
//!    one, the daemon can stand down** (`standDownForOthers`, off by
//!    default): it lets go of the fans, the power override and any
//!    overclock, and waits, so that a person who never asked for Pyren
//!    does not get another person's fan curve. The keyboard stays lit -
//!    its controller holds the colours by itself - and only an animated
//!    effect, which is the daemon's doing, stops.
//!
//! "One of Pyren's users" is membership of the socket's group - the same
//! line the daemon already draws around who may talk to it. The install is
//! machine-wide, so "has Pyren installed" cannot mean the binaries; what a
//! user has or has not is the admin's `usermod -aG pyren`.
//!
//! # Switching is a restart
//!
//! No module can reload its config while running, and teaching six of them
//! to would be six new ways for a fan loop to be caught between two
//! curves. Every module already knows how to start from a config file,
//! though, so that is what a switch is: the daemon lets go of the hardware
//! exactly as it does on SIGTERM, replaces itself with a fresh copy, and
//! the fresh copy - before it builds a single module - swaps the files and
//! marks the start as a hand-over ([`pyren_core::handover`]). Standing
//! down is the same restart, with the fresh copy waiting instead of
//! building anything.
//!
//! # Not knowing is not an answer
//!
//! Who is active is asked of other programs, and they can fail to reply.
//! A look that got no answer changes nothing: the daemon carries on with
//! the last person it did see ([`directory::Unknown`]). Without that, one
//! slow `loginctl` would start a daemon that is standing down, and one
//! slow directory lookup would stand down a daemon for its own user.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use pyren_config::ConfigStore;
use pyren_core::{log_info, log_warn, EventBus, Module, ModuleError, ModuleResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod directory;
pub mod profiles;

pub use directory::{ActiveUser, Directory, Unknown};
use profiles::Profiles;

/// How often the active user is asked after. One `loginctl` each time.
const POLL: Duration = Duration::from_secs(3);

/// How often the seat's hint is read between polls - a small sysfs file,
/// so a switch between two people's sessions is acted on in a fraction of
/// a second rather than up to a whole poll later.
const GLANCE: Duration = Duration::from_millis(250);

/// How many glances after the hint moved are followed by a look. logind
/// learns of a switch from the same kernel event this does, and may not
/// have caught up on the first.
const EAGER_LOOKS: u8 = 4;

/// What is persisted to `users.json`. The machine's, not any one user's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsersConfig {
    /// Let go of the hardware while somebody who is not one of Pyren's
    /// users is the active one. Off by default: a machine with one user
    /// and a guest account should not lose its fan curve to the guest
    /// without having been told to.
    pub stand_down_for_others: bool,
    /// Whose the settings in use are. `None` until one of Pyren's users
    /// has logged in under a build that keeps track.
    pub owner: Option<u32>,
    /// Set for the length of a restore and cleared after it, so one that
    /// was interrupted is finished rather than mistaken for the previous
    /// owner's settings and saved over theirs. While it is set the files
    /// in use are nobody's, and nothing else is done with them.
    pub restoring: Option<u32>,
}

/// The active user, and whether they are one of Pyren's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub uid: u32,
    pub name: String,
    pub member: bool,
}

/// What a look at who is active calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing: nobody is there, or the settings in use are already
    /// theirs, or they are not Pyren's user and nothing says to mind.
    Keep,
    /// One of Pyren's users with no settings of their own: the ones in
    /// use become theirs. Needs no restart.
    Adopt(u32),
    /// One of Pyren's users who has left settings behind: put them in use.
    Restore(u32),
    /// Somebody else, and the daemon was told to stand down for them.
    StandDown,
}

/// The whole policy, with nothing to look up.
pub fn decide(
    config: &UsersConfig,
    active: Option<&Person>,
    has_profile: impl Fn(u32) -> bool,
) -> Decision {
    let Some(person) = active else {
        return Decision::Keep;
    };
    if !person.member {
        return if config.stand_down_for_others {
            Decision::StandDown
        } else {
            Decision::Keep
        };
    }
    if config.owner == Some(person.uid) {
        Decision::Keep
    } else if has_profile(person.uid) {
        Decision::Restore(person.uid)
    } else {
        Decision::Adopt(person.uid)
    }
}

struct State {
    config: UsersConfig,
    /// The last person a look actually saw. Kept across looks that got no
    /// answer.
    active: Option<Person>,
    /// True while looks are getting no answer, so it is said once.
    blind: bool,
    /// A user whose turn could not be given them because the previous
    /// owner's settings could not be put aside. Not tried again until
    /// somebody else has been active: the watcher would otherwise restart
    /// the daemon every poll for as long as the disk is full.
    gave_up_on: Option<u32>,
}

/// Called when the daemon has to replace itself; given the reason for the
/// log. Does not return in the daemon.
pub type HandOver = Box<dyn Fn(&str) + Send>;

/// Called before the settings in use are copied as their owner's.
pub type Flush = Box<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct UsersModule {
    store: ConfigStore,
    profiles: Profiles,
    directory: Arc<dyn Directory>,
    state: Arc<Mutex<State>>,
    events: Arc<OnceLock<Arc<EventBus>>>,
    flush: Arc<OnceLock<Flush>>,
    poll: Duration,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

impl UsersModule {
    pub fn new() -> Self {
        Self::with_parts(ConfigStore::system(), Arc::new(directory::System::new()))
    }

    /// Builds the module against an explicit config store and account
    /// directory. Tests use this to stay out of `/etc` and to decide who
    /// is logged in.
    pub fn with_parts(store: ConfigStore, directory: Arc<dyn Directory>) -> Self {
        let config = store.load::<UsersConfig>("users").value;
        Self {
            profiles: Profiles::new(store.root()),
            store,
            directory,
            state: Arc::new(Mutex::new(State {
                config,
                active: None,
                blind: false,
                gave_up_on: None,
            })),
            events: Arc::new(OnceLock::new()),
            flush: Arc::new(OnceLock::new()),
            poll: POLL,
        }
    }

    /// Hands the module the bus to announce `users.changed` on.
    pub fn publish_to(&self, events: Arc<EventBus>) {
        let _ = self.events.set(events);
    }

    /// Hands the module a way to get the modules' files up to date before
    /// it copies them.
    ///
    /// A config file is not always what its module is doing - the running
    /// fan and power modes are only written down when something asks for
    /// them to survive a restart - and a copy taken without asking would
    /// put aside a mode this user left long ago as the one they were in.
    pub fn before_setting_aside(&self, flush: Flush) {
        let _ = self.flush.set(flush);
    }

    /// Makes the settings on disk the right person's, before any module
    /// reads them. Returns whether that makes this start a hand-over.
    ///
    /// **Blocks for as long as the daemon is to stand down** - which is
    /// the standing down: nothing has been built yet, so nothing is
    /// driving the hardware, and the caller simply does not get to build
    /// it until one of Pyren's users is back (or the setting is turned
    /// off in `users.json`, which is re-read while waiting because the
    /// socket that would otherwise change it is not being served).
    pub fn settle(&self) -> bool {
        let mut handed_over = self.finish_interrupted_restore();
        let mut waiting = false;
        loop {
            match self.look().0 {
                Decision::Keep => break,
                Decision::Adopt(uid) => {
                    self.adopt(uid);
                    break;
                }
                Decision::Restore(uid) => {
                    handed_over |= self.restore(uid);
                    break;
                }
                Decision::StandDown => {
                    if !waiting {
                        // Printed, not logged: it is the answer to "the
                        // service is running and nothing works".
                        println!(
                            "pyren-daemon: standing down - {} is the active user and is not in \
                             the '{}' group. Waiting for one of Pyren's users.",
                            self.active_name(),
                            pyren_core::socket_group()
                        );
                    }
                    waiting = true;
                    handed_over = true;
                    std::thread::sleep(self.poll);
                    self.reload();
                }
            }
        }
        if waiting {
            println!("pyren-daemon: no longer standing down; starting");
        }
        handed_over
    }

    /// Follows the active user for as long as the daemon runs.
    ///
    /// A user with no settings of their own is dealt with here. Anything
    /// that needs the modules rebuilt goes to `hand_over`, and this stops
    /// looking: the daemon it was looking for is on its way out.
    pub fn watch(&self, hand_over: HandOver) {
        let module = self.clone();
        let spawned = std::thread::Builder::new()
            .name("pyren-users".into())
            .spawn(move || {
                let mut hint = module.directory.seat_hint();
                let mut eager = 0;
                loop {
                    module.wait(&mut hint, &mut eager);
                    let (decision, changed) = module.look();
                    match decision {
                        Decision::Keep => {
                            if changed {
                                module.announce();
                            }
                        }
                        Decision::Adopt(uid) => {
                            module.adopt(uid);
                            module.announce();
                        }
                        Decision::Restore(_) => {
                            hand_over(&format!(
                                "{} is now the active user; switching to their settings",
                                module.active_name()
                            ));
                            return;
                        }
                        Decision::StandDown => {
                            hand_over(&format!(
                                "{} is now the active user and is not one of Pyren's; \
                                 standing down",
                                module.active_name()
                            ));
                            return;
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            log_warn!("could not start the active-user watcher: {e}");
        }
    }

    /// Sleeps until the next look is due: a whole poll, or less when the
    /// seat's hint has just moved.
    ///
    /// The time between somebody arriving and this noticing is time in
    /// which what they change is still taken for the previous owner's, so
    /// it is kept short where a cheap sign of an arrival exists.
    fn wait(&self, hint: &mut Option<String>, eager: &mut u8) {
        let glance = GLANCE.min(self.poll);
        if *eager > 0 {
            *eager -= 1;
            std::thread::sleep(glance);
            return;
        }
        let due = Instant::now() + self.poll;
        while Instant::now() < due {
            std::thread::sleep(glance);
            let now = self.directory.seat_hint();
            if now != *hint {
                *hint = now;
                *eager = EAGER_LOOKS;
                return;
            }
        }
    }

    /// One line for the startup report.
    pub fn summary(&self) -> String {
        let state = lock(&self.state);
        let owner = match state.config.owner {
            Some(uid) => format!("{}'s settings", self.name_of(uid)),
            None => "settings nobody has claimed yet".to_string(),
        };
        let active = match &state.active {
            Some(person) if person.member => format!("{} is active", person.name),
            Some(person) => format!("{} is active (not one of Pyren's users)", person.name),
            None => "nobody is logged in at the seat".to_string(),
        };
        format!("{owner}; {active}")
    }

    /// The person at the seat, or [`Unknown`] if either question about
    /// them went unanswered.
    fn see(&self) -> Result<Option<Person>, Unknown> {
        let Some(user) = self.directory.active()? else {
            return Ok(None);
        };
        Ok(Some(Person {
            member: self.directory.is_member(user.uid)?,
            uid: user.uid,
            name: user.name,
        }))
    }

    /// Asks who is active and what that calls for. The second value is
    /// whether it is a different answer from last time.
    fn look(&self) -> (Decision, bool) {
        let seen = self.see();
        let mut state = lock(&self.state);
        let changed = match seen {
            Ok(active) => {
                state.blind = false;
                let changed = state.active != active;
                state.active = active;
                changed
            }
            Err(Unknown) => {
                if !state.blind {
                    log_warn!(
                        "could not find out who is at the machine; \
                         carrying on as if nothing has changed"
                    );
                }
                state.blind = true;
                false
            }
        };
        let active_uid = state.active.as_ref().map(|person| person.uid);
        if state.gave_up_on.is_some() && state.gave_up_on != active_uid {
            state.gave_up_on = None;
        }
        // Half-restored files are nobody's to save or to restore over.
        if state.config.restoring.is_some() {
            return (Decision::Keep, changed);
        }
        let decision = decide(&state.config, state.active.as_ref(), |uid| {
            self.profiles.exists(uid)
        });
        let decision = match decision {
            Decision::Adopt(uid) | Decision::Restore(uid) if state.gave_up_on == Some(uid) => {
                Decision::Keep
            }
            other => other,
        };
        (decision, changed)
    }

    /// Puts the settings in use aside as their owner's, if they have one
    /// who is not `next`. False when that could not be done - and then
    /// nothing may be changed on top of them, or the owner's latest
    /// settings exist nowhere.
    fn set_aside(&self, config: &UsersConfig, next: u32) -> bool {
        let Some(owner) = config.owner.filter(|owner| *owner != next) else {
            return true;
        };
        if let Some(flush) = self.flush.get() {
            flush();
        }
        match self.profiles.save(owner) {
            Ok(()) => true,
            Err(e) => {
                log_warn!(
                    "could not keep {}'s settings aside ({e}); leaving them in use rather \
                     than losing them",
                    self.name_of(owner)
                );
                false
            }
        }
    }

    fn adopt(&self, uid: u32) {
        let mut state = lock(&self.state);
        if !self.set_aside(&state.config, uid) {
            state.gave_up_on = Some(uid);
            return;
        }
        state.config.owner = Some(uid);
        self.persist(&state.config);
        log_info!(
            "{} has no settings of their own yet; the ones in use are now theirs",
            self.name_of(uid)
        );
    }

    /// Returns whether the files in use changed.
    fn restore(&self, uid: u32) -> bool {
        let mut state = lock(&self.state);
        if !self.set_aside(&state.config, uid) {
            state.gave_up_on = Some(uid);
            return false;
        }
        state.config.restoring = Some(uid);
        self.persist(&state.config);
        self.put_back(&mut state.config, uid);
        true
    }

    /// A restore that was cut short - a power cut between two files - left
    /// the files in use half one person's and half another's. Running it
    /// again is safe, and has to happen before anything is set aside.
    fn finish_interrupted_restore(&self) -> bool {
        let mut state = lock(&self.state);
        let Some(uid) = state.config.restoring else {
            return false;
        };
        log_warn!(
            "switching to {}'s settings was interrupted; finishing it",
            self.name_of(uid)
        );
        self.put_back(&mut state.config, uid);
        true
    }

    fn put_back(&self, config: &mut UsersConfig, uid: u32) {
        match self.profiles.restore(uid) {
            Ok(restored) => {
                log_info!(
                    "switched to {}'s settings ({})",
                    self.name_of(uid),
                    restored.join(", ")
                );
                config.owner = Some(uid);
                config.restoring = None;
                self.persist(config);
            }
            // `restoring` stays set. The files in use may be half one
            // person's and half another's now, and with the owner left as
            // it was they would be saved as that owner's at the next
            // switch. Marked, they are nobody's until a later start gets
            // the restore through.
            Err(e) => log_warn!(
                "could not put {}'s settings back: {e}; it will be tried again at the next start",
                self.name_of(uid)
            ),
        }
    }

    fn persist(&self, config: &UsersConfig) {
        if let Err(e) = self.store.save("users", config) {
            log_warn!("could not save the users config: {e}");
        }
    }

    /// Picks up `standDownForOthers` from `users.json` again, while the
    /// daemon is standing down and a person editing that file is the only
    /// way to change it.
    ///
    /// Read directly, not through `ConfigStore::load`: that moves a file
    /// it cannot parse aside as `.json.bad`, which is the right thing to
    /// do to a corrupt file at startup and the wrong thing to do to one
    /// somebody is halfway through saving from an editor - it would take
    /// `owner` with it. A file that does not read is simply looked at
    /// again in a moment.
    fn reload(&self) {
        let Ok(text) = std::fs::read_to_string(self.store.path_for("users")) else {
            return;
        };
        let Ok(file) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if let Some(enabled) = file.get("standDownForOthers").and_then(Value::as_bool) {
            lock(&self.state).config.stand_down_for_others = enabled;
        }
    }

    fn announce(&self) {
        if let Some(bus) = self.events.get() {
            bus.publish("users.changed", self.status());
        }
    }

    fn name_of(&self, uid: u32) -> String {
        self.directory
            .name(uid)
            .unwrap_or_else(|| format!("uid {uid}"))
    }

    fn active_name(&self) -> String {
        lock(&self.state)
            .active
            .as_ref()
            .map(|person| person.name.clone())
            .unwrap_or_else(|| "nobody".to_string())
    }

    fn user(&self, uid: u32) -> Value {
        json!({ "uid": uid, "name": self.directory.name(uid) })
    }

    fn status(&self) -> Value {
        let state = lock(&self.state);
        json!({
            "standDownForOthers": state.config.stand_down_for_others,
            "group": pyren_core::socket_group(),
            "activeUser": state.active.as_ref().map(|person| json!({
                "uid": person.uid,
                "name": person.name,
                "member": person.member,
            })),
            "owner": state.config.owner.map(|uid| self.user(uid)),
            "profiles": self.profiles.list().into_iter().map(|uid| self.user(uid)).collect::<Vec<_>>(),
        })
    }
}

impl Default for UsersModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for UsersModule {
    fn id(&self) -> &'static str {
        "users"
    }

    fn is_supported(&self) -> bool {
        true
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getStatus" => Ok(self.status()),
            "setStandDownForOthers" => {
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| ModuleError::InvalidParams("enabled must be a bool".into()))?;
                {
                    let mut state = lock(&self.state);
                    state.config.stand_down_for_others = enabled;
                    self.persist(&state.config);
                }
                // Nothing is done about it here: the watcher's next look
                // sees the setting, and it is the one place that acts.
                self.announce();
                Ok(self.status())
            }
            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    /// A machine whose accounts and active user the test decides. uids
    /// from 1000 are people; the names are made from the number.
    #[derive(Default)]
    struct Machine {
        active: Mutex<Option<u32>>,
        members: BTreeSet<u32>,
        /// logind not answering.
        seat_down: AtomicBool,
        /// The account database not answering.
        accounts_down: AtomicBool,
    }

    impl Machine {
        fn with_members(members: &[u32]) -> Arc<Self> {
            Arc::new(Self {
                members: members.iter().copied().collect(),
                ..Self::default()
            })
        }

        fn log_in(&self, uid: u32) {
            *self.active.lock().unwrap() = Some(uid);
        }

        fn log_out(&self) {
            *self.active.lock().unwrap() = None;
        }
    }

    impl Directory for Machine {
        fn active(&self) -> Result<Option<ActiveUser>, Unknown> {
            if self.seat_down.load(Ordering::SeqCst) {
                return Err(Unknown);
            }
            Ok((*self.active.lock().unwrap()).map(|uid| ActiveUser {
                uid,
                name: format!("user{uid}"),
            }))
        }

        fn is_member(&self, uid: u32) -> Result<bool, Unknown> {
            if self.accounts_down.load(Ordering::SeqCst) {
                return Err(Unknown);
            }
            Ok(self.members.contains(&uid))
        }

        fn name(&self, uid: u32) -> Option<String> {
            Some(format!("user{uid}"))
        }
    }

    const ANA: u32 = 1000;
    const BEA: u32 = 1001;
    const GUEST: u32 = 1002;

    fn store(tag: &str) -> ConfigStore {
        let root =
            std::env::temp_dir().join(format!("pyren-users-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        ConfigStore::at(root)
    }

    fn module(store: &ConfigStore, machine: &Arc<Machine>) -> UsersModule {
        let mut module = UsersModule::with_parts(store.clone(), machine.clone());
        module.poll = Duration::from_millis(5);
        module
    }

    fn set_power(store: &ConfigStore, mode: &str) {
        store.save("power", &json!({ "mode": mode })).unwrap();
    }

    fn power(store: &ConfigStore) -> String {
        store.load::<Value>("power").value["mode"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn person(uid: u32, member: bool) -> Person {
        Person {
            uid,
            name: format!("user{uid}"),
            member,
        }
    }

    #[test]
    fn nobody_at_the_seat_changes_nothing() {
        let config = UsersConfig {
            stand_down_for_others: true,
            owner: Some(ANA),
            restoring: None,
        };
        assert_eq!(decide(&config, None, |_| true), Decision::Keep);
    }

    #[test]
    fn the_owner_coming_back_changes_nothing() {
        let config = UsersConfig {
            owner: Some(ANA),
            ..UsersConfig::default()
        };
        assert_eq!(
            decide(&config, Some(&person(ANA, true)), |_| true),
            Decision::Keep
        );
    }

    #[test]
    fn another_user_gets_their_settings_if_they_left_any() {
        let config = UsersConfig {
            owner: Some(ANA),
            ..UsersConfig::default()
        };
        assert_eq!(
            decide(&config, Some(&person(BEA, true)), |_| true),
            Decision::Restore(BEA)
        );
        assert_eq!(
            decide(&config, Some(&person(BEA, true)), |_| false),
            Decision::Adopt(BEA)
        );
    }

    #[test]
    fn someone_who_is_not_pyrens_is_left_with_the_settings_in_use_by_default() {
        let mut config = UsersConfig {
            owner: Some(ANA),
            ..UsersConfig::default()
        };
        let guest = person(GUEST, false);
        assert_eq!(decide(&config, Some(&guest), |_| true), Decision::Keep);
        config.stand_down_for_others = true;
        assert_eq!(decide(&config, Some(&guest), |_| true), Decision::StandDown);
    }

    #[test]
    fn the_first_user_to_log_in_takes_the_settings_as_they_are() {
        let store = store("first");
        let machine = Machine::with_members(&[ANA]);
        set_power(&store, "eco");
        machine.log_in(ANA);

        let users = module(&store, &machine);
        assert!(!users.settle(), "nothing changed hands on disk");
        assert_eq!(power(&store), "eco");
        assert_eq!(users.status()["owner"]["uid"], ANA);
        assert_eq!(users.status()["owner"]["name"], "user1000");
        // Recorded, so the next start knows too.
        assert_eq!(
            store.load::<UsersConfig>("users").value.owner,
            Some(ANA),
            "the owner must survive a restart"
        );
    }

    #[test]
    fn each_user_comes_back_to_what_they_left() {
        let store = store("two-users");
        let machine = Machine::with_members(&[ANA, BEA]);

        set_power(&store, "eco");
        machine.log_in(ANA);
        module(&store, &machine).settle();

        // Bea has never been here: she carries on with what is in use,
        // and from then on it is hers to change.
        machine.log_in(BEA);
        let users = module(&store, &machine);
        assert!(!users.settle());
        assert_eq!(power(&store), "eco");
        set_power(&store, "performance");

        // Ana's are as she left them, and Bea's are put aside for her.
        machine.log_in(ANA);
        let users = module(&store, &machine);
        assert!(users.settle(), "putting a profile back is a hand-over");
        assert_eq!(power(&store), "eco");

        machine.log_in(BEA);
        assert!(module(&store, &machine).settle());
        assert_eq!(power(&store), "performance");
        assert_eq!(users.status()["profiles"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn logging_out_leaves_the_last_users_settings_in_use() {
        let store = store("logout");
        let machine = Machine::with_members(&[ANA]);
        set_power(&store, "balanced");
        machine.log_in(ANA);
        module(&store, &machine).settle();

        machine.log_out();
        let users = module(&store, &machine);
        assert!(!users.settle());
        assert_eq!(users.status()["owner"]["uid"], ANA);
        assert_eq!(users.status()["activeUser"], Value::Null);
    }

    #[test]
    fn someone_who_is_not_pyrens_never_owns_the_settings() {
        let store = store("guest");
        let machine = Machine::with_members(&[ANA]);
        machine.log_in(ANA);
        module(&store, &machine).settle();

        machine.log_in(GUEST);
        let users = module(&store, &machine);
        assert!(!users.settle());
        let status = users.status();
        assert_eq!(status["owner"]["uid"], ANA);
        assert_eq!(status["activeUser"]["member"], false);
        assert!(status["profiles"].as_array().unwrap().is_empty());
    }

    #[test]
    fn standing_down_waits_for_one_of_pyrens_users() {
        let store = store("stand-down");
        let machine = Machine::with_members(&[ANA]);
        store
            .save(
                "users",
                &UsersConfig {
                    stand_down_for_others: true,
                    owner: Some(ANA),
                    restoring: None,
                },
            )
            .unwrap();
        machine.log_in(GUEST);

        let users = module(&store, &machine);
        let (done_tx, done_rx) = mpsc::channel();
        let waiting = std::thread::spawn(move || done_tx.send(users.settle()).unwrap());

        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "the daemon came up for someone it was told to stand down for"
        );
        machine.log_in(ANA);
        let handed_over = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the daemon stayed down after its user came back");
        assert!(
            handed_over,
            "coming back from standing down puts the settings back on the hardware"
        );
        waiting.join().unwrap();
    }

    #[test]
    fn standing_down_ends_when_the_setting_is_turned_off_on_disk() {
        let store = store("stand-down-off");
        let machine = Machine::with_members(&[ANA]);
        let mut config = UsersConfig {
            stand_down_for_others: true,
            owner: Some(ANA),
            restoring: None,
        };
        store.save("users", &config).unwrap();
        machine.log_in(GUEST);

        let users = module(&store, &machine);
        let (done_tx, done_rx) = mpsc::channel();
        let waiting = std::thread::spawn(move || done_tx.send(users.settle()).unwrap());
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());

        // An editor halfway through saving. Not a corrupt config to be
        // moved aside - that would take the owner with it - and not a
        // reason to start.
        let path = store.path_for("users");
        fs::write(&path, "{ \"standDownForOthers\": fal").unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(path.exists(), "the half-written file was moved aside");
        assert!(!path.with_extension("json.bad").exists());

        config.stand_down_for_others = false;
        store.save("users", &config).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the daemon did not notice the setting change");
        waiting.join().unwrap();
    }

    #[test]
    fn an_interrupted_restore_is_finished_not_saved_over_someone() {
        let store = store("interrupted");
        let machine = Machine::with_members(&[ANA, BEA]);

        set_power(&store, "eco");
        machine.log_in(ANA);
        module(&store, &machine).settle();
        machine.log_in(BEA);
        module(&store, &machine).settle();
        set_power(&store, "performance");
        machine.log_in(ANA);
        module(&store, &machine).settle();
        assert_eq!(power(&store), "eco");

        // The switch back to Bea got as far as saying so, and no further:
        // the files in use are still Ana's, the owner still Ana.
        Profiles::new(store.root()).save(ANA).unwrap();
        store
            .save(
                "users",
                &UsersConfig {
                    stand_down_for_others: false,
                    owner: Some(ANA),
                    restoring: Some(BEA),
                },
            )
            .unwrap();

        machine.log_in(BEA);
        let users = module(&store, &machine);
        assert!(users.settle());
        assert_eq!(power(&store), "performance");
        let saved = store.load::<UsersConfig>("users").value;
        assert_eq!(saved.owner, Some(BEA));
        assert_eq!(saved.restoring, None);

        // And Ana's were not overwritten with the half-switched files.
        machine.log_in(ANA);
        module(&store, &machine).settle();
        assert_eq!(power(&store), "eco");
    }

    #[test]
    fn the_watcher_adopts_in_place_and_hands_over_for_a_restore() {
        let store = store("watch");
        let machine = Machine::with_members(&[ANA, BEA]);
        set_power(&store, "eco");
        machine.log_in(ANA);
        let users = module(&store, &machine);
        users.settle();
        let events = Arc::new(EventBus::new());
        users.publish_to(Arc::clone(&events));

        let (reason_tx, reason_rx) = mpsc::channel();
        users.watch(Box::new(move |reason| {
            let _ = reason_tx.send(reason.to_string());
        }));

        // Bea has nothing of her own: no restart, an announcement.
        machine.log_in(BEA);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while users.status()["owner"]["uid"] != BEA {
            assert!(
                std::time::Instant::now() < deadline,
                "Bea was never adopted"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reason_rx.try_recv().is_err(), "adopting must not restart");
        let batch = events.read_since(0, Duration::from_millis(200));
        assert!(batch
            .events
            .iter()
            .any(|event| event.topic == "users.changed" && event.payload["owner"]["uid"] == BEA));

        // Ana has: that is a restart, and the watcher asks for it once.
        machine.log_in(ANA);
        let reason = reason_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("no hand-over was asked for");
        assert!(reason.contains("user1000"), "{reason}");
        // Left for the fresh daemon to do: nothing has moved on disk.
        assert_eq!(store.load::<UsersConfig>("users").value.owner, Some(BEA));
    }

    #[test]
    fn the_setting_is_persisted_and_announced() {
        let store = store("setting");
        let machine = Machine::with_members(&[ANA]);
        let users = module(&store, &machine);
        let events = Arc::new(EventBus::new());
        users.publish_to(Arc::clone(&events));

        let status = users
            .call("setStandDownForOthers", json!({ "enabled": true }))
            .unwrap();
        assert_eq!(status["standDownForOthers"], true);
        assert!(
            store
                .load::<UsersConfig>("users")
                .value
                .stand_down_for_others
        );
        let batch = events.read_since(0, Duration::from_millis(0));
        assert_eq!(batch.events[0].topic, "users.changed");

        let err = users.call("setStandDownForOthers", json!({})).unwrap_err();
        assert_eq!(err.kind(), pyren_core::ErrorKind::InvalidParams);
        let err = users.call("nope", Value::Null).unwrap_err();
        assert_eq!(err.kind(), pyren_core::ErrorKind::UnknownMethod);
    }

    fn set(store: &ConfigStore, namespace: &str, value: Value) {
        store.save(namespace, &value).unwrap();
    }

    #[test]
    fn what_one_user_set_does_not_reach_a_user_who_never_set_it() {
        let store = store("no-leak");
        let machine = Machine::with_members(&[ANA, BEA]);

        // Ana only ever chose a power mode.
        set_power(&store, "eco");
        machine.log_in(ANA);
        module(&store, &machine).settle();

        // Bea remaps a key and overclocks.
        machine.log_in(BEA);
        module(&store, &machine).settle();
        set(&store, "keymap", json!({ "enabled": true }));
        set(&store, "overclock", json!({ "restoreOnStart": true }));

        machine.log_in(ANA);
        module(&store, &machine).settle();
        assert!(
            !store.path_for("keymap").exists(),
            "Ana is typing on Bea's key remaps"
        );
        assert!(!store.path_for("overclock").exists());

        // And they are still Bea's when she is back.
        machine.log_in(BEA);
        module(&store, &machine).settle();
        assert_eq!(store.load::<Value>("keymap").value["enabled"], true);
        assert_eq!(
            store.load::<Value>("overclock").value["restoreOnStart"],
            true
        );
    }

    #[test]
    fn a_look_that_gets_no_answer_does_not_end_a_stand_down() {
        let store = store("blind-stand-down");
        let machine = Machine::with_members(&[ANA]);
        store
            .save(
                "users",
                &UsersConfig {
                    stand_down_for_others: true,
                    owner: Some(ANA),
                    restoring: None,
                },
            )
            .unwrap();
        machine.log_in(GUEST);

        let users = module(&store, &machine);
        let (done_tx, done_rx) = mpsc::channel();
        let waiting = std::thread::spawn(move || done_tx.send(users.settle()).unwrap());
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());

        // logind stops answering. The guest has not gone anywhere.
        machine.seat_down.store(true, Ordering::SeqCst);
        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "a failed lookup started the daemon on the guest's session"
        );

        machine.seat_down.store(false, Ordering::SeqCst);
        machine.log_in(ANA);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the daemon stayed down after its user came back");
        waiting.join().unwrap();
    }

    #[test]
    fn a_membership_that_cannot_be_checked_does_not_stand_the_daemon_down() {
        let store = store("blind-membership");
        let machine = Machine::with_members(&[ANA]);
        store
            .save(
                "users",
                &UsersConfig {
                    stand_down_for_others: true,
                    owner: Some(ANA),
                    restoring: None,
                },
            )
            .unwrap();
        machine.log_in(ANA);
        let users = module(&store, &machine);
        assert_eq!(users.look().0, Decision::Keep);

        // The directory stops answering while Ana is still sitting there.
        machine.accounts_down.store(true, Ordering::SeqCst);
        assert_eq!(
            users.look(),
            (Decision::Keep, false),
            "the daemon stood down for its own user"
        );
        assert_eq!(users.status()["activeUser"]["member"], true);
    }

    #[test]
    fn nothing_changes_hands_when_the_owners_settings_cannot_be_put_aside() {
        let store = store("aside-fails");
        let machine = Machine::with_members(&[ANA, BEA]);
        set_power(&store, "eco");
        machine.log_in(ANA);
        module(&store, &machine).settle();

        // Nowhere to keep Ana's: `users` is a file, not a directory.
        fs::write(store.root().join("users"), "in the way").unwrap();

        machine.log_in(BEA);
        let users = module(&store, &machine);
        assert!(!users.settle());
        assert_eq!(
            store.load::<UsersConfig>("users").value.owner,
            Some(ANA),
            "the settings became Bea's with Ana's only copy about to be overwritten"
        );
        // Not asked for again and again while Bea stays.
        assert_eq!(users.look().0, Decision::Keep);
    }

    #[test]
    fn a_restore_that_fails_leaves_the_files_marked_as_nobodys() {
        let store = store("restore-fails");
        let machine = Machine::with_members(&[ANA, BEA]);
        set_power(&store, "eco");
        machine.log_in(ANA);
        module(&store, &machine).settle();
        machine.log_in(BEA);
        module(&store, &machine).settle();
        set_power(&store, "performance");

        // Ana's copy of the power settings cannot be read back.
        let anas = store.root().join("users").join(ANA.to_string());
        fs::remove_file(anas.join("power.json")).unwrap();
        fs::create_dir(anas.join("power.json")).unwrap();
        fs::write(anas.join("rgb.json"), "{}").unwrap();

        machine.log_in(ANA);
        let users = module(&store, &machine);
        users.settle();
        let saved = store.load::<UsersConfig>("users").value;
        assert_eq!(
            saved.restoring,
            Some(ANA),
            "the failed restore was forgotten"
        );
        assert_eq!(saved.owner, Some(BEA));
        // Nothing is saved or restored on top of them in the meantime.
        machine.log_in(BEA);
        assert_eq!(users.look().0, Decision::Keep);
    }

    #[test]
    fn the_modules_are_asked_to_write_down_what_is_running_before_a_copy() {
        let store = store("flush");
        let machine = Machine::with_members(&[ANA, BEA]);
        set_power(&store, "eco");
        machine.log_in(ANA);
        module(&store, &machine).settle();

        machine.log_in(BEA);
        let users = module(&store, &machine);
        let flushed = store.clone();
        // What a module does when asked: the mode it is really in.
        users.before_setting_aside(Box::new(move || set_power(&flushed, "performance")));
        users.settle();

        let anas = store.root().join("users").join(ANA.to_string());
        let kept: Value =
            serde_json::from_slice(&fs::read(anas.join("power.json")).unwrap()).unwrap();
        assert_eq!(kept["mode"], "performance");
    }
}
