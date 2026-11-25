//! Sound to MP3 by LAME, compiled into the app, cut into the frames the MP4 stores.
//!
//! Constant bitrate and no Xing tag: the MP4's index already says how long the track
//! is, and a tag frame at the front would play as a frame of silence.

use std::io;

use mp3lame_encoder::{
    Bitrate, Builder, Encoder, FlushNoGap, InterleavedPcm, Mode, MonoPcm, Quality,
};

pub const RATE: u32 = 48_000;

pub struct Mp3 {
    encoder: Box<Encoder>,
    channels: u16,
    out: Vec<u8>,
    pending: Vec<u8>,
}

/// The bitrate for `channels`, in bits a second.
pub fn bitrate(channels: u16) -> u32 {
    if channels == 1 {
        128_000
    } else {
        192_000
    }
}

impl Mp3 {
    pub fn new(channels: u16) -> io::Result<Self> {
        let bad = |e: mp3lame_encoder::BuildError| io::Error::other(format!("lame: {e:?}"));
        let mut b = Builder::new().ok_or_else(|| io::Error::other("lame"))?;
        b.set_num_channels(channels as u8).map_err(bad)?;
        b.set_sample_rate(RATE).map_err(bad)?;
        b.set_mode(if channels == 1 { Mode::Mono } else { Mode::JointStereo }).map_err(bad)?;
        b.set_brate(if channels == 1 { Bitrate::Kbps128 } else { Bitrate::Kbps192 })
            .map_err(bad)?;
        b.set_quality(Quality::Good).map_err(bad)?;
        b.set_to_write_vbr_tag(false).map_err(bad)?;
        Ok(Self {
            encoder: Box::new(b.build().map_err(bad)?),
            channels,
            out: Vec::new(),
            pending: Vec::new(),
        })
    }

    /// Samples in, interleaved for stereo, and whatever whole frames that completes.
    pub fn encode(&mut self, pcm: &[i16]) -> io::Result<Vec<Vec<u8>>> {
        self.out.clear();
        self.out.reserve(mp3lame_encoder::max_required_buffer_size(pcm.len()));
        let done = if self.channels == 1 {
            self.encoder.encode_to_vec(MonoPcm(pcm), &mut self.out)
        } else {
            self.encoder.encode_to_vec(InterleavedPcm(pcm), &mut self.out)
        };
        done.map_err(|e| io::Error::other(format!("lame: {e:?}")))?;
        self.pending.extend_from_slice(&self.out);
        Ok(take_frames(&mut self.pending))
    }

    /// The last frames LAME was holding.
    pub fn finish(mut self) -> io::Result<Vec<Vec<u8>>> {
        self.out.clear();
        self.out.reserve(7200);
        self.encoder
            .flush_to_vec::<FlushNoGap>(&mut self.out)
            .map_err(|e| io::Error::other(format!("lame: {e:?}")))?;
        self.pending.extend_from_slice(&self.out);
        let mut frames = take_frames(&mut self.pending);
        // Whatever is left is a frame cut short; padding it keeps the stream whole.
        if let Some(len) = frame_len(&self.pending) {
            self.pending.resize(len, 0);
            frames.push(std::mem::take(&mut self.pending));
        }
        Ok(frames)
    }
}

/// The whole frames at the front of `stream`, taken out of it.
fn take_frames(stream: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut at = 0;
    while let Some(len) = frame_len(&stream[at..]) {
        if at + len > stream.len() {
            break;
        }
        frames.push(stream[at..at + len].to_vec());
        at += len;
    }
    stream.drain(..at);
    frames
}

/// The length of the MPEG-1 Layer III frame whose header starts `bytes`.
pub fn frame_len(bytes: &[u8]) -> Option<usize> {
    let h = bytes.get(..4)?;
    // Sync, MPEG-1, Layer III.
    if h[0] != 0xff || h[1] & 0xfe != 0xfa {
        return None;
    }
    const KBPS: [usize; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const HZ: [usize; 3] = [44_100, 48_000, 32_000];
    let kbps = *KBPS.get(usize::from(h[2] >> 4)).filter(|k| **k > 0)?;
    let hz = *HZ.get(usize::from((h[2] >> 2) & 3))?;
    let padding = usize::from((h[2] >> 1) & 1);
    Some(144 * kbps * 1000 / hz + padding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_says_how_long_its_frame_is() {
        // 160 kbps at 48 kHz: 480 bytes, 481 padded.
        assert_eq!(frame_len(&[0xff, 0xfb, 0xa4, 0x00]), Some(480));
        assert_eq!(frame_len(&[0xff, 0xfb, 0xa6, 0x00]), Some(481));
        assert_eq!(frame_len(&[0xff, 0xf3, 0xa4, 0x00]), None, "MPEG-2");
        assert_eq!(frame_len(&[0x49, 0x44, 0x33, 0x04]), None, "an ID3 tag");
    }

    #[test]
    fn a_second_of_sound_is_whole_frames_of_1152_samples() {
        for channels in [1u16, 2] {
            let mut mp3 = Mp3::new(channels).unwrap();
            let n = RATE as usize * usize::from(channels);
            let tone: Vec<i16> =
                (0..n).map(|i| ((i as f32 / 20.0).sin() * 8000.0) as i16).collect();
            let mut frames = Vec::new();
            for chunk in tone.chunks(960 * usize::from(channels)) {
                frames.extend(mp3.encode(chunk).unwrap());
            }
            frames.extend(mp3.finish().unwrap());
            for f in &frames {
                assert_eq!(frame_len(f), Some(f.len()));
            }
            // A second is 41.7 frames; the flush leaves out a last partial one.
            assert!((41..=43).contains(&frames.len()), "{channels}: {} frames", frames.len());
        }
    }
}
