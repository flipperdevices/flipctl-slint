//! The microphone: one capture stream that feeds the level meter all the time and a
//! take while one is being recorded.
//!
//! The samples come from a record stream on the codec's source over the sound
//! server's socket (pulse.rs), and everything after that happens here, in the one
//! thread that reads them: the channel is chosen, filtered, levelled, measured, and
//! written as WAV, or encoded to MP3 by LAME or to OGG by libvorbis, both compiled
//! into the app. Nothing is run and nothing has to be installed.
//!
//! The codec puts the built-in microphone on the left channel and the headset jack on
//! the right, so choosing the input is choosing a channel, and nothing on the
//! machine changes. The prototype's chain was that channel, a high-pass at 80Hz and
//! a fixed 6dB of gain; a fixed gain left speech 37dB under the peaks of a real take
//! (RMS -38.5dBFS against a -1.6dBFS click), so the gain here is an AGC instead, with
//! a limiter after it.
//!
//! Pausing keeps the stream running and stops the writing, so the meter stays live
//! while a take is held.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use pulseaudio::protocol;

use crate::length::wav_header;
use crate::pulse::{self, Conn};

/// The codec's native rate. Resampling down to 16kHz cut everything above 8kHz and
/// was audible, which is why the prototype went back up.
pub const RATE: u32 = 48_000;

/// The high-pass corner, the prototype's `highpass 80`.
const HIGHPASS_HZ: f32 = 80.0;

/// The three formats, in the order the Format row offers them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    Wav,
    Mp3,
    Ogg,
}

pub const FORMATS: [Format; 3] = [Format::Wav, Format::Mp3, Format::Ogg];

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Wav => "WAV",
            Format::Mp3 => "MP3",
            Format::Ogg => "OGG",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Format::Wav => "wav",
            Format::Mp3 => "mp3",
            Format::Ogg => "ogg",
        }
    }

    /// Bytes a second of voice takes on disk, before a take has run long enough to
    /// measure its own: the prototype's figures for 48kHz mono.
    pub fn nominal_rate(self) -> u64 {
        match self {
            Format::Wav => 96_000,
            Format::Mp3 => 20_625,
            Format::Ogg => 16_000,
        }
    }
}

/// Where a take's samples go. LAME at VBR quality 4 and Vorbis at quality 0.4 are
/// the prototype's `lame -V 4` and `oggenc -q 4`.
enum Sink {
    Wav { file: BufWriter<File>, bytes: u64 },
    Mp3 { file: BufWriter<File>, enc: Box<mp3lame_encoder::Encoder>, out: Vec<u8> },
    Ogg { enc: Box<vorbis_rs::VorbisEncoder<BufWriter<File>>>, block: Vec<f32> },
}

impl Sink {
    fn open(path: &Path, format: Format) -> io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        Ok(match format {
            Format::Wav => {
                file.write_all(&wav_header(0, RATE, 1))?;
                Sink::Wav { file, bytes: 0 }
            }
            Format::Mp3 => {
                use mp3lame_encoder::{Builder, Mode, Quality, VbrMode};
                let bad = |e: mp3lame_encoder::BuildError| io::Error::other(format!("lame: {e:?}"));
                let mut b = Builder::new().ok_or_else(|| io::Error::other("lame"))?;
                b.set_num_channels(1).map_err(bad)?;
                b.set_sample_rate(RATE).map_err(bad)?;
                b.set_mode(Mode::Mono).map_err(bad)?;
                b.set_vbr_mode(VbrMode::Mtrh).map_err(bad)?;
                b.set_vbr_quality(Quality::Nice).map_err(bad)?;
                b.set_to_write_vbr_tag(true).map_err(bad)?;
                Sink::Mp3 { file, enc: Box::new(b.build().map_err(bad)?), out: Vec::new() }
            }
            Format::Ogg => {
                use std::num::{NonZeroU32, NonZeroU8};
                let mut b = vorbis_rs::VorbisEncoderBuilder::new_with_serial(
                    NonZeroU32::new(RATE).unwrap(),
                    NonZeroU8::new(1).unwrap(),
                    file,
                    std::process::id() as i32,
                );
                b.bitrate_management_strategy(vorbis_rs::VorbisBitrateManagementStrategy::QualityVbr {
                    target_quality: 0.4,
                });
                Sink::Ogg { enc: Box::new(b.build().map_err(|e| io::Error::other(e.to_string()))?), block: Vec::new() }
            }
        })
    }

    fn write(&mut self, pcm: &[i16]) -> io::Result<()> {
        match self {
            Sink::Wav { file, bytes } => {
                for s in pcm {
                    file.write_all(&s.to_le_bytes())?;
                }
                *bytes += pcm.len() as u64 * 2;
                Ok(())
            }
            Sink::Mp3 { file, enc, out } => {
                out.clear();
                out.reserve(mp3lame_encoder::max_required_buffer_size(pcm.len()));
                enc.encode_to_vec(mp3lame_encoder::MonoPcm(pcm), out)
                    .map_err(|e| io::Error::other(format!("lame: {e:?}")))?;
                file.write_all(out)
            }
            Sink::Ogg { enc, block } => {
                block.clear();
                block.extend(pcm.iter().map(|s| f32::from(*s) / 32768.0));
                enc.encode_audio_block([&block[..]]).map_err(|e| io::Error::other(e.to_string()))
            }
        }
    }

    /// Close the file properly: the WAV's sizes filled in, LAME's last frames and the
    /// tag that says how many there are, Vorbis's last page.
    fn finish(self) -> io::Result<()> {
        let file = match self {
            Sink::Wav { file, bytes } => {
                let mut file = file.into_inner().map_err(|e| e.into_error())?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&wav_header(bytes.min(u64::from(u32::MAX - 36)) as u32, RATE, 1))?;
                file
            }
            Sink::Mp3 { mut file, mut enc, mut out } => {
                out.clear();
                out.reserve(7200);
                enc.flush_to_vec::<mp3lame_encoder::FlushNoGap>(&mut out)
                    .map_err(|e| io::Error::other(format!("lame: {e:?}")))?;
                file.write_all(&out)?;
                let mut file = file.into_inner().map_err(|e| e.into_error())?;
                let mut tag = Vec::with_capacity(enc.lame_tag_size());
                if enc.lame_tag_size() > 0 && enc.lame_tag_encode_to_vec(&mut tag).is_some() {
                    file.seek(SeekFrom::Start(enc.id3v2_tag_size() as u64))?;
                    file.write_all(&tag)?;
                }
                file
            }
            Sink::Ogg { enc, .. } => {
                let file = (*enc).finish().map_err(|e| io::Error::other(e.to_string()))?;
                file.into_inner().map_err(|e| e.into_error())?
            }
        };
        file.sync_all()
    }
}

/// What the page asks of the capture thread.
enum Ask {
    Start(PathBuf, Format, Sender<io::Result<()>>),
    Stop(Sender<io::Result<()>>),
}

/// What the capture thread and the page share.
struct Shared {
    /// The loudest sample since the page last asked, 0 to 32767.
    peak: AtomicU32,
    /// 0 the left channel, the built-in microphone; 1 the right, the jack.
    channel: AtomicUsize,
    paused: AtomicBool,
    /// Samples written to the current take, which is its length.
    frames: AtomicU64,
    /// Set when a write failed, usually a full disk: the take is over.
    failed: AtomicBool,
}

/// The open capture stream.
pub struct Capture {
    socket: UnixStream,
    asks: Sender<Ask>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl Capture {
    /// Start listening on the codec's source, keeping `channel`.
    pub fn open(channel: usize) -> io::Result<Self> {
        let mut conn = Conn::open()?;
        let fragment = RATE / 50 * 4;
        let reply: protocol::CreateRecordStreamReply =
            conn.ask(&protocol::Command::CreateRecordStream(protocol::RecordStreamParams {
                sample_spec: protocol::SampleSpec {
                    format: protocol::SampleFormat::S16Le,
                    channels: 2,
                    sample_rate: RATE,
                },
                channel_map: protocol::ChannelMap::stereo(),
                source_name: Some(std::ffi::CString::new(pulse::SOURCE).unwrap()),
                buffer_attr: protocol::stream::BufferAttr {
                    fragment_size: fragment,
                    ..Default::default()
                },
                flags: protocol::stream::StreamFlags { adjust_latency: true, ..Default::default() },
                ..Default::default()
            }))?;
        let socket = conn.sock.get_ref().try_clone()?;
        let shared = Arc::new(Shared {
            peak: AtomicU32::new(0),
            channel: AtomicUsize::new(channel),
            paused: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        });
        let (asks, asked) = mpsc::channel();
        let reading = Arc::clone(&shared);
        let stream = reply.channel;
        let thread = std::thread::Builder::new()
            .name("capture".into())
            .spawn(move || run(conn, stream, &reading, &asked))?;
        Ok(Self { socket, asks, thread: Some(thread), shared })
    }

    /// The loudest sample since the last call, 0.0 to 1.0.
    pub fn level(&self) -> f32 {
        self.shared.peak.swap(0, Ordering::Relaxed) as f32 / 32767.0
    }

    pub fn set_channel(&self, channel: usize) {
        self.shared.channel.store(channel, Ordering::Relaxed);
    }

    fn ask(&self, ask: impl FnOnce(Sender<io::Result<()>>) -> Ask) -> io::Result<()> {
        let (tx, rx) = mpsc::channel();
        self.asks.send(ask(tx)).map_err(|_| io::Error::other("capture stopped"))?;
        rx.recv_timeout(Duration::from_secs(5)).map_err(|_| io::Error::other("capture stopped"))?
    }

    /// Start writing to `path`.
    pub fn start(&self, path: &Path, format: Format) -> io::Result<()> {
        self.shared.frames.store(0, Ordering::Relaxed);
        self.shared.paused.store(false, Ordering::Relaxed);
        self.shared.failed.store(false, Ordering::Relaxed);
        self.ask(|tx| Ask::Start(path.to_path_buf(), format, tx))
    }

    /// Stop writing and close the file.
    pub fn stop(&self) -> io::Result<()> {
        self.ask(Ask::Stop)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    /// How much of the take is on disk.
    pub fn elapsed(&self) -> Duration {
        let frames = self.shared.frames.load(Ordering::Relaxed);
        Duration::from_micros(frames * 1_000_000 / u64::from(RATE))
    }

    /// Whether the take ended on a failed write.
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop();
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The capture thread: messages off the socket until it closes, data chunks through
/// the chain, and the page's asks between them.
fn run(mut conn: Conn, stream: u32, shared: &Shared, asks: &Receiver<Ask>) {
    let mut chain = Chain::new();
    let mut sink: Option<Sink> = None;
    let mut buf = Vec::new();
    while let Ok(desc) = protocol::read_descriptor(&mut conn.sock) {
        buf.resize(desc.length as usize, 0);
        if conn.sock.read_exact(&mut buf).is_err() {
            break;
        }
        if desc.channel == stream {
            process(&buf, &mut chain, shared, &mut sink);
        }
        while let Ok(ask) = asks.try_recv() {
            match ask {
                Ask::Start(path, format, reply) => {
                    if let Some(old) = sink.take() {
                        let _ = old.finish();
                    }
                    match Sink::open(&path, format) {
                        Ok(s) => {
                            sink = Some(s);
                            let _ = reply.send(Ok(()));
                        }
                        Err(e) => drop(reply.send(Err(e))),
                    }
                }
                Ask::Stop(reply) => {
                    let _ = reply.send(sink.take().map_or(Ok(()), Sink::finish));
                }
            }
        }
    }
    for ask in asks.try_iter() {
        match ask {
            Ask::Start(_, _, reply) => drop(reply.send(Err(io::Error::other("capture stopped")))),
            Ask::Stop(reply) => drop(reply.send(sink.take().map_or(Ok(()), Sink::finish))),
        }
    }
    if let Some(sink) = sink {
        let _ = sink.finish();
    }
}

/// One chunk of stereo frames: the chosen channel through the chain, measured, and
/// written when a take is running and not held.
fn process(bytes: &[u8], chain: &mut Chain, shared: &Shared, sink: &mut Option<Sink>) {
    let channel = shared.channel.load(Ordering::Relaxed).min(1);
    if channel != chain.channel {
        *chain = Chain::new();
        chain.channel = channel;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    let mut peak = 0u32;
    for frame in bytes.as_chunks::<4>().0 {
        let at = channel * 2;
        let x = f32::from(i16::from_le_bytes([frame[at], frame[at + 1]])) / 32768.0;
        let y = (chain.step(x) * 32767.0) as i16;
        peak = peak.max(u32::from(y.unsigned_abs()));
        out.push(y);
    }
    shared.peak.fetch_max(peak.min(32767), Ordering::Relaxed);
    if shared.paused.load(Ordering::Relaxed) {
        return;
    }
    if let Some(s) = sink.as_mut() {
        if s.write(&out).is_ok() {
            shared.frames.fetch_add(out.len() as u64, Ordering::Relaxed);
        } else {
            shared.failed.store(true, Ordering::Relaxed);
            if let Some(s) = sink.take() {
                let _ = s.finish();
            }
        }
    }
}

/// What the gain aims a voice's envelope at, -12dBFS: speech peaks then sit well
/// above the meter's middle and clear of the limiter.
const AGC_TARGET: f32 = 0.25;
/// The most the gain will lift, +30dB, enough for a voice across a room.
const AGC_MAX: f32 = 31.6;
/// The least, -12dB, for shouting into the microphone.
const AGC_MIN: f32 = 0.25;
/// Below -55dBFS the input is the room, not a voice, and the gain holds where it is
/// rather than lifting the hiss up to speech level between words.
const AGC_GATE: f32 = 0.0018;
/// Where the limiter starts to bend, -1dBFS.
const LIMIT: f32 = 0.89;

/// The chain after the channel: high-pass, AGC, limiter.
struct Chain {
    channel: usize,
    highpass: Highpass,
    /// The input's envelope: quick to rise, slow to fall.
    env: f32,
    gain: f32,
    env_up: f32,
    env_down: f32,
    gain_down: f32,
    gain_up: f32,
}

impl Chain {
    fn new() -> Self {
        let per = |seconds: f32| 1.0 - (-1.0 / (seconds * RATE as f32)).exp();
        Self {
            channel: 0,
            highpass: Highpass::new(HIGHPASS_HZ, RATE as f32),
            env: 0.0,
            gain: 4.0,
            env_up: per(0.005),
            env_down: per(0.3),
            gain_down: per(0.02),
            gain_up: per(2.0),
        }
    }

    fn step(&mut self, x: f32) -> f32 {
        let x = self.highpass.step(x);
        let level = x.abs();
        let k = if level > self.env { self.env_up } else { self.env_down };
        self.env += (level - self.env) * k;
        if self.env > AGC_GATE {
            let want = (AGC_TARGET / self.env).clamp(AGC_MIN, AGC_MAX);
            let k = if want < self.gain { self.gain_down } else { self.gain_up };
            self.gain += (want - self.gain) * k;
        }
        limit(x * self.gain)
    }
}

/// Soft above `LIMIT`, never past full scale: a click the AGC has not caught up with
/// is rounded off rather than clipped.
fn limit(y: f32) -> f32 {
    let a = y.abs();
    if a <= LIMIT {
        return y;
    }
    let room = 1.0 - LIMIT;
    (LIMIT + room * ((a - LIMIT) / room).tanh()).copysign(y)
}

/// A two-pole high-pass, the RBJ cookbook's, at Q 0.707 as sox's `highpass` is.
struct Highpass {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl Highpass {
    fn new(corner: f32, rate: f32) -> Self {
        let w = std::f32::consts::TAU * corner / rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        Self {
            b: [(1.0 + cos) / 2.0 / a0, -(1.0 + cos) / a0, (1.0 + cos) / 2.0 / a0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn step(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// The path a take of `name` in `format` is written to.
pub fn path(dir: &Path, name: &str, format: Format) -> PathBuf {
    dir.join(format!("{name}.{}", format.ext()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stereo frames of a 440Hz tone at `left` on the left and `right` on the right.
    fn tone(left: f32, right: f32, n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| {
                let s = (std::f32::consts::TAU * 440.0 * i as f32 / RATE as f32).sin();
                [((left * s) as i16).to_le_bytes(), ((right * s) as i16).to_le_bytes()].concat()
            })
            .collect()
    }

    fn shared(channel: usize) -> Shared {
        Shared {
            peak: AtomicU32::new(0),
            channel: AtomicUsize::new(channel),
            paused: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        }
    }

    fn rms(samples: &[i16]) -> f32 {
        (samples.iter().map(|s| (f32::from(*s) / 32768.0).powi(2)).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn the_meter_reads_only_the_chosen_channel() {
        let input = tone(8000.0, 0.0, 4800);
        let (left, right) = (shared(0), shared(1));
        process(&input, &mut Chain::new(), &left, &mut None);
        assert!(left.peak.load(Ordering::Relaxed) > 8000, "lifted");
        process(&input, &mut Chain::new(), &right, &mut None);
        assert_eq!(right.peak.load(Ordering::Relaxed), 0);
    }

    /// The take that prompted it: a voice at about -38dBFS comes out near the target,
    /// and a loud one is brought down to it, after a few seconds of each.
    #[test]
    fn the_agc_brings_quiet_and_loud_speech_to_the_same_level() {
        for amplitude in [400.0, 26_000.0] {
            let mut chain = Chain::new();
            let input = tone(amplitude, 0.0, RATE as usize * 6);
            let out: Vec<i16> = input
                .as_chunks::<4>()
                .0
                .iter()
                .map(|f| (chain.step(f32::from(i16::from_le_bytes([f[0], f[1]])) / 32768.0) * 32767.0) as i16)
                .collect();
            let settled = rms(&out[out.len() - RATE as usize..]);
            let want = AGC_TARGET / std::f32::consts::SQRT_2;
            assert!((settled / want - 1.0).abs() < 0.25, "{amplitude}: rms {settled}, want about {want}");
        }
    }

    /// Silence is not lifted: the gate holds the gain where it started.
    #[test]
    fn hiss_is_not_lifted_to_speech_level() {
        let mut chain = Chain::new();
        for _ in 0..RATE * 3 {
            chain.step(0.0005);
        }
        assert!((chain.gain - 4.0).abs() < 0.01, "{}", chain.gain);
    }

    #[test]
    fn the_limiter_never_passes_full_scale() {
        assert_eq!(limit(0.5), 0.5);
        assert!(limit(5.0) <= 1.0 && limit(5.0) > LIMIT);
        assert_eq!(limit(-5.0), -limit(5.0));
    }

    fn take(format: Format) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("recorder-capture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = path(&dir, &format!("take-{}", format.ext()), format);
        let s = shared(0);
        let mut chain = Chain::new();
        let mut sink = Some(Sink::open(&file, format).unwrap());
        for _ in 0..30 {
            process(&tone(4000.0, 0.0, 4800), &mut chain, &s, &mut sink);
        }
        s.paused.store(true, Ordering::Relaxed);
        process(&tone(4000.0, 0.0, 4800), &mut chain, &s, &mut sink);
        assert_eq!(s.frames.load(Ordering::Relaxed), 30 * 4800, "nothing written while paused");
        sink.unwrap().finish().unwrap();
        file
    }

    /// Each format comes out three seconds long by its own header, which is also
    /// what proves LAME's tag and the last Vorbis page were written.
    #[test]
    fn every_format_writes_a_file_of_the_right_length() {
        for format in FORMATS {
            let file = take(format);
            let length = crate::length::of(&file).unwrap_or_default();
            assert!(length.abs_diff(Duration::from_secs(3)) < Duration::from_millis(80), "{format:?}: {length:?}");
        }
        let wav = take(Format::Wav);
        assert_eq!(std::fs::metadata(&wav).unwrap().len(), 44 + 30 * 4800 * 2);
    }

    /// What each encoder wrote plays back: the right rate, about the right number of
    /// frames, the tone's level, and a seek that lands one second in, which for the
    /// compressed two is a reopen and a decode forward.
    #[test]
    fn every_format_decodes_and_seeks() {
        for format in FORMATS {
            let file = take(format);
            let (rate, channels, all, after) = crate::player::decode_all(&file);
            assert_eq!((rate, channels), (RATE, 1), "{format:?}");
            let frames = all.len() as i64;
            assert!((frames - 3 * RATE as i64).abs() < 4000, "{format:?}: {frames} frames");
            let tail = &all[all.len() / 2..all.len() / 2 + 4800];
            assert!(rms(tail) > 0.1, "{format:?}: rms {}", rms(tail));
            let one = &all[RATE as usize..RATE as usize + after.len()];
            let close = after.iter().zip(one).all(|(a, b)| (i32::from(*a) - i32::from(*b)).abs() < 2000);
            assert!(close, "{format:?}: the seek landed somewhere else");
        }
    }

    #[test]
    fn the_highpass_removes_dc() {
        let mut f = Highpass::new(HIGHPASS_HZ, RATE as f32);
        let last = (0..48_000).map(|_| f.step(10_000.0)).last().unwrap();
        assert!(last.abs() < 1.0, "{last}");
    }
}
