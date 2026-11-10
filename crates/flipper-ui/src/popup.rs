//! A dialog that asks for one of a few options, over anything on the panel.
//!
//! The modal's frame around a centred title, the options on the dropdown picker's
//! pitch with an optional note for each in a second column, and a few lines under
//! them about the picked option. The
//! frame is as wide as the widest of everything it may show and as tall as the most
//! hint lines any option has, so it does not change size as the selector moves.
//!
//! Laid out here rather than in the component because the frame is sized to its
//! text and the title and hints are centred, and Slint cannot measure a string it is
//! about to draw.

use crate::font::{fit, tw, ROW_ACTIVE};
use crate::theme::{metric, PANEL_H, PANEL_W};

/// What is being asked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choice {
    pub title: String,
    pub options: Vec<String>,
    /// One per option, drawn in a column after the options. Empty for none.
    pub notes: Vec<String>,
    /// The lines under the list for each option, none for an option with nothing
    /// to say.
    pub hints: Vec<Vec<String>>,
    pub selected: usize,
}

impl Choice {
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self) {
        self.selected = (self.selected + 1).min(self.options.len().saturating_sub(1));
    }
}

/// One option, as the component draws it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    /// Top of the option, in panel coordinates.
    pub y: i32,
    /// Already cut to the room the frame has.
    pub text: String,
    pub note: String,
    /// Where the note column starts, from the option's left edge.
    pub note_x: i32,
    /// The rule under it, which every option but the last has.
    pub rule: bool,
}

/// A centred line of text, in panel coordinates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub x: i32,
    pub y: i32,
    pub text: String,
}

/// The whole dialog, in panel coordinates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub title: Line,
    /// The options' left edge and width: the selector's, and the rules'.
    pub row_x: i32,
    pub row_w: i32,
    pub rows: Vec<Row>,
    pub sel_y: i32,
    /// The selected option's hint lines.
    pub hints: Vec<Line>,
}

/// Lay out `choice`, centred on the panel between the status bar and the soft keys.
pub fn view(choice: &Choice) -> View {
    let widest = |lines: &mut dyn Iterator<Item = &String>| lines.map(|l| tw(l)).max().unwrap_or(0);
    // Every line keeps the margin an option's text has inside the selector.
    let margin = metric::CHOICE_INSET + metric::CHOICE_TEXT_X;
    let label_w = widest(&mut choice.options.iter());
    let note_w = widest(&mut choice.notes.iter());
    let row_content = if note_w > 0 { label_w + metric::CHOICE_COL_GAP + note_w } else { label_w };
    // The title is in the heavy face, which measures with its own table.
    let bold = |text: &str| i32::from(ROW_ACTIVE.text_width(text));
    let content =
        bold(&choice.title).max(row_content).max(widest(&mut choice.hints.iter().flatten()));
    let w = (content + margin * 2).min(metric::MODAL_W);
    let x = (PANEL_W as i32 - w) / 2;

    let n = choice.options.len() as i32;
    let pitch = metric::DROP_PICK_ITEM_H + metric::DROP_PICK_GAP;
    // The selector wraps an option and a pixel either side, so the list's own
    // extent is the selector's at both ends.
    let list_h =
        (metric::DROP_PICK_SEL_H - metric::DROP_PICK_ITEM_H) + pitch * n - metric::DROP_PICK_GAP;
    let hint_lines = choice.hints.iter().map(Vec::len).max().unwrap_or(0) as i32;
    let h = metric::CHOICE_PAD * 2
        + metric::MODAL_LINE_H * (1 + hint_lines)
        + metric::CHOICE_TITLE_GAP
        + list_h
        + metric::CHOICE_HINT_GAP;
    let top = metric::STATUS_BAR_H;
    let bottom = PANEL_H as i32 - metric::BUTTON_H;
    let y = top + (bottom - top - h) / 2;

    let room = w - metric::CHOICE_INSET * 2;
    let centred = |text: &str, y: i32| {
        let text = fit(text, w - margin * 2);
        Line { x: x + (w - tw(&text)) / 2, y, text }
    };
    let title_line = y + metric::CHOICE_PAD;
    let title = Line {
        x: x + (w - bold(&choice.title)) / 2,
        y: title_line + metric::CHOICE_TITLE_DY,
        text: choice.title.clone(),
    };
    let list_y = title_line + metric::MODAL_LINE_H + metric::CHOICE_TITLE_GAP;
    let first = list_y + metric::DROP_PICK_PAD;
    let rows = choice
        .options
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let note_x = metric::CHOICE_TEXT_X + label_w + metric::CHOICE_COL_GAP;
            let note = choice.notes.get(i).map_or("", String::as_str);
            Row {
                y: first + i as i32 * pitch,
                text: fit(text, room - metric::CHOICE_TEXT_X * 2),
                note: fit(note, room - note_x - metric::CHOICE_TEXT_X),
                note_x,
                rule: i as i32 != n - 1,
            }
        })
        .collect();
    let sel = choice.selected.min(choice.options.len().saturating_sub(1)) as i32;
    let hint_y = list_y + list_h + metric::CHOICE_HINT_GAP;
    let hints = choice
        .hints
        .get(sel as usize)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, text)| centred(text, hint_y + i as i32 * metric::MODAL_LINE_H))
        .collect();
    View {
        x,
        y,
        w,
        h,
        title,
        row_x: x + metric::CHOICE_INSET,
        row_w: room,
        rows,
        sel_y: list_y + sel * pitch,
        hints,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(selected: usize) -> Choice {
        Choice {
            title: "Select USB-C1 output power".into(),
            options: vec!["5V 3A (15W)".into(), "5-20V (18W)".into(), "5-20V (27W)".into()],
            notes: vec![
                "Compatibility mode".into(),
                "Normal mode".into(),
                "Burst-only high power mode".into(),
            ],
            hints: vec![vec![], vec![], vec!["CAUTION: the device will overheat quickly".into()]],
            selected,
        }
    }

    #[test]
    fn the_frame_is_centred_between_the_status_bar_and_the_soft_keys() {
        let v = view(&choice(0));
        let above = v.y - metric::STATUS_BAR_H;
        let below = PANEL_H as i32 - metric::BUTTON_H - (v.y + v.h);
        assert!(above >= 0 && below >= 0, "{above} above, {below} below");
        assert!((above - below).abs() <= 1, "{above} above, {below} below");
        let right = PANEL_W as i32 - (v.x + v.w);
        assert!((v.x - right).abs() <= 1, "{} left, {right} right", v.x);
    }

    /// As wide as the widest line it may show, whichever option is picked, and
    /// no wider.
    #[test]
    fn the_frame_fits_its_widest_line() {
        let v = view(&choice(0));
        let widest = tw("5-20V (27W)") + metric::CHOICE_COL_GAP + tw("Burst-only high power mode");
        assert_eq!(v.w, widest + (metric::CHOICE_INSET + metric::CHOICE_TEXT_X) * 2);
        let row = &v.rows[2];
        assert_eq!(row.note_x + tw(&row.note), v.row_w - metric::CHOICE_TEXT_X);
        assert!(v.w < metric::MODAL_W);
    }

    /// The selector moves and nothing else does: the frame keeps the room for the
    /// most hint lines any option has.
    #[test]
    fn moving_the_selector_moves_only_the_selector_and_the_hints() {
        let (a, b) = (view(&choice(0)), view(&choice(1)));
        assert_eq!((a.x, a.y, a.w, a.h, &a.rows), (b.x, b.y, b.w, b.h, &b.rows));
        assert_eq!(b.sel_y - a.sel_y, metric::DROP_PICK_ITEM_H + metric::DROP_PICK_GAP);
        assert!(b.hints.is_empty());
        assert_eq!(view(&choice(2)).h, a.h);
    }

    #[test]
    fn everything_fits_inside_the_frame() {
        let v = view(&choice(2));
        let last = v.rows.last().unwrap();
        let first_hint = v.hints[0].y;
        assert!(v.sel_y + metric::DROP_PICK_SEL_H <= first_hint);
        assert!(last.y + metric::DROP_PICK_ITEM_H < first_hint);
        assert!(v.hints[0].y + metric::MODAL_LINE_H + metric::CHOICE_PAD <= v.y + v.h);
        for row in &v.rows {
            assert_eq!(row.text, fit(&row.text, v.row_w));
        }
        assert!(!last.rule && v.rows[0].rule);
        for line in &v.hints {
            assert!(line.x > v.x && line.x + tw(&line.text) < v.x + v.w, "{line:?}");
        }
        let title_w = i32::from(ROW_ACTIVE.text_width(&v.title.text));
        assert!(v.title.x > v.x && v.title.x + title_w < v.x + v.w);
    }

    #[test]
    fn the_selector_stays_on_the_list() {
        let mut c = choice(0);
        c.up();
        assert_eq!(c.selected, 0);
        c.down();
        c.down();
        c.down();
        assert_eq!(c.selected, 2);
    }
}
