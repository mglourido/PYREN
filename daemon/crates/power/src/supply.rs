//! Mains/battery state, from `/sys/class/power_supply`.

use std::fs;

use serde::Serialize;

const POWER_SUPPLY: &str = "/sys/class/power_supply";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerSupplyState {
    /// `None` on machines with no battery at all (desktops), which is not
    /// the same as "on battery" and must not be treated as such.
    pub on_battery: Option<bool>,
    pub battery_percent: Option<f64>,
    pub battery_status: Option<String>,
    pub has_battery: bool,
}

impl PowerSupplyState {
    pub fn read() -> Self {
        let Ok(entries) = fs::read_dir(POWER_SUPPLY) else {
            return Self {
                on_battery: None,
                battery_percent: None,
                battery_status: None,
                has_battery: false,
            };
        };

        let mut mains_online: Option<bool> = None;
        let mut battery_percent = None;
        let mut battery_status = None;
        let mut has_battery = false;

        for entry in entries.filter_map(|e| e.ok()) {
            let dir = entry.path();
            let kind = read_trimmed(&dir, "type").unwrap_or_default();

            // Peripherals (wireless mice, keyboards, headsets) also register
            // as batteries here. `scope=Device` is what distinguishes them
            // from the machine's own battery - without this check a
            // discharging mouse looks like a laptop running on battery.
            if read_trimmed(&dir, "scope").as_deref() == Some("Device") {
                continue;
            }

            match kind.as_str() {
                "Mains" | "USB" | "USB_PD_DRP" => {
                    let online = read_trimmed(&dir, "online").as_deref() == Some("1");
                    // Any online adapter counts as on-mains.
                    mains_online = Some(mains_online.unwrap_or(false) || online);
                }
                "Battery" => {
                    has_battery = true;
                    battery_percent = battery_percent
                        .or_else(|| read_trimmed(&dir, "capacity")?.parse::<f64>().ok());
                    battery_status = battery_status.or_else(|| read_trimmed(&dir, "status"));
                }
                _ => {}
            }
        }

        // A machine with no battery is never "on battery"; one with a
        // battery but no adapter reading is judged by the battery's own
        // status, which is the only signal left.
        let on_battery = match (has_battery, mains_online) {
            (false, _) => None,
            (true, Some(online)) => Some(!online),
            (true, None) => battery_status.as_deref().map(|s| s == "Discharging"),
        };

        Self { on_battery, battery_percent, battery_status, has_battery }
    }
}

fn read_trimmed(dir: &std::path::Path, name: &str) -> Option<String> {
    let value = fs::read_to_string(dir.join(name)).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}
