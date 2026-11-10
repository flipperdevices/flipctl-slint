// The decoder in two halves that can run on separate threads, and the channel power scan.
//
//   Front:  raw IQ blocks -> 20 Msps demodulated video, and auto-tune's measurement of the
//           signal's offset.
//   Colour: the 20 Msps video -> colour carrier at complex baseband, 10 Msps.
//   Back:   the 20 Msps video -> 10 Msps brightness -> line sync -> fields in a 768 x 576
//           picture. The brightness filter was a sixth of the front at 80 Msps.

use crate::dsp::chroma::{luma_taps, ChromaDemod, NTSC_FSC, PAL_FSC};
use crate::dsp::fm::Discriminator;
use crate::dsp::halfband::{ComplexHalfBand, ComplexHalfBands, RealLowPass};
use crate::dsp::linesync::LineSync;
use crate::dsp::shift::{QuarterShift, SCALE};
use crate::dsp::video::{line_omega, Standard, Video};
use crate::dsp::IQ_RATE;
use rustfft::num_complex::Complex32;
use std::sync::Arc;

/// FM deviation the discriminator is scaled to; the level control absorbs the difference.
pub const DEVIATION: f64 = 4e6;
/// Rates: scanning sees the most channels per capture, viewing needs the least work.
pub const SCAN_RATE: f64 = 80e6;
pub const VIEW_RATE: f64 = 20e6;

/// Where a viewed channel sits from the centre the chip is tuned to: a quarter of the rate
/// below it, off the DC spike, where moving it to 0 costs only signs and swaps. At 20 Msps the
/// DC spike lands in the signal; recordings made so decode as well as wider ones.
pub fn view_offset(rate: f64) -> f64 {
    -rate / 4.0
}
/// Line sync lock above which a line is counted as locked, as the plugin's indicator.
pub const HLOCKED: i32 = 750;

/// The 20 Msps demodulated video, and the colour carrier from it at complex baseband, 10 Msps.
#[derive(Default)]
pub struct VideoBlock {
    pub video20: Vec<f32>,
    pub cr: Vec<f32>,
    pub ci: Vec<f32>,
}

impl VideoBlock {
    pub fn clear(&mut self) {
        self.cr.clear();
        self.ci.clear();
        self.video20.clear();
    }

    /// Samples of 10 Msps video it makes.
    pub fn len(&self) -> usize {
        self.video20.len() / 2
    }
}

/// Colour from the front's 20 Msps video, apart from it: on the front's thread the colour
/// filter left it no time to spare.
pub struct Colour {
    demod: ChromaDemod,
    ntsc: bool,
}

impl Colour {
    pub fn new() -> Colour {
        Colour { demod: ChromaDemod::new(PAL_FSC, IQ_RATE), ntsc: false }
    }

    /// Fill the block's colour from its 20 Msps video, by NTSC's or PAL's carrier.
    pub fn process(&mut self, block: &mut VideoBlock, ntsc: bool) {
        if ntsc != self.ntsc {
            self.ntsc = ntsc;
            self.demod = ChromaDemod::new(if ntsc { NTSC_FSC } else { PAL_FSC }, IQ_RATE);
        }
        self.demod.process(&block.video20, &mut block.cr, &mut block.ci);
    }
}

/// IQ half-bands down to 20 Msps: 80 to 40 only has to keep what would fold into the final
/// 20 MHz, which the next stage's edge cleans up, so 11 taps do; 40 to 20 gets 15, which
/// decoded the recordings with the same locks and noise as 23 and 31.
enum Decimator {
    None,
    Half(ComplexHalfBand<4>),
    Quarter(ComplexHalfBands<3, 4>),
}

/// `iq` to `decimate` in whole groups of four samples, so the quarter shift starts over with
/// each; what is left over waits in `tail` for the next block.
fn quarter(tail: &mut Vec<i16>, mut iq: &[i16], decimate: &mut dyn FnMut(&[i16])) {
    if !tail.is_empty() {
        let take = (8 - tail.len()).min(iq.len());
        tail.extend_from_slice(&iq[..take]);
        iq = &iq[take..];
        if tail.len() < 8 {
            return;
        }
        decimate(tail);
        tail.clear();
    }
    let whole = iq.len() / 8 * 8;
    decimate(&iq[..whole]);
    tail.extend_from_slice(&iq[whole..]);
}

/// I,Q values per part of a block the front works through at a time: 2k samples.
const PART: usize = 4096;

pub struct Front {
    rate: f64,
    shift: QuarterShift,
    decimator: Decimator,
    disc: Discriminator,
    afc: Afc,
    /// Above 20 Msps, the end of the last block short of a group of four samples.
    tail: Vec<i16>,
    re: Vec<f32>,
    im: Vec<f32>,
    phase: Vec<f32>,
}

impl Front {
    /// `rate` is 20, 40 or 80 Msps, with the channel at view_offset(rate) from the centre.
    pub fn new(rate: f64) -> Front {
        assert!([1.0, 2.0, 4.0].contains(&(rate / IQ_RATE)), "rate must be 20, 40 or 80 Msps");
        Front {
            rate,
            shift: QuarterShift::new(),
            decimator: match (rate / IQ_RATE) as u32 {
                1 => Decimator::None,
                2 => Decimator::Half(ComplexHalfBand::new()),
                _ => Decimator::Quarter(ComplexHalfBands::new()),
            },
            disc: Discriminator::new(DEVIATION, IQ_RATE),
            afc: Afc::new(),
            tail: Vec::new(),
            re: Vec::new(),
            im: Vec::new(),
            phase: Vec::new(),
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// The channel moved, by retuning: what auto-tune measured no longer holds.
    pub fn reset_afc(&mut self) {
        self.afc.reset();
    }

    /// The 20 Msps video for one block of interleaved I,Q into `out`, which must be clear.
    pub fn process(&mut self, iq: &[i16], out: &mut VideoBlock) {
        self.phase.clear();
        // In parts, so what one stage leaves is still in L1 for the next: whole blocks of 16k
        // samples went out to L2 and back between every pair of stages
        for iq in iq.chunks(PART) {
            self.re.clear();
            self.im.clear();
            // Above 20 Msps the shift goes straight into the half-bands
            match &mut self.decimator {
                Decimator::None => self.shift.process(iq, &mut self.re, &mut self.im),
                Decimator::Half(hb) => quarter(&mut self.tail, iq, &mut |iq| hb.process_quarter(iq, SCALE, &mut self.re, &mut self.im)),
                Decimator::Quarter(hb) => quarter(&mut self.tail, iq, &mut |iq| hb.process_quarter(iq, SCALE, &mut self.re, &mut self.im)),
            }
            let from = self.phase.len();
            self.disc.process(&self.re, &self.im, &mut self.phase);
            self.afc.add(&self.phase[from..]);
        }
        // Handed on by swapping buffers, not copying
        debug_assert!(out.video20.is_empty());
        std::mem::swap(&mut out.video20, &mut self.phase);
    }

    /// Auto-tune: the amount to retune by, in Hz, if the signal is off the channel. A move
    /// shifts the video level by the same amount: `nudge` gets what to add to the level offset.
    pub fn afc(&mut self, hlocked: bool, nudge: &mut dyn FnMut(f32)) -> Option<f64> {
        let hz = self.afc.step(hlocked)?;
        nudge((hz / DEVIATION) as f32);
        Some(hz)
    }

    /// The last measured offset of the signal from the channel, Hz.
    pub fn signal_offset(&self) -> f64 {
        self.afc.last
    }
}

/// The mean instantaneous frequency says where the signal is. While horizontal sync is
/// unlocked the channel moves there from 0.75 MHz off; once locked only from 2.5 MHz, since the
/// picture itself moves that mean by a MHz or so. The measurement after a move straddles it.
struct Afc {
    sum: f64,
    count: usize,
    settling: bool,
    last: f64,
}

impl Afc {
    const SAMPLES: usize = (0.05 * IQ_RATE) as usize;

    fn new() -> Afc {
        Afc { sum: 0.0, count: 0, settling: false, last: 0.0 }
    }

    fn reset(&mut self) {
        self.sum = 0.0;
        self.count = 0;
        self.settling = true;
    }

    fn add(&mut self, video: &[f32]) {
        // Eight partial sums, so it vectorizes; with four it did not
        let chunks = video.chunks_exact(8);
        let rest: f32 = chunks.remainder().iter().sum();
        let s = chunks.fold([0.0f32; 8], |mut s, c| {
            for (s, &v) in s.iter_mut().zip(c) {
                *s += v;
            }
            s
        });
        self.sum += (s.iter().sum::<f32>() + rest) as f64;
        self.count += video.len();
    }

    fn step(&mut self, hlocked: bool) -> Option<f64> {
        if self.count < Self::SAMPLES {
            return None;
        }
        let hz = self.sum / self.count as f64 * DEVIATION;
        self.sum = 0.0;
        self.count = 0;
        self.last = hz;
        if self.settling {
            self.settling = false;
            return None;
        }
        let deadband = if hlocked { 2.5e6 } else { 0.75e6 };
        if hz.abs() < deadband {
            return None;
        }
        self.settling = true;
        Some(hz)
    }
}

pub struct Back {
    pub sync: LineSync,
    pub video: Video,
    /// Brightness, notched at the colour carrier of the standard in `ntsc`.
    luma: RealLowPass<6>,
    ntsc: bool,
    y: Vec<f32>,
}

impl Back {
    pub fn new() -> Back {
        Back { sync: LineSync::new(line_omega(Standard::Pal)), video: Video::new(), luma: RealLowPass::new(&luma_taps(PAL_FSC, IQ_RATE)), ntsc: false, y: Vec::new() }
    }

    pub fn set_standard(&mut self, std: Standard) {
        if let Some(omega) = self.video.set_standard(std) {
            self.sync.set_omega(omega);
        }
    }

    pub fn hlocked(&self) -> bool {
        self.sync.locked > HLOCKED
    }

    /// Feed video; `field` runs as each field finishes, with the line sync's lock.
    pub fn process(&mut self, video: &VideoBlock, field: &mut dyn FnMut(&Video, i32)) {
        let ntsc = self.video.effective() == Standard::Ntsc;
        if ntsc != self.ntsc {
            self.ntsc = ntsc;
            self.luma = RealLowPass::new(&luma_taps(if ntsc { NTSC_FSC } else { PAL_FSC }, IQ_RATE));
        }
        self.y.clear();
        self.luma.process(&video.video20, &mut self.y);
        let v = &mut self.video;
        // Blocks made while colour was off carry none
        let colour = v.colour && video.cr.len() == self.y.len();
        self.sync.run(&self.y, &video.cr, &video.ci, colour, &mut |line, cr, ci, locked| {
            let r = v.line(line, cr, ci);
            if r.field_done {
                field(v, locked);
            }
            r.retune
        });
    }
}

/// Channel power: tune to each centre of a plan in turn, skip what arrives while the chip
/// settles, then average an FFT and read every channel the centre covers over its +-5 MHz.
pub struct Scanner {
    rate: f64,
    fft: Arc<dyn rustfft::Fft<f32>>,
    window: Vec<f32>,
    buf: Vec<Complex32>,
    acc: Vec<f32>,
    frames: usize,
    channels: Vec<f64>,
    plan: Vec<(f64, Vec<usize>)>,
    step: usize,
    settle: usize,
    measured: usize,
    pub powers: Vec<f32>,
}

pub enum ScanStep {
    Continue,
    /// Tune the chip to this centre, Hz.
    Tune(f64),
    /// Every channel measured: mean power per channel in `powers`, linear.
    Done,
}

/// Centres covering every channel: each within USABLE of a centre, and at least DC_GUARD from
/// it so the DC spike stays out of the channel's power.
fn plan(channels: &[f64], usable: f64) -> Vec<(f64, Vec<usize>)> {
    const DC_GUARD: f64 = 6e6;
    let mut order: Vec<usize> = (0..channels.len()).collect();
    order.sort_by(|&a, &b| channels[a].total_cmp(&channels[b]));
    let mut done = vec![false; channels.len()];
    let mut plan = Vec::new();
    while let Some(&first) = order.iter().find(|&&i| !done[i]) {
        let center = channels[first] + usable;
        let covered: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&i| !done[i] && (channels[i] - center).abs() <= usable && (channels[i] - center).abs() >= DC_GUARD)
            .collect();
        for &i in &covered {
            done[i] = true;
        }
        plan.push((center, covered));
    }
    plan
}

/// The channels worth checking for video after a sweep, strongest first, one per frequency:
/// those standing out of the band's noise, whatever their level. `db` is each channel's power
/// over the median channel's, so the noise sits around 0 and spreads by the median absolute
/// deviation; a channel needs three of those spreads, and MIN_DB at least, so a quiet band's
/// ripple does not qualify while a weak transmitter in it does.
pub fn candidates(db: &[f32], hz: &[f64]) -> Vec<usize> {
    const MIN_DB: f32 = 1.5;
    let mut spread: Vec<f32> = db.iter().map(|d| d.abs()).collect();
    spread.sort_by(f32::total_cmp);
    let mad = spread.get(spread.len() / 2).copied().unwrap_or(0.0);
    let threshold = (3.0 * 1.4826 * mad).max(MIN_DB);
    let mut picked: Vec<usize> = (0..db.len()).filter(|&i| db[i] > threshold).collect();
    picked.sort_by(|&a, &b| db[b].total_cmp(&db[a]));
    let mut seen = Vec::new();
    picked.retain(|&i| {
        let new = !seen.contains(&hz[i]);
        seen.push(hz[i]);
        new
    });
    picked
}

impl Scanner {
    const N: usize = 2048;
    const SETTLE_S: f64 = 0.03;
    const MEASURE_S: f64 = 0.02;

    pub fn new(rate: f64) -> Scanner {
        let fft = rustfft::FftPlanner::new().plan_fft_forward(Self::N);
        let window = (0..Self::N).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / Self::N as f64).cos()) as f32).collect();
        Scanner { rate, fft, window, buf: vec![Complex32::new(0.0, 0.0); Self::N], acc: vec![0.0; Self::N], frames: 0, channels: Vec::new(), plan: Vec::new(), step: 0, settle: 0, measured: 0, powers: Vec::new() }
    }

    /// Start a sweep over `channels` (Hz); returns the first centre to tune to.
    pub fn start(&mut self, channels: Vec<f64>) -> f64 {
        // The outer eighth of each side is the filters' edge
        self.plan = plan(&channels, 0.375 * self.rate);
        self.channels = channels;
        self.powers = vec![0.0; self.channels.len()];
        self.step = 0;
        self.begin();
        self.plan[0].0
    }

    /// The share of the sweep's centres measured, 0 to 1.
    pub fn progress(&self) -> f32 {
        self.step as f32 / self.plan.len().max(1) as f32
    }

    fn begin(&mut self) {
        self.settle = (Self::SETTLE_S * self.rate) as usize;
        self.measured = 0;
        self.frames = 0;
        self.acc.iter_mut().for_each(|a| *a = 0.0);
    }

    pub fn feed(&mut self, iq: &[i16]) -> ScanStep {
        let skip = self.settle.min(iq.len() / 2);
        self.settle -= skip;
        for frame in iq[2 * skip..].chunks_exact(2 * Self::N) {
            for ((b, x), &w) in self.buf.iter_mut().zip(frame.chunks_exact(2)).zip(&self.window) {
                *b = Complex32::new(x[0] as f32 * w, x[1] as f32 * w);
            }
            self.fft.process(&mut self.buf);
            for (a, b) in self.acc.iter_mut().zip(&self.buf) {
                *a += b.norm_sqr();
            }
            self.frames += 1;
            self.measured += Self::N;
        }
        if self.settle > 0 || (self.measured as f64) < Self::MEASURE_S * self.rate {
            return ScanStep::Continue;
        }
        let (center, covered) = &self.plan[self.step];
        let n = Self::N as f64;
        for &ch in covered {
            let off = self.channels[ch] - center;
            let (mut sum, mut bins) = (0.0f64, 0usize);
            for k in 0..Self::N {
                // Bin k is (k - N/2) * rate / N once the FFT's halves are swapped
                let f = (((k + Self::N / 2) % Self::N) as f64 - n / 2.0) * self.rate / n;
                if (f - off).abs() <= 5e6 && f.abs() > 1e6 {
                    sum += self.acc[k] as f64;
                    bins += 1;
                }
            }
            self.powers[ch] = (sum / (bins.max(1) * self.frames.max(1)) as f64) as f32;
        }
        self.step += 1;
        if self.step == self.plan.len() {
            return ScanStep::Done;
        }
        self.begin();
        ScanStep::Tune(self.plan[self.step].0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_covers_every_channel_once() {
        let ch: Vec<f64> = crate::channels::all().iter().map(|c| c.hz()).collect();
        let p = plan(&ch, 0.375 * SCAN_RATE);
        let mut seen = vec![0; ch.len()];
        for (center, covered) in &p {
            for &i in covered {
                seen[i] += 1;
                let off = (ch[i] - center).abs();
                assert!((6e6..=30e6).contains(&off), "{} at {off}", ch[i]);
            }
        }
        assert!(seen.iter().all(|&n| n == 1), "{seen:?}");
        assert!(p.len() <= 14, "{} centres", p.len());
    }

    #[test]
    fn candidates_stand_out_of_the_noise() {
        // A quiet band with a weak transmitter, a strong one, and a second name for its frequency
        let mut db: Vec<f32> = (0..48).map(|i| [-0.4, 0.3, -0.2, 0.5, 0.0, -0.3][i % 6]).collect();
        let mut hz: Vec<f64> = (0..48).map(|i| 5.6e9 + i as f64 * 1e6).collect();
        db[10] = 2.5;
        db[20] = 18.0;
        db[30] = 18.0;
        hz[30] = hz[20];
        assert_eq!(candidates(&db, &hz), vec![20, 10]);
        // A busy band: its own spread decides, so only the outlier passes
        let noisy: Vec<f32> = (0..48).map(|i| [-3.0, 2.5, -1.0, 3.0, 0.0, -2.0][i % 6]).collect();
        let mut db = noisy.clone();
        db[7] = 12.0;
        assert_eq!(candidates(&db, &hz), vec![7]);
        assert!(candidates(&noisy, &hz).is_empty());
    }

    #[test]
    fn block_sizes_do_not_matter() {
        let iq: Vec<i16> = (0..200_000).map(|i| ((i * 7919) % 4000) as i16 - 2000).collect();
        for rate in [20e6, 40e6, 80e6] {
            let mut whole = VideoBlock::default();
            Front::new(rate).process(&iq, &mut whole);
            let mut parts = Vec::new();
            let mut front = Front::new(rate);
            let mut pos = 0;
            for len in [2, 6, 14, 7778, 30, 16384, 10, iq.len() - 24224] {
                let mut block = VideoBlock::default();
                front.process(&iq[pos..pos + len], &mut block);
                parts.extend_from_slice(&block.video20);
                pos += len;
            }
            assert_eq!(whole.video20, parts, "at {rate}");
        }
    }
}
