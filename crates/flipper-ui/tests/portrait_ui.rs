//! A settings row built for a portrait page, and the page turned onto the panel.
//!
//! A portrait screen has two halves that were never proven together: a row that
//! measures its chip against a 144-wide page rather than the panel's 256, and a
//! frame that is turned a quarter turn on its way to the glass. This draws a
//! real `DropLine` - the component the voice recorder uses, not a look-alike -
//! into a portrait window and turns the result the way the renderer does on the
//! device, so both are checked on the same pixels the panel would get.
//!
//! The turn is the renderer's own (`set_rendering_rotation`), not a helper applied
//! to a finished frame: `turn_left` stays behind as the reference this compares
//! against, and a test that used it to *produce* the picture would only be checking
//! the helper against itself.

use flipper_ui::pixel::Gray8;
use flipper_ui::slint_render::{render_frame, render_rotated, FlipperSlintPlatform};
use flipper_ui::theme::{PANEL_H, PANEL_W};
use slint::platform::software_renderer::RenderingRotation;

// The fixture's own generated module: `build.rs` compiles `tests/portrait_row.slint` on its
// own, so this is the row and not the production screens - which do not carry it.
include!(concat!(env!("OUT_DIR"), "/portrait_row.rs"));

/// The page: the panel read the long way up.
const PW: usize = PANEL_H as usize;
const PH: usize = PANEL_W as usize;

/// The chip a portrait page gives a row: two thirds of its 144 columns, so a title like
/// "Audio out" still fits beside it. The panel's own chip is 180 wide and does not fit on
/// this page at all, which is the whole reason the row takes a width.
const CHIP_W: i32 = 62;

/// A row with a value that fills a narrow chip.
///
/// `dropdown::line_in` works out the fitting and the centring in Rust and the result is
/// carried over to Slint as a `DropRow`, which is how the voice recorder does it too: Slint
/// cannot measure a string it is about to draw.
///
/// `selected` belongs to the caller: with it on, the selector frame outlines the whole row
/// and inks every column of it, so a test about where the *chip* landed has to turn it off.
fn row(selected: bool) -> DropRow {
    let line = flipper_ui::dropdown::line_in(CHIP_W, 40, "Power", "22 dBm", None, selected);
    DropRow {
        y: line.y as f32,
        title: line.title.as_str().into(),
        value: line.value.as_str().into(),
        value_x: line.value_x as f32,
        kind: line.kind,
        fill_w: line.fill_w as f32,
        selected: line.selected,
    }
}

/// The row drawn into a portrait window: the page as it is laid out, and the same page
/// turned onto the panel by the renderer's own rotation - the path a device takes.
/// `Rotate270` is the quarter turn this framework calls left.
///
/// One window for both, because a platform may only be installed once per thread: the
/// second install is the one that panics.
fn frames(selected: bool) -> (Vec<Gray8>, Vec<Gray8>) {
    let window = FlipperSlintPlatform::install_sized(PW as u32, PH as u32);
    let ui = PortraitRow::new().expect("the component");
    ui.set_chip_w(CHIP_W as f32);
    ui.set_line(row(selected));
    ui.show().expect("show");

    let page = render_frame(&window).expect("a frame");
    // The first render leaves the window clean, and a clean window draws nothing.
    window.request_redraw();
    let panel = render_rotated(&window, RenderingRotation::Rotate270).expect("a turned frame");
    (page, panel)
}

#[test]
fn the_chip_is_measured_from_the_page_and_not_from_the_panel() {
    let (frame, _) = frames(false);

    // Where the page puts a chip of its own width: 144 - 5 - 62 = 77, running to 138.
    let left = flipper_ui::dropdown::chip_x_in(PW as i32, CHIP_W);
    let right = left + CHIP_W - 1;
    assert_eq!((left, right), (77, 138), "the page's chip, worked out from its own width");

    // The chip is a text area rather than a drawn box, so the row's rightmost ink is the
    // last glyph of the value, sitting inside it. That is still the measurement that
    // matters: centred in a chip measured from the panel - 71 to 250 - the same value
    // would run past column 138 and off a 144-wide page.
    let inked: Vec<usize> =
        (0..PW).filter(|&x| (0..PH).any(|y| frame[y * PW + x].0 < 128)).collect();
    let hi = *inked.last().expect("the row drew nothing at all");
    assert!(
        hi <= right as usize,
        "the row's ink runs to x={hi}, past the chip's right edge {right}: the chip was \
         measured against something wider than the page"
    );
    assert!(
        hi >= left as usize,
        "the row's ink stops at x={hi}, short of the chip starting at {left}"
    );

    // And the chip's own band carries ink, so the check above is about a chip that is there
    // rather than about a row that drew nothing.
    let inked =
        (left..=right).filter(|&x| (0..PH).any(|y| frame[y * PW + x as usize].0 < 128)).count();
    assert!(inked > 20, "only {inked} columns of the chip's band are inked");
}

#[test]
fn a_portrait_page_turns_onto_the_panel_pixel_for_pixel() {
    let (page, panel) = frames(false);

    assert_eq!(
        panel.len(),
        PANEL_W as usize * PANEL_H as usize,
        "the turned frame is the panel's size"
    );

    // Every pixel, not a summary: a quarter turn sends page `(x, y)` to panel `(y, 143 - x)`,
    // so panel row `y` is page column `143 - y`. A frame turned the other way, or not turned
    // at all, fails here on nearly every pixel.
    let mut wrong = 0;
    for y in 0..PANEL_H as usize {
        for x in 0..PANEL_W as usize {
            if panel[y * PANEL_W as usize + x] != page[x * PW + (PANEL_H as usize - 1 - y)] {
                wrong += 1;
            }
        }
    }
    assert_eq!(wrong, 0, "{wrong} pixels are not where a quarter turn puts them");
}
