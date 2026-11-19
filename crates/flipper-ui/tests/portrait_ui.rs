//! A settings row built for a portrait page, and the page turned onto the panel.
//!
//! A portrait screen has two halves that were never proven together: a row that
//! measures its chip against a 144-wide page rather than the panel's 256, and a
//! frame that is turned a quarter turn on its way to the glass. This draws a
//! real `DropLine` - the component the voice recorder uses, not a look-alike -
//! into a portrait window and turns the result, so both are checked on the same
//! pixels the device would get.

use flipper_ui::pixel::Gray8;
use flipper_ui::slint_render::{render_frame, turn_left, FlipperSlintPlatform};
use flipper_ui::theme::{PANEL_H, PANEL_W};

slint::include_modules!();

/// The page: the panel read the long way up.
const PW: usize = PANEL_H as usize;
const PH: usize = PANEL_W as usize;

/// A row with a value that fills a narrow chip, picked so the chevrons are on.
///
/// `dropdown::line_in` works out the fitting and the centring in Rust and the
/// result is carried over to Slint as a `DropRow`, which is how the voice
/// recorder does it too: Slint cannot measure a string it is about to draw.
fn row() -> DropRow {
    let line = flipper_ui::dropdown::line_in(62, 40, "Power", "22 dBm", None, true);
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

/// The x of the inked columns, to see where a chip landed.
fn ink_columns(frame: &[Gray8], w: usize, h: usize) -> (usize, usize) {
    let mut xs = (0..w).filter(|&x| (0..h).any(|y| frame[y * w + x].0 < 128));
    let lo = xs.next().unwrap_or(0);
    let hi = xs.last().unwrap_or(0);
    (lo, hi)
}

#[test]
fn a_portrait_row_draws_its_chip_against_the_page_and_not_the_panel() {
    let window = FlipperSlintPlatform::install_sized(PW as u32, PH as u32);
    let ui = PortraitRow::new().expect("the component");
    ui.set_line(row());
    ui.show().expect("show");

    let frame = render_frame(&window).expect("a frame");
    assert_eq!(frame.len(), PW * PH, "the frame is the portrait page's");

    // The chip is on the right of a 144-wide page: 144 - 5 - 62 = 77, and it
    // runs to the page's right margin. On the panel's metrics it would have
    // started at 256 - 5 - 180 = 71 and run 63 px past the edge of this page,
    // so the check is that its own width put it inside the page.
    let hi = ink_columns(&frame, PW, PH).1;
    assert!(
        hi < PW,
        "the row's ink runs to x={hi}: the chip is wider than the page it is on",
    );
    // The chip's own band, 77 to 139, has to carry ink; the title and the
    // selector are elsewhere and say nothing about the chip.
    let chip_ink: usize = (77..140)
        .filter(|&x| (0..PH).any(|y| frame[y * PW + x].0 < 128))
        .count();
    assert!(
        chip_ink > 20,
        "only {chip_ink} columns of the chip's band are inked: the chip is not on the right",
    );
}

#[test]
fn a_portrait_page_turns_onto_the_panel() {
    let window = FlipperSlintPlatform::install_sized(PW as u32, PH as u32);
    let ui = PortraitRow::new().expect("the component");
    ui.set_line(row());
    ui.show().expect("show");

    let page = render_frame(&window).expect("a frame");
    let panel = turn_left(&page, PW, PH);

    // The panel is 256 across and 144 down, and a portrait page turned onto it
    // has to come out that shape.
    assert_eq!(panel.len(), PANEL_W as usize * PANEL_H as usize, "the turned frame is the panel's size");

    // What was inked is still inked: a turn moves pixels and loses none.
    let inked = |f: &[Gray8]| f.iter().filter(|px| px.0 < 128).count();
    assert_eq!(inked(&page), inked(&panel), "a turn moves the ink, it does not drop any");
}

#[test]
fn a_turned_row_is_where_the_panel_expects_its_right_edge() {
    let window = FlipperSlintPlatform::install_sized(PW as u32, PH as u32);
    let ui = PortraitRow::new().expect("the component");
    ui.set_line(row());
    ui.show().expect("show");

    let page = render_frame(&window).expect("a frame");
    let panel = turn_left(&page, PW, PH);

    // The page's right edge is the panel's top edge after a turn left. The chip
    // hugs that edge on the page, so its ink has to sit at the top of the panel
    // and not in the middle of it.
    let (lo, _hi) = ink_columns(&panel, PANEL_W as usize, PANEL_H as usize);
    let top_band: usize = (0..PANEL_H as usize)
        .filter(|&y| (0..PANEL_W as usize).any(|x| panel[y * PANEL_W as usize + x].0 < 128))
        .take(3)
        .count();
    assert!(
        top_band > 0,
        "the chip's edge is at the top of the panel and nothing was inked there"
    );
    assert!(lo < PANEL_W as usize, "and the ink is inside the panel");
}
