//! Voice recorder, as a flipctl app: the panel's microphone or the headset jack to a
//! WAV, MP3 or OGG file, and the takes listed and played back.
//!
//! Ported from the prototype's js/apps/record.js and voice_recorder_files.js, which
//! asked a root server to run sox and amixer for them. There is no server here, no
//! root and no helper program: pulse.rs speaks to the sound server's socket,
//! capture.rs records and encodes, player.rs decodes and plays, all in this process,
//! so the app needs nothing installed. Where this differs from the prototype,
//! docs/inventory.md says so.
//!
//! A take keeps going when the app is sent to the background, since Back never
//! reaches an app and the process simply keeps running; Stop or Close ends it, and
//! closing finishes the file properly rather than abandoning it.

mod capture;
mod files;
mod length;
mod player;
mod pulse;
mod view;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use flipctl_app::{Key, StatusSource};
use slint::{ComponentHandle, ModelRc, VecModel};

use capture::{Capture, Format, FORMATS};
use files::SORTS;

slint::include_modules!();

/// The inputs, in the order the Input device row offers them: the codec's left
/// channel and its right.
pub const INPUTS: [&str; 2] = ["Built-in mic", "3.5 Audio"];

/// The gains move in fives, as the radio's volume does.
const STEP: i32 = 5;

/// The Edit box's rows: Delete and Sort.
pub const EDIT_ROWS: usize = 2;

/// How far Left and Right move a take being played.
const SEEK: u64 = 10;

/// How often the page is redrawn: the meter and the hundredths need it.
const TICK: Duration = Duration::from_millis(50);

/// How often the free space is read while a take runs. Faster just flickers
/// between estimates that are all the same.
const FREE_EVERY: Duration = Duration::from_secs(60);

/// How long a seek's `-10s` stays up.
const FLASH: Duration = Duration::from_millis(600);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Row {
    Gain,
    Input,
    Name,
    Output,
    Format,
}

pub const ROWS: [Row; 5] = [Row::Gain, Row::Input, Row::Output, Row::Name, Row::Format];

/// File name says what the next take is called; it is not a choice.
fn selectable(row: Row) -> bool {
    row != Row::Name
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Page {
    Main,
    Files,
}

/// The take being recorded.
pub struct Take {
    pub name: String,
    pub path: PathBuf,
    pub format: Format,
}

/// The take being played.
pub struct Playback {
    pub name: String,
    pub player: Option<player::Player>,
    /// The file is there but would not play.
    pub failed: bool,
    pub position: Duration,
    pub length: Option<Duration>,
    pub playing: bool,
    pub ended: bool,
    /// A seek's direction and size, while it is shown.
    pub flash: Option<(i32, u64)>,
    pub flashed: Option<Instant>,
    pub volume: i32,
}

pub enum Overlay {
    None,
    Edit { selected: usize },
    Delete { name: String, selected: usize },
    Player(Playback),
}

/// What the thread sent to ask the sound server came back with: the connection the
/// volumes are set over, the input gain if the codec's source is there, and the
/// speaker's volume.
struct Nodes {
    conn: Option<pulse::Conn>,
    gain: Option<i32>,
    outputs: Vec<(String, String)>,
}

pub struct Recorder {
    pub page: Page,
    /// Index into ROWS; always a selectable row.
    pub selected: usize,
    pub picking: Option<usize>,
    pub input: usize,
    pub format: usize,
    /// The input gain, 0 to 100, once the source has answered.
    pub gain: Option<i32>,
    /// The connection the two volumes are set over.
    control: Option<pulse::Conn>,
    /// Whether the codec's source is there; nothing until asked.
    pub codec: Option<bool>,
    asking: Option<Receiver<Nodes>>,
    capture: Option<Capture>,
    pub level: f32,
    pub take: Option<Take>,
    pub paused: bool,
    pub elapsed: Duration,
    pub dir: PathBuf,
    pub next: String,
    pub free: Option<u64>,
    freed: Option<Instant>,
    /// Bytes a second the take is actually costing, once it has run two seconds.
    pub rate: Option<u64>,
    pub files: Vec<files::Take>,
    pub file_sel: usize,
    pub file_scroll: usize,
    pub sort: usize,
    pub overlay: Overlay,
    pub error: Option<Vec<String>>,
    /// Where a take is played, as `(sink name, label)`, and which of them.
    pub outputs: Vec<(String, String)>,
    pub output: usize,
    started: Instant,
}

impl Recorder {
    fn new() -> Self {
        let dir = files::dir();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("nodes".into())
            .spawn(move || {
                let mut conn = pulse::Conn::open().ok();
                let gain = conn.as_mut().and_then(|c| c.source_volume(pulse::SOURCE));
                let outputs = conn.as_mut().map(|c| c.outputs()).unwrap_or_default();
                let _ = tx.send(Nodes { conn, gain, outputs });
            })
            .expect("spawn nodes");
        Self {
            page: Page::Main,
            selected: 0,
            picking: None,
            input: 0,
            format: 0,
            gain: None,
            control: None,
            codec: None,
            asking: Some(rx),
            capture: None,
            level: 0.0,
            take: None,
            paused: false,
            elapsed: Duration::ZERO,
            next: files::next_name(&dir),
            dir,
            free: None,
            freed: None,
            rate: None,
            files: Vec::new(),
            file_sel: 0,
            file_scroll: 0,
            sort: 0,
            overlay: Overlay::None,
            error: None,
            outputs: Vec::new(),
            output: 0,
            started: Instant::now(),
        }
    }

    fn fail(&mut self, title: &str, body: &str) {
        self.error = Some(vec![title.into(), body.into()]);
    }

    /// The capture stream runs while the meter is on screen and while a take needs
    /// it, and not otherwise.
    fn listen(&mut self) {
        let wanted = self.codec == Some(true) && (self.page == Page::Main || self.take.is_some());
        if wanted && self.capture.is_none() {
            self.capture = Capture::open(self.input).ok();
        } else if !wanted && self.capture.is_some() {
            self.capture = None;
            self.level = 0.0;
        }
    }

    fn start(&mut self) {
        if self.codec != Some(true) {
            self.fail("Recording error", "Microphone not found");
            return;
        }
        self.listen();
        let Some(capture) = self.capture.as_ref() else {
            self.fail("Recording error", "Could not open the microphone");
            return;
        };
        if std::fs::create_dir_all(&self.dir).is_err() {
            self.fail("Recording error", "Could not create the folder");
            return;
        }
        let format = FORMATS[self.format];
        let name = files::next_name(&self.dir);
        let path = capture::path(&self.dir, &name, format);
        if let Err(e) = capture.start(&path, format) {
            let what = if e.kind() == std::io::ErrorKind::NotFound { "Encoder not installed" } else { "Could not write the file" };
            self.fail("Recording error", what);
            return;
        }
        self.take = Some(Take { name, path, format });
        self.paused = false;
        self.elapsed = Duration::ZERO;
        self.rate = None;
        self.selected = 0;
        self.free = files::free(&self.dir);
        self.freed = Some(Instant::now());
    }

    fn stop(&mut self) {
        if let Some(capture) = self.capture.as_ref() {
            let _ = capture.stop();
        }
        self.take = None;
        self.paused = false;
        self.next = files::next_name(&self.dir);
        self.listen();
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if let Some(capture) = self.capture.as_ref() {
            capture.set_paused(paused);
        }
    }

    fn set_input(&mut self, input: usize) {
        self.input = input;
        if let Some(capture) = self.capture.as_ref() {
            capture.set_channel(input);
        }
    }

    fn adjust_gain(&mut self, delta: i32) {
        let Some(gain) = self.gain else { return };
        let gain = (gain + delta).clamp(0, 100);
        self.gain = Some(gain);
        self.control(|c| c.set_source_volume(pulse::SOURCE, gain));
    }

    /// Set a volume over the control connection, opening it again once if the
    /// sound server went away under it.
    fn control(&mut self, mut set: impl FnMut(&mut pulse::Conn) -> std::io::Result<()>) {
        if self.control.as_mut().is_some_and(|c| set(c).is_ok()) {
            return;
        }
        self.control = pulse::Conn::open().ok();
        if let Some(c) = self.control.as_mut() {
            let _ = set(c);
        }
    }

    fn choose(&mut self, row: Row, index: usize) {
        match row {
            Row::Input => self.set_input(index.min(INPUTS.len() - 1)),
            Row::Format => self.format = index.min(FORMATS.len() - 1),
            Row::Output => self.output = index.min(self.outputs.len().saturating_sub(1)),
            _ => {}
        }
    }

    fn current(&self, row: Row) -> usize {
        match row {
            Row::Input => self.input,
            Row::Format => self.format,
            Row::Output => self.output,
            _ => 0,
        }
    }

    fn open_files(&mut self) {
        self.page = Page::Files;
        self.picking = None;
        self.refresh_files(None);
        self.listen();
    }

    /// Read the folder again, keeping `keep` selected when it is still there.
    fn refresh_files(&mut self, keep: Option<String>) {
        let mut takes = files::list(&self.dir);
        if let Some(take) = &self.take {
            let name = take.path.file_name().map(|n| n.to_string_lossy().into_owned());
            takes.retain(|t| Some(&t.name) != name.as_ref());
        }
        SORTS[self.sort].apply(&mut takes);
        let keep = keep.or_else(|| self.files.get(self.file_sel).map(|t| t.name.clone()));
        self.files = takes;
        self.file_sel = keep
            .and_then(|k| self.files.iter().position(|t| t.name == k))
            .unwrap_or(self.file_sel)
            .min(self.files.len().saturating_sub(1));
        self.scroll_files();
    }

    fn scroll_files(&mut self) {
        let visible = flipctl_app::theme::count::LIST_VISIBLE_ROWS as usize;
        if self.file_sel < self.file_scroll {
            self.file_scroll = self.file_sel;
        } else if self.file_sel >= self.file_scroll + visible {
            self.file_scroll = self.file_sel + 1 - visible;
        }
        self.file_scroll = self.file_scroll.min(self.files.len().saturating_sub(visible));
    }

    /// The sink a take is played on: the one chosen in the Edit box, or the one
    /// flipctl pinned before the outputs are known.
    pub fn sink(&self) -> String {
        self.outputs.get(self.output).map_or_else(pulse::sink, |(name, _)| name.clone())
    }

    /// The chosen output's label, for its row.
    pub fn output_label(&self) -> String {
        self.outputs.get(self.output).map_or_else(|| "Speaker".into(), |(_, label)| label.clone())
    }

    fn play(&mut self) {
        let Some(take) = self.files.get(self.file_sel).cloned() else { return };
        let sink = self.sink();
        let volume = self.control.as_mut().and_then(|c| c.sink_volume(&sink)).unwrap_or(0);
        let player = player::Player::start(&self.dir.join(&take.name), &sink).ok();
        self.overlay = Overlay::Player(Playback {
            name: take.name.clone(),
            playing: player.is_some(),
            failed: player.is_none(),
            player,
            position: Duration::ZERO,
            length: take.length,
            ended: false,
            flash: None,
            flashed: None,
            volume,
        });
    }

    fn tick(&mut self) {
        if let Some(rx) = self.asking.as_ref() {
            if let Ok(nodes) = rx.try_recv() {
                self.asking = None;
                self.codec = Some(nodes.gain.is_some());
                self.gain = nodes.gain;
                self.control = nodes.conn;
                let pinned = pulse::sink();
                self.output = nodes.outputs.iter().position(|(n, _)| *n == pinned).unwrap_or(0);
                self.outputs = nodes.outputs;
                self.listen();
            }
        }
        if let Some(capture) = self.capture.as_ref() {
            self.level = capture.level().max(self.level * 0.7);
            if self.take.is_some() {
                self.elapsed = capture.elapsed();
                if capture.failed() {
                    self.stop();
                    self.fail("Recording error", "Could not write the file");
                }
            }
        }
        if let Some(take) = &self.take {
            if self.freed.is_none_or(|t| t.elapsed() >= FREE_EVERY) {
                self.free = files::free(&self.dir);
                self.freed = Some(Instant::now());
                let secs = self.elapsed.as_secs();
                let bytes = std::fs::metadata(&take.path).map(|m| m.len()).unwrap_or(0);
                if secs >= 2 && bytes > 0 {
                    self.rate = Some(bytes / secs);
                }
            }
        }
        if let Overlay::Player(p) = &mut self.overlay {
            if p.flashed.is_some_and(|t| t.elapsed() >= FLASH) {
                p.flash = None;
                p.flashed = None;
            }
            if let Some(player) = p.player.as_ref() {
                if !player.alive() {
                    p.player = None;
                    p.playing = false;
                    p.failed = true;
                    return;
                }
                p.position = player.position();
                p.length = player.length().or(p.length);
                p.ended = player.ended();
                p.playing = player.playing();
            }
        }
    }

    fn close(&mut self) {
        self.overlay = Overlay::None;
        if self.take.is_some() {
            self.stop();
        }
        self.capture = None;
    }

    /// One key on the way down. Returns false when the app should close.
    fn key(&mut self, key: Key) -> bool {
        if self.error.is_some() {
            if matches!(key, Key::Ok | Key::Run | Key::Escape) {
                self.error = None;
            }
            return true;
        }
        match self.page {
            Page::Main => self.main_key(key),
            Page::Files => self.files_key(key),
        }
    }

    fn main_key(&mut self, key: Key) -> bool {
        let row = ROWS[self.selected];
        if let Some(highlighted) = self.picking {
            let total = self.options(row).len();
            match key {
                Key::Escape => self.picking = None,
                Key::Down if total > 0 => self.picking = Some((highlighted + 1) % total),
                Key::Up if total > 0 => self.picking = Some((highlighted + total - 1) % total),
                Key::Ok | Key::Run => {
                    self.picking = None;
                    self.choose(row, highlighted);
                }
                _ => {}
            }
            return true;
        }
        match key {
            Key::Escape => {
                self.close();
                return false;
            }
            Key::Run if !self.paused => {
                if self.take.is_some() {
                    self.stop();
                } else {
                    self.start();
                }
            }
            Key::Edit if self.take.is_some() => self.set_paused(!self.paused),
            Key::View => self.open_files(),
            Key::Down | Key::Up if self.take.is_none() => {
                let forward = key == Key::Down;
                let n = ROWS.len();
                let mut at = self.selected;
                loop {
                    at = if forward { (at + 1) % n } else { (at + n - 1) % n };
                    if selectable(ROWS[at]) {
                        break;
                    }
                }
                self.selected = at;
            }
            Key::Left | Key::Right => {
                let forward = key == Key::Right;
                if row == Row::Gain {
                    self.adjust_gain(if forward { STEP } else { -STEP });
                } else if self.take.is_none() {
                    let total = self.options(row).len();
                    if total > 1 {
                        let at = self.current(row);
                        self.choose(row, if forward { (at + 1) % total } else { (at + total - 1) % total });
                    }
                }
            }
            Key::Ok if self.take.is_none() && !self.options(row).is_empty() => {
                self.picking = Some(self.current(row));
            }
            _ => {}
        }
        true
    }

    fn files_key(&mut self, key: Key) -> bool {
        match std::mem::replace(&mut self.overlay, Overlay::None) {
            Overlay::None => {}
            Overlay::Edit { selected } => {
                self.overlay = Overlay::Edit { selected };
                self.edit_key(key, selected);
                return true;
            }
            Overlay::Delete { name, selected } => {
                match key {
                    Key::Up | Key::Down => self.overlay = Overlay::Delete { name, selected: 1 - selected },
                    Key::Ok | Key::Run if selected == 1 => {
                        let _ = std::fs::remove_file(self.dir.join(&name));
                        self.refresh_files(None);
                        self.next = files::next_name(&self.dir);
                    }
                    Key::Ok | Key::Run | Key::Escape => {}
                    _ => self.overlay = Overlay::Delete { name, selected },
                }
                return true;
            }
            Overlay::Player(mut p) => {
                if key == Key::Escape {
                    return true;
                }
                self.player_key(&mut p, key);
                self.overlay = Overlay::Player(p);
                return true;
            }
        }
        let n = self.files.len();
        match key {
            Key::Escape => {
                self.close();
                return false;
            }
            Key::View => {
                self.page = Page::Main;
                self.listen();
            }
            Key::Down if n > 0 => {
                self.file_sel = (self.file_sel + 1) % n;
                self.scroll_files();
            }
            Key::Up if n > 0 => {
                self.file_sel = (self.file_sel + n - 1) % n;
                self.scroll_files();
            }
            Key::Ok | Key::Run if n > 0 => self.play(),
            Key::Edit => self.overlay = Overlay::Edit { selected: 0 },
            _ => {}
        }
        true
    }

    /// The Edit box: Delete, then Sort in a chip, which Left and Right step through
    /// and Ok applies, closing the box.
    fn edit_key(&mut self, key: Key, selected: usize) {
        let rows = EDIT_ROWS;
        match key {
            Key::Edit | Key::Escape => self.overlay = Overlay::None,
            Key::Down => self.overlay = Overlay::Edit { selected: (selected + 1) % rows },
            Key::Up => self.overlay = Overlay::Edit { selected: (selected + rows - 1) % rows },
            Key::Left | Key::Right if selected > 0 => {
                let n = SORTS.len();
                self.sort = if key == Key::Right { (self.sort + 1) % n } else { (self.sort + n - 1) % n };
                self.refresh_files(None);
            }
            Key::Ok | Key::Run if selected > 0 => self.overlay = Overlay::None,
            Key::Ok | Key::Run => {
                if let Some(take) = self.files.get(self.file_sel) {
                    self.overlay = Overlay::Delete { name: take.name.clone(), selected: 0 };
                }
            }
            _ => {}
        }
    }

    fn player_key(&mut self, p: &mut Playback, key: Key) {
        match key {
            Key::Up | Key::Down => {
                let delta = if key == Key::Up { STEP } else { -STEP };
                p.volume = (p.volume + delta).clamp(0, 100);
                let (volume, sink) = (p.volume, self.sink());
                self.control(|c| c.set_sink_volume(&sink, volume));
            }
            _ => {}
        }
        let Some(player) = p.player.as_ref() else {
            if matches!(key, Key::Ok | Key::Run) {
                let file = self.dir.join(&p.name);
                p.player = player::Player::start(&file, &self.sink()).ok();
                p.playing = p.player.is_some();
                p.failed = p.player.is_none();
                p.position = Duration::ZERO;
            }
            return;
        };
        match key {
            Key::Ok | Key::Run => {
                if p.ended {
                    player.set_paused(false);
                    player.seek(Duration::ZERO);
                    p.ended = false;
                    p.playing = true;
                } else {
                    p.playing = !p.playing;
                    player.set_paused(!p.playing);
                }
            }
            Key::Left | Key::Right => {
                let forward = key == Key::Right;
                let step = Duration::from_secs(SEEK);
                let to = if forward { p.position + step } else { p.position.saturating_sub(step) };
                let to = p.length.map_or(to, |l| to.min(l));
                player.seek(to);
                p.position = to;
                p.flash = Some((if forward { 1 } else { -1 }, SEEK));
                p.flashed = Some(Instant::now());
            }
            _ => {}
        }
    }
}

fn frames(list: &[view::Frame]) -> ModelRc<Frame> {
    ModelRc::new(VecModel::from(
        list.iter()
            .map(|f| Frame {
                x: f.x as f32,
                y: f.y as f32,
                w: f.w as f32,
                h: f.h as f32,
                radius: f.radius as f32,
                fill: f.fill,
                stroke: f.stroke,
                shadow: f.shadow,
                square_right: f.square_right,
            })
            .collect::<Vec<_>>(),
    ))
}

fn to_label(l: &view::Label) -> Label {
    Label { x: l.x as f32, y: l.y as f32, text: l.text.as_str().into(), bold: l.bold, ink: l.ink }
}

fn labels(list: &[view::Label]) -> ModelRc<Label> {
    ModelRc::new(VecModel::from(list.iter().map(to_label).collect::<Vec<_>>()))
}

fn strings(list: &[&str]) -> ModelRc<slint::SharedString> {
    ModelRc::new(VecModel::from(list.iter().map(|s| (*s).into()).collect::<Vec<_>>()))
}

/// Put the state on screen. Every frame is built from the state, so what is drawn
/// cannot disagree with what is true.
fn apply(ui: &AppWindow, state: &Recorder) {
    let s = state.screen(state.started.elapsed());
    ui.set_page(s.page);
    ui.set_buttons(strings(&s.buttons));
    ui.set_rows(ModelRc::new(VecModel::from(
        s.rows
            .iter()
            .map(|line| DropRow {
                y: line.y as f32,
                title: line.title.as_str().into(),
                value: line.value.as_str().into(),
                value_x: line.value_x as f32,
                kind: line.kind,
                fill_w: line.fill_w as f32,
                selected: line.selected,
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_selector(s.sel_y.is_some());
    ui.set_sel_y(s.sel_y.unwrap_or(0) as f32);
    ui.set_frames(frames(&s.frames));
    ui.set_labels(labels(&s.labels));
    ui.set_sprites(ModelRc::new(VecModel::from(
        s.sprites
            .iter()
            .map(|p| Sprite { x: p.x as f32, y: p.y as f32, which: p.which, white: true, opacity: p.opacity })
            .collect::<Vec<_>>(),
    )));
    ui.set_picking(s.picker.is_some());
    if let Some((view, title)) = &s.picker {
        ui.set_view(DropView {
            x: view.x as f32,
            y: view.y as f32,
            h: view.h as f32,
            title: "".into(),
            sel_y: view.sel_y as f32,
            sel_w: view.sel_w as f32,
            sel_h: view.sel_h as f32,
        });
        ui.set_items(ModelRc::new(VecModel::from(
            view.items
                .iter()
                .map(|item| DropItem {
                    y: item.y as f32,
                    text: item.text.as_str().into(),
                    text_x: item.text_x as f32,
                    rule: item.rule,
                })
                .collect::<Vec<_>>(),
        )));
        ui.set_pick_title(to_label(title));
    }
    ui.set_files(ModelRc::new(VecModel::from(
        s.files
            .iter()
            .map(|(name, length)| ListItem {
                label: name.as_str().into(),
                status: length.as_str().into(),
                frames: 1,
                ..ListItem::default()
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_file_selected(s.file_selected as i32);
    ui.set_file_scroll(s.file_scroll as i32);
    ui.set_scrim(s.scrim);
    ui.set_over_frames(frames(&s.over_frames));
    ui.set_over_labels(labels(&s.over_labels));
    ui.set_over_buttons(strings(&s.over_buttons));
    ui.set_dialog(ModelRc::new(VecModel::from(
        s.dialog.iter().map(|l| l.as_str().into()).collect::<Vec<slint::SharedString>>(),
    )));
    ui.set_dialog_right(if s.dialog.is_empty() { "".into() } else { "Ok".into() });
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let state = Rc::new(RefCell::new(Recorder::new()));
    apply(&ui, &state.borrow());

    let mut status = StatusSource::new(Duration::from_secs(2));
    flipctl_app::apply_status!(&ui, PanelStatus, status.current());

    let tick = slint::Timer::default();
    let ticking = ui.as_weak();
    let ticked = Rc::clone(&state);
    let mut polled = Instant::now();
    tick.start(slint::TimerMode::Repeated, TICK, move || {
        let Some(ui) = ticking.upgrade() else {
            return;
        };
        let mut state = ticked.borrow_mut();
        state.tick();
        apply(&ui, &state);
        if polled.elapsed() >= Duration::from_secs(1) {
            polled = Instant::now();
            if let Some(now) = status.poll() {
                flipctl_app::apply_status!(&ui, PanelStatus, now);
            }
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
        ui.set_pressed_slot(match (down, pressed.soft_slot()) {
            (true, Some(slot)) => slot as i32,
            _ => -1,
        });
        if !down {
            return;
        }
        let mut state = keyed.borrow_mut();
        if !state.key(pressed) {
            let _ = slint::quit_event_loop();
            return;
        }
        apply(&ui, &state);
    });

    let result = ui.run();
    state.borrow_mut().close();
    result
}

#[cfg(test)]
mod tests;
