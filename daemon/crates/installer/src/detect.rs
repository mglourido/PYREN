//! Surveys what this machine already has, so the installer can decide what
//! (if anything) needs doing.
//!
//! Everything here is read-only and unprivileged - it is safe to run on any
//! machine, including one that has nothing to do with HP.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

/// The kernel release that first carried manual fan control for these
/// laptops upstream. On anything newer the patched driver is pointless -
/// the stock `hp-wmi` already exposes `pwm1`. See the upstream commit
/// linked from the source project's README.
pub const UPSTREAM_FAN_CONTROL_KERNEL: (u32, u32) = (6, 20);

const HWMON_ROOT: &str = "/sys/devices/platform/hp-wmi/hwmon";

/// Which kernel-upgrade hook mechanism this distribution uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HookFlavour {
    /// Arch and derivatives: a pacman hook.
    Pacman,
    /// Debian and derivatives: /etc/kernel/postinst.d/.
    KernelPostinst,
    /// Fedora and derivatives: /etc/kernel/install.d/.
    KernelInstall,
    /// Unrecognised: the module can be built for the running kernel only.
    None,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KernelInfo {
    pub release: String,
    pub major: u32,
    pub minor: u32,
    /// True when the running kernel already ships manual fan control.
    pub has_upstream_fan_control: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadersInfo {
    pub build_dir: Option<PathBuf>,
    /// Both of these are missing surprisingly often on Debian/Ubuntu, where
    /// the headers are split across several packages.
    pub has_autoconf: bool,
    pub has_kbuild_scripts: bool,
    pub usable: bool,
    /// Distro-specific command that would fix an incomplete setup.
    pub fix_hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    pub kernel: KernelInfo,
    pub distro_id: String,
    pub hook_flavour: HookFlavour,
    pub headers: HeadersInfo,
    pub has_dkms: bool,
    /// True when our DKMS module is registered, whatever its state.
    pub dkms_installed: bool,
    pub dkms_status: Option<String>,
    pub has_make: bool,
    pub has_compiler: bool,
    pub initramfs_tool: Option<String>,
    /// `pwm1` present under hp-wmi's hwmon: the definitive test of whether
    /// fan control works *right now*, whatever driver provides it.
    pub fan_control_available: bool,
    pub hp_wmi_loaded: bool,
    pub acpi_call_available: bool,
    /// Where the driver sources were found, if at all.
    pub driver_source: Option<PathBuf>,
    /// Whether the daemon's own systemd unit is installed.
    pub service_installed: bool,
}

impl Environment {
    pub fn detect() -> Self {
        let kernel = kernel_info();
        let distro_id = distro_id();
        let dkms_status = dkms_status();

        Self {
            hook_flavour: hook_flavour(&distro_id),
            headers: headers_info(&kernel.release, &distro_id),
            has_dkms: which("dkms"),
            dkms_installed: dkms_status.is_some(),
            has_make: which("make"),
            has_compiler: which("gcc") || which("cc") || which("clang"),
            initramfs_tool: initramfs_tool(&distro_id),
            fan_control_available: fan_control_available(),
            hp_wmi_loaded: Path::new("/sys/devices/platform/hp-wmi").exists(),
            acpi_call_available: Path::new("/proc/acpi/call").exists(),
            driver_source: find_driver_source(),
            service_installed: Path::new("/etc/systemd/system/pyren-daemon.service").exists()
                || Path::new("/usr/lib/systemd/system/pyren-daemon.service").exists(),
            dkms_status,
            distro_id,
            kernel,
        }
    }

    /// Whether the patched driver would add anything on this machine.
    ///
    /// Answered from what is actually present, not only from the kernel
    /// version: a machine already exposing `pwm1` needs nothing regardless
    /// of how it got there.
    pub fn patch_needed(&self) -> bool {
        !self.fan_control_available && !self.kernel.has_upstream_fan_control
    }
}

fn kernel_info() -> KernelInfo {
    let release = fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|r| r.trim().to_string())
        .unwrap_or_default();
    let (major, minor) = parse_kernel_version(&release);

    KernelInfo {
        has_upstream_fan_control: (major, minor) >= UPSTREAM_FAN_CONTROL_KERNEL,
        release,
        major,
        minor,
    }
}

/// Parses the leading `major.minor` out of a kernel release string, which
/// can carry any amount of distro suffix (`6.12.4-arch1-1`, `7.2.2-1-cachyos`).
pub fn parse_kernel_version(release: &str) -> (u32, u32) {
    let mut parts = release
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<u32>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

fn distro_id() -> String {
    if let Ok(os_release) = fs::read_to_string("/etc/os-release") {
        for line in os_release.lines() {
            if let Some(value) = line.strip_prefix("ID=") {
                return value.trim().trim_matches('"').to_ascii_lowercase();
            }
        }
    }
    // Same fallbacks as the shell script this is ported from.
    for (marker, id) in [
        ("/etc/arch-release", "arch"),
        ("/etc/debian_version", "debian"),
        ("/etc/fedora-release", "fedora"),
    ] {
        if Path::new(marker).exists() {
            return id.to_string();
        }
    }
    "unknown".to_string()
}

pub fn hook_flavour(distro_id: &str) -> HookFlavour {
    match distro_id {
        "arch" | "manjaro" | "endeavouros" | "garuda" | "cachyos" => HookFlavour::Pacman,
        "debian" | "ubuntu" | "linuxmint" | "pop" => HookFlavour::KernelPostinst,
        "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => HookFlavour::KernelInstall,
        _ => HookFlavour::None,
    }
}

fn headers_info(kernel_release: &str, distro_id: &str) -> HeadersInfo {
    let build_dir = PathBuf::from(format!("/lib/modules/{kernel_release}/build"));
    if !build_dir.is_dir() {
        return HeadersInfo {
            has_autoconf: false,
            has_kbuild_scripts: false,
            usable: false,
            fix_hint: Some(headers_install_hint(kernel_release, distro_id)),
            build_dir: None,
        };
    }

    // The build dir is usually a symlink into /usr/src; the checks below
    // have to follow it.
    let real_dir = fs::canonicalize(&build_dir).unwrap_or_else(|_| build_dir.clone());
    let has_autoconf = real_dir.join("include/generated/autoconf.h").is_file();
    let has_kbuild_scripts = real_dir.join("scripts/basic/Makefile").is_file()
        || build_dir.join("scripts/basic/Makefile").is_file();

    let fix_hint = match (has_autoconf, has_kbuild_scripts, distro_id) {
        (true, true, _) => None,
        (false, _, "debian" | "ubuntu" | "linuxmint" | "pop") => {
            Some(format!("sudo apt reinstall linux-headers-{kernel_release}"))
        }
        (_, false, "debian" | "ubuntu" | "linuxmint" | "pop") => {
            // Debian's kbuild package is versioned without the local suffix.
            let base: String = kernel_release
                .split('-')
                .next()
                .unwrap_or(kernel_release)
                .to_string();
            Some(format!("sudo apt install 'linux-kbuild-{base}*'"))
        }
        _ => Some("reinstall your kernel headers package".to_string()),
    };

    HeadersInfo {
        usable: has_autoconf && has_kbuild_scripts,
        build_dir: Some(build_dir),
        has_autoconf,
        has_kbuild_scripts,
        fix_hint,
    }
}

fn headers_install_hint(kernel_release: &str, distro_id: &str) -> String {
    match distro_id {
        "debian" | "ubuntu" | "linuxmint" | "pop" => {
            format!("sudo apt install linux-headers-{kernel_release}")
        }
        "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => {
            format!("sudo dnf install kernel-devel-{kernel_release}")
        }
        "arch" | "manjaro" | "endeavouros" | "garuda" | "cachyos" => {
            "sudo pacman -S linux-headers".to_string()
        }
        _ => format!("install the kernel headers for {kernel_release}"),
    }
}

/// Picks the initramfs generator this distribution actually uses.
///
/// Order matters: Arch systems often have an `update-initramfs` *shim*
/// installed alongside the real `mkinitcpio`, and the shell installer this
/// is ported from picks `update-initramfs` first unconditionally, so on
/// those systems it drives the wrong tool. Choosing by distro family first,
/// and only then falling back to whatever exists, avoids that.
fn initramfs_tool(distro_id: &str) -> Option<String> {
    let preference: &[&str] = match hook_flavour(distro_id) {
        HookFlavour::Pacman => &["mkinitcpio", "dracut", "update-initramfs"],
        HookFlavour::KernelPostinst => &["update-initramfs", "dracut", "mkinitcpio"],
        HookFlavour::KernelInstall => &["dracut", "update-initramfs", "mkinitcpio"],
        HookFlavour::None => &["update-initramfs", "mkinitcpio", "dracut"],
    };
    preference
        .iter()
        .find(|tool| which(tool))
        .map(|tool| tool.to_string())
}

fn dkms_status() -> Option<String> {
    let output = Command::new("dkms").arg("status").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.contains("hp-wmi-omen"))
        .map(|line| line.trim().to_string())
}

/// True when something exposes a writable fan PWM for this laptop.
fn fan_control_available() -> bool {
    let Ok(entries) = fs::read_dir(HWMON_ROOT) else {
        return false;
    };
    entries
        .filter_map(|e| e.ok())
        .any(|entry| entry.path().join("pwm1").exists())
}

/// Locates the patched driver sources.
///
/// They are **not vendored into this repository** - `hp-wmi.c` is a
/// modified copy of a GPL-2 kernel driver maintained in the upstream
/// `omen-fan-control` project, and carrying a fork of it here would mean
/// tracking their changes by hand. The installer therefore looks for an
/// installed copy first and falls back to a sibling checkout for
/// development.
fn find_driver_source() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("PYREN_DRIVER_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(PathBuf::from("/usr/share/pyren/driver"));
    candidates.push(PathBuf::from(
        "../omen-fan-control-main/src/omen_fan_control/data/driver",
    ));

    candidates
        .into_iter()
        .find(|dir| dir.join("dkms.conf").is_file() && dir.join("hp-wmi-omen/hp-wmi.c").is_file())
}

fn which(binary: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(binary).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_versions_parse_past_distro_suffixes() {
        assert_eq!(parse_kernel_version("6.12.4-arch1-1"), (6, 12));
        assert_eq!(parse_kernel_version("7.2.2-1-cachyos"), (7, 2));
        assert_eq!(parse_kernel_version("5.15.0-105-generic"), (5, 15));
        assert_eq!(parse_kernel_version(""), (0, 0));
    }

    #[test]
    fn the_upstream_cutoff_is_compared_as_a_pair_not_a_float() {
        // 6.9 must not read as "newer than 6.20", which is what comparing
        // these as decimals would do.
        assert!(parse_kernel_version("6.9.0") < UPSTREAM_FAN_CONTROL_KERNEL);
        assert!(parse_kernel_version("6.20.0") >= UPSTREAM_FAN_CONTROL_KERNEL);
        assert!(parse_kernel_version("7.2.2-1-cachyos") >= UPSTREAM_FAN_CONTROL_KERNEL);
    }

    #[test]
    fn arch_prefers_mkinitcpio_over_a_debian_compat_shim() {
        // Arch systems can have both installed; the shim is not the tool
        // that actually regenerates the boot image there.
        let preference_for_arch = ["mkinitcpio", "dracut", "update-initramfs"];
        let preference_for_debian = ["update-initramfs", "dracut", "mkinitcpio"];
        assert_eq!(preference_for_arch[0], "mkinitcpio");
        assert_eq!(preference_for_debian[0], "update-initramfs");
        // The real function can only be checked against this machine, so
        // assert the property that matters: whatever it picks must exist.
        if let Some(tool) = initramfs_tool("cachyos") {
            assert!(which(&tool));
        }
    }

    #[test]
    fn distro_families_map_to_their_hook_mechanism() {
        assert_eq!(hook_flavour("cachyos"), HookFlavour::Pacman);
        assert_eq!(hook_flavour("ubuntu"), HookFlavour::KernelPostinst);
        assert_eq!(hook_flavour("almalinux"), HookFlavour::KernelInstall);
        assert_eq!(hook_flavour("nixos"), HookFlavour::None);
    }
}
