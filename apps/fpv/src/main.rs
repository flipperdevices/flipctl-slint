//! FPV: analog 5.8 GHz video through the Flipper One's Wi-Fi chip, on the panel.
//!
//! The chip is an MT7921 that BirriaSDR turns into a receiver. The radio and the
//! decoder are the FPV receiver's own (FlipperOne-WiFi-SDR-FPV-receiver), copied here
//! unchanged apart from `birria.rs` and the fixes marked "Changed in flipctl's copy" in
//! `dsp/video.rs` and `weave.rs`; this file replaces its egui window with the panel's
//! pages, and `picture.rs` puts its 768x576 frame on a 256x144 grey panel.
//!
//! Four pages: the picture, the channel list from the last scan, the receiver's state,
//! and its settings. The picture has the whole panel, the soft keys over it until Hide
//! takes them away and any key brings them back. On it Left and Right step through the
//! 48 channels, Up and Down jump between the ones the scan found video on, and Ok opens
//! the list.

mod birria;
#[allow(dead_code)]
mod channels;
mod chip;
mod lag;
#[allow(dead_code)]
mod dsp;
#[allow(dead_code)]
mod front;
mod picture;
mod pin;
#[allow(dead_code)]
mod post;
// Unchanged from the receiver, warnings and all
#[allow(dead_code, deprecated)]
mod radio;
#[allow(dead_code)]
mod weave;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use birria::{Gain, ANTENNA_AUTO};
use channels::Channel;
#[cfg(test)]
use dsp::video::{HEIGHT, WIDTH};
use flipctl_app::paint::Surface;
use flipctl_app::theme::PANEL_W;
use flipctl_app::{heading, item, pair, rule, text, App, Key, Page, Row, Text, Ui};
use picture::{Fit, Levels, Tone};
use radio::{Command, Radio, Shared, NOT_VIEWING};

/// The picture at most this often. Fields arrive about every 20ms, with jitter, so a
/// limit of 20 skipped every field that came a little early.
const FRAME_MS: u64 = 12;
/// The channel shown at start, rather than a scan: Raceband 1, where the drones this was
/// built with sit. Scan is a key away.
const FIRST: &str = "R1";
/// The gain the radio starts with, before its AGC takes over.
const GAIN: Gain = Gain { lna: 4, lpf: 8, digital: 0 };

enum State {
    /// Taking the chip and opening the radio, on a thread: the claim takes seconds.
    Starting(Receiver<Result<Radio, String>>),
    Running(Radio),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Video,
    Channels,
    Info,
    Setup,
    /// The picture, with the D-pad on contrast and brightness.
    Tune,
}

struct Fpv {
    state: State,
    view: View,
    channels: Vec<Channel>,
    fit: Fit,
    tone: Tone,
    levels: Levels,
    /// The manual gain, which the setup changes and the radio uses with auto gain off.
    gain: Gain,
    error: Option<String>,
    /// The soft keys are off the picture, until the next key.
    hidden: bool,
    /// Whether the picture is on screen, which is when the radio's fields draw.
    live: Arc<AtomicBool>,
    meter: RateMeter,
    /// Ticks seen, which step the waiting dots.
    ticks: usize,
}

impl Fpv {
    fn new() -> Self {
        // BirriaSDR reads these when it opens: the burst birria_burst patched in, and its USB
        // threads on fast cores of their own. Set here, before any thread exists, so the
        // radio does not set them with the window's threads running.
        for (k, v) in [("BIRRIA_BURST", "3"), ("BIRRIA_CPUS", "6-7")] {
            if std::env::var_os(k).is_none() {
                std::env::set_var(k, v);
            }
        }
        // The window on the slow cores, leaving the fast ones to the decoder, which pins its
        // own threads when it starts. On a decoder's core it made the decoder fall behind.
        pin::pin_to(&[0, 1, 2, 3]);
        let live = Arc::new(AtomicBool::new(true));
        Fpv {
            state: State::Starting(start(live.clone())),
            view: View::Video,
            channels: channels::all(),
            fit: Fit::Box,
            tone: Tone::Grey,
            levels: Levels::DEFAULT,
            gain: GAIN,
            error: None,
            hidden: false,
            live,
            meter: RateMeter { at: Instant::now(), count: 0, rate: 0.0 },
            ticks: 0,
        }
    }

    fn radio(&self) -> Option<&Radio> {
        match &self.state {
            State::Running(r) => Some(r),
            _ => None,
        }
    }

    fn show(&mut self, view: View) {
        self.view = view;
        self.live.store(matches!(view, View::Video | View::Tune), Relaxed);
    }

    fn send(&self, c: Command) {
        if let Some(r) = self.radio() {
            r.send(c);
        }
    }

    /// Move `by` channels from the one on screen, round the ends.
    fn step(&self, s: &Shared, by: isize) {
        let n = self.channels.len() as isize;
        let current = viewing(s).unwrap_or(0) as isize;
        self.send(Command::View((current + by).rem_euclid(n) as usize));
    }

    /// The next channel the last scan found video on, in direction `by`; with none, the
    /// next channel.
    fn jump(&self, s: &Shared, by: isize) {
        let video = s.video.lock().unwrap().clone();
        let n = video.len() as isize;
        let current = viewing(s).unwrap_or(0) as isize;
        match (1..=n).map(|k| (current + by * k).rem_euclid(n) as usize).find(|&i| video[i] == Some(true)) {
            Some(i) => self.send(Command::View(i)),
            None => self.step(s, by),
        }
    }

    fn video(&self, s: &Shared, readout: Option<String>) -> Page {
        let post = s.post.load(Relaxed);
        let frame = if post { s.post_frame.lock().unwrap() } else { s.frame.lock().unwrap() };
        let osd = Osd::of(s, &self.channels, self.error.as_deref());
        // The decoder's lumas, unless they were made for the other shape
        let mine;
        let lumas = if frame.serial == 0 {
            None
        } else if frame.fill == (self.fit == Fit::Fill) && !frame.lumas.is_empty() {
            Some(&frame.lumas[..])
        } else {
            mine = picture::lumas_for(&frame.pixels, self.fit);
            Some(&mine[..])
        };
        let page = video_page(lumas, &osd, self.fit, self.tone, self.levels, readout);
        lag::drawn(frame.t);
        page
    }

    fn channel_list(&self, s: &Shared) -> Page {
        let powers = s.powers.lock().unwrap().clone();
        let video = s.video.lock().unwrap().clone();
        let on = viewing(s);
        let items = self
            .channels
            .iter()
            .enumerate()
            .map(|(i, ch)| {
                let mark = if on == Some(i) { " <" } else { "" };
                let row = item(format!("{} {}{mark}", ch.name(), ch.mhz));
                match (powers[i], video[i]) {
                    (Some(db), Some(true)) => row.status(format!("video {db:+.0}")),
                    (Some(db), Some(false)) => row.status(format!("{db:+.0} dB")).dim(),
                    (Some(db), None) => row.status(format!("{db:+.0} dB")),
                    _ => row,
                }
            })
            .collect();
        Page::menu(items).id("channels")
    }

    fn info(&self, s: &Shared) -> Page {
        let osd = Osd::of(s, &self.channels, None);
        let g = s.gain();
        let antenna = s.antenna.load(Relaxed);
        let used = s.antenna_used.load(Relaxed) + 1;
        let mut rows = vec![
            pair("Channel", if let Some(p) = osd.scan { format!("Scanning {p}%") } else { format!("{} {} MHz", osd.name, osd.mhz) }),
            pair("Sync", osd.sync_words()),
            pair("Video", format!("{} {} lines", if s.ntsc.load(Relaxed) { "NTSC" } else { "PAL" }, s.field_lines.load(Relaxed))),
            pair("Signal", format!("{:+.2} MHz off", s.signal_hz.load(Relaxed) as f64 / 1e6)),
            rule(),
            pair("Input", format!("{:.1} of {:.0} Msps", self.meter.rate / 1e6, s.rate.load(Relaxed) as f64 / 1e6)),
            pair("Gain", format!("{}/{}/{}{}", g.lna, g.lpf, g.digital, if s.auto_gain.load(Relaxed) { " auto" } else { "" })),
            pair("Antenna", if antenna == ANTENNA_AUTO { format!("Auto ({used})") } else { used.to_string() }),
            pair("Lag", lag::summary()),
            pair("Lost", format!("{} usb, {} dec, {} gaps", short(s.lost_usb.load(Relaxed)), short(s.lost_video.load(Relaxed)), s.gaps.load(Relaxed))),
        ];
        if let Some(e) = &self.error {
            rows.push(rule());
            rows.extend(wrap(e).into_iter().map(text));
        }
        Page::detail(rows).id("info")
    }

    fn setup(&self, s: &Shared) -> Page {
        let auto = s.auto_gain.load(Relaxed);
        let items = SETTINGS
            .iter()
            .map(|&which| {
                if which == Setting::Tune {
                    return item(which.label()).status(format!("{}% {:+}", self.levels.contrast, self.levels.brightness));
                }
                let (at, n) = (self.current(s, which), options(which));
                let row = item(which.label()).range(self.option(which, at), at == 0, at + 1 == n);
                if auto && matches!(which, Setting::Lna | Setting::Lpf | Setting::Digital) {
                    row.dim()
                } else {
                    row
                }
            })
            .collect();
        Page::menu(items).id("setup")
    }

    /// The tuner's numbers, as its row and its strip show them.
    fn levels_text(&self) -> String {
        format!("Contrast {}%  Brightness {:+}", self.levels.contrast, self.levels.brightness)
    }

    fn option(&self, which: Setting, at: usize) -> String {
        let names: &[&str] = match which {
            Setting::Standard => &["Auto", "PAL", "NTSC"],
            Setting::Rate => &["20 Msps", "40 Msps", "80 Msps"],
            Setting::Antenna => &["Auto", "1", "2"],
            Setting::Picture => &["Tones", "1-bit", "Grey"],
            Setting::Fit => &["4:3", "Fill"],
            Setting::Lna | Setting::Lpf | Setting::Digital => return at.to_string(),
            _ => &["Off", "On"],
        };
        names[at].to_string()
    }

    fn current(&self, s: &Shared, which: Setting) -> usize {
        let g = if s.auto_gain.load(Relaxed) { s.gain() } else { self.gain };
        match which {
            Setting::Standard => s.standard.load(Relaxed) as usize,
            Setting::Rate => RATES.iter().position(|&r| r == s.view_rate.load(Relaxed)).unwrap_or(0),
            Setting::Antenna => match s.antenna.load(Relaxed) {
                ANTENNA_AUTO => 0,
                a => a as usize + 1,
            },
            Setting::AutoGain => s.auto_gain.load(Relaxed) as usize,
            Setting::Lna => g.lna as usize,
            Setting::Lpf => g.lpf as usize,
            Setting::Digital => g.digital as usize,
            Setting::CleanUp => s.post.load(Relaxed) as usize,
            Setting::AutoTune => s.afc.load(Relaxed) as usize,
            Setting::Strongest => s.to_strongest.load(Relaxed) as usize,
            Setting::Picture => self.tone as usize,
            Setting::Tune => 0,
            Setting::Fit => self.fit as usize,
        }
    }

    /// Change a setting to option `at`, as the receiver's window did for its control.
    fn apply(&mut self, s: &Shared, which: Setting, at: usize) {
        match which {
            Setting::Standard => s.standard.store(at as u8, Relaxed),
            Setting::Rate => {
                s.view_rate.store(RATES[at], Relaxed);
                // The rate cannot change during a stream: viewing again restarts it
                if let Some(i) = viewing(s) {
                    self.send(Command::View(i));
                }
            }
            Setting::Antenna => {
                s.antenna.store(if at == 0 { ANTENNA_AUTO } else { at as u8 - 1 }, Relaxed);
                self.send(Command::Antenna);
            }
            Setting::AutoGain => {
                s.auto_gain.store(at == 1, Relaxed);
                if at == 0 {
                    // Manual from where the AGC left it, not from a value set long ago
                    self.gain = s.gain();
                    self.send(Command::Gain(self.gain));
                }
            }
            Setting::Lna | Setting::Lpf | Setting::Digital => {
                if s.auto_gain.load(Relaxed) {
                    return;
                }
                let code = at as u8;
                match which {
                    Setting::Lna => self.gain.lna = code,
                    Setting::Lpf => self.gain.lpf = code,
                    _ => self.gain.digital = code,
                }
                self.send(Command::Gain(self.gain));
            }
            Setting::CleanUp => s.post.store(at == 1, Relaxed),
            Setting::AutoTune => s.afc.store(at == 1, Relaxed),
            Setting::Strongest => s.to_strongest.store(at == 1, Relaxed),
            Setting::Picture => self.tone = [Tone::Tones, Tone::Bits, Tone::Grey][at],
            Setting::Tune => {}
            Setting::Fit => {
                self.fit = [Fit::Box, Fit::Fill][at];
                s.fill.store(self.fit == Fit::Fill, Relaxed);
            }
        }
    }

    /// Left and Right move a setting along its options and stop at the ends; Ok goes
    /// round.
    fn change(&mut self, s: &Shared, row: usize, key: Key) {
        let Some(&which) = SETTINGS.get(row) else { return };
        if which == Setting::Tune {
            if key == Key::Ok {
                self.show(View::Tune);
            }
            return;
        }
        let (at, n) = (self.current(s, which), options(which));
        let to = match key {
            Key::Left => at.saturating_sub(1),
            Key::Right => (at + 1).min(n - 1),
            _ => (at + 1) % n,
        };
        if to != at {
            self.apply(s, which, to);
        }
    }
}

impl App for Fpv {
    fn page(&self) -> Page {
        match &self.state {
            State::Starting(_) => Page::detail(vec![
                heading("Configuring", ""),
                text(""),
                text("Configuring the Wi-Fi device."),
                text(format!("Please wait{}", &"..."[..self.ticks % 4])),
            ])
            .id("starting"),
            State::Failed(e) => Page::detail(wrap(e).into_iter().map(text).collect::<Vec<Row>>()).id("failed"),
            State::Running(r) => match self.view {
                View::Video => self.video(&r.shared, None),
                View::Tune => self.video(&r.shared, Some(self.levels_text())),
                View::Channels => self.channel_list(&r.shared),
                View::Info => self.info(&r.shared),
                View::Setup => self.setup(&r.shared),
            },
        }
    }

    fn buttons(&self) -> [&str; 5] {
        match (&self.state, self.view) {
            (State::Starting(_), _) => ["Close", "", "", "", ""],
            (State::Failed(_), _) => ["Close", "Retry", "", "", ""],
            (_, View::Video) if self.hidden => ["", "", "", "", ""],
            (_, View::Video) => ["Close", "Scan", "", "Hide", "Setup"],
            (_, View::Channels) => ["Back", "Scan", "", "Info", "View"],
            (_, View::Info) => ["Back", "Scan", "", "", ""],
            (_, View::Setup) => ["Back", "", "", "", ""],
            (_, View::Tune) => ["Back", "", "", "Reset", ""],
        }
    }

    fn key(&mut self, key: Key, ui: &mut Ui) -> bool {
        if let State::Failed(_) = self.state {
            if key == Key::View {
                self.state = State::Starting(start(self.live.clone()));
                return true;
            }
            return false;
        }
        let Some(s) = self.radio().map(|r| r.shared.clone()) else { return false };
        // With the keys hidden, a key only brings them back: pressed blind, it would
        // otherwise do whatever the slot under the finger does, Close included
        if self.hidden {
            self.hidden = false;
            return true;
        }
        match (self.view, key) {
            (View::Tune, Key::Left) => self.levels.contrast_by(-1),
            (View::Tune, Key::Right) => self.levels.contrast_by(1),
            (View::Tune, Key::Up) => self.levels.brightness_by(1),
            (View::Tune, Key::Down) => self.levels.brightness_by(-1),
            (View::Tune, Key::Edit) => self.levels = Levels::DEFAULT,
            (View::Tune, Key::Escape) => self.show(View::Setup),
            (View::Tune, _) => return false,
            (_, Key::View) if self.view != View::Setup => self.send(Command::Scan),
            (View::Video, Key::Left) => self.step(&s, -1),
            (View::Video, Key::Right) => self.step(&s, 1),
            (View::Video, Key::Up) => self.jump(&s, -1),
            (View::Video, Key::Down) => self.jump(&s, 1),
            (View::Video, Key::Ok) => self.show(View::Channels),
            (View::Video, Key::Edit) => self.hidden = true,
            (View::Channels, Key::Edit) => self.show(View::Info),
            (View::Video, Key::Run) => self.show(View::Setup),
            (View::Channels, Key::Ok | Key::Run) => {
                self.send(Command::View(ui.selected()));
                self.show(View::Video);
            }
            (View::Setup, Key::Left | Key::Right | Key::Ok) => self.change(&s, ui.selected(), key),
            (View::Channels | View::Info | View::Setup, Key::Escape) => self.show(View::Video),
            _ => return false,
        }
        true
    }

    fn every(&self) -> Option<Duration> {
        Some(Duration::from_millis(250))
    }

    fn tick(&mut self, _: &mut Ui) {
        self.ticks += 1;
        if let State::Starting(rx) = &self.state {
            match rx.try_recv() {
                Ok(Ok(r)) => self.state = State::Running(r),
                Ok(Err(e)) => self.state = State::Failed(e),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.state = State::Failed("The radio did not start".into()),
            }
        }
        if let Some(s) = self.radio().map(|r| r.shared.clone()) {
            if let Some(e) = s.error.lock().unwrap().take() {
                self.error = Some(e);
            }
            self.meter.update(s.input.load(Relaxed));
        }
    }

    fn close(&mut self) {
        // The radio closed before the chip goes back: powering it off under a reader
        // leaves it stuck. A start still running is waited for, so it cannot take the chip
        // after the app has given it back.
        match std::mem::replace(&mut self.state, State::Failed(String::new())) {
            State::Starting(rx) => drop(rx.recv()),
            State::Running(r) => drop(r),
            State::Failed(_) => {}
        }
        chip::release();
    }
}

/// Take the chip if it is not held yet and start the radio, on a thread of its own, on
/// the first channel.
fn start(live: Arc<AtomicBool>) -> Receiver<Result<Radio, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let opened = (|| {
            lag::event(format_args!("claiming"));
            birria::load()?;
            chip::claim()?;
            lag::event(format_args!("claimed"));
            // The panel is grey: colour decoding would cost most of a slow core for nothing
            let radio = Radio::start(GAIN, false, throttled(live))?;
            let first = channels::all().iter().position(|c| c.name() == FIRST).unwrap_or(0);
            radio.send(Command::View(first));
            Ok(radio)
        })();
        let _ = tx.send(opened);
        flipctl_app::redraw();
    });
    rx
}

/// The radio's repaint: a redraw per FRAME_MS at most, and none while the picture is not
/// on screen, where a redraw would rebuild a menu fifty times a second.
fn throttled(live: Arc<AtomicBool>) -> impl Fn() + Send + Sync + 'static {
    let (start, last) = (Instant::now(), AtomicU64::new(0));
    move || {
        let now = start.elapsed().as_millis() as u64;
        let prev = last.load(Relaxed);
        if live.load(Relaxed) && now >= prev + FRAME_MS && last.compare_exchange(prev, now, Relaxed, Relaxed).is_ok() {
            flipctl_app::redraw();
        }
    }
}

fn viewing(s: &Shared) -> Option<usize> {
    let v = s.viewing.load(Relaxed);
    (v != NOT_VIEWING).then_some(v)
}

/// What the picture page says beside or over the picture.
#[derive(Default)]
struct Osd {
    name: String,
    mhz: String,
    hlock: bool,
    vlock: bool,
    /// The scan's progress, percent, while it runs.
    scan: Option<u32>,
    error: bool,
}

impl Osd {
    fn of(s: &Shared, channels: &[Channel], error: Option<&str>) -> Osd {
        let (name, mhz) = match viewing(s) {
            Some(i) => (channels[i].name(), channels[i].mhz.to_string()),
            None => ("-".into(), "-".into()),
        };
        Osd {
            name,
            mhz,
            hlock: s.hlock.load(Relaxed) > front::HLOCKED,
            vlock: s.vlock.load(Relaxed) > 15,
            scan: s.scanning.load(Relaxed).then(|| (f32::from_bits(s.scan_progress.load(Relaxed)) * 100.0) as u32),
            error: error.is_some(),
        }
    }

    /// The lock in the four characters a side bar has room for.
    fn sync(&self) -> &'static str {
        match (self.hlock, self.vlock) {
            (true, true) => "Lock",
            (true, false) => "H",
            (false, true) => "V",
            (false, false) => "--",
        }
    }

    fn sync_words(&self) -> String {
        let word = |on| if on { "locked" } else { "-" };
        format!("H {}, V {}", word(self.hlock), word(self.vlock))
    }
}

/// The picture page: the frame, or black before the first one, and what the OSD says.
/// With the 4:3 box it goes in the bars beside the picture; stretched across, it goes
/// in a black strip over the top only while there is something to say. Either way below
/// flipctl's status strip, which the panel draws over the app's top rows.
fn video_page(lumas: Option<&[u8]>, osd: &Osd, fit: Fit, tone: Tone, levels: Levels, readout: Option<String>) -> Page {
    let mut surface = Surface::panel();
    match lumas {
        Some(lumas) => picture::paint(&mut surface, lumas, fit, tone, levels),
        None => surface.rect(0, 0, i32::from(PANEL_W), picture::VIDEO_Y + picture::VIDEO_H, flipctl_app::theme::color::BLACK),
    }
    let label = |x: i32, y: i32, line: String, font: i32, align: i32| Text { x: x as f32, y: y as f32, text: line, font, align, white: true };
    let (right, sync) = match osd.scan {
        Some(p) => (format!("{p}%"), "Scan"),
        None => (if osd.error { "Err".to_string() } else { String::new() }, osd.sync()),
    };
    let mut texts = Vec::new();
    if let Some(line) = readout {
        // The tuner's numbers, in a strip over the top, in place of the OSD
        surface.rect(0, picture::VIDEO_Y, i32::from(PANEL_W), 13, flipctl_app::theme::color::BLACK);
        texts.push(label(i32::from(PANEL_W) / 2, picture::VIDEO_Y, line, 1, 0));
        return Page::canvas(flipctl_app::picture(&surface), texts);
    }
    match fit {
        Fit::Box => {
            let (x, _, w, _) = picture::area(Fit::Box);
            let (left_mid, right_mid) = (x / 2, x + w + (i32::from(PANEL_W) - x - w) / 2);
            let top = picture::VIDEO_Y + 2;
            texts.push(label(left_mid, top, osd.name.clone(), 2, 0));
            texts.push(label(left_mid, top + 14, osd.mhz.clone(), 1, 0));
            texts.push(label(right_mid, top, sync.into(), 1, 0));
            texts.push(label(right_mid, top + 14, right, 1, 0));
        }
        Fit::Fill if osd.scan.is_some() || !(osd.hlock && osd.vlock) || osd.error => {
            let top = picture::VIDEO_Y;
            surface.rect(0, top, i32::from(PANEL_W), 13, flipctl_app::theme::color::BLACK);
            texts.push(label(3, top, format!("{} {}", osd.name, osd.mhz), 1, -1));
            texts.push(label(i32::from(PANEL_W) - 3, top, format!("{sync} {right}").trim_end().to_string(), 1, 1));
        }
        Fit::Fill => {}
    }
    Page::canvas(flipctl_app::picture(&surface), texts)
}

/// Samples per second from a running count, over the last second or so.
struct RateMeter {
    at: Instant,
    count: u64,
    rate: f64,
}

impl RateMeter {
    fn update(&mut self, count: u64) {
        let secs = self.at.elapsed().as_secs_f64();
        if secs >= 1.0 {
            self.rate = count.saturating_sub(self.count) as f64 / secs;
            self.count = count;
            self.at = Instant::now();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Setting {
    Standard,
    Rate,
    Antenna,
    AutoGain,
    Lna,
    Lpf,
    Digital,
    CleanUp,
    AutoTune,
    Strongest,
    Picture,
    Tune,
    Fit,
}

const SETTINGS: [Setting; 13] = [
    Setting::Fit,
    Setting::Picture,
    Setting::Tune,
    Setting::Standard,
    Setting::Rate,
    Setting::Antenna,
    Setting::AutoGain,
    Setting::Lna,
    Setting::Lpf,
    Setting::Digital,
    Setting::CleanUp,
    Setting::AutoTune,
    Setting::Strongest,
];

const RATES: [u32; 3] = [20_000_000, 40_000_000, 80_000_000];

impl Setting {
    fn label(self) -> &'static str {
        match self {
            Setting::Standard => "Standard",
            Setting::Rate => "View rate",
            Setting::Antenna => "Antenna",
            Setting::AutoGain => "Auto gain",
            Setting::Lna => "LNA",
            Setting::Lpf => "Filter gain",
            Setting::Digital => "Digital gain",
            Setting::CleanUp => "Clean up",
            Setting::AutoTune => "Auto-tune",
            Setting::Strongest => "Best after scan",
            Setting::Picture => "Picture",
            Setting::Tune => "Contrast, brightness",
            Setting::Fit => "Shape",
        }
    }
}

/// How many options a setting has: the gain codes' ranges are the library's.
fn options(which: Setting) -> usize {
    match which {
        Setting::Standard | Setting::Rate | Setting::Antenna | Setting::Picture => 3,
        Setting::Lna => 6,
        Setting::Lpf => 9,
        Setting::Digital => 13,
        _ => 2,
    }
}

/// A count in four characters or so.
fn short(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=9_999_999 => format!("{}k", n / 1000),
        _ => format!("{}M", n / 1_000_000),
    }
}

/// A message as lines a row has room for, in the fonts' printable ASCII.
fn wrap(message: &str) -> Vec<String> {
    const WIDTH: usize = 36;
    let clean: String = message.chars().map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' }).collect();
    let mut lines = vec![String::new()];
    for word in clean.split_whitespace() {
        let line = lines.last_mut().unwrap();
        if !line.is_empty() && line.len() + 1 + word.len() > WIDTH {
            lines.push(word.to_string());
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    lines
}

flipctl_app::main!(Fpv::new());

#[cfg(test)]
mod tests {
    use super::*;
    use flipctl_app::{theme, Status};

    /// A page with no radio behind it, to draw.
    struct Still(Box<dyn Fn() -> Page>);

    impl App for Still {
        fn page(&self) -> Page {
            (self.0)()
        }
        fn buttons(&self) -> [&str; 5] {
            ["Close", "Scan", "", "Info", "Setup"]
        }
    }

    /// A test card: a ramp from black to white across, with a white box on black in the
    /// middle, so both the dither and the edges show.
    fn card() -> Vec<u8> {
        let mut rgb = vec![0u8; WIDTH * HEIGHT * 3];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let v = if (HEIGHT / 3..2 * HEIGHT / 3).contains(&y) {
                    if (WIDTH / 3..2 * WIDTH / 3).contains(&x) { 255 } else { 0 }
                } else {
                    (x * 255 / (WIDTH - 1)) as u8
                };
                rgb[(y * WIDTH + x) * 3..(y * WIDTH + x) * 3 + 3].fill(v);
            }
        }
        rgb
    }

    fn osd(locked: bool) -> Osd {
        Osd { name: "R5".into(), mhz: "5806".into(), hlock: locked, vlock: locked, ..Osd::default() }
    }

    /// The frame as the panel gets it, left in target/render when `FPV_RENDER=1` asks.
    fn shot(name: &str, fit: Fit, tone: Tone, locked: bool) -> Vec<u8> {
        let rgb = card();
        let frame = flipctl_pages::shot(Still(Box::new(move || video_page(Some(&picture::lumas_for(&rgb, fit)), &osd(locked), fit, tone, Levels { contrast: 100, brightness: 0 }, None))), Status::default());
        if std::env::var_os("FPV_RENDER").is_some() {
            let path = std::path::Path::new("target/render").join(format!("{name}.png"));
            flipctl_app::render::save(&path, &frame);
        }
        frame
    }

    #[test]
    fn the_picture_is_drawn_only_in_tokens() {
        let allowed: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
        for (name, fit, tone, locked) in [
            ("box-tones", Fit::Box, Tone::Tones, true),
            ("box-bits", Fit::Box, Tone::Bits, false),
            ("fill-tones", Fit::Fill, Tone::Tones, false),
            ("fill-locked", Fit::Fill, Tone::Tones, true),
        ] {
            let frame = shot(name, fit, tone, locked);
            let stride = usize::from(theme::PANEL_W);
            let offenders: Vec<String> = frame
                .iter()
                .enumerate()
                .filter(|(_, px)| !allowed.contains(px))
                .take(8)
                .map(|(i, px)| format!("({}, {}) = {px}", i % stride, i / stride))
                .collect();
            assert!(offenders.is_empty(), "{name}: off-palette pixels: {offenders:?}");
        }
    }

    #[test]
    fn grey_keeps_the_decoders_own_levels() {
        let frame = shot("box-grey", Fit::Box, Tone::Grey, true);
        let allowed: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
        assert!(frame.iter().any(|px| !allowed.contains(px)));
    }

    #[test]
    fn a_message_wraps_at_words_and_keeps_to_ascii() {
        let lines = wrap("Could not open the Wi-Fi chip: busy. Is another SDR app running? \u{2014} now");
        assert!(lines.iter().all(|l| l.len() <= 36 && l.is_ascii()), "{lines:?}");
        assert_eq!(lines.join(" "), "Could not open the Wi-Fi chip: busy. Is another SDR app running? ? now");
    }

    #[test]
    fn every_setting_shows_each_of_its_options() {
        for which in SETTINGS {
            assert!(options(which) >= 2, "{which:?}");
        }
        assert_eq!(options(Setting::Rate), RATES.len());
    }
}
