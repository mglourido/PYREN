//! Working out the installer's inputs instead of asking for them.
//!
//! An install used to need four answers typed by hand: the two fan
//! ceilings, the board id, and which of the driver's tables that board
//! belongs in. Every one of them is already knowable on the machine -
//! DMI carries the board id and the model name, the driver source carries
//! the tables, and `fan.calibrate` carries the measured ceilings - so
//! asking for them was asking the user to look up facts that are sitting
//! in `/sys` and in a C file.
//!
//! What is *not* knowable is which board-params variant an unlisted board
//! wants: the variants differ in the EC offset the driver reads a thermal
//! profile back from, and DMI does not say. So this picks the conservative
//! variant of the right family and says so in a note, rather than pretending
//! to know. The manual fields stay, and override anything decided here.

use std::path::Path;

use pyren_core::{msg, Msg};
use serde::{Deserialize, Serialize};

use crate::detect::Environment;
use crate::ec::EcProbe;
use crate::patch::{board_in_table, BoardParams, BoardTable, MaxRpm};

const DMI: &str = "/sys/class/dmi/id";

/// Which of HP's two gaming families this machine is, as far as DMI says.
///
/// It decides the thermal-profile values the driver writes over WMI - the
/// OMEN and Victus S code paths use different profile ids - so it is the
/// one part of the board choice that must not be guessed at random.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Family {
    Omen,
    Victus,
    Unknown,
}

/// Whether the board-params variant changes anything on this board.
///
/// All four variants share one `fan_profile`, so none of them affects fan
/// control - the reason the driver gets installed at all. What they do
/// affect is which EC byte the *thermal profile* is read back from, and
/// even that only reaches the hardware when the driver takes the Victus S
/// profile path. `hp_wmi_platform_profile_setup` tests
/// `is_omen_thermal_profile()` and `is_victus_thermal_profile()` first, so
/// a board already in either of those tables takes that path and the
/// variant's `thermal_profile` field is never read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParamsEffect {
    /// The board is in `omen_thermal_profile_boards`, so the driver takes
    /// the OMEN path and reads EC 0x95 regardless. The variant is inert.
    InertOmenPath,
    /// The same, via `victus_thermal_profile_boards`.
    InertVictusPath,
    /// In neither table: the variant decides the readback offset, and is
    /// worth getting right.
    DecidesReadback,
}

/// Where a suggested fan ceiling came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpmSource {
    /// Measured by `fan.calibrate` and persisted in `fan.json`.
    Calibrated,
    /// Nothing measured; the driver's own fallback is left in place.
    DriverFallback,
}

/// The DMI facts the decision is made from, kept apart from the filesystem
/// so the decision itself is testable on any machine.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dmi {
    pub board_name: Option<String>,
    pub product_name: Option<String>,
    pub product_family: Option<String>,
    pub sys_vendor: Option<String>,
}

impl Dmi {
    pub fn read() -> Self {
        Self {
            board_name: dmi("board_name"),
            product_name: dmi("product_name"),
            product_family: dmi("product_family"),
            sys_vendor: dmi("sys_vendor"),
        }
    }

    /// OMEN or Victus, from whichever DMI string names the model.
    ///
    /// `board_name` is deliberately not consulted: it is a four-character
    /// code like `8D2F` that says nothing about the family.
    pub fn family(&self) -> Family {
        let haystack = [&self.product_name, &self.product_family, &self.sys_vendor]
            .into_iter()
            .flatten()
            .map(|s| s.to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(" ");

        if haystack.contains("VICTUS") {
            Family::Victus
        } else if haystack.contains("OMEN") {
            Family::Omen
        } else {
            Family::Unknown
        }
    }
}

/// Everything the installer would otherwise have asked for, plus the
/// reasoning, so the wizard can show what it decided and why.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Autodetected {
    pub dmi: Dmi,
    pub family: Family,
    /// True when the driver already lists this board in
    /// `hp_wmi_feature_boards`, so nothing needs injecting.
    pub board_known: bool,
    /// The board id to add, or `None` when it is already listed, unknown,
    /// or the family could not be told apart.
    pub experimental_board: Option<String>,
    pub board_table: Option<BoardTable>,
    /// Whether the params variant above changes anything here.
    pub params_effect: ParamsEffect,
    /// What the embedded controller said, when it was asked.
    pub ec: EcProbe,
    pub cpu_max_rpm: Option<u32>,
    pub gpu_max_rpm: Option<u32>,
    pub rpm_source: RpmSource,
    /// Translatable - render each with `tm()`.
    pub notes: Vec<Msg>,
}

impl Autodetected {
    /// Surveys the machine: DMI, the driver's own tables, and any measured
    /// fan ceilings.
    pub fn detect(env: &Environment, probe_ec: bool) -> Self {
        let source = env
            .driver_source
            .as_ref()
            .and_then(|dir| read_driver_source(dir));
        let (max_rpm, rpm_source) = calibrated_max_rpm();
        // Asked for only when it can change the answer. Loading a module
        // to settle a question whose answer is inert would be a side
        // effect bought for nothing.
        let ec = if probe_ec { EcProbe::detect(true) } else { EcProbe::NotProbed };
        decide(Dmi::read(), source.as_deref(), max_rpm, rpm_source, ec)
    }

    /// The board fields as `execute` wants them, or `None` when there is
    /// nothing to inject.
    pub fn board(&self) -> Option<(BoardTable, String)> {
        match (self.board_table, &self.experimental_board) {
            (Some(table), Some(name)) => Some((table, name.clone())),
            _ => None,
        }
    }

    pub fn max_rpm(&self) -> MaxRpm {
        MaxRpm { cpu: self.cpu_max_rpm, gpu: self.gpu_max_rpm }
    }
}

/// The whole decision, as a pure function over what was found.
pub fn decide(
    dmi: Dmi,
    driver_source: Option<&str>,
    max_rpm: MaxRpm,
    rpm_source: RpmSource,
    ec: EcProbe,
) -> Autodetected {
    let family = dmi.family();
    let mut notes = Vec::new();

    let mut board_known = false;
    let mut experimental_board = None;
    let mut board_table = None;
    let mut params_effect = ParamsEffect::DecidesReadback;

    match (&dmi.board_name, driver_source) {
        (None, _) => notes.push(msg!(
            "installer.auto.noBoardId",
            "This machine's firmware does not publish a DMI board name, so no board can be \
             added to the driver's tables."
        )),
        (Some(board), None) => notes.push(msg!(
            "installer.auto.noDriverSource",
            { "board" => board.clone() },
            "Board {board} was found, but without the driver sources there is no way to tell \
             whether the driver already lists it."
        )),
        (Some(board), Some(source)) => {
            // Membership of `hp_wmi_feature_boards` is what gives the driver
            // a fan profile at all; the thermal-profile tables only pick
            // which platform_profile path it takes. So that is the table the
            // question is asked of, and the one an unlisted board is added to.
            let listed = board_in_table(source, BoardTable::Features(BoardParams::VictusS), board)
                .unwrap_or(false);
            board_known = listed;

            if listed {
                notes.push(msg!(
                    "installer.auto.boardKnown",
                    { "board" => board.clone() },
                    "Board {board} is already in the driver's feature table, so no board patch \
                     is needed."
                ));
            } else if let Some(default_params) = params_for(family) {
                params_effect = effect_for(source, board);
                let (params, why) = choose_params(family, default_params, params_effect, &ec);

                experimental_board = Some(board.clone());
                board_table = Some(BoardTable::Features(params));
                notes.push(msg!(
                    "installer.auto.boardNew",
                    { "board" => board.clone(), "params" => params_name(params) },
                    "Board {board} is not in the driver's feature table; it will be added with \
                     the {params} parameters."
                ));
                notes.push(why);
            } else {
                notes.push(msg!(
                    "installer.auto.unknownFamily",
                    { "board" => board.clone() },
                    "Board {board} is not in the driver's tables, and this machine does not \
                     identify itself as an OMEN or a Victus, so which parameters it wants \
                     cannot be guessed. Choose them by hand below."
                ));
            }
        }
    }

    match rpm_source {
        RpmSource::Calibrated => notes.push(msg!(
            "installer.auto.rpmCalibrated",
            {
                // Either fan can be missing from a run that only spun one
                // up; a bare 0 there would read as a measured ceiling.
                "cpu" => rpm_text(max_rpm.cpu),
                "gpu" => rpm_text(max_rpm.gpu),
            },
            "Fan ceilings taken from the last calibration run: {cpu} (CPU), {gpu} (GPU)."
        )),
        RpmSource::DriverFallback => notes.push(msg!(
            "installer.auto.rpmNotCalibrated",
            "No fan calibration has been run here, so the driver keeps its own ceiling - it \
             asks the firmware for one first, and only falls back to a compiled-in number if \
             that fails. Run the fan calibration and install again to pin down measured values."
        )),
    }

    Autodetected {
        dmi,
        family,
        board_known,
        experimental_board,
        board_table,
        params_effect,
        ec,
        cpu_max_rpm: max_rpm.cpu,
        gpu_max_rpm: max_rpm.gpu,
        rpm_source,
        notes,
    }
}

/// Whether the params variant will change anything on this board.
///
/// Read out of the driver's own tables rather than assumed: a board already
/// listed for the OMEN or Victus profile path takes that path, and the
/// variant's thermal-profile half is then dead weight.
fn effect_for(source: &str, board: &str) -> ParamsEffect {
    if board_in_table(source, BoardTable::OmenThermalProfile, board).unwrap_or(false) {
        ParamsEffect::InertOmenPath
    } else if board_in_table(source, BoardTable::VictusThermalProfile, board).unwrap_or(false) {
        ParamsEffect::InertVictusPath
    } else {
        ParamsEffect::DecidesReadback
    }
}

/// The variant to use, and the sentence explaining how it was arrived at.
///
/// Three cases, and only one of them is a guess:
///
/// - **Inert.** The driver takes a thermal-profile path that ignores the
///   variant, and all four share one fan profile, so any of them behaves
///   identically. Nothing to decide, and saying so is more use than a
///   caveat about a choice that does not matter.
/// - **Measured.** The EC was read and one of the two candidate offsets is
///   holding a value the OMEN v1 path recognises. That names the variant.
/// - **Unmeasured.** The EC could not be read, or holds a profile at
///   neither offset. The variant that reads no EC byte is then correct in
///   the second case and the safe answer in the first.
fn choose_params(
    family: Family,
    default_params: BoardParams,
    effect: ParamsEffect,
    ec: &EcProbe,
) -> (BoardParams, Msg) {
    if effect != ParamsEffect::DecidesReadback {
        let path = match effect {
            ParamsEffect::InertVictusPath => "victus_thermal_profile_boards",
            _ => "omen_thermal_profile_boards",
        };
        return (
            default_params,
            msg!(
                "installer.auto.paramsInert",
                { "table" => path },
                "This choice does not affect this board. It is already in the driver's \
                 {table}, so the driver takes that thermal-profile path and never reads the \
                 variant's own EC offset - and all four variants share one fan profile, so \
                 none of them changes fan control either."
            ),
        );
    }

    // Only the OMEN variants differ by offset. The Victus S params are the
    // only ones with their profile values, and upstream marks their offset
    // unknown, so there is nothing here to measure against.
    if family != Family::Omen {
        return (
            default_params,
            msg!(
                "installer.auto.paramsVictus",
                "Victus boards have one parameter set in this driver, and upstream marks its \
                 EC offset unknown - so the thermal profile is remembered rather than read \
                 back. Nothing to measure."
            ),
        );
    }

    match ec.omen_offset_in_use() {
        Some(crate::ec::OMEN_THERMAL_OFFSET) => (
            BoardParams::OmenV1Legacy,
            msg!(
                "installer.auto.paramsMeasuredOmen",
                "Measured, not guessed: EC offset 0x95 is holding a value the OMEN thermal \
                 profile uses, so that is where this board keeps it. omen_v1_legacy is the \
                 variant that reads there."
            ),
        ),
        Some(_) => (
            BoardParams::OmenV1,
            msg!(
                "installer.auto.paramsMeasuredVictusS",
                "Measured, not guessed: EC offset 0x59 is holding a value the OMEN thermal \
                 profile uses, so that is where this board keeps it. omen_v1 is the variant \
                 that reads there."
            ),
        ),
        None => (
            BoardParams::OmenV1NoEc,
            match ec {
                EcProbe::Read { .. } => msg!(
                    "installer.auto.paramsNoOffset",
                    "The embedded controller was read and holds a thermal profile at neither \
                     offset the driver knows, so this board keeps it somewhere else. \
                     omen_v1_no_ec is the variant that reads none - which is the right answer \
                     here, not a fallback: the driver remembers the profile it set instead."
                ),
                _ => msg!(
                    "installer.auto.paramsUnmeasured",
                    "The embedded controller could not be read, so which EC offset holds the \
                     thermal profile is unknown. omen_v1_no_ec reads none of them, which is \
                     the safe half of that choice - the driver remembers the profile it set. \
                     Only the readback is affected; setting a profile goes over WMI either way."
                ),
            },
        ),
    }
}

/// The conservative board-params variant for a family.
///
/// Both variants chosen here have no usable EC thermal-profile offset, so
/// the driver falls back to remembering the profile it set rather than
/// reading a byte out of the embedded controller at an offset nobody
/// verified. They still differ in the profile values written over WMI,
/// which is exactly why the family has to be right and cannot be defaulted.
fn params_for(family: Family) -> Option<BoardParams> {
    match family {
        Family::Omen => Some(BoardParams::OmenV1NoEc),
        Family::Victus => Some(BoardParams::VictusS),
        Family::Unknown => None,
    }
}

fn params_name(params: BoardParams) -> &'static str {
    match params {
        BoardParams::VictusS => "victus_s",
        BoardParams::OmenV1 => "omen_v1",
        BoardParams::OmenV1Legacy => "omen_v1_legacy",
        BoardParams::OmenV1NoEc => "omen_v1_no_ec",
    }
}

fn dmi(name: &str) -> Option<String> {
    let value = std::fs::read_to_string(Path::new(DMI).join(name)).ok()?;
    let value = value.trim();
    // Retail boards ship these unset as literal placeholder strings.
    let placeholder = matches!(
        value.to_ascii_lowercase().as_str(),
        "" | "default string" | "to be filled by o.e.m." | "system product name" | "unknown"
    );
    (!placeholder).then(|| value.to_string())
}

/// Reads the driver's pristine source: `.orig` first, since that is what
/// the patcher itself works from and therefore the file whose tables decide
/// the outcome.
fn read_driver_source(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("hp-wmi-omen/hp-wmi.c.orig"))
        .or_else(|_| std::fs::read_to_string(dir.join("hp-wmi-omen/hp-wmi.c")))
        .ok()
}

/// The measured fan ceilings, read straight from the fan module's own
/// config file.
///
/// Read rather than asked for over IPC, and with a local struct rather than
/// a dependency on `pyren-fan`: this crate needs two numbers, not a fan
/// controller, and the daemon would otherwise be calling itself.
fn calibrated_max_rpm() -> (MaxRpm, RpmSource) {
    #[derive(Default, Deserialize)]
    #[serde(default, rename_all = "camelCase")]
    struct CalibratedCeilings {
        fan1_max_rpm: Option<i64>,
        fan2_max_rpm: Option<i64>,
    }

    let loaded = pyren_config::ConfigStore::system().load::<CalibratedCeilings>("fan");
    // fan1 is the CPU fan and fan2 the GPU fan, the order both the hwmon
    // channels and the driver's own constants use.
    let max_rpm = MaxRpm {
        cpu: loaded.value.fan1_max_rpm.and_then(positive_rpm),
        gpu: loaded.value.fan2_max_rpm.and_then(positive_rpm),
    };
    let source = if max_rpm.is_empty() {
        RpmSource::DriverFallback
    } else {
        RpmSource::Calibrated
    };
    (max_rpm, source)
}

fn rpm_text(rpm: Option<u32>) -> String {
    rpm.map(|rpm| format!("{rpm} rpm"))
        .unwrap_or_else(|| "not measured".to_string())
}

fn positive_rpm(rpm: i64) -> Option<u32> {
    (rpm > 0).then_some(rpm as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
static const char * const omen_thermal_profile_boards[] = {
	"8D2F",
};

static const struct dmi_system_id hp_wmi_feature_boards[] __initconst = {
	{
		.matches = {DMI_MATCH(DMI_BOARD_NAME, "8C99")},
		.driver_data = (void *)&victus_s_board_params,
	},
	{},
};
"#;

    fn dmi_for(board: &str, product: &str) -> Dmi {
        Dmi {
            board_name: Some(board.to_string()),
            product_name: Some(product.to_string()),
            sys_vendor: Some("HP".to_string()),
            product_family: None,
        }
    }

    fn suggest(board: &str, product: &str) -> Autodetected {
        suggest_with(board, product, EcProbe::NotProbed)
    }

    fn suggest_with(board: &str, product: &str, ec: EcProbe) -> Autodetected {
        decide(
            dmi_for(board, product),
            Some(SOURCE),
            MaxRpm::default(),
            RpmSource::DriverFallback,
            ec,
        )
    }

    #[test]
    fn a_board_the_driver_already_lists_needs_no_patch() {
        let result = suggest("8C99", "OMEN Gaming Laptop 16-am0xxx");
        assert!(result.board_known);
        assert_eq!(result.experimental_board, None);
        assert!(result.notes.iter().any(|n| n.key == "installer.auto.boardKnown"));
    }

    /// The test laptop: listed as an omen thermal-profile board, but absent
    /// from the feature table, which is the one that gives it a fan profile.
    #[test]
    fn a_board_missing_from_the_feature_table_is_offered_for_injection() {
        let result = suggest("8D2F", "OMEN Gaming Laptop 16-am0xxx");
        assert!(!result.board_known);
        assert_eq!(result.experimental_board.as_deref(), Some("8D2F"));
        assert_eq!(
            result.board_table,
            Some(BoardTable::Features(BoardParams::OmenV1NoEc))
        );
    }

    /// The test laptop's real situation. 8D2F is in
    /// `omen_thermal_profile_boards` but not in the feature table, so it
    /// needs the entry - and the variant that entry names changes nothing,
    /// because the driver takes the OMEN profile path for it either way.
    /// Saying "this is a guess, pick another if you know better" about a
    /// choice with no effect is worse than saying nothing.
    #[test]
    fn a_board_on_the_omen_profile_path_is_told_the_variant_is_inert() {
        let result = suggest("8D2F", "OMEN Gaming Laptop 16-am0xxx");
        assert_eq!(result.params_effect, ParamsEffect::InertOmenPath);
        assert!(result.notes.iter().any(|n| n.key == "installer.auto.paramsInert"));
        assert!(
            !result.notes.iter().any(|n| n.key == "installer.auto.paramsUnmeasured"),
            "no caveat about a choice that does not matter"
        );
    }

    /// Where it *does* matter, the EC decides it. A profile value sitting at
    /// 0x95 is the classic OMEN layout, which is what omen_v1_legacy reads.
    #[test]
    fn a_board_off_the_profile_path_has_its_variant_measured() {
        let ec = EcProbe::Read { victus_s: 0x00, omen: 0x31 };
        let result = suggest_with("8FFF", "OMEN Gaming Laptop 16-am0xxx", ec);
        assert_eq!(result.params_effect, ParamsEffect::DecidesReadback);
        assert_eq!(
            result.board_table,
            Some(BoardTable::Features(BoardParams::OmenV1Legacy))
        );
        assert!(result.notes.iter().any(|n| n.key == "installer.auto.paramsMeasuredOmen"));
    }

    #[test]
    fn the_other_offset_names_the_other_variant() {
        let ec = EcProbe::Read { victus_s: 0x30, omen: 0x00 };
        let result = suggest_with("8FFF", "OMEN Gaming Laptop 16-am0xxx", ec);
        assert_eq!(result.board_table, Some(BoardTable::Features(BoardParams::OmenV1)));
    }

    /// An EC holding a profile at neither offset is a real answer, not a
    /// failure: the variant that reads none of them is then correct.
    #[test]
    fn an_ec_with_no_profile_at_either_offset_settles_on_no_ec() {
        let ec = EcProbe::Read { victus_s: 0x07, omen: 0xa2 };
        let result = suggest_with("8FFF", "OMEN Gaming Laptop 16-am0xxx", ec);
        assert_eq!(result.board_table, Some(BoardTable::Features(BoardParams::OmenV1NoEc)));
        assert!(result.notes.iter().any(|n| n.key == "installer.auto.paramsNoOffset"));
    }

    /// And an unreadable one still gets the safe variant, but says it did
    /// not measure - the two must not read the same.
    #[test]
    fn an_unreadable_ec_says_so_rather_than_claiming_a_measurement() {
        let result = suggest_with("8FFF", "OMEN Gaming Laptop 16-am0xxx", EcProbe::ModuleNotLoaded);
        assert_eq!(result.board_table, Some(BoardTable::Features(BoardParams::OmenV1NoEc)));
        assert!(result.notes.iter().any(|n| n.key == "installer.auto.paramsUnmeasured"));
    }

    #[test]
    fn a_victus_gets_the_victus_parameters() {
        let result = suggest("8D2F", "Victus by HP Gaming Laptop 16-s1xxx");
        assert_eq!(
            result.board_table,
            Some(BoardTable::Features(BoardParams::VictusS))
        );
    }

    /// The families write different thermal-profile values, so a machine
    /// that names neither is a question, not a default.
    #[test]
    fn a_machine_of_neither_family_is_left_to_the_user() {
        let result = suggest("8D2F", "ThinkPad X1 Carbon");
        assert_eq!(result.family, Family::Unknown);
        assert_eq!(result.experimental_board, None);
        assert!(result.notes.iter().any(|n| n.key == "installer.auto.unknownFamily"));
    }

    #[test]
    fn without_the_driver_sources_no_board_claim_is_made() {
        let result = decide(
            dmi_for("8D2F", "OMEN Gaming Laptop 16-am0xxx"),
            None,
            MaxRpm::default(),
            RpmSource::DriverFallback,
            EcProbe::NotProbed,
        );
        assert!(!result.board_known);
        assert_eq!(result.experimental_board, None);
        assert!(result.notes.iter().any(|n| n.key == "installer.auto.noDriverSource"));
    }

    #[test]
    fn firmware_placeholders_are_not_treated_as_a_model_name() {
        let dmi = Dmi {
            board_name: Some("8D2F".into()),
            product_name: Some("Default string".into()),
            ..Dmi::default()
        };
        // Read() applies the filter; here the point is that a machine with
        // nothing usable falls through to Unknown rather than matching.
        assert_eq!(dmi.family(), Family::Unknown);
    }

    #[test]
    fn measured_ceilings_are_carried_through_as_the_installer_wants_them() {
        let result = decide(
            dmi_for("8D2F", "OMEN Gaming Laptop 16-am0xxx"),
            Some(SOURCE),
            MaxRpm { cpu: Some(6400), gpu: Some(5800) },
            RpmSource::Calibrated,
            EcProbe::NotProbed,
        );
        assert_eq!(result.max_rpm().cpu, Some(6400));
        assert_eq!(result.max_rpm().gpu, Some(5800));
        assert!(result.notes.iter().any(|n| n.key == "installer.auto.rpmCalibrated"));
    }

    #[test]
    fn an_uncalibrated_machine_says_so_and_patches_no_ceiling() {
        let result = suggest("8D2F", "OMEN Gaming Laptop 16-am0xxx");
        assert!(result.max_rpm().is_empty());
        assert!(result
            .notes
            .iter()
            .any(|n| n.key == "installer.auto.rpmNotCalibrated"));
    }

    /// The suggestion has to survive the round trip into `execute`'s
    /// arguments, which is the only reason it is computed.
    #[test]
    fn the_suggestion_converts_into_execute_arguments() {
        let result = suggest("8D2F", "OMEN Gaming Laptop 16-am0xxx");
        let (table, board) = result.board().expect("a board to inject");
        assert_eq!(board, "8D2F");
        assert_eq!(table.array_name(), "hp_wmi_feature_boards");
    }
}

/// Checked against the driver actually vendored in this repository, so a
/// board decision is verified against the tables that will really be
/// patched rather than against the snippet above.
#[cfg(test)]
mod vendored_driver_tests {
    use super::*;
    use crate::detect::REPO_DRIVER_DIR;

    fn source() -> Option<String> {
        read_driver_source(Path::new(REPO_DRIVER_DIR))
    }

    #[test]
    fn a_board_from_the_real_feature_table_is_recognised() {
        let Some(source) = source() else {
            eprintln!("skipped: vendored driver not found");
            return;
        };
        let result = decide(
            Dmi {
                board_name: Some("8C99".into()),
                product_name: Some("Victus by HP Gaming Laptop".into()),
                ..Dmi::default()
            },
            Some(&source),
            MaxRpm::default(),
            RpmSource::DriverFallback,
            EcProbe::NotProbed,
        );
        assert!(result.board_known, "8C99 is in the real feature table");
    }

    #[test]
    fn the_test_laptops_board_is_the_case_this_exists_for() {
        let Some(source) = source() else {
            eprintln!("skipped: vendored driver not found");
            return;
        };
        let result = decide(
            Dmi {
                board_name: Some("8D2F".into()),
                product_name: Some("OMEN Gaming Laptop 16-am0xxx".into()),
                ..Dmi::default()
            },
            Some(&source),
            MaxRpm::default(),
            RpmSource::DriverFallback,
            EcProbe::NotProbed,
        );
        // Listed among the omen thermal-profile boards, absent from the
        // feature table - so the stock driver comes up with no pwm1.
        assert!(!result.board_known);
        assert_eq!(result.experimental_board.as_deref(), Some("8D2F"));
    }
}
