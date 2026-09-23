//! Network booster - system-wide smart queuing, not per-application control.
//!
//! | method | params | result |
//! |---|---|---|
//! | `network.getStatus` | none | `{ "supported": bool, "interface": string \| null, "mode": "off" \| "auto", "activeQdisc": string \| null }` |
//! | `network.setMode` | `{ "mode": "off" \| "auto" }` | as `getStatus` |
//!
//! ## Why there is no per-application priority or block list here
//!
//! The app's network page (`app/src/routes/system/network`) was built with a
//! per-process table - priority, block, a "double force" toggle - before any
//! backend existed behind it. Building that for real needs two things this
//! project has neither of: per-process traffic accounting (Linux gives you
//! that via `cgroup net_cls`/`nftables` socket matching or eBPF, not for
//! free from `/proc`) and a way to turn "high priority" into an actual
//! `tc`/`nftables` rule *per PID*, which means tracking PIDs as they start
//! and stop. `dev/TODO.md` §2 flagged this as the larger and less
//! valuable half of the page, and it stays undone - see the app's
//! `system/network` page for what it shows instead.
//!
//! ## What this module does instead
//!
//! One honest, machine-wide knob: hand the default-route interface a
//! queuing discipline that keeps latency down when something else is
//! saturating the link (a game or a call staying responsive while a big
//! download runs), via `cake` - or `fq_codel` on a kernel without
//! `sch_cake` - instead of the plain FIFO most interfaces default to. This
//! is the same idea `cake`'s own name suggests (Common Applications Kept
//! Enhanced) and does not need to know which process owns which packet:
//! both qdiscs fair-queue by flow, so a handful of small interactive flows
//! naturally get more of the link than one greedy bulk transfer sharing it.
//! `off` deletes the root qdisc, handing the interface back to the kernel's
//! own default.
//!
//! ## What "mode" means here
//!
//! There is no way to ask the kernel "did *pyren* set this qdisc, or was it
//! already there" - `fq_codel` is the default `net.core.default_qdisc` on
//! several distributions, so seeing it active proves nothing about who put
//! it there. `mode` is therefore this daemon's own memory of the last
//! `setMode` call, not a read of the interface - it resets to `off` on
//! restart rather than guessing. `activeQdisc` is the separate, honest
//! ground truth: whatever `tc qdisc show` actually reports right now, ours
//! or not.

#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;

use pyren_core::{msg, ErrorKind, Module, ModuleError, ModuleResult};
use serde_json::{json, Value};

#[cfg(test)]
type BeforeModeCommit = Arc<dyn Fn(NetworkMode) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMode {
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
        let output = pyren_core::process::output(
            pyren_core::process::command(tc_bin())
                .args(["qdisc", "replace", "dev", interface, "root", qdisc]),
        );
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
    Err(ModuleError::Failed(format!("tc could not remove the qdisc on {interface}: {}", detail.trim())))
}

pub struct NetworkModule {
    /// Serializes every physical qdisc operation through its observation
    /// and logical commit. An earlier tc process cannot write after a later
    /// setMode has returned.
    operation: Mutex<()>,
    mode: Mutex<ModeState>,
    #[cfg(test)]
    before_mode_commit: Mutex<Option<BeforeModeCommit>>,
}

#[derive(Debug, Clone, Copy)]
struct ModeState {
    requested: NetworkMode,
    /// Last mode verified by a successful command or an authoritative read.
    /// None means the command was ambiguous and the read also failed.
    committed: Option<NetworkMode>,
    generation: u64,
}

impl NetworkModule {
    pub fn new() -> Self {
        Self {
            operation: Mutex::new(()),
            mode: Mutex::new(ModeState {
                requested: NetworkMode::Off,
                committed: Some(NetworkMode::Off),
                generation: 0,
            }),
            #[cfg(test)]
            before_mode_commit: Mutex::new(None),
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
        })
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

                let operation = self.operation.lock().unwrap_or_else(|p| p.into_inner());
                let interface = default_route_interface(&read_to_string(&route_path()))
                    .ok_or_else(|| {
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

                let observation = outcome.as_ref().err().and_then(|_| observe_mode(&interface));
                {
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
                }
                drop(operation);
                outcome?;
                Ok(self.status())
            }

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
        let fx = FakeTc::new(r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace")
    if [ "$6" = cake ] && [ -f "$root/block-replace" ]; then
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
"#);
        std::fs::write(fx.dir.join("qdisc"), if mode == "off" { "cake\n" } else { "pfifo_fast\n" }).unwrap();
        std::fs::write(fx.dir.join(if mode == "off" { "block-delete" } else { "block-replace" }), "").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = Arc::new(NetworkModule::new());
        let caller = Arc::clone(&module);
        let requested = mode.to_string();
        let operation = std::thread::spawn(move || caller.call("setMode", json!({ "mode": requested })));
        wait_for_path(&fx.dir.join("started"));
        let reader = Arc::clone(&module);
        let (tx, rx) = std::sync::mpsc::channel();
        let status_worker = std::thread::spawn(move || tx.send(reader.call("getStatus", Value::Null)).unwrap());
        let status_before_release = rx.recv_timeout(std::time::Duration::from_secs(4)).ok();
        std::fs::write(fx.dir.join("release"), "go").unwrap();
        let _ = operation.join().unwrap();
        status_worker.join().unwrap();
        assert!(status_before_release.is_some(), "getStatus waited for a blocked tc command to be released");
        std::fs::remove_file(fx.dir.join(if mode == "off" { "block-delete" } else { "block-replace" })).unwrap();
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
        let fx = FakeTc::new(r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; sleep 5; exit 2 ;;
  "qdisc replace") echo cake > "$root/qdisc"; exit 0 ;;
esac
exit 2
"#);
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
            "if [ \"$6\" = cake ]; then echo 'Error: Specified qdisc kind is unknown.' >&2; exit 2; fi\nexit 0",
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
        assert!(result.is_err(), "tc refused deletion but setMode reported success");
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
        let fx = FakeTc::new(r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace") echo cake > "$root/qdisc"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; echo 'late failure' >&2; exit 2 ;;
esac
exit 2
"#);
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
        let fx = FakeTc::new(r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace")
    if [ "$6" = cake ]; then echo cake > "$root/qdisc"; sleep 4; fi
    exit 2 ;;
esac
exit 2
"#);
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
            off_done_tx.send(second.call("setMode", json!({ "mode": "off" }))).unwrap();
        });
        assert!(off_done_rx.recv_timeout(std::time::Duration::from_millis(100)).is_err());
        release_auto_tx.send(()).unwrap();
        auto_done_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap();
        auto.join().unwrap();
        let off = off_done_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap()
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
        let fx = FakeTc::new(r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace") echo cake > "$root/qdisc"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; exit 0 ;;
esac
exit 2
"#);
        std::fs::write(fx.dir.join("qdisc"), "pfifo_fast\n").unwrap();
        let route = fx.dir.join("route");
        std::fs::write(&route, ROUTE_TABLE).unwrap();
        std::env::set_var("PYREN_NET_ROUTE_PATH", &route);
        let module = NetworkModule::new();
        for mode in ["auto", "auto", "off", "off", "auto"] {
            let status = module.call("setMode", json!({"mode":mode})).unwrap();
            assert_eq!(status["mode"], mode);
            assert_eq!(status["activeQdisc"], if mode == "auto" { "cake" } else { "pfifo_fast" });
        }
    }

    #[test]
    fn later_auto_request_wins_over_paused_off_commit() {
        let fx = FakeTc::new(r#"
root=$(dirname "$0")
case "$1 $2" in
  "-Version ") exit 0 ;;
  "qdisc show") echo "qdisc $(cat "$root/qdisc") 0: root"; exit 0 ;;
  "qdisc replace") echo cake > "$root/qdisc"; exit 0 ;;
  "qdisc del") echo pfifo_fast > "$root/qdisc"; exit 0 ;;
esac
exit 2
"#);
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
        paused_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let second = Arc::clone(&module);
        let (newer_tx, newer_rx) = std::sync::mpsc::channel();
        let auto = std::thread::spawn(move || {
            newer_tx.send(second.call("setMode", json!({"mode":"auto"}))).unwrap();
        });
        assert!(newer_rx.recv_timeout(std::time::Duration::from_millis(100)).is_err());
        resume_tx.send(()).unwrap();
        off.join().unwrap().unwrap();
        let newer = newer_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap();
        auto.join().unwrap();
        assert_eq!(newer["mode"], "auto");
        let final_status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(final_status["mode"], "auto");
        assert_eq!(final_status["activeQdisc"], "cake");
    }

    #[test]
    fn later_off_cannot_return_while_older_auto_has_not_written_yet() {
        let fx = FakeTc::new(r#"
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
"#);
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
            status_tx.send(reader.call("getStatus", Value::Null)).unwrap();
        });
        assert!(status_rx.recv_timeout(std::time::Duration::from_millis(100)).is_err());
        let second = Arc::clone(&module);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let off = std::thread::spawn(move || done_tx.send(second.call("setMode", json!({"mode":"off"}))).unwrap());
        let returned_while_auto_in_flight = done_rx.recv_timeout(std::time::Duration::from_secs(1)).ok();
        let returned_early = returned_while_auto_in_flight.is_some();
        std::fs::write(fx.dir.join("release-auto"), "go").unwrap();
        auto.join().unwrap().unwrap();
        let off_result = match returned_while_auto_in_flight {
            Some(result) => result,
            None => done_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        };
        off_result.unwrap();
        off.join().unwrap();
        let in_flight_status = status_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(
            in_flight_status["activeQdisc"],
            if in_flight_status["mode"] == "auto" { "cake" } else { "pfifo_fast" },
        );
        status_worker.join().unwrap();
        assert!(!returned_early, "Off returned while an older Auto could still write cake");
        let status = module.call("getStatus", Value::Null).unwrap();
        assert_eq!(status["mode"], "off");
        assert_eq!(status["activeQdisc"], "pfifo_fast");
    }

    #[test]
    fn later_auto_cannot_return_while_older_off_has_not_written_yet() {
        let fx = FakeTc::new(r#"
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
"#);
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
        let auto = std::thread::spawn(move || done_tx.send(second.call("setMode", json!({"mode":"auto"}))).unwrap());
        let returned_early = done_rx.recv_timeout(std::time::Duration::from_secs(1)).ok();
        let did_return_early = returned_early.is_some();
        std::fs::write(fx.dir.join("release-off"), "go").unwrap();
        off.join().unwrap().unwrap();
        let auto_result = match returned_early {
            Some(result) => result,
            None => done_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        };
        auto_result.unwrap();
        auto.join().unwrap();
        assert!(!did_return_early, "Auto returned while an older Off could still delete cake");
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
    if [ "$6" = "fq_codel" ]; then exit 2; fi
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
        assert!(second_done_rx.recv_timeout(std::time::Duration::from_millis(100)).is_err());
        release_first_tx.send(()).unwrap();
        first_done_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap();
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
}
