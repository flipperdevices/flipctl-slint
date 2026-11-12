//! The brightness of each LED group the MCU drives.
//!
//! The MCU scales every LED in a group by a group setting, 0 (off) to 255 (full),
//! which the LED driver exposes on its controller device. A change is written as it
//! is made, off the render loop, since the write goes through sudo. The MCU's own
//! menu can change the setting too, with no notice to the host, so a group with
//! nothing waiting is always read back from the MCU.

use std::path::PathBuf;
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread;

/// Where the LED driver binds the MCU's LED controller.
const DRIVER: &str = "/sys/bus/platform/drivers/flipper-one-leds";

/// The LED groups, in the order the screen lists them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Group {
    Link,
    Power,
    Wattmeter,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Link, Group::Power, Group::Wattmeter];

    fn file(self) -> &'static str {
        match self {
            Group::Link => "link_brightness",
            Group::Power => "power_brightness",
            Group::Wattmeter => "wattmeter_brightness",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The controller's directory, if the driver has one with the group settings.
fn device() -> Option<PathBuf> {
    std::fs::read_dir(DRIVER)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.join(Group::Link.file()).exists())
}

/// Whether this machine has the group settings at all.
pub fn available() -> bool {
    device().is_some()
}

/// A group's setting: the value waiting to be written, else the MCU's.
pub fn read(group: Group) -> Option<u8> {
    if let Some(v) = writer().held.lock().unwrap().values[group.index()] {
        return Some(v);
    }
    let text = std::fs::read_to_string(device()?.join(group.file())).ok()?;
    text.trim().parse().ok()
}

/// Write `value` for `group`.
pub fn set(group: Group, value: u8) {
    let w = writer();
    w.held.lock().unwrap().values[group.index()] = Some(value);
    w.wake.notify_one();
}

/// The setting for a percentage.
pub fn raw(percent: u8) -> u8 {
    ((u32::from(percent.min(100)) * 255 + 50) / 100) as u8
}

/// The percentage a setting shows as.
pub fn percent(raw: u8) -> u8 {
    ((u32::from(raw) * 100 + 127) / 255) as u8
}

/// The percentages left and right step through: 5% for a dim room, then tenths.
pub const STEPS: [u8; 12] = [0, 5, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100];

/// The next step from `raw`, up or down. A value set off the steps from the
/// MCU's menu moves to the nearest step in that direction.
pub fn step(raw_now: u8, up: bool) -> u8 {
    let steps = STEPS.iter().map(|&p| raw(p));
    if up {
        steps.into_iter().find(|&s| s > raw_now).unwrap_or(raw_now)
    } else {
        steps.rev().find(|&s| s < raw_now).unwrap_or(raw_now)
    }
}

/// How a setting reads on screen.
pub fn label(raw: u8) -> String {
    if raw == 0 {
        "Off".into()
    } else {
        format!("{}%", percent(raw))
    }
}

struct Held {
    values: [Option<u8>; 3],
}

struct Writer {
    held: Mutex<Held>,
    wake: Condvar,
}

/// The writer, and its thread on first use.
fn writer() -> &'static Writer {
    static WRITER: OnceLock<Writer> = OnceLock::new();
    let mut fresh = false;
    let w = WRITER.get_or_init(|| {
        fresh = true;
        Writer { held: Mutex::new(Held { values: [None; 3] }), wake: Condvar::new() }
    });
    if fresh {
        thread::spawn(move || flush(w));
    }
    w
}

/// Write held values. A value stays held until it is written, so a read in between
/// does not show the old one, and presses made during a write go out as one.
fn flush(w: &'static Writer) {
    let mut held = w.held.lock().unwrap();
    loop {
        if held.values.iter().all(Option::is_none) {
            held = w.wake.wait(held).unwrap();
            continue;
        }
        let values = held.values;
        drop(held);
        let dir = device();
        for (group, value) in Group::ALL.into_iter().zip(values) {
            let (Some(v), Some(dir)) = (value, &dir) else { continue };
            if let Err(e) = crate::system::write_sysfs(&dir.join(group.file()), &v.to_string()) {
                eprintln!("leds           {e}");
            }
        }
        held = w.held.lock().unwrap();
        for (slot, written) in held.values.iter_mut().zip(values) {
            if *slot == written {
                *slot = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_tenths() {
        let mut at = 0;
        let mut seen = vec![label(at)];
        while step(at, true) != at {
            at = step(at, true);
            seen.push(label(at));
        }
        assert_eq!(
            seen,
            ["Off", "5%", "10%", "20%", "30%", "40%", "50%", "60%", "70%", "80%", "90%", "100%"]
        );
        assert_eq!(at, 255);
        assert_eq!(step(0, false), 0);
    }

    #[test]
    fn off_step_value_moves_to_the_next_step() {
        assert_eq!(label(140), "55%");
        assert_eq!(label(step(140, true)), "60%");
        assert_eq!(label(step(140, false)), "50%");
    }

    #[test]
    fn round_trips() {
        for p in STEPS {
            assert_eq!(percent(raw(p)), p);
        }
    }
}
