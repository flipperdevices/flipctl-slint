//! An app that says what it shows and leaves the rest here.
//!
//! The app implements [`App`]: the page it is on, the soft keys' labels, and what its
//! keys do. The window, the status bar's poll, the press shown on a soft key before
//! it acts, scrolling, a menu's cursor and Escape closing are done here, by the pages
//! in `ui/pages.slint`, which `build::pages()` compiles and [`main!`](crate::main)
//! starts:
//!
//! ```ignore
//! // build.rs
//! fn main() {
//!     flipctl_app::build::pages();
//! }
//! ```
//!
//! ```ignore
//! use flipctl_app::{gauge, pair, App, Key, Page, Ui};
//!
//! struct Hello {
//!     presses: u32,
//! }
//!
//! impl App for Hello {
//!     fn page(&self) -> Page {
//!         Page::detail(vec![pair("Presses", self.presses.to_string())])
//!     }
//!     fn buttons(&self) -> [&str; 5] {
//!         ["Close", "", "", "", "Add"]
//!     }
//!     fn key(&mut self, key: Key, _: &mut Ui) -> bool {
//!         if key != Key::Run {
//!             return false;
//!         }
//!         self.presses += 1;
//!         true
//!     }
//! }
//!
//! flipctl_app::main!(Hello { presses: 0 });
//! ```
//!
//! Everything that decides what is drawn is plain Rust here, tested without a
//! window. The types Slint generates belong to the app's crate, so the one piece that
//! touches them, [`Screen`], is written by the macro.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use flipper_ui::status::{Status, StatusSource};
use flipper_ui::theme::{count, timing};

use crate::Key;

/// What a page shows. The app returns one from [`App::page`] each time it is drawn.
pub struct Page {
    id: &'static str,
    body: Body,
}

/// The kinds of page, one per body in `pages.slint`.
pub enum Body {
    Detail { rows: Vec<Row>, fit_gauges: bool },
    Menu { items: Vec<Item> },
    Log { title: String, lines: Vec<String> },
    Canvas { picture: slint::Image, texts: Vec<Text> },
}

impl Page {
    /// Rows, made with [`pair`], [`gauge`] and the rest. Up and Down scroll a page
    /// longer than the panel.
    pub fn detail(rows: Vec<Row>) -> Self {
        Self::of(Body::Detail { rows, fit_gauges: false })
    }

    /// A menu of [`item`]s. Up and Down move the cursor, round from the last row to the
    /// first; [`Ui::selected`] is the row under it, and Ok flashes that row before the
    /// app sees it.
    pub fn menu(items: Vec<Item>) -> Self {
        Self::of(Body::Menu { items })
    }

    /// Lines of output under a title, oldest first. Up and Down scroll.
    pub fn log(title: impl Into<String>, lines: Vec<String>) -> Self {
        Self::of(Body::Log { title: title.into(), lines })
    }

    /// A picture the app paints, the whole panel as `paint::Surface::panel()` makes it,
    /// with text drawn over it in the panel's fonts.
    pub fn canvas(picture: slint::Image, texts: Vec<Text>) -> Self {
        Self::of(Body::Canvas { picture, texts })
    }

    /// Size a gauge around its own label rather than the column flipctl's screens use.
    pub fn fit_gauges(mut self) -> Self {
        if let Body::Detail { fit_gauges, .. } = &mut self.body {
            *fit_gauges = true;
        }
        self
    }

    /// Which page this is, for an app with more than one of a kind: each keeps its own
    /// scroll and cursor. Without one, a page is known by its kind.
    pub fn id(mut self, id: &'static str) -> Self {
        self.id = id;
        self
    }

    fn of(body: Body) -> Self {
        Self { id: "", body }
    }

    fn kind(&self) -> i32 {
        match self.body {
            Body::Detail { .. } => 0,
            Body::Log { .. } => 1,
            Body::Menu { .. } => 2,
            Body::Canvas { .. } => 3,
        }
    }
}

/// One detail row, as `DetailRow` in `detail.slint` takes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub kind: i32,
    pub label: String,
    pub value: String,
    pub percent: i32,
    pub dim: bool,
    /// For a [`choice`]: whether its value is at the first and the last of its range.
    pub ends: Option<(bool, bool)>,
}

fn row(kind: i32, label: impl Into<String>, value: impl Into<String>) -> Row {
    Row { kind, label: label.into(), value: value.into(), ..Row::default() }
}

/// A label on the left and its value on the right.
pub fn pair(label: impl Into<String>, value: impl Into<String>) -> Row {
    row(0, label, value)
}

/// A divider between groups of rows.
pub fn rule() -> Row {
    row(1, "", "")
}

/// A label, a bar filled to `percent` (0 to 100), and the value beside it.
pub fn gauge(label: impl Into<String>, value: impl Into<String>, percent: i32) -> Row {
    Row { percent: percent.clamp(0, 100), ..row(2, label, value) }
}

/// A line of text across the whole row.
pub fn text(line: impl Into<String>) -> Row {
    row(3, line, "")
}

/// A value Left and Right change, between chevrons. `first` and `last` say it is at an
/// end of its range, which drops the chevron pointing past it.
pub fn choice(label: impl Into<String>, value: impl Into<String>, first: bool, last: bool) -> Row {
    Row { ends: Some((first, last)), ..row(4, label, value) }
}

/// A label and value with the label in the bold face.
pub fn heading(label: impl Into<String>, value: impl Into<String>) -> Row {
    row(5, label, value)
}

impl Row {
    /// In the dim tone, for a secondary line.
    pub fn dim(mut self) -> Self {
        self.dim = true;
        self
    }
}

/// One menu row, as `ListItem` in `list.slint` takes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    pub label: String,
    pub tag: String,
    pub status: String,
    pub chevrons: i32,
    pub dim: bool,
    pub value: String,
    pub at_start: bool,
    pub at_end: bool,
}

/// A menu row with its label, built on with [`Item::status`] or [`Item::toggle`] and
/// the rest.
pub fn item(label: impl Into<String>) -> Item {
    Item { label: label.into(), ..Item::default() }
}

impl Item {
    /// What the row says at its right edge.
    pub fn status(mut self, status: impl Into<String>) -> Self {
        self.status = status.into();
        self
    }

    /// An on/off value between chevrons, which Left and Right flip.
    pub fn toggle(mut self, value: impl Into<String>) -> Self {
        self.chevrons = 1;
        self.value = value.into();
        self
    }

    /// A value with a range between chevrons, which Left and Right step. `first` and
    /// `last` drop the chevron pointing past an end.
    pub fn range(mut self, value: impl Into<String>, first: bool, last: bool) -> Self {
        self.chevrons = 2;
        self.value = value.into();
        (self.at_start, self.at_end) = (first, last);
        self
    }

    /// Keep the status in the dim tone when the row is selected: a remark about the row
    /// rather than its value.
    pub fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    /// A short word drawn right after the label.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = tag.into();
        self
    }
}

/// Text over a canvas, as `CanvasText` in `canvas.slint` takes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Text {
    pub x: f32,
    pub y: f32,
    pub text: String,
    /// 0 the title face, 1 and 2 a row's.
    pub font: i32,
    /// -1 starts at `x`, 0 centres on it, 1 ends at it.
    pub align: i32,
    pub white: bool,
}

/// An app on the pages: what it shows and what its keys do.
pub trait App {
    /// The page as it is now. Called after every key, on [`App::every`]'s clock and
    /// whenever [`redraw`] is called.
    fn page(&self) -> Page;

    /// The five soft keys' labels, left to right; an empty one is a blank slot.
    fn buttons(&self) -> [&str; 5];

    /// A key as it goes down, after a soft key or a menu row has shown it pressed.
    /// Return true for a key the app used; one it leaves keeps its default, Escape
    /// closing and Up and Down scrolling or moving the cursor. Back never arrives:
    /// flipctl keeps it to send the app to the background.
    fn key(&mut self, key: Key, ui: &mut Ui) -> bool {
        let _ = (key, ui);
        false
    }

    /// How often to call [`App::tick`] and draw, for an app showing something that
    /// changes on its own.
    fn every(&self) -> Option<Duration> {
        None
    }

    /// Called on [`App::every`]'s clock, before the page is drawn.
    fn tick(&mut self, ui: &mut Ui) {
        let _ = ui;
    }

    /// Called once as the app closes, to give back whatever it took.
    fn close(&mut self) {}
}

/// The page's own state, which a key handler reads and moves.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ui {
    offset: usize,
    selected: usize,
    closing: bool,
}

impl Ui {
    /// The menu row under the cursor.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Put the menu's cursor on a row; drawing keeps it in range and in view.
    pub fn select(&mut self, row: usize) {
        self.selected = row;
    }

    /// The first row or line shown.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Scroll so `row` is the first shown; drawing clamps it to what there is.
    pub fn scroll_to(&mut self, row: usize) {
        self.offset = row;
    }

    /// Close the app, after [`App::close`].
    pub fn close(&mut self) {
        self.closing = true;
    }
}

/// The page component, as the app's generated code has it. [`main!`](crate::main)
/// writes this for `pages.slint`; it is public so tests can stand a fake in.
pub trait Screen {
    fn set_mode(&self, mode: i32);
    fn set_buttons(&self, labels: [&str; 5]);
    /// Detail rows, the first shown, and the ends of a choice row's range.
    fn set_rows(&self, rows: &[Row], fit_gauges: bool, offset: usize, ends: (bool, bool));
    /// How many detail rows fit, which the body works out from the panel.
    fn visible_rows(&self) -> usize;
    fn set_menu(&self, items: &[Item], selected: usize, scroll: usize);
    fn set_row_pressed(&self, pressed: bool);
    fn set_log(&self, title: &str, lines: &[String], total: usize, offset: usize);
    fn set_canvas(&self, picture: &slint::Image, texts: &[Text]);
    fn set_pressed_slot(&self, slot: i32);
    fn set_arrow_pressed(&self, arrow: i32);
    fn set_status(&self, status: &Status);
}

/// What a key going down asks for: to act now, or to show a press first and act when
/// [`Driver::release`] is called `press_flash_ms` later.
#[derive(Debug, PartialEq)]
pub enum Press {
    Now,
    Flash,
    Ignored,
}

/// The app, its screen, and the state of each page it has shown.
pub struct Driver<A, S> {
    pub app: A,
    pub screen: S,
    states: Vec<(i32, &'static str, Ui)>,
    /// The page last drawn, which a key acts on.
    current: (i32, &'static str),
    /// Items in the menu last drawn, which the cursor wraps round.
    count: usize,
    closed: bool,
}

impl<A: App, S: Screen> Driver<A, S> {
    pub fn new(app: A, screen: S) -> Self {
        Self { app, screen, states: Vec::new(), current: (-1, ""), count: 0, closed: false }
    }

    /// Whether the app has closed, which the event loop then leaves.
    pub fn closed(&self) -> bool {
        self.closed
    }

    fn state(&mut self, page: (i32, &'static str)) -> &mut Ui {
        let at = match self.states.iter().position(|s| (s.0, s.1) == page) {
            Some(at) => at,
            None => {
                self.states.push((page.0, page.1, Ui::default()));
                self.states.len() - 1
            }
        };
        &mut self.states[at].2
    }

    /// Draw the app's page as it is now.
    pub fn draw(&mut self) {
        let page = self.app.page();
        let here = (page.kind(), page.id);
        self.current = here;
        self.screen.set_mode(here.0);
        match &page.body {
            Body::Detail { rows, fit_gauges } => {
                // Clamped to what is there now, so a row that goes away does not leave
                // the page scrolled past its end.
                let last = rows.len().saturating_sub(self.screen.visible_rows());
                let ui = self.state(here);
                ui.offset = ui.offset.min(last);
                let offset = ui.offset;
                let ends = rows.iter().find_map(|r| r.ends).unwrap_or_default();
                self.screen.set_rows(rows, *fit_gauges, offset, ends);
            }
            Body::Menu { items } => {
                self.count = items.len();
                let visible = count::LIST_VISIBLE_ROWS as usize;
                let ui = self.state(here);
                ui.selected = ui.selected.min(items.len().saturating_sub(1));
                // The window moves only as far as keeps the cursor in it.
                let top = ui.offset.max((ui.selected + 1).saturating_sub(visible)).min(ui.selected);
                ui.offset = top.min(items.len().saturating_sub(visible));
                let (selected, scroll) = (ui.selected, ui.offset);
                self.screen.set_menu(items, selected, scroll);
            }
            Body::Log { title, lines } => {
                let visible = count::LOG_VISIBLE_LINES as usize;
                let ui = self.state(here);
                ui.offset = ui.offset.min(lines.len().saturating_sub(visible));
                let offset = ui.offset;
                let shown = &lines[offset..lines.len().min(offset + visible)];
                self.screen.set_log(title, shown, lines.len(), offset);
            }
            Body::Canvas { picture, texts } => self.screen.set_canvas(picture, texts),
        }
        self.screen.set_buttons(self.app.buttons());
    }

    /// A key from the panel. Left and Right light their chevron while held; a soft key,
    /// and Ok on a menu, is shown pressed and acted on by [`Driver::release`], so the
    /// press is seen even when what it does replaces the page.
    pub fn key(&mut self, key: Key, down: bool) -> Press {
        if matches!(key, Key::Left | Key::Right) {
            self.screen.set_arrow_pressed(match (down, key) {
                (false, _) => 0,
                (true, Key::Left) => 1,
                _ => 2,
            });
        }
        if !down || self.closed {
            return Press::Ignored;
        }
        if let Some(slot) = key.soft_slot() {
            self.screen.set_pressed_slot(slot as i32);
            return Press::Flash;
        }
        if key == Key::Ok && self.current.0 == 2 {
            self.screen.set_row_pressed(true);
            return Press::Flash;
        }
        self.act(key);
        Press::Now
    }

    /// The end of a press's flash: drawn released, then acted on.
    pub fn release(&mut self, key: Key) {
        self.screen.set_pressed_slot(-1);
        self.screen.set_row_pressed(false);
        self.act(key);
    }

    /// The clock [`App::every`] asked for.
    pub fn tick(&mut self) {
        if self.closed {
            return;
        }
        let here = self.current;
        let mut ui = *self.state(here);
        self.app.tick(&mut ui);
        self.settle(here, ui);
    }

    /// A status reading for the bar.
    pub fn status(&self, status: &Status) {
        self.screen.set_status(status);
    }

    fn act(&mut self, key: Key) {
        if self.closed {
            return;
        }
        let here = self.current;
        let mut ui = *self.state(here);
        if !self.app.key(key, &mut ui) {
            match key {
                Key::Escape => ui.closing = true,
                Key::Down => self.step(&mut ui, 1),
                Key::Up => self.step(&mut ui, -1),
                _ => {}
            }
        }
        self.settle(here, ui);
    }

    fn step(&self, ui: &mut Ui, by: isize) {
        if self.current.0 == 2 {
            if self.count > 0 {
                ui.selected = (ui.selected as isize + by).rem_euclid(self.count as isize) as usize;
            }
        } else {
            ui.offset = ui.offset.saturating_add_signed(by);
        }
    }

    /// Keep what a handler did to the page's state, then close or draw.
    fn settle(&mut self, here: (i32, &'static str), ui: Ui) {
        *self.state(here) = Ui { closing: false, ..ui };
        if ui.closing {
            self.closed = true;
            self.app.close();
        } else {
            self.draw();
        }
    }
}

thread_local! {
    static REDRAW: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Draw the app's page again, from any thread: for an app whose page changes when
/// something it is waiting on arrives rather than on a key or a clock.
pub fn redraw() {
    let _ = slint::invoke_from_event_loop(|| {
        REDRAW.with(|hook| {
            if let Some(hook) = hook.borrow().as_ref() {
                hook();
            }
        });
    });
}

/// Run an app on a screen until it closes: what [`main!`](crate::main) calls once it
/// has made the window. `bind` hands the screen's key callback over, as the generated
/// code takes it, and `run` is the window's own event loop.
pub fn run<A, S>(
    app: A,
    screen: S,
    bind: impl FnOnce(Box<dyn Fn(&str, bool)>),
    run: impl FnOnce() -> Result<(), slint::PlatformError>,
) -> Result<(), slint::PlatformError>
where
    A: App + 'static,
    S: Screen + 'static,
{
    let every = app.every();
    let driver = Rc::new(RefCell::new(Driver::new(app, screen)));
    driver.borrow_mut().draw();

    let keys = Rc::downgrade(&driver);
    bind(Box::new(move |text, down| {
        let (Some(driver), Some(key)) = (keys.upgrade(), Key::from_slint(text)) else {
            return;
        };
        let press = driver.borrow_mut().key(key, down);
        if press == Press::Flash {
            let later = Rc::downgrade(&driver);
            let flash = Duration::from_millis(timing::PRESS_FLASH_MS as u64);
            slint::Timer::single_shot(flash, move || {
                if let Some(driver) = later.upgrade() {
                    driver.borrow_mut().release(key);
                }
            });
        }
        if driver.borrow().closed() {
            let _ = slint::quit_event_loop();
        }
    }));

    let hooked = Rc::downgrade(&driver);
    REDRAW.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            if let Some(driver) = hooked.upgrade() {
                driver.borrow_mut().draw();
            }
        }));
    });

    let mut source = StatusSource::new(Duration::from_secs(2));
    driver.borrow().status(&source.current());
    let polled = Rc::downgrade(&driver);
    let status = slint::Timer::default();
    status.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        if let (Some(driver), Some(now)) = (polled.upgrade(), source.poll()) {
            driver.borrow().status(&now);
        }
    });

    let clock = slint::Timer::default();
    if let Some(every) = every {
        let ticked = Rc::downgrade(&driver);
        clock.start(slint::TimerMode::Repeated, every, move || {
            if let Some(driver) = ticked.upgrade() {
                driver.borrow_mut().tick();
                if driver.borrow().closed() {
                    let _ = slint::quit_event_loop();
                }
            }
        });
    }

    run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Seen {
        mode: i32,
        buttons: Vec<String>,
        rows: Vec<Row>,
        offset: usize,
        ends: (bool, bool),
        items: usize,
        selected: usize,
        scroll: usize,
        row_pressed: bool,
        lines: Vec<String>,
        total: usize,
        slot: i32,
        arrow: i32,
    }

    #[derive(Default)]
    struct Fake(RefCell<Seen>);

    impl Screen for Fake {
        fn set_mode(&self, mode: i32) {
            self.0.borrow_mut().mode = mode;
        }
        fn set_buttons(&self, labels: [&str; 5]) {
            self.0.borrow_mut().buttons = labels.map(String::from).to_vec();
        }
        fn set_rows(&self, rows: &[Row], _: bool, offset: usize, ends: (bool, bool)) {
            let mut s = self.0.borrow_mut();
            (s.rows, s.offset, s.ends) = (rows.to_vec(), offset, ends);
        }
        fn visible_rows(&self) -> usize {
            8
        }
        fn set_menu(&self, items: &[Item], selected: usize, scroll: usize) {
            let mut s = self.0.borrow_mut();
            (s.items, s.selected, s.scroll) = (items.len(), selected, scroll);
        }
        fn set_row_pressed(&self, pressed: bool) {
            self.0.borrow_mut().row_pressed = pressed;
        }
        fn set_log(&self, _: &str, lines: &[String], total: usize, offset: usize) {
            let mut s = self.0.borrow_mut();
            (s.lines, s.total, s.offset) = (lines.to_vec(), total, offset);
        }
        fn set_canvas(&self, _: &slint::Image, _: &[Text]) {}
        fn set_pressed_slot(&self, slot: i32) {
            self.0.borrow_mut().slot = slot;
        }
        fn set_arrow_pressed(&self, arrow: i32) {
            self.0.borrow_mut().arrow = arrow;
        }
        fn set_status(&self, _: &Status) {}
    }

    /// Rows, a menu or a log by `kind`, of `n` entries; Run counts and Ok picks.
    struct Test {
        kind: i32,
        n: usize,
        runs: u32,
        picked: Option<usize>,
        closed: bool,
    }

    impl App for Test {
        fn page(&self) -> Page {
            match self.kind {
                0 => Page::detail(
                    std::iter::once(choice("Mode", "Fast", true, false))
                        .chain((1..self.n).map(|i| pair(format!("Row {i}"), "")))
                        .collect(),
                ),
                1 => Page::log("Log", (0..self.n).map(|i| format!("line {i}")).collect()),
                _ => Page::menu((0..self.n).map(|i| item(format!("Item {i}"))).collect()),
            }
        }
        fn buttons(&self) -> [&str; 5] {
            ["Close", "", "", "", if self.runs > 0 { "Again" } else { "Run" }]
        }
        fn key(&mut self, key: Key, ui: &mut Ui) -> bool {
            match key {
                Key::Run => self.runs += 1,
                Key::Ok => self.picked = Some(ui.selected()),
                _ => return false,
            }
            true
        }
        fn close(&mut self) {
            self.closed = true;
        }
    }

    fn driver(kind: i32, n: usize) -> Driver<Test, Fake> {
        let test = Test { kind, n, runs: 0, picked: None, closed: false };
        let mut d = Driver::new(test, Fake::default());
        d.draw();
        d
    }

    fn seen<R>(d: &Driver<Test, Fake>, f: impl FnOnce(&Seen) -> R) -> R {
        f(&d.screen.0.borrow())
    }

    #[test]
    fn a_soft_key_shows_pressed_then_acts() {
        let mut d = driver(0, 3);
        assert_eq!(d.key(Key::Run, true), Press::Flash);
        assert_eq!(seen(&d, |s| s.slot), 4);
        assert_eq!(d.app.runs, 0, "nothing happens until the flash ends");
        d.release(Key::Run);
        assert_eq!(seen(&d, |s| s.slot), -1);
        assert_eq!(d.app.runs, 1);
        assert_eq!(seen(&d, |s| s.buttons[4].clone()), "Again", "drawn again after the key");
    }

    #[test]
    fn detail_scrolls_and_clamps() {
        let mut d = driver(0, 12);
        for _ in 0..10 {
            d.key(Key::Down, true);
        }
        assert_eq!(seen(&d, |s| s.offset), 4, "12 rows, 8 shown");
        d.key(Key::Up, true);
        assert_eq!(seen(&d, |s| s.offset), 3);
        d.app.n = 9;
        d.draw();
        assert_eq!(seen(&d, |s| s.offset), 1, "rows that go away do not leave it past the end");
        assert_eq!(seen(&d, |s| s.ends), (true, false), "the choice row's ends");
    }

    #[test]
    fn menu_wraps_and_follows_the_cursor() {
        let mut d = driver(2, 7);
        for _ in 0..6 {
            d.key(Key::Down, true);
        }
        assert_eq!(seen(&d, |s| (s.selected, s.scroll)), (6, 2));
        d.key(Key::Down, true);
        assert_eq!(seen(&d, |s| (s.selected, s.scroll)), (0, 0));
        d.key(Key::Up, true);
        assert_eq!(seen(&d, |s| (s.selected, s.scroll)), (6, 2));
    }

    #[test]
    fn ok_on_a_menu_flashes_the_row() {
        let mut d = driver(2, 4);
        d.key(Key::Down, true);
        assert_eq!(d.key(Key::Ok, true), Press::Flash);
        assert!(seen(&d, |s| s.row_pressed));
        assert_eq!(d.app.picked, None);
        d.release(Key::Ok);
        assert!(!seen(&d, |s| s.row_pressed));
        assert_eq!(d.app.picked, Some(1));
    }

    #[test]
    fn log_shows_a_window_of_lines() {
        let mut d = driver(1, 20);
        for _ in 0..30 {
            d.key(Key::Down, true);
        }
        let visible = count::LOG_VISIBLE_LINES as usize;
        assert_eq!(seen(&d, |s| (s.offset, s.total, s.lines.len())), (20 - visible, 20, visible));
        assert_eq!(seen(&d, |s| s.lines[0].clone()), format!("line {}", 20 - visible));
    }

    #[test]
    fn arrows_light_while_held() {
        let mut d = driver(0, 3);
        d.key(Key::Left, true);
        assert_eq!(seen(&d, |s| s.arrow), 1);
        d.key(Key::Left, false);
        assert_eq!(seen(&d, |s| s.arrow), 0);
        d.key(Key::Right, true);
        assert_eq!(seen(&d, |s| s.arrow), 2);
    }

    #[test]
    fn escape_closes_after_the_flash() {
        let mut d = driver(0, 3);
        assert_eq!(d.key(Key::Escape, true), Press::Flash);
        assert!(!d.closed());
        d.release(Key::Escape);
        assert!(d.closed() && d.app.closed);
        assert_eq!(d.key(Key::Run, true), Press::Ignored, "nothing after closing");
    }

    #[test]
    fn each_page_keeps_its_own_place() {
        let mut d = driver(2, 7);
        for _ in 0..3 {
            d.key(Key::Down, true);
        }
        d.app.kind = 0;
        d.draw();
        d.key(Key::Down, true);
        d.app.kind = 2;
        d.draw();
        assert_eq!(seen(&d, |s| s.selected), 3, "the menu's cursor is where it was");
    }
}
