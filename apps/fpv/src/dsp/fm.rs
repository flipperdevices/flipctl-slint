// FM discriminator: the phase step between consecutive samples, in units of the deviation.

/// atan2 to about 2e-6 rad: octant reduction and a minimax polynomial, by fused multiply-adds,
/// which halve both the instructions and the chain each sample waits on.
#[inline(always)]
pub fn fast_atan2(y: f32, x: f32) -> f32 {
    let (ax, ay) = (x.abs(), y.abs());
    let (mx, mn) = (ax.max(ay), ax.min(ay));
    let a = mn / (mx + 1e-30);
    let s = a * a;
    let p = s.mul_add(-0.01172120, 0.05265332);
    let p = s.mul_add(p, -0.11643287);
    let p = s.mul_add(p, 0.19354346);
    let p = s.mul_add(p, -0.33262347);
    let r = a * s.mul_add(p, 0.99997726);
    let r = if ay > ax { std::f32::consts::FRAC_PI_2 - r } else { r };
    let r = if x < 0.0 { std::f32::consts::PI - r } else { r };
    r.copysign(y)
}

pub struct Discriminator {
    last: (f32, f32),
    scale: f32,
}

impl Discriminator {
    /// `deviation` Hz maps to 1.0 at `rate` samples per second.
    pub fn new(deviation: f64, rate: f64) -> Discriminator {
        Discriminator { last: (1.0, 0.0), scale: (rate / (2.0 * std::f64::consts::PI * deviation)) as f32 }
    }

    pub fn process(&mut self, re: &[f32], im: &[f32], out: &mut Vec<f32>) {
        let n = re.len();
        if n == 0 {
            return;
        }
        // The angle of x[i] * conj(x[i - 1])
        let scale = self.scale;
        let step = move |r1: f32, i1: f32, r0: f32, i0: f32| fast_atan2(i1.mul_add(r0, -(r1 * i0)), r1.mul_add(r0, i1 * i0)) * scale;
        let (lr, li) = self.last;
        out.push(step(re[0], im[0], lr, li));
        // For i >= 1 zipped over shifted slices, so it vectorizes
        out.extend(re[1..n].iter().zip(&im[1..n]).zip(&re[..n - 1]).zip(&im[..n - 1]).map(|(((&r1, &i1), &r0), &i0)| step(r1, i1, r0, i0)));
        self.last = (re[n - 1], im[n - 1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atan2_matches_libm() {
        let mut worst = 0.0f64;
        for i in 0..100_000 {
            let a = i as f64 * 0.000_731 - 36.0;
            let (y, x) = ((a.sin() * (1.0 + (i % 7) as f64)) as f32, (a.cos() * (1.0 + (i % 5) as f64)) as f32);
            let e = (fast_atan2(y, x) as f64 - (y as f64).atan2(x as f64)).abs();
            let e = e.min((e - 2.0 * std::f64::consts::PI).abs());
            worst = worst.max(e);
        }
        assert!(worst < 5e-6, "worst error {worst}");
    }

    #[test]
    fn reads_a_carrier_as_its_frequency() {
        let rate = 20e6;
        let f = 3e6;
        let (re, im): (Vec<f32>, Vec<f32>) = (0..1000)
            .map(|i| {
                let a = 2.0 * std::f64::consts::PI * f * i as f64 / rate;
                (a.cos() as f32, a.sin() as f32)
            })
            .unzip();
        let mut d = Discriminator::new(4e6, rate);
        let mut out = Vec::new();
        d.process(&re, &im, &mut out);
        assert!(out[10..].iter().all(|v| (v - 0.75).abs() < 1e-4));
    }
}
