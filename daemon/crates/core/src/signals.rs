//! Leaving on SIGTERM with a chance to tidy up first.
//!
//! Until something needed it the daemon simply died on SIGTERM, which was
//! fine: nothing it drives has to be put back when it goes. A lighting
//! effect is the first thing that does - a thread rewriting the keyboard
//! thirty times a second, killed mid-frame, leaves whatever colour that
//! frame was - and the shutdown animation is the first thing that has to
//! *run* at exit.
//!
//! The mechanism is the one that needs no signal-safe code at all: the
//! termination signals are blocked in every thread, and one thread waits
//! for them with `sigwait`, where it can do anything an ordinary thread
//! can. Blocking has to happen before any other thread exists, because a
//! thread inherits the mask it was started with - hence two calls, and
//! the order matters.
//!
//! External children must be constructed through `process::command`, which
//! removes this private mask in the child immediately before exec.

const SIGNALS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];

fn set() -> libc::sigset_t {
    // SAFETY: a zeroed `sigset_t` is valid storage for `sigemptyset`, and
    // both calls only write to it.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in SIGNALS {
            libc::sigaddset(&mut set, signal);
        }
        set
    }
}

/// Blocks SIGTERM and SIGINT in this thread and every thread it starts
/// from now on. Call first thing in `main`, before anything spawns.
pub fn block_termination() {
    let set = set();
    // SAFETY: `set` is initialised above; the old mask is not wanted.
    unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// Undoes [`block_termination`] in the calling thread.
///
/// For the one caller that replaces the process with `exec`: the new image
/// inherits the mask of the thread that called it, and a daemon that starts
/// with SIGTERM already blocked cannot be stopped until it reaches its own
/// `on_termination` - which a daemon standing down for another user never
/// does.
pub fn unblock_termination() {
    let set = set();
    // SAFETY: `set` is initialised above; the old mask is not wanted.
    unsafe {
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }
}

/// Sends the signal to this process again.
///
/// For a termination signal that arrived while the daemon was already
/// replacing itself: `sigwait` has consumed it, and a consumed signal does
/// not follow the process through `exec`. Raised again it is pending -
/// every thread has it blocked and nobody is waiting on it any more - and
/// a pending signal does survive `exec`, or is delivered the moment the
/// thread about to `exec` calls [`unblock_termination`]. Either way the
/// request to stop is not lost.
pub fn raise(signal: libc::c_int) {
    // SAFETY: kill and getpid have no memory-safety preconditions.
    unsafe {
        libc::kill(libc::getpid(), signal);
    }
}

/// Waits on a thread of its own for SIGTERM or SIGINT, runs `tidy` with
/// the signal's number, and exits the process.
///
/// Needs [`block_termination`] to have been called first; without it the
/// signal's default action kills the process before this sees it.
pub fn on_termination<F>(tidy: F)
where
    F: FnOnce(libc::c_int) + Send + 'static,
{
    let spawned = std::thread::Builder::new()
        .name("pyren-signals".into())
        .spawn(move || {
            let set = set();
            let mut signal: libc::c_int = 0;
            // SAFETY: `set` is initialised and `signal` is a valid out pointer.
            let waited = unsafe { libc::sigwait(&set, &mut signal) };
            if waited != 0 {
                crate::log_warn!("sigwait failed ({waited}); SIGTERM will not be handled");
                return;
            }
            tidy(signal);
            std::process::exit(0);
        });
    if let Err(e) = spawned {
        crate::log_warn!("could not start the signal thread: {e}");
    }
}

/// The signal's name, for a log line.
pub fn name(signal: libc::c_int) -> &'static str {
    match signal {
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        _ => "a signal",
    }
}

/// Whether the whole machine is on its way down, as opposed to this
/// service being stopped or restarted on its own. systemd says `stopping`
/// from the moment a shutdown or reboot starts.
///
/// Asked rather than inferred from the signal: SIGTERM is the same signal
/// either way.
pub fn system_is_stopping() -> bool {
    crate::process::command("systemctl")
        .arg("is-system-running")
        .stderr(std::process::Stdio::null())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "stopping")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RestoreMask(libc::sigset_t);

    impl Drop for RestoreMask {
        fn drop(&mut self) {
            // SAFETY: the mask was populated by pthread_sigmask below and
            // remains valid for the duration of this test thread.
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
            }
        }
    }

    #[test]
    fn spawned_processes_do_not_inherit_the_daemons_termination_mask() {
        // Record and restore the test thread's mask so this regression test
        // cannot affect other tests in the same process.
        let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
        let empty: libc::sigset_t = unsafe {
            let mut value = std::mem::zeroed();
            libc::sigemptyset(&mut value);
            value
        };
        let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &empty, &mut old) };
        assert_eq!(rc, 0);
        let _restore = RestoreMask(old);

        block_termination();
        let output = crate::process::command("sh")
            .args(["-c", "grep '^SigBlk:' /proc/self/status"])
            .output()
            .expect("spawn child process");
        assert!(output.status.success());

        let line = String::from_utf8(output.stdout).expect("SigBlk is ASCII");
        let mask = u64::from_str_radix(line.split_whitespace().nth(1).expect("SigBlk value"), 16)
            .expect("hexadecimal SigBlk value");
        let termination = (1_u64 << (libc::SIGINT - 1)) | (1_u64 << (libc::SIGTERM - 1));

        assert_eq!(
            mask & termination,
            0,
            "an exec'd child inherited blocked SIGINT/SIGTERM"
        );
    }
}
