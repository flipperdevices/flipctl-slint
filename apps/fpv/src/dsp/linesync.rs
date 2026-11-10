// Horizontal sync: resamples 10 Msps video into lines of LINE_SIZE samples, keeping the sync
// pulse at the start of each. A port of the SDR++ FPV plugin's LineSync (from SDR++'s ATV
// decoder), with the same fixed-point timing: period and phase in units of 2^-30 input samples.
// Finished lines go straight to the handler on this thread. The colour carrier at baseband
// is resampled at the same instants, when asked for.

pub const LINE_SIZE: usize = 640;
pub const SYNC_LEN: usize = 47;
const SYNC_SIDE_LEN: usize = 12;
const SYNC_L_START: usize = LINE_SIZE - SYNC_SIDE_LEN;
const SYNC_R_START: usize = SYNC_LEN / 2;
const SYNC_R_END: usize = SYNC_R_START + SYNC_LEN / 2 + SYNC_SIDE_LEN;
const SYNC_HALF_LEN: usize = SYNC_LEN / 2 + SYNC_SIDE_LEN;
const MAX_LOCK: i32 = 1000;

const PHASES: usize = 128;
const TAPS: usize = 8;
const PERIOD_WINDOW: f64 = 0.002;
const ONE: f64 = (1u32 << 30) as f64;

pub struct LineSync {
    omega_gain: f32,
    mu_gain: f32,
    bank: Vec<[f32; TAPS]>,
    buffer: Vec<f32>,
    cr: Vec<f32>,
    ci: Vec<f32>,
    line: Vec<f32>,
    line_cr: Vec<f32>,
    line_ci: Vec<f32>,
    period: u32,
    min_period: u32,
    max_period: u32,
    phase: u32,
    offset: i32,
    pixel: usize,
    pub locked: i32,
    pub fast_lock: bool,
}

/// SDR++'s windowed sinc (Nuttall window, as it calls it) split into a polyphase bank.
fn interp_bank() -> Vec<[f32; TAPS]> {
    let count = PHASES * TAPS;
    let omega = 2.0 * std::f64::consts::PI * (0.5 / PHASES as f64);
    let half = count as f64 / 2.0;
    let corr = PHASES as f64 * omega / std::f64::consts::PI;
    let coefs = [0.355768, 0.487396, 0.144232, 0.012604];
    let nuttall = |n: f64, big_n: f64| -> f64 {
        let mut w = 0.0;
        let mut sign = 1.0;
        for (i, c) in coefs.iter().enumerate() {
            w += sign * c * (i as f64 * 2.0 * std::f64::consts::PI * n / big_n).cos();
            sign = -sign;
        }
        w
    };
    let mut bank = vec![[0.0f32; TAPS]; PHASES];
    for i in 0..count {
        let t = i as f64 - half + 0.5;
        let x = t * omega;
        let sinc = if x == 0.0 { 1.0 } else { x.sin() / x };
        let tap = sinc * nuttall(t - half, count as f64) * corr;
        bank[(PHASES - 1) - (i % PHASES)][i / PHASES] = tap as f32;
    }
    bank
}

impl LineSync {
    pub fn new(omega: f64) -> LineSync {
        let mut s = LineSync {
            omega_gain: 1e-6,
            mu_gain: 1.0,
            bank: interp_bank(),
            buffer: vec![0.0; TAPS - 1],
            cr: vec![0.0; TAPS - 1],
            ci: vec![0.0; TAPS - 1],
            line: vec![0.0; LINE_SIZE],
            line_cr: vec![0.0; LINE_SIZE],
            line_ci: vec![0.0; LINE_SIZE],
            period: 0,
            min_period: 0,
            max_period: 0,
            phase: 0,
            offset: 0,
            pixel: 0,
            locked: 0,
            fast_lock: true,
        };
        s.set_omega(omega);
        s
    }

    /// Input samples per output sample: 1.0 for PAL at 10 Msps, the loop staying within
    /// PERIOD_WINDOW of it.
    pub fn set_omega(&mut self, omega: f64) {
        self.max_period = (omega * (1.0 + PERIOD_WINDOW) * ONE) as u32;
        self.min_period = (omega * (1.0 - PERIOD_WINDOW) * ONE) as u32;
        self.period = (omega * ONE) as u32;
        self.locked = 0;
    }

    /// Feed video, and with `colour` the colour carrier (cr, ci) alongside it; each finished
    /// line goes to `handler` (brightness, colour re and im, lock), which may return a new omega.
    pub fn run(&mut self, input: &[f32], cr: &[f32], ci: &[f32], colour: bool, handler: &mut dyn FnMut(&mut [f32], &[f32], &[f32], i32) -> Option<f64>) {
        let count = input.len() as i32;
        self.buffer.truncate(TAPS - 1);
        self.buffer.extend_from_slice(input);
        self.cr.truncate(TAPS - 1);
        self.ci.truncate(TAPS - 1);
        if colour {
            self.cr.extend_from_slice(&cr[..input.len()]);
            self.ci.extend_from_slice(&ci[..input.len()]);
        } else {
            self.cr.resize(TAPS - 1 + input.len(), 0.0);
            self.ci.resize(TAPS - 1 + input.len(), 0.0);
        }
        let dot = |x: &[f32], t: &[f32; TAPS]| ((x[0] * t[0] + x[1] * t[1]) + (x[2] * t[2] + x[3] * t[3])) + ((x[4] * t[4] + x[5] * t[5]) + (x[6] * t[6] + x[7] * t[7]));

        // Noise instead of video can drive the loop anywhere: a bounded number of lines per call
        let mut lines_left = count / LINE_SIZE as i32 + 8;
        while self.offset < count {
            if lines_left <= 0 {
                self.offset = count;
                self.phase = 0;
                break;
            }
            {
                let (mut off, mut px, mut ph) = (self.offset, self.pixel, self.phase);
                let per = self.period;
                while off < 0 && px < LINE_SIZE {
                    self.line[px] = 0.0;
                    self.line_cr[px] = 0.0;
                    self.line_ci[px] = 0.0;
                    px += 1;
                    ph = ph.wrapping_add(per);
                    off += (ph >> 30) as i32;
                    ph &= 0x3FFF_FFFF;
                }
                while off < count && px < LINE_SIZE {
                    let t = &self.bank[((ph >> 23) & 0x7F) as usize];
                    let at = off as usize..off as usize + TAPS;
                    self.line[px] = dot(&self.buffer[at.clone()], t);
                    if colour {
                        self.line_cr[px] = dot(&self.cr[at.clone()], t);
                        self.line_ci[px] = dot(&self.ci[at], t);
                    }
                    px += 1;
                    ph = ph.wrapping_add(per);
                    off += (ph >> 30) as i32;
                    ph &= 0x3FFF_FFFF;
                }
                self.offset = off;
                self.pixel = px;
                self.phase = ph;
            }

            if self.pixel == LINE_SIZE {
                let line = &self.line;
                let left: f32 = line[SYNC_L_START..].iter().sum::<f32>() + line[..SYNC_R_START].iter().sum::<f32>();
                let right: f32 = line[SYNC_R_START..SYNC_R_END].iter().sum();

                // The error, bounded: unbounded it made the phase loops spin
                let mut error = (left - right) * (1.0 / SYNC_HALF_LEN as f32);
                if !error.is_finite() {
                    error = 0.0;
                }
                let error = error.clamp(-1.0, 1.0);
                let period_delta = error * self.omega_gain;
                let mut phase_delta = error * self.mu_gain;
                while phase_delta <= -1.0 {
                    phase_delta += 1.0;
                    self.offset -= 1;
                }
                while phase_delta >= 1.0 {
                    phase_delta -= 1.0;
                    self.offset += 1;
                }
                self.period = self.period.wrapping_add((period_delta * ONE as f32) as i32 as u32).clamp(self.min_period, self.max_period);
                self.phase = self.phase.wrapping_add((phase_delta * ONE as f32) as i32 as u32);
                let overflow = (self.phase >> 30) as i32;
                if overflow != 0 {
                    if error < 0.0 {
                        self.offset -= 4 - overflow;
                    } else {
                        self.offset += overflow;
                    }
                }
                self.phase &= 0x3FFF_FFFF;

                let mut lowest = f32::INFINITY;
                let mut lowest_id = 0usize;
                for (i, &v) in line.iter().enumerate() {
                    if v < lowest {
                        lowest = v;
                        lowest_id = i;
                    }
                }
                let line_locked = lowest_id < SYNC_R_END || lowest_id >= SYNC_L_START;
                if !line_locked && self.locked > 0 {
                    self.locked -= 1;
                } else if line_locked && self.locked < MAX_LOCK {
                    self.locked += 1;
                }
                // Not locked: force the sync to where it belongs
                if self.locked == 0 && self.fast_lock {
                    self.offset += lowest_id as i32 - SYNC_R_START as i32;
                    self.locked = MAX_LOCK / 2;
                }
                // A line that lands far outside this buffer is lost sync, not a correction
                if self.offset < -(LINE_SIZE as i32) || self.offset > count + LINE_SIZE as i32 {
                    self.offset = 0;
                    self.phase = 0;
                }

                if let Some(omega) = handler(&mut self.line, &self.line_cr, &self.line_ci, self.locked) {
                    self.set_omega(omega);
                }
                self.pixel = 0;
                lines_left -= 1;
            }
        }
        self.offset -= count;
        self.buffer.drain(..count as usize);
        self.cr.drain(..count as usize);
        self.ci.drain(..count as usize);
    }
}
