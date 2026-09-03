//! Bitmap fonts.
//!
//! The three fonts are the prototype's packed 1bpp tables, converted verbatim by
//! tools/js-font-to-rust.py. They were generated from the flipctl-fonts TTFs
//! with a hard threshold, and "everything renders on pixel boundaries, no
//! antialiasing" is a stated invariant of the design, so the tables are the
//! contract rather than the TTFs.
//!
//! Advance width is per glyph, so text width is a sum and never a
//! multiplication. Codepoints outside ASCII 32..=126 render as `?`, matching the
//! prototype.

/// One glyph. `rows` is `BitmapFont::rows` long; within a row, bit
/// `cols - 1 - col` is the pixel at `col`.
pub struct Glyph {
    pub advance: u8,
    pub rows: &'static [u16],
}

pub struct BitmapFont {
    pub rows: u8,
    pub cols: u8,
    /// Codepoint of `glyphs[0]`.
    pub first: u8,
    pub glyphs: &'static [Glyph],
}

const FALLBACK: u8 = b'?';

impl BitmapFont {
    pub fn glyph(&self, c: char) -> &Glyph {
        let code = u32::from(c);
        let index = code
            .checked_sub(u32::from(self.first))
            .filter(|i| (*i as usize) < self.glyphs.len())
            .unwrap_or(u32::from(FALLBACK - self.first));
        &self.glyphs[index as usize]
    }

    /// Width of `text` in pixels.
    ///
    /// The prototype sums the advances and subtracts one, because each advance
    /// includes a trailing spacing column that the last glyph does not need.
    /// Reproduced exactly: status-bar right alignment depends on it.
    pub fn text_width(&self, text: &str) -> u16 {
        let sum: u16 = text.chars().map(|c| u16::from(self.glyph(c).advance)).sum();
        sum.saturating_sub(1)
    }

    /// Visit every set pixel of `text` drawn with its top-left at `(x, y)`.
    ///
    /// Coordinates may fall outside the panel; clipping belongs to the caller so
    /// this stays allocation-free and usable for measurement.
    pub fn for_each_pixel(&self, text: &str, x: i32, y: i32, mut plot: impl FnMut(i32, i32)) {
        let mut cx = x;
        for c in text.chars() {
            let glyph = self.glyph(c);
            for (row, bits) in glyph.rows.iter().enumerate() {
                if *bits == 0 {
                    continue;
                }
                for col in 0..self.cols {
                    if bits & (1 << (self.cols - 1 - col)) != 0 {
                        plot(cx + i32::from(col), y + row as i32);
                    }
                }
            }
            cx += i32::from(glyph.advance);
        }
    }
}

/// Width of `text` in the panel's title face, in the prototype's own units.
///
/// The face outside the menu's own rows, so this is the measurement a layout
/// almost always wants. It is here rather than beside a screen because Slint
/// cannot measure a string it is about to draw and drops a trailing space when it
/// tries, which makes measuring in Rust the rule and not one screen's workaround.
pub fn tw(text: &str) -> i32 {
    i32::from(TITLE.text_width(text))
}

/// The Latin letters with marks on them, by codepoint, as the letter underneath.
///
/// Generated from Unicode's own decompositions rather than typed out: NFD splits
/// "ä" into "a" and a combining diaeresis, so dropping the marks leaves the letter.
/// `?` is a codepoint the decomposition does not answer for, which is either not a
/// letter (0xd7 is a multiplication sign) or a letter whose Latin form is more than
/// one character and so is in `PAIRS` below.
const LATIN_1: &str = "AAAAAA?CEEEEIIII?NOOOOO??UUUUY??aaaaaa?ceeeeiiii?nooooo??uuuuy?y";
const LATIN_A: &str = "AaAaAaCcCcCcCcDd??EeEeEeEeEeGgGgGgGgHh??IiIiIiIiI???JjKk?LlLlLl????NnNnNn???OoOoOo??RrRrRrSsSsSsSsTtTt??UuUuUuUuUuUuWwYyYZzZzZz?";
/// Cyrillic, by codepoint from 0x430, as one Latin letter. The ones that need more
/// than one are `?` here and in `PAIRS` instead.
const CYRILLIC: &str = "abvgde?ziyklmnoprstufhc????y?e??ee?ge?iij??ckiu?";

/// What one letter becomes when it is more than one letter.
const PAIRS: &[(char, &str)] = &[
    ('ж', "zh"),
    ('ч', "ch"),
    ('ш', "sh"),
    ('щ', "sch"),
    ('ю', "yu"),
    ('я', "ya"),
    ('ђ', "dj"),
    ('ѕ', "dz"),
    ('џ', "dz"),
    ('љ', "lj"),
    ('њ', "nj"),
    // The two that are not sounds of their own: they change the letter before
    // them, and there is nothing to write for them.
    ('ъ', ""),
    ('ь', ""),
    ('æ', "ae"),
    ('œ', "oe"),
    ('ß', "ss"),
    ('þ', "th"),
    ('ð', "d"),
    ('ø', "o"),
    ('đ', "dj"),
    ('ħ', "h"),
    ('ł', "l"),
    ('ŧ', "t"),
    ('ŋ', "ng"),
    ('ĳ', "ij"),
    ('ı', "i"),
    ('ſ', "s"),
];

/// The Latin form of one lowercase letter, or None for anything not covered.
fn drawable(lower: char) -> Option<&'static str> {
    if let Some((_, latin)) = PAIRS.iter().find(|(c, _)| *c == lower) {
        return Some(latin);
    }
    let (table, first) = match u32::from(lower) {
        0xc0..=0xff => (LATIN_1, 0xc0),
        0x100..=0x17f => (LATIN_A, 0x100),
        0x430..=0x45f => (CYRILLIC, 0x430),
        _ => return None,
    };
    let at = u32::from(lower) as usize - first;
    let letter = table.get(at..=at)?;
    (letter != "?").then_some(letter)
}

/// Rewrite `text` so the panel's fonts can draw it.
///
/// The three fonts are printable ASCII, 32..=126: anything else draws as `?`, so a
/// Cyrillic now-playing title arrives on the panel as a row of question marks. The
/// letters are therefore transliterated rather than left to the glyph table, which
/// is the same answer the prototype reached by hand when it wrote every station
/// name in ASCII on purpose.
///
/// What is covered is the Latin letters with marks on them and the Cyrillic
/// alphabet, Serbian and Ukrainian included, which between them are what a
/// European radio station puts in a title. Anything else is still `?`, and becomes
/// one here rather than at the glyph table, so a string measured in Rust is the
/// string that gets drawn.
///
/// Case is kept as the writer meant it: a capital in front of a lowercase letter
/// starts a word, so "Кино" is "Kino", and a capital next to another is inside
/// one, so "ДДТ" is "DDT" and not "DdT".
pub fn ascii(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, c) in chars.iter().copied().enumerate() {
        if c.is_ascii() {
            // A control character has no glyph either, and a tab or a newline in a
            // title is a gap between words.
            out.push(if (' '..='~').contains(&c) { c } else { ' ' });
            continue;
        }
        let lower = c.to_lowercase().next().unwrap_or(c);
        let Some(latin) = drawable(lower) else {
            out.push('?');
            continue;
        };
        if c == lower {
            out.push_str(latin);
            continue;
        }
        let touching = |at: Option<&char>| at.copied().filter(|n| n.is_alphabetic());
        let neighbour = touching(i.checked_sub(1).and_then(|before| chars.get(before)))
            .or_else(|| touching(chars.get(i + 1)));
        if neighbour.is_some_and(char::is_uppercase) {
            out.extend(latin.chars().map(|l| l.to_ascii_uppercase()));
        } else {
            let mut letters = latin.chars();
            if let Some(first) = letters.next() {
                out.push(first.to_ascii_uppercase());
                out.extend(letters);
            }
        }
    }
    out
}

/// Cut `text` to `budget`, ending in ".." when it had to.
///
/// The prototype appends U+2026, which the panel's fonts do not have: every one
/// of them is printable ASCII and the prototype's own table substitutes "?" for
/// anything else, so a truncated SSID would read "MyNetwo?" on the device.
pub fn fit(text: &str, budget: i32) -> String {
    if budget <= 0 {
        return String::new();
    }
    if tw(text) <= budget {
        return text.to_string();
    }
    let tail = tw("..");
    let mut out = String::new();
    for c in text.chars() {
        let mut probe = out.clone();
        probe.push(c);
        if tw(&probe) + tail > budget {
            break;
        }
        out = probe;
    }
    out.push_str("..");
    out
}

include!("font/title.rs");
include!("font/row.rs");
include!("font/row_active.rs");

#[cfg(test)]
mod tests {
    use super::*;

    /// The three tables are indexed by codepoint, so their lengths are part of the
    /// arithmetic rather than a detail: one character short or one space long and
    /// every letter after the mistake is somebody else's, which turns a title into
    /// plausible nonsense instead of an obvious failure. Both happened while this
    /// was written, which is why the lengths are asserted and not just the letters.
    #[test]
    fn the_tables_are_as_long_as_the_ranges_they_index() {
        assert_eq!(LATIN_1.len(), 0xff - 0xc0 + 1, "Latin-1 Supplement");
        assert_eq!(LATIN_A.len(), 0x17f - 0x100 + 1, "Latin Extended-A");
        assert_eq!(CYRILLIC.len(), 0x45f - 0x430 + 1, "Cyrillic");
    }
}
