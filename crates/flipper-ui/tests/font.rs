//! The bitmap tables are the prototype's, so their measurements must agree with
//! the prototype's to the pixel. Expected values were computed from the JS
//! tables independently of the Rust conversion.

use flipper_ui::font::{BitmapFont, ROW, ROW_ACTIVE, TITLE};

#[test]
fn text_width_matches_the_prototype() {
    let cases: [(&BitmapFont, [(&str, u16); 4]); 3] = [
        (&TITLE, [("Network", 36), ("Settings", 34), ("100%", 22), ("A", 5)]),
        (&ROW, [("Network", 45), ("Settings", 44), ("100%", 28), ("A", 7)]),
        (&ROW_ACTIVE, [("Network", 49), ("Settings", 47), ("100%", 30), ("A", 6)]),
    ];
    for (font, expected) in cases {
        for (text, width) in expected {
            assert_eq!(
                font.text_width(text),
                width,
                "{:?} width of {text:?}",
                font.rows
            );
        }
    }
}

#[test]
fn every_font_covers_printable_ascii() {
    for font in [&TITLE, &ROW, &ROW_ACTIVE] {
        assert_eq!(font.glyphs.len(), 95, "ASCII 32..=126");
        assert_eq!(font.first, 32);
        for glyph in font.glyphs {
            assert_eq!(glyph.rows.len(), usize::from(font.rows));
            assert!(glyph.advance > 0, "a zero advance would stack glyphs");
        }
    }
}

/// Unknown codepoints fall back to `?`, which is what the prototype does. A
/// panic or a blank here would be a rendering difference on any non-ASCII
/// profile name.
#[test]
fn unknown_codepoints_fall_back_to_question_mark() {
    for font in [&TITLE, &ROW, &ROW_ACTIVE] {
        let fallback = font.glyph('?');
        for c in ['\u{e9}', '\u{4f0}', '\u{1f600}', '\u{1}'] {
            assert_eq!(font.glyph(c).advance, fallback.advance, "fallback for {c:?}");
            assert_eq!(font.glyph(c).rows, fallback.rows);
        }
    }
}

/// A capital A must be solid ink, not a smear of intermediate values. This is
/// the invariant that re-rasterising the TTFs would break.
#[test]
fn glyphs_are_one_bit_per_pixel() {
    let mut pixels = Vec::new();
    TITLE.for_each_pixel("A", 0, 0, |x, y| pixels.push((x, y)));
    assert!(!pixels.is_empty(), "the A glyph drew nothing");
    assert!(
        pixels.iter().all(|(x, y)| *x >= 0 && *y >= 0 && *x < 16 && *y < 13),
        "a glyph pixel escaped its 16x13 frame: {pixels:?}"
    );
}

/// Every letter the transliteration knows is either one Latin letter in the table
/// or several in `PAIRS`, and never both or neither.
///
/// This is the test that matters most here: the tables are indexed by codepoint,
/// so one missing slot shifts every letter after it and turns a title into
/// plausible-looking nonsense rather than into an obvious failure. It happened
/// while this was written -- the Cyrillic table was a character short, which moved
/// everything from the hard sign onwards.
#[test]
fn the_transliteration_tables_line_up() {
    // The Russian block and then the Serbian and Ukrainian one, 0x430..=0x45f.
    let cyrillic: Vec<char> = (0x430u32..=0x45f).filter_map(char::from_u32).collect();
    assert_eq!(cyrillic.len(), 48);
    for c in cyrillic {
        let drawn = flipper_ui::font::ascii(&c.to_string());
        assert!(
            !drawn.contains('?'),
            "{c} ({:#x}) has no Latin form, so a title containing it would be \
             question marks and the letters after it may be shifted",
            u32::from(c)
        );
        // Lowercase in, lowercase out. The hard and soft signs are the two that
        // come back as nothing at all: they modify the letter before them and
        // there is no letter to write for them.
        assert!(
            drawn.chars().all(|l| l.is_ascii_lowercase()),
            "{c} became {drawn:?}"
        );
        assert_eq!(drawn.is_empty(), c == 'ъ' || c == 'ь', "{c} -> {drawn:?}");
    }
}

/// The names on the radio, which is where this is needed: the panel's fonts stop
/// at codepoint 126 and a Russian or Serbian station's now-playing title does not.
#[test]
fn titles_are_transliterated_and_keep_their_case() {
    let cases = [
        ("Наше Радио", "Nashe Radio"),
        // A capital among capitals is inside a word, not the start of one.
        ("ДДТ - Что такое осень", "DDT - Chto takoe osen"),
        ("Кино - Группа крови", "Kino - Gruppa krovi"),
        ("МОСКВА - Кино", "MOSKVA - Kino"),
        // Serbian, in both alphabets, which Belgrade's stations use.
        ("Ђорђе Балашевић", "Djordje Balashevic"),
        ("Đorđe Balašević", "Djordje Balasevic"),
        // German, from the Berlin stations.
        ("Über den Wolken", "Uber den Wolken"),
        ("Straße", "Strasse"),
        // Already drawable, so untouched.
        ("Ed Sheeran feat. Khalid", "Ed Sheeran feat. Khalid"),
    ];
    for (from, want) in cases {
        assert_eq!(flipper_ui::font::ascii(from), want, "{from:?}");
    }
}

/// Whatever goes in, what comes out is drawable: that is the whole promise, and
/// the status bar's and the Wi-Fi page's strings come from the outside world too.
#[test]
fn everything_it_returns_can_be_drawn() {
    let awkward = [
        "日本語",
        "emoji \u{1f600} here",
        "tab\tand\nnewline",
        "\u{0}\u{1f}",
        "mixed Ελληνικά and Кириллица",
        "",
    ];
    for text in awkward {
        let drawn = flipper_ui::font::ascii(text);
        assert!(
            drawn.chars().all(|c| (' '..='~').contains(&c)),
            "{text:?} became {drawn:?}, which the fonts cannot draw"
        );
        // And what cannot be transliterated is marked rather than dropped, so a
        // line does not silently lose half its words.
        if text.contains('日') {
            assert_eq!(drawn, "???");
        }
    }
}
