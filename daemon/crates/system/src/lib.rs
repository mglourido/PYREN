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
//! | `system.getInfo` | none | machine identity + compatibility verdict (cached at startup) |
//! | `system.getMetrics` | none | live CPU/memory/temps/fans/disks/network/GPU/process readings |

mod boards;
mod identity;
mod metrics;

use std::sync::Mutex;

use omen_hub_core::{Module, ModuleError, ModuleResult};
use serde_json::Value;

pub use identity::{Compatibility, SystemIdentity};
pub use metrics::Metrics;

pub struct SystemModule {
    identity: SystemIdentity,
    /// Rate calculations need the previous sample, so this is stateful.
    /// One `Mutex` for all callers: sampling takes a few milliseconds and
    /// the daemon serves a handful of clients, so contention is a non-issue.
    sampler: Mutex<metrics::Sampler>,
}

impl SystemModule {
    pub fn new() -> Self {
        Self {
            identity: SystemIdentity::detect(),
            sampler: Mutex::new(metrics::Sampler::new()),
        }
    }

    pub fn identity(&self) -> &SystemIdentity {
        &self.identity
    }
}

impl Default for SystemModule {
    fn default() -> Self {
        Self::new()
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
            "getInfo" => serde_json::to_value(&self.identity)
                .map_err(|e| ModuleError::Other(e.to_string())),
            "getMetrics" => {
                // A panicking sampler would poison the lock; recover the
                // data rather than taking the whole daemon down with it.
                let mut sampler = self.sampler.lock().unwrap_or_else(|e| e.into_inner());
                serde_json::to_value(sampler.sample())
                    .map_err(|e| ModuleError::Other(e.to_string()))
            }
            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}
