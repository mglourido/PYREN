//! One colour, and the two shapes callers write it in.
//!
//! On the wire a colour is always `"#rrggbb"`, because that is what the
//! frontend's colour pickers and the source project's CLI both speak. It
//! is *accepted* as either that or `[r, g, b]`, so a script does not have
//! to build a hex string to say 255,0,0.

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };

    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// `#rgb`, `#rrggbb`, or either without the `#`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let hex = text.trim().trim_start_matches('#');
        let bytes = match hex.len() {
            // The short form is what people type; expanding each nibble is
            // what every CSS engine does with it, and #f00 meaning
            // anything other than #ff0000 would surprise everyone.
            3 => {
                let mut expanded = String::with_capacity(6);
                for c in hex.chars() {
                    expanded.push(c);
                    expanded.push(c);
                }
                expanded
            }
            6 => hex.to_string(),
            _ => {
                return Err(format!(
                    "'{text}' is not a colour: expected #rgb or #rrggbb, or [r, g, b]"
                ))
            }
        };
        let byte = |i: usize| {
            u8::from_str_radix(&bytes[i..i + 2], 16)
                .map_err(|_| format!("'{text}' is not a colour: '{}' is not hex", &bytes[i..i + 2]))
        };
        Ok(Self { r: byte(0)?, g: byte(2)?, b: byte(4)? })
    }
}

impl Serialize for Rgb {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Written {
            Hex(String),
            Triple([i64; 3]),
        }

        match Written::deserialize(deserializer)? {
            Written::Hex(text) => Rgb::parse(&text).map_err(de::Error::custom),
            Written::Triple(values) => {
                // Clamping rather than refusing: 0-255 is the only range
                // there is, and a caller that sent 300 meant "as much red
                // as there is", not "reject my whole request".
                let clamp = |v: i64| v.clamp(0, 255) as u8;
                Ok(Rgb::new(clamp(values[0]), clamp(values[1]), clamp(values[2])))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_colour_goes_out_as_hex_whatever_it_came_in_as() {
        let from_hex: Rgb = serde_json::from_value(json!("#ff9900")).unwrap();
        let from_triple: Rgb = serde_json::from_value(json!([255, 153, 0])).unwrap();

        assert_eq!(from_hex, from_triple);
        assert_eq!(serde_json::to_value(from_hex).unwrap(), json!("#ff9900"));
    }

    #[test]
    fn the_short_form_expands_the_way_css_does() {
        assert_eq!(Rgb::parse("#f00").unwrap(), Rgb::new(0xff, 0, 0));
        assert_eq!(Rgb::parse("0f0").unwrap(), Rgb::new(0, 0xff, 0));
    }

    #[test]
    fn nonsense_is_refused_with_the_text_that_was_sent() {
        assert!(Rgb::parse("#gg0000").unwrap_err().contains("#gg0000"));
        assert!(Rgb::parse("red").unwrap_err().contains("red"));
    }

    #[test]
    fn a_component_out_of_range_is_clamped_rather_than_refused() {
        let colour: Rgb = serde_json::from_value(json!([300, -4, 128])).unwrap();
        assert_eq!(colour, Rgb::new(255, 0, 128));
    }
}
