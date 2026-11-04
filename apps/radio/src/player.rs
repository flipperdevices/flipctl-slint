//! One playing stream, with nothing to install or to start: fetched, decoded and
//! played here.
//!
//! Two threads. One reads the station and decodes it, with the titles taken out
//! on the way, and hands frames over a short queue; the other owns a playback
//! stream on the sound server and answers its requests for audio from that queue.
//! The network sets the pace, which is the station's own: a stall leaves the server
//! short and it plays silence until the frames come again.
//!
//! Stopping shuts both sockets, which ends whatever read either thread is waiting
//! in, and the threads are left to end by themselves rather than joined: a server
//! that is slow to answer a connection would otherwise hold the page.

use std::io::{self, Cursor, Read};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use pulseaudio::protocol::{self, Command};

use crate::http;
use crate::icy::Icy;
use crate::pulse::{self, Conn};

/// Decoded frames waiting for the server: an MP3 frame is 26ms, so about a second.
const QUEUE: usize = 40;

#[derive(Default)]
struct State {
    /// Sound has gone to the server.
    started: bool,
    /// The station or the server went away.
    failed: bool,
    /// The server's index for the stream, which is what moving it takes.
    index: Option<u32>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    title: Arc<Mutex<String>>,
    stopped: AtomicBool,
    tcp: Mutex<Option<TcpStream>>,
    pulse: Mutex<Option<UnixStream>>,
}

impl Shared {
    fn fail(&self) {
        if !self.stopped.load(Ordering::SeqCst) {
            self.state.lock().unwrap().failed = true;
        }
    }
}

pub struct Player {
    shared: Arc<Shared>,
}

impl Player {
    /// Start playing `url` on the sink called `sink`.
    pub fn start(url: &str, sink: &str) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::sync_channel(QUEUE);

        let (net, url) = (Arc::clone(&shared), url.to_string());
        std::thread::Builder::new().name("stream".into()).spawn(move || {
            if fetch(&url, &net, &tx).is_err() {
                net.fail();
            }
        })?;
        let (out, sink) = (Arc::clone(&shared), sink.to_string());
        std::thread::Builder::new().name("playback".into()).spawn(move || {
            if play(&sink, &out, &rx).is_err() {
                out.fail();
            }
        })?;
        Ok(Self { shared })
    }

    /// Whether the stream is still up. A dead URL, a stream that is not MP3, a
    /// server that stops sending and a name that does not resolve all end here.
    pub fn alive(&self) -> bool {
        !self.shared.state.lock().unwrap().failed
    }

    /// Whether sound has started: false while the station is still being reached.
    pub fn started(&self) -> bool {
        self.shared.state.lock().unwrap().started
    }

    /// What the station says it is playing, or nothing when it does not say.
    pub fn title(&self) -> String {
        self.shared.title.lock().unwrap().clone()
    }

    /// The server's index for the stream, once it has one.
    pub fn index(&self) -> Option<u32> {
        self.shared.state.lock().unwrap().index
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        if let Some(tcp) = self.shared.tcp.lock().unwrap().as_ref() {
            let _ = tcp.shutdown(Shutdown::Both);
        }
        if let Some(sock) = self.shared.pulse.lock().unwrap().as_ref() {
            let _ = sock.shutdown(Shutdown::Both);
        }
    }
}

/// The station, decoded into frames for the playback thread.
fn fetch(url: &str, shared: &Shared, tx: &SyncSender<minimp3::Frame>) -> io::Result<()> {
    let response = http::get(url, &|tcp| {
        if let Ok(clone) = tcp.try_clone() {
            *shared.tcp.lock().unwrap() = Some(clone);
        }
        // Stopped while connecting: the socket stored a moment too late to be shut.
        if shared.stopped.load(Ordering::SeqCst) {
            let _ = tcp.shutdown(Shutdown::Both);
        }
    })?;
    let icy = Icy::new(response.body, response.metaint, Arc::clone(&shared.title));
    let mut decoder = minimp3::Decoder::new(icy);
    loop {
        match decoder.next_frame() {
            Ok(frame) => {
                if tx.send(frame).is_err() {
                    return Ok(());
                }
            }
            // Junk before a frame, which a stream joined part way through has.
            Err(minimp3::Error::SkippedData) => {}
            Err(minimp3::Error::Io(e)) => return Err(e),
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
    }
}

/// Frames as interleaved bytes for a stream of `channels`: a mono frame is doubled
/// onto a stereo stream, and anything wider is cut to the first channels.
fn samples(frame: &minimp3::Frame, channels: usize, out: &mut Vec<u8>) {
    let from = frame.channels.max(1);
    for chunk in frame.data.chunks_exact(from) {
        for c in 0..channels {
            out.extend_from_slice(&chunk[c.min(from - 1)].to_le_bytes());
        }
    }
}

/// The playback stream, fed from the queue. The first frame says what shape it is.
fn play(sink: &str, shared: &Shared, rx: &Receiver<minimp3::Frame>) -> io::Result<()> {
    let first = rx.recv().map_err(|_| io::Error::other("no audio"))?;
    let rate = u32::try_from(first.sample_rate).map_err(|_| io::Error::other("rate"))?;
    let channels = first.channels.clamp(1, 2);

    let mut conn = Conn::open()?;
    *shared.pulse.lock().unwrap() = Some(conn.sock.get_ref().try_clone()?);
    if shared.stopped.load(Ordering::SeqCst) {
        return Ok(());
    }
    let reply: protocol::CreatePlaybackStreamReply =
        conn.ask(&Command::CreatePlaybackStream(protocol::PlaybackStreamParams {
            sample_spec: protocol::SampleSpec {
                format: protocol::SampleFormat::S16Le,
                channels: channels as u8,
                sample_rate: rate,
            },
            channel_map: if channels == 1 { protocol::ChannelMap::mono() } else { protocol::ChannelMap::stereo() },
            sink_name: Some(pulse::cstr(sink)),
            // A second of audio held by the server, which is what rides out a
            // network that hesitates.
            buffer_attr: protocol::stream::BufferAttr {
                target_length: rate * channels as u32 * 2,
                ..Default::default()
            },
            flags: protocol::stream::StreamFlags { adjust_latency: true, ..Default::default() },
            ..Default::default()
        }))?;
    shared.state.lock().unwrap().index = Some(reply.stream_index);
    let channel = reply.channel;

    let mut pending = Vec::new();
    samples(&first, channels, &mut pending);
    let send = |conn: &mut Conn, bytes: usize, pending: &mut Vec<u8>| -> io::Result<()> {
        let bytes = bytes / (2 * channels) * (2 * channels);
        while pending.len() < bytes {
            let frame = rx.recv().map_err(|_| io::Error::other("stream ended"))?;
            samples(&frame, channels, pending);
        }
        protocol::write_memblock(conn.sock.get_mut(), channel, &pending[..bytes], 0).map_err(pulse::err)?;
        pending.drain(..bytes);
        shared.state.lock().unwrap().started = true;
        Ok(())
    };
    send(&mut conn, reply.requested_bytes as usize, &mut pending)?;

    loop {
        let desc = protocol::read_descriptor(&mut conn.sock).map_err(pulse::err)?;
        let mut body = vec![0u8; desc.length as usize];
        conn.sock.read_exact(&mut body)?;
        if desc.channel != u32::MAX {
            continue;
        }
        let mut cursor = Cursor::new(&body[..]);
        let Ok((_, msg)) = Command::read_tag_prefixed(&mut cursor, conn.version) else { continue };
        match msg {
            Command::Request(r) if r.channel == channel => send(&mut conn, r.length as usize, &mut pending)?,
            // The sink it was on has gone: the server says so rather than moving it.
            Command::PlaybackStreamKilled(c) if c == channel => return Err(io::Error::other("killed")),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(channels: usize, data: Vec<i16>) -> minimp3::Frame {
        minimp3::Frame { data, sample_rate: 44100, channels, layer: 3, bitrate: 128 }
    }

    #[test]
    fn mono_is_doubled_and_stereo_left_alone() {
        let mut out = Vec::new();
        samples(&frame(1, vec![1, 2]), 2, &mut out);
        assert_eq!(out, [1, 0, 1, 0, 2, 0, 2, 0]);
        out.clear();
        samples(&frame(2, vec![1, -1]), 2, &mut out);
        assert_eq!(out, [1, 0, 0xff, 0xff]);
    }

    /// A stopped player is stopped for good: nothing it learns afterwards, such as
    /// its station going away, is a failure to report.
    #[test]
    fn a_stop_is_not_a_failure() {
        let shared = Shared::default();
        shared.stopped.store(true, Ordering::SeqCst);
        shared.fail();
        assert!(!shared.state.lock().unwrap().failed);
    }
}

/// Against the real stations, so `#[ignore]`: it needs the network and some
/// seconds. Run it with
///
///     cargo test --release -- --ignored --nocapture
///
/// Every station is fetched and decoded for a while, which covers the HTTP, the
/// TLS, the titles and the decoder; the sound server is not needed for any of it.
#[cfg(test)]
mod live {
    use super::*;

    #[test]
    #[ignore = "needs the network and about half a minute"]
    fn every_station_answers_with_mp3() {
        let mut failed = Vec::new();
        for (station, url) in crate::URLS {
            let shared = Shared::default();
            let (tx, rx) = mpsc::sync_channel(QUEUE);
            let fetched = std::thread::scope(|s| {
                let net = s.spawn(|| fetch(url, &shared, &tx));
                // A couple of seconds of audio, then stop it the way Drop does.
                let frames: Vec<_> = rx.iter().take(80).collect();
                shared.stopped.store(true, Ordering::SeqCst);
                if let Some(tcp) = shared.tcp.lock().unwrap().as_ref() {
                    let _ = tcp.shutdown(Shutdown::Both);
                }
                drop(rx);
                let _ = net.join();
                frames
            });
            let title = shared.title.lock().unwrap().clone();
            match fetched.first() {
                Some(f) => println!(
                    "{station}: {} Hz x{}, {} frames, title {:?}",
                    f.sample_rate,
                    f.channels,
                    fetched.len(),
                    flipctl_app::font::ascii(&title)
                ),
                None => failed.push(*station),
            }
        }
        assert!(failed.is_empty(), "no audio from {failed:?}");
    }
}
