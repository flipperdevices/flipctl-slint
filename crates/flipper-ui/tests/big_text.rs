//! Does a Slint `Text` rasterise cleanly at a size other than 16px?
//!
//! The panel's fonts are pixel faces on a 64-unit grid, and `fonts.slint` warns
//! that they are clean "at 16px and only at 16px". A portrait screen needs the
//! frequency several times larger than a row, so whether an enlarged `Text` is
//! clean blocks or a smear decides how that number can be drawn at all: if Slint
//! can enlarge it, the number is a `Text` like everything else, and if it cannot
//! it has to be drawn as pixels somewhere.

use flipper_ui::pixel::Gray8;
use flipper_ui::slint_render::{render_frame, FlipperSlintPlatform};
use flipper_ui::theme::{PANEL_H, PANEL_W};

/// The page: the panel read the long way up.
const PW: usize = PANEL_H as usize;
const PH: usize = PANEL_W as usize;

/// The distinct grey values a rendering uses. A pixel font rasterised on its own
/// grid has none between its two extremes; antialiased text has many.
fn tones(frame: &[Gray8]) -> usize {
    let mut seen = [false; 256]; // not-a-panel-dimension: a grey level, of which there are 256
    for px in frame {
        seen[px.0 as usize] = true;
    }
    seen.iter().filter(|s| **s).count()
}

/// Inked pixels per row, to see whether a stroke is a run of whole blocks or a
/// gradient of partial ones.
fn stems(frame: &[Gray8], w: usize, h: usize) -> Vec<usize> {
    (0..h).map(|y| (0..w).filter(|&x| frame[y * w + x].0 < 128).count()).collect()
}

slint::slint! {
    // Imported for the side effect: a .ttf import registers the face, which is
    // what `fonts.slint` does for the screens that use the theme's names.
    import "../../third_party/flipctl-fonts/HaxrCorp4090-FlipCTL/HaxrCorp4090-FlipCTL.ttf";
    export component Big inherits Window {
        width: 144px; // not-a-panel-dimension: the page, stated as the test's subject
        height: 256px; // not-a-panel-dimension: the page's long side, likewise
        background: #ffffff;  // not-a-raw-colour: a fixture of known black on white, so the frame can be counted
        Text {
            x: 4px;
            y: 40px;
            text: "868.300";
            font-size: 32px;
            font-family: "HaxrCorp 4090";
            color: #000000;  // not-a-raw-colour: a fixture of known black on white, so the frame can be counted
        }
    }
}

#[test]
fn an_enlarged_text_is_still_whole_blocks() {
    let window = FlipperSlintPlatform::install_sized(PW as u32, PH as u32);
    let big = Big::new().expect("the component");
    big.show().expect("show");
    let frame = render_frame(&window).expect("a frame");
    assert_eq!(frame.len(), PW * PH, "the frame is the window's");

    // Clean means two tones only: the paper and the ink. Anything between is the
    // rasteriser covering a pixel in part, which on this panel is a smudge.
    let used = tones(&frame);
    assert!(
        used <= 2,
        "the text rasterised into {used} greys: that is antialiasing, not a pixel face"
    );
}
