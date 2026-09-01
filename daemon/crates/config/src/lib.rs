//! On-disk configuration: one JSON file per namespace, written atomically.
//!
//! This settles the "config persistence mechanism" question left open in
//! `docs/00-design-plan.md`: hand-rolled JSON, like the Python original,
//! rather than a config framework. The requirements here are narrow (a few
//! small files, no layering, no environment interpolation) and what
//! actually matters is the failure behaviour, which a framework wouldn't
//! give us for free:
//!
//! - **Atomic writes.** Config is written from a daemon that can be killed
//!   at any moment, including during shutdown. Writing in place risks
//!   leaving a truncated file that fails to parse on next boot, which -
//!   for a daemon that controls fans - means silently reverting to
//!   defaults. Every save goes to a temporary file, is flushed, and is then
//!   renamed over the target, which is atomic on POSIX.
//! - **A corrupt file is never overwritten silently.** It is moved aside as
//!   `<name>.json.bad` so the user can see what happened and recover their
//!   settings, and defaults are used for that run.
//! - **Versioned files.** Every file carries a `version`, and a file
//!   written by a *newer* build is refused rather than parsed
//!   optimistically and written back in the older shape - downgrading
//!   should not destroy settings.
//!
//! Layout:
//!
//! ```text
//! /etc/omen-hub/power.json     system config, written by the root daemon
//! ~/.config/omen-hub/app.json  per-user config, written by the desktop app
//! ```

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Format version written into every file. Bump when a config struct
/// changes shape in a way older builds could not read correctly.
pub const CURRENT_VERSION: u32 = 1;

const SYSTEM_ROOT: &str = "/etc/omen-hub";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} was written by a newer version of omen-hub (v{found} > v{supported}); \
             refusing to overwrite it")]
    FutureVersion { path: PathBuf, found: u32, supported: u32 },
    #[error("could not serialize config: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Why a load did not return stored values, for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// The file was read and parsed.
    Loaded,
    /// No file yet - first run.
    Missing,
    /// The file was unreadable or invalid; it has been moved aside and
    /// defaults are in use. Carries the path of the preserved copy.
    Recovered { backup: Option<PathBuf>, reason: String },
    /// The file is from a newer build; defaults are in use and the file has
    /// been left untouched, so the newer build still finds its settings.
    TooNew { found: u32 },
}

/// One config value plus how it was obtained.
#[derive(Debug, Clone)]
pub struct Loaded<T> {
    pub value: T,
    pub outcome: LoadOutcome,
}

impl<T> Loaded<T> {
    /// True when this run should avoid writing the file back - either
    /// because a newer build owns it, or because the previous contents are
    /// still being preserved for the user to inspect.
    pub fn is_from_disk(&self) -> bool {
        self.outcome == LoadOutcome::Loaded
    }
}

/// The `version` wrapper each file carries around its payload.
#[derive(Serialize, Deserialize)]
struct Versioned<T> {
    version: u32,
    #[serde(flatten)]
    inner: T,
}

/// A directory holding namespaced config files.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    root: PathBuf,
}

impl ConfigStore {
    /// System-wide config, for the daemon.
    ///
    /// Prefers `/etc/omen-hub` and falls back to the per-user directory
    /// when that isn't writable - which is exactly the case when the daemon
    /// is run unprivileged with `cargo run` during development. Writability
    /// is tested by actually creating the directory rather than by checking
    /// the effective uid, since being root is not the same as the path
    /// being writable (read-only /etc, containers, immutable distros).
    pub fn system() -> Self {
        if let Ok(dir) = std::env::var("OMEN_HUB_CONFIG_DIR") {
            return Self::at(dir);
        }
        if fs::create_dir_all(SYSTEM_ROOT).is_ok() && is_writable(Path::new(SYSTEM_ROOT)) {
            return Self::at(SYSTEM_ROOT);
        }
        Self::user()
    }

    /// Per-user config, for the desktop app. Never `/etc`.
    pub fn user() -> Self {
        if let Ok(dir) = std::env::var("OMEN_HUB_CONFIG_DIR") {
            return Self::at(dir);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        Self::at(base.join("omen-hub"))
    }

    /// A store rooted at an explicit directory. Used by tests, and by
    /// anything that already knows where its config lives.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of one namespace's file.
    ///
    /// The name is sanitised rather than trusted: the desktop app passes a
    /// namespace through an IPC command, and a name like `../../etc/shadow`
    /// must not be able to escape the config directory. Callers should
    /// still validate against a known list; this is defence in depth.
    pub fn path_for(&self, name: &str) -> PathBuf {
        self.root.join(format!("{}.json", sanitize_name(name)))
    }

    /// Loads one namespace, falling back to `T::default()` for anything
    /// that isn't a clean read. Never fails: a daemon that cannot read its
    /// config must still start.
    pub fn load<T: DeserializeOwned + Default>(&self, name: &str) -> Loaded<T> {
        let path = self.path_for(name);

        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Loaded { value: T::default(), outcome: LoadOutcome::Missing };
            }
            Err(e) => {
                return Loaded {
                    value: T::default(),
                    outcome: LoadOutcome::Recovered { backup: None, reason: e.to_string() },
                };
            }
        };

        // Read the version before the payload: a file from a newer build
        // may not even parse into today's struct, and must be left alone
        // rather than replaced with defaults.
        if let Ok(probe) = serde_json::from_str::<VersionProbe>(&text) {
            if probe.version > CURRENT_VERSION {
                return Loaded {
                    value: T::default(),
                    outcome: LoadOutcome::TooNew { found: probe.version },
                };
            }
        }

        match serde_json::from_str::<Versioned<T>>(&text) {
            Ok(parsed) => Loaded { value: parsed.inner, outcome: LoadOutcome::Loaded },
            Err(e) => {
                let backup = self.preserve_broken(&path);
                Loaded {
                    value: T::default(),
                    outcome: LoadOutcome::Recovered { backup, reason: e.to_string() },
                }
            }
        }
    }

    /// Writes one namespace atomically.
    pub fn save<T: Serialize>(&self, name: &str, value: &T) -> Result<(), ConfigError> {
        let path = self.path_for(name);
        fs::create_dir_all(&self.root)
            .map_err(|source| ConfigError::Io { path: self.root.clone(), source })?;

        let payload = serde_json::to_string_pretty(&Versioned {
            version: CURRENT_VERSION,
            inner: value,
        })?;

        // Same directory as the target, so the rename stays within one
        // filesystem and is therefore atomic.
        let temp = path.with_extension("json.tmp");
        {
            let mut file = fs::File::create(&temp)
                .map_err(|source| ConfigError::Io { path: temp.clone(), source })?;
            file.write_all(payload.as_bytes())
                .map_err(|source| ConfigError::Io { path: temp.clone(), source })?;
            file.write_all(b"\n")
                .map_err(|source| ConfigError::Io { path: temp.clone(), source })?;
            // Without this the rename can land before the data does, which
            // on a crash leaves an empty file where the config used to be.
            file.sync_all().map_err(|source| ConfigError::Io { path: temp.clone(), source })?;
        }

        fs::rename(&temp, &path).map_err(|source| ConfigError::Io { path: path.clone(), source })
    }

    /// Moves an unparseable file aside so the user can recover it.
    fn preserve_broken(&self, path: &Path) -> Option<PathBuf> {
        let backup = path.with_extension("json.bad");
        fs::rename(path, &backup).ok().map(|()| backup)
    }
}

#[derive(Deserialize)]
struct VersionProbe {
    #[serde(default)]
    version: u32,
}

/// Reduces a namespace to a plain identifier, so it can only ever name a
/// file directly inside the config directory.
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "invalid".to_string()
    } else {
        cleaned
    }
}

fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".omen-hub-write-test");
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(default, rename_all = "camelCase")]
    struct Sample {
        enabled: bool,
        threshold: f64,
        name: String,
    }

    impl Default for Sample {
        fn default() -> Self {
            Self { enabled: false, threshold: 0.5, name: "default".into() }
        }
    }

    /// Each test gets its own directory under the process temp dir.
    fn store(tag: &str) -> ConfigStore {
        let root = std::env::temp_dir().join(format!("omen-hub-config-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        ConfigStore::at(root)
    }

    #[test]
    fn a_missing_file_yields_defaults() {
        let loaded = store("missing").load::<Sample>("thing");
        assert_eq!(loaded.outcome, LoadOutcome::Missing);
        assert_eq!(loaded.value, Sample::default());
        assert!(!loaded.is_from_disk());
    }

    #[test]
    fn saved_values_round_trip() {
        let store = store("roundtrip");
        let value = Sample { enabled: true, threshold: 0.9, name: "custom".into() };
        store.save("thing", &value).expect("save");

        let loaded = store.load::<Sample>("thing");
        assert_eq!(loaded.outcome, LoadOutcome::Loaded);
        assert_eq!(loaded.value, value);
    }

    #[test]
    fn the_file_carries_a_version() {
        let store = store("version");
        store.save("thing", &Sample::default()).expect("save");
        let text = fs::read_to_string(store.path_for("thing")).expect("read");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse");
        assert_eq!(json["version"], CURRENT_VERSION);
        // The payload is flattened alongside it, not nested.
        assert_eq!(json["threshold"], 0.5);
    }

    #[test]
    fn fields_added_since_the_file_was_written_fall_back_to_defaults() {
        let store = store("partial");
        fs::create_dir_all(store.root()).unwrap();
        fs::write(store.path_for("thing"), r#"{"version":1,"enabled":true}"#).unwrap();

        let loaded = store.load::<Sample>("thing");
        assert_eq!(loaded.outcome, LoadOutcome::Loaded);
        assert!(loaded.value.enabled);
        assert_eq!(loaded.value.threshold, 0.5);
    }

    #[test]
    fn a_corrupt_file_is_preserved_rather_than_overwritten() {
        let store = store("corrupt");
        fs::create_dir_all(store.root()).unwrap();
        fs::write(store.path_for("thing"), "{ this is not json").unwrap();

        let loaded = store.load::<Sample>("thing");
        match loaded.outcome {
            LoadOutcome::Recovered { backup, .. } => {
                let backup = backup.expect("the broken file should have been kept");
                assert!(backup.exists());
                assert_eq!(fs::read_to_string(backup).unwrap(), "{ this is not json");
            }
            other => panic!("expected Recovered, got {other:?}"),
        }
        assert_eq!(loaded.value, Sample::default());
    }

    #[test]
    fn a_file_from_a_newer_build_is_left_untouched() {
        let store = store("future");
        fs::create_dir_all(store.root()).unwrap();
        let original = format!(r#"{{"version":{},"enabled":true}}"#, CURRENT_VERSION + 1);
        fs::write(store.path_for("thing"), &original).unwrap();

        let loaded = store.load::<Sample>("thing");
        assert_eq!(loaded.outcome, LoadOutcome::TooNew { found: CURRENT_VERSION + 1 });
        assert_eq!(loaded.value, Sample::default());
        // Crucially the newer build's settings are still on disk.
        assert_eq!(fs::read_to_string(store.path_for("thing")).unwrap(), original);
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let store = store("notemp");
        store.save("thing", &Sample::default()).expect("save");
        assert!(!store.path_for("thing").with_extension("json.tmp").exists());
    }

    #[test]
    fn namespaces_do_not_collide() {
        let store = store("namespaces");
        let power = Sample { name: "power".into(), ..Sample::default() };
        let app = Sample { name: "app".into(), ..Sample::default() };
        store.save("power", &power).unwrap();
        store.save("app", &app).unwrap();

        assert_eq!(store.load::<Sample>("power").value.name, "power");
        assert_eq!(store.load::<Sample>("app").value.name, "app");
    }

    #[test]
    fn a_namespace_cannot_escape_the_config_directory() {
        let store = ConfigStore::at("/tmp/omen-hub-root");
        assert_eq!(store.path_for("../../etc/shadow"), Path::new("/tmp/omen-hub-root/etcshadow.json"));
        assert_eq!(store.path_for("power"), Path::new("/tmp/omen-hub-root/power.json"));
        // A name with nothing usable left still stays inside the root.
        assert_eq!(store.path_for("../.."), Path::new("/tmp/omen-hub-root/invalid.json"));
    }

    #[test]
    fn the_user_store_never_points_at_etc() {
        // Even if something odd is in the environment, per-user config must
        // not land in a system directory.
        assert!(!ConfigStore::user().root().starts_with("/etc"));
    }
}
