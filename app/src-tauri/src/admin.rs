//! Admin mode: what the app is *allowed* to do, and how to fix it.
//!
//! Most of what makes Pyren look broken is not broken code, it is missing
//! privilege - and the two are indistinguishable from the UI. A machine
//! whose daemon is not running, or whose user is not in the `pyren` group,
//! shows the same wall of demo numbers as one with no supported hardware
//! at all.
//!
//! So this module answers two questions the rest of the app cannot:
//!
//! - **What is missing?** Every check here runs unprivileged, from this
//!   process, and needs no daemon - which matters, because the daemon
//!   being unreachable is one of the things being diagnosed.
//! - **Can we fix it?** Each fix is a fixed command run under `pkexec`,
//!   which puts the authentication in the desktop's hands rather than
//!   ours. There is no free-form command: the frontend picks an action
//!   from a closed set, and nothing it sends reaches a shell.
//!
//! Installing the systemd unit is the one privileged action that cannot go
//! through the daemon's IPC: the unit is what *makes* the daemon
//! privileged, so asking an unprivileged daemon to install it is a chicken
//! and egg. That is broken here by running the daemon binary itself under
//! `pkexec` with `--install-service`, which drives the very same
//! `installer::{plan, execute}` the IPC method does. One implementation,
//! two callers - not a second installer.
//!
//! Installing the *driver* is still out of scope: it replaces a kernel
//! module, it deserves the reviewable plan the installer page will show,
//! and it does not belong behind a single button.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

/// The group the daemon binds its socket to (`pyren_core::socket`).
const SOCKET_GROUP: &str = "pyren";
const SERVICE: &str = "pyren-daemon.service";
const UNIT_PATHS: &[&str] = &[
    "/etc/systemd/system/pyren-daemon.service",
    "/usr/lib/systemd/system/pyren-daemon.service",
];

/// What the frontend may ask to have changed. A closed set on purpose: the
/// alternative is a command string arriving from a webview and being run
/// as root.
#[derive(Debug, Clone, Copy)]
pub enum Grant {
    /// Create the socket group if needed and put this user in it.
    JoinGroup,
    /// Write the systemd unit and enable it, so the daemon runs as root
    /// from now on and across reboots.
    InstallService,
    /// Enable and start an already-installed unit.
    EnableService,
    /// Load `acpi_call` now, and arrange for it to be loaded at boot.
    ///
    /// Not the same shape as the others: it is a kernel module rather
    /// than a service or a group. It is here because the fan cleaner and
    /// the RGB lightbar both need it and both fail with the same
    /// "permission denied" the rest of this panel exists to explain -
    /// which sends people to the wrong fix.
    LoadAcpiCall,
}

impl Grant {
    fn parse(action: &str) -> Result<Self, String> {
        match action {
            "joinGroup" => Ok(Self::JoinGroup),
            "installService" => Ok(Self::InstallService),
            "enableService" => Ok(Self::EnableService),
            "loadAcpiCall" => Ok(Self::LoadAcpiCall),
            other => Err(format!("unknown admin action '{other}'")),
        }
    }
}

pub fn status(socket_path: &str) -> Value {
    let unit_path = UNIT_PATHS.iter().find(|p| Path::new(p).exists());
    let group_gid = group_gid(SOCKET_GROUP);

    // Two different questions, and confusing them is the classic way to
    // leave someone staring at a "fixed" checklist that still does not
    // work: `usermod` changes the group database immediately, but a login
    // session keeps the groups it started with until the user logs back in.
    let in_group_database = group_gid.is_some_and(|gid| user_is_listed_in(SOCKET_GROUP, gid));
    let session_has_group = group_gid.is_some_and(|gid| current_groups().contains(&gid));

    // One attempt, two answers: connecting twice could report a state that
    // never existed if the daemon starts or stops between the two.
    let connection = UnixStream::connect(socket_path);
    let denied = matches!(&connection, Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied);

    json!({
        "socketPath": socket_path,
        "socketReachable": connection.is_ok(),
        "socketDenied": denied,
        "unitPath": unit_path,
        "serviceActive": systemctl_says("is-active", "active"),
        "serviceEnabled": systemctl_says("is-enabled", "enabled"),
        "groupName": SOCKET_GROUP,
        "groupExists": group_gid.is_some(),
        "inGroupDatabase": in_group_database,
        "sessionHasGroup": session_has_group,
        // The fix is applied but the session predates it: nothing else will
        // work until the user logs out, and no button can do it for them.
        "needsRelogin": in_group_database && !session_has_group,
        // Loaded is the only state the feature works in; installed but
        // unloaded is one `modprobe` away, and neither is one `pacman`
        // away. Three states, because the remedy differs for each.
        "acpiCallLoaded": Path::new("/proc/acpi/call").exists(),
        "acpiCallInstalled": Path::new("/proc/acpi/call").exists() || modinfo_finds_acpi_call(),
        "canElevate": which("pkexec"),
        "daemonBinary": daemon_binary(),
        "user": username(),
    })
}

/// Runs one fix under `pkexec`. Returns what ran and what it said, so a
/// failure is something the user can read rather than a silent no-op.
/// Whether `acpi_call` is built for this kernel but not loaded. Asked with
/// `modinfo`, which does not load anything - the same question
/// `pyren_core::acpi::is_module_installed` puts, from the app's side of
/// the socket, because this panel has to work when the daemon does not.
fn modinfo_finds_acpi_call() -> bool {
    Command::new("modinfo")
        .args(["-n", "acpi_call"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

pub fn grant(action: &str) -> Result<Value, String> {
    if !which("pkexec") {
        return Err("pkexec is not installed; this needs a polkit agent".to_string());
    }

    let output = match Grant::parse(action)? {
        // The username is passed as an *argument*, never interpolated into
        // the script text, so there is no shell to inject into. It comes
        // from the OS anyway, not from the webview.
        Grant::JoinGroup => Command::new("pkexec")
            .args([
                "/bin/sh",
                "-c",
                "groupadd -f \"$1\" && usermod -aG \"$1\" \"$2\"",
                "--",
                SOCKET_GROUP,
                &username().ok_or_else(|| "cannot determine the current user".to_string())?,
            ])
            .output(),
        Grant::InstallService => {
            let binary = daemon_binary()
                .ok_or_else(|| "cannot find the pyren-daemon binary to install".to_string())?;
            Command::new("pkexec").args([&binary, "--install-service"]).output()
        }
        Grant::EnableService => Command::new("pkexec")
            .args(["systemctl", "enable", "--now", SERVICE])
            .output(),
        // `modprobe` alone lasts until the next reboot, and a feature that
        // works today and not tomorrow is worse than one that never did -
        // so the modules-load.d drop-in goes down with it. Both are fixed
        // strings; nothing from the webview reaches this shell.
        Grant::LoadAcpiCall => Command::new("pkexec")
            .args([
                "/bin/sh",
                "-c",
                "modprobe acpi_call && \
                 mkdir -p /etc/modules-load.d && \
                 printf 'acpi_call\\n' > /etc/modules-load.d/pyren-acpi-call.conf",
            ])
            .output(),
    };

    let output = output.map_err(|e| format!("could not run pkexec: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        // 126 is polkit's "the user dismissed the dialog", which is a
        // choice rather than a failure worth an error message.
        if output.status.code() == Some(126) {
            return Ok(json!({ "applied": false, "cancelled": true, "detail": "" }));
        }
        return Err(if stderr.is_empty() {
            format!("{action} failed with status {}", output.status)
        } else {
            stderr
        });
    }

    Ok(json!({ "applied": true, "cancelled": false, "detail": stderr }))
}

/// Where the daemon binary is, since installing the service means running
/// it. `PYREN_DAEMON` wins, then `PATH`, then the usual prefixes, then the
/// development build sitting beside this app's own target directory.
///
/// Returns `None` rather than a guess: a wrong path here would be handed to
/// `pkexec`, and "we could not find it" is a far better thing to show than
/// a root prompt for something that will fail.
fn daemon_binary() -> Option<String> {
    const BINARY: &str = "pyren-daemon";

    if let Ok(path) = std::env::var("PYREN_DAEMON") {
        if Path::new(&path).is_file() {
            return Some(path);
        }
    }

    if let Ok(path) = std::env::var("PATH") {
        if let Some(found) = std::env::split_paths(&path)
            .map(|dir| dir.join(BINARY))
            .find(|candidate| candidate.is_file())
        {
            return Some(found.to_string_lossy().into_owned());
        }
    }

    let mut candidates = vec![
        std::path::PathBuf::from("/usr/bin").join(BINARY),
        std::path::PathBuf::from("/usr/local/bin").join(BINARY),
    ];
    // `app/src-tauri/target/<profile>/app` -> `daemon/target/<profile>/`
    if let Ok(own) = std::env::current_exe() {
        if let (Some(profile), Some(repo)) = (
            own.parent().and_then(|p| p.file_name()),
            own.ancestors().nth(5),
        ) {
            candidates.push(repo.join("daemon/target").join(profile).join(BINARY));
        }
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|found| found.to_string_lossy().into_owned())
}

fn systemctl_says(query: &str, expected: &str) -> bool {
    Command::new("systemctl")
        .args([query, SERVICE])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == expected)
        .unwrap_or(false)
}

/// The gid of a group, or `None` when no such group exists.
fn group_gid(name: &str) -> Option<u32> {
    let entry = Command::new("getent").args(["group", name]).output().ok()?;
    if !entry.status.success() {
        return None;
    }
    // name:x:gid:member,member
    let text = String::from_utf8_lossy(&entry.stdout);
    text.split(':').nth(2)?.trim().parse().ok()
}

/// Whether the current user is a member of `name` *according to the group
/// database* - which is not the same as their session having it.
fn user_is_listed_in(name: &str, gid: u32) -> bool {
    let Some(user) = username() else { return false };
    let Ok(output) = Command::new("id").args(["-nG", &user]).output() else {
        return false;
    };
    if String::from_utf8_lossy(&output.stdout).split_whitespace().any(|g| g == name) {
        return true;
    }
    // A user whose *primary* group is this one is a member without ever
    // appearing in the member list.
    primary_gid() == Some(gid)
}

/// The groups this process actually carries, straight from the kernel.
fn current_groups() -> Vec<u32> {
    // SAFETY: the first call asks only for the count, passing a null list
    // as the API allows; the second fills a buffer of exactly that size.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count < 0 {
        return Vec::new();
    }
    let mut groups = vec![0 as libc::gid_t; count as usize];
    let filled = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
    if filled < 0 {
        return Vec::new();
    }
    groups.truncate(filled as usize);
    let mut groups: Vec<u32> = groups.into_iter().collect();
    if let Some(primary) = primary_gid() {
        groups.push(primary);
    }
    groups
}

fn primary_gid() -> Option<u32> {
    // SAFETY: getgid cannot fail and touches no memory we own.
    Some(unsafe { libc::getgid() })
}

fn username() -> Option<String> {
    std::env::var("USER").ok().or_else(|| std::env::var("LOGNAME").ok())
}

fn which(binary: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(binary).is_file())
}
