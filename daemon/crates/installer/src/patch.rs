//! Patching the driver's C source before it is built.
//!
//! Ported from `_patch_driver_source` in the Python original. It is plain
//! text substitution rather than a diff, and it always works from a
//! pristine `hp-wmi.c.orig` snapshot so re-running never patches an
//! already-patched file.
//!
//! These are pure string functions on purpose: this is the one part of the
//! installer whose correctness can be checked without a kernel, a build
//! toolchain, or HP hardware.

use std::fs;
use std::path::Path;

use serde::Serialize;

/// Fan-ceiling constants the driver falls back to when its hardware query
/// returns nothing. Values are in hundreds of RPM (`60` means 6000 rpm).
///
/// Note this does **not** match the source project's own documentation,
/// which describes a single `OMEN_MAX_RPM`. The shipped `hp-wmi.c` splits
/// it per fan, and the source is what gets compiled - patching by the
/// documented name silently does nothing and leaves an uncalibrated
/// ceiling. The legacy name is still tried as a fallback so older driver
/// checkouts keep working.
const CPU_MAX_RPM_DEFINE: &str = "#define OMEN_CPU_MAX_RPM";
const GPU_MAX_RPM_DEFINE: &str = "#define OMEN_GPU_MAX_RPM";
const LEGACY_MAX_RPM_DEFINE: &str = "#define OMEN_MAX_RPM";

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not find `{0}` in the driver source; it may have changed shape upstream")]
    AnchorMissing(String),
}

/// The board-parameter variants `hp_wmi_feature_boards` entries point at.
///
/// They differ in which EC offset holds the current thermal profile, so
/// picking the wrong one gives a driver that loads and then misreads the
/// hardware - this is the choice a user has to make deliberately when
/// enabling an untested board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BoardParams {
    VictusS,
    OmenV1,
    OmenV1Legacy,
    OmenV1NoEc,
}

impl BoardParams {
    fn symbol(self) -> &'static str {
        match self {
            Self::VictusS => "victus_s_board_params",
            Self::OmenV1 => "omen_v1_board_params",
            Self::OmenV1Legacy => "omen_v1_legacy_board_params",
            Self::OmenV1NoEc => "omen_v1_no_ec_board_params",
        }
    }
}

/// Which table in `hp-wmi.c` an untested board should be added to.
///
/// The source project's documentation describes a
/// `victus_s_thermal_profile_boards` array; the shipped driver has no such
/// symbol. The real equivalent is the `hp_wmi_feature_boards` DMI table,
/// whose entries also select a [`BoardParams`] variant. Following the docs
/// here would produce an installer that always fails to patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "table", content = "params")]
pub enum BoardTable {
    /// `omen_thermal_profile_boards[]`
    OmenThermalProfile,
    /// `omen_thermal_profile_force_v0_boards[]`
    OmenForceV0,
    /// `omen_timed_thermal_profile_boards[]`
    OmenTimed,
    /// `victus_thermal_profile_boards[]`
    VictusThermalProfile,
    /// `hp_wmi_feature_boards[]`, a `dmi_system_id` table.
    Features(BoardParams),
}

impl BoardTable {
    /// Name of the C array the board id has to be inserted into.
    pub fn array_name(self) -> &'static str {
        match self {
            Self::OmenThermalProfile => "omen_thermal_profile_boards",
            Self::OmenForceV0 => "omen_thermal_profile_force_v0_boards",
            Self::OmenTimed => "omen_timed_thermal_profile_boards",
            Self::VictusThermalProfile => "victus_thermal_profile_boards",
            Self::Features(_) => "hp_wmi_feature_boards",
        }
    }

    /// The C source of one entry for this table.
    fn entry_for(self, board_name: &str) -> String {
        match self {
            Self::Features(params) => format!(
                "\t{{\n\t\t.matches = {{DMI_MATCH(DMI_BOARD_NAME, \"{board_name}\")}},\n\
                 \t\t.driver_data = (void *)&{},\n\t}},\n",
                params.symbol()
            ),
            _ => format!("\t\"{board_name}\",\n"),
        }
    }

    /// Whether the table ends in an empty sentinel entry that must stay last.
    fn has_sentinel(self) -> bool {
        matches!(self, Self::Features(_))
    }
}

/// Calibrated fan ceilings, in RPM. Either may be left unset, in which
/// case the driver keeps its own fallback for that fan.
#[derive(Debug, Clone, Copy, Default)]
pub struct MaxRpm {
    pub cpu: Option<u32>,
    pub gpu: Option<u32>,
}

impl MaxRpm {
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.gpu.is_none()
    }
}

/// Replaces the driver's fallback max-RPM constants with calibrated values.
///
/// The driver stores hundreds of RPM, so 6000 RPM is written as `60`.
/// Returns the patched source and a description of what was changed.
pub fn patch_max_rpm(source: &str, max_rpm: MaxRpm) -> Result<(String, Vec<String>), PatchError> {
    let mut patched = source.to_string();
    let mut applied = Vec::new();

    for (define, rpm) in [
        (CPU_MAX_RPM_DEFINE, max_rpm.cpu),
        (GPU_MAX_RPM_DEFINE, max_rpm.gpu),
    ] {
        let Some(rpm) = rpm else { continue };
        if let Some(updated) = replace_define_value(&patched, define, rpm / 100) {
            patched = updated;
            applied.push(format!("{define} = {} ({rpm} rpm)", rpm / 100));
        }
    }

    // Older driver checkouts have one shared constant instead of two.
    if applied.is_empty() {
        if let Some(rpm) = max_rpm.cpu.or(max_rpm.gpu) {
            if let Some(updated) = replace_define_value(&patched, LEGACY_MAX_RPM_DEFINE, rpm / 100) {
                patched = updated;
                applied.push(format!("{LEGACY_MAX_RPM_DEFINE} = {} ({rpm} rpm)", rpm / 100));
            }
        }
    }

    if applied.is_empty() && !max_rpm.is_empty() {
        // Never fail silently here: an unpatched ceiling means the fan
        // curve is calibrated against a number the driver doesn't use.
        return Err(PatchError::AnchorMissing(format!(
            "{CPU_MAX_RPM_DEFINE} / {GPU_MAX_RPM_DEFINE} / {LEGACY_MAX_RPM_DEFINE}"
        )));
    }

    Ok((patched, applied))
}

/// Rewrites the value of `#define <name> <value>`, keeping the rest of the
/// line. Returns `None` when the define isn't present.
fn replace_define_value(source: &str, define: &str, value: u32) -> Option<String> {
    let start = find_define(source, define)?;
    let line_end = source[start..]
        .find('\n')
        .map(|offset| start + offset)
        .unwrap_or(source.len());

    let mut patched = String::with_capacity(source.len());
    patched.push_str(&source[..start]);
    patched.push_str(&format!("{define} {value}"));
    patched.push_str(&source[line_end..]);
    Some(patched)
}

/// Finds a define, making sure the match is the whole macro name.
///
/// Without this, searching for `#define OMEN_MAX_RPM` would happily match
/// inside `#define OMEN_MAX_RPM_SOMETHING`, and searching for the legacy
/// name would match the CPU/GPU ones on a driver that has both.
fn find_define(source: &str, define: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = source[from..].find(define) {
        let start = from + offset;
        let after = source[start + define.len()..].chars().next();
        match after {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' => from = start + define.len(),
            _ => return Some(start),
        }
    }
    None
}

/// Byte range of a table's body, between its braces.
fn table_body(source: &str, table: BoardTable) -> Result<(usize, usize), PatchError> {
    let array = table.array_name();
    let Some(array_start) = source.find(array) else {
        return Err(PatchError::AnchorMissing(array.to_string()));
    };
    let Some(open) = source[array_start..].find('{').map(|o| array_start + o) else {
        return Err(PatchError::AnchorMissing(format!("opening brace of {array}")));
    };
    let Some(close) = find_matching_brace(source, open) else {
        return Err(PatchError::AnchorMissing(format!("closing brace of {array}")));
    };
    Ok((open, close))
}

/// Whether the driver already knows this board in the given table.
///
/// This is what decides whether an install needs to patch a board in at
/// all: a board the driver already lists needs no experimental entry, and
/// adding one would be a no-op the user was asked to authorise for nothing.
pub fn board_in_table(
    source: &str,
    table: BoardTable,
    board_name: &str,
) -> Result<bool, PatchError> {
    let (open, close) = table_body(source, table)?;
    Ok(source[open + 1..close].contains(&format!("\"{board_name}\"")))
}

/// Adds a board id to a thermal-profile table so an untested board takes an
/// existing code path.
///
/// Returns the source unchanged if the board is already listed, which makes
/// the whole patch step idempotent.
pub fn inject_board(
    source: &str,
    table: BoardTable,
    board_name: &str,
) -> Result<String, PatchError> {
    let (open, close) = table_body(source, table)?;

    if board_in_table(source, table, board_name)? {
        return Ok(source.to_string());
    }

    // A dmi_system_id table is scanned until its empty `{}` entry, so a
    // board added after the sentinel would never be matched. Plain string
    // arrays have no sentinel and simply take one more entry at the end.
    let insert_at = if table.has_sentinel() {
        find_sentinel(source, open + 1, close).unwrap_or(close)
    } else {
        close
    };

    let entry = table.entry_for(board_name);

    let mut patched = String::with_capacity(source.len() + entry.len());
    patched.push_str(&source[..insert_at]);
    patched.push_str(&entry);
    patched.push_str(&source[insert_at..]);
    Ok(patched)
}

/// Byte offset of the brace matching the one at `open`.
fn find_matching_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Offset where an entry must be inserted to land before the trailing `{}`
/// sentinel of a dmi_system_id table: the **start of the sentinel's line**,
/// not the `{}` itself.
///
/// Inserting at the `{}` would splice the new entry into the middle of that
/// line, after its leading tab - which compiles, but leaves the entry
/// double-indented and the sentinel with no indentation at all. This file
/// is upstream kernel code that someone may well read or send back, so it
/// should come out looking like the entries already in it.
fn find_sentinel(source: &str, from: usize, to: usize) -> Option<usize> {
    let body = &source[from..to];
    let sentinel = from + body.rfind("{}")?;
    Some(match source[..sentinel].rfind('\n') {
        Some(newline) => newline + 1,
        None => sentinel,
    })
}

/// Applies every patch to the driver tree on disk, in place.
///
/// `hp-wmi.c.orig` is snapshotted on first run and is the input every time
/// after that, so patching twice gives the same result as patching once.
pub fn patch_driver_tree(
    driver_dir: &Path,
    max_rpm: MaxRpm,
    experimental_board: Option<(BoardTable, &str)>,
) -> Result<Vec<String>, PatchError> {
    let source_path = driver_dir.join("hp-wmi-omen/hp-wmi.c");
    let pristine_path = driver_dir.join("hp-wmi-omen/hp-wmi.c.orig");

    let read = |path: &Path| {
        fs::read_to_string(path).map_err(|source| PatchError::Io {
            path: path.display().to_string(),
            source,
        })
    };

    if !pristine_path.exists() {
        let current = read(&source_path)?;
        fs::write(&pristine_path, &current).map_err(|source| PatchError::Io {
            path: pristine_path.display().to_string(),
            source,
        })?;
    }

    let mut source = read(&pristine_path)?;
    let mut applied = Vec::new();

    if !max_rpm.is_empty() {
        let (patched, changes) = patch_max_rpm(&source, max_rpm)?;
        source = patched;
        applied.extend(changes);
    }

    if let Some((table, board)) = experimental_board {
        let before = source.len();
        source = inject_board(&source, table, board)?;
        applied.push(if source.len() == before {
            format!("board {board} was already listed in {}", table.array_name())
        } else {
            format!("board {board} added to {}", table.array_name())
        });
    }

    fs::write(&source_path, source).map_err(|source| PatchError::Io {
        path: source_path.display().to_string(),
        source,
    })?;

    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
/* fallback */
#define OMEN_CPU_MAX_RPM 60
#define OMEN_GPU_MAX_RPM 58

static const char * const omen_thermal_profile_boards[] = {
	"8607", "8746", "8747",
};

static const struct dmi_system_id hp_wmi_feature_boards[] __initconst = {
	{
		.matches = {DMI_MATCH(DMI_BOARD_NAME, "8C99")},
		.driver_data = (void *)&victus_s_board_params,
	},
	{},
};
"#;

    #[test]
    fn max_rpm_is_written_in_hundreds_of_rpm() {
        let (patched, applied) =
            patch_max_rpm(SOURCE, MaxRpm { cpu: Some(6400), gpu: Some(5800) }).unwrap();
        assert!(patched.contains("#define OMEN_CPU_MAX_RPM 64"));
        assert!(patched.contains("#define OMEN_GPU_MAX_RPM 58"));
        assert_eq!(applied.len(), 2);
    }

    #[test]
    fn each_fan_can_be_calibrated_on_its_own() {
        let (patched, applied) =
            patch_max_rpm(SOURCE, MaxRpm { cpu: Some(7000), gpu: None }).unwrap();
        assert!(patched.contains("#define OMEN_CPU_MAX_RPM 70"));
        // The GPU fan keeps the driver's own fallback.
        assert!(patched.contains("#define OMEN_GPU_MAX_RPM 58"));
        assert_eq!(applied.len(), 1);
    }

    #[test]
    fn a_driver_with_the_old_single_constant_still_patches() {
        let legacy = "#define OMEN_MAX_RPM 60\n";
        let (patched, applied) =
            patch_max_rpm(legacy, MaxRpm { cpu: Some(6400), gpu: None }).unwrap();
        assert_eq!(patched, "#define OMEN_MAX_RPM 64\n");
        assert_eq!(applied.len(), 1);
    }

    #[test]
    fn the_legacy_name_does_not_match_the_per_fan_constants() {
        // "#define OMEN_MAX_RPM" is not a prefix of the CPU/GPU names, but
        // a sloppy search could still mangle the wrong line.
        let (patched, _) = patch_max_rpm(SOURCE, MaxRpm { cpu: Some(6400), gpu: None }).unwrap();
        assert!(patched.contains("#define OMEN_GPU_MAX_RPM 58"));
    }

    #[test]
    fn a_prefix_match_is_not_mistaken_for_the_define() {
        let source = "#define OMEN_CPU_MAX_RPM_LIMIT 99\n";
        // Only a longer macro is present, so there is nothing to patch and
        // the caller must be told rather than shipping an unpatched driver.
        assert!(patch_max_rpm(source, MaxRpm { cpu: Some(6000), gpu: None }).is_err());
    }

    #[test]
    fn a_missing_anchor_is_an_error_rather_than_a_silent_no_op() {
        // If upstream renames the constants, the build must not quietly
        // produce a driver with the wrong fan ceiling.
        assert!(patch_max_rpm("int main(void) { return 0; }", MaxRpm { cpu: Some(6000), gpu: None })
            .is_err());
    }

    #[test]
    fn asking_for_no_calibration_is_not_an_error() {
        let (patched, applied) = patch_max_rpm(SOURCE, MaxRpm::default()).unwrap();
        assert_eq!(patched, SOURCE);
        assert!(applied.is_empty());
    }

    #[test]
    fn a_board_is_appended_to_the_plain_string_table() {
        let patched = inject_board(SOURCE, BoardTable::OmenThermalProfile, "8D41").unwrap();
        assert!(patched.contains("\"8D41\""));
        // ...inside the right array, before its closing brace.
        let array = patched
            .split("omen_thermal_profile_boards[] = {")
            .nth(1)
            .unwrap()
            .split("};")
            .next()
            .unwrap();
        assert!(array.contains("\"8D41\""));
    }

    #[test]
    fn injecting_a_board_twice_changes_nothing() {
        let once = inject_board(SOURCE, BoardTable::OmenThermalProfile, "8D41").unwrap();
        let twice = inject_board(&once, BoardTable::OmenThermalProfile, "8D41").unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn a_board_is_recognised_in_the_table_that_lists_it() {
        assert!(board_in_table(SOURCE, BoardTable::OmenThermalProfile, "8746").unwrap());
        assert!(!board_in_table(SOURCE, BoardTable::OmenThermalProfile, "8D41").unwrap());
        // Listed in one table says nothing about another: 8C99 has feature
        // data but is not an omen thermal-profile board.
        assert!(board_in_table(SOURCE, BoardTable::Features(BoardParams::VictusS), "8C99").unwrap());
        assert!(!board_in_table(SOURCE, BoardTable::OmenThermalProfile, "8C99").unwrap());
    }

    #[test]
    fn an_already_listed_board_is_left_alone() {
        let patched = inject_board(SOURCE, BoardTable::OmenThermalProfile, "8746").unwrap();
        assert_eq!(patched, SOURCE);
    }

    #[test]
    fn the_victus_s_sentinel_stays_last() {
        let patched = inject_board(SOURCE, BoardTable::Features(BoardParams::VictusS), "8D41").unwrap();
        let table = patched.split("hp_wmi_feature_boards[] __initconst = {").nth(1).unwrap();
        let new_entry = table.find("8D41").unwrap();
        let sentinel = table.find("{}").unwrap();
        // A dmi_system_id table is scanned until the empty entry, so a board
        // added after it would never be matched.
        assert!(new_entry < sentinel, "the board must come before the {{}} sentinel");
    }

    /// The injected entry has to look like the ones already there: this is
    /// upstream kernel source, and a double-indented entry next to a
    /// de-indented sentinel is the kind of thing that gets noticed when the
    /// file is pasted into a bug report.
    #[test]
    fn an_injected_entry_keeps_the_tables_indentation() {
        let patched =
            inject_board(SOURCE, BoardTable::Features(BoardParams::VictusS), "8D2F").unwrap();

        assert!(
            patched.contains("\t{\n\t\t.matches = {DMI_MATCH(DMI_BOARD_NAME, \"8D2F\")},"),
            "the entry should open at one tab:\n{patched}"
        );
        // ...and the sentinel keeps its own line and its own indentation.
        assert!(patched.contains("\t},\n\t{},\n"), "sentinel lost its line:\n{patched}");
    }

    #[test]
    fn boards_go_into_the_table_the_profile_names() {
        let patched =
            inject_board(SOURCE, BoardTable::Features(BoardParams::VictusS), "8ABC").unwrap();
        let omen_table = patched
            .split("omen_thermal_profile_boards[] = {")
            .nth(1)
            .unwrap()
            .split("};")
            .next()
            .unwrap();
        assert!(!omen_table.contains("8ABC"));
    }

    #[test]
    fn an_unknown_table_is_reported_rather_than_guessed_at() {
        let source = "static const char * const something_else[] = { \"1\" };";
        assert!(inject_board(source, BoardTable::OmenThermalProfile, "8D41").is_err());
    }
}

/// Checks the patcher against the real upstream driver source rather than
/// only against the synthetic snippet above - anchors that exist in a test
/// fixture but not in the actual file would otherwise go unnoticed until
/// someone tried to install.
///
/// Skips when the sources aren't checked out; see `detect::find_driver_source`.
#[cfg(test)]
mod upstream_source_tests {
    use super::*;

    fn real_source() -> Option<String> {
        let dir = crate::detect::Environment::detect().driver_source?;
        fs::read_to_string(dir.join("hp-wmi-omen/hp-wmi.c.orig"))
            .or_else(|_| fs::read_to_string(dir.join("hp-wmi-omen/hp-wmi.c")))
            .ok()
    }

    #[test]
    fn every_anchor_exists_in_the_real_driver() {
        let Some(source) = real_source() else {
            eprintln!("skipped: driver sources not found");
            return;
        };

        let (_, applied) = patch_max_rpm(&source, MaxRpm { cpu: Some(6000), gpu: Some(5800) })
            .expect("max-rpm anchors missing");
        assert_eq!(applied.len(), 2, "expected both per-fan constants: {applied:?}");
        for table in [
            BoardTable::OmenThermalProfile,
            BoardTable::OmenForceV0,
            BoardTable::OmenTimed,
            BoardTable::VictusThermalProfile,
            BoardTable::Features(BoardParams::VictusS),
        ] {
            assert!(
                inject_board(&source, table, "FFFF").is_ok(),
                "{} table not found in the real driver",
                table.array_name()
            );
        }
    }

    #[test]
    fn patching_the_real_driver_changes_exactly_one_line() {
        let Some(source) = real_source() else {
            eprintln!("skipped: driver sources not found");
            return;
        };

        let (patched, _) = patch_max_rpm(&source, MaxRpm { cpu: Some(6400), gpu: None }).unwrap();
        let differing = source
            .lines()
            .zip(patched.lines())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(differing, 1, "patching the cpu ceiling should touch one line only");
        assert_eq!(source.lines().count(), patched.lines().count());
    }

    #[test]
    fn a_board_injected_into_the_real_driver_lands_inside_its_table() {
        let Some(source) = real_source() else {
            eprintln!("skipped: driver sources not found");
            return;
        };

        let patched = inject_board(&source, BoardTable::OmenThermalProfile, "FFFF").unwrap();
        let table_start = patched.find("omen_thermal_profile_boards").unwrap();
        let open = patched[table_start..].find('{').unwrap() + table_start;
        let close = find_matching_brace(&patched, open).unwrap();
        let inserted = patched.find("\"FFFF\"").expect("board should be present");
        assert!(open < inserted && inserted < close);
    }
}
