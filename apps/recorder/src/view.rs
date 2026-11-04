//! Where everything goes: the app's state turned into positioned shapes.
//!
//! Every number here is the prototype's (js/apps/record.js and
//! voice_recorder_files.js), and every one is worked out in Rust because Slint
//! cannot measure a string or stack a page. `ui/app.slint` draws what this returns
//! and decides nothing.

use std::time::Duration;

use flipctl_app::dropdown::{self, Line, Picker};
use flipctl_app::font::{self, fit, tw};
use flipctl_app::theme::{metric, radius, PANEL_H, PANEL_W};

use crate::capture::FORMATS;
use crate::files::{self, SORTS};
use crate::{Overlay, Page, Recorder, Row, INPUTS, ROWS};

/// Inks, as `ui/app.slint`'s `Inks.of` numbers them.
pub const BLACK: i32 = 0;
pub const WHITE: i32 = 1;
pub const DIM: i32 = 2;
pub const BODY: i32 = 3;
pub const TRACK: i32 = 4;
pub const CHIP: i32 = 5;
const NONE: i32 = -1;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub radius: i32,
    pub fill: i32,
    pub stroke: bool,
    pub shadow: bool,
    pub square_right: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Label {
    pub x: i32,
    pub y: i32,
    pub text: String,
    pub bold: bool,
    pub ink: i32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sprite {
    pub x: i32,
    pub y: i32,
    /// 0 the record lamp, 1 the pause bars.
    pub which: i32,
    pub opacity: f32,
}

/// One frame's worth of screen.
#[derive(Debug, Default)]
pub struct Screen {
    pub page: i32,
    pub buttons: [&'static str; 5],
    pub rows: Vec<Line>,
    pub sel_y: Option<i32>,
    pub frames: Vec<Frame>,
    pub labels: Vec<Label>,
    pub sprites: Vec<Sprite>,
    pub picker: Option<(Picker, Label)>,
    pub files: Vec<(String, String)>,
    pub file_selected: usize,
    pub file_scroll: usize,
    pub scrim: bool,
    pub over_frames: Vec<Frame>,
    pub over_labels: Vec<Label>,
    pub over_buttons: Vec<&'static str>,
    pub dialog: Vec<String>,
}

/// The title face, for the few strings drawn in it.
fn twb(text: &str) -> i32 {
    i32::from(font::ROW_ACTIVE.text_width(text))
}

fn label(x: i32, y: i32, text: impl Into<String>, ink: i32) -> Label {
    Label { x, y, text: text.into(), bold: false, ink }
}

fn bold(x: i32, y: i32, text: impl Into<String>, ink: i32) -> Label {
    Label { x, y, text: text.into(), bold: true, ink }
}

fn rect(x: i32, y: i32, w: i32, h: i32, fill: i32) -> Frame {
    Frame { x, y, w, h, radius: 0, fill, ..Frame::default() }
}

/// A white box with a black outline, the prototype's popup body.
fn popup(x: i32, y: i32, w: i32, h: i32) -> Frame {
    Frame { x, y, w, h, radius: radius::BOX, fill: WHITE, stroke: true, ..Frame::default() }
}

/// The menu selector's outline, with its shadow.
fn selector(x: i32, y: i32, w: i32, h: i32) -> Frame {
    Frame { x, y, w, h, radius: radius::MENU, fill: NONE, stroke: true, shadow: true, ..Frame::default() }
}

/// A bar filled from the left, square on the right until it is full.
fn bar(x: i32, y: i32, w: i32, h: i32, fill: i32, full_w: i32) -> Option<Frame> {
    (w >= radius::SMALL * 2).then(|| Frame {
        x,
        y,
        w,
        h,
        radius: radius::SMALL,
        fill,
        square_right: w < full_w,
        ..Frame::default()
    })
}

// The recorder page, under the status bar and the app's own title bar.
const LEVEL_Y: i32 = metric::STATUS_BAR_H + metric::APP_BAR_H + 2;
const LEVEL_H: i32 = 7;
const LEVEL_BAR_Y: i32 = LEVEL_Y + (metric::DROP_CHIP_H - LEVEL_H) / 2;
const GAIN_Y: i32 = LEVEL_BAR_Y + LEVEL_H + 3;

// The recording panel, which takes the place of every row but the gain.
const REC_X: i32 = 3;
const REC_Y: i32 = 65;
const REC_W: i32 = PANEL_W as i32 - 6;
const REC_H: i32 = 60;
const LAMP_W: i32 = 10;
const LINE_H: i32 = 11;
const LINE_GAP: i32 = 2;

/// The top of a settings row.
pub fn row_y(index: usize) -> i32 {
    GAIN_Y + index as i32 * dropdown::PITCH
}

impl Recorder {
    /// The rows the page shows: all five, or only the gain while a take runs.
    pub fn shown_rows(&self) -> Vec<Row> {
        ROWS.iter().copied().filter(|r| self.take.is_none() || *r == Row::Gain).collect()
    }

    fn title(row: Row) -> &'static str {
        match row {
            Row::Gain => "Input gain",
            Row::Input => "Input device",
            Row::Name => "File name",
            Row::Output => "Output device",
            Row::Format => "Format",
        }
    }

    fn value(&self, row: Row) -> String {
        match row {
            Row::Gain => self.gain.map_or("-".into(), |g| format!("{g}%")),
            Row::Input if self.codec == Some(false) => "Not found".into(),
            Row::Input => INPUTS[self.input].into(),
            Row::Name => self.next.clone(),
            Row::Output => self.output_label(),
            Row::Format => FORMATS[self.format].label().into(),
        }
    }

    pub fn screen(&self, now: Duration) -> Screen {
        let mut s = match self.page {
            Page::Main => self.recorder(now),
            Page::Files => self.files_page(),
        };
        if let Some(error) = &self.error {
            s.dialog = error.clone();
        }
        s
    }

    fn recorder(&self, now: Duration) -> Screen {
        let mut s = Screen { page: 0, ..Screen::default() };
        let picking = self.picking.is_some() || self.error.is_some();
        let selected = ROWS[self.selected];
        for (i, row) in self.shown_rows().into_iter().enumerate() {
            let slider = (row == Row::Gain).then(|| self.gain.unwrap_or(0) as f32 / 100.0);
            let chevrons = row == selected && !picking && (row != Row::Input || self.codec != Some(false));
            s.rows.push(dropdown::line(row_y(i), Self::title(row), &self.value(row), slider, chevrons));
        }
        if !picking {
            s.sel_y = Some(row_y(if self.take.is_some() { 0 } else { self.selected }));
        }

        s.labels.push(label(metric::DROP_TITLE_X, LEVEL_Y, "Input level", BLACK));
        let chip_w = metric::DROP_CHIP_W;
        s.frames.push(Frame {
            x: dropdown::CHIP_X,
            y: LEVEL_BAR_Y,
            w: chip_w,
            h: LEVEL_H,
            radius: radius::SMALL,
            fill: NONE,
            stroke: true,
            ..Frame::default()
        });
        let fill = (chip_w as f32 * self.level.clamp(0.0, 1.0)).round() as i32;
        s.frames.extend(bar(dropdown::CHIP_X, LEVEL_BAR_Y, fill, LEVEL_H, BLACK, chip_w));

        if let Some(take) = &self.take {
            self.panel(&mut s, take, now);
        }

        s.buttons = match (&self.take, self.paused) {
            (None, _) => ["Close", "Files", "", "", "Start"],
            (Some(_), false) => ["Close", "Files", "", "Pause", "Stop"],
            (Some(_), true) => ["Close", "Files", "", "Resume", ""],
        };

        if let Some(highlighted) = self.picking {
            let options = self.options(selected);
            let chip_y = row_y(self.selected) + metric::DROP_TOP_PAD;
            s.picker = Some((upward(dropdown::picker(chip_y, "", &options, highlighted)), label(
                metric::DROP_TITLE_X,
                chip_y,
                Self::title(selected),
                BLACK,
            )));
        }
        s
    }

    /// The black panel a take shows while it runs.
    fn panel(&self, s: &mut Screen, take: &crate::Take, now: Duration) {
        s.frames.push(Frame {
            x: REC_X,
            y: REC_Y,
            w: REC_W,
            h: REC_H,
            radius: radius::BOX,
            fill: BLACK,
            ..Frame::default()
        });
        let name_y = REC_Y + 1;
        s.labels.push(bold(REC_X + 7, name_y, fit(&take.name, REC_W - 14), WHITE));
        s.frames.push(rect(REC_X, name_y + 17, REC_W, 1, WHITE));

        let lamp_x = REC_X + 6;
        let lamp_y = REC_Y + 27;
        let pulse = 0.5 + 0.5 * (now.as_millis() as f32 / 250.0).sin();
        s.sprites.push(Sprite {
            x: lamp_x,
            y: lamp_y,
            which: i32::from(self.paused),
            opacity: if self.paused { 1.0 } else { 0.35 + 0.65 * pulse },
        });
        s.labels.push(bold(lamp_x + LAMP_W + 4, lamp_y + LAMP_W / 2 - 7, timecode(self.elapsed), WHITE));

        let bottom = REC_Y + REC_H - 3 - LINE_H;
        let format = take.format.label();
        s.labels.push(label(REC_X + 6, bottom, format, WHITE));
        s.labels.push(label(REC_X + 6 + tw(format) + 7, bottom, files::label(&self.dir), WHITE));

        let right = REC_X + REC_W - 5;
        let rate = self.rate.unwrap_or_else(|| take.format.nominal_rate()).max(1);
        let free = self.free.map_or("-".into(), files::bytes);
        let left = self.free.map_or("-".into(), |f| files::left(f / rate));
        for (i, text) in ["Available:", free.as_str(), left.as_str()].into_iter().enumerate() {
            let y = bottom - (2 - i as i32) * (LINE_GAP + LINE_H);
            s.labels.push(label(right - tw(text), y, text, WHITE));
        }
    }

    pub fn options(&self, row: Row) -> Vec<String> {
        match row {
            Row::Input if self.codec != Some(false) => INPUTS.iter().map(|s| s.to_string()).collect(),
            Row::Format => FORMATS.iter().map(|f| f.label().to_string()).collect(),
            Row::Output => self.outputs.iter().map(|(_, l)| l.clone()).collect(),
            _ => Vec::new(),
        }
    }

    fn files_page(&self) -> Screen {
        let mut s = Screen { page: 1, ..Screen::default() };
        s.files = self.files.iter().map(|t| (t.name.clone(), files::clock(t.length))).collect();
        s.file_selected = self.file_sel;
        s.file_scroll = self.file_scroll;
        s.buttons = ["Close", "Record", "", "Edit", "Play"];
        if self.files.is_empty() {
            let msg = "(no recordings yet)";
            s.labels.push(label((PANEL_W as i32 - tw(msg)) / 2, metric::LIST_CONTAINER_Y + 18, msg, DIM));
            s.buttons = ["Close", "Record", "", "Edit", ""];
        }
        match &self.overlay {
            Overlay::None => {}
            Overlay::Edit { selected } => self.edit(&mut s, *selected),
            Overlay::Delete { name, selected } => delete(&mut s, name, *selected),
            Overlay::Player(p) => player(&mut s, p),
        }
        s
    }

    /// The Edit box: Delete, then the sort order in a chip.
    fn edit(&self, s: &mut Screen, selected: usize) {
        const PAD: i32 = 4;
        const TITLE_H: i32 = 14;
        const OPT_H: i32 = 14;
        const SIDE: i32 = 8;
        const SEL_PAD: i32 = 5;
        let chip_w = SORTS.iter().map(|o| tw(o.label())).max().unwrap_or(0) + 30;
        let titles = ["Sort"];
        let title_w = titles.iter().map(|t| tw(t)).max().unwrap_or(0);
        let w = (SIDE + title_w + 2 + chip_w + SIDE).max(twb("Edit") + SIDE * 2);
        let h = PAD + TITLE_H + crate::EDIT_ROWS as i32 * OPT_H + PAD;
        let x = (PANEL_W as i32 - w) / 2;
        let y = (PANEL_H as i32 - h) / 2;
        let top = y + PAD + TITLE_H;
        s.scrim = true;
        s.over_frames.push(popup(x, y, w, h));
        s.over_labels.push(bold(x + (w - twb("Edit")) / 2, y + PAD, "Edit", BLACK));
        s.over_labels.push(label(x + (w - tw("Delete")) / 2, top + 1, "Delete", BLACK));

        let chips = [SORTS[self.sort].label().to_string()];
        for (i, (title, value)) in titles.iter().zip(chips.iter()).enumerate() {
            let row = top + (i as i32 + 1) * OPT_H;
            s.over_labels.push(label(x + SIDE, row + 1, *title, BLACK));
            let chip_x = x + w - SIDE - chip_w;
            let chip_y = row + (OPT_H - metric::DROP_CHIP_H) / 2;
            s.over_frames.push(Frame {
                x: chip_x,
                y: chip_y,
                w: chip_w,
                h: metric::DROP_CHIP_H,
                radius: radius::MENU,
                fill: CHIP,
                ..Frame::default()
            });
            s.over_labels.push(label(chip_x + (chip_w - tw(value)) / 2, chip_y, value.clone(), BLACK));
            if selected == i + 1 {
                let pad = metric::DROP_ARROW_PAD;
                s.over_labels.push(label(chip_x + pad, chip_y, "<", BLACK));
                s.over_labels.push(label(chip_x + chip_w - tw(">") - pad, chip_y, ">", BLACK));
            }
        }
        s.over_frames.push(selector(x + SEL_PAD, top + selected as i32 * OPT_H - 1, w - SEL_PAD * 2, OPT_H + 1));
    }
}

/// A picker that would run off the bottom of the panel opens upward instead, its
/// last option on the row's chip, as the prototype's Format picker does.
fn upward(mut p: Picker) -> Picker {
    let room = PANEL_H as i32 - 2;
    if p.y + p.h <= room {
        return p;
    }
    let shift = p.y + metric::DROP_CHIP_H - p.h - p.y;
    p.y += shift;
    p.sel_y += shift;
    for item in &mut p.items {
        item.y += shift;
    }
    p
}

/// The Delete box: the name, and Cancel over Delete with Cancel chosen first.
fn delete(s: &mut Screen, name: &str, selected: usize) {
    const SIDE: i32 = 15;
    const ROW_H: i32 = 14;
    const PAD: i32 = 6;
    let name = fit(name, PANEL_W as i32 - SIDE * 2 - 16);
    let w = (twb("Delete").max(tw(&name)) + SIDE * 2).max(150);
    let h = PAD + ROW_H * 2 + ROW_H + 2 + ROW_H + PAD;
    let x = (PANEL_W as i32 - w) / 2;
    let y = (PANEL_H as i32 - h) / 2;
    s.scrim = true;
    s.over_frames.push(popup(x, y, w, h));
    s.over_labels.push(bold(x + (w - twb("Delete")) / 2, y + PAD, "Delete", BLACK));
    let name_y = y + PAD + ROW_H;
    s.over_labels.push(label(x + (w - tw(&name)) / 2, name_y + 2, name, BODY));
    let btn_w = w - 16;
    let btn_x = x + (w - btn_w) / 2;
    for (i, text) in ["Cancel", "Delete"].into_iter().enumerate() {
        let by = name_y + ROW_H + i as i32 * (ROW_H + 2);
        s.over_labels.push(label(btn_x + (btn_w - tw(text)) / 2, by + (ROW_H - LINE_H) / 2, text, BLACK));
        if i == selected {
            s.over_frames.push(selector(btn_x, by, btn_w, ROW_H));
        }
    }
}

/// The player, over the list.
fn player(s: &mut Screen, p: &crate::Playback) {
    const X: i32 = 4;
    const Y: i32 = 41;
    const W: i32 = 228;
    const H: i32 = 48;
    const PROG_PAD: i32 = 6;
    const PROG_H: i32 = 7;
    s.scrim = true;
    s.over_buttons = vec!["Close", "", "", "", ""];
    s.over_frames.push(popup(X, Y, W, H));
    s.over_labels.push(bold(X + 7, Y + 2, fit(&p.name, W - 14), BLACK));

    let prog_x = X + PROG_PAD;
    let prog_w = W - PROG_PAD * 2;
    let prog_y = Y + (H - PROG_H) / 2;
    s.over_frames.push(Frame {
        x: prog_x,
        y: prog_y,
        w: prog_w,
        h: PROG_H,
        radius: radius::SMALL,
        fill: TRACK,
        ..Frame::default()
    });
    let pct = match p.length {
        Some(l) if !l.is_zero() => (p.position.as_secs_f32() / l.as_secs_f32()).clamp(0.0, 1.0),
        _ => 0.0,
    };
    s.over_frames.extend(bar(prog_x, prog_y, (prog_w as f32 * pct).round() as i32, PROG_H, BLACK, prog_w));

    let bot = Y + H - 16;
    s.over_labels.push(label(X + 6, bot, mmss(p.position), BLACK));
    let total = mmss(p.length.unwrap_or_default());
    s.over_labels.push(label(X + W - 6 - tw(&total), bot, total, BLACK));

    const ICON_W: i32 = 7;
    const GAP: i32 = 7;
    let (lw, rw) = (tw("< -60s"), tw("+60s >"));
    let mid_x = X + (W - (lw + GAP + ICON_W + GAP + rw)) / 2;
    match p.flash {
        Some((-1, step)) => s.over_labels.push(label(mid_x, bot, format!("< -{step}s"), BLACK)),
        _ => s.over_labels.push(label(mid_x, bot, "<", DIM)),
    }
    let icon_x = mid_x + lw + GAP;
    if p.playing {
        s.over_frames.push(rect(icon_x, bot + 1, 2, 8, BLACK));
        s.over_frames.push(rect(icon_x + 3, bot + 1, 2, 8, BLACK));
    } else {
        for r in 0..8 {
            let w = 4 - (r - 3i32).abs() + 1;
            s.over_frames.push(rect(icon_x, bot + 1 + r, w, 1, BLACK));
        }
    }
    let right = icon_x + ICON_W + GAP;
    match p.flash {
        Some((1, step)) => s.over_labels.push(label(right, bot, format!("+{step}s >"), BLACK)),
        _ => s.over_labels.push(label(right + rw - tw(">"), bot, ">", DIM)),
    }

    const VOL_W: i32 = 16;
    let vol_x = X + W + (PANEL_W as i32 - (X + W) - VOL_W) / 2;
    s.over_frames.push(popup(vol_x, Y, VOL_W, H));
    let inner = H - 4;
    let fill_h = (inner as f32 * p.volume.clamp(0, 100) as f32 / 100.0).round() as i32;
    if fill_h >= radius::SMALL * 2 {
        s.over_frames.push(Frame {
            x: vol_x + 2,
            y: Y + H - 2 - fill_h,
            w: VOL_W - 4,
            h: fill_h,
            radius: radius::SMALL,
            fill: BLACK,
            ..Frame::default()
        });
    }
}

/// `HH:MM:SS:FF`, hundredths last, as the recording panel counts.
pub fn timecode(t: Duration) -> String {
    let ms = t.as_millis() as u64;
    let hh = (ms / 3_600_000).min(99);
    format!("{hh:02}:{:02}:{:02}:{:02}", ms / 60_000 % 60, ms / 1000 % 60, ms / 10 % 100)
}

/// `M:SS`, as the player counts.
fn mmss(t: Duration) -> String {
    let s = t.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_stacks_as_the_prototype_does() {
        assert_eq!(LEVEL_Y, 31);
        assert_eq!(LEVEL_BAR_Y, 33);
        assert_eq!((row_y(0), row_y(4)), (43, 103));
        assert_eq!(dropdown::CHIP_X, 71);
    }

    #[test]
    fn the_timecode_counts_hundredths() {
        assert_eq!(timecode(Duration::from_millis(3_723_456)), "01:02:03:45");
        assert_eq!(mmss(Duration::from_millis(65_900)), "1:05");
    }

    #[test]
    fn the_format_picker_opens_upward() {
        let options: Vec<String> = FORMATS.iter().map(|f| f.label().to_string()).collect();
        let chip_y = row_y(4) + metric::DROP_TOP_PAD;
        let p = upward(dropdown::picker(chip_y, "", &options, 0));
        assert_eq!(p.y + p.h, chip_y + metric::DROP_CHIP_H);
        assert!(p.y + p.h <= PANEL_H as i32);
        let down = upward(dropdown::picker(row_y(1) + metric::DROP_TOP_PAD, "", &options[..2], 0));
        assert_eq!(down.y, row_y(1) + metric::DROP_TOP_PAD, "one that fits stays put");
    }
}
