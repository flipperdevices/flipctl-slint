// Lines to fields: level control, vertical sync, PAL/NTSC, colour, and a 768 x 576 RGB picture.
// A port of the SDR++ FPV plugin's FpvVideo, with colour added.

use super::linesync::{LINE_SIZE, SYNC_LEN};

const EQUAL_LEN: usize = 23;
const HBLANK_START: usize = SYNC_LEN;
const HBLANK_END: usize = 105;
const HBLANK_LEN: usize = HBLANK_END - HBLANK_START + 1;
const SYNC_LEVEL: f32 = -0.428;

/// The colour burst: 5.6 us after the start of line sync, 2.25 us long, at 10 Msps; its
/// amplitude in brightness units (black to white is 1), the same in PAL and NTSC.
const BURST_START: usize = 57;
const BURST_LEN: usize = 22;
const BURST_AMP: f32 = 0.214;
/// Below this share of a full burst there is no colour to decode.
const COLOUR_KILL: f32 = 0.25;

/// Black and white levels after the level control (blanking 0, sync SYNC_LEVEL): PAL's are
/// 0 and 1; NTSC's sync is 40 IRE against PAL's 43, so its 100 IRE white is 1.07, and its
/// black is 7.5 IRE of setup above blanking.
fn levels(std: Standard) -> (f32, f32) {
    match std {
        Standard::Ntsc => (0.0803, 1.0700),
        _ => (0.0, 1.0),
    }
}

/// 52 us of picture (520 samples at 10 Msps) after 10.5 us of blanking, shown 4:3 in both
/// standards so the picture keeps its size when the standard changes.
pub const WIDTH: usize = 768;
pub const HEIGHT: usize = 576;
const PICTURE_SAMPLES: usize = 520;
const FIRST_PIXEL: usize = 105;
/// No vertical sync within this many lines starts a new field anyway.
const FREE_RUN_LINES: i32 = 340;
/// Fields without vertical lock before Auto tries the other standard.
const HUNT_FIELDS: i32 = 25;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Standard {
    Auto,
    Pal,
    Ntsc,
}

struct Geometry {
    first_active: i32,
    active_lines: i32,
}

fn geometry(std: Standard) -> Geometry {
    match std {
        Standard::Ntsc => Geometry { first_active: 14, active_lines: 240 },
        _ => Geometry { first_active: 19, active_lines: 288 },
    }
}

/// Line sync omega for a standard: LINE_SIZE samples per PAL line at 10 Msps.
pub fn line_omega(std: Standard) -> f64 {
    match std {
        Standard::Ntsc => (10e6 / 15734.264) / LINE_SIZE as f64,
        _ => 1.0,
    }
}

#[derive(Default)]
pub struct LineResult {
    pub field_done: bool,
    /// The line sync should move to this omega.
    pub retune: Option<f64>,
}

pub struct Video {
    pub standard: Standard,
    pub invert: bool,
    pub detected: Standard,
    pub field_lines: i32,
    pub vlock: i32,
    /// Fields ended by a vertical sync at a length PAL or NTSC has, counted for ever: the
    /// scan's check for video looks at how many arrive.
    pub good_fields: u32,
    pub offset: f32,
    pub gain: f32,
    /// Decode colour, when the signal has it.
    pub colour: bool,
    /// The last line had a colour burst strong enough to decode.
    pub has_colour: bool,
    /// RGB, WIDTH x HEIGHT.
    pub pixels: Vec<u8>,
    prev_burst: (f32, f32),
    burst: (f32, f32),
    burst_amp: f32,
    /// The -U axis, smoothed over lines.
    axis: (f32, f32),
    /// PAL: this line's V sign, alternating, and how well measurements agree with it.
    v_sign: f32,
    v_agree: f32,
    prev_u: Vec<f32>,
    prev_v: Vec<f32>,
    line_no: i32,
    broad_run: i32,
    line_std: Standard,
    unlocked_fields: i32,
    /// The broad pulses that started the field before this one.
    last_broad_run: i32,
    /// This field was started a line further down: its parity.
    lowered: bool,
    /// The parity of the field just completed, for whoever weaves fields together.
    pub field_lowered: bool,
}

impl Video {
    pub fn new() -> Video {
        Video {
            standard: Standard::Auto,
            invert: false,
            detected: Standard::Pal,
            field_lines: 0,
            vlock: 0,
            good_fields: 0,
            offset: 0.0,
            gain: 1.0,
            colour: true,
            has_colour: false,
            pixels: vec![0; WIDTH * HEIGHT * 3],
            prev_burst: (0.0, 0.0),
            burst: (0.0, 0.0),
            burst_amp: 0.0,
            axis: (0.0, 0.0),
            v_sign: 1.0,
            v_agree: 0.0,
            prev_u: vec![0.0; WIDTH],
            prev_v: vec![0.0; WIDTH],
            line_no: 0,
            broad_run: 0,
            line_std: Standard::Pal,
            unlocked_fields: 0,
            last_broad_run: 0,
            lowered: false,
            field_lowered: false,
        }
    }

    /// The standard being drawn: the forced one, or in Auto the current guess.
    pub fn effective(&self) -> Standard {
        if self.standard != Standard::Auto {
            self.standard
        } else {
            self.line_std
        }
    }

    /// Picture lines per field in the standard being drawn.
    pub fn active_lines(&self) -> usize {
        geometry(self.effective()).active_lines as usize
    }

    /// Returns the omega to retune the line sync to, if any.
    pub fn set_standard(&mut self, std: Standard) -> Option<f64> {
        self.standard = std;
        self.tune(if std != Standard::Auto { std } else { self.line_std })
    }

    fn tune(&mut self, std: Standard) -> Option<f64> {
        if std == self.line_std {
            return None;
        }
        self.line_std = std;
        self.unlocked_fields = 0;
        self.vlock = 0;
        Some(line_omega(std))
    }

    /// One line: brightness `data`, and the colour carrier at baseband (cr, ci).
    pub fn line(&mut self, data: &mut [f32], cr: &[f32], ci: &[f32]) -> LineResult {
        let mut result = LineResult::default();
        if self.invert {
            data.iter_mut().for_each(|v| *v = -*v);
        }
        let (offset, gain) = (self.offset, self.gain);
        data.iter_mut().for_each(|v| *v = (*v + offset) * gain);

        let sync_l = data[..EQUAL_LEN].iter().sum::<f32>() / EQUAL_LEN as f32;
        let sync_r = data[EQUAL_LEN..SYNC_LEN].iter().sum::<f32>() / (SYNC_LEN - EQUAL_LEN) as f32;
        let sync_level = (sync_l + sync_r) * 0.5;
        let blank = data[HBLANK_START..HBLANK_START + HBLANK_LEN].iter().sum::<f32>() / HBLANK_LEN as f32;

        self.offset = (self.offset - (blank / self.gain) * 0.001).clamp(-1.0, 1.0);
        self.gain = (self.gain - (blank - sync_level + SYNC_LEVEL) * 0.01).clamp(0.1, 10.0);

        // A broad pulse keeps the signal at sync level past a normal line sync; a run of them is
        // vertical sync, and the field starts when the run ends.
        let broad = sync_l < 0.5 * SYNC_LEVEL && sync_r < 0.5 * SYNC_LEVEL && blank < 0.5 * SYNC_LEVEL;
        if broad {
            self.broad_run += 1;
        } else {
            if self.broad_run >= 2 && self.line_no > self.field_window().0 {
                self.end_field(true, &mut result);
                // Interlace: the fields' vertical syncs are half a line apart, and in PAL the
                // runs read as 3 and 2 lines. Counted from where the longer run ends, that
                // field was drawn a whole line higher than the other on a still scene, and the
                // picture bounced at the frame rate. Start it a line further down.
                if self.broad_run == self.last_broad_run + 1 {
                    self.line_no = 1;
                    self.lowered = true;
                }
                self.last_broad_run = self.broad_run;
            }
            self.broad_run = 0;
        }

        let colour = self.colour && self.decode_burst(cr, ci);
        self.has_colour = colour;

        // Each field fills the whole image: a line repeated over the rows it covers
        let g = geometry(self.effective());
        let row = self.line_no - g.first_active;
        if row >= 0 && row < g.active_lines {
            let top = (row as usize * HEIGHT) / g.active_lines as usize;
            let bottom = ((row as usize + 1) * HEIGHT) / g.active_lines as usize;
            let stride = 3 * WIDTH;
            if colour {
                self.colour_row(data, cr, ci, top);
            } else {
                let (black, white) = levels(self.effective());
                let k = 255.0 / (white - black);
                let first = &mut self.pixels[top * stride..(top + 1) * stride];
                for (i, p) in first.chunks_exact_mut(3).enumerate() {
                    let y = ((data[FIRST_PIXEL + i * PICTURE_SAMPLES / WIDTH] - black) * k).clamp(0.0, 255.0) as u8;
                    p.copy_from_slice(&[y, y, y]);
                }
            }
            for r in top + 1..bottom {
                self.pixels.copy_within(top * stride..(top + 1) * stride, r * stride);
            }
        }

        self.line_no += 1;
        if self.line_no >= self.field_window().1 {
            self.end_field(false, &mut result);
        }
        result
    }

    /// Measure this line's burst and update the colour reference from it; false if the
    /// bursts are too weak to decode colour by.
    ///
    /// The burst sits on -U. In PAL it is 45 degrees to one side of it on lines where V is
    /// sent as is and to the other where V is inverted, so the sum of two lines' bursts lies
    /// on -U; NTSC's burst sits on -U itself. One line's burst is too noisy at low signal to
    /// noise to decode by: the axis is smoothed over about ten lines, as it only drifts with
    /// the carrier's frequency error, and PAL's V switch is taken to alternate, each line's
    /// burst only voting on which way round.
    fn decode_burst(&mut self, cr: &[f32], ci: &[f32]) -> bool {
        let n = BURST_LEN as f32;
        let br = cr[BURST_START..BURST_START + BURST_LEN].iter().sum::<f32>() / n;
        let bi = ci[BURST_START..BURST_START + BURST_LEN].iter().sum::<f32>() / n;
        self.prev_burst = self.burst;
        self.burst = (br, bi);
        let pal = self.effective() != Standard::Ntsc;
        let (ar, ai) = if pal { (br + self.prev_burst.0, bi + self.prev_burst.1) } else { (br, bi) };
        self.axis = (0.9 * self.axis.0 + 0.1 * ar, 0.9 * self.axis.1 + 0.1 * ai);
        if pal {
            self.v_sign = -self.v_sign;
            // Which side of the axis this burst lies on: positive is the V-inverted side
            let side = bi * self.axis.0 - br * self.axis.1;
            let vote = if side > 0.0 { -1.0 } else { 1.0 };
            self.v_agree = 0.9 * self.v_agree + 0.1 * vote * self.v_sign;
            if self.v_agree < 0.0 {
                self.v_sign = -self.v_sign;
                self.v_agree = -self.v_agree;
            }
        }
        // Mixing to baseband halves the carrier, and luma is scaled by the level control
        let amp = (br * br + bi * bi).sqrt();
        self.burst_amp = 0.9 * self.burst_amp + 0.1 * amp;
        2.0 * self.burst_amp * self.gain > COLOUR_KILL * BURST_AMP
    }

    /// Decode one line in colour into image row `row`, by the reference decode_burst keeps.
    /// Colour is scaled by the measured burst, so neither gain nor pre-emphasis changes it,
    /// and averaged with the previous line's, as a PAL delay line does.
    fn colour_row(&mut self, data: &[f32], cr: &[f32], ci: &[f32], row: usize) {
        let theta = self.axis.1.atan2(self.axis.0);
        let v_sign = if self.effective() != Standard::Ntsc { self.v_sign } else { 1.0 };
        // Rotate -U onto the positive real axis and scale by the burst, then all of it from
        // black to white
        let (black, white) = levels(self.effective());
        let span = 1.0 / (white - black);
        let k = BURST_AMP / self.burst_amp.max(1e-6) * span;
        let (rc, rs) = ((theta + std::f32::consts::PI).cos() * k, (theta + std::f32::consts::PI).sin() * k);
        let stride = 3 * WIDTH;
        let out = &mut self.pixels[row * stride..(row + 1) * stride];
        for (i, p) in out.chunks_exact_mut(3).enumerate() {
            let s = FIRST_PIXEL + i * PICTURE_SAMPLES / WIDTH;
            let (c, d) = (cr[s], ci[s]);
            let u = c * rc + d * rs;
            let v = (d * rc - c * rs) * v_sign;
            let (ua, va) = ((u + self.prev_u[i]) * 0.5, (v + self.prev_v[i]) * 0.5);
            self.prev_u[i] = u;
            self.prev_v[i] = v;
            let y = (data[s] - black) * span;
            let rgb = [y + 1.140 * va, y - 0.395 * ua - 0.581 * va, y + 2.032 * ua];
            for (p, c) in p.iter_mut().zip(rgb) {
                *p = (c * 255.0).clamp(0.0, 255.0) as u8;
            }
        }
    }

    /// After how many lines a vertical sync is believed, and after how many the field ends
    /// without one. Once the field rate is locked, only around where a field of the standard
    /// ends: interference that reads as a broad pulse mid-field otherwise cut the field
    /// short, and one over the real sync let it run on to FREE_RUN_LINES, a field and a third
    /// of NTSC. Unlocked, anywhere past line 100, so it can find the sync again. (Changed in
    /// flipctl's copy, with the black rows in end_field.)
    fn field_window(&self) -> (i32, i32) {
        if self.vlock <= 5 {
            return (100, FREE_RUN_LINES);
        }
        match self.effective() {
            Standard::Ntsc => (250, 275),
            _ => (300, 330),
        }
    }

    fn end_field(&mut self, synced: bool, result: &mut LineResult) {
        // The rows this field never reached are not left as the last field that did reach
        // them. A field cut short, by a vertical sync early or interference that reads as
        // one, otherwise left an older picture under the bottom of the new one, the lower
        // the rows the older. Locked, they are black; with nothing locked the field is noise,
        // and they are this field's own rows from the top again, so no signal reads as snow
        // rather than snow with black bands. (Changed in flipctl's copy; the receiver keeps
        // them.)
        let g = geometry(self.effective());
        let drawn = (self.line_no - g.first_active).clamp(0, g.active_lines) as usize;
        let from = drawn * HEIGHT / g.active_lines as usize;
        let stride = 3 * WIDTH;
        if self.vlock > 5 || from == 0 {
            self.pixels[from * stride..].fill(0);
        } else {
            for r in from..HEIGHT {
                self.pixels.copy_within((r - from) * stride..(r - from + 1) * stride, r * stride);
            }
        }
        if synced {
            self.field_lines = self.line_no;
            let pal = (300..=330).contains(&self.field_lines);
            let ntsc = (250..=275).contains(&self.field_lines);
            if pal {
                self.detected = Standard::Pal;
            }
            if ntsc {
                self.detected = Standard::Ntsc;
            }
            if pal || ntsc {
                self.good_fields = self.good_fields.wrapping_add(1);
            }
            if (pal || ntsc) && self.vlock < 20 {
                self.vlock += 1;
            } else if !pal && !ntsc && self.vlock > 0 {
                self.vlock -= 1;
            }
        } else if self.vlock > 0 {
            self.vlock -= 1;
        }
        result.field_done = true;
        self.line_no = 0;
        self.field_lowered = self.lowered;
        self.lowered = false;

        // Auto: follow a field length that says the other standard, and while nothing locks,
        // try the other line length every HUNT_FIELDS fields.
        if self.standard == Standard::Auto {
            if self.vlock > 5 && self.detected != self.line_std {
                result.retune = self.tune(self.detected).or(result.retune);
            } else if self.vlock > 5 {
                self.unlocked_fields = 0;
            } else {
                self.unlocked_fields += 1;
                if self.unlocked_fields >= HUNT_FIELDS {
                    let other = if self.line_std == Standard::Pal { Standard::Ntsc } else { Standard::Pal };
                    result.retune = self.tune(other).or(result.retune);
                }
            }
        }
    }
}
