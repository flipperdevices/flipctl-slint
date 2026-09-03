//! The settings row with a value chip, and the picker that opens over it.
//!
//! From component-library/MenuDropdownLine.js and the dropdown internet_radio.js
//! draws over one. A title on the left, a chip anchored to the panel's right edge
//! on the right, and in the chip either a value that left and right cycle through
//! or a bar that they move. Rows stack, so a hosted app's settings page is a list
//! of these and its own title bar and nothing else.
//!
//! Every number a row needs is worked out here rather than in the component, for
//! the reason the whole port measures in Rust: Slint cannot measure a string it is
//! about to draw, and a centred value is a measurement. What the component gets is
//! a string already cut to fit and the x it starts at.

use crate::font::{fit, tw};
use crate::theme::{metric, PANEL_W};

/// A chip showing one of a set of values, which left and right cycle through.
pub const CHIP: i32 = 0;
/// A chip that fills from the left, which left and right move.
pub const SLIDER: i32 = 1;

/// The chip's left edge. Fixed rather than sized to its content, so a column of
/// them lines up whatever the values say.
pub const CHIP_X: i32 = PANEL_W as i32 - metric::DROP_CHIP_PAD_R - metric::DROP_CHIP_W;

/// The pitch a stack of rows sits on.
pub const PITCH: i32 = metric::DROP_ROW_H + metric::DROP_ROW_GAP;

/// One settings row, as the component draws it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    /// Top of the row. Given, because an app puts its own gaps between them.
    pub y: i32,
    pub title: String,
    /// Already cut to the room the chip has, ending in ".." when it had to.
    pub value: String,
    /// Where the value starts, from the chip's left edge. Centred in the whole
    /// chip, not in what the chevrons leave of it: the value would otherwise
    /// shift sideways as the selector arrived and left.
    pub value_x: i32,
    /// `CHIP` or `SLIDER`.
    pub kind: i32,
    /// A slider's fill, in pixels from the chip's left edge. Zero on a chip.
    pub fill_w: i32,
    /// Puts the chevrons in the chip. The selector frame around the row is the
    /// page's own business, and this is only the ornament inside it.
    pub selected: bool,
}

/// Build a row. `slider` is `None` for a chip and a 0..1 fraction for a slider.
pub fn line(y: i32, title: &str, value: &str, slider: Option<f32>, selected: bool) -> Line {
    // The chevrons take their room out of the value's, so a long value is cut to
    // what is left rather than drawn over them.
    let arrows = if selected && slider.is_none() {
        tw("<") + tw(">") + metric::DROP_ARROW_PAD * 2
    } else {
        0
    };
    let value = fit(value, metric::DROP_CHIP_W - metric::DROP_CHIP_INSET - arrows);
    Line {
        y,
        title: title.to_string(),
        value_x: (metric::DROP_CHIP_W - tw(&value)) / 2,
        value,
        kind: if slider.is_some() { SLIDER } else { CHIP },
        fill_w: slider.map_or(0, fill_w),
        selected,
    }
}

/// A slider's fill, in pixels from the chip's left edge.
///
/// Never narrower than the nub: an empty chip reads as a control that is missing
/// rather than as a value at the bottom of its range.
pub fn fill_w(fraction: f32) -> i32 {
    let width = (fraction.clamp(0.0, 1.0) * metric::DROP_CHIP_W as f32).round() as i32;
    width.max(metric::DROP_SLIDER_MIN_W)
}

/// One option of an open picker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    /// Top of the option, in panel coordinates.
    pub y: i32,
    /// Already cut to the picker's width.
    pub text: String,
    /// Centred, from the picker's left edge.
    pub text_x: i32,
    /// A rule under it, which the last option does not have.
    pub rule: bool,
}

/// The picker open over a chip: the chip grown downwards to show its options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Picker {
    /// The frame, in panel coordinates. Its top-left corner is the chip's own,
    /// which is what makes it read as the chip having grown rather than as a
    /// dialog that happens to be near it.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// The row's title, drawn again over the wash so the user can see which row
    /// they are picking a value for.
    pub title: String,
    pub items: Vec<Item>,
    /// The selector, in panel coordinates. It wraps the option and the pixel of
    /// fill above and below it.
    pub sel_y: i32,
    pub sel_w: i32,
    pub sel_h: i32,
}

/// How tall a picker of `n` options is.
pub fn picker_h(n: usize) -> i32 {
    let n = n as i32;
    metric::DROP_PICK_ITEM_H * n + metric::DROP_PICK_GAP * (n - 1).max(0)
        + metric::DROP_PICK_PAD * 2
}

/// Lay out the picker for a row whose chip is at `chip_y`.
pub fn picker(chip_y: i32, title: &str, options: &[String], selected: usize) -> Picker {
    let pitch = metric::DROP_PICK_ITEM_H + metric::DROP_PICK_GAP;
    let last = options.len().saturating_sub(1);
    let items: Vec<Item> = options
        .iter()
        .enumerate()
        .map(|(i, option)| {
            let text = fit(option, metric::DROP_CHIP_W - metric::DROP_CHIP_INSET);
            Item {
                y: chip_y + metric::DROP_PICK_PAD + i as i32 * pitch,
                text_x: (metric::DROP_CHIP_W - tw(&text)) / 2,
                text,
                rule: i != last,
            }
        })
        .collect();
    let sel = selected.min(last) as i32;
    Picker {
        x: CHIP_X,
        y: chip_y,
        w: metric::DROP_CHIP_W,
        h: picker_h(options.len()),
        title: title.to_string(),
        items,
        sel_y: chip_y + sel * pitch,
        sel_w: metric::DROP_CHIP_W - metric::DROP_PICK_SEL_INSET,
        sel_h: metric::DROP_PICK_SEL_H,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// The chip is right-anchored, which is the whole reason a column of them
    /// lines up: 256 - 5 - 180.
    #[test]
    fn the_chip_hugs_the_panels_right_edge() {
        assert_eq!(CHIP_X, 71);
        assert_eq!(CHIP_X + metric::DROP_CHIP_W, PANEL_W as i32 - metric::DROP_CHIP_PAD_R);
    }

    #[test]
    fn a_value_is_centred_in_the_whole_chip() {
        let row = line(0, "City", "London", None, false);
        assert_eq!(row.value, "London");
        assert_eq!(row.value_x, (metric::DROP_CHIP_W - tw("London")) / 2);
        // Centred, so the room on the right is the room on the left, give or take
        // the odd pixel integer division leaves.
        let right = metric::DROP_CHIP_W - tw(&row.value) - row.value_x;
        assert!((right - row.value_x).abs() <= 1, "{} vs {right}", row.value_x);
    }

    /// The value does not move when the selector arrives, because it is centred
    /// in the chip and not in what the chevrons leave of it.
    #[test]
    fn selecting_a_row_does_not_shift_a_short_value() {
        let plain = line(0, "City", "London", None, false);
        let picked = line(0, "City", "London", None, true);
        assert_eq!(plain.value_x, picked.value_x);
        assert!(picked.selected && !plain.selected);
    }

    /// A long value is cut to what the chevrons leave, rather than drawn under
    /// them.
    #[test]
    fn the_chevrons_take_their_room_out_of_a_long_value() {
        let long = "Radio Nacional de Espana Clasica FM 105.5 Madrid";
        let plain = line(0, "Station", long, None, false);
        let picked = line(0, "Station", long, None, true);
        assert!(plain.value.ends_with(".."), "{}", plain.value);
        assert!(picked.value.ends_with(".."), "{}", picked.value);
        assert!(
            tw(&picked.value) < tw(&plain.value),
            "selected {} is not shorter than unselected {}",
            picked.value,
            plain.value
        );
        let room = metric::DROP_CHIP_W - metric::DROP_CHIP_INSET - tw("<") - tw(">")
            - metric::DROP_ARROW_PAD * 2;
        assert!(tw(&picked.value) <= room);
    }

    /// A slider keeps a visible nub at the bottom of its range and fills the chip
    /// at the top of it.
    #[test]
    fn a_sliders_fill_never_disappears_and_never_overruns() {
        assert_eq!(fill_w(0.0), metric::DROP_SLIDER_MIN_W);
        assert_eq!(fill_w(-1.0), metric::DROP_SLIDER_MIN_W);
        assert_eq!(fill_w(0.01), metric::DROP_SLIDER_MIN_W);
        assert_eq!(fill_w(1.0), metric::DROP_CHIP_W);
        assert_eq!(fill_w(2.0), metric::DROP_CHIP_W);
        assert_eq!(fill_w(0.5), metric::DROP_CHIP_W / 2);
        // Rounded, not truncated: a third of 180 is 60 and a third of the way up
        // should not read a pixel short.
        assert_eq!(fill_w(1.0 / 3.0), 60);
    }

    /// A slider's own value is centred in the chip like any other, and its
    /// chevrons do not shorten it: they invert across the fill instead of sitting
    /// beside the value.
    #[test]
    fn a_slider_row_carries_its_fill_and_its_value() {
        let row = line(0, "Volume", "60%", Some(0.6), true);
        assert_eq!(row.kind, SLIDER);
        assert_eq!(row.fill_w, fill_w(0.6));
        assert_eq!(row.value_x, (metric::DROP_CHIP_W - tw("60%")) / 2);
        assert_eq!(row.value, line(0, "Volume", "60%", Some(0.6), false).value);
    }

    /// The prototype's own arithmetic: 13 x 3 + 2 + 2.
    #[test]
    fn three_options_come_to_fortythree_pixels() {
        assert_eq!(picker_h(3), 43);
        assert_eq!(picker_h(1), metric::DROP_PICK_ITEM_H + metric::DROP_PICK_PAD * 2);
    }

    #[test]
    fn the_options_stack_under_the_chip_with_a_rule_between_them() {
        let picker = picker(30, "Audio device", &opts(&["Speaker", "Headphones", "HDMI"]), 1);
        assert_eq!((picker.x, picker.y), (CHIP_X, 30));
        assert_eq!(picker.h, 43);
        assert_eq!(picker.title, "Audio device");
        let ys: Vec<i32> = picker.items.iter().map(|i| i.y).collect();
        assert_eq!(ys, vec![31, 45, 59]);
        // The last option has nothing under it to be kept apart from.
        assert_eq!(
            picker.items.iter().map(|i| i.rule).collect::<Vec<_>>(),
            vec![true, true, false]
        );
        // Every option ends inside the frame, the last one a pixel clear of its
        // bottom edge.
        let bottom = picker.items.last().unwrap().y + metric::DROP_PICK_ITEM_H;
        assert_eq!(bottom, picker.y + picker.h - metric::DROP_PICK_PAD);
    }

    /// The selector wraps its option and the pixel of fill either side of it, and
    /// stops a pixel short of the frame's right edge so the shadow it paints
    /// stays inside the picker's silhouette.
    #[test]
    fn the_selector_is_inset_by_the_pixel_its_shadow_needs() {
        let picker = picker(30, "City", &opts(&["London", "Paris", "Madrid"]), 2);
        assert_eq!(picker.sel_y, 30 + 2 * 14);
        assert_eq!(picker.items[2].y, picker.sel_y + metric::DROP_PICK_PAD);
        assert_eq!(picker.sel_h, metric::DROP_PICK_ITEM_H + metric::DROP_PICK_PAD * 2);
        assert_eq!(picker.sel_w, picker.w - 1);
    }

    /// A selection past the end lands on the last option rather than off the
    /// frame: the options come from the device and the index from the last time
    /// the user looked at them.
    #[test]
    fn a_stale_selection_is_clamped_to_the_options_there_are() {
        let picker = picker(30, "Station", &opts(&["Jazz FM", "Radio 4"]), 9);
        assert_eq!(picker.sel_y, picker.items[1].y - metric::DROP_PICK_PAD);
    }

    #[test]
    fn an_option_too_long_for_the_picker_is_cut_the_way_a_value_is() {
        let picker = picker(30, "Station", &opts(&["Radio Nacional de Espana Clasica FM 105.5 Madrid"]), 0);
        assert!(picker.items[0].text.ends_with(".."));
        assert!(tw(&picker.items[0].text) <= metric::DROP_CHIP_W - metric::DROP_CHIP_INSET);
        assert!(picker.items[0].text_x >= 0);
    }
}
