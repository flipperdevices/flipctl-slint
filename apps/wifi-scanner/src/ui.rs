//! The scanner's own screens, painted as the Figma frames draw them: the network list,
//! the network card over it, and the dolphin's advice. All of it is pixels on the
//! canvas, text included, so the greys of the design can be used for text too.

use flipctl_app::font::{self, BitmapFont, HELVB08, ROW_ACTIVE, TITLE};
use flipctl_app::paint::{Corners, Surface};
use flipctl_app::pixel::Gray8;
use flipctl_app::theme::color;

use crate::sprites::{self, Sprite};

/// The list's first row, and the distance from one row to the next.
const TOP: i32 = 30;
const PITCH: i32 = 31;
/// Rows wholly visible above the soft keys; the next one fades out under them.
pub const VISIBLE: usize = 3;
/// The right edge everything on a row's right side lines up on.
const RIGHT: i32 = 241;
/// Where the fade at the foot of the list starts, and how it grows.
const FADE_TOP: i32 = 123;
const FADE_STEP: f32 = 0.0486;
/// The scrollbar's track.
const TRACK_TOP: i32 = 30;
const TRACK_BOTTOM: i32 = 128;
/// White over what is behind the card, and behind the dolphin.
pub const CARD_SCRIM: u8 = 127;
pub const ADVICE_SCRIM: u8 = 236;

/// One network as its list row shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub ssid: String,
    /// Band, channel and width.
    pub sub: String,
    /// Arcs drawn black, 0 to 3; the rest are grey.
    pub level: u8,
    pub lock: bool,
    pub pill: String,
    pub warn: bool,
    /// WPS, or the key management and cipher.
    pub note: String,
}

/// What the network card shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Card {
    pub ssid: String,
    pub lock: bool,
    pub signal: i32,
    /// Signal bars drawn black, 0 to 4.
    pub bars: u8,
    pub line: String,
    /// Label, value, and whether a warning follows the value.
    pub rows: Vec<(String, String, bool)>,
}

/// The dolphin's advice about one network.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Advice {
    pub ssid: String,
    pub heading: String,
    pub body: String,
}

fn sprite(s: &mut Surface, sp: &Sprite, x: i32, y: i32, map: impl Fn(u8) -> Option<Gray8>) {
    debug_assert_eq!(sp.px.len() as i32, sp.w * sp.h);
    for (i, v) in sp.px.iter().enumerate() {
        if let Some(c) = map(*v) {
            s.pixel(x + i as i32 % sp.w, y + i as i32 / sp.w, c);
        }
    }
}

fn plain(v: u8) -> Option<Gray8> {
    (v != 255).then_some(Gray8(v))
}

pub fn text(s: &mut Surface, f: &BitmapFont, t: &str, x: i32, y: i32, c: Gray8) {
    s.text(f, &font::ascii(t), x, y, c);
}

pub fn width(f: &BitmapFont, t: &str) -> i32 {
    i32::from(f.text_width(&font::ascii(t)))
}

/// `t` cut with ".." to fit `budget` pixels in font `f`.
pub fn fit(f: &BitmapFont, t: &str, budget: i32) -> String {
    let t = font::ascii(t);
    if width(f, &t) <= budget {
        return t;
    }
    let mut out = String::new();
    for c in t.chars() {
        let probe = format!("{out}{c}");
        if width(f, &probe) + width(f, "..") > budget {
            break;
        }
        out = probe;
    }
    out + ".."
}

/// The first row shown, so `selected` is among the wholly visible ones.
pub fn scroll_for(selected: usize, scroll: usize, total: usize) -> usize {
    let mut top = scroll.min(selected);
    if selected >= top + VISIBLE {
        top = selected + 1 - VISIBLE;
    }
    top.min(total.saturating_sub(VISIBLE))
}

/// The header: the app's icon and name, and what it is doing.
fn header(s: &mut Surface, status: &str) {
    s.rect(0, 14, 256, 15, color::APP_BAR);
    sprite(s, &sprites::HEADER_ICON, 2, 14, plain);
    text(s, &ROW_ACTIVE, "Wi-Fi", 19, 13, color::BLACK);
    let w = width(&TITLE, status);
    text(s, &TITLE, status, 124 - w / 2, 16, color::BLACK);
}

/// A pill ending at `right`, four pixels of fill around the label's ink each side.
/// Returns its left edge.
fn pill(s: &mut Surface, label: &str, right: i32, y: i32) -> i32 {
    let w = width(&TITLE, label);
    let x = right - w - 3;
    s.round_rect(x - 4, y, w + 8, 11, 2, color::SCAN_TEXT);
    text(s, &TITLE, label, x, y, color::WHITE);
    x - 4
}

fn row(s: &mut Surface, r: &Row, top: i32, selected: bool) {
    if selected {
        s.selector_frame(2, top, 245, 32, 3, Corners::default(), Some(color::WHITE), color::BLACK);
    }
    let level = r.level.min(3);
    sprite(s, &sprites::FAN, 6, top + 7, |v| match v {
        255 => None,
        4 => Some(color::BLACK),
        arc if 3 - arc < level => Some(color::BLACK),
        _ => Some(color::SCAN_WEAK),
    });
    let mut left_of = RIGHT + 1;
    if !r.pill.is_empty() {
        let right = if r.warn {
            sprite(s, &sprites::WARN, 231, top + 4, plain);
            227
        } else {
            RIGHT
        };
        left_of = pill(s, &r.pill, right, top + 4);
        if r.lock {
            left_of -= 12;
            sprite(s, &sprites::LOCK, left_of, top + 4, plain);
        }
    }
    text(s, &ROW_ACTIVE, &fit(&ROW_ACTIVE, &r.ssid, left_of - 36), 32, top + 2, color::BLACK);
    let note_w = width(&TITLE, &r.note);
    if !r.note.is_empty() {
        text(s, &TITLE, &r.note, RIGHT + 1 - note_w, top + 18, color::SCAN_TEXT);
    }
    let sub_budget = RIGHT - note_w - 40;
    text(s, &TITLE, &fit(&TITLE, &r.sub, sub_budget), 32, top + 18, color::SCAN_TEXT);
}

fn scrollbar(s: &mut Surface, first: usize, total: usize) {
    if total <= VISIBLE {
        return;
    }
    let track = TRACK_BOTTOM - TRACK_TOP;
    let thumb = (track * VISIBLE as i32 / total as i32).max(8);
    let span = total - VISIBLE;
    let at = TRACK_TOP + (track - thumb) * first as i32 / span as i32;
    let mut y = TRACK_TOP + (at - TRACK_TOP) % 2;
    while y < TRACK_BOTTOM {
        if y < at - 1 || y > at + thumb {
            s.pixel(253, y, color::BLACK);
        }
        y += 2;
    }
    s.pixel(253, at, color::BLACK);
    s.rect(252, at + 1, 3, thumb - 1, color::BLACK);
}

/// The fade at the foot of the list, under the soft keys.
fn fade(s: &mut Surface) {
    for y in FADE_TOP..144 {
        let alpha = (((y - FADE_TOP) as f32 * FADE_STEP).min(1.0) * 255.0).round() as u8;
        for x in 0..256 {
            let v = s.get(x as u16, y as u16);
            s.pixel(x, y, v.blend(color::SCAN_FADE, alpha));
        }
    }
}

/// The network list, `first` the top row shown, and `empty` in its place when there
/// is none.
pub fn list(rows: &[Row], selected: usize, first: usize, status: &str, empty: &str) -> Surface {
    let mut s = Surface::panel();
    header(&mut s, status);
    if rows.is_empty() {
        // Left-aligned on the centred width of the full text, so the dots do not shift it
        let x = 128 - width(&TITLE, empty.trim_end_matches('.')) / 2 - 4;
        text(&mut s, &TITLE, empty, x, 70, color::BLACK);
    }
    for (i, r) in rows.iter().enumerate().skip(first).take(VISIBLE + 1) {
        let top = TOP + PITCH * (i - first) as i32;
        let picked = i == selected;
        if i > first && !picked && i - 1 != selected {
            s.hline(5, top, 239, color::DIVIDER);
        }
        row(&mut s, r, top, picked);
    }
    scrollbar(&mut s, first, rows.len());
    fade(&mut s);
    s
}

/// Whatever is underneath, behind the modal scrim.
fn behind(mut s: Surface) -> Surface {
    s.scrim(color::WHITE, CARD_SCRIM);
    s
}

fn signal_bars(s: &mut Surface, x: i32, bars: u8) {
    for (i, h) in [2, 3, 5, 7].into_iter().enumerate() {
        let c = if (i as u8) < bars { color::BLACK } else { color::SCAN_WEAK };
        s.rect(x + 3 * i as i32, 37 - h, 2, h, c);
    }
}

/// The network card over `under`.
pub fn card(under: Surface, c: &Card) -> Surface {
    let mut s = behind(under);
    // The tab, its top corners cut, and the frame it opens into
    for i in 0..4 {
        let inset = 3 - i;
        s.rect(50 + inset, 7 + i, 74 - 2 * inset, 1, color::BLACK);
    }
    s.rect(50, 11, 74, 15, color::BLACK);
    s.round_rect(50, 25, 156, 98, 3, color::WHITE);
    s.round_frame(50, 25, 156, 98, 3, color::BLACK);
    for (x, y) in [(50, 25), (51, 25), (52, 25), (50, 26), (51, 26), (52, 26), (50, 27), (51, 27)] {
        s.pixel(x, y, color::BLACK);
    }
    let mut tx = 56;
    if c.lock {
        sprite(&mut s, &sprites::LOCK, 56, 11, |v| (v != 255).then_some(color::WHITE));
        tx = 67;
    }
    text(&mut s, &ROW_ACTIVE, &fit(&ROW_ACTIVE, &c.ssid, 120 - tx), tx, 8, color::WHITE);
    sprite(&mut s, &sprites::ROUTER, 61, 34, plain);
    text(&mut s, &TITLE, "Signal level:", 89, 28, color::SCAN_TEXT);
    signal_bars(&mut s, 139, c.bars);
    text(&mut s, &TITLE, &format!("{} dBm", c.signal), 153, 28, color::BLACK);
    text(&mut s, &TITLE, &fit(&TITLE, &c.line, 112), 89, 40, color::SCAN_TEXT);
    for (k, (label, value, warn)) in c.rows.iter().take(6).enumerate() {
        let top = 54 + 11 * k as i32;
        if k % 2 == 0 {
            s.rect(52, top, 152, 11, color::SCAN_STRIPE);
        }
        text(&mut s, &TITLE, label, 58, top, color::SCAN_TEXT);
        let value = fit(&TITLE, value, 100);
        text(&mut s, &TITLE, &value, 102, top, color::BLACK);
        if *warn {
            sprite(&mut s, &sprites::WARN, 102 + width(&TITLE, &value) + 5, top, plain);
        }
    }
    s
}

/// The dolphin's advice over `under`, with the arrow when more follows.
pub fn dolphin(under: Surface, a: &Advice, more: bool) -> Surface {
    let mut s = under;
    s.scrim(color::WHITE, ADVICE_SCRIM);
    sprite(&mut s, &sprites::DOLPHIN, 0, 8, plain);
    sprite(&mut s, &sprites::HELP_TAB, 50, 122, plain);
    s.round_rect(119, 24, 117, 89, 4, color::WHITE);
    for y in 25..50 {
        let inset = (28 - y).max(0);
        s.rect(120 + inset, y, 115 - 2 * inset, 1, color::APP_BAR);
    }
    s.round_frame(119, 24, 117, 89, 4, color::BLACK);
    sprite(&mut s, &sprites::ROUTER, 126, 31, plain);
    text(&mut s, &ROW_ACTIVE, &fit(&ROW_ACTIVE, &a.ssid, 82), 149, 30, color::BLACK);
    text(&mut s, &HELVB08, &fit(&HELVB08, &a.heading, 106), 125, 56, color::BLACK);
    let lines = font::wrap(&font::ascii(&a.body), 107, 4);
    for (i, line) in lines.iter().take(4).enumerate() {
        text(&mut s, &TITLE, line, 125, 68 + 10 * i as i32, color::BLACK);
    }
    if more {
        for (i, w) in [2, 3, 4, 5, 5, 5, 4, 3, 2].into_iter().enumerate() {
            s.rect(226, 100 + i as i32, w, 1, color::BLACK);
        }
    }
    s
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The networks of the Figma frames, and a few below them for the scrollbar.
    pub fn design_rows() -> Vec<Row> {
        let row = |ssid: &str, sub: &str, level, lock, pill: &str, warn, note: &str| Row {
            ssid: ssid.into(),
            sub: sub.into(),
            level,
            lock,
            pill: pill.into(),
            warn,
            note: note.into(),
        };
        let mut rows = vec![
            row("My_Home", "2.4GHz  Channel 6  20MHz", 3, true, "WEP", true, "WPS"),
            row("Cafe-WiFi", "2.4GHz Channel 1 20MHz", 2, false, "OPEN", false, ""),
            row("YourMomGuest", "5GHz Channel 120 80MHz", 2, true, "WPA3", false, "SAE AES"),
            row("Office_Guest", "5GHz Channel 36 80MHz", 1, true, "WPA3", false, "SAE AES"),
        ];
        for i in 0..4 {
            rows.push(row(
                &format!("Net{i}"),
                "2.4GHz Channel 11 20MHz",
                0,
                true,
                "WPA2",
                false,
                "PSK AES",
            ));
        }
        rows
    }

    pub fn design_card() -> Card {
        let r = |l: &str, v: &str, w| (l.to_string(), v.to_string(), w);
        Card {
            ssid: "My_Home".into(),
            lock: true,
            signal: -45,
            bars: 4,
            line: "2.4GHz  Channel 6  20MHz".into(),
            rows: vec![
                r("Security:", "WEP", true),
                r("WPS:", "Yes", false),
                r("Protocol:", "802.11g", false),
                r("BSSID:", "50:c7:bf:1f:b8:02", false),
                r("Vendor:", "TP-Link", false),
                r("Clients:", "3", false),
            ],
        }
    }

    pub fn design_advice() -> Advice {
        Advice {
            ssid: "My_Home".into(),
            heading: "Weak security!".into(),
            body: "WEP encryption is flipping ancient! Anyone can hack this network.".into(),
        }
    }

    fn save(name: &str, s: &Surface) {
        if std::env::var_os("WIFI_RENDER").is_none() {
            return;
        }
        let dir = std::path::Path::new("target/render");
        std::fs::create_dir_all(dir).unwrap();
        let px: Vec<u8> = s.pixels().iter().map(|p| p.0).collect();
        let file = std::fs::File::create(dir.join(format!("{name}.pgm"))).unwrap();
        use std::io::Write;
        let mut w = std::io::BufWriter::new(file);
        write!(w, "P5\n{} {}\n255\n", s.width(), s.height()).unwrap();
        w.write_all(&px).unwrap();
    }

    #[test]
    fn the_design_screens_paint() {
        let rows = design_rows();
        let list = list(&rows, 0, 0, "Scanning", "");
        save("design-list", &list);
        save("design-card", &card(super::list(&rows, 0, 0, "Scanning", ""), &design_card()));
        save(
            "design-dolphin",
            &dolphin(super::list(&rows, 0, 0, "Scanning", ""), &design_advice(), true),
        );
    }

    #[test]
    fn the_selected_row_stays_in_view() {
        assert_eq!(scroll_for(0, 0, 8), 0);
        assert_eq!(scroll_for(3, 0, 8), 1);
        assert_eq!(scroll_for(7, 0, 8), 5);
        assert_eq!(scroll_for(2, 5, 8), 2);
    }
}
