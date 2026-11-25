//! The sound track's samples: what the speaker plays, the built-in microphone, or
//! both.
//!
//! One record stream on the sound server, read on a thread of its own, each chunk
//! stamped with the `CLOCK_MONOTONIC` time it arrived: the same clock flipctl stamps
//! frames with, so the two line up without either side knowing about the other.
//!
//! The speaker's monitor is stereo and taken as it is. The microphone is the codec's
//! left channel, through Voice Recorder's chain (a high-pass, an AGC and a limiter),
//! since a voice comes in far too quiet to hear without it. With both, each is a
//! stream of its own and the encoding thread mixes them (session.rs).

use std::io::{self, Read};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use pulseaudio::protocol;

use crate::mp3::RATE;
use crate::pulse::{self, Conn};
use crate::session::Event;

/// Which sound goes with the picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    Off,
    System,
    Mic,
    SystemMic,
}

/// In the order the Sound row offers them.
pub const SOUNDS: [Sound; 4] = [Sound::Off, Sound::System, Sound::Mic, Sound::SystemMic];

impl Sound {
    pub fn label(self) -> &'static str {
        match self {
            Sound::Off => "Off",
            Sound::System => "System",
            Sound::Mic => "Mic",
            Sound::SystemMic => "System + Mic",
        }
    }

    /// The streams it takes.
    pub fn sources(self) -> &'static [Source] {
        match self {
            Sound::Off => &[],
            Sound::System => &[Source::Speaker],
            Sound::Mic => &[Source::Mic],
            Sound::SystemMic => &[Source::Speaker, Source::Mic],
        }
    }

    /// Channels in the track, or none at all.
    pub fn channels(self) -> Option<u16> {
        match self {
            Sound::Off => None,
            Sound::System | Sound::SystemMic => Some(2),
            Sound::Mic => Some(1),
        }
    }
}

/// One stream from the sound server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Stereo, as the speaker plays it.
    Speaker,
    /// Mono, through the chain.
    Mic,
}

/// The open record stream. Dropping it closes the stream.
pub struct Capture {
    socket: UnixStream,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Start capturing `source`, sending each chunk to `events`.
    pub fn open(source: Source, events: Sender<Event>) -> io::Result<Self> {
        let mut conn = Conn::open()?;
        let name = match source {
            Source::Mic => pulse::MIC.to_string(),
            Source::Speaker => pulse::speaker_monitor(),
        };
        let reply: protocol::CreateRecordStreamReply =
            conn.ask(&protocol::Command::CreateRecordStream(protocol::RecordStreamParams {
                sample_spec: protocol::SampleSpec {
                    format: protocol::SampleFormat::S16Le,
                    channels: 2,
                    sample_rate: RATE,
                },
                channel_map: protocol::ChannelMap::stereo(),
                source_name: Some(pulse::cstr(&name)),
                buffer_attr: protocol::stream::BufferAttr {
                    fragment_size: RATE / 50 * 4,
                    ..Default::default()
                },
                flags: protocol::stream::StreamFlags { adjust_latency: true, ..Default::default() },
                ..Default::default()
            }))?;
        let socket = conn.sock.get_ref().try_clone()?;
        let stream = reply.channel;
        let thread = std::thread::Builder::new()
            .name("sound".into())
            .spawn(move || run(conn, stream, source, &events))?;
        Ok(Self { socket, thread: Some(thread) })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(mut conn: Conn, stream: u32, source: Source, events: &Sender<Event>) {
    let mut chain = Chain::new();
    let mut buf = Vec::new();
    while let Ok(desc) = protocol::read_descriptor(&mut conn.sock) {
        buf.resize(desc.length as usize, 0);
        if conn.sock.read_exact(&mut buf).is_err() {
            break;
        }
        if desc.channel != stream {
            continue;
        }
        let at_us = flipper_ui::screen_rec::now_us();
        let pcm = match source {
            Source::Mic => mic(&buf, &mut chain),
            Source::Speaker => {
                buf.as_chunks::<2>().0.iter().map(|s| i16::from_le_bytes(*s)).collect()
            }
        };
        if events.send(Event::Sound { source, pcm, at_us }).is_err() {
            break;
        }
    }
}

/// The left channel of a stereo chunk, through the chain.
fn mic(bytes: &[u8], chain: &mut Chain) -> Vec<i16> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|frame| {
            let x = f32::from(i16::from_le_bytes([frame[0], frame[1]])) / 32768.0;
            (chain.step(x) * 32767.0) as i16
        })
        .collect()
}

// The chain below is Voice Recorder's (apps/recorder/src/capture.rs), unchanged.

/// The high-pass corner, the prototype's `highpass 80`.
const HIGHPASS_HZ: f32 = 80.0;
/// What the gain aims a voice's envelope at, -12dBFS.
const AGC_TARGET: f32 = 0.25;
/// The most the gain will lift, +30dB.
const AGC_MAX: f32 = 31.6;
/// The least, -12dB.
const AGC_MIN: f32 = 0.25;
/// Below -55dBFS the input is the room, and the gain holds where it is.
const AGC_GATE: f32 = 0.0018;
/// Where the limiter starts to bend, -1dBFS.
const LIMIT: f32 = 0.89;

/// High-pass, AGC, limiter.
struct Chain {
    highpass: Highpass,
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

/// Soft above `LIMIT`, never past full scale.
fn limit(y: f32) -> f32 {
    let a = y.abs();
    if a <= LIMIT {
        return y;
    }
    let room = 1.0 - LIMIT;
    (LIMIT + room * ((a - LIMIT) / room).tanh()).copysign(y)
}

/// A two-pole high-pass, the RBJ cookbook's, at Q 0.707.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mic_is_the_left_channel_lifted() {
        let mut chain = Chain::new();
        let mut bytes = Vec::new();
        for i in 0..RATE as usize {
            let left = ((i as f32 / 10.0).sin() * 600.0) as i16;
            bytes.extend_from_slice(&left.to_le_bytes());
            bytes.extend_from_slice(&20_000i16.to_le_bytes());
        }
        let out = mic(&bytes, &mut chain);
        assert_eq!(out.len(), RATE as usize);
        let peak = out[out.len() / 2..].iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 3000, "quiet speech is brought up, peak {peak}");
        assert!(peak < 32767, "and never clips");
    }
}
