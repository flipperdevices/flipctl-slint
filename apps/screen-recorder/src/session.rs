//! One recording: flipctl's frames and the sound, into one file.
//!
//! The sound is opened first, so it is already running when the picture starts, and
//! then flipctl is asked for the frames; it answers and sends the app to the
//! background. From there: one thread reads frames off flipctl's socket, one per
//! sound stream reads the sound server's, and one encodes it all and writes the file.
//! The recording ends when the app closes the stream (Stop or Close), or flipctl does.
//!
//! The movie starts at the first frame. Both sides are stamped on `CLOCK_MONOTONIC`, so
//! sound from before that frame is cut at the right sample, and every frame after it
//! is placed at its own time: a still screen is one long frame, not many. While the
//! app itself is in front flipctl holds the recording, and the stretch from its mark to
//! the next frame is cut out of both the picture and the sound.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

use flipper_ui::screen_rec::{self, Record};

use crate::mp3::{self, Mp3, RATE};
use crate::mp4::{self, Mp4, VIDEO_SCALE};
use crate::sound::{Capture, Sound, Source};
use crate::video::{self, Video, SCALE};

/// What the encoding thread is given.
pub enum Event {
    /// A frame, or with no size, flipctl's mark that the app came to the front.
    Frame(Record),
    /// Samples, interleaved when stereo, that finished arriving at `at_us`.
    Sound {
        source: Source,
        pcm: Vec<i16>,
        at_us: u64,
    },
    End {
        at_us: u64,
    },
}

/// How much sound is kept from before the first frame, in chunks of about 20 ms.
const EARLY_CHUNKS: usize = 100;

pub struct Session {
    socket: UnixStream,
    done: Receiver<Result<PathBuf, String>>,
    recorded: Arc<AtomicU64>,
}

impl Session {
    /// Start recording into `path`, with `sound` alongside. Blocks while the sound
    /// server and flipctl answer, so call it off the drawing thread.
    pub fn start(sound: Sound, path: PathBuf) -> Result<Self, String> {
        let (events, received) = mpsc::channel();
        let captures = sound
            .sources()
            .iter()
            .map(|s| Capture::open(*s, events.clone()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| format!("No sound: {e}"))?;
        let mut frames = ask()?;
        let socket = frames.get_ref().try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("frames".into())
            .spawn(move || loop {
                let event = match screen_rec::read(&mut frames) {
                    Ok(Some(record)) => Event::Frame(record),
                    _ => Event::End { at_us: screen_rec::now_us() },
                };
                let end = matches!(event, Event::End { .. });
                if events.send(event).is_err() || end {
                    break;
                }
            })
            .map_err(|e| e.to_string())?;
        let (tell, done) = mpsc::channel();
        let recorded = Arc::new(AtomicU64::new(0));
        let counting = Arc::clone(&recorded);
        std::thread::Builder::new()
            .name("encode".into())
            .spawn(move || {
                let outcome = encode(&received, captures, sound, &path, &counting);
                if outcome.is_err() {
                    let _ = std::fs::remove_file(&path);
                }
                let _ = tell.send(outcome.map(|()| path));
                flipctl_app::redraw();
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { socket, done, recorded })
    }

    /// A session with nothing behind it, `recorded` long, for drawing the page.
    #[cfg(test)]
    pub fn idle(recorded: Duration) -> Self {
        let (socket, _) = UnixStream::pair().unwrap();
        Self {
            socket,
            done: mpsc::channel().1,
            recorded: Arc::new(AtomicU64::new(recorded.as_micros() as u64)),
        }
    }

    /// How long the movie is so far.
    pub fn recorded(&self) -> Duration {
        Duration::from_micros(self.recorded.load(Ordering::Relaxed))
    }

    /// The file, once it is finished, or why there is none. Taken once.
    pub fn done(&self) -> Option<Result<PathBuf, String>> {
        self.done.try_recv().ok()
    }

    /// End the recording from this side.
    pub fn stop(&self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }

    /// Wait for the file, as the app closes.
    pub fn finish(self) {
        self.stop();
        let _ = self.done.recv_timeout(Duration::from_secs(10));
    }
}

/// `POST /screen-record` on flipctl's socket: the stream of frames once it answers.
fn ask() -> Result<BufReader<UnixStream>, String> {
    let socket = std::env::var_os("FLIPCTL_SOCKET")
        .ok_or_else(|| "flipctl is too old to ask; update it".to_string())?;
    let flipctl = |e: std::io::Error| format!("flipctl: {e}");
    let mut s = UnixStream::connect(socket).map_err(flipctl)?;
    write!(s, "POST /screen-record HTTP/1.1\r\nHost: flipctl\r\nContent-Length: 0\r\n\r\n")
        .map_err(flipctl)?;
    let mut reader = BufReader::new(s);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(flipctl)?;
    let mut line = String::new();
    while reader.read_line(&mut line).map_err(flipctl)? > 0 && line.trim_end() != "" {
        line.clear();
    }
    if status.split(' ').nth(1) != Some("200") {
        let mut body = String::new();
        let _ = reader.read_line(&mut body);
        return Err(error_of(&body).unwrap_or_else(|| status.trim().to_string()));
    }
    Ok(reader)
}

/// The `error` of flipctl's `{"error":"..."}`.
fn error_of(body: &str) -> Option<String> {
    let rest = body.split_once("\"error\":\"")?.1;
    Some(rest.split('"').next()?.to_string())
}

/// Microseconds into the movie as video ticks.
fn ticks(us: u64) -> u64 {
    us * u64::from(VIDEO_SCALE) / 1_000_000
}

/// Clock time to movie time.
struct Clock {
    /// When the first frame was committed.
    t0: u64,
    /// How much has been cut out for the app being in front.
    cut: u64,
    /// When flipctl marked the app coming to the front, while it is there.
    held_at: Option<u64>,
    /// Sound before this is not part of the movie: the start, or the end of a hold.
    sound_from: u64,
}

impl Clock {
    fn movie(&self, at_us: u64) -> u64 {
        at_us.saturating_sub(self.t0 + self.cut)
    }

    /// A frame at `at_us` ends a hold.
    fn resume(&mut self, at_us: u64) {
        if let Some(held) = self.held_at.take() {
            self.cut += at_us.saturating_sub(held);
            self.sound_from = at_us;
        }
    }
}

/// The microphone mixed into the speaker's stereo, for System + Mic.
///
/// Both come off one codec on one clock, so once the microphone is lined up with the
/// speaker by the times its first chunks arrived it stays lined up, and is taken
/// sample for sample. A queue that grows past `MIX_SLACK` has fallen out of step, a
/// stall on one stream, and is lined up again.
#[derive(Default)]
struct Mixer {
    mic: VecDeque<i16>,
    /// When the last microphone chunk arrived.
    mic_at_us: u64,
    lined_up: bool,
}

/// The most microphone the mixer will hold, a fifth of a second.
const MIX_SLACK: usize = RATE as usize / 5;

impl Mixer {
    fn mic(&mut self, pcm: &[i16], at_us: u64) {
        self.mic.extend(pcm);
        self.mic_at_us = at_us;
        if self.mic.len() > MIX_SLACK {
            self.lined_up = false;
        }
    }

    /// Stereo `speaker`, which finished arriving at `at_us`, with the microphone in it.
    fn mix(&mut self, speaker: &[i16], at_us: u64) -> Vec<i16> {
        let frames = speaker.len() / 2;
        if !self.lined_up && !self.mic.is_empty() {
            let start = at_us.saturating_sub(span_us(frames));
            let head = self.mic_at_us.saturating_sub(span_us(self.mic.len()));
            if head < start {
                let late = samples(start - head).min(self.mic.len());
                self.mic.drain(..late);
            } else {
                for _ in 0..samples(head - start).min(frames) {
                    self.mic.push_front(0);
                }
            }
            self.lined_up = true;
        }
        let add = |a: i16, b: i16| (i32::from(a) + i32::from(b)).clamp(-32768, 32767) as i16;
        let mut out = Vec::with_capacity(speaker.len());
        for pair in speaker.chunks_exact(2) {
            let m = self.mic.pop_front().unwrap_or(0);
            out.push(add(pair[0], m));
            out.push(add(pair[1], m));
        }
        out
    }
}

/// How long `frames` samples last, in microseconds.
fn span_us(frames: usize) -> u64 {
    frames as u64 * 1_000_000 / u64::from(RATE)
}

/// How many samples `us` microseconds is.
fn samples(us: u64) -> usize {
    (us * u64::from(RATE) / 1_000_000) as usize
}

/// The part of a chunk of sound that falls in `from..until`, the chunk having
/// finished arriving at `at_us`.
fn clip(pcm: &[i16], at_us: u64, from: u64, until: Option<u64>, channels: u16) -> &[i16] {
    let c = usize::from(channels);
    let frames = pcm.len() / c;
    let start = at_us.saturating_sub(span_us(frames));
    let first = samples(from.saturating_sub(start)).min(frames);
    let last = until.map_or(frames, |u| samples(u.saturating_sub(start)).min(frames));
    if last <= first {
        return &[];
    }
    &pcm[first * c..last * c]
}

/// The encoding thread: events until the end, then the file finished.
fn encode(
    events: &Receiver<Event>,
    captures: Vec<Capture>,
    sound: Sound,
    path: &Path,
    recorded: &AtomicU64,
) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let channels = sound.channels();
    let mut mp3 = channels.map(Mp3::new).transpose().map_err(io)?;
    let spec = channels.map(|c| mp4::Sound { rate: RATE, channels: c, bitrate: mp3::bitrate(c) });
    let mut out: Option<(Mp4, Video, Clock)> = None;
    let mut early: VecDeque<(Vec<i16>, u64)> = VecDeque::new();
    let mut mixer = Mixer::default();
    let mut end_us = None;
    let mut big = Vec::new();

    for event in events {
        match event {
            Event::Frame(record) if record.width == 0 => {
                if let Some((_, _, clock)) = out.as_mut() {
                    let held = *clock.held_at.get_or_insert(record.at_us);
                    recorded.store(clock.movie(held), Ordering::Relaxed);
                    flipctl_app::redraw();
                }
            }
            Event::Frame(record) => {
                if out.is_none() {
                    let (w, h) = (record.width * SCALE as u16, record.height * SCALE as u16);
                    let video = Video::new(w, h)?;
                    let mp4 = Mp4::create(path, w, h, spec).map_err(io)?;
                    let t0 = record.at_us;
                    let clock = Clock { t0, cut: 0, held_at: None, sound_from: t0 };
                    out = Some((mp4, video, clock));
                    if let (Some(mp3), Some(c), Some((mp4, _, _))) =
                        (mp3.as_mut(), channels, out.as_mut())
                    {
                        for (pcm, at_us) in early.drain(..) {
                            for frame in mp3.encode(clip(&pcm, at_us, t0, None, c)).map_err(io)? {
                                mp4.audio(&frame).map_err(io)?;
                            }
                        }
                    }
                }
                let Some((mp4, video, clock)) = out.as_mut() else { continue };
                clock.resume(record.at_us);
                let at = clock.movie(record.at_us);
                video::enlarge(&record.pixels, usize::from(record.width), &mut big);
                if let Some(sample) = video.encode(&big, record.at_us)? {
                    mp4.video(ticks(at), &sample.data, sample.key).map_err(io)?;
                }
                recorded.store(at, Ordering::Relaxed);
            }
            Event::Sound { source, pcm, at_us } => {
                let pcm = match (sound, source) {
                    (Sound::SystemMic, Source::Mic) => {
                        mixer.mic(&pcm, at_us);
                        continue;
                    }
                    (Sound::SystemMic, Source::Speaker) => mixer.mix(&pcm, at_us),
                    _ => pcm,
                };
                match (out.as_mut(), mp3.as_mut(), channels) {
                    (Some((mp4, _, clock)), Some(mp3), Some(c)) => {
                        let kept = clip(&pcm, at_us, clock.sound_from, clock.held_at, c);
                        for frame in mp3.encode(kept).map_err(io)? {
                            mp4.audio(&frame).map_err(io)?;
                        }
                    }
                    _ => {
                        early.push_back((pcm, at_us));
                        if early.len() > EARLY_CHUNKS {
                            early.pop_front();
                        }
                    }
                }
            }
            Event::End { at_us } => {
                end_us = Some(at_us);
                break;
            }
        }
    }
    drop(captures);

    let Some((mut mp4, video, clock)) = out else {
        return Err("Nothing was recorded".into());
    };
    if let Some(mp3) = mp3 {
        for frame in mp3.finish().map_err(io)? {
            mp4.audio(&frame).map_err(io)?;
        }
    }
    let (sps, pps) = video.parameter_sets().ok_or("no picture was encoded")?;
    mp4.set_parameter_sets(sps, pps);
    let end = clock.held_at.or(end_us).unwrap_or_else(screen_rec::now_us);
    mp4.finish(ticks(clock.movie(end))).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flipctl_says_why_it_refused() {
        assert_eq!(
            error_of(r#"{"error":"only the app in front can record, one at a time"}"#).as_deref(),
            Some("only the app in front can record, one at a time")
        );
        assert_eq!(error_of("{}"), None);
    }

    #[test]
    fn sound_is_cut_at_both_ends_of_what_is_kept() {
        // 480 stereo frames, 10 ms, finishing at 1.010 s.
        let pcm: Vec<i16> = (0..960).map(|i| i as i16).collect();
        let kept = clip(&pcm, 1_010_000, 1_005_000, None, 2);
        assert_eq!((kept.len(), kept[0]), (480, 480));
        assert!(clip(&pcm, 1_010_000, 2_000_000, None, 2).is_empty());
        assert_eq!(clip(&pcm, 1_010_000, 0, None, 2).len(), 960);
        let held = clip(&pcm, 1_010_000, 0, Some(1_002_500), 2);
        assert_eq!(held.len(), 240, "up to the hold");
        assert!(clip(&pcm, 1_010_000, 0, Some(900_000), 2).is_empty());
    }

    #[test]
    fn the_mic_is_lined_up_with_the_speaker_and_mixed_into_both_channels() {
        let mut mixer = Mixer::default();
        // 10 ms of mic finishing at 1.005 s, so starting at 0.995 s.
        mixer.mic(&[100; 480], 1_005_000);
        // 10 ms of speaker finishing at 1.010 s, starting at 1.000 s: the first 5 ms of
        // the mic are older and dropped.
        let out = mixer.mix(&[1000; 960], 1_010_000);
        assert_eq!(&out[..4], &[1100, 1100, 1100, 1100]);
        assert_eq!(&out[478..482], &[1100, 1100, 1000, 1000], "the mic runs out halfway");
        assert_eq!(mixer.mix(&[32_000; 4], 1_010_100), vec![32_000; 4]);
        mixer.mic(&[2000; 2], 1_010_200);
        assert_eq!(mixer.mix(&[32_000; 4], 1_010_300), vec![32_767; 4], "clamped, not wrapped");
    }

    #[test]
    fn video_time_is_counted_in_ninety_thousandths() {
        assert_eq!(ticks(1_000_000), 90_000);
        assert_eq!(ticks(16_667), 1500);
    }

    fn frame(at_us: u64, n: usize) -> Event {
        let pixels: Vec<u8> =
            (0..256 * 144).map(|i| if (i + n * 256) % 9 == 0 { 0 } else { 255 }).collect();
        Event::Frame(Record { at_us, width: 256, height: 144, pixels })
    }

    fn tone(chunk: u64, at_us: u64) -> Event {
        let pcm = (0..960 * 2)
            .map(|i| ((((chunk * 960) as usize + i / 2) as f32 / 10.0).sin() * 8000.0) as i16)
            .collect();
        Event::Sound { source: Source::Speaker, pcm, at_us }
    }

    /// The length FFmpeg reads for each stream, when ffprobe is installed.
    fn durations(path: &Path) -> Option<Vec<(String, f64)>> {
        let probe = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=codec_name,duration", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .ok()?;
        Some(
            String::from_utf8_lossy(&probe.stdout)
                .lines()
                .filter_map(|l| {
                    l.split_once(',').map(|(c, d)| (c.to_string(), d.parse().unwrap_or(0.0)))
                })
                .collect(),
        )
    }

    /// Three seconds of frames at uneven times and a tone, through the same path a
    /// recording takes. The file goes to target/ so FFmpeg can be pointed at it.
    #[test]
    fn a_recording_is_a_file_ffmpeg_reads() {
        let path = PathBuf::from("target/session-test.mp4");
        let (tx, rx) = mpsc::channel();
        let t0 = 10_000_000u64;
        // Sound from 100 ms before the first frame, in 20 ms chunks.
        for chunk in 0..155u64 {
            let at_us = t0 - 100_000 + (chunk + 1) * 20_000;
            tx.send(tone(chunk, at_us)).unwrap();
            if chunk % 3 == 0 && chunk >= 5 {
                tx.send(frame(at_us + 1000, (chunk - 5) as usize)).unwrap();
            }
        }
        tx.send(Event::End { at_us: t0 + 3_000_000 }).unwrap();
        encode(&rx, Vec::new(), Sound::System, &path, &AtomicU64::new(0)).unwrap();
        let Some(said) = durations(&path) else { return };
        assert_eq!(said.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(), ["h264", "mp3"]);
        for (codec, d) in said {
            assert!((2.9..=3.1).contains(&d), "{codec}: {d}");
        }
    }

    /// Four seconds with the app in front for the middle two, and stopped from there:
    /// the movie is the two seconds either side, sound and picture alike.
    #[test]
    fn the_time_the_app_is_in_front_is_cut_out() {
        let path = PathBuf::from("target/session-held.mp4");
        let (tx, rx) = mpsc::channel();
        let t0 = 10_000_000u64;
        let recorded = AtomicU64::new(0);
        for chunk in 0..250u64 {
            let at_us = t0 + (chunk + 1) * 20_000;
            tx.send(tone(chunk, at_us)).unwrap();
            let in_front = (50..150).contains(&chunk);
            if chunk == 50 {
                tx.send(Event::Frame(Record { at_us, width: 0, height: 0, pixels: Vec::new() }))
                    .unwrap();
            }
            if chunk % 3 == 0 && !in_front {
                tx.send(frame(at_us + 1000, chunk as usize)).unwrap();
            }
        }
        tx.send(Event::Frame(Record {
            at_us: t0 + 5_010_000,
            width: 0,
            height: 0,
            pixels: Vec::new(),
        }))
        .unwrap();
        tx.send(Event::End { at_us: t0 + 6_000_000 }).unwrap();
        encode(&rx, Vec::new(), Sound::System, &path, &recorded).unwrap();
        // Frames from 20 ms to 1 s, and from 3.04 s to 5.01 s, less the 2 s between.
        let movie = Duration::from_micros(recorded.load(Ordering::Relaxed)).as_secs_f64();
        assert!((2.9..=3.1).contains(&movie), "{movie}");
        let Some(said) = durations(&path) else { return };
        for (codec, d) in said {
            assert!((2.9..=3.1).contains(&d), "{codec}: {d}");
        }
    }
}
