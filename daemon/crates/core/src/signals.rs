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
//! Children are unaffected: the standard library empties the signal mask
//! of every process it spawns.

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

/// Waits on a thread of its own for SIGTERM or SIGINT, runs `tidy` with
/// the signal's number, and exits the process.
///
/// Needs [`block_termination`] to have been called first; without it the
/// signal's default action kills the process before this sees it.
pub fn on_termination<F>(tidy: F)
where
    F: FnOnce(libc::c_int) + Send + 'static,
{
    let spawned = std::thread::Builder::new().name("pyren-signals".into()).spawn(move || {
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
    std::process::Command::new("systemctl")
        .arg("is-system-running")
        .stderr(std::process::Stdio::null())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "stopping")
        .unwrap_or(false)
}
