//! Who is sitting at the machine, and whether they are one of Pyren's
//! users.
//!
//! Both questions are put to the system rather than to a file of ours: the
//! first to logind, the second to the account database. They sit behind
//! one trait so the rest of the crate can be tested against a machine with
//! any number of people on it.

use std::ffi::{CStr, CString};

use pyren_core::process;

/// Whoever holds the active session on the seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveUser {
    pub uid: u32,
    pub name: String,
}

/// The answers this crate needs about the machine's accounts.
pub trait Directory: Send + Sync {
    /// The person in front of the machine, if there is one. `None` at the
    /// login screen, with nobody logged in, and wherever logind is absent.
    fn active(&self) -> Option<ActiveUser>;
    /// Whether this uid is one of Pyren's users - somebody the daemon's
    /// socket admits.
    fn is_member(&self, uid: u32) -> bool;
    /// The account's name, for showing. `None` for a uid nobody has any
    /// more.
    fn name(&self, uid: u32) -> Option<String>;
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
    fn active(&self) -> Option<ActiveUser> {
        // Asked of `loginctl` rather than read from /run/systemd/seats,
        // which holds the same answer under a first line saying "This is
        // private data. Do not parse."
        let session = loginctl(&["show-seat", SEAT, "--property=ActiveSession", "--value"])?;
        let session = session.trim();
        if session.is_empty() {
            return None;
        }
        let properties = loginctl(&[
            "show-session",
            session,
            "--property=User",
            "--property=Name",
            "--property=Class",
        ])?;
        person(&properties)
    }

    fn is_member(&self, uid: u32) -> bool {
        // The daemon's own user is always admitted by the socket, group or
        // no group - which is the whole of the answer in development,
        // where the daemon runs as the person using it.
        // SAFETY: geteuid has no preconditions.
        uid == unsafe { libc::geteuid() } || in_group(uid, &self.group)
    }

    fn name(&self, uid: u32) -> Option<String> {
        account(uid).map(|account| account.name)
    }
}

/// The seat with the laptop's own screen and keyboard. A second seat is a
/// second set of hardware, and this daemon drives the first one's.
const SEAT: &str = "seat0";

fn loginctl(args: &[&str]) -> Option<String> {
    let mut command = process::command("loginctl");
    command.args(args);
    let output = process::output_within(&mut command, process::DEFAULT_TIMEOUT).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
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

/// Room for one passwd or group entry. Both calls report `ERANGE` when it
/// is not enough, and a group with members by the hundred needs more than
/// the customary 1 KiB.
const ENTRY_BUFFER: usize = 16 * 1024;

fn account(uid: u32) -> Option<Account> {
    let mut buffer = vec![0 as libc::c_char; ENTRY_BUFFER];
    // SAFETY: a zeroed passwd is valid storage for getpwuid_r to fill.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call, and `buffer` - which the
    // entry's strings point into - outlives the reads below.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() {
        return None;
    }
    // SAFETY: on success pw_name is a NUL-terminated string in `buffer`.
    let name = unsafe { CStr::from_ptr(entry.pw_name) }
        .to_string_lossy()
        .into_owned();
    Some(Account {
        name,
        gid: entry.pw_gid,
    })
}

/// The reentrant lookup, because this runs on the watcher's thread while
/// `pyren_core::socket` may be in `getgrnam` on another.
fn group_id(name: &str) -> Option<u32> {
    let c_name = CString::new(name).ok()?;
    let mut buffer = vec![0 as libc::c_char; ENTRY_BUFFER];
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
    (rc == 0 && !found.is_null()).then_some(entry.gr_gid)
}

/// Whether the account is in the group, as its primary group or any other.
///
/// Read from the account database rather than from a running process's
/// credentials: `usermod -aG` takes effect here at once, and it is the
/// admin's decision that is being asked about, not what one login session
/// happened to start with.
fn in_group(uid: u32, group: &str) -> bool {
    let (Some(account), Some(wanted)) = (account(uid), group_id(group)) else {
        return false;
    };
    if account.gid == wanted {
        return true;
    }
    let Ok(c_name) = CString::new(account.name) else {
        return false;
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
            return groups[..count.min(groups.len())].contains(&wanted);
        }
        // -1 means the list was too short; `count` says how long it is.
        if count <= groups.len() {
            return false;
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
        let Some(account) = account(uid) else {
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
        assert!(in_group(uid, &group));
        assert!(!in_group(uid, "pyren-no-such-group-anywhere"));
    }

    #[test]
    fn a_uid_nobody_has_is_in_no_group_and_has_no_name() {
        let system = System::new();
        // 4294967294 is `nobody`'s overflow neighbour: unassigned everywhere.
        assert_eq!(system.name(4_294_967_294), None);
        assert!(!in_group(4_294_967_294, "root"));
    }
}
