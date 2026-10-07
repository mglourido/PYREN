//! The real [`Kernel`]: the embedded eBPF object, loaded and attached.
//!
//! Everything interesting about the programs is in `../ebpf/src/main.rs`.
//! This file only gets them into the kernel and hands their two maps to
//! [`crate::apps`].
//!
//! ## Where they attach, and how they leave
//!
//! To the root of the cgroup v2 tree, which is the one place that sees
//! every socket on the machine without moving any process anywhere - the
//! thing that ruled out matching on cgroups from `nftables`. They attach
//! with `BPF_F_ALLOW_MULTI`, so systemd's own per-unit programs further
//! down the tree keep running beside them.
//!
//! Each attachment is a `bpf_link` held by a file descriptor in this
//! process. When the daemon exits - cleanly or not - the kernel closes the
//! descriptors and detaches the programs itself: there is no state left
//! behind to clean up, and a crashed daemon cannot leave a process blocked.

use std::fs::File;

use aya::maps::{HashMap, MapData, PerCpuHashMap};
use aya::programs::{CgroupAttachMode, CgroupSkb, CgroupSkbAttachType, CgroupSock, CgroupSockAddr};
use aya::{Ebpf, Pod};
use pyren_core::{msg, Msg};

use crate::apps::{Counters, Kernel};

/// Built from `../ebpf` by `tools/build-bpf.sh` and checked in, so that
/// building the daemon needs neither a nightly toolchain nor `bpf-linker`.
static OBJECT: &[u8] = aya::include_bytes_aligned!("../bpf/pyren-net.bpf.o");

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Mirrors `Traffic` in the eBPF source, field for field.
#[repr(C)]
#[derive(Clone, Copy)]
struct Traffic {
    rx_bytes: u64,
    tx_bytes: u64,
}

// Plain integers, no padding: any bit pattern is a valid `Traffic`.
unsafe impl Pod for Traffic {}

pub struct BpfKernel {
    traffic: PerCpuHashMap<MapData, u32, Traffic>,
    policy: HashMap<MapData, u32, u32>,
    /// Owns the programs and their links; dropping it detaches them.
    _ebpf: Ebpf,
}

impl BpfKernel {
    /// Loads and attaches everything, or says why per-process networking
    /// is not available on this machine.
    pub fn load() -> Result<Self, Msg> {
        // Asked up front rather than read off the failure: an unprivileged
        // `bpf()` fails with the same EPERM at whichever step comes first,
        // wrapped differently by each, and "run it as root" is the only
        // thing worth saying about any of them.
        if unsafe { libc::geteuid() } != 0 {
            return Err(msg!(
                "network.apps.unavailable.needsRoot",
                "per-process networking needs the daemon to run as root"
            ));
        }
        // On a cgroup v1 or hybrid layout the programs would attach to a
        // hierarchy sockets are not accounted in, and count nothing.
        if !std::path::Path::new(CGROUP_ROOT)
            .join("cgroup.controllers")
            .exists()
        {
            return Err(msg!(
                "network.apps.unavailable.noCgroup2",
                "per-process networking needs the unified cgroup v2 hierarchy at /sys/fs/cgroup"
            ));
        }
        Self::attach().map_err(|detail| {
            msg!(
                "network.apps.unavailable.loadFailed",
                { "detail" => detail },
                "this kernel refused the per-process network programs: {detail}"
            )
        })
    }

    fn attach() -> Result<Self, String> {
        let mut ebpf = Ebpf::load(OBJECT).map_err(|e| describe(&e))?;
        let cgroup = File::open(CGROUP_ROOT).map_err(|e| format!("{CGROUP_ROOT}: {e}"))?;
        let mode = CgroupAttachMode::AllowMultiple;

        // Ownership first: a packet hook that runs before any socket has
        // an owner only lets traffic through uncounted, but there is no
        // reason to start with a window of it.
        let program: &mut CgroupSock = program_named(&mut ebpf, "pyren_sock_create")?;
        program.load().map_err(|e| describe(&e))?;
        program.attach(&cgroup, mode).map_err(|e| describe(&e))?;

        for name in [
            "pyren_connect4",
            "pyren_connect6",
            "pyren_sendmsg4",
            "pyren_sendmsg6",
        ] {
            let program: &mut CgroupSockAddr = program_named(&mut ebpf, name)?;
            program.load().map_err(|e| describe(&e))?;
            program.attach(&cgroup, mode).map_err(|e| describe(&e))?;
        }

        for (name, direction) in [
            ("pyren_egress", CgroupSkbAttachType::Egress),
            ("pyren_ingress", CgroupSkbAttachType::Ingress),
        ] {
            let program: &mut CgroupSkb = program_named(&mut ebpf, name)?;
            program.load().map_err(|e| describe(&e))?;
            program
                .attach(&cgroup, direction, mode)
                .map_err(|e| describe(&e))?;
        }

        let traffic = ebpf
            .take_map("TRAFFIC")
            .ok_or("the object has no TRAFFIC map")?;
        let policy = ebpf
            .take_map("POLICY")
            .ok_or("the object has no POLICY map")?;
        Ok(Self {
            traffic: PerCpuHashMap::try_from(traffic).map_err(|e| describe(&e))?,
            policy: HashMap::try_from(policy).map_err(|e| describe(&e))?,
            _ebpf: ebpf,
        })
    }
}

fn program_named<'a, P>(ebpf: &'a mut Ebpf, name: &str) -> Result<&'a mut P, String>
where
    &'a mut P: TryFrom<&'a mut aya::programs::Program, Error = aya::programs::ProgramError>,
{
    ebpf.program_mut(name)
        .ok_or_else(|| format!("the object has no program {name}"))?
        .try_into()
        .map_err(|e: aya::programs::ProgramError| describe(&e))
}

/// An error with its causes, because aya's own `Display` stops at "the
/// program could not be loaded" and the verifier's reason is one level down.
fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

impl Kernel for BpfKernel {
    fn traffic(&mut self) -> Vec<(u32, Counters)> {
        // An entry that vanishes mid-walk ends the iteration with an
        // error; what was read up to there is still true.
        self.traffic
            .iter()
            .map_while(Result::ok)
            .map(|(tgid, per_cpu)| {
                let total = per_cpu
                    .iter()
                    .fold(Counters::default(), |sum, cpu| Counters {
                        rx_bytes: sum.rx_bytes.wrapping_add(cpu.rx_bytes),
                        tx_bytes: sum.tx_bytes.wrapping_add(cpu.tx_bytes),
                    });
                (tgid, total)
            })
            .collect()
    }

    fn set_policy(&mut self, tgid: u32, policy: Option<u32>) -> Result<(), String> {
        match policy {
            Some(policy) => self
                .policy
                .insert(tgid, policy, 0)
                .map_err(|e| describe(&e)),
            // Already absent is the goal state, not a failure.
            None => match self.policy.remove(&tgid) {
                Ok(()) | Err(aya::maps::MapError::KeyNotFound) => Ok(()),
                Err(e) => Err(describe(&e)),
            },
        }
    }

    fn forget(&mut self, tgid: u32) {
        let _ = self.traffic.remove(&tgid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_object_defines_what_the_loader_asks_for() {
        // Parsing needs no privileges; creating the maps does, so this
        // reads the ELF directly rather than through `Ebpf::load`.
        let object = aya_obj::Object::parse(OBJECT).expect("embedded object should parse");
        for name in [
            "pyren_sock_create",
            "pyren_connect4",
            "pyren_connect6",
            "pyren_sendmsg4",
            "pyren_sendmsg6",
            "pyren_egress",
            "pyren_ingress",
        ] {
            assert!(object.programs.contains_key(name), "missing program {name}");
        }
        for name in ["TRAFFIC", "POLICY", "SOCK_OWNER"] {
            assert!(object.maps.contains_key(name), "missing map {name}");
        }
    }

    #[test]
    fn an_unprivileged_load_says_it_needs_root() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let reason = BpfKernel::load().err().expect("load should be refused");
        assert_eq!(reason.key, "network.apps.unavailable.needsRoot");
    }
}
