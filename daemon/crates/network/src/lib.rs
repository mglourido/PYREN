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
//! and stop. `dev/TODO.md` §2.1 flagged this as the larger and less
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

use std::process::Command;
use std::sync::Mutex;

use pyren_core::{msg, ErrorKind, Module, ModuleError, ModuleResult};
use serde_json::{json, Value};

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
        if best.as_ref().is_none_or(|(best_metric, _)| metric < *best_metric) {
            best = Some((metric, iface.to_string()));
        }
    }
    best.map(|(_, iface)| iface)
}

/// The qdisc kind off the first line of `tc qdisc show dev <iface>`, e.g.
/// `"qdisc cake 8003: root refcnt 2 ..."` -> `"cake"`.
fn qdisc_kind(show_output: &str) -> Option<String> {
    let mut words = show_output.lines().next()?.split_whitespace();
    (words.next()? == "qdisc").then(|| words.next()).flatten().map(str::to_string)
}

fn tc_present() -> bool {
    Command::new(tc_bin()).arg("-Version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn read_qdisc(interface: &str) -> Option<String> {
    let output = Command::new(tc_bin()).args(["qdisc", "show", "dev", interface]).output().ok()?;
    output.status.success().then(|| qdisc_kind(&String::from_utf8_lossy(&output.stdout))).flatten()
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
        let output = Command::new(tc_bin())
            .args(["qdisc", "replace", "dev", interface, "root", qdisc])
            .output();
        match output {
            Ok(out) if out.status.success() => return Ok(qdisc),
            Ok(out) => failures.push(format!("{qdisc}: {}", String::from_utf8_lossy(&out.stderr).trim())),
            Err(e) => failures.push(format!("{qdisc}: {e}")),
        }
    }
    if !failures.is_empty() && failures.iter().all(|f| f.contains("Operation not permitted")) {
        return Err(QdiscFailure::PermissionDenied);
    }
    Err(QdiscFailure::NotCapable(failures.join("; ")))
}

/// Best-effort: hands the interface back to whatever qdisc the kernel
/// would have chosen on its own. "No such file or directory" (nothing to
/// delete - already the default) is not a failure, it is the goal state.
fn disable_smart_queuing(interface: &str) {
    let _ = Command::new(tc_bin()).args(["qdisc", "del", "dev", interface, "root"]).output();
}

pub struct NetworkModule {
    /// This daemon's own memory of the last `setMode`, not a read of the
    /// interface - see the module doc's "What `mode` means here".
    mode: Mutex<NetworkMode>,
}

impl NetworkModule {
    pub fn new() -> Self {
        Self { mode: Mutex::new(NetworkMode::Off) }
    }

    fn status(&self) -> Value {
        let interface = default_route_interface(&read_to_string(&route_path()));
        let active_qdisc = interface.as_deref().and_then(read_qdisc);
        let mode = *self.mode.lock().unwrap_or_else(|p| p.into_inner());
        json!({
            "supported": tc_present() && interface.is_some(),
            "interface": interface,
            "mode": mode.as_str(),
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
                        msg!("network.err.modeRequired", "params.mode is required: 'off' or 'auto'"),
                    )
                })?;
                let mode = NetworkMode::parse(raw).ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::InvalidParams,
                        msg!("network.err.modeUnknown", { "mode" => raw.to_string() }, "'{mode}' is not a network mode"),
                    )
                })?;

                let interface = default_route_interface(&read_to_string(&route_path())).ok_or_else(|| {
                    ModuleError::localised(
                        ErrorKind::NotCapable,
                        msg!("network.err.noInterface", "no default-route network interface found"),
                    )
                })?;

                match mode {
                    NetworkMode::Off => disable_smart_queuing(&interface),
                    NetworkMode::Auto => {
                        enable_smart_queuing(&interface).map_err(|failure| match failure {
                            QdiscFailure::PermissionDenied => ModuleError::localised(
                                ErrorKind::PermissionDenied,
                                msg!(
                                    "network.err.needsRoot",
                                    { "interface" => interface.clone() },
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
                        })?;
                    }
                }

                *self.mode.lock().unwrap_or_else(|p| p.into_inner()) = mode;
                Ok(self.status())
            }

            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
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
            let dir = std::env::temp_dir()
                .join(format!("pyren-network-test-{}-{:?}", std::process::id(), std::thread::current().id()));
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
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn every_attempt_refused_with_eperm_is_permission_denied() {
        let _fx = FakeTc::new("echo 'RTNETLINK answers: Operation not permitted' >&2; exit 2");
        assert_eq!(enable_smart_queuing("wlan0"), Err(QdiscFailure::PermissionDenied));
    }

    #[test]
    fn an_unknown_qdisc_kind_is_not_capable_not_permission_denied() {
        let _fx = FakeTc::new("echo 'Error: Specified qdisc kind is unknown.' >&2; exit 2");
        match enable_smart_queuing("wlan0") {
            Err(QdiscFailure::NotCapable(detail)) => {
                assert!(detail.contains("cake") && detail.contains("fq_codel"), "got: {detail}");
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

    const ROUTE_TABLE: &str = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
wlan0\t00000000\t0102A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
enp3s0\t00000000\t0102A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
";

    #[test]
    fn default_route_picks_the_lowest_metric_gateway_route() {
        assert_eq!(default_route_interface(ROUTE_TABLE), Some("enp3s0".to_string()));
    }

    #[test]
    fn non_default_and_gatewayless_routes_are_ignored() {
        let table = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n\
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
    fn set_mode_without_the_param_is_invalid_params() {
        let module = NetworkModule::new();
        let err = module.call("setMode", json!({})).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidParams);
    }

    #[test]
    fn set_mode_with_an_unknown_name_is_invalid_params() {
        let module = NetworkModule::new();
        let err = module.call("setMode", json!({ "mode": "custom" })).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidParams);
    }

    #[test]
    fn unknown_method_is_reported_by_name() {
        let module = NetworkModule::new();
        let err = module.call("frobnicate", Value::Null).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnknownMethod);
    }
}
