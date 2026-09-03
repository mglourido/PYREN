//! What this machine will let anyone change about its GPUs, and - when the
//! answer is nothing - which of the reasons applies.
//!
//! The rule is the one the rest of the project follows: **nothing here is
//! looked up**. There is no table of cards that can be overclocked, because
//! whether this one can depends on the driver version, the session and the
//! X configuration far more than on the model. Every field below is the
//! answer to a question actually put to the machine, and every `detail` is
//! written to be the whole explanation someone needs before filing a bug
//! that says "the overclocking page is empty".

use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::nvidia::Nvidia;
use crate::plan::{Ceiling, Range};

const DRM: &str = "/sys/class/drm";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Unknown,
}

/// One GPU, and what of it can be moved.
///
/// A `Some(range)` is a knob the driver advertised, with the ends it
/// advertised. `None` is the absence of the knob - never "the knob is there
/// and its range is zero", which is a different thing and would let a UI
/// draw a slider nobody can move.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuProbe {
    /// Stable within a boot, and how every call names a card:
    /// `nvidia:0` for `nvidia-smi`'s index, `drm:card1` for a sysfs card.
    pub id: String,
    pub name: String,
    pub vendor: Vendor,
    pub driver: String,
    pub core_offset: Option<Range>,
    pub mem_offset: Option<Range>,
    /// The frequencies this card lists as supported. A lock may ask for a
    /// range inside this and nothing outside it.
    pub clock_lock: Option<Range>,
    /// Whether the offsets can be *written*, which reading them does not
    /// answer. `None` means the question was not put - it costs a write,
    /// so `overclock.probe` only asks when told to.
    pub offsets_writable: Option<bool>,
    /// Everything a person needs to know about this line, in a sentence.
    pub detail: String,
}

impl GpuProbe {
    /// What [`crate::plan::clamp`] needs: the knobs, without the prose.
    pub fn ceiling(&self) -> Ceiling {
        Ceiling {
            core_offset: self.core_offset,
            mem_offset: self.mem_offset,
            clock: self.clock_lock,
        }
    }

    /// Whether anything on this card can be driven at all.
    pub fn drivable(&self) -> bool {
        self.core_offset.is_some() || self.mem_offset.is_some() || self.clock_lock.is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub gpus: Vec<GpuProbe>,
    /// Whether any card has any knob. What `core.capabilities` reports for
    /// this module, and what a UI should hide the page on.
    pub supported: bool,
    pub detail: String,
}

impl Probe {
    pub fn gpu(&self, id: &str) -> Option<&GpuProbe> {
        self.gpus.iter().find(|gpu| gpu.id == id)
    }

    /// The card a UI should open on: the first one that can be driven.
    pub fn default_gpu(&self) -> Option<&GpuProbe> {
        self.gpus.iter().find(|gpu| gpu.drivable())
    }
}

/// Looks at every GPU on the machine. `allow_writes` opts into the one
/// question that cannot be answered by reading - whether the offsets can be
/// set - which is asked by writing an offset back at the value it already
/// has. Nothing else here touches the hardware.
pub fn probe(allow_writes: bool) -> Probe {
    let mut gpus = probe_nvidia(allow_writes);
    gpus.extend(probe_drm());

    let supported = gpus.iter().any(GpuProbe::drivable);
    let detail = if gpus.is_empty() {
        "no GPU was found to ask about".to_string()
    } else if supported {
        let names: Vec<&str> =
            gpus.iter().filter(|g| g.drivable()).map(|g| g.name.as_str()).collect();
        format!("tunable: {}", names.join(", "))
    } else {
        "a GPU was found, and nothing about it can be changed on this machine".to_string()
    };

    Probe { gpus, supported, detail }
}

fn probe_nvidia(allow_writes: bool) -> Vec<GpuProbe> {
    let nvidia = Nvidia::detect();
    if !nvidia.smi && !nvidia.settings {
        return Vec::new();
    }

    nvidia
        .gpus()
        .into_iter()
        .map(|gpu| {
            // Both ends have to be known before a lock is offered: a
            // range with a made-up floor is exactly the kind of number
            // this project refuses to invent.
            let clock_lock = nvidia.supported_clocks(gpu.index);

            let core = nvidia.core_offset(gpu.index);
            let mem = nvidia.mem_offset(gpu.index);
            let (core_offset, mem_offset, offset_detail) = match (&core, &mem) {
                (Ok((_, core_range)), Ok((_, mem_range))) => {
                    (*core_range, *mem_range, String::from("clock offsets are readable"))
                }
                (Err(e), _) | (_, Err(e)) => (None, None, format!("no clock offsets: {e}")),
            };

            // Only asked when the offsets exist at all: writing back an
            // attribute that is not there proves nothing.
            let offsets_writable = match (allow_writes, core_offset.is_some()) {
                (true, true) => nvidia.probe_writable(gpu.index).ok(),
                _ => None,
            };

            let detail =
                describe_nvidia(&offset_detail, offsets_writable, clock_lock, gpu.max_core_mhz);
            GpuProbe {
                id: format!("nvidia:{}", gpu.index),
                name: gpu.name,
                vendor: Vendor::Nvidia,
                driver: "nvidia".to_string(),
                // An offset that is known to be unwritable is not offered:
                // a slider that always fails is worse than no slider.
                core_offset: core_offset.filter(|_| offsets_writable != Some(false)),
                mem_offset: mem_offset.filter(|_| offsets_writable != Some(false)),
                clock_lock,
                offsets_writable,
                detail,
            }
        })
        .collect()
}

fn describe_nvidia(
    offset_detail: &str,
    writable: Option<bool>,
    clock_lock: Option<Range>,
    max_core_mhz: Option<i32>,
) -> String {
    let offsets = match writable {
        Some(true) => "clock offsets can be set".to_string(),
        Some(false) => "clock offsets are readable but not settable; the X screen needs \
                        Coolbits before the driver will take one"
            .to_string(),
        None => offset_detail.to_string(),
    };
    match clock_lock {
        Some(range) => format!(
            "{offsets}. Clocks can be pinned between {} and {} MHz, which needs root and \
             stays inside what the card already supports",
            range.min, range.max
        ),
        None => match max_core_mhz {
            Some(mhz) => format!(
                "{offsets}. The card reports a maximum of {mhz} MHz and will not enumerate \
                 the clocks it supports, so pinning them is not offered"
            ),
            None => format!("{offsets}. This card's clocks cannot be pinned"),
        },
    }
}

/// The cards sysfs knows about, minus the NVIDIA ones `nvidia-smi` has
/// already answered for far better.
fn probe_drm() -> Vec<GpuProbe> {
    let Ok(entries) = fs::read_dir(DRM) else {
        return Vec::new();
    };

    let mut cards: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|path| match path.file_name().map(|n| n.to_string_lossy()) {
            // Whole cards ("card0"), not their connectors ("card0-DP-1").
            Some(name) => name.starts_with("card") && !name.contains('-'),
            None => false,
        })
        .collect();
    cards.sort();

    cards
        .iter()
        .filter_map(|card| {
            let device = card.join("device");
            let uevent = fs::read_to_string(device.join("uevent")).ok()?;
            let driver = uevent
                .lines()
                .find_map(|l| l.strip_prefix("DRIVER="))
                .unwrap_or("unknown")
                .to_string();
            if driver == "nvidia" {
                return None;
            }
            let id = format!("drm:{}", card.file_name()?.to_string_lossy());
            Some(match driver.as_str() {
                "amdgpu" => amd(id, &driver, &device),
                "i915" | "xe" => intel(id, &driver, card),
                _ => GpuProbe {
                    id,
                    name: format!("{driver} graphics"),
                    vendor: Vendor::Unknown,
                    driver: driver.clone(),
                    core_offset: None,
                    mem_offset: None,
                    clock_lock: None,
                    offsets_writable: None,
                    detail: format!("no tuning interface is known for the {driver} driver"),
                },
            })
        })
        .collect()
}

/// AMD Overdrive: **detected, deliberately not driven.**
///
/// `pp_od_clk_voltage` is a real overclocking interface and writing it is a
/// two-line change. It is not made here for the same reason `pyren-rgb`
/// probes the per-key keyboard without driving it: there is no AMD machine
/// to test on, and the failure mode of a wrong write to this file is not an
/// error message - it is a card that comes back at the wrong voltage. A
/// knob that has never been moved once must not ship as a slider.
fn amd(id: String, driver: &str, device: &Path) -> GpuProbe {
    let od = device.join("pp_od_clk_voltage");
    let detail = match fs::read_to_string(&od) {
        Ok(contents) if contents.contains("OD_SCLK") => {
            "AMD Overdrive is enabled on this card and pyren does not drive it yet: \
             it has never been tested on AMD hardware, and an untested write here \
             is not the kind that fails with an error message"
                .to_string()
        }
        Ok(_) => "this card exposes pp_od_clk_voltage without an OD_SCLK table, \
                  so Overdrive is off (amdgpu.ppfeaturemask enables it)"
            .to_string(),
        Err(_) => "no pp_od_clk_voltage on this card, so the kernel is offering no \
                   Overdrive interface at all"
            .to_string(),
    };
    GpuProbe {
        id,
        name: "AMD graphics".to_string(),
        vendor: Vendor::Amd,
        driver: driver.to_string(),
        core_offset: None,
        mem_offset: None,
        clock_lock: None,
        offsets_writable: None,
        detail,
    }
}

/// Intel graphics have a frequency *ceiling*, not an offset.
///
/// `gt_max_freq_mhz` cannot be pushed above `gt_RP0_freq_mhz`, which is the
/// chip's own maximum - so there is nothing here to overclock, and lowering
/// the ceiling is a power decision that belongs to the power module rather
/// than a second owner on this page.
fn intel(id: String, driver: &str, card: &Path) -> GpuProbe {
    let rp0 = ["gt_RP0_freq_mhz", "gt/gt0/rps_rp0_freq_mhz"]
        .iter()
        .find_map(|name| fs::read_to_string(card.join(name)).ok())
        .and_then(|text| text.trim().parse::<i32>().ok());
    let detail = match rp0 {
        Some(mhz) => format!(
            "Intel graphics expose a frequency ceiling ({mhz} MHz here), not an offset: \
             it cannot be raised above what the chip already runs at, so there is \
             nothing on this card to overclock"
        ),
        None => "Intel graphics expose a frequency ceiling, not an offset, and this \
                 kernel publishes no maximum for it"
            .to_string(),
    };
    GpuProbe {
        id,
        name: "Intel graphics".to_string(),
        vendor: Vendor::Intel,
        driver: driver.to_string(),
        core_offset: None,
        mem_offset: None,
        clock_lock: None,
        offsets_writable: None,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe has to be safe to run anywhere, including CI with no GPU
    /// at all - it is called for every `core.capabilities`.
    #[test]
    fn probing_a_machine_with_no_gpu_is_not_an_error() {
        let probe = probe(false);
        if probe.gpus.is_empty() {
            assert!(!probe.supported);
            assert!(!probe.detail.is_empty(), "a machine with nothing must still say so");
        }
    }

    /// `supported` is what the daemon reports to a client that decides
    /// whether to show the page, so it must mean "something can be driven",
    /// not "a GPU exists".
    #[test]
    fn a_card_with_no_knobs_does_not_count_as_supported() {
        let gpu = GpuProbe {
            id: "drm:card0".into(),
            name: "Intel graphics".into(),
            vendor: Vendor::Intel,
            driver: "i915".into(),
            core_offset: None,
            mem_offset: None,
            clock_lock: None,
            offsets_writable: None,
            detail: String::new(),
        };
        assert!(!gpu.drivable());
    }
}
