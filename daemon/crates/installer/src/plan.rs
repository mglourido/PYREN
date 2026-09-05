//! Turning "install the driver" into an explicit, inspectable list of steps.
//!
//! Planning is separated from execution on purpose. Installing this driver
//! means unloading a kernel module, replacing a file under `/lib/modules`
//! and regenerating the initramfs; a user deserves to see exactly what is
//! about to run before authorising it, and a plan that can be rendered is
//! also a plan that can be reviewed in a bug report.
//!
//! It also means the interesting logic - what to do, in what order, and
//! when to refuse - is a pure function over [`Environment`], testable
//! anywhere, including on a machine that has nothing to do with HP.

use pyren_core::{msg, Msg};
use serde::{Deserialize, Serialize};

use crate::detect::{Environment, HookFlavour};

/// DKMS package identity, matching the source project's `dkms.conf`.
pub const DKMS_NAME: &str = "hp-wmi-omen";
/// Where the measured ceilings are written for `modprobe` to pick up. In
/// `/etc` rather than `/usr/lib` because it is this machine's measurement,
/// not the package's opinion.
pub const MODPROBE_CONF_PATH: &str = "/etc/modprobe.d/pyren-hp-wmi.conf";
pub const DKMS_VERSION: &str = "1.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    InstallDriver,
    RestoreDriver,
    InstallService,
    RemoveService,
    /// Hand the driver a measured fan ceiling, without rebuilding it.
    ///
    /// This is what makes a calibration reach the hardware at all. The
    /// ceilings the installer patches into the source are the driver's
    /// last fallback - a board whose firmware answers the fan-table query
    /// overwrites them during probe - and they can only be changed by
    /// compiling the module again. The module parameters can be changed by
    /// writing a file and reloading, they outrank the firmware, and they
    /// survive both a DKMS rebuild and a kernel upgrade.
    PinFanCeiling,
}

/// How a permanent install keeps the module across kernel upgrades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Strategy {
    /// DKMS rebuilds the module itself on every kernel upgrade.
    Dkms,
    /// Build once now, plus a distro kernel hook to rebuild later.
    Hooks,
}

/// One unit of work. `command` is what would actually run; internal steps
/// (patching source, copying the tree) carry an empty command and are
/// performed by the daemon itself.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub id: String,
    /// Translatable - render with `tm()`.
    pub description: Msg,
    pub command: Vec<String>,
    /// Failure is tolerated and reported, rather than aborting the run.
    pub optional: bool,
}

impl Step {
    fn command(id: &str, description: impl Into<Msg>, command: &[&str]) -> Self {
        Self {
            id: id.to_string(),
            description: description.into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            optional: false,
        }
    }

    fn internal(id: &str, description: impl Into<Msg>) -> Self {
        Self {
            id: id.to_string(),
            description: description.into(),
            command: Vec::new(),
            optional: false,
        }
    }

    fn optional(mut self) -> Self {
        self.optional = true;
        self
    }
}

/// Something that must be fixed before the plan can run at all.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Blocker {
    pub id: String,
    /// Translatable - render with `tm()`.
    pub message: Msg,
    /// A command the user can run to resolve it, where one exists. A shell
    /// command, quoted verbatim - not translated.
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub action: Action,
    pub strategy: Option<Strategy>,
    pub steps: Vec<Step>,
    pub blockers: Vec<Blocker>,
    /// Translatable - render each with `tm()`.
    pub warnings: Vec<Msg>,
    /// True when the daemon must be running as root to carry this out.
    pub needs_root: bool,
}

impl Plan {
    pub fn is_runnable(&self) -> bool {
        self.blockers.is_empty() && !self.steps.is_empty()
    }
}

/// Options a caller can use to override the defaults.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlanOptions {
    /// Force the hook strategy even when DKMS is available.
    pub prefer_hooks: bool,
    /// Proceed on hardware that isn't a recognised HP gaming laptop, or
    /// where the driver appears unnecessary. Never set by default.
    pub force: bool,
}

pub fn plan(env: &Environment, action: Action, options: PlanOptions) -> Plan {
    match action {
        Action::InstallDriver => plan_install_driver(env, options),
        Action::PinFanCeiling => plan_pin_fan_ceiling(env),
        Action::RestoreDriver => plan_restore_driver(env),
        Action::InstallService => plan_install_service(env),
        Action::RemoveService => plan_remove_service(env),
    }
}

fn plan_install_driver(env: &Environment, options: PlanOptions) -> Plan {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    // The most useful thing this installer can do on a modern kernel is
    // say that it isn't needed. Manual fan control went upstream in 6.20,
    // and replacing the stock driver on such a kernel is a downgrade.
    if env.fan_control_available && env.patched_driver_installed {
        // Working *because of a previous install*. Calling that "the
        // patched driver would add nothing" describes the thing doing the
        // work, and blocking it would refuse the rebuild a kernel upgrade
        // needs - so this warns and lets the reinstall through.
        warnings.push(msg!(
            "installer.warn.alreadyInstalled",
            "Fan control works here because the patched driver is already installed. \
             Reinstalling rebuilds and reinstates it, which is what a kernel upgrade needs \
             if no hook rebuilt it automatically. To go back to the distribution's own \
             driver, use \"restore\" instead."
        ));
    } else if env.fan_control_available {
        warnings.push(msg!(
            "installer.warn.alreadyWorks",
            "Fan control already works on this machine (pwm1 is present) and it is not this \
             installer's doing, so the stock driver supports this board and the patched one \
             would add nothing."
        ));
        if !options.force {
            blockers.push(Blocker {
                id: "already-working".to_string(),
                message: msg!(
                    "installer.blocker.alreadyWorking",
                    "Fan control is already available; nothing to install."
                ),
                fix: None,
            });
        }
    } else if env.kernel.has_upstream_fan_control {
        // Reaching here means the kernel *has* manual fan control and this
        // machine still has no `pwm1`. That is not "the patch is probably
        // unnecessary" - it is the one case where it is necessary, and
        // saying otherwise contradicts the verdict shown above it.
        //
        // Which of the two it is depends on whether the driver is even
        // loaded: a bound hp-wmi that produced no pwm1 did not recognise
        // this board (see `dev/FINDINGS.md` - on the test laptop the driver
        // logs none of the lines it emits when it finds fan hardware, so it
        // never enters the code path at all).
        if env.hp_wmi_loaded {
            warnings.push(msg!(
                "installer.warn.boardMissing",
                {
                    "kernel" => env.kernel.release.clone(),
                    "major" => crate::detect::UPSTREAM_FAN_CONTROL_KERNEL.0,
                    "minor" => crate::detect::UPSTREAM_FAN_CONTROL_KERNEL.1,
                },
                "Kernel {kernel} ships manual fan control upstream (since {major}.{minor}), \
                 and hp-wmi is loaded here - yet there is no pwm1. The stock driver did not \
                 recognise this board, which is exactly the case the patched driver plus \
                 your board's id in the right table exists for. Whether the firmware then \
                 honours a fan write is something only trying it can answer."
            ));
        } else {
            warnings.push(msg!(
                "installer.warn.noHpWmi",
                {
                    "kernel" => env.kernel.release.clone(),
                    "major" => crate::detect::UPSTREAM_FAN_CONTROL_KERNEL.0,
                    "minor" => crate::detect::UPSTREAM_FAN_CONTROL_KERNEL.1,
                },
                "Kernel {kernel} ships manual fan control upstream (since {major}.{minor}), \
                 but hp-wmi is not loaded at all here, so there is nothing to say this is one \
                 of the laptops either driver is for. Installing is unlikely to help."
            ));
        }
    }

    let Some(driver_source) = env.driver_source.clone() else {
        blockers.push(Blocker {
            id: "no-driver-source".to_string(),
            message: msg!(
                "installer.blocker.noDriverSource",
                "The patched hp-wmi sources were not found. Pyren ships them in its own \
                 driver/ directory; a packaged build has to install that to \
                 /usr/share/pyren/driver, or PYREN_DRIVER_DIR has to point at a copy."
            ),
            fix: None,
        });
        return Plan {
            action: Action::InstallDriver,
            strategy: None,
            steps: Vec::new(),
            blockers,
            warnings,
            needs_root: true,
        };
    };

    if !env.headers.usable {
        blockers.push(Blocker {
            id: "kernel-headers".to_string(),
            message: msg!(
                "installer.blocker.kernelHeaders",
                "Kernel headers for the running kernel are missing or incomplete, so the \
                 module cannot be compiled."
            ),
            fix: env.headers.fix_hint.clone(),
        });
    }
    if !env.has_make || !env.has_compiler {
        blockers.push(Blocker {
            id: "build-tools".to_string(),
            message: msg!(
                "installer.blocker.buildTools",
                "A C compiler and make are required to build the module."
            ),
            fix: Some("install your distribution's base development tools".to_string()),
        });
    }

    let strategy = if options.prefer_hooks || !env.has_dkms {
        Strategy::Hooks
    } else {
        Strategy::Dkms
    };

    if strategy == Strategy::Hooks && env.hook_flavour == HookFlavour::None {
        warnings.push(msg!(
            "installer.warn.noHook",
            "This distribution has no recognised kernel-hook mechanism, so the module will \
             be built for the running kernel only and will need reinstalling after a kernel \
             upgrade."
        ));
    }

    let dkms_src = format!("/usr/src/{DKMS_NAME}-{DKMS_VERSION}");
    // Staging comes first so that everything after it works on the copy
    // under /usr/src: the tree the sources are read from - this repository's
    // `driver/`, or an installed /usr/share/pyren/driver - is never written
    // to, so it stays a pristine snapshot of upstream and a second install
    // never starts from the first one's output.
    let mut steps = vec![
        Step::internal(
            "stage-source",
            msg!(
                "installer.step.stage-source",
                { "path" => dkms_src.clone() },
                "Copy the driver sources and dkms.conf to {path}"
            ),
        ),
        Step::internal(
            "patch-source",
            msg!(
                "installer.step.patch-source",
                { "path" => dkms_src.clone() },
                "Patch the staged source under {path} (fan ceilings, and any experimental board id)"
            ),
        ),
        Step::internal(
            "backup-driver",
            msg!(
                "installer.step.backup-driver",
                "Back up the stock hp-wmi.ko next to itself as .bak, then remove it so \
                 depmod picks the new one unambiguously"
            ),
        ),
    ];

    // A machine that was installed one way and is being installed the
    // other keeps working either way - both mechanisms rebuild from the
    // same patched source - but it ends up with two things claiming the
    // module across a kernel upgrade, in two different directories. That
    // is how the stock backup got overwritten with a patched module once
    // already, so the strategy being left behind is retired here.
    match strategy {
        Strategy::Dkms if env.hook_installed => steps.push(
            Step::internal(
                "remove-hooks",
                msg!(
                    "installer.step.remove-hooks-switch",
                    "Delete the kernel-upgrade hook from the previous install; DKMS rebuilds \
                     the module from now on"
                ),
            )
            .optional(),
        ),
        Strategy::Hooks if env.dkms_installed => steps.push(
            Step::command(
                "dkms-remove-old",
                msg!(
                    "installer.step.dkms-remove-switch",
                    "Deregister the DKMS module from the previous install; the kernel hook \
                     rebuilds the module from now on"
                ),
                &["dkms", "remove", &format!("{DKMS_NAME}/{DKMS_VERSION}"), "--all"],
            )
            .optional(),
        ),
        _ => {}
    }

    match strategy {
        Strategy::Dkms => {
            if env.dkms_installed {
                steps.push(
                    Step::command(
                        "dkms-remove-old",
                        msg!(
                            "installer.step.dkms-remove-old",
                            "Remove the previously registered DKMS module"
                        ),
                        &["dkms", "remove", &format!("{DKMS_NAME}/{DKMS_VERSION}"), "--all"],
                    )
                    .optional(),
                );
            }
            steps.push(Step::command(
                "dkms-add",
                msg!("installer.step.dkms-add", "Register the module with DKMS"),
                &["dkms", "add", "-m", DKMS_NAME, "-v", DKMS_VERSION],
            ));
            steps.push(Step::command(
                "dkms-build",
                msg!("installer.step.dkms-build", "Build the module"),
                &["dkms", "build", "-m", DKMS_NAME, "-v", DKMS_VERSION],
            ));
            steps.push(Step::command(
                "dkms-install",
                msg!("installer.step.dkms-install", "Install the built module"),
                &["dkms", "install", "-m", DKMS_NAME, "-v", DKMS_VERSION],
            ));
        }
        Strategy::Hooks => {
            steps.push(Step::command(
                "make",
                msg!("installer.step.make", "Build the module for the running kernel"),
                &["make", "-C", &format!("{dkms_src}/src/hp-wmi-omen")],
            ));
            steps.push(Step::internal(
                "install-module",
                msg!(
                    "installer.step.install-module",
                    "Install hp-wmi.ko into /lib/modules/<kernel>/kernel/drivers/platform/x86/hp"
                ),
            ));
            if let Some(hook) = hook_step(env.hook_flavour) {
                steps.push(hook);
            }
            steps.push(
                Step::command(
                    "make-clean",
                    msg!("installer.step.make-clean", "Clean the build tree"),
                    &["make", "-C", &format!("{dkms_src}/src/hp-wmi-omen"), "clean"],
                )
                .optional(),
            );
        }
    }

    steps.push(Step::command(
        "depmod",
        msg!("installer.step.depmod", "Rebuild module dependencies"),
        &["depmod", "-a"],
    ));
    steps.push(
        Step::command(
            "modprobe-remove",
            msg!("installer.step.modprobe-remove", "Unload the current hp-wmi"),
            &["modprobe", "-r", "hp-wmi"],
        )
        .optional(),
    );
    steps.push(Step::command(
        "modprobe",
        msg!("installer.step.modprobe-patched", "Load the patched hp-wmi"),
        &["modprobe", "hp-wmi"],
    ));

    if let Some(step) = initramfs_step(env) {
        steps.push(step);
    }

    let _ = driver_source;
    Plan {
        action: Action::InstallDriver,
        strategy: Some(strategy),
        steps,
        blockers,
        warnings,
        needs_root: true,
    }
}

fn hook_step(flavour: HookFlavour) -> Option<Step> {
    let (id, description) = match flavour {
        HookFlavour::Pacman => (
            "install-hook-pacman",
            msg!(
                "installer.step.install-hook-pacman",
                "Install the pacman hook that rebuilds the module on kernel upgrades"
            ),
        ),
        HookFlavour::KernelPostinst => (
            "install-hook-postinst",
            msg!(
                "installer.step.install-hook-postinst",
                "Install the /etc/kernel/postinst.d hook that rebuilds on kernel upgrades"
            ),
        ),
        HookFlavour::KernelInstall => (
            "install-hook-kernel-install",
            msg!(
                "installer.step.install-hook-kernel-install",
                "Install the /etc/kernel/install.d hook that rebuilds on kernel upgrades"
            ),
        ),
        HookFlavour::None => return None,
    };
    Some(Step::internal(id, description))
}

fn initramfs_step(env: &Environment) -> Option<Step> {
    let tool = env.initramfs_tool.as_deref()?;
    let command: Vec<&str> = match tool {
        "update-initramfs" => vec!["update-initramfs", "-u"],
        "mkinitcpio" => vec!["mkinitcpio", "-P"],
        "dracut" => vec!["dracut", "--force"],
        _ => return None,
    };
    // Known to fail on some EFI layouts without breaking the install, so
    // it must not abort the run.
    Some(
        Step::command(
            "initramfs",
            msg!("installer.step.initramfs", "Regenerate the initramfs"),
            &command,
        )
        .optional(),
    )
}

/// Writing the measurement where the driver will read it, and reloading
/// so it does. Deliberately small: no compiler, no DKMS, no initramfs - if
/// this needed any of those it could not be run after every calibration,
/// which is the point of it existing.
fn plan_pin_fan_ceiling(env: &Environment) -> Plan {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    if !env.driver_accepts_measured_rpm {
        blockers.push(Blocker {
            id: "driver-has-no-parameter".to_string(),
            message: msg!(
                "installer.blocker.noMeasuredRpmParam",
                "The hp-wmi loaded here has no measured-ceiling parameter, so there is \
                 nowhere to put the measurement. It was built before this existed; \
                 installing the driver again produces one that has it."
            ),
            fix: None,
        });
    }

    // Losing hp-wmi for a moment takes fan control with it, along with the
    // hotkeys and the firmware profile. Short, but not nothing, and a user
    // who is about to be asked to confirm deserves to know.
    warnings.push(msg!(
        "installer.warn.reloadDropsFanControl",
        "hp-wmi is unloaded and loaded again, so fan control, the laptop's hotkeys and \
         the firmware profile are gone for a moment. The fans keep running on the \
         firmware's own curve while they are."
    ));

    let steps = vec![
        Step::internal(
            "write-modprobe-conf",
            msg!(
                "installer.step.write-modprobe-conf",
                { "path" => MODPROBE_CONF_PATH },
                "Write the measured ceilings to {path}, so every later load uses them too"
            ),
        ),
        Step::command(
            "modprobe-remove",
            msg!("installer.step.modprobe-remove-plain", "Unload hp-wmi"),
            &["modprobe", "-r", "hp-wmi"],
        )
        .optional(),
        Step::command(
            "modprobe",
            msg!(
                "installer.step.modprobe-measured",
                "Load hp-wmi again, this time with the measured ceilings"
            ),
            &["modprobe", "hp-wmi"],
        ),
    ];

    Plan {
        action: Action::PinFanCeiling,
        strategy: None,
        steps,
        blockers,
        warnings,
        needs_root: true,
    }
}

fn plan_restore_driver(env: &Environment) -> Plan {
    let mut steps = Vec::new();

    if env.dkms_installed {
        steps.push(
            Step::command(
                "dkms-remove",
                msg!("installer.step.dkms-remove", "Deregister the DKMS module"),
                &["dkms", "remove", &format!("{DKMS_NAME}/{DKMS_VERSION}"), "--all"],
            )
            .optional(),
        );
    }
    steps.push(Step::internal(
        "remove-sources",
        msg!(
            "installer.step.remove-sources",
            { "name" => DKMS_NAME, "version" => DKMS_VERSION },
            "Delete /usr/src/{name}-{version}"
        ),
    ));
    steps.push(Step::internal(
        "remove-hooks",
        msg!("installer.step.remove-hooks", "Delete any installed kernel-upgrade hook"),
    ));
    // Not tidiness: modprobe refuses a module given a parameter it does
    // not have, and the distribution's own hp-wmi has neither of these.
    // Left behind, this file stops the restored driver from loading at
    // all - a "restore" that takes fan control away for good.
    steps.push(Step::internal(
        "remove-modprobe-conf",
        msg!(
            "installer.step.remove-modprobe-conf",
            { "path" => MODPROBE_CONF_PATH },
            "Delete {path}; the stock driver has no measured-ceiling parameter and refuses \
             to load when given one"
        ),
    ));
    steps.push(Step::internal(
        "restore-backups",
        msg!(
            "installer.step.restore-backups",
            "Rename every hp-wmi.ko.bak back to hp-wmi.ko, restoring the stock driver"
        ),
    ));
    steps.push(Step::command(
        "depmod",
        msg!("installer.step.depmod", "Rebuild module dependencies"),
        &["depmod", "-a"],
    ));
    steps.push(
        Step::command(
            "modprobe-remove",
            msg!("installer.step.modprobe-remove-plain", "Unload hp-wmi"),
            &["modprobe", "-r", "hp-wmi"],
        )
        .optional(),
    );
    steps.push(Step::command(
        "modprobe",
        msg!("installer.step.modprobe-stock", "Load the stock hp-wmi"),
        &["modprobe", "hp-wmi"],
    ));
    if let Some(step) = initramfs_step(env) {
        steps.push(step);
    }

    Plan {
        action: Action::RestoreDriver,
        strategy: None,
        steps,
        blockers: Vec::new(),
        warnings: Vec::new(),
        needs_root: true,
    }
}

fn plan_install_service(env: &Environment) -> Plan {
    let mut warnings = Vec::new();
    if env.service_installed {
        warnings.push(msg!(
            "installer.warn.serviceInstalled",
            "The service unit is already installed; it will be replaced."
        ));
    }

    // The daemon is root and its socket is the trust boundary, so the socket
    // is handed to a group rather than left open to every local user. That
    // group has to exist before the daemon first binds, or nobody but root
    // can connect. See `pyren_core::socket`.
    let group = pyren_core::socket_group();
    warnings.push(msg!(
        "installer.warn.joinGroup",
        { "group" => group.clone() },
        "Add each user who should control this machine to the '{group}' group: \
         sudo usermod -aG {group} $USER (then log out and back in)."
    ));

    Plan {
        action: Action::InstallService,
        strategy: None,
        steps: vec![
            Step::command(
                "create-group",
                msg!(
                    "installer.step.create-group",
                    { "group" => group.clone() },
                    "Create the '{group}' group that may reach the daemon"
                ),
                &["groupadd", "-f", &group],
            ),
            Step::internal(
                "write-unit",
                msg!(
                    "installer.step.write-unit",
                    "Write /etc/systemd/system/pyren-daemon.service"
                ),
            ),
            Step::command(
                "daemon-reload",
                msg!("installer.step.daemon-reload", "Reload systemd units"),
                &["systemctl", "daemon-reload"],
            ),
            Step::command(
                "enable",
                msg!("installer.step.enable", "Enable and start the daemon at boot"),
                &["systemctl", "enable", "--now", "pyren-daemon.service"],
            ),
        ],
        blockers: Vec::new(),
        warnings,
        needs_root: true,
    }
}

fn plan_remove_service(_env: &Environment) -> Plan {
    Plan {
        action: Action::RemoveService,
        strategy: None,
        steps: vec![
            Step::command(
                "disable",
                msg!("installer.step.disable", "Stop the daemon and remove it from boot"),
                &["systemctl", "disable", "--now", "pyren-daemon.service"],
            )
            .optional(),
            Step::internal(
                "remove-unit",
                msg!("installer.step.remove-unit", "Delete the systemd unit file"),
            ),
            Step::command(
                "daemon-reload",
                msg!("installer.step.daemon-reload", "Reload systemd units"),
                &["systemctl", "daemon-reload"],
            ),
        ],
        blockers: Vec::new(),
        warnings: Vec::new(),
        needs_root: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{HeadersInfo, KernelInfo};
    use std::path::PathBuf;

    /// A machine where everything the installer needs is present, and fan
    /// control is *not* yet working - i.e. the case installing makes sense.
    fn ready_env() -> Environment {
        Environment {
            kernel: KernelInfo {
                release: "6.12.4-arch1-1".into(),
                major: 6,
                minor: 12,
                has_upstream_fan_control: false,
            },
            distro_id: "arch".into(),
            hook_flavour: HookFlavour::Pacman,
            hook_installed: false,
            driver_accepts_measured_rpm: true,
            headers: HeadersInfo {
                build_dir: Some(PathBuf::from("/lib/modules/6.12.4-arch1-1/build")),
                has_autoconf: true,
                has_kbuild_scripts: true,
                usable: true,
                fix_hint: None,
            },
            has_dkms: true,
            dkms_installed: false,
            dkms_status: None,
            has_make: true,
            has_compiler: true,
            initramfs_tool: Some("mkinitcpio".into()),
            fan_control_available: false,
            hp_wmi_loaded: true,
            acpi_call_available: false,
            driver_source: Some(PathBuf::from("/usr/share/pyren/driver")),
            service_installed: false,
            patched_driver_installed: false,
        }
    }

    fn ids(plan: &Plan) -> Vec<&str> {
        plan.steps.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn a_ready_machine_gets_a_dkms_plan() {
        let plan = plan(&ready_env(), Action::InstallDriver, PlanOptions::default());
        assert!(plan.is_runnable());
        assert_eq!(plan.strategy, Some(Strategy::Dkms));
        assert!(ids(&plan).contains(&"dkms-build"));
        assert!(!ids(&plan).contains(&"make"));
    }

    /// The wizard shows the plan in the user's language: each step's
    /// description carries a catalog key alongside its English text.
    #[test]
    fn steps_and_warnings_are_translatable() {
        let env = Environment { kernel: KernelInfo { has_upstream_fan_control: true, ..ready_env().kernel }, ..ready_env() };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());

        let stage = plan.steps.iter().find(|s| s.id == "stage-source").unwrap();
        assert_eq!(stage.description.key, "installer.step.stage-source");
        assert!(stage.description.text.contains("dkms.conf"));

        assert!(plan.warnings.iter().any(|w| w.key == "installer.warn.boardMissing"
            && w.params["kernel"] == "6.12.4-arch1-1"));
    }

    #[test]
    fn without_dkms_the_plan_builds_and_installs_a_hook() {
        let env = Environment { has_dkms: false, ..ready_env() };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        assert_eq!(plan.strategy, Some(Strategy::Hooks));
        assert!(ids(&plan).contains(&"make"));
        assert!(ids(&plan).contains(&"install-hook-pacman"));
    }

    #[test]
    fn hooks_can_be_forced_even_when_dkms_exists() {
        let options = PlanOptions { prefer_hooks: true, ..PlanOptions::default() };
        let plan = plan(&ready_env(), Action::InstallDriver, options);
        assert_eq!(plan.strategy, Some(Strategy::Hooks));
    }

    #[test]
    fn an_unknown_distro_warns_that_the_module_will_not_survive_upgrades() {
        let env = Environment {
            has_dkms: false,
            hook_flavour: HookFlavour::None,
            ..ready_env()
        };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        assert!(plan.is_runnable());
        assert!(!ids(&plan).iter().any(|id| id.starts_with("install-hook")));
        assert!(plan.warnings.iter().any(|w| w.contains("kernel upgrade")));
    }

    #[test]
    fn a_machine_where_fan_control_already_works_is_refused() {
        let env = Environment { fan_control_available: true, ..ready_env() };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        assert!(!plan.is_runnable());
        assert!(plan.blockers.iter().any(|b| b.id == "already-working"));
    }

    /// Reinstalling on top of a previous install is a legitimate thing to
    /// want - it is how the module comes back after a kernel upgrade that
    /// no hook covered. It must not be refused as "already working", and
    /// must not be described as adding nothing.
    #[test]
    fn reinstalling_over_our_own_driver_is_allowed_and_says_why() {
        let env = Environment {
            fan_control_available: true,
            patched_driver_installed: true,
            ..ready_env()
        };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());

        assert!(plan.is_runnable(), "a rebuild must not need --force");
        assert!(plan.warnings.iter().any(|w| w.key == "installer.warn.alreadyInstalled"));
        assert!(
            !plan.warnings.iter().any(|w| w.contains("would add nothing")),
            "the patched driver is what is doing the work here"
        );
    }

    #[test]
    fn that_refusal_can_be_overridden_deliberately() {
        let env = Environment { fan_control_available: true, ..ready_env() };
        let options = PlanOptions { force: true, ..PlanOptions::default() };
        let plan = plan(&env, Action::InstallDriver, options);
        assert!(plan.is_runnable());
        // ...but the reason it was questionable is still stated.
        assert!(plan.warnings.iter().any(|w| w.contains("already works")));
    }

    /// The test laptop, and the case the whole driver path exists for: a
    /// kernel new enough to have fan control of its own, hp-wmi bound, and
    /// still no `pwm1` - meaning the stock driver did not recognise the
    /// board. The warning must say that, not "probably unnecessary": the
    /// panel's own verdict one line above already says the patch is the
    /// remedy here, and two sentences disagreeing is worse than either.
    #[test]
    fn a_modern_kernel_with_no_pwm1_says_the_board_is_the_reason() {
        let env = Environment {
            kernel: KernelInfo {
                release: "7.2.2-1-cachyos".into(),
                major: 7,
                minor: 2,
                has_upstream_fan_control: true,
            },
            hp_wmi_loaded: true,
            fan_control_available: false,
            ..ready_env()
        };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        assert!(plan.is_runnable());
        let warning = plan
            .warnings
            .iter()
            .find(|w| w.key == "installer.warn.boardMissing")
            .expect("the board-missing warning");
        assert!(warning.contains("did not recognise this board"));
        assert!(
            !plan.warnings.iter().any(|w| w.contains("probably unnecessary")),
            "nothing here should call the patch unnecessary"
        );
    }

    /// Without hp-wmi bound there is no evidence this is one of these
    /// laptops at all, and that is a different sentence.
    #[test]
    fn a_machine_with_no_hp_wmi_is_told_the_patch_probably_will_not_help() {
        let env = Environment {
            kernel: KernelInfo { has_upstream_fan_control: true, ..ready_env().kernel },
            hp_wmi_loaded: false,
            ..ready_env()
        };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        assert!(plan.warnings.iter().any(|w| w.key == "installer.warn.noHpWmi"));
    }

    #[test]
    fn missing_driver_sources_block_with_no_steps_at_all() {
        let env = Environment { driver_source: None, ..ready_env() };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        assert!(!plan.is_runnable());
        assert!(plan.steps.is_empty());
        assert!(plan.blockers.iter().any(|b| b.id == "no-driver-source"));
    }

    #[test]
    fn incomplete_headers_block_and_carry_the_fix_command() {
        let env = Environment {
            headers: HeadersInfo {
                usable: false,
                fix_hint: Some("sudo apt install linux-kbuild-6.12".into()),
                ..ready_env().headers
            },
            ..ready_env()
        };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        assert!(!plan.is_runnable());
        let blocker = plan.blockers.iter().find(|b| b.id == "kernel-headers").unwrap();
        assert!(blocker.fix.as_deref().unwrap().contains("linux-kbuild"));
    }

    /// Patching writes to `hp-wmi.c`, so it must happen after the copy into
    /// /usr/src - otherwise it would edit the read-only source tree the
    /// snapshot lives in, and the next install would start from a patched
    /// file rather than from upstream's.
    #[test]
    fn the_sources_are_staged_before_they_are_patched() {
        let plan = plan(&ready_env(), Action::InstallDriver, PlanOptions::default());
        let steps = ids(&plan);
        assert!(
            steps.iter().position(|id| *id == "stage-source")
                < steps.iter().position(|id| *id == "patch-source")
        );
    }

    #[test]
    fn the_stock_driver_is_backed_up_before_anything_replaces_it() {
        let plan = plan(&ready_env(), Action::InstallDriver, PlanOptions::default());
        let steps = ids(&plan);
        let backup = steps.iter().position(|id| *id == "backup-driver").unwrap();
        let install = steps.iter().position(|id| *id == "dkms-install").unwrap();
        assert!(backup < install, "the backup must happen before the install");
    }

    #[test]
    fn an_existing_dkms_registration_is_removed_first() {
        let env = Environment { dkms_installed: true, ..ready_env() };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        let steps = ids(&plan);
        assert!(steps.contains(&"dkms-remove-old"));
        assert!(
            steps.iter().position(|id| *id == "dkms-remove-old")
                < steps.iter().position(|id| *id == "dkms-add")
        );
    }

    /// Switching from the hook strategy to DKMS has to retire the hook.
    /// Left in place, both rebuild the module on a kernel upgrade, into
    /// two different directories - which is how a patched module ended up
    /// recorded as the distribution's own backup once already.
    #[test]
    fn moving_to_dkms_retires_the_hook_the_last_install_left() {
        let env = Environment { hook_installed: true, ..ready_env() };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        let steps = ids(&plan);

        assert_eq!(plan.strategy, Some(Strategy::Dkms));
        assert!(steps.contains(&"remove-hooks"));
        assert!(
            steps.iter().position(|id| *id == "remove-hooks")
                < steps.iter().position(|id| *id == "dkms-add")
        );
        // Failing to delete a hook must not abort an otherwise good
        // install: the module is built and loaded either way.
        assert!(plan.steps.iter().find(|s| s.id == "remove-hooks").unwrap().optional);
    }

    /// And the same in the other direction.
    #[test]
    fn moving_to_hooks_deregisters_the_dkms_module() {
        let env = Environment { dkms_installed: true, ..ready_env() };
        let options = PlanOptions { prefer_hooks: true, ..PlanOptions::default() };
        let plan = plan(&env, Action::InstallDriver, options);
        let steps = ids(&plan);

        assert_eq!(plan.strategy, Some(Strategy::Hooks));
        assert!(steps.contains(&"dkms-remove-old"));
        assert!(
            steps.iter().position(|id| *id == "dkms-remove-old")
                < steps.iter().position(|id| *id == "make")
        );
        assert!(plan.steps.iter().find(|s| s.id == "dkms-remove-old").unwrap().optional);
    }

    /// A machine with neither leftover gets neither step - the plan says
    /// what will run, and a no-op step in it is a lie about the machine.
    #[test]
    fn a_machine_with_nothing_to_retire_gets_no_retirement_steps() {
        let plan = plan(&ready_env(), Action::InstallDriver, PlanOptions::default());
        let steps = ids(&plan);
        assert!(!steps.contains(&"remove-hooks"));
        assert!(!steps.contains(&"dkms-remove-old"));
    }

    /// The whole reason this action exists: a measurement reaches the
    /// driver without a compiler, DKMS or an initramfs anywhere in it.
    #[test]
    fn pinning_a_measured_ceiling_needs_no_rebuild() {
        let plan = plan(&ready_env(), Action::PinFanCeiling, PlanOptions::default());
        let steps = ids(&plan);

        assert!(plan.blockers.is_empty());
        assert_eq!(steps, vec!["write-modprobe-conf", "modprobe-remove", "modprobe"]);
        for absent in ["dkms-build", "make", "initramfs", "depmod"] {
            assert!(!steps.contains(&absent), "{absent} would make this too expensive to run");
        }
        assert!(plan.needs_root);
    }

    /// A driver built before the parameter existed has nowhere to put the
    /// measurement, and saying "reinstall" is more use than writing a file
    /// that nothing will read.
    #[test]
    fn pinning_is_blocked_when_the_driver_has_no_parameter_for_it() {
        let env = Environment { driver_accepts_measured_rpm: false, ..ready_env() };
        let plan = plan(&env, Action::PinFanCeiling, PlanOptions::default());
        assert!(!plan.is_runnable());
        assert_eq!(plan.blockers[0].id, "driver-has-no-parameter");
    }

    /// Left behind, the file stops the stock driver from loading at all:
    /// modprobe refuses a parameter the module does not have.
    #[test]
    fn restoring_takes_the_modprobe_conf_away() {
        let plan = plan(&ready_env(), Action::RestoreDriver, PlanOptions::default());
        let steps = ids(&plan);
        assert!(steps.contains(&"remove-modprobe-conf"));
        assert!(
            steps.iter().position(|id| *id == "remove-modprobe-conf")
                < steps.iter().position(|id| *id == "modprobe"),
            "it has to be gone before the stock driver is loaded"
        );
    }

    #[test]
    fn steps_that_are_known_to_fail_harmlessly_are_optional() {
        let plan = plan(&ready_env(), Action::InstallDriver, PlanOptions::default());
        let optional = |id: &str| plan.steps.iter().find(|s| s.id == id).unwrap().optional;
        // Regenerating the initramfs is known to fail on odd EFI layouts,
        // and unloading a driver that isn't loaded is not a problem.
        assert!(optional("initramfs"));
        assert!(optional("modprobe-remove"));
        assert!(!optional("modprobe"));
    }

    #[test]
    fn restoring_puts_the_backed_up_driver_back() {
        let plan = plan(&ready_env(), Action::RestoreDriver, PlanOptions::default());
        assert!(plan.is_runnable());
        assert!(ids(&plan).contains(&"restore-backups"));
        assert!(ids(&plan).contains(&"remove-hooks"));
    }

    /// Without the group, the daemon binds its socket 0600 and the desktop
    /// user it exists for cannot reach it.
    #[test]
    fn installing_the_service_creates_the_group_that_may_reach_it() {
        let plan = plan(&ready_env(), Action::InstallService, PlanOptions::default());
        let group = pyren_core::socket_group();

        let step = plan.steps.iter().find(|s| s.id == "create-group").expect("group step");
        assert_eq!(step.command, ["groupadd", "-f", &group]);
        assert_eq!(plan.steps[0].id, "create-group", "must exist before the daemon starts");
        assert!(plan.warnings.iter().any(|w| w.contains("usermod -aG")));
    }

    #[test]
    fn every_plan_declares_that_it_needs_root() {
        for action in [
            Action::InstallDriver,
            Action::RestoreDriver,
            Action::InstallService,
            Action::RemoveService,
        ] {
            assert!(plan(&ready_env(), action, PlanOptions::default()).needs_root);
        }
    }

    #[test]
    fn a_machine_with_no_initramfs_tool_simply_skips_that_step() {
        let env = Environment { initramfs_tool: None, ..ready_env() };
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        assert!(!ids(&plan).contains(&"initramfs"));
    }
}
