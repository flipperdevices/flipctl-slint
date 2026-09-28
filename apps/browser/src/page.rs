//! A frame of the page, in grey, and the page at any scale.
//!
//! The prototype learned that shrinking a page sevenfold by sampling it looks
//! speckled: most source pixels are never read. It built a mip chain to average all
//! of them. A summed-area table does the same at every scale at once: any rectangle's
//! mean is four lookups, so each panel pixel is the exact average of the page pixels
//! under it, whatever the zoom, and a frame costs one pass to prepare.

use crate::view::{Placement, Size};

/// A frame no larger than this cannot overflow a u32 sum: 4096 * 4096 * 255 is
/// just under 2^32.
const MAX_SIDE: u32 = 4096;

/// What the panel shows where there is no page.
const GROUND: u8 = 0xff;

pub struct Page {
    pub w: u32,
    pub h: u32,
    /// CSS pixels per frame pixel, for turning a point on the frame into one the
    /// engine can click.
    pub css: (f32, f32),
    /// Whether the page is scrolled to its top, which is where pushing past the top
    /// brings the strip down rather than scrolling.
    pub at_top: bool,
    /// Row-major, (w + 1) * (h + 1), with a zero row and column in front.
    sat: Vec<u32>,
}

impl Page {
    /// From one byte of grey per pixel. `None` for a frame too big to sum.
    pub fn from_luma(w: u32, h: u32, grey: &[u8], css: (f32, f32)) -> Option<Self> {
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || grey.len() != (w * h) as usize {
            return None;
        }
        let stride = w as usize + 1;
        let mut sat = vec![0u32; stride * (h as usize + 1)];
        for y in 0..h as usize {
            let mut row = 0u32;
            for x in 0..w as usize {
                row += u32::from(grey[y * w as usize + x]);
                sat[(y + 1) * stride + x + 1] = sat[y * stride + x + 1] + row;
            }
        }
        Some(Self { w, h, css, at_top: true, sat })
    }

    pub fn size(&self) -> Size {
        Size { w: self.w as f32, h: self.h as f32 }
    }

    /// The mean of the half-open rectangle, which must not be empty.
    fn mean(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> u8 {
        let stride = self.w as usize + 1;
        let at = |x: usize, y: usize| u64::from(self.sat[y * stride + x]);
        let sum = at(x1, y1) + at(x0, y0) - at(x1, y0) - at(x0, y1);
        let n = ((x1 - x0) * (y1 - y0)) as u64;
        ((sum + n / 2) / n) as u8
    }

    /// The page pixels one panel pixel covers along one axis, or `None` off the page.
    /// A pixel narrower than a page pixel covers one, which magnifies by repeating.
    fn spans(n: usize, d: i32, s: f32, len: u32) -> Vec<Option<(usize, usize)>> {
        (0..n as i32)
            .map(|o| {
                let u0 = (o - d) as f32 / s;
                let u1 = (o + 1 - d) as f32 / s;
                if u1 <= 0.0 || u0 >= len as f32 {
                    return None;
                }
                let a = u0.max(0.0).floor() as usize;
                let b = (u1.floor() as usize).clamp(a + 1, len as usize);
                Some((a, b))
            })
            .collect()
    }

    /// The page as `place` puts it, `w` by `h` bytes of grey.
    pub fn render(&self, place: Placement, w: usize, h: usize) -> Vec<u8> {
        let cols = Self::spans(w, place.dx, place.s, self.w);
        let rows = Self::spans(h, place.dy, place.s, self.h);
        let mut out = vec![GROUND; w * h];
        for (y, row) in rows.iter().enumerate() {
            let Some((y0, y1)) = *row else { continue };
            for (x, col) in cols.iter().enumerate() {
                if let Some((x0, x1)) = *col {
                    out[y * w + x] = self.mean(x0, y0, x1, y1);
                }
            }
        }
        out
    }

    /// The whole page shrunk to `w` by `h`.
    pub fn thumbnail(&self, w: usize, h: usize) -> Vec<u8> {
        let s = w as f32 / self.w as f32;
        self.render(Placement { s, dx: 0, dy: 0 }, w, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> Page {
        let grey: Vec<u8> =
            (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| f(x, y)).collect();
        Page::from_luma(w, h, &grey, (1.0, 1.0)).expect("page")
    }

    #[test]
    fn shrinking_averages_every_pixel() {
        // One-pixel stripes: any sampling lands on one colour or the other, and only
        // an average gives the grey between them.
        let p = page(14, 14, |x, _| if x % 2 == 0 { 0 } else { 254 });
        let out = p.render(Placement { s: 0.5, dx: 0, dy: 0 }, 7, 7);
        assert!(out.iter().all(|&g| g == 127), "{out:?}");
    }

    #[test]
    fn magnifying_repeats_pixels() {
        let p = page(2, 1, |x, _| if x == 0 { 0 } else { 200 });
        let out = p.render(Placement { s: 2.0, dx: 0, dy: 0 }, 4, 2);
        assert_eq!(out, vec![0, 0, 200, 200, 0, 0, 200, 200]);
    }

    #[test]
    fn off_the_page_is_ground() {
        let p = page(2, 2, |_, _| 0);
        let out = p.render(Placement { s: 1.0, dx: 1, dy: 0 }, 4, 3);
        assert_eq!(
            out,
            vec![GROUND, 0, 0, GROUND, GROUND, 0, 0, GROUND, GROUND, GROUND, GROUND, GROUND]
        );
    }

    #[test]
    fn an_offset_reads_the_right_part_of_the_page() {
        let p = page(10, 1, |x, _| x as u8 * 10);
        let out = p.render(Placement { s: 1.0, dx: -7, dy: 0 }, 3, 1);
        assert_eq!(out, vec![70, 80, 90]);
    }

    #[test]
    fn a_frame_too_large_to_sum_is_refused() {
        assert!(
            Page::from_luma(MAX_SIDE + 1, 1, &vec![0; MAX_SIDE as usize + 1], (1.0, 1.0)).is_none()
        );
        assert!(Page::from_luma(2, 2, &[0; 3], (1.0, 1.0)).is_none());
    }
}
