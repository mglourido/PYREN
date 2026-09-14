//! The `debug` IPC module: the one switch behind "Registros de
//! depuración", and the file it's persisted in.
//!
//! Deliberately tiny. It owns exactly one boolean - whether the files in
//! [`crate::debuglog`] get written - and nothing about what goes into
//! them; every category file is written by a one-line call from wherever
//! the thing being logged already happens. See
//! `docs/superpowers/specs/2026-09-14-debug-logging-design.md`.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use pyren_config::ConfigStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::events::EventBus;
use crate::{debuglog, Module, ModuleError, ModuleResult};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase", default)]
struct DebugConfig {
    enabled: bool,
}

#[derive(Clone)]
pub struct DebugModule {
    store: ConfigStore,
    events: Arc<OnceLock<Arc<EventBus>>>,
    daemon_dir: PathBuf,
    user_dir: PathBuf,
}

impl DebugModule {
    /// Loads the persisted setting and puts `pyren_core::debuglog` in the
    /// same state, so the two never disagree about whether logging is on.
    pub fn new() -> Self {
        Self::with_store(ConfigStore::system())
    }

    pub fn with_store(store: ConfigStore) -> Self {
        let loaded = store.load::<DebugConfig>("debug");
        debuglog::set_enabled(loaded.value.enabled);
        Self {
            store,
            events: Arc::new(OnceLock::new()),
            daemon_dir: debuglog::daemon_root(),
            user_dir: debuglog::user_root(),
        }
    }

    /// Hands the module the bus to announce `debug.changed` on. Called
    /// once, by the daemon binary, after the registry exists - same shape
    /// as `PowerModule::publish_to`/`FanModule::publish_to`.
    pub fn publish_to(&self, events: Arc<EventBus>) {
        let _ = self.events.set(events);
    }

    fn status(&self) -> Value {
        json!({
            "enabled": debuglog::enabled(),
            "daemonDir": self.daemon_dir.display().to_string(),
            "daemonDirWritable": debuglog::is_writable(&self.daemon_dir),
            "userDir": self.user_dir.display().to_string(),
        })
    }
}

impl Default for DebugModule {
    fn default() -> Self {
        Self::new()
    }
}

impl Module for DebugModule {
    fn id(&self) -> &'static str {
        "debug"
    }

    fn is_supported(&self) -> bool {
        true
    }

    fn call(&self, method: &str, params: Value) -> ModuleResult {
        match method {
            "getStatus" => Ok(self.status()),
            "setEnabled" => {
                let enabled = params
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| ModuleError::InvalidParams("enabled must be a bool".into()))?;
                debuglog::set_enabled(enabled);
                let _ = self.store.save("debug", &DebugConfig { enabled });
                if let Some(bus) = self.events.get() {
                    bus.publish("debug.changed", json!({ "enabled": enabled }));
                }
                Ok(self.status())
            }
            other => Err(ModuleError::UnknownMethod(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn store(tag: &str) -> ConfigStore {
        let root = std::env::temp_dir()
            .join(format!("pyren-debug-module-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        ConfigStore::at(root)
    }

    #[test]
    fn a_fresh_module_starts_disabled() {
        // `with_store` calls `debuglog::set_enabled` internally, so even
        // this read-only-looking test mutates the shared global and must
        // hold the lock, same as `energy_profiles.rs`'s `Machine::new`.
        let _guard = debuglog::test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let module = DebugModule::with_store(store("fresh"));
        let status = module.status();
        assert_eq!(status["enabled"], false);
    }

    #[test]
    fn set_enabled_flips_the_flag_and_persists_it() {
        let _guard = debuglog::test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let store = store("persist");
        let module = DebugModule::with_store(store.clone());

        let result = module.call("setEnabled", json!({ "enabled": true })).unwrap();
        assert_eq!(result["enabled"], true);
        assert!(debuglog::enabled());

        // A fresh module reading the same store picks up the persisted value.
        let reloaded = DebugModule::with_store(store);
        assert_eq!(reloaded.status()["enabled"], true);

        // Leave global state as found for whichever test runs next in this
        // binary.
        debuglog::set_enabled(false);
    }

    #[test]
    fn set_enabled_without_a_bool_is_invalid_params() {
        let _guard = debuglog::test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let module = DebugModule::with_store(store("bad-params"));
        let err = module.call("setEnabled", json!({})).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::InvalidParams);
    }

    #[test]
    fn an_unknown_method_is_refused() {
        let _guard = debuglog::test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let module = DebugModule::with_store(store("unknown-method"));
        let err = module.call("nope", Value::Null).unwrap_err();
        assert_eq!(err.kind(), crate::ErrorKind::UnknownMethod);
    }

    #[test]
    fn set_enabled_publishes_debug_changed_once_wired_to_a_bus() {
        let _guard = debuglog::test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let module = DebugModule::with_store(store("publish"));
        let events = Arc::new(EventBus::new());
        module.publish_to(Arc::clone(&events));

        module.call("setEnabled", json!({ "enabled": true })).unwrap();

        let batch = events.read_since(0, std::time::Duration::from_millis(0));
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].topic, "debug.changed");
        assert_eq!(batch.events[0].payload["enabled"], true);

        debuglog::set_enabled(false);
    }
}
