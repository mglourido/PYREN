//! HP board identifiers known to work with the patched hp-wmi driver.
//!
//! Curated subset of the driver's own DMI tables, copied from
//! `omen_fan_control/_constants.py` (`SUPPORTED_BOARDS`) in the source
//! project. Like there, this list is **advisory only**: it decides whether
//! the UI shows a "your board is untested" warning, never whether anything
//! is allowed to run. A board missing from it may work perfectly.

/// `/sys/class/dmi/id/board_name` values with confirmed fan control.
pub const SUPPORTED_BOARDS: &[&str] = &[
    "84DA", "84DB", "84DC", //
    "8572", "8573", "8574", "8575", //
    "8600", "8601", "8602", "8603", "8604", "8605", "8606", "8607", "860A", //
    "8746", "8747", "8748", "8749", "874A", "8786", "8787", "8788", "878A", //
    "878B", "878C", "87B5", //
    "886B", "886C", "88C8", "88CB", "88D1", "88D2", "88F4", "88F5", "88F6", //
    "88F7", "88F8", "88FD", "88FE", "88FF", //
    "8900", "8901", "8902", "8912", "8917", "8918", "8949", "894A", "89EB", //
    "8A15", "8A25", "8A3D", "8A42", "8A43", "8A44", "8A4D", //
    "8B2F", "8BA9", "8BAA", "8BAB", "8BAD", "8BB3", "8BBE", "8BC2", "8BCA", //
    "8BCD", "8BD4", "8BD5", //
    "8C4D", "8C58", "8C76", "8C77", "8C78", "8C99", "8C9C", //
    "8D26", "8D2F", "8D41", "8D87", "8D88", "8DD6", "8E35", "8E41",
];

pub fn is_supported_board(board_name: &str) -> bool {
    let board = board_name.trim().to_ascii_uppercase();
    SUPPORTED_BOARDS.iter().any(|b| *b == board)
}
