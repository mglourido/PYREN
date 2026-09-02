//! The other end of [`crate::serve_unix_socket`]: a blocking client for
//! anything that wants to talk to a running daemon.
//!
//! Lives here so the wire format has one implementation on each side rather
//! than one per caller. The Tauri app still carries its own copy - it is a
//! separate Cargo workspace and a separate binary - which is exactly the
//! duplication this is meant to stop spreading any further.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use serde_json::Value;

/// Default socket path, matching the daemon's own unprivileged fallback.
pub const DEFAULT_SOCKET: &str = "/tmp/pyren-daemon.sock";

/// The socket the installed systemd unit listens on.
pub const SERVICE_SOCKET: &str = "/run/pyren/daemon.sock";

/// Where the daemon might be, most-likely first.
///
/// Knowing only [`DEFAULT_SOCKET`] is a bug that hides until the day the
/// service is properly installed: the unit sets `PYREN_SOCKET` to
/// [`SERVICE_SOCKET`], so a client looking only in `/tmp` finds nothing on
/// every real installation.
pub fn socket_candidates() -> Vec<String> {
    match std::env::var("PYREN_SOCKET") {
        // An explicit setting is a decision, not a hint.
        Ok(path) => vec![path],
        Err(_) => vec![SERVICE_SOCKET.to_string(), DEFAULT_SOCKET.to_string()],
    }
}

/// The socket to *name*: the one that answers, else the one that at least
/// exists, else the first we would try.
pub fn socket_path() -> String {
    let candidates = socket_candidates();
    candidates
        .iter()
        .find(|path| UnixStream::connect(path).is_ok())
        .or_else(|| candidates.iter().find(|path| std::path::Path::new(path).exists()))
        .cloned()
        .unwrap_or_else(|| candidates[0].clone())
}

/// Opens the first socket that answers, or reports the most useful failure.
fn connect() -> Result<UnixStream, ClientError> {
    let candidates = socket_candidates();
    let mut failure: Option<(String, std::io::Error)> = None;

    for path in &candidates {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(source) => {
                // "You are not in the group" says far more than "no such
                // file" from whichever path we happened to try last, so it
                // wins regardless of the order the failures arrive in.
                let more_useful = source.kind() == std::io::ErrorKind::PermissionDenied
                    || failure.as_ref().is_none_or(|(_, previous)| {
                        previous.kind() != std::io::ErrorKind::PermissionDenied
                    });
                if more_useful {
                    failure = Some((path.clone(), source));
                }
            }
        }
    }

    let (path, source) = failure.expect("there is always at least one candidate");
    Err(ClientError::Connect { path, source })
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Nothing is listening, or this user may not open the socket.
    #[error("cannot reach pyren-daemon at {path}: {source}")]
    Connect { path: String, source: std::io::Error },
    #[error("talking to pyren-daemon: {0}")]
    Io(#[from] std::io::Error),
    #[error("pyren-daemon sent something that is not a response: {0}")]
    Protocol(String),
    /// The daemon answered, and the answer was a refusal.
    #[error("{0}")]
    Daemon(String),
}

impl ClientError {
    /// Whether this is "you are not allowed to open the socket", which has
    /// a specific fix worth telling the user about.
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::Connect { source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied)
    }
}

/// One request, one response, one connection.
///
/// Deliberately not a long-lived connection: a CLI makes a handful of calls
/// and exits, and the daemon serves one request at a time per connection
/// anyway.
pub fn call(module: &str, method: &str, params: Value) -> Result<Value, ClientError> {
    let stream = connect()?;

    let mut writer = stream.try_clone()?;
    let request = serde_json::json!({
        "id": 1, "module": module, "method": method, "params": params,
    });
    let mut line = request.to_string();
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    if response.trim().is_empty() {
        return Err(ClientError::Protocol("the connection closed without an answer".into()));
    }

    let parsed: Value =
        serde_json::from_str(&response).map_err(|e| ClientError::Protocol(e.to_string()))?;
    if let Some(error) = parsed.get("error").and_then(Value::as_str) {
        return Err(ClientError::Daemon(error.to_string()));
    }
    Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
}
