//! Argument parsing, kept separate from doing anything so it can be tested
//! without a running daemon.
//!
//! Hand-rolled rather than pulling in a parser crate: the grammar is a
//! dozen forms, the daemon workspace has no dependencies beyond serde, and
//! the error messages matter more here than the generality would.

use std::collections::BTreeMap;

/// A parsed command line: what to run, and the options it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// e.g. `["power", "set"]`.
    pub path: Vec<String>,
    /// Everything that was not a `--flag`.
    pub positional: Vec<String>,
    /// `--name value`, or `--name` on its own (stored as `""`).
    pub options: BTreeMap<String, String>,
    pub json: bool,
}

impl Command {
    pub fn option(&self, name: &str) -> Option<&str> {
        self.options.get(name).map(String::as_str)
    }

    /// A `--flag` that takes `on`/`off`, `true`/`false`, `yes`/`no`.
    pub fn switch(&self, name: &str) -> Result<Option<bool>, String> {
        match self.option(name) {
            None => Ok(None),
            Some("on" | "true" | "yes" | "1" | "") => Ok(Some(true)),
            Some("off" | "false" | "no" | "0") => Ok(Some(false)),
            Some(other) => Err(format!("--{name} takes on or off, not '{other}'")),
        }
    }

    pub fn number(&self, name: &str) -> Result<Option<f64>, String> {
        match self.option(name) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<f64>()
                .map(Some)
                .map_err(|_| format!("--{name} takes a number, not '{raw}'")),
        }
    }
}

/// Splits arguments into a subcommand path, positionals and options.
///
/// The path is the leading run of non-`--` words, which is what makes
/// `power set eco` and `fan curve 40:20,80:100` parse the same way without
/// a grammar per command.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut command =
        Command { path: Vec::new(), positional: Vec::new(), options: BTreeMap::new(), json: false };
    let mut seen_option = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(name) = arg.strip_prefix("--") {
            if name.is_empty() {
                return Err("'--' on its own is not an option".to_string());
            }
            // `--json` is the one option documented as coming *before* the
            // command (`pyren-ctl [--json] <command>`), so it must not
            // start the options: doing so put every following word into
            // the positionals and left the path empty, which is how
            // `pyren-ctl --json hotkey learn` came back "no command given".
            if name == "json" {
                command.json = true;
                command.options.insert("json".into(), String::new());
                i += 1;
                continue;
            }
            seen_option = true;
            // `--name=value` and `--name value` are the same thing.
            if let Some((name, value)) = name.split_once('=') {
                command.options.insert(name.to_string(), value.to_string());
                i += 1;
                continue;
            }
            let value = match args.get(i + 1) {
                Some(next) if !next.starts_with("--") => {
                    i += 1;
                    next.clone()
                }
                _ => String::new(),
            };
            command.options.insert(name.to_string(), value);
            i += 1;
            continue;
        }

        if seen_option || !command.positional.is_empty() {
            command.positional.push(arg.clone());
        } else {
            command.path.push(arg.clone());
        }
        i += 1;
    }

    // The last word of a path is a positional when it is not a known
    // subcommand - `power set eco` is path `power set` and value `eco`.
    Ok(command)
}

/// A `temp:percent` curve, e.g. `40:20,60:50,80:100`.
pub fn parse_curve(spec: &str) -> Result<Vec<(f64, f64)>, String> {
    let mut points = Vec::new();
    for pair in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (temp, percent) = pair
            .split_once(':')
            .ok_or_else(|| format!("'{pair}' should look like temperature:percent, e.g. 60:50"))?;
        let temp: f64 =
            temp.trim().parse().map_err(|_| format!("'{temp}' is not a temperature"))?;
        let percent: f64 =
            percent.trim().parse().map_err(|_| format!("'{percent}' is not a percentage"))?;
        if !(0.0..=100.0).contains(&percent) {
            return Err(format!("{percent} is not a percentage between 0 and 100"));
        }
        points.push((temp, percent));
    }
    if points.is_empty() {
        return Err("a curve needs at least one temperature:percent point".to_string());
    }
    Ok(points)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Command {
        let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        parse(&args).expect("should parse")
    }

    /// `--json` is documented as coming before the command, and it used to
    /// swallow it: everything after the flag became a positional and the
    /// path came out empty, so `pyren-ctl --json hotkey learn` failed with
    /// "no command given" while the same line without the flag worked.
    #[test]
    fn json_before_the_command_does_not_swallow_it() {
        let command = parse_str("--json hotkey learn --seconds 30");
        assert!(command.json);
        assert_eq!(command.path, ["hotkey", "learn"]);
        assert_eq!(command.option("seconds"), Some("30"));
    }

    #[test]
    fn a_subcommand_path_is_the_leading_words() {
        let command = parse_str("power set eco");
        assert_eq!(command.path, ["power", "set", "eco"]);
    }

    #[test]
    fn options_take_the_following_word() {
        let command = parse_str("power tune --pl1 35 --mode eco");
        assert_eq!(command.path, ["power", "tune"]);
        assert_eq!(command.option("pl1"), Some("35"));
        assert_eq!(command.option("mode"), Some("eco"));
    }

    #[test]
    fn equals_form_means_the_same_thing() {
        assert_eq!(parse_str("power tune --pl1=35").option("pl1"), Some("35"));
    }

    /// `--turbo off` and a bare `--json` must not eat each other.
    #[test]
    fn a_valueless_option_does_not_swallow_the_next_option() {
        let command = parse_str("power tune --turbo --json");
        assert_eq!(command.switch("turbo").unwrap(), Some(true));
        assert!(command.json);
    }

    #[test]
    fn switches_accept_the_words_people_actually_type() {
        let command = parse_str("x --a on --b off --c yes --d 0");
        assert_eq!(command.switch("a").unwrap(), Some(true));
        assert_eq!(command.switch("b").unwrap(), Some(false));
        assert_eq!(command.switch("c").unwrap(), Some(true));
        assert_eq!(command.switch("d").unwrap(), Some(false));
        assert_eq!(command.switch("missing").unwrap(), None);
    }

    #[test]
    fn a_switch_given_nonsense_says_so_rather_than_guessing() {
        assert!(parse_str("x --turbo maybe").switch("turbo").is_err());
        assert!(parse_str("x --pl1 lots").number("pl1").is_err());
    }

    #[test]
    fn a_curve_parses_into_points() {
        assert_eq!(parse_curve("40:20, 60:50,80:100").unwrap(), [
            (40.0, 20.0),
            (60.0, 50.0),
            (80.0, 100.0)
        ]);
    }

    #[test]
    fn a_malformed_curve_names_the_offending_pair() {
        assert!(parse_curve("40-20").unwrap_err().contains("40-20"));
        assert!(parse_curve("40:200").is_err(), "200% is not a percentage");
        assert!(parse_curve("").is_err());
    }
}
