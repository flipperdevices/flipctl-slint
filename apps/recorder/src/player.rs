//! Playing a take back: decoded here and played over the sound server's socket, so
//! there is no player to install or to start. A WAV is read directly, an MP3 by
//! minimp3 and an OGG by libvorbis, both compiled in.
//!
//! The prototype played with sox's `play` and paused and seeked by killing it and
//! starting another at the new offset. Here one thread owns the decoder and a
//! playback stream: pausing corks the stream, seeking moves the decoder and flushes
//! what the server still holds, and the position is the server's own read offset, so
//! the bar shows what has come out of the speaker rather than what was sent.
//!
//! At the end the stream is drained and kept, so Ok plays the take again.

use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pulseaudio::protocol::{self, Command};

use crate::pulse::{self, Conn};

/// What the page sees and asks for.
#[derive(Default)]
struct State {
    position: Duration,
    length: Option<Duration>,
    paused: bool,
    ended: bool,
    failed: bool,
    /// A seek the thread has not done yet.
    seek: Option<Duration>,
    /// A pause or resume the thread has not passed on yet.
    cork: Option<bool>,
}

pub struct Player {
    state: Arc<Mutex<State>>,
    socket: UnixStream,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    /// Open `file` and start playing it. The decoder is opened on the player thread,
    /// since libvorbis's cannot move between threads, and the answer comes back here.
    pub fn start(file: &Path, sink: &str) -> io::Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        let path = file.to_path_buf();
        let sink = sink.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("player".into()).spawn(move || {
            let opened = open(&path, &sink).and_then(|(conn, reply, decoder)| {
                let socket = conn.sock.get_ref().try_clone()?;
                Ok((conn, reply, decoder, socket))
            });
            let (conn, reply, decoder) = match opened {
                Ok((conn, reply, decoder, socket)) => {
                    shared.lock().unwrap().length = decoder.length;
                    let _ = tx.send(Ok(socket));
                    (conn, reply, decoder)
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            if play(conn, reply, decoder, &shared).is_err() {
                shared.lock().unwrap().failed = true;
            }
        })?;
        let socket = rx.recv().map_err(|_| io::Error::other("player stopped"))??;
        Ok(Self { state, socket, thread: Some(thread) })
    }

    pub fn alive(&self) -> bool {
        !self.state.lock().unwrap().failed
    }

    pub fn position(&self) -> Duration {
        self.state.lock().unwrap().position
    }

    pub fn length(&self) -> Option<Duration> {
        self.state.lock().unwrap().length
    }

    pub fn playing(&self) -> bool {
        let s = self.state.lock().unwrap();
        !s.paused && !s.ended
    }

    pub fn ended(&self) -> bool {
        self.state.lock().unwrap().ended
    }

    pub fn set_paused(&self, paused: bool) {
        let mut s = self.state.lock().unwrap();
        s.paused = paused;
        s.cork = Some(paused);
    }

    pub fn seek(&self, to: Duration) {
        let mut s = self.state.lock().unwrap();
        s.seek = Some(to);
        s.position = to;
        s.ended = false;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The file's decoder and a playback stream shaped to it.
fn open(path: &Path, sink: &str) -> io::Result<(Conn, protocol::CreatePlaybackStreamReply, Decoder)> {
    let decoder = Decoder::open(path)?;
    let mut conn = Conn::open()?;
    let channels = decoder.channels;
    let reply: protocol::CreatePlaybackStreamReply =
        conn.ask(&Command::CreatePlaybackStream(protocol::PlaybackStreamParams {
            sample_spec: protocol::SampleSpec {
                format: protocol::SampleFormat::S16Le,
                channels: channels as u8,
                sample_rate: decoder.rate,
            },
            channel_map: if channels == 1 { protocol::ChannelMap::mono() } else { protocol::ChannelMap::stereo() },
            sink_name: Some(std::ffi::CString::new(sink).unwrap_or_default()),
            buffer_attr: protocol::stream::BufferAttr {
                target_length: decoder.rate / 5 * channels as u32 * 2,
                ..Default::default()
            },
            flags: protocol::stream::StreamFlags { adjust_latency: true, ..Default::default() },
            ..Default::default()
        }))?;
    Ok((conn, reply, decoder))
}

/// What a file is read with.
enum Source {
    Wav { file: BufReader<File>, start: u64, end: u64 },
    Mp3(Box<minimp3::Decoder<BufReader<File>>>),
    Ogg(Box<vorbis_rs::VorbisDecoder<BufReader<File>>>),
}

/// A file being decoded to interleaved 16-bit frames.
struct Decoder {
    path: PathBuf,
    source: Source,
    rate: u32,
    /// 1 or 2: a file with more is played as its first two.
    channels: usize,
    from: usize,
    length: Option<Duration>,
    /// Decoded and not yet sent.
    pending: Vec<i16>,
    /// Frames still to drop after a seek, which decodes forward to the time asked for.
    skip: u64,
    /// Frames the source has produced since it was opened.
    produced: u64,
    done: bool,
}

impl Decoder {
    fn open(path: &Path) -> io::Result<Self> {
        let (source, rate, from, first) = Self::source(path)?;
        let mut dec = Self {
            path: path.to_path_buf(),
            source,
            rate,
            channels: from.min(2),
            from,
            length: crate::length::of(path),
            pending: Vec::new(),
            skip: 0,
            produced: 0,
            done: false,
        };
        dec.take(&first);
        Ok(dec)
    }

    /// The source, its rate and channels, and any samples read to learn them.
    fn source(path: &Path) -> io::Result<(Source, u32, usize, Vec<i16>)> {
        let bad = |what: &str| io::Error::other(what.to_string());
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let mut file = File::open(path)?;
        match ext.as_str() {
            "wav" => {
                let len = file.metadata()?.len();
                let w = crate::length::wav_layout(&mut file, len).ok_or_else(|| bad("not a WAV"))?;
                if w.format != 1 || w.bits != 16 {
                    return Err(bad("not 16-bit PCM"));
                }
                file.seek(SeekFrom::Start(w.data_start))?;
                let end = w.data_start + w.data_len;
                Ok((Source::Wav { file: BufReader::new(file), start: w.data_start, end }, w.rate, w.channels.into(), Vec::new()))
            }
            "mp3" => {
                let mut d = minimp3::Decoder::new(BufReader::new(file));
                let frame = d.next_frame().map_err(|e| bad(&e.to_string()))?;
                let rate = u32::try_from(frame.sample_rate).map_err(|_| bad("rate"))?;
                Ok((Source::Mp3(Box::new(d)), rate, frame.channels.max(1), frame.data))
            }
            "ogg" => {
                let d = vorbis_rs::VorbisDecoder::new(BufReader::new(file)).map_err(|e| bad(&e.to_string()))?;
                let (rate, ch) = (d.sampling_frequency().get(), usize::from(d.channels().get()));
                Ok((Source::Ogg(Box::new(d)), rate, ch, Vec::new()))
            }
            _ => Err(bad("unknown format")),
        }
    }

    /// Interleaved samples from the source, cut to the channels played and to what
    /// a seek still has to pass over.
    fn take(&mut self, all: &[i16]) {
        let frames = all.len() / self.from;
        self.produced += frames as u64;
        let drop = (self.skip.min(frames as u64)) as usize;
        self.skip -= drop as u64;
        for frame in all.chunks_exact(self.from).skip(drop) {
            self.pending.extend_from_slice(&frame[..self.channels]);
        }
    }

    /// Up to `bytes` of frames, fewer only at the end.
    fn fill(&mut self, bytes: usize, out: &mut Vec<u8>) {
        let want = bytes / 2 / self.channels * self.channels;
        while self.pending.len() < want && !self.done {
            self.decode();
        }
        let n = want.min(self.pending.len());
        out.clear();
        out.extend(self.pending.drain(..n).flat_map(i16::to_le_bytes));
    }

    fn decode(&mut self) {
        let samples: Vec<i16> = match &mut self.source {
            Source::Wav { file, end, .. } => {
                let at = file.stream_position().unwrap_or(*end);
                let n = (*end - at.min(*end)).min(8192) as usize / (2 * self.from) * 2 * self.from;
                let mut bytes = vec![0u8; n];
                if n == 0 || file.read_exact(&mut bytes).is_err() {
                    Vec::new()
                } else {
                    bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect()
                }
            }
            Source::Mp3(d) => d.next_frame().map(|f| f.data).unwrap_or_default(),
            Source::Ogg(d) => match d.decode_audio_block() {
                Ok(Some(block)) => {
                    let chans = block.samples();
                    let n = chans.first().map_or(0, |c| c.len());
                    (0..n)
                        .flat_map(|i| chans.iter().map(move |c| (c[i] * 32767.0).clamp(-32768.0, 32767.0) as i16))
                        .collect()
                }
                _ => Vec::new(),
            },
        };
        if samples.is_empty() {
            self.done = true;
        } else {
            self.take(&samples);
        }
    }

    /// Move to `to`: directly in a WAV, by decoding forward in the others, from the
    /// start again when the time is behind what has been decoded.
    fn seek(&mut self, to: Duration) -> Duration {
        let target = (to.as_secs_f64() * f64::from(self.rate)) as u64;
        self.pending.clear();
        self.done = false;
        self.skip = 0;
        if let Source::Wav { file, start, end } = &mut self.source {
            let at = (*start + target * 2 * self.from as u64).min(*end);
            if file.seek(SeekFrom::Start(at)).is_ok() {
                self.produced = target;
                return to;
            }
        }
        if target < self.produced {
            match Self::source(&self.path) {
                Ok((source, _, _, first)) => {
                    self.source = source;
                    self.produced = 0;
                    self.skip = target;
                    self.take(&first);
                }
                Err(_) => self.done = true,
            }
        } else {
            self.skip = target - self.produced;
        }
        to
    }
}

const LATENCY: u32 = u32::MAX - 1;
const DRAIN: u32 = u32::MAX - 2;

/// The player thread: the server's requests answered from the decoder, and the
/// page's pauses and seeks between them.
fn play(
    mut conn: Conn,
    stream: protocol::CreatePlaybackStreamReply,
    mut dec: Decoder,
    state: &Mutex<State>,
) -> io::Result<()> {
    let channel = stream.channel;
    let frame_bytes = dec.channels as i64 * 2;
    let mut out = Vec::new();
    // Bytes sent in all, and how many had been sent when playing last restarted at
    // `base`: the server's read offset past that point is what has been heard since.
    let (mut written, mut since, mut base) = (0i64, 0i64, Duration::ZERO);
    let mut draining = false;
    let mut asked = Instant::now();

    let mut send = |conn: &mut Conn, dec: &mut Decoder, bytes: usize, written: &mut i64| -> io::Result<bool> {
        dec.fill(bytes, &mut out);
        if out.is_empty() {
            return Ok(false);
        }
        protocol::write_memblock(conn.sock.get_mut(), channel, &out, 0).map_err(pulse::err)?;
        *written += out.len() as i64;
        Ok(true)
    };
    send(&mut conn, &mut dec, stream.requested_bytes as usize, &mut written)?;
    let prime = (stream.buffer_attr.target_length as usize).min(dec.rate as usize * frame_bytes as usize);

    loop {
        let (seek, cork) = {
            let mut s = state.lock().unwrap();
            (s.seek.take(), s.cork.take())
        };
        if let Some(to) = seek {
            base = dec.seek(to);
            conn.send(&Command::FlushPlaybackStream(channel))?;
            since = written;
            draining = false;
            // Primed rather than left to the server's next request: after a drain has
            // finished there is no next request, and a replay from the end would sit
            // silent with the stream uncorked and empty.
            send(&mut conn, &mut dec, prime, &mut written)?;
            let paused = {
                let mut s = state.lock().unwrap();
                s.position = base;
                s.paused
            };
            if !paused {
                conn.send(&Command::CorkPlaybackStream(protocol::stream::CorkStreamParams { channel, cork: false }))?;
            }
        }
        if let Some(cork) = cork {
            conn.send(&Command::CorkPlaybackStream(protocol::stream::CorkStreamParams { channel, cork }))?;
        }
        if asked.elapsed() >= Duration::from_millis(100) {
            conn.send_as(LATENCY, &Command::GetPlaybackLatency(protocol::LatencyParams {
                channel,
                now: std::time::SystemTime::now(),
            }))?;
            asked = Instant::now();
        }

        if conn.sock.buffer().is_empty() && !readable(conn.sock.get_ref(), 50)? {
            continue;
        }
        let desc = protocol::read_descriptor(&mut conn.sock).map_err(pulse::err)?;
        let mut body = vec![0u8; desc.length as usize];
        conn.sock.read_exact(&mut body)?;
        if desc.channel != u32::MAX {
            continue;
        }
        let mut cursor = Cursor::new(&body[..]);
        let Ok((seq, msg)) = Command::read_tag_prefixed(&mut cursor, conn.version) else { continue };
        match msg {
            Command::Request(r) if r.channel == channel => {
                if !send(&mut conn, &mut dec, r.length as usize, &mut written)? && dec.done && !draining {
                    conn.send_as(DRAIN, &Command::DrainPlaybackStream(channel))?;
                    draining = true;
                }
            }
            Command::Reply if seq == LATENCY => {
                let mut ts = protocol::TagStructReader::new(&mut cursor, conn.version);
                if let Ok(t) = ts.read::<protocol::PlaybackLatency>() {
                    let heard = (t.read_offset - since).max(0) / frame_bytes;
                    let mut s = state.lock().unwrap();
                    // Everything sent has been played: the end, whether or not the
                    // drain's own reply ever comes.
                    if draining && s.seek.is_none() && t.read_offset >= written {
                        s.ended = true;
                    }
                    if s.seek.is_none() && !s.ended {
                        s.position = base + Duration::from_secs_f64(heard as f64 / f64::from(dec.rate));
                        if let Some(l) = s.length {
                            s.position = s.position.min(l);
                        }
                    }
                }
            }
            Command::Reply if seq == DRAIN && draining => {
                let mut s = state.lock().unwrap();
                if s.seek.is_none() {
                    s.ended = true;
                    if let Some(l) = s.length {
                        s.position = l;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Whether the socket has something to read within `ms`.
fn readable(sock: &UnixStream, ms: i32) -> io::Result<bool> {
    let mut fd = libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    match unsafe { libc::poll(&mut fd, 1, ms) } {
        n if n < 0 => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) }
        }
        0 => Ok(false),
        _ if fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 && fd.revents & libc::POLLIN == 0 => {
            Err(io::Error::other("closed"))
        }
        _ => Ok(true),
    }
}

/// Every frame of a file, and the frame a seek to one second lands on, for the
/// encoder tests in capture.rs to read their own output back with.
#[cfg(test)]
pub fn decode_all(path: &Path) -> (u32, usize, Vec<i16>, Vec<i16>) {
    let mut dec = Decoder::open(path).unwrap();
    let mut all = Vec::new();
    while !dec.done {
        dec.decode();
        all.append(&mut dec.pending);
    }
    dec.seek(Duration::from_secs(1));
    let mut out = Vec::new();
    dec.fill(200, &mut out);
    let after = out.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
    (dec.rate, dec.channels, all, after)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A take as the app writes it decodes to the samples that went in, and a seek
    /// lands where it was asked to.
    #[test]
    fn a_wav_decodes_and_seeks() {
        let dir = std::env::temp_dir().join(format!("recorder-player-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ramp.wav");
        let n = 48_000 * 2;
        let mut bytes = crate::length::wav_header(n * 2, 48_000, 1).to_vec();
        bytes.extend((0..n).flat_map(|i| ((i % 30_000) as i16).to_le_bytes()));
        std::fs::write(&file, bytes).unwrap();

        let mut dec = Decoder::open(&file).unwrap();
        assert_eq!((dec.rate, dec.channels), (48_000, 1));
        assert_eq!(dec.length, Some(Duration::from_secs(2)));
        let mut out = Vec::new();
        dec.fill(8, &mut out);
        assert_eq!(out, [0, 0, 1, 0, 2, 0, 3, 0]);
        let at = dec.seek(Duration::from_secs(1));
        assert_eq!(at, Duration::from_secs(1));
        dec.fill(2, &mut out);
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), (48_000 % 30_000) as i16);
    }
}
