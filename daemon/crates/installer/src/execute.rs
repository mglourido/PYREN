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

use pyren_core::{msg, Msg};
use serde::Serialize;

use crate::detect::{hook_paths, Environment, HookFlavour, HOOK_FLAVOURS};
use crate::patch::{self, BoardTable, MaxRpm};
use crate::plan::{Plan, Step, DKMS_NAME, DKMS_VERSION, MODPROBE_CONF_PATH};

/// Extra inputs an install needs beyond the plan itself.
#[derive(Debug, Clone, Default)]
pub struct ExecuteContext {
    pub max_rpm: MaxRpm,
    pub experimental_board: Option<(BoardTable, String)>,
    /// Path of the pyren-daemon binary, for the systemd unit.
    pub daemon_binary: Option<PathBuf>,
    /// Ids of steps the caller asked not to run.
    ///
    /// Only a step the plan marked `optional` may appear here, and
    /// `installer.apply` refuses the request otherwise. "Optional" is the
    /// plan's own word for a step whose failure it tolerates - so it is
    /// also the only step whose absence it can tolerate. Skipping `depmod`
    /// or the `modprobe` that loads the new module would leave a half
    /// install behind and call it a success.
    pub skip_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepResult {
    pub id: String,
    /// Translatable - render with `tm()`. Copied from the plan step.
    pub description: Msg,
    pub status: StepStatus,
    /// The step's own words for a planned/skipped step (translatable), or
    /// the command output / error text of a real run (verbatim, not
    /// translated).
    pub detail: Msg,
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
    /// Optional, and the caller asked for it not to be run. Kept apart
    /// from `Skipped`: one is a consequence of a failure, the other is a
    /// decision, and a report that called them the same thing would hide
    /// which of the two happened.
    Declined,
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

/// One thing worth telling a watching UI, as it happens.
///
/// Emitted twice per step - once as it starts, once with what it did - so
/// a progress bar can advance on the *start* of a step rather than on the
/// end of the previous one. The difference is visible: `dkms-build` takes
/// most of an install, and a bar that only moves when a step finishes sits
/// still through it with nothing on screen saying why.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress<'a> {
    /// 0-based, so `index + 1` of `total` reads naturally.
    pub index: usize,
    pub total: usize,
    pub id: &'a str,
    /// Translatable - render with `tm()`. Copied from the plan step.
    pub description: &'a Msg,
    /// `None` while the step is running; the outcome once it is over.
    pub status: Option<StepStatus>,
    /// Only on the second emission, and only when there is something to
    /// show: command output, or the error.
    pub detail: Option<&'a Msg>,
}

/// Somewhere to send [`Progress`] as it happens. `Sync` because the
/// daemon's event bus is shared.
pub type ProgressSink<'a> = &'a (dyn Fn(Progress) + Sync);

pub fn execute(
    plan: &Plan,
    env: &Environment,
    context: &ExecuteContext,
    dry_run: bool,
) -> ExecutionReport {
    execute_watched(plan, env, context, dry_run, None)
}

/// [`execute`], with somebody watching.
///
/// A dry run reports its steps too: the wizard shows the same panel for
/// both, and a rehearsal that skipped straight to the report would look
/// like a real run that had gone wrong.
pub fn execute_watched(
    plan: &Plan,
    env: &Environment,
    context: &ExecuteContext,
    dry_run: bool,
    progress: Option<ProgressSink>,
) -> ExecutionReport {
    let mut results = Vec::new();
    let mut failed = false;
    let total = plan.steps.len();

    for (index, step) in plan.steps.iter().enumerate() {
        if let Some(sink) = progress {
            sink(Progress {
                index,
                total,
                id: &step.id,
                description: &step.description,
                status: None,
                detail: None,
            });
        }
        // One outcome per step, worked out before anything is pushed, so
        // there is a single place that both records it and announces it.
        // The early returns this replaced each skipped the announcement.
        let done = if failed {
            result(step, StepStatus::Skipped, msg!("installer.exec.skipped", "skipped after an earlier failure"))
        } else if step.optional && context.skip_steps.iter().any(|id| id == &step.id) {
            result(step, StepStatus::Declined, msg!("installer.exec.declined", "not run, at your request"))
        } else if dry_run {
            let detail = if step.command.is_empty() {
                // Deliberately the same sentence as the plan step's
                // `installer.internalStep`: one concept, one name. A client
                // with no catalog reads this text, so they have to match
                // here too, not only in the catalog.
                msg!("installer.exec.internalAction", "carried out by the daemon itself")
            } else {
                // A command line, quoted verbatim.
                Msg::literal(step.command.join(" "))
            };
            result(step, StepStatus::Planned, detail)
        } else {
            match run_step(step, env, context) {
                // Real-run detail is command output / OS text: verbatim.
                Ok(detail) => result(step, StepStatus::Ok, Msg::literal(detail)),
                Err(e) if step.optional => result(step, StepStatus::Warned, Msg::literal(e)),
                Err(e) => {
                    failed = true;
                    result(step, StepStatus::Failed, Msg::literal(e))
                }
            }
        };
        results.push(done);

        // Whatever the branch above decided, it is the last thing pushed.
        if let (Some(sink), Some(done)) = (progress, results.last()) {
            sink(Progress {
                index,
                total,
                id: &step.id,
                description: &step.description,
                status: Some(done.status),
                detail: Some(&done.detail),
            });
        }
    }

    ExecutionReport { dry_run, succeeded: !failed, results }
}

fn result(step: &Step, status: StepStatus, detail: Msg) -> StepResult {
    StepResult { id: step.id.clone(), description: step.description.clone(), status, detail }
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
        "write-modprobe-conf" => write_modprobe_conf(context),
        "remove-modprobe-conf" => remove_modprobe_conf(),
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

/// Patches the *staged* copy under `/usr/src`, never the source tree it was
/// copied from: that tree is a read-only snapshot of upstream, and patching
/// it in place would both dirty the checkout and make the next install start
/// from the previous one's output.
fn patch_source(_env: &Environment, context: &ExecuteContext) -> Result<String, String> {
    let dir = dkms_src_dir().join("src");
    if !dir.join("hp-wmi-omen/hp-wmi.c").is_file() {
        return Err(format!("{} has not been staged", dir.display()));
    }
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
        stock_module_dir(kernel_release),
        PathBuf::from(format!("/lib/modules/{kernel_release}/updates")),
    ]
}

/// The one directory a distribution ships its own `hp-wmi` in. Anything
/// under `updates/` got there from DKMS or from this installer.
fn stock_module_dir(kernel_release: &str) -> PathBuf {
    PathBuf::from(format!(
        "/lib/modules/{kernel_release}/kernel/drivers/platform/x86/hp"
    ))
}

/// Backs up and removes any `hp-wmi.ko` in the way of the one about to be
/// installed.
///
/// **Only the first module found in a directory is the stock one**, and
/// only in the directory a distribution actually ships to. Both halves of
/// that were learned the hard way:
///
/// - The backup used to be keyed on the *filename*, so a run that found
///   `hp-wmi.ko` next to an existing `hp-wmi.ko.zst.bak` wrote a second
///   backup - of an already-patched module, since a previous install by
///   the hook strategy leaves its uncompressed `hp-wmi.ko` exactly there.
///   Restoring then renamed both back, and `depmod` preferred the
///   uncompressed one: "restored the distribution's driver" put the
///   patched one in place. A backup already present in a directory means
///   the stock module is safe, so what is there now is ours to delete.
/// - `updates/` never holds a distribution's own module, so nothing found
///   there is a candidate for the pristine backup - it is a previous DKMS
///   install of ours, and backing it up would tell the same lie.
fn backup_stock_driver(kernel_release: &str) -> Result<String, String> {
    let stock_dir = stock_module_dir(kernel_release);
    let mut backed_up = Vec::new();

    for dir in module_dirs(kernel_release) {
        // `updates/` holds no stock module to preserve, whatever is or
        // isn't backed up there.
        backed_up.extend(backup_in(&dir, dir == stock_dir)?);
    }

    Ok(if backed_up.is_empty() {
        "no stock driver found to back up".to_string()
    } else {
        format!("backed up {}", backed_up.join(", "))
    })
}

/// One directory's worth of the backup, and what it preserved. Split out
/// for the same reason as [`restore_in`]: a function whose whole job is
/// copying and deleting kernel modules can only be tested against a
/// temporary directory.
fn backup_in(dir: &Path, may_hold_stock: bool) -> Result<Vec<String>, String> {
    let modules = find_modules(dir, "hp-wmi.ko");
    let already_backed_up = modules.iter().any(|m| m.to_string_lossy().ends_with(".bak"));
    let holds_stock = may_hold_stock && !already_backed_up;

    let mut backed_up = Vec::new();
    for module in modules {
        if module.to_string_lossy().ends_with(".bak") {
            continue;
        }
        if holds_stock {
            let backup = PathBuf::from(format!("{}.bak", module.display()));
            fs::copy(&module, &backup)
                .map_err(|e| format!("backing up {}: {e}", module.display()))?;
            backed_up.push(backup.display().to_string());
        }
        fs::remove_file(&module).map_err(|e| format!("removing {}: {e}", module.display()))?;
    }
    Ok(backed_up)
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
    for flavour in HOOK_FLAVOURS {
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

/// Puts the stock module back, and takes the patched one away.
///
/// Both halves are needed, and only doing the first is a restore that does
/// not restore. Distributions ship the module compressed (`hp-wmi.ko.zst`
/// on Arch), while [`install_module`] writes an uncompressed `hp-wmi.ko` -
/// so renaming the `.bak` back leaves the two side by side, and `depmod`
/// resolves that in favour of the patched one. The machine goes on running
/// the patched driver while the report says the stock one was restored.
fn restore_backups(kernel_release: &str) -> Result<String, String> {
    let mut restored = Vec::new();
    let mut removed = Vec::new();

    for dir in module_dirs(kernel_release) {
        let (dir_restored, dir_removed) = restore_in(&dir)?;
        restored.extend(dir_restored);
        removed.extend(dir_removed);
    }

    Ok(match (restored.is_empty(), removed.is_empty()) {
        (true, _) => "no backups found; the distribution's own module will be used".to_string(),
        (false, true) => format!("restored {}", restored.join(", ")),
        (false, false) => format!(
            "restored {}, removed the patched {}",
            restored.join(", "),
            removed.join(", ")
        ),
    })
}

/// One directory's worth of the restore: what came back, and what was
/// taken away. Split out so it can be pointed at a temporary directory,
/// which is the only way to test a function whose whole job is renaming and
/// deleting kernel modules.
fn restore_in(dir: &Path) -> Result<(Vec<String>, Vec<String>), String> {
    let mut restored = Vec::new();
    for backup in find_modules(dir, "hp-wmi.ko") {
        let name = backup.to_string_lossy().to_string();
        let Some(original) = name.strip_suffix(".bak") else { continue };
        fs::rename(&backup, original).map_err(|e| format!("restoring {original}: {e}"))?;
        restored.push(original.to_string());
    }

    let mut removed = Vec::new();
    for module in leftovers(dir, &restored) {
        let name = module.to_string_lossy().to_string();
        fs::remove_file(&module).map_err(|e| format!("removing {name}: {e}"))?;
        removed.push(name);
    }
    Ok((restored, removed))
}

/// Modules in `dir` that this installer put there and that must go, now
/// that the stock one is back.
///
/// Never returns the only module present: with nothing to fall back on,
/// removing it would leave the machine with no hp-wmi at all, which is
/// worse than the state being restored from.
fn leftovers(dir: &Path, restored: &[String]) -> Vec<PathBuf> {
    let present: Vec<PathBuf> = find_modules(dir, "hp-wmi.ko")
        .into_iter()
        .filter(|p| !p.to_string_lossy().ends_with(".bak"))
        .collect();
    if present.len() < 2 {
        return Vec::new();
    }

    if !restored.is_empty() {
        // Something came back here, so anything else is ours: the backup
        // step removes every stock module before the new one lands.
        return present
            .into_iter()
            .filter(|p| !restored.contains(&p.to_string_lossy().to_string()))
            .collect();
    }

    // No backup to restore, yet two modules are here. A distribution ships
    // one, so this is the wreckage of an earlier restore that put the stock
    // module back without taking ours away - and `depmod` resolves it in
    // favour of the uncompressed one, which is the one we install.
    let compressed_present = present
        .iter()
        .any(|p| p.file_name().is_some_and(|n| n != "hp-wmi.ko"));
    if !compressed_present {
        return Vec::new();
    }
    present
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n == "hp-wmi.ko"))
        .collect()
}

/// Writes the measured ceilings where `modprobe` will find them.
///
/// Two properties this file has that a patched constant does not: it is
/// read on *every* load, so a kernel upgrade or a DKMS rebuild keeps the
/// measurement without recompiling anything, and it is applied after the
/// driver's firmware queries, so it is the only one of the two that a
/// board answering that query actually honours.
///
/// A fan with no measurement is left out of the file rather than written
/// as zero. They mean the same thing to the driver, but only one of them
/// says it: a file listing `gpu_max_rpm_measured=0` reads like a measured
/// ceiling of nothing.
fn write_modprobe_conf(context: &ExecuteContext) -> Result<String, String> {
    pin_measured_ceiling(context.max_rpm)
}

/// The durable half of pinning a measurement, on its own.
///
/// Separate from the reload because the two have completely different
/// costs. Writing this file is inert: nothing changes until `hp-wmi` is
/// next loaded, so it can be done after *any* calibration, by anyone, with
/// no disruption at all. Reloading is what makes it take effect now, and
/// it takes fan control, the hotkeys and the firmware profile down for a
/// moment - and it recreates the hwmon directory, so every cached sysfs
/// path in this process points at a file that no longer exists. That is
/// why the daemon writes and does not reload: an install is already
/// reloading anyway, and a plain recalibration lands at the next boot
/// rather than breaking the running daemon to save the wait.
pub fn pin_measured_ceiling(max_rpm: MaxRpm) -> Result<String, String> {
    let mut options = Vec::new();
    for (name, rpm) in [(patch::CPU_RPM_PARAM, max_rpm.cpu), (patch::GPU_RPM_PARAM, max_rpm.gpu)] {
        // The driver counts in hundreds of RPM, and its parameters are u8,
        // so a ceiling above 25500 rpm cannot be expressed. No fan in one
        // of these laptops comes close, but silently truncating one would
        // pin a ceiling nobody measured.
        let Some(rpm) = rpm else { continue };
        let hundreds = rpm / 100;
        if hundreds == 0 || hundreds > u8::MAX as u32 {
            return Err(format!("{rpm} rpm is outside what {name} can hold (100-25500 rpm)"));
        }
        options.push(format!("{name}={hundreds}"));
    }
    if options.is_empty() {
        return Err("no measured fan ceiling to write; run the fan calibration first".to_string());
    }

    let path = Path::new(MODPROBE_CONF_PATH);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    let body = format!(
        "# Written by Pyren from a fan calibration run on this machine.\n\
         # These outrank the ceiling the firmware reports; delete this file\n\
         # to go back to whatever the driver works out for itself.\n\
         options hp-wmi {}\n",
        options.join(" ")
    );
    fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(format!("wrote {} to {}", options.join(" "), path.display()))
}

fn remove_modprobe_conf() -> Result<String, String> {
    let path = Path::new(MODPROBE_CONF_PATH);
    if !path.exists() {
        return Ok(format!("{} is not there", path.display()));
    }
    fs::remove_file(path).map_err(|e| format!("removing {}: {e}", path.display()))?;
    Ok(format!("removed {}", path.display()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{plan, Action, PlanOptions};
    use crate::detect::{HeadersInfo, KernelInfo};
    use std::path::PathBuf;

    /// A machine where an install would go ahead, so the plan under test
    /// has both required and optional steps in it.
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

    /// Reproduces the layout the restore used to leave behind: a
    /// compressed stock module put back from its backup, and the
    /// uncompressed patched one still sitting next to it. `depmod` picks
    /// the uncompressed one, so the machine kept running the patched
    /// driver while the report said it had been restored.
    #[test]
    fn restoring_takes_the_patched_module_away_as_well() {
        let dir = std::env::temp_dir().join(format!("pyren-restore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("hp-wmi.ko"), b"patched").unwrap();
        fs::write(dir.join("hp-wmi.ko.zst.bak"), b"stock").unwrap();

        let (restored, removed) = restore_in(&dir).unwrap();

        assert!(dir.join("hp-wmi.ko.zst").is_file(), "the stock module comes back");
        assert!(!dir.join("hp-wmi.ko").exists(), "the patched one must not be left behind");
        assert!(!dir.join("hp-wmi.ko.zst.bak").exists(), "the backup is consumed");
        assert_eq!(restored.len(), 1);
        assert_eq!(removed.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    /// The bug this machine was found in: a hook-strategy install had left
    /// its uncompressed patched module in the distro's own directory, next
    /// to the compressed stock module's backup. Keying the backup on the
    /// filename saw no `hp-wmi.ko.bak` and made one - of the patched
    /// module - and a restore then had two "stock" modules to put back.
    #[test]
    fn a_second_backup_is_never_taken_of_our_own_module() {
        let dir = std::env::temp_dir().join(format!("pyren-backup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("hp-wmi.ko.zst.bak"), b"stock").unwrap();
        fs::write(dir.join("hp-wmi.ko"), b"patched").unwrap();

        let backed_up = backup_in(&dir, true).unwrap();

        assert!(backed_up.is_empty(), "the stock module is already safe");
        assert!(!dir.join("hp-wmi.ko.bak").exists(), "and must not be shadowed by a patched one");
        assert!(!dir.join("hp-wmi.ko").exists(), "ours still goes, to leave depmod one answer");
        assert_eq!(fs::read(dir.join("hp-wmi.ko.zst.bak")).unwrap(), b"stock");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The first install on a fresh machine, which is the one run that has
    /// a stock module to preserve.
    #[test]
    fn the_first_install_backs_the_stock_module_up() {
        let dir = std::env::temp_dir().join(format!("pyren-backup-first-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("hp-wmi.ko.zst"), b"stock").unwrap();

        let backed_up = backup_in(&dir, true).unwrap();

        assert_eq!(backed_up.len(), 1);
        assert_eq!(fs::read(dir.join("hp-wmi.ko.zst.bak")).unwrap(), b"stock");
        assert!(!dir.join("hp-wmi.ko.zst").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    /// `updates/` is DKMS's directory and ours. A module found there is a
    /// previous install of the patched driver, so backing it up would
    /// record it as the distribution's - the same lie, in the other place.
    #[test]
    fn nothing_under_updates_is_mistaken_for_the_stock_module() {
        let dir = std::env::temp_dir().join(format!("pyren-backup-upd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("dkms")).unwrap();
        fs::write(dir.join("dkms/hp-wmi.ko.zst"), b"ours, from last time").unwrap();

        let backed_up = backup_in(&dir, false).unwrap();

        assert!(backed_up.is_empty());
        assert!(!dir.join("dkms/hp-wmi.ko.zst.bak").exists());
        assert!(!dir.join("dkms/hp-wmi.ko.zst").exists(), "it is still cleared out of the way");

        let _ = fs::remove_dir_all(&dir);
    }

    /// With nothing to restore, the module present is all the machine has -
    /// removing it would leave no hp-wmi at all.
    #[test]
    fn with_no_backup_nothing_is_removed() {
        let dir = std::env::temp_dir().join(format!("pyren-restore-none-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("hp-wmi.ko"), b"whatever is here").unwrap();

        let (restored, removed) = restore_in(&dir).unwrap();

        assert!(restored.is_empty());
        assert!(removed.is_empty());
        assert!(dir.join("hp-wmi.ko").is_file(), "the only module stays");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The simple case, where the distribution also ships an uncompressed
    /// module: the restore overwrites ours by rename and there is nothing
    /// left over to delete.
    #[test]
    fn a_backup_of_the_same_name_simply_replaces_ours() {
        let dir = std::env::temp_dir().join(format!("pyren-restore-same-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("hp-wmi.ko"), b"patched").unwrap();
        fs::write(dir.join("hp-wmi.ko.bak"), b"stock").unwrap();

        let (restored, removed) = restore_in(&dir).unwrap();

        assert_eq!(fs::read(dir.join("hp-wmi.ko")).unwrap(), b"stock");
        assert_eq!(restored.len(), 1);
        assert!(removed.is_empty(), "nothing is left over to remove");

        let _ = fs::remove_dir_all(&dir);
    }

    /// The state a broken restore left on the test laptop: the stock
    /// module back, ours still beside it, and no backup left to notice it
    /// by. Running restore again has to be able to finish the job.
    #[test]
    fn a_leftover_patched_module_is_cleaned_up_even_with_no_backup() {
        let dir = std::env::temp_dir().join(format!("pyren-restore-left-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("hp-wmi.ko"), b"patched").unwrap();
        fs::write(dir.join("hp-wmi.ko.zst"), b"stock").unwrap();

        let (restored, removed) = restore_in(&dir).unwrap();

        assert!(restored.is_empty(), "there was no backup to restore");
        assert_eq!(removed.len(), 1, "but the leftover is ours and must go");
        assert!(!dir.join("hp-wmi.ko").exists());
        assert!(dir.join("hp-wmi.ko.zst").is_file(), "the stock module stays");

        let _ = fs::remove_dir_all(&dir);
    }

    /// One module and no backup is a machine we have not touched. Removing
    /// it would leave no hp-wmi at all.
    #[test]
    fn a_lone_module_is_never_removed() {
        for name in ["hp-wmi.ko", "hp-wmi.ko.zst"] {
            let dir = std::env::temp_dir()
                .join(format!("pyren-restore-lone-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(name), b"stock").unwrap();

            let (restored, removed) = restore_in(&dir).unwrap();

            assert!(restored.is_empty() && removed.is_empty());
            assert!(dir.join(name).is_file(), "{name} must survive");
            let _ = fs::remove_dir_all(&dir);
        }
    }

    fn status_of(report: &ExecutionReport, id: &str) -> StepStatus {
        report.results.iter().find(|r| r.id == id).expect(id).status
    }

    /// The point of the checkbox: regenerating the initramfs is known to
    /// fail on odd EFI layouts, and someone who knows their own boot setup
    /// should be able to tell the installer to leave it alone.
    #[test]
    fn an_optional_step_the_caller_declined_is_not_run() {
        let env = ready_env();
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        let context = ExecuteContext {
            skip_steps: vec!["initramfs".to_string()],
            ..ExecuteContext::default()
        };

        let report = execute(&plan, &env, &context, true);
        assert_eq!(status_of(&report, "initramfs"), StepStatus::Declined);
        // ...and nothing else changes: the rest of a dry run is still planned.
        assert_eq!(status_of(&report, "depmod"), StepStatus::Planned);
        assert!(report.succeeded, "declining a step is not a failure");
    }

    /// `Declined` and `Skipped` must stay distinct - one is a decision, the
    /// other is the wreckage of an earlier failure, and a reader of the
    /// report needs to know which happened.
    #[test]
    fn declining_is_reported_as_its_own_outcome() {
        let env = ready_env();
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        let context = ExecuteContext {
            skip_steps: vec!["modprobe-remove".to_string()],
            ..ExecuteContext::default()
        };

        let report = execute(&plan, &env, &context, true);
        let result = report.results.iter().find(|r| r.id == "modprobe-remove").unwrap();
        assert_eq!(result.status, StepStatus::Declined);
        assert_eq!(result.detail.key, "installer.exec.declined");
        assert!(!report.results.iter().any(|r| r.status == StepStatus::Skipped));
    }

    /// Belt and braces behind `apply`'s own validation: even handed a
    /// required step, `execute` runs it. Skipping `depmod` would leave a
    /// module installed that nothing can find, and report success.
    #[test]
    fn a_required_step_is_run_even_if_it_was_named() {
        let env = ready_env();
        let plan = plan(&env, Action::InstallDriver, PlanOptions::default());
        let context = ExecuteContext {
            skip_steps: vec!["depmod".to_string()],
            ..ExecuteContext::default()
        };

        let report = execute(&plan, &env, &context, true);
        assert_eq!(status_of(&report, "depmod"), StepStatus::Planned);
    }
}
