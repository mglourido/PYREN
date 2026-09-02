//! Carrying out a [`Plan`].
//!
//! Steps with a `command` are spawned; the rest are internal actions
//! implemented here (patching, staging sources, backing up the stock
//! module, installing hooks and the systemd unit).
//!
//! Two deliberate safety properties:
//!
//! - **Dry run is the default.** Callers must ask for a real run
//!   explicitly, so a mis-sent IPC message cannot replace a kernel module.
//! - **The stock driver is always backed up before being removed**, and
//!   only if no backup exists yet - re-running an install must never
//!   overwrite the *original* backup with an already-patched module.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::detect::{Environment, HookFlavour};
use crate::patch::{self, BoardTable, MaxRpm};
use crate::plan::{Plan, Step, DKMS_NAME, DKMS_VERSION};

/// Extra inputs an install needs beyond the plan itself.
#[derive(Debug, Clone, Default)]
pub struct ExecuteContext {
    pub max_rpm: MaxRpm,
    pub experimental_board: Option<(BoardTable, String)>,
    /// Path of the pyren-daemon binary, for the systemd unit.
    pub daemon_binary: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepResult {
    pub id: String,
    pub description: String,
    pub status: StepStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StepStatus {
    Ok,
    /// Failed, but the step was optional so the run continued.
    Warned,
    Failed,
    /// Not attempted because an earlier required step failed.
    Skipped,
    /// Dry run: this is what would have happened.
    Planned,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionReport {
    pub dry_run: bool,
    pub succeeded: bool,
    pub results: Vec<StepResult>,
}

pub fn execute(
    plan: &Plan,
    env: &Environment,
    context: &ExecuteContext,
    dry_run: bool,
) -> ExecutionReport {
    let mut results = Vec::new();
    let mut failed = false;

    for step in &plan.steps {
        if failed {
            results.push(result(step, StepStatus::Skipped, "skipped after an earlier failure"));
            continue;
        }
        if dry_run {
            let detail = if step.command.is_empty() {
                "internal action".to_string()
            } else {
                step.command.join(" ")
            };
            results.push(result(step, StepStatus::Planned, &detail));
            continue;
        }

        let outcome = run_step(step, env, context);
        match outcome {
            Ok(detail) => results.push(result(step, StepStatus::Ok, &detail)),
            Err(e) if step.optional => {
                results.push(result(step, StepStatus::Warned, &e));
            }
            Err(e) => {
                results.push(result(step, StepStatus::Failed, &e));
                failed = true;
            }
        }
    }

    ExecutionReport { dry_run, succeeded: !failed, results }
}

fn result(step: &Step, status: StepStatus, detail: &str) -> StepResult {
    StepResult {
        id: step.id.clone(),
        description: step.description.clone(),
        status,
        detail: detail.to_string(),
    }
}

fn run_step(step: &Step, env: &Environment, context: &ExecuteContext) -> Result<String, String> {
    if !step.command.is_empty() {
        return run_command(&step.command);
    }

    match step.id.as_str() {
        "patch-source" => patch_source(env, context),
        "stage-source" => stage_source(env),
        "backup-driver" => backup_stock_driver(&env.kernel.release),
        "install-module" => install_module(&env.kernel.release),
        id if id.starts_with("install-hook") => install_hook(env),
        "remove-sources" => remove_sources(),
        "remove-hooks" => remove_hooks(),
        "restore-backups" => restore_backups(&env.kernel.release),
        "write-unit" => write_service_unit(context),
        "remove-unit" => remove_service_unit(),
        other => Err(format!("no implementation for internal step '{other}'")),
    }
}

fn run_command(command: &[String]) -> Result<String, String> {
    let output = Command::new(&command[0])
        .args(&command[1..])
        .output()
        .map_err(|e| format!("could not run {}: {e}", command[0]))?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(stdout.lines().last().unwrap_or("").to_string());
    }
    // stderr is where kbuild and dkms put anything actionable.
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "{} exited with {}: {}",
        command[0],
        output.status.code().unwrap_or(-1),
        stderr.trim()
    ))
}

fn driver_source(env: &Environment) -> Result<PathBuf, String> {
    env.driver_source
        .clone()
        .ok_or_else(|| "driver sources are not available".to_string())
}

fn patch_source(env: &Environment, context: &ExecuteContext) -> Result<String, String> {
    let dir = driver_source(env)?;
    let board = context
        .experimental_board
        .as_ref()
        .map(|(table, name)| (*table, name.as_str()));

    let applied = patch::patch_driver_tree(&dir, context.max_rpm, board)
        .map_err(|e| e.to_string())?;

    Ok(if applied.is_empty() {
        "no source changes requested".to_string()
    } else {
        applied.join("; ")
    })
}

fn dkms_src_dir() -> PathBuf {
    PathBuf::from(format!("/usr/src/{DKMS_NAME}-{DKMS_VERSION}"))
}

/// Copies the driver tree into `/usr/src`, filling in `dkms.conf`'s
/// placeholders the way the shell installer's `sed` did.
fn stage_source(env: &Environment) -> Result<String, String> {
    let source = driver_source(env)?;
    let dest = dkms_src_dir();

    let _ = fs::remove_dir_all(&dest);
    fs::create_dir_all(dest.join("src")).map_err(|e| e.to_string())?;

    let dkms_conf = fs::read_to_string(source.join("dkms.conf")).map_err(|e| e.to_string())?;
    let dkms_conf = dkms_conf
        .replace("@PKGNAME@", DKMS_NAME)
        .replace("@PKGVER@", DKMS_VERSION);
    fs::write(dest.join("dkms.conf"), dkms_conf).map_err(|e| e.to_string())?;

    fs::copy(source.join("src/Makefile"), dest.join("src/Makefile"))
        .map_err(|e| format!("copying src/Makefile: {e}"))?;
    copy_tree(&source.join("hp-wmi-omen"), &dest.join("src/hp-wmi-omen"))?;

    Ok(format!("staged to {}", dest.display()))
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .map_err(|e| format!("copying {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn module_dirs(kernel_release: &str) -> [PathBuf; 2] {
    [
        PathBuf::from(format!(
            "/lib/modules/{kernel_release}/kernel/drivers/platform/x86/hp"
        )),
        PathBuf::from(format!("/lib/modules/{kernel_release}/updates")),
    ]
}

/// Backs up and removes any stock `hp-wmi.ko`.
///
/// The `.bak` is only written when one doesn't already exist: re-running an
/// install must not replace the pristine backup with an already-patched
/// module, or restoring would never get back to the distro's driver.
fn backup_stock_driver(kernel_release: &str) -> Result<String, String> {
    let mut backed_up = Vec::new();

    for dir in module_dirs(kernel_release) {
        for module in find_modules(&dir, "hp-wmi.ko") {
            if module.to_string_lossy().ends_with(".bak") {
                continue;
            }
            let backup = PathBuf::from(format!("{}.bak", module.display()));
            if !backup.exists() {
                fs::copy(&module, &backup)
                    .map_err(|e| format!("backing up {}: {e}", module.display()))?;
                backed_up.push(backup.display().to_string());
            }
            fs::remove_file(&module)
                .map_err(|e| format!("removing {}: {e}", module.display()))?;
        }
    }

    Ok(if backed_up.is_empty() {
        "no stock driver found to back up".to_string()
    } else {
        format!("backed up {}", backed_up.join(", "))
    })
}

/// Every file under `dir` whose name starts with `name` (so `.ko`, `.ko.xz`
/// and `.ko.zst` are all found, as distros compress modules differently).
fn find_modules(dir: &Path, name: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            found.extend(find_modules(&path, name));
        } else if entry.file_name().to_string_lossy().starts_with(name) {
            found.push(path);
        }
    }
    found
}

fn install_module(kernel_release: &str) -> Result<String, String> {
    let built = dkms_src_dir().join("src/hp-wmi-omen/hp-wmi.ko");
    if !built.exists() {
        return Err(format!("{} was not built", built.display()));
    }
    let dest_dir = &module_dirs(kernel_release)[0];
    fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    let dest = dest_dir.join("hp-wmi.ko");
    fs::copy(&built, &dest).map_err(|e| format!("installing {}: {e}", dest.display()))?;
    Ok(format!("installed {}", dest.display()))
}

/// Where each distro family's kernel hook has to live, and what it's called
/// in the source project's `hooks/` directory.
fn hook_paths(flavour: HookFlavour) -> Option<(&'static str, PathBuf)> {
    match flavour {
        HookFlavour::Pacman => Some((
            "90-hp-wmi-omen.hook",
            PathBuf::from("/etc/pacman.d/hooks/90-hp-wmi-omen.hook"),
        )),
        HookFlavour::KernelPostinst => Some((
            "zz-hp-wmi-omen",
            PathBuf::from("/etc/kernel/postinst.d/zz-hp-wmi-omen"),
        )),
        HookFlavour::KernelInstall => Some((
            "99-hp-wmi-omen.install",
            PathBuf::from("/etc/kernel/install.d/99-hp-wmi-omen.install"),
        )),
        HookFlavour::None => None,
    }
}

fn install_hook(env: &Environment) -> Result<String, String> {
    let source = driver_source(env)?;
    let Some((name, dest)) = hook_paths(env.hook_flavour) else {
        return Ok("no hook mechanism for this distribution".to_string());
    };

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::copy(source.join("hooks").join(name), &dest)
        .map_err(|e| format!("installing {}: {e}", dest.display()))?;

    // The postinst and kernel-install hooks are executed directly.
    if env.hook_flavour != HookFlavour::Pacman {
        set_executable(&dest)?;
    }
    Ok(format!("installed {}", dest.display()))
}

fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).map_err(|e| e.to_string())?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).map_err(|e| e.to_string())
}

fn remove_sources() -> Result<String, String> {
    let mut removed = Vec::new();
    for dir in [dkms_src_dir(), PathBuf::from(format!("/usr/src/{DKMS_NAME}"))] {
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| format!("removing {}: {e}", dir.display()))?;
            removed.push(dir.display().to_string());
        }
    }
    Ok(if removed.is_empty() {
        "nothing to remove".to_string()
    } else {
        format!("removed {}", removed.join(", "))
    })
}

fn remove_hooks() -> Result<String, String> {
    let mut removed = Vec::new();
    for flavour in [
        HookFlavour::Pacman,
        HookFlavour::KernelPostinst,
        HookFlavour::KernelInstall,
    ] {
        let Some((_, path)) = hook_paths(flavour) else { continue };
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("removing {}: {e}", path.display()))?;
            removed.push(path.display().to_string());
        }
    }
    Ok(if removed.is_empty() {
        "no hooks installed".to_string()
    } else {
        format!("removed {}", removed.join(", "))
    })
}

fn restore_backups(kernel_release: &str) -> Result<String, String> {
    let mut restored = Vec::new();
    for dir in module_dirs(kernel_release) {
        for backup in find_modules(&dir, "hp-wmi.ko") {
            let name = backup.to_string_lossy().to_string();
            let Some(original) = name.strip_suffix(".bak") else { continue };
            fs::rename(&backup, original)
                .map_err(|e| format!("restoring {original}: {e}"))?;
            restored.push(original.to_string());
        }
    }
    Ok(if restored.is_empty() {
        "no backups found; the distribution's own module will be used".to_string()
    } else {
        format!("restored {}", restored.join(", "))
    })
}

const SERVICE_PATH: &str = "/etc/systemd/system/pyren-daemon.service";

fn write_service_unit(context: &ExecuteContext) -> Result<String, String> {
    let binary = context
        .daemon_binary
        .clone()
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| "could not determine the daemon's own path".to_string())?;

    // Socket lives under /run so it disappears on reboot; RuntimeDirectory
    // makes systemd create and clean it up. The directory stays traversable
    // by everyone on purpose - the gate is the socket inside it, which the
    // daemon binds 0660 to the 'pyren' group (see
    // `pyren_core::socket`). A 0750 directory here would instead lock
    // out the very group members it is meant to admit.
    // CAP_PERFMON is what lets the daemon open the i915 perf PMU, the only
    // interface Intel exposes iGPU utilisation through; without it the
    // integrated GPU reports no usage at all. Running as root already
    // carries it, so `AmbientCapabilities` changes nothing today - it is
    // here to *state the requirement*, so that hardening this unit (a
    // `User=`, a `CapabilityBoundingSet`) does not silently take the
    // reading away with no clue as to why.
    let unit = format!(
        "[Unit]\n\
         Description=Pyren hardware daemon\n\
         Documentation=https://github.com/mglourido/PYREN\n\
         After=multi-user.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={}\n\
         Environment=PYREN_SOCKET=/run/pyren/daemon.sock\n\
         RuntimeDirectory=pyren\n\
         RuntimeDirectoryMode=0755\n\
         AmbientCapabilities=CAP_PERFMON\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        binary.display()
    );

    fs::write(SERVICE_PATH, unit).map_err(|e| format!("writing {SERVICE_PATH}: {e}"))?;
    Ok(format!("wrote {SERVICE_PATH}"))
}

fn remove_service_unit() -> Result<String, String> {
    if !Path::new(SERVICE_PATH).exists() {
        return Ok("no unit installed".to_string());
    }
    fs::remove_file(SERVICE_PATH).map_err(|e| e.to_string())?;
    Ok(format!("removed {SERVICE_PATH}"))
}
