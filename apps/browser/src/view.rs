//! Where the page sits on the panel, and what the pad does to that.
//!
//! Ported from the prototype's js/apps/browser.js. The page is fitted to the panel
//! at x1 and magnified around a point of it, the pan point, which is also where the
//! cursor is. The view centres on that point until it runs into an edge of the page,
//! and from there the view stays put and the cursor slides towards the edge instead.
//!
//! The page has the whole panel, except while the strip is up: that is only at the
//! top of the site, and then the page is pushed down under it rather than covered,
//! so nothing at its top is hidden. `bar` is how far down it is pushed.
//!
//! Everything here is page pixels in and panel pixels out, and nothing draws.

use flipctl_app::theme::{PANEL_H, PANEL_W};
use flipctl_app::Touch;

/// How far the strip pushes the page down while it is up.
pub const TOP: i32 = crate::strip::HEIGHT;
const W: f32 = PANEL_W as f32;
const H: f32 = PANEL_H as f32;

/// x1 is the whole page fitted to the panel.
pub const ZOOM_MIN: f32 = 1.0;
pub const ZOOM_MAX: f32 = 8.0;
/// Finger travel for one doubling, in raw pad units. A full swipe is about 1000.
const TP_UNITS_PER_DOUBLING: f32 = 300.0;
/// The factor's granularity.
const ZOOM_QUANTUM: f32 = 0.05;
/// A tick of the motor every this much of factor.
const ZOOM_HAPTIC_STEP: f32 = 0.1;
/// Raw pad units per panel pixel of cursor travel.
const TP_CURSOR_DIVIDER: f32 = 2.0;
/// Panel pixels the cursor moves per press of the arrows.
const NUDGE_PX: f32 = 16.0;
/// How far past the top of the page a drag has to push, in panel pixels, before
/// it counts as asking for the strip rather than as a drag that reached the edge.
const PUSH_PX: f32 = 12.0;

/// The crosshair: arm length from the centre, and the hole in the middle.
const CURSOR_ARM: i32 = 4;
const CURSOR_GAP: i32 = 1;

/// The minimap's width. It sits in the bottom-right corner, in by the width of its
/// outline, so all four sides of the outline are on the panel.
pub const MINI_W: i32 = 64;
const MINI_OUTLINE: i32 = 3;

/// A page's size in its own pixels.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct Size {
    pub w: f32,
    pub h: f32,
}

/// Where the scaled page lands: panel pixels per page pixel, and the panel position
/// of its top-left corner.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct Placement {
    pub s: f32,
    pub dx: i32,
    pub dy: i32,
}

/// One filled rectangle, on the panel.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Mark {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub black: bool,
}

impl Mark {
    const fn new(x: i32, y: i32, w: i32, h: i32, black: bool) -> Self {
        Self { x, y, w, h, black }
    }
}

/// The minimap: where the thumbnail goes, and the marks drawn around and on it.
#[derive(Clone, PartialEq, Debug)]
pub struct Minimap {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Round the map, from the outside in: white, black, and a pixel of white
    /// between the black and the map. The two outer lines make its edge read on a
    /// dark page and a light one; the white inside keeps the view's frame off the
    /// black, since at the page's edge the two would otherwise merge into one thick
    /// line. Drawn in this order, under the map.
    pub border: [Mark; 3],
    /// What the panel shows, framed black with white just inside it, for the same
    /// reason as the outline: a black frame alone vanishes on a dark page. The white
    /// comes first, so the black is drawn over it where the two meet.
    pub frame: Vec<Mark>,
}

/// What a report asks of the motor.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Buzz {
    None,
    /// A factor step crossed.
    Tick,
    /// A limit hit, once per push past it.
    Thump,
}

/// What the finger is doing, anchored where it went down. Re-anchored whenever PTT
/// changes under a finger, so neither the factor nor the cursor jumps.
#[derive(Copy, Clone, Debug)]
enum Gesture {
    Idle,
    Pan { tp: (i32, i32), at: (f32, f32) },
    Zoom { x: i32, zoom: f32, edge: bool },
}

#[derive(Clone, Debug)]
pub struct View {
    pub zoom: f32,
    /// The page point the view centres on and zooms around, in page pixels.
    pub pan: (f32, f32),
    gesture: Gesture,
    /// A drag pushed on past the top of the page.
    pushed_up: bool,
    /// How far the page is pushed down: the strip's height while it is up, else 0.
    pub bar: i32,
}

impl View {
    /// Fitted, with the cursor at the centre of the page.
    pub fn new(page: Size) -> Self {
        Self {
            zoom: ZOOM_MIN,
            pan: (page.w / 2.0, page.h / 2.0),
            gesture: Gesture::Idle,
            pushed_up: false,
            bar: 0,
        }
    }

    /// A page of another size arrived: the pan point is kept where it is on the
    /// page, and pulled back inside it if the page shrank.
    pub fn resize(&mut self, page: Size) {
        self.pan = (self.pan.0.clamp(0.0, page.w), self.pan.1.clamp(0.0, page.h));
        self.gesture = Gesture::Idle;
    }

    /// Panel pixels per page pixel.
    pub fn scale(&self, page: Size) -> f32 {
        (W / page.w).min(H / page.h) * self.zoom
    }

    pub fn place(&self, page: Size) -> Placement {
        let s = self.scale(page);
        let (dw, dh) = ((page.w * s).round(), (page.h * s).round());
        let axis = |d: f32, panel: f32, pan: f32| -> i32 {
            if d <= panel {
                ((panel - d) / 2.0).floor() as i32
            } else {
                (panel / 2.0 - pan * s).round().clamp(panel - d, 0.0) as i32
            }
        };
        Placement { s, dx: axis(dw, W, self.pan.0), dy: self.bar + axis(dh, H, self.pan.1) }
    }

    /// Whether the view reaches the page's top edge, which with the page scrolled
    /// to its top is the top of the site, where the strip belongs.
    pub fn shows_top(&self, page: Size) -> bool {
        self.place(page).dy - self.bar >= 0
    }

    /// The cursor's centre on the panel: where its page point lands, kept far enough
    /// in that the whole glyph and its halo are on the page, clear of the strip.
    pub fn cursor(&self, page: Size) -> (i32, i32) {
        let p = self.place(page);
        let a = CURSOR_ARM;
        let x = (p.dx as f32 + self.pan.0 * p.s).round() as i32;
        let y = (p.dy as f32 + self.pan.1 * p.s).round() as i32;
        (x.clamp(a + 1, PANEL_W as i32 - 2 - a), y.clamp(self.bar + a + 1, PANEL_H as i32 - 2 - a))
    }

    /// The crosshair, halo first: the same arms a pixel fatter on each side, in
    /// white, so it reads on any page.
    pub fn cursor_marks(&self, page: Size) -> Vec<Mark> {
        let (x, y) = self.cursor(page);
        let (a, g) = (CURSOR_ARM, CURSOR_GAP);
        vec![
            Mark::new(x - a - 1, y - 1, a - g + 1, 3, false),
            Mark::new(x + g, y - 1, a - g + 1, 3, false),
            Mark::new(x - 1, y - a - 1, 3, a - g + 1, false),
            Mark::new(x - 1, y + g, 3, a - g + 1, false),
            Mark::new(x - a, y, a - g, 1, true),
            Mark::new(x + g + 1, y, a - g, 1, true),
            Mark::new(x, y - a, 1, a - g, true),
            Mark::new(x, y + g + 1, 1, a - g, true),
        ]
    }

    /// Only once something is cropped: at x1 the whole page is already on screen.
    pub fn minimap(&self, page: Size) -> Option<Minimap> {
        if self.zoom <= ZOOM_MIN {
            return None;
        }
        let p = self.place(page);
        let w = MINI_W;
        let h = ((page.h * w as f32 / page.w).round() as i32).max(1);
        let (x, y) = (PANEL_W as i32 - MINI_OUTLINE - w, PANEL_H as i32 - MINI_OUTLINE - h);
        let ms = w as f32 / page.w;

        // What the panel shows of the page, in page pixels: the scaled page cut to
        // the panel, and to below the strip while it is up.
        let (dw, dh) = ((page.w * p.s).round() as i32, (page.h * p.s).round() as i32);
        let (left, right) = (p.dx.max(0), (p.dx + dw).min(PANEL_W as i32));
        let (top, bottom) = (p.dy.max(self.bar), (p.dy + dh).min(PANEL_H as i32));
        let vx = (left - p.dx) as f32 / p.s;
        let vy = (top - p.dy) as f32 / p.s;
        let vw = (right - left) as f32 / p.s;
        let vh = (bottom - top) as f32 / p.s;
        let rx = x + (vx * ms).round() as i32;
        let ry = y + (vy * ms).round() as i32;
        let rw = ((vw * ms).round() as i32).max(3).min(x + w - rx);
        let rh = ((vh * ms).round() as i32).max(3).min(y + h - ry);
        Some(Minimap {
            x,
            y,
            w,
            h,
            border: [
                Mark::new(x - 3, y - 3, w + 6, h + 6, false),
                Mark::new(x - 2, y - 2, w + 4, h + 4, true),
                Mark::new(x - 1, y - 1, w + 2, h + 2, false),
            ],
            frame: [false, true]
                .into_iter()
                .flat_map(|black| {
                    let i = i32::from(!black);
                    let (x, y, w, h) = (rx + i, ry + i, (rw - 2 * i).max(1), (rh - 2 * i).max(1));
                    [
                        Mark::new(x, y, w, 1, black),
                        Mark::new(x, y + h - 1, w, 1, black),
                        Mark::new(x, y, 1, h, black),
                        Mark::new(x + w - 1, y, 1, h, black),
                    ]
                })
                .collect(),
        })
    }

    /// The factor as the pill shows it.
    pub fn label(&self) -> String {
        format!("x{:.1}", self.zoom)
    }

    /// A pad report. With PTT held the finger's horizontal travel is the zoom,
    /// right to magnify; without it the finger drags the cursor. Returns whether anything
    /// on screen moved, and what the motor should say.
    pub fn touch(&mut self, t: Touch, ptt: bool, page: Size) -> (bool, Buzz) {
        if !t.down {
            self.gesture = Gesture::Idle;
            return (false, Buzz::None);
        }
        if !ptt {
            let Gesture::Pan { tp, at } = self.gesture else {
                self.gesture = Gesture::Pan { tp: (t.x, t.y), at: self.pan };
                return (false, Buzz::None);
            };
            let s = self.scale(page);
            let x = at.0 + (t.x - tp.0) as f32 / TP_CURSOR_DIVIDER / s;
            let y = at.1 + (t.y - tp.1) as f32 / TP_CURSOR_DIVIDER / s;
            self.pushed_up |= y < -PUSH_PX / s;
            let pan = (x.clamp(0.0, page.w), y.clamp(0.0, page.h));
            let moved = pan != self.pan;
            self.pan = pan;
            return (moved, Buzz::None);
        }

        let Gesture::Zoom { x, zoom, edge } = self.gesture else {
            self.gesture = Gesture::Zoom { x: t.x, zoom: self.zoom, edge: false };
            return (false, Buzz::None);
        };
        let raw = zoom * 2f32.powf((t.x - x) as f32 / TP_UNITS_PER_DOUBLING);
        let z = (raw / ZOOM_QUANTUM).round() * ZOOM_QUANTUM;
        let clamped = z.clamp(ZOOM_MIN, ZOOM_MAX);
        let prev = self.zoom;
        self.zoom = clamped;

        let past = z < ZOOM_MIN || z > ZOOM_MAX;
        let buzz = if past {
            if edge {
                Buzz::None
            } else {
                Buzz::Thump
            }
        } else if (clamped / ZOOM_HAPTIC_STEP).round() != (prev / ZOOM_HAPTIC_STEP).round() {
            Buzz::Tick
        } else {
            Buzz::None
        };
        self.gesture = Gesture::Zoom { x, zoom, edge: past };
        (clamped != prev, buzz)
    }

    /// Whether a drag pushed on past the top of the page since the last time this
    /// was asked. The drag ends with it, so a finger that stays down starts afresh.
    pub fn take_pushed_up(&mut self) -> bool {
        let pushed = std::mem::take(&mut self.pushed_up);
        if pushed {
            self.gesture = Gesture::Idle;
        }
        pushed
    }

    /// PTT went down or up. A finger that stays on the pad is re-anchored by its
    /// next report, since the gesture it was part of has ended.
    pub fn ptt(&mut self) {
        self.gesture = Gesture::Idle;
    }

    /// How far the page scrolls when the arrows push past its edge, in page pixels:
    /// most of what is visible, so the next screenful overlaps the last by a little.
    pub fn scroll_step(&self, page: Size) -> (f32, f32) {
        let s = self.scale(page);
        ((W / s).min(page.w) * 0.8, ((H - self.bar as f32) / s).min(page.h) * 0.8)
    }

    /// A press of the arrows, `dir` one of the four unit steps. Moves the cursor as
    /// the pad does, a fixed distance on the panel whatever the zoom. With the cursor
    /// already against the page's edge in that direction there is nowhere left to
    /// move it, so the answer is how far to scroll the page instead.
    pub fn nudge(&mut self, dir: (i32, i32), page: Size) -> Option<(f32, f32)> {
        let step = NUDGE_PX / self.scale(page);
        let pinned = |at: f32, d: i32, len: f32| (d < 0 && at <= 0.0) || (d > 0 && at >= len);
        if pinned(self.pan.0, dir.0, page.w) || pinned(self.pan.1, dir.1, page.h) {
            let (sx, sy) = self.scroll_step(page);
            return Some((dir.0 as f32 * sx, dir.1 as f32 * sy));
        }
        self.pan = (
            (self.pan.0 + dir.0 as f32 * step).clamp(0.0, page.w),
            (self.pan.1 + dir.1 as f32 * step).clamp(0.0, page.h),
        );
        self.gesture = Gesture::Idle;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seven panels each way: x7 is one page pixel to one panel pixel.
    const PAGE: Size = Size { w: 1792.0, h: 1008.0 };

    fn touch(x: i32, y: i32) -> Touch {
        Touch { x, y, down: true }
    }

    #[test]
    fn at_x1_the_page_fills_the_panel() {
        let v = View::new(PAGE);
        let p = v.place(PAGE);
        assert_eq!(p, Placement { s: 1.0 / 7.0, dx: 0, dy: 0 });
        assert_eq!(v.cursor(PAGE), (128, 72));
        assert!(v.minimap(PAGE).is_none());
    }

    #[test]
    fn a_page_of_another_shape_is_centred() {
        let tall = Size { w: 1008.0, h: 1008.0 };
        let p = View::new(tall).place(tall);
        assert_eq!((p.dx, p.dy), (56, 0));
    }

    #[test]
    fn zoomed_in_the_view_centres_on_the_pan_point() {
        let mut v = View::new(PAGE);
        v.zoom = 7.0;
        v.pan = (900.0, 500.0);
        let p = v.place(PAGE);
        assert_eq!((p.dx, p.dy), (128 - 900, 72 - 500));
        assert_eq!(v.cursor(PAGE), (128, 72));
    }

    #[test]
    fn at_an_edge_the_view_stops_and_the_cursor_slides() {
        let mut v = View::new(PAGE);
        v.zoom = 7.0;
        v.pan = (10.0, 1000.0);
        let p = v.place(PAGE);
        assert_eq!(p.dx, 0, "the view left the page's left edge");
        assert_eq!(p.dy, PANEL_H as i32 - 1008, "the view left the page's bottom edge");
        // The point is 10px in from the left and 8px up from the bottom.
        assert_eq!(v.cursor(PAGE), (10, PANEL_H as i32 - 8));
        // At the corner itself the glyph is kept whole on the panel.
        v.pan = (1792.0, 1008.0);
        assert_eq!(
            v.cursor(PAGE),
            (PANEL_W as i32 - 2 - CURSOR_ARM, PANEL_H as i32 - 2 - CURSOR_ARM)
        );
        v.pan = (0.0, 0.0);
        assert_eq!(v.cursor(PAGE), (CURSOR_ARM + 1, CURSOR_ARM + 1));
        // And with the strip up it is kept clear of it.
        v.bar = TOP;
        assert_eq!(v.cursor(PAGE), (CURSOR_ARM + 1, TOP + CURSOR_ARM + 1));
    }

    /// The strip pushes the page down rather than covering it, and belongs only to
    /// the top of the page: zoomed in and panned down, the view no longer shows it.
    #[test]
    fn the_strip_pushes_the_page_down_and_only_at_its_top() {
        let mut v = View::new(PAGE);
        assert!(v.shows_top(PAGE), "x1 shows the whole page");
        v.bar = TOP;
        assert_eq!(v.place(PAGE), Placement { s: 1.0 / 7.0, dx: 0, dy: TOP }, "not pushed down");
        assert!(v.shows_top(PAGE));
        // The minimap frames what is left below the strip.
        v.zoom = 2.0;
        v.pan = (896.0, 0.0);
        assert!(v.shows_top(PAGE), "panned to the top");
        let m = v.minimap(PAGE).unwrap();
        let (s, visible) = (2.0 / 7.0, (PANEL_H as i32 - TOP) as f32);
        assert_eq!(m.frame[6].h, ((visible / s) * 36.0 / 1008.0).round() as i32);
        v.pan = (896.0, 504.0);
        assert!(!v.shows_top(PAGE), "panned down, and still claiming the top");
    }

    #[test]
    fn the_crosshair_has_a_hole_and_a_halo() {
        let v = View::new(PAGE);
        let marks = v.cursor_marks(PAGE);
        let (x, y) = v.cursor(PAGE);
        let covers = |px: i32, py: i32, black: bool| {
            marks.iter().any(|m| {
                m.black == black && px >= m.x && px < m.x + m.w && py >= m.y && py < m.y + m.h
            })
        };
        assert!(!covers(x, y, true), "the centre is inked");
        assert!(covers(x + 2, y, true) && covers(x + CURSOR_ARM, y, true));
        assert!(!covers(x + CURSOR_ARM + 1, y, true), "the arm is too long");
        assert!(!covers(x + 1, y, true) && !covers(x - 1, y, true), "the hole is filled");
        assert!(covers(x + 2, y + 1, false), "no halo beside the arm");
        // The prototype's halo runs one past the left and top arms and stops level
        // with the right and bottom ones. Kept, since it is what the design shows.
        assert!(covers(x - CURSOR_ARM - 1, y, false));
        assert!(!covers(x + CURSOR_ARM + 1, y, false));
    }

    #[test]
    fn ptt_and_a_swipe_to_the_right_zoom_in() {
        let mut v = View::new(PAGE);
        assert_eq!(v.touch(touch(300, 500), true, PAGE), (false, Buzz::None), "touch-down jumped");
        let (moved, buzz) = v.touch(touch(600, 500), true, PAGE);
        assert!(moved);
        assert_eq!(buzz, Buzz::Tick);
        assert!((v.zoom - 2.0).abs() < 1e-4, "300 units is a doubling, got {}", v.zoom);
        // Back left past x1: clamped, and one thump rather than one per report.
        assert_eq!(v.touch(touch(-100, 500), true, PAGE).1, Buzz::Thump);
        assert_eq!(v.zoom, ZOOM_MIN);
        assert_eq!(v.touch(touch(-200, 500), true, PAGE).1, Buzz::None);
        // Back inside the range re-arms it.
        v.touch(touch(400, 500), true, PAGE);
        assert_eq!(v.touch(touch(-200, 500), true, PAGE).1, Buzz::Thump);
    }

    #[test]
    fn lifting_and_touching_again_never_jumps_the_factor() {
        let mut v = View::new(PAGE);
        v.touch(touch(300, 0), true, PAGE);
        v.touch(touch(600, 0), true, PAGE);
        let zoom = v.zoom;
        v.touch(Touch { x: 600, y: 0, down: false }, true, PAGE);
        assert_eq!(v.touch(touch(0, 0), true, PAGE), (false, Buzz::None));
        assert_eq!(v.zoom, zoom);
    }

    #[test]
    fn without_ptt_the_finger_drags_the_cursor() {
        let mut v = View::new(PAGE);
        v.touch(touch(100, 100), false, PAGE);
        v.touch(touch(120, 100), false, PAGE);
        // 20 units is 10 panel pixels, which is 70 page pixels at x1.
        assert_eq!(v.pan, (896.0 + 70.0, 504.0));
        v.zoom = 7.0;
        v.touch(Touch { x: 120, y: 100, down: false }, false, PAGE);
        v.touch(touch(0, 0), false, PAGE);
        v.touch(touch(0, 20), false, PAGE);
        assert_eq!(v.pan.1, 504.0 + 10.0, "at x7 a panel pixel is a page pixel");
        // Never off the page.
        v.touch(touch(0, -100000), false, PAGE);
        assert_eq!(v.pan.1, 0.0);
    }

    #[test]
    fn a_drag_pushed_past_the_top_says_so_once() {
        let mut v = View::new(PAGE);
        v.pan.1 = 7.0;
        v.touch(touch(0, 100), false, PAGE);
        // 10 panel pixels up is 70 page pixels at x1: the edge, but not past it.
        v.touch(touch(0, 80), false, PAGE);
        assert_eq!(v.pan.1, 0.0);
        assert!(!v.take_pushed_up());
        v.touch(touch(0, 60), false, PAGE);
        assert!(v.take_pushed_up());
        assert!(!v.take_pushed_up(), "said twice");
    }

    #[test]
    fn pressing_ptt_mid_stroke_reanchors_rather_than_jumping() {
        let mut v = View::new(PAGE);
        v.touch(touch(0, 500), false, PAGE);
        v.touch(touch(0, 520), false, PAGE);
        v.ptt();
        let (zoom, pan) = (v.zoom, v.pan);
        v.touch(touch(0, 200), true, PAGE);
        assert_eq!((v.zoom, v.pan), (zoom, pan));
    }

    #[test]
    fn the_minimap_frames_what_the_panel_shows() {
        let mut v = View::new(PAGE);
        v.zoom = 2.0;
        let m = v.minimap(PAGE).expect("a minimap once zoomed");
        assert_eq!((m.w, m.h), (MINI_W, 36));
        // The whole outline on the panel, touching its right and bottom edges.
        let [white, _, _] = m.border;
        assert_eq!(white.x + white.w, PANEL_W as i32, "the outline runs off the right");
        assert_eq!(white.y + white.h, PANEL_H as i32, "the outline runs off the bottom");
        // Half the page each way, centred: 32x18 of the 64x36 map, from (16, 9).
        let top = m.frame[4];
        assert!(top.black);
        assert_eq!((top.x - m.x, top.y - m.y, top.w), (16, 9, 32));
        assert_eq!(m.frame[6].h, 18);
        // White one pixel inside the black.
        assert_eq!((m.frame[0].x, m.frame[0].y, m.frame[0].w), (top.x + 1, top.y + 1, top.w - 2));
        assert!(!m.frame[0].black);
        // White, black, then a pixel of white before the map, one pixel each.
        let [white, black, gap] = m.border;
        assert!(!white.black && black.black && !gap.black);
        assert_eq!((gap.x, gap.y, gap.w), (m.x - 1, m.y - 1, m.w + 2));
        assert_eq!((black.x, black.y, black.w), (m.x - 2, m.y - 2, m.w + 4));
        assert_eq!((white.x, white.y, white.w), (m.x - 3, m.y - 3, m.w + 6));
    }

    #[test]
    fn the_arrows_move_the_cursor_and_scroll_only_at_the_edge() {
        let mut v = View::new(PAGE);
        // 16 panel pixels, which is 112 page pixels at x1.
        assert_eq!(v.nudge((1, 0), PAGE), None);
        assert_eq!(v.pan, (896.0 + 112.0, 504.0));
        v.zoom = 7.0;
        assert_eq!(v.nudge((0, -1), PAGE), None);
        assert_eq!(v.pan.1, 504.0 - 16.0, "at x7 a panel pixel is a page pixel");
        // Up to the top edge, and one more press scrolls rather than moves.
        v.pan.1 = 5.0;
        assert_eq!(v.nudge((0, -1), PAGE), None);
        assert_eq!(v.pan.1, 0.0);
        let (x, y) = v.nudge((0, -1), PAGE).expect("a scroll at the edge");
        assert!(x == 0.0 && y < 0.0);
        assert_eq!(v.pan.1, 0.0);
        // The other way is still a move.
        assert_eq!(v.nudge((0, 1), PAGE), None);
        assert_eq!(v.pan.1, 16.0);
    }

    #[test]
    fn scrolling_steps_most_of_a_screen() {
        let mut v = View::new(PAGE);
        let (x, y) = v.scroll_step(PAGE);
        assert!((x - 1792.0 * 0.8).abs() < 1e-2 && (y - 1008.0 * 0.8).abs() < 1e-2);
        v.zoom = 7.0;
        let (x, y) = v.scroll_step(PAGE);
        assert!((x - W * 0.8).abs() < 1e-3 && (y - H * 0.8).abs() < 1e-3);
    }
}
