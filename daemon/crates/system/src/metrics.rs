//! Live machine readings: CPU, memory, temperatures, fans, disks, network,
//! GPUs and the busiest processes.
//!
//! All of this is generic Linux (`/proc`, `/sys`, `statvfs`) rather than
//! HP-specific, so it works on any machine - which is what lets the vitals
//! UI be developed and tested away from an OMEN laptop.
//!
//! Rates (CPU %, network throughput, per-process CPU) are deltas between
//! consecutive samples, so the [`Sampler`] has to keep the previous one.
//! It is primed at construction, so the first call already returns real
//! numbers instead of zeroes.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::gpu::{read_nvidia_gpus, DrmUsageReader, GpuMetrics, GpuReader, GpuUsage};

/// Busiest processes reported per sample. Matches what the UI table shows.
const TOP_PROCESSES: usize = 12;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub cpu: CpuMetrics,
    pub memory: MemoryMetrics,
    pub temperatures: Vec<TempReading>,
    pub fans: Vec<FanReading>,
    pub disks: Vec<DiskUsage>,
    pub network: NetworkMetrics,
    pub gpus: Vec<GpuMetrics>,
    pub processes: Vec<ProcessUsage>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuMetrics {
    pub usage_percent: f64,
    pub per_core_percent: Vec<f64>,
    pub clocks_mhz: Vec<f64>,
    /// Package temperature, or the hottest core if no package sensor exists.
    pub temp_c: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryMetrics {
    pub total_gb: f64,
    pub used_gb: f64,
    pub available_gb: f64,
    pub percent: f64,
    pub swap_total_gb: f64,
    pub swap_used_gb: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TempReading {
    pub chip: String,
    pub label: String,
    pub celsius: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FanReading {
    pub chip: String,
    pub label: String,
    pub rpm: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskUsage {
    pub mount: String,
    pub device: String,
    pub fstype: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkMetrics {
    pub up_mbps: f64,
    pub down_mbps: f64,
    pub interfaces: Vec<InterfaceRate>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InterfaceRate {
    pub name: String,
    pub up_mbps: f64,
    pub down_mbps: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessUsage {
    pub pid: i32,
    pub name: String,
    pub cpu_percent: f64,
    pub mem_mb: f64,
    /// `None` for a process holding no GPU at all, and for every process on
    /// a card whose driver publishes no per-client accounting - which is
    /// not the same thing as zero, so the UI shows the two differently.
    pub gpu_percent: Option<f64>,
}

/// Counters from one `/proc/stat` CPU line.
#[derive(Debug, Clone, Copy, Default)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

impl CpuTimes {
    /// Percentage of time spent busy between two samples. `iowait` counts as
    /// idle, matching what `top` and most system monitors report.
    fn usage_since(&self, previous: &CpuTimes) -> f64 {
        let total = self.total.saturating_sub(previous.total);
        if total == 0 {
            return 0.0;
        }
        let busy = self.busy.saturating_sub(previous.busy);
        (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

#[derive(Debug, Default, Clone)]
struct CpuSample {
    total: CpuTimes,
    per_core: Vec<CpuTimes>,
}

#[derive(Debug, Clone, Copy, Default)]
struct NetCounters {
    rx_bytes: u64,
    tx_bytes: u64,
}

/// Everything one sample gathers from the machine, before anything is
/// derived from it. Exists so [`Sampler::gather`] has one thing to return
/// after joining the threads it spread the work over.
struct Raw {
    temperatures: Vec<TempReading>,
    fans: Vec<FanReading>,
    drm: GpuUsage,
    disks: Vec<DiskUsage>,
    processes: Vec<(i32, ProcessStat)>,
    nvidia: Vec<GpuMetrics>,
    cpu: CpuSample,
    clocks: Vec<f64>,
    memory: MemoryMetrics,
    net: HashMap<String, NetCounters>,
}

/// Holds the previous sample so rates can be derived. Not `Sync` by itself -
/// the module wraps it in a `Mutex`.
pub struct Sampler {
    ticks_per_second: f64,
    page_size: u64,
    cpu: CpuSample,
    net: HashMap<String, NetCounters>,
    process_ticks: HashMap<i32, u64>,
    /// `None` for the very first sample taken at construction.
    last_sampled: Instant,
    hwmon: HwmonCatalog,
    gpus: GpuReader,
    drm_usage: DrmUsageReader,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        let mut sampler = Self {
            // sysconf never fails for these two, but a bad value would make
            // every derived rate nonsense, so fall back to the usual ones.
            ticks_per_second: match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
                t if t > 0 => t as f64,
                _ => 100.0,
            },
            page_size: match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
                p if p > 0 => p as u64,
                _ => 4096,
            },
            cpu: CpuSample::default(),
            net: HashMap::new(),
            process_ticks: HashMap::new(),
            last_sampled: Instant::now(),
            hwmon: HwmonCatalog::new(),
            gpus: GpuReader::new(),
            drm_usage: DrmUsageReader::new(),
        };
        // Prime the deltas so the first real call reports actual usage.
        sampler.prime();
        sampler
    }

    fn prime(&mut self) {
        self.cpu = read_cpu_sample();
        self.net = read_net_counters();
        self.process_ticks = read_process_ticks();
        self.last_sampled = Instant::now();
    }

    /// See [`GpuReader::engine_stats_available`].
    pub fn engine_stats_available(&self) -> bool {
        self.gpus.engine_stats_available()
    }

    pub fn sample(&mut self) -> Metrics {
        let elapsed = self.last_sampled.elapsed().as_secs_f64().max(0.001);
        self.last_sampled = Instant::now();

        let raw = self.gather(elapsed);

        // Everything below is arithmetic over what was gathered, plus the
        // reads that had to wait for it. None of it touches the disk.
        let cpu = self.sample_cpu(raw.cpu, raw.clocks, &raw.temperatures);
        let network = self.sample_network(raw.net, elapsed);
        let processes = self.sample_processes(elapsed, &raw.drm.per_pid, raw.processes);
        let gpus = self.gpus.sample(elapsed, &raw.drm.per_card, raw.nvidia);

        Metrics {
            cpu,
            memory: raw.memory,
            temperatures: raw.temperatures,
            fans: raw.fans,
            disks: raw.disks,
            network,
            gpus,
            processes,
        }
    }

    /// Runs every independent sweep at once.
    ///
    /// These are blocking reads that have nothing to say to each other, and
    /// in series they added up: the sample was as slow as their sum. Run
    /// together it is as slow as the worst of them, which is `nvidia-smi`.
    ///
    /// A sweep that panics degrades to its empty value rather than taking
    /// the sample - and with it the connection - down with it.
    fn gather(&mut self, elapsed: f64) -> Raw {
        // Disjoint field borrows, so the two stateful sweeps can be handed
        // to threads without borrowing the whole sampler.
        let hwmon = &mut self.hwmon;
        let drm_usage = &mut self.drm_usage;
        let nvidia_available = self.gpus.nvidia_smi_available();

        thread::scope(|scope| {
            let hwmon_job = scope.spawn(|| hwmon.sample());
            // One walk of /proc/*/fdinfo feeds both the per-card utilisation
            // and the process table's GPU column.
            let drm_job = scope.spawn(|| drm_usage.sample(elapsed));
            let disks_job = scope.spawn(read_disks);
            let processes_job = scope.spawn(read_process_stats);
            let nvidia_job = scope.spawn(move || {
                if nvidia_available {
                    read_nvidia_gpus()
                } else {
                    Vec::new()
                }
            });

            // Four small /proc reads, done on this thread while the above
            // run: each is well under a millisecond, so a thread apiece
            // would cost more to start than to do.
            let cpu = read_cpu_sample();
            let clocks = read_cpu_clocks();
            let memory = read_memory();
            let net = read_net_counters();

            let (temperatures, fans) = hwmon_job.join().unwrap_or_default();
            Raw {
                temperatures,
                fans,
                drm: drm_job.join().unwrap_or_default(),
                disks: disks_job.join().unwrap_or_default(),
                processes: processes_job.join().unwrap_or_default(),
                nvidia: nvidia_job.join().unwrap_or_default(),
                cpu,
                clocks,
                memory,
                net,
            }
        })
    }

    fn sample_cpu(
        &mut self,
        current: CpuSample,
        clocks: Vec<f64>,
        temperatures: &[TempReading],
    ) -> CpuMetrics {
        let usage_percent = current.total.usage_since(&self.cpu.total);
        let per_core_percent = current
            .per_core
            .iter()
            .enumerate()
            .map(|(i, core)| core.usage_since(self.cpu.per_core.get(i).unwrap_or(&CpuTimes::default())))
            .collect();

        self.cpu = current;

        CpuMetrics {
            usage_percent,
            per_core_percent,
            clocks_mhz: clocks,
            temp_c: cpu_temperature(temperatures),
        }
    }

    fn sample_network(
        &mut self,
        current: HashMap<String, NetCounters>,
        elapsed: f64,
    ) -> NetworkMetrics {
        let mut interfaces = Vec::new();
        let (mut up_total, mut down_total) = (0.0, 0.0);

        for (name, counters) in &current {
            let previous = self.net.get(name).copied().unwrap_or(*counters);
            let down = to_mbps(counters.rx_bytes.saturating_sub(previous.rx_bytes), elapsed);
            let up = to_mbps(counters.tx_bytes.saturating_sub(previous.tx_bytes), elapsed);
            up_total += up;
            down_total += down;
            interfaces.push(InterfaceRate { name: name.clone(), up_mbps: up, down_mbps: down });
        }

        interfaces.sort_by(|a, b| a.name.cmp(&b.name));
        self.net = current;

        NetworkMetrics { up_mbps: up_total, down_mbps: down_total, interfaces }
    }

    /// Turns one `/proc` walk into the busiest processes. The walk itself is
    /// [`read_process_stats`], done on another thread; only the rates need
    /// the previous sample and therefore the sampler.
    fn sample_processes(
        &mut self,
        elapsed: f64,
        gpu: &HashMap<i32, f64>,
        stats: Vec<(i32, ProcessStat)>,
    ) -> Vec<ProcessUsage> {
        let cores = self.cpu.per_core.len().max(1) as f64;
        let mut current_ticks = HashMap::with_capacity(stats.len());
        let mut processes = Vec::with_capacity(stats.len());

        for (pid, parsed) in stats {
            current_ticks.insert(pid, parsed.cpu_ticks);
            let previous = self.process_ticks.get(&pid).copied().unwrap_or(parsed.cpu_ticks);
            let delta = parsed.cpu_ticks.saturating_sub(previous) as f64;

            processes.push(ProcessUsage {
                pid,
                name: parsed.name,
                // As a share of the whole machine, so the column reads 0-100
                // like the reference app rather than per-core percentages.
                cpu_percent: (delta / self.ticks_per_second / elapsed / cores * 100.0)
                    .clamp(0.0, 100.0),
                mem_mb: (parsed.rss_pages * self.page_size) as f64 / 1024.0 / 1024.0,
                gpu_percent: gpu.get(&pid).copied(),
            });
        }

        self.process_ticks = current_ticks;
        processes.sort_by(|a, b| {
            b.cpu_percent
                .partial_cmp(&a.cpu_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.mem_mb.partial_cmp(&a.mem_mb).unwrap_or(std::cmp::Ordering::Equal))
        });
        processes.truncate(TOP_PROCESSES);
        processes
    }

}

fn to_mbps(bytes: u64, elapsed: f64) -> f64 {
    bytes as f64 * 8.0 / 1_000_000.0 / elapsed
}

pub(crate) fn which(binary: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(binary).is_file())
}

fn read_cpu_sample() -> CpuSample {
    let Ok(stat) = fs::read_to_string("/proc/stat") else {
        return CpuSample::default();
    };

    let mut sample = CpuSample::default();
    for line in stat.lines() {
        if !line.starts_with("cpu") {
            break; // the cpu* lines always come first
        }
        let mut fields = line.split_whitespace();
        let Some(label) = fields.next() else { continue };

        // user nice system idle iowait irq softirq steal ...
        let values: Vec<u64> = fields.filter_map(|v| v.parse::<u64>().ok()).collect();
        if values.len() < 5 {
            continue;
        }
        let total: u64 = values.iter().sum();
        let idle = values[3] + values[4];
        let times = CpuTimes { busy: total.saturating_sub(idle), total };

        if label == "cpu" {
            sample.total = times;
        } else {
            sample.per_core.push(times);
        }
    }
    sample
}

fn read_cpu_clocks() -> Vec<f64> {
    let mut clocks = Vec::new();
    let mut index = 0;
    // cpufreq is absent on some systems (VMs, cpufreq driver not loaded);
    // an empty list is a valid answer, not an error.
    loop {
        let path = format!("/sys/devices/system/cpu/cpu{index}/cpufreq/scaling_cur_freq");
        let Ok(value) = fs::read_to_string(&path) else {
            break;
        };
        match value.trim().parse::<f64>() {
            Ok(khz) => clocks.push(khz / 1000.0),
            Err(_) => break,
        }
        index += 1;
    }
    clocks
}

fn read_memory() -> MemoryMetrics {
    let mut values: HashMap<&str, f64> = HashMap::new();
    let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();

    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else { continue };
        let kb = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        // Interned keys keep the map borrow-free; only these are needed.
        for wanted in ["MemTotal", "MemAvailable", "SwapTotal", "SwapFree"] {
            if key == wanted {
                values.insert(wanted, kb);
            }
        }
    }

    let to_gb = |kb: f64| kb / 1024.0 / 1024.0;
    let total_gb = to_gb(values.get("MemTotal").copied().unwrap_or(0.0));
    let available_gb = to_gb(values.get("MemAvailable").copied().unwrap_or(0.0));
    // "Used" here means total minus available, i.e. what applications would
    // actually have to give back - not total minus free, which counts cache.
    let used_gb = (total_gb - available_gb).max(0.0);
    let swap_total_gb = to_gb(values.get("SwapTotal").copied().unwrap_or(0.0));
    let swap_free_gb = to_gb(values.get("SwapFree").copied().unwrap_or(0.0));

    MemoryMetrics {
        percent: if total_gb > 0.0 { used_gb / total_gb * 100.0 } else { 0.0 },
        total_gb,
        used_gb,
        available_gb,
        swap_total_gb,
        swap_used_gb: (swap_total_gb - swap_free_gb).max(0.0),
    }
}

// ---------------------------------------------------------------------------
// hwmon
// ---------------------------------------------------------------------------

/// How long a chip's sweep has to take before it is demoted to the slow
/// cadence below.
///
/// Reading a `temp*_input` is normally a cached value the driver already
/// has - under a millisecond, and often under a hundred microseconds. On
/// some chips it is not: an NVMe controller turns the read into a SMART
/// command to the drive, measured at 40-130 ms on the test laptop (130 when
/// the drive was idle enough to have gone quiet). That one file was the
/// largest single cost in a sample, and paying it every two seconds also
/// keeps the drive awake for nothing.
const SLOW_CHIP_THRESHOLD: Duration = Duration::from_millis(10);

/// How often a chip whose temperatures are slow is actually read for them.
/// Nothing behind such a sensor - a disk controller, a battery - moves fast
/// enough for this to lose a reading anyone would notice.
///
/// This applies to temperatures and deliberately not to fans, however
/// expensive a chip's `fan*_input` turns out to be: a temperature that is
/// half a minute old is off by a degree, while an RPM that is half a minute
/// old is the wrong answer to the question the fan page exists to ask. The
/// `hp` chip on the test laptop costs ~15 ms for its two fans, over the
/// threshold above, and is still read every sample for exactly that reason.
/// The sweep runs alongside `nvidia-smi`, which is slower still, so it
/// costs nothing in wall time anyway.
const SLOW_CHIP_INTERVAL: Duration = Duration::from_secs(30);

/// How often the catalog is rebuilt, so a chip that shows up later - a USB
/// sensor plugged in, a module loaded after the daemon started - is picked
/// up without restarting the daemon.
const REDISCOVER_INTERVAL: Duration = Duration::from_secs(30);

/// One `temp*_input` or `fan*_input`.
///
/// The label is resolved at discovery because it is fixed for the life of
/// the node; re-reading `temp1_label` on every sample was one extra open
/// per sensor for a string that never changed.
struct HwmonInput {
    path: PathBuf,
    label: String,
}

/// One hwmon chip and everything worth reading from it.
struct HwmonChip {
    name: String,
    /// The chip's directory, which is what identifies it across a rescan.
    dir: PathBuf,
    temps: Vec<HwmonInput>,
    fans: Vec<HwmonInput>,
    /// Set once this chip's *temperature* sweep has been measured over
    /// [`SLOW_CHIP_THRESHOLD`]. Measured rather than matched against a list
    /// of chip names: which sensors are expensive is a property of the
    /// machine, and a hardcoded list would be wrong on the next one.
    slow_temps: bool,
    /// The last temperatures read. Returned unchanged while a slow chip is
    /// between refreshes, so the UI keeps showing a real reading rather
    /// than a gap.
    temp_readings: Vec<TempReading>,
    fan_readings: Vec<FanReading>,
    last_temp_read: Option<Instant>,
}

impl HwmonChip {
    /// Refreshes this chip: fans always, temperatures unless this is a slow
    /// chip that was read recently.
    fn read(&mut self) {
        self.fan_readings = self
            .fans
            .iter()
            .filter_map(|input| {
                Some(FanReading {
                    chip: self.name.clone(),
                    label: input.label.clone(),
                    rpm: read_number(&input.path)? as i64,
                })
            })
            .collect();

        if self.temps.is_empty() || self.skip_temps() {
            return;
        }

        let started = Instant::now();
        let mut temps: Vec<TempReading> = self
            .temps
            .iter()
            .filter_map(|input| {
                Some(TempReading {
                    chip: self.name.clone(),
                    label: input.label.clone(),
                    celsius: read_number(&input.path)? / 1000.0,
                })
            })
            .collect();
        temps.sort_by(|a, b| a.label.cmp(&b.label));

        self.slow_temps = started.elapsed() >= SLOW_CHIP_THRESHOLD;
        self.last_temp_read = Some(Instant::now());
        self.temp_readings = temps;
    }

    fn skip_temps(&self) -> bool {
        if !self.slow_temps {
            return false;
        }
        match self.last_temp_read {
            Some(last) => last.elapsed() < SLOW_CHIP_INTERVAL,
            None => false,
        }
    }
}

/// Every hwmon chip, discovered once and rescanned occasionally.
///
/// This used to be two functions that each walked `/sys/class/hwmon` from
/// scratch - one for temperatures, one for fans - so every sample did the
/// directory walk twice and re-read every label. The walk was never the
/// expensive part; the reads on a slow chip were, and doing them twice as
/// often as needed made it worse.
struct HwmonCatalog {
    chips: Vec<HwmonChip>,
    discovered: Instant,
}

impl HwmonCatalog {
    fn new() -> Self {
        let mut catalog = Self { chips: discover_hwmon(), discovered: Instant::now() };
        // Read once here, at startup, rather than leaving it to the first
        // client: this is where the cost of a slow chip is discovered, and
        // paying it before anyone is waiting means the first `getMetrics`
        // is already fast and already knows which chips to back off from.
        for chip in &mut catalog.chips {
            chip.read();
        }
        catalog
    }

    /// Temperatures in chip order (labels sorted within each chip), and
    /// fans sorted by chip and label - the order the UI has always seen.
    fn sample(&mut self) -> (Vec<TempReading>, Vec<FanReading>) {
        if self.discovered.elapsed() >= REDISCOVER_INTERVAL {
            self.rediscover();
        }

        let mut temperatures = Vec::new();
        let mut fans = Vec::new();
        for chip in &mut self.chips {
            chip.read();
            temperatures.extend(chip.temp_readings.iter().cloned());
            fans.extend(chip.fan_readings.iter().cloned());
        }

        fans.sort_by(|a, b| (&a.chip, &a.label).cmp(&(&b.chip, &b.label)));
        (temperatures, fans)
    }

    fn rediscover(&mut self) {
        let mut fresh = discover_hwmon();
        for chip in &mut fresh {
            // Carry over what was already learned about a chip that is
            // still there. Without this, every rescan would re-measure the
            // slow chips by reading them - which is the cost being avoided.
            let Some(known) = self.chips.iter().find(|c| c.dir == chip.dir && c.name == chip.name)
            else {
                continue;
            };
            chip.slow_temps = known.slow_temps;
            chip.last_temp_read = known.last_temp_read;
            chip.temp_readings = known.temp_readings.clone();
            chip.fan_readings = known.fan_readings.clone();
        }
        self.chips = fresh;
        self.discovered = Instant::now();
    }
}

/// Walks `/sys/class/hwmon` and resolves every input path and label.
fn discover_hwmon() -> Vec<HwmonChip> {
    let Ok(entries) = fs::read_dir("/sys/class/hwmon") else {
        return Vec::new();
    };

    let mut chips: Vec<HwmonChip> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter_map(|dir| {
            let name = fs::read_to_string(dir.join("name")).ok()?.trim().to_string();
            let mut temps = Vec::new();
            let mut fans = Vec::new();

            for entry in fs::read_dir(&dir).ok()?.filter_map(|e| e.ok()) {
                let file = entry.file_name();
                let file = file.to_string_lossy();
                let index = |prefix: &str| {
                    file.strip_prefix(prefix)
                        .and_then(|rest| rest.strip_suffix("_input"))
                        .map(str::to_string)
                };

                if let Some(i) = index("temp") {
                    temps.push(HwmonInput { label: label_for(&dir, "temp", &i), path: entry.path() });
                } else if let Some(i) = index("fan") {
                    fans.push(HwmonInput { label: label_for(&dir, "fan", &i), path: entry.path() });
                }
            }

            // A chip exposing neither is one this module has nothing to say
            // about; keeping it would mean reading its name again forever.
            if temps.is_empty() && fans.is_empty() {
                return None;
            }

            Some(HwmonChip {
                name,
                dir,
                temps,
                fans,
                slow_temps: false,
                temp_readings: Vec::new(),
                fan_readings: Vec::new(),
                last_temp_read: None,
            })
        })
        .collect();

    chips.sort_by(|a, b| (&a.name, &a.dir).cmp(&(&b.name, &b.dir)));
    chips
}

/// `temp1_label` where the chip provides one, `temp1` otherwise.
fn label_for(dir: &Path, prefix: &str, index: &str) -> String {
    fs::read_to_string(dir.join(format!("{prefix}{index}_label")))
        .map(|label| label.trim().to_string())
        .unwrap_or_else(|_| format!("{prefix}{index}"))
}

/// Picks the reading the UI should call "the CPU temperature".
///
/// Prefers a package sensor from a CPU driver, then the hottest core from
/// one, then the ACPI thermal zone - the same fallback chain the Python
/// original used, generalised to whatever chips are present.
fn cpu_temperature(readings: &[TempReading]) -> Option<f64> {
    const CPU_CHIPS: &[&str] = &["coretemp", "k10temp", "zenpower", "cpu_thermal"];

    let from_cpu_chip = |predicate: &dyn Fn(&TempReading) -> bool| -> Option<f64> {
        readings
            .iter()
            .filter(|r| CPU_CHIPS.contains(&r.chip.as_str()) && predicate(r))
            .map(|r| r.celsius)
            .fold(None, |acc: Option<f64>, c| Some(acc.map_or(c, |a| a.max(c))))
    };

    from_cpu_chip(&|r| {
        let label = r.label.to_ascii_lowercase();
        label.contains("package") || label.contains("tctl") || label.contains("tdie")
    })
    .or_else(|| from_cpu_chip(&|_| true))
    .or_else(|| {
        readings
            .iter()
            .find(|r| r.chip == "acpitz")
            .map(|r| r.celsius)
    })
}

pub(crate) fn read_number(path: &Path) -> Option<f64> {
    fs::read_to_string(path).ok()?.trim().parse::<f64>().ok()
}

/// Mounted filesystems worth showing, with free space from `statvfs`.
fn read_disks() -> Vec<DiskUsage> {
    const REAL_FSTYPES: &[&str] = &[
        "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "zfs", "vfat", "exfat", "ntfs", "ntfs3",
        "bcachefs",
    ];

    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };

    let mut disks: Vec<DiskUsage> = Vec::new();
    for line in mounts.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [device, mount, fstype, ..] = fields[..] else { continue };
        if !REAL_FSTYPES.contains(&fstype) {
            continue;
        }
        // btrfs subvolumes and bind mounts show the same device many times;
        // keep the shallowest mount point for each device.
        if let Some(existing) = disks.iter_mut().find(|d| d.device == device) {
            if mount.len() < existing.mount.len() {
                existing.mount = unescape_mount(mount);
            }
            continue;
        }
        let Some((total_bytes, free_bytes)) = statvfs(mount) else { continue };

        disks.push(DiskUsage {
            mount: unescape_mount(mount),
            device: device.to_string(),
            fstype: fstype.to_string(),
            total_bytes,
            free_bytes,
        });
    }

    disks.sort_by(|a, b| a.mount.cmp(&b.mount));
    disks
}

/// `/proc/mounts` octal-escapes spaces and a few other characters.
fn unescape_mount(mount: &str) -> String {
    let mut out = String::with_capacity(mount.len());
    let mut chars = mount.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) if digits.len() == 3 => {
                out.push(byte as char);
                for _ in 0..3 {
                    chars.next();
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

/// (total bytes, bytes free to unprivileged users) for a mount point.
fn statvfs(mount: &str) -> Option<(u64, u64)> {
    let path = CString::new(mount).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };

    // SAFETY: `path` is a valid NUL-terminated string that outlives the
    // call, and `stat` is a correctly sized, zeroed output buffer.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }

    let block_size = if stat.f_frsize > 0 { stat.f_frsize } else { stat.f_bsize } as u64;
    Some((stat.f_blocks as u64 * block_size, stat.f_bavail as u64 * block_size))
}

fn read_net_counters() -> HashMap<String, NetCounters> {
    // Bridges, containers and virtual pairs would double-count traffic that
    // also crosses a physical interface.
    const VIRTUAL_PREFIXES: &[&str] = &["lo", "veth", "docker", "br-", "virbr", "vnet", "tap"];

    let Ok(entries) = fs::read_dir("/sys/class/net") else {
        return HashMap::new();
    };

    let mut counters = HashMap::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        if VIRTUAL_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let stats = entry.path().join("statistics");
        let rx = read_number(&stats.join("rx_bytes"));
        let tx = read_number(&stats.join("tx_bytes"));
        if let (Some(rx), Some(tx)) = (rx, tx) {
            counters.insert(name, NetCounters { rx_bytes: rx as u64, tx_bytes: tx as u64 });
        }
    }
    counters
}

struct ProcessStat {
    name: String,
    cpu_ticks: u64,
    rss_pages: u64,
}

/// Parses `/proc/<pid>/stat`.
///
/// The command name is in parentheses and may itself contain spaces and
/// parentheses, so the remaining fields are located from the *last* `)`
/// rather than by splitting the whole line.
fn parse_process_stat(stat: &str) -> Option<ProcessStat> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();

    // Fields after the closing paren start at field 3 (state), so field N
    // lives at index N - 3.
    let fields: Vec<&str> = stat.get(close + 2..)?.split_whitespace().collect();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    let rss_pages = fields.get(21)?.parse::<u64>().ok()?;

    Some(ProcessStat { name, cpu_ticks: utime + stime, rss_pages })
}

/// One walk of `/proc/*/stat`.
///
/// Split out from the process table so it can be run on its own thread:
/// it is a few hundred small reads that depend on nothing else in a sample.
fn read_process_stats() -> Vec<(i32, ProcessStat)> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let pid = entry.file_name().to_string_lossy().parse::<i32>().ok()?;
            // Processes exit between the readdir and the read; that's normal,
            // not an error worth reporting.
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            Some((pid, parse_process_stat(&stat)?))
        })
        .collect()
}

fn read_process_ticks() -> HashMap<i32, u64> {
    read_process_stats().into_iter().map(|(pid, stat)| (pid, stat.cpu_ticks)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_stat_survives_names_with_spaces_and_parens() {
        // utime is field 14 and stime field 15; here 111 and 22.
        let stat = "1234 (weird ) name) S 1 1234 1234 0 -1 4194560 100 0 0 0 111 22 0 0 20 0 4 0 \
                    9876 123456 555 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
        let parsed = parse_process_stat(stat).expect("should parse");
        assert_eq!(parsed.name, "weird ) name");
        assert_eq!(parsed.cpu_ticks, 133);
        assert_eq!(parsed.rss_pages, 555);
    }

    #[test]
    fn cpu_usage_is_the_busy_share_of_the_delta() {
        let previous = CpuTimes { busy: 100, total: 200 };
        let current = CpuTimes { busy: 150, total: 300 };
        assert!((current.usage_since(&previous) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn cpu_usage_of_an_empty_delta_is_zero_not_nan() {
        let same = CpuTimes { busy: 10, total: 20 };
        assert_eq!(same.usage_since(&same), 0.0);
    }

    #[test]
    fn counters_that_go_backwards_do_not_underflow() {
        // Can happen when an interface is reset or a process is replaced.
        let previous = CpuTimes { busy: 500, total: 900 };
        let current = CpuTimes { busy: 100, total: 200 };
        assert_eq!(current.usage_since(&previous), 0.0);
    }

    #[test]
    fn mount_points_are_unescaped() {
        assert_eq!(unescape_mount(r"/mnt/my\040disk"), "/mnt/my disk");
        assert_eq!(unescape_mount("/home"), "/home");
    }

    #[test]
    fn package_sensor_wins_over_individual_cores() {
        let readings = vec![
            TempReading { chip: "coretemp".into(), label: "Core 0".into(), celsius: 80.0 },
            TempReading { chip: "coretemp".into(), label: "Package id 0".into(), celsius: 65.0 },
            TempReading { chip: "acpitz".into(), label: "temp1".into(), celsius: 27.0 },
        ];
        assert_eq!(cpu_temperature(&readings), Some(65.0));
    }

    #[test]
    fn without_a_cpu_chip_the_thermal_zone_is_used() {
        let readings =
            vec![TempReading { chip: "acpitz".into(), label: "temp1".into(), celsius: 42.0 }];
        assert_eq!(cpu_temperature(&readings), Some(42.0));
    }

    #[test]
    fn no_sensors_at_all_reports_nothing_rather_than_zero() {
        assert_eq!(cpu_temperature(&[]), None);
    }
}
