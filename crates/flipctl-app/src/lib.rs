//! The framework a hosted app draws itself with.
//!
//! flipctl no longer describes an app's screen for it: an app is a Wayland client
//! on the compositor flipctl runs, and it owns every pixel of its surface. What it
//! takes from here is the interface: the same widgets flipctl's own screens are
//! built from, the same tokens, the same two fonts, and the panel's buttons as
//! names rather than as scancodes.
//!
//! Most apps need no `.slint` of their own: they implement [`App`] and say what the
//! page shows, and [`main!`] runs them on the pages this crate ships. See [`app`].
//!
//! An app that draws a page of its own is a `.slint` file composing the widgets,
//! plus a `main` that fills in the properties and calls [`attach!`] for the keys and
//! the status bar:
//!
//! ```ignore
//! // build.rs
//! fn main() {
//!     flipctl_app::build::compile("ui/app.slint");
//! }
//! ```
//!
//! ```ignore
//! slint::include_modules!();
//! use flipctl_app::Key;
//!
//! fn main() -> Result<(), slint::PlatformError> {
//!     let app = AppWindow::new()?;
//!     let _keys = flipctl_app::attach!(&app, PanelStatus, AppKeys, |key| match key {
//!         Key::Ok => { ..; true }
//!         _ => false,
//!     });
//!     app.run()
//! }
//! ```

pub use flipper_ui::key::FlipperKey;
pub use flipper_ui::platform::Touch;
pub use flipper_ui::status::{Status, StatusSource};
pub use flipper_ui::Ethernet;
pub use flipper_ui::{dropdown, font, keyboard, paint, pixel, theme};

pub mod app;
#[cfg(feature = "render")]
pub mod render;
pub use app::{
    choice, gauge, heading, item, pair, redraw, rule, text, App, Item, Page, Row, Text, Ui,
};

/// The touchpad and the motor, which a hosted app opens itself: the compositor has
/// no pointer, so the pad reaches nobody unless somebody reads it.
#[cfg(feature = "device")]
pub use flipper_ui::{evdev::TouchpadSource, haptic::Haptic};

/// Put a status reading on the panel's bar.
///
/// A macro because the global it writes to belongs to the app's own compiled
/// components, so no function here can name its type.
///
/// ```ignore
/// let mut source = flipctl_app::StatusSource::new(Duration::from_secs(2));
/// if let Some(now) = source.poll() {
///     flipctl_app::apply_status!(ui, PanelStatus, now);
/// }
/// ```
#[macro_export]
macro_rules! apply_status {
    ($ui:expr, $global:ident, $status:expr) => {{
        let s = $status;
        let bar = $ui.global::<$global>();
        bar.set_battery(s.battery);
        bar.set_charging(s.charging);
        bar.set_wifi_connected(s.wifi_connected);
        bar.set_wifi_quality(s.wifi_quality);
        bar.set_ethernet(match s.ethernet {
            $crate::Ethernet::Down => 0,
            $crate::Ethernet::Real => 1,
            $crate::Ethernet::Usb => 2,
        });
        bar.set_modem_available(s.modem_available);
        bar.set_access_tech(s.access_tech.into());
        bar.set_modem_quality(s.modem_quality);
    }};
}

/// A greyscale picture as an image Slint can draw.
///
/// For an app that draws pixels rather than rows: it paints into a `paint::Surface`
/// with the same primitives and fonts flipctl's own screens use, and hands the
/// result over here. The grey is expanded to RGB because that is what an image
/// takes, and the panel converts it back on the way out; one panel-sized picture
/// is 110KB, which at the rate a graph changes is nothing.
pub fn picture(surface: &paint::Surface) -> slint::Image {
    let (w, h) = (surface.width(), surface.height());
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(u32::from(w), u32::from(h));
    for (px, grey) in buffer.make_mut_slice().iter_mut().zip(surface.pixels()) {
        *px = slint::Rgb8Pixel { r: grey.0, g: grey.0, b: grey.0 };
    }
    slint::Image::from_rgb8(buffer)
}

/// The keys and the status bar, wired to a page of the app's own.
///
/// What [`attach!`] does, for the closures it makes from the app's generated types:
/// the status bar polled every second, Left and Right lit while held, a soft key shown
/// pressed for `press_flash_ms` before `handler` sees it, and Escape closing when the
/// handler leaves it. Keep the result: dropping it stops the status poll.
pub fn attach(
    set_slot: impl Fn(i32) + 'static,
    set_arrow: impl Fn(i32) + 'static,
    set_status: impl Fn(&Status) + 'static,
    bind: impl FnOnce(Box<dyn Fn(&str, bool)>),
    handler: impl FnMut(Key) -> bool + 'static,
) -> Attached {
    use std::cell::RefCell;
    use std::rc::Rc;
    let act = Rc::new(RefCell::new(handler));
    let act = move |key: Key| {
        if !(act.borrow_mut())(key) && key == Key::Escape {
            let _ = slint::quit_event_loop();
        }
    };
    let set_slot = Rc::new(set_slot);
    bind(Box::new(move |text, down| {
        let Some(key) = Key::from_slint(text) else { return };
        if matches!(key, Key::Left | Key::Right) {
            set_arrow(match (down, key) {
                (false, _) => 0,
                (true, Key::Left) => 1,
                _ => 2,
            });
        }
        if !down {
            return;
        }
        let Some(slot) = key.soft_slot() else {
            act(key);
            return;
        };
        set_slot(slot as i32);
        let (set_slot, act) = (set_slot.clone(), act.clone());
        let flash = std::time::Duration::from_millis(theme::timing::PRESS_FLASH_MS as u64);
        slint::Timer::single_shot(flash, move || {
            set_slot(-1);
            act(key);
        });
    }));
    Attached { _status: poll_status(set_status) }
}

/// What [`attach`] keeps running for the app.
pub struct Attached {
    _status: slint::Timer,
}

/// The status bar's reading now and then every second, while the timer is kept.
pub(crate) fn poll_status(set: impl Fn(&Status) + 'static) -> slint::Timer {
    let mut source = StatusSource::new(std::time::Duration::from_secs(2));
    set(&source.current());
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), move || {
        if let Some(now) = source.poll() {
            set(&now);
        }
    });
    timer
}

/// Wire the keys and the status bar to a page of the app's own: see [`attach`].
///
/// The page binds `AppKeys.pressed_slot` and `AppKeys.arrow_pressed` into its bodies,
/// imported with `Shell` from `@app/shell.slint`, and re-exports `AppKeys` and
/// `PanelStatus` so Rust can reach them.
#[macro_export]
macro_rules! attach {
    ($ui:expr, $status:ident, $keys:ident, $handler:expr) => {{
        let ui = $ui;
        let (slot, arrow, status, bind) = (ui.as_weak(), ui.as_weak(), ui.as_weak(), ui.as_weak());
        $crate::attach(
            move |n| {
                if let Some(ui) = slot.upgrade() {
                    ui.global::<$keys>().set_pressed_slot(n);
                }
            },
            move |n| {
                if let Some(ui) = arrow.upgrade() {
                    ui.global::<$keys>().set_arrow_pressed(n);
                }
            },
            move |now: &$crate::Status| {
                if let Some(ui) = status.upgrade() {
                    $crate::apply_status!(&ui, $status, *now);
                }
            },
            move |route| {
                if let Some(ui) = bind.upgrade() {
                    ui.global::<$keys>().on_key(move |text, down| route(&text, down));
                }
            },
            $handler,
        )
    }};
}

/// Run an [`App`] on the pages: the whole of an app's `main`.
///
/// The build script compiles the pages with `flipctl_app::build::pages()`. Their
/// generated types are kept in a module of their own, `flipctl_pages`, so they cannot
/// clash with the app's names or this crate's. A test with the `render` feature can
/// draw the app as the panel would with `flipctl_pages::shot(app, status)`.
#[macro_export]
macro_rules! main {
    ($app:expr) => {
        mod flipctl_pages {
            slint::include_modules!();

            use slint::{ComponentHandle as _, Model as _};
            use std::rc::Rc;

            fn shared(s: &str) -> slint::SharedString {
                s.into()
            }

            pub struct Screen {
                page: Page,
                rows: Rc<slint::VecModel<DetailRow>>,
                items: Rc<slint::VecModel<ListItem>>,
                lines: Rc<slint::VecModel<slint::SharedString>>,
                texts: Rc<slint::VecModel<CanvasText>>,
                buttons: Rc<slint::VecModel<slint::SharedString>>,
            }

            impl $crate::app::Screen for Screen {
                fn set_mode(&self, mode: i32) {
                    self.page.set_mode(mode);
                }
                fn set_buttons(&self, labels: [&str; 5]) {
                    self.buttons.set_vec(labels.map(shared).to_vec());
                }
                fn set_rows(
                    &self,
                    rows: &[$crate::Row],
                    fit: bool,
                    offset: usize,
                    ends: (bool, bool),
                ) {
                    self.rows.set_vec(
                        rows.iter()
                            .map(|r| DetailRow {
                                kind: r.kind,
                                label: shared(&r.label),
                                value: shared(&r.value),
                                percent: r.percent,
                                dim: r.dim,
                            })
                            .collect::<Vec<_>>(),
                    );
                    self.page.set_fit_gauges(fit);
                    self.page.set_offset(offset as i32);
                    self.page.set_at_start(ends.0);
                    self.page.set_at_end(ends.1);
                }
                fn visible_rows(&self) -> usize {
                    self.page.get_visible_rows().max(1) as usize
                }
                fn set_menu(&self, items: &[$crate::Item], selected: usize, scroll: usize) {
                    self.items.set_vec(
                        items
                            .iter()
                            .map(|i| ListItem {
                                label: shared(&i.label),
                                tag: shared(&i.tag),
                                status: shared(&i.status),
                                icon: 0,
                                picture: slint::Image::default(),
                                frames: 1,
                                chevrons: i.chevrons,
                                dim: i.dim,
                                value: shared(&i.value),
                                at_start: i.at_start,
                                at_end: i.at_end,
                            })
                            .collect::<Vec<_>>(),
                    );
                    self.page.set_selected(selected as i32);
                    self.page.set_scroll(scroll as i32);
                }
                fn set_row_pressed(&self, pressed: bool) {
                    self.page.set_pressed(pressed);
                }
                fn set_log(&self, title: &str, lines: &[String], total: usize, offset: usize) {
                    self.page.set_log_title(shared(title));
                    self.lines.set_vec(lines.iter().map(|l| shared(l)).collect::<Vec<_>>());
                    self.page.set_total(total as i32);
                    self.page.set_log_offset(offset as i32);
                }
                fn set_canvas(&self, picture: &slint::Image, texts: &[$crate::Text]) {
                    self.page.set_picture(picture.clone());
                    self.texts.set_vec(
                        texts
                            .iter()
                            .map(|t| CanvasText {
                                x: t.x,
                                y: t.y,
                                text: shared(&t.text),
                                font: t.font,
                                align: t.align,
                                white: t.white,
                            })
                            .collect::<Vec<_>>(),
                    );
                }
                fn set_pressed_slot(&self, slot: i32) {
                    self.page.global::<AppKeys>().set_pressed_slot(slot);
                }
                fn set_arrow_pressed(&self, arrow: i32) {
                    self.page.global::<AppKeys>().set_arrow_pressed(arrow);
                }
                fn set_status(&self, status: &$crate::Status) {
                    $crate::apply_status!(&self.page, PanelStatus, *status);
                }
            }

            /// The page and the screen that fills it, its lists bound once and
            /// refilled in place.
            pub fn open() -> Result<(Page, Screen), slint::PlatformError> {
                let page = Page::new()?;
                let screen = Screen {
                    page: page.clone_strong(),
                    rows: Rc::new(slint::VecModel::default()),
                    items: Rc::new(slint::VecModel::default()),
                    lines: Rc::new(slint::VecModel::default()),
                    texts: Rc::new(slint::VecModel::default()),
                    buttons: Rc::new(slint::VecModel::default()),
                };
                page.set_rows(slint::ModelRc::from(screen.rows.clone()));
                page.set_items(slint::ModelRc::from(screen.items.clone()));
                page.set_lines(slint::ModelRc::from(screen.lines.clone()));
                page.set_texts(slint::ModelRc::from(screen.texts.clone()));
                page.set_buttons(slint::ModelRc::from(screen.buttons.clone()));
                Ok((page, screen))
            }

            pub fn run(app: impl $crate::App + 'static) -> Result<(), slint::PlatformError> {
                let (page, screen) = open()?;
                let keys = page.as_weak();
                $crate::app::run(
                    app,
                    screen,
                    move |route| {
                        if let Some(page) = keys.upgrade() {
                            page.global::<AppKeys>().on_key(move |text, down| route(&text, down));
                        }
                    },
                    move || page.run(),
                )
            }

            /// The app's page as the panel would get it, 256x144 bytes of grey, with
            /// `status` on the bar.
            #[cfg(test)]
            pub fn shot(app: impl $crate::App, status: $crate::Status) -> Vec<u8> {
                let window = $crate::render::panel();
                let (page, screen) = open().expect("open the pages");
                $crate::app::Screen::set_status(&screen, &status);
                let mut driver = $crate::app::Driver::new(app, screen);
                driver.draw();
                page.show().expect("show");
                $crate::render::frame(&window)
            }
        }

        fn main() -> Result<(), slint::PlatformError> {
            flipctl_pages::run($app)
        }
    };
}

/// The panel's buttons, as they arrive from the compositor.
///
/// flipctl forwards the panel's evdev codes to the focused app, so what reaches a
/// client is an ordinary key: the D-pad as arrows, Enter and Backspace, and the
/// five soft buttons as the letters `key.rs` maps them to. An app should never
/// have to know that, which is what this enum is for.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Ok,
    Back,
    /// The five soft buttons, in silkscreen order.
    Escape,
    View,
    Power,
    Edit,
    Run,
    /// Push to talk. AppSwitch never reaches an app: flipctl keeps it.
    Ptt,
}

impl Key {
    /// The key behind a Slint key event, or `None` for one the panel has no
    /// button for.
    pub fn from_slint(text: &str) -> Option<Self> {
        use slint::platform::Key as K;
        let mut chars = text.chars();
        let (first, rest) = (chars.next()?, chars.next());
        if rest.is_some() {
            return None;
        }
        let named = |k: K| -> char { k.into() };
        Some(match first {
            c if c == named(K::UpArrow) => Self::Up,
            c if c == named(K::DownArrow) => Self::Down,
            c if c == named(K::LeftArrow) => Self::Left,
            c if c == named(K::RightArrow) => Self::Right,
            c if c == named(K::Return) => Self::Ok,
            c if c == named(K::Backspace) => Self::Back,
            'z' | 'Z' => Self::Escape,
            'x' | 'X' => Self::View,
            'c' | 'C' => Self::Power,
            'v' | 'V' => Self::Edit,
            'b' | 'B' => Self::Run,
            'a' | 'A' => Self::Ptt,
            _ => return None,
        })
    }

    /// The same button under flipper-ui's name, which is what the on-screen
    /// keyboard takes.
    pub const fn flipper(self) -> FlipperKey {
        match self {
            Self::Up => FlipperKey::Up,
            Self::Down => FlipperKey::Down,
            Self::Left => FlipperKey::Left,
            Self::Right => FlipperKey::Right,
            Self::Ok => FlipperKey::Ok,
            Self::Back => FlipperKey::Back,
            Self::Escape => FlipperKey::Escape,
            Self::View => FlipperKey::View,
            Self::Power => FlipperKey::Power,
            Self::Edit => FlipperKey::Edit,
            Self::Run => FlipperKey::Run,
            Self::Ptt => FlipperKey::Ptt,
        }
    }

    /// Whether this is one of the five soft buttons, and which, from the left.
    pub const fn soft_slot(self) -> Option<usize> {
        Some(match self {
            Self::Escape => 0,
            Self::View => 1,
            Self::Power => 2,
            Self::Edit => 3,
            Self::Run => 4,
            _ => return None,
        })
    }
}

/// The build half: what an app's build script calls.
///
/// Generating the theme here rather than shipping a generated file keeps one
/// parse of tokens.toml behind every theme in the tree, flipctl's and every app's.
#[cfg(feature = "build")]
pub mod build {
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Compile the pages an [`App`](crate::App) runs on, for an app with no `.slint`
    /// of its own.
    pub fn pages() {
        compile(concat!(env!("FLIPCTL_APP_DIR"), "/pages.slint"));
    }

    /// Compile an app's components against flipctl's widget library.
    ///
    /// The app's `.slint` imports the widgets it wants from `@flipctl` and the
    /// tokens from `@theme`:
    ///
    /// ```slint
    /// import { DetailBody, DetailRow } from "@flipctl/detail.slint";
    /// import { SoftBar } from "@flipctl/frame.slint";
    /// import { FlipperTheme } from "@theme";
    /// ```
    pub fn compile(entry: &str) {
        let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
        let tokens = PathBuf::from(env!("FLIPCTL_TOKENS"));
        let ui = PathBuf::from(env!("FLIPCTL_UI_DIR"));
        println!("cargo:rerun-if-changed={}", tokens.display());
        println!("cargo:rerun-if-changed={}", ui.display());
        println!("cargo:rerun-if-changed={}", env!("FLIPCTL_APP_DIR"));
        println!("cargo:rerun-if-changed={entry}");

        let doc = flipper_tokens::parse(&std::fs::read_to_string(&tokens).expect("tokens.toml"));
        let theme = out.join("theme.slint");
        flipper_tokens::write_if_changed(&theme, &flipper_tokens::slint_theme(&doc));

        // Pinned at 16, and it must stay there: the panel's fonts are pixel fonts
        // that rasterise with zero partial coverage at 16px and antialias at any
        // other size.
        std::env::set_var("SLINT_FONT_SIZES", "16");

        let libs = HashMap::from([
            ("theme".to_string(), theme),
            ("flipctl".to_string(), ui),
            ("app".to_string(), PathBuf::from(env!("FLIPCTL_APP_DIR"))),
        ]);
        let config = slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer)
            .with_library_paths(libs);
        slint_build::compile_with_config(entry, config)
            .unwrap_or_else(|e| panic!("compile {entry}: {e}"));
    }
}
