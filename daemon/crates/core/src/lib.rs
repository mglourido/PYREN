//! Shared host contract for omen-hub-daemon modules.
//!
//! A "module" owns one hardware surface (fans, RGB, ...) and is loaded
//! statically into the daemon binary. The daemon exposes every registered
//! module's methods over a single Unix domain socket, namespaced by
//! module id, using a small JSON-RPC-like protocol. See
//! `docs/01-ipc-protocol.md` at the repo root for the wire format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod client;
mod socket;
pub use socket::{serve_unix_socket, socket_group, Audience};

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
    pub(crate) fn ok(id: u64, result: Value) -> Self {
        Self { id, result: Some(result), error: None }
    }

    pub(crate) fn err(id: u64, message: impl Into<String>) -> Self {
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
