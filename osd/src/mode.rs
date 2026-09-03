//! The four power modes, as the widget shows them.
//!
//! The names and the one-line descriptions are the app's, copied from
//! `app/src/lib/i18n/locales/*.json`; the test at the bottom reads those
//! files and fails if they drift. The widget and the app are one product,
//! and a laptop that says "Rendimiento" in one window and "Performance" in
//! the other is two.
//!
//! Only the strings this widget actually draws are here. This is not a
//! second translation runtime, and if it ever needs to become one, the
//! answer is to read the app's catalogues at runtime rather than to grow
//! this table.

use crate::icon;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Eco,
    Balanced,
    Performance,
    Unlimited,
}

/// Which language to draw in. Two, because the app ships two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Es,
}

impl Mode {
    /// In the order the key steps through them, which is the order the app
    /// lists them in - the daemon's `PowerMode::ALL` is the same list.
    pub const ALL: [Mode; 4] = [Mode::Eco, Mode::Balanced, Mode::Performance, Mode::Unlimited];

    /// The name on the wire.
    pub fn id(self) -> &'static str {
        match self {
            Mode::Eco => "eco",
            Mode::Balanced => "balanced",
            Mode::Performance => "performance",
            Mode::Unlimited => "unlimited",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.id() == value)
    }

    pub fn icon(self) -> &'static str {
        match self {
            Mode::Eco => icon::LEAF,
            Mode::Balanced => icon::DIAMOND,
            Mode::Performance => icon::BARS,
            Mode::Unlimited => icon::BOLTBARS,
        }
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match (lang, self) {
            (Lang::En, Mode::Eco) => "Eco",
            (Lang::En, Mode::Balanced) => "Balanced",
            (Lang::En, Mode::Performance) => "Performance",
            (Lang::En, Mode::Unlimited) => "Unlimited",
            (Lang::Es, Mode::Eco) => "ECO",
            (Lang::Es, Mode::Balanced) => "Equilibrado",
            (Lang::Es, Mode::Performance) => "Rendimiento",
            (Lang::Es, Mode::Unlimited) => "Sin límites",
        }
    }

    pub fn description(self, lang: Lang) -> &'static str {
        match (lang, self) {
            (Lang::En, Mode::Eco) => "Lowest power draw and the quietest fans. Best for battery life.",
            (Lang::En, Mode::Balanced) => "The firmware decides. Good default for everyday use.",
            (Lang::En, Mode::Performance) => "Higher power limits and a more aggressive fan curve.",
            (Lang::En, Mode::Unlimited) => "Unlocks manual power limits and manual fan control.",
            (Lang::Es, Mode::Eco) => "El menor consumo y los ventiladores más silenciosos. Ideal para la batería.",
            (Lang::Es, Mode::Balanced) => "Decide el firmware. Buen valor predeterminado para el día a día.",
            (Lang::Es, Mode::Performance) => "Límites de potencia más altos y una curva de ventilador más agresiva.",
            (Lang::Es, Mode::Unlimited) => "Desbloquea los límites de potencia manuales y el control manual del ventilador.",
        }
    }
}

impl Lang {
    /// The heading above the four cards.
    pub fn title(self) -> &'static str {
        match self {
            Lang::En => "POWER MODE",
            Lang::Es => "MODO DE ALIMENTACIÓN",
        }
    }

    /// Shown when the daemon could not move the machine. The detail from
    /// the daemon goes on the line below this one; this is the part that
    /// says whose fault it is not.
    pub fn refused(self) -> &'static str {
        match self {
            Lang::En => "The mode did not change:",
            Lang::Es => "El modo no cambió:",
        }
    }

    /// Which language the user has already chosen, asked in the order that
    /// respects that choice: the app's own setting first, the desktop's
    /// locale second, English last.
    ///
    /// The app writes `mainLanguage` to `~/.config/pyren/app.json`, and a
    /// widget that ignored it would be the one window on the machine that
    /// does not follow the setting in Settings.
    pub fn detect() -> Self {
        if let Some(chosen) = Self::from_app_settings() {
            return chosen;
        }
        let locale = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LC_MESSAGES"))
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default();
        if locale.to_ascii_lowercase().starts_with("es") {
            Lang::Es
        } else {
            Lang::En
        }
    }

    fn from_app_settings() -> Option<Self> {
        let home = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.config")))?;
        let raw = std::fs::read_to_string(format!("{home}/pyren/app.json")).ok()?;
        let settings: serde_json::Value = serde_json::from_str(&raw).ok()?;
        match settings.get("mainLanguage")?.as_str()? {
            "es" => Some(Lang::Es),
            "en" => Some(Lang::En),
            // A language the app has and this widget does not. Falling
            // through to the locale is better than drawing Spanish at
            // somebody who picked German.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogue(locale: &str) -> serde_json::Value {
        let path = format!(
            "{}/../app/src/lib/i18n/locales/{locale}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let raw = std::fs::read_to_string(&path).expect("the app's catalogue must be readable");
        serde_json::from_str(&raw).expect("the app's catalogue must be valid JSON")
    }

    /// The widget's strings are the app's strings. Copied, so this is what
    /// keeps the copy honest.
    #[test]
    fn the_labels_are_the_ones_the_app_uses() {
        for (lang, locale) in [(Lang::En, "en"), (Lang::Es, "es")] {
            let catalogue = catalogue(locale);
            for mode in Mode::ALL {
                assert_eq!(
                    catalogue["performance"]["modes"][mode.id()].as_str(),
                    Some(mode.label(lang)),
                    "{locale}: the {} label has drifted from the app",
                    mode.id()
                );
                assert_eq!(
                    catalogue["performance"]["modeDesc"][mode.id()].as_str(),
                    Some(mode.description(lang)),
                    "{locale}: the {} description has drifted from the app",
                    mode.id()
                );
            }
        }
    }

    /// The daemon sends `"eco"`, not `"Eco"`, and a widget that fails to
    /// parse the mode it was told has nothing to highlight.
    #[test]
    fn every_mode_survives_the_round_trip_through_its_wire_name() {
        for mode in Mode::ALL {
            assert_eq!(Mode::parse(mode.id()), Some(mode));
        }
        assert_eq!(Mode::parse("turbo"), None);
    }

    #[test]
    fn every_mode_has_a_glyph_of_its_own() {
        for mode in Mode::ALL {
            for other in Mode::ALL {
                assert!(
                    mode == other || mode.icon() != other.icon(),
                    "{} and {} draw the same icon",
                    mode.id(),
                    other.id()
                );
            }
        }
    }
}
