//! The boot menu, apart from how it is drawn.
//!
//! Two programs show this screen: flipctl, where it is one screen among many, and
//! the boot menu image, where it is the only one. Everything they would otherwise
//! each implement lives here: which profiles there are and which kernel each one
//! boots, where the cursor is, the countdown, the popups and their actions, and the
//! words each line says. What is
//! left to the caller is drawing it, and the two things it owns that this cannot:
//! the on-screen keyboard a rename needs, and where Back goes.
//!
//! Nothing here mentions Slint. `view()` returns plain data, which each binary maps
//! onto its own compiled components.

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::boot::{self, Kernels, Listing, Profile, Space};
use crate::key::{FlipperKey, KeyEvent};
use crate::theme::{metric::SPIN_FRAMES, timing::SPIN_FRAME_MS};

/// How often the partition table is compared against the one the list was read from.
const PARTS_POLL: Duration = Duration::from_millis(500);

/// How long a boot waits for an arm that is still loading before going ahead without it.
/// The load measures 2.4s on this board; the rest is room for a machine under load.
const ARM_WAIT: Duration = Duration::from_secs(6);

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
    /// What the View key opens: the profile's facts, and the way into Config.
    View,
    /// What this profile boots with: the four hardware lines that are not built yet,
    /// and the kernel it boots.
    Config,
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
    /// 0 a plain line, 1 an action, 2 a line carrying a value on the right.
    pub kind: i32,
    /// Where it sits in the popup's body, measured from the top. Computed here because
    /// one popup mixes pitches -- the View screen carries facts at the Info pitch and a
    /// Config action at the taller action pitch -- and Slint cannot add up a model.
    pub y: f32,
    pub text: String,
    /// What a kind-2 line shows on the right. Spun with Left and Right while the line
    /// is the selected one, as the menu's own settings rows are.
    pub value: String,
    pub selected: bool,
    pub heart: bool,
}

/// The widest value a settings row of this profile can hold.
///
/// Not the value showing: the kernel line spins through every kernel the profile has,
/// and a popup measured from whichever one is up would grow and shrink as somebody
/// walked through them, moving the frame under their eyes. Measured across all of them
/// once, so the frame is the size it needs and then stays put.
fn widest_value(p: &Profile) -> u16 {
    use crate::font::TITLE;
    p.entries
        .iter()
        .map(|e| TITLE.text_width(if e.short.is_empty() { &e.version } else { &e.short }))
        .max()
        .unwrap_or(0)
}

/// How tall a popup line of each kind is, matching what boot.slint draws.
fn line_h(kind: i32) -> f32 {
    use crate::theme::metric::{POPUP_LINE_H, POPUP_ROW_H};
    if kind == 0 { POPUP_LINE_H as f32 } else { POPUP_ROW_H as f32 }
}

/// Which of `profile`'s entries names the kernel `running`.
///
/// Only for the profile that is actually running: another profile's rows say nothing
/// about this machine's kernel, and its own first entry is what it would boot. Falls
/// back to the first entry when the running kernel has no entry here at all, which is
/// a profile booted from an entry that has since been removed.
fn kernel_base_of(profile: &Profile, running: &str) -> usize {
    if !profile.booted || running.is_empty() {
        return 0;
    }
    profile
        .entries
        .iter()
        .position(|e| e.version == running)
        .unwrap_or(0)
}

/// The Config screen, top to bottom.
///
/// Only Kernel works. The four above it are the hardware groups this screen is going to
/// hold, listed so the shape is visible and dimmed because neither of the two things
/// they need exists yet: a catalog shipped with the kernel that maps a `.dtbo` to a
/// category and a readable label, and a way to enable a unit inside a profile that is
/// not running. The profile's facts are not here: they are the View screen this opens
/// from.
const CONFIG_LINES: [&str; 5] = [
    "GPIO",
    "Hardware config",
    "Video Out",
    "Services",
    "Kernel",
];
const CONFIG_KERNEL: usize = 4;

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
    /// How tall its rows come to, measured the same way: they are not all one pitch.
    pub popup_body_h: f32,
    /// The width the size value's slot holds while it is still a spinner, so the
    /// line does not change width as the value lands.
    pub size_slot_w: f32,
    /// The five soft keys, as the prototype labels them on this screen.
    pub buttons: [&'static str; 5],
}

/// Whether a row can show its status text beside its name.
///
/// A derived profile's name is its label in brackets and can be long, while the status is
/// right-aligned against the other edge, so on the widest rows the two would meet. When
/// they would, the status goes: it is the half that says least about which profile this
/// is, and the View screen has it in full either way.
///
/// Measured in HaxrCorp 4090, which is what a boot row is drawn in: this screen keeps one
/// font in every state, unlike the menu's rows, which swap to Born2bSportyV2 when active.
///
/// The heart or the medium badge sits right after the name, so it is part of what has to
/// fit; only one of the two is ever drawn.
pub fn status_fits(label: &str, status: &str, auto: bool, medium: boot::Medium) -> bool {
    use crate::font::TITLE;
    use crate::theme::metric::{
        BOOT_ICON_BOX_W, BOOT_SD_W, BOOT_TEXT_GAP, ICON_PAD, STATUS_PAD_R,
    };

    if status.is_empty() {
        return true;
    }
    let badge = if auto {
        BADGE_GAP + HEART_W
    } else if medium != boot::Medium::Internal {
        BADGE_GAP + BOOT_SD_W
    } else {
        0
    };
    let label_x = ICON_PAD + BOOT_ICON_BOX_W + BOOT_TEXT_GAP;
    let used = label_x
        + i32::from(TITLE.text_width(label))
        + badge
        + GAP
        + i32::from(TITLE.text_width(status))
        + STATUS_PAD_R;
    used <= crate::PANEL_W as i32
}

/// The gap boot.slint leaves between a label and the badge after it, and the least this
/// leaves between either of them and the status.
const BADGE_GAP: i32 = 3;
const GAP: i32 = 4;
/// The auto-start heart, as boot.slint draws it.
const HEART_W: i32 = 7;

/// The kernel's view of what block devices exist, as the cheapest thing that changes when
/// one arrives: a few hundred bytes, read straight from procfs.
fn partitions() -> String {
    std::fs::read_to_string("/proc/partitions").unwrap_or_default()
}

/// The boot menu's state.
pub struct BootMenu {
    /// The rows: one per profile, each carrying its entries in boot order, so
    /// `entries[0]` is the kernel that row boots. A profile whose every kernel is
    /// hidden has no entries and no row, and is still here for a name check.
    profiles: Vec<Profile>,
    /// Which profile boots when nobody presses anything, as `boot::listing` decides.
    first: Option<usize>,
    pending: Option<Receiver<Listing>>,
    /// Which kernels have rows, as the caller was told on its command line.
    kernels: Kernels,
    selected: i32,
    /// When the countdown started, and whether a key has stopped it.
    started: Option<Instant>,
    cancelled: bool,
    auto_start: AutoStart,
    booting: Option<(String, Receiver<Result<bool, String>>)>,
    /// An arm in flight: the image for the marked profile being loaded ahead of the
    /// boot that would ask for it. Carries nothing but its finish, since what was
    /// loaded is recorded by the tool and checked there.
    arming: Option<Receiver<Result<(), String>>>,
    /// Whether the marked profile's image has been asked for, so it is asked for once.
    armed: bool,
    /// What the kernel's partition table looked like when the list was last read, and
    /// when it was last compared, so a drive that arrives late is noticed.
    parts: String,
    parts_polled: Instant,
    /// The table changed at the last look, so the next one decides whether it has settled.
    /// Devices arrive in pieces -- the disk, then its partitions, and a USB drive later
    /// still -- and a list read costs a mount and a subvolume walk per filesystem, so it
    /// is worth waiting one more look rather than paying that three times per insertion.
    parts_settling: bool,
    /// The profile the cursor was on across a re-read nobody asked for, so it lands
    /// back on it rather than wherever the new list happens to put that row.
    keep: Option<(String, String)>,
    popup: Option<Popup>,
    popup_index: usize,
    /// Where the kernel spinner stands, as an index into the selected profile's
    /// entries, while it is somewhere other than the kernel that profile boots.
    ///
    /// A spin is not a write: walking past three kernels would otherwise rewrite the
    /// profile's entries and put each one on trial in turn. The value is committed when
    /// the line is left -- by OK, by Back, or by moving off it -- so one choice is one
    /// write.
    kernel_pick: Option<usize>,
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
    /// Where the spinners count their frames from.
    spin_at: Instant,
    /// How many rows the list can show at once, from the caller's own metrics.
    visible: i32,
}

impl BootMenu {
    /// Open the menu: the read starts now, on a thread, because listing the profiles
    /// walks every subvolume on every filesystem and the screen has to appear first.
    pub fn open(visible: i32, auto_start: AutoStart, kernels: Kernels) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let started = std::thread::Builder::new()
            .name("boot-listing".into())
            .spawn(move || {
                let _ = tx.send(boot::listing(kernels));
            })
            .is_ok();
        Self {
            profiles: Vec::new(),
            first: None,
            pending: started.then_some(rx),
            kernels,
            selected: 0,
            started: None,
            cancelled: false,
            auto_start,
            booting: None,
            arming: None,
            armed: false,
            parts: partitions(),
            parts_polled: Instant::now(),
            parts_settling: false,
            keep: None,
            popup: None,
            popup_index: 0,
            kernel_pick: None,
            space: None,
            space_rx: None,
            space_done: false,
            measured: HashMap::new(),
            space_key: None,
            spin_at: Instant::now(),
            visible: visible.max(1),
        }
    }

    /// Put the cursor on the profile the countdown would boot.
    ///
    /// The highlighted row is what a person reads as "this is what happens next", so it has
    /// to be the marked one rather than whatever sorts first: otherwise the menu shows one
    /// row and boots another when the countdown ends, and pressing OK on what looks
    /// selected boots something else again. Nothing marked leaves the cursor alone, and so
    /// does a list that arrived after somebody already started moving around in it.
    fn select_marked(&mut self) {
        if self.cancelled {
            return;
        }
        if let Some(at) = self.first {
            self.selected = at as i32;
        }
    }

    /// Load the image of the profile that boots by itself, so the boot that follows
    /// does not have to.
    ///
    /// That profile and no other: the kernel holds one image at a time, so arming
    /// anything else would throw this one away, and it is both what a countdown boots
    /// by itself and what somebody opening this list most often picks.
    /// Where it can be pivoted into, which is usually the case for the running profile,
    /// the tool loads nothing and this costs a decision.
    fn arm_marked(&mut self) {
        // One at a time: a re-read allows this again, and a load still running would
        // otherwise be joined by a second one, which waits out the kexec lock and then
        // loads the same image over again.
        if self.armed || self.arming.is_some() {
            return;
        }
        let Some(p) = self.first.and_then(|at| self.profiles.get(at)).cloned() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("boot-arm".into())
            .spawn(move || {
                let _ = tx.send(boot::arm(&p));
            })
            .is_ok()
        {
            self.arming = Some(rx);
            self.armed = true;
        }
    }

    /// Read the drives again because one appeared, without moving anything the user is
    /// looking at.
    ///
    /// The card that arrives 2.7s into a boot is the case: enumerating an UHS-I card takes
    /// longer than this menu takes to draw, so a list read once can be a list read too
    /// early, and after a warm reboot it loses that race more often, since the card has to
    /// be brought back down from 1.8V signalling first. Someone pushing a card in while the
    /// menu sits there is the same thing, later.
    ///
    /// Nothing about the screen is reset: the cursor comes back to the profile it was on,
    /// the countdown keeps whatever time it had left, and a popup or a boot already under
    /// way defers this rather than being interrupted. The list itself changes, which is the
    /// whole point, and a row appearing above the cursor is what the restore is for.
    fn drives_changed(&mut self) {
        if self.popup.is_some() || self.booting.is_some() || self.pending.is_some() {
            return;
        }
        crate::logline!("boot menu      the drives changed, reading them again");
        self.keep = self
            .selected_profile()
            .map(|p| (p.name.clone(), p.dev.clone()));
        let again = Self::open(self.visible, self.auto_start, self.kernels);
        self.pending = again.pending;
        self.armed = false;
    }

    /// Read the drives again, from nothing: the list, the marker and the cursor.
    ///
    /// What it is for is a drive that was not there a moment ago. A card can be pushed in
    /// while this screen is up, and nothing else in the program goes looking for it, so a
    /// list read once at startup would show a machine that no longer exists.
    ///
    /// Prefetching is allowed again, because the marker may now name a different profile
    /// (a card carries its own), and prefetching the one already loaded costs a decision
    /// and no load.
    pub fn reread(&mut self) {
        crate::logline!("boot menu      reading the drives again");
        let again = Self::open(self.visible, self.auto_start, self.kernels);
        self.profiles = Vec::new();
        self.first = None;
        self.pending = again.pending;
        self.selected = 0;
        self.popup_index = 0;
        self.armed = false;
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
            // Boot the one under the cursor, with the kernel its row shows. Nothing
            // else acts on a choice: U-Boot boots the boot menu itself and never lists
            // these entries, so this and the countdown are the only ways over.
            FlipperKey::Ok | FlipperKey::Run if count > 0 => self.boot_selected(),
            // View is slot 1 and Edit slot 3, the two labelled keys.
            FlipperKey::View if count > 0 => self.open_view(),
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
            Popup::Config => return self.config_key(key, &profile),
            // The facts, and one line that leads on: OK follows it, Back and Escape
            // leave, and anything else leaves too, as this screen always has.
            Popup::View => match key {
                // Right as well as OK: the row ends in a chevron, and a chevron is a
                // promise that Right does something.
                FlipperKey::Ok | FlipperKey::Run | FlipperKey::Right => {
                    self.popup = Some(Popup::Config);
                    self.popup_index = 0;
                    self.kernel_pick = None;
                }
                FlipperKey::Escape | FlipperKey::Back | FlipperKey::Edit => {}
                _ => self.popup = Some(Popup::View),
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

    /// Which entry the spinner is on: what has been picked, or where it starts.
    fn kernel_at(&self, profile: &Option<Profile>) -> usize {
        self.kernel_pick.unwrap_or_else(|| {
            profile.as_ref().map_or(0, |p| self.kernel_base(p))
        })
    }

    /// A key on the Config screen.
    ///
    /// Up and Down move; OK opens what a line opens; Left and Right spin the value of a
    /// line that has one, which is the kernel. Leaving the kernel line at all commits
    /// what the spinner shows, so a choice is one write however it was arrived at.
    fn config_key(&mut self, key: FlipperKey, profile: &Option<Profile>) -> Outcome {
        let lines = CONFIG_LINES.len();
        let kernels = profile.as_ref().map_or(0, |p| p.entries.len());
        self.popup = Some(Popup::Config);
        match key {
            FlipperKey::Down => {
                let commit = self.popup_index == CONFIG_KERNEL;
                self.popup_index = (self.popup_index + 1) % lines;
                if commit {
                    return self.commit_kernel(profile);
                }
            }
            FlipperKey::Up => {
                let commit = self.popup_index == CONFIG_KERNEL;
                self.popup_index = (self.popup_index + lines - 1) % lines;
                if commit {
                    return self.commit_kernel(profile);
                }
            }
            // One kernel installed is a value to read, not a choice to make.
            FlipperKey::Right if self.popup_index == CONFIG_KERNEL && kernels > 1 => {
                let at = self.kernel_at(profile);
                self.kernel_pick = Some((at + 1) % kernels);
            }
            FlipperKey::Left if self.popup_index == CONFIG_KERNEL && kernels > 1 => {
                let at = self.kernel_at(profile);
                self.kernel_pick = Some((at + kernels - 1) % kernels);
            }
            FlipperKey::Ok | FlipperKey::Run => match self.popup_index {
                CONFIG_KERNEL => return self.commit_kernel(profile),
                // The four hardware lines: listed, and honest about it.
                at => {
                    self.popup = Some(Popup::Said(format!(
                        "{} is not configurable yet",
                        CONFIG_LINES.get(at).copied().unwrap_or("that")
                    )));
                }
            },
            FlipperKey::Escape | FlipperKey::Back => {
                let leaving = self.commit_kernel(profile);
                if self.popup.as_ref().is_some_and(|p| matches!(p, Popup::Config)) {
                    // Nothing to commit, so Back goes up to View, which opened this.
                    self.popup = Some(Popup::View);
                    self.popup_index = 0;
                }
                return leaving;
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// Where the kernel spinner starts, and what its line shows before it is touched.
    ///
    /// The kernel this profile is *running*, for the profile that is running, and
    /// otherwise the one it boots. Not always the first entry: installing a kernel
    /// writes its entry at rank 0, so a machine that has taken an update is running
    /// one kernel and set to boot another. Opening on the one it would boot said the
    /// running kernel had already been changed away from, and offered no way to see
    /// what it is running.
    fn kernel_base(&self, profile: &Profile) -> usize {
        kernel_base_of(profile, boot::running_kernel())
    }

    /// Write the kernel the spinner shows, if it is not the one already booting.
    ///
    /// Clearing the pick first: the write is what makes it true, and a pick left behind
    /// a failed write would show a choice the entries do not have.
    fn commit_kernel(&mut self, profile: &Option<Profile>) -> Outcome {
        let Some(at) = self.kernel_pick.take() else {
            return Outcome::Stay;
        };
        if at == 0 {
            return Outcome::Stay;
        }
        let Some(p) = profile.clone() else { return Outcome::Stay };
        let Some(entry) = p.entries.get(at).cloned() else { return Outcome::Stay };
        crate::logline!("boot menu      {} is to boot {}", p.name, entry.id);
        let (tx, rx) = std::sync::mpsc::channel();
        self.popup = match std::thread::Builder::new()
            .name("boot-kernel".into())
            .spawn(move || {
                let _ = tx.send(boot::set_kernel(&p.dev, &entry.id).map(|()| false));
            }) {
            Ok(_) => Some(Popup::Busy("Saving".into(), Some(rx))),
            Err(_) => Some(Popup::Said("could not start the change".into())),
        };
        Outcome::Stay
    }

    /// What the kernel line shows: the spinner's kernel while one is picked, else the
    /// kernel this profile boots. Empty for a profile with none.
    ///
    /// See `widest_value` for why the popup is not measured from this.
    fn kernel_value(&self, profile: &Profile) -> String {
        let at = self.kernel_pick.unwrap_or_else(|| self.kernel_base(profile));
        match profile.entries.get(at) {
            Some(e) if e.short.is_empty() => e.version.clone(),
            Some(e) => e.short.clone(),
            None => String::new(),
        }
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

    /// Open View, and start the read its size line needs.
    ///
    /// On a thread because it shells out to a tool that mounts the top level and then
    /// walks the subvolume, which is seconds. The overlays it also shows need no read:
    /// they were parsed from the entry when the list was.
    fn open_view(&mut self) {
        self.popup = Some(Popup::View);
        self.popup_index = 0;
        self.kernel_pick = None;
        self.space = None;
        self.space_done = false;
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
    }

    /// Open one of the screens the soft keys reach: Config, or Edit.
    fn open_popup(&mut self, which: Popup) {
        self.popup = Some(which);
        self.popup_index = 0;
        self.kernel_pick = None;
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
            // Which moves a digit in this profile's entries: what boots by itself is
            // the first entry there is, not a marker naming one.
            "Auto Start" => {
                let name = p.name.clone();
                (
                    "Saving",
                    Box::new(move || boot::set_auto_start(&p.dev, &name).map(|_| false)),
                )
            }
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
        crate::logline!(
            "boot menu      boot {} ({}) on {}",
            p.name,
            p.entries.first().map_or("no kernel", |e| e.id.as_str()),
            if p.dev.is_empty() { "the booted filesystem" } else { p.dev.as_str() }
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let label = boot::display_name(&p.name);
        // An arm still loading is this boot's own image being made ready, and the kernel
        // takes one caller at a time: starting the boot now means loading it again behind
        // the first load. Wait for it instead, on the boot's thread so the panel keeps
        // painting, and then hand over with nothing left to load. Bounded because a wait
        // that never ends would be a menu that never boots.
        let arming = self.arming.take();
        if std::thread::Builder::new()
            .name("boot-now".into())
            .spawn(move || {
                if let Some(rx) = arming {
                    if rx.recv_timeout(ARM_WAIT).is_err() {
                        crate::logline!("boot menu      gave up waiting for the arm, booting anyway");
                    }
                }
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

        // Every block device the kernel knows, so an SD card, a USB drive, a reader that
        // took its time and a drive pulled out are all the same event. Cheap enough to keep
        // doing for as long as the menu is up: a few hundred bytes of procfs every 500ms.
        if self.parts_polled.elapsed() >= PARTS_POLL {
            self.parts_polled = Instant::now();
            let now = partitions();
            if now != self.parts {
                self.parts = now;
                self.parts_settling = true;
            } else if self.parts_settling {
                self.parts_settling = false;
                self.drives_changed();
            }
        }

        if let Some(rx) = self.pending.as_ref() {
            match rx.try_recv() {
                Ok(listing) => {
                    crate::logline!(
                        "boot menu      {} profiles, {} entries",
                        listing.profiles.len(),
                        listing.profiles.iter().map(|p| p.entries.len()).sum::<usize>()
                    );
                    self.profiles = listing.profiles;
                    self.first = listing.first;
                    self.pending = None;
                    match self.keep.take() {
                        // A re-read nobody asked for: back onto the same profile, or as
                        // close as the shorter list allows if it is gone.
                        Some((name, dev)) => {
                            match self
                                .profiles
                                .iter()
                                .position(|p| p.name == name && p.dev == dev)
                            {
                                Some(at) => self.selected = at as i32,
                                None => {
                                    let last = self.profiles.len().saturating_sub(1) as i32;
                                    self.selected = self.selected.clamp(0, last.max(0));
                                }
                            }
                        }
                        None => self.select_marked(),
                    }
                    moved = true;
                    // The countdown only runs when there is something for it to
                    // boot: it boots the row wearing the heart, so with nothing marked
                    // -- including a mark on an entry that is gone -- there is nothing
                    // to count down to and the menu waits. Started when the list lands
                    // rather than when the screen opens, because counting down against
                    // an empty list is a race.
                    if self.auto_start == AutoStart::Countdown
                        && !self.cancelled
                        && self.started.is_none()
                        && self.first.is_some()
                    {
                        self.started = Some(Instant::now());
                        if let Some(p) = self.first.and_then(|at| self.profiles.get(at)) {
                            crate::logline!(
                                "boot menu      countdown {}s to {} ({})",
                                TIMEOUT.as_secs(),
                                p.name,
                                p.entries.first().map_or("no kernel", |e| e.id.as_str())
                            );
                        }
                    }
                    // Not part of the countdown: flipctl opens this list without one and
                    // still boots the marked profile more often than any other. Loading
                    // starts as the list lands rather than near a deadline, since the
                    // whole point is to spend the wait on it, and a boot asked for while
                    // it runs waits out the kexec lock either way.
                    self.arm_marked();
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
        // Nothing on screen turns on this: an arm is a boot made faster, not an answer.
        // It is collected only so a failure is said out loud, and so the thread's end is
        // noticed rather than left in flight for the life of the menu.
        if let Some(rx) = self.arming.as_ref() {
            match rx.try_recv() {
                Ok(res) => {
                    self.arming = None;
                    if let Err(e) = res {
                        crate::logline!("boot menu      arming failed: {e}");
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.arming = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
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
            self.reread();
        }

        // Time up: boot the first entry there is. The countdown is cleared first, so a boot
        // that fails leaves the menu sitting there rather than trying again every
        // turn.
        if let Some(at) = self.started {
            if !self.cancelled && self.booting.is_none() && at.elapsed() >= TIMEOUT {
                self.started = None;
                self.cancelled = true;
                if let Some(at) = self.first {
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
                let label = boot::display_name(&p.name);
                let status = boot::used_ago(&p.last_used, now);
                Row {
                    status: if status_fits(&label, &status, p.auto_boot, p.medium) {
                        status
                    } else {
                        String::new()
                    },
                    label,
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
        // The kernel this profile boots, which every screen below describes.
        let entry = profile.entries.first().cloned().unwrap_or_default();
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
        let plain = |text: String| PopupLine {
            kind: 0,
            y: 0.0,
            text,
            value: String::new(),
            selected: false,
            heart: false,
        };
        let joined = |v: &Vec<String>| if v.is_empty() { "none".to_string() } else { v.join(" ") };

        match self.popup.as_ref() {
            Some(Popup::View) => {
                // Where it is. First, because on a machine with a card in it two
                // profiles can carry the same name, and this is what says which one.
                if !profile.disk.is_empty() {
                    lines.push(plain(format!("Drive: {} ({})", profile.disk, profile.kind)));
                }
                // The kernel this line is about: the one the profile is running where
                // it is the running profile, else the one it would boot. The state in
                // brackets has to describe the version beside it, and for a profile
                // that is up those are two different kernels the moment a newer one is
                // installed: the installed one has never booted, while the one carrying
                // the session plainly has. The kernel it will boot next is the Kernel
                // line in Config, which is where choosing it happens.
                let shown = if profile.booted {
                    profile
                        .entries
                        .iter()
                        .find(|e| e.version == boot::running_kernel())
                        .unwrap_or(&entry)
                } else {
                    &entry
                };
                // The kernel that is running is good, whatever its counter says: it
                // booted, and the machine is the proof. Its entry can still carry a
                // counter -- a kernel reinstalled or handed new overlays goes back on
                // trial, and the boot that would have cleared it has already happened
                // -- but "untried" about the kernel underneath the screen is wrong.
                //
                // "good" is said here and nowhere else, because here it is known.
                // An entry with no counter at all says nothing: the spec makes an entry
                // good by REMOVING its counter, so one a boot blessed and one that was
                // never counted are the same file, and set-boot-order --list prints a
                // dash for both.
                let state = if profile.booted && shown.version == boot::running_kernel() {
                    Some("good")
                } else {
                    shown.state()
                };
                lines.push(plain(format!(
                    "Kernel: {}",
                    match (shown.version.as_str(), state) {
                        ("", _) => "-".to_string(),
                        (version, None) => version.to_string(),
                        (version, Some(state)) => format!("{version} ({state})"),
                    }
                )));
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
                // Read from the entry itself, so they are the overlays of the kernel
                // this profile boots rather than of whichever entry a second look picked.
                lines.push(plain(format!("DTBO system: {}", joined(&entry.system))));
                lines.push(plain(format!("DTBO user: {}", joined(&entry.user))));
                // The way on, drawn as the current action because it is the only
                // one: the facts above it cannot be selected, so there is nothing for
                // Up and Down to move between.
                lines.push(PopupLine {
                    kind: 1,
                    y: 0.0,
                    text: "Config".into(),
                    value: String::new(),
                    selected: true,
                    heart: false,
                });
            }
            Some(Popup::Config) => {
                for (i, line) in CONFIG_LINES.iter().enumerate() {
                    let kernel = i == CONFIG_KERNEL;
                    lines.push(PopupLine {
                        // All one kind: a setting, its name on the left and its value on
                        // the right. The four that are not built yet have no value to
                        // show; when they do, it goes in the same grey the kernel's
                        // version is in.
                        kind: 2,
                        y: 0.0,
                        text: (*line).to_string(),
                        value: if kernel { self.kernel_value(&profile) } else { String::new() },
                        selected: i == self.popup_index,
                        heart: false,
                    });
                }
            }
            Some(Popup::Edit) => {
                for (i, action) in boot::edit_actions(&profile).iter().enumerate() {
                    lines.push(PopupLine {
                        kind: 1,
                        y: 0.0,
                        text: (*action).to_string(),
                        value: String::new(),
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
        // Each row's offset, and what they come to: two pitches in one popup.
        let mut popup_body_h = 0.0f32;
        for line in &mut lines {
            line.y = popup_body_h;
            popup_body_h += line_h(line.kind);
        }

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
            // The body is whichever of the two lists is showing. A settings row is its
            // name, a gap, the widest value it can hold and the chevrons either side --
            // the WIDEST, not the one showing, so spinning through a profile's kernels
            // does not resize the popup under the person doing the spinning. Same
            // reason the size line keeps a fixed slot while it is still a spinner.
            let mut body = 0u16;
            for line in &lines {
                let mut w = TITLE.text_width(&line.text);
                if line.kind == 2 && !line.value.is_empty() {
                    w += SIZE_GAP as u16
                        + widest_value(&profile)
                        + 2 * (SIZE_GAP as u16 + TITLE.text_width(">"));
                }
                body = body.max(w);
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
            spin_frame: (frame % SPIN_FRAMES as u128) as i32,
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
            popup_body_h,
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
            // Config on slot 2 and Edit on slot 4, which is where boot_menu.js puts
            // its two labels: the outer slots stay empty on this screen. Once a boot is
            // under way `key` answers nothing, so the labels go with it rather than
            // offer two presses that do not happen.
            buttons: if self.booting.is_some() {
                ["", "", "", "", ""]
            } else {
                ["", "View", "", "Edit", ""]
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str) -> boot::Entry {
        boot::Entry {
            version: version.into(),
            ..Default::default()
        }
    }

    /// The Config screen opens on the kernel the machine is running, not on the one
    /// its profile would boot next. Installing a kernel writes its entry at rank 0,
    /// so the two differ on any machine that has taken an update.
    #[test]
    fn the_spinner_starts_on_the_running_kernel() {
        let updated = boot::Profile {
            booted: true,
            entries: vec![entry("7.3.0-new"), entry("7.2.0-running")],
            ..Default::default()
        };
        assert_eq!(kernel_base_of(&updated, "7.2.0-running"), 1);

        // Nothing installed since the boot: the running kernel is also the first.
        let settled = boot::Profile {
            booted: true,
            entries: vec![entry("7.2.0-running"), entry("7.1.0-old")],
            ..Default::default()
        };
        assert_eq!(kernel_base_of(&settled, "7.2.0-running"), 0);

        // Another profile's rows say nothing about this machine's kernel, even when
        // one of them happens to name it. Its own first entry is what it would boot.
        let other = boot::Profile {
            booted: false,
            entries: vec![entry("7.3.0-new"), entry("7.2.0-running")],
            ..Default::default()
        };
        assert_eq!(kernel_base_of(&other, "7.2.0-running"), 0);

        // Booted from an entry that has since been removed, and a kernel that could
        // not be read at all: the first entry either way rather than a panic.
        assert_eq!(kernel_base_of(&updated, "7.0.0-gone"), 0);
        assert_eq!(kernel_base_of(&updated, ""), 0);
    }
}
