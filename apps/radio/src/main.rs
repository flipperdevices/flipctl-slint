//! Internet radio, as a flipctl app: a station picked from a list and played on
//! the panel's speaker.
//!
//! Ported from the prototype's js/apps/internet_radio.js, which asked a server to
//! run mpg123 for it and polled an API to find out what happened. There is no
//! server here and no player to install: the stream is fetched, decoded and played
//! in this binary, as the recorder plays its takes, so the app needs nothing on the
//! device but the sound server every profile has.
//!
//! The screen is four of flipctl's dropdown lines. Each one's numbers were worked
//! out by `flipctl_app::dropdown`, which is also where the strings were cut to the
//! chip, so what this file does is decide what the rows say, stack them, and turn
//! a keypress into a change.

mod http;
mod icy;
mod outputs;
mod player;
mod pulse;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use flipctl_app::theme::{metric, radius, PANEL_W};
use flipctl_app::{dropdown, font, Key, StatusSource};
use slint::{ComponentHandle, ModelRc, VecModel};

slint::include_modules!();

/// The stations, by city, and the streams behind them.
///
/// Hard-coded as the prototype has them, and for its reasons: every URL there was
/// probed for an MP3 ICY response first, and the names are ASCII because the
/// panel's fonts are. A station list that came from somewhere would be a directory
/// service, an HTTP client and a parser, and none of those are what this app is
/// for.
const CITIES: &[(&str, &[&str])] = &[
    ("London", &["Capital London", "Heart London", "Jungletrain", "Rage FM"]),
    ("New York", &["WNYC-FM", "WQXR Classical"]),
    ("Tokyo", &["J-Pop Sakura", "Japan Hits"]),
    ("Berlin", &["Berliner Rundfunk", "Spreeradio 105.5"]),
    ("Belgrade", &["Cool Radio", "Naxi Radio"]),
    ("Saratov", &["Nashe Radio"]),
];

const URLS: &[(&str, &str)] = &[
    ("Capital London", "http://media-ice.musicradio.com/CapitalMP3"),
    ("Heart London", "http://media-ice.musicradio.com/HeartLondonMP3"),
    // Drum and bass, probed on 2026-09-03 like the rest. Jungletrain is the one
    // that sends a track title; Rage FM sends none, so its row falls back to the
    // station's own name.
    ("Jungletrain", "http://stream1.jungletrain.net:8000/"),
    ("Rage FM", "http://uk2-pn.mixstream.net:8002/"),
    ("WNYC-FM", "https://fm939.wnyc.org/wnycfm"),
    ("WQXR Classical", "https://stream.wqxr.org/wqxr-web"),
    ("J-Pop Sakura", "https://quincy.torontocast.com:2070/stream.mp3"),
    ("Japan Hits", "http://quincy.torontocast.com:2020/stream.mp3"),
    ("Berliner Rundfunk", "http://stream.berliner-rundfunk.de/brf/mp3-128/internetradio/"),
    ("Spreeradio 105.5", "http://stream.spreeradio.de/spree-live/mp3-192/radio-browser.info/"),
    ("Nashe Radio", "https://nashe1.hostingradio.ru/nashe-128.mp3"),
    // Belgrade's two, found in the same directory the Spreeradio URL above came
    // from and probed on 2026-09-03: both play, and Cool Radio is the one that
    // sends a now-playing title.
    ("Cool Radio", "http://live.coolradio.rs/cool320"),
    ("Naxi Radio", "http://naxi128.streaming.rs:9150/"),
];

/// How many lines the now-playing note may take: from its first line at 97 on the
/// item_h pitch the second ends clear of the soft keys at 130, and a third ran
/// into them.
const NOTE_LINES: usize = 2;

/// `text` broken at spaces into at most `lines` lines of `room` pixels. A word too
/// long for a line is cut, and whatever does not fit on the last line is cut there
/// with "..".
fn wrap(text: &str, room: i32, lines: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut words = text.split_whitespace().peekable();
    while out.len() < lines {
        let Some(first) = words.next() else { break };
        let mut line = first.to_string();
        while let Some(word) = words.peek() {
            let longer = format!("{line} {word}");
            if font::tw(&longer) > room {
                break;
            }
            line = longer;
            words.next();
        }
        if out.len() + 1 == lines && words.peek().is_some() {
            let rest: Vec<&str> = words.by_ref().collect();
            line = format!("{line} {}", rest.join(" "));
        }
        let cut = font::fit(&line, room);
        out.push(match cut.strip_suffix("..") {
            Some(kept) if cut != line => format!("{}..", kept.trim_end()),
            _ => cut,
        });
    }
    out
}

/// The volume moves in fives, which is 20 presses end to end.
const VOLUME_STEP: i32 = 5;

/// The page, from internet_radio.js. The rows stack below the title bar, three of
/// them above a rule and the rest below it: the first three are the stream and the
/// fourth is where it comes out, which is a different question.
const BODY_TOP: i32 = metric::STATUS_BAR_H + metric::APP_BAR_H;
const RULE_DY: i32 = 49;
const RULE_AFTER: usize = 3;
const RULE_PAD_AFTER: i32 = 1;

/// What the sink flipctl pinned is called on the row, as the recorder calls it.
const SPEAKER: &str = "Speaker";

/// What the Audio device row says until the outputs are known, as the prototype's
/// own picker says it while its request is in flight. They are asked for off the
/// thread that draws, so the first frame does not wait for the sound server.
const ASKING: &str = "Updating...";

/// The rows, in the order they appear.
#[derive(Clone, Copy, PartialEq)]
enum Row {
    City,
    Station,
    Volume,
    Device,
}

const ROWS: [Row; 4] = [Row::City, Row::Station, Row::Volume, Row::Device];

/// Where a row sits. The rule takes a pixel of its own and leaves a gap after it,
/// so the rows below it are not on the pitch the rows above are.
fn row_y(index: usize) -> i32 {
    if index < RULE_AFTER {
        BODY_TOP + index as i32 * dropdown::PITCH
    } else {
        rule_y() + 1 + RULE_PAD_AFTER + (index - RULE_AFTER) as i32 * dropdown::PITCH
    }
}

fn rule_y() -> i32 {
    BODY_TOP + RULE_DY
}

fn pinned() -> String {
    pulse::pinned()
}

/// The sound server's sinks, named and ordered as the recorder has them: the
/// pinned sink first as Speaker, then HDMI, then DisplayPort, then anything else under its
/// own description. The speaker is there even if it is not listed, since that is
/// where the app plays by default.
fn outputs(devices: Vec<(String, String)>) -> Vec<(String, String)> {
    let pinned = pinned();
    let mut others: Vec<_> = devices
        .into_iter()
        .filter(|(id, _)| *id != pinned)
        .map(|(id, description)| {
            let (rank, label) = label(&id, description);
            (rank, id, label)
        })
        .collect();
    others.sort_by_key(|(rank, _, _)| *rank);
    std::iter::once((pinned, SPEAKER.to_string()))
        .chain(others.into_iter().map(|(_, id, label)| (id, label)))
        .collect()
}

/// What an output other than the speaker is called, and where it sorts.
fn label(id: &str, description: String) -> (usize, String) {
    let lower = id.to_ascii_lowercase();
    if lower.contains("hdmi") {
        (0, "HDMI".into())
    } else if lower.contains("-dp") || lower.contains("displayport") {
        (1, "DisplayPort".into())
    } else {
        (2, description)
    }
}

fn url(station: &str) -> Option<&'static str> {
    URLS.iter().find(|(name, _)| *name == station).map(|(_, url)| *url)
}

struct Radio {
    city: usize,
    /// Which station, per city: coming back to a city returns to the station it
    /// was left on rather than to the first one in its list.
    stations: Vec<usize>,
    volume: i32,
    /// Where the stream can go, as `(sink name, what to show)`. Empty
    /// until the sound server has answered, and never empty after.
    devices: Vec<(String, String)>,
    /// Each new list of them, from the thread watching the sound server.
    watching: Option<Receiver<Vec<(String, String)>>>,
    /// Which of `devices`. 0 is the pinned sink, which [`outputs`] puts first.
    device: usize,
    selected: usize,
    /// The option highlighted in the open picker, which is always the selected
    /// row's own.
    picking: Option<usize>,
    player: Option<player::Player>,
    /// The sound server, for the volume and for moving the stream. Opened again
    /// after a failure, so a server that restarted is found again.
    control: Option<pulse::Conn>,
    /// Whether to reach the sound server at all, which the tests do not.
    online: bool,
    /// The station a stream was started for, whether or not it came up.
    playing: Option<String>,
    /// The title the station is broadcasting, when it broadcasts one.
    now: String,
    /// True from the moment a stream is started until sound actually comes out of
    /// it. The player being up is not the same as the stream being up: an
    /// unreachable host leaves it waiting.
    connecting: bool,
    /// The station a stream was wanted for and never arrived from.
    failed: Option<String>,
}

impl Radio {
    fn new() -> Self {
        Self {
            city: 0,
            stations: vec![0; CITIES.len()],
            // Until the server has said: see `connect`.
            volume: 50,
            devices: Vec::new(),
            watching: None,
            device: 0,
            selected: 0,
            picking: None,
            player: None,
            control: None,
            online: false,
            playing: None,
            now: String::new(),
            connecting: false,
            failed: None,
        }
    }

    /// Reach the sound server: the volume as the machine has it, asked rather than
    /// assumed, and the outputs from now on. Kept out of `new` so the tests owe
    /// nothing to the machine they run on.
    fn connect(&mut self) {
        self.online = true;
        let sink = self.sink();
        if let Some(volume) = self.control(|c| Ok(c.sink_volume(&sink))).flatten() {
            self.volume = volume;
        }
        self.watching = Some(outputs::watch());
    }

    /// One request on the control connection, opening it first if need be. A
    /// connection that fails is dropped, and the next request opens another.
    fn control<T>(&mut self, ask: impl FnOnce(&mut pulse::Conn) -> std::io::Result<T>) -> Option<T> {
        if !self.online {
            return None;
        }
        if self.control.is_none() {
            self.control = pulse::Conn::open().ok();
        }
        let answer = ask(self.control.as_mut()?);
        if answer.is_err() {
            self.control = None;
        }
        answer.ok()
    }

    fn city_stations(&self) -> &'static [&'static str] {
        CITIES[self.city].1
    }

    fn station(&self) -> &'static str {
        let stations = self.city_stations();
        stations.get(self.stations[self.city]).copied().unwrap_or_default()
    }

    /// What a row can be set to. The volume is not one of these: it moves rather
    /// than being chosen from a list, which is what its chip being a bar says.
    fn options(&self, row: Row) -> Vec<String> {
        match row {
            Row::City => CITIES.iter().map(|(city, _)| city.to_string()).collect(),
            Row::Station => self.city_stations().iter().map(|s| s.to_string()).collect(),
            Row::Volume => Vec::new(),
            // None at all until they are known, which is what stops the chevrons
            // and Ok from offering a choice between one thing and itself.
            Row::Device if self.devices.is_empty() => Vec::new(),
            Row::Device => self.devices.iter().map(|(_, label)| label.clone()).collect(),
        }
    }

    /// Which of them is set.
    fn current(&self, row: Row) -> usize {
        match row {
            Row::City => self.city,
            Row::Station => self.stations[self.city],
            Row::Volume => 0,
            Row::Device => self.device,
        }
    }

    fn value(&self, row: Row) -> String {
        match row {
            Row::Volume => format!("{}%", self.volume),
            Row::Device if self.devices.is_empty() => ASKING.to_string(),
            row => self.options(row).get(self.current(row)).cloned().unwrap_or_default(),
        }
    }

    /// Set a row to one of its options, and do whatever that means.
    ///
    /// Both ways of changing a row come through here, the picker and the chevrons,
    /// so what a change does cannot drift between the two.
    fn choose(&mut self, row: Row, index: usize) {
        match row {
            Row::City => {
                self.city = index.min(CITIES.len() - 1);
                // The city's own station follows, and a live stream moves with it:
                // a city is a choice of what to listen to, not of what to look at.
                if self.playing.is_some() {
                    self.play();
                }
            }
            Row::Station => {
                self.stations[self.city] = index.min(self.city_stations().len() - 1);
                if self.playing.is_some() {
                    self.play();
                }
            }
            Row::Volume => {}
            Row::Device => {
                self.device = index.min(self.devices.len().saturating_sub(1));
                self.route();
            }
        }
    }

    /// Move the stream to the chosen output. The server moves a playing stream
    /// without a gap the user has to hear, and the volume row becomes that
    /// output's.
    fn route(&mut self) {
        let sink = self.sink();
        if let Some(index) = self.player.as_ref().and_then(player::Player::index) {
            self.control(|c| c.move_stream(index, &sink));
        }
        if let Some(volume) = self.control(|c| Ok(c.sink_volume(&sink))).flatten() {
            self.volume = volume;
        }
    }

    /// A new list of outputs. The chosen one stays chosen while it is there; when
    /// it goes, the stream comes back to the speaker rather than following the
    /// server's default, which is usually another display.
    fn set_outputs(&mut self, listed: Vec<(String, String)>) {
        let was = self.sink();
        self.devices = outputs(listed);
        match self.devices.iter().position(|(id, _)| *id == was) {
            Some(index) => self.device = index,
            None => {
                self.device = 0;
                self.route();
            }
        }
        // An open picker stays on a row that exists.
        if ROWS[self.selected] == Row::Device {
            let last = self.devices.len() - 1;
            self.picking = self.picking.map(|p| p.min(last));
        }
    }

    /// The sink the stream plays on: the chosen output, or the pinned sink until
    /// the outputs are known. Always named, never left to the server's default,
    /// which is often a display.
    fn sink(&self) -> String {
        self.devices.get(self.device).map_or_else(pinned, |(id, _)| id.clone())
    }

    /// The volume is the sink's own gain, not the player's, so it moves whether
    /// anything is playing or not and stays where it was left afterwards.
    fn adjust_volume(&mut self, delta: i32) {
        self.volume = (self.volume + delta).clamp(0, 100);
        // Failure is silent on purpose: the row has already moved, and a dialog in
        // front of somebody pressing a volume key would say nothing they can act on.
        let (sink, volume) = (self.sink(), self.volume);
        self.control(|c| c.set_sink_volume(&sink, volume));
    }

    /// Start the current station, replacing whatever was playing.
    fn play(&mut self) {
        let station = self.station().to_string();
        let Some(url) = url(&station) else {
            self.failed = Some(station);
            return;
        };
        // Dropped before the new one is started, so there is never a moment with
        // two players and two streams on the same speaker.
        self.player = None;
        self.now.clear();
        match player::Player::start(url, &self.sink()) {
            Ok(player) => {
                self.player = Some(player);
                self.playing = Some(station);
                self.connecting = true;
            }
            // Only a thread that could not be started ends up here, which is the
            // machine rather than the station; the dialog says what happened.
            Err(_) => {
                self.playing = None;
                self.connecting = false;
                self.failed = Some(station);
            }
        }
    }

    fn stop(&mut self) {
        self.player = None;
        self.playing = None;
        self.connecting = false;
        self.now.clear();
    }

    fn toggle(&mut self) {
        if self.playing.is_some() {
            self.stop();
        } else {
            self.play();
        }
    }

    /// Once a second: is the stream up, is it still up, and what is it playing.
    ///
    /// Nothing here asks anything of a stopped app: the player is the only thing
    /// polled, and when there is no player there is nothing to poll.
    ///
    /// The prototype could only ask whether mpg123 was still running, so it gave a
    /// stream 3.5 seconds to prove itself and called anything else a failure.
    /// Sound reaching the server is a better question than a timer. Which of the
    /// two things that can go wrong has gone wrong is then plain, and neither is a
    /// guess:
    ///
    ///   * a URL that answers with an error or with something that is not MP3,
    ///     or a stream that stops, ends the player.
    ///   * a host that is slow to answer leaves it waiting, for as long as the
    ///     connect and read timeouts in http.rs allow. Nothing has failed yet,
    ///     and saying so would be a lie; the bar says it is connecting instead.
    fn tick(&mut self) {
        // The outputs, if they have changed. Only the newest list matters.
        if let Some(listed) = self.watching.as_ref().and_then(|rx| rx.try_iter().last()) {
            self.set_outputs(listed);
        }
        let Some(player) = self.player.as_mut() else {
            return;
        };
        if !player.alive() {
            // A player that dies before its first sound never started, which is
            // worth a dialog. One that dies after playing has stopped, and the
            // button going back to Play says everything there is to say.
            let station = self.playing.clone().unwrap_or_default();
            let never_started = self.connecting;
            self.stop();
            if never_started {
                self.failed = Some(station);
            }
            return;
        }
        self.connecting = !player.started();
        // The station's own title, which most of them broadcast and some do not.
        // Not read while connecting: a title out of a player that has not made a
        // sound yet would be a claim that it is playing.
        self.now = if self.connecting { String::new() } else { player.title() };
    }

    /// The rows as the component draws them.
    fn rows(&self) -> Vec<DropRow> {
        ROWS.iter()
            .enumerate()
            .map(|(i, row)| {
                let slider = (*row == Row::Volume).then(|| self.volume as f32 / 100.0);
                let line = dropdown::line(
                    row_y(i),
                    self.title(*row),
                    &self.value(*row),
                    slider,
                    i == self.selected && self.picking.is_none(),
                );
                DropRow {
                    y: line.y as f32,
                    title: line.title.into(),
                    value: line.value.into(),
                    value_x: line.value_x as f32,
                    kind: line.kind,
                    fill_w: line.fill_w as f32,
                    selected: line.selected,
                }
            })
            .collect()
    }

    fn title(&self, row: Row) -> &'static str {
        match row {
            Row::City => "City",
            Row::Station => "Station",
            Row::Volume => "Volume",
            Row::Device => "Audio device",
        }
    }

    /// What is coming out of the speaker: the station's own now-playing title when
    /// it sends one, and its name when it does not.
    ///
    /// Under the last row rather than in the title bar, so the line has the page's
    /// full width. A station's title is the longest string on this screen and the
    /// bar could only spare what the app's name left of it, which cut most of them.
    ///
    /// Wrapped at spaces onto as many lines as clear the soft keys, and cut with
    /// ".." only on the last of them: a title is often an artist, a song and a
    /// programme, and one line kept only the first of the three.
    fn note(&self) -> Vec<String> {
        let Some(station) = self.playing.as_ref() else {
            return Vec::new();
        };
        if self.connecting {
            return vec!["Connecting...".to_string()];
        }
        let what = if self.now.is_empty() { station.as_str() } else { self.now.as_str() };
        // Transliterated, because a station's title is written in the alphabet of
        // wherever it is broadcasting from and the panel's fonts are ASCII: Nashe
        // Radio's titles arrive in Cyrillic and would otherwise be a row of
        // question marks. Done before the measurement, so what is measured is what
        // is drawn.
        // The page's own margins: the rows' title inset on the left and the rule's
        // inset on the right.
        let room = PANEL_W as i32 - metric::DROP_TITLE_X - metric::DROP_RULE_PAD_X;
        wrap(&font::ascii(&format!("Playing: {what}")), room, NOTE_LINES)
    }

    /// Where its first line sits: the line a fifth row would have started on. The
    /// rest follow on the item_h pitch.
    fn note_y() -> i32 {
        row_y(ROWS.len()) + metric::DROP_TOP_PAD
    }

    /// The dialog's lines, empty when there is no dialog.
    fn dialog(&self) -> Vec<slint::SharedString> {
        let Some(station) = self.failed.as_ref() else {
            return Vec::new();
        };
        let body = if station.is_empty() {
            "Could not start stream".to_string()
        } else {
            // Inside the frame's own corners: a line that reached into the chamfer
            // would be a letter with a diagonal cut through it.
            font::fit(
                &format!("Could not start {station}"),
                metric::MODAL_W - radius::BOX * 2,
            )
        };
        vec!["Stream error".into(), body.into()]
    }
}

/// Put the state on screen. Every frame is built from the state and nothing is
/// updated in place, so what is drawn cannot disagree with what is true.
fn apply(ui: &AppWindow, state: &Radio) {
    ui.set_rows(ModelRc::new(VecModel::from(state.rows())));
    ui.set_rule_y(rule_y() as f32);
    ui.set_sel_y(row_y(state.selected) as f32);
    ui.set_selector(state.picking.is_none() && state.failed.is_none());
    let note: Vec<slint::SharedString> = state.note().into_iter().map(Into::into).collect();
    ui.set_note(ModelRc::new(VecModel::from(note)));
    ui.set_note_y(Radio::note_y() as f32);
    ui.set_play_label(if state.playing.is_some() { "Stop".into() } else { "Play".into() });
    ui.set_dialog(ModelRc::new(VecModel::from(state.dialog())));
    ui.set_dialog_right(if state.failed.is_some() { "Ok".into() } else { "".into() });

    let row = ROWS[state.selected];
    let picking = state.picking.filter(|_| !state.options(row).is_empty());
    ui.set_picking(picking.is_some());
    if let Some(highlighted) = picking {
        let options = state.options(row);
        // The picker opens over the chip of the row it belongs to, so its own top
        // is that row's chip.
        let chip_y = row_y(state.selected) + metric::DROP_TOP_PAD;
        let view = dropdown::picker(chip_y, state.title(row), &options, highlighted);
        ui.set_view(DropView {
            x: view.x as f32,
            y: view.y as f32,
            h: view.h as f32,
            title: view.title.into(),
            sel_y: view.sel_y as f32,
            sel_w: view.sel_w as f32,
            sel_h: view.sel_h as f32,
        });
        ui.set_items(ModelRc::new(VecModel::from(
            view.items
                .into_iter()
                .map(|item| DropItem {
                    y: item.y as f32,
                    text: item.text.into(),
                    text_x: item.text_x as f32,
                    rule: item.rule,
                })
                .collect::<Vec<_>>(),
        )));
    }
}

/// One key, on the way down. Returns false when the app should close.
fn key(state: &mut Radio, key: Key) -> bool {
    // The dialog owns the keys while it is up, and any of them dismisses it: there
    // is one thing to say about a stream that would not start.
    if state.failed.is_some() {
        if matches!(key, Key::Ok | Key::Run | Key::Back | Key::Escape) {
            state.failed = None;
        }
        return true;
    }

    let row = ROWS[state.selected];

    // The picker owns them next, and moves within its own list rather than the
    // page's: two selectors on screen would be two things being chosen.
    if let Some(highlighted) = state.picking {
        let options = state.options(row);
        let total = options.len();
        match key {
            Key::Back | Key::Escape => state.picking = None,
            Key::Down if total > 0 => state.picking = Some((highlighted + 1) % total),
            Key::Up if total > 0 => state.picking = Some((highlighted + total - 1) % total),
            Key::Ok | Key::Run => {
                state.picking = None;
                state.choose(row, highlighted);
            }
            _ => {}
        }
        return true;
    }

    match key {
        Key::Back | Key::Escape => return false,
        // The right-hand soft key, which is Play or Stop depending on what is
        // happening. Taken before anything else a key could mean on a row.
        Key::Run => state.toggle(),
        Key::Down => state.selected = (state.selected + 1) % ROWS.len(),
        Key::Up => state.selected = (state.selected + ROWS.len() - 1) % ROWS.len(),
        // Left and right move the volume and cycle every other row, so a value can
        // be changed without opening anything.
        Key::Left | Key::Right => {
            let forward = matches!(key, Key::Right);
            if row == Row::Volume {
                state.adjust_volume(if forward { VOLUME_STEP } else { -VOLUME_STEP });
            } else {
                let total = state.options(row).len();
                // One option is not a choice, and cycling it would restart a
                // stream to arrive back where it was.
                if total > 1 {
                    let at = state.current(row);
                    let next =
                        if forward { (at + 1) % total } else { (at + total - 1) % total };
                    state.choose(row, next);
                }
            }
        }
        // Ok opens the row's own picker, on whatever it is currently set to.
        Key::Ok if !state.options(row).is_empty() => state.picking = Some(state.current(row)),
        _ => {}
    }
    true
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let mut radio = Radio::new();
    radio.connect();
    let state = Rc::new(RefCell::new(radio));
    apply(&ui, &state.borrow());

    // The panel's own bar, read here rather than handed over: an app has the same
    // access to sysfs that flipctl does.
    let mut status = StatusSource::new(Duration::from_secs(2));
    flipctl_app::apply_status!(&ui, PanelStatus, status.current());

    let tick = slint::Timer::default();
    let ticking = ui.as_weak();
    let ticked = Rc::clone(&state);
    tick.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        let Some(ui) = ticking.upgrade() else {
            return;
        };
        let mut state = ticked.borrow_mut();
        state.tick();
        apply(&ui, &state);
        if let Some(now) = status.poll() {
            flipctl_app::apply_status!(&ui, PanelStatus, now);
        }
    });

    let keys = ui.as_weak();
    let keyed = Rc::clone(&state);
    ui.on_keyed(move |text, down| {
        let Some(ui) = keys.upgrade() else {
            return;
        };
        let Some(pressed) = Key::from_slint(text.as_str()) else {
            return;
        };
        // The soft bar shows which button is held, as it does everywhere else.
        ui.set_pressed_slot(match (down, pressed.soft_slot()) {
            (true, Some(slot)) => slot as i32,
            _ => -1,
        });
        if !down {
            return;
        }
        let mut state = keyed.borrow_mut();
        if !key(&mut state, pressed) {
            let _ = slint::quit_event_loop();
            return;
        }
        apply(&ui, &state);
    });

    ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flipctl_app::theme;
    use flipper_ui::slint_render::{render_frame, FlipperSlintPlatform};
    use slint::platform::software_renderer::MinimalSoftwareWindow;

    /// The panel, once per thread.
    ///
    /// Installing the platform twice is a panic and the tests run in a thread
    /// each, so the window is a thread-local: one test that draws two states is
    /// then the same as two tests that draw one.
    fn panel() -> Rc<MinimalSoftwareWindow> {
        thread_local! {
            static WINDOW: Rc<MinimalSoftwareWindow> = FlipperSlintPlatform::install();
        }
        WINDOW.with(Rc::clone)
    }

    /// Draw the page as the panel would receive it: 256x144 bytes of grey.
    fn shot(name: &str, state: &Radio) -> Vec<u8> {
        let window = panel();
        let ui = AppWindow::new().expect("create AppWindow");
        apply(&ui, state);
        ui.show().expect("show");
        slint::platform::update_timers_and_animations();
        let frame: Vec<u8> =
            render_frame(&window).expect("frame").iter().map(|px| px.0).collect();
        drop(ui);
        // Written only when asked for, because a test that writes files on every
        // run is a test nobody runs. `RADIO_RENDER=1 cargo test` then leaves the
        // frames in target/render for somebody to look at, which is the only
        // thing that settles a question about pixels.
        if std::env::var_os("RADIO_RENDER").is_some() {
            save(name, &frame);
        }
        frame
    }

    fn save(name: &str, frame: &[u8]) {
        let dir = std::path::Path::new("target/render");
        std::fs::create_dir_all(dir).expect("create target/render");
        let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("create png");
        let mut encoder = png::Encoder::new(
            std::io::BufWriter::new(file),
            u32::from(theme::PANEL_W),
            u32::from(theme::PANEL_H),
        );
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("png header")
            .write_image_data(frame)
            .expect("png data");
    }

    fn at(frame: &[u8], x: i32, y: i32) -> u8 {
        frame[(y * i32::from(theme::PANEL_W) + x) as usize]
    }

    /// A state that owes nothing to the machine the test runs on: the device list
    /// comes from the sound server, which is not here.
    fn radio() -> Radio {
        Radio {
            devices: outputs(vec![
                ("alsa_output.platform-sound.stereo-fallback".into(), "Panel Speaker".into()),
                ("alsa_output.hdmi-stereo".into(), "Digital Stereo (HDMI)".into()),
            ]),
            // Not watching, so the tests are not racing the machine's sound server.
            watching: None,
            ..Radio::new()
        }
    }

    /// Invariant 1: every pixel holds a design-token value. A fractional text
    /// offset, a fallback font or a `border-radius` corner all show up here as a
    /// grey that is not in tokens.toml.
    #[test]
    fn the_page_is_drawn_only_in_tokens() {
        let mut state = radio();
        state.selected = 2;
        let frame = shot("page", &state);
        let allowed: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
        let stride = i32::from(theme::PANEL_W);
        let offenders: Vec<String> = frame
            .iter()
            .enumerate()
            .filter(|(_, px)| !allowed.contains(px))
            .take(8)
            .map(|(i, px)| {
                format!("({}, {}) = {px}", i as i32 % stride, i as i32 / stride)
            })
            .collect();
        assert!(offenders.is_empty(), "off-palette pixels: {offenders:?}");
    }

    /// The rows sit where the prototype puts them, read back from the pixels
    /// rather than from the numbers that placed them: the chip is the only wide
    /// run of its own grey on the row, so finding it proves the row is there.
    #[test]
    fn the_rows_and_the_rule_land_where_they_belong() {
        let frame = shot("rows", &radio());
        let chip = theme::color::CHIP_FILL.0;
        for i in 0..ROWS.len() {
            // The chip's right end, which is its own grey on every row: the left
            // end of the volume row is under the slider's fill.
            let y = row_y(i) + metric::DROP_TOP_PAD + metric::DROP_CHIP_H / 2;
            let right = dropdown::CHIP_X + metric::DROP_CHIP_W - 2;
            assert_eq!(at(&frame, right, y), chip, "row {i} chip at y {y}");
            // A pixel clear of its left edge is the page, not the chip.
            assert_ne!(at(&frame, dropdown::CHIP_X - 2, y), chip, "row {i} chip is too wide");
        }
        // The rule, inset from both edges.
        let rule = theme::color::DIVIDER.0;
        assert_eq!(at(&frame, 128, rule_y()), rule, "the rule");
        assert_ne!(at(&frame, 2, rule_y()), rule, "the rule reaches the edge");
        // Three rows above it and one below, which is what the divider is for.
        assert!(row_y(2) + metric::DROP_ROW_H <= rule_y());
        assert!(row_y(3) > rule_y());
    }

    /// The volume chip is a bar: black where the fill has reached and the chip's
    /// own grey after it, with the value legible across the boundary.
    #[test]
    fn the_volume_chip_fills_to_its_value() {
        let mut state = radio();
        state.selected = 2;
        state.volume = 60;
        let frame = shot("volume", &state);
        let y = row_y(2) + metric::DROP_TOP_PAD + metric::DROP_CHIP_H / 2;
        let filled = dropdown::fill_w(0.6);
        assert_eq!(at(&frame, dropdown::CHIP_X + filled - 2, y), theme::color::BLACK.0);
        assert_eq!(at(&frame, dropdown::CHIP_X + filled + 2, y), theme::color::CHIP_FILL.0);
    }

    /// An open picker is the chip grown downwards: same left edge, same width, and
    /// as many options as the row has.
    #[test]
    fn a_picker_opens_over_its_own_chip() {
        let mut state = radio();
        // London's four, under the row they belong to.
        state.selected = 1;
        state.picking = Some(2);
        shot("stations", &state);

        state.selected = 0;
        state.picking = Some(2);
        let frame = shot("picker", &state);
        let view = dropdown::picker(
            row_y(0) + metric::DROP_TOP_PAD,
            "City",
            &state.options(Row::City),
            2,
        );
        assert_eq!(view.x, dropdown::CHIP_X);
        assert_eq!(view.h, dropdown::picker_h(CITIES.len()));
        // The frame reaches the last option, over ground that is page below it.
        let inside = view.y + view.h - metric::DROP_PICK_PAD - 1;
        assert_eq!(at(&frame, view.x + 1, inside), theme::color::CHIP_FILL.0);
        // A rule between two options, in its own grey.
        let between = view.items[0].y + metric::DROP_PICK_ITEM_H;
        assert_eq!(at(&frame, view.x + 90, between), theme::color::PICK_RULE.0);
        // It grows downwards from a chip that does not move, so the list cannot
        // grow for ever: the soft bar is where it has to stop. Six cities leave
        // room for one more.
        assert!(
            view.y + view.h < theme::PANEL_H as i32 - metric::BUTTON_H,
            "a picker of {} runs into the soft bar",
            CITIES.len()
        );
    }

    /// The line under the rows is what is coming out of the speaker: the station's
    /// own now-playing title when it sends one, and its name when it does not.
    #[test]
    fn the_page_says_what_is_playing() {
        let mut state = radio();
        assert!(state.note().is_empty(), "nothing is playing");

        state.playing = Some("Heart London".into());
        state.connecting = true;
        assert_eq!(state.note(), ["Connecting..."], "no sound yet, so no claim of one");

        state.connecting = false;
        assert_eq!(state.note(), ["Playing: Heart London"]);

        state.now = "Jamiroquai - Cosmic Girl".into();
        assert_eq!(state.note(), ["Playing: Jamiroquai - Cosmic Girl"]);
        shot("playing", &state);

        // A Cyrillic title, which is what Saratov and Belgrade send: drawable
        // letters rather than the row of question marks the fonts would give.
        state.now = "ДДТ - Что такое осень".into();
        assert_eq!(state.note(), ["Playing: DDT - Chto takoe osen"]);
        shot("cyrillic", &state);

        // Cut rather than run off the edge of the page, but with the whole width
        // to use: a title the title bar would have cut in half now fits.
        state.now = "Jamiroquai - Virtual Insanity".into();
        assert_eq!(state.note(), ["Playing: Jamiroquai - Virtual Insanity"]);
        let room = PANEL_W as i32 - metric::DROP_TITLE_X - metric::DROP_RULE_PAD_X;
        // A long title wraps at spaces, every line inside the page.
        state.now = "Dado Polumenta - Ljubavi moja (ft Elitni odredi) [Live at the Sava Centar]".into();
        let lines = state.note();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines.join(" "), format!("Playing: {}", state.now));
        assert!(lines.iter().all(|l| font::tw(l) <= room), "{lines:?}");
        shot("playing-long", &state);

        // Longer than every line there is: cut on the last of them.
        state.now = "A title of the kind a station sends when it is being thorough. ".repeat(4);
        let lines = state.note();
        assert_eq!(lines.len(), NOTE_LINES, "{lines:?}");
        assert!(lines[NOTE_LINES - 1].ends_with(".."), "{lines:?}");
        assert!(!lines[NOTE_LINES - 1].ends_with(" .."), "{lines:?}");
        assert!(lines.iter().all(|l| font::tw(l) <= room), "{lines:?}");
        shot("playing-longest", &state);

        // Clear of the last row's chip, and the last line clear of the soft bar.
        assert!(Radio::note_y() > row_y(ROWS.len() - 1) + metric::DROP_CHIP_H);
        let last = Radio::note_y() + (NOTE_LINES as i32 - 1) * metric::ITEM_H;
        assert!(last + metric::DROP_ROW_H <= theme::PANEL_H as i32 - metric::BUTTON_H, "{last}");
    }

    /// Stopping ends the player, and with it every claim on screen about what is
    /// playing.
    #[test]
    fn stopping_clears_the_bar_and_the_button() {
        let mut state = radio();
        state.playing = Some("Heart London".into());
        state.now = "something".into();
        state.stop();
        assert_eq!(state.playing, None);
        assert_eq!(state.now, "");
    }

    /// A picker grows downwards from the chip of the row it belongs to, so the
    /// lower the row the shorter its list may be. The station row is the lowest
    /// one with a list and the longest list is a city's stations, which is the
    /// pair that runs out of room first.
    #[test]
    fn the_longest_list_still_fits_under_its_own_row() {
        let floor = theme::PANEL_H as i32 - metric::BUTTON_H;
        for (city, stations) in CITIES {
            let bottom = row_y(1) + metric::DROP_TOP_PAD + dropdown::picker_h(stations.len());
            assert!(
                bottom < floor,
                "{city} has {} stations, whose picker reaches {bottom} and the soft \
                 bar starts at {floor}",
                stations.len()
            );
        }
    }

    /// Every station a city offers has a stream: a name in one list and not the
    /// other is a row that says Play and then says Stream error.
    #[test]
    fn every_station_has_a_url() {
        for (city, stations) in CITIES {
            for station in *stations {
                assert!(url(station).is_some(), "{city}'s {station} has no URL");
                assert!(
                    station.is_ascii(),
                    "{station} cannot be drawn: the fonts are ASCII"
                );
            }
        }
        // And nothing is in the URL list that no city offers.
        for (station, _) in URLS {
            assert!(
                CITIES.iter().any(|(_, list)| list.contains(station)),
                "{station} has a URL but no city"
            );
        }
    }

    /// A city carries its own last station, so coming back to it is where the user
    /// left it rather than the top of its list.
    #[test]
    fn each_city_remembers_its_station() {
        let mut state = radio();
        state.choose(Row::City, 3);
        state.choose(Row::Station, 1);
        assert_eq!(state.station(), "Spreeradio 105.5");
        state.choose(Row::City, 0);
        assert_eq!(state.station(), "Capital London");
        state.choose(Row::City, 3);
        assert_eq!(state.station(), "Spreeradio 105.5");
        state.choose(Row::City, 4);
        assert_eq!(state.station(), "Cool Radio");
    }

    /// The chevrons cycle a row without opening anything, and wrap.
    #[test]
    fn left_and_right_move_a_row_and_wrap() {
        let mut state = radio();
        key(&mut state, Key::Right);
        assert_eq!(state.value(Row::City), "New York");
        key(&mut state, Key::Left);
        assert_eq!(state.value(Row::City), "London");
        key(&mut state, Key::Left);
        assert_eq!(state.value(Row::City), "Saratov");

        // The volume row moves instead, in fives, and stops at the ends.
        state.selected = 2;
        key(&mut state, Key::Right);
        assert_eq!(state.volume, 55);
        state.volume = 100;
        key(&mut state, Key::Right);
        assert_eq!(state.volume, 100);
    }

    /// Ok opens the row's picker on what it is set to, and Ok in the picker takes
    /// the highlighted option. Back leaves it as it was.
    #[test]
    fn a_picker_commits_on_ok_and_not_on_back() {
        let mut state = radio();
        state.selected = 3;
        key(&mut state, Key::Ok);
        assert_eq!(state.picking, Some(0));
        key(&mut state, Key::Down);
        assert_eq!(state.picking, Some(1));
        key(&mut state, Key::Ok);
        assert_eq!(state.picking, None);
        assert_eq!(state.value(Row::Device), "HDMI");
        assert_eq!(state.sink(), "alsa_output.hdmi-stereo");

        key(&mut state, Key::Ok);
        key(&mut state, Key::Up);
        key(&mut state, Key::Back);
        assert_eq!(state.picking, None);
        assert_eq!(state.value(Row::Device), "HDMI", "back changed it");
    }

    /// The first option is the sink flipctl pinned, once and called Speaker, and
    /// it is named to the server rather than left to the server's default.
    #[test]
    fn the_first_output_is_the_pinned_speaker() {
        let mut state = radio();
        assert_eq!(state.options(Row::Device), [SPEAKER, "HDMI"]);
        assert_eq!(state.value(Row::Device), SPEAKER);
        assert_eq!(state.sink(), pinned());
        state.choose(Row::Device, 1);
        assert_eq!(state.sink(), "alsa_output.hdmi-stereo");
    }

    /// Speaker, HDMI, DisplayPort and then the rest, whatever order the server lists them
    /// in, under the recorder's names.
    #[test]
    fn outputs_are_named_and_ordered_as_the_recorder_has_them() {
        let listed = vec![
            ("bluez_output.AA_BB.1".into(), "Headphones".into()),
            ("alsa_output.platform-dp0-sound.stereo-fallback".into(), "DisplayPort Digital Output".into()),
            ("alsa_output.platform-sound.stereo-fallback".into(), "On-board NAU8822 Analog Output".into()),
            ("alsa_output.platform-hdmi-sound.pro-output-0".into(), "HDMI Digital Output".into()),
        ];
        let labels: Vec<String> = outputs(listed.clone()).into_iter().map(|(_, label)| label).collect();
        assert_eq!(labels, [SPEAKER, "HDMI", "DisplayPort", "Headphones"]);

        // The longest of the names, on its chip and in the picker.
        let mut state = radio();
        state.set_outputs(listed);
        state.selected = 3;
        state.choose(Row::Device, 2);
        assert_eq!(state.value(Row::Device), "DisplayPort");
        shot("output-dp", &state);
        state.picking = Some(2);
        shot("output-dp-picker", &state);
    }

    /// The chosen output stays chosen when the list changes around it, and one
    /// that goes sends the stream back to the speaker.
    #[test]
    fn a_new_list_keeps_the_choice_while_it_is_there() {
        let mut state = radio();
        state.choose(Row::Device, 1);
        let hdmi = ("alsa_output.hdmi-stereo".to_string(), "Digital Stereo (HDMI)".to_string());
        let dp = ("alsa_output.platform-dp0-sound.stereo-fallback".to_string(), "DisplayPort".to_string());
        state.set_outputs(vec![dp.clone(), hdmi]);
        assert_eq!(state.sink(), "alsa_output.hdmi-stereo");
        assert_eq!(state.device, 1);
        state.set_outputs(vec![dp]);
        assert_eq!(state.sink(), pinned());
        assert_eq!(state.value(Row::Device), SPEAKER);
    }

    /// Until the outputs are known the row says so and offers nothing, so neither
    /// the chevrons nor Ok can choose between one thing and itself.
    #[test]
    fn the_output_row_says_it_is_still_asking() {
        let mut state = radio();
        state.devices.clear();
        assert_eq!(state.value(Row::Device), ASKING);
        assert!(state.options(Row::Device).is_empty());
        state.selected = 3;
        key(&mut state, Key::Ok);
        assert_eq!(state.picking, None, "a picker opened with nothing in it");
        key(&mut state, Key::Right);
        assert_eq!(state.value(Row::Device), ASKING);
    }

    /// The dialog owns the keys while it is up, and any of them dismisses it.
    #[test]
    fn a_dialog_swallows_the_page_until_it_is_dismissed() {
        let mut state = radio();
        state.failed = Some("Heart London".into());
        let lines = state.dialog();
        assert_eq!(lines[0], "Stream error");
        assert_eq!(lines[1], "Could not start Heart London");
        shot("dialog", &state);
        key(&mut state, Key::Down);
        assert_eq!(state.selected, 0, "the page moved under the dialog");
        key(&mut state, Key::Ok);
        assert_eq!(state.failed, None);
        assert!(state.dialog().is_empty());
    }

    /// Back closes the app, and only from the page: it dismisses a dialog or a
    /// picker first, so one press never both closes a picker and leaves.
    #[test]
    fn back_leaves_only_from_the_page() {
        let mut state = radio();
        state.picking = Some(0);
        assert!(key(&mut state, Key::Back), "the picker was closed, not the app");
        assert!(!key(&mut state, Key::Back), "the app closes");
    }
}
