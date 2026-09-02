//! GPU readings, from whatever each vendor actually exposes.
//!
//! Three sources, in decreasing order of how much they tell us:
//!
//! - `nvidia-smi`, which reports everything in one query.
//! - DRM sysfs, where amdgpu publishes utilisation and VRAM, and Intel
//!   publishes its GT clock.
//! - The i915 perf PMU, which is the *only* place Intel exposes engine
//!   utilisation. It needs `CAP_PERFMON` (the daemon runs as root in
//!   production); unprivileged it simply stays unavailable and the field
//!   comes back `None` rather than a made-up zero.
//!
//! Marketing names come from one `lspci` at construction - a card's name
//! never changes, so paying for it per poll would be waste.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::metrics::{read_number, which};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuMetrics {
    pub name: String,
    pub driver: String,
    pub usage_percent: Option<f64>,
    pub temp_c: Option<f64>,
    pub mem_used_mb: Option<f64>,
    pub mem_total_mb: Option<f64>,
    pub power_w: Option<f64>,
    pub clock_mhz: Option<f64>,
    /// Whether this is the chip inside the CPU package or a card of its
    /// own. Hybrid laptops have both, and which one is busy is the whole
    /// question, so the UI shows them side by side and has to label them.
    /// `None` when the machine gave us nothing to judge by.
    pub integrated: Option<bool>,
}

/// Holds what is expensive to discover (card names) or stateful (the PMU
/// counters, which are cumulative and only mean something as a delta).
pub struct GpuReader {
    nvidia_smi_available: bool,
    /// PCI slot -> the name `lspci` resolved for it.
    names: HashMap<String, String>,
    i915: Option<I915Pmu>,
}

impl GpuReader {
    pub fn new() -> Self {
        Self {
            nvidia_smi_available: which("nvidia-smi"),
            names: pci_names(),
            i915: I915Pmu::open(),
        }
    }

    /// Whether Intel engine utilisation is actually readable. False on a
    /// machine with no Intel GPU, and on one where the daemon lacks
    /// `CAP_PERFMON` - the UI needs to tell "this card reports nothing"
    /// apart from "we were not allowed to ask".
    pub fn engine_stats_available(&self) -> bool {
        self.i915.is_some()
    }

    /// `elapsed` is the wall time since the previous sample, which is what
    /// the PMU's busy nanoseconds have to be divided by.
    pub fn sample(&mut self, elapsed: f64) -> Vec<GpuMetrics> {
        let mut gpus = Vec::new();
        if self.nvidia_smi_available {
            gpus.extend(read_nvidia_gpus());
        }
        let intel_busy = self.i915.as_mut().and_then(|pmu| pmu.busy_percent(elapsed));
        gpus.extend(self.read_drm_gpus(intel_busy));
        gpus
    }

    /// GPUs exposed through DRM sysfs. amdgpu reports utilisation and VRAM
    /// directly; Intel reports its clock here and its utilisation through
    /// `intel_busy`, measured by the PMU.
    fn read_drm_gpus(&self, intel_busy: Option<f64>) -> Vec<GpuMetrics> {
        let Ok(entries) = fs::read_dir("/sys/class/drm") else {
            return Vec::new();
        };

        let mut cards: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|path| match path.file_name().map(|n| n.to_string_lossy()) {
                // Whole cards ("card0"), not their connectors ("card0-DP-1").
                Some(name) => name.starts_with("card") && !name.contains('-'),
                None => false,
            })
            .collect();
        cards.sort();

        let mut gpus = Vec::new();
        for card in cards {
            let device = card.join("device");
            let Ok(uevent) = fs::read_to_string(device.join("uevent")) else { continue };
            let driver = uevent
                .lines()
                .find_map(|l| l.strip_prefix("DRIVER="))
                .unwrap_or("unknown")
                .to_string();

            // NVIDIA cards are already covered, with far better data, by nvidia-smi.
            if driver == "nvidia" {
                continue;
            }

            let intel = matches!(driver.as_str(), "i915" | "xe");

            gpus.push(GpuMetrics {
                name: self.name_for(&device, &driver, &card),
                usage_percent: read_number(&device.join("gpu_busy_percent"))
                    .or(if intel { intel_busy } else { None }),
                temp_c: hwmon_value(&device, "temp1_input").map(|v| v / 1000.0),
                power_w: hwmon_value(&device, "power1_average").map(|v| v / 1_000_000.0),
                clock_mhz: gt_clock_mhz(&card, &device),
                mem_used_mb: read_number(&device.join("mem_info_vram_used")).map(bytes_to_mb),
                mem_total_mb: read_number(&device.join("mem_info_vram_total")).map(bytes_to_mb),
                integrated: pci_slot(&device).as_deref().map(is_on_the_root_bus),
                driver,
            });
        }
        gpus
    }

    /// The marketing name where `lspci` knew one, otherwise something that
    /// at least identifies the card rather than "unknown".
    fn name_for(&self, device: &Path, driver: &str, card: &Path) -> String {
        if let Some(name) = pci_slot(device).and_then(|slot| self.names.get(&slot)) {
            return name.clone();
        }
        let card = card.file_name().map(|n| n.to_string_lossy().to_string());
        format!("{driver} ({})", card.unwrap_or_else(|| "card".into()))
    }
}

fn bytes_to_mb(bytes: f64) -> f64 {
    bytes / 1024.0 / 1024.0
}

/// The GT clock Intel publishes. i915 puts it on the DRM node, xe under a
/// per-tile directory on the PCI device; amdgpu publishes neither.
fn gt_clock_mhz(card: &Path, device: &Path) -> Option<f64> {
    read_number(&card.join("gt_act_freq_mhz"))
        .or_else(|| read_number(&device.join("tile0/gt0/freq0/act_freq")))
}

/// The PCI slot a DRM device sits in, e.g. `0000:00:02.0`.
fn pci_slot(device: &Path) -> Option<String> {
    let target = fs::canonicalize(device).ok()?;
    Some(target.file_name()?.to_string_lossy().to_string())
}

/// Integrated GPUs hang off the root bus; a discrete card sits behind a
/// PCIe bridge and so has a non-zero bus number. Reading the slot is the
/// one test that does not depend on knowing every driver's habits.
fn is_on_the_root_bus(slot: &str) -> bool {
    // "0000:00:02.0" -> domain, bus, device.function
    match slot.split(':').nth(1) {
        Some(bus) => bus == "00",
        None => false,
    }
}

/// PCI slot -> display-adapter name, from one `lspci -mm` run. Empty when
/// pciutils isn't installed, which is not an error - the caller falls back
/// to the driver name.
fn pci_names() -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(output) = Command::new("lspci").arg("-mm").output() else { return names };
    if !output.status.success() {
        return names;
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields = split_lspci_fields(line);
        // -mm output: slot, class, vendor, device, [rev/subsystem...]
        let Some(class) = fields.get(1).map(|c| c.to_ascii_lowercase()) else { continue };
        if !(class.contains("vga") || class.contains("3d") || class.contains("display")) {
            continue;
        }
        let (Some(slot), Some(vendor), Some(device)) =
            (fields.first(), fields.get(2), fields.get(3))
        else {
            continue;
        };
        // lspci abbreviates the domain; sysfs always spells it out.
        let slot = if slot.matches(':').count() == 1 { format!("0000:{slot}") } else { slot.clone() };
        names.insert(slot, format!("{vendor} {device}"));
    }
    names
}

/// `lspci -mm` quotes any field containing spaces; a plain split would cut
/// device names in half.
fn split_lspci_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in line.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ' ' if !in_quotes => {
                if !current.is_empty() {
                    fields.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        fields.push(current);
    }
    fields
}

/// Reads one attribute from a hwmon node the device registers.
fn hwmon_value(device: &Path, attribute: &str) -> Option<f64> {
    let entries = fs::read_dir(device.join("hwmon")).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        if let Some(value) = read_number(&entry.path().join(attribute)) {
            return Some(value);
        }
    }
    None
}

/// NVIDIA cards, via one `nvidia-smi` query.
///
/// Shelling out once per poll is cheap (~25 ms) and needs no NVML bindings;
/// if the binary is missing or the driver isn't loaded it simply reports
/// nothing and the sysfs path above still covers other vendors.
fn read_nvidia_gpus() -> Vec<GpuMetrics> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,temperature.gpu,memory.used,memory.total,power.draw,clocks.gr",
            "--format=csv,noheader,nounits",
        ])
        .output();

    let Ok(output) = output else { return Vec::new() };
    if !output.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 7 {
                return None;
            }
            // "[N/A]" appears for values a given card doesn't report.
            let number = |i: usize| fields.get(i).and_then(|v| v.parse::<f64>().ok());
            Some(GpuMetrics {
                name: fields[0].to_string(),
                driver: "nvidia".to_string(),
                usage_percent: number(1),
                temp_c: number(2),
                mem_used_mb: number(3),
                mem_total_mb: number(4),
                power_w: number(5),
                clock_mhz: number(6),
                // No NVIDIA part on a machine this runs on is integrated.
                integrated: Some(false),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// i915 PMU
// ---------------------------------------------------------------------------

/// Intel's engine-busy counters, read through `perf_event_open`.
///
/// Each `*-busy` event counts nanoseconds that engine spent executing, so
/// utilisation is the delta over the wall time between two samples. The
/// number reported is the busiest engine rather than a sum: a machine whose
/// render engine is saturated is a busy GPU, and averaging that against
/// idle video-decode engines would hide it.
struct I915Pmu {
    /// One open counter per engine, with its last reading.
    engines: Vec<(Counter, u64)>,
}

impl I915Pmu {
    fn open() -> Option<Self> {
        let base = Path::new("/sys/devices/i915");
        let pmu_type = read_number(&base.join("type"))? as u32;
        // The PMU is per-package and only readable on the CPU its cpumask
        // names; the first entry is what perf itself uses.
        let cpu = fs::read_to_string(base.join("cpumask"))
            .ok()
            .and_then(|text| first_cpu(&text))
            .unwrap_or(0);

        let mut engines = Vec::new();
        let entries = fs::read_dir(base.join("events")).ok()?;
        let mut event_files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().ends_with("-busy"))
                    .unwrap_or(false)
            })
            .collect();
        event_files.sort();

        for path in event_files {
            let Some(config) = event_config(&path) else { continue };
            let Some(counter) = Counter::open(pmu_type, config, cpu) else { continue };
            let seed = counter.read().unwrap_or(0);
            engines.push((counter, seed));
        }

        if engines.is_empty() {
            // No permission (perf_event_paranoid > 0 and not root), or no
            // engines: either way there is nothing to report.
            return None;
        }
        Some(Self { engines })
    }

    fn busy_percent(&mut self, elapsed: f64) -> Option<f64> {
        let window_ns = elapsed * 1_000_000_000.0;
        if window_ns <= 0.0 {
            return None;
        }

        let mut busiest: Option<f64> = None;
        for (counter, previous) in &mut self.engines {
            let Some(current) = counter.read() else { continue };
            let delta = current.saturating_sub(*previous) as f64;
            *previous = current;
            let percent = (delta / window_ns * 100.0).clamp(0.0, 100.0);
            busiest = Some(busiest.map_or(percent, |b: f64| b.max(percent)));
        }
        busiest
    }
}

/// First CPU id in a sysfs cpumask list like `0-15` or `0,8`.
fn first_cpu(text: &str) -> Option<u32> {
    text.trim()
        .split([',', '-'])
        .next()
        .and_then(|v| v.trim().parse::<u32>().ok())
}

/// Parses a PMU event definition, e.g. `config=0x1`.
fn event_config(path: &Path) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    let value = text.trim().strip_prefix("config=")?;
    match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => value.parse::<u64>().ok(),
    }
}

/// The subset of `perf_event_attr` this needs, at the original ABI size.
///
/// Sticking to `PERF_ATTR_SIZE_VER0` (64 bytes) means every kernel since
/// perf existed accepts the struct; the fields added later are all ones we
/// leave at zero anyway.
#[repr(C)]
#[derive(Default)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    /// Bitfield (`disabled`, `inherit`, `exclude_*`, ...). All zero: the
    /// counter runs from the moment it is opened, and an uncore PMU rejects
    /// the `exclude_*` bits outright.
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

/// An open perf counter. Owns the fd and closes it on drop.
struct Counter {
    fd: i32,
}

impl Counter {
    fn open(pmu_type: u32, config: u64, cpu: u32) -> Option<Self> {
        let attr = PerfEventAttr {
            type_: pmu_type,
            size: std::mem::size_of::<PerfEventAttr>() as u32,
            config,
            ..Default::default()
        };

        // SAFETY: `attr` is a correctly sized, fully initialised
        // perf_event_attr that outlives the call. pid = -1 with a real cpu
        // is the system-wide form these uncore counters require, and
        // PERF_FLAG_FD_CLOEXEC (8) keeps the fd out of child processes.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &attr as *const PerfEventAttr,
                -1i32,
                cpu as i32,
                -1i32,
                8u64,
            )
        };
        if fd < 0 {
            return None;
        }
        Some(Self { fd: fd as i32 })
    }

    /// The counter's current value: nanoseconds busy, since it was opened.
    fn read(&self) -> Option<u64> {
        let mut buffer = [0u8; 8];
        // SAFETY: the buffer is 8 bytes and the fd is ours and still open.
        let read = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read != buffer.len() as isize {
            return None;
        }
        Some(u64::from_ne_bytes(buffer))
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        // SAFETY: the fd is ours, still open, and never used again.
        unsafe { libc::close(self.fd) };
    }
}

// ---------------------------------------------------------------------------
// Per-process GPU time
// ---------------------------------------------------------------------------

/// How much GPU each process is using, from the kernel's DRM fdinfo
/// interface (`/proc/<pid>/fdinfo/<fd>`, the `drm-engine-*` keys).
///
/// This is the same source `nvtop` and `intel_gpu_top` read, and it covers
/// every in-tree driver - i915, xe, amdgpu. NVIDIA's proprietary driver
/// publishes no fdinfo, so processes on such a card report `None` rather
/// than a zero that would read as "idle". `nvidia-smi pmon` is the only
/// alternative there and it is not one worth having: it samples for a whole
/// second per invocation, and on consumer cards it answers "-" anyway.
///
/// Two things make this less obvious than the CPU equivalent:
///
/// - Several file descriptors can refer to the *same* DRM client, each
///   reporting that client's whole counter. They have to be deduplicated by
///   `drm-client-id`, or a compositor holding four fds reads as four times
///   its real usage.
/// - An engine can have several instances (`drm-engine-capacity-video: 2`),
///   in which case its nanoseconds are spread over that many units of
///   parallel capacity.
pub struct ProcessGpuReader {
    /// pid -> engine-busy nanoseconds at the previous sample. Cumulative
    /// counters mean only the delta says anything.
    previous: HashMap<i32, u64>,
}

impl Default for ProcessGpuReader {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessGpuReader {
    pub fn new() -> Self {
        // Primed, so the first real sample is a delta over a known window
        // rather than over "since each process started".
        Self { previous: read_process_gpu_ns() }
    }

    /// pid -> percentage of `elapsed` that process kept the GPU busy.
    /// Absent from the map means the process holds no DRM client at all.
    pub fn sample(&mut self, elapsed: f64) -> HashMap<i32, f64> {
        let window_ns = elapsed * 1_000_000_000.0;
        let current = read_process_gpu_ns();

        let mut percents = HashMap::with_capacity(current.len());
        if window_ns > 0.0 {
            for (pid, busy_ns) in &current {
                let previous = self.previous.get(pid).copied().unwrap_or(*busy_ns);
                let delta = busy_ns.saturating_sub(previous) as f64;
                percents.insert(*pid, (delta / window_ns * 100.0).clamp(0.0, 100.0));
            }
        }

        self.previous = current;
        percents
    }
}

fn read_process_gpu_ns() -> HashMap<i32, u64> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return HashMap::new();
    };

    let mut busy = HashMap::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Some(ns) = pid_gpu_ns(pid) {
            busy.insert(pid, ns);
        }
    }
    busy
}

/// Engine-busy nanoseconds across every DRM client a process owns, or
/// `None` when it owns none.
fn pid_gpu_ns(pid: i32) -> Option<u64> {
    // Reading every fdinfo of every process would be thousands of file
    // reads per poll. The link tells us which descriptors are worth opening
    // for a fraction of the cost.
    let Ok(descriptors) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return None;
    };

    let mut clients: HashMap<(String, String), u64> = HashMap::new();
    for descriptor in descriptors.filter_map(|e| e.ok()) {
        match fs::read_link(descriptor.path()) {
            Ok(target) if target.starts_with("/dev/dri/") => {}
            _ => continue,
        }
        let name = descriptor.file_name();
        let Ok(text) = fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", name.to_string_lossy()))
        else {
            continue;
        };
        if let Some(client) = parse_drm_client(&text) {
            // Insert, never add: a second fd onto the same client is the
            // same counter read twice.
            clients.insert(client.key, client.busy_ns);
        }
    }

    if clients.is_empty() {
        return None;
    }
    Some(clients.values().sum())
}

struct DrmClient {
    /// What makes a client unique: its id, per GPU.
    key: (String, String),
    busy_ns: u64,
}

/// Parses one `/proc/<pid>/fdinfo/<fd>` describing a DRM client.
fn parse_drm_client(text: &str) -> Option<DrmClient> {
    const ENGINE: &str = "drm-engine-";
    const CAPACITY: &str = "drm-engine-capacity-";

    let mut pdev = String::new();
    let mut client_id = None;
    let mut engines: HashMap<&str, u64> = HashMap::new();
    let mut capacities: HashMap<&str, u64> = HashMap::new();

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();

        // The capacity keys share the engine prefix, so they have to be
        // recognised first or they would be read as an engine named
        // "capacity-video" whose value has no "ns" and is dropped.
        if let Some(engine) = key.strip_prefix(CAPACITY) {
            capacities.insert(engine, value.parse().unwrap_or(1));
        } else if let Some(engine) = key.strip_prefix(ENGINE) {
            if let Some(ns) = value.strip_suffix("ns").and_then(|v| v.trim().parse::<u64>().ok()) {
                engines.insert(engine, ns);
            }
        } else if key == "drm-pdev" {
            pdev = value.to_string();
        } else if key == "drm-client-id" {
            client_id = Some(value.to_string());
        }
    }

    let busy_ns = engines
        .iter()
        .map(|(engine, ns)| ns / capacities.get(engine).copied().unwrap_or(1).max(1))
        .sum();

    Some(DrmClient { key: (pdev, client_id?), busy_ns })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn several_fds_onto_one_client_are_not_counted_twice() {
        let fdinfo = "\
drm-driver:\ti915
drm-client-id:\t29
drm-pdev:\t0000:00:02.0
drm-engine-render:\t9000000000 ns
drm-engine-copy:\t50232 ns
";
        let first = parse_drm_client(fdinfo).expect("should parse");
        let second = parse_drm_client(fdinfo).expect("should parse");
        assert_eq!(first.key, second.key);

        let mut clients = HashMap::new();
        clients.insert(first.key, first.busy_ns);
        clients.insert(second.key, second.busy_ns);
        let total: u64 = clients.values().sum();
        assert_eq!(total, 9_000_050_232);
    }

    #[test]
    fn engine_capacity_spreads_the_nanoseconds() {
        // Two video engines busy for 2 s over a 1 s window is 100%, not 200%.
        let fdinfo = "\
drm-client-id:\t7
drm-engine-video:\t2000000000 ns
drm-engine-capacity-video:\t2
";
        let client = parse_drm_client(fdinfo).expect("should parse");
        assert_eq!(client.busy_ns, 1_000_000_000);
    }

    #[test]
    fn an_fdinfo_without_a_client_id_is_not_a_drm_client() {
        assert!(parse_drm_client("pos:\t0\nflags:\t0100002\n").is_none());
    }

    #[test]
    fn lspci_slots_are_normalised_to_the_sysfs_form() {
        let fields = split_lspci_fields(
            r#"00:02.0 "VGA compatible controller" "Intel Corporation" "Arrow Lake-P" -r04 "HP" "Device 8d2f""#,
        );
        assert_eq!(fields[0], "00:02.0");
        assert_eq!(fields[2], "Intel Corporation");
        assert_eq!(fields[3], "Arrow Lake-P");
    }

    #[test]
    fn the_root_bus_is_what_tells_an_igpu_from_a_card() {
        assert!(is_on_the_root_bus("0000:00:02.0"));
        assert!(!is_on_the_root_bus("0000:01:00.0"));
        assert!(!is_on_the_root_bus("nonsense"));
    }

    #[test]
    fn event_config_reads_both_radices() {
        let dir = std::env::temp_dir().join("pyren-gpu-test");
        fs::create_dir_all(&dir).unwrap();
        let hex = dir.join("hex");
        fs::write(&hex, "config=0x1f\n").unwrap();
        assert_eq!(event_config(&hex), Some(0x1f));
        let decimal = dir.join("decimal");
        fs::write(&decimal, "config=3\n").unwrap();
        assert_eq!(event_config(&decimal), Some(3));
    }

    #[test]
    fn cpumask_lists_and_ranges_both_yield_the_first_cpu() {
        assert_eq!(first_cpu("0-15\n"), Some(0));
        assert_eq!(first_cpu("4,12\n"), Some(4));
        assert_eq!(first_cpu(""), None);
    }

    #[test]
    fn perf_event_attr_is_the_original_abi_size() {
        assert_eq!(std::mem::size_of::<PerfEventAttr>(), 64);
    }
}
