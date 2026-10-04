//! The daemon's Unix-socket front door, and the permissions on it.
//!
//! The socket *is* the trust boundary (see `docs/00-design-plan.md`): the
//! daemon is root and the app never is, so anyone who can open this socket
//! can change power modes and, later, drive fans. Access is therefore
//! decided by the filesystem — the standard Unix answer, and the only one
//! the kernel enforces for us — rather than by anything in the protocol.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;

use crate::log_warn;
use crate::{Registry, Request, Response};

/// Group whose members may talk to the daemon. Overridable with
/// `PYREN_SOCKET_GROUP` so a packager can use whatever name the
/// distribution prefers.
const DEFAULT_GROUP: &str = "pyren";

/// Holds the system-control singleton for the entire daemon lifetime.
/// This is separate from the lock protecting one IPC socket pathname.
pub struct DaemonInstanceLock {
    _listener: UnixListener,
}

fn acquire_instance_lock_named(name: &[u8]) -> std::io::Result<DaemonInstanceLock> {
    let address = SocketAddr::from_abstract_name(name)?;
    // An abstract AF_UNIX address is unique in the kernel namespace across
    // processes and UIDs. It has no filesystem pathname to unlink, no stale
    // PID, and is released when the final listener descriptor closes.
    let listener = UnixListener::bind_addr(&address).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            std::io::Error::new(
                e.kind(),
                "another PYREN daemon already controls this system",
            )
        } else {
            e
        }
    })?;
    Ok(DaemonInstanceLock {
        _listener: listener,
    })
}

/// Acquire this before constructing hardware modules or starting threads.
/// Unlike the IPC socket lock, this fixed identity spans root and development
/// users that can otherwise reach the same physical controls.
pub fn acquire_daemon_instance() -> std::io::Result<DaemonInstanceLock> {
    acquire_instance_lock_named(b"pyren.daemon.system-control.v1")
}

/// Who ended up being able to reach the socket.
///
/// Deliberately not "everyone, read-only": a non-member gets *nothing*.
/// Splitting reads from writes would mean opening a root daemon's socket to
/// every local process — including sandboxed and compromised ones — to save
/// an admin one `usermod -aG`. Vitals are not worth that. Whoever should
/// see them joins the group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    /// Only the user the daemon runs as. This is the right answer in
    /// development (that user is also the one running the app) and a
    /// misconfiguration in production (nobody but root can connect).
    OwnerOnly,
    /// The daemon's user, plus members of this group.
    Group(String),
}

impl Audience {
    /// One line for the startup log, saying who can now connect.
    pub fn summary(&self) -> String {
        match self {
            Self::OwnerOnly => "restricted to the daemon's own user".to_string(),
            Self::Group(g) => format!("restricted to root and group '{g}'"),
        }
    }
}

/// Configured group name, before we know whether it exists on this machine.
///
/// Public because the installer has to create this group, and the two must
/// not be able to disagree about its name.
pub fn socket_group() -> String {
    std::env::var("PYREN_SOCKET_GROUP").unwrap_or_else(|_| DEFAULT_GROUP.to_string())
}

/// gid for a group name, or `None` if no such group exists here.
///
/// `getgrnam` returns a pointer into static storage and is only safe while
/// no other thread is in a `getgr*` call. This runs once, from `main`,
/// before any connection thread exists, and nothing else in the daemon
/// looks up groups.
pub(crate) fn lookup_gid(name: &str) -> Option<u32> {
    let c_name = CString::new(name).ok()?;
    // SAFETY: c_name outlives the call; the returned pointer is read
    // immediately and never stored.
    unsafe {
        let entry = libc::getgrnam(c_name.as_ptr());
        if entry.is_null() {
            None
        } else {
            Some((*entry).gr_gid)
        }
    }
}

pub(crate) fn chown_group(path: &Path, gid: u32) -> std::io::Result<()> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // uid_t of -1 means "leave the owner alone".
    // SAFETY: c_path is a valid NUL-terminated path for the duration of the call.
    let rc = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Binds `path` with permissions that keep other local users out.
///
/// The window between `bind` and `chmod` is closed by setting the umask
/// first, so the socket is never briefly world-writable: it is created
/// `0600` and only widened to `0660` once it belongs to the right group.
struct BoundListener {
    listener: UnixListener,
    _lock: File,
}

impl Deref for BoundListener {
    type Target = UnixListener;

    fn deref(&self) -> &Self::Target {
        &self.listener
    }
}

fn bind_restricted(path: &Path, group: &str) -> std::io::Result<(BoundListener, Audience)> {
    let gid = lookup_gid(group);

    if let Some(parent) = path.parent() {
        // Only touch a directory we are the ones creating. The development
        // fallback lives in /tmp, and tightening *that* would be a nasty
        // surprise.
        let ours = !parent.exists();
        std::fs::create_dir_all(parent)?;
        if ours {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o750))?;
            if let Some(gid) = gid {
                let _ = chown_group(parent, gid);
            }
        }
    }

    // The lock is a separate persistent inode: unlinking a socket after a
    // crash is safe only when no other instance still owns this endpoint.
    // Never remove the lock file, including at shutdown, or waiters could
    // acquire different inodes and both believe they own the path.
    let lock_path = path.with_extension(format!(
        "{}lock",
        path.extension()
            .map_or(String::new(), |ext| format!("{}.", ext.to_string_lossy()))
    ));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)?;
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "socket lock must be a regular file owned by the daemon user",
        ));
    }
    // SAFETY: lock is a live file descriptor. The kernel releases the flock
    // when BoundListener (and its file) is dropped.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "another daemon owns the socket endpoint",
        ));
    }

    // A stale socket file from an unclean shutdown would make bind() fail.
    let _ = std::fs::remove_file(path);

    // umask is process-wide, so this briefly tightens any file another
    // thread happens to create. The window is two syscalls long and the
    // worst case is a config file that ends up 0600 in a daemon that runs
    // as root anyway - cheaper than the alternative, which is a socket that
    // is world-connectable between bind() and chmod().
    // SAFETY: umask only reads and replaces a per-process value.
    let previous_umask = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(path);
    unsafe { libc::umask(previous_umask) };
    let listener = listener?;

    let audience = match gid {
        Some(gid) if chown_group(path, gid).is_ok() => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
            Audience::Group(group.to_string())
        }
        // No such group, or we are not privileged enough to hand the socket
        // to it. Either way the safe outcome is the same: leave it 0600.
        _ => Audience::OwnerOnly,
    };

    Ok((
        BoundListener {
            listener,
            _lock: lock,
        },
        audience,
    ))
}

/// Runs the daemon's IPC server: binds `path` as a Unix domain socket and
/// serves newline-delimited JSON requests/responses forever, one thread per
/// connection. This call blocks the calling thread.
///
/// Intentionally simple (std threads, blocking IO, one request in flight
/// per connection) rather than async - the socket is a local low-throughput
/// control plane, not a hot path. Revisit only if that stops being true.
///
/// `on_bind` is handed the resulting [`Audience`] so the caller can report
/// it; an `OwnerOnly` result on a root daemon means no desktop user can
/// connect, which is worth saying out loud.
pub fn serve_unix_socket(
    path: &str,
    registry: Arc<Registry>,
    on_bind: impl FnOnce(&Audience),
) -> std::io::Result<()> {
    let (listener, audience) = bind_restricted(Path::new(path), &socket_group())?;
    on_bind(&audience);

    for stream in listener.incoming() {
        let stream = stream?;
        let registry = Arc::clone(&registry);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &registry) {
                log_warn!("connection error: {e}");
            }
        });
    }

    Ok(())
}

fn handle_connection(stream: UnixStream, registry: &Registry) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => registry.dispatch(req),
            Err(e) => Response::err(
                0,
                crate::ErrorKind::MalformedRequest,
                format!("invalid request: {e}"),
            ),
        };

        // The fallback has to be the same shape as everything else, or a
        // client that branches on `error.kind` meets a bare string on the
        // one reply it can do least about.
        let mut payload = serde_json::to_string(&response).unwrap_or_else(|_| {
            "{\"id\":0,\"error\":{\"kind\":\"internal\",\
             \"message\":\"the daemon could not serialise its own reply\"}}"
                .to_string()
        });
        payload.push('\n');
        writer.write_all(payload.as_bytes())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory to bind a socket in, with its mode set explicitly.
    ///
    /// `bind_restricted` tightens `umask` for the two syscalls around
    /// `bind()`, and umask is **process-wide** - so a fixture created by
    /// another test at that exact moment came out `0600`, with no execute
    /// bit, and binding inside it failed with `EACCES`. Once every few
    /// runs, and only when the whole crate's tests ran together. Setting
    /// the mode after creating it is immune to whatever umask was in
    /// force.
    fn fixture(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pyren-socket-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The name of a group this test process is actually in, so the
    /// group-membership path can be exercised without root.
    fn own_group() -> Option<String> {
        // SAFETY: getegid cannot fail; getgrgid is read immediately.
        unsafe {
            let entry = libc::getgrgid(libc::getegid());
            if entry.is_null() {
                return None;
            }
            std::ffi::CStr::from_ptr((*entry).gr_name)
                .to_str()
                .ok()
                .map(str::to_string)
        }
    }

    #[test]
    fn an_unknown_group_leaves_the_socket_to_its_owner_alone() {
        let path = fixture("nogroup").join("daemon.sock");
        let (_listener, audience) =
            bind_restricted(&path, "pyren-group-that-does-not-exist").unwrap();

        assert_eq!(audience, Audience::OwnerOnly);
        assert_eq!(
            mode_of(&path),
            0o600,
            "must not be reachable by other users"
        );
    }

    #[test]
    fn a_known_group_gets_group_access() {
        let Some(group) = own_group() else { return };
        let path = fixture("group").join("daemon.sock");
        let (_listener, audience) = bind_restricted(&path, &group).unwrap();

        assert_eq!(audience, Audience::Group(group));
        assert_eq!(mode_of(&path), 0o660);
    }

    /// Never world-accessible, whichever branch was taken.
    #[test]
    fn other_users_are_never_granted_access() {
        let path = fixture("others").join("daemon.sock");
        let (_listener, _) = bind_restricted(&path, &own_group().unwrap_or_default()).unwrap();

        assert_eq!(mode_of(&path) & 0o007, 0);
    }

    #[test]
    fn a_runtime_directory_we_create_is_restricted_too() {
        let path = fixture("mkdir").join("run").join("daemon.sock");
        let (_listener, _) = bind_restricted(&path, &own_group().unwrap_or_default()).unwrap();

        assert_eq!(mode_of(path.parent().unwrap()), 0o750);
    }

    /// /tmp is the development socket's parent. Tightening a directory we
    /// did not create would be a nasty surprise.
    #[test]
    fn an_existing_directory_is_left_as_it_was() {
        let dir = fixture("existing");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let (_listener, _) = bind_restricted(&dir.join("daemon.sock"), "pyren").unwrap();

        assert_eq!(mode_of(&dir), 0o777);
    }

    #[test]
    fn a_stale_socket_file_does_not_block_startup() {
        let path = fixture("stale").join("daemon.sock");
        std::fs::write(&path, "left over from a crash").unwrap();

        assert!(bind_restricted(&path, "pyren").is_ok());
    }

    #[test]
    fn a_second_instance_cannot_replace_a_live_socket() {
        let path = fixture("live-owner").join("daemon.sock");
        let (first, _) = bind_restricted(&path, "pyren").expect("first instance binds");

        // A separate test-harness process has the same effective uid as
        // this one, just as two privileged service instances do.
        let second = crate::process::command(std::env::current_exe().unwrap())
            .arg("second_instance_bind_helper")
            .arg("--nocapture")
            .env("PYREN_TEST_LIVE_SOCKET", &path)
            .status()
            .expect("run second instance");

        assert!(
            first.local_addr().is_ok(),
            "the first listener is still alive"
        );
        assert!(
            second.success(),
            "a second process with the same effective uid unlinked and rebound the live path"
        );
    }

    #[test]
    fn an_alternate_socket_cannot_start_a_second_daemon() {
        let dir = fixture("different-endpoints");
        let instance_name = format!("pyren-test-different-endpoints-{}", std::process::id());
        let first_path = dir.join("first.sock");
        let second_path = dir.join("second.sock");
        let _instance = acquire_instance_lock_named(instance_name.as_bytes())
            .expect("first daemon owns hardware");
        let (first, _) = bind_restricted(&first_path, "pyren").expect("first daemon binds");
        let second = crate::process::command(std::env::current_exe().unwrap())
            .arg("alternate_socket_bind_helper")
            .arg("--nocapture")
            .env("PYREN_TEST_ALTERNATE_SOCKET", &second_path)
            .env("PYREN_TEST_INSTANCE_LOCK", &instance_name)
            .status()
            .expect("run second daemon process");
        assert!(first.local_addr().is_ok());
        assert!(
            second.success(),
            "a second daemon acquired a distinct endpoint in the same control domain"
        );
    }

    #[test]
    fn alternate_socket_bind_helper() {
        let Some(path) = std::env::var_os("PYREN_TEST_ALTERNATE_SOCKET") else {
            return;
        };
        let instance_name = std::env::var("PYREN_TEST_INSTANCE_LOCK").unwrap();
        let rebound = acquire_instance_lock_named(instance_name.as_bytes()).is_ok()
            && bind_restricted(Path::new(&path), "pyren").is_ok();
        std::process::exit(if rebound { 42 } else { 0 });
    }

    #[test]
    fn abrupt_process_exit_releases_the_instance_lock() {
        let name = format!("pyren-test-dead-instance-{}", std::process::id());
        let child = crate::process::command(std::env::current_exe().unwrap())
            .arg("instance_lock_exit_helper")
            .arg("--nocapture")
            .env("PYREN_TEST_LOCK_AND_EXIT", &name)
            .status()
            .expect("run owner process");
        assert!(child.success(), "owner process could not acquire lock");
        let _new_owner = acquire_instance_lock_named(name.as_bytes())
            .expect("kernel must release lock after process death");
    }

    #[test]
    fn instance_lock_exit_helper() {
        let Some(name) = std::env::var_os("PYREN_TEST_LOCK_AND_EXIT") else {
            return;
        };
        let _instance =
            acquire_instance_lock_named(name.as_encoded_bytes()).expect("child owns lock");
        std::process::exit(0);
    }

    #[test]
    fn killed_owner_releases_the_instance_lock() {
        let name = format!("pyren-test-killed-instance-{}", std::process::id());
        let mut child = crate::process::command(std::env::current_exe().unwrap())
            .arg("instance_lock_kill_helper")
            .arg("--nocapture")
            .env("PYREN_TEST_LOCK_UNTIL_KILLED", &name)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start owner process");
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        loop {
            assert_ne!(
                output.read_line(&mut ready).expect("owner readiness line"),
                0
            );
            if ready.contains("INSTANCE_LOCK_READY") {
                break;
            }
            ready.clear();
        }
        assert!(acquire_instance_lock_named(name.as_bytes()).is_err());
        child.kill().expect("kill lock owner");
        child.wait().expect("reap lock owner");
        let _new_owner =
            acquire_instance_lock_named(name.as_bytes()).expect("SIGKILL must release lock");
    }

    #[test]
    fn instance_lock_kill_helper() {
        let Some(name) = std::env::var_os("PYREN_TEST_LOCK_UNTIL_KILLED") else {
            return;
        };
        let _instance =
            acquire_instance_lock_named(name.as_encoded_bytes()).expect("child owns lock");
        println!("INSTANCE_LOCK_READY");
        std::io::stdout().flush().unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn instance_descriptor_is_close_on_exec() {
        let name = format!("pyren-test-cloexec-{}", std::process::id());
        let instance = acquire_instance_lock_named(name.as_bytes()).unwrap();
        // SAFETY: the listener is alive and owns a valid descriptor.
        let flags = unsafe { libc::fcntl(instance._listener.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn second_instance_bind_helper() {
        let Some(path) = std::env::var_os("PYREN_TEST_LIVE_SOCKET") else {
            return;
        };
        let rebound = bind_restricted(Path::new(&path), "pyren").is_ok();
        std::process::exit(if rebound { 42 } else { 0 });
    }
}
