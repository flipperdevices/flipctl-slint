//! The strip over the top of the page: the tabs, and the address with the buttons
//! beside it, laid out the way Chromium lays out its own.
//!
//! It is always there, over the page, and the highlight moves up into it when the
//! cursor is pushed past the top of a page that is at its top, so there are no soft
//! keys to learn. The highlighted item is drawn inverted, which is how every list on
//! the device shows what Ok will do.

use flipctl_app::font;
use flipctl_app::theme::PANEL_W;

use crate::view::Mark;

/// The tab row: a pixel of the strip's grey above tabs 14 tall.
pub const TAB_Y: i32 = 1;
pub const TAB_H: i32 = 14;
/// The address row, white, and the address field inside it.
pub const ADDR_Y: i32 = TAB_Y + TAB_H;
pub const ADDR_H: i32 = 16;
pub const FIELD_H: i32 = 14;
/// Under both rows, a black rule between the strip and the page, which is the
/// strip's last row.
pub const RULE_Y: i32 = ADDR_Y + ADDR_H;
/// The strip's height: the page is shown below it.
pub const HEIGHT: i32 = RULE_Y + 1;

/// Where the tabs start, the widest one may be, and the new-tab button's width.
const TABS_X: i32 = 2;
const TAB_MAX_W: i32 = 100;
const PLUS_W: i32 = 14;
/// Text inside a tab, and the room its close cross takes at the right.
const TAB_PAD_X: i32 = 5;
const CLOSE_W: i32 = 9;

/// The buttons in the address row: a 7x7 icon each, on a pitch of 12.
const ICON: usize = 7;
const BUTTONS_X: i32 = 4;
const BUTTON_PITCH: i32 = 12;
/// The field starts after the three buttons and runs to the right edge but two.
const FIELD_X: i32 = BUTTONS_X + 3 * BUTTON_PITCH;
const FIELD_PAD_X: i32 = 4;

#[rustfmt::skip]
const BACK: [&str; ICON] = [
    "...#...",
    "..#....",
    ".#.....",
    "#######",
    ".#.....",
    "..#....",
    "...#...",
];
#[rustfmt::skip]
const FORWARD: [&str; ICON] = [
    "...#...",
    "....#..",
    ".....#.",
    "#######",
    ".....#.",
    "....#..",
    "...#...",
];
#[rustfmt::skip]
const RELOAD: [&str; ICON] = [
    "..###.#",
    ".#...##",
    "#...###",
    "#......",
    "#.....#",
    ".#...#.",
    "..###..",
];

/// Something in the strip Ok can act on.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Item {
    Tab(usize),
    Close(usize),
    NewTab,
    Back,
    Forward,
    Reload,
    Address,
}

/// A tab as the strip draws it.
#[derive(Clone, PartialEq, Debug)]
pub struct TabView {
    pub x: i32,
    pub w: i32,
    pub title: String,
    pub active: bool,
    /// The cursor is over the tab itself, not its cross.
    pub hot: bool,
}

/// The whole strip, placed.
#[derive(Clone, PartialEq, Debug)]
pub struct StripView {
    pub tabs: Vec<TabView>,
    /// The field, and the address in it as much as fits.
    pub field_x: i32,
    pub field_w: i32,
    pub address: String,
    pub address_hot: bool,
    /// What goes over the grey and the white: rules between tabs, the icons and
    /// the cross, with the black box under whichever of them is hot.
    pub marks: Vec<Mark>,
}

/// What each tab says: its title, or its address while it has none.
pub struct Tab<'a> {
    pub title: &'a str,
}

/// Something the highlight can be on, and where.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Hit {
    pub item: Item,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Hit {
    pub fn centre(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

fn tab_w(n: usize) -> i32 {
    let room = PANEL_W as i32 - TABS_X - PLUS_W - 2;
    (room / n.max(1) as i32).min(TAB_MAX_W)
}

/// Everything the highlight can be on, left to right and top to bottom.
pub fn hits(tabs: usize, active: usize) -> Vec<Hit> {
    let w = tab_w(tabs);
    let mut out = Vec::new();
    for i in 0..tabs {
        let x = TABS_X + i as i32 * w;
        if i == active {
            out.push(Hit { item: Item::Tab(i), x, y: TAB_Y, w: w - CLOSE_W, h: TAB_H });
            out.push(Hit {
                item: Item::Close(i),
                x: x + w - CLOSE_W,
                y: TAB_Y,
                w: CLOSE_W,
                h: TAB_H,
            });
        } else {
            out.push(Hit { item: Item::Tab(i), x, y: TAB_Y, w, h: TAB_H });
        }
    }
    out.push(Hit {
        item: Item::NewTab,
        x: TABS_X + tabs as i32 * w,
        y: TAB_Y,
        w: PLUS_W,
        h: TAB_H,
    });
    for (i, item) in [Item::Back, Item::Forward, Item::Reload].into_iter().enumerate() {
        let x = BUTTONS_X + i as i32 * BUTTON_PITCH - 2;
        out.push(Hit { item, x, y: ADDR_Y + 1, w: ICON as i32 + 4, h: FIELD_H });
    }
    let field_w = PANEL_W as i32 - 2 - FIELD_X;
    out.push(Hit { item: Item::Address, x: FIELD_X, y: ADDR_Y + 1, w: field_w, h: FIELD_H });
    out
}

/// Where the arrows take the highlight from `from`: along its row, or to whichever
/// item of the other row is nearest it. `None` is down off the bottom row, which is
/// back onto the page below; the strip itself stays where it is.
pub fn step(tabs: usize, active: usize, from: Item, dir: (i32, i32)) -> Option<Item> {
    let hits = hits(tabs, active);
    let Some(here) = hits.iter().find(|h| h.item == from).copied() else {
        return Some(Item::Address);
    };
    let row: Vec<&Hit> = hits.iter().filter(|h| h.y == here.y).collect();
    let i = row.iter().position(|h| h.item == from).unwrap_or(0);
    match dir {
        (-1, 0) => Some(row[i.saturating_sub(1)].item),
        (1, 0) => Some(row[(i + 1).min(row.len() - 1)].item),
        (0, dy) => {
            let y = if dy < 0 { TAB_Y } else { ADDR_Y + 1 };
            if dy > 0 && here.y != TAB_Y {
                return None;
            }
            let cx = here.centre().0;
            // Never onto a cross: arriving on one from another row is one press of
            // Ok away from closing a tab nobody chose.
            hits.iter()
                .filter(|h| h.y == y && !matches!(h.item, Item::Close(_)))
                .min_by_key(|h| (h.centre().0 - cx).abs())
                .map_or(Some(from), |h| Some(h.item))
        }
        _ => Some(from),
    }
}

/// `text` cut to `room`, and never on a space: "the free .." reads as a word
/// missing, and a trailing space is measured a pixel short, so it overran.
fn cut(text: &str, room: i32) -> String {
    let fitted = font::fit(&font::ascii(text), room);
    match fitted.strip_suffix("..") {
        Some(head) if head.ends_with(' ') => format!("{}..", head.trim_end()),
        _ => fitted,
    }
}

/// An icon's lit pixels as marks, a run of them per mark.
fn icon(rows: &[&str; ICON], x: i32, y: i32, black: bool, out: &mut Vec<Mark>) {
    for (dy, row) in rows.iter().enumerate() {
        let bytes = row.as_bytes();
        let mut dx = 0;
        while dx < ICON {
            if bytes[dx] != b'#' {
                dx += 1;
                continue;
            }
            let start = dx;
            while dx < ICON && bytes[dx] == b'#' {
                dx += 1;
            }
            out.push(Mark {
                x: x + start as i32,
                y: y + dy as i32,
                w: (dx - start) as i32,
                h: 1,
                black,
            });
        }
    }
}

/// The strip, with the cursor over `hot`.
pub fn view(tabs: &[Tab<'_>], active: usize, address: &str, hot: Option<Item>) -> StripView {
    let hits = hits(tabs.len(), active);
    let w = tab_w(tabs.len());
    let mut marks = Vec::new();

    let views = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let x = TABS_X + i as i32 * w;
            let room = w - 2 * TAB_PAD_X - if i == active { CLOSE_W } else { 0 };
            TabView {
                x,
                w,
                title: cut(tab.title, room),
                active: i == active,
                hot: hot == Some(Item::Tab(i)),
            }
        })
        .collect::<Vec<_>>();

    // A rule between two tabs neither of which is the white one, as Chromium
    // separates the tabs behind the active one.
    for i in 1..tabs.len() {
        if i != active && i - 1 != active {
            let x = TABS_X + i as i32 * w;
            marks.push(Mark { x, y: TAB_Y + 3, w: 1, h: TAB_H - 6, black: true });
        }
    }

    for hit in &hits {
        let is_hot = hot == Some(hit.item);
        let (x, y) = (hit.x, hit.y);
        let lit = !is_hot;
        let glyph = match hit.item {
            Item::Close(_) => {
                Some([".......", ".......", "..#.#..", "...#...", "..#.#..", ".......", "......."])
            }
            Item::NewTab => {
                Some([".......", "...#...", "...#...", "#######", "...#...", "...#...", "......."])
            }
            Item::Back => Some(BACK),
            Item::Forward => Some(FORWARD),
            Item::Reload => Some(RELOAD),
            Item::Tab(_) | Item::Address => None,
        };
        let Some(glyph) = glyph else { continue };
        if is_hot {
            marks.push(Mark { x, y, w: hit.w, h: hit.h, black: true });
        }
        let gx = x + (hit.w - ICON as i32) / 2;
        let gy = y + (hit.h - ICON as i32) / 2;
        icon(&glyph, gx, gy, lit, &mut marks);
    }

    let field_w = PANEL_W as i32 - 2 - FIELD_X;
    StripView {
        tabs: views,
        field_x: FIELD_X,
        field_w,
        address: cut(address, field_w - 2 * FIELD_PAD_X),
        address_hot: hot == Some(Item::Address),
        marks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_fits_across_the_panel() {
        for n in 1..=6 {
            let hits = hits(n, 0);
            let last = hits.iter().filter(|h| h.y == TAB_Y).map(|h| h.x + h.w).max().unwrap();
            assert!(last <= PANEL_W as i32, "{n} tabs run to {last}");
            let field = hits.iter().find(|h| h.item == Item::Address).unwrap();
            assert_eq!(field.x + field.w, PANEL_W as i32 - 2);
        }
    }

    #[test]
    fn the_arrows_walk_the_strip_and_down_leaves_it() {
        // Three tabs, the middle one on screen, so its cross is an item of its own.
        assert_eq!(step(3, 1, Item::Address, (-1, 0)), Some(Item::Reload));
        assert_eq!(step(3, 1, Item::Back, (-1, 0)), Some(Item::Back), "no wrapping");
        assert_eq!(step(3, 1, Item::Tab(1), (1, 0)), Some(Item::Close(1)));
        assert_eq!(step(3, 1, Item::Close(1), (1, 0)), Some(Item::Tab(2)));
        assert_eq!(step(3, 1, Item::Tab(2), (1, 0)), Some(Item::NewTab));
        assert_eq!(step(3, 1, Item::NewTab, (1, 0)), Some(Item::NewTab));
        assert_eq!(step(3, 1, Item::Back, (0, -1)), Some(Item::Tab(0)));
        assert_eq!(step(3, 1, Item::Address, (0, -1)), Some(Item::Tab(1)));
        assert_eq!(step(3, 1, Item::Tab(0), (0, 1)), Some(Item::Reload), "the nearest below it");
        assert_eq!(step(3, 1, Item::Tab(0), (0, -1)), Some(Item::Tab(0)));
        assert_eq!(step(3, 1, Item::Address, (0, 1)), None, "Down off the address is the page");
        // A tab that went away leaves the highlight somewhere that exists.
        assert_eq!(step(1, 0, Item::Tab(5), (1, 0)), Some(Item::Address));
    }

    #[test]
    fn a_long_title_is_cut_to_its_tab() {
        let tabs = [Tab { title: "Wikipedia, the free encyclopedia" }, Tab { title: "x" }];
        let v = view(&tabs, 0, "https://en.wikipedia.org/", None);
        assert!(v.tabs[0].title.ends_with(".."), "{}", v.tabs[0].title);
        assert!(
            font::tw(&v.tabs[0].title) <= v.tabs[0].w - 2 * TAB_PAD_X - CLOSE_W,
            "{} is {} wide in {}",
            v.tabs[0].title,
            font::tw(&v.tabs[0].title),
            v.tabs[0].w
        );
    }
}
