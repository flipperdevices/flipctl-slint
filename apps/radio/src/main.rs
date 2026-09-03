//! Internet radio, as a flipctl app: a station picked from a list and played on
//! the panel's speaker.
//!
//! Ported from the prototype's js/apps/internet_radio.js, which asked a server to
//! run mpg123 for it and polled an API to find out what happened. There is no
//! server here: the app starts the player itself, and everything the API answered
//! comes either from the child process or from the socket it listens on.
//!
//! The screen is four of flipctl's dropdown lines. Each one's numbers were worked
//! out by `flipctl_app::dropdown`, which is also where the strings were cut to the
//! chip, so what this file does is decide what the rows say, stack them, and turn
//! a keypress into a change.

mod mpv;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
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

/// The volume moves in fives, which is 20 presses end to end.
const VOLUME_STEP: i32 = 5;

/// The page, from internet_radio.js. The rows stack below the title bar, three of
/// them above a rule and the rest below it: the first three are the stream and the
/// fourth is where it comes out, which is a different question.
const BODY_TOP: i32 = metric::STATUS_BAR_H + metric::APP_BAR_H;
const RULE_DY: i32 = 49;
const RULE_AFTER: usize = 3;
const RULE_PAD_AFTER: i32 = 1;

/// The output the app plays to when it asks for none: the sink flipctl pinned for
/// it, which is the panel's own speaker.
const DEFAULT_DEVICE: &str = "Default";

/// What the Audio device row says until the outputs are known, as the prototype's
/// own picker says it while its request is in flight. Asking mpv what it can play
/// to means starting mpv, which takes 2.3 seconds on the device: long enough that
/// doing it before the first frame would show the user a blank panel.
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

fn url(station: &str) -> Option<&'static str> {
    URLS.iter().find(|(name, _)| *name == station).map(|(_, url)| *url)
}

struct Radio {
    city: usize,
    /// Which station, per city: coming back to a city returns to the station it
    /// was left on rather than to the first one in its list.
    stations: Vec<usize>,
    volume: i32,
    /// What mpv says it can play to, as `(what to pass it, what to show)`.
    devices: Vec<(String, String)>,
    /// The thread asking it, while it is still asking.
    asking: Option<Receiver<Vec<(String, String)>>>,
    /// 0 is the pinned sink, and the rest index `devices`.
    device: usize,
    selected: usize,
    /// The option highlighted in the open picker, which is always the selected
    /// row's own.
    picking: Option<usize>,
    player: Option<mpv::Player>,
    /// The station a stream was started for, whether or not it came up.
    playing: Option<String>,
    /// The title the station is broadcasting, when it broadcasts one.
    now: String,
    /// True from the moment a stream is started until sound actually comes out of
    /// it. The player being up is not the same as the stream being up: an
    /// unreachable host leaves mpv running and waiting.
    connecting: bool,
    /// The station a stream was wanted for and never arrived from.
    failed: Option<String>,
}

impl Radio {
    fn new() -> Self {
        // Off the thread that draws, so the first frame does not wait for mpv to
        // start, enumerate and exit. The answer is picked up by the next tick.
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("outputs".into())
            .spawn(move || {
                let _ = tx.send(mpv::devices());
            })
            .expect("spawn outputs");
        Self {
            city: 0,
            stations: vec![0; CITIES.len()],
            volume: 50,
            devices: Vec::new(),
            asking: Some(rx),
            device: 0,
            selected: 0,
            picking: None,
            player: None,
            playing: None,
            now: String::new(),
            connecting: false,
            failed: None,
        }
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
            Row::Device if self.asking.is_some() => Vec::new(),
            Row::Device => std::iter::once(DEFAULT_DEVICE.to_string())
                .chain(self.devices.iter().map(|(_, label)| label.clone()))
                .collect(),
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
            Row::Device if self.asking.is_some() => ASKING.to_string(),
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
                self.device = index.min(self.devices.len());
                // mpv reopens its output when told, so the stream moves without a
                // gap the user has to hear.
                let device = self.device_id().unwrap_or("auto").to_string();
                if let Some(player) = self.player.as_ref() {
                    player.set("audio-device", &format!("\"{device}\""));
                }
            }
        }
    }

    /// What to pass mpv for the chosen output, or nothing for the pinned sink.
    fn device_id(&self) -> Option<&str> {
        let index = self.device.checked_sub(1)?;
        self.devices.get(index).map(|(id, _)| id.as_str())
    }

    fn adjust_volume(&mut self, delta: i32) {
        self.volume = (self.volume + delta).clamp(0, 100);
        if let Some(player) = self.player.as_ref() {
            player.set("volume", &self.volume.to_string());
        }
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
        match mpv::Player::start(url, self.volume, self.device_id()) {
            Ok(player) => {
                self.player = Some(player);
                self.playing = Some(station);
                self.connecting = true;
            }
            // mpv is declared in the manifest, so flipctl installed it before the
            // app was launched. Missing anyway means something is wrong with the
            // machine rather than with the station, and the dialog says what
            // happened either way.
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
    /// `time-pos` is a better question than a timer: it is the position of the
    /// audio being played, so it appears when sound does. Which of the two things
    /// that can go wrong has gone wrong is then plain, and neither is a guess:
    ///
    ///   * a URL that answers with something unplayable, or does not answer at
    ///     all, ends mpv. Measured: a 404 exits inside five seconds.
    ///   * a host that cannot be reached leaves mpv up and waiting, with no
    ///     position, for as long as its own timeouts take. Nothing has failed yet,
    ///     and saying so would be a lie; the bar says it is connecting instead.
    fn tick(&mut self) {
        // The outputs, if the thread that went to ask has come back.
        if let Some(rx) = self.asking.as_ref() {
            if let Ok(devices) = rx.try_recv() {
                self.devices = devices;
                self.asking = None;
            }
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
        self.connecting = player.get("time-pos").is_none();
        // The station's own title, which most of them broadcast and some do not.
        // Not read while connecting: a title out of a player that has not made a
        // sound yet would be a claim that it is playing.
        self.now = if self.connecting {
            String::new()
        } else {
            player.get("metadata/by-key/icy-title").unwrap_or_default()
        };
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
    fn note(&self) -> String {
        let Some(station) = self.playing.as_ref() else {
            return String::new();
        };
        if self.connecting {
            return "Connecting...".to_string();
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
        font::fit(&font::ascii(&format!("Playing: {what}")), room)
    }

    /// Where that line sits: the line a fifth row would have started on.
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
    ui.set_note(state.note().into());
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
    let state = Rc::new(RefCell::new(Radio::new()));
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
    /// comes from mpv, which is not here.
    fn radio() -> Radio {
        Radio {
            devices: vec![
                ("pipewire/alsa_output.platform-sound.stereo-fallback".into(), "Panel Speaker".into()),
                ("pipewire/alsa_output.hdmi-stereo".into(), "Digital Stereo (HDMI)".into()),
            ],
            // Answered, so the tests are not racing a thread that starts mpv.
            asking: None,
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
        assert_eq!(state.note(), "", "nothing is playing");

        state.playing = Some("Heart London".into());
        state.connecting = true;
        assert_eq!(state.note(), "Connecting...", "no sound yet, so no claim of one");

        state.connecting = false;
        assert_eq!(state.note(), "Playing: Heart London");

        state.now = "Jamiroquai - Cosmic Girl".into();
        assert_eq!(state.note(), "Playing: Jamiroquai - Cosmic Girl");
        shot("playing", &state);

        // A Cyrillic title, which is what Saratov and Belgrade send: drawable
        // letters rather than the row of question marks the fonts would give.
        state.now = "ДДТ - Что такое осень".into();
        assert_eq!(state.note(), "Playing: DDT - Chto takoe osen");
        shot("cyrillic", &state);

        // Cut rather than run off the edge of the page, but with the whole width
        // to use: a title the title bar would have cut in half now fits.
        state.now = "Jamiroquai - Virtual Insanity".into();
        assert_eq!(state.note(), "Playing: Jamiroquai - Virtual Insanity");
        let room = PANEL_W as i32 - metric::DROP_TITLE_X - metric::DROP_RULE_PAD_X;
        state.now = "A very long title of the kind a station sends when it is being thorough".into();
        assert!(state.note().ends_with(".."), "{}", state.note());
        assert!(font::tw(&state.note()) <= room, "{}", font::tw(&state.note()));

        // Clear of the last row's chip, and clear of the soft bar under it.
        assert!(Radio::note_y() > row_y(ROWS.len() - 1) + metric::DROP_CHIP_H);
        assert!(Radio::note_y() + metric::DROP_ROW_H < theme::PANEL_H as i32 - metric::BUTTON_H);
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
        key(&mut state, Key::Down);
        assert_eq!(state.picking, Some(2));
        key(&mut state, Key::Ok);
        assert_eq!(state.picking, None);
        assert_eq!(state.value(Row::Device), "Digital Stereo (HDMI)");
        assert_eq!(state.device_id(), Some("pipewire/alsa_output.hdmi-stereo"));

        key(&mut state, Key::Ok);
        key(&mut state, Key::Up);
        key(&mut state, Key::Back);
        assert_eq!(state.picking, None);
        assert_eq!(state.value(Row::Device), "Digital Stereo (HDMI)", "back changed it");
    }

    /// The first option is the sink flipctl pinned, which is what passing mpv no
    /// device at all means.
    #[test]
    fn the_default_output_asks_for_nothing() {
        let state = radio();
        assert_eq!(state.value(Row::Device), DEFAULT_DEVICE);
        assert_eq!(state.device_id(), None);
    }

    /// Until the outputs are known the row says so and offers nothing, so neither
    /// the chevrons nor Ok can choose between one thing and itself.
    #[test]
    fn the_output_row_says_it_is_still_asking() {
        let mut state = radio();
        let (_tx, rx) = mpsc::channel();
        state.asking = Some(rx);
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
