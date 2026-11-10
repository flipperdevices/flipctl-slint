// Fields to frames: each new field woven with the latest field of the other parity, which the
// decoder tells (Video::field_lowered is the top field). The fields of the camera's picture are
// half a line apart and an FPV camera's OSD is not, so shown one by one either the picture or
// the text bobs by half a line; woven, neither does.
//
// Where the picture moves, the two fields disagree and woven they comb, so there the new field
// is interpolated instead. Motion is told over time, as yadif does, by each field against the
// field of its own parity before it: told within the frame, by the other field against the new
// one interpolated, the sharp edges of still text read as motion and the text bobbed again.

use crate::dsp::video::{HEIGHT, WIDTH};

/// A change this large (of 255) between a field and the one before it of its parity is motion.
/// FM noise alone changes pixels by more than a few tens: on a live capture, 24 took two
/// thirds of still text for motion and it moved half a row from frame to frame; at 64 a tenth,
/// and it moved 0.21 rows, against 0.16 woven with no check at all.
const MOTION: u8 = 64;
const ROW: usize = 3 * WIDTH;

pub struct Weave {
    /// The latest field of each parity and the one before it, RGB lines, and how many lines
    /// the latest has.
    fields: [Vec<u8>; 2],
    older: [Vec<u8>; 2],
    lines: [usize; 2],
    /// Fields in, and the count when each parity last had one.
    count: u64,
    seen: [u64; 2],
    frame: Vec<u8>,
}

impl Weave {
    pub fn new() -> Weave {
        let field = || vec![0; ROW * HEIGHT];
        Weave { fields: [field(), field()], older: [field(), field()], lines: [0; 2], count: 0, seen: [0; 2], frame: field() }
    }

    /// One field (its `lines` lines, each repeated over the rows it covers in `image`, RGB
    /// WIDTH x HEIGHT) in, with its parity; the frame, RGB WIDTH x HEIGHT, out.
    pub fn process(&mut self, image: &[u8], lines: usize, lowered: bool, out: &mut [u8]) {
        let (own, other) = (lowered as usize, !lowered as usize);
        // The other parity's latest field is only woven in while it is the one just before.
        // When the parity is misread, field after field arrives as one parity, the other's
        // lines are from seconds ago, and against their own older copy they read as still:
        // woven, an old picture showed through every other line. (Changed in flipctl's copy.)
        self.count += 1;
        self.seen[own] = self.count;
        let stale = self.count - self.seen[other] > 2;
        let n = lines * ROW;
        let resized = self.lines[own] != lines;
        std::mem::swap(&mut self.fields[own], &mut self.older[own]);
        for r in 0..lines {
            self.fields[own][r * ROW..][..ROW].copy_from_slice(&image[(r * HEIGHT / lines) * ROW..][..ROW]);
        }
        // A field after one of another size has nothing before it: it stands for both
        if resized {
            let (f, o) = (&self.fields[own], &mut self.older[own]);
            o[..n].copy_from_slice(&f[..n]);
        }
        self.lines[own] = lines;
        // Until the other parity has fields this size, they are this one
        if self.lines[other] != lines {
            for v in [&mut self.fields, &mut self.older] {
                let [f0, f1] = v;
                let (from, to) = if lowered { (&*f1, f0) } else { (&*f0, f1) };
                to[..n].copy_from_slice(&from[..n]);
            }
            self.lines[other] = lines;
        }
        let rows = 2 * lines;
        let fields = Fields { new: &self.fields[own][..n], new_old: &self.older[own][..n], other: &self.fields[other][..n], other_old: &self.older[other][..n] };
        if rows == HEIGHT {
            weave(&fields, lines, lowered, stale, out);
            return;
        }
        weave(&fields, lines, lowered, stale, &mut self.frame[..rows * ROW]);
        // Out at HEIGHT rows: NTSC's 480 stretched
        for y in 0..HEIGHT {
            out[y * ROW..][..ROW].copy_from_slice(&self.frame[(y * rows / HEIGHT) * ROW..][..ROW]);
        }
    }
}

/// The new field and the other parity's latest, each with the field of its parity before it.
struct Fields<'a> {
    new: &'a [u8],
    new_old: &'a [u8],
    other: &'a [u8],
    other_old: &'a [u8],
}

/// The new field's lines on one parity; on the other, the other field's line where neither it
/// nor the new lines around it changed since their parity's last field, and the new field
/// interpolated where something did, or everywhere when the other field is `stale`.
fn weave(f: &Fields, lines: usize, top_is_new: bool, stale: bool, out: &mut [u8]) {
    let still = if stale { 0 } else { MOTION };
    fn row(v: &[u8], k: usize) -> &[u8] {
        &v[k * ROW..][..ROW]
    }
    for r in 0..lines {
        let (own, other) = if top_is_new { (2 * r, 2 * r + 1) } else { (2 * r + 1, 2 * r) };
        out[own * ROW..][..ROW].copy_from_slice(&f.new[r * ROW..][..ROW]);
        // The other line lies between new lines r and r + 1 (top new) or r - 1 and r
        let (a, b) = if top_is_new { (r, (r + 1).min(lines - 1)) } else { (r.saturating_sub(1), r) };
        let (x, y, x0, y0) = (row(f.new, a), row(f.new, b), row(f.new_old, a), row(f.new_old, b));
        let (q, q0) = (row(f.other, r), row(f.other_old, r));
        // Byte by byte in lockstep, no indexing: so it vectorizes, as the back thread needs
        let pixels = out[other * ROW..][..ROW].iter_mut().zip(q).zip(q0).zip(x).zip(x0).zip(y).zip(y0);
        for ((((((o, &q), &q0), &x), &x0), &y), &y0) in pixels {
            let motion = q.abs_diff(q0).max(x.abs_diff(x0)).max(y.abs_diff(y0));
            let interp = ((x as u16 + y as u16 + 1) >> 1) as u8;
            *o = if motion < still { q } else { interp };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A field of `frame` (HEIGHT rows): its even rows for the top field, odd for the bottom,
    /// each line repeated over two rows as the decoder draws it.
    fn field(frame: &[u8], top: bool) -> Vec<u8> {
        let mut image = vec![0; ROW * HEIGHT];
        for r in 0..HEIGHT / 2 {
            let src = &frame[(2 * r + !top as usize) * ROW..][..ROW];
            image[2 * r * ROW..][..ROW].copy_from_slice(src);
            image[(2 * r + 1) * ROW..][..ROW].copy_from_slice(src);
        }
        image
    }

    #[test]
    fn a_still_picture_comes_back_whole_text_and_all() {
        // A smooth gradient with a sharp bar a few rows tall, as text is: still, it is woven
        // whole once each parity has had two fields
        let mut frame: Vec<u8> = (0..ROW * HEIGHT).map(|i| ((i / ROW) / 3) as u8).collect();
        for row in 300..309 {
            frame[row * ROW..][..ROW].fill(250);
        }
        let mut weave = Weave::new();
        let mut out = vec![0; ROW * HEIGHT];
        for top in [true, false, true, false, true] {
            weave.process(&field(&frame, top), HEIGHT / 2, top, &mut out);
        }
        assert!(out == frame);
        weave.process(&field(&frame, false), HEIGHT / 2, false, &mut out);
        assert!(out == frame);
    }

    #[test]
    fn a_field_of_one_parity_after_another_is_not_woven_with_an_old_one() {
        // Both parities once from a still picture, then the top parity only, from another:
        // the bottom field is old by then and none of it shows
        let (old, new) = (vec![40; ROW * HEIGHT], vec![200; ROW * HEIGHT]);
        let mut weave = Weave::new();
        let mut out = vec![0; ROW * HEIGHT];
        for top in [true, false] {
            weave.process(&field(&old, top), HEIGHT / 2, top, &mut out);
        }
        for _ in 0..4 {
            weave.process(&field(&new, true), HEIGHT / 2, true, &mut out);
        }
        assert!(out.iter().all(|&v| v == 200));
    }

    #[test]
    fn motion_is_interpolated_not_combed() {
        // A still picture, then a new top field from a different one: its lines are not woven
        // with the old bottom field's
        let (still, moved) = (vec![40; ROW * HEIGHT], vec![200; ROW * HEIGHT]);
        let mut weave = Weave::new();
        let mut out = vec![0; ROW * HEIGHT];
        for top in [true, false, true, false] {
            weave.process(&field(&still, top), HEIGHT / 2, top, &mut out);
        }
        weave.process(&field(&moved, true), HEIGHT / 2, true, &mut out);
        assert!(out.iter().all(|&v| v == 200));
    }
}
