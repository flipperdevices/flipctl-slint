//! The decoder's frame on the panel: where it goes, how it is shrunk, and how its greys
//! become the panel's.
//!
//! The frame is 768x576 RGB and the panel 256x144 grey. The picture has the whole
//! panel: the soft keys are drawn over its bottom 14px, and can be hidden. Each panel pixel is the mean
//! luma of the block of frame pixels under it, so a line of the picture is never lost
//! between samples the way it would be by picking one.
//!
//! The panel is black ink on an orange ground, not a backlit screen, and no fixed curve
//! suits every camera. So the lumas go through [`Levels`], contrast about mid-grey and
//! then brightness, which the Setup's tuner sets by eye on the live picture.
//!
//! The panel's greys are a fixed set of tokens, so the picture is dithered onto them
//! with an ordered 4x4 Bayer matrix. Ordered rather than error diffusion on purpose:
//! the same grey dithers to the same pattern every frame, so a still scene stays still
//! instead of crawling with noise.

use flipctl_app::paint::Surface;
use flipctl_app::pixel::Gray8;
use flipctl_app::theme::{color, PANEL_H, PANEL_W};

/// How the 4:3 picture fills a 16:9 panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// 4:3, with black bars at the sides that carry the channel and the lock.
    Box,
    /// Stretched across the whole width.
    Fill,
}

/// What the picture's greys become.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Dithered across six token greys.
    Tones,
    /// Dithered to black and white.
    Bits,
    /// The decoder's own greys, undithered: off the token palette, but on this panel the
    /// one that reads best for live video, so the default.
    Grey,
}

/// Six token greys, dark to light, roughly evenly spaced. The ones between are close
/// enough to a neighbour to add nothing to a dither.
const TONES: [Gray8; 6] = [color::BLACK, color::CHEVRON_FLASH, color::CHEVRON_BAR, color::STATUS_DIM, color::DIVIDER, color::WHITE];
const BITS: [Gray8; 2] = [color::BLACK, color::WHITE];

const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// The panel rows the picture takes: all of them.
pub const VIDEO_Y: i32 = 0;
pub const VIDEO_H: i32 = PANEL_H as i32;

/// Contrast and brightness, what the lumas go through before the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Levels {
    /// Percent, about mid-grey: 100 leaves the picture as it is.
    pub contrast: i32,
    /// Greys added after contrast.
    pub brightness: i32,
}

impl Levels {
    /// 200%: the decoder's greys are flat on this panel.
    pub const DEFAULT: Levels = Levels { contrast: 200, brightness: 0 };
    /// Least, most and one step of each.
    pub const CONTRAST: (i32, i32, i32) = (20, 400, 10);
    pub const BRIGHTNESS: (i32, i32, i32) = (-128, 128, 8);

    /// One output grey per input grey.
    pub fn table(self) -> [u8; 256] {
        std::array::from_fn(|i| ((i as i32 - 128) * self.contrast / 100 + 128 + self.brightness).clamp(0, 255) as u8)
    }

    /// Move contrast `by` steps, within its range.
    pub fn contrast_by(&mut self, by: i32) {
        let (lo, hi, step) = Self::CONTRAST;
        self.contrast = (self.contrast + by * step).clamp(lo, hi);
    }

    /// Move brightness `by` steps, within its range.
    pub fn brightness_by(&mut self, by: i32) {
        let (lo, hi, step) = Self::BRIGHTNESS;
        self.brightness = (self.brightness + by * step).clamp(lo, hi);
    }
}

/// Where the picture goes on the panel, as x, y, w, h.
pub fn area(fit: Fit) -> (i32, i32, i32, i32) {
    let full = i32::from(PANEL_W);
    match fit {
        Fit::Fill => (0, VIDEO_Y, full, VIDEO_H),
        Fit::Box => {
            let w = (VIDEO_H * 4 + 1) / 3;
            ((full - w) / 2, VIDEO_Y, w, VIDEO_H)
        }
    }
}

/// Shrink an RGB frame of `src_w` x `src_h` into `(w, h)` mean lumas, row by row.
pub fn shrink(rgb: &[u8], src_w: usize, src_h: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h);
    for oy in 0..h {
        let (y0, y1) = span(oy, h, src_h);
        for ox in 0..w {
            let (x0, x1) = span(ox, w, src_w);
            let mut sum = 0u32;
            for y in y0..y1 {
                let row = &rgb[(y * src_w + x0) * 3..(y * src_w + x1) * 3];
                for px in row.chunks_exact(3) {
                    // BT.601 luma, in integers
                    sum += (77 * u32::from(px[0]) + 150 * u32::from(px[1]) + 29 * u32::from(px[2])) >> 8;
                }
            }
            out.push((sum / ((y1 - y0) * (x1 - x0)) as u32) as u8);
        }
    }
    out
}

/// The source pixels behind output pixel `i` of `n`, never empty.
fn span(i: usize, n: usize, src: usize) -> (usize, usize) {
    let a = i * src / n;
    let b = ((i + 1) * src / n).max(a + 1).min(src);
    (a.min(src - 1), b)
}

/// One grey onto the palette, at panel position `x`, `y`.
pub fn dither(v: u8, x: i32, y: i32, tone: Tone) -> Gray8 {
    let levels: &[Gray8] = match tone {
        Tone::Grey => return Gray8(v),
        Tone::Tones => &TONES,
        Tone::Bits => &BITS,
    };
    // The pair of levels around v, and how far v is from the darker towards the lighter
    let hi = levels.iter().position(|l| l.0 >= v).unwrap_or(levels.len() - 1).max(1);
    let (a, b) = (levels[hi - 1].0, levels[hi].0);
    let t = u32::from(v.saturating_sub(a)) * 16;
    let threshold = u32::from(BAYER[(y & 3) as usize][(x & 3) as usize]) * u32::from(b - a) + u32::from(b - a) / 2;
    if t > threshold {
        levels[hi]
    } else {
        levels[hi - 1]
    }
}

/// A decoded frame's lumas at the size `fit` draws it. The decoder does this once per
/// field on a fast core, where it costs the window nothing.
pub fn lumas_for(rgb: &[u8], fit: Fit) -> Vec<u8> {
    let started = crate::lag::now();
    let (_, _, w, h) = area(fit);
    let lumas = shrink(rgb, crate::dsp::video::WIDTH, crate::dsp::video::HEIGHT, w as usize, h as usize);
    crate::lag::cost(0, started);
    lumas
}

/// Paint `lumas`, made by [`lumas_for`] for the same `fit`, into its area on `surface`,
/// black around it.
pub fn paint(surface: &mut Surface, lumas: &[u8], fit: Fit, tone: Tone, levels: Levels) {
    surface.rect(0, 0, i32::from(PANEL_W), VIDEO_Y + VIDEO_H, color::BLACK);
    let (x, y, w, _) = area(fit);
    let started = crate::lag::now();
    let table = levels.table();
    for (i, &v) in lumas.iter().map(|v| &table[usize::from(*v)]).enumerate() {
        let (px, py) = (x + (i % w as usize) as i32, y + (i / w as usize) as i32);
        surface.pixel(px, py, dither(v, px, py, tone));
    }
    crate::lag::cost(1, started);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_box_is_four_by_three_and_centred() {
        let (x, y, w, h) = area(Fit::Box);
        assert_eq!((y, h), (0, 144));
        assert_eq!(w, 192);
        assert_eq!(x * 2 + w, i32::from(PANEL_W));
        assert_eq!(area(Fit::Fill), (0, 0, 256, 144));
    }

    #[test]
    fn shrinking_takes_the_mean_of_each_block() {
        // 4x2, left half black and right half white, into 2x1
        let mut rgb = vec![0u8; 4 * 2 * 3];
        for y in 0..2 {
            for x in 2..4 {
                rgb[(y * 4 + x) * 3..(y * 4 + x) * 3 + 3].fill(255);
            }
        }
        assert_eq!(shrink(&rgb, 4, 2, 2, 1), vec![0, 255]);
        // Every source pixel is read once when the sizes do not divide
        let grey = vec![100u8; 768 * 576 * 3];
        assert!(shrink(&grey, 768, 576, 173, 130).iter().all(|&v| v == 100));
    }

    #[test]
    fn levels_stretch_about_mid_grey_then_add() {
        let t = Levels { contrast: 100, brightness: 0 }.table();
        assert!(t.iter().enumerate().all(|(i, &v)| v as usize == i), "100% changes nothing");
        let t = Levels { contrast: 200, brightness: 0 }.table();
        assert_eq!((t[0], t[64], t[128], t[192], t[255]), (0, 0, 128, 255, 255));
        let t = Levels { contrast: 100, brightness: -8 }.table();
        assert_eq!((t[0], t[100], t[255]), (0, 92, 247));
        let mut l = Levels::DEFAULT;
        l.contrast_by(-100);
        l.brightness_by(100);
        assert_eq!(l, Levels { contrast: Levels::CONTRAST.0, brightness: Levels::BRIGHTNESS.1 });
    }

    #[test]
    fn tokens_go_through_unchanged() {
        for tone in [Tone::Tones, Tone::Bits] {
            let levels: &[Gray8] = if tone == Tone::Tones { &TONES } else { &BITS };
            for l in levels {
                for (x, y) in [(0, 0), (1, 2), (3, 3)] {
                    assert_eq!(dither(l.0, x, y, tone), *l, "{tone:?} {}", l.0);
                }
            }
        }
    }

    #[test]
    fn a_grey_between_two_tokens_dithers_to_its_share_of_each() {
        // Halfway between black and white is half of the 4x4 cell white
        let whites = (0..16).filter(|i| dither(128, i % 4, i / 4, Tone::Bits) == color::WHITE).count();
        assert_eq!(whites, 8);
        // And only ever the two tokens around it
        let mid = (color::STATUS_DIM.0 as u16 + color::DIVIDER.0 as u16) / 2;
        for i in 0..16 {
            let g = dither(mid as u8, i % 4, i / 4, Tone::Tones);
            assert!(g == color::STATUS_DIM || g == color::DIVIDER, "{}", g.0);
        }
    }
}
