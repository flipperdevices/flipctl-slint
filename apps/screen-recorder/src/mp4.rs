//! An MP4 with one H.264 track and, when there is sound, one MP3 track.
//!
//! Samples go into `mdat` as they are made and the index is written after them when
//! the recording ends, so nothing is held in memory but sizes and offsets. Each sample
//! is its own chunk, which keeps the index to one table per track at the cost of a
//! few bytes a sample. Video samples carry their own times, since flipctl sends a
//! frame only when the screen changes; audio samples are MP3 frames of 1152 samples.
//!
//! MP3 is stored as MPEG-1 audio (object type 0x6B), which browsers, QuickTime and
//! every player built on FFmpeg read.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// Ticks per second on the video track, the usual one for video.
pub const VIDEO_SCALE: u32 = 90_000;

/// Samples per MP3 frame at 48 kHz, MPEG-1 Layer III.
pub const MP3_FRAME: u32 = 1152;

/// The movie's own timescale, for the durations in the headers.
const MOVIE_SCALE: u32 = 1000;

/// The sound as the audio track describes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sound {
    pub rate: u32,
    pub channels: u16,
    pub bitrate: u32,
}

#[derive(Default)]
struct Track {
    sizes: Vec<u32>,
    offsets: Vec<u64>,
    /// Video: when each sample starts, in ticks. Audio has one duration for all.
    starts: Vec<u64>,
    /// Video: 1-based numbers of the samples a player can start from.
    sync: Vec<u32>,
}

pub struct Mp4 {
    file: BufWriter<File>,
    at: u64,
    mdat: u64,
    width: u16,
    height: u16,
    sps: Vec<u8>,
    pps: Vec<u8>,
    video: Track,
    sound: Option<Sound>,
    audio: Track,
}

impl Mp4 {
    pub fn create(path: &Path, width: u16, height: u16, sound: Option<Sound>) -> io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        let mut head = Vec::new();
        bx(&mut head, b"ftyp", |o| {
            o.extend_from_slice(b"isom");
            o.extend_from_slice(&512u32.to_be_bytes());
            for brand in [b"isom", b"iso2", b"avc1", b"mp41"] {
                o.extend_from_slice(brand);
            }
        });
        let mdat = head.len() as u64;
        // A 64-bit size, filled in at the end: size 1 says the real one follows.
        head.extend_from_slice(&1u32.to_be_bytes());
        head.extend_from_slice(b"mdat");
        head.extend_from_slice(&0u64.to_be_bytes());
        file.write_all(&head)?;
        Ok(Self {
            file,
            at: head.len() as u64,
            mdat,
            width,
            height,
            sps: Vec::new(),
            pps: Vec::new(),
            video: Track::default(),
            sound,
            audio: Track::default(),
        })
    }

    /// The encoder's parameter sets, which go in the index rather than the stream.
    pub fn set_parameter_sets(&mut self, sps: &[u8], pps: &[u8]) {
        self.sps = sps.to_vec();
        self.pps = pps.to_vec();
    }

    /// One picture, length-prefixed NAL units, starting `start` ticks into the movie.
    pub fn video(&mut self, start: u64, data: &[u8], key: bool) -> io::Result<()> {
        let start = start.max(self.video.starts.last().map_or(0, |s| s + 1));
        self.video.starts.push(start);
        if key {
            self.video.sync.push(self.video.sizes.len() as u32 + 1);
        }
        let at = self.write(data)?;
        self.video.offsets.push(at);
        self.video.sizes.push(data.len() as u32);
        Ok(())
    }

    /// One MP3 frame.
    pub fn audio(&mut self, frame: &[u8]) -> io::Result<()> {
        let at = self.write(frame)?;
        self.audio.offsets.push(at);
        self.audio.sizes.push(frame.len() as u32);
        Ok(())
    }

    fn write(&mut self, data: &[u8]) -> io::Result<u64> {
        let at = self.at;
        self.file.write_all(data)?;
        self.at += data.len() as u64;
        Ok(at)
    }

    /// The index, with the movie ending `end` ticks in: the last picture stays up
    /// until then.
    pub fn finish(mut self, end: u64) -> io::Result<()> {
        let mdat_size = self.at - self.mdat;
        let moov = self.moov(end);
        self.file.write_all(&moov)?;
        let mut file = self.file.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(self.mdat + 8))?;
        file.write_all(&mdat_size.to_be_bytes())?;
        file.sync_all()
    }

    fn moov(&self, end: u64) -> Vec<u8> {
        let durations = video_durations(&self.video.starts, end);
        let video_ticks: u64 = durations.iter().map(|d| u64::from(*d)).sum();
        let audio = self.sound.filter(|_| !self.audio.sizes.is_empty());
        let audio_ticks = audio.map_or(0, |_| self.audio.sizes.len() as u64 * u64::from(MP3_FRAME));
        let movie = |ticks: u64, scale: u32| ticks * u64::from(MOVIE_SCALE) / u64::from(scale);
        let video_ms = movie(video_ticks, VIDEO_SCALE);
        let audio_ms = audio.map_or(0, |s| movie(audio_ticks, s.rate));

        let mut o = Vec::new();
        bx(&mut o, b"moov", |o| {
            full(o, b"mvhd", 0, 0, |o| {
                put32(o, 0);
                put32(o, 0);
                put32(o, MOVIE_SCALE);
                put32(o, video_ms.max(audio_ms) as u32);
                put32(o, 0x0001_0000);
                put16(o, 0x0100);
                o.extend_from_slice(&[0; 10]);
                matrix(o);
                o.extend_from_slice(&[0; 24]);
                put32(o, if audio.is_some() { 3 } else { 2 });
            });
            self.video_trak(o, video_ms, video_ticks, &durations);
            if let Some(sound) = audio {
                self.audio_trak(o, sound, audio_ms, audio_ticks);
            }
        });
        o
    }

    fn video_trak(&self, o: &mut Vec<u8>, ms: u64, ticks: u64, durations: &[u32]) {
        bx(o, b"trak", |o| {
            tkhd(o, 1, ms, 0, self.width, self.height);
            bx(o, b"mdia", |o| {
                mdhd(o, VIDEO_SCALE, ticks);
                hdlr(o, b"vide", b"VideoHandler\0");
                bx(o, b"minf", |o| {
                    full(o, b"vmhd", 0, 1, |o| o.extend_from_slice(&[0; 8]));
                    dinf(o);
                    bx(o, b"stbl", |o| {
                        full(o, b"stsd", 0, 0, |o| {
                            put32(o, 1);
                            bx(o, b"avc1", |o| {
                                o.extend_from_slice(&[0; 6]);
                                put16(o, 1);
                                o.extend_from_slice(&[0; 16]);
                                put16(o, self.width);
                                put16(o, self.height);
                                put32(o, 0x0048_0000);
                                put32(o, 0x0048_0000);
                                put32(o, 0);
                                put16(o, 1);
                                o.extend_from_slice(&[0; 32]);
                                put16(o, 0x0018);
                                put16(o, 0xffff);
                                bx(o, b"avcC", |o| avcc(o, &self.sps, &self.pps));
                            });
                        });
                        stts(o, durations);
                        if self.video.sync.len() < self.video.sizes.len() {
                            full(o, b"stss", 0, 0, |o| {
                                put32(o, self.video.sync.len() as u32);
                                for n in &self.video.sync {
                                    put32(o, *n);
                                }
                            });
                        }
                        chunks(o, &self.video);
                    });
                });
            });
        });
    }

    fn audio_trak(&self, o: &mut Vec<u8>, sound: Sound, ms: u64, ticks: u64) {
        bx(o, b"trak", |o| {
            tkhd(o, 2, ms, 0x0100, 0, 0);
            bx(o, b"mdia", |o| {
                mdhd(o, sound.rate, ticks);
                hdlr(o, b"soun", b"SoundHandler\0");
                bx(o, b"minf", |o| {
                    full(o, b"smhd", 0, 0, |o| put32(o, 0));
                    dinf(o);
                    bx(o, b"stbl", |o| {
                        full(o, b"stsd", 0, 0, |o| {
                            put32(o, 1);
                            bx(o, b"mp4a", |o| {
                                o.extend_from_slice(&[0; 6]);
                                put16(o, 1);
                                o.extend_from_slice(&[0; 8]);
                                put16(o, sound.channels);
                                put16(o, 16);
                                put32(o, 0);
                                put32(o, sound.rate << 16);
                                full(o, b"esds", 0, 0, |o| esds(o, sound.bitrate));
                            });
                        });
                        stts(o, &vec![MP3_FRAME; self.audio.sizes.len()]);
                        chunks(o, &self.audio);
                    });
                });
            });
        });
    }
}

/// How long each picture is up: until the next one, and the last until `end`.
fn video_durations(starts: &[u64], end: u64) -> Vec<u32> {
    starts
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let next = starts.get(i + 1).copied().unwrap_or(end.max(s + 1));
            (next - s).min(u64::from(u32::MAX)) as u32
        })
        .collect()
}

fn put16(o: &mut Vec<u8>, v: u16) {
    o.extend_from_slice(&v.to_be_bytes());
}

fn put32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_be_bytes());
}

fn bx(o: &mut Vec<u8>, kind: &[u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let at = o.len();
    o.extend_from_slice(&[0; 4]);
    o.extend_from_slice(kind);
    body(o);
    let size = (o.len() - at) as u32;
    o[at..at + 4].copy_from_slice(&size.to_be_bytes());
}

fn full(o: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32, body: impl FnOnce(&mut Vec<u8>)) {
    bx(o, kind, |o| {
        o.push(version);
        o.extend_from_slice(&flags.to_be_bytes()[1..]);
        body(o);
    });
}

fn matrix(o: &mut Vec<u8>) {
    for v in [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000u32] {
        put32(o, v);
    }
}

fn tkhd(o: &mut Vec<u8>, id: u32, ms: u64, volume: u16, width: u16, height: u16) {
    // Enabled and in the movie.
    full(o, b"tkhd", 0, 3, |o| {
        put32(o, 0);
        put32(o, 0);
        put32(o, id);
        put32(o, 0);
        put32(o, ms as u32);
        o.extend_from_slice(&[0; 8]);
        put16(o, 0);
        put16(o, 0);
        put16(o, volume);
        put16(o, 0);
        matrix(o);
        put32(o, u32::from(width) << 16);
        put32(o, u32::from(height) << 16);
    });
}

fn mdhd(o: &mut Vec<u8>, scale: u32, ticks: u64) {
    full(o, b"mdhd", 0, 0, |o| {
        put32(o, 0);
        put32(o, 0);
        put32(o, scale);
        put32(o, ticks.min(u64::from(u32::MAX)) as u32);
        // "und", packed as three 5-bit letters.
        put16(o, 0x55c4);
        put16(o, 0);
    });
}

fn hdlr(o: &mut Vec<u8>, kind: &[u8; 4], name: &[u8]) {
    full(o, b"hdlr", 0, 0, |o| {
        put32(o, 0);
        o.extend_from_slice(kind);
        o.extend_from_slice(&[0; 12]);
        o.extend_from_slice(name);
    });
}

fn dinf(o: &mut Vec<u8>) {
    bx(o, b"dinf", |o| {
        full(o, b"dref", 0, 0, |o| {
            put32(o, 1);
            // The data is in this file.
            full(o, b"url ", 0, 1, |_| {});
        });
    });
}

/// Durations, run-length coded.
fn stts(o: &mut Vec<u8>, durations: &[u32]) {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for d in durations {
        match runs.last_mut() {
            Some((n, last)) if last == d => *n += 1,
            _ => runs.push((1, *d)),
        }
    }
    full(o, b"stts", 0, 0, |o| {
        put32(o, runs.len() as u32);
        for (n, d) in runs {
            put32(o, n);
            put32(o, d);
        }
    });
}

/// One sample per chunk: what that says, the sizes and where each one is.
fn chunks(o: &mut Vec<u8>, track: &Track) {
    full(o, b"stsc", 0, 0, |o| {
        put32(o, 1);
        put32(o, 1);
        put32(o, 1);
        put32(o, 1);
    });
    full(o, b"stsz", 0, 0, |o| {
        put32(o, 0);
        put32(o, track.sizes.len() as u32);
        for s in &track.sizes {
            put32(o, *s);
        }
    });
    full(o, b"co64", 0, 0, |o| {
        put32(o, track.offsets.len() as u32);
        for at in &track.offsets {
            o.extend_from_slice(&at.to_be_bytes());
        }
    });
}

fn avcc(o: &mut Vec<u8>, sps: &[u8], pps: &[u8]) {
    o.push(1);
    o.extend_from_slice(sps.get(1..4).unwrap_or(&[66, 0, 30]));
    // Four-byte lengths before each NAL unit.
    o.push(0xff);
    o.push(0xe1);
    put16(o, sps.len() as u16);
    o.extend_from_slice(sps);
    o.push(1);
    put16(o, pps.len() as u16);
    o.extend_from_slice(pps);
}

/// The MPEG-4 descriptors that say "MP3".
fn esds(o: &mut Vec<u8>, bitrate: u32) {
    let mut config = vec![0x6b, 0x15];
    config.extend_from_slice(&[0, 0, 0]);
    config.extend_from_slice(&bitrate.to_be_bytes());
    config.extend_from_slice(&bitrate.to_be_bytes());
    let mut es = vec![0, 0, 0];
    descriptor(&mut es, 0x04, &config);
    descriptor(&mut es, 0x06, &[0x02]);
    descriptor(o, 0x03, &es);
}

fn descriptor(o: &mut Vec<u8>, tag: u8, body: &[u8]) {
    o.push(tag);
    o.push(body.len() as u8);
    o.extend_from_slice(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_picture_lasts_until_the_end() {
        assert_eq!(video_durations(&[0, 900, 4500], 9000), vec![900, 3600, 4500]);
        assert_eq!(video_durations(&[0], 0), vec![1], "never zero");
    }

    #[test]
    fn equal_durations_are_one_run() {
        let mut o = Vec::new();
        stts(&mut o, &[5, 5, 5, 7]);
        assert_eq!(&o[4..8], b"stts");
        assert_eq!(&o[12..16], &2u32.to_be_bytes());
        assert_eq!(&o[16..24], &[0, 0, 0, 3, 0, 0, 0, 5]);
    }

    #[test]
    fn a_box_knows_its_size() {
        let mut o = Vec::new();
        bx(&mut o, b"outr", |o| full(o, b"innr", 0, 1, |o| o.push(9)));
        assert_eq!(&o[..4], &21u32.to_be_bytes());
        assert_eq!(&o[8..12], &13u32.to_be_bytes());
        assert_eq!(&o[16..20], &[0, 0, 0, 1]);
    }
}
