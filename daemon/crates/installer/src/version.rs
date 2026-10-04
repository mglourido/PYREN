//! Which `hp-wmi` is installed here, and whether it is the one this build
//! of Pyren ships.
//!
//! The driver is not embedded in the daemon: an install copies the tree
//! [`find_driver_source`](crate::detect) located to `/usr/src` and builds
//! that copy. So updating Pyren replaces the sources a *future* install
//! would use and leaves the module that is already built exactly as it
//! was - nothing rebuilds it, and until this module existed nothing said
//! so either. A machine could run a driver several upstream revisions old
//! behind an app that had been updated every week.
//!
//! # What identifies a driver
//!
//! The sha256 of the **pristine** source, `hp-wmi.c.orig` - never of
//! `hp-wmi.c`. The patcher writes this machine's fan ceilings and board id
//! into `hp-wmi.c`, so two installs of the same upstream revision differ
//! there and would each look "outdated" to the other. `.orig` is the
//! snapshot the patcher always starts from and never writes, so it is the
//! same bytes on every machine that installed the same revision.
//!
//! A hash has no order, so the verdict is "differs from the bundled one",
//! not "older than". In practice a difference means older, because the
//! only thing that changes the bundled driver is a Pyren update, but a
//! downgrade of Pyren reports the same way and is just as true: a
//! reinstall brings the two back in line in either direction.
//!
//! # Where the identity of an install is recorded
//!
//! In [`STAMP_FILE`], at the top of the staged tree under `/usr/src`,
//! written by the install's `record-driver-version` step once the module
//! is in place. Living inside the tree it describes is the point:
//!
//! - it survives an update of Pyren, which never touches `/usr/src`;
//! - the next install wipes it along with the tree (`stage-source`) and
//!   only writes a new one after its own module is installed;
//! - restoring the stock driver deletes it with the tree
//!   (`remove-sources`), with no second path to remember.
//!
//! A stamp can therefore never describe sources that are no longer there.
//!
//! # An install that did not finish
//!
//! `stage-source` replaces the tree before anything is built, so a build
//! that then fails leaves the *new* sources under `/usr/src` beside the
//! *old* module, and no stamp. Hashing those sources would report the
//! driver this build ships as installed when it is not. So staging also
//! leaves [`PENDING_FILE`] in the tree, and the stamp takes it away again:
//! while it is there the tree is known not to describe the module, and the
//! verdict is [`DriverVersionState::Unknown`].
//!
//! # Installs made before the stamp existed
//!
//! Have no stamp, and must not be reported as up to date on that account.
//! The staged `.orig` is still on disk, so it is hashed instead and the
//! answer is marked as coming from the source rather than from a stamp.
//! When that is missing too - a trace of an install with no tree left -
//! the verdict is [`DriverVersionState::Unknown`]: not "current", which
//! would hide a stale driver, and not "outdated", which would nag about
//! something nobody has established.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::detect::Environment;
use crate::plan::{DKMS_NAME, DKMS_VERSION};

/// The stamp's file name inside the staged tree. Not under `src/`, which
/// is what the kernel build system is pointed at.
pub const STAMP_FILE: &str = "pyren-driver.json";

/// Left in the staged tree from `stage-source` until the stamp is written:
/// the sources are there and the module built from them is not known to be.
pub const PENDING_FILE: &str = "pyren-driver.pending";

/// `driver/README.md`, compiled in for its provenance table.
///
/// The README is where a maintainer already records the upstream commit
/// and the hash when replacing the vendored tree, so the label shown to
/// the user is read from there instead of being typed a second time into
/// a constant here. `the_recorded_provenance_matches_the_vendored_driver`
/// fails when that table and the file it describes disagree.
const VENDORED_README: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../driver/README.md"
));

/// The pristine source, relative to a driver *source* tree (this
/// repository's `driver/`, or `/usr/share/pyren/driver`).
const PRISTINE_IN_SOURCE: &str = "hp-wmi-omen/hp-wmi.c.orig";
/// What a source tree that lost its `.orig` would be installed from: the
/// patcher snapshots this file as the pristine baseline in that case.
const UNPATCHED_IN_SOURCE: &str = "hp-wmi-omen/hp-wmi.c";

/// The same two files, relative to the *staged* tree, where `stage-source`
/// puts the driver one level down under `src/`.
const PRISTINE_IN_STAGE: &str = "src/hp-wmi-omen/hp-wmi.c.orig";
const UNPATCHED_IN_STAGE: &str = "src/hp-wmi-omen/hp-wmi.c";

/// Where an install stages the driver, and so where its stamp lives.
pub fn stage_dir() -> PathBuf {
    PathBuf::from(format!("/usr/src/{DKMS_NAME}-{DKMS_VERSION}"))
}

/// One revision of the driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverIdentity {
    /// Lowercase hex sha256 of the pristine `hp-wmi.c.orig`. The identity
    /// itself: two drivers are the same revision exactly when this is equal.
    pub sha256: String,
    /// Something a person can read - the upstream commit and the day it was
    /// vendored, e.g. `2d3f2a4 (2026-10-04)`. Decoration only, never
    /// compared. `None` for a revision this build has no record of, which
    /// is every install that predates the stamp and no longer matches.
    #[serde(default)]
    pub label: Option<String>,
}

/// How the installed driver's identity was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum IdentitySource {
    /// Read from the stamp the install wrote.
    Stamp,
    /// No usable stamp: the staged `hp-wmi.c.orig` was hashed instead.
    Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DriverVersionState {
    /// The patched driver is not installed at all. Nothing to update, and
    /// already somebody else's notice ("driver not installed").
    NotInstalled,
    /// Installed, and the same revision this build ships.
    Current,
    /// Installed, and a different revision from the one this build ships.
    Outdated,
    /// Installed, but one side of the comparison could not be established.
    Unknown,
}

/// The answer `installer.inspect` gives under `driverVersion`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverVersion {
    pub state: DriverVersionState,
    /// `state == outdated`, spelled out so a client that only wants to know
    /// whether to offer a reinstall does not have to know the other states.
    pub outdated: bool,
    pub installed: Option<DriverIdentity>,
    /// `None` exactly when `installed` is.
    pub installed_from: Option<IdentitySource>,
    /// What an install started now would put on the machine. `None` when
    /// no driver sources were found.
    pub bundled: Option<DriverIdentity>,
}

impl DriverVersion {
    pub fn detect(env: &Environment) -> Self {
        let bundled = env.driver_source.as_deref().and_then(bundled_identity);
        let installed = env
            .patched_driver_installed
            .then(|| installed_identity(&stage_dir()))
            .flatten();
        Self::assess(env.patched_driver_installed, installed, bundled)
    }

    /// The verdict, from facts already gathered. Separate from
    /// [`detect`](Self::detect) so every combination can be tested on a
    /// machine that has none of them.
    pub fn assess(
        patched_driver_installed: bool,
        installed: Option<(DriverIdentity, IdentitySource)>,
        bundled: Option<DriverIdentity>,
    ) -> Self {
        // Checked first, and on the installer's own evidence rather than on
        // whether a stamp was found: a stock driver is "not installed"
        // whatever happens to be lying under /usr/src.
        let installed = installed.filter(|_| patched_driver_installed);
        let state = match (patched_driver_installed, &installed, &bundled) {
            (false, _, _) => DriverVersionState::NotInstalled,
            (true, Some((installed, _)), Some(bundled)) => {
                if installed.sha256 == bundled.sha256 {
                    DriverVersionState::Current
                } else {
                    DriverVersionState::Outdated
                }
            }
            (true, _, _) => DriverVersionState::Unknown,
        };
        let (installed, installed_from) = match installed {
            Some((identity, source)) => (Some(identity), Some(source)),
            None => (None, None),
        };
        Self {
            state,
            outdated: state == DriverVersionState::Outdated,
            installed,
            installed_from,
            bundled,
        }
    }
}

/// What `driver/README.md`'s provenance table says about the vendored tree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Provenance {
    commit: String,
    sha256: String,
    copied_on: String,
}

impl Provenance {
    fn label(&self) -> String {
        format!("{} ({})", self.commit, self.copied_on)
    }
}

/// Reads the three rows of the provenance table that identify a revision.
///
/// Deliberately strict: a row that is missing or reworded yields `None`
/// rather than a label made of whatever was found, and the test below
/// turns that `None` into a failure at the moment the README is edited.
fn parse_provenance(readme: &str) -> Option<Provenance> {
    let cell = |row_name: &str| -> Option<&str> {
        readme.lines().find_map(|line| {
            let mut cells = line.trim().trim_matches('|').split('|').map(str::trim);
            (cells.next()? == row_name).then(|| cells.next()).flatten()
        })
    };
    // The first `backticked` word in a cell that satisfies `wanted`.
    let ticked = |cell: &str, wanted: fn(&str) -> bool| -> Option<String> {
        cell.split('`')
            .skip(1)
            .step_by(2)
            .find(|word| wanted(word))
            .map(str::to_string)
    };

    let commit = ticked(cell("Taken from")?, |word| {
        (7..=40).contains(&word.len()) && is_lower_hex(word)
    })?;
    let sha256 = ticked(cell("`hp-wmi.c` sha256")?, is_sha256)?;
    let copied_on = cell("Copied on")?.to_string();
    (!copied_on.is_empty()).then_some(Provenance {
        commit,
        sha256,
        copied_on,
    })
}

fn is_lower_hex(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && is_lower_hex(text)
}

/// The label for `sha256`, if it is the revision this build was vendored
/// with. Any other hash gets none: the README knows one revision, and
/// lending its commit to a different file would be a lie with a hash
/// beside it to prove it.
fn label_for(sha256: &str) -> Option<String> {
    parse_provenance(VENDORED_README)
        .filter(|provenance| provenance.sha256 == sha256)
        .map(|provenance| provenance.label())
}

fn sha256_of(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(
        Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// The first of `candidates` under `root` that can be read, hashed.
fn identity_of(root: &Path, candidates: &[&str]) -> Option<DriverIdentity> {
    let sha256 = candidates
        .iter()
        .find_map(|file| sha256_of(&root.join(file)))?;
    Some(DriverIdentity {
        label: label_for(&sha256),
        sha256,
    })
}

/// The identity of the driver in a *source* tree: what installing from it
/// would put on the machine.
///
/// Hashed from the tree that was actually found rather than taken from a
/// constant compiled into the daemon, because that tree is a directory on
/// disk (`/usr/share/pyren/driver`, or wherever `PYREN_DRIVER_DIR` points)
/// and is what an install really copies. A constant would keep claiming
/// the revision the daemon was built beside.
pub fn bundled_identity(source: &Path) -> Option<DriverIdentity> {
    identity_of(source, &[PRISTINE_IN_SOURCE, UNPATCHED_IN_SOURCE])
}

/// The identity of the driver staged under `stage`, and how it was found.
///
/// Only ever falls back to `.orig`, never to the staged `hp-wmi.c`: that
/// file has been patched for this machine, and its hash would call every
/// install outdated. And never while [`PENDING_FILE`] is there: those
/// sources belong to an install that did not finish, not to the module.
pub fn installed_identity(stage: &Path) -> Option<(DriverIdentity, IdentitySource)> {
    if stage.join(PENDING_FILE).exists() {
        return None;
    }
    if let Some(stamp) = read_stamp(stage) {
        return Some((stamp, IdentitySource::Stamp));
    }
    identity_of(stage, &[PRISTINE_IN_STAGE]).map(|identity| (identity, IdentitySource::Source))
}

/// A stamp that says something usable, or nothing.
///
/// A file that does not parse, or whose hash is not a hash, is treated as
/// absent so the caller falls back to the source - a truncated stamp must
/// not turn a driver that can still be identified into an unknown one, and
/// must certainly not compare unequal to everything and report "outdated".
fn read_stamp(stage: &Path) -> Option<DriverIdentity> {
    let text = fs::read_to_string(stage.join(STAMP_FILE)).ok()?;
    let stamp: DriverIdentity = serde_json::from_str(&text).ok()?;
    is_sha256(&stamp.sha256).then_some(stamp)
}

/// What the stamp holds on disk: the identity, plus the Pyren version that
/// installed it. The version is for a person reading the file or a bug
/// report; nothing compares it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Stamp<'a> {
    sha256: &'a str,
    label: Option<&'a str>,
    installed_by: &'static str,
}

/// Marks the tree under `stage` as staged but not yet installed. Called by
/// `stage-source`; undone by [`record_installed`].
pub fn mark_pending(stage: &Path) -> Result<(), String> {
    let path = stage.join(PENDING_FILE);
    fs::write(&path, "").map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Records the identity of the driver staged under `stage` beside it.
///
/// Hashes the staged tree, not the tree it was copied from: this is a
/// record of what was built, and the copy is the thing that was built.
/// `.orig` is preferred; the unpatched `hp-wmi.c` only stands in when an
/// install with `skipPatches` staged a tree that never had one, since
/// nothing has rewritten the file in that case.
pub fn record_installed(stage: &Path) -> Result<DriverIdentity, String> {
    let identity =
        identity_of(stage, &[PRISTINE_IN_STAGE, UNPATCHED_IN_STAGE]).ok_or_else(|| {
            format!(
                "no pristine driver source under {} to identify",
                stage.display()
            )
        })?;
    let stamp = Stamp {
        sha256: &identity.sha256,
        label: identity.label.as_deref(),
        installed_by: env!("CARGO_PKG_VERSION"),
    };
    let mut text = serde_json::to_string_pretty(&stamp).map_err(|e| e.to_string())?;
    text.push('\n');
    let path = stage.join(STAMP_FILE);
    fs::write(&path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
    // Last, and only once the stamp is on disk: a marker that outlived a
    // failed write keeps the verdict at "unknown" rather than trusting a
    // tree nothing vouches for.
    let pending = stage.join(PENDING_FILE);
    if pending.exists() {
        fs::remove_file(&pending).map_err(|e| format!("removing {}: {e}", pending.display()))?;
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::REPO_DRIVER_DIR;

    /// A staged tree in a temporary directory, holding `pristine` as its
    /// `.orig` and something else as the patched `hp-wmi.c`.
    fn staged(name: &str, pristine: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pyren-version-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/hp-wmi-omen")).unwrap();
        fs::write(dir.join(PRISTINE_IN_STAGE), pristine).unwrap();
        fs::write(dir.join(UNPATCHED_IN_STAGE), b"patched for this machine").unwrap();
        dir
    }

    fn identity(sha256: &str) -> DriverIdentity {
        DriverIdentity {
            sha256: sha256.to_string(),
            label: None,
        }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// The guard for updating `driver/`: replacing the files without
    /// updating the README's table (or the reverse) fails here, instead of
    /// shipping a driver whose label names a different commit.
    #[test]
    fn the_recorded_provenance_matches_the_vendored_driver() {
        let provenance = parse_provenance(VENDORED_README)
            .expect("driver/README.md has no readable provenance table");
        let source = Path::new(REPO_DRIVER_DIR);

        let pristine = sha256_of(&source.join(PRISTINE_IN_SOURCE)).expect("hp-wmi.c.orig");
        assert_eq!(
            provenance.sha256, pristine,
            "driver/README.md records a different sha256 from the vendored hp-wmi.c.orig"
        );
        assert_eq!(
            sha256_of(&source.join(UNPATCHED_IN_SOURCE)).expect("hp-wmi.c"),
            pristine,
            "hp-wmi.c and hp-wmi.c.orig must be byte-identical in the vendored tree"
        );
    }

    #[test]
    fn the_bundled_driver_carries_the_label_from_the_readme() {
        let bundled = bundled_identity(Path::new(REPO_DRIVER_DIR)).unwrap();
        let provenance = parse_provenance(VENDORED_README).unwrap();
        assert_eq!(bundled.label, Some(provenance.label()));
        assert!(bundled.label.unwrap().starts_with(&provenance.commit));
    }

    #[test]
    fn a_reworded_provenance_table_is_no_provenance_rather_than_a_guess() {
        let readme = "| Taken from | upstream main, some time ago |\n\
                      | `hp-wmi.c` sha256 | `not-a-hash` |\n\
                      | Copied on | 2026-10-04 |\n";
        assert_eq!(parse_provenance(readme), None);
    }

    #[test]
    fn a_revision_the_readme_does_not_describe_gets_no_label() {
        let dir = staged("unlabelled", b"some other revision");
        let (found, _) = installed_identity(&dir).unwrap();
        assert_eq!(found.label, None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The property the whole design rests on: the stamp names the
    /// pristine source, so the per-machine patch does not change it.
    #[test]
    fn the_stamp_identifies_the_pristine_source_not_the_patched_file() {
        let dir = staged("stamp", b"pristine upstream source");

        let recorded = record_installed(&dir).unwrap();

        assert_eq!(
            Some(recorded.sha256.clone()),
            sha256_of(&dir.join(PRISTINE_IN_STAGE))
        );
        assert_ne!(
            Some(recorded.sha256.clone()),
            sha256_of(&dir.join(UNPATCHED_IN_STAGE))
        );
        assert_eq!(
            installed_identity(&dir),
            Some((recorded, IdentitySource::Stamp))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stamp_is_believed_over_the_source_beside_it() {
        let dir = staged("stamp-wins", b"pristine upstream source");
        fs::write(
            dir.join(STAMP_FILE),
            format!(r#"{{ "sha256": "{A}", "label": "abc1234 (2026-01-01)" }}"#),
        )
        .unwrap();

        let (found, from) = installed_identity(&dir).unwrap();

        assert_eq!(from, IdentitySource::Stamp);
        assert_eq!(found.sha256, A);
        assert_eq!(found.label.as_deref(), Some("abc1234 (2026-01-01)"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Every install made before this module existed.
    #[test]
    fn an_install_with_no_stamp_is_identified_from_its_pristine_source() {
        let dir = staged("no-stamp", b"an older upstream source");

        let (found, from) = installed_identity(&dir).unwrap();

        assert_eq!(from, IdentitySource::Source);
        assert_eq!(
            Some(found.sha256),
            sha256_of(&dir.join(PRISTINE_IN_STAGE)),
            "the pristine snapshot, not the patched hp-wmi.c"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_stamp_falls_back_to_the_source_instead_of_reporting_a_difference() {
        let dir = staged("corrupt", b"pristine upstream source");
        for garbage in ["", "{ not json", r#"{ "sha256": "tampered" }"#] {
            fs::write(dir.join(STAMP_FILE), garbage).unwrap();
            let (found, from) = installed_identity(&dir).unwrap();
            assert_eq!(from, IdentitySource::Source, "stamp: {garbage:?}");
            assert_eq!(
                Some(found.sha256),
                sha256_of(&dir.join(PRISTINE_IN_STAGE)),
                "stamp: {garbage:?}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Staging succeeded and the build after it failed: the new sources
    /// sit beside the old module. They must not be read as the installed
    /// driver - that would report the bundled revision as current.
    #[test]
    fn sources_staged_by_an_install_that_did_not_finish_identify_nothing() {
        let dir = staged("pending", b"the revision this build ships");
        mark_pending(&dir).unwrap();

        assert_eq!(installed_identity(&dir), None);
        let version = DriverVersion::assess(true, installed_identity(&dir), Some(identity(B)));
        assert_eq!(version.state, DriverVersionState::Unknown);

        // The install that does finish clears the marker with its stamp.
        let recorded = record_installed(&dir).unwrap();
        assert!(!dir.join(PENDING_FILE).exists());
        assert_eq!(
            installed_identity(&dir),
            Some((recorded, IdentitySource::Stamp))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A leftover stamp does not outrank the marker: whatever the stamp
    /// says, the tree was replaced after it was written.
    #[test]
    fn a_pending_marker_outranks_a_stamp() {
        let dir = staged("pending-stamp", b"pristine upstream source");
        record_installed(&dir).unwrap();
        mark_pending(&dir).unwrap();
        assert_eq!(installed_identity(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A staged tree holding only a patched `hp-wmi.c` cannot be
    /// identified, and saying so is the only honest answer.
    #[test]
    fn a_patched_file_alone_never_identifies_an_install() {
        let dir = staged("patched-only", b"x");
        fs::remove_file(dir.join(PRISTINE_IN_STAGE)).unwrap();
        assert_eq!(installed_identity(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tree_with_nothing_to_hash_is_not_stamped() {
        let dir = std::env::temp_dir().join(format!("pyren-version-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        assert!(record_installed(&dir).is_err());
        assert!(!dir.join(STAMP_FILE).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_revision_is_current_and_a_different_one_is_outdated() {
        let same = DriverVersion::assess(
            true,
            Some((identity(A), IdentitySource::Stamp)),
            Some(identity(A)),
        );
        assert_eq!(same.state, DriverVersionState::Current);
        assert!(!same.outdated);

        let different = DriverVersion::assess(
            true,
            Some((identity(A), IdentitySource::Source)),
            Some(identity(B)),
        );
        assert_eq!(different.state, DriverVersionState::Outdated);
        assert!(different.outdated);
        assert_eq!(different.installed_from, Some(IdentitySource::Source));
    }

    /// Labels are decoration: a stamp written by a build with a different
    /// README wording is still the same driver.
    #[test]
    fn only_the_hash_is_compared_never_the_label() {
        let installed = DriverIdentity {
            sha256: A.to_string(),
            label: Some("2d3f2a4 (2026-10-04)".to_string()),
        };
        let version = DriverVersion::assess(
            true,
            Some((installed, IdentitySource::Stamp)),
            Some(identity(A)),
        );
        assert_eq!(version.state, DriverVersionState::Current);
    }

    #[test]
    fn an_install_that_cannot_be_identified_is_unknown_not_current_and_not_outdated() {
        let version = DriverVersion::assess(true, None, Some(identity(B)));
        assert_eq!(version.state, DriverVersionState::Unknown);
        assert!(!version.outdated);
        assert_eq!(version.installed, None);
        assert_eq!(version.installed_from, None);
    }

    #[test]
    fn with_no_bundled_driver_to_compare_against_the_verdict_is_unknown() {
        let version = DriverVersion::assess(true, Some((identity(A), IdentitySource::Stamp)), None);
        assert_eq!(version.state, DriverVersionState::Unknown);
        assert!(!version.outdated);
    }

    /// The stock driver is the existing "driver not installed" notice's
    /// business. A leftover tree under /usr/src must not earn it a second
    /// one about being out of date.
    #[test]
    fn a_stock_driver_is_not_installed_rather_than_outdated() {
        let version = DriverVersion::assess(
            false,
            Some((identity(A), IdentitySource::Source)),
            Some(identity(B)),
        );
        assert_eq!(version.state, DriverVersionState::NotInstalled);
        assert!(!version.outdated);
        assert_eq!(version.installed, None);
    }

    #[test]
    fn the_wire_format_is_camel_case() {
        let version = DriverVersion::assess(
            true,
            Some((identity(A), IdentitySource::Source)),
            Some(identity(B)),
        );
        let json = serde_json::to_value(&version).unwrap();
        assert_eq!(json["state"], "outdated");
        assert_eq!(json["outdated"], true);
        assert_eq!(json["installedFrom"], "source");
        assert_eq!(json["installed"]["sha256"], A);
        assert_eq!(json["bundled"]["label"], serde_json::Value::Null);

        let stock = serde_json::to_value(DriverVersion::assess(false, None, None)).unwrap();
        assert_eq!(stock["state"], "notInstalled");
    }
}
