//! Locate's beep: a short tone that repeats faster and climbs as the signal gets
//! stronger, like a metal detector, and stops when the target is not heard.
//!
//! Played over the PulseAudio socket PipeWire serves, which flipctl links into the
//! app's runtime directory because the manifest says `audio`, on the sink flipctl
//! pinned the app to (the panel's speaker). A thread of its own writes the samples
//! as the server asks for them; the page only sets the level. Nothing about the
//! system's sound is changed: no volume, no default output.

use std::ffi::CString;
use std::io::{self, BufReader, Cursor, Read};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use pulseaudio::protocol::{self, Command};

const RATE: u32 = 48_000;
/// One beep, and its fade in and out, so it clicks neither way.
const TONE_MS: u32 = 40;
const FADE_MS: u32 = 4;
/// The gap after a beep, from the weakest signal heard to the strongest.
const SLOWEST_MS: f32 = 900.0;
const FASTEST_MS: f32 = 40.0;
/// The tone's pitch over the same span.
const LOW_HZ: f32 = 900.0;
const HIGH_HZ: f32 = 1800.0;
/// The server holds this much ahead, so a new level is heard within it.
const AHEAD_MS: u32 = 60;

/// A level of 0 to 1000, or `SILENT`.
const SILENT: u32 = u32::MAX;

struct Shared {
    level: AtomicU32,
    stop: AtomicBool,
}

/// The beeper. Dropping it stops the sound.
pub struct Beep {
    shared: Arc<Shared>,
    sock: Option<UnixStream>,
}

impl Beep {
    pub fn start() -> Self {
        let shared =
            Arc::new(Shared { level: AtomicU32::new(SILENT), stop: AtomicBool::new(false) });
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = shared.clone();
        std::thread::spawn(move || {
            if let Err(e) = play(&worker, &tx) {
                eprintln!("wifi-scanner: beep: {e}");
            }
        });
        Self { shared, sock: rx.recv_timeout(std::time::Duration::from_secs(1)).ok() }
    }

    /// How strong the signal is, 0 to 1, or `None` for silence.
    pub fn set(&self, level: Option<f32>) {
        let v = level.map_or(SILENT, |l| (l.clamp(0.0, 1.0) * 1000.0) as u32);
        self.shared.level.store(v, Ordering::Relaxed);
    }
}

impl Drop for Beep {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Wake the thread out of its read
        if let Some(s) = &self.sock {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn err(e: protocol::ProtocolError) -> io::Error {
    match e {
        protocol::ProtocolError::Io(e) => e,
        e => io::Error::other(e.to_string()),
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

/// The tone generator: where it is in the current beep and gap.
#[derive(Default)]
struct Tone {
    /// Samples into the current cycle, and the cycle's tone and gap lengths.
    at: u32,
    gap: u32,
    hz: f32,
    phase: f32,
}

impl Tone {
    /// `n` samples at `level` (0 to 1000, or silent), as signed 16-bit little endian.
    fn fill(&mut self, level: u32, n: usize, out: &mut Vec<u8>) {
        let tone = RATE * TONE_MS / 1000;
        let fade = RATE * FADE_MS / 1000;
        let gap = |level: u32| {
            let l = level as f32 / 1000.0;
            (RATE as f32 * (SLOWEST_MS + (FASTEST_MS - SLOWEST_MS) * l) / 1000.0) as u32
        };
        for _ in 0..n {
            // A new level cuts the gap short at once, so a stronger signal is heard
            // without waiting out a slow beep's silence
            if self.at >= tone {
                if level == SILENT {
                    self.at = 0;
                } else {
                    self.gap = gap(level);
                    if self.at >= tone + self.gap {
                        self.at = 0;
                    }
                }
            }
            // The pitch changes only between beeps, so a change never cuts one short
            if self.at == 0 {
                if level == SILENT {
                    out.extend_from_slice(&0i16.to_le_bytes());
                    continue;
                }
                self.gap = gap(level);
                self.hz = LOW_HZ + (HIGH_HZ - LOW_HZ) * level as f32 / 1000.0;
            }
            let sample = if self.at < tone {
                let edge = self.at.min(tone - 1 - self.at).min(fade) as f32 / fade as f32;
                self.phase = (self.phase + self.hz / RATE as f32).fract();
                (self.phase * std::f32::consts::TAU).sin() * edge * 0.5
            } else {
                0.0
            };
            out.extend_from_slice(&((sample * f32::from(i16::MAX)) as i16).to_le_bytes());
            self.at += 1;
            if self.at >= tone + self.gap {
                self.at = 0;
            }
        }
    }
}

fn play(shared: &Shared, sock_out: &std::sync::mpsc::Sender<UnixStream>) -> io::Result<()> {
    let path =
        pulseaudio::socket_path_from_env().ok_or_else(|| io::Error::other("no sound server"))?;
    let raw = UnixStream::connect(path)?;
    let _ = sock_out.send(raw.try_clone()?);
    let mut sock = BufReader::new(raw);
    let mut seq = 0;
    let mut ask = |sock: &mut BufReader<UnixStream>, c: &Command, version: u16| -> io::Result<()> {
        seq += 1;
        protocol::write_command_message(sock.get_mut(), seq, c, version).map_err(err)
    };
    let cookie =
        pulseaudio::cookie_path_from_env().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
    ask(
        &mut sock,
        &Command::Auth(protocol::AuthParams {
            version: protocol::MAX_VERSION,
            supports_shm: false,
            supports_memfd: false,
            cookie,
        }),
        protocol::MAX_VERSION,
    )?;
    let (_, auth) =
        protocol::read_reply_message::<protocol::AuthReply>(&mut sock, protocol::MAX_VERSION)
            .map_err(err)?;
    let version = protocol::MAX_VERSION.min(auth.version);
    let mut props = protocol::Props::new();
    props.set(protocol::Prop::ApplicationName, cstr("Wi-Fi Scanner"));
    ask(&mut sock, &Command::SetClientName(props), version)?;
    protocol::read_reply_message::<protocol::SetClientNameReply>(&mut sock, version)
        .map_err(err)?;

    let sink = std::env::var("PULSE_SINK").ok();
    let ahead = RATE * 2 * AHEAD_MS / 1000;
    ask(
        &mut sock,
        &Command::CreatePlaybackStream(protocol::PlaybackStreamParams {
            sample_spec: protocol::SampleSpec {
                format: protocol::SampleFormat::S16Le,
                channels: 1,
                sample_rate: RATE,
            },
            channel_map: protocol::ChannelMap::mono(),
            sink_name: sink.as_deref().map(cstr),
            buffer_attr: protocol::stream::BufferAttr {
                target_length: ahead,
                minimum_request_length: ahead / 4,
                ..Default::default()
            },
            ..Default::default()
        }),
        version,
    )?;
    let (_, reply) =
        protocol::read_reply_message::<protocol::CreatePlaybackStreamReply>(&mut sock, version)
            .map_err(err)?;
    let channel = reply.channel;

    let mut tone = Tone::default();
    let mut buf = Vec::new();
    let mut send =
        |sock: &mut BufReader<UnixStream>, bytes: usize, tone: &mut Tone| -> io::Result<()> {
            buf.clear();
            tone.fill(shared.level.load(Ordering::Relaxed), bytes / 2, &mut buf);
            protocol::write_memblock(sock.get_mut(), channel, &buf, 0).map_err(err)
        };
    send(&mut sock, reply.requested_bytes as usize, &mut tone)?;
    while !shared.stop.load(Ordering::Relaxed) {
        let desc = protocol::read_descriptor(&mut sock).map_err(err)?;
        let mut body = vec![0u8; desc.length as usize];
        sock.read_exact(&mut body)?;
        if desc.channel != u32::MAX {
            continue;
        }
        let mut cursor = Cursor::new(&body[..]);
        let Ok((_, msg)) = Command::read_tag_prefixed(&mut cursor, version) else { continue };
        match msg {
            Command::Request(r) if r.channel == channel => {
                send(&mut sock, r.length as usize, &mut tone)?
            }
            Command::PlaybackStreamKilled(c) if c == channel => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beeps(level: u32, ms: u32) -> usize {
        let mut t = Tone::default();
        let mut out = Vec::new();
        t.fill(level, (RATE * ms / 1000) as usize, &mut out);
        let loud: Vec<bool> =
            out.chunks(2).map(|s| i16::from_le_bytes([s[0], s[1]]).abs() > 1000).collect();
        // Count the starts of loud runs, allowing the zero crossings inside a tone
        let mut n = 0;
        let mut quiet = RATE as usize;
        for l in loud {
            if l {
                if quiet > 200 {
                    n += 1;
                }
                quiet = 0;
            } else {
                quiet += 1;
            }
        }
        n
    }

    #[test]
    fn stronger_beeps_faster_and_silent_is_silent() {
        let weak = beeps(0, 4000);
        let strong = beeps(1000, 4000);
        assert!((4..=5).contains(&weak), "{weak}");
        assert!(strong >= 40, "{strong}");
        assert_eq!(beeps(SILENT, 2000), 0);
    }

    #[test]
    fn a_stronger_signal_is_heard_without_waiting_out_the_gap() {
        let mut t = Tone::default();
        let mut out = Vec::new();
        // One weak beep, then 100 ms into its 900 ms gap the signal jumps
        t.fill(0, (RATE * (TONE_MS + 100) / 1000) as usize, &mut out);
        out.clear();
        t.fill(1000, (RATE / 10) as usize, &mut out);
        let loud = out.chunks(2).any(|s| i16::from_le_bytes([s[0], s[1]]).abs() > 1000);
        assert!(loud);
    }
}
