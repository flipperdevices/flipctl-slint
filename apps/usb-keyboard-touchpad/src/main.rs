//! USB BT Keyboard Touchpad: the Flipper as a host's keyboard, mouse and media keys.
//!
//! The page is TV Media Box's keyboard. The D-pad always works the on-screen
//! keyboard: it moves the highlight and OK types the key it is on into the host.
//! The touchpad is the Flipper's while PTT has not handed it over: it moves the
//! highlight too, and a tap types. Handed to the host, it is the host's mouse and a
//! tap clicks. The ▲ and ▼ cells are volume up and down. The fifth soft key says how
//! else the host can be reached, USB-C or Bluetooth, and switches to it.

mod bt;
mod hid;
mod usb;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use flipctl_app::keyboard::{self, TextInput, Typed};
use flipctl_app::{Haptic, Key, StatusSource, Touch, TouchpadSource};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::bt::Bluetooth;
use crate::usb::Usb;

slint::include_modules!();

/// A touch counts as a tap when it is this short and moves this little, in raw pad
/// units.
const TAP: Duration = Duration::from_millis(250);
const TAP_TRAVEL: i32 = 40;

/// Where the touchpad glyph sits, on the Flipper or handed to the host, and the
/// PTT mark between it and the monitor: TV Media Box's positions.
const PAD_X: [f32; 2] = [70.0, 137.0];
const PTT_X: [f32; 2] = [144.0, 112.0];

/// How the host is reached.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Link {
    Usb,
    Bluetooth,
}

/// The finger on the pad: where it went down and when, where it was last seen, and
/// how far it has gone, for telling a tap from a drag.
struct Stroke {
    at: Instant,
    last: (i32, i32),
    travel: i32,
}

struct State {
    kb: TextInput,
    usb: Option<Usb>,
    bt: Option<Bluetooth>,
    pad: Option<TouchpadSource>,
    buzz: Option<Haptic>,
    pointer: hid::Pointer,
    stroke: Option<Stroke>,
    /// PTT has handed the pad and the D-pad to the host.
    to_host: bool,
    link: Link,
    /// What the host is doing, as the monitor says it.
    state: String,
    /// The case's name for the USB-C port.
    port: String,
}

impl State {
    fn new() -> State {
        let port = flipctl_app::device_port_name();
        State {
            kb: TextInput::keys(""),
            usb: Some(Usb::start()),
            bt: None,
            pad: TouchpadSource::open().ok(),
            buzz: Haptic::open().ok(),
            pointer: hid::Pointer::default(),
            stroke: None,
            to_host: false,
            link: Link::Usb,
            state: "waiting".into(),
            port: if port.is_empty() { "USB-C".into() } else { port },
        }
    }

    fn link_name(&self) -> String {
        match self.link {
            Link::Usb => self.port.clone(),
            Link::Bluetooth => "Bluetooth".into(),
        }
    }

    /// Reach the host the other way: the USB claim given back for Bluetooth, or
    /// taken again.
    fn switch(&mut self) {
        self.link = if self.link == Link::Usb { Link::Bluetooth } else { Link::Usb };
        // One at a time: the old link is given back before the new one is asked for
        self.usb = None;
        self.bt = None;
        match self.link {
            Link::Usb => self.usb = Some(Usb::start()),
            Link::Bluetooth => self.bt = Some(Bluetooth::start()),
        }
    }

    fn send(&mut self, report: &[u8]) {
        if let Some(usb) = self.usb.as_mut() {
            usb.send(report);
        }
        if let Some(bt) = self.bt.as_mut() {
            bt.send(report);
        }
    }

    /// Everything the keyboard has typed since the last look, as key presses and
    /// releases.
    fn send_typed(&mut self) {
        for typed in self.kb.take_typed() {
            match typed {
                Typed::Text(text) => {
                    for (modifiers, key) in text.chars().filter_map(hid::usage) {
                        self.send(&hid::keyboard(modifiers, key));
                        self.send(&hid::keyboard(0, 0));
                    }
                }
                Typed::Backspace => {
                    self.send(&hid::keyboard(0, hid::BACKSPACE));
                    self.send(&hid::keyboard(0, 0));
                }
                Typed::Up | Typed::Down => {
                    let key = if typed == Typed::Up { hid::VOLUME_UP } else { hid::VOLUME_DOWN };
                    self.send(&hid::consumer(key));
                    self.send(&hid::consumer(0));
                }
            }
        }
    }

    /// One touchpad report: the keyboard's highlight, or the host's pointer, and a
    /// tap types or clicks. Returns whether the page changed.
    fn touch(&mut self, t: Touch) -> bool {
        let previous = self.stroke.as_ref().map(|s| s.last);
        if t.down {
            let stroke = self.stroke.get_or_insert(Stroke {
                at: Instant::now(),
                last: (t.x, t.y),
                travel: 0,
            });
            stroke.travel += (t.x - stroke.last.0).abs() + (t.y - stroke.last.1).abs();
            stroke.last = (t.x, t.y);
        }
        let mut changed = false;
        if self.to_host {
            if let (true, Some((x, y))) = (t.down, previous) {
                for (dx, dy) in self.pointer.moved(t.x - x, t.y - y) {
                    self.send(&hid::mouse(0, dx, dy, 0));
                }
            }
        } else if self.kb.touch(t) {
            if let Some(buzz) = self.buzz.as_mut() {
                buzz.play(3, 10);
            }
            changed = true;
        }
        if !t.down {
            let tap =
                self.stroke.take().is_some_and(|s| s.at.elapsed() < TAP && s.travel < TAP_TRAVEL);
            self.pointer.reset();
            if tap && self.to_host {
                self.send(&hid::mouse(1, 0, 0, 0));
                self.send(&hid::mouse(0, 0, 0, 0));
            } else if tap {
                self.kb.key(flipctl_app::FlipperKey::Ok, true);
                self.kb.release(flipctl_app::FlipperKey::Ok);
                self.send_typed();
                changed = true;
            }
        }
        changed
    }

    /// The claim and the host, refreshed each tick. Returns whether either changed.
    fn poll(&mut self) -> bool {
        let state = match (self.usb.as_mut(), self.bt.as_ref()) {
            (Some(usb), _) => {
                usb.poll();
                usb.state()
            }
            (None, Some(bt)) => bt.state(),
            (None, None) => String::new(),
        };
        let changed = state != self.state;
        self.state = state;
        changed
    }

    /// The fifth soft key: the other way to reach the host.
    fn switch_label(&self) -> &'static str {
        match self.link {
            Link::Usb => "BT",
            Link::Bluetooth => "USB",
        }
    }
}

fn strings(items: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(items.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>()))
}

/// Put the state on screen.
fn apply(ui: &AppWindow, s: &State) {
    let v = s.kb.view("", false);
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
    ui.set_buttons(strings(&["Close", "", "", "", s.switch_label()]));
    let at = usize::from(s.to_host);
    ui.set_to_host(s.to_host);
    ui.set_pad_x(PAD_X[at]);
    ui.set_ptt_x(PTT_X[at]);
    ui.set_link(s.link_name().as_str().into());
    ui.set_state(s.state.as_str().into());
}

/// One key. Returns false when the app should close.
fn key(s: &mut State, key: Key, down: bool) -> bool {
    if !down {
        s.kb.release(key.flipper());
        return true;
    }
    match key {
        Key::Escape => return false,
        Key::Ptt => {
            s.to_host = !s.to_host;
            s.stroke = None;
            s.pointer.reset();
            // TV Media Box's: the long effect handing the pad over, a short one back
            if let Some(buzz) = s.buzz.as_mut() {
                if s.to_host {
                    buzz.play(108, 0);
                } else {
                    buzz.play(97, 100);
                }
            }
        }
        Key::Run => s.switch(),
        _ => {
            if let Some(keyboard::Exit::Cancel) = s.kb.key(key.flipper(), true) {
                return false;
            }
            s.send_typed();
        }
    }
    true
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let state = Rc::new(RefCell::new(State::new()));
    apply(&ui, &state.borrow());

    let mut status = StatusSource::new(Duration::from_secs(2));
    flipctl_app::apply_status!(&ui, PanelStatus, status.current());

    let tick = slint::Timer::default();
    let ticking = ui.as_weak();
    let ticked = Rc::clone(&state);
    tick.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
        let Some(ui) = ticking.upgrade() else { return };
        let mut s = ticked.borrow_mut();
        let mut changed = s.poll();
        while let Some(t) = s.pad.as_mut().and_then(TouchpadSource::poll) {
            changed |= s.touch(t);
        }
        if s.kb.animating() || changed {
            apply(&ui, &s);
        }
        if let Some(now) = status.poll() {
            flipctl_app::apply_status!(&ui, PanelStatus, now);
        }
    });

    let keys = ui.as_weak();
    let keyed = Rc::clone(&state);
    ui.on_keyed(move |text, down| {
        let Some(ui) = keys.upgrade() else { return };
        let Some(pressed) = Key::from_slint(text.as_str()) else { return };
        ui.set_pressed_slot(match (down, pressed.soft_slot()) {
            (true, Some(slot)) => slot as i32,
            _ => -1,
        });
        let mut s = keyed.borrow_mut();
        if !key(&mut s, pressed, down) {
            let _ = slint::quit_event_loop();
            return;
        }
        apply(&ui, &s);
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

    /// The page as the panel gets it. `HID_RENDER=1 cargo test` leaves the frames
    /// in target/render to look at.
    fn shot(name: &str, s: &State) -> Vec<u8> {
        let window = panel();
        let ui = AppWindow::new().expect("create AppWindow");
        apply(&ui, s);
        ui.show().expect("show");
        slint::platform::update_timers_and_animations();
        let frame: Vec<u8> = render_frame(&window).expect("frame").iter().map(|px| px.0).collect();
        drop(ui);
        if std::env::var_os("HID_RENDER").is_some() {
            let dir = std::path::Path::new("target/render");
            std::fs::create_dir_all(dir).expect("create target/render");
            let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("png");
            let mut enc = png::Encoder::new(
                std::io::BufWriter::new(file),
                u32::from(theme::PANEL_W),
                u32::from(theme::PANEL_H),
            );
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().expect("header").write_image_data(&frame).expect("data");
        }
        frame
    }

    #[test]
    fn both_modes_draw() {
        let mut s = State::new();
        s.usb = None;
        s.port = "USB-C1".into();
        s.state = "ready".into();
        shot("keys", &s);
        s.to_host = true;
        s.link = Link::Bluetooth;
        s.state = "waiting".into();
        shot("host", &s);
        s.to_host = false;
        s.state = "failed".into();
        shot("failed", &s);
    }
}
