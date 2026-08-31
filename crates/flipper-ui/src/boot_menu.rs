//! The boot menu, apart from how it is drawn.
//!
//! Two programs show this screen: flipctl, where it is one screen among many, and
//! the boot menu image, where it is the only one. Everything they would otherwise
//! each implement lives here: which profiles there are, where the cursor is, the
//! countdown, the popups and their actions, and the words each line says. What is
//! left to the caller is drawing it, and the two things it owns that this cannot:
//! the on-screen keyboard a rename needs, and where Back goes.
//!
//! Nothing here mentions Slint. `view()` returns plain data, which each binary maps
//! onto its own compiled components.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::boot::{self, Profile, Space};
use crate::key::{FlipperKey, KeyEvent};
use crate::theme::{metric::BOOT_SPIN_FRAMES, timing::SPIN_FRAME_MS};

/// How long the marked profile has before it is booted.
///
/// The prototype's own five seconds. Not a token: nothing else in the design uses
/// it, and it is the boot menu's behaviour rather than its geometry.
const TIMEOUT: Duration = Duration::from_secs(5);

/// An action to run on a thread. The bool it answers with is "and now the device has
/// to reboot", which only a factory reset says yes to.
type Work = Box<dyn FnOnce() -> Result<bool, String> + Send>;

/// Which popup is open, and what it is doing.
///
/// Info is read-only. Edit lists the actions for the selected profile, asks before
/// the destructive ones, and reports what happened: the tools take tens of seconds,
/// so "working" and "failed" are states the screen has to have.
enum Popup {
    Info,
    Edit,
    /// Waiting for a yes on the action at this index.
    Confirm(usize),
    /// An action is running; the string is what to call it while it does. No
    /// receiver means there is nothing left to wait for: the device is on its way
    /// down, so the message stays up until it goes.
    Busy(String, Option<Receiver<Result<bool, String>>>),
    /// Finished: what to say. The message is the tool's own words either way.
    Said(String),
}

/// What the caller has to do about a key the menu could not finish itself.
/// Whether the marked profile starts itself when nothing is pressed.
///
/// It is the standalone boot menu's whole reason for existing: a machine left alone
/// has to reach a system. Reached from a booted profile there is nothing to rescue and
/// nobody absent, so a menu opened there counts down to replacing the session that
/// opened it, which is never what was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoStart {
    /// Count down, then boot the marked profile.
    Countdown,
    /// Never boot by itself: the menu waits for a key, with no timer and no bar.
    Off,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Handled. Redraw and carry on.
    Stay,
    /// Back out of the menu: where to is the caller's business, because flipctl has
    /// a menu behind it and the boot image has nothing.
    Leave,
    /// A rename wants a name. The caller runs its keyboard, seeded with this label,
    /// and hands the answer back through `renamed`.
    Rename { name: String, label: String },
}

/// One row of the list.
pub struct Row {
    pub label: String,
    /// "Running", "Used 3 hours ago", or empty for one never booted.
    pub status: String,
    /// 0 none, else 1-based into the icon table the drawing side holds.
    pub icon: i32,
    pub icon_w: f32,
    pub icon_h: f32,
    /// Marked to boot next, which draws a heart after the label.
    pub auto: bool,
    /// 0 internal, 1 SD, 2 USB, 3 drive: what its filesystem sits on.
    pub medium: i32,
}

/// One line of an open popup.
pub struct PopupLine {
    /// 0 a plain line, 1 an action.
    pub kind: i32,
    pub text: String,
    pub selected: bool,
    pub heart: bool,
}

/// Everything on screen, as data.
pub struct View {
    pub rows: Vec<Row>,
    pub selected: i32,
    pub scroll: i32,
    /// How far the countdown has run, 0 to 100, or -1 when it is not running.
    pub countdown: i32,
    /// Seconds left, for the text beside the bar.
    pub remaining: i32,
    /// The profile read is still in flight, so the list shows a spinner.
    pub loading: bool,
    pub spin_frame: i32,
    /// Non-empty while a profile is being booted, which takes the whole panel.
    pub booting: String,
    pub popup_open: bool,
    pub popup_title: String,
    pub popup_icon: i32,
    pub popup_lines: Vec<PopupLine>,
    /// A dialog's message lines, for the popups that ask or report rather than list.
    pub popup_message: Vec<String>,
    pub popup_button: String,
    /// The exclusive size: what deleting this profile alone would free.
    pub size_num: String,
    pub size_unit: String,
    /// True while the size is still a spinner, so the slot keeps its widest width.
    pub size_loading: bool,
    /// The popup frame's width, measured from its own content.
    pub popup_w: f32,
    /// The width the size value's slot holds while it is still a spinner, so the
    /// line does not change width as the value lands.
    pub size_slot_w: f32,
    /// The five soft keys, as the prototype labels them on this screen.
    pub buttons: [&'static str; 5],
}

/// The boot menu's state.
pub struct BootMenu {
    profiles: Vec<Profile>,
    pending: Option<Receiver<Vec<Profile>>>,
    selected: i32,
    /// When the countdown started, and whether a key has stopped it.
    started: Option<Instant>,
    cancelled: bool,
    auto_start: AutoStart,
    booting: Option<(String, Receiver<Result<bool, String>>)>,
    popup: Option<Popup>,
    popup_index: usize,
    space: Option<Space>,
    space_rx: Option<Receiver<Option<Space>>>,
    space_done: bool,
    /// Sizes already measured, by drive and profile: the same profile name exists on
    /// more than one drive and they are different sizes. Kept because the measurement
    /// walks the subvolume and takes seconds, and nothing here can change a profile's
    /// size, so a second look at one costs nothing. Only answers are kept; a failed
    /// measurement is asked again.
    measured: HashMap<(String, String), Space>,
    /// What the running measurement will be filed under.
    space_key: Option<(String, String)>,
    dtbo: Option<(Vec<String>, Vec<String>)>,
    dtbo_rx: Option<Receiver<(Vec<String>, Vec<String>)>>,
    /// Where the spinners count their frames from.
    spin_at: Instant,
    /// How many rows the list can show at once, from the caller's own metrics.
    visible: i32,
}

impl BootMenu {
    /// Open the menu: the profile read starts now, on a thread, because listing them
    /// walks every subvolume on every filesystem and the screen has to appear first.
    pub fn open(visible: i32, auto_start: AutoStart) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let started = std::thread::Builder::new()
            .name("boot-profiles".into())
            .spawn(move || {
                let _ = tx.send(boot::profiles());
            })
            .is_ok();
        Self {
            profiles: Vec::new(),
            pending: started.then_some(rx),
            selected: 0,
            started: None,
            cancelled: false,
            auto_start,
            booting: None,
            popup: None,
            popup_index: 0,
            space: None,
            space_rx: None,
            space_done: false,
            measured: HashMap::new(),
            space_key: None,
            dtbo: None,
            dtbo_rx: None,
            spin_at: Instant::now(),
            visible: visible.max(1),
        }
    }

    /// The profiles as listed, for a caller judging a new name against them.
    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    /// The profile under the cursor.
    pub fn selected_profile(&self) -> Option<&Profile> {
        self.profiles.get(self.selected as usize)
    }

    /// Whether a popup is open, which is what tells a caller its soft keys are not
    /// the list's.
    pub fn popup_open(&self) -> bool {
        self.popup.is_some()
    }
}

impl BootMenu {
    /// A key. Every one of them stops the countdown first: a person pressing buttons
    /// is not waiting for the default to be chosen for them.
    pub fn key(&mut self, event: KeyEvent) -> Outcome {
        if !event.down {
            return Outcome::Stay;
        }
        self.cancelled = true;

        if let Some(open) = self.popup.take() {
            return self.popup_key(open, event.key);
        }
        // Nothing to answer once the kexec is loading: the machine is on its way
        // out, and Back to a list that is about to stop existing is not a choice
        // worth offering.
        if self.booting.is_some() {
            return Outcome::Stay;
        }

        let count = self.profiles.len() as i32;
        match event.key {
            FlipperKey::Down if count > 0 => self.selected = (self.selected + 1).rem_euclid(count),
            FlipperKey::Up if count > 0 => self.selected = (self.selected - 1).rem_euclid(count),
            // Boot the one under the cursor. Nothing else acts on a choice: U-Boot
            // has no menu and ignores the marker, so this and the countdown are the
            // only ways a profile is entered.
            FlipperKey::Ok | FlipperKey::Run if count > 0 => self.boot_selected(),
            // Info is slot 1 and Edit slot 3, the two labelled keys.
            FlipperKey::View if count > 0 => self.open_popup(Popup::Info),
            FlipperKey::Edit if count > 0 => self.open_popup(Popup::Edit),
            FlipperKey::Escape | FlipperKey::Back => return Outcome::Leave,
            _ => {}
        }
        Outcome::Stay
    }

    /// A key while a popup is open. The popup was taken out, so every arm that keeps
    /// it has to put it back.
    fn popup_key(&mut self, open: Popup, key: FlipperKey) -> Outcome {
        let profile = self.selected_profile().cloned();
        let actions = profile.as_ref().map(boot::edit_actions).unwrap_or_default();
        match open {
            // Read-only: any way out closes it.
            Popup::Info => match key {
                FlipperKey::Escape
                | FlipperKey::Back
                | FlipperKey::Ok
                | FlipperKey::Run
                | FlipperKey::Edit => {}
                _ => self.popup = Some(Popup::Info),
            },
            Popup::Edit => match key {
                FlipperKey::Down => {
                    self.popup_index = (self.popup_index + 1) % actions.len().max(1);
                    self.popup = Some(Popup::Edit);
                }
                FlipperKey::Up => {
                    self.popup_index = (self.popup_index + actions.len().saturating_sub(1))
                        % actions.len().max(1);
                    self.popup = Some(Popup::Edit);
                }
                FlipperKey::Ok | FlipperKey::Run => {
                    // Auto Start is reversible and immediate; the rest change or
                    // destroy a profile, so they ask first.
                    match actions.get(self.popup_index).copied() {
                        Some("Auto Start") => self.popup = self.start_action("Auto Start", &profile),
                        // Rename asks for the name first. The popup stays as it was,
                        // so backing out of the keyboard returns to the same list of
                        // actions.
                        Some("Rename") => {
                            let p = profile.unwrap_or_default();
                            self.popup = Some(Popup::Edit);
                            return Outcome::Rename {
                                label: boot::profile_label(&p.name),
                                name: p.name,
                            };
                        }
                        Some(_) => self.popup = Some(Popup::Confirm(self.popup_index)),
                        None => {}
                    }
                }
                FlipperKey::Escape | FlipperKey::Back => {}
                _ => self.popup = Some(Popup::Edit),
            },
            Popup::Confirm(at) => match key {
                FlipperKey::Ok | FlipperKey::Run => {
                    let action = actions.get(at).copied().unwrap_or("");
                    self.popup = self.start_action(action, &profile);
                }
                // Anything else is a no, and returns to the list.
                _ => self.popup = Some(Popup::Edit),
            },
            // Not interruptible: stopping a create-profile halfway would leave a
            // half-made subvolume.
            Popup::Busy(what, rx) => self.popup = Some(Popup::Busy(what, rx)),
            // The only thing this says is why an action failed, so any key dismisses
            // it and the list stands: a failed action changed nothing to re-read.
            Popup::Said(_) => {}
        }
        Outcome::Stay
    }

    /// The name the caller's keyboard came back with, or nothing if it was cancelled.
    ///
    /// An unchanged name is not a rename: it would move a subvolume onto itself.
    pub fn renamed(&mut self, name: &str, text: Option<&str>) {
        let Some(text) = text else { return };
        let dest = boot::rename_dest(
            &Profile { name: name.to_string(), ..Default::default() },
            text,
        );
        if dest.is_empty() || dest == name {
            self.popup = Some(Popup::Edit);
            return;
        }
        // Which drive: a name does not say, since the same one exists on more than one.
        // The rename was started from the selection, so that is the profile it means.
        let dev = self
            .selected_profile()
            .filter(|p| p.name == name)
            .or_else(|| self.profiles.iter().find(|p| p.name == name))
            .map(|p| p.dev.clone())
            .unwrap_or_default();

        let (tx, rx) = std::sync::mpsc::channel();
        let from = name.to_string();
        let to = dest.clone();
        self.popup = match std::thread::Builder::new()
            .name("boot-rename".into())
            .spawn(move || {
                let _ = tx.send(boot::rename(&dev, &from, &to).map(|()| false));
            }) {
            Ok(_) => Some(Popup::Busy("Renaming".into(), Some(rx))),
            Err(_) => Some(Popup::Said("could not start rename".into())),
        };
    }

    /// Open a popup and start the two reads it fills its lines from.
    ///
    /// Separate threads on purpose. Each shells out to a tool that mounts the top
    /// level, and the space measurement walks the subvolume on top of that, so
    /// sharing a thread would hold the faster line behind the slower one.
    fn open_popup(&mut self, which: Popup) {
        self.popup = Some(which);
        self.popup_index = 0;
        self.space = None;
        self.space_done = false;
        self.dtbo = None;
        let Some(p) = self.selected_profile().cloned() else { return };

        let key = (p.dev.clone(), p.name.clone());
        if let Some(space) = self.measured.get(&key) {
            self.space = Some(space.clone());
            self.space_done = true;
        } else {
            let (tx, rx) = std::sync::mpsc::channel();
            let name = p.name.clone();
            let dev = p.dev.clone();
            self.space_key = Some(key);
            self.space_rx = std::thread::Builder::new()
                .name("boot-space".into())
                .spawn(move || {
                    let at = Instant::now();
                    let space = boot::space(&dev, &name);
                    crate::logline!(
                        "boot           size {} in {:.3}s: {}",
                        name,
                        at.elapsed().as_secs_f64(),
                        space.as_ref().map_or("unknown", |s| s.total.as_str())
                    );
                    let _ = tx.send(space);
                })
                .ok()
                .map(|_| rx);
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let name = p.name.clone();
        let dev = p.dev.clone();
        self.dtbo_rx = std::thread::Builder::new()
            .name("boot-dtbo".into())
            .spawn(move || {
                let at = Instant::now();
                let (system, user) = boot::dtbo(&dev, &name);
                crate::logline!(
                    "boot           overlays {} in {:.3}s: {} system, {} user",
                    name,
                    at.elapsed().as_secs_f64(),
                    system.len(),
                    user.len()
                );
                let _ = tx.send((system, user));
            })
            .ok()
            .map(|_| rx);
    }

    /// Start one of the Edit actions on a thread.
    ///
    /// Every one of these shells out to a tool that takes seconds to a minute, so
    /// none of them can run on the render loop. The message each reports is the
    /// tool's own last line, because that is where these tools put the reason.
    fn start_action(&self, action: &str, profile: &Option<Profile>) -> Option<Popup> {
        let p = profile.clone()?;
        let existing = self.profiles.clone();
        let (busy, work): (&str, Work) = match action {
            "Auto Start" => (
                "Saving",
                Box::new(move || boot::set_auto_start(&p.dev, &p.id).map(|_| false)),
            ),
            "Clone" => {
                let dest = boot::clone_dest(&p, &existing);
                (
                    "Cloning",
                    Box::new(move || boot::clone(&p.dev, &p.name, &dest).map(|_| false)),
                )
            }
            "Delete" => (
                "Deleting",
                Box::new(move || boot::delete(&p.dev, &p.name).map(|_| false)),
            ),
            // The one action that can answer "and now the device has to reboot": the
            // running root is the copy moved aside, so the fresh one is only
            // reachable through a reboot.
            "Factory Reset" => (
                "Resetting",
                Box::new(move || boot::factory_reset(&p.dev, &p.name, &p.origin)),
            ),
            // Rename is not started here: it needs a name first.
            _ => return Some(Popup::Edit),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("boot-action".into())
            .spawn(move || {
                let _ = tx.send(work());
            })
            .ok()?;
        Some(Popup::Busy(busy.to_string(), Some(rx)))
    }

    /// Boot the profile under the cursor, and give the panel over to saying so.
    fn boot_selected(&mut self) {
        let Some(p) = self.selected_profile().cloned() else { return };
        // Whatever was being read stops mattering here, and a read of ours in flight is
        // not free: a size walk holds a top-level mount for seconds, and the machine
        // leaves with that filesystem still mounted. The answers are dropped so nothing
        // new starts and no spinner outlives the takeover; boot-profile unmounts what
        // the tools still hold before it jumps.
        self.popup = None;
        self.space_rx = None;
        self.space_key = None;
        self.dtbo_rx = None;
        crate::logline!("boot menu      boot {} on {}", p.name, if p.dev.is_empty() { "the booted filesystem" } else { p.dev.as_str() });
        let (tx, rx) = std::sync::mpsc::channel();
        let label = boot::display_name(&p.name);
        if std::thread::Builder::new()
            .name("boot-now".into())
            .spawn(move || {
                let _ = tx.send(boot::boot_now(&p));
            })
            .is_ok()
        {
            self.booting = Some((label, rx));
        }
    }
}

impl BootMenu {
    /// Everything that lands on its own: the profile read, the popup's two reads, an
    /// action finishing, the countdown running out and the boot answering.
    ///
    /// Returns whether anything changed, so a caller can skip a repaint. Spinners
    /// count as change: nothing else asks for a frame while a read is in flight, and
    /// a minute of still spinner reads as a hung program.
    pub fn tick(&mut self) -> bool {
        let mut moved = false;

        if let Some(rx) = self.pending.as_ref() {
            match rx.try_recv() {
                Ok(list) => {
                    crate::logline!("boot menu      {} profiles", list.len());
                    self.profiles = list;
                    self.pending = None;
                    moved = true;
                    // The countdown only runs when there is something for it to
                    // boot: it enters the profile wearing the heart, so with nothing
                    // marked there is nothing to count down to and the menu waits.
                    // Started when the list lands rather than when the screen opens,
                    // because counting down against an empty list is a race.
                    if self.auto_start == AutoStart::Countdown
                        && !self.cancelled
                        && self.profiles.iter().any(|p| p.auto_boot)
                    {
                        self.started = Some(Instant::now());
                        if let Some(p) = self.profiles.iter().find(|p| p.auto_boot) {
                            crate::logline!(
                                "boot menu      countdown {}s to {}",
                                TIMEOUT.as_secs(),
                                p.name
                            );
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // The load spinner picks its frame from the clock, so it needs a
                    // repaint asked for while the read runs.
                    moved = true;
                }
                Err(_) => self.pending = None,
            }
        }

        if let Some(rx) = self.space_rx.as_ref() {
            match rx.try_recv() {
                Ok(space) => {
                    if let (Some(key), Some(measured)) = (self.space_key.take(), space.as_ref()) {
                        self.measured.insert(key, measured.clone());
                    }
                    self.space = space;
                    self.space_done = true;
                    self.space_rx = None;
                    moved = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => moved = true,
                Err(_) => {
                    self.space_rx = None;
                    self.space_key = None;
                }
            }
        }
        if let Some(rx) = self.dtbo_rx.as_ref() {
            if let Ok(dtbo) = rx.try_recv() {
                self.dtbo = Some(dtbo);
                self.dtbo_rx = None;
                moved = true;
            }
        }

        // A finished action says nothing: the popup closes and the list is read
        // again, which is the answer. Only a failure has something to report, and it
        // waits for a key.
        let mut restart_read = false;
        if let Some(Popup::Busy(what, Some(rx))) = self.popup.as_ref() {
            match rx.try_recv() {
                Ok(Ok(rebooting)) => {
                    crate::logline!("boot action    {what} done, rebooting={rebooting}");
                    self.popup = if rebooting {
                        Some(Popup::Busy("Rebooting".into(), None))
                    } else {
                        restart_read = true;
                        None
                    };
                    moved = true;
                }
                Ok(Err(e)) => {
                    crate::logline!("boot action    {what} failed: {e}");
                    self.popup = Some(Popup::Said(e));
                    moved = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => moved = true,
                Err(_) => {
                    self.popup = Some(Popup::Said("action stopped".into()));
                    moved = true;
                }
            }
        }
        if restart_read {
            let again = Self::open(self.visible, self.auto_start);
            self.profiles = Vec::new();
            self.pending = again.pending;
            self.selected = 0;
            self.popup_index = 0;
        }

        // Time up: boot the marked profile. The countdown is cleared first, so a boot
        // that fails leaves the menu sitting there rather than trying again every
        // turn.
        if let Some(at) = self.started {
            if !self.cancelled && self.booting.is_none() && at.elapsed() >= TIMEOUT {
                self.started = None;
                self.cancelled = true;
                if let Some(at) = self.profiles.iter().position(|p| p.auto_boot) {
                    crate::logline!("boot menu      countdown done");
                    self.selected = at as i32;
                    self.boot_selected();
                }
                moved = true;
            } else if !self.cancelled {
                // The bar sweeps, so every turn is a change while it does.
                moved = true;
            }
        }

        // The two ways the machine can still be here after a boot: a boot that would not
        // load, which is the one boot failure a person can act on, and a dry run, which
        // loaded the image and unloaded it again. Both have to take the takeover down and
        // say so, or the panel claims a boot that never happened.
        let said = match self.booting.as_ref() {
            Some((label, rx)) => match rx.try_recv() {
                Ok(Err(e)) => {
                    crate::logline!("boot menu      boot refused: {e}");
                    Some(e)
                }
                Ok(Ok(true)) => {
                    crate::logline!("boot menu      dry run: {label} loaded and unloaded");
                    Some(format!("{label}: loaded OK (dry run)"))
                }
                // Handing over, or the thread is gone: nothing left to say.
                Ok(Ok(false)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
                // Still loading the kernel, and nothing is asked to change while it
                // does: a frame committed now is an SPI transfer that leaves the DMA
                // controller armed for the next kernel to trip over, which is a panic
                // it cannot even report. The takeover is drawn once and then stands
                // still. The boot itself is on its own thread and waits for none of it.
                Err(std::sync::mpsc::TryRecvError::Empty) => None
            },
            None => None,
        };
        if let Some(message) = said {
            self.booting = None;
            self.popup = Some(Popup::Said(message));
            self.popup_index = 0;
            moved = true;
        }

        moved
    }
}

impl BootMenu {
    /// Everything on screen, as data. Called per frame; nothing here does I/O.
    pub fn view(&self) -> View {
        let now = std::time::SystemTime::now();
        // The text spinner, for a value still being read: four frames at the same
        // rate the icon spinner turns, so nothing on screen beats to two clocks.
        let frame = self.spin_at.elapsed().as_millis() / SPIN_FRAME_MS as u128;
        let spin = ["-", "\\", "|", "/"][frame as usize % 4];

        let rows: Vec<Row> = self
            .profiles
            .iter()
            .map(|p| {
                // Each sprite's own size, so none is stretched: the fallback is
                // 14x14 where the profile icons are 10x8, and router_small is 9x7.
                let (icon, (icon_w, icon_h)) = match boot::icon_key(&p.name, &p.origin) {
                    "minimal" => (1, (10.0, 8.0)),
                    "desktop" => (2, (10.0, 8.0)),
                    "router" => (3, (9.0, 7.0)),
                    "media" => (4, (10.0, 8.0)),
                    "graphics" => (5, (10.0, 8.0)),
                    _ => (6, (14.0, 14.0)),
                };
                Row {
                    label: boot::display_name(&p.name),
                    status: boot::used_ago(&p.last_used, now),
                    icon,
                    icon_w,
                    icon_h,
                    auto: p.auto_boot,
                    medium: p.medium.as_i32(),
                }
            })
            .collect();

        // The window the cursor is in, clamped to the ends: the list is taller than
        // the panel as soon as a card is in the slot.
        let count = self.profiles.len() as i32;
        let scroll = (self.selected - self.visible + 1)
            .max(0)
            .min((count - self.visible).max(0));

        let counting = self.started.is_some() && !self.cancelled;
        let countdown = match self.started {
            Some(at) if !self.cancelled => {
                let elapsed = at.elapsed().min(TIMEOUT);
                (elapsed.as_secs_f32() / TIMEOUT.as_secs_f32() * 100.0) as i32
            }
            // Cancelled, or never started: no bar and no countdown text.
            _ => -1,
        };
        let remaining = if counting {
            let at = self.started.unwrap_or_else(Instant::now);
            TIMEOUT.saturating_sub(at.elapsed()).as_secs_f32().ceil() as i32
        } else {
            0
        };

        let profile = self.selected_profile().cloned().unwrap_or_default();
        let popup_icon = match boot::icon_key(&profile.name, &profile.origin) {
            "minimal" => 1,
            "desktop" => 2,
            "router" => 3,
            "media" => 4,
            "graphics" => 5,
            _ => 6,
        };

        let mut lines: Vec<PopupLine> = Vec::new();
        let mut message: Vec<String> = Vec::new();
        let mut button = String::new();
        let plain = |text: String| PopupLine { kind: 0, text, selected: false, heart: false };
        let joined = |v: &Vec<String>| if v.is_empty() { "none".to_string() } else { v.join(" ") };

        match self.popup.as_ref() {
            Some(Popup::Info) => {
                // Where it is. First, because on a machine with a card in it two
                // profiles can carry the same name, and this is what says which one.
                if !profile.disk.is_empty() {
                    lines.push(plain(format!("Drive: {} ({})", profile.disk, profile.kind)));
                }
                lines.push(plain(format!(
                    "Cloned from: {}",
                    if profile.parent.is_empty() {
                        "-".to_string()
                    } else {
                        boot::display_name(&profile.parent)
                    }
                )));
                lines.push(plain(format!(
                    "Factory: {}",
                    if profile.origin.is_empty() {
                        "-".to_string()
                    } else {
                        profile.origin.trim_start_matches('@').to_string()
                    }
                )));
                lines.push(plain(format!(
                    "Last used: {}",
                    boot::info_last_used(&profile.last_used, now)
                )));
                let dt = self.dtbo.as_ref();
                lines.push(plain(format!(
                    "DTBO system: {}",
                    dt.map_or(spin.to_string(), |(sys, _)| joined(sys))
                )));
                lines.push(plain(format!(
                    "DTBO user: {}",
                    dt.map_or(spin.to_string(), |(_, usr)| joined(usr))
                )));
            }
            Some(Popup::Edit) => {
                for (i, action) in boot::edit_actions(&profile).iter().enumerate() {
                    lines.push(PopupLine {
                        kind: 1,
                        text: (*action).to_string(),
                        selected: i == self.popup_index,
                        heart: *action == "Auto Start" && profile.auto_boot,
                    });
                }
            }
            Some(Popup::Confirm(at)) => {
                let action = boot::edit_actions(&profile).get(*at).copied().unwrap_or("");
                message.push(action.to_string());
                message.push(boot::display_name(&profile.name));
                button = "OK = yes    Back = no".into();
            }
            Some(Popup::Busy(what, _)) => message.push(format!("{what} {spin}")),
            Some(Popup::Said(msg)) => {
                message.push(msg.clone());
                button = "Press any key".into();
            }
            None => {}
        }

        // The exclusive size: what deleting this profile alone would free, which is
        // the number the popup labels "Size".
        let (size_num, size_unit) = match (self.space.as_ref(), self.space_done) {
            (Some(sp), _) => boot::size_parts(&sp.unique),
            // Asked and got nothing: "?" as sizeParts gives for a null, rather than
            // a spinner that never stops.
            (None, true) => ("?".to_string(), String::new()),
            (None, false) => (spin.to_string(), String::new()),
        };

        // The frame is sized to its content, as boot_menu.js does:
        //
        //     min(252, max(sizeLine, nameLine, body + 2 * padH) + 12)
        //
        // Measured here rather than in Slint, which cannot take a maximum across a
        // model, and because the advance tables are on this side. The name is in
        // Born2bSportyV2 and everything else in HaxrCorp, so two tables are involved.
        let popup_w = {
            use crate::font::{ROW_ACTIVE, TITLE};
            use crate::theme::metric::{POPUP_MAX_W, POPUP_PAD_H, SIZE_GAP};

            let size_line = TITLE.text_width("Size:")
                + SIZE_GAP as u16
                + TITLE.text_width(&size_num)
                + if size_unit.is_empty() {
                    0
                } else {
                    SIZE_GAP as u16 + TITLE.text_width(&size_unit)
                };
            let name_line = POPUP_PAD_H as u16
                + 14
                + 4
                + ROW_ACTIVE.text_width(&boot::display_name(&profile.name))
                + POPUP_PAD_H as u16;
            // The body is whichever of the two lists is showing.
            let mut body = 0u16;
            for line in &lines {
                body = body.max(TITLE.text_width(&line.text));
            }
            for line in &message {
                body = body.max(TITLE.text_width(line));
            }
            if !button.is_empty() {
                body = body.max(TITLE.text_width(&button));
            }
            let widest = size_line.max(name_line).max(body + 2 * POPUP_PAD_H as u16);
            f32::from((widest + 12).min(POPUP_MAX_W as u16))
        };

        View {
            rows,
            selected: self.selected,
            scroll,
            countdown,
            remaining,
            loading: self.pending.is_some(),
            spin_frame: (frame % BOOT_SPIN_FRAMES as u128) as i32,
            booting: self.booting.as_ref().map(|(l, _)| l.clone()).unwrap_or_default(),
            popup_open: self.popup.is_some(),
            popup_title: boot::display_name(&profile.name).trim_matches(['[', ']']).to_string(),
            popup_icon,
            popup_lines: lines,
            popup_message: message,
            popup_button: button,
            size_num,
            size_unit,
            size_loading: !(self.space.is_some() || self.space_done),
            popup_w,
            // While loading, the slot is the widest spinner frame; once the value
            // lands it is the value's own width.
            size_slot_w: if self.space.is_some() || self.space_done {
                0.0
            } else {
                // The widest spinner frame, not an arbitrary character: the slot has
                // to hold every frame without the line reflowing between them.
                let w = ["-", "\\", "|", "/"]
                    .iter()
                    .map(|f| crate::font::TITLE.text_width(f))
                    .max()
                    .unwrap_or(0);
                f32::from(w)
            },
            // Info on slot 2 and Edit on slot 4, which is where boot_menu.js puts
            // them: the outer slots stay empty on this screen. Once a boot is under
            // way `key` answers nothing, so the labels go with it rather than offer
            // two presses that do not happen.
            buttons: if self.booting.is_some() {
                ["", "", "", "", ""]
            } else {
                ["", "Info", "", "Edit", ""]
            },
        }
    }
}
