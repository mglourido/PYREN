//! Per-process traffic: who is using the link, and what to do about it.
//!
//! The kernel half (`../ebpf`) knows thread-group ids and nothing else: it
//! counts bytes per id and looks each id up in a policy map. Everything a
//! person would recognise lives here - process names, rules, rates - and
//! this module's job is to keep the two in step.
//!
//! ## Rules are by name, policy is by id
//!
//! A rule says `"steam": block`; the kernel can only be told `4127: block`.
//! So every [`Engine::tick`] walks the process table and gives each process
//! whose name has a rule the matching policy entry, and takes entries away
//! from ids that no longer deserve one. A process that starts between two
//! ticks runs unruled until the next one - up to [`TICK`] - which is the
//! price of not tracking `exec` in the kernel.
//!
//! The name is the main thread's `comm`, the same 15-byte name
//! `system.getMetrics` lists processes under, so a row here and a row on
//! the vitals page are the same process spelled the same way.
//!
//! ## Priority needs a queue that listens
//!
//! Blocking is the kernel half's own doing. Priority is not: the packet is
//! only stamped with a class, and it takes `cake` on the way out to act on
//! it. [`Engine::set_priority_active`] is how the module says whether that
//! qdisc is in place; while it is not, a `high`/`low` rule is remembered
//! and reported but written nowhere.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How often the background thread samples and re-applies rules.
pub const TICK: Duration = Duration::from_secs(1);

/// How long after the last `getProcesses` the sampler keeps running with no
/// rules to enforce. Nobody watching and nothing to enforce means there is
/// no reason to walk `/proc` once a second on a laptop's battery.
const WATCH_LINGER: Duration = Duration::from_secs(5);

/// A gap this long between two samples is not a rate, it is the sampler
/// having been idle; the first sample after it only sets the baseline.
const STALE_SAMPLE: Duration = Duration::from_secs(3);

/// The handle `auto` gives its `cake` qdisc. A packet's `skb->priority`
/// selects a tin only when its major number is the qdisc's own handle, so
/// the two halves have to agree on it.
pub const CAKE_HANDLE: &str = "1:";

/// `cake diffserv4` tins by `skb->priority`: `1:1` is Bulk, the tin that
/// yields to everything else, and `1:3` is Video, which is served ahead of
/// best effort as long as it stays under half the link. Voice (`1:4`) is
/// tighter still and loses its priority at a quarter, which a game's whole
/// traffic is likelier to cross.
const PRIORITY_LOW: u32 = 0x0001_0001;
const PRIORITY_HIGH: u32 = 0x0001_0003;

/// Policy value meaning "drop every packet". Mirrors `POLICY_BLOCK` in the
/// eBPF source.
pub const POLICY_BLOCK: u32 = u32::MAX;

/// Linux truncates `comm` to 15 bytes, so a longer rule could never match.
const MAX_NAME_LEN: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Normal,
    High,
    Low,
    Block,
}

impl Action {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "normal" => Some(Self::Normal),
            "high" => Some(Self::High),
            "low" => Some(Self::Low),
            "block" => Some(Self::Block),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::High => "high",
            Self::Low => "low",
            Self::Block => "block",
        }
    }

    /// What the kernel should hold for a process under this rule, if
    /// anything.
    fn policy(self, priority_active: bool) -> Option<u32> {
        match self {
            Self::Normal => None,
            Self::Block => Some(POLICY_BLOCK),
            Self::High => priority_active.then_some(PRIORITY_HIGH),
            Self::Low => priority_active.then_some(PRIORITY_LOW),
        }
    }
}

/// What `network.json` holds - one per user, like every other setting that
/// is somebody's (`pyren_users::profiles::NAMESPACES`). A name absent from `rules` is `normal`;
/// `normal` itself is never stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// The mode the user chose, put back in place after a restart.
    #[serde(default)]
    pub mode: crate::NetworkMode,
    #[serde(default)]
    pub rules: BTreeMap<String, Action>,
}

/// Whether `name` could be a process name at all.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.chars().any(|c| c.is_control() || c == '/')
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// The kernel half, as much of it as this module needs. A trait so the
/// bookkeeping here can be tested against a fake with no eBPF, no root and
/// no network.
pub trait Kernel: Send {
    /// Bytes counted so far, per thread-group id.
    fn traffic(&mut self) -> Vec<(u32, Counters)>;
    /// Sets, or with `None` clears, what happens to one id's packets.
    fn set_policy(&mut self, tgid: u32, policy: Option<u32>) -> Result<(), String>;
    /// Drops the counters of an id that no longer exists.
    fn forget(&mut self, tgid: u32);
}

/// Every running process as `tgid -> name`.
pub type ProcessList = Box<dyn FnMut() -> HashMap<u32, String> + Send>;

/// One walk of `/proc/*/comm`.
// The test build's module has no kernel half and never asks.
#[cfg_attr(test, allow(dead_code))]
pub fn running_processes() -> HashMap<u32, String> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let tgid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let comm = std::fs::read_to_string(entry.path().join("comm")).ok()?;
            Some((tgid, comm.trim_end_matches('\n').to_string()))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
struct Row {
    name: String,
    pids: Vec<u32>,
    down_bps: f64,
    up_bps: f64,
    down_bytes: u64,
    up_bytes: u64,
}

pub struct Engine {
    kernel: Box<dyn Kernel>,
    processes: ProcessList,
    rules: BTreeMap<String, Action>,
    priority_active: bool,
    /// What the kernel's policy map holds, as far as this side knows.
    applied: HashMap<u32, u32>,
    last: HashMap<u32, Counters>,
    last_at: Option<Instant>,
    /// Ids with counters but no process at the last sample.
    missing: HashSet<u32>,
    watched_until: Option<Instant>,
    rows: Vec<Row>,
}

impl Engine {
    pub fn new(
        kernel: Box<dyn Kernel>,
        processes: ProcessList,
        rules: BTreeMap<String, Action>,
    ) -> Self {
        Self {
            kernel,
            processes,
            rules,
            priority_active: false,
            applied: HashMap::new(),
            last: HashMap::new(),
            last_at: None,
            missing: HashSet::new(),
            watched_until: None,
            rows: Vec::new(),
        }
    }

    pub fn rules(&self) -> &BTreeMap<String, Action> {
        &self.rules
    }

    pub fn priority_active(&self) -> bool {
        self.priority_active
    }

    /// Whether the background thread has anything to do right now.
    pub fn wants_tick(&self, now: Instant) -> bool {
        !self.rules.is_empty() || self.watched_until.is_some_and(|until| now < until)
    }

    /// Notes that a client is looking, and makes sure what it is about to
    /// be shown is not a sample from before the sampler went idle.
    pub fn watch(&mut self, now: Instant) {
        let was_idle = !self.wants_tick(now);
        self.watched_until = Some(now + WATCH_LINGER);
        if was_idle {
            self.tick(now);
        }
    }

    /// Sets one rule and enforces it at once, rather than at the next tick:
    /// "block" that takes a second to mean it reads as a button that did
    /// not work.
    pub fn set_rule(&mut self, name: &str, action: Action) {
        if action == Action::Normal {
            self.rules.remove(name);
        } else {
            self.rules.insert(name.to_string(), action);
        }
        let processes = (self.processes)();
        self.enforce(&processes);
    }

    pub fn set_priority_active(&mut self, active: bool) {
        if self.priority_active == active {
            return;
        }
        self.priority_active = active;
        let processes = (self.processes)();
        self.enforce(&processes);
    }

    /// Brings the kernel's policy map in line with the rules and the
    /// processes that exist right now.
    fn enforce(&mut self, processes: &HashMap<u32, String>) {
        let wanted: HashMap<u32, u32> = processes
            .iter()
            .filter_map(|(tgid, name)| {
                let policy = self.rules.get(name)?.policy(self.priority_active)?;
                Some((*tgid, policy))
            })
            .collect();

        // Ids that lost their rule, or whose process is gone - and may
        // have had its id handed to something else entirely.
        let stale: Vec<u32> = self
            .applied
            .keys()
            .filter(|tgid| !wanted.contains_key(tgid))
            .copied()
            .collect();
        for tgid in stale {
            if self.kernel.set_policy(tgid, None).is_ok() {
                self.applied.remove(&tgid);
            }
        }
        for (tgid, policy) in wanted {
            if self.applied.get(&tgid) == Some(&policy) {
                continue;
            }
            if self.kernel.set_policy(tgid, Some(policy)).is_ok() {
                self.applied.insert(tgid, policy);
            }
        }
    }

    /// One sample: re-applies rules, reads the counters, and works out
    /// rates against the previous sample.
    pub fn tick(&mut self, now: Instant) {
        let processes = (self.processes)();
        self.enforce(&processes);

        let elapsed = self
            .last_at
            .map(|at| now.saturating_duration_since(at))
            .filter(|gap| !gap.is_zero() && *gap < STALE_SAMPLE);

        let mut by_name: BTreeMap<&str, Row> = BTreeMap::new();
        let mut seen = HashMap::new();
        let mut missing = HashSet::new();
        for (tgid, counters) in self.kernel.traffic() {
            let Some(name) = processes.get(&tgid) else {
                // Gone - or started after the process list was read, with
                // its first bytes already counted. Only a second miss in a
                // row says which, so only that drops the counters.
                if self.missing.contains(&tgid) {
                    self.kernel.forget(tgid);
                } else {
                    missing.insert(tgid);
                }
                continue;
            };
            seen.insert(tgid, counters);
            let row = by_name.entry(name).or_insert_with(|| Row {
                name: name.clone(),
                pids: Vec::new(),
                down_bps: 0.0,
                up_bps: 0.0,
                down_bytes: 0,
                up_bytes: 0,
            });
            row.pids.push(tgid);
            row.down_bytes += counters.rx_bytes;
            row.up_bytes += counters.tx_bytes;
            // No previous sample for this id means it has no rate yet, not
            // that everything it ever sent went out in the last second.
            if let (Some(gap), Some(before)) = (elapsed, self.last.get(&tgid)) {
                let secs = gap.as_secs_f64();
                // Saturating: the kernel map is an LRU, and an evicted id
                // starts again from zero.
                row.down_bps += counters.rx_bytes.saturating_sub(before.rx_bytes) as f64 / secs;
                row.up_bps += counters.tx_bytes.saturating_sub(before.tx_bytes) as f64 / secs;
            }
        }

        let mut rows: Vec<Row> = by_name.into_values().collect();
        for row in &mut rows {
            row.pids.sort_unstable();
        }
        rows.sort_by(|a, b| {
            (b.down_bps + b.up_bps)
                .total_cmp(&(a.down_bps + a.up_bps))
                .then_with(|| (b.down_bytes + b.up_bytes).cmp(&(a.down_bytes + a.up_bytes)))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.rows = rows;
        self.last = seen;
        self.missing = missing;
        self.last_at = Some(now);
    }

    /// The `processes` array of `network.getProcesses`: everything with
    /// traffic, then every rule whose process has none - not running, or
    /// blocked before it sent a byte - so a rule can always be seen and
    /// taken back.
    pub fn processes_json(&self) -> Value {
        let mut out: Vec<Value> = self
            .rows
            .iter()
            .map(|row| {
                json!({
                    "name": row.name,
                    "pids": row.pids,
                    "downBps": row.down_bps,
                    "upBps": row.up_bps,
                    "downBytes": row.down_bytes,
                    "upBytes": row.up_bytes,
                    "action": self.action_of(&row.name).as_str(),
                })
            })
            .collect();
        for (name, action) in &self.rules {
            if self.rows.iter().any(|row| &row.name == name) {
                continue;
            }
            out.push(json!({
                "name": name,
                "pids": [],
                "downBps": 0.0,
                "upBps": 0.0,
                "downBytes": 0,
                "upBytes": 0,
                "action": action.as_str(),
            }));
        }
        Value::Array(out)
    }

    fn action_of(&self, name: &str) -> Action {
        self.rules.get(name).copied().unwrap_or(Action::Normal)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub(crate) struct FakeState {
        pub traffic: Vec<(u32, Counters)>,
        pub policy: HashMap<u32, u32>,
        pub forgotten: Vec<u32>,
        pub processes: HashMap<u32, String>,
        pub refuse_policy: bool,
    }

    /// A kernel and a process table sharing one inspectable state.
    #[derive(Clone, Default)]
    pub(crate) struct Fake(pub Arc<Mutex<FakeState>>);

    impl Fake {
        pub fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.0.lock().unwrap()
        }

        pub fn run(&self, tgid: u32, name: &str) {
            self.state().processes.insert(tgid, name.to_string());
        }

        pub fn count(&self, tgid: u32, rx_bytes: u64, tx_bytes: u64) {
            let mut state = self.state();
            state.traffic.retain(|(id, _)| *id != tgid);
            state.traffic.push((tgid, Counters { rx_bytes, tx_bytes }));
        }

        pub fn process_list(&self) -> ProcessList {
            let fake = self.clone();
            Box::new(move || fake.state().processes.clone())
        }

        pub fn engine(&self) -> Engine {
            Engine::new(Box::new(self.clone()), self.process_list(), BTreeMap::new())
        }
    }

    impl Kernel for Fake {
        fn traffic(&mut self) -> Vec<(u32, Counters)> {
            self.state().traffic.clone()
        }

        fn set_policy(&mut self, tgid: u32, policy: Option<u32>) -> Result<(), String> {
            let mut state = self.state();
            if state.refuse_policy {
                return Err("refused".into());
            }
            match policy {
                Some(policy) => state.policy.insert(tgid, policy),
                None => state.policy.remove(&tgid),
            };
            Ok(())
        }

        fn forget(&mut self, tgid: u32) {
            let mut state = self.state();
            state.forgotten.push(tgid);
            state.traffic.retain(|(id, _)| *id != tgid);
        }
    }

    fn row<'a>(rows: &'a Value, name: &str) -> &'a Value {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("no row for {name} in {rows}"))
    }

    #[test]
    fn a_block_rule_reaches_every_process_of_that_name_at_once() {
        let fake = Fake::default();
        fake.run(10, "steam");
        fake.run(11, "steam");
        fake.run(12, "firefox");
        let mut engine = fake.engine();

        engine.set_rule("steam", Action::Block);

        let state = fake.state();
        assert_eq!(state.policy.get(&10), Some(&POLICY_BLOCK));
        assert_eq!(state.policy.get(&11), Some(&POLICY_BLOCK));
        assert_eq!(state.policy.get(&12), None);
    }

    #[test]
    fn a_process_started_after_the_rule_is_caught_by_the_next_tick() {
        let fake = Fake::default();
        let mut engine = fake.engine();
        engine.set_rule("steam", Action::Block);
        assert!(fake.state().policy.is_empty());

        fake.run(40, "steam");
        engine.tick(Instant::now());

        assert_eq!(fake.state().policy.get(&40), Some(&POLICY_BLOCK));
    }

    #[test]
    fn a_dead_process_loses_its_policy_so_a_reused_id_is_not_blocked() {
        let fake = Fake::default();
        fake.run(10, "steam");
        let mut engine = fake.engine();
        engine.set_rule("steam", Action::Block);

        fake.state().processes.clear();
        fake.run(10, "firefox");
        engine.tick(Instant::now());

        assert!(fake.state().policy.is_empty());
    }

    #[test]
    fn back_to_normal_removes_the_rule_and_the_policy() {
        let fake = Fake::default();
        fake.run(10, "steam");
        let mut engine = fake.engine();
        engine.set_rule("steam", Action::Block);

        engine.set_rule("steam", Action::Normal);

        assert!(fake.state().policy.is_empty());
        assert!(engine.rules().is_empty());
    }

    #[test]
    fn priority_is_written_only_while_a_qdisc_is_listening() {
        let fake = Fake::default();
        fake.run(10, "game");
        fake.run(11, "torrent");
        let mut engine = fake.engine();
        engine.set_rule("game", Action::High);
        engine.set_rule("torrent", Action::Low);
        assert!(
            fake.state().policy.is_empty(),
            "a priority class with no cake to read it must not be stamped"
        );

        engine.set_priority_active(true);
        assert_eq!(fake.state().policy.get(&10), Some(&PRIORITY_HIGH));
        assert_eq!(fake.state().policy.get(&11), Some(&PRIORITY_LOW));

        engine.set_priority_active(false);
        assert!(fake.state().policy.is_empty());
    }

    #[test]
    fn a_block_does_not_depend_on_the_qdisc() {
        let fake = Fake::default();
        fake.run(10, "steam");
        let mut engine = fake.engine();
        engine.set_rule("steam", Action::Block);
        engine.set_priority_active(true);
        engine.set_priority_active(false);
        assert_eq!(fake.state().policy.get(&10), Some(&POLICY_BLOCK));
    }

    #[test]
    fn a_refused_policy_write_is_retried_on_the_next_tick() {
        let fake = Fake::default();
        fake.run(10, "steam");
        let mut engine = fake.engine();
        fake.state().refuse_policy = true;
        engine.set_rule("steam", Action::Block);
        assert!(fake.state().policy.is_empty());

        fake.state().refuse_policy = false;
        engine.tick(Instant::now());

        assert_eq!(fake.state().policy.get(&10), Some(&POLICY_BLOCK));
    }

    #[test]
    fn rates_are_the_difference_between_two_samples_summed_per_name() {
        let fake = Fake::default();
        fake.run(10, "firefox");
        fake.run(11, "firefox");
        let mut engine = fake.engine();
        let start = Instant::now();

        fake.count(10, 1_000, 100);
        fake.count(11, 500, 50);
        engine.tick(start);
        fake.count(10, 3_000, 300);
        fake.count(11, 1_500, 150);
        engine.tick(start + Duration::from_secs(2));

        let rows = engine.processes_json();
        let firefox = row(&rows, "firefox");
        assert_eq!(firefox["pids"], json!([10, 11]));
        assert_eq!(firefox["downBps"], json!(1_500.0));
        assert_eq!(firefox["upBps"], json!(150.0));
        assert_eq!(firefox["downBytes"], json!(4_500));
        assert_eq!(firefox["upBytes"], json!(450));
        assert_eq!(firefox["action"], "normal");
    }

    #[test]
    fn the_first_sample_of_a_process_has_no_rate() {
        let fake = Fake::default();
        fake.run(10, "firefox");
        let mut engine = fake.engine();
        let start = Instant::now();
        engine.tick(start);

        fake.count(10, 9_000_000, 0);
        engine.tick(start + Duration::from_secs(1));

        assert_eq!(row(&engine.processes_json(), "firefox")["downBps"], 0.0);
    }

    #[test]
    fn a_sample_after_an_idle_gap_is_a_baseline_not_a_rate() {
        let fake = Fake::default();
        fake.run(10, "firefox");
        let mut engine = fake.engine();
        let start = Instant::now();
        fake.count(10, 1_000, 0);
        engine.tick(start);

        fake.count(10, 600_000, 0);
        engine.tick(start + Duration::from_secs(60));

        assert_eq!(row(&engine.processes_json(), "firefox")["downBps"], 0.0);
    }

    #[test]
    fn a_counter_that_went_backwards_is_not_a_negative_rate() {
        let fake = Fake::default();
        fake.run(10, "firefox");
        let mut engine = fake.engine();
        let start = Instant::now();
        fake.count(10, 5_000, 5_000);
        engine.tick(start);

        fake.count(10, 10, 10);
        engine.tick(start + Duration::from_secs(1));

        let rows = engine.processes_json();
        assert_eq!(row(&rows, "firefox")["downBps"], 0.0);
        assert_eq!(row(&rows, "firefox")["upBps"], 0.0);
    }

    #[test]
    fn counters_of_a_process_that_exited_are_dropped() {
        let fake = Fake::default();
        fake.count(99, 1_000, 1_000);
        let mut engine = fake.engine();

        let start = Instant::now();
        engine.tick(start);
        assert!(
            fake.state().forgotten.is_empty(),
            "one miss may be a process newer than the list it was checked against"
        );
        assert_eq!(engine.processes_json(), json!([]));

        engine.tick(start + Duration::from_secs(1));
        assert_eq!(fake.state().forgotten, vec![99]);
    }

    #[test]
    fn a_process_newer_than_the_process_list_keeps_its_first_bytes() {
        let fake = Fake::default();
        fake.count(50, 4_000, 0);
        let mut engine = fake.engine();
        let start = Instant::now();
        engine.tick(start);

        fake.run(50, "curl");
        engine.tick(start + Duration::from_secs(1));

        assert!(fake.state().forgotten.is_empty());
        assert_eq!(row(&engine.processes_json(), "curl")["downBytes"], 4_000);
    }

    #[test]
    fn the_busiest_process_comes_first() {
        let fake = Fake::default();
        fake.run(10, "idle");
        fake.run(11, "busy");
        let mut engine = fake.engine();
        let start = Instant::now();
        fake.count(10, 0, 0);
        fake.count(11, 0, 0);
        engine.tick(start);
        fake.count(10, 10, 0);
        fake.count(11, 10_000, 0);
        engine.tick(start + Duration::from_secs(1));

        let rows = engine.processes_json();
        assert_eq!(rows[0]["name"], "busy");
        assert_eq!(rows[1]["name"], "idle");
    }

    #[test]
    fn a_rule_with_no_traffic_is_still_listed() {
        let fake = Fake::default();
        let mut engine = fake.engine();
        engine.set_rule("steam", Action::Block);
        engine.tick(Instant::now());

        let rows = engine.processes_json();
        let steam = row(&rows, "steam");
        assert_eq!(steam["action"], "block");
        assert_eq!(steam["pids"], json!([]));
    }

    #[test]
    fn the_sampler_idles_with_no_rules_and_nobody_watching() {
        let fake = Fake::default();
        let mut engine = fake.engine();
        let now = Instant::now();
        assert!(!engine.wants_tick(now));

        engine.watch(now);
        assert!(engine.wants_tick(now + Duration::from_secs(1)));
        assert!(!engine.wants_tick(now + WATCH_LINGER + Duration::from_secs(1)));

        engine.set_rule("steam", Action::Low);
        assert!(engine.wants_tick(now + Duration::from_secs(3600)));
    }

    #[test]
    fn names_that_could_never_be_a_process_are_refused() {
        assert!(valid_name("steam"));
        assert!(valid_name("Web Content"));
        assert!(valid_name("exactly15chars."));
        assert!(!valid_name(""));
        assert!(!valid_name("sixteen-chars-xx"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("tab\there"));
    }

    #[test]
    fn rules_round_trip_through_the_config_file_shape() {
        let mut config = NetworkConfig::default();
        config.rules.insert("steam".into(), Action::Block);
        config.rules.insert("game".into(), Action::High);
        let text = serde_json::to_string(&config).unwrap();
        assert_eq!(
            text,
            r#"{"mode":"off","rules":{"game":"high","steam":"block"}}"#
        );
        assert_eq!(
            serde_json::from_str::<NetworkConfig>(&text).unwrap(),
            config
        );
    }
}
