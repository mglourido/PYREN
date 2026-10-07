//! Network booster - system-wide smart queuing, and per-process rules.
//!
//! | method | params | result |
//! |---|---|---|
//! | `network.getStatus` | none | `{ "supported": bool, "interface": string \| null, "mode": "off" \| "auto", "activeQdisc": string \| null, "perProcess": { "available": bool, "reason": Msg \| null } }` |
//! | `network.setMode` | `{ "mode": "off" \| "auto" }` | as `getStatus` |
//! | `network.getProcesses` | none | `{ "available": bool, "reason": Msg \| null, "priorityActive": bool, "processes": [{ "name", "pids", "downBps", "upBps", "downBytes", "upBytes", "action" }] }` |
//! | `network.setRule` | `{ "name": string, "action": "normal" \| "high" \| "low" \| "block" }` | as `getProcesses` |
//!
//! ## The machine-wide knob
//!
//! `setMode` hands the default-route interface a queuing discipline that
//! keeps latency down when something else is saturating the link (a game
//! or a call staying responsive while a big download runs), via `cake` -
//! or `fq_codel` on a kernel without `sch_cake` - instead of the plain FIFO
//! most interfaces default to. This is the same idea `cake`'s own name
//! suggests (Common Applications Kept Enhanced) and does not need to know
//! which process owns which packet: both qdiscs fair-queue by flow, so a
//! handful of small interactive flows naturally get more of the link than
//! one greedy bulk transfer sharing it. `off` deletes the root qdisc,
//! handing the interface back to the kernel's own default.
//!
//! ## The per-process half
//!
//! Linux does not say which process a packet belongs to - not in `/proc`,
//! and not to `nftables`, which can match a socket's cgroup but not its
//! owner. Matching by cgroup would mean moving every process of interest
//! into a cgroup of this daemon's making, out from under whatever systemd
//! scope it was started in. So the accounting is done where the answer is
//! known: a few eBPF programs on the root cgroup note which process opens
//! each socket, then count, drop or tag that socket's packets. `ebpf/` is
//! their source, `bpf.rs` loads them, and `apps.rs` turns thread-group ids
//! and byte counters into named processes, rates and rules.
//!
//! What a rule can do is uneven, and the page says so rather than hiding
//! it. `block` is absolute and works both ways. `high` and `low` only
//! reorder what the process *sends*, and only while `auto` has `cake` in
//! place to act on the class the packet was stamped with: there is no
//! queue of ours in front of a packet that has already arrived, so
//! downloads cannot be reordered at all - `block` is the only thing that
//! slows one.
//!
//! On a machine where the programs cannot be loaded - the daemon is not
//! root, the kernel has no cgroup-BPF, the cgroup tree is v1 - everything
//! above the per-process half works as before and `perProcess.reason`
//! carries the reason.
//!
//! ## What "mode" means here
//!
//! There is no way to ask the kernel "did *pyren* set this qdisc, or was it
//! already there" - `fq_codel` is the default `net.core.default_qdisc` on
//! several distributions, so seeing it active proves nothing about who put
//! it there. `mode` is therefore this daemon's own record of what it last
//! verifiably put on the interface, not a read of it. `activeQdisc` is the
//! separate, honest ground truth: whatever `tc qdisc show` actually
//! reports right now, ours or not.
//!
//! ## What is remembered
//!
//! The mode the user chose and their rules, in `network.json` - one per
//! user, like the fan curve or the lighting. A daemon that has just started
//! reports `off`, because nothing is on the interface yet; the background
//! thread then puts a remembered `auto` in place as soon as there is a
//! default route to put it on, which at boot is usually some seconds after
//! the daemon is up, and puts it back if the route later moves to another
//! interface. On the way out the qdisc is removed again, so that whoever
//! the daemon starts for next gets their own choice and not the last
//! person's.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use pyren_config::{ConfigStore, LoadOutcome};
use pyren_core::{log_info, log_warn, msg, ErrorKind, Module, ModuleError, ModuleResult, Msg};
use serde_json::{json, Value};

mod apps;
mod bpf;

use apps::{Action, Engine, Kernel, NetworkConfig, ProcessList};

#[cfg(test)]
type BeforeModeCommit = Arc<dyn Fn(NetworkMode) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    #[default]
    Off,
    Auto,
}

impl NetworkMode {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
        }
    }
}

/// The two disciplines tried, in order, when switching to `auto`. `cake` is
/// the more capable of the two (per-host fairness as well as per-flow) and
/// is tried first; a kernel built without `sch_cake` falls back to
/// `fq_codel`, which every kernel iproute2 targets has shipped for years.
const QDISCS_TO_TRY: [&str; 2] = ["cake", "fq_codel"];

fn tc_bin() -> String {
    std::env::var("PYREN_TC_BIN").unwrap_or_else(|_| "tc".to_string())
}

fn route_path() -> String {
    std::env::var("PYREN_NET_ROUTE_PATH").unwrap_or_else(|_| "/proc/net/route".to_string())
}

/// The interface the default route goes out - the one link that matters
/// for "is my connection responsive right now". Reads `/proc/net/route`
/// directly rather than shelling to `ip route` so the parser can be tested
/// on a fixture string with no network stack involved.
fn default_route_interface(route_table: &str) -> Option<String> {
    const RTF_UP: u32 = 0x1;
    const RTF_GATEWAY: u32 = 0x2;

    let mut best: Option<(u32, String)> = None;
    for line in route_table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 7 {
            continue;
        }
        let (iface, destination, flags_hex, metric) = (fields[0], fields[1], fields[3], fields[6]);
        if destination != "00000000" {
            continue;
        }
        let flags = u32::from_str_radix(flags_hex, 16).unwrap_or(0);
        if flags & RTF_UP == 0 || flags & RTF_GATEWAY == 0 {
            continue;
        }
        let metric: u32 = metric.parse().unwrap_or(u32::MAX);
        if best
            .as_ref()
            .is_none_or(|(best_metric, _)| metric < *best_metric)
        {
            best = Some((metric, iface.to_string()));
        }
    }
    best.map(|(_, iface)| iface)
}

/// The qdisc kind off the first line of `tc qdisc show dev <iface>`, e.g.
/// `"qdisc cake 8003: root refcnt 2 ..."` -> `"cake"`.
fn qdisc_kind(show_output: &str) -> Option<String> {
    let mut words = show_output.lines().next()?.split_whitespace();
    (words.next()? == "qdisc")
        .then(|| words.next())
        .flatten()
        .map(str::to_string)
}

fn tc_present() -> bool {
    pyren_core::process::output(pyren_core::process::command(tc_bin()).arg("-Version"))
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn read_qdisc(interface: &str) -> Option<String> {
    let output = pyren_core::process::output(
        pyren_core::process::command(tc_bin()).args(["qdisc", "show", "dev", interface]),
    )
    .ok()?;
    output
        .status
        .success()
        .then(|| qdisc_kind(&String::from_utf8_lossy(&output.stdout)))
        .flatten()
}

/// A failed command may already have changed the kernel. This read is kept
/// separate from the best-effort status read so failure never means Off.
fn observe_mode(interface: &str) -> Option<NetworkMode> {
    let output = pyren_core::process::output(
        pyren_core::process::command(tc_bin()).args(["qdisc", "show", "dev", interface]),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let kind = qdisc_kind(&String::from_utf8_lossy(&output.stdout));
    match kind.as_deref() {
        Some("cake") => Some(NetworkMode::Auto),
        // fq_codel can also be the kernel default after deletion. Its kind
        // alone cannot establish which operation produced it.
        Some("fq_codel") => None,
        _ => Some(NetworkMode::Off),
    }
}

/// Why [`enable_smart_queuing`] could not set a qdisc - distinct from
/// [`ModuleError::NotCapable`] the way `gpu`'s own [`ModuleError`] mapping
/// is: `tc` unprivileged always answers `RTNETLINK answers: Operation not
/// permitted` (`EPERM`) for every kind tried, which is "run this as root",
/// not "this kernel cannot do it" - and those two need different UI copy.
#[derive(Debug, PartialEq, Eq)]
enum QdiscFailure {
    PermissionDenied,
    NotCapable(String),
}

/// Replaces the root qdisc, trying each of [`QDISCS_TO_TRY`] in turn.
/// Returns the one that took, or every attempt's stderr for a refusal a
/// person can act on (usually "Error: Specified qdisc kind is unknown" -
/// `sch_cake` not built into this kernel).
fn enable_smart_queuing(interface: &str) -> Result<&'static str, QdiscFailure> {
    let mut failures = Vec::new();
    for qdisc in QDISCS_TO_TRY {
        // The handle is fixed so a per-process priority can name the
        // qdisc it is meant for, and `diffserv4` is the tin layout those
        // priorities index - see `apps::CAKE_HANDLE`.
        let mut command = pyren_core::process::command(tc_bin());
        command.args([
            "qdisc",
            "replace",
            "dev",
            interface,
            "root",
            "handle",
            apps::CAKE_HANDLE,
            qdisc,
        ]);
        if qdisc == "cake" {
            command.arg("diffserv4");
        }
        let output = pyren_core::process::output(&mut command);
        match output {
            Ok(out) if out.status.success() => return Ok(qdisc),
            Ok(out) => failures.push(format!(
                "{qdisc}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => failures.push(format!("{qdisc}: {e}")),
        }
    }
    if !failures.is_empty()
        && failures
            .iter()
            .all(|f| f.contains("Operation not permitted"))
    {
        return Err(QdiscFailure::PermissionDenied);
    }
    Err(QdiscFailure::NotCapable(failures.join("; ")))
}

/// Best-effort: hands the interface back to whatever qdisc the kernel
/// would have chosen on its own. "No such file or directory" (nothing to
/// delete - already the default) is not a failure, it is the goal state.
fn disable_smart_queuing(interface: &str) -> Result<(), ModuleError> {
    let output = pyren_core::process::output(
        pyren_core::process::command(tc_bin()).args(["qdisc", "del", "dev", interface, "root"]),
    )
    .map_err(|e| ModuleError::Failed(format!("could not run tc to remove qdisc: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    if detail.contains("No such file or directory") {
        return Ok(());
    }
    if detail.contains("Operation not permitted") {
        return Err(ModuleError::localised(
            ErrorKind::PermissionDenied,
            msg!("network.err.needsRoot", { "interface" => interface.to_string() },
                "changing the qdisc on {interface} needs root"),
        ));
    }
    Err(ModuleError::Failed(format!(
        "tc could not remove the qdisc on {interface}: {}",
        detail.trim()
    )))
}

/// A handle: clones share one module, which is how the background thread
/// and the daemon's exit path reach the same state the registry serves.
#[derive(Clone)]
pub struct NetworkModule {
    inner: Arc<Inner>,
}

pub struct Inner {
    /// Serializes every physical qdisc operation through its observation
    /// and logical commit. An earlier tc process cannot write after a later
    /// setMode has returned.
    operation: Mutex<()>,
    mode: Mutex<ModeState>,
    /// The mode the user chose, as `network.json` holds it. `mode` above
    /// is what the interface is known to be in; this is what it should be
    /// in, and [`NetworkModule::keep_mode`] closes the gap.
    wanted: Mutex<NetworkMode>,
    /// Where `auto` was last put in place.
    applied_on: Mutex<Option<Link>>,
    /// Set after a failed attempt to restore `auto`, so a kernel that
    /// refuses is not asked again every few seconds for ever.
    retry_after: Mutex<Option<Instant>>,
    /// Per-process accounting and rules, or why this machine has none.
    apps: Result<Arc<Mutex<Engine>>, Msg>,
    /// The rules on disk, kept so that saving the mode on a machine with
    /// no per-process half does not throw them away.
    stored_rules: BTreeMap<String, Action>,
    store: ConfigStore,
    #[cfg(test)]
    before_mode_commit: Mutex<Option<BeforeModeCommit>>,
}

impl std::ops::Deref for NetworkModule {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.inner
    }
}

/// One network device, as far as "is this still the one `auto` was applied
/// to" goes. The index is what tells a device that went away and came back
/// under the same name - a USB tether, a reloaded driver - from one that
/// never left: the new one has no qdisc of ours.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    interface: String,
    ifindex: Option<u32>,
}

impl Link {
    fn of(interface: &str) -> Self {
        let ifindex = std::fs::read_to_string(format!("/sys/class/net/{interface}/ifindex"))
            .ok()
            .and_then(|text| text.trim().parse().ok());
        Self {
            interface: interface.to_string(),
            ifindex,
        }
    }
}

/// How often the background thread checks that the wanted mode is still
/// in place, in sampler ticks.
const KEEP_EVERY_TICKS: u32 = 5;

/// How long a refused restore waits before the next attempt.
const RESTORE_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy)]
struct ModeState {
    requested: NetworkMode,
    /// Last mode verified by a successful command or an authoritative read.
    /// None means the command was ambiguous and the read also failed.
    committed: Option<NetworkMode>,
    generation: u64,
}

impl NetworkModule {
    #[cfg(not(test))]
    pub fn new() -> Self {
        Self::with_parts(
            ConfigStore::system(),
            bpf::BpfKernel::load().map(|kernel| Box::new(kernel) as Box<dyn Kernel>),
            Box::new(apps::running_processes),
            true,
        )
    }

    /// Under test there is no kernel half and no `/etc/pyren`: the qdisc
    /// tests get a module whose per-process side reports itself
    /// unavailable, which is also what an unprivileged daemon looks like.
    #[cfg(test)]
    pub fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pyren-network-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        Self::with_parts(
            ConfigStore::at(dir),
            Err(msg!(
                "network.apps.unavailable.needsRoot",
                "per-process networking needs the daemon to run as root"
            )),
            Box::new(std::collections::HashMap::new),
            false,
        )
    }

    fn with_parts(
        store: ConfigStore,
        kernel: Result<Box<dyn Kernel>, Msg>,
        processes: ProcessList,
        background: bool,
    ) -> Self {
        let loaded = store.load::<NetworkConfig>("network");
        match &loaded.outcome {
            LoadOutcome::Loaded | LoadOutcome::Missing => {}
            LoadOutcome::Recovered { backup, reason } => {
                log_warn!(
                    "network config was unreadable ({reason}); using defaults{}",
                    backup
                        .as_ref()
                        .map(|b| format!(", previous file kept at {}", b.display()))
                        .unwrap_or_default()
                );
            }
            LoadOutcome::TooNew { found } => {
                log_warn!(
                    "network config is version {found}, newer than this build understands; \
                     using defaults and leaving the file alone"
                );
            }
        }

        let apps = match kernel {
            Ok(kernel) => {
                let mut engine = Engine::new(kernel, processes, loaded.value.rules.clone());
                // Rules kept from the last run apply from the first moment,
                // not from whenever the sampler first comes round.
                engine.tick(Instant::now());
                let engine = Arc::new(Mutex::new(engine));
                log_info!("per-process network accounting attached");
                Ok(engine)
            }
            Err(reason) => {
                log_info!("per-process networking unavailable: {}", reason.text);
                Err(reason)
            }
        };

        let module = Self {
            inner: Arc::new(Inner {
                operation: Mutex::new(()),
                mode: Mutex::new(ModeState {
                    requested: NetworkMode::Off,
                    committed: Some(NetworkMode::Off),
                    generation: 0,
                }),
                wanted: Mutex::new(loaded.value.mode),
                applied_on: Mutex::new(None),
                retry_after: Mutex::new(None),
                apps,
                stored_rules: loaded.value.rules,
                store,
                #[cfg(test)]
                before_mode_commit: Mutex::new(None),
            }),
        };
        if background {
            spawn_background(Arc::downgrade(&module.inner));
        }
        module
    }

    fn wanted(&self) -> NetworkMode {
        *self.wanted.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Writes `network.json`: the chosen mode and every rule.
    fn save(&self) {
        let rules = match &self.apps {
            Ok(engine) => lock_engine(engine).rules().clone(),
            Err(_) => self.stored_rules.clone(),
        };
        let config = NetworkConfig {
            mode: self.wanted(),
            rules,
        };
        // What was asked for is already in force; a config directory that
        // cannot be written costs it the next restart, not this session.
        if let Err(e) = self.store.save("network", &config) {
            log_warn!("could not save network config: {e}");
        }
    }

    /// Puts `mode` on the default-route interface and records what the
    /// interface is known to be in afterwards. Everything `setMode` does
    /// short of remembering the choice.
    fn switch_mode(&self, mode: NetworkMode) -> Result<(), ModuleError> {
        let operation = self.operation.lock().unwrap_or_else(|p| p.into_inner());
        let interface =
            default_route_interface(&read_to_string(&route_path())).ok_or_else(|| {
                ModuleError::localised(
                    ErrorKind::NotCapable,
                    msg!(
                        "network.err.noInterface",
                        "no default-route network interface found"
                    ),
                )
            })?;

        {
            let mut state = self.mode.lock().unwrap_or_else(|p| p.into_inner());
            state.generation = state.generation.wrapping_add(1);
            state.requested = mode;
        }

        let outcome = apply_mode(&interface, mode);

        #[cfg(test)]
        let before_mode_commit = {
            self.before_mode_commit
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        };
        #[cfg(test)]
        if let Some(hook) = before_mode_commit.filter(|_| outcome.is_ok()) {
            hook(mode);
        }

        let observation = outcome
            .as_ref()
            .err()
            .and_then(|_| observe_mode(&interface));
        let committed = {
            let mut state = self.mode.lock().unwrap_or_else(|p| p.into_inner());
            match &outcome {
                Ok(()) => state.committed = Some(mode),
                Err(_) => {
                    let previous = state.committed;
                    state.committed = observation;
                    state.requested = observation.or(previous).unwrap_or(state.requested);
                    state.generation = state.generation.wrapping_add(1);
                }
            }
            state.committed
        };
        *self.applied_on.lock().unwrap_or_else(|p| p.into_inner()) =
            (committed == Some(NetworkMode::Auto)).then(|| Link::of(&interface));
        self.sync_priority(&interface);
        drop(operation);
        outcome
    }

    /// Makes the interface match the mode the user chose, if it does not.
    ///
    /// This is what makes `auto` survive a restart, and it is a loop
    /// rather than one call at startup because of when a daemon starts: at
    /// boot there is usually no default route yet, and the interface that
    /// eventually carries it may not be the one that did yesterday. So the
    /// background thread asks again every few seconds - one read of
    /// `/proc/net/route`, and `tc` only when something has changed.
    pub fn keep_mode(&self) {
        if self.wanted() != NetworkMode::Auto {
            return;
        }
        let Some(interface) = default_route_interface(&read_to_string(&route_path())) else {
            return;
        };
        let link = Link::of(&interface);
        let previous = self
            .applied_on
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if previous.as_ref() == Some(&link) {
            return;
        }
        {
            let retry_after = self.retry_after.lock().unwrap_or_else(|p| p.into_inner());
            if retry_after.is_some_and(|at| Instant::now() < at) {
                return;
            }
        }

        // The default route moved to another device: the qdisc on the one
        // it left is ours and no longer wanted there. A device that merely
        // came back under its old name took the qdisc with it when it went.
        if let Some(old) = previous.filter(|old| old.interface != interface) {
            let _operation = self.operation.lock().unwrap_or_else(|p| p.into_inner());
            let _ = disable_smart_queuing(&old.interface);
        }

        let outcome = self.switch_mode(NetworkMode::Auto);
        let mut retry_after = self.retry_after.lock().unwrap_or_else(|p| p.into_inner());
        match outcome {
            Ok(()) => {
                *retry_after = None;
                log_info!("network: smart queuing in place on {interface}");
            }
            Err(e) => {
                *retry_after = Some(Instant::now() + RESTORE_BACKOFF);
                log_warn!(
                    "network: could not put smart queuing on {interface}: {}",
                    e.as_msg().text
                );
            }
        }
    }

    /// Hands the interface back on the way out.
    ///
    /// The qdisc would otherwise outlive the daemon, and the next start
    /// may be for somebody else - a user whose own choice is `off` would
    /// inherit the last person's `cake` with nothing to say it is there.
    /// The choice itself is not touched: this user's `auto` is back in
    /// place the next time the daemon starts for them. The eBPF programs
    /// need nothing here; the kernel detaches them when the process ends.
    pub fn on_exit(&self) {
        let applied = self
            .applied_on
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let Some(link) = applied else {
            return;
        };
        let _operation = self.operation.lock().unwrap_or_else(|p| p.into_inner());
        match disable_smart_queuing(&link.interface) {
            Ok(()) => log_info!("network: handed {} back its own qdisc", link.interface),
            Err(e) => log_warn!(
                "network: could not remove the qdisc on {}: {}",
                link.interface,
                e.as_msg().text
            ),
        }
    }

    fn status(&self) -> Value {
        let _operation = self.operation.lock().unwrap_or_else(|p| p.into_inner());
        let interface = default_route_interface(&read_to_string(&route_path()));
        let active_qdisc = interface.as_deref().and_then(read_qdisc);
        let mode = self
            .mode
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .committed;
        json!({
            "supported": tc_present() && interface.is_some(),
            "interface": interface,
            "mode": mode.map(NetworkMode::as_str),
            "activeQdisc": active_qdisc,
            "perProcess": {
                "available": self.apps.is_ok(),
                "reason": self.apps.as_ref().err(),
            },
        })
    }

    /// Tells the per-process side whether a priority would be acted on:
    /// only while `auto` is the committed mode *and* the qdisc that took
    /// was `cake`. Under the `fq_codel` fallback there are no tins, and
    /// the class would be read by nothing - or, worse, by a qdisc of the
    /// user's own that happens to use the same handle.
    fn sync_priority(&self, interface: &str) {
        let Ok(engine) = &self.apps else {
            return;
        };
        let auto = self
            .mode
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .committed
            == Some(NetworkMode::Auto);
        let active = auto && read_qdisc(interface).as_deref() == Some("cake");
        lock_engine(engine).set_priority_active(active);
    }

    /// `network.getProcesses`.
    fn processes(&self) -> Value {
        match &self.apps {
            Ok(engine) => {
                let mut engine = lock_engine(engine);
                engine.watch(Instant::now());
                json!({
                    "available": true,
                    "reason": null,
                    "priorityActive": engine.priority_active(),
                    "processes": engine.processes_json(),
                })
            }
            Err(reason) => json!({
                "available": false,
                "reason": reason,
                "priorityActive": false,
                "processes": [],
            }),
        }
    }

    /// `network.setRule`.
    fn set_rule(&self, params: &Value) -> ModuleResult {
        let name = params.get("name").and_then(Value::as_str).ok_or_else(|| {
            ModuleError::localised(
                ErrorKind::InvalidParams,
                msg!(
                    "network.err.ruleNameRequired",
                    "params.name is required: the process name the rule is for"
                ),
            )
        })?;
        if !apps::valid_name(name) {
            return Err(ModuleError::localised(
                ErrorKind::InvalidParams,
                msg!(
                    "network.err.ruleNameInvalid",
                    { "name" => name.to_string() },
                    "'{name}' cannot be a process name: 1 to 15 characters, no '/'"
                ),
            ));
        }
        let raw = params
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ModuleError::localised(
                    ErrorKind::InvalidParams,
                    msg!(
                        "network.err.actionRequired",
                        "params.action is required: 'normal', 'high', 'low' or 'block'"
                    ),
                )
            })?;
        let action = Action::parse(raw).ok_or_else(|| {
            ModuleError::localised(
                ErrorKind::InvalidParams,
                msg!("network.err.actionUnknown", { "action" => raw.to_string() }, "'{action}' is not a network rule"),
            )
        })?;
        let engine = self
            .apps
            .as_ref()
            .map_err(|reason| ModuleError::localised(ErrorKind::NotCapable, reason.clone()))?;

        lock_engine(engine).set_rule(name, action);
        self.save();
        Ok(self.processes())
    }
}

fn lock_engine(engine: &Mutex<Engine>) -> std::sync::MutexGuard<'_, Engine> {
    engine.lock().unwrap_or_else(|p| p.into_inner())
}

/// The module's one thread, alive as long as the module is: once a
/// [`apps::TICK`] it samples per-process traffic (only while there is a
/// rule to enforce or a client looking), and every few ticks it checks the
/// wanted mode is still in place - starting with the first, which is what
/// restores `auto` after a restart without holding up the daemon's start.
fn spawn_background(inner: Weak<Inner>) {
    let spawned = std::thread::Builder::new()
        .name("pyren-network".into())
        .spawn(move || {
            let mut tick: u32 = 0;
            loop {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let module = NetworkModule { inner };
                if tick.is_multiple_of(KEEP_EVERY_TICKS) {
                    module.keep_mode();
                }
                if let Ok(engine) = &module.apps {
                    let mut engine = lock_engine(engine);
                    let now = Instant::now();
                    if engine.wants_tick(now) {
                        engine.tick(now);
                    }
                }
                drop(module);
                tick = tick.wrapping_add(1);
                std::thread::sleep(apps::TICK);
            }
        });
    if let Err(e) = spawned {
        log_warn!("could not start the network thread: {e}");
    }
}

impl Default for NetworkModule {
    fn default() -> Self {
        Self::new()
    }
}

fn read_to_string(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

impl Module for NetworkModule {
    fn id(&self) -> &'static str {
        "network"
    }

    fn is_supported(&self) -> bool {
        tc_present() && default_route_interface(&read_to_string(&route_path())).is_some()
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getStatus" => Ok(self.status()),

            "setMode" => {
                let raw = params.get("mode").and_then(Value::as_str).ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!(
                            "network.err.modeRequired",
                            "params.mode is required: 'off' or 'auto'"
                        ),
                    )
                })?;
                let mode = NetworkMode::parse(raw).ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!("network.err.modeUnknown", { "mode" => raw.to_string() }, "'{mode}' is not a network mode"),
                    )
                })?;

                self.switch_mode(mode)?;
                // Read before the choice is written down: a request queued
                // behind this one takes the operation lock the moment it is
                // free, and the reply to this one should not wait on a
                // config write and then on somebody else's tc.
                let status = self.status();
                // Remembered only once it has taken: a mode that was
                // refused is not one to keep retrying after every restart.
                *self.wanted.lock().unwrap_or_else(|p| p.into_inner()) = mode;
                *self.retry_after.lock().unwrap_or_else(|p| p.into_inner()) = None;
                self.save();
                Ok(status)
            }

            "getProcesses" => Ok(self.processes()),

            "setRule" => self.set_rule(&params),

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

fn apply_mode(interface: &str, mode: NetworkMode) -> Result<(), ModuleError> {
    match mode {
        NetworkMode::Off => disable_smart_queuing(interface),
        NetworkMode::Auto => enable_smart_queuing(interface)
            .map(|_| ())
            .map_err(|failure| match failure {
                QdiscFailure::PermissionDenied => ModuleError::localised(
                    ErrorKind::PermissionDenied,
                    msg!(
                        "network.err.needsRoot",
                        { "interface" => interface.to_string() },
                        "changing the qdisc on {interface} needs root"
                    ),
                ),
                QdiscFailure::NotCapable(detail) => ModuleError::localised(
                    ErrorKind::NotCapable,
                    msg!(
                        "network.err.qdiscRefused",
                        { "detail" => detail },
                        "this kernel refused smart queuing: {detail}"
                    ),
                ),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;

    // `PYREN_TC_BIN` is process-global state; tests that set it must not
    // run concurrently with each other.
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    /// Points `PYREN_TC_BIN` at a throwaway shell script for the duration
    /// of the guard, so `enable_smart_queuing` can be exercised without a
    /// real `tc` or real privileges.
    struct FakeTc {
        _guard: std::sync::MutexGuard<'static, ()>,
        dir: PathBuf,
    }

    impl FakeTc {
        fn new(script: &str) -> Self {
            let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let dir = std::env::temp_dir().join(format!(
                "pyren-network-test-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("tc");
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::env::set_var("PYREN_TC_BIN", &path);
            Self { _guard: guard, dir }
        }
    }

    impl Drop for FakeTc {
        fn drop(&mut self) {
            std::env::remove_var("PYREN_TC_BIN");
            std::env::remove_var("PYREN_NET_ROUTE_PATH");
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn wait_for_path(path: &std::path::Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(path.exists(), "timed out waiting for {}", path.display());
    }

    fn a_blocked_tc_command_does_not_hold_status_forever(mode: &str) {
        let fx = FakeTc::new(
            r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace")
    if [ "$8" = cake ] && [ -f "$root/block-replace" ]; then
      : > "$root/started"
      while [ ! -f "$root/release" ]; do sleep 0.01; done
      exit 2
    fi
    echo fq_codel > "$root/qdisc"; exit 0 ;;
  "qdisc del")
    if [ -f "$root/block-delete" ]; then
      : > "$root/started"
      while [ ! -f "$root/release" ]; do sleep 0.01; done
      exit 2
    fi
    echo pfifo_fast > "$root/qdisc"; exit 0 ;;
esac
exit 2
"#,
        );
        std::fs::write(
            fx.dir.join("qdisc"),
            if mode == "off" {
                "cake\n"
            } else {
                "pfifo_fast\n"
            },
        )
        .unwrap();
        std::fs::write(
            fx.dir.join(if mode == "off" {
                "block-delete"
            } else {
                "block-replace"
            }),
            "",
        )
        .unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = Arc::new(NetworkModule::new());
        let caller = Arc::clone(&module);
        let requested = mode.to_string();
        let operation =
            std::thread::spawn(move || caller.call("setMode", json!({ "mode": requested })));
        wait_for_path(&fx.dir.join("started"));
        let reader = Arc::clone(&module);
        let (tx, rx) = std::sync::mpsc::channel();
        let status_worker =
            std::thread::spawn(move || tx.send(reader.call("getStatus", Value::Null)).unwrap());
        let status_before_release = rx.recv_timeout(std::time::Duration::from_secs(4)).ok();
        std::fs::write(fx.dir.join("release"), "go").unwrap();
        let _ = operation.join().unwrap();
        status_worker.join().unwrap();
        assert!(
            status_before_release.is_some(),
            "getStatus waited for a blocked tc command to be released"
        );
        std::fs::remove_file(fx.dir.join(if mode == "off" {
            "block-delete"
        } else {
            "block-replace"
        }))
        .unwrap();
        module.call("setMode", json!({ "mode": "off" })).unwrap();
        let recovered = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(recovered["mode"], "off");
        assert_eq!(recovered["activeQdisc"], "pfifo_fast");
    }

    #[test]
    fn blocked_replace_cannot_hold_network_status_forever() {
        a_blocked_tc_command_does_not_hold_status_forever("auto");
    }

    #[test]
    fn blocked_delete_cannot_hold_network_status_forever() {
        a_blocked_tc_command_does_not_hold_status_forever("off");
    }

    #[test]
    fn delete_timeout_after_physical_effect_reconciles_and_allows_next_request() {
        let fx = FakeTc::new(
            r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; sleep 5; exit 2 ;;
  "qdisc replace") echo cake > "$root/qdisc"; exit 0 ;;
esac
exit 2
"#,
        );
        std::fs::write(fx.dir.join("qdisc"), "cake\n").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = NetworkModule::new();
        assert!(module.call("setMode", json!({ "mode": "off" })).is_err());
        let observed = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(observed["mode"], "off");
        assert_eq!(observed["activeQdisc"], "pfifo_fast");
        module.call("setMode", json!({ "mode": "auto" })).unwrap();
        let recovered = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(recovered["mode"], "auto");
        assert_eq!(recovered["activeQdisc"], "cake");
    }

    #[test]
    fn every_attempt_refused_with_eperm_is_permission_denied() {
        let _fx = FakeTc::new("echo 'RTNETLINK answers: Operation not permitted' >&2; exit 2");
        assert_eq!(
            enable_smart_queuing("wlan0"),
            Err(QdiscFailure::PermissionDenied)
        );
    }

    #[test]
    fn an_unknown_qdisc_kind_is_not_capable_not_permission_denied() {
        let _fx = FakeTc::new("echo 'Error: Specified qdisc kind is unknown.' >&2; exit 2");
        match enable_smart_queuing("wlan0") {
            Err(QdiscFailure::NotCapable(detail)) => {
                assert!(
                    detail.contains("cake") && detail.contains("fq_codel"),
                    "got: {detail}"
                );
            }
            other => panic!("expected NotCapable, got {other:?}"),
        }
    }

    #[test]
    fn cake_succeeding_never_tries_fq_codel() {
        let _fx = FakeTc::new("exit 0");
        assert_eq!(enable_smart_queuing("wlan0"), Ok("cake"));
    }

    #[test]
    fn cake_unavailable_falls_back_to_fq_codel() {
        let _fx = FakeTc::new(
            "if [ \"$8\" = cake ]; then echo 'Error: Specified qdisc kind is unknown.' >&2; exit 2; fi\nexit 0",
        );
        assert_eq!(enable_smart_queuing("wlan0"), Ok("fq_codel"));
    }

    #[test]
    fn failed_qdisc_delete_cannot_commit_off() {
        let fx = FakeTc::new(
            "case \"$1 $2\" in\n  '-Version ') exit 0 ;;\n  'qdisc show') echo 'qdisc cake 0: root'; exit 0 ;;\n  'qdisc del') echo 'RTNETLINK answers: Operation not permitted' >&2; exit 2 ;;\nesac\nexit 2",
        );
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = NetworkModule::new();
        let result = module.call("setMode", json!({ "mode": "off" }));
        assert!(
            result.is_err(),
            "tc refused deletion but setMode reported success"
        );
        let status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(status["activeQdisc"], "cake");
        // The fake reports cake on the interface before and after the
        // refusal; Off cannot be claimed despite the failed request.
        assert_eq!(status["mode"], "auto");
        let state = module.mode.lock().unwrap();
        assert_eq!(Some(state.requested), state.committed);
    }

    #[test]
    fn delete_effect_followed_by_error_records_the_observed_off_mode() {
        let fx = FakeTc::new(
            r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace") echo cake > "$root/qdisc"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; echo 'late failure' >&2; exit 2 ;;
esac
exit 2
"#,
        );
        std::fs::write(fx.dir.join("qdisc"), "pfifo_fast\n").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = NetworkModule::new();
        module.call("setMode", json!({"mode":"auto"})).unwrap();
        assert!(module.call("setMode", json!({"mode":"off"})).is_err());
        let status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(status["activeQdisc"], "pfifo_fast");
        assert_eq!(status["mode"], "off");
    }

    #[test]
    fn replace_effect_followed_by_timeout_records_the_observed_auto_mode() {
        let fx = FakeTc::new(
            r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace")
    if [ "$8" = cake ]; then echo cake > "$root/qdisc"; sleep 4; fi
    exit 2 ;;
esac
exit 2
"#,
        );
        std::fs::write(fx.dir.join("qdisc"), "pfifo_fast\n").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = NetworkModule::new();
        assert!(module.call("setMode", json!({"mode":"auto"})).is_err());
        let status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(status["activeQdisc"], "cake");
        assert_eq!(status["mode"], "auto");
    }

    const ROUTE_TABLE: &str = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
wlan0\t00000000\t0102A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
enp3s0\t00000000\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
";

    #[test]
    fn default_route_picks_the_lowest_metric_gateway_route() {
        assert_eq!(
            default_route_interface(ROUTE_TABLE),
            Some("enp3s0".to_string())
        );
    }

    #[test]
    fn non_default_and_gatewayless_routes_are_ignored() {
        let table =
            "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
                     lo\t0000007F\t00000000\t0001\t0\t0\t0\t000000FF\t0\t0\t0\n";
        assert_eq!(default_route_interface(table), None);
    }

    #[test]
    fn empty_route_table_is_no_interface() {
        assert_eq!(default_route_interface(""), None);
    }

    #[test]
    fn qdisc_kind_reads_the_first_word_after_qdisc() {
        assert_eq!(
            qdisc_kind("qdisc cake 8003: root refcnt 2 bandwidth unlimited\n"),
            Some("cake".to_string())
        );
        assert_eq!(
            qdisc_kind("qdisc fq_codel 0: root refcnt 2 limit 10240p\n"),
            Some("fq_codel".to_string())
        );
    }

    #[test]
    fn qdisc_kind_of_empty_output_is_none() {
        assert_eq!(qdisc_kind(""), None);
    }

    #[test]
    fn mode_parses_both_names_and_rejects_the_rest() {
        assert_eq!(NetworkMode::parse("off"), Some(NetworkMode::Off));
        assert_eq!(NetworkMode::parse("AUTO"), Some(NetworkMode::Auto));
        assert_eq!(NetworkMode::parse("custom"), None);
    }

    #[test]
    fn the_last_concurrent_request_owns_both_mode_and_qdisc() {
        let fx = FakeTc::new(
            r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc replace") echo cake > "$root/qdisc"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
esac
exit 2
"#,
        );
        std::fs::write(fx.dir.join("qdisc"), "pfifo_fast\n").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);

        let module = Arc::new(NetworkModule::new());
        let (auto_hardware_done_tx, auto_hardware_done_rx) = std::sync::mpsc::channel();
        let (release_auto_tx, release_auto_rx) = std::sync::mpsc::channel();
        let release_auto_rx = Arc::new(Mutex::new(release_auto_rx));
        *module
            .before_mode_commit
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(move |mode| {
            if mode == NetworkMode::Auto {
                auto_hardware_done_tx.send(()).unwrap();
                release_auto_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("release the auto request");
            }
        }));

        let first = Arc::clone(&module);
        let (auto_done_tx, auto_done_rx) = std::sync::mpsc::channel();
        let auto = std::thread::spawn(move || {
            let _ = auto_done_tx.send(first.call("setMode", json!({ "mode": "auto" })));
        });
        auto_hardware_done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("auto request reaches the pre-commit boundary");

        // The later request waits for the first operation's physical write
        // and commit, then becomes the final owner of the qdisc.
        let second = Arc::clone(&module);
        let (off_done_tx, off_done_rx) = std::sync::mpsc::channel();
        let off_worker = std::thread::spawn(move || {
            off_done_tx
                .send(second.call("setMode", json!({ "mode": "off" })))
                .unwrap();
        });
        assert!(off_done_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
        release_auto_tx.send(()).unwrap();
        auto_done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        auto.join().unwrap();
        let off = off_done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .expect("later off request succeeds");
        off_worker.join().unwrap();
        assert_eq!(off["mode"], "off");
        assert_eq!(off["activeQdisc"], "pfifo_fast");
        let final_status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(final_status["activeQdisc"], "pfifo_fast");
        assert_eq!(
            final_status["mode"], "off",
            "the earlier request committed after the later one and made logical state disagree with qdisc"
        );
    }

    #[test]
    fn repeated_requests_keep_the_observed_qdisc_in_sync() {
        let fx = FakeTc::new(
            r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace") echo cake > "$root/qdisc"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; exit 0 ;;
esac
exit 2
"#,
        );
        std::fs::write(fx.dir.join("qdisc"), "pfifo_fast\n").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = NetworkModule::new();
        for mode in ["auto", "auto", "off", "off", "auto"] {
            let status = module.call("setMode", json!({"mode":mode})).unwrap();
            assert_eq!(status["mode"], mode);
            assert_eq!(
                status["activeQdisc"],
                if mode == "auto" { "cake" } else { "pfifo_fast" }
            );
        }
    }

    #[test]
    fn later_auto_request_wins_over_paused_off_commit() {
        let fx = FakeTc::new(
            r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace") echo cake > "$root/qdisc"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; exit 0 ;;
esac
exit 2
"#,
        );
        std::fs::write(fx.dir.join("qdisc"), "pfifo_fast\n").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = Arc::new(NetworkModule::new());
        module.call("setMode", json!({"mode":"auto"})).unwrap();
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let resume_rx = Arc::new(Mutex::new(resume_rx));
        *module.before_mode_commit.lock().unwrap() = Some(Arc::new(move |mode| {
            if mode == NetworkMode::Off {
                paused_tx.send(()).unwrap();
                resume_rx.lock().unwrap().recv().unwrap();
            }
        }));
        let first = Arc::clone(&module);
        let off = std::thread::spawn(move || first.call("setMode", json!({"mode":"off"})));
        paused_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let second = Arc::clone(&module);
        let (newer_tx, newer_rx) = std::sync::mpsc::channel();
        let auto = std::thread::spawn(move || {
            newer_tx
                .send(second.call("setMode", json!({"mode":"auto"})))
                .unwrap();
        });
        assert!(newer_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
        resume_tx.send(()).unwrap();
        off.join().unwrap().unwrap();
        let newer = newer_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        auto.join().unwrap();
        assert_eq!(newer["mode"], "auto");
        let final_status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(final_status["mode"], "auto");
        assert_eq!(final_status["activeQdisc"], "cake");
    }

    #[test]
    fn later_off_cannot_return_while_older_auto_has_not_written_yet() {
        let fx = FakeTc::new(
            r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace")
    if mkdir "$root/first" 2>/dev/null; then
      : > "$root/auto-before-write"
      n=0
      while [ ! -f "$root/release-auto" ] && [ "$n" -lt 500 ]; do
        n=$((n + 1)); sleep 0.01
      done
    fi
    echo cake > "$root/qdisc"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; exit 0 ;;
esac
exit 2
"#,
        );
        std::fs::write(fx.dir.join("qdisc"), "pfifo_fast\n").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = Arc::new(NetworkModule::new());
        let first = Arc::clone(&module);
        let auto = std::thread::spawn(move || first.call("setMode", json!({"mode":"auto"})));
        wait_for_path(&fx.dir.join("auto-before-write"));
        let reader = Arc::clone(&module);
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let status_worker = std::thread::spawn(move || {
            status_tx
                .send(reader.call("getStatus", Value::Null))
                .unwrap();
        });
        assert!(status_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
        let second = Arc::clone(&module);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let off = std::thread::spawn(move || {
            done_tx
                .send(second.call("setMode", json!({"mode":"off"})))
                .unwrap()
        });
        let returned_while_auto_in_flight =
            done_rx.recv_timeout(std::time::Duration::from_secs(1)).ok();
        let returned_early = returned_while_auto_in_flight.is_some();
        std::fs::write(fx.dir.join("release-auto"), "go").unwrap();
        auto.join().unwrap().unwrap();
        let off_result = match returned_while_auto_in_flight {
            Some(result) => result,
            None => done_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
        };
        off_result.unwrap();
        off.join().unwrap();
        let in_flight_status = status_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(
            in_flight_status["activeQdisc"],
            if in_flight_status["mode"] == "auto" {
                "cake"
            } else {
                "pfifo_fast"
            },
        );
        status_worker.join().unwrap();
        assert!(
            !returned_early,
            "Off returned while an older Auto could still write cake"
        );
        let status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(status["mode"], "off");
        assert_eq!(status["activeQdisc"], "pfifo_fast");
    }

    #[test]
    fn later_auto_cannot_return_while_older_off_has_not_written_yet() {
        let fx = FakeTc::new(
            r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace") echo cake > "$root/qdisc"; exit 0 ;;
  "qdisc del")
    : > "$root/off-before-write"
    n=0
    while [ ! -f "$root/release-off" ] && [ "$n" -lt 500 ]; do
      n=$((n + 1)); sleep 0.01
    done
    echo pfifo_fast > "$root/qdisc"; exit 0 ;;
esac
exit 2
"#,
        );
        std::fs::write(fx.dir.join("qdisc"), "cake\n").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = Arc::new(NetworkModule::new());
        // A successful initial Auto makes the committed state match cake.
        module.call("setMode", json!({"mode":"auto"})).unwrap();
        let first = Arc::clone(&module);
        let off = std::thread::spawn(move || first.call("setMode", json!({"mode":"off"})));
        wait_for_path(&fx.dir.join("off-before-write"));
        let second = Arc::clone(&module);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let auto = std::thread::spawn(move || {
            done_tx
                .send(second.call("setMode", json!({"mode":"auto"})))
                .unwrap()
        });
        let returned_early = done_rx.recv_timeout(std::time::Duration::from_secs(1)).ok();
        let did_return_early = returned_early.is_some();
        std::fs::write(fx.dir.join("release-off"), "go").unwrap();
        off.join().unwrap().unwrap();
        let auto_result = match returned_early {
            Some(result) => result,
            None => done_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
        };
        auto_result.unwrap();
        auto.join().unwrap();
        assert!(
            !did_return_early,
            "Auto returned while an older Off could still delete cake"
        );
        let status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(status["mode"], "auto");
        assert_eq!(status["activeQdisc"], "cake");
    }

    #[test]
    fn a_failed_request_observes_qdisc_after_prior_operation_commits() {
        let fx = FakeTc::new(
            r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; exit 0 ;;
  "qdisc replace")
    if [ "$8" = "fq_codel" ]; then exit 2; fi
    if mkdir "$root/first" 2>/dev/null; then
      echo cake > "$root/qdisc"
      exit 0
    fi
    if mkdir "$root/second" 2>/dev/null; then
      : > "$root/second-ready"
      n=0
      while [ ! -f "$root/release-second" ] && [ "$n" -lt 400 ]; do
        n=$((n + 1))
        sleep 0.01
      done
      exit 2
    fi
    if mkdir "$root/third" 2>/dev/null; then
      : > "$root/third-ready"
      n=0
      while [ ! -f "$root/release-third" ] && [ "$n" -lt 400 ]; do
        n=$((n + 1))
        sleep 0.01
      done
      echo cake > "$root/qdisc"
      exit 0
    fi
    echo cake > "$root/qdisc"
    exit 0 ;;
esac
exit 2
"#,
        );
        std::fs::write(fx.dir.join("qdisc"), "pfifo_fast\n").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);

        let module = Arc::new(NetworkModule::new());
        let (first_applied_tx, first_applied_rx) = std::sync::mpsc::channel();
        let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
        let release_first_rx = Arc::new(Mutex::new(release_first_rx));
        *module
            .before_mode_commit
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(move |_| {
            first_applied_tx.send(()).unwrap();
            release_first_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("release the first request");
        }));

        let first_module = Arc::clone(&module);
        let (first_done_tx, first_done_rx) = std::sync::mpsc::channel();
        let first = std::thread::spawn(move || {
            first_done_tx
                .send(first_module.call("setMode", json!({ "mode": "auto" })))
                .unwrap();
        });
        first_applied_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("the first request reaches its pre-commit boundary");

        let second_module = Arc::clone(&module);
        let (second_done_tx, second_done_rx) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            second_done_tx
                .send(second_module.call("setMode", json!({ "mode": "auto" })))
                .unwrap();
        });
        // The second request cannot reach tc until the first has committed.
        assert!(second_done_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
        release_first_tx.send(()).unwrap();
        first_done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        first.join().unwrap();
        wait_for_path(&fx.dir.join("second-ready"));
        std::fs::write(fx.dir.join("release-second"), "go").unwrap();
        assert!(second_done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("the second request finishes")
            .is_err());
        second.join().unwrap();
        {
            let state = module.mode.lock().unwrap_or_else(|p| p.into_inner());
            assert_eq!(state.requested, NetworkMode::Auto);
            assert_eq!(state.generation, 3);
        }

        let status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(status["mode"], "auto");
        assert_eq!(status["activeQdisc"], "cake");
    }

    #[test]
    fn set_mode_without_the_param_is_invalid_params() {
        let module = NetworkModule::new();
        let err = module.call("setMode", json!({})).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidParams);
    }

    #[test]
    fn set_mode_with_an_unknown_name_is_invalid_params() {
        let module = NetworkModule::new();
        let err = module
            .call("setMode", json!({ "mode": "custom" }))
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidParams);
    }

    #[test]
    fn unknown_method_is_reported_by_name() {
        let module = NetworkModule::new();
        let err = module.call("frobnicate", Value::Null).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownMethod);
    }
    fn apps_module(tag: &str, fake: &apps::tests::Fake) -> (NetworkModule, ConfigStore) {
        let dir =
            std::env::temp_dir().join(format!("pyren-network-apps-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ConfigStore::at(dir);
        let module = NetworkModule::with_parts(
            store.clone(),
            Ok(Box::new(fake.clone())),
            fake.process_list(),
            false,
        );
        (module, store)
    }

    #[test]
    fn set_rule_blocks_the_process_and_reports_it() {
        let fake = apps::tests::Fake::default();
        fake.run(10, "steam");
        let (module, _store) = apps_module("block", &fake);

        let reply = module
            .call("setRule", json!({ "name": "steam", "action": "block" }))
            .unwrap();

        assert_eq!(fake.state().policy.get(&10), Some(&apps::POLICY_BLOCK));
        assert_eq!(reply["available"], true);
        assert_eq!(reply["processes"][0]["name"], "steam");
        assert_eq!(reply["processes"][0]["action"], "block");
    }

    #[test]
    fn rules_survive_a_restart_and_apply_before_the_first_request() {
        let fake = apps::tests::Fake::default();
        fake.run(10, "steam");
        let (module, store) = apps_module("restart", &fake);
        module
            .call("setRule", json!({ "name": "steam", "action": "block" }))
            .unwrap();
        drop(module);
        fake.state().policy.clear();

        let _restarted = NetworkModule::with_parts(
            store,
            Ok(Box::new(fake.clone())),
            fake.process_list(),
            false,
        );

        assert_eq!(fake.state().policy.get(&10), Some(&apps::POLICY_BLOCK));
    }

    #[test]
    fn a_rule_set_back_to_normal_is_not_kept() {
        let fake = apps::tests::Fake::default();
        let (module, store) = apps_module("normal", &fake);
        module
            .call("setRule", json!({ "name": "steam", "action": "low" }))
            .unwrap();
        module
            .call("setRule", json!({ "name": "steam", "action": "normal" }))
            .unwrap();

        assert!(store
            .load::<NetworkConfig>("network")
            .value
            .rules
            .is_empty());
    }

    #[test]
    fn set_rule_refuses_what_it_cannot_mean() {
        let fake = apps::tests::Fake::default();
        let (module, _store) = apps_module("invalid", &fake);
        for params in [
            json!({ "action": "block" }),
            json!({ "name": "", "action": "block" }),
            json!({ "name": "far-too-long-a-name", "action": "block" }),
            json!({ "name": "steam" }),
            json!({ "name": "steam", "action": "throttle" }),
        ] {
            let error = module.call("setRule", params.clone()).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidParams, "{params}");
        }
        assert!(fake.state().policy.is_empty());
    }

    #[test]
    fn without_the_kernel_half_the_page_is_told_why() {
        let module = NetworkModule::new();

        let listing = module.call("getProcesses", Value::Null).unwrap();
        assert_eq!(listing["available"], false);
        assert_eq!(
            listing["reason"]["key"],
            "network.apps.unavailable.needsRoot"
        );
        assert_eq!(listing["processes"], json!([]));

        let error = module
            .call("setRule", json!({ "name": "steam", "action": "block" }))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotCapable);
    }

    #[test]
    fn priority_follows_cake_and_not_the_fq_codel_fallback() {
        for (refuse_cake, expect_active) in [(false, true), (true, false)] {
            let fx = FakeTc::new(
                r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 1: root"; exit 0 ;;
  "qdisc replace")
    if [ "$8" = cake ] && [ -f "$root/no-cake" ]; then exit 2; fi
    echo "$8" > "$root/qdisc"; echo "$*" >> "$root/replaced"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; exit 0 ;;
esac
exit 2
"#,
            );
            std::fs::write(fx.dir.join("qdisc"), "pfifo_fast\n").unwrap();
            if refuse_cake {
                std::fs::write(fx.dir.join("no-cake"), "").unwrap();
            }
            let route = fx.dir.join("route");
            std::fs::write(&route, ROUTE_TABLE).unwrap();
            std::env::set_var("PYREN_NET_ROUTE_PATH", &route);

            let fake = apps::tests::Fake::default();
            fake.run(10, "game");
            let (module, _store) = apps_module(&format!("prio-{refuse_cake}"), &fake);
            module
                .call("setRule", json!({ "name": "game", "action": "high" }))
                .unwrap();
            assert!(fake.state().policy.is_empty());

            module.call("setMode", json!({ "mode": "auto" })).unwrap();
            assert_eq!(
                fake.state().policy.contains_key(&10),
                expect_active,
                "refuse_cake={refuse_cake}"
            );
            let listing = module.call("getProcesses", Value::Null).unwrap();
            assert_eq!(listing["priorityActive"], expect_active);

            if !refuse_cake {
                let replaced = std::fs::read_to_string(fx.dir.join("replaced")).unwrap();
                assert!(
                    replaced.contains("root handle 1: cake diffserv4"),
                    "{replaced}"
                );
            }

            module.call("setMode", json!({ "mode": "off" })).unwrap();
            assert!(fake.state().policy.is_empty());
        }
    }
    /// A `tc` that keeps one qdisc per interface in a file and logs every
    /// change it is asked to make.
    const STATEFUL_TC: &str = r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show")
    kind=$(cat "$root/qdisc-$4" 2>/dev/null || echo pfifo_fast)
    echo "qdisc $kind 1: root"; exit 0 ;;
  "qdisc replace")
    if [ -f "$root/refuse" ]; then echo "Error: refused." >&2; exit 2; fi
    echo "$8" > "$root/qdisc-$4"; echo "replace $4 $8" >> "$root/log"; exit 0 ;;
  "qdisc del")
    rm -f "$root/qdisc-$4"; echo "del $4" >> "$root/log"; exit 0 ;;
esac
exit 2
"#;

    fn route_via(fx: &FakeTc, interface: Option<&str>) {
        let route = fx.dir.join("route");
        let table = match interface {
            Some(interface) => format!(
                "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                 {interface}\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\n"
            ),
            None => "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n".to_string(),
        };
        std::fs::write(&route, table).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
    }

    fn tc_log(fx: &FakeTc) -> Vec<String> {
        std::fs::read_to_string(fx.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn module_at(store: &ConfigStore) -> NetworkModule {
        NetworkModule::with_parts(
            store.clone(),
            Err(msg!("network.apps.unavailable.needsRoot", "no kernel half")),
            Box::new(std::collections::HashMap::new),
            false,
        )
    }

    fn fresh_store(tag: &str) -> ConfigStore {
        let dir =
            std::env::temp_dir().join(format!("pyren-network-keep-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ConfigStore::at(dir)
    }

    #[test]
    fn auto_is_remembered_and_put_back_after_a_restart() {
        let fx = FakeTc::new(STATEFUL_TC);
        route_via(&fx, Some("wlan0"));
        let store = fresh_store("restart");

        let module = module_at(&store);
        module.call("setMode", json!({ "mode": "auto" })).unwrap();
        module.on_exit();
        drop(module);
        assert_eq!(tc_log(&fx), ["replace wlan0 cake", "del wlan0"]);

        let restarted = module_at(&store);
        assert_eq!(restarted.status()["mode"], "off", "nothing applied yet");
        restarted.keep_mode();

        assert_eq!(restarted.status()["mode"], "auto");
        assert_eq!(tc_log(&fx).last().unwrap(), "replace wlan0 cake");
    }

    #[test]
    fn a_mode_kept_in_place_is_not_reapplied() {
        let fx = FakeTc::new(STATEFUL_TC);
        route_via(&fx, Some("wlan0"));
        let module = module_at(&fresh_store("steady"));
        module.call("setMode", json!({ "mode": "auto" })).unwrap();

        module.keep_mode();
        module.keep_mode();

        assert_eq!(tc_log(&fx), ["replace wlan0 cake"]);
    }

    #[test]
    fn restoring_waits_for_a_default_route() {
        let fx = FakeTc::new(STATEFUL_TC);
        route_via(&fx, Some("wlan0"));
        let store = fresh_store("boot");
        module_at(&store)
            .call("setMode", json!({ "mode": "auto" }))
            .unwrap();

        // Boot: the daemon is up before the network is.
        route_via(&fx, None);
        let booted = module_at(&store);
        booted.keep_mode();
        assert_eq!(tc_log(&fx), ["replace wlan0 cake"], "no route, no tc");

        route_via(&fx, Some("wlan0"));
        booted.keep_mode();
        assert_eq!(tc_log(&fx).len(), 2);
        assert_eq!(booted.status()["mode"], "auto");
    }

    #[test]
    fn the_qdisc_follows_the_default_route_to_another_interface() {
        let fx = FakeTc::new(STATEFUL_TC);
        route_via(&fx, Some("wlan0"));
        let module = module_at(&fresh_store("roam"));
        module.call("setMode", json!({ "mode": "auto" })).unwrap();

        route_via(&fx, Some("eth0"));
        module.keep_mode();

        assert_eq!(
            tc_log(&fx),
            ["replace wlan0 cake", "del wlan0", "replace eth0 cake"]
        );
    }

    #[test]
    fn off_is_remembered_too_and_nothing_is_restored() {
        let fx = FakeTc::new(STATEFUL_TC);
        route_via(&fx, Some("wlan0"));
        let store = fresh_store("off");
        let module = module_at(&store);
        module.call("setMode", json!({ "mode": "auto" })).unwrap();
        module.call("setMode", json!({ "mode": "off" })).unwrap();
        drop(module);

        let restarted = module_at(&store);
        restarted.keep_mode();
        restarted.on_exit();

        assert_eq!(tc_log(&fx), ["replace wlan0 cake", "del wlan0"]);
    }

    #[test]
    fn a_refused_mode_is_not_remembered() {
        let fx = FakeTc::new(STATEFUL_TC);
        route_via(&fx, Some("wlan0"));
        std::fs::write(fx.dir.join("refuse"), "").unwrap();
        let store = fresh_store("refused");

        let module = module_at(&store);
        module
            .call("setMode", json!({ "mode": "auto" }))
            .unwrap_err();

        assert_eq!(
            store.load::<NetworkConfig>("network").value.mode,
            NetworkMode::Off
        );
    }

    #[test]
    fn a_refused_restore_backs_off_instead_of_retrying_every_tick() {
        let fx = FakeTc::new(STATEFUL_TC);
        route_via(&fx, Some("wlan0"));
        let store = fresh_store("backoff");
        module_at(&store)
            .call("setMode", json!({ "mode": "auto" }))
            .unwrap();

        std::fs::write(fx.dir.join("refuse"), "").unwrap();
        std::fs::write(fx.dir.join("attempts"), "").unwrap();
        let restarted = module_at(&store);
        restarted.keep_mode();
        std::fs::remove_file(fx.dir.join("refuse")).unwrap();
        restarted.keep_mode();

        assert_eq!(
            tc_log(&fx),
            ["replace wlan0 cake"],
            "the second attempt is not due for a minute"
        );
    }

    #[test]
    fn saving_the_mode_keeps_rules_the_kernel_half_could_not_load() {
        let fx = FakeTc::new(STATEFUL_TC);
        route_via(&fx, Some("wlan0"));
        let store = fresh_store("keep-rules");
        let mut config = NetworkConfig::default();
        config.rules.insert("steam".into(), Action::Block);
        store.save("network", &config).unwrap();

        module_at(&store)
            .call("setMode", json!({ "mode": "auto" }))
            .unwrap();

        let saved = store.load::<NetworkConfig>("network").value;
        assert_eq!(saved.mode, NetworkMode::Auto);
        assert_eq!(saved.rules.get("steam"), Some(&Action::Block));
    }

    #[test]
    fn a_restored_auto_brings_priority_rules_back_to_life() {
        let fx = FakeTc::new(STATEFUL_TC);
        route_via(&fx, Some("wlan0"));
        let fake = apps::tests::Fake::default();
        fake.run(10, "game");
        let (module, store) = apps_module("restore-prio", &fake);
        module
            .call("setRule", json!({ "name": "game", "action": "high" }))
            .unwrap();
        module.call("setMode", json!({ "mode": "auto" })).unwrap();
        module.on_exit();
        drop(module);
        fake.state().policy.clear();

        let restarted = NetworkModule::with_parts(
            store,
            Ok(Box::new(fake.clone())),
            fake.process_list(),
            false,
        );
        assert!(fake.state().policy.is_empty(), "no cake yet");
        restarted.keep_mode();

        assert!(fake.state().policy.contains_key(&10));
    }
}
