//! Slint software renderer targeting the greyscale panel.
//!
//! Slint is taken with `renderer-software` alone and no backend, so we own the
//! window adapter, the frame buffer and the event loop. `Gray8` implements
//! Slint's public `TargetPixel` trait, so the renderer composites straight into
//! the surface the panel and every other sink already consume: no intermediate
//! RGB buffer, no format conversion in userspace.

use std::cell::RefCell;
use std::rc::Rc;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RenderingRotation, RepaintBufferType,
    TargetPixel,
};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::PhysicalSize;

use crate::pixel::{Gray8, Rect};
use crate::theme::{PANEL_H, PANEL_W};

impl TargetPixel for Gray8 {
    /// Blend a premultiplied colour over this sample.
    ///
    /// The colour arrives premultiplied, so its luma is added rather than
    /// interpolated. Luma uses the same Rec.601 weights as the kernel's
    /// `drm_fb_xrgb8888_to_gray8`, which keeps what we compute and what the
    /// panel receives in agreement.
    #[inline]
    fn blend(&mut self, colour: PremultipliedRgbaColor) {
        let keep = (u8::MAX - colour.alpha) as u16;
        let under = (self.0 as u16 * keep) / 255;
        let over = Gray8::from_rgb(colour.red, colour.green, colour.blue).0 as u16;
        self.0 = (under + over).min(255) as u8;
    }

    #[inline]
    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Gray8::from_rgb(r, g, b)
    }

    #[inline]
    fn background() -> Self {
        Gray8::WHITE
    }
}

/// Minimal platform: one fixed-size window, a monotonic clock, no event loop.
///
/// flipctl drives the loop itself, because it also owns DRM and input and must
/// keep running when no component wants to redraw.
pub struct FlipperSlintPlatform {
    window: Rc<MinimalSoftwareWindow>,
    start: std::time::Instant,
}

impl FlipperSlintPlatform {
    /// Install the platform. Must be called once, before any component is
    /// created.
    ///
    /// `ReusedBuffer` would let Slint track partial damage across frames, but
    /// the panel driver re-transmits the whole framebuffer on every atomic
    /// commit regardless, so there is nothing to save yet.
    pub fn install() -> Rc<MinimalSoftwareWindow> {
        Self::install_sized(u32::from(PANEL_W), u32::from(PANEL_H))
    }

    /// Install the platform with a window of `width` by `height`.
    ///
    /// A window is not always the panel's shape. An app whose design is a
    /// portrait page draws into a portrait window and turns the frame
    /// afterwards, which is the only way to get portrait out of a renderer that
    /// cannot rotate a component in place: `supports_transformations` is false
    /// for the software renderer, so `transform-rotation` in a `.slint` file is
    /// silently ignored and a turned layout has to be turned as pixels.
    ///
    /// The window is what Slint lays out in and [`render_into`] reads its size
    /// back, so the two can never disagree.
    pub fn install_sized(width: u32, height: u32) -> Rc<MinimalSoftwareWindow> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        window.set_size(PhysicalSize::new(width, height));

        let platform = Self { window: window.clone(), start: std::time::Instant::now() };
        slint::platform::set_platform(Box::new(platform)).expect("set_platform called twice");
        window
    }
}

impl Platform for FlipperSlintPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        self.start.elapsed()
    }
}

/// Render whatever the window currently holds into `into`, returning the damaged
/// region.
///
/// The buffer is the window's own size and stride, read back from the window
/// rather than from the panel's: a portrait window renders into a portrait
/// buffer, and the caller is the one that turns it. `None` means nothing needed
/// repainting, so the caller can skip the commit entirely rather than
/// re-transmitting an identical frame over SPI. `into` is reused across frames
/// so the steady state allocates nothing.
pub fn render_into(window: &MinimalSoftwareWindow, into: &mut Vec<Gray8>) -> Option<Rect> {
    render_rotated_into(window, RenderingRotation::NoRotation, into)
}

/// Convenience wrapper for callers that want an owned frame and do not care about
/// reusing the buffer, such as tests and the screenshot example.
pub fn render_frame(window: &MinimalSoftwareWindow) -> Option<Vec<Gray8>> {
    let mut frame = Vec::new();
    render_into(window, &mut frame).map(|_| frame)
}

/// Render whatever the window holds, turned a quarter or half turn on the way into
/// the buffer.
///
/// A portrait app's window is taller than it is wide, and the panel is the other way
/// round. The software renderer can turn the frame itself
/// ([`SoftwareRenderer::set_rendering_rotation`]), and it does the whole job in the
/// inner loop: the glyphs are drawn at the orientation they end up in, so text comes
/// out crisp rather than resampled. Slint's `transform-rotation` cannot be used for
/// this: the software renderer has no `supports_transformations` support and drops
/// the property without a word, so a page turned in the markup draws upright text
/// over turned shapes.
///
/// `into` is the panel-shaped buffer and is resized to the turned frame, so a
/// portrait window of `w` by `h` needs `w * h` pixels either way: a quarter turn
/// swaps the two sides, it does not add any.
///
/// [`SoftwareRenderer::set_rendering_rotation`]: slint::platform::software_renderer::SoftwareRenderer::set_rendering_rotation
pub fn render_rotated_into(
    window: &MinimalSoftwareWindow,
    rotation: RenderingRotation,
    into: &mut Vec<Gray8>,
) -> Option<Rect> {
    let size = window.size();
    let (w, h) = (size.width as usize, size.height as usize);
    // A quarter turn is a transpose: the stride is the window's height and the
    // buffer holds the window's width of lines. Anything else renders as it lays
    // out, so the stride stays the width and the renderer only mirrors.
    let transpose = matches!(rotation, RenderingRotation::Rotate90 | RenderingRotation::Rotate270);
    let stride = if transpose { h } else { w };
    into.resize(w * h, Gray8::WHITE);

    let region = Rc::new(RefCell::new(None));
    let taken = {
        let region = region.clone();
        window.draw_if_needed(|renderer| {
            renderer.set_rendering_rotation(rotation);
            let damaged = renderer.render(into.as_mut_slice(), stride);
            renderer.set_rendering_rotation(RenderingRotation::NoRotation);
            let origin = damaged.bounding_box_origin();
            let size = damaged.bounding_box_size();
            *region.borrow_mut() = Some(Rect::new(
                origin.x.max(0) as u16,
                origin.y.max(0) as u16,
                size.width as u16,
                size.height as u16,
            ));
        })
    };

    if !taken {
        return None;
    }
    let damage = *region.borrow();
    damage
}

/// Convenience wrapper for [`render_rotated_into`], for a caller that wants an
/// owned frame.
pub fn render_rotated(
    window: &MinimalSoftwareWindow,
    rotation: RenderingRotation,
) -> Option<Vec<Gray8>> {
    let mut frame = Vec::new();
    render_rotated_into(window, rotation, &mut frame).map(|_| frame)
}

/// Turn a `width` by `height` frame a quarter turn left, into a `height` by
/// `width` one.
///
/// Portrait `(x, y)` becomes `(y, width - 1 - x)`: the source's rows become the
/// result's columns and its columns run up the result from the bottom. Turning
/// the other way leaves the page upside down, which is how this direction was
/// found. It is a rotation and not a transposition - a transposition mirrors the
/// page and the text on it reads backwards.
#[cfg(test)]
pub fn turn_left(frame: &[Gray8], width: usize, height: usize) -> Vec<Gray8> {
    assert_eq!(frame.len(), width * height, "the frame is not width by height");
    let mut out = vec![Gray8::WHITE; width * height];
    for y in 0..height {
        for x in 0..width {
            out[(width - 1 - x) * height + y] = frame[y * width + x];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame with one distinguishable pixel in each corner, so a turn that
    /// mirrors rather than rotates has nowhere to hide.
    fn corners(w: usize, h: usize) -> Vec<Gray8> {
        let mut f = vec![Gray8::WHITE; w * h];
        f[0] = Gray8(1); // top-left
        f[w - 1] = Gray8(2); // top-right
        f[(h - 1) * w] = Gray8(3); // bottom-left
        f[h * w - 1] = Gray8(4); // bottom-right
        f
    }

    #[test]
    fn a_turn_moves_the_corners_round_and_does_not_mirror_them() {
        // 144 wide and 256 tall, the portrait page on its way to the panel.
        let (w, h) = (144usize, 256usize);
        let turned = turn_left(&corners(w, h), w, h);
        assert_eq!(turned.len(), w * h, "the frame keeps its pixel count");
        // The result is panel-shaped, so it is `w` rows of `h`. A quarter turn
        // left carries each corner round one place; a transposition would swap
        // them across instead, which is what the names here would catch.
        assert_eq!(turned[0], Gray8(2), "the top-right corner lands top-left");
        assert_eq!(turned[h - 1], Gray8(4), "the bottom-right corner lands top-right");
        assert_eq!(turned[(w - 1) * h], Gray8(1), "the top-left corner lands bottom-left");
        assert_eq!(turned[w * h - 1], Gray8(3), "the bottom-left corner lands bottom-right");
    }

    #[test]
    fn four_turns_bring_the_frame_back() {
        let (w, h) = (144usize, 256usize);
        let start = corners(w, h);
        let mut frame = start.clone();
        let (mut cw, mut ch) = (w, h);
        for _ in 0..4 {
            frame = turn_left(&frame, cw, ch);
            std::mem::swap(&mut cw, &mut ch);
        }
        assert_eq!(frame, start, "four quarter turns are a full turn");
    }

    #[test]
    fn a_turned_frame_is_panel_shaped() {
        let (w, h) = (144usize, 256usize);
        let turned = turn_left(&corners(w, h), w, h);
        // The panel is 256 wide and 144 tall: a portrait page turned onto it has
        // to come out that shape, which is the whole reason for turning.
        assert_eq!((h, w), (usize::from(PANEL_W), usize::from(PANEL_H)));
        assert_eq!(turned.len(), usize::from(PANEL_W) * usize::from(PANEL_H));
    }

    #[test]
    fn a_window_renders_into_a_buffer_of_its_own_size() {
        // Installing twice in one thread is a panic, and each test runs on its
        // own thread, so this is safe. A portrait window: the panel read the long way up.
        let (w, h) = (u32::from(PANEL_H), u32::from(PANEL_W));
        let window = FlipperSlintPlatform::install_sized(w, h);
        let mut buffer = Vec::new();
        render_into(&window, &mut buffer);
        assert_eq!(buffer.len(), (w * h) as usize, "the buffer is the window's, not the panel's");
    }
}

#[cfg(test)]
mod turned {
    use super::*;
    use slint::ComponentHandle as _;

    /// The portrait page: the panel read the long way up.
    const PW: u32 = PANEL_H as u32;
    const PH: u32 = PANEL_W as u32;

    slint::slint! {
        // Not a symmetric shape: a tall bar with a foot reads differently upright,
        // turned a quarter and mirrored, so a rotation confused with a transpose
        // cannot pass.
        export component Page inherits Window {
            width: 144px; // not-a-panel-dimension: the page's, which the tests state
            height: 256px; // not-a-panel-dimension: and the panel's long side, named
            background: #ffffff;  // not-a-raw-colour: a fixture of known black on white, so the frame can be counted
            Rectangle { x: 4px; y: 4px; width: 6px; height: 40px; background: #000000; }  // not-a-raw-colour: a fixture of known black on white, so the frame can be counted
            Rectangle { x: 4px; y: 38px; width: 30px; height: 6px; background: #000000; }  // not-a-raw-colour: a fixture of known black on white, so the frame can be counted
        }
    }

    /// The renderer's own quarter turn is the same pixels as turning the buffer by
    /// hand, which is what lets a portrait page be laid out in Slint rather than
    /// painted in Rust.
    ///
    /// `Rotate270` and not `Rotate90`: the panel's left edge is the page's top, which
    /// is the turn the framework's `turn_left` makes.
    #[test]
    fn the_renderers_quarter_turn_is_the_hand_turn() {
        let window = FlipperSlintPlatform::install_sized(PW, PH);
        let ui = Page::new().expect("the component");
        ui.show().expect("show");

        let portrait = render_frame(&window).expect("a portrait frame");
        let hand = turn_left(&portrait, PW as usize, PH as usize);

        window.request_redraw();
        let rotated =
            render_rotated(&window, RenderingRotation::Rotate270).expect("a turned frame");
        assert_eq!(rotated.len(), (PW * PH) as usize, "the turned frame is panel-shaped");

        let differing = hand.iter().zip(&rotated).filter(|(a, b)| a.0 != b.0).count();
        assert_eq!(differing, 0, "Rotate270 is not the framework's own turn");
    }

    /// And the turn is the one the device's orientation means: the page's top edge
    /// becomes the panel's left edge, and its left edge becomes the panel's bottom.
    ///
    /// That is what `rotate = "left"` in a manifest promises -- the panel is read with
    /// its left edge up -- and it is the mapping `turn_left` documents as
    /// `(x, y) -> (y, W - 1 - x)`. Getting it backwards is how a page came out upside
    /// down once, so the coordinates are checked rather than the shape alone.
    #[test]
    fn a_turned_page_maps_its_top_edge_onto_the_panels_left() {
        let window = FlipperSlintPlatform::install_sized(PW, PH);
        let ui = Page::new().expect("the component");
        ui.show().expect("show");
        let panel = render_rotated(&window, RenderingRotation::Rotate270).expect("a frame");

        let inked = |x0: usize, y0: usize, w: usize, h: usize| -> usize {
            (y0..y0 + h)
                .flat_map(|y| (x0..x0 + w).map(move |x| (x, y)))
                .filter(|&(x, y)| panel[y * PANEL_W as usize + x].0 < 128)
                .count()
        };

        // The bar stands at page x=4..9, y=4..43. Under (x, y) -> (y, 143 - x) it lies
        // along the panel's bottom: columns 4..43, rows 134..139.
        assert!(
            inked(4, 134, 40, 6) > 200,
            "the bar is not along the panel's bottom edge, so the turn is not the one rotate=left means"
        );
        assert_eq!(inked(4, 0, 40, 6), 0, "and it is not along the top");
        assert_eq!(inked(60, 100, 40, 40), 0, "nor in the middle");
    }

    slint::slint! {
        export component Portrait inherits Window {
            width: 144px; // not-a-panel-dimension: the page's, which the tests state
            height: 256px; // not-a-panel-dimension: and the panel's long side, named
            background: #ffffff;  // not-a-raw-colour: a fixture of known black on white, so the frame can be counted
            Rectangle { x: 140px; y: 0px; width: 4px; height: 256px; background: #000000; } // not-a-panel-dimension: the page's edge  // not-a-raw-colour: a fixture of known black on white, so the frame can be counted
        }
    }

    /// A strip down the page's right edge is along the panel's top edge once the
    /// frame is turned left, which is the whole point of turning it.
    #[test]
    fn a_rendered_page_comes_out_panel_shaped_and_turned() {
        let window = FlipperSlintPlatform::install_sized(PW, PH);
        let ui = Portrait::new().expect("the component");
        ui.show().expect("show");

        let panel = render_rotated(&window, RenderingRotation::Rotate270).expect("a turned frame");
        assert_eq!(panel.len(), (PW * PH) as usize, "the turned frame is the panel's");

        // The page's right edge, x = 143, is the panel's top row after a turn.
        let top: usize = (0..PANEL_W as usize).filter(|&x| panel[x].0 < 128).count();
        let row = PANEL_W as usize * 128;
        let middle: usize = (row..row + PANEL_W as usize).filter(|&i| panel[i].0 < 128).count();
        assert!(top > 200, "only {top} of the panel's top row is inked");
        assert_eq!(middle, 0, "and the page's middle is not");
    }
}
