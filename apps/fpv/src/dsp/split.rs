// BirriaSDR's interleaved i16 I,Q into planar f32: scaled, moved up by a quarter of the rate
// (sample n times j^n: (r, i), (-i, r), (-r, -i), (i, -r) for n = 0, 1, 2, 3) and dealt out by
// phase for the half-bands after it, in one pass. On aarch64 by NEON's de-interleaving load:
// the compiler's own version loaded one lane at a time and took a quarter of the front.

/// Grow each of `outs` by `n`, written by `fill` through pointers to the new ends: zeroing
/// first and overwriting cost as much again.
fn append<const N: usize>(outs: [&mut Vec<f32>; N], n: usize, fill: impl FnOnce([*mut f32; N])) {
    let mut outs = outs;
    for v in outs.iter_mut() {
        v.reserve(n);
    }
    // SAFETY: room for n more was reserved, and fill writes all n of each
    fill(outs.each_mut().map(|v| unsafe { v.as_mut_ptr().add(v.len()) }));
    for v in outs {
        unsafe { v.set_len(v.len() + n) };
    }
}

/// Into two phases: pair j (samples 2j, 2j + 1) gives `even` sample 2j and `odd` 2j + 1,
/// `[re, im]` each. Whole groups of four samples: the shift starts over with each call.
pub fn halves(iq: &[i16], scale: f32, even: [&mut Vec<f32>; 2], odd: [&mut Vec<f32>; 2]) {
    assert!(iq.len() % 8 == 0);
    let [er, ei] = even;
    let [or, oi] = odd;
    append([er, ei, or, oi], iq.len() / 4, |[er, ei, or, oi]| {
        #[allow(unused_mut)] // advanced by the NEON part only
        let mut j = 0;
        #[cfg(target_arch = "aarch64")]
        // SAFETY: 32 i16 read and 8 floats written per output while 32 remain
        unsafe {
            use std::arch::aarch64::*;
            let alt = vld1q_f32([scale, -scale, scale, -scale].as_ptr());
            let nalt = vnegq_f32(alt);
            let put = |x: int16x8_t, sign: float32x4_t, at: *mut f32| {
                vst1q_f32(at, vmulq_f32(vcvtq_f32_s32(vmovl_s16(vget_low_s16(x))), sign));
                vst1q_f32(at.add(4), vmulq_f32(vcvtq_f32_s32(vmovl_high_s16(x)), sign));
            };
            while 4 * j + 32 <= iq.len() {
                let v = vld4q_s16(iq.as_ptr().add(4 * j));
                put(v.0, alt, er.add(j));
                put(v.1, alt, ei.add(j));
                put(v.3, nalt, or.add(j));
                put(v.2, alt, oi.add(j));
                j += 8;
            }
        }
        for (c, j) in iq[4 * j..].chunks_exact(4).zip(j..) {
            let s = if j & 1 == 0 { scale } else { -scale };
            // SAFETY: j below iq.len() / 4
            unsafe {
                er.add(j).write(c[0] as f32 * s);
                ei.add(j).write(c[1] as f32 * s);
                or.add(j).write(c[3] as f32 * -s);
                oi.add(j).write(c[2] as f32 * s);
            }
        }
    });
}

/// Into four phases: sample 4q + r to `re[r]`, `im[r]`. Whole groups of four samples.
pub fn quarters(iq: &[i16], scale: f32, re: [&mut Vec<f32>; 4], im: [&mut Vec<f32>; 4]) {
    assert!(iq.len() % 8 == 0);
    let [r0, r1, r2, r3] = re;
    let [i0, i1, i2, i3] = im;
    append([r0, i0, r1, i1, r2, i2, r3, i3], iq.len() / 8, |[r0, i0, r1, i1, r2, i2, r3, i3]| {
        #[allow(unused_mut)] // advanced by the NEON part only
        let mut q = 0;
        #[cfg(target_arch = "aarch64")]
        // SAFETY: 64 i16 read and 8 floats written per output while 64 remain
        unsafe {
            use std::arch::aarch64::*;
            let (pos, neg) = (vdupq_n_f32(scale), vdupq_n_f32(-scale));
            let put = |x: int16x8_t, sign: float32x4_t, at: *mut f32| {
                vst1q_f32(at, vmulq_f32(vcvtq_f32_s32(vmovl_s16(vget_low_s16(x))), sign));
                vst1q_f32(at.add(4), vmulq_f32(vcvtq_f32_s32(vmovl_high_s16(x)), sign));
            };
            while 8 * q + 64 <= iq.len() {
                // Lane l of a.0 is the real part of sample 2l, a.2 of sample 2l + 1; the even
                // lanes of a and b together are samples 4q, the odd ones 4q + 2
                let a = vld4q_s16(iq.as_ptr().add(8 * q));
                let b = vld4q_s16(iq.as_ptr().add(8 * q + 32));
                put(vuzp1q_s16(a.0, b.0), pos, r0.add(q));
                put(vuzp1q_s16(a.1, b.1), pos, i0.add(q));
                put(vuzp1q_s16(a.3, b.3), neg, r1.add(q));
                put(vuzp1q_s16(a.2, b.2), pos, i1.add(q));
                put(vuzp2q_s16(a.0, b.0), neg, r2.add(q));
                put(vuzp2q_s16(a.1, b.1), neg, i2.add(q));
                put(vuzp2q_s16(a.3, b.3), pos, r3.add(q));
                put(vuzp2q_s16(a.2, b.2), neg, i3.add(q));
                q += 8;
            }
        }
        for (g, q) in iq[8 * q..].chunks_exact(8).zip(q..) {
            let s = |k: usize, sign: f32| g[k] as f32 * (sign * scale);
            // SAFETY: q below iq.len() / 8
            unsafe {
                r0.add(q).write(s(0, 1.0));
                i0.add(q).write(s(1, 1.0));
                r1.add(q).write(s(3, -1.0));
                i1.add(q).write(s(2, 1.0));
                r2.add(q).write(s(4, -1.0));
                i2.add(q).write(s(5, -1.0));
                r3.add(q).write(s(7, 1.0));
                i3.add(q).write(s(6, -1.0));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> Vec<i16> {
        (0..4096 + 72).map(|i| ((i * 7919) % 4000) as i16 - 2000).collect()
    }

    /// Sample n of `iq` times j^n.
    fn shifted(iq: &[i16], n: usize, scale: f32) -> (f32, f32) {
        let (r, i) = (iq[2 * n] as f32 * scale, iq[2 * n + 1] as f32 * scale);
        match n & 3 {
            0 => (r, i),
            1 => (-i, r),
            2 => (-r, -i),
            _ => (i, -r),
        }
    }

    #[test]
    fn halves_are_the_shifted_samples() {
        let iq = input();
        let mut v: [Vec<f32>; 4] = Default::default();
        let [er, ei, or, oi] = &mut v;
        halves(&iq, 0.5, [er, ei], [or, oi]);
        for j in 0..iq.len() / 4 {
            assert_eq!((v[0][j], v[1][j]), shifted(&iq, 2 * j, 0.5), "even {j}");
            assert_eq!((v[2][j], v[3][j]), shifted(&iq, 2 * j + 1, 0.5), "odd {j}");
        }
    }

    #[test]
    fn quarters_are_the_shifted_samples() {
        let iq = input();
        let mut re: [Vec<f32>; 4] = Default::default();
        let mut im: [Vec<f32>; 4] = Default::default();
        quarters(&iq, 0.5, re.each_mut(), im.each_mut());
        for q in 0..iq.len() / 8 {
            for r in 0..4 {
                assert_eq!((re[r][q], im[r][q]), shifted(&iq, 4 * q + r, 0.5), "phase {r} at {q}");
            }
        }
    }
}
