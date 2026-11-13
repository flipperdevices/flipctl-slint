//! The battery pack as the fuel gauge last announced it.
//!
//! Every read of the gauge's sysfs is an I2C transaction: the bq27xxx driver caches
//! only capacity and flags, and goes to the chip for temperature, power, current and
//! the rest each time, on the bus the Type-C controller needs during an attach. So
//! nothing here reads it. The kernel builds each `power_supply` uevent from every
//! property at once, whoever listens, and that snapshot is kept.
//!
//! The gauge announces a change in capacity, flags or charging state, found on its
//! own poll or half a second after the charger reports one. Between those the
//! numbers stand still. The one read of the gauge is at startup, through its
//! `uevent` file, which costs what one announcement does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crate::netlink::Socket;

/// `NETLINK_KOBJECT_UEVENT`, and its one group: the kernel's own broadcast.
const KOBJECT_UEVENT: libc::c_int = 15;
const UEVENT_KERNEL: u32 = 1;

const SUPPLIES: &str = "/sys/class/power_supply";

/// A snapshot, by the property's sysfs name: `capacity`, `status`, `temp`, ...
pub type Props = HashMap<String, String>;

fn cell() -> &'static Mutex<Option<Arc<Props>>> {
    static LATEST: OnceLock<Mutex<Option<Arc<Props>>>> = OnceLock::new();
    LATEST.get_or_init(|| Mutex::new(None))
}

/// The pack's last snapshot, or None with no gauge. The first call starts the
/// listener and reads the gauge once.
pub fn latest() -> Option<Arc<Props>> {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(start);
    cell().lock().ok()?.clone()
}

/// One property of the last snapshot.
pub fn prop(name: &str) -> Option<String> {
    latest()?.get(name).cloned()
}

/// One property, as a number.
pub fn num(name: &str) -> Option<i64> {
    prop(name)?.parse().ok()
}

fn store(props: Option<Props>) {
    if let Ok(mut held) = cell().lock() {
        *held = props.map(Arc::new);
    }
}

fn start() {
    // Subscribed before the gauge is read, so an announcement in between is not lost.
    let socket = Socket::subscribe(KOBJECT_UEVENT, UEVENT_KERNEL);
    store(read_pack());
    let socket = match socket {
        Ok(socket) => socket,
        Err(e) => {
            eprintln!("battery        no uevents, not following: {e}");
            return;
        }
    };
    let spawned = thread::Builder::new().name("watch-battery".into()).spawn(move || {
        let mut buf = vec![0u8; 8192];
        loop {
            let got = match socket.recv(&mut buf) {
                Ok(got) => got,
                // A burst overran the socket: what was dropped is older than what
                // comes next, so carry on.
                Err(e) if e.raw_os_error() == Some(libc::ENOBUFS) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            match pack_event(&buf[..got]) {
                Some(Event::Changed(props)) => store(Some(props)),
                Some(Event::Removed) => store(None),
                None => {}
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("battery        no thread, not following: {e}");
    }
}

/// The gauge's directory: the one supply whose type is Battery. Its driver has been
/// called both bq28z610 and bq28z620. `type` comes from the driver's description,
/// not the chip.
fn pack_dir() -> Option<PathBuf> {
    std::fs::read_dir(SUPPLIES)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|dir| std::fs::read_to_string(dir.join("type")).is_ok_and(|t| t.trim() == "Battery"))
}

fn read_pack() -> Option<Props> {
    read_uevent_file(&pack_dir()?)
}

fn read_uevent_file(dir: &Path) -> Option<Props> {
    let text = std::fs::read_to_string(dir.join("uevent")).ok()?;
    Some(props(text.lines().map(str::as_bytes)))
}

/// `POWER_SUPPLY_CAPACITY=87` and the like, by sysfs name: `capacity`.
fn props<'a>(fields: impl Iterator<Item = &'a [u8]>) -> Props {
    fields
        .filter_map(|field| std::str::from_utf8(field).ok())
        .filter_map(|field| field.strip_prefix("POWER_SUPPLY_"))
        .filter_map(|field| field.split_once('='))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.to_string()))
        .collect()
}

#[derive(Debug, PartialEq)]
enum Event {
    Changed(Props),
    Removed,
}

/// What a uevent says about the pack, or None for anything else.
fn pack_event(message: &[u8]) -> Option<Event> {
    let fields = || message.split(|b| *b == 0);
    let has = |want: &[u8]| fields().any(|field| field == want);
    if !has(b"SUBSYSTEM=power_supply") || !has(b"POWER_SUPPLY_TYPE=Battery") {
        return None;
    }
    if has(b"ACTION=remove") {
        return Some(Event::Removed);
    }
    Some(Event::Changed(props(fields())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(fields: &[&str]) -> Vec<u8> {
        fields.iter().flat_map(|f| f.bytes().chain([0])).collect()
    }

    #[test]
    fn a_pack_announcement_is_its_properties() {
        let message = event(&[
            "change@/devices/platform/i2c/bq28z620-0/power_supply/bq28z620-0",
            "ACTION=change",
            "SUBSYSTEM=power_supply",
            "POWER_SUPPLY_NAME=bq28z620-0",
            "POWER_SUPPLY_TYPE=Battery",
            "POWER_SUPPLY_STATUS=Discharging",
            "POWER_SUPPLY_CAPACITY=87",
            "POWER_SUPPLY_TEMP=312",
            "POWER_SUPPLY_POWER_AVG=-2412000",
        ]);
        let Some(Event::Changed(p)) = pack_event(&message) else { panic!() };
        assert_eq!(p["capacity"], "87");
        assert_eq!(p["status"], "Discharging");
        assert_eq!(p["temp"], "312");
        assert_eq!(p["power_avg"], "-2412000");
        assert!(!p.contains_key("action"));
    }

    #[test]
    fn the_charger_and_other_subsystems_are_not_the_pack() {
        let charger = event(&[
            "ACTION=change",
            "SUBSYSTEM=power_supply",
            "POWER_SUPPLY_TYPE=USB",
            "POWER_SUPPLY_ONLINE=1",
        ]);
        let typec = event(&["ACTION=change", "SUBSYSTEM=typec"]);
        assert_eq!(pack_event(&charger), None);
        assert_eq!(pack_event(&typec), None);
        let gone = event(&["ACTION=remove", "SUBSYSTEM=power_supply", "POWER_SUPPLY_TYPE=Battery"]);
        assert_eq!(pack_event(&gone), Some(Event::Removed));
    }

    #[test]
    fn the_uevent_file_reads_the_same_way() {
        let dir = std::env::temp_dir().join(format!("flipper-battery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("uevent"),
            "POWER_SUPPLY_NAME=bq28z620-0\nPOWER_SUPPLY_TYPE=Battery\nPOWER_SUPPLY_CAPACITY=5\n",
        )
        .unwrap();
        let p = read_uevent_file(&dir).unwrap();
        assert_eq!((p["capacity"].as_str(), p["type"].as_str()), ("5", "Battery"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
