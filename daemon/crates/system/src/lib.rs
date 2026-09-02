//! System information module: what this machine is, and what it's doing.
//!
//! Unlike the `fan` module this one is not HP-specific - it reads generic
//! Linux interfaces (`/proc`, `/sys`, `statvfs`) and therefore reports
//! useful data on any machine. That is deliberate: it lets the vitals UI be
//! built and tested away from an OMEN laptop, and it is what answers the
//! "is this hardware compatible?" question the rest of the app depends on.
//!
//! | method | params | result |
//! |---|---|---|
//! | `system.getInfo` | none | machine identity + what the machine was found able to control (cached at startup) |
//! | `system.getMetrics` | none | live CPU/memory/temps/fans/disks/network/GPU/process readings |
//!
//! `getInfo` also carries a `privileges` block. Some readings are gated on
//! what the daemon was started with rather than on what the hardware can
//! do, and an app that cannot tell those two apart ends up telling the user
//! their GPU is broken when the real answer is "run me as root".

mod gpu;
mod identity;
mod metrics;

use std::sync::Mutex;

use pyren_core::{Module, ModuleError, ModuleResult};
use serde::Serialize;
use serde_json::Value;

pub use identity::{Compatibility, Controls, SystemIdentity};
pub use gpu::GpuMetrics;
pub use metrics::Metrics;

/// What the daemon was allowed to do, as opposed to what the machine can
/// do. Fixed at startup: privileges are not something a running process
/// picks up later.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Privileges {
    /// Running as uid 0, which is how the systemd unit runs it.
    pub root: bool,
    /// Intel engine utilisation is readable, i.e. the i915 perf PMU opened.
    /// Needs `CAP_PERFMON`; without it the iGPU reports no usage at all.
    pub perf_events: bool,
}

pub struct SystemModule {
    identity: SystemIdentity,
    privileges: Privileges,
    /// Rate calculations need the previous sample, so this is stateful.
    /// One `Mutex` for all callers: sampling takes a few milliseconds and
    /// the daemon serves a handful of clients, so contention is a non-issue.
    sampler: Mutex<metrics::Sampler>,
}

impl SystemModule {
    /// `controls` is what the hardware modules reported they could actually
    /// do; the daemon collects it from them (see `daemon/daemon/src/main.rs`)
    /// because no amount of reading DMI can answer it.
    pub fn new(controls: Controls) -> Self {
        let sampler = metrics::Sampler::new();
        let privileges = Privileges {
            // SAFETY: geteuid cannot fail and touches no memory we own.
            root: unsafe { libc::geteuid() } == 0,
            perf_events: sampler.engine_stats_available(),
        };
        Self {
            identity: SystemIdentity::detect(controls),
            privileges,
            sampler: Mutex::new(sampler),
        }
    }

    pub fn privileges(&self) -> Privileges {
        self.privileges
    }

    pub fn identity(&self) -> &SystemIdentity {
        &self.identity
    }
}

impl Module for SystemModule {
    fn id(&self) -> &'static str {
        "system"
    }

    /// Always true: every Linux machine can report its own vitals. The
    /// *hardware-control* modules are the ones that can be unsupported -
    /// use `system.getInfo`'s `compatibility` field for that question.
    fn is_supported(&self) -> bool {
        true
    }

    fn call(&self, method: &str, _params: Value) -> ModuleResult {
        match method {
            "getInfo" => {
                let mut info = serde_json::to_value(&self.identity)
                    .map_err(|e| ModuleError::Internal(e.to_string()))?;
                let privileges = serde_json::to_value(self.privileges)
                    .map_err(|e| ModuleError::Internal(e.to_string()))?;
                // Merged rather than nested inside the identity struct:
                // this says something about the daemon, not the machine.
                info["privileges"] = privileges;
                Ok(info)
            }
            "getMetrics" => {
                // A panicking sampler would poison the lock; recover the
                // data rather than taking the whole daemon down with it.
                let mut sampler = self.sampler.lock().unwrap_or_else(|e| e.into_inner());
                serde_json::to_value(sampler.sample())
                    .map_err(|e| ModuleError::Internal(e.to_string()))
            }
            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}
