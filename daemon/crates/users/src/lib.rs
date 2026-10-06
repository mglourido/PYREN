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
//!    default): it lets go of the fans, the lights and the power override
//!    and waits, so that a person who never asked for Pyren does not get
//!    another person's fan curve.
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

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use pyren_config::ConfigStore;
use pyren_core::{log_info, log_warn, EventBus, Module, ModuleError, ModuleResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod directory;
pub mod profiles;

pub use directory::{ActiveUser, Directory};
use profiles::Profiles;

/// How often the active user is asked after. One `loginctl` each time; a
/// switch noticed three seconds late is a switch nobody saw being late.
const POLL: Duration = Duration::from_secs(3);

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
    /// owner's settings and saved over theirs.
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
    active: Option<Person>,
}

/// Called when the daemon has to replace itself; given the reason for the
/// log. Does not return in the daemon.
pub type HandOver = Box<dyn Fn(&str) + Send>;

#[derive(Clone)]
pub struct UsersModule {
    store: ConfigStore,
    profiles: Profiles,
    directory: Arc<dyn Directory>,
    state: Arc<Mutex<State>>,
    events: Arc<OnceLock<Arc<EventBus>>>,
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
            })),
            events: Arc::new(OnceLock::new()),
            poll: POLL,
        }
    }

    /// Hands the module the bus to announce `users.changed` on.
    pub fn publish_to(&self, events: Arc<EventBus>) {
        let _ = self.events.set(events);
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
                    self.restore(uid);
                    handed_over = true;
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
            println!("pyren-daemon: {} is back; starting", self.active_name());
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
            .spawn(move || loop {
                std::thread::sleep(module.poll);
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
                            "{} is now the active user and is not one of Pyren's; standing down",
                            module.active_name()
                        ));
                        return;
                    }
                }
            });
        if let Err(e) = spawned {
            log_warn!("could not start the active-user watcher: {e}");
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

    /// Asks who is active and what that calls for. The second value is
    /// whether it is a different answer from last time.
    fn look(&self) -> (Decision, bool) {
        let active = self.directory.active().map(|user| Person {
            member: self.directory.is_member(user.uid),
            uid: user.uid,
            name: user.name,
        });
        let mut state = lock(&self.state);
        let changed = state.active != active;
        state.active = active;
        let decision = decide(&state.config, state.active.as_ref(), |uid| {
            self.profiles.exists(uid)
        });
        (decision, changed)
    }

    /// Puts the settings in use aside as their owner's, if they have one
    /// who is not `next`.
    fn set_aside(&self, config: &UsersConfig, next: u32) {
        let Some(owner) = config.owner.filter(|owner| *owner != next) else {
            return;
        };
        if let Err(e) = self.profiles.save(owner) {
            // Carried on from: the alternative is refusing the person in
            // front of the machine their settings over a copy of someone
            // else's.
            log_warn!(
                "could not keep {}'s settings aside: {e}",
                self.name_of(owner)
            );
        }
    }

    fn adopt(&self, uid: u32) {
        let mut state = lock(&self.state);
        self.set_aside(&state.config, uid);
        state.config.owner = Some(uid);
        self.persist(&state.config);
        log_info!(
            "{} has no settings of their own yet; the ones in use are now theirs",
            self.name_of(uid)
        );
    }

    fn restore(&self, uid: u32) {
        let mut state = lock(&self.state);
        self.set_aside(&state.config, uid);
        state.config.restoring = Some(uid);
        self.persist(&state.config);
        self.put_back(&mut state.config, uid);
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
            }
            Err(e) => log_warn!(
                "could not put {}'s settings back: {e}; carrying on with the ones in use",
                self.name_of(uid)
            ),
        }
        // Theirs either way: they are the one who will be changing them
        // from here, and what they change must not be saved as the
        // previous owner's.
        config.owner = Some(uid);
        config.restoring = None;
        self.persist(config);
    }

    fn persist(&self, config: &UsersConfig) {
        if let Err(e) = self.store.save("users", config) {
            log_warn!("could not save the users config: {e}");
        }
    }

    /// Picks up `users.json` again. Only what a person edits by hand is
    /// taken - the rest is this process's to know.
    fn reload(&self) {
        let loaded = self.store.load::<UsersConfig>("users");
        if loaded.is_from_disk() {
            lock(&self.state).config.stand_down_for_others = loaded.value.stand_down_for_others;
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
    use std::sync::mpsc;

    /// A machine whose accounts and active user the test decides. uids
    /// from 1000 are people; the names are made from the number.
    #[derive(Default)]
    struct Machine {
        active: Mutex<Option<u32>>,
        members: BTreeSet<u32>,
    }

    impl Machine {
        fn with_members(members: &[u32]) -> Arc<Self> {
            Arc::new(Self {
                active: Mutex::new(None),
                members: members.iter().copied().collect(),
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
        fn active(&self) -> Option<ActiveUser> {
            let uid = (*self.active.lock().unwrap())?;
            Some(ActiveUser {
                uid,
                name: format!("user{uid}"),
            })
        }

        fn is_member(&self, uid: u32) -> bool {
            self.members.contains(&uid)
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
}
