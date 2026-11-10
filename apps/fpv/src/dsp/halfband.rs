// 2:1 half-band decimation in polyphase form.
//
// Output j is the centre tap times even sample 2j plus each tap times the two odd samples
// either side, so the input is split once into even and odd arrays and every loop then runs
// over contiguous floats. Taps: a Blackman-windowed sinc at a quarter of the rate, whose even
// offsets are zero by construction; HALF taps on the odd offsets 1, 3, ..., 2 * HALF - 1.

use super::split;

fn design<const HALF: usize>() -> ([f32; HALF], f32) {
    let mut hb = [0.0f64; HALF];
    let mut sum = 0.5;
    for (k, h) in hb.iter_mut().enumerate() {
        let d = (2 * k + 1) as f64;
        let sinc = (std::f64::consts::PI * d / 2.0).sin() / (std::f64::consts::PI * d);
        let n = 2.0 * HALF as f64;
        let w = 0.42 + 0.5 * (std::f64::consts::PI * d / n).cos() + 0.08 * (2.0 * std::f64::consts::PI * d / n).cos();
        *h = sinc * w;
        sum += 2.0 * *h;
    }
    let mut out = [0.0f32; HALF];
    for k in 0..HALF {
        out[k] = (hb[k] / sum) as f32;
    }
    (out, (0.5 / sum) as f32)
}

/// Outputs per pass of the filters: few enough that a piece and its input stay in L1 from one
/// tap's pass to the next.
const PIECE: usize = 1024;

/// Append `n` outputs, `hc * centre[i]` plus `hb[k] * (earlier[i] + later[i])` for each tap
/// k, where `pair(k)` gives (earlier, later): a pass per tap over pieces that stay in L1, the
/// first extending rather than zeroing and overwriting. Taps by fused multiply-add here and in
/// every filter: one NEON instruction for two.
fn taps<'a, const HALF: usize>(out: &mut Vec<f32>, n: usize, hc: f32, centre: &[f32], hb: &[f32; HALF], pair: impl Fn(usize) -> (&'a [f32], &'a [f32])) {
    for c in (0..n).step_by(PIECE) {
        let m = PIECE.min(n - c);
        let start = out.len();
        out.extend(centre[c..][..m].iter().map(|&x| hc * x));
        let y = &mut out[start..];
        for (k, &h) in hb.iter().enumerate() {
            let (a, b) = pair(k);
            for ((y, &a), &b) in y.iter_mut().zip(&a[c..][..m]).zip(&b[c..][..m]) {
                *y = h.mul_add(a + b, *y);
            }
        }
    }
}

/// One channel's even and odd samples, each with 2 * HALF of history in front.
struct Phases<const HALF: usize> {
    even: Vec<f32>,
    odd: Vec<f32>,
    carry: Option<f32>,
}

impl<const HALF: usize> Phases<HALF> {
    fn new() -> Self {
        Phases { even: vec![0.0; 2 * HALF], odd: vec![0.0; 2 * HALF], carry: None }
    }

    /// Split `x` into pairs after the history; an odd sample left over waits for the next call.
    fn push(&mut self, x: &[f32]) {
        let mut i = 0;
        if let Some(c) = self.carry.take() {
            match x.first() {
                Some(&v) => {
                    self.even.push(c);
                    self.odd.push(v);
                    i = 1;
                }
                None => {
                    self.carry = Some(c);
                    return;
                }
            }
        }
        let pairs = (x.len() - i) / 2;
        let (e0, o0) = (self.even.len(), self.odd.len());
        self.even.resize(e0 + pairs, 0.0);
        self.odd.resize(o0 + pairs, 0.0);
        let src = &x[i..i + 2 * pairs];
        for ((e, o), s) in self.even[e0..].iter_mut().zip(&mut self.odd[o0..]).zip(src.chunks_exact(2)) {
            *e = s[0];
            *o = s[1];
        }
        i += 2 * pairs;
        if i < x.len() {
            self.carry = Some(x[i]);
        }
    }

    fn pairs(&self) -> usize {
        self.even.len() - 2 * HALF
    }

    /// Append one output per pair to `out`, then keep the last 2 * HALF of each phase.
    fn filter(&mut self, hb: &[f32; HALF], hc: f32, out: &mut Vec<f32>) {
        let n = self.pairs();
        let odd = &self.odd;
        taps(out, n, hc, &self.even[HALF..], hb, |k| (&odd[HALF - 1 - k..], &odd[HALF + k..]));
        self.even.drain(..n);
        self.odd.drain(..n);
    }

    /// Like filter, for a general symmetric FIR of 4 * HALF - 1 taps centred on the same even
    /// sample, so its output lines up with the half-band's: `even[u]` is the tap at offset
    /// 2 * (u - (HALF - 1)) from the centre, `odd[u]` the one at 2 * (u - HALF) + 1. Taps are
    /// symmetric, so the two samples sharing one are added first: half the multiplies.
    fn filter_fir(&mut self, even: &[f32], odd: &[f32], out: &mut Vec<f32>) {
        debug_assert!(even.len() == 2 * HALF - 1 && odd.len() == 2 * HALF);
        let n = self.pairs();
        let hc = even[HALF - 1];
        for c in (0..n).step_by(PIECE) {
            let m = PIECE.min(n - c);
            let start = out.len();
            out.extend(self.even[HALF + c..][..m].iter().map(|&x| hc * x));
            let y = &mut out[start..];
            for (u, &h) in even[..HALF - 1].iter().enumerate() {
                let (a, b) = (u + 1 + c, 2 * HALF - 1 - u + c);
                for ((y, &a), &b) in y.iter_mut().zip(&self.even[a..a + m]).zip(&self.even[b..b + m]) {
                    *y = h.mul_add(a + b, *y);
                }
            }
            for (u, &h) in odd[..HALF].iter().enumerate() {
                let (a, b) = (u + c, 2 * HALF - 1 - u + c);
                for ((y, &a), &b) in y.iter_mut().zip(&self.odd[a..a + m]).zip(&self.odd[b..b + m]) {
                    *y = h.mul_add(a + b, *y);
                }
            }
        }
        self.even.drain(..n);
        self.odd.drain(..n);
    }
}

/// A Blackman-windowed sinc low-pass of `span` taps, unit gain at DC; `cutoff` as a fraction
/// of the rate.
pub fn low_pass(span: usize, cutoff: f64) -> Vec<f64> {
    let mid = (span / 2) as f64;
    let mut taps: Vec<f64> = (0..span)
        .map(|i| {
            let t = i as f64 - mid;
            let sinc = if t == 0.0 { 2.0 * cutoff } else { (2.0 * std::f64::consts::PI * cutoff * t).sin() / (std::f64::consts::PI * t) };
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (span - 1) as f64).cos() + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / (span - 1) as f64).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

/// Symmetric taps of odd length 4 * HALF - 1 split by the parity of their offset from the centre.
fn split_taps<const HALF: usize>(taps: &[f64]) -> (Vec<f32>, Vec<f32>) {
    assert_eq!(taps.len(), 4 * HALF - 1);
    let half = (taps.len() / 2) as isize;
    let (mut even, mut odd) = (Vec::new(), Vec::new());
    for (i, &t) in taps.iter().enumerate() {
        if (i as isize - half) % 2 == 0 {
            even.push(t as f32);
        } else {
            odd.push(t as f32);
        }
    }
    (even, odd)
}

/// 2:1 decimation through a symmetric FIR of 4 * HALF - 1 taps, aligned with
/// RealHalfBand<HALF>: both outputs refer to the same input samples.
pub struct RealLowPass<const HALF: usize> {
    x: Phases<HALF>,
    even: Vec<f32>,
    odd: Vec<f32>,
}

impl<const HALF: usize> RealLowPass<HALF> {
    pub fn new(taps: &[f64]) -> Self {
        let (even, odd) = split_taps::<HALF>(taps);
        RealLowPass { x: Phases::new(), even, odd }
    }

    pub fn process(&mut self, x: &[f32], out: &mut Vec<f32>) {
        self.x.push(x);
        self.x.filter_fir(&self.even, &self.odd, out);
    }
}

/// The same for a complex signal, through a low-pass.
pub struct ComplexLowPass<const HALF: usize> {
    re: Phases<HALF>,
    im: Phases<HALF>,
    even: Vec<f32>,
    odd: Vec<f32>,
}

impl<const HALF: usize> ComplexLowPass<HALF> {
    /// `cutoff` as a fraction of the input rate.
    pub fn new(cutoff: f64) -> Self {
        let (even, odd) = split_taps::<HALF>(&low_pass(4 * HALF - 1, cutoff));
        ComplexLowPass { re: Phases::new(), im: Phases::new(), even, odd }
    }

    pub fn process(&mut self, re: &[f32], im: &[f32], out_re: &mut Vec<f32>, out_im: &mut Vec<f32>) {
        self.re.push(re);
        self.im.push(im);
        self.re.filter_fir(&self.even, &self.odd, out_re);
        self.im.filter_fir(&self.even, &self.odd, out_im);
    }
}

pub struct ComplexHalfBand<const HALF: usize> {
    re: Phases<HALF>,
    im: Phases<HALF>,
    hb: [f32; HALF],
    hc: f32,
}

impl<const HALF: usize> ComplexHalfBand<HALF> {
    pub fn new() -> Self {
        let (hb, hc) = design::<HALF>();
        ComplexHalfBand { re: Phases::new(), im: Phases::new(), hb, hc }
    }

    #[cfg(test)]
    pub fn process(&mut self, re: &[f32], im: &[f32], out_re: &mut Vec<f32>, out_im: &mut Vec<f32>) {
        self.re.push(re);
        self.im.push(im);
        self.re.filter(&self.hb, self.hc, out_re);
        self.im.filter(&self.hb, self.hc, out_im);
    }

    /// BirriaSDR's i16 I,Q, scaled, moved up by a quarter of the rate as QuarterShift does and
    /// split into even and odd in the same pass, then filtered: one pass over the input fewer.
    /// Whole groups of four samples only: each call starts the shift over.
    pub fn process_quarter(&mut self, iq: &[i16], scale: f32, out_re: &mut Vec<f32>, out_im: &mut Vec<f32>) {
        assert!(self.re.carry.is_none() && self.im.carry.is_none());
        split::halves(iq, scale, [&mut self.re.even, &mut self.im.even], [&mut self.re.odd, &mut self.im.odd]);
        self.re.filter(&self.hb, self.hc, out_re);
        self.im.filter(&self.hb, self.hc, out_im);
    }
}

/// Two half-bands in a row, 4:1, behind the quarter shift, as ComplexHalfBand<FIRST> then
/// ComplexHalfBand<SECOND> would do it sample for sample. The input is dealt out four ways, so
/// the first half-band's even and odd outputs each come from whole arrays and go straight to
/// the second's two phases: nothing is split twice.
pub struct ComplexHalfBands<const FIRST: usize, const SECOND: usize> {
    /// Sample 4q + r at `[r][FIRST + q]`, behind FIRST of history.
    re: [Vec<f32>; 4],
    im: [Vec<f32>; 4],
    hb: [f32; FIRST],
    hc: f32,
    next: ComplexHalfBand<SECOND>,
}

impl<const FIRST: usize, const SECOND: usize> ComplexHalfBands<FIRST, SECOND> {
    pub fn new() -> Self {
        let (hb, hc) = design::<FIRST>();
        let history = || std::array::from_fn(|_| vec![0.0; FIRST]);
        ComplexHalfBands { re: history(), im: history(), hb, hc, next: ComplexHalfBand::new() }
    }

    /// As ComplexHalfBand::process_quarter, at a quarter of the rate.
    pub fn process_quarter(&mut self, iq: &[i16], scale: f32, out_re: &mut Vec<f32>, out_im: &mut Vec<f32>) {
        split::quarters(iq, scale, self.re.each_mut(), self.im.each_mut());
        let n = self.re[0].len() - FIRST;
        let next = &mut self.next;
        for (x, to) in [(&mut self.re, &mut next.re), (&mut self.im, &mut next.im)] {
            // Output 2i + e of the first is centred on sample 4i + 2e - 2 FIRST, its taps the
            // odd distances either side of that
            for (e, out) in [(0, &mut to.even), (1, &mut to.odd)] {
                let at = |offset: isize| {
                    let r = offset.rem_euclid(4);
                    &x[r as usize][(FIRST as isize + (offset - r) / 4) as usize..]
                };
                let centre = 2 * e - 2 * FIRST as isize;
                taps(out, n, self.hc, at(centre), &self.hb, |k| {
                    let d = 2 * k as isize + 1;
                    (at(centre - d), at(centre + d))
                });
            }
            for phase in x.iter_mut() {
                phase.drain(..n);
            }
        }
        next.re.filter(&next.hb, next.hc, out_re);
        next.im.filter(&next.hb, next.hc, out_im);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_gain(f: f64) -> f64 {
        let mut hb = ComplexHalfBand::<6>::new();
        let x: Vec<f32> = (0..20_000).map(|i| (2.0 * std::f64::consts::PI * f * i as f64).cos() as f32).collect();
        let (mut y, mut z) = (Vec::new(), Vec::new());
        hb.process(&x, &vec![0.0; x.len()], &mut y, &mut z);
        y[1000..].iter().fold(0.0f64, |m, v| m.max(v.abs() as f64))
    }

    #[test]
    fn half_band_passes_low_and_stops_high() {
        // Frequencies as fractions of the input rate: 0.05 passes, 0.4 is stopped
        assert!((tone_gain(0.05) - 1.0).abs() < 0.01);
        assert!(tone_gain(0.4) < 0.01);
    }

    #[test]
    fn brightness_and_colour_filters_line_up() {
        // A step through both: they cross half way at the same output
        let x: Vec<f32> = (0..2000).map(|i| if i < 1000 { 0.0 } else { 1.0 }).collect();
        let (mut luma, mut chroma) = (Vec::new(), Vec::new());
        RealLowPass::<6>::new(&low_pass(23, 0.2)).process(&x, &mut luma);
        ComplexLowPass::<6>::new(0.07).process(&x, &vec![0.0; x.len()], &mut chroma, &mut Vec::new());
        let cross = |v: &[f32]| v.iter().position(|&s| s >= 0.5).unwrap();
        assert_eq!(cross(&luma), cross(&chroma));
        assert!((chroma[900] - 1.0).abs() < 1e-3 && chroma[100].abs() < 1e-3);
    }

    #[test]
    fn quarter_path_matches_shift_then_filter() {
        let iq: Vec<i16> = (0..4096).map(|i| ((i * 7919) % 2000) as i16 - 1000).collect();
        let (mut a_re, mut a_im) = (Vec::new(), Vec::new());
        let mut hb = ComplexHalfBand::<6>::new();
        hb.process_quarter(&iq, 1.0 / 8192.0, &mut a_re, &mut a_im);
        let (mut re, mut im, mut b_re, mut b_im) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        crate::dsp::shift::QuarterShift::new().process(&iq, &mut re, &mut im);
        ComplexHalfBand::<6>::new().process(&re, &im, &mut b_re, &mut b_im);
        assert_eq!(a_re, b_re);
        assert_eq!(a_im, b_im);
    }

    #[test]
    fn two_in_a_row_match_one_after_the_other() {
        let iq: Vec<i16> = (0..8192).map(|i| ((i * 7919) % 2000) as i16 - 1000).collect();
        let (mut a_re, mut a_im) = (Vec::new(), Vec::new());
        let mut both = ComplexHalfBands::<3, 4>::new();
        for part in iq.chunks(2008) {
            both.process_quarter(part, 1.0 / 8192.0, &mut a_re, &mut a_im);
        }
        let (mut re, mut im, mut re2, mut im2, mut b_re, mut b_im) = Default::default();
        crate::dsp::shift::QuarterShift::new().process(&iq, &mut re, &mut im);
        ComplexHalfBand::<3>::new().process(&re, &im, &mut re2, &mut im2);
        ComplexHalfBand::<4>::new().process(&re2, &im2, &mut b_re, &mut b_im);
        assert_eq!(a_re, b_re);
        assert_eq!(a_im, b_im);
    }

    #[test]
    fn block_sizes_do_not_matter() {
        let x: Vec<f32> = (0..10_001).map(|i| ((i * 7919) % 1000) as f32 / 500.0 - 1.0).collect();
        let taps = low_pass(23, 0.2);
        let mut whole = Vec::new();
        RealLowPass::<6>::new(&taps).process(&x, &mut whole);
        let mut parts = Vec::new();
        let mut lp = RealLowPass::<6>::new(&taps);
        let mut pos = 0;
        for len in [1, 2, 3, 777, 4097, 5, 2000] {
            lp.process(&x[pos..pos + len], &mut parts);
            pos += len;
        }
        lp.process(&x[pos..], &mut parts);
        assert_eq!(whole.len(), parts.len());
        let worst = whole.iter().zip(&parts).fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(worst < 1e-6, "worst difference {worst}");
    }
}
