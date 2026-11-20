//! USB Audio Bridge: the Flipper as the PC's USB sound card.
//!
//! The device port becomes a USB Audio Class 2 card, stereo at 48 kHz both ways,
//! which Linux, macOS and Windows 10 and later drive without a driver of their own.
//! The port is named as the case names it, USB-C1. What the PC plays comes out of the
//! output chosen on From USB-C1, and what the PC records is chosen on To USB-C1: the
//! sound of the Flipper's own apps, a Flipper input, or nothing. The outputs are named
//! as Internet radio names them. Each direction has a volume of its own, set on the
//! card's side for it, so nothing else on the Flipper changes. Speaker Volume is the
//! speaker's own, the one Internet radio moves; OK mutes it, and a mute the bridge
//! made is undone when it closes. Test sends the PC a quiet tone for two seconds.
//!
//! Record saves what goes over the cable while the bridge runs, one file per
//! direction in ~/Recordings, as WAV, FLAC or MP3; changing either row starts a new
//! set of files.
//! The Flipper's apps are linked to the card one by one rather than through a monitor,
//! so the PC never hears its own sound coming back. Both may change while running.
//!
//! The card has no volume the PC can set, so the PC's volume and mute work on the PC. While
//! it runs the port is the card's alone: the Flipper's USB network, MTP and serial
//! console come back when it stops.

mod graph;
mod record;
mod route;

use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use flipctl_app::font::{BitmapFont, ROW, ROW_ACTIVE};
use flipctl_app::{item, App, Item, Key, Page, Ui};

use crate::graph::Graph;
use crate::record::{Format, Recording, FORMATS};
use crate::route::{set_mute, set_volume, Loopback, Meter, Seen, Tone, Watch};

// A menu row's text runs from past its icon (3 + 14 + 4) to the status inset (5) of
// its 224px line. A value between chevrons gives up room to them.
const MENU_WIDTH: u16 = 224 - 21 - 5;
const GAP: u16 = 8;
const CHEVRONS: u16 = 16;

/// A volume as set on a node: the node and the percent.
type Set = (u64, i32);

/// The rows that take keys, by position.
const ROW_FROM: usize = 0;
const ROW_TO: usize = 1;
const ROW_VOLUME_FROM: usize = 2;
const ROW_VOLUME_TO: usize = 3;
const ROW_SPEAKER: usize = 4;
const ROW_RECORD: usize = 5;
const ROW_FORMAT: usize = 6;

/// Which ways the sound is saved, in the order the Record row steps through them.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Record {
    None,
    From,
    To,
    Both,
}

const RECORDS: [Record; 4] = [Record::None, Record::From, Record::To, Record::Both];

/// How long Record and Format have to stay put before a new set of files starts, so
/// stepping past a choice on the way to another leaves no file behind.
const SETTLE: Duration = Duration::from_secs(1);

/// Where `value` is in `all`.
fn at<T: PartialEq>(all: &[T], value: T) -> usize {
    all.iter().position(|v| *v == value).unwrap_or(0)
}

/// Start saving what `record` asks for into ~/Recordings: what the PC plays from the
/// card's source `from`, what it hears from its sink `to`.
fn record_to(
    record: Record,
    format: Format,
    from: &str,
    to: &str,
) -> std::io::Result<Vec<Recording>> {
    let dir = record::folder();
    std::fs::create_dir_all(&dir)?;
    let when = stamp();
    let mut started = Vec::new();
    if matches!(record, Record::From | Record::Both) {
        let path = dir.join(record::file_name(&when, "from", format));
        started.push(Recording::start("from", from, false, &path, format)?);
    }
    if matches!(record, Record::To | Record::Both) {
        let path = dir.join(record::file_name(&when, "to", format));
        started.push(Recording::start("to", to, true, &path, format)?);
    }
    Ok(started)
}

/// The local time as a recording's name has it.
fn stamp() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

/// The volumes move in fives, as Internet radio's does.
const VOLUME_STEP: i32 = 5;

/// A level shown falls by this much each tick, so a peak stays long enough to read,
/// and below the floor it is silence.
const FALL_DB: f32 = 1.5;
const FLOOR_DB: f32 = -60.0;

/// `text`, cut with `..` to fit `room` pixels in `font`. The fonts have no ellipsis.
fn fit(text: &str, room: u16, font: &BitmapFont) -> String {
    let text: String =
        text.chars().map(|c| if (' '..='~').contains(&c) { c } else { '?' }).collect();
    let text = text.trim().to_string();
    if font.text_width(&text) <= room {
        return text;
    }
    let mut cut = text;
    while !cut.is_empty() && font.text_width(&format!("{}..", cut.trim_end())) > room {
        cut.pop();
    }
    format!("{}..", cut.trim_end())
}

/// A volume row, between chevrons in fives.
fn volume(label: &str, percent: i32) -> Item {
    let steps = (100 / VOLUME_STEP + 1) as usize;
    chosen(label, &format!("{percent}%"), (percent / VOLUME_STEP) as usize, steps)
}

/// A row whose label and status share its width without touching.
fn listed(label: &str, status: &str) -> Item {
    let budget = MENU_WIDTH - GAP;
    let room = budget.saturating_sub(ROW.text_width(status)).max(budget / 2);
    let label = fit(label, room, &ROW_ACTIVE);
    let status = fit(status, budget.saturating_sub(ROW_ACTIVE.text_width(&label)), &ROW);
    item(label).status(status)
}

/// A row Left and Right step through.
fn chosen(label: &str, value: &str, at: usize, count: usize) -> Item {
    if count < 2 {
        return listed(label, value);
    }
    let room = MENU_WIDTH.saturating_sub(ROW_ACTIVE.text_width(label) + GAP + CHEVRONS);
    item(label).range(fit(value, room, &ROW), at == 0, at + 1 == count)
}

/// A peak sample as dBFS.
fn db(peak: u32) -> f32 {
    if peak == 0 {
        return f32::NEG_INFINITY;
    }
    20.0 * (peak as f32 / 32768.0).log10()
}

/// A level row: a bar from the floor to full scale with the figure in it, while
/// there is a level to show.
fn level_row(label: &str, running: bool, db: f32) -> Item {
    let text = level(running, db);
    if !running {
        return listed(label, &text);
    }
    let fraction = if db < FLOOR_DB { 0.0 } else { 1.0 - db.min(0.0) / FLOOR_DB };
    let room = MENU_WIDTH - flipctl_app::theme::metric::MENU_BAR_W as u16 - GAP;
    item(fit(label, room, &ROW_ACTIVE)).bar(text, fraction)
}

fn level(running: bool, db: f32) -> String {
    match running {
        false => "-".into(),
        true if db < FLOOR_DB => "silent".into(),
        // Rounded first, or a peak just under full scale reads "-0"
        true => format!("{} dB", db.min(0.0).round() as i32),
    }
}

/// What the PC records.
#[derive(Clone, Debug, PartialEq)]
enum ToPc {
    Off,
    /// The Flipper's apps.
    Flipper,
    /// A Flipper input, by node name.
    Input(String),
}

/// What the claim's thread says.
enum Msg {
    Held(UnixStream),
    Error(String),
}

struct Bridge {
    /// What the case calls the port the PC is on.
    port: String,
    watch: Watch,
    seen: Seen,
    /// The output the PC's sound plays on, by node name, or none.
    from_pc: Option<String>,
    to_pc: ToPc,
    /// Start was pressed and not Stop since.
    wanted: bool,
    held: Option<UnixStream>,
    error: String,
    from_loop: Option<Loopback>,
    to_loop: Option<Loopback>,
    meters: Option<(Meter, Meter)>,
    levels: (f32, f32),
    /// What the PC is heard at here and hears from here, in percent, set on the card's
    /// own two sides, and each as last set: the node and the volume.
    volumes: (i32, i32),
    set: (Option<Set>, Option<Set>),
    tone: Option<Tone>,
    /// Whether the speaker was muted before this app first muted or unmuted it, to
    /// leave it that way on close: the mute is for while the bridge runs.
    speaker_was_muted: Option<bool>,
    /// What is saved and how, the files being written, and what they were started
    /// for: a change of either starts a new set.
    record: Record,
    format: Format,
    recordings: Vec<Recording>,
    recording_for: Option<(Record, Format)>,
    /// When Record or Format last changed.
    chosen_at: Instant,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Bridge {
    fn new() -> Bridge {
        let (tx, rx) = channel();
        let watch = Watch::start();
        let seen = watch.seen.lock().unwrap().clone();
        // The first output, which is the speaker, as Internet radio starts on it
        let from_pc = seen
            .graph
            .as_ref()
            .ok()
            .and_then(|g| g.named_sinks().into_iter().next())
            .map(|(n, _)| n);
        let port = flipctl_app::device_port_name();
        Bridge {
            port: if port.is_empty() { "USB-C".into() } else { port },
            watch,
            seen,
            from_pc,
            to_pc: ToPc::Flipper,
            wanted: false,
            held: None,
            error: String::new(),
            from_loop: None,
            to_loop: None,
            meters: None,
            levels: (f32::NEG_INFINITY, f32::NEG_INFINITY),
            volumes: (100, 100),
            set: (None, None),
            tone: None,
            speaker_was_muted: None,
            record: Record::None,
            format: Format::Wav,
            recordings: Vec::new(),
            recording_for: None,
            chosen_at: Instant::now(),
            tx,
            rx,
        }
    }

    fn graph(&self) -> Option<&Graph> {
        self.seen.graph.as_ref().ok()
    }

    fn running(&self) -> bool {
        self.held.is_some()
    }

    // What there is to choose: each list starts with Off, and a choice whose device
    // has gone shows as Off until it comes back.

    fn outputs(&self) -> Vec<(Option<String>, String)> {
        let mut all = vec![(None, "Off".to_string())];
        if let Some(g) = self.graph() {
            all.extend(g.named_sinks().into_iter().map(|(name, label)| (Some(name), label)));
        }
        all
    }

    fn sends(&self) -> Vec<(ToPc, String)> {
        let mut all = vec![(ToPc::Off, "Off".to_string()), (ToPc::Flipper, "App sound".into())];
        if let Some(g) = self.graph() {
            all.extend(
                g.named_sources().into_iter().map(|(name, label)| (ToPc::Input(name), label)),
            );
        }
        all
    }

    fn output_at(&self) -> usize {
        self.outputs().iter().position(|(c, _)| *c == self.from_pc).unwrap_or(0)
    }

    fn send_at(&self) -> usize {
        self.sends().iter().position(|(c, _)| *c == self.to_pc).unwrap_or(0)
    }

    fn record_name(&self) -> String {
        match self.record {
            Record::None => "None".into(),
            Record::From => format!("From {}", self.port),
            Record::To => format!("To {}", self.port),
            Record::Both => "Both".into(),
        }
    }

    /// The speaker's own volume, which Internet radio's Volume row moves too, and
    /// whether it is muted, which OK changes.
    fn speaker_row(&self) -> Item {
        match self.seen.speaker {
            Some((_, (v, muted))) => {
                let shown = if muted { format!("{v}% muted") } else { format!("{v}%") };
                let steps = (100 / VOLUME_STEP + 1) as usize;
                chosen("Speaker Volume", &shown, (v / VOLUME_STEP) as usize, steps)
            }
            None => listed("Speaker Volume", "-"),
        }
    }

    // Running

    /// The card's side the PC records, while there is one.
    fn card_sink(&self) -> Option<String> {
        self.graph()?.gadget("Audio/Sink").map(|n| n.name.clone())
    }

    /// A tone can be sent while the card is up and a PC has it.
    fn testable(&self) -> bool {
        self.running() && self.seen.connected && self.card_sink().is_some()
    }

    fn start(&mut self) {
        if self.wanted {
            return;
        }
        self.wanted = true;
        self.error.clear();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(match flipctl_app::claim("uac2") {
                Ok(held) => Msg::Held(held),
                Err(e) => Msg::Error(e),
            });
            flipctl_app::redraw();
        });
    }

    /// The sound stops before the card goes, so nothing plays into a vanishing device.
    fn stop(&mut self) {
        self.wanted = false;
        self.watch.link_streams(false);
        self.from_loop = None;
        self.to_loop = None;
        self.meters = None;
        self.tone = None;
        self.recordings.clear();
        self.recording_for = None;
        self.set = (None, None);
        self.held = None;
    }

    fn take(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                // Stop was pressed while the port was being taken: give it straight back
                Msg::Held(held) if self.wanted => self.held = Some(held),
                Msg::Held(_) => {}
                Msg::Error(e) => {
                    self.wanted = false;
                    self.error = e;
                }
            }
        }
    }

    /// Make what carries the sound match the choices and what exists: each loopback
    /// started, moved or ended, the Flipper's apps linked or not, the meters on the card.
    fn route(&mut self) {
        let running = self.running();
        let (card_in, card_out, from, to) = match self.graph() {
            Some(g) if running => {
                let name = |n: &graph::Node| n.name.clone();
                let present = |name: &str| g.node_named(name).is_some();
                (
                    g.gadget("Audio/Source").map(name),
                    g.gadget("Audio/Sink").map(name),
                    self.from_pc.clone().filter(|s| present(s)),
                    match &self.to_pc {
                        ToPc::Input(s) if present(s) => Some(s.clone()),
                        _ => None,
                    },
                )
            }
            _ => (None, None, None, None),
        };
        let (in_id, out_id) = match self.graph() {
            Some(g) if running => {
                (g.gadget("Audio/Source").map(|n| n.id), g.gadget("Audio/Sink").map(|n| n.id))
            }
            _ => (None, None),
        };
        // Again whenever the card's sides are made anew, as they are on every start
        let wanted = (in_id.map(|id| (id, self.volumes.0)), out_id.map(|id| (id, self.volumes.1)));
        if wanted.0.is_some() && wanted.0 != self.set.0 {
            self.set.0 = wanted.0.filter(|(id, v)| set_volume(*id, *v));
        }
        if wanted.1.is_some() && wanted.1 != self.set.1 {
            self.set.1 = wanted.1.filter(|(id, v)| set_volume(*id, *v));
        }
        let want = card_in.clone().zip(from);
        if self.from_loop.as_mut().and_then(Loopback::running) != want {
            self.from_loop = None;
            self.from_loop = want.and_then(|(s, k)| self.loopback("from-pc", &s, &k));
        }
        let want = to.zip(card_out.clone());
        if self.to_loop.as_mut().and_then(Loopback::running) != want {
            self.to_loop = None;
            self.to_loop = want.and_then(|(s, k)| self.loopback("to-pc", &s, &k));
        }
        self.watch.link_streams(card_out.is_some() && self.to_pc == ToPc::Flipper);
        // A new set of files whenever what is saved, or how, changes: the old set ends
        // at once, the new one starts once the choice has settled
        let want = match (&card_in, &card_out) {
            (Some(_), Some(_)) if self.record != Record::None => Some((self.record, self.format)),
            _ => None,
        };
        if want != self.recording_for {
            self.recordings.clear();
            self.recording_for = None;
        }
        if want.is_some() && self.recording_for.is_none() && self.chosen_at.elapsed() >= SETTLE {
            self.recording_for = want;
            if let (Some((record, format)), Some(i), Some(o)) = (want, &card_in, &card_out) {
                match record_to(record, format, i, o) {
                    Ok(started) => self.recordings = started,
                    Err(e) => self.error = format!("recording: {e}"),
                }
            }
        }
        match (card_in, card_out) {
            (Some(i), Some(o)) if self.meters.is_none() => {
                self.meters = Meter::start("from-pc", &i, false)
                    .and_then(|a| Meter::start("to-pc", &o, true).map(|b| (a, b)))
                    .map_err(|e| self.error = format!("pw-record: {e}"))
                    .ok();
            }
            (Some(_), Some(_)) => {}
            _ => self.meters = None,
        }
    }

    fn loopback(&mut self, name: &str, source: &str, sink: &str) -> Option<Loopback> {
        Loopback::start(name, source, sink)
            .map_err(|e| self.error = format!("pw-loopback: {e}"))
            .ok()
    }

    /// What went wrong, if anything: taking the port, or reading the sound server.
    fn problem(&self) -> Option<String> {
        match &self.seen.graph {
            Err(e) if self.wanted && !e.is_empty() => Some(e.clone()),
            _ => Some(self.error.clone()).filter(|e| !e.is_empty()),
        }
    }

    fn items(&self) -> Vec<Item> {
        let from = self.outputs();
        let to = self.sends();
        let (fa, ta) = (self.output_at(), self.send_at());
        let measured = self.meters.is_some();
        let mut rows = vec![
            chosen(&format!("From {} to", self.port), &from[fa].1, fa, from.len()),
            chosen(&format!("To {} from", self.port), &to[ta].1, ta, to.len()),
            volume(&format!("Volume from {}", self.port), self.volumes.0),
            volume(&format!("Volume to {}", self.port), self.volumes.1),
            self.speaker_row(),
            chosen("Record", &self.record_name(), at(&RECORDS, self.record), RECORDS.len()),
            chosen("Format", self.format.name(), at(&FORMATS, self.format), FORMATS.len()),
            level_row(&format!("Level from {}", self.port), measured, self.levels.0),
            level_row(&format!("Level to {}", self.port), measured, self.levels.1),
        ];
        // Last, so the rows that take keys keep their places
        rows.extend(self.problem().map(|e| item(fit(&e, MENU_WIDTH, &ROW_ACTIVE))));
        rows
    }
}

impl App for Bridge {
    fn page(&self) -> Page {
        Page::menu(self.items())
    }

    fn buttons(&self) -> [&str; 5] {
        let test = if self.testable() { "Test" } else { "" };
        ["Close", "", "", test, if self.wanted { "Stop" } else { "Start" }]
    }

    fn key(&mut self, key: Key, ui: &mut Ui) -> bool {
        match key {
            Key::Edit => {
                if let Some(sink) = self.card_sink().filter(|_| self.testable()) {
                    self.tone = None;
                    self.tone =
                        Tone::start(&sink).map_err(|e| self.error = format!("pw-play: {e}")).ok();
                }
                true
            }
            Key::Ok if ui.selected() == ROW_SPEAKER => {
                if let Some((id, (v, muted))) = self.seen.speaker {
                    self.speaker_was_muted.get_or_insert(muted);
                    if set_mute(id, !muted) {
                        self.seen.speaker = Some((id, (v, !muted)));
                    }
                }
                true
            }
            Key::Run => {
                if self.wanted {
                    self.stop();
                } else {
                    self.start();
                }
                true
            }
            Key::Left | Key::Right => {
                let step = |at: usize, count: usize| -> usize {
                    if key == Key::Right {
                        (at + 1).min(count.saturating_sub(1))
                    } else {
                        at.saturating_sub(1)
                    }
                };
                let turn = |v: i32| {
                    let by = if key == Key::Right { VOLUME_STEP } else { -VOLUME_STEP };
                    (v + by).clamp(0, 100)
                };
                match ui.selected() {
                    ROW_VOLUME_FROM => self.volumes.0 = turn(self.volumes.0),
                    ROW_VOLUME_TO => self.volumes.1 = turn(self.volumes.1),
                    ROW_RECORD => {
                        self.record = RECORDS[step(at(&RECORDS, self.record), RECORDS.len())];
                        self.chosen_at = Instant::now();
                    }
                    ROW_FORMAT => {
                        self.format = FORMATS[step(at(&FORMATS, self.format), FORMATS.len())];
                        self.chosen_at = Instant::now();
                    }
                    ROW_SPEAKER => {
                        if let Some((id, (v, muted))) = self.seen.speaker {
                            let v = turn(v);
                            if set_volume(id, v) {
                                self.seen.speaker = Some((id, (v, muted)));
                            }
                        }
                    }
                    ROW_FROM => {
                        let all = self.outputs();
                        self.from_pc = all[step(self.output_at(), all.len())].0.clone();
                    }
                    ROW_TO => {
                        let all = self.sends();
                        self.to_pc = all[step(self.send_at(), all.len())].0.clone();
                    }
                    _ => {}
                }
                self.route();
                true
            }
            _ => false,
        }
    }

    fn every(&self) -> Option<Duration> {
        Some(Duration::from_millis(50))
    }

    fn tick(&mut self, _: &mut Ui) {
        self.take();
        self.seen = self.watch.seen.lock().unwrap().clone();
        self.route();
        if let Some((from, to)) = &self.meters {
            let fall = |shown: f32, peak: u32| db(peak).max(shown - FALL_DB);
            self.levels = (fall(self.levels.0, from.take()), fall(self.levels.1, to.take()));
        } else {
            self.levels = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        }
    }

    fn close(&mut self) {
        self.stop();
        if let (Some(was), Some((id, (_, muted)))) = (self.speaker_was_muted, self.seen.speaker) {
            if was != muted {
                set_mute(id, was);
            }
        }
    }
}

flipctl_app::main!(Bridge::new());

#[cfg(test)]
mod tests {
    use super::*;

    /// The page as the panel gets it, with both level bars part full. Every pixel is
    /// a token's grey. `BRIDGE_RENDER=1` leaves the frame in target/render to look at.
    #[test]
    fn the_level_bars_draw_in_tokens() {
        struct Shown;
        impl App for Shown {
            fn buttons(&self) -> [&str; 5] {
                ["Close", "", "", "Test", "Stop"]
            }

            fn page(&self) -> Page {
                Page::menu(vec![
                    chosen("From USB-C1 to", "Speaker", 0, 3),
                    chosen("To USB-C1 from", "App sound", 1, 4),
                    level_row("Level from USB-C1", true, -12.0),
                    level_row("Level to USB-C1", true, -45.0),
                    level_row("Level of nothing", true, -90.0),
                ])
            }
        }
        let frame = flipctl_pages::shot(Shown, flipctl_app::Status::default());
        if std::env::var_os("BRIDGE_RENDER").is_some() {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/render");
            let _ = std::fs::create_dir_all(&dir);
            flipctl_app::render::save(&dir.join("levels.png"), &frame);
        }
        let theme = flipctl_app::theme::ALL_COLORS;
        let allowed: Vec<u8> = theme.iter().map(|(_, _, r, ..)| *r).collect();
        let stride = usize::from(flipctl_app::theme::PANEL_W);
        let offenders: Vec<String> = frame
            .iter()
            .enumerate()
            .filter(|(_, px)| !allowed.contains(px))
            .take(8)
            .map(|(i, px)| format!("({}, {}) = {px}", i % stride, i / stride))
            .collect();
        assert!(offenders.is_empty(), "off-palette pixels: {offenders:?}");
    }

    #[test]
    fn levels_read_in_whole_decibels() {
        assert_eq!(level(false, -3.0), "-");
        assert_eq!(level(true, db(32767)), "0 dB");
        assert_eq!(level(true, db(3277)), "-20 dB");
        assert_eq!(level(true, db(0)), "silent");
        assert_eq!(level(true, -61.0), "silent");
    }

    /// The longest rows the page makes keep their label and status apart.
    #[test]
    fn rows_fit_the_panel() {
        let row = listed("Level from USB-C1", "-");
        assert_eq!((row.label.as_str(), row.status.as_str()), ("Level from USB-C1", "-"));
        let row = chosen("To USB-C1", "App sound", 1, 3);
        assert_eq!(row.value, "App sound");
        let row = chosen("From USB-C1", "DisplayPort", 1, 3);
        assert_eq!(row.value, "DisplayPort");
        let long = chosen("From USB-C1", "A Very Long Output Name Indeed", 1, 3);
        assert!(long.value.ends_with(".."), "{}", long.value);
    }
}
