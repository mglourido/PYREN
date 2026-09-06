//! The processes Pyren needs running, and starting them so nobody has to.
//!
//! There are three, and they are not the same kind of thing:
//!
//! | process | privilege | who should start it |
//! |---|---|---|
//! | `pyren-daemon` | root | systemd, at boot |
//! | `pyren-osd` (the widget) | the user's session | this app, at launch |
//! | the app itself | the user's session | the desktop, at login (optional) |
//!
//! **Only the middle one is this module's to start.** The daemon is a
//! system service: it must be there before anyone logs in - the
//! performance key works at the login screen, and the fan curve should not
//! wait for a window to be opened - so its unit is enabled once and
//! systemd handles it from then on. Starting it *from here* would mean a
//! polkit password prompt on every launch of the app, which is a worse
//! answer than the daemon simply already running. When it is not,
//! [`crate::admin`] says why and offers the one-time fix.
//!
//! The widget is the opposite: it draws on somebody's screen, so it has to
//! be in their session, and starting it costs no privilege at all. That is
//! the gap this closes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// The widget's binary and its user unit.
const OSD_BINARY: &str = "pyren-osd";
const OSD_UNIT: &str = "pyren-osd.service";

/// The desktop entry that starts the app at login, if the user asks for it.
const AUTOSTART_ENTRY: &str = "pyren.desktop";

/// …and the user unit that does the same job where nothing reads the entry.
const APP_UNIT: &str = "pyren.service";

/// Desktops whose session manager reads `~/.config/autostart` itself.
///
/// An allowlist, and not the list of compositors that do not: the case this
/// exists for is a bare wlroots session (Hyprland, Sway, river, niri) with
/// no session manager at all, and there is a new one of those every year.
/// Assuming an unknown desktop reads the entry would put "start on login"
/// back to failing in silence, which is the whole bug this is here to stop.
const DESKTOPS_WITH_A_SESSION_MANAGER: &[&str] = &[
    "gnome", "kde", "plasma", "xfce", "lxqt", "lxde", "mate", "cinnamon", "deepin", "pantheon",
    "budgie", "ukui", "unity",
];

/// Everything this module can report, for a settings page that has to
/// explain what is and is not running.
pub fn status() -> Value {
    let binary = find_osd();
    json!({
        "osd": {
            "running": osd_is_running(),
            "binary": binary.as_ref().map(|p| p.display().to_string()),
            "unitInstalled": user_unit_path().exists(),
            "startsAtLogin": unit_enabled(OSD_UNIT),
            "loginCommand": format!("systemctl --user start {OSD_UNIT}"),
        },
        "app": {
            // Either mechanism counts: turning the setting on writes both,
            // and a desktop that honours one of them is enough.
            "startsAtLogin": autostart_path().exists() || unit_enabled(APP_UNIT),
            "entry": autostart_path().display().to_string(),
            "unit": app_unit_path().display().to_string(),
            "loginCommand": format!("systemctl --user start {APP_UNIT}"),
        },
        // A fact about the session rather than about either process, and
        // it decides both: whether anything here acts on an autostart
        // entry or reaches `graphical-session.target` at all. False is not
        // a broken install - it is a compositor with no session manager,
        // and the settings page says which line to add rather than leaving
        // two toggles that write correct files nobody reads.
        //
        // The daemon is untouched by this. It is a *system* service
        // started by systemd at boot, so it comes up long before any
        // compositor and does not care which one it is.
        "loginWorks": login_start_is_honoured(),
    })
}

/// Starts whatever should be running and is not. Called once at launch.
///
/// Deliberately silent about success and never fatal: a machine with no
/// widget binary (the daemon-only install, or a build tree that has not
/// been built yet) is a machine that should still open its window.
pub fn ensure_running() {
    match start_osd() {
        Ok(true) => println!("pyren: started {OSD_BINARY}"),
        Ok(false) => {}
        Err(e) => eprintln!("pyren: could not start {OSD_BINARY}: {e}"),
    }
}

/// `Ok(true)` if it was started, `Ok(false)` if it was already running.
///
/// The "already running" check is not an optimisation: `pyren-osd` is
/// single-instance, and a second launch **shows the widget** rather than
/// starting a second process. Without this, opening the app would flash
/// the power-mode widget across the screen every time.
pub fn start_osd() -> Result<bool, String> {
    if osd_is_running() {
        return Ok(false);
    }

    // Through systemd when the unit is there, so the widget gets the
    // lifecycle the user configured - restarts included - rather than
    // becoming a child of a window that may close.
    if user_unit_path().exists() {
        let started = Command::new("systemctl")
            .args(["--user", "start", OSD_UNIT])
            .status()
            .map_err(|e| format!("running systemctl: {e}"))?;
        if started.success() {
            return Ok(true);
        }
        // Fall through: a unit that fails to start is not a reason to go
        // without the widget when the binary is right there.
    }

    let binary = find_osd().ok_or_else(|| {
        format!("{OSD_BINARY} is not installed (looked beside this binary, in PATH, and in the build tree)")
    })?;

    // Detached: stdio to /dev/null and no handle kept, so the widget
    // outlives the window that started it and nothing inherits the
    // webview's pipes.
    Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawning {}: {e}", binary.display()))?;
    Ok(true)
}

/// Shows the widget now, without changing anything.
///
/// Not `hotkey.press`, which is the *key*: that cycles the power mode,
/// because cycling is what the key does. Showing the widget and changing
/// the machine are two different requests, and a button labelled "preview"
/// that moved the user's power profile every time they pressed it was a
/// real bug, not a subtlety.
///
/// The widget is single-instance, so a second launch activates the one
/// that is up and shows it - which is exactly the "open the mode switcher"
/// gesture `pyren-osd` was built to answer.
pub fn show_osd() -> Result<(), String> {
    let binary = find_osd().ok_or_else(|| format!("{OSD_BINARY} is not installed"))?;

    let mut command = Command::new(&binary);
    // Nothing up yet: this copy becomes the widget, and `--show` puts it
    // on screen straight away. One already up: this copy activates it and
    // exits, and passing `--show` to a process that exits does nothing.
    if !osd_is_running() {
        command.arg("--show");
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawning {}: {e}", binary.display()))?;
    Ok(())
}

/// Stops the widget.
///
/// Deliberately does not touch the unit's `enabled` state: whether the
/// widget comes back at login is its own setting with its own toggle, and
/// silently flipping that one from here would be a control moving on its
/// own. What makes the feature *off* across a reboot is the daemon's
/// `hotkey.enabled`, which is what the settings page turns off alongside
/// this - a running widget that no key can reach never appears.
///
/// `Ok(false)` if nothing was running - which is not a failure, it is the
/// state being asked for.
pub fn stop_osd() -> Result<bool, String> {
    // Through systemd when it owns the process: killing a unit's process
    // directly would have it restarted by `Restart=on-failure`, and the
    // widget would come straight back.
    if user_unit_path().exists() {
        let _ = systemctl_user(&["stop", OSD_UNIT]);
    }

    let pids = osd_pids();
    for pid in &pids {
        // SIGTERM, not SIGKILL: GTK's main loop leaves on it, and there is
        // nothing here worth killing a process over.
        // SAFETY: `kill` with a pid this process just read from /proc and
        // confirmed belongs to this user.
        unsafe { libc::kill(*pid, libc::SIGTERM) };
    }

    Ok(!pids.is_empty())
}

/// Whether the widget should come up at login, with or without this app.
///
/// Writing the unit rather than shipping it: the file has to name the
/// binary's path, and in a development tree that is somewhere under
/// `osd/target`. A unit installed by a package will name `/usr/bin`, and
/// this leaves that one alone - it only ever writes under
/// `~/.config/systemd/user`.
pub fn set_osd_at_login(enabled: bool) -> Result<Value, String> {
    let path = user_unit_path();

    if enabled {
        let binary = find_osd()
            .ok_or_else(|| format!("{OSD_BINARY} is not installed, so it cannot start at login"))?;
        let parent = path.parent().ok_or("no systemd user directory")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
        std::fs::write(&path, unit_text(&binary))
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
        systemctl_user(&["daemon-reload"])?;
        systemctl_user(&["enable", OSD_UNIT])?;
        // `enable` and `start` separately, and the start only when nothing
        // is up yet. `enable --now` on a widget the app already spawned
        // would launch a second copy, which the single-instance widget
        // answers by *showing itself* and exiting - a unit that reads as
        // dead, and a widget that flashes across the screen for no reason.
        if !osd_is_running() {
            systemctl_user(&["start", OSD_UNIT])?;
        }
    } else {
        // Disable before removing: systemd cannot remove the symlinks for
        // a unit whose file has already gone, and would leave the widget
        // enabled-but-missing.
        let _ = systemctl_user(&["disable", "--now", OSD_UNIT]);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| format!("removing {}: {e}", path.display()))?;
        }
        let _ = systemctl_user(&["daemon-reload"]);
    }

    Ok(status())
}

/// Whether the app itself should start at login.
///
/// **Two mechanisms, because neither one covers Linux on its own.**
///
/// The XDG autostart entry is the standard, and it is what every other app
/// on the machine uses - but the thing that *reads* `~/.config/autostart`
/// is the session manager, and GNOME, KDE and XFCE have one while a bare
/// wlroots compositor does not. On Hyprland the entry is written, is
/// perfectly valid, and is then read by nobody: the setting looked broken
/// because it silently did nothing.
///
/// So a systemd user unit is written alongside it, pulled in by
/// `graphical-session.target` the way the widget's is. That covers the
/// sessions systemd manages, including Hyprland under uwsm.
///
/// It does **not** cover a compositor that reaches neither - no session
/// manager and no `graphical-session.target` - and no file this writes can.
/// That case is detected rather than papered over: [`status`] reports
/// `loginWorks: false` and the settings page shows the one line to add to
/// the compositor's own config.
pub fn set_app_at_login(enabled: bool) -> Result<Value, String> {
    let desktop = autostart_path();
    let unit = app_unit_path();

    if enabled {
        let binary =
            std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;

        let parent = desktop.parent().ok_or("no autostart directory")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
        std::fs::write(&desktop, desktop_entry(&binary))
            .map_err(|e| format!("writing {}: {e}", desktop.display()))?;

        // Best-effort, unlike the entry above: a session with no
        // `systemctl --user` at all is a session that still has the
        // desktop file, and failing the whole call would take away the
        // mechanism that does work there.
        if let Some(parent) = unit.parent() {
            let _ = std::fs::create_dir_all(parent);
            if std::fs::write(&unit, app_unit_text(&binary)).is_ok() {
                let _ = systemctl_user(&["daemon-reload"]);
                let _ = systemctl_user(&["enable", APP_UNIT]);
            }
        }
    } else {
        if desktop.exists() {
            std::fs::remove_file(&desktop)
                .map_err(|e| format!("removing {}: {e}", desktop.display()))?;
        }
        // Disable before removing, for the reason `set_osd_at_login` gives:
        // systemd cannot clean up the symlinks for a unit file that is
        // already gone. Not `--now` - this is the running app.
        let _ = systemctl_user(&["disable", APP_UNIT]);
        if unit.exists() {
            let _ = std::fs::remove_file(&unit);
        }
        let _ = systemctl_user(&["daemon-reload"]);
    }

    Ok(status())
}

/// Whether *something* in this session will act on what the toggle writes.
///
/// The two mechanisms have one reader each, so this asks after both: a
/// systemd-managed graphical session reaches the target the unit is wanted
/// by, and the desktops in [`DESKTOPS_WITH_A_SESSION_MANAGER`] read the
/// autostart entry themselves.
fn login_start_is_honoured() -> bool {
    graphical_session_active() || desktop_reads_autostart()
}

fn graphical_session_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "graphical-session.target"])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "active")
}

/// `XDG_CURRENT_DESKTOP` is colon-separated and not always spelled the way
/// the project spells itself (`ubuntu:GNOME`, `X-Cinnamon`), so this
/// matches loosely rather than comparing whole names.
fn desktop_reads_autostart() -> bool {
    let Ok(current) = std::env::var("XDG_CURRENT_DESKTOP") else {
        return false;
    };
    current.split(':').any(|name| {
        let name = name.trim().to_ascii_lowercase();
        DESKTOPS_WITH_A_SESSION_MANAGER.iter().any(|known| name.contains(known))
    })
}

fn unit_text(binary: &Path) -> String {
    format!(
        "# Written by Pyren. Delete it, or turn the setting off, to remove it.\n\
         [Unit]\n\
         Description=Pyren on-screen display for the performance key\n\
         PartOf=graphical-session.target\n\
         After=graphical-session.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=2\n\n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        binary.display()
    )
}

/// The app's own unit. Deliberately unlike the widget's in two places:
/// no `Restart=`, because quitting Pyren from the tray has to mean quit
/// rather than "until systemd notices"; and `PartOf=`, so logging out
/// takes it with the session instead of leaving a window behind.
fn app_unit_text(binary: &Path) -> String {
    format!(
        "# Written by Pyren. Delete it, or turn the setting off, to remove it.\n\
         [Unit]\n\
         Description=Pyren gaming hub\n\
         PartOf=graphical-session.target\n\
         After=graphical-session.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={}\n\
         Restart=no\n\n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        binary.display()
    )
}

fn desktop_entry(binary: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Pyren\n\
         Comment=Gaming hub for HP OMEN laptops\n\
         Exec={}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        binary.display()
    )
}

/// Whether a `pyren-osd` belonging to this user is already up.
///
/// Read from `/proc` rather than shelled out to `pgrep`, which is not
/// installed everywhere and would be a process spawn on every launch.
/// `comm` is the thread name - 15 characters at most, which `pyren-osd`
/// fits inside with room to spare.
fn osd_is_running() -> bool {
    !osd_pids().is_empty()
}

/// Every `pyren-osd` belonging to this user. Normally one - the widget is
/// single-instance - but a list rather than an `Option` because a stop
/// that leaves a second copy behind is worse than no stop at all.
fn osd_pids() -> Vec<i32> {
    let uid = unsafe { libc::getuid() };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.join("comm").exists() {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(path.join("comm")) else {
            continue;
        };
        if comm.trim() != OSD_BINARY {
            continue;
        }
        // Somebody else's widget is not ours to stop - and on a shared
        // machine it is not ours to see either.
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != uid {
            continue;
        }
        // A widget this app spawned and then stopped becomes a zombie:
        // nothing here waits on it, so it keeps its /proc entry, its name
        // and its pid until the app itself exits. Counting one as running
        // is not cosmetic - it makes the settings toggle a one-way
        // switch, because turning the widget back on finds it "already
        // up" and starts nothing.
        if is_zombie(&path) {
            continue;
        }
        if let Some(pid) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse().ok()) {
            pids.push(pid);
        }
    }
    pids
}

/// Whether `/proc/<pid>` belongs to a process that has exited and not yet
/// been waited on.
///
/// From `status` rather than `stat`: the second field of `stat` is the
/// command in parentheses and a command containing one would break a
/// naive split, and this reads processes it did not name.
fn is_zombie(proc_dir: &Path) -> bool {
    std::fs::read_to_string(proc_dir.join("status")).is_ok_and(|status| {
        status
            .lines()
            .find_map(|line| line.strip_prefix("State:"))
            .is_some_and(|state| state.trim_start().starts_with('Z'))
    })
}

/// Where the widget's binary might be, most-likely first.
///
/// The development tree is last and is not a fallback anybody should rely
/// on. Leaving it out would mean that the one setup where this matters
/// most, the machine this is being written on, is the one setup where it
/// does not work.
fn find_osd() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // An install puts them side by side.
            candidates.push(dir.join(OSD_BINARY));
            // …and `cargo`/`tauri dev` puts the app at
            // <repo>/app/src-tauri/target/<profile>/pyren, with the widget
            // in its own workspace four levels up.
            for profile in ["debug", "release"] {
                candidates.push(
                    dir.join("../../../../osd/target").join(profile).join(OSD_BINARY),
                );
            }
        }
    }

    candidates.push(PathBuf::from("/usr/local/bin").join(OSD_BINARY));
    candidates.push(PathBuf::from("/usr/bin").join(OSD_BINARY));

    if let Ok(path) = std::env::var("PATH") {
        candidates.extend(path.split(':').map(|dir| Path::new(dir).join(OSD_BINARY)));
    }

    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|found| found.canonicalize().unwrap_or(found))
}

fn user_unit_path() -> PathBuf {
    config_home().join("systemd/user").join(OSD_UNIT)
}

fn autostart_path() -> PathBuf {
    config_home().join("autostart").join(AUTOSTART_ENTRY)
}

fn app_unit_path() -> PathBuf {
    config_home().join("systemd/user").join(APP_UNIT)
}

fn config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".config")
        })
}

fn unit_enabled(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", unit])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "enabled")
}

fn systemctl_user(args: &[&str]) -> Result<(), String> {
    let mut command = Command::new("systemctl");
    command.arg("--user").args(args);
    let output = command.output().map_err(|e| format!("running systemctl --user: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("systemctl --user {} failed", args.join(" "))
    } else {
        stderr
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unit has to name the binary it was written for: a development
    /// build lives under `osd/target`, and a unit pointing at `/usr/bin`
    /// would silently fail to start on the machine that wrote it.
    #[test]
    fn the_unit_names_the_binary_it_was_written_for() {
        let text = unit_text(Path::new("/home/someone/pyren/osd/target/debug/pyren-osd"));
        assert!(text.contains("ExecStart=/home/someone/pyren/osd/target/debug/pyren-osd"));
        assert!(text.contains("WantedBy=graphical-session.target"));
        assert!(text.contains("Written by Pyren"), "it must say who to blame for the file");
    }

    #[test]
    fn the_autostart_entry_is_a_desktop_file_any_desktop_will_honour() {
        let text = desktop_entry(Path::new("/usr/bin/pyren"));
        assert!(text.starts_with("[Desktop Entry]"));
        assert!(text.contains("Exec=/usr/bin/pyren"));
        assert!(text.contains("Type=Application"));
    }

    /// Quitting from the tray has to mean quit: a `Restart=` here would
    /// have systemd bring the window straight back, which is the widget's
    /// behaviour and exactly wrong for the app.
    #[test]
    fn the_apps_unit_does_not_restart_itself() {
        let text = app_unit_text(Path::new("/usr/local/bin/pyren"));
        assert!(text.contains("ExecStart=/usr/local/bin/pyren"));
        assert!(text.contains("Restart=no"));
        assert!(text.contains("WantedBy=graphical-session.target"));
        assert!(text.contains("Written by Pyren"), "it must say who to blame for the file");
    }

    /// The compositors this exists for. A session manager reads the
    /// autostart entry; a bare wlroots compositor does not, and calling
    /// that "works" is what made the setting fail in silence.
    #[test]
    fn a_bare_wlroots_session_is_not_mistaken_for_one_with_a_session_manager() {
        // SAFETY: single-threaded test, and the variable is read back
        // immediately by the function under test.
        let check = |value: &str| {
            std::env::set_var("XDG_CURRENT_DESKTOP", value);
            desktop_reads_autostart()
        };

        assert!(check("GNOME"));
        assert!(check("ubuntu:GNOME"), "the distro may prefix its own name");
        assert!(check("KDE"));
        assert!(check("X-Cinnamon"), "and some spell it with a prefix");

        assert!(!check("Hyprland"));
        assert!(!check("sway"));
        assert!(!check("niri"));
        assert!(!check(""));
    }

    /// Every file live under the user's own config directory - nothing here
    /// writes to /etc or /usr, and nothing here needs a password.
    #[test]
    fn everything_this_writes_is_under_the_users_own_config() {
        let home = config_home();
        assert!(user_unit_path().starts_with(&home));
        assert!(autostart_path().starts_with(&home));
        assert!(app_unit_path().starts_with(&home));
    }
}
