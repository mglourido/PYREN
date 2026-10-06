//! One copy of the daemon's settings per user, beside the ones in use.
//!
//! ```text
//! /etc/pyren/fan.json              the settings in use - somebody's
//! /etc/pyren/users/1000/fan.json   uid 1000's, as they last left them
//! /etc/pyren/users/1001/fan.json   uid 1001's
//! ```
//!
//! The files in use stay exactly where every module already reads and
//! writes them, so no module knows any of this exists. A user's copy is
//! taken when the settings stop being theirs and put back when they are
//! theirs again; between those two moments it is not touched.
//!
//! Files are copied as bytes, not through `ConfigStore`: a copy must not
//! care what is in a file, and one written by a newer build has to come
//! back out exactly as it went in.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The namespaces that are somebody's. `debug` and this crate's own
/// `users` are the machine's, and stay put whoever logs in.
pub const NAMESPACES: [&str; 6] = ["fan", "power", "rgb", "keymap", "hotkey", "overclock"];

/// Keys that describe the machine although they live in a user's file, by
/// namespace. They are kept as they are when a profile is put back.
///
/// `fan.json` is the one such file: beside the curves it holds what
/// calibration and the hardware checks *measured* - how fast these fans
/// go, how slow they hold, whether the controller obeys at all. A user
/// who was last here before a recalibration must not bring the old
/// numbers back with their curve. The names are `FanConfig`'s own, and a
/// test in `daemon/tests` holds the two together.
pub const MACHINE_KEYS: [(&str, &[&str]); 1] = [(
    "fan",
    &[
        "fanMaxRpm",
        "fan1MaxRpm",
        "fan2MaxRpm",
        "fanMinRpm",
        "fanStableMinRpm",
        "fanFloorNotices",
        "speedControl",
        "splitControl",
    ],
)];

/// The per-user copies under one config directory.
#[derive(Debug, Clone)]
pub struct Profiles {
    root: PathBuf,
}

impl Profiles {
    /// `root` is the directory the settings in use live in.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn live(&self, namespace: &str) -> PathBuf {
        self.root.join(format!("{namespace}.json"))
    }

    fn dir(&self, uid: u32) -> PathBuf {
        self.root.join("users").join(uid.to_string())
    }

    /// Whether this user has left any settings behind.
    pub fn exists(&self, uid: u32) -> bool {
        let dir = self.dir(uid);
        NAMESPACES
            .iter()
            .any(|namespace| dir.join(format!("{namespace}.json")).is_file())
    }

    /// Everyone with a profile, lowest uid first.
    pub fn list(&self) -> Vec<u32> {
        let Ok(entries) = fs::read_dir(self.root.join("users")) else {
            return Vec::new();
        };
        let mut uids: Vec<u32> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .filter(|uid| self.exists(*uid))
            .collect();
        uids.sort_unstable();
        uids
    }

    /// Copies the settings in use into this user's profile.
    ///
    /// A namespace with no file in use loses its copy too: the profile is
    /// what the settings *were*, and "never set" is one of the things they
    /// can have been.
    pub fn save(&self, uid: u32) -> io::Result<()> {
        let dir = self.dir(uid);
        fs::create_dir_all(&dir)?;
        for namespace in NAMESPACES {
            let copy = dir.join(format!("{namespace}.json"));
            match fs::read(self.live(namespace)) {
                Ok(bytes) => write_atomic(&copy, &bytes)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::remove_file(&copy) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                },
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Puts this user's profile in use, and says which namespaces changed
    /// hands.
    ///
    /// A namespace the profile has no file for keeps whatever is in use -
    /// the same "otherwise keep the current ones" a user with no profile
    /// at all gets, one file at a time. Safe to run twice: the profile is
    /// only read.
    pub fn restore(&self, uid: u32) -> io::Result<Vec<&'static str>> {
        let dir = self.dir(uid);
        let mut restored = Vec::new();
        for namespace in NAMESPACES {
            let bytes = match fs::read(dir.join(format!("{namespace}.json"))) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let live = self.live(namespace);
            let bytes = keep_machine_keys(namespace, bytes, &live);
            write_atomic(&live, &bytes)?;
            restored.push(namespace);
        }
        Ok(restored)
    }
}

/// `profile` with the machine's own keys taken from the file in use.
///
/// Anything that is not a JSON object on both sides is passed through
/// untouched: a file this cannot read is still the user's file, and the
/// module that owns it decides what to make of it.
fn keep_machine_keys(namespace: &str, profile: Vec<u8>, live: &Path) -> Vec<u8> {
    let Some((_, keys)) = MACHINE_KEYS.iter().find(|(name, _)| *name == namespace) else {
        return profile;
    };
    let Ok(Value::Object(mut theirs)) = serde_json::from_slice::<Value>(&profile) else {
        return profile;
    };
    let Some(Value::Object(current)) = fs::read(live)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return profile;
    };
    for key in *keys {
        match current.get(*key) {
            Some(value) => theirs.insert((*key).to_string(), value.clone()),
            None => theirs.remove(*key),
        };
    }
    match serde_json::to_vec_pretty(&Value::Object(theirs)) {
        Ok(mut merged) => {
            merged.push(b'\n');
            merged
        }
        Err(_) => profile,
    }
}

/// Temporary file, flush, rename - the same guarantee `ConfigStore::save`
/// gives, for the same reason: this can be interrupted at any point, and a
/// truncated `fan.json` is a daemon on default fan settings.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profiles(tag: &str) -> Profiles {
        let root =
            std::env::temp_dir().join(format!("pyren-users-profiles-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Profiles::new(root)
    }

    fn write(profiles: &Profiles, namespace: &str, value: Value) {
        fs::write(profiles.live(namespace), value.to_string()).unwrap();
    }

    fn read(profiles: &Profiles, namespace: &str) -> Value {
        serde_json::from_slice(&fs::read(profiles.live(namespace)).unwrap()).unwrap()
    }

    #[test]
    fn nobody_has_a_profile_to_begin_with() {
        let profiles = profiles("empty");
        assert!(!profiles.exists(1000));
        assert!(profiles.list().is_empty());
        assert!(profiles.restore(1000).unwrap().is_empty());
    }

    #[test]
    fn a_saved_profile_comes_back_over_someone_elses_settings() {
        let profiles = profiles("roundtrip");
        write(&profiles, "power", json!({ "version": 1, "mode": "eco" }));
        write(&profiles, "rgb", json!({ "version": 1, "brightness": 10 }));
        profiles.save(1000).unwrap();

        write(
            &profiles,
            "power",
            json!({ "version": 1, "mode": "performance" }),
        );
        write(&profiles, "rgb", json!({ "version": 1, "brightness": 200 }));
        profiles.save(1001).unwrap();

        assert_eq!(profiles.restore(1000).unwrap(), vec!["power", "rgb"]);
        assert_eq!(read(&profiles, "power")["mode"], "eco");
        assert_eq!(read(&profiles, "rgb")["brightness"], 10);

        profiles.restore(1001).unwrap();
        assert_eq!(read(&profiles, "power")["mode"], "performance");
        assert_eq!(profiles.list(), vec![1000, 1001]);
    }

    #[test]
    fn a_namespace_the_profile_lacks_keeps_what_is_in_use() {
        let profiles = profiles("partial");
        write(&profiles, "power", json!({ "version": 1, "mode": "eco" }));
        profiles.save(1000).unwrap();

        write(
            &profiles,
            "keymap",
            json!({ "version": 1, "enabled": true }),
        );
        assert_eq!(profiles.restore(1000).unwrap(), vec!["power"]);
        assert_eq!(read(&profiles, "keymap")["enabled"], true);
    }

    #[test]
    fn saving_again_drops_a_namespace_that_is_no_longer_set() {
        let profiles = profiles("drops");
        write(&profiles, "hotkey", json!({ "version": 1 }));
        profiles.save(1000).unwrap();
        fs::remove_file(profiles.live("hotkey")).unwrap();
        profiles.save(1000).unwrap();
        assert!(!profiles.exists(1000));
    }

    #[test]
    fn the_machines_settings_are_nobodys() {
        let profiles = profiles("machine");
        fs::write(profiles.live("debug"), r#"{"version":1,"enabled":true}"#).unwrap();
        profiles.save(1000).unwrap();
        assert!(!profiles.dir(1000).join("debug.json").exists());
    }

    #[test]
    fn what_was_measured_about_the_fans_does_not_travel_with_a_user() {
        let profiles = profiles("calibration");
        write(
            &profiles,
            "fan",
            json!({ "version": 1, "maWindow": 3, "fanMaxRpm": 5000, "speedControl": "untested" }),
        );
        profiles.save(1000).unwrap();

        // Recalibrated since, by whoever came next.
        write(
            &profiles,
            "fan",
            json!({ "version": 1, "maWindow": 9, "fanMaxRpm": 5800, "speedControl": "honoured" }),
        );

        profiles.restore(1000).unwrap();
        let fan = read(&profiles, "fan");
        assert_eq!(fan["maWindow"], 3, "the preference is the user's");
        assert_eq!(fan["fanMaxRpm"], 5800, "the measurement is the machine's");
        assert_eq!(fan["speedControl"], "honoured");
    }

    #[test]
    fn a_measurement_the_machine_no_longer_has_is_not_brought_back() {
        let profiles = profiles("uncalibrated");
        write(&profiles, "fan", json!({ "version": 1, "fanMaxRpm": 5000 }));
        profiles.save(1000).unwrap();
        write(&profiles, "fan", json!({ "version": 1 }));

        profiles.restore(1000).unwrap();
        assert!(read(&profiles, "fan").get("fanMaxRpm").is_none());
    }

    #[test]
    fn a_profile_that_is_not_json_is_copied_as_it_is() {
        let profiles = profiles("raw");
        fs::write(profiles.live("fan"), "{ not json").unwrap();
        profiles.save(1000).unwrap();
        write(&profiles, "fan", json!({ "version": 1, "fanMaxRpm": 5000 }));

        profiles.restore(1000).unwrap();
        assert_eq!(
            fs::read_to_string(profiles.live("fan")).unwrap(),
            "{ not json"
        );
    }
}
