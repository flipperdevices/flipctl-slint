//! The WAV header a take is written under, and how long a take is.
//!
//! The prototype asked `soxi -D` for every file's length, which is a process per row
//! and sox on the device for nothing else. The three formats this app writes all say
//! their own length in a few bytes: a WAV in its data chunk, an OGG in the granule
//! position of its last page, and an MP3 from lame in the Xing tag of its first
//! frame. A file that does not is shown without one.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

/// A 44-byte WAV header for `data` bytes of 16-bit little-endian PCM.
///
/// Written with a size of zero when a take starts and again with the real one when
/// it ends, so a take cut off by a crash still opens, as a file with no length.
pub fn wav_header(data: u32, rate: u32, channels: u16) -> [u8; 44] {
    let block = channels * 2;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&data.saturating_add(36).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * u32::from(block)).to_le_bytes());
    h[32..34].copy_from_slice(&block.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

/// How long the take at `path` plays for, by its extension.
pub fn of(path: &Path) -> Option<Duration> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    match ext.as_str() {
        "wav" => wav(&mut file, len),
        "ogg" => ogg(&mut file, len),
        "mp3" => mp3(&mut file, len),
        _ => None,
    }
}

fn u32_le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u32_be(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Where a WAV's samples are and what they are.
pub struct WavLayout {
    pub rate: u32,
    pub channels: u16,
    pub bits: u16,
    /// 1 for integer PCM.
    pub format: u16,
    pub data_start: u64,
    pub data_len: u64,
}

/// A WAV's `fmt ` and `data` chunks. A data size the writer never came back to fill
/// in, zero or the 4GB placeholder, is read as the rest of the file.
pub fn wav_layout(file: &mut File, len: u64) -> Option<WavLayout> {
    let mut head = vec![0u8; 4096.min(len as usize)];
    file.read_exact(&mut head).ok()?;
    if head.get(0..4)? != b"RIFF" || head.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut at = 12;
    let mut fmt = None;
    while at + 8 <= head.len() {
        let id = &head[at..at + 4];
        let size = u32_le(&head, at + 4);
        let body = at + 8;
        if id == b"fmt " && body + 16 <= head.len() {
            let u16_at = |o: usize| u16::from_le_bytes([head[body + o], head[body + o + 1]]);
            fmt = Some((u16_at(0), u16_at(2), u32_le(&head, body + 4), u16_at(14)));
        } else if id == b"data" {
            let rest = len.saturating_sub(body as u64);
            let data_len = if size == 0 || u64::from(size) > rest { rest } else { u64::from(size) };
            let (format, channels, rate, bits) = fmt?;
            return (rate > 0 && channels > 0).then_some(WavLayout {
                rate,
                channels,
                bits,
                format,
                data_start: body as u64,
                data_len,
            });
        }
        at = body + size as usize + (size as usize & 1);
    }
    None
}

/// The data chunk's size over the format's byte rate.
fn wav(file: &mut File, len: u64) -> Option<Duration> {
    let w = wav_layout(file, len)?;
    let block = u64::from(w.channels) * u64::from(w.bits.max(8) / 8);
    Some(Duration::from_millis(w.data_len / block * 1000 / u64::from(w.rate)))
}

/// The last page's granule position, which for Vorbis is the sample count, over the
/// rate in the identification header at the front.
fn ogg(file: &mut File, len: u64) -> Option<Duration> {
    let mut head = [0u8; 64];
    file.read_exact(&mut head).ok()?;
    if &head[0..4] != b"OggS" {
        return None;
    }
    let packet = 27 + usize::from(head[26]);
    if head.get(packet..packet + 7)? != b"\x01vorbis" {
        return None;
    }
    let rate = u64::from(u32_le(&head, packet + 12));
    let tail = len.min(65536);
    file.seek(SeekFrom::Start(len - tail)).ok()?;
    let mut end = vec![0u8; tail as usize];
    file.read_exact(&mut end).ok()?;
    let page = end.windows(4).rposition(|w| w == b"OggS")?;
    let granule = u64::from_le_bytes(end.get(page + 6..page + 14)?.try_into().ok()?);
    if rate == 0 || granule == u64::MAX {
        return None;
    }
    Some(Duration::from_millis(granule * 1000 / rate))
}

/// The frame count lame puts in its Xing or Info tag, or failing that the file's
/// size at the first frame's bitrate, which is right for a constant-rate file.
fn mp3(file: &mut File, len: u64) -> Option<Duration> {
    let mut head = vec![0u8; 8192.min(len as usize)];
    file.read_exact(&mut head).ok()?;
    let mut at = 0;
    if head.starts_with(b"ID3") && head.len() >= 10 {
        let size = head[6..10].iter().fold(0usize, |n, b| (n << 7) | usize::from(b & 0x7f));
        at = 10 + size;
        if at + 4 > head.len() {
            file.seek(SeekFrom::Start(at as u64)).ok()?;
            head = vec![0u8; 8192.min(len.saturating_sub(at as u64) as usize)];
            file.read_exact(&mut head).ok()?;
            at = 0;
        }
    }
    let start = at as u64;
    let frame = head.get(at..)?.windows(2).position(|w| w[0] == 0xff && w[1] & 0xe0 == 0xe0)? + at;
    let h = head.get(frame..frame + 4)?;
    let version = (h[1] >> 3) & 3;
    let layer = (h[1] >> 1) & 3;
    if layer != 1 {
        return None;
    }
    let rates = [11025u64, 12000, 8000];
    let rate = match ((h[2] >> 2) & 3, version) {
        (3, _) => return None,
        (i, 3) => rates[usize::from(i)] * 4,
        (i, 2) => rates[usize::from(i)] * 2,
        (i, 0) => rates[usize::from(i)],
        _ => return None,
    };
    let mono = (h[3] >> 6) == 3;
    let mpeg1 = version == 3;
    let side = match (mpeg1, mono) {
        (true, true) => 17,
        (true, false) => 32,
        (false, true) => 9,
        (false, false) => 17,
    };
    let spf = if mpeg1 { 1152 } else { 576 };
    let tag = frame + 4 + side;
    if let Some(id) = head.get(tag..tag + 4) {
        if (id == b"Xing" || id == b"Info") && u32_be(&head, tag + 4) & 1 == 1 {
            let frames = u64::from(u32_be(&head, tag + 8));
            return Some(Duration::from_millis(frames * spf * 1000 / rate));
        }
    }
    let kbps = [
        [0u64, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ][usize::from(!mpeg1)]
    .get(usize::from(h[2] >> 4))
    .copied()
    .filter(|k| *k > 0)?;
    let bytes = len.saturating_sub(start + (frame - at) as u64);
    Some(Duration::from_millis(bytes * 8 / kbps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("recorder-length-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        File::create(&path).unwrap().write_all(bytes).unwrap();
        path
    }

    #[test]
    fn a_wav_is_its_data_over_its_byte_rate() {
        let mut bytes = wav_header(96_000 * 3, 48_000, 1).to_vec();
        bytes.resize(44 + 96_000 * 3, 0);
        assert_eq!(of(&temp("three.wav", &bytes)), Some(Duration::from_secs(3)));
    }

    /// A take the app never closed: the header still says zero.
    #[test]
    fn an_unfinished_wav_is_the_rest_of_the_file() {
        let mut bytes = wav_header(0, 48_000, 1).to_vec();
        bytes.resize(44 + 48_000, 0);
        assert_eq!(of(&temp("half.wav", &bytes)), Some(Duration::from_millis(500)));
    }

    #[test]
    fn an_ogg_is_its_last_granule_over_its_rate() {
        let mut first = b"OggS".to_vec();
        first.resize(26, 0);
        first.push(1);
        first.push(30);
        first.extend_from_slice(b"\x01vorbis");
        first.extend_from_slice(&0u32.to_le_bytes());
        first.push(1);
        first.extend_from_slice(&48_000u32.to_le_bytes());
        first.resize(200, 0);
        let mut last = b"OggS\0\x04".to_vec();
        last.extend_from_slice(&(48_000u64 * 7).to_le_bytes());
        last.resize(60, 0);
        first.extend_from_slice(&last);
        assert_eq!(of(&temp("seven.ogg", &first)), Some(Duration::from_secs(7)));
    }

    /// MPEG-1 layer III, 48kHz mono, with lame's Info tag counting 125 frames:
    /// 125 * 1152 / 48000 = 3s.
    #[test]
    fn an_mp3_is_its_xing_frames() {
        let mut bytes = vec![0xff, 0xfb, 0x94, 0xc4];
        bytes.resize(4 + 17, 0);
        bytes.extend_from_slice(b"Info");
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&125u32.to_be_bytes());
        bytes.resize(600, 0);
        assert_eq!(of(&temp("three.mp3", &bytes)), Some(Duration::from_secs(3)));
    }

    /// No tag: the size at the first frame's bitrate, here 128kbps for 16000 bytes.
    #[test]
    fn an_untagged_mp3_is_its_size_at_its_bitrate() {
        let mut bytes = vec![0xff, 0xfb, 0x94, 0x04];
        bytes.resize(16_000, 0);
        assert_eq!(of(&temp("cbr.mp3", &bytes)), Some(Duration::from_millis(1000)));
    }

    #[test]
    fn anything_else_has_no_length() {
        assert_eq!(of(&temp("noise.wav", b"not a riff file at all")), None);
        assert_eq!(of(&temp("noise.txt", b"RIFF")), None);
    }
}
