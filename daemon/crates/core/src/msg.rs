//! User-facing strings that a client may show in the user's language.
//!
//! The daemon writes every sentence a person reads, in English, and that
//! stays true: [`Msg::text`] is the wording, it is what the CLI prints, what
//! `--json` bug reports carry, what the log shows and what every test
//! asserts on. What changes is that a `Msg` travels next to that text with a
//! stable [`Msg::key`] and its interpolation [`Msg::params`], so a client
//! that ships a translation catalog - the desktop app - can render the same
//! sentence in Spanish. A client without the key falls back to `text`, so
//! nothing regresses for a consumer that does not localise.
//!
//! On the wire a `Msg` is `{ "key": "...", "params": { ... }, "text": "..." }`
//! (`params` omitted when empty). See `docs/01-ipc-protocol.md`.
//!
//! Raw operating-system error text (the tail of a failed `exec`, an
//! `io::Error`, a driver's own words) is passed through as a `param` and
//! never translated - it is not ours to translate, and hiding it would cost
//! the one detail a bug report needs.

use serde::Serialize;
use serde_json::{Map, Value};

/// A translatable, ready-to-show string. Build it with [`msg!`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Msg {
    /// Dotted catalog key, e.g. `"overclock.probe.noOffsets"`. Stable:
    /// changing it strands every translation, so reword `text` freely but
    /// only rename a key when the meaning itself changed.
    pub key: &'static str,
    /// `{name}` values interpolated into the translated string. Kept even
    /// when a client localises, because the translation needs them too.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
    /// The English sentence, already interpolated. Authoritative for any
    /// consumer that does not localise.
    pub text: String,
}

impl Msg {
    /// Prefer [`msg!`]; this is for the rare dynamic-key case.
    pub fn new(key: &'static str, params: Map<String, Value>, text: String) -> Self {
        Self { key, params, text }
    }

    /// A `Msg` with no interpolation and key and text carrying the same
    /// literal - for a plain sentence not worth a catalog entry yet.
    pub fn literal(text: impl Into<String>) -> Self {
        Self { key: "", params: Map::new(), text: text.into() }
    }

    /// Whether the English text contains `needle`. A convenience for tests,
    /// which assert on the wording the daemon still owns.
    pub fn contains(&self, needle: &str) -> bool {
        self.text.contains(needle)
    }

    /// Whether there is no text at all.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Join several messages into one line. A single message is returned
    /// untouched (key and all); two or more collapse to a `key`-less `Msg`
    /// whose `params.parts` is the list, so a client that localises can
    /// translate each part and re-join with its own separator.
    pub fn join(parts: Vec<Msg>, sep: &str) -> Option<Msg> {
        match parts.len() {
            0 => None,
            1 => parts.into_iter().next(),
            _ => {
                let text = parts.iter().map(|m| m.text.as_str()).collect::<Vec<_>>().join(sep);
                let list = Value::Array(
                    parts
                        .into_iter()
                        .map(|m| {
                            let mut o = Map::new();
                            o.insert("key".into(), Value::String(m.key.to_string()));
                            if !m.params.is_empty() {
                                o.insert("params".into(), Value::Object(m.params));
                            }
                            o.insert("text".into(), Value::String(m.text));
                            Value::Object(o)
                        })
                        .collect(),
                );
                let mut params = Map::new();
                params.insert("parts".into(), list);
                Some(Msg { key: "", params, text })
            }
        }
    }
}

impl From<Msg> for String {
    fn from(m: Msg) -> String {
        m.text
    }
}

/// A bare string becomes a key-less [`Msg`] - the same text, untranslated.
/// Lets a call site that has not been given a catalog key yet keep passing
/// a `&str` or `String` where a `Msg` is now wanted.
impl From<&str> for Msg {
    fn from(text: &str) -> Msg {
        Msg::literal(text)
    }
}

impl From<String> for Msg {
    fn from(text: String) -> Msg {
        Msg::literal(text)
    }
}

impl std::fmt::Display for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Build a [`Msg`]: a stable key, the English text (a `format!` expression),
/// and the named params the translation will interpolate.
///
/// ```ignore
/// msg!("overclock.probe.clockRange",
///      { "min": min, "max": max },
///      "Clocks can be pinned between {min} and {max} MHz, which needs root.");
/// ```
///
/// The `text` uses the same `{name}` placeholders as the catalog, filled
/// from the params - so the English and the translation read from one list
/// of values and cannot drift in which name means what.
#[macro_export]
macro_rules! msg {
    ($key:expr, { $($pname:expr => $pval:expr),* $(,)? }, $text:expr) => {{
        let mut params = ::serde_json::Map::new();
        $( params.insert($pname.to_string(), ::serde_json::json!($pval)); )*
        let text = $crate::msg::__msg_interpolate($text, &params);
        $crate::msg::Msg::new($key, params, text)
    }};
    ($key:expr, $text:expr) => {
        $crate::msg::Msg::new($key, ::serde_json::Map::new(), ($text).to_string())
    };
}

/// Replace `{name}` in `template` with `params[name]` (strings unquoted).
/// Public only so [`msg!`] can call it; not part of the API.
#[doc(hidden)]
pub fn __msg_interpolate(template: &str, params: &Map<String, Value>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open + 1..];
        match rest.find('}') {
            Some(close) => {
                let name = &rest[..close];
                match params.get(name) {
                    Some(Value::String(s)) => out.push_str(s),
                    Some(v) => out.push_str(&v.to_string()),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &rest[close + 1..];
            }
            None => {
                out.push('{');
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_interpolated_from_the_same_params_the_translation_gets() {
        let m = msg!("t.range", { "min" => 180, "max" => 3090 }, "Between {min} and {max} MHz.");
        assert_eq!(m.text, "Between 180 and 3090 MHz.");
        assert_eq!(m.key, "t.range");
        assert_eq!(m.params["min"], 180);
    }

    #[test]
    fn a_string_param_lands_without_quotes() {
        let m = msg!("t.err", { "error" => "no display" }, "failed: {error}");
        assert_eq!(m.text, "failed: no display");
    }

    #[test]
    fn an_unknown_placeholder_is_left_verbatim_rather_than_dropped() {
        let params = Map::new();
        assert_eq!(__msg_interpolate("a {missing} b", &params), "a {missing} b");
    }

    #[test]
    fn the_no_param_form_carries_key_and_text() {
        let m = msg!("t.plain", "Just a sentence.");
        assert_eq!(m.text, "Just a sentence.");
        assert_eq!(m.key, "t.plain");
        assert!(m.params.is_empty());
    }

    #[test]
    fn empty_params_are_left_off_the_wire() {
        let v = serde_json::to_value(msg!("t.plain", "hi")).unwrap();
        assert!(v.get("params").is_none());
        assert_eq!(v["key"], "t.plain");
        assert_eq!(v["text"], "hi");
    }
}
