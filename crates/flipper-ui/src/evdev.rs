//! The button source: raw evdev, no libinput.
//!
//! The MCU input driver registers `Flipper One Buttons` and `Flipper One Software
//! Buttons` with identical keycodes, one carrying physical presses and one
//! carrying injected ones, so a remote viewer's click and a finger are
//! indistinguishable downstream. Both are opened. The headset is ignored.
//!
//! The touchpad is a device of its own, `TouchpadSource` below: it reports
//! absolute axes rather than keys, and a consumer that wants buttons should not
//! have to filter them out.
//!
//! libinput is deliberately absent: it would drag libwacom and glib into a
//! maskrom-loaded initramfs to service thirteen buttons, and libxkbcommon would
//! compile a keymap for a device with no keyboard. Enumeration goes through sysfs
//! instead of udev, which is safe because these buttons are soldered on and
//! cannot hotplug.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::platform::{InputSource, Touch};
use crate::{FlipperKey, KeyEvent};

/// `struct input_event` on 64-bit Linux: a 16-byte timeval, then type, code and
/// value. Read in whole units; a short read means the device went away.
const EVENT_SIZE: usize = 24;
const EV_KEY: u16 = 1;

/// Devices whose names start and end like this are button devices.
const NAME_PREFIX: &str = "Flipper One";
const NAME_SUFFIX: &str = "Buttons";

pub struct EvdevSource {
    devices: Vec<File>,
    queue: std::collections::VecDeque<KeyEvent>,
    buf: [u8; EVENT_SIZE * 32],
}

impl EvdevSource {
    /// Open every Flipper One button device.
    ///
    /// Fails only when none is found; a device that exists but cannot be opened
    /// is skipped, so a permissions problem on one node does not take out the
    /// whole UI.
    pub fn open() -> std::io::Result<Self> {
        let paths = Self::find_devices()?;
        if paths.is_empty() {
            return Err(std::io::Error::other(format!(
                "no input device named {NAME_PREFIX} ... {NAME_SUFFIX}"
            )));
        }

        let devices: Vec<File> = paths
            .iter()
            .filter_map(|path| {
                OpenOptions::new().read(true).custom_flags(libc_o_nonblock()).open(path).ok()
            })
            .collect();

        if devices.is_empty() {
            return Err(std::io::Error::other(
                "found button devices but could not open any of them",
            ));
        }

        Ok(Self { devices, queue: std::collections::VecDeque::new(), buf: [0; EVENT_SIZE * 32] })
    }

    /// `/dev/input/eventN` for every `/sys/class/input/eventN/device/name` that
    /// looks like a Flipper One button device.
    fn find_devices() -> std::io::Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir("/sys/class/input")? {
            let entry = entry?;
            let node = entry.file_name();
            let Some(node) = node.to_str() else { continue };
            if !node.starts_with("event") {
                continue;
            }
            let Ok(name) = std::fs::read_to_string(entry.path().join("device/name")) else {
                continue;
            };
            let name = name.trim();
            if name.starts_with(NAME_PREFIX) && name.ends_with(NAME_SUFFIX) {
                found.push(PathBuf::from("/dev/input").join(node));
            }
        }
        found.sort();
        Ok(found)
    }

    /// Drain every device once, appending to the queue.
    fn drain(&mut self) {
        for i in 0..self.devices.len() {
            loop {
                let read = match self.devices[i].read(&mut self.buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    // WouldBlock is the normal way to find out there is nothing
                    // left; anything else means this device is done for now.
                    Err(_) => break,
                };
                for chunk in self.buf[..read].chunks_exact(EVENT_SIZE) {
                    let kind = u16::from_ne_bytes([chunk[16], chunk[17]]);
                    let code = u16::from_ne_bytes([chunk[18], chunk[19]]);
                    let value = i32::from_ne_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
                    if kind != EV_KEY {
                        continue;
                    }
                    // value 2 is auto-repeat. The UI wants held state, which it
                    // already has from the down event, so repeats are dropped.
                    let down = match value {
                        0 => false,
                        1 => true,
                        _ => continue,
                    };
                    if let Some(key) = FlipperKey::from_evdev(code) {
                        self.queue.push_back(KeyEvent { key, down });
                    }
                }
                if read < EVENT_SIZE {
                    break;
                }
            }
        }
    }

    /// Raw fds, for a caller that wants to block in `poll(2)` rather than spin.
    pub fn fds(&self) -> Vec<std::os::fd::BorrowedFd<'_>> {
        use std::os::fd::AsFd;
        self.devices.iter().map(|f| f.as_fd()).collect()
    }
}

impl InputSource for EvdevSource {
    fn poll(&mut self) -> Option<KeyEvent> {
        if self.queue.is_empty() {
            self.drain();
        }
        self.queue.pop_front()
    }
}

/// The pad beside the screen.
///
/// Single touch: the driver reports `ABS_X`, `ABS_Y` and `BTN_TOUCH` and has no
/// MT slots, so one finger is all there is to track and a report is a whole
/// position rather than a delta.
///
/// Absent on a board without one, which is why `open` returning an error is an
/// ordinary outcome and not a reason for anything to stop.
pub struct TouchpadSource {
    device: File,
    queue: std::collections::VecDeque<Touch>,
    buf: [u8; EVENT_SIZE * 32],
    /// The report being assembled. A packet is several events and then a
    /// `SYN_REPORT`, and only the whole of it means anything.
    at: Touch,
    /// What was last handed out, so a packet that changed nothing is dropped
    /// rather than waking the caller.
    sent: Option<Touch>,
}

const TOUCHPAD_NAME: &str = "Flipper One Touchpad";
const EV_SYN: u16 = 0;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const BTN_TOUCH: u16 = 0x14a;

impl TouchpadSource {
    pub fn open() -> std::io::Result<Self> {
        let mut path = None;
        for entry in std::fs::read_dir("/sys/class/input")? {
            let entry = entry?;
            let node = entry.file_name();
            let Some(node) = node.to_str() else { continue };
            if !node.starts_with("event") {
                continue;
            }
            let Ok(name) = std::fs::read_to_string(entry.path().join("device/name")) else {
                continue;
            };
            if name.trim() == TOUCHPAD_NAME {
                path = Some(PathBuf::from("/dev/input").join(node));
                break;
            }
        }
        let Some(path) = path else {
            return Err(std::io::Error::other(format!("no input device named {TOUCHPAD_NAME}")));
        };
        let device = OpenOptions::new().read(true).custom_flags(libc_o_nonblock()).open(path)?;
        Ok(Self {
            device,
            queue: std::collections::VecDeque::new(),
            buf: [0; EVENT_SIZE * 32],
            at: Touch { x: 0, y: 0, down: false },
            sent: None,
        })
    }

    /// Non-blocking. One item per report the pad actually changed something in.
    pub fn poll(&mut self) -> Option<Touch> {
        if self.queue.is_empty() {
            self.drain();
        }
        self.queue.pop_front()
    }

    fn drain(&mut self) {
        loop {
            let read = match self.device.read(&mut self.buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            for chunk in self.buf[..read].chunks_exact(EVENT_SIZE) {
                let kind = u16::from_ne_bytes([chunk[16], chunk[17]]);
                let code = u16::from_ne_bytes([chunk[18], chunk[19]]);
                let value = i32::from_ne_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
                match (kind, code) {
                    (EV_ABS, ABS_X) => self.at.x = value,
                    (EV_ABS, ABS_Y) => self.at.y = value,
                    (EV_KEY, BTN_TOUCH) => self.at.down = value != 0,
                    (EV_SYN, 0) => {
                        if self.sent != Some(self.at) {
                            self.sent = Some(self.at);
                            self.queue.push_back(self.at);
                        }
                    }
                    _ => {}
                }
            }
            if read < EVENT_SIZE {
                break;
            }
        }
    }

    /// The raw fd, for a caller that blocks in `poll(2)` over every source.
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.device.as_fd()
    }
}

/// `O_NONBLOCK`, without taking a dependency on libc for one constant.
const fn libc_o_nonblock() -> i32 {
    0o4000
}
