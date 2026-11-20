//! Saving what goes over the cable, one file per direction, in ~/Recordings.
//!
//! WAV and FLAC are written by `pw-record` itself, through the system's libsndfile.
//! It has no MP3 writer, so for MP3 it hands the raw sound over and LAME, compiled
//! into the app as the voice recorder has it, encodes it here.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;

use crate::graph::OURS;
use crate::route::{end, spawn};

const RATE: u32 = 48000;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Format {
    Wav,
    Flac,
    Mp3,
}

pub const FORMATS: [Format; 3] = [Format::Wav, Format::Flac, Format::Mp3];

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Wav => "WAV",
            Format::Flac => "FLAC",
            Format::Mp3 => "MP3",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Format::Wav => "wav",
            Format::Flac => "flac",
            Format::Mp3 => "mp3",
        }
    }
}

/// `~/Recordings`, where the voice recorder keeps its takes too. `/home` is shared by
/// every profile, so a recording survives switching one.
pub fn folder() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
    home.join("Recordings")
}

/// A file's name: when it was started, and which way the sound went.
pub fn file_name(stamp: &str, way: &str, format: Format) -> String {
    format!("usb-audio-{stamp}-{way}.{}", format.extension())
}

/// One direction being written to a file, until dropped, which finishes it.
pub struct Recording {
    child: Child,
    encoder: Option<JoinHandle<()>>,
}

impl Recording {
    /// Record `target`, a node by name, into `path`. `sink` reads what goes into a
    /// sink rather than what a source gives.
    pub fn start(
        name: &str,
        target: &str,
        sink: bool,
        path: &Path,
        format: Format,
    ) -> io::Result<Self> {
        // On PipeWire's own timer, not the PC's clock, which stops while the PC is
        // silent and would leave the pause out of the file instead of keeping it as
        // silence: the dummy driver outranks every device, and the card follows it.
        let props = format!(
            "node.name={OURS}.rec.{name} stream.capture.sink={sink} node.group=pipewire.dummy"
        );
        let mut cmd = Command::new("pw-record");
        cmd.args(["--target", target, "-P", &props]).args([
            "--rate",
            "48000",
            "--channels",
            "2",
            "--format",
            "s16",
        ]);
        if format != Format::Mp3 {
            cmd.arg(path);
            let child = spawn(&mut cmd, Stdio::null())?;
            return Ok(Recording { child, encoder: None });
        }
        let mut lame = Mp3::create(path)?;
        let mut child = spawn(cmd.args(["--raw", "-"]), Stdio::piped())?;
        let mut out = child.stdout.take().ok_or_else(|| io::Error::other("pw-record"))?;
        let encoder = std::thread::spawn(move || {
            // 10 ms of stereo s16 a read
            let mut buf = [0u8; 1920];
            let mut pcm = Vec::with_capacity(buf.len() / 2);
            while out.read_exact(&mut buf).is_ok() {
                pcm.clear();
                pcm.extend(buf.as_chunks::<2>().0.iter().map(|s| i16::from_le_bytes(*s)));
                if lame.write(&pcm).is_err() {
                    break;
                }
            }
            let _ = lame.finish();
        });
        Ok(Recording { child, encoder: Some(encoder) })
    }
}

impl Drop for Recording {
    /// pw-record closes its file properly when told to stop, and for MP3 the encoder
    /// writes LAME's last frames and its tag once the sound runs out.
    fn drop(&mut self) {
        end(&mut self.child);
        if let Some(encoder) = self.encoder.take() {
            let _ = encoder.join();
        }
    }
}

/// LAME at VBR quality 2, joint stereo: about 190 kbit/s, fit for music.
struct Mp3 {
    file: BufWriter<File>,
    enc: Box<mp3lame_encoder::Encoder>,
    out: Vec<u8>,
}

fn lame_error(e: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("lame: {e:?}"))
}

impl Mp3 {
    fn create(path: &Path) -> io::Result<Self> {
        use mp3lame_encoder::{Builder, Mode, Quality, VbrMode};
        let mut b = Builder::new().ok_or_else(|| io::Error::other("lame"))?;
        b.set_num_channels(2).map_err(lame_error)?;
        b.set_sample_rate(RATE).map_err(lame_error)?;
        b.set_mode(Mode::JointStereo).map_err(lame_error)?;
        b.set_vbr_mode(VbrMode::Mtrh).map_err(lame_error)?;
        b.set_vbr_quality(Quality::NearBest).map_err(lame_error)?;
        b.set_to_write_vbr_tag(true).map_err(lame_error)?;
        let enc = Box::new(b.build().map_err(lame_error)?);
        Ok(Mp3 { file: BufWriter::new(File::create(path)?), enc, out: Vec::new() })
    }

    fn write(&mut self, pcm: &[i16]) -> io::Result<()> {
        self.out.clear();
        self.out.reserve(mp3lame_encoder::max_required_buffer_size(pcm.len() / 2));
        self.enc
            .encode_to_vec(mp3lame_encoder::InterleavedPcm(pcm), &mut self.out)
            .map_err(lame_error)?;
        self.file.write_all(&self.out)
    }

    /// LAME's last frames, then the tag at the head of the file that says how many
    /// there are, so a player knows the length of a VBR file.
    fn finish(mut self) -> io::Result<()> {
        self.out.clear();
        self.out.reserve(7200);
        self.enc.flush_to_vec::<mp3lame_encoder::FlushNoGap>(&mut self.out).map_err(lame_error)?;
        self.file.write_all(&self.out)?;
        let mut file = self.file.into_inner().map_err(|e| e.into_error())?;
        let mut tag = Vec::with_capacity(self.enc.lame_tag_size());
        if self.enc.lame_tag_size() > 0 && self.enc.lame_tag_encode_to_vec(&mut tag).is_some() {
            file.seek(SeekFrom::Start(self.enc.id3v2_tag_size() as u64))?;
            file.write_all(&tag)?;
        }
        file.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_named_by_its_start_and_its_way() {
        assert_eq!(
            file_name("20261120-121500", "from", Format::Flac),
            "usb-audio-20261120-121500-from.flac"
        );
        assert_eq!(
            file_name("20261120-121500", "to", Format::Mp3),
            "usb-audio-20261120-121500-to.mp3"
        );
    }

    /// A second of a tone through LAME makes an MP3 a player can read: frame sync
    /// after the tag, and the Xing frame count filled in.
    #[test]
    fn lame_writes_a_finished_mp3() {
        let dir = std::env::temp_dir().join(format!("bridge-mp3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.mp3");
        let mut mp3 = Mp3::create(&path).unwrap();
        let pcm: Vec<i16> = (0..RATE)
            .flat_map(|i| {
                let v = (4000.0 * (std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin())
                    as i16;
                [v, v]
            })
            .collect();
        mp3.write(&pcm).unwrap();
        mp3.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(bytes.len() > 1000, "{} bytes", bytes.len());
        assert_eq!(bytes[0], 0xff, "frame sync");
        assert!(bytes.windows(4).any(|w| w == b"Xing" || w == b"Info"), "VBR tag");
    }
}
