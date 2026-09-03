//! Shared host contract for pyren-daemon modules.
//!
//! A "module" owns one hardware surface (fans, RGB, ...) and is loaded
//! statically into the daemon binary. The daemon exposes every registered
//! module's methods over a single Unix domain socket, namespaced by
//! module id, using a small JSON-RPC-like protocol. See
//! `docs/01-ipc-protocol.md` at the repo root for the wire format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod acpi;
pub mod client;
mod socket;
pub use socket::{serve_unix_socket, socket_group, Audience};

/// What kind of failure this is, as it appears on the wire.
///
/// A closed set, and the point of it: the message beside it is written for
/// a person and gets reworded, so anything that branched on the prose broke
/// the next time somebody improved a sentence. A client matches on `kind`
/// and *shows* `message`.
///
/// Unknown values must not be an error for a client: a newer daemon may
/// name a kind this build has never heard of, and the honest response to
/// that is to treat it as [`ErrorKind::Failed`] and show the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    /// No module with that id is loaded in this daemon.
    UnknownModule,
    /// The module is loaded; the method is not one of its own.
    UnknownMethod,
    /// This module's hardware is absent on this machine.
    Unsupported,
    /// The hardware is present and cannot do this *particular* thing.
    ///
    /// Distinct from [`Self::Unsupported`] for the reason `capabilities`
    /// exists at all: board 8D2F can switch fan modes and cannot set a
    /// speed, and a UI that conflates the two either hides a control that
    /// works or offers one that never will.
    NotCapable,
    /// The caller sent something wrong. Retrying it unchanged will not
    /// help, and no amount of privilege makes it work.
    InvalidParams,
    /// The work needs root. The one kind a UI should turn into "elevate"
    /// rather than "this machine cannot do that".
    PermissionDenied,
    /// The machine refused while the work was being done.
    Io,
    /// Something else already has the hardware. The only kind where
    /// waiting and asking again is the right response.
    Busy,
    /// The daemon's own fault - it could not serialise its own reply.
    Internal,
    /// A genuine runtime failure that is none of the above.
    Failed,
    /// What arrived was not a request. Carries id 0, since the id is the
    /// thing that could not be read.
    MalformedRequest,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownModule => "unknownModule",
            Self::UnknownMethod => "unknownMethod",
            Self::Unsupported => "unsupported",
            Self::NotCapable => "notCapable",
            Self::InvalidParams => "invalidParams",
            Self::PermissionDenied => "permissionDenied",
            Self::Io => "io",
            Self::Busy => "busy",
            Self::Internal => "internal",
            Self::Failed => "failed",
            Self::MalformedRequest => "malformedRequest",
        }
    }

    /// Every kind, so `parse` and the tests have one list to drift from
    /// rather than two.
    pub const ALL: &'static [Self] = &[
        Self::UnknownModule,
        Self::UnknownMethod,
        Self::Unsupported,
        Self::NotCapable,
        Self::InvalidParams,
        Self::PermissionDenied,
        Self::Io,
        Self::Busy,
        Self::Internal,
        Self::Failed,
        Self::MalformedRequest,
    ];

    /// The inverse, for a client reading a reply. `None` means a kind this
    /// build does not know, which is a newer daemon rather than a bug.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == value)
    }
}

/// Error returned by a module while handling a call. Reaches the other
/// side of the socket as `{ kind, message }` - see [`Response`].
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("unknown method '{0}'")]
    UnknownMethod(String),
    #[error("this module is not supported on this hardware")]
    Unsupported,
    #[error("{0}")]
    NotCapable(String),
    #[error("{0}")]
    InvalidParams(String),
    #[error("operation requires elevated privileges: {0}")]
    PermissionDenied(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("{0}")]
    Busy(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("{0}")]
    Failed(String),
}

impl ModuleError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::UnknownMethod(_) => ErrorKind::UnknownMethod,
            Self::Unsupported => ErrorKind::Unsupported,
            Self::NotCapable(_) => ErrorKind::NotCapable,
            Self::InvalidParams(_) => ErrorKind::InvalidParams,
            Self::PermissionDenied(_) => ErrorKind::PermissionDenied,
            Self::Io(_) => ErrorKind::Io,
            Self::Busy(_) => ErrorKind::Busy,
            Self::Internal(_) => ErrorKind::Internal,
            Self::Failed(_) => ErrorKind::Failed,
        }
    }
}

pub type ModuleResult = Result<Value, ModuleError>;

/// One hardware-control surface (fans, RGB lighting, battery, ...).
///
/// Implementors live in their own crate (e.g. `pyren-fan`) and are
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

/// The `error` half of a [`Response`], as it goes over the socket.
#[derive(Debug, Serialize)]
pub struct WireError {
    pub kind: ErrorKind,
    /// For a person to read. Never for a client to match on.
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl Response {
    pub(crate) fn ok(id: u64, result: Value) -> Self {
        Self { id, result: Some(result), error: None }
    }

    pub(crate) fn err(id: u64, kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { id, result: None, error: Some(WireError { kind, message: message.into() }) }
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
            None => Response::err(
                req.id,
                ErrorKind::UnknownModule,
                format!("unknown module '{}'", req.module),
            ),
            Some(m) => match m.call(&req.method, req.params) {
                Ok(v) => Response::ok(req.id, v),
                Err(e) => Response::err(req.id, e.kind(), e.to_string()),
            },
        }
    }

    fn dispatch_core(&self, req: &Request) -> Response {
        match req.method.as_str() {
            "capabilities" => {
                let caps = self.capabilities();
                Response::ok(req.id, serde_json::to_value(caps).unwrap_or(Value::Null))
            }
            other => Response::err(
                req.id,
                ErrorKind::UnknownMethod,
                format!("unknown core method '{other}'"),
            ),
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(fn() -> ModuleError);

    impl Module for Stub {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn is_supported(&self) -> bool {
            true
        }
        fn call(&self, method: &str, _params: Value) -> ModuleResult {
            match method {
                "ok" => Ok(serde_json::json!({ "fine": true })),
                _ => Err((self.0)()),
            }
        }
    }

    fn reply(module: &str, method: &str, error: fn() -> ModuleError) -> Value {
        let mut registry = Registry::new();
        registry.register(Box::new(Stub(error)));
        let response = registry.dispatch(Request {
            id: 7,
            module: module.to_string(),
            method: method.to_string(),
            params: Value::Null,
        });
        serde_json::to_value(response).expect("a response must serialise")
    }

    fn kind_of(module: &str, method: &str, error: fn() -> ModuleError) -> String {
        reply(module, method, error)["error"]["kind"].as_str().unwrap().to_string()
    }

    /// The whole point of the change: a caller branches on `kind` and shows
    /// `message`, so both have to be there and the error must not be a bare
    /// string any more.
    #[test]
    fn a_refusal_carries_a_kind_beside_its_message() {
        let reply = reply("stub", "nope", || {
            ModuleError::PermissionDenied("writing /sys/x needs root".into())
        });

        assert_eq!(reply["error"]["kind"], "permissionDenied");
        assert!(reply["error"]["message"].as_str().unwrap().contains("/sys/x"));
        assert_eq!(reply["id"], 7);
        assert!(reply.get("result").is_none(), "a refusal has no result");
    }

    #[test]
    fn a_successful_call_carries_no_error_at_all() {
        let reply = reply("stub", "ok", || ModuleError::Failed("unused".into()));

        assert_eq!(reply["result"]["fine"], true);
        assert!(reply.get("error").is_none());
    }

    /// The three refusals `fan.setMode` used to return as indistinguishable
    /// prose, which is what this item existed to fix.
    #[test]
    fn the_three_kinds_of_refusal_are_told_apart() {
        assert_eq!(
            kind_of("stub", "x", || ModuleError::NotCapable("no pwm1 here".into())),
            "notCapable"
        );
        assert_eq!(
            kind_of("stub", "x", || ModuleError::InvalidParams("pwm must be 0-255".into())),
            "invalidParams"
        );
        assert_eq!(
            kind_of("stub", "x", || ModuleError::PermissionDenied("needs root".into())),
            "permissionDenied"
        );
    }

    #[test]
    fn every_variant_reaches_the_wire_as_its_own_kind() {
        for (error, expected) in [
            (ModuleError::UnknownMethod("x".into()), ErrorKind::UnknownMethod),
            (ModuleError::Unsupported, ErrorKind::Unsupported),
            (ModuleError::NotCapable("x".into()), ErrorKind::NotCapable),
            (ModuleError::InvalidParams("x".into()), ErrorKind::InvalidParams),
            (ModuleError::PermissionDenied("x".into()), ErrorKind::PermissionDenied),
            (ModuleError::Io("x".into()), ErrorKind::Io),
            (ModuleError::Busy("x".into()), ErrorKind::Busy),
            (ModuleError::Internal("x".into()), ErrorKind::Internal),
            (ModuleError::Failed("x".into()), ErrorKind::Failed),
        ] {
            assert_eq!(error.kind(), expected, "{error:?}");
        }
    }

    /// A module that is absent and a method that is absent are different
    /// mistakes, and a client that shows "unknown" for both sends people
    /// looking in the wrong place.
    #[test]
    fn an_absent_module_and_an_absent_method_are_different_kinds() {
        assert_eq!(kind_of("nosuch", "x", || ModuleError::Failed("x".into())), "unknownModule");
        assert_eq!(
            kind_of("stub", "x", || ModuleError::UnknownMethod("x".into())),
            "unknownMethod"
        );
        assert_eq!(kind_of("core", "nosuch", || ModuleError::Failed("x".into())), "unknownMethod");
    }

    /// `as_str` is exhaustive because the compiler says so; `ALL` is not,
    /// and a kind missing from it is a kind no client can parse.
    #[test]
    fn every_kind_survives_a_round_trip_through_its_wire_name() {
        for kind in ErrorKind::ALL {
            assert_eq!(ErrorKind::parse(kind.as_str()), Some(*kind));
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                Value::String(kind.as_str().to_string()),
                "the derived Serialize and as_str must agree"
            );
        }
        assert_eq!(ErrorKind::ALL.len(), 11, "a new kind also belongs in ALL");
    }

    #[test]
    fn a_kind_from_a_newer_daemon_is_not_an_error_here() {
        assert_eq!(ErrorKind::parse("somethingFromTheFuture"), None);
    }
}
