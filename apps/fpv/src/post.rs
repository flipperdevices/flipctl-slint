// Optional clean-up of the woven picture, after ffmpeg's atadenoise/hqdn3d, unsharp and pp=al:
//
//   denoise:     a running average per pixel, strong where it barely changes, none where it does;
//   sharpen:     plus a quarter of the difference from a 7 x 7 blur;
//   levels:      the 0.5 to 99.5 percentiles of brightness stretched to full scale, smoothed.
//
// Planar f32 throughout, so the loops vectorize, and the three colour planes in parallel: on
// one slow core it managed 18 frames a second. Runs on the slow cores.

use crate::dsp::video::{HEIGHT, WIDTH};

/// A pixel changing by less than NOISE (of 255) is averaged, the more the smaller the change,
/// down to weight DENOISE_MIN (about 13 frames); by more, it is passed through. FM video at low
/// signal to noise varies by about 20 on a still wall, so the threshold sits well above that.
const NOISE: f32 = 48.0;
const DENOISE_MIN: f32 = 0.15;
/// 0.5 drew halos round edges and doubled the colour speckle of a noisy signal.
const SHARPEN: f32 = 0.25;
const RADIUS: usize = 3;

pub struct Post {
    frame: [Vec<f32>; 3],
    acc: [Vec<f32>; 3],
    blur: [Vec<f32>; 3],
    levels: (f32, f32),
}

impl Post {
    pub fn new() -> Post {
        let plane = || vec![0.0f32; WIDTH * HEIGHT];
        Post { frame: [plane(), plane(), plane()], acc: [plane(), plane(), plane()], blur: [plane(), plane(), plane()], levels: (0.0, 255.0) }
    }

    /// One woven frame in, RGB WIDTH x HEIGHT; the cleaned-up frame, the same, out.
    pub fn process(&mut self, image: &[u8], out: &mut [u8]) {
        std::thread::scope(|s| {
            let planes = self.frame.iter_mut().zip(&mut self.acc).zip(&mut self.blur).enumerate();
            for (c, ((frame, acc), blur)) in planes {
                s.spawn(move || {
                    for (d, s) in frame.iter_mut().zip(image.iter().skip(c).step_by(3)) {
                        *d = *s as f32;
                    }
                    denoise(frame, acc);
                    sharpen(acc, HEIGHT, blur, frame);
                });
            }
        });
        self.update_levels();

        let (lo, hi) = self.levels;
        let k = 255.0 / (hi - lo).max(16.0);
        for c in 0..3 {
            for (d, &s) in out.iter_mut().skip(c).step_by(3).zip(&self.frame[c]) {
                *d = ((s - lo) * k).clamp(0.0, 255.0) as u8;
            }
        }
    }

    /// Brightness percentiles over a sparse sample of green, smoothed over frames.
    fn update_levels(&mut self) {
        let mut hist = [0u32; 256];
        for v in self.frame[1].iter().step_by(7) {
            hist[v.clamp(0.0, 255.0) as usize] += 1;
        }
        let total: u32 = hist.iter().sum();
        let percentile = |p: f32| {
            let target = (total as f32 * p) as u32;
            let mut seen = 0;
            hist.iter().position(|&h| {
                seen += h;
                seen > target
            }).unwrap_or(255) as f32
        };
        let (lo, hi) = (percentile(0.005), percentile(0.995));
        self.levels = (0.9 * self.levels.0 + 0.1 * lo, 0.9 * self.levels.1 + 0.1 * hi);
    }
}

/// acc moves towards x by a weight that grows with the square of the difference.
fn denoise(x: &[f32], acc: &mut [f32]) {
    for (a, &v) in acc.iter_mut().zip(x) {
        let d = v - *a;
        let r = d / NOISE;
        let w = (r * r).clamp(DENOISE_MIN, 1.0);
        *a += w * d;
    }
}

/// out = x + SHARPEN * (x - box blur of x), the blur (2 * RADIUS + 1) square, done as two
/// passes of running sums.
fn sharpen(x: &[f32], rows: usize, tmp: &mut [f32], out: &mut [f32]) {
    let n = (2 * RADIUS + 1) as f32;
    // Horizontal: running sum along each row, edges clamped
    for r in 0..rows {
        let row = &x[r * WIDTH..][..WIDTH];
        let dst = &mut tmp[r * WIDTH..][..WIDTH];
        let at = |i: isize| row[i.clamp(0, WIDTH as isize - 1) as usize];
        let mut sum: f32 = (-(RADIUS as isize)..=RADIUS as isize).map(at).sum();
        for i in 0..WIDTH as isize {
            dst[i as usize] = sum / n;
            sum += at(i + RADIUS as isize + 1) - at(i - RADIUS as isize);
        }
    }
    // Vertical: a running sum per column, all columns at once
    let mut sum = vec![0.0f32; WIDTH];
    let row = |r: isize| &tmp[(r.clamp(0, rows as isize - 1) as usize) * WIDTH..][..WIDTH];
    for k in -(RADIUS as isize)..=RADIUS as isize {
        for (s, &v) in sum.iter_mut().zip(row(k)) {
            *s += v;
        }
    }
    for r in 0..rows as isize {
        let src = &x[r as usize * WIDTH..][..WIDTH];
        let dst = &mut out[r as usize * WIDTH..][..WIDTH];
        for ((o, &v), &s) in dst.iter_mut().zip(src).zip(&sum) {
            *o = v + SHARPEN * (v - s / n);
        }
        let (add, sub) = (row(r + RADIUS as isize + 1), row(r - RADIUS as isize));
        for ((s, &a), &b) in sum.iter_mut().zip(add).zip(sub) {
            *s += a - b;
        }
    }
}
