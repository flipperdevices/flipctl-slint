//! The boot menu, in a character grid.
//!
//! The panel's own view model, drawn with cells instead of pixels. Everything on
//! screen is a function of `boot_menu::View`, so this redraws whole rather than
//! patching, for the same reason the panel commits a frame rather than a diff:
//! there is no second copy of the state to fall out of step.
//!
//! The strings are the panel's. "Boot Menu", "Auto start in Ns...", "(no profiles)"
//! and "Booting" are what `boot.slint` writes, and a second wording for the same
//! screen would be a second thing to keep true.
//!
//! What a cell cannot express is dropped rather than approximated, as `apps/tuidemo`
//! does: no icons, no chamfered corners, no drop shadow. The pixel fields of `View`
//! (`popup_w`, `popup_body_h`, `size_slot_w`, `PopupLine::y`) are measured against
//! 16px advance tables and mean nothing here, so they are ignored and the box is
//! measured in columns instead.

use cursive::event::{Event, EventResult, Key};
use cursive::reexports::crossbeam_channel::Sender;
use cursive::view::CannotFocus;
use cursive::direction::Direction;
use cursive::style::ColorStyle;
use cursive::{Cursive, Printer, Vec2};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::boot_menu::{Row, View};
use crate::key::{FlipperKey, KeyEvent};

use super::TerminalEvent;

/// The name the snapshot callback looks the screen up by.
pub const NAME: &str = "boot";

/// Rows the chrome takes: the header, and the soft bar.
const CHROME_H: usize = 2;

/// A profile's storage, as a badge. Internal has none: it is the common case and
/// the panel does not draw one either.
fn medium_badge(medium: i32) -> &'static str {
    match medium {
        1 => "[SD]",
        2 => "[USB]",
        3 => "[SSD]",
        _ => "",
    }
}

/// Which row the list starts at, so the cursor is on screen.
///
/// Computed here rather than taken from `View::scroll`: that one is the panel's,
/// clamped to its six rows, and a terminal with twenty-two of them needs no scroll
/// at all. Same formula, this window's height.
pub fn window(selected: i32, count: usize, visible: usize) -> usize {
    if count <= visible || visible == 0 {
        return 0;
    }
    let last = count - visible;
    (selected as isize + 1 - visible as isize).clamp(0, last as isize) as usize
}

/// The header: the screen's name, and what the countdown is doing.
///
/// Padded to the full width because the fill inverts a prefix of it, and a short
/// string would invert to a stub rather than a bar.
pub fn header(view: &View, cols: usize) -> String {
    let left = "Boot Menu";
    let right = if view.countdown >= 0 {
        format!("Auto start in {}s...", view.remaining)
    } else if view.loading {
        // The same four frames the panel spins, so the two screens beat together.
        format!("Reading profiles {}", ["-", "\\", "|", "/"][view.spin_frame as usize % 4])
    } else {
        String::new()
    };
    line(left, &right, cols)
}

/// How much of the header the countdown fill has reached, in columns.
pub fn fill(view: &View, cols: usize) -> usize {
    if view.countdown < 0 {
        return 0;
    }
    cols * view.countdown.clamp(0, 100) as usize / 100
}

/// One profile, as a line: what it is on the left, when it was last used on the right.
pub fn row_line(row: &Row, selected: bool, cols: usize) -> String {
    let mut left = String::from(if selected { ">" } else { " " });
    left.push(' ');
    left.push_str(&row.label);
    // The heart the panel draws after the name, which says this is the one that
    // starts by itself.
    if row.auto {
        left.push_str(" *");
    }
    let badge = medium_badge(row.medium);
    if !badge.is_empty() {
        left.push(' ');
        left.push_str(badge);
    }
    line(&left, &row.status, cols)
}

/// `left`, then `right` against the far edge, in `cols` columns.
///
/// The right half goes when the two would meet, which is the terminal's version of
/// what `status_fits` decides in pixels: the status says least about which profile a
/// row is, and the View screen carries it in full either way.
fn line(left: &str, right: &str, cols: usize) -> String {
    let mut out = String::with_capacity(cols);
    let left = clip(left, cols);
    out.push_str(&left);
    let used = left.chars().count();
    let want = right.chars().count();
    if want > 0 && used + want + 2 <= cols {
        out.push_str(&" ".repeat(cols - used - want - 1));
        out.push_str(right);
        out.push(' ');
    } else {
        out.push_str(&" ".repeat(cols - used));
    }
    out
}

/// `s` cut to `cols` columns, with `..` where it was cut.
///
/// Two dots, not an ellipsis: the panel's fonts are printable ASCII and the serial
/// console is a vt100, so neither end of this has a character for it.
fn clip(s: &str, cols: usize) -> String {
    if s.chars().count() <= cols {
        return s.to_string();
    }
    if cols <= 2 {
        return ".".repeat(cols);
    }
    s.chars().take(cols - 2).collect::<String>() + ".."
}

/// The five soft keys, in the slots the panel puts them in: one flush left, three
/// spread across the middle, one flush right. Empty slots are drawn as nothing,
/// which is what the panel does with them too.
pub fn soft_bar(buttons: &[&str; 5], cols: usize) -> Vec<(usize, String)> {
    let labels: Vec<String> = buttons
        .iter()
        .enumerate()
        .map(|(i, b)| if b.is_empty() { String::new() } else { format!("F{} {}", i + 1, b) })
        .collect();
    let mut out = Vec::new();
    let first = labels[0].chars().count();
    let last = labels[4].chars().count();
    let spare = cols.saturating_sub(first + last);
    let step = (spare / 3).max(1);
    for (slot, label) in labels.iter().enumerate() {
        if label.is_empty() {
            continue;
        }
        let want = label.chars().count();
        let x = match slot {
            0 => 0,
            4 => cols.saturating_sub(want),
            i => first + (i - 1) * step + step.saturating_sub(want) / 2,
        };
        out.push((x.min(cols.saturating_sub(want)), label.clone()));
    }
    out
}

/// `s` centred in `cols` columns, cut to fit if it cannot be.
fn centred(s: &str, cols: usize) -> String {
    let s = clip(s, cols);
    " ".repeat((cols - s.chars().count()) / 2) + &s
}

/// One line of an open popup, before it knows how wide the box is.
///
/// Kept apart from its rendering because the box is measured from its content, as
/// the panel's is: a line padded to a width cannot then be asked how wide it wants
/// to be, which is the mistake that made the first version of this span the screen.
enum Line {
    /// Centred in the box: the size, a message, the hint under it.
    Centred(String),
    /// A name on the left and, for a settings row, a value against the right edge.
    Split { text: String, value: String, selected: bool },
}

/// The width a value slot takes: the value, and the chevrons either side.
///
/// The chevrons are drawn only on the selected line, but the slot is the same width
/// on every line, so nothing moves sideways as the cursor passes. The panel reserves
/// the same slot for the same reason, measured in pixels there.
fn slot(value: &str) -> String {
    format!("  {value}  ")
}

/// The popup's body, top to bottom: what it lists, or what it says.
fn popup_content(view: &View) -> Vec<Line> {
    let mut out = Vec::new();
    // The size the panel centres under the title: what deleting this profile alone
    // would free.
    out.push(Line::Centred(if view.size_unit.is_empty() {
        format!("Size: {}", view.size_num)
    } else {
        format!("Size: {} {}", view.size_num, view.size_unit)
    }));
    for l in &view.popup_lines {
        let mut text = l.text.clone();
        if l.heart {
            text.push_str(" *");
        }
        out.push(Line::Split {
            text,
            value: if l.kind == 2 { l.value.clone() } else { String::new() },
            selected: l.selected,
        });
    }
    for m in &view.popup_message {
        out.push(Line::Centred(m.clone()));
    }
    if !view.popup_button.is_empty() {
        out.push(Line::Centred(view.popup_button.clone()));
    }
    out
}

/// How wide the content wants to be, ignoring the border and the gutters.
fn content_width(content: &[Line]) -> usize {
    content
        .iter()
        .map(|l| match l {
            Line::Centred(s) => s.chars().count(),
            Line::Split { text, value, .. } if value.is_empty() => text.chars().count(),
            Line::Split { text, value, .. } => {
                text.chars().count() + 1 + slot(value).chars().count()
            }
        })
        .max()
        .unwrap_or(0)
}

/// The body laid out for a box whose inside is `inner` columns wide, with a gutter
/// on each side and the chevrons shown only where the cursor is.
pub fn popup_body(view: &View, inner: usize) -> Vec<(String, bool)> {
    popup_content(view)
        .iter()
        .map(|l| match l {
            Line::Centred(s) => (centred(s, inner), false),
            Line::Split { text, value, selected } => {
                let right = if value.is_empty() {
                    String::new()
                } else if *selected {
                    format!("< {value} >")
                } else {
                    slot(value)
                };
                (line(&format!(" {text}"), &right, inner), *selected)
            }
        })
        .collect()
}

/// Where the popup box sits, and how big: wide enough for what is in it, capped by
/// the terminal, and centred. The panel measures its own frame the same way, in
/// advance widths rather than columns.
pub fn popup_rect(view: &View, cols: usize, rows: usize) -> (usize, usize, usize, usize) {
    let content = popup_content(view);
    // Two borders and a gutter each side for the body; the title sits inside the top
    // border with a space on each side of it.
    let want = (content_width(&content) + 4).max(view.popup_title.chars().count() + 4);
    let w = want.min(cols);
    let h = (content.len() + 2).min(rows);
    ((cols - w) / 2, rows.saturating_sub(h) / 2, w, h)
}

/// What a run of text is, so the theme can decide how it looks.
///
/// Roles rather than colours: the palette is cursive's, the same one flipperos-installer
/// draws its serial console with, so a terminal that renders one renders the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ink {
    /// Inside a pane: the theme's view colours.
    Panel,
    /// A pane's name, in its border.
    Title,
    /// The row the cursor is on, and the part of the countdown that has run.
    Selected,
    /// The bar outside the panes, on the desktop rather than in a pane.
    Footer,
}

/// A run of text at a position, and what it is.
///
/// The whole screen is a list of these, painted in order, so a later one covers an
/// earlier one. That is what lets the countdown be the header drawn twice: once
/// plain, then its filled prefix again as the highlight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Painted {
    pub x: usize,
    pub y: usize,
    pub text: String,
    pub ink: Ink,
}

fn plain(x: usize, y: usize, text: String) -> Painted {
    Painted { x, y, text, ink: Ink::Panel }
}

fn inked(x: usize, y: usize, text: String, ink: Ink) -> Painted {
    Painted { x, y, text, ink }
}

/// The narrowest and shortest terminal the two-pane layout is worth drawing in.
/// Below either, the flat one-column layout is used instead: cramped but readable,
/// which is more use than a frame with nothing room inside it.
const TWO_PANE_MIN_COLS: usize = 60;
const TWO_PANE_MIN_ROWS: usize = 10;

/// Rows the footer takes: the countdown, and the soft keys.
const FOOTER_H: usize = 2;

/// An empty box, as one run per row. Whole rows rather than corners and edges,
/// because every run is bytes on a 1.5 Mbaud line shared with the kernel log.
fn frame(x: usize, y: usize, w: usize, h: usize, title: &str) -> Vec<Painted> {
    let mut out = Vec::new();
    if w < 2 || h < 2 {
        return out;
    }
    let inner = w - 2;
    let mut top = String::from("\u{250c}");
    top.push_str(&"\u{2500}".repeat(inner));
    top.push('\u{2510}');
    out.push(plain(x, y, top));
    if !title.is_empty() {
        let title = clip(title, inner.saturating_sub(4));
        out.push(inked(x + 2, y, format!(" {title} "), Ink::Title));
    }
    for row in 1..h - 1 {
        out.push(plain(x, y + row, format!("\u{2502}{}\u{2502}", " ".repeat(inner))));
    }
    out.push(plain(
        x,
        y + h - 1,
        format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(inner)),
    ));
    out
}

/// What the right pane says about the selected profile when no popup is open.
///
/// Everything here is already in the view: the panel keeps most of it behind the
/// View key, and a terminal has the room to simply show it.
fn summary(view: &View) -> Vec<(String, String)> {
    let row = view.rows.get(view.selected.max(0) as usize);
    // selected_size, not size_num: that one belongs to an open popup and holds the
    // last profile View measured, so on the list it reads the same beside every row.
    let size = match view.selected_size.as_ref() {
        Some((num, unit)) if unit.is_empty() => num.clone(),
        Some((num, unit)) => format!("{num} {unit}"),
        None => "press View to measure".into(),
    };
    let status = row.map(|r| r.status.clone()).unwrap_or_default();
    vec![
        ("Size".into(), size),
        (
            "Last used".into(),
            if status.is_empty() { "never".into() } else { status },
        ),
        (
            "Auto start".into(),
            match row.map(|r| r.auto) {
                Some(true) => "yes".into(),
                Some(false) => "no".into(),
                None => "-".into(),
            },
        ),
        (
            "Media".into(),
            match row.map(|r| r.medium) {
                Some(1) => "SD card".into(),
                Some(2) => "USB".into(),
                Some(3) => "SSD".into(),
                Some(_) => "internal".into(),
                None => "-".into(),
            },
        ),
    ]
}

/// The whole screen, as text.
///
/// Pure, so it can be rendered and read without a terminal: `screen()` lays these
/// out into a character grid, which is what the tests assert against and what the
/// probe example prints. The cursive view does nothing but paint the result.
pub fn render(view: &View, cols: usize, rows: usize) -> Vec<Painted> {
    if cols == 0 || rows == 0 {
        return Vec::new();
    }
    // A boot takes the screen, exactly as it takes the panel: there is nothing left
    // to choose, and a list under a handover reads as one.
    if !view.booting.is_empty() {
        let y = rows / 2;
        return vec![
            plain(0, y.saturating_sub(1), centred("Booting", cols)),
            plain(0, y, centred(&view.booting, cols)),
        ];
    }
    if cols >= TWO_PANE_MIN_COLS && rows >= TWO_PANE_MIN_ROWS {
        two_pane(view, cols, rows)
    } else {
        flat(view, cols, rows)
    }
}

/// The list on the left, what is selected on the right, the countdown and the soft
/// keys underneath. The shape flipperos-installer's own serial console has.
fn two_pane(view: &View, cols: usize, rows: usize) -> Vec<Painted> {
    let mut out = Vec::new();
    let left = (cols / 3).clamp(20, 34);
    let right_x = left;
    let right_w = cols - left;
    let pane_h = rows - FOOTER_H;
    let inner_h = pane_h - 2;

    out.extend(frame(0, 0, left, pane_h, "Profiles"));
    let title = if view.popup_open || !view.popup_title.is_empty() {
        view.popup_title.clone()
    } else {
        String::new()
    };
    out.extend(frame(right_x, 0, right_w, pane_h, &title));

    // Left: one profile a line. The status is not here, it is the right pane's
    // "Last used", so a narrow list never has to choose between name and status.
    if view.rows.is_empty() {
        if !view.loading {
            out.push(plain(1, 1, clip("(no profiles)", left - 2)));
        }
    } else {
        let first = window(view.selected, view.rows.len(), inner_h);
        for (i, row) in view.rows.iter().skip(first).take(inner_h).enumerate() {
            let selected = first + i == view.selected as usize;
            let mut text = String::from(if selected { ">" } else { " " });
            text.push(' ');
            text.push_str(&row.label);
            if row.auto {
                text.push_str(" *");
            }
            let badge = medium_badge(row.medium);
            let text = if badge.is_empty() {
                line(&text, "", left - 2)
            } else {
                line(&text, badge, left - 2)
            };
            let ink = if selected { Ink::Selected } else { Ink::Panel };
            out.push(inked(1, 1 + i, text, ink));
        }
    }

    // Right: the popup when one is open, else the facts about what is selected.
    let inner_w = right_w - 2;
    if view.popup_open {
        for (i, (text, selected)) in popup_body(view, inner_w).iter().enumerate() {
            if i >= inner_h {
                break;
            }
            let ink = if *selected { Ink::Selected } else { Ink::Panel };
            out.push(inked(right_x + 1, 1 + i, text.clone(), ink));
        }
    } else {
        for (i, (label, value)) in summary(view).iter().enumerate() {
            if i >= inner_h {
                break;
            }
            out.push(plain(
                right_x + 1,
                1 + i,
                line(&format!(" {label:<11}{value}"), "", inner_w),
            ));
        }
    }

    // The countdown, as the panel draws it: a bar that fills, with the text inside.
    let bar_y = rows - 2;
    let head = header(view, cols);
    out.push(inked(0, bar_y, head.clone(), Ink::Footer));
    let filled = fill(view, cols);
    if filled > 0 {
        out.push(inked(0, bar_y, head.chars().take(filled).collect(), Ink::Selected));
    }
    for (x, label) in soft_bar(&view.buttons, cols) {
        out.push(inked(x, rows - 1, label, Ink::Selected));
    }
    out
}

/// One column, for a terminal too small to frame.
fn flat(view: &View, cols: usize, rows: usize) -> Vec<Painted> {
    let mut out = Vec::new();
    let head = header(view, cols);
    let filled = fill(view, cols);
    out.push(plain(0, 0, head.clone()));
    if filled > 0 {
        // The bar is the header inverted as far as the countdown has run, which is
        // what the panel draws and why the text stays readable inside it.
        out.push(inked(0, 0, head.chars().take(filled).collect(), Ink::Selected));
    }

    let visible = rows.saturating_sub(CHROME_H);
    if view.rows.is_empty() {
        // Only once the read has finished. An empty list that is still being read is
        // not the same thing as a machine with nothing to boot, and the panel makes
        // the same distinction: it spins instead of saying this.
        if visible > 0 && !view.loading {
            out.push(plain(0, 1, centred("(no profiles)", cols)));
        }
    } else {
        let first = window(view.selected, view.rows.len(), visible);
        for (i, row) in view.rows.iter().skip(first).take(visible).enumerate() {
            let selected = first + i == view.selected as usize;
            let text = row_line(row, selected, cols);
            let ink = if selected { Ink::Selected } else { Ink::Panel };
            out.push(inked(0, 1 + i, text, ink));
        }
    }

    if rows >= CHROME_H {
        for (x, label) in soft_bar(&view.buttons, cols) {
            out.push(inked(x, rows - 1, label, Ink::Selected));
        }
    }

    if view.popup_open {
        out.extend(popup(view, cols, rows));
    }
    out
}

/// The popup box: its own frame, over whatever the list was showing.
fn popup(view: &View, cols: usize, rows: usize) -> Vec<Painted> {
    let (x, y, w, h) = popup_rect(view, cols, rows);
    let mut out = Vec::new();
    if w < 4 || h < 2 {
        return out;
    }
    let inner = w - 2;
    // Drawn in ASCII rather than with the terminal's line-drawing set: the panel's
    // fonts are printable ASCII and the console is a vt100, so a box character is
    // one more thing that can arrive as a question mark. There are no chamfered
    // corners here either, which is the same trade tuidemo makes.
    let top = format!("+{}+", "-".repeat(inner));
    let title = clip(&view.popup_title, inner.saturating_sub(2));
    out.push(plain(x, y, top.clone()));
    out.push(plain(x + 1, y, format!(" {title} ")));
    let body = popup_body(view, inner);
    for row in 0..h.saturating_sub(2) {
        out.push(plain(x, y + 1 + row, format!("|{}|", " ".repeat(inner))));
        let Some((text, selected)) = body.get(row) else {
            continue;
        };
        let ink = if *selected { Ink::Selected } else { Ink::Panel };
        out.push(inked(x + 1, y + 1 + row, text.clone(), ink));
    }
    out.push(plain(x, y + h - 1, top));
    out
}

/// Lay the runs out into a character grid, one string per row.
///
/// The dump the tests read and the probe prints. Effects are dropped here: what this
/// proves is that the right characters are in the right cells.
pub fn screen(view: &View, cols: usize, rows: usize) -> Vec<String> {
    let mut grid = vec![vec![' '; cols]; rows];
    for run in render(view, cols, rows) {
        if run.y >= rows {
            continue;
        }
        for (i, c) in run.text.chars().enumerate() {
            let x = run.x + i;
            if x >= cols {
                break;
            }
            grid[run.y][x] = c;
        }
    }
    grid.into_iter().map(|r| r.into_iter().collect()).collect()
}

/// The boot menu as a cursive view.
///
/// It owns no state of its own beyond the last snapshot: keys go out through the
/// channel to the loop that owns the `BootMenu`, and come back as the next snapshot.
/// The panel is driven the same way, so neither screen can disagree with the other.
pub struct BootScreen {
    view: Option<View>,
    tx: Sender<TerminalEvent>,
    /// Draw one empty frame, so the next one is sent in full.
    ///
    /// Only cells that differ from the last frame go on the wire, and a repaint that
    /// drew the same screen again would therefore send nothing at all. Cursive's own
    /// `clear()` does not help: it blanks the buffer being drawn into, which this then
    /// paints straight back over, leaving it identical to what the terminal already
    /// has. Emptying the screen for one frame is what makes the next one a difference.
    blank: AtomicBool,
}

impl BootScreen {
    pub fn new(tx: Sender<TerminalEvent>) -> Self {
        Self { view: None, tx, blank: AtomicBool::new(false) }
    }

    pub fn set(&mut self, view: View) {
        self.view = Some(view);
    }
}

impl cursive::View for BootScreen {
    fn draw(&self, printer: &Printer) {
        if self.blank.load(Ordering::Relaxed) {
            let empty = " ".repeat(printer.size.x);
            for y in 0..printer.size.y {
                printer.print((0, y), &empty);
            }
            return;
        }
        let Some(view) = self.view.as_ref() else {
            printer.print((0, 0), &clip("Starting...", printer.size.x));
            return;
        };
        for run in render(view, printer.size.x, printer.size.y) {
            let colour = match run.ink {
                Ink::Panel => ColorStyle::view(),
                Ink::Title => ColorStyle::title_primary(),
                Ink::Selected => ColorStyle::highlight(),
                Ink::Footer => ColorStyle::primary(),
            };
            printer.with_color(colour, |p| p.print((run.x, run.y), &run.text));
        }
    }

    fn required_size(&mut self, constraint: Vec2) -> Vec2 {
        constraint
    }

    fn take_focus(&mut self, _: Direction) -> Result<EventResult, CannotFocus> {
        Ok(EventResult::consumed())
    }

    fn on_event(&mut self, event: Event) -> EventResult {
        // The way out to a shell, for somebody diagnosing a boot. The loop disarms
        // whatever is loaded and execs one; init respawns the menu when it exits.
        if event == Event::CtrlChar('c') {
            let _ = self.tx.send(TerminalEvent::Quit);
            return EventResult::consumed();
        }
        // Repaint everything, the conventional key for it.
        //
        // Not a nicety on a serial console: only cells that changed are ever put on
        // the wire, so somebody who attaches after the menu started receives the
        // deltas and never the screen they belong to. This is how they get it, and
        // how a screen the kernel log has written over comes back.
        if event == Event::CtrlChar('l') {
            return EventResult::with_cb(|siv| {
                // The size first: a window that was resized told nobody, so this is
                // also where that is noticed, and a relayout wants the new size before
                // the repaint rather than a frame after it.
                super::resync_size();
                siv.call_on_name(NAME, |s: &mut BootScreen| s.blank.store(true, Ordering::Relaxed));
                let again = siv.cb_sink().clone();
                // After a pause, not straight into the sink: the event loop drains
                // every queued callback before it draws once, so clearing the flag
                // from here would undo it without a frame in between and nothing
                // would be sent. One poll interval is enough to get that frame.
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(60));
                    let _ = again.send(Box::new(|siv: &mut Cursive| {
                        siv.call_on_name(NAME, |s: &mut BootScreen| {
                            s.blank.store(false, Ordering::Relaxed)
                        });
                    }));
                });
            });
        }
        match map(&event) {
            // A terminal has no key-up, so each press is sent as both halves: the
            // menu counts a release as the end of a press, and a key that only ever
            // went down reads as held.
            Some(key) => {
                let _ = self.tx.send(TerminalEvent::Key(KeyEvent { key, down: true }));
                let _ = self.tx.send(TerminalEvent::Key(KeyEvent { key, down: false }));
                EventResult::consumed()
            }
            // Ignored rather than consumed, so line noise on the UART cannot cancel
            // the countdown.
            None => EventResult::Ignored,
        }
    }
}

/// The key behind a terminal event, or `None` for one the panel has no button for.
///
/// Both ways of naming the five soft keys: the letters flipctl's own browser view
/// sends, and the function keys somebody at a serial console reaches for. Escape is
/// the soft key it is on the hardware, slot 0, not a way out of the program.
pub fn map(event: &Event) -> Option<FlipperKey> {
    Some(match event {
        Event::Key(Key::Up) => FlipperKey::Up,
        Event::Key(Key::Down) => FlipperKey::Down,
        Event::Key(Key::Left) => FlipperKey::Left,
        Event::Key(Key::Right) => FlipperKey::Right,
        Event::Key(Key::Enter) => FlipperKey::Ok,
        Event::Key(Key::Backspace) => FlipperKey::Back,
        Event::Key(Key::Esc) | Event::Key(Key::F1) => FlipperKey::Escape,
        Event::Key(Key::F2) => FlipperKey::View,
        Event::Key(Key::F3) => FlipperKey::Power,
        Event::Key(Key::F4) => FlipperKey::Edit,
        Event::Key(Key::F5) => FlipperKey::Run,
        Event::Char(c) => match c.to_ascii_lowercase() {
            'z' => FlipperKey::Escape,
            'x' => FlipperKey::View,
            'c' => FlipperKey::Power,
            'v' => FlipperKey::Edit,
            'b' => FlipperKey::Run,
            _ => return None,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot_menu::PopupLine;

    fn row(label: &str, status: &str, auto: bool, medium: i32) -> Row {
        Row {
            label: label.into(),
            status: status.into(),
            icon: 1,
            icon_w: 10.0,
            icon_h: 8.0,
            auto,
            medium,
        }
    }

    #[test]
    fn a_row_carries_its_badges_and_its_status() {
        let line = row_line(&row("Desktop", "Running", true, 1), true, 40);
        assert_eq!(line, "> Desktop * [SD]                Running ");
        assert_eq!(line.chars().count(), 40);
    }

    #[test]
    fn an_unselected_row_keeps_the_marker_column() {
        let line = row_line(&row("Minimal", "", false, 0), false, 20);
        assert_eq!(line, "  Minimal           ");
    }

    /// The pixel screen drops a status that would meet the name. So does this one,
    /// and for the same reason: the name is the half that says which profile it is.
    #[test]
    fn a_status_that_would_meet_the_name_is_dropped() {
        let line = row_line(&row("A rather long profile name", "Used 3 hours ago", false, 0), false, 32);
        assert!(!line.contains("Used"), "{line}");
        assert_eq!(line.chars().count(), 32);
    }

    #[test]
    fn a_name_too_long_for_the_row_is_cut_with_two_dots() {
        let line = row_line(&row("An extremely long profile name", "", false, 0), false, 16);
        assert_eq!(line, "  An extremely..");
    }

    #[test]
    fn the_window_follows_the_cursor_and_stops_at_the_ends() {
        assert_eq!(window(0, 3, 6), 0, "a list shorter than the window never scrolls");
        assert_eq!(window(5, 3, 6), 0);
        assert_eq!(window(0, 10, 6), 0);
        assert_eq!(window(5, 10, 6), 0, "the last row of the first window is still the first");
        assert_eq!(window(6, 10, 6), 1);
        assert_eq!(window(9, 10, 6), 4, "the end of the list is the end of the scroll");
    }

    /// An empty list still being read is not a machine with nothing to boot, and the
    /// panel draws a spinner rather than saying so. This says which one it is in the
    /// header and keeps the body quiet.
    #[test]
    fn nothing_to_boot_is_only_said_once_the_read_has_finished() {
        let mut view = blank();
        view.loading = true;
        let dump = screen(&view, 40, 8).join("\n");
        assert!(!dump.contains("(no profiles)"), "{dump}");
        assert!(dump.contains("Reading profiles"), "{dump}");

        view.loading = false;
        assert!(screen(&view, 40, 8).join("\n").contains("(no profiles)"));
    }

    #[test]
    fn the_countdown_fills_the_header_and_says_what_it_is_doing() {
        let mut view = blank();
        view.countdown = 50;
        view.remaining = 3;
        let head = header(&view, 40);
        assert!(head.starts_with("Boot Menu"), "{head}");
        assert!(head.contains("Auto start in 3s..."), "{head}");
        assert_eq!(head.chars().count(), 40);
        assert_eq!(fill(&view, 40), 20);
    }

    #[test]
    fn a_cancelled_countdown_fills_nothing() {
        let mut view = blank();
        view.countdown = -1;
        assert_eq!(fill(&view, 40), 0);
        assert_eq!(header(&view, 12).trim_end(), "Boot Menu");
    }

    /// Slot 0 flush left, slot 4 flush right, the middle three across what is left:
    /// the arrangement SoftBar draws in pixels, and tuidemo already writes in cells.
    #[test]
    fn the_soft_bar_puts_its_labels_in_the_panels_slots() {
        let bar = soft_bar(&["Back", "View", "", "Edit", "Run"], 60);
        assert_eq!(bar[0], (0, "F1 Back".to_string()));
        assert_eq!(bar.last().unwrap(), &(54, "F5 Run".to_string()));
        assert_eq!(bar.len(), 4, "the empty slot draws nothing: {bar:?}");
        assert!(bar[1].0 >= 7 && bar[1].0 < 54, "{bar:?}");
    }

    #[test]
    fn the_boot_menus_own_two_labels_land_in_the_middle() {
        let bar = soft_bar(&["", "View", "", "Edit", ""], 60);
        assert_eq!(bar.len(), 2);
        assert!(bar[0].0 > 0 && bar[1].0 > bar[0].0, "{bar:?}");
    }

    /// The chevrons say which line Left and Right would spin, so only the selected
    /// one has them. The slot stays the same width either way, or the value would
    /// step sideways as the cursor passed it.
    #[test]
    fn a_settings_line_shows_its_chevrons_only_while_it_is_selected() {
        let mut view = blank();
        view.popup_lines = vec![PopupLine {
            kind: 2,
            y: 0.0,
            text: "Kernel".into(),
            value: "7.0.1".into(),
            selected: true,
            heart: false,
        }];
        let selected = popup_body(&view, 30);
        assert!(selected[1].0.contains("< 7.0.1 >"), "{:?}", selected[1]);
        assert!(selected[1].1, "the line reports itself selected");

        view.popup_lines[0].selected = false;
        let idle = popup_body(&view, 30);
        assert!(idle[1].0.contains("7.0.1") && !idle[1].0.contains('<'), "{:?}", idle[1]);
        assert_eq!(
            selected[1].0.chars().count(),
            idle[1].0.chars().count(),
            "the slot does not change width with the cursor"
        );
    }

    #[test]
    fn the_popup_is_wide_enough_for_what_is_in_it_and_no_wider() {
        let mut view = blank();
        view.popup_open = true;
        view.popup_title = "Desktop".into();
        view.size_num = "378.0".into();
        view.size_unit = "MiB".into();
        view.popup_lines = vec![PopupLine {
            kind: 0,
            y: 0.0,
            text: "Drive: /dev/sda (UFS)".into(),
            value: String::new(),
            selected: false,
            heart: false,
        }];
        let (x, _, w, h) = popup_rect(&view, 80, 24);
        // Its longest line is "Drive: /dev/sda (UFS)": a border and a gutter each
        // side of that, and nothing more. Measuring the padded lines instead made
        // this the width of the terminal.
        assert_eq!(w, "Drive: /dev/sda (UFS)".len() + 4);
        assert_eq!(x, (80 - w) / 2, "centred");
        assert_eq!(h, 2 + 2, "a size line, one body line, and the box");
    }

    #[test]
    fn a_popup_never_grows_past_the_terminal() {
        let mut view = blank();
        view.popup_open = true;
        view.popup_message = vec!["x".repeat(200)];
        let (x, _, w, _) = popup_rect(&view, 40, 10);
        assert_eq!(w, 40);
        assert_eq!(x, 0);
    }

    /// The two-pane layout is what a terminal with room gets, and the flat one is
    /// the fallback. The boundary matters: below it a frame leaves no room inside.
    #[test]
    fn a_terminal_with_room_gets_two_panes_and_a_small_one_does_not() {
        let mut view = blank();
        view.rows = vec![row("Desktop", "Running", true, 0)];
        view.size_num = "378.0".into();
        view.size_unit = "MiB".into();

        let wide = screen(&view, 80, 24).join("\n");
        assert!(wide.contains("\u{250c}\u{2500} Profiles"), "{wide}");
        assert!(wide.contains("Last used"), "the right pane carries the status");
        // The popup's number is not this row's, so it must not appear beside it.
        assert!(!wide.contains("378.0 MiB"), "{wide}");
        assert!(wide.contains("press View to measure"), "{wide}");

        view.selected_size = Some(("12.5".into(), "GiB".into()));
        assert!(screen(&view, 80, 24).join("\n").contains("12.5 GiB"));

        let small = screen(&view, 40, 8).join("\n");
        assert!(!small.contains('\u{250c}'), "no frame in the flat layout: {small}");
        assert!(small.contains("Running"), "the status is on the row instead");
    }

    /// The operator's own terminal was 275x19: wider than anything and shorter than
    /// the 24 rows this used to assume, so the footer has to land inside it.
    #[test]
    fn a_wide_short_terminal_keeps_its_footer_on_screen() {
        let mut view = blank();
        view.rows = vec![row("Desktop", "Running", true, 0)];
        let lines = screen(&view, 275, 19);
        assert_eq!(lines.len(), 19);
        assert!(lines[18].contains("F2 View"), "soft keys on the last row: {}", lines[18]);
        assert!(lines[17].contains("Boot Menu"), "countdown above them: {}", lines[17]);
        assert!(lines[16].starts_with('\u{2514}'), "panes close above that: {}", lines[16]);
        for l in &lines {
            assert_eq!(l.chars().count(), 275);
        }
    }

    #[test]
    fn every_key_the_panel_has_a_button_for_arrives_both_ways() {
        use cursive::event::{Event, Key};
        assert_eq!(map(&Event::Key(Key::Up)), Some(FlipperKey::Up));
        assert_eq!(map(&Event::Key(Key::Enter)), Some(FlipperKey::Ok));
        assert_eq!(map(&Event::Key(Key::Backspace)), Some(FlipperKey::Back));
        for (letter, function, key) in [
            ('z', Key::F1, FlipperKey::Escape),
            ('x', Key::F2, FlipperKey::View),
            ('c', Key::F3, FlipperKey::Power),
            ('v', Key::F4, FlipperKey::Edit),
            ('b', Key::F5, FlipperKey::Run),
        ] {
            assert_eq!(map(&Event::Char(letter)), Some(key));
            assert_eq!(map(&Event::Char(letter.to_ascii_uppercase())), Some(key));
            assert_eq!(map(&Event::Key(function)), Some(key));
        }
        assert_eq!(map(&Event::Key(Key::Esc)), Some(FlipperKey::Escape));
        assert_eq!(map(&Event::Char('q')), None, "an unmapped key is not a press");
    }

    fn blank() -> View {
        View {
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            countdown: -1,
            remaining: 0,
            loading: false,
            spin_frame: 0,
            booting: String::new(),
            going_down: false,
            popup_open: false,
            popup_title: String::new(),
            popup_icon: 0,
            popup_lines: Vec::new(),
            popup_message: Vec::new(),
            popup_button: String::new(),
            size_num: "?".into(),
            size_unit: String::new(),
            size_loading: false,
            popup_w: 0.0,
            popup_body_h: 0.0,
            size_slot_w: 0.0,
            buttons: ["", "View", "", "Edit", ""],
            selected_size: None,
        }
    }
}
