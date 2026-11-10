// BirriaSDR's interleaved i16 I,Q to f32, shifted so that the sample at `offset` Hz lands at 0.
//
// QuarterShift is what the decoder uses: the chip is tuned so the channel sits at exactly a
// quarter of the rate below the centre, and moving it to 0 is then multiplying sample n by
// j^n, only signs and a swap. Shift moves by any amount, for recordings made at other centres.
//
// A phasor stepped once per sample is a dependency chain that does not vectorize. Instead a
// table holds the phasor for TABLE consecutive samples, and each block of TABLE samples is
// multiplied by its start phasor times the table: two independent complex multiplies per
// sample. The start phasor advances in f64 and is renormalized, so it does not drift.

const TABLE: usize = 1024;
pub const SCALE: f32 = 1.0 / 8192.0;

pub struct Shift {
    rate: f64,
    table_re: Vec<f32>,
    table_im: Vec<f32>,
    step: (f64, f64),
    base: (f64, f64),
    pos: usize,
    offset: f64,
}

impl Shift {
    pub fn new(rate: f64, offset: f64) -> Shift {
        let mut s = Shift { rate, table_re: vec![0.0; TABLE], table_im: vec![0.0; TABLE], step: (1.0, 0.0), base: (1.0, 0.0), pos: 0, offset: 0.0 };
        s.set_offset(offset);
        s
    }

    pub fn offset(&self) -> f64 {
        self.offset
    }

    pub fn set_offset(&mut self, offset: f64) {
        self.offset = offset;
        let w = -2.0 * std::f64::consts::PI * offset / self.rate;
        for k in 0..TABLE {
            let a = w * k as f64;
            self.table_re[k] = a.cos() as f32;
            self.table_im[k] = a.sin() as f32;
        }
        let a = w * TABLE as f64;
        self.step = (a.cos(), a.sin());
        self.pos = 0;
    }

    /// Appends iq.len() / 2 samples to `re` and `im`.
    pub fn process(&mut self, iq: &[i16], re: &mut Vec<f32>, im: &mut Vec<f32>) {
        let n = iq.len() / 2;
        let start = re.len();
        re.resize(start + n, 0.0);
        im.resize(start + n, 0.0);
        let (re, im) = (&mut re[start..], &mut im[start..]);
        let mut i = 0;
        while i < n {
            // As far as the end of this table block or of the input
            let len = (TABLE - self.pos).min(n - i);
            let (br, bi) = (self.base.0 as f32, self.base.1 as f32);
            let tr = &self.table_re[self.pos..self.pos + len];
            let ti = &self.table_im[self.pos..self.pos + len];
            let x = &iq[2 * i..2 * (i + len)];
            let (or, oi) = (&mut re[i..i + len], &mut im[i..i + len]);
            // Zipped, not indexed: bounds checks per element kept this from vectorizing
            for ((((x, &tr), &ti), or), oi) in x.chunks_exact(2).zip(tr).zip(ti).zip(or.iter_mut()).zip(oi.iter_mut()) {
                let pr = tr * br - ti * bi;
                let pi = tr * bi + ti * br;
                let (xr, xi) = (x[0] as f32 * SCALE, x[1] as f32 * SCALE);
                *or = xr * pr - xi * pi;
                *oi = xr * pi + xi * pr;
            }
            i += len;
            self.pos += len;
            if self.pos == TABLE {
                self.pos = 0;
                let (a, b) = self.base;
                let (c, d) = self.step;
                let (r, s) = (a * c - b * d, a * d + b * c);
                let m = (r * r + s * s).sqrt();
                self.base = (r / m, s / m);
            }
        }
    }
}

/// i16 I,Q to f32 with the channel moved up from -rate/4 to 0.
pub struct QuarterShift {
    /// n modulo 4 of the next sample
    pub n: usize,
}

impl QuarterShift {
    pub fn new() -> QuarterShift {
        QuarterShift { n: 0 }
    }

    /// Appends iq.len() / 2 samples to `re` and `im`.
    pub fn process(&mut self, iq: &[i16], re: &mut Vec<f32>, im: &mut Vec<f32>) {
        // x * j^n for n = 0, 1, 2, 3: (r, i), (-i, r), (-r, -i), (i, -r)
        let one = |n: usize, r: f32, i: f32| match n & 3 {
            0 => (r, i),
            1 => (-i, r),
            2 => (-r, -i),
            _ => (i, -r),
        };
        let mut x = iq;
        // Up to a multiple of four, one at a time
        while self.n & 3 != 0 && x.len() >= 2 {
            let (r, i) = one(self.n, x[0] as f32 * SCALE, x[1] as f32 * SCALE);
            re.push(r);
            im.push(i);
            self.n += 1;
            x = &x[2..];
        }
        // Then four at a time, which vectorizes
        let groups = x.len() / 8;
        let (r0, i0) = (re.len(), im.len());
        re.resize(r0 + 4 * groups, 0.0);
        im.resize(i0 + 4 * groups, 0.0);
        for ((g, r), i) in x.chunks_exact(8).zip(re[r0..].chunks_exact_mut(4)).zip(im[i0..].chunks_exact_mut(4)) {
            let s = |k: usize| g[k] as f32 * SCALE;
            r.copy_from_slice(&[s(0), -s(3), -s(4), s(7)]);
            i.copy_from_slice(&[s(1), s(2), -s(5), -s(6)]);
        }
        x = &x[8 * groups..];
        for c in x.chunks_exact(2) {
            let (r, i) = one(self.n, c[0] as f32 * SCALE, c[1] as f32 * SCALE);
            re.push(r);
            im.push(i);
            self.n += 1;
        }
        self.n &= 3;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_shift_moves_a_carrier_at_minus_a_quarter_to_zero() {
        let n = 10_000;
        let iq: Vec<i16> = (0..n)
            .flat_map(|i| {
                let a = -std::f64::consts::FRAC_PI_2 * i as f64 + 0.3;
                [(a.cos() * 4000.0).round() as i16, (a.sin() * 4000.0).round() as i16]
            })
            .collect();
        let mut s = QuarterShift::new();
        let (mut re, mut im) = (Vec::new(), Vec::new());
        for chunk in iq.chunks(2 * 7) {
            s.process(chunk, &mut re, &mut im);
        }
        assert_eq!(re.len(), n);
        assert!(re.iter().zip(&im).all(|(r, i)| (i.atan2(*r) - 0.3).abs() < 1e-3));
    }

    #[test]
    fn moves_a_carrier_to_zero() {
        let rate = 40e6;
        let f = -8e6;
        let n = 50_000;
        let iq: Vec<i16> = (0..n)
            .flat_map(|i| {
                let a = 2.0 * std::f64::consts::PI * f * i as f64 / rate;
                [(a.cos() * 4000.0) as i16, (a.sin() * 4000.0) as i16]
            })
            .collect();
        let mut s = Shift::new(rate, f);
        let (mut re, mut im) = (Vec::new(), Vec::new());
        for chunk in iq.chunks(2 * 777) {
            s.process(chunk, &mut re, &mut im);
        }
        // Constant phase after the shift: no residual rotation
        let p0 = im[0].atan2(re[0]);
        let p1 = im[n - 1].atan2(re[n - 1]);
        assert!((p1 - p0).abs() < 1e-2, "phase drifted from {p0} to {p1}");
    }
}
