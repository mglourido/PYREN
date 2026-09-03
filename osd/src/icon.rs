//! The four mode glyphs, drawn with cairo from the app's own SVG paths.
//!
//! The paths are copied from `app/src/lib/components/Icon.svelte`, and the
//! test at the bottom reads that file and fails if they ever stop matching.
//! Copying them is deliberate: the widget and the app draw the *same* four
//! icons, the app cannot be imported from a GTK process, and two hand-drawn
//! approximations of "the OMEN eco leaf" would drift apart on the first
//! change to either.
//!
//! The parser understands the subset those four paths use - `M`, `L`, `H`,
//! `V`, `C`, `Z` and their relative forms. It is not a general SVG
//! implementation and should not grow into one: if an icon ever needs an
//! arc, the honest fix is to draw that icon differently.

use gtk4::cairo::Context;

/// Every glyph is drawn in this coordinate space, then scaled.
pub const VIEWBOX: f64 = 24.0;

pub const LEAF: &str = "M5 19c0-8 5-13 14-13 0 9-5 13-11 13H5z M5 19c2-4 5-7 9-9";
pub const DIAMOND: &str = "M12 3 21 12l-9 9-9-9z M12 8l4 4-4 4-4-4z";
pub const BARS: &str = "M5 20V12 M12 20V5 M19 20v-9";
pub const BOLTBARS: &str = "M4 20v-7 M10 20V9 M16 20v-5 M20 3l-4 7h4l-4 6";

/// Strokes a glyph onto `cr`, scaled from the 24-unit box to `size` pixels.
pub fn draw(cr: &Context, path: &str, size: f64) {
    let scale = size / VIEWBOX;
    cr.save().ok();
    cr.scale(scale, scale);
    trace(cr, path);
    cr.stroke().ok();
    cr.restore().ok();
}

/// Walks an SVG path onto `cr` without stroking it.
///
/// Unknown commands stop the walk rather than being skipped: a glyph that
/// is half-drawn is a visible bug, and one that quietly ignores the command
/// that would have closed it is not.
pub fn trace(cr: &Context, path: &str) {
    let mut tokens = Tokens::new(path);
    let (mut x, mut y) = (0.0, 0.0);
    let (mut start_x, mut start_y) = (0.0, 0.0);
    let mut command = ' ';

    while let Some(token) = tokens.peek_command() {
        // A repeated coordinate pair means "the same command again", which
        // is how "M12 3 21 12" draws a line without naming `L`.
        if token.is_ascii_alphabetic() {
            command = tokens.take_command();
        } else if command == 'M' {
            command = 'L';
        } else if command == 'm' {
            command = 'l';
        }

        let relative = command.is_ascii_lowercase();
        match command.to_ascii_uppercase() {
            'M' => {
                let (dx, dy) = match tokens.pair() {
                    Some(pair) => pair,
                    None => break,
                };
                (x, y) = if relative { (x + dx, y + dy) } else { (dx, dy) };
                (start_x, start_y) = (x, y);
                cr.move_to(x, y);
            }
            'L' => {
                let (dx, dy) = match tokens.pair() {
                    Some(pair) => pair,
                    None => break,
                };
                (x, y) = if relative { (x + dx, y + dy) } else { (dx, dy) };
                cr.line_to(x, y);
            }
            'H' => {
                let dx = match tokens.number() {
                    Some(value) => value,
                    None => break,
                };
                x = if relative { x + dx } else { dx };
                cr.line_to(x, y);
            }
            'V' => {
                let dy = match tokens.number() {
                    Some(value) => value,
                    None => break,
                };
                y = if relative { y + dy } else { dy };
                cr.line_to(x, y);
            }
            'C' => {
                let (Some((x1, y1)), Some((x2, y2)), Some((x3, y3))) =
                    (tokens.pair(), tokens.pair(), tokens.pair())
                else {
                    break;
                };
                let (ox, oy) = if relative { (x, y) } else { (0.0, 0.0) };
                cr.curve_to(ox + x1, oy + y1, ox + x2, oy + y2, ox + x3, oy + y3);
                (x, y) = (ox + x3, oy + y3);
            }
            'Z' => {
                cr.close_path();
                (x, y) = (start_x, start_y);
            }
            _ => break,
        }
    }
}

/// A cursor over a path string: commands, numbers, and the separators SVG
/// allows between them (spaces, commas, and a minus sign that ends the
/// previous number without any separator at all).
struct Tokens<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Tokens<'a> {
    fn new(path: &'a str) -> Self {
        Self { bytes: path.as_bytes(), at: 0 }
    }

    fn skip_separators(&mut self) {
        while self.at < self.bytes.len() {
            match self.bytes[self.at] {
                b' ' | b',' | b'\t' | b'\n' | b'\r' => self.at += 1,
                _ => break,
            }
        }
    }

    /// The next meaningful byte, without consuming it.
    fn peek_command(&mut self) -> Option<char> {
        self.skip_separators();
        self.bytes.get(self.at).map(|b| *b as char)
    }

    fn take_command(&mut self) -> char {
        let command = self.bytes[self.at] as char;
        self.at += 1;
        command
    }

    fn number(&mut self) -> Option<f64> {
        self.skip_separators();
        let start = self.at;
        if self.at < self.bytes.len() && matches!(self.bytes[self.at], b'-' | b'+') {
            self.at += 1;
        }
        while self.at < self.bytes.len()
            && (self.bytes[self.at].is_ascii_digit() || self.bytes[self.at] == b'.')
        {
            self.at += 1;
        }
        if start == self.at {
            return None;
        }
        std::str::from_utf8(&self.bytes[start..self.at]).ok()?.parse().ok()
    }

    fn pair(&mut self) -> Option<(f64, f64)> {
        Some((self.number()?, self.number()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parsing is checked through cairo itself: the path walked onto a
    /// surface, and the extents cairo reports back. A path the parser gave
    /// up on halfway has smaller extents than the whole glyph.
    fn extents(path: &str) -> (f64, f64, f64, f64) {
        let surface =
            gtk4::cairo::ImageSurface::create(gtk4::cairo::Format::ARgb32, 64, 64).unwrap();
        let cr = Context::new(&surface).unwrap();
        trace(&cr, path);
        cr.path_extents().unwrap_or((0.0, 0.0, 0.0, 0.0))
    }

    #[test]
    fn every_glyph_is_walked_to_its_full_extent() {
        for (name, path) in
            [("leaf", LEAF), ("diamond", DIAMOND), ("bars", BARS), ("boltbars", BOLTBARS)]
        {
            let (x0, y0, x1, y1) = extents(path);
            assert!(
                x1 - x0 > 8.0 && y1 - y0 > 8.0,
                "{name} covered only {}x{} of a 24-unit box, so the parser stopped early",
                x1 - x0,
                y1 - y0
            );
            assert!(x0 >= 0.0 && y0 >= 0.0 && x1 <= VIEWBOX && y1 <= VIEWBOX, "{name} left the box");
        }
    }

    #[test]
    fn a_repeated_coordinate_pair_continues_the_previous_command() {
        // "M12 3 21 12" is a move followed by a line, with no `L` in it.
        let (x0, _, x1, _) = extents("M12 3 21 12");
        assert!((x1 - x0 - 9.0).abs() < 0.01, "the implicit lineto was not drawn");
    }

    #[test]
    fn an_unknown_command_stops_the_walk_rather_than_being_skipped() {
        // `A` (an arc) is not supported; what came before it still draws.
        let (_, _, x1, _) = extents("M2 2 10 2 A 5 5 0 0 1 20 2");
        assert!(x1 <= 10.01, "the arc's endpoint was drawn as though it were a line");
    }

    /// The app and the widget draw the same four icons. This reads the
    /// app's own component so the copy above cannot drift silently.
    #[test]
    fn the_glyphs_are_the_ones_the_app_draws() {
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../app/src/lib/components/Icon.svelte"
        ))
        .expect("the app's Icon.svelte must be readable from here");

        for (name, path) in
            [("leaf", LEAF), ("diamond", DIAMOND), ("bars", BARS), ("boltbars", BOLTBARS)]
        {
            let expected = format!("{name}: \"{path}\"");
            assert!(
                source.contains(&expected),
                "{name} has drifted from the app's icon; expected to find\n  {expected}"
            );
        }
    }
}
