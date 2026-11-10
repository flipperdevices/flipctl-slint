// The colour carrier on the demodulated video: a brightness filter with a zero at it, and a
// demodulator that brings it to complex baseband at 10 Msps, lined up with brightness.

use super::halfband::{low_pass, ComplexLowPass};

pub const PAL_FSC: f64 = 4_433_618.75;
pub const NTSC_FSC: f64 = 315e6 / 88.0;

/// Colour bandwidth either side of the carrier, as a fraction of the 20 Msps input.
const CHROMA_CUTOFF: f64 = 1.4e6 / 20e6;

/// Brightness filter for 20 to 10 Msps: a low-pass with a zero at the colour carrier, 23 taps,
/// in place of a half-band and a notch. The notch was an IIR, which does not vectorize, and
/// took as long as the half-band and the discriminator's split together.
pub fn luma_taps(fsc: f64, rate: f64) -> Vec<f64> {
    let lp = low_pass(21, 3.9e6 / rate);
    let w = 2.0 * std::f64::consts::PI * fsc / rate;
    let zero = [1.0, -2.0 * w.cos(), 1.0];
    let mut taps = vec![0.0; lp.len() + 2];
    for (i, a) in lp.iter().enumerate() {
        for (j, b) in zero.iter().enumerate() {
            taps[i + j] += a * b;
        }
    }
    let sum: f64 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

/// Mixes video down by the colour carrier and low-passes it: (re, im) at half the rate.
pub struct ChromaDemod {
    cos: Vec<f32>,
    sin: Vec<f32>,
    step: (f64, f64),
    base: (f64, f64),
    pos: usize,
    re: Vec<f32>,
    im: Vec<f32>,
    lowpass: ComplexLowPass<6>,
}

const TABLE: usize = 1024;

impl ChromaDemod {
    pub fn new(fsc: f64, rate: f64) -> ChromaDemod {
        let w = -2.0 * std::f64::consts::PI * fsc / rate;
        let cos = (0..TABLE).map(|k| (w * k as f64).cos() as f32).collect();
        let sin = (0..TABLE).map(|k| (w * k as f64).sin() as f32).collect();
        let a = w * TABLE as f64;
        ChromaDemod { cos, sin, step: (a.cos(), a.sin()), base: (1.0, 0.0), pos: 0, re: Vec::new(), im: Vec::new(), lowpass: ComplexLowPass::new(CHROMA_CUTOFF) }
    }

    pub fn process(&mut self, x: &[f32], out_re: &mut Vec<f32>, out_im: &mut Vec<f32>) {
        self.re.clear();
        self.im.clear();
        self.re.resize(x.len(), 0.0);
        self.im.resize(x.len(), 0.0);
        let mut i = 0;
        while i < x.len() {
            let len = (TABLE - self.pos).min(x.len() - i);
            let (br, bi) = (self.base.0 as f32, self.base.1 as f32);
            let (c, s) = (&self.cos[self.pos..self.pos + len], &self.sin[self.pos..self.pos + len]);
            for ((((&x, &c), &s), re), im) in x[i..i + len].iter().zip(c).zip(s).zip(&mut self.re[i..i + len]).zip(&mut self.im[i..i + len]) {
                *re = x * (c * br - s * bi);
                *im = x * (c * bi + s * br);
            }
            i += len;
            self.pos += len;
            if self.pos == TABLE {
                self.pos = 0;
                let ((a, b), (c, d)) = (self.base, self.step);
                let (r, s) = (a * c - b * d, a * d + b * c);
                let m = (r * r + s * s).sqrt();
                self.base = (r / m, s / m);
            }
        }
        self.lowpass.process(&self.re, &self.im, out_re, out_im);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luma_filter_keeps_brightness_and_stops_the_carrier() {
        let rate = 20e6;
        for fsc in [PAL_FSC, NTSC_FSC] {
            let taps = luma_taps(fsc, rate);
            assert_eq!(taps.len(), 23);
            let gain = |f: f64| {
                let w = 2.0 * std::f64::consts::PI * f / rate;
                let (re, im) = taps.iter().enumerate().fold((0.0, 0.0), |(r, i), (k, t)| (r + t * (w * k as f64).cos(), i - t * (w * k as f64).sin()));
                (re * re + im * im).sqrt()
            };
            assert!((gain(0.0) - 1.0).abs() < 1e-9);
            // The zero at the carrier tilts the passband by about half a dB at 1 MHz
            assert!(gain(1e6) > 0.9, "1 MHz: {}", gain(1e6));
            assert!(gain(fsc) < 1e-6, "carrier: {}", gain(fsc));
            assert!(gain(7e6) < 0.02, "7 MHz: {}", gain(7e6));
        }
    }

    #[test]
    fn demodulates_a_carrier_to_its_phase() {
        let rate = 20e6;
        let phase = 1.0f64;
        let x: Vec<f32> = (0..20_000).map(|i| (2.0 * std::f64::consts::PI * PAL_FSC * i as f64 / rate + phase).cos() as f32).collect();
        let mut d = ChromaDemod::new(PAL_FSC, rate);
        let (mut re, mut im) = (Vec::new(), Vec::new());
        for c in x.chunks(3001) {
            d.process(c, &mut re, &mut im);
        }
        // cos mixed down: half amplitude at the carrier's phase
        for k in [2000, 6000, 9000] {
            let (a, p) = ((re[k] * re[k] + im[k] * im[k]).sqrt(), im[k].atan2(re[k]));
            assert!((a - 0.5).abs() < 0.01 && (p as f64 - phase).abs() < 0.01, "{a} {p}");
        }
    }
}
