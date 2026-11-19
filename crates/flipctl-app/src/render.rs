//! An app drawn headlessly, as the panel would get it, for the app's own tests.
//!
//! The same software renderer the device runs, on a window of panel size with no
//! compositor behind it, so a test reads the very bytes that would go to glass. An
//! app built with [`main!`](crate::main) gets `flipctl_pages::shot` for this; one with
//! a page of its own shows it after [`panel`] and calls [`frame`].

use std::path::Path;
use std::rc::Rc;

use flipper_ui::Rotate;
use flipper_ui::slint_render::{FlipperSlintPlatform, render_frame, render_rotated};
use slint::platform::software_renderer::MinimalSoftwareWindow;

use crate::theme::{PANEL_H, PANEL_W};

/// The panel, once per thread: installing the platform twice is a panic, and tests
/// run in a thread each.
pub fn panel() -> Rc<MinimalSoftwareWindow> {
    thread_local! {
        static WINDOW: Rc<MinimalSoftwareWindow> = FlipperSlintPlatform::install();
    }
    WINDOW.with(Rc::clone)
}

/// A portrait page's window, once per thread, the way [`panel`] is the panel's.
///
/// A portrait app lays out 144 by 256 and flipctl turns the frame, so its tests render a
/// window of that shape. It is not the panel's window: a component measured in 256 columns
/// is not what the device gets.
pub fn portrait_panel() -> Rc<MinimalSoftwareWindow> {
    thread_local! {
        static WINDOW: Rc<MinimalSoftwareWindow> =
            FlipperSlintPlatform::install_sized(u32::from(PANEL_H), u32::from(PANEL_W));
    }
    WINDOW.with(Rc::clone)
}

/// What the window shows now, 256x144 bytes of grey.
pub fn frame(window: &MinimalSoftwareWindow) -> Vec<u8> {
    slint::platform::update_timers_and_animations();
    render_frame(window).expect("frame").iter().map(|px| px.0).collect()
}

/// What a portrait window shows, turned onto the panel: 256x144 bytes of grey.
///
/// This is a portrait app's whole path to the glass in one call: lay the page out 144 by
/// 256 and let the renderer turn it. `turn` is what the manifest says, so a test checks the
/// direction the device will use rather than assuming it.
pub fn portrait_frame(window: &MinimalSoftwareWindow, turn: Rotate) -> Vec<u8> {
    slint::platform::update_timers_and_animations();
    render_rotated(window, crate::portrait::rotation(turn))
        .expect("a turned frame")
        .iter()
        .map(|px| px.0)
        .collect()
}

/// A frame as an 8-bit greyscale PNG at panel size.
pub fn save(path: &Path, frame: &[u8]) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("create the directory");
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut encoder =
        png::Encoder::new(std::io::BufWriter::new(file), u32::from(PANEL_W), u32::from(PANEL_H));
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().expect("png header").write_image_data(frame).expect("png data");
}
