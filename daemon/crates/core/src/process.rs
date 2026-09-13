//! Running an external program without letting it hang the daemon.
//!
//! `Command::output` waits for as long as the child does, and the programs
//! this daemon asks about power and GPUs - `busctl`, `tlp`, `auto-cpufreq`,
//! `nvidia-smi` - can each wait on something else: a stuck D-Bus, TLP's own
//! lock during a charger event, a GPU driver mid-reset. Called while a
//! module holds its state lock, one of those stalls every request behind it.
//! [`output_within`] puts a ceiling on that and kills the child at it.

use std::io::{self, Read};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long a status query or profile request may take before it is given
/// up on. Every program called this way answers in well under a second on
/// a healthy system; three is a stall, not a slow machine.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// `command.output()`, but killed and reported as [`io::ErrorKind::TimedOut`]
/// once `timeout` has passed.
///
/// Stdout and stderr are drained on their own threads, so a child that
/// fills a pipe buffer cannot deadlock against the wait. Stdin is closed:
/// nothing called this way is interactive, and one that prompted would
/// otherwise sit there until the deadline.
pub fn output_within(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("did not finish within {}s", timeout.as_secs_f32()),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };

    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// [`output_within`] at [`DEFAULT_TIMEOUT`].
pub fn output(command: &mut Command) -> io::Result<Output> {
    output_within(command, DEFAULT_TIMEOUT)
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    })
}

/// Kills and reaps, so a timed-out child does not linger as a zombie.
///
/// The drain threads are not joined: a grandchild that inherited the pipes
/// can keep them open past the kill, and waiting on it would be the very
/// hang this module exists to prevent. They finish when the pipe closes.
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quick_command_returns_its_output() {
        let output = output(Command::new("sh").args(["-c", "echo hi; echo err >&2"])).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "err");
    }

    #[test]
    fn a_hanging_command_is_killed_at_the_deadline() {
        let started = Instant::now();
        let error = output_within(
            Command::new("sh").args(["-c", "exec sleep 30"]),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_not_found_rather_than_timed_out() {
        let error = output(&mut Command::new("pyren-no-such-program")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
