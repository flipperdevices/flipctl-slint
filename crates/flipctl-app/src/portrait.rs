//! A page drawn the long way up, for an app whose design is portrait.
//!
//! The panel is 256 by 144. A portrait app draws 144 by 256 and is read with the device
//! turned, which the manifest declares as `rotate = "left"` (or `"right"`). flipctl turns
//! the frame on its way to the glass; what this module does is the app's half: the keys,
//! the status bar, and the turn done headlessly for the app's own tests.
//!
//! **The turn is the renderer's, not the app's.** Slint's software renderer reports
//! `supports_transformations() == false` and drops `transform-rotation` without a word,
//! so a page turned in the markup draws upright text over turned shapes -- on the device
//! as well as in a test. `SoftwareRenderer::set_rendering_rotation` does turn the whole
//! frame, glyphs included, and
//! [`render_rotated`](flipper_ui::slint_render::render_rotated) is the one call that does
//! it. An app never turns a buffer itself.
//!
//! **The status bar is flipctl's.** With `status = true` in the manifest flipctl paints
//! the real bar along the edge that is the page's top, so a portrait page reserves
//! [`STRIP`] rows there and draws nothing in them. That is the same bar, in the same font,
//! as everywhere else on the device; an app that drew its own would be a copy that drifts.
//!
//! ```ignore
//! // The page, in ui/app.slint:
//! //   import { PortraitShell, PanelStatus } from "@app/shell.slint";
//! //   export component AppWindow inherits PortraitShell { ... }
//! //
//! // build.rs
//! fn main() {
//!     flipctl_app::build::compile("ui/app.slint");
//! }
//! ```
//!
//! ```ignore
//! slint::include_modules!();
//!
//! fn main() -> Result<(), slint::PlatformError> {
//!     let ui = AppWindow::new()?;
//!     let _keys = flipctl_app::portrait_attach!(ui, PanelStatus, AppKeys, |key| {
//!         // what the key does; `false` leaves it to the defaults
//!         true
//!     });
//!     ui.run()
//! }
//! ```

use std::time::Duration;

use flipper_ui::status::{Status, StatusSource};
use flipper_ui::theme::{metric, PANEL_H, PANEL_W};
use flipper_ui::Rotate;

/// The rows a portrait page loses to flipctl's status bar, measured in its own
/// portrait rows.
///
/// This is the token, and it was checked against the glass rather than assumed: flipctl
/// draws the strip `STATUS_BAR_H` rows tall and `paint_status` maps it onto the edge that
/// is the page's top, where it lands on rows 0..12 -- the battery text on 4..10 and the
/// bar's own bottom rule on 12. An earlier version of this port carried 21, measured off
/// a page that had drawn its own bar; the token is right and a page that started at 21
/// left five rows of nothing under the bar.
pub const STRIP: i32 = metric::STATUS_BAR_H as i32;

/// The page's width, which is the panel's short side.
pub const W: i32 = PANEL_H as i32;

/// The page's height, which is the panel's long side.
pub const H: i32 = PANEL_W as i32;

/// The first row a page may use: the status bar's strip, plus the one row of air a page
/// leaves under the bar.
pub const TOP: i32 = STRIP + 1;

/// Where the panel's five soft keys are, in the page's own frame.
///
/// This is the part of a portrait page that surprises everybody, so it is spelled out. The
/// keys are physically along the panel's bottom edge, 256 long and `button_h` deep. A page
/// turned with `rotate = "left"` maps its own `(x, y)` to panel `(y, W - 1 - x)`, so the
/// panel's bottom rows are the page's **left** columns: the soft row runs down the page's
/// left edge, not across its bottom.
///
/// flipctl paints only the status bar over an app's frame; the soft row is the app's to
/// draw, and a portrait page cannot: a label there would be lying on its side, and the
/// pixel fonts only exist upright (`transform-rotation` is dropped by the software
/// renderer). So a portrait app draws no labels and relies on the keys' silkscreen, which
/// is what the hardware itself does. [`SOFT_W`] is here so a page that finds something
/// worth putting there knows where it would go.
pub const SOFT_W: i32 = metric::BUTTON_H as i32;

/// The last row a page may use.
///
/// Nothing is painted along the page's bottom edge -- that is the panel's right edge, which
/// flipctl leaves alone -- so this is one row of margin rather than a reservation.
pub const BOTTOM: i32 = H - 1;

/// The renderer's rotation for a manifest's `rotate` field.
///
/// `left` is `Rotate270`: the panel's left edge becomes the page's top, which is the
/// direction the framework's own `turn_left` makes and the one `paint_status` in flipctl
/// assumes. `right` is the other side up. Getting the two the wrong way round is how a
/// page came out upside down once.
pub fn rotation(turn: Rotate) -> slint::platform::software_renderer::RenderingRotation {
    use slint::platform::software_renderer::RenderingRotation;
    match turn {
        Rotate::Left => RenderingRotation::Rotate270,
        Rotate::Right => RenderingRotation::Rotate90,
        Rotate::None => RenderingRotation::NoRotation,
    }
}

/// The keys and the status bar, wired to a portrait page of the app's own: see
/// [`crate::attach`].
///
/// The same contract as [`crate::attach!`]: the page inherits `PortraitShell` from
/// `@app/shell.slint`, binds `AppKeys.pressed_slot` and `AppKeys.arrow_pressed` into its
/// bodies, and re-exports `AppKeys` and `PanelStatus`.
///
/// A page that leaves `status = true` to flipctl draws no bar, so filling `PanelStatus`
/// every second is waste rather than harm; `status_timer` is for the page that draws one.
#[macro_export]
macro_rules! portrait_attach {
    ($ui:expr, $status:ident, $keys:ident, $handler:expr) => {{
        $crate::attach!($ui, $status, $keys, $handler)
    }};
}

/// Poll the status bar onto a page that draws its own.
///
/// Keep the result: dropping it stops the poll. An app with `status = true` in its
/// manifest has flipctl painting the bar over its frame and needs none of this.
pub fn status_timer(set: impl Fn(&Status) + 'static) -> slint::Timer {
    let mut source = StatusSource::new(Duration::from_secs(2));
    set(&source.current());
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        if let Some(now) = source.poll() {
            set(&now);
        }
    });
    timer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_is_the_panel_read_the_long_way() {
        // The design is 144 by 256 and the panel is 256 by 144: one rectangle read two
        // ways, and a page that had it the other way round would not fit the frame it is
        // turned into.
        // The page is the panel turned on its side: both halves, not one.
        assert_eq!((W, H), (144, 256)); // not-a-panel-dimension: the pairing is the thing asserted
        assert_eq!((W, H), (i32::from(PANEL_H), i32::from(PANEL_W)));
    }

    #[test]
    fn the_page_keeps_out_of_the_status_bar() {
        assert_eq!(TOP, STRIP + 1, "a page has one row of air under the bar");
        assert!(TOP < BOTTOM, "and room between them to draw in");
        assert_eq!(BOTTOM, H - 1, "nothing is painted along the page's bottom edge");
    }

    /// The soft keys are on the panel's bottom edge, which a turned page sees as its own
    /// left columns. Asserted because getting it the other way round silently moves a page's
    /// content under the user's thumb, and the mapping is not obvious from either side.
    #[test]
    fn the_soft_keys_are_down_the_pages_left_edge() {
        // panel (X, Y) <- page (143 - Y, X); the keys are panel Y = 143..130.
        let to_panel = |x: i32, y: i32| (y, (W - 1) - x);
        assert_eq!(to_panel(0, 0), (0, W - 1), "the page's top-left is the panel's bottom-left");
        assert_eq!(to_panel(W - 1, 0), (0, 0), "and its top-right the panel's top-left");
        // The page's left column lands on the panel's bottom row, which is where the keys are.
        assert_eq!(to_panel(0, 100).0, 100);
        assert_eq!(to_panel(0, 100).1, W - 1);
        assert_eq!(SOFT_W, metric::BUTTON_H as i32);
    }

    #[test]
    fn the_turn_matches_what_the_manifest_promises() {
        use slint::platform::software_renderer::RenderingRotation;
        assert_eq!(rotation(Rotate::Left), RenderingRotation::Rotate270);
        assert_eq!(rotation(Rotate::Right), RenderingRotation::Rotate90);
        assert_eq!(rotation(Rotate::None), RenderingRotation::NoRotation);
    }
}
