//! Grey frames to H.264 by OpenH264, compiled into the app.
//!
//! The panel is black ink on an orange ground, so a frame is coloured the way the
//! browser view shows it: each grey multiplied by Flipper orange, white becoming
//! `#FF8200` and black staying black, in full range. H.264 keeps one chroma sample
//! per 2x2 pixels, which is why the frames arrive here enlarged (`SCALE`). The
//! quantiser is pinned low: the pixel fonts are one pixel wide, and anything coarser
//! rings around every glyph. A flipctl screen at QP 12 comes back within 3 grey levels
//! everywhere, where OpenH264's default of 26 was off by 66 around the text.
//!
//! The pin is a QP range of one value under quality rate control with a bitrate no
//! frame reaches. With rate control off OpenH264 takes the layer's own QP instead,
//! which the crate does not expose, and ignores the range; 12 is also its floor.
//!
//! A frame that changes everywhere in single pixels can come out larger at QP 12 than
//! the 55 KB buffer OpenH264 gives it, and an overrun is not a lost frame: the encoder
//! tears itself down. Such a frame is encoded again by a fresh encoder at `COARSE_QP`,
//! which keeps going until the next key picture is due, when `QP` is tried again: dense
//! content tends to stay dense, a game more than a menu. Every instance writes the same
//! parameter sets, since the picture's initial QP in them is always 26.

use std::time::Duration;

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, QpRange,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;

/// The quantiser for every frame.
const QP: u8 = 12;

/// The quantiser for a frame too dense to fit at `QP`.
const COARSE_QP: u8 = 36;

/// The longest a player has to go back for a picture to start from.
const KEY_EVERY: Duration = Duration::from_secs(2);

/// One picture as the MP4 stores it.
pub struct Sample {
    /// NAL units, each after its four-byte length.
    pub data: Vec<u8>,
    pub key: bool,
}

/// Flipper orange, the panel's ground.
const ORANGE: [f32; 3] = [255.0, 130.0, 0.0];

/// Each grey as full-range BT.709 Y, Cb and Cr once multiplied by [`ORANGE`].
fn palette() -> [[u8; 3]; 256] {
    let mut out = [[0; 3]; 256];
    for (g, yuv) in out.iter_mut().enumerate() {
        let [r, gr, b] = ORANGE.map(|c| c * g as f32 / 255.0);
        let y = 0.2126 * r + 0.7152 * gr + 0.0722 * b;
        let cb = 128.0 + (b - y) / 1.8556;
        let cr = 128.0 + (r - y) / 1.5748;
        *yuv = [y, cb, cr].map(|v| v.round().clamp(0.0, 255.0) as u8);
    }
    out
}

pub struct Video {
    encoder: Encoder,
    qp: u8,
    width: usize,
    height: usize,
    palette: [[u8; 3]; 256],
    luma: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    last_key_us: Option<u64>,
    /// Encoding at `COARSE_QP` since a frame overran.
    coarse: bool,
}

fn encoder(qp: u8) -> Result<Encoder, String> {
    let config = EncoderConfig::new()
        .usage_type(UsageType::CameraVideoRealTime)
        .complexity(Complexity::Low)
        .rate_control_mode(RateControlMode::Quality)
        .bitrate(BitRate::from_bps(100_000_000))
        .qp(QpRange::new(qp, qp))
        .skip_frames(false)
        .max_frame_rate(FrameRate::from_hz(60.0))
        .intra_frame_period(IntraFramePeriod::from_num_frames(0))
        .num_threads(1)
        .vui(VuiConfig::bt709().full_range(true));
    Encoder::with_api_config(OpenH264API::from_source(), config).map_err(|e| format!("H.264: {e}"))
}

impl Video {
    pub fn new(width: u16, height: u16) -> Result<Self, String> {
        Self::with_qp(width, height, QP)
    }

    fn with_qp(width: u16, height: u16, qp: u8) -> Result<Self, String> {
        let (width, height) = (usize::from(width), usize::from(height));
        Ok(Self {
            encoder: encoder(qp)?,
            qp,
            width,
            height,
            palette: palette(),
            luma: vec![0; width * height],
            cb: vec![128; width.div_ceil(2) * height.div_ceil(2)],
            cr: vec![128; width.div_ceil(2) * height.div_ceil(2)],
            sps: None,
            pps: None,
            last_key_us: None,
            coarse: false,
        })
    }

    /// One frame, committed at `at_us`. `None` when the encoder made no picture of it.
    pub fn encode(&mut self, grey: &[u8], at_us: u64) -> Result<Option<Sample>, String> {
        if self.last_key_us.is_some_and(|k| at_us.saturating_sub(k) >= KEY_EVERY.as_micros() as u64)
        {
            if self.coarse {
                // A fresh encoder starts with a key picture of its own.
                self.encoder = encoder(self.qp)?;
                self.coarse = false;
            } else {
                self.encoder.force_intra_frame();
            }
        }
        self.colour(grey);
        let stream = match self.attempt() {
            Ok(stream) => stream,
            Err(_) if !self.coarse => {
                self.encoder = encoder(COARSE_QP)?;
                self.coarse = true;
                self.attempt()?
            }
            Err(e) => return Err(e),
        };
        let mut sample = Sample { data: Vec::with_capacity(stream.len()), key: false };
        for nal in nal_units(&stream) {
            match nal.first().map(|b| b & 0x1f) {
                Some(7) => drop(self.sps.get_or_insert_with(|| nal.to_vec())),
                Some(8) => drop(self.pps.get_or_insert_with(|| nal.to_vec())),
                Some(kind) => {
                    sample.key |= kind == 5;
                    sample.data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    sample.data.extend_from_slice(nal);
                }
                None => {}
            }
        }
        if sample.key {
            self.last_key_us = Some(at_us);
        }
        Ok((!sample.data.is_empty()).then_some(sample))
    }

    /// The three planes of `grey` in orange, chroma averaged over each 2x2 block.
    fn colour(&mut self, grey: &[u8]) {
        let (w, h, half) = (self.width, self.height, self.width.div_ceil(2));
        for (y, px) in self.luma.iter_mut().zip(grey) {
            *y = self.palette[usize::from(*px)][0];
        }
        for by in 0..h.div_ceil(2) {
            for bx in 0..half {
                let (mut cb, mut cr, mut n) = (0u32, 0u32, 0u32);
                for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (x, y) = (bx * 2 + x, by * 2 + y);
                    if x < w && y < h {
                        let [_, u, v] = self.palette[usize::from(grey[y * w + x])];
                        (cb, cr, n) = (cb + u32::from(u), cr + u32::from(v), n + 1);
                    }
                }
                self.cb[by * half + bx] = ((cb + n / 2) / n) as u8;
                self.cr[by * half + bx] = ((cr + n / 2) / n) as u8;
            }
        }
    }

    /// The current planes through the current encoder, as Annex B.
    fn attempt(&mut self) -> Result<Vec<u8>, String> {
        let half = self.width.div_ceil(2);
        let yuv = YUVSlices::new(
            (&self.luma, &self.cb, &self.cr),
            (self.width, self.height),
            (self.width, half, half),
        );
        Ok(self.encoder.encode(&yuv).map_err(|e| format!("H.264: {e}"))?.to_vec())
    }

    /// The sequence and picture parameter sets, once the first picture has made them.
    pub fn parameter_sets(&self) -> Option<(&[u8], &[u8])> {
        Some((self.sps.as_deref()?, self.pps.as_deref()?))
    }
}

/// How many times the panel's size the video is: 1024x576. Even, so each panel
/// pixel has chroma samples of its own: at the panel's size four pixels share one,
/// and every glyph's edge came out brown, by up to 110 of 255 in a channel, however
/// many bits were spent. Twice was within 7; four times is larger on a screen that
/// plays it smoothed.
pub const SCALE: usize = 4;

/// `grey`, `width` pixels across, with every pixel made `SCALE` by `SCALE`.
pub fn enlarge(grey: &[u8], width: usize, out: &mut Vec<u8>) {
    out.clear();
    for row in grey.chunks_exact(width.max(1)) {
        let start = out.len();
        for px in row {
            out.extend(std::iter::repeat_n(*px, SCALE));
        }
        let wide = out.len() - start;
        for _ in 1..SCALE {
            out.extend_from_within(start..start + wide);
        }
    }
}

/// The NAL units of an Annex B stream, without their start codes.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let mut end = starts.get(n + 1).map_or(stream.len(), |next| next - 3);
            // A four-byte start code leaves its leading zero on the unit before it.
            while end > s && stream[end - 1] == 0 && starts.get(n + 1).is_some() {
                end -= 1;
            }
            &stream[s..end]
        })
        .filter(|nal| !nal.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real screen through the encoder, for FFmpeg to decode and compare:
    /// `SCREEN_FRAME=<256x144 raw grey> cargo test one_real_frame -- --ignored` writes
    /// target/frame.h264.
    #[test]
    #[ignore]
    fn one_real_frame() {
        let grey = std::fs::read(std::env::var("SCREEN_FRAME").unwrap()).unwrap();
        // The panel's size times one to four.
        let scale = [1u16, 2, 3, 4]
            .into_iter()
            .find(|k| grey.len() == 256 * 144 * usize::from(k * k))
            .unwrap();
        let (w, h) = (256 * scale, 144 * scale);
        let mut video = Video::new(w, h).unwrap();
        let sample = video.encode(&grey, 0).unwrap().unwrap();
        let (sps, pps) = video.parameter_sets().unwrap();
        let mut out = Vec::new();
        for nal in [sps, pps] {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        let mut data = &sample.data[..];
        while data.len() > 4 {
            let len = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[4..4 + len]);
            data = &data[4 + len..];
        }
        std::fs::write("target/frame.h264", out).unwrap();
    }

    /// Single-pixel noise overruns the buffer at QP 12; the frames still come out,
    /// coarser, QP 12 is tried again at the next key picture, and the parameter sets
    /// never change.
    #[test]
    fn a_frame_too_dense_for_the_buffer_is_encoded_coarser() {
        let mut video = Video::new(256, 144).unwrap();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut noise = || {
            (0..256 * 144)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    if seed & 1 == 0 {
                        0
                    } else {
                        255
                    }
                })
                .collect::<Vec<u8>>()
        };
        let calm = ui_like();
        video.encode(&calm, 0).unwrap().unwrap();
        let sets = video.parameter_sets().map(|(s, p)| (s.to_vec(), p.to_vec())).unwrap();
        for i in 1..4u64 {
            let dense = video.encode(&noise(), i * 16_000).unwrap().unwrap();
            assert!(dense.data.len() < 60_000);
        }
        assert!(video.coarse);
        let back = video.encode(&calm, 2_100_000).unwrap().unwrap();
        assert!(back.key && !video.coarse, "QP 12 again at the next key picture");
        let mut fresh = Video::with_qp(256, 144, COARSE_QP).unwrap();
        fresh.encode(&calm, 0).unwrap().unwrap();
        let other = fresh.parameter_sets().map(|(s, p)| (s.to_vec(), p.to_vec())).unwrap();
        assert_eq!(sets, other, "one avcC serves every encoder");
    }

    /// Rules and short strokes on white, the density of a flipctl screen.
    fn ui_like() -> Vec<u8> {
        (0..256 * 144)
            .map(|i| {
                let (x, y) = (i % 256, i / 256);
                if y % 16 == 15 || (y % 16 > 4 && y % 16 < 11 && x % 6 < 2 && x % 64 < 40) {
                    0
                } else {
                    255
                }
            })
            .collect()
    }

    #[test]
    fn white_is_flipper_orange_and_black_stays_black() {
        let p = palette();
        assert_eq!(p[0], [0, 128, 128]);
        // #FF8200 in full-range BT.709.
        assert_eq!(p[255], [147, 49, 196]);
    }

    #[test]
    fn a_frame_is_enlarged_pixel_for_pixel() {
        let mut out = Vec::new();
        enlarge(&[1, 2, 3, 4], 2, &mut out);
        let wide = 2 * SCALE;
        assert_eq!(out.len(), wide * wide);
        for (i, px) in out.iter().enumerate() {
            let (x, y) = (i % wide / SCALE, i / wide / SCALE);
            assert_eq!(*px, [1, 2, 3, 4][y * 2 + x], "at {i}");
        }
    }

    #[test]
    fn start_codes_of_either_length_split_the_stream() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        assert_eq!(nal_units(&stream), vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
    }

    #[test]
    fn the_first_picture_is_a_key_with_its_parameter_sets() {
        let mut video = Video::new(256, 144).unwrap();
        let frame = ui_like();
        let first = video.encode(&frame, 0).unwrap().unwrap();
        assert!(first.key);
        let (sps, pps) = video.parameter_sets().unwrap();
        assert_eq!((sps[0] & 0x1f, pps[0] & 0x1f), (7, 8));
        let second = video.encode(&frame, 16_000).unwrap().unwrap();
        assert!(!second.key, "an unchanged frame is predicted");
        assert!(second.data.len() < first.data.len());
        let later = video.encode(&frame, 2_100_000).unwrap().unwrap();
        assert!(later.key, "a key picture at least every two seconds");
    }
}
