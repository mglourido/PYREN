//! Who is sitting at the machine, and whether they are one of Pyren's
//! users.
//!
//! Both questions are put to the system rather than to a file of ours: the
//! first to logind, the second to the account database. They sit behind
//! one trait so the rest of the crate can be tested against a machine with
//! any number of people on it.
//!
//! Every answer here can also be *no answer* - `loginctl` timing out, the
//! bus restarting, a directory service not replying - and that is kept
//! apart from "nobody" and from "not a member" all the way up
//! ([`Unknown`]). The two are acted on in opposite ways: nobody at the
//! seat wakes a daemon that is standing down, and a user who is not a
//! member makes one stand down. A lookup that merely failed must do
//! neither.

use std::ffi::{CStr, CString};

use pyren_core::process;

/// Whoever holds the active session on the seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveUser {
    pub uid: u32,
    pub name: String,
}

/// The question could not be put, or was not answered. Not "no".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unknown;

/// The answers this crate needs about the machine's accounts.
pub trait Directory: Send + Sync {
    /// The person in front of the machine, if there is one. `Ok(None)` at
    /// the login screen and with nobody logged in.
    fn active(&self) -> Result<Option<ActiveUser>, Unknown>;
    /// Whether this uid is one of Pyren's users - somebody the daemon's
    /// socket admits.
    fn is_member(&self, uid: u32) -> Result<bool, Unknown>;
    /// The account's name, for showing. `None` for a uid nobody has any
    /// more.
    fn name(&self, uid: u32) -> Option<String>;
    /// Something cheap to read that changes when the seat changes hands.
    /// Not an answer - only a reason to ask [`Directory::active`] now
    /// rather than at the next poll.
    fn seat_hint(&self) -> Option<String> {
        None
    }
}

/// The real machine: `loginctl` and the account database.
pub struct System {
    group: String,
}

impl System {
    pub fn new() -> Self {
        Self {
            group: pyren_core::socket_group(),
        }
    }
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

impl Directory for System {
    fn active(&self) -> Result<Option<ActiveUser>, Unknown> {
        // Asked of `loginctl` rather than read from /run/systemd/seats,
        // which holds the same answer under a first line saying "This is
        // private data. Do not parse."
        let session = loginctl(&["show-seat", SEAT, "--property=ActiveSession", "--value"])?;
        let session = session.trim();
        if session.is_empty() {
            return Ok(None);
        }
        let properties = loginctl(&[
            "show-session",
            session,
            "--property=User",
            "--property=Name",
            "--property=Class",
        ])?;
        Ok(person(&properties))
    }

    fn is_member(&self, uid: u32) -> Result<bool, Unknown> {
        // The daemon's own user is always admitted by the socket, group or
        // no group - which is the whole of the answer in development,
        // where the daemon runs as the person using it.
        // SAFETY: geteuid has no preconditions.
        if uid == unsafe { libc::geteuid() } {
            return Ok(true);
        }
        in_group(uid, &self.group)
    }

    fn name(&self, uid: u32) -> Option<String> {
        account(uid).ok().flatten().map(|account| account.name)
    }

    fn seat_hint(&self) -> Option<String> {
        // The foreground virtual terminal. Switching between two people's
        // sessions is a switch between two of these, and the kernel
        // publishes it for anyone to read.
        std::fs::read_to_string("/sys/class/tty/tty0/active").ok()
    }
}

/// The seat with the laptop's own screen and keyboard. A second seat is a
/// second set of hardware, and this daemon drives the first one's.
const SEAT: &str = "seat0";

/// Any failure is [`Unknown`], including `loginctl` not being there and
/// the seat not existing: a machine without logind is one where nobody can
/// be told apart, and "nothing is known" is what leaves the daemon serving
/// the settings it has.
fn loginctl(args: &[&str]) -> Result<String, Unknown> {
    let mut command = process::command("loginctl");
    command.args(args);
    let output =
        process::output_within(&mut command, process::DEFAULT_TIMEOUT).map_err(|_| Unknown)?;
    if !output.status.success() {
        return Err(Unknown);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads `loginctl show-session`'s `Key=Value` lines into the person they
/// describe.
///
/// Only a `user` session counts. The login screen is a session too
/// (`greeter`, owned by the display manager's account), and treating it as
/// somebody arriving would have the daemon stand down every time a person
/// logs out. Root is left out for the same reason a greeter is: it is
/// nobody's desktop, and it has no settings of its own to switch to.
fn person(properties: &str) -> Option<ActiveUser> {
    let field = |key: &str| {
        properties
            .lines()
            .filter_map(|line| line.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, value)| value.trim())
    };
    if field("Class")? != "user" {
        return None;
    }
    let uid = field("User")?.parse::<u32>().ok()?;
    if uid == 0 {
        return None;
    }
    Some(ActiveUser {
        uid,
        name: field("Name")
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| uid.to_string()),
    })
}

struct Account {
    name: String,
    gid: u32,
}

/// Room for one passwd or group entry to begin with. Both calls answer
/// `ERANGE` when it is not enough, and are asked again with more.
const ENTRY_BUFFER: usize = 16 * 1024;
/// Where asking again stops. A group entry is its member list, so a large
/// site's can be long - but not this long.
const ENTRY_BUFFER_MAX: usize = 4 * 1024 * 1024;

/// The errors glibc documents for "no such entry", as opposed to "could
/// not look".
fn means_not_found(rc: libc::c_int) -> bool {
    matches!(rc, libc::ENOENT | libc::ESRCH | libc::EBADF | libc::EPERM)
}

/// `Ok(None)` is an account that does not exist; `Err` is a lookup that
/// did not get an answer.
fn account(uid: u32) -> Result<Option<Account>, Unknown> {
    let mut size = ENTRY_BUFFER;
    loop {
        let mut buffer = vec![0 as libc::c_char; size];
        // SAFETY: a zeroed passwd is valid storage for getpwuid_r to fill.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call, and `buffer` - which
        // the entry's strings point into - outlives the reads below.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        if rc == libc::ERANGE && size < ENTRY_BUFFER_MAX {
            size *= 4;
            continue;
        }
        if rc != 0 {
            return if means_not_found(rc) {
                Ok(None)
            } else {
                Err(Unknown)
            };
        }
        if found.is_null() {
            return Ok(None);
        }
        // SAFETY: on success pw_name is a NUL-terminated string in `buffer`.
        let name = unsafe { CStr::from_ptr(entry.pw_name) }
            .to_string_lossy()
            .into_owned();
        return Ok(Some(Account {
            name,
            gid: entry.pw_gid,
        }));
    }
}

/// The reentrant lookup, because this runs on the watcher's thread while
/// `pyren_core::socket` may be in `getgrnam` on another.
fn group_id(name: &str) -> Result<Option<u32>, Unknown> {
    let Ok(c_name) = CString::new(name) else {
        return Ok(None);
    };
    let mut size = ENTRY_BUFFER;
    loop {
        let mut buffer = vec![0 as libc::c_char; size];
        // SAFETY: a zeroed group is valid storage for getgrnam_r to fill.
        let mut entry: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call.
        let rc = unsafe {
            libc::getgrnam_r(
                c_name.as_ptr(),
                &mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        if rc == libc::ERANGE && size < ENTRY_BUFFER_MAX {
            size *= 4;
            continue;
        }
        if rc != 0 {
            return if means_not_found(rc) {
                Ok(None)
            } else {
                Err(Unknown)
            };
        }
        return Ok((!found.is_null()).then_some(entry.gr_gid));
    }
}

/// Whether the account is in the group, as its primary group or any other.
///
/// Read from the account database rather than from a running process's
/// credentials: `usermod -aG` takes effect here at once, and it is the
/// admin's decision that is being asked about, not what one login session
/// happened to start with.
fn in_group(uid: u32, group: &str) -> Result<bool, Unknown> {
    let (Some(account), Some(wanted)) = (account(uid)?, group_id(group)?) else {
        return Ok(false);
    };
    if account.gid == wanted {
        return Ok(true);
    }
    let Ok(c_name) = CString::new(account.name) else {
        return Ok(false);
    };
    let mut groups = vec![0 as libc::gid_t; 64];
    loop {
        let mut count = groups.len() as libc::c_int;
        // SAFETY: `groups` has room for `count` entries, and `count` is
        // updated to how many there are.
        let rc = unsafe {
            libc::getgrouplist(
                c_name.as_ptr(),
                account.gid,
                groups.as_mut_ptr(),
                &mut count,
            )
        };
        let count = count.max(0) as usize;
        if rc >= 0 {
            return Ok(groups[..count.min(groups.len())].contains(&wanted));
        }
        // -1 means the list was too short; `count` says how long it is.
        if count <= groups.len() {
            return Err(Unknown);
        }
        groups.resize(count, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_session_names_its_person() {
        let found = person("User=1000\nName=ana\nClass=user\n");
        assert_eq!(
            found,
            Some(ActiveUser {
                uid: 1000,
                name: "ana".into()
            })
        );
    }

    #[test]
    fn the_login_screen_is_nobody() {
        assert_eq!(person("User=967\nName=sddm\nClass=greeter\n"), None);
    }

    #[test]
    fn root_is_nobody() {
        assert_eq!(person("User=0\nName=root\nClass=user\n"), None);
    }

    #[test]
    fn a_session_with_no_class_is_nobody() {
        assert_eq!(person("User=1000\nName=ana\n"), None);
        assert_eq!(person(""), None);
    }

    #[test]
    fn a_missing_name_falls_back_to_the_uid() {
        assert_eq!(
            person("Class=user\nUser=1001\n").map(|p| p.name),
            Some("1001".to_string())
        );
    }

    #[test]
    fn the_user_running_this_is_in_their_own_primary_group() {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let Ok(Some(account)) = account(uid) else {
            // A uid with no passwd entry (some containers): nothing to ask.
            return;
        };
        // SAFETY: getgrgid returns static storage read immediately, and no
        // other test in this binary looks groups up non-reentrantly.
        let group = unsafe {
            let entry = libc::getgrgid(account.gid);
            if entry.is_null() {
                return;
            }
            CStr::from_ptr((*entry).gr_name)
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(in_group(uid, &group), Ok(true));
        assert_eq!(in_group(uid, "pyren-no-such-group-anywhere"), Ok(false));
    }

    #[test]
    fn a_uid_nobody_has_is_in_no_group_and_has_no_name() {
        let system = System::new();
        // 4294967294 is `nobody`'s overflow neighbour: unassigned everywhere.
        assert_eq!(system.name(4_294_967_294), None);
        assert_eq!(in_group(4_294_967_294, "root"), Ok(false));
    }
}
