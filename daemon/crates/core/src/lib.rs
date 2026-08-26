//! Shared host contract for omen-hub-daemon modules.
//!
//! A "module" owns one hardware surface (fans, RGB, ...) and is loaded
//! statically into the daemon binary. The daemon exposes every registered
//! module's methods over a single Unix domain socket, namespaced by
//! module id, using a small JSON-RPC-like protocol. See
//! `docs/01-ipc-protocol.md` at the repo root for the wire format.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Error returned by a module while handling a call. Converted to a plain
/// string at the IPC boundary (see [`Response`]) - callers on the other
/// side of the socket only ever see `error: string`.
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("unknown method '{0}'")]
    UnknownMethod(String),
    #[error("this module is not supported on this hardware")]
    Unsupported,
    #[error("operation requires elevated privileges: {0}")]
    PermissionDenied(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type ModuleResult = Result<Value, ModuleError>;

/// One hardware-control surface (fans, RGB lighting, battery, ...).
///
/// Implementors live in their own crate (e.g. `omen-hub-fan`) and are
/// registered into the daemon's [`Registry`] at startup. A module should
/// never talk to another module directly - cross-module coordination, if
/// ever needed, belongs in the daemon binary or a new shared crate, not in
/// module-to-module calls.
pub trait Module: Send + Sync {
    /// Stable identifier used as the JSON-RPC `module` namespace, e.g. `"fan"`.
    /// Must be unique across all registered modules.
    fn id(&self) -> &'static str;

    /// Whether this module's hardware was detected on this machine. The
    /// frontend uses this (via `core.capabilities`) to decide whether to
    /// show the module's UI at all - mirrors how the original Python GUI
    /// hides the Fan Cleaner page on unsupported hardware.
    fn is_supported(&self) -> bool;

    /// Dispatch one method call within this module's namespace.
    fn call(&self, method: &str, params: Value) -> ModuleResult;
}

#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: u64,
    pub module: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    fn ok(id: u64, result: Value) -> Self {
        Self { id, result: Some(result), error: None }
    }

    fn err(id: u64, message: impl Into<String>) -> Self {
        Self { id, result: None, error: Some(message.into()) }
    }
}

#[derive(Debug, Serialize)]
pub struct ModuleCapability {
    pub id: String,
    pub supported: bool,
}

/// Holds every module the daemon loaded and routes requests to them.
///
/// Also implements the built-in `core` module (id `"core"`), which exists
/// so a client can discover what's available (`core.capabilities`) without
/// hardcoding a module list.
pub struct Registry {
    modules: Vec<Box<dyn Module>>,
}

impl Registry {
    pub fn new() -> Self {
        Self { modules: Vec::new() }
    }

    pub fn register(&mut self, module: Box<dyn Module>) {
        self.modules.push(module);
    }

    pub fn capabilities(&self) -> Vec<ModuleCapability> {
        self.modules
            .iter()
            .map(|m| ModuleCapability { id: m.id().to_string(), supported: m.is_supported() })
            .collect()
    }

    pub fn dispatch(&self, req: Request) -> Response {
        if req.module == "core" {
            return self.dispatch_core(&req);
        }

        match self.modules.iter().find(|m| m.id() == req.module) {
            None => Response::err(req.id, format!("unknown module '{}'", req.module)),
            Some(m) => match m.call(&req.method, req.params) {
                Ok(v) => Response::ok(req.id, v),
                Err(e) => Response::err(req.id, e.to_string()),
            },
        }
    }

    fn dispatch_core(&self, req: &Request) -> Response {
        match req.method.as_str() {
            "capabilities" => {
                let caps = self.capabilities();
                Response::ok(req.id, serde_json::to_value(caps).unwrap_or(Value::Null))
            }
            other => Response::err(req.id, format!("unknown core method '{other}'")),
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs the daemon's IPC server: binds `path` as a Unix domain socket and
/// serves newline-delimited JSON requests/responses forever, one thread per
/// connection. This call blocks the calling thread.
///
/// Intentionally simple (std threads, blocking IO, one request in flight
/// per connection) rather than async - the socket is a local low-throughput
/// control plane, not a hot path. Revisit only if that stops being true.
pub fn serve_unix_socket(path: &str, registry: Arc<Registry>) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Stale socket file from a previous run (e.g. unclean shutdown) would
    // otherwise make bind() fail with "address in use".
    let _ = std::fs::remove_file(path);

    let listener = UnixListener::bind(path)?;

    for stream in listener.incoming() {
        let stream = stream?;
        let registry = Arc::clone(&registry);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &registry) {
                eprintln!("omen-hub-daemon: connection error: {e}");
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
            Err(e) => Response::err(0, format!("invalid request: {e}")),
        };

        let mut payload = serde_json::to_string(&response)
            .unwrap_or_else(|_| "{\"id\":0,\"error\":\"internal serialization error\"}".to_string());
        payload.push('\n');
        writer.write_all(payload.as_bytes())?;
    }

    Ok(())
}
