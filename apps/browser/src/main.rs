//! A browser, as a flipctl app: Chromium lays the page out, the panel looks at it
//! through a magnifier.
//!
//! Ported from the prototype's js/apps/browser.js, which pans and zooms over a
//! screenshot. Here the screenshot is live: Chromium runs headless at seven panels'
//! width and sends a picture whenever the page changes (src/cdp.rs), the picture is
//! shrunk to whatever the zoom asks for (src/page.rs), and where it sits and where
//! the cursor is are worked out in src/view.rs.
//!
//! The pad drags the cursor; with PTT held it zooms instead. The arrows move the
//! cursor too, and scroll once it is against the page's edge. Ok clicks under the
//! cursor, and a click that lands in a text field opens the keyboard on it. Back
//! goes back a page or, on the first one, leaves.
//!
//! At the top of the site the strip is across the top of the panel, with the page
//! pushed down under it: the tabs, and the address with back, forward and reload
//! beside it (src/strip.rs). It goes once the view leaves the top, by the page
//! scrolling or by the view panning down it. Pushing past the top moves the
//! highlight up into it; there the arrows and the pad step from item to item and Ok
//! does what it is on, and Down off the address or Back comes back down to the page.
//! Edit is a shortcut to the address.

mod address;
mod cdp;
mod page;
mod strip;
mod view;

use std::cell::RefCell;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use flipctl_app::theme::{metric, radius, PANEL_H, PANEL_W};
use flipctl_app::{font, keyboard, Haptic, Key, StatusSource, Touch, TouchpadSource};
use slint::{ComponentHandle, ModelRc, SharedPixelBuffer, SharedString, VecModel};

use page::Page;
use view::{Buzz, View};

slint::include_modules!();

/// Where the browser opens when it is given nowhere.
const HOME: &str = "https://flipper.net/pages/flipper-one";

/// The engine, as Debian names it.
const CHROMIUM: &str = "chromium";

/// The pill with the zoom factor: its padding either side of the text, and its
/// distance from the panel's top and right edges.
const PILL_PAD_X: i32 = metric::KB_INPUT_PAD;
const PILL_MARGIN: i32 = 2;

/// How often the keyboard is asked whether it moved: the caret blinks and a press
/// flashes, and neither says so on its own.
const KB_TICK: Duration = Duration::from_millis(50);

/// How long the minimap stays up after the view last moved.
const MINI_LINGER: Duration = Duration::from_secs(3);

/// Pad travel per item in the strip, in raw units: a column of the keyboard's is
/// 96 across and a row 180 down, and the strip's items are wider than its keys.
const STRIP_STEP_X: i32 = 96;
const STRIP_STEP_Y: i32 = 180;

/// Something for the thread that draws.
enum Wake {
    /// A tab was opened on the address it carries, and is to go on screen.
    Opened(Result<(cdp::Tab, String), String>),
    Engine(cdp::Event),
    Touch(Touch),
    /// A click landed, and this is the text field it focused, if it focused one.
    Clicked(Option<cdp::Field>),
}

/// What the keyboard is typing.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Typing {
    Address,
    /// The address of a tab that does not exist yet.
    NewTab,
    /// The page's own field, the one that has the focus.
    Field,
}

/// A tab, and what the strip says about it.
struct TabInfo {
    tab: cdp::Tab,
    title: String,
    url: String,
    /// Laid out as a phone rather than as a desktop, which is how a tab starts.
    mobile: bool,
}

struct App {
    page: Option<Page>,
    /// The page's thumbnail for the minimap, made once per frame.
    thumbnail: Option<slint::Image>,
    view: View,
    engine: Option<cdp::Browser>,
    url: String,
    kb: Option<keyboard::TextInput>,
    typing: Typing,
    ptt: bool,
    /// Why the engine is not there, while the dialog saying so is up.
    failed: Option<String>,
    /// When the view last moved, which is what keeps the minimap up.
    moved: Option<Instant>,
    /// The strip, while it is down, and what is highlighted in it.
    strip: Option<strip::Item>,
    /// Where the finger was when it last moved the strip's highlight.
    strip_tp: Option<(i32, i32)>,
    /// Every tab, and which one is on screen.
    tabs: Vec<TabInfo>,
    active: usize,
}

impl App {
    fn new(url: &str) -> Self {
        let (w, h) = cdp::MOBILE;
        Self {
            page: None,
            thumbnail: None,
            view: View::new(view::Size { w: w as f32, h: h as f32 }),
            engine: None,
            url: url.to_string(),
            kb: None,
            typing: Typing::Address,
            ptt: false,
            failed: None,
            moved: None,
            strip: None,
            strip_tp: None,
            tabs: Vec::new(),
            active: 0,
        }
    }

    /// A frame arrived. The view keeps its place on the page unless the page
    /// changed shape.
    fn frame(&mut self, page: Page) {
        match self.page.as_ref().map(Page::size) {
            Some(from) if from != page.size() => self.view.resize(from, page.size()),
            _ => {}
        }
        let h = (page.h as i32 * view::MINI_W / page.w as i32).max(1) as usize;
        let small = page.thumbnail(view::MINI_W as usize, h);
        self.thumbnail = Some(grey_image(&small, view::MINI_W as u32, h as u32));
        self.page = Some(page);
    }

    /// The view moved: the minimap comes up, and stays for a while.
    fn moved(&mut self) {
        self.moved = Some(Instant::now());
    }

    /// Whether the minimap is up: while PTT is held, since that is zooming, and for
    /// a while after the view last moved.
    fn minimap_up(&self) -> bool {
        self.ptt || self.moved.is_some_and(|at| at.elapsed() < MINI_LINGER)
    }

    /// How long until the minimap goes down, while it is up and nothing holds it.
    fn minimap_left(&self) -> Option<Duration> {
        if self.ptt {
            return None;
        }
        let left = MINI_LINGER.checked_sub(self.moved?.elapsed())?;
        (!left.is_zero()).then_some(left)
    }

    fn type_address(&mut self) {
        self.kb = Some(keyboard::TextInput::new("Address", &self.url));
        self.typing = Typing::Address;
    }

    /// The keyboard, on a field of the page. Its title is what the page calls the
    /// field, in letters the panel can draw.
    fn type_into(&mut self, field: &cdp::Field) {
        let label =
            font::fit(&font::ascii(&field.label), PANEL_W as i32 - 2 * metric::KB_INPUT_PAD);
        let title = if label.is_empty() { "Text".to_string() } else { label };
        self.kb = Some(keyboard::TextInput::new(&title, &font::ascii(&field.value)));
        self.typing = Typing::Field;
    }

    /// A tab opened: it goes on screen, and the one that was there goes behind it.
    fn opened(&mut self, tab: cdp::Tab, url: String) {
        if let Some(engine) = self.engine.as_ref() {
            engine.switch(self.tabs.get(self.active).map(|t| &t.tab), &tab);
        }
        self.tabs.push(TabInfo { tab, title: String::new(), url: url.clone(), mobile: true });
        self.active = self.tabs.len() - 1;
        self.shown(url);
    }

    /// Another tab is on screen: the picture is the old tab's until the new one
    /// sends its own, so there is none until then.
    fn shown(&mut self, url: String) {
        self.url = url;
        self.page = None;
        self.thumbnail = None;
    }

    fn switch_to(&mut self, i: usize) {
        if i == self.active || i >= self.tabs.len() {
            return;
        }
        if let Some(engine) = self.engine.as_ref() {
            engine.switch(Some(&self.tabs[self.active].tab), &self.tabs[i].tab);
        }
        self.active = i;
        self.shown(self.tabs[i].url.clone());
    }

    /// Close a tab. The last one is not closed but sent home, since a browser with
    /// no tab has nothing to show.
    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            if let Some(engine) = self.engine.as_ref() {
                engine.navigate(HOME);
            }
            self.strip = None;
            return;
        }
        let was_on_screen = i == self.active;
        let closed = self.tabs.remove(i);
        if let Some(engine) = self.engine.as_ref() {
            engine.close(&closed.tab);
        }
        // A tab before the one on screen moves it left; so does closing the one on
        // screen when it was the last, since the one after it is gone too.
        if i < self.active || self.active == self.tabs.len() {
            self.active -= 1;
        }
        if was_on_screen {
            if let Some(engine) = self.engine.as_ref() {
                engine.switch(None, &self.tabs[self.active].tab);
            }
            self.shown(self.tabs[self.active].url.clone());
        }
        self.strip = Some(strip::Item::Tab(self.active));
    }

    /// Ok on something in the strip.
    fn activate(&mut self, item: strip::Item) {
        use strip::Item;
        match item {
            Item::Tab(i) => {
                self.switch_to(i);
                self.strip = None;
            }
            Item::Close(i) => self.close_tab(i),
            Item::NewTab => {
                self.strip = None;
                self.kb = Some(keyboard::TextInput::new("New tab", ""));
                self.typing = Typing::NewTab;
            }
            Item::Back => {
                if let Some(engine) = self.engine.as_ref() {
                    engine.back();
                }
            }
            Item::Forward => {
                if let Some(engine) = self.engine.as_ref() {
                    engine.forward();
                }
            }
            Item::Reload => {
                if let Some(engine) = self.engine.as_ref() {
                    engine.reload();
                }
            }
            Item::Mobile => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.mobile = !tab.mobile;
                    if let Some(engine) = self.engine.as_ref() {
                        engine.set_mode(&tab.tab, tab.mobile);
                    }
                }
            }
            Item::Address => {
                self.strip = None;
                self.type_address();
            }
        }
    }

    /// The strip's highlight, moved. Off the bottom is back onto the page.
    /// Returns whether it went anywhere.
    fn strip_step(&mut self, dir: (i32, i32)) -> bool {
        let Some(hot) = self.strip else {
            return false;
        };
        self.strip = strip::step(self.tabs.len(), self.active, hot, dir);
        self.strip != Some(hot)
    }

    /// A pad report while the strip is down: a stroke steps the highlight an item
    /// per so many units, measured from where it last stepped. Returns whether the
    /// highlight moved.
    fn strip_touch(&mut self, t: Touch) -> bool {
        if !t.down {
            self.strip_tp = None;
            return false;
        }
        let Some((ax, ay)) = self.strip_tp else {
            self.strip_tp = Some((t.x, t.y));
            return false;
        };
        let (dx, dy) = (t.x - ax, t.y - ay);
        let dir = if dx.abs() >= STRIP_STEP_X {
            (dx.signum(), 0)
        } else if dy.abs() >= STRIP_STEP_Y {
            (0, dy.signum())
        } else {
            return false;
        };
        self.strip_tp = Some((t.x, t.y));
        self.strip_step(dir)
    }

    fn open_strip(&mut self) {
        self.strip = Some(strip::Item::Address);
        self.strip_tp = None;
    }

    /// How far the strip pushes the page down: all of it at the top of the site,
    /// which is the page scrolled to its top with the view showing that top, and
    /// while the highlight is up there; nothing otherwise, and never over the
    /// dialog that says the engine is gone, since there is nothing left for it to
    /// drive. Before the first picture there is no page to be anywhere on, and the
    /// strip is up so the address is there.
    fn bar(&self) -> i32 {
        let top = self.page.as_ref().is_none_or(|p| p.at_top && self.view.shows_top(p.size()));
        if self.failed.is_none() && (self.strip.is_some() || top) {
            view::TOP
        } else {
            0
        }
    }

    /// The view as it is drawn, pushed down under the strip while that is up.
    fn drawn(&self) -> View {
        let mut view = self.view.clone();
        view.bar = self.bar();
        view
    }

    /// The cursor's page point, as the engine counts pixels.
    fn target(&self) -> Option<(f32, f32)> {
        let page = self.page.as_ref()?;
        Some((self.view.pan.0 * page.css.0, self.view.pan.1 * page.css.1))
    }

    fn dialog(&self) -> Vec<SharedString> {
        let Some(why) = self.failed.as_ref() else {
            return Vec::new();
        };
        vec![
            "Browser error".into(),
            font::fit(&font::ascii(why), metric::MODAL_W - radius::BOX * 2).into(),
        ]
    }
}

fn grey_image(grey: &[u8], w: u32, h: u32) -> slint::Image {
    let mut buffer = SharedPixelBuffer::<slint::Rgb8Pixel>::new(w, h);
    for (px, &g) in buffer.make_mut_slice().iter_mut().zip(grey) {
        *px = slint::Rgb8Pixel { r: g, g, b: g };
    }
    slint::Image::from_rgb8(buffer)
}

fn marks(marks: impl IntoIterator<Item = view::Mark>) -> ModelRc<Mark> {
    let marks: Vec<Mark> = marks
        .into_iter()
        .map(|m| Mark {
            x: m.x as f32,
            y: m.y as f32,
            w: m.w as f32,
            h: m.h as f32,
            black: m.black,
        })
        .collect();
    ModelRc::new(VecModel::from(marks))
}

/// Put the state on screen.
fn apply(ui: &AppWindow, app: &App) {
    ui.set_dialog(ModelRc::new(VecModel::from(app.dialog())));
    ui.set_waiting(if app.failed.is_some() { "".into() } else { "Loading...".into() });
    apply_strip(ui, app);

    let Some(page) = app.page.as_ref() else {
        ui.set_has_page(false);
        ui.set_mini_shown(false);
        ui.set_under(marks([]));
        ui.set_over(marks([]));
        ui.set_pill("".into());
        apply_kb(ui, app);
        return;
    };
    let size = page.size();
    let view = app.drawn();
    let (w, h) = (usize::from(PANEL_W), usize::from(PANEL_H));
    ui.set_page(grey_image(&page.render(view.place(size), w, h), w as u32, h as u32));
    ui.set_has_page(true);

    let mini = view.minimap(size).filter(|_| app.minimap_up());
    let mut over = Vec::new();
    match (&mini, &app.thumbnail) {
        (Some(m), Some(thumbnail)) => {
            ui.set_mini(thumbnail.clone());
            ui.set_mini_x(m.x as f32);
            ui.set_mini_y(m.y as f32);
            ui.set_mini_w(m.w as f32);
            ui.set_mini_h(m.h as f32);
            ui.set_mini_shown(true);
            ui.set_under(marks(m.border));
            over.extend(m.frame.iter().copied());
        }
        _ => {
            ui.set_mini_shown(false);
            ui.set_under(marks([]));
        }
    }
    // The cursor is on the page, and while the strip is down the highlight is what
    // Ok acts on, so only one of them is drawn.
    if app.strip.is_none() {
        over.extend(view.cursor_marks(size));
    }
    ui.set_over(marks(over));

    if app.ptt {
        let label = app.view.label();
        let w = font::tw(&label) + 2 * PILL_PAD_X;
        ui.set_pill(label.into());
        ui.set_pill_w(w as f32);
        ui.set_pill_x((PANEL_W as i32 - PILL_MARGIN - w) as f32);
        ui.set_pill_y((view.bar + PILL_MARGIN) as f32);
    } else {
        ui.set_pill("".into());
    }
    apply_kb(ui, app);
}

/// The strip, and the highlight in it while the highlight is up there.
fn apply_strip(ui: &AppWindow, app: &App) {
    ui.set_strip_shown(app.bar() > 0);
    let hot = app.strip;
    let tabs: Vec<strip::Tab<'_>> = app
        .tabs
        .iter()
        .map(|t| strip::Tab {
            title: if t.title.is_empty() { t.url.as_str() } else { t.title.as_str() },
        })
        .collect();
    let mobile = app.tabs.get(app.active).is_none_or(|t| t.mobile);
    let v = strip::view(&tabs, app.active, &app.url, mobile, hot);
    let placed: Vec<StripTab> = v
        .tabs
        .iter()
        .map(|t| StripTab {
            x: t.x as f32,
            w: t.w as f32,
            title: t.title.as_str().into(),
            active: t.active,
            hot: t.hot,
        })
        .collect();
    ui.set_strip_tabs(ModelRc::new(VecModel::from(placed)));
    ui.set_strip_tab_y(strip::TAB_Y as f32);
    ui.set_strip_tab_h(strip::TAB_H as f32);
    ui.set_strip_addr_y(strip::ADDR_Y as f32);
    ui.set_strip_addr_h(strip::ADDR_H as f32);
    ui.set_strip_field_x(v.field_x as f32);
    ui.set_strip_field_w(v.field_w as f32);
    ui.set_strip_field_h(strip::FIELD_H as f32);
    ui.set_strip_address(v.address.as_str().into());
    ui.set_strip_address_hot(v.address_hot);
    ui.set_strip_marks(marks(v.marks));
    ui.set_strip_rule_y(strip::RULE_Y as f32);
}

/// The keyboard's half of the screen, which is all a caret blink changes.
fn apply_kb(ui: &AppWindow, app: &App) {
    ui.set_keyboard(app.kb.is_some());
    let Some(field) = app.kb.as_ref() else {
        return;
    };
    let v = field.view("", field.cursor_visible());
    ui.set_kb_title(v.title.as_str().into());
    ui.set_kb_text(v.text.as_str().into());
    ui.set_kb_field_w(v.field_w);
    ui.set_kb_cursor_dx(v.cursor_dx);
    ui.set_kb_cursor_on(v.cursor_on);
    ui.set_kb_field_focused(v.field_focused);
    ui.set_kb_warning(v.warning.as_str().into());
    let cells: Vec<KbCell> = v
        .cells
        .iter()
        .map(|c| KbCell {
            x: c.x as f32,
            y: c.y as f32,
            w: c.w as f32,
            text: c.text.as_str().into(),
            icon: c.icon,
            icon_w: c.icon_w as f32,
            icon_h: c.icon_h as f32,
            selected: c.selected,
            pressed: c.pressed,
            clip_h: c.clip_h as f32,
        })
        .collect();
    ui.set_kb_cells(ModelRc::new(VecModel::from(cells)));
    ui.set_kb_chrome_x(v.chrome.0);
    ui.set_kb_chrome_y(v.chrome.1);
    ui.set_kb_chrome_w(v.chrome.2);
    ui.set_kb_chrome_h(v.chrome.3);
    ui.set_kb_lang_label(v.lang_label.into());
    ui.set_kb_tab_label(v.tab_label.into());
    ui.set_kb_tab_focus(v.tab_focus);
    ui.set_kb_tab_pressed(v.tab_pressed);
    ui.set_kb_discard(v.discard);
    let buttons: Vec<SharedString> =
        ["Cancel", "", "", "", "Done"].iter().map(|s| (*s).into()).collect();
    ui.set_kb_buttons(ModelRc::new(VecModel::from(buttons)));
}

/// What a key asked for beyond a redraw. The ones that wait on the engine are
/// done on a thread of their own, since a page answers when it likes.
#[derive(PartialEq, Debug)]
enum Then {
    Stay,
    Quit,
    /// A new tab, on this address.
    OpenTab(String),
    /// Click here, in CSS pixels, and say whether a text field took the focus.
    Click((f32, f32)),
    /// Put this text in the field that has the focus.
    Fill(String),
}

fn key(app: &mut App, key: Key, down: bool) -> Then {
    if app.failed.is_some() {
        return if down && matches!(key, Key::Ok | Key::Run | Key::Back | Key::Escape) {
            Then::Quit
        } else {
            Then::Stay
        };
    }

    if let Some(field) = app.kb.as_mut() {
        if !down {
            field.release(key.flipper());
            return Then::Stay;
        }
        return match field.key(key.flipper(), true) {
            Some(keyboard::Exit::Save(text)) => {
                let changed = field.changed();
                app.kb = None;
                // Unchanged text is left alone: what the panel showed was the
                // field's value in letters it can draw, which is not always the value.
                match app.typing {
                    Typing::Field => {
                        return if changed { Then::Fill(text) } else { Then::Stay };
                    }
                    Typing::NewTab => {
                        return address::resolve(&text).map_or(Then::Stay, Then::OpenTab);
                    }
                    Typing::Address => {}
                }
                if let (Some(url), Some(engine)) = (address::resolve(&text), app.engine.as_ref()) {
                    engine.navigate(&url);
                    app.url = url;
                }
                Then::Stay
            }
            Some(keyboard::Exit::Cancel) => {
                app.kb = None;
                Then::Stay
            }
            None => Then::Stay,
        };
    }

    if let Some(hot) = app.strip {
        if !down {
            return Then::Stay;
        }
        match key {
            Key::Up => {
                app.strip_step((0, -1));
            }
            Key::Down => {
                app.strip_step((0, 1));
            }
            Key::Left => {
                app.strip_step((-1, 0));
            }
            Key::Right => {
                app.strip_step((1, 0));
            }
            Key::Ok => app.activate(hot),
            Key::Back => app.strip = None,
            Key::Edit => app.activate(strip::Item::Address),
            Key::Escape => return Then::Quit,
            _ => {}
        }
        return Then::Stay;
    }

    if key == Key::Ptt {
        app.ptt = down;
        app.view.ptt();
        app.moved();
        return Then::Stay;
    }
    if !down {
        return Then::Stay;
    }
    let size = app.page.as_ref().map(Page::size);
    match key {
        Key::Edit => app.type_address(),
        Key::Escape => return Then::Quit,
        Key::Back => {
            if !app.engine.as_ref().is_some_and(cdp::Browser::back) {
                return Then::Quit;
            }
        }
        Key::Ok => {
            if let (Some(_), Some(at)) = (app.engine.as_ref(), app.target()) {
                return Then::Click(at);
            }
        }
        // The arrows move the cursor, and scroll only once it is against the edge.
        Key::Up | Key::Down | Key::Left | Key::Right => {
            let Some(size) = size else {
                return Then::Stay;
            };
            let dir = match key {
                Key::Up => (0, -1),
                Key::Down => (0, 1),
                Key::Left => (-1, 0),
                _ => (1, 0),
            };
            let at_top = app.bar() > 0;
            app.view.bar = app.bar();
            let scroll = app.view.nudge(dir, size);
            app.moved();
            // Up against the top of the site has nowhere to scroll, and goes up into
            // the strip instead.
            if dir == (0, -1) && scroll.is_some() && at_top {
                app.open_strip();
                return Then::Stay;
            }
            if let (Some(engine), Some(at), Some(page)) =
                (app.engine.as_ref(), app.target(), app.page.as_ref())
            {
                match scroll {
                    Some(by) => engine.wheel(at, (by.0 * page.css.0, by.1 * page.css.1)),
                    None => engine.hover(at),
                }
            }
        }
        _ => {}
    }
    Then::Stay
}

/// The pad, on a thread of its own that sleeps until the finger moves.
fn read_pad(tx: Sender<Wake>, wake: impl Fn()) {
    let Ok(mut pad) = TouchpadSource::open() else {
        return;
    };
    loop {
        let mut fd = libc::pollfd { fd: pad.fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut fd, 1, -1) } < 0 {
            continue;
        }
        if fd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return;
        }
        while let Some(t) = pad.poll() {
            if tx.send(Wake::Touch(t)).is_err() {
                return;
            }
        }
        wake();
    }
}

/// Everything the other threads queued. Frames that arrived together are one
/// frame: only the newest is worth shrinking.
fn drain(app: &mut App, rx: &Receiver<Wake>, buzz: &mut Option<Haptic>) {
    let mut newest = None;
    let mut hover = false;
    while let Ok(wake) = rx.try_recv() {
        match wake {
            Wake::Opened(Ok((tab, url))) => app.opened(tab, url),
            // The first tab not opening is the browser not starting; a later one is
            // only that tab, and the one on screen is still there.
            Wake::Opened(Err(why)) if app.tabs.is_empty() => app.failed = Some(why),
            Wake::Opened(Err(why)) => eprintln!("browser: new tab: {why}"),
            Wake::Engine(cdp::Event::Frame(page)) => newest = Some(page),
            Wake::Engine(cdp::Event::Url { session, url }) => {
                let active = app.tabs.get(app.active).map(|t| t.tab.session.clone());
                if active.as_deref() == Some(session.as_str()) {
                    app.url = url.clone();
                }
                if let Some(tab) = app.tabs.iter_mut().find(|t| t.tab.session == session) {
                    tab.url = url;
                }
            }
            Wake::Engine(cdp::Event::Title { session, title }) => {
                if let Some(tab) = app.tabs.iter_mut().find(|t| t.tab.session == session) {
                    tab.title = title;
                }
            }
            // Only onto an empty panel: Edit may have opened the keyboard meanwhile.
            Wake::Clicked(Some(field)) if app.kb.is_none() && app.failed.is_none() => {
                app.type_into(&field);
            }
            Wake::Clicked(_) => {}
            Wake::Engine(cdp::Event::Gone(why)) => {
                app.engine = None;
                app.failed = Some(why);
            }
            Wake::Touch(t) => {
                let tick = if let Some(field) = app.kb.as_mut() {
                    if field.touch(t) {
                        Buzz::Tick
                    } else {
                        Buzz::None
                    }
                } else if app.strip.is_some() {
                    if app.strip_touch(t) {
                        Buzz::Tick
                    } else {
                        Buzz::None
                    }
                } else if let Some(size) = app.page.as_ref().map(Page::size) {
                    let (moved, said) = app.view.touch(t, app.ptt, size);
                    if moved {
                        app.moved();
                    }
                    if app.view.take_pushed_up() && app.bar() > 0 {
                        app.open_strip();
                    }
                    hover |= moved && !app.ptt;
                    said
                } else {
                    Buzz::None
                };
                if let Some(motor) = buzz.as_mut() {
                    match tick {
                        Buzz::Tick => motor.play(3, 10),
                        Buzz::Thump => motor.play(3, 0),
                        Buzz::None => {}
                    }
                }
            }
        }
    }
    if let Some(page) = newest {
        app.frame(page);
    }
    if hover {
        if let (Some(engine), Some(at)) = (app.engine.as_ref(), app.target()) {
            engine.hover(at);
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let url = std::env::args().nth(1).unwrap_or_else(|| HOME.to_string());
    let ui = AppWindow::new()?;
    let state = Rc::new(RefCell::new(App::new(&url)));
    apply(&ui, &state.borrow());

    let (tx, rx) = mpsc::channel();
    let weak = ui.as_weak();
    let waker = move || {
        let _ = weak.upgrade_in_event_loop(|ui| ui.invoke_wake());
    };

    // Forked here, on the thread that lives as long as the app, since the engine
    // dies with the thread that started it. The page takes seconds to open, so that
    // part goes to a thread of its own and the panel says Loading until the first
    // picture.
    {
        // The app's own writable directory, which is where flipctl starts it: the
        // profile keeps cookies between runs and never meets a desktop Chromium's.
        let profile = std::env::current_dir().unwrap_or_default().join("chromium");
        let events = tx.clone();
        let told = waker.clone();
        let launched = cdp::Browser::launch(CHROMIUM, &profile, move |event| {
            let _ = events.send(Wake::Engine(event));
            told();
        });
        match launched {
            Ok(engine) => {
                let remote = engine.remote();
                state.borrow_mut().engine = Some(engine);
                let tx = tx.clone();
                let waker = waker.clone();
                std::thread::Builder::new()
                    .name("open".into())
                    .spawn(move || {
                        let opened = remote.open(&url, true).map(|tab| (tab, url));
                        let _ = tx.send(Wake::Opened(opened));
                        waker();
                    })
                    .expect("spawn open");
            }
            Err(why) => state.borrow_mut().failed = Some(why),
        }
        apply(&ui, &state.borrow());
    }
    {
        let tx = tx.clone();
        let waker = waker.clone();
        std::thread::Builder::new()
            .name("pad".into())
            .spawn(move || read_pad(tx, waker))
            .expect("spawn pad");
    }
    let asks = tx;
    let asked = waker;

    // The timers, called after every event. The minimap's goes off once when it is
    // due to go down, and is pushed back by every move before then. The keyboard's
    // ticks while it is up: the caret and a press flash change on their own, and the
    // keyboard screen is the one place the status bar shows.
    let status = Rc::new(RefCell::new(StatusSource::new(Duration::from_secs(2))));
    let timer = Rc::new(slint::Timer::default());
    let hide = slint::Timer::default();
    let tick = {
        let state = Rc::clone(&state);
        Rc::new(move |ui: &AppWindow| {
            if let Some(left) = state.borrow().minimap_left() {
                let weak = ui.as_weak();
                let state = Rc::clone(&state);
                hide.start(slint::TimerMode::SingleShot, left, move || {
                    if let (Some(ui), Ok(app)) = (weak.upgrade(), state.try_borrow()) {
                        apply(&ui, &app);
                    }
                });
            }
            let up = state.borrow().kb.is_some();
            if !up {
                timer.stop();
                return;
            }
            if timer.running() {
                return;
            }
            flipctl_app::apply_status!(ui, PanelStatus, status.borrow_mut().current());
            let weak = ui.as_weak();
            let state = Rc::clone(&state);
            let status = Rc::clone(&status);
            timer.start(slint::TimerMode::Repeated, KB_TICK, move || {
                let (Some(ui), Ok(mut app)) = (weak.upgrade(), state.try_borrow_mut()) else {
                    return;
                };
                if let Some(field) = app.kb.as_mut() {
                    field.animating();
                }
                apply_kb(&ui, &app);
                if let Some(now) = status.borrow_mut().poll() {
                    flipctl_app::apply_status!(&ui, PanelStatus, now);
                }
            });
        })
    };

    let mut buzz = Haptic::open().ok();
    let woken = ui.as_weak();
    let drained = Rc::clone(&state);
    let tick_woken = Rc::clone(&tick);
    ui.on_wake(move || {
        let Some(ui) = woken.upgrade() else {
            return;
        };
        let mut app = drained.borrow_mut();
        drain(&mut app, &rx, &mut buzz);
        apply(&ui, &app);
        drop(app);
        tick_woken(&ui);
    });

    let keys = ui.as_weak();
    let keyed = Rc::clone(&state);
    let ticked = Rc::clone(&tick);
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
        let mut app = keyed.borrow_mut();
        let then = key(&mut app, pressed, down);
        let remote = app.engine.as_ref().map(cdp::Browser::remote);
        match (then, remote) {
            (Then::Quit, _) => {
                let _ = slint::quit_event_loop();
                return;
            }
            (Then::Click(at), Some(remote)) => {
                let tx = asks.clone();
                let waker = asked.clone();
                let _ = std::thread::Builder::new().name("click".into()).spawn(move || {
                    let field = remote.click(at).and_then(|()| remote.focused());
                    let field = field.unwrap_or_else(|why| {
                        eprintln!("browser: click: {why}");
                        None
                    });
                    let _ = tx.send(Wake::Clicked(field));
                    waker();
                });
            }
            (Then::OpenTab(url), Some(remote)) => {
                let tx = asks.clone();
                let waker = asked.clone();
                let _ = std::thread::Builder::new().name("tab".into()).spawn(move || {
                    let opened = remote.open(&url, true).map(|tab| (tab, url));
                    let _ = tx.send(Wake::Opened(opened));
                    waker();
                });
            }
            (Then::Fill(text), Some(remote)) => {
                let _ = std::thread::Builder::new().name("fill".into()).spawn(move || match remote
                    .fill(&text)
                {
                    Ok(true) => {}
                    Ok(false) => eprintln!("browser: fill: the field lost the focus"),
                    Err(why) => eprintln!("browser: fill: {why}"),
                });
            }
            _ => {}
        }
        apply(&ui, &app);
        drop(app);
        ticked(&ui);
    });

    ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flipctl_app::theme;
    use flipper_ui::slint_render::{render_frame, FlipperSlintPlatform};
    use slint::platform::software_renderer::MinimalSoftwareWindow;

    fn panel() -> Rc<MinimalSoftwareWindow> {
        thread_local! {
            static WINDOW: Rc<MinimalSoftwareWindow> = FlipperSlintPlatform::install();
        }
        WINDOW.with(Rc::clone)
    }

    /// The screen as the panel would receive it. `BROWSER_RENDER=1 cargo test` leaves
    /// the frames in target/render to be looked at.
    fn shot(name: &str, app: &App) -> Vec<u8> {
        let window = panel();
        let ui = AppWindow::new().expect("create AppWindow");
        apply(&ui, app);
        ui.show().expect("show");
        slint::platform::update_timers_and_animations();
        let frame: Vec<u8> = render_frame(&window).expect("frame").iter().map(|px| px.0).collect();
        drop(ui);
        if std::env::var_os("BROWSER_RENDER").is_some() {
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
        encoder.write_header().expect("png header").write_image_data(frame).expect("png data");
    }

    fn off_palette(frame: &[u8]) -> Vec<String> {
        let allowed: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
        let stride = usize::from(theme::PANEL_W);
        frame
            .iter()
            .enumerate()
            .filter(|(_, px)| !allowed.contains(px))
            .take(8)
            .map(|(i, px)| format!("({}, {}) = {px}", i % stride, i / stride))
            .collect()
    }

    /// A page of our own: white, with black blocks standing in for text, so the
    /// shrinking shows as greys and the chrome over it as tokens.
    fn app_with_page(blocks: bool) -> App {
        let (w, h) = cdp::DESKTOP;
        let grey: Vec<u8> = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| {
                let text = blocks && (y / 24) % 2 == 1 && x % 160 < 130 && x > 100 && x < 1700;
                if text && (x / 3 + y / 3) % 3 != 0 {
                    0
                } else {
                    0xff
                }
            })
            .collect();
        let mut app = App::new(HOME);
        app.frame(Page::from_luma(w, h, &grey, (1.0, 1.0)).expect("page"));
        app
    }

    #[test]
    fn before_the_first_picture_the_panel_says_so() {
        let app = App::new(HOME);
        let frame = shot("loading", &app);
        assert!(off_palette(&frame).is_empty(), "{:?}", off_palette(&frame));
        assert!(frame.contains(&theme::color::BLACK.0), "no text");
    }

    /// Over a white page every pixel is a token: the cursor, the minimap's border
    /// and frame, and the pill. The page's own greys are the page's.
    #[test]
    fn the_chrome_is_drawn_only_in_tokens() {
        let mut app = app_with_page(false);
        app.view.zoom = 2.0;
        app.ptt = true;
        let frame = shot("chrome", &app);
        assert!(off_palette(&frame).is_empty(), "{:?}", off_palette(&frame));
        let at = |x: usize, y: usize| frame[y * usize::from(theme::PANEL_W) + x];
        // The minimap's border along its top, near the right edge.
        let m = app.view.minimap(app.page.as_ref().unwrap().size()).unwrap();
        let right = usize::from(theme::PANEL_W) - 10;
        assert_eq!(at(right, (m.y - 2) as usize), theme::color::BLACK.0);

        // On a black page the white outside the black is what shows its edge.
        let (w, h) = cdp::DESKTOP;
        let mut dark = App::new(HOME);
        dark.frame(Page::from_luma(w, h, &vec![0; (w * h) as usize], (1.0, 1.0)).expect("page"));
        dark.view.zoom = 2.0;
        dark.moved();
        let frame = shot("mini-dark", &dark);
        let at = |x: usize, y: usize| frame[y * usize::from(theme::PANEL_W) + x];
        assert_eq!(at(right, (m.y - 3) as usize), theme::color::WHITE.0, "no white outline");
        assert_eq!(at(right, (m.y - 2) as usize), theme::color::BLACK.0);
        assert_eq!(at(right, (m.y - 1) as usize), theme::color::WHITE.0, "no gap before the map");
        // The pill, in the top-right corner of the page, under the strip if it is up.
        let pill_y = (app.bar() + 8) as usize;
        assert_eq!(at(usize::from(theme::PANEL_W) - 6, pill_y), theme::color::BLACK.0);
    }

    #[test]
    fn the_page_fitted_and_magnified() {
        let mut app = app_with_page(true);
        let fitted = shot("fitted", &app);
        assert!(fitted.iter().any(|&g| g != 0 && g != 0xff), "shrinking made no greys");
        app.view.zoom = 7.0;
        app.view.pan = (300.0, 200.0);
        app.ptt = true;
        shot("x7", &app);
    }

    #[test]
    fn the_address_keyboard() {
        let mut app = app_with_page(true);
        assert_eq!(key(&mut app, Key::Edit, true), Then::Stay);
        let field = app.kb.as_ref().expect("keyboard");
        assert_eq!(field.text, HOME);
        let frame = shot("keyboard", &app);
        assert!(off_palette(&frame).is_empty(), "{:?}", off_palette(&frame));
        // Back on an unchanged field leaves it.
        assert_eq!(key(&mut app, Key::Back, true), Then::Stay);
        assert!(app.kb.is_none());
    }

    /// A click that lands in a field opens the keyboard on it, titled with what the
    /// page calls it, and Done hands the text back for the field rather than going
    /// anywhere with it.
    #[test]
    fn a_page_field_opens_the_keyboard_and_done_fills_it() {
        let mut app = app_with_page(false);
        app.type_into(&cdp::Field { value: "flipper".into(), label: "Search Wikipedia".into() });
        let field = app.kb.as_ref().expect("keyboard");
        assert_eq!((field.title.as_str(), field.text.as_str()), ("Search Wikipedia", "flipper"));
        shot("field", &app);
        assert_eq!(key(&mut app, Key::Run, true), Then::Stay, "Done on unchanged text");
        let mut app = app_with_page(false);
        app.type_into(&cdp::Field { value: String::new(), label: String::new() });
        assert_eq!(app.kb.as_ref().unwrap().title, "Text");
        app.kb.as_mut().unwrap().text = "one".into();
        assert_eq!(key(&mut app, Key::Run, true), Then::Fill("one".into()));
        assert!(app.kb.is_none());
        assert_eq!(app.url, HOME, "a field's text was taken for an address");
    }

    /// The strip, over a page: three tabs with the second on screen, and the cursor
    /// on each kind of thing in turn. For looking at, and every pixel of it is a
    /// token.
    #[test]
    fn the_strip_is_drawn_only_in_tokens() {
        let three = || tabs(&["Wikipedia, the free encyclopedia", "Flipper One", "DuckDuckGo"]);
        let mut app = app_with_page(false);
        app.tabs = three();
        app.active = 1;
        app.url = "https://docs.flipper.net/one/getting-started".into();
        for (name, hot) in [
            ("strip-address", strip::Item::Address),
            ("strip-tab", strip::Item::Tab(2)),
            ("strip-close", strip::Item::Close(1)),
            ("strip-back", strip::Item::Back),
            ("strip-new", strip::Item::NewTab),
        ] {
            app.strip = Some(hot);
            let frame = shot(name, &app);
            assert!(off_palette(&frame).is_empty(), "{name}: {:?}", off_palette(&frame));
        }
        let mut real = app_with_page(true);
        real.tabs = three();
        real.active = 1;
        real.url = app.url.clone();
        real.strip = Some(strip::Item::Address);
        shot("strip-over-page", &real);
    }

    fn tabs(titles: &[&str]) -> Vec<TabInfo> {
        titles
            .iter()
            .enumerate()
            .map(|(i, title)| TabInfo {
                tab: cdp::Tab { target: format!("t{i}"), session: format!("s{i}") },
                title: title.to_string(),
                url: format!("https://example.com/{i}"),
                mobile: true,
            })
            .collect()
    }

    /// Up with the cursor on the top of a page at its top brings the strip down on
    /// the address; a page scrolled down scrolls instead.
    #[test]
    fn pushing_up_at_the_top_brings_the_strip_down() {
        let mut app = app_with_page(false);
        app.tabs = tabs(&["One"]);
        app.view.pan.1 = 0.0;
        app.page.as_mut().unwrap().at_top = false;
        key(&mut app, Key::Up, true);
        assert_eq!(app.strip, None, "a page with more above it scrolled");
        app.page.as_mut().unwrap().at_top = true;
        key(&mut app, Key::Up, true);
        assert_eq!(app.strip, Some(strip::Item::Address));
        // The cursor is not drawn under it: only the highlight says what Ok does.
        shot("strip-open", &app);
        // Looked for in the middle of the page, clear of the strip.
        app.view.pan = (896.0, 504.0);
        let frame = shot("strip-open-mid", &app);
        let (cx, cy) = app.view.cursor(app.page.as_ref().unwrap().size());
        let stride = usize::from(theme::PANEL_W);
        let arm = frame[cy as usize * stride + cx as usize + 3];
        assert_eq!(arm, 0xff, "the crosshair is drawn under the strip");
        key(&mut app, Key::Left, true);
        assert_eq!(app.strip, Some(strip::Item::Mobile));
        key(&mut app, Key::Back, true);
        assert_eq!(app.strip, None, "Back put it away");
        app.view.pan.1 = 0.0;
        key(&mut app, Key::Up, true);
        key(&mut app, Key::Down, true);
        assert_eq!(app.strip, None, "Down off the address is the page");
    }

    /// The pad steps the strip's highlight an item per stroke length, and a stroke
    /// down off the address is back onto the page.
    #[test]
    fn the_pad_steps_the_strip() {
        let mut app = app_with_page(false);
        app.tabs = tabs(&["One", "Two"]);
        app.open_strip();
        let t = |x, y| Touch { x, y, down: true };
        assert!(!app.strip_touch(t(500, 500)));
        assert!(!app.strip_touch(t(450, 500)), "stepped before a whole item");
        assert!(app.strip_touch(t(400, 500)));
        assert_eq!(app.strip, Some(strip::Item::Mobile));
        assert!(app.strip_touch(t(400, 200)));
        assert!(matches!(app.strip, Some(strip::Item::Tab(_) | strip::Item::Close(_))));
        app.strip_touch(Touch { x: 400, y: 200, down: false });
        app.strip = Some(strip::Item::Address);
        app.strip_touch(t(0, 0));
        app.strip_touch(t(0, 200));
        assert_eq!(app.strip, None);
    }

    /// A tab starts as a phone, the button beside reload shows it, and Ok on the
    /// button makes it a desktop and back. Drawn both ways for looking at.
    #[test]
    fn the_mode_button_switches_a_tab_between_phone_and_desktop() {
        let mut app = app_with_page(false);
        app.tabs = tabs(&["One"]);
        assert!(app.tabs[0].mobile, "a tab starts as a desktop");
        app.strip = Some(strip::Item::Mobile);
        let phone = shot("strip-mode-phone", &app);
        assert!(off_palette(&phone).is_empty(), "{:?}", off_palette(&phone));
        key(&mut app, Key::Ok, true);
        assert!(!app.tabs[0].mobile);
        assert_eq!(app.strip, Some(strip::Item::Mobile), "the strip went away");
        app.strip = None;
        let monitor = shot("strip-mode-monitor", &app);
        assert_ne!(phone, monitor, "the button looks the same either way");
        app.strip = Some(strip::Item::Mobile);
        key(&mut app, Key::Ok, true);
        assert!(app.tabs[0].mobile);
    }

    /// Tabs: Ok on one puts it on screen, the cross closes one, + asks for an
    /// address and hands it over for a tab of its own, and the last tab is sent
    /// home rather than closed.
    #[test]
    fn the_strip_switches_opens_and_closes_tabs() {
        let mut app = app_with_page(false);
        app.tabs = tabs(&["One", "Two", "Three"]);
        app.active = 0;
        app.strip = Some(strip::Item::Tab(2));
        key(&mut app, Key::Ok, true);
        assert_eq!((app.active, app.url.as_str()), (2, "https://example.com/2"));
        assert!(app.strip.is_none() && app.page.is_none());

        app.strip = Some(strip::Item::Close(2));
        key(&mut app, Key::Ok, true);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active, 1, "closing the last of three leaves the one before it");
        assert_eq!(app.url, "https://example.com/1");

        app.active = 1;
        app.strip = Some(strip::Item::Close(0));
        key(&mut app, Key::Ok, true);
        assert_eq!((app.tabs.len(), app.active), (1, 0), "the one on screen kept its place");
        assert_eq!(app.tabs[0].title, "Two");

        app.strip = Some(strip::Item::Close(0));
        key(&mut app, Key::Ok, true);
        assert_eq!(app.tabs.len(), 1, "the last tab closed");

        app.strip = Some(strip::Item::NewTab);
        key(&mut app, Key::Ok, true);
        assert_eq!(app.typing, Typing::NewTab);
        app.kb.as_mut().unwrap().text = "flipper.net".into();
        assert_eq!(key(&mut app, Key::Run, true), Then::OpenTab("https://flipper.net".into()));
    }

    #[test]
    fn a_dead_engine_says_so_and_any_key_leaves() {
        let mut app = App::new(HOME);
        app.failed = Some("chromium: No such file or directory (os error 2)".into());
        let frame = shot("failed", &app);
        assert!(off_palette(&frame).is_empty(), "{:?}", off_palette(&frame));
        assert_eq!(key(&mut app, Key::Down, true), Then::Stay);
        assert_eq!(key(&mut app, Key::Ok, true), Then::Quit);
    }

    /// The minimap comes up when the view moves and goes down three seconds later,
    /// unless PTT is held, which is zooming.
    #[test]
    fn the_minimap_goes_down_when_the_view_is_still() {
        let mut app = app_with_page(false);
        app.view.zoom = 2.0;
        let m = app.view.minimap(app.page.as_ref().unwrap().size()).unwrap();
        let stride = usize::from(theme::PANEL_W);
        let border = (m.y as usize - 2) * stride + stride - 10;
        let corner = |frame: &[u8]| frame[border];
        assert!(!app.minimap_up(), "up before anything moved");
        let still = shot("mini-down", &app);
        assert_eq!(corner(&still), 0xff);

        key(&mut app, Key::Down, true);
        assert!(app.minimap_up());
        let left = app.minimap_left().expect("a time to go down");
        assert!(left <= MINI_LINGER && left > MINI_LINGER - Duration::from_secs(1));
        assert_eq!(corner(&shot("mini-up", &app)), theme::color::BLACK.0);

        app.moved = Some(Instant::now() - MINI_LINGER);
        assert!(!app.minimap_up());
        assert_eq!(app.minimap_left(), None, "nothing left to wait for");

        app.ptt = true;
        assert!(app.minimap_up(), "held down while zooming");
        assert_eq!(app.minimap_left(), None);
    }

    #[test]
    fn ptt_shows_the_factor_only_while_held() {
        let mut app = app_with_page(false);
        key(&mut app, Key::Ptt, true);
        assert!(app.ptt);
        key(&mut app, Key::Ptt, false);
        assert!(!app.ptt);
    }

    /// A real site through the whole path, for looking at: the engine, the frame and
    /// the screen, at x1 and zoomed in. Needs chromium and the network, so it runs
    /// only when asked for: `BROWSER_RENDER=1 cargo test -- --ignored real`.
    #[test]
    #[ignore]
    fn a_real_page_on_the_panel() {
        let url = std::env::var("BROWSER_URL").unwrap_or_else(|_| HOME.to_string());
        let dir = std::env::temp_dir().join(format!("browser-app-real-{}", std::process::id()));
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let engine = cdp::Browser::launch(CHROMIUM, &dir, move |event| {
            let _ = tx.lock().unwrap().send(event);
        })
        .expect("launch");
        let tab = engine.remote().open(&url, false).expect("open");
        engine.switch(None, &tab);
        let mut app = App::new(&url);
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(cdp::Event::Frame(page)) => app.frame(page),
                Ok(cdp::Event::Url { url, .. }) => app.url = url,
                Ok(cdp::Event::Title { .. }) => {}
                Ok(cdp::Event::Gone(why)) => panic!("{why}"),
                Err(_) => break,
            }
        }
        assert!(app.page.is_some(), "no picture of {url}");
        shot("real-x1", &app);
        app.view.zoom = 3.0;
        app.view.pan = (500.0, 300.0);
        shot("real-x3", &app);
        app.view.zoom = 7.0;
        app.ptt = true;
        shot("real-x7", &app);

        // Down with the cursor on the bottom edge scrolls: the key goes through the
        // same path a press on the panel takes, and the page that comes back is not
        // the page that went.
        app.ptt = false;
        app.view.zoom = 1.0;
        app.view.pan.1 = app.page.as_ref().unwrap().size().h;
        app.engine = Some(engine);
        let before = shot("real-before-scroll", &app);
        assert_eq!(key(&mut app, Key::Down, true), Then::Stay);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(cdp::Event::Frame(page)) => app.frame(page),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let after = shot("real-after-scroll", &app);
        assert_ne!(before, after, "Down did not scroll the page");
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn back_with_no_engine_leaves() {
        let mut app = app_with_page(false);
        assert_eq!(key(&mut app, Key::Back, true), Then::Quit);
    }
}
