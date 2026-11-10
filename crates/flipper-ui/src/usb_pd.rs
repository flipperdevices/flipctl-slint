//! What the USB-C port offers a device that draws power from it.
//!
//! The device tree gives the port three sets of source capabilities and the kernel
//! lets userspace pick one through `select_usb_power_delivery`: 5V only, which is
//! the one in effect after reset because some displays refuse DisplayPort when a
//! source offers more; up to 18W, which the battery sustains until it is empty; and
//! up to 27W, which heats the pack to its cut-off in about twenty minutes. The sets
//! and their PDOs are read from sysfs rather than written down here, so a device
//! tree that changes them changes the labels with it.
//!
//! The port prefers to be a sink, so a phone plugged in starts out charging the
//! Flipper. Choosing a set therefore also asks for the source role, except from a
//! charger, which is the one partner a person never wants to power. A mains-powered
//! source counts as one even when it could take power: a monitor or a dock.

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crate::netlink::{Socket, Stop, Stopper, Wake};

/// The port, as the typec class names it.
pub const PORT: &str = "/sys/class/typec/port0";
/// Whatever is plugged into it, present only while something is.
pub const PARTNER: &str = "/sys/class/typec/port0-partner";
/// Where the capability sets live, by the names `select_usb_power_delivery` lists.
pub const SETS: &str = "/sys/class/usb_power_delivery";

/// Above this a set outruns the battery's cooling: 18W is what the device tree
/// derates every voltage to so that the pack runs empty before it gets too hot.
const SUSTAINED_MW: u32 = 18_000;

/// One fixed supply a set offers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Pdo {
    pub mv: u32,
    pub ma: u32,
}

impl Pdo {
    pub fn mw(self) -> u32 {
        self.mv * self.ma / 1000
    }
}

/// One capability set: its name in sysfs and the fixed supplies it offers as a
/// source. Variable supplies are left out, since the fixed ones already reach
/// the same voltages at the same or higher current.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Set {
    pub name: String,
    pub pdos: Vec<Pdo>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Role {
    Source,
    Sink,
}

/// What is plugged in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Partner {
    /// It negotiated a PD contract. Without one it only ever sees 5V, whatever
    /// set is selected, so there is nothing to ask about.
    pub pd: bool,
    /// Whether its own source capabilities say it can also sink. None when it
    /// sent none, which is a partner that is not offering us power.
    pub dual_role: Option<bool>,
    /// Whether those capabilities say it has power from outside, the mains.
    pub mains: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    pub sets: Vec<Set>,
    /// Index into `sets`.
    pub selected: usize,
    pub role: Role,
    pub partner: Option<Partner>,
}

impl Port {
    /// The port as it stands, or None on a kernel or device tree without
    /// selectable capability sets.
    pub fn read() -> Option<Port> {
        read_at(Path::new(PORT), Path::new(PARTNER), Path::new(SETS))
    }

    /// The Flipper is being charged by something that only supplies power or
    /// runs off the mains: a wall charger, a monitor, a dock, or a plain Type-C
    /// source with no PD at all.
    pub fn charger(&self) -> bool {
        self.role == Role::Sink
            && self.partner.as_ref().is_some_and(|p| p.dual_role != Some(true) || p.mains)
    }

    /// Whether to ask which set to offer: a PD partner that is not a charger.
    pub fn worth_asking(&self) -> bool {
        self.partner.as_ref().is_some_and(|p| p.pd) && !self.charger()
    }
}

/// `[pd0] pd1 pd2` as the names and the index of the bracketed one.
pub fn parse_choice(text: &str) -> Option<(Vec<String>, usize)> {
    let mut selected = None;
    let names: Vec<String> = text
        .split_whitespace()
        .enumerate()
        .map(|(i, word)| match word.strip_prefix('[').and_then(|w| w.strip_suffix(']')) {
            Some(inner) => {
                selected = Some(i);
                inner.to_string()
            }
            None => word.to_string(),
        })
        .collect();
    Some((names, selected?))
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// `5000mV` or `3000mA` as the number.
fn milli(text: &str) -> Option<u32> {
    text.trim_end_matches(|c: char| c.is_ascii_alphabetic()).parse().ok()
}

/// The fixed supplies in one `source-capabilities` directory, in PDO order.
fn fixed_pdos(dir: &Path) -> Vec<Pdo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(u32, Pdo)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let (index, kind) = name.split_once(':')?;
            if kind != "fixed_supply" {
                return None;
            }
            let path = e.path();
            let mv = milli(&read(&path.join("voltage"))?)?;
            let ma = milli(&read(&path.join("maximum_current"))?)?;
            Some((index.parse().ok()?, Pdo { mv, ma }))
        })
        .collect();
    found.sort_by_key(|(index, _)| *index);
    found.into_iter().map(|(_, pdo)| pdo).collect()
}

fn read_partner(partner: &Path) -> Option<Partner> {
    if !partner.is_dir() {
        return None;
    }
    let pd = read(&partner.join("supports_usb_power_delivery")).is_some_and(|v| v == "yes");
    let pdo = partner.join("usb_power_delivery/source-capabilities/1:fixed_supply");
    let dual_role = read(&pdo.join("dual_role_power")).map(|v| v == "1");
    let mains = read(&pdo.join("unconstrained_power")).is_some_and(|v| v == "1");
    Some(Partner { pd, dual_role, mains })
}

/// `read`, against any three directories, so the tests can build a port of
/// their own.
pub fn read_at(port: &Path, partner: &Path, sets: &Path) -> Option<Port> {
    let (names, selected) = parse_choice(&read(&port.join("select_usb_power_delivery"))?)?;
    let sets: Vec<Set> = names
        .into_iter()
        .map(|name| {
            let pdos = fixed_pdos(&sets.join(&name).join("source-capabilities"));
            Set { name, pdos }
        })
        .collect();
    let role = match parse_choice(&read(&port.join("power_role"))?)? {
        (names, at) if names.get(at).is_some_and(|n| n == "source") => Role::Source,
        _ => Role::Sink,
    };
    Some(Port { sets, selected, role, partner: read_partner(partner) })
}

/// A count of thousandths, as the shortest decimal: 1350 is "1.35", 3000 is "3".
fn decimal(milli: u32) -> String {
    let text = format!("{}.{:03}", milli / 1000, milli % 1000);
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

impl Set {
    /// The supply that delivers the most power, and the highest voltage of those:
    /// what the set is good for, in one entry.
    fn best(&self) -> Option<Pdo> {
        self.pdos.iter().copied().max_by_key(|p| (p.mw(), p.mv))
    }

    fn five_volt_only(&self) -> bool {
        self.pdos.iter().all(|p| p.mv <= 5000)
    }

    /// The option as the popup lists it. One supply says its volts and amps; a
    /// range of them says its volts, since the current differs at each, and both
    /// say the most power the set delivers.
    pub fn label(&self) -> String {
        let Some(best) = self.best() else {
            return self.name.clone();
        };
        let watts = decimal(best.mw());
        let low = self.pdos.iter().map(|p| p.mv).min().unwrap_or(best.mv);
        let high = self.pdos.iter().map(|p| p.mv).max().unwrap_or(best.mv);
        if low == high {
            format!("{}V {}A ({watts}W)", decimal(best.mv), decimal(best.ma))
        } else {
            format!("{}-{}V ({watts}W)", decimal(low), decimal(high))
        }
    }

    /// What the settings row shows for it.
    pub fn short(&self) -> String {
        match self.best() {
            Some(_) if self.five_volt_only() => "5V".into(),
            Some(best) => format!("{}W", decimal(best.mw())),
            None => self.name.clone(),
        }
    }

    /// What the set is for, beside it in the list.
    pub fn mode(&self) -> &'static str {
        match self.best() {
            None => "",
            Some(_) if self.five_volt_only() => "Compatibility mode",
            Some(best) if best.mw() > SUSTAINED_MW => "Burst-only high power mode",
            Some(_) => "Normal mode",
        }
    }

    /// The lines under the list while this set is the one picked: a warning for
    /// one the battery cannot keep up.
    pub fn hint(&self) -> Vec<&'static str> {
        match self.best() {
            Some(best) if best.mw() > SUSTAINED_MW => {
                vec!["CAUTION: the device will overheat quickly"]
            }
            _ => Vec::new(),
        }
    }
}

/// Offer `set`, taking the source role first unless a charger is what is
/// plugged in. Blocks for as long as a power role swap takes, which is up to
/// about a second, so it belongs on a thread of its own.
pub fn apply(port: &Port, set: &str) -> Result<(), String> {
    if !port.charger() && port.role == Role::Sink && port.partner.is_some() {
        write(&Path::new(PORT).join("power_role"), "source")?;
    }
    write(&Path::new(PORT).join("select_usb_power_delivery"), set)
}

/// One sysfs write, through sudo: the nodes are root's and flipctl runs as user.
fn write(path: &Path, value: &str) -> Result<(), String> {
    let path = path.to_string_lossy();
    let out = crate::boot::tool(&["sh", "-c", "echo \"$1\" > \"$2\"", "sh", value, &path])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{path}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{path}: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// `NETLINK_KOBJECT_UEVENT`, and its one group: the kernel's own broadcast.
const KOBJECT_UEVENT: libc::c_int = 15;
const UEVENT_KERNEL: u32 = 1;

/// Whether a uevent is about the port, its partner or a capability set.
fn concerns_port(message: &[u8]) -> bool {
    message
        .split(|b| *b == 0)
        .any(|field| field == b"SUBSYSTEM=typec" || field == b"SUBSYSTEM=usb_power_delivery")
}

/// Raised whenever the kernel says something about the port. The port itself is
/// read by whoever takes the flag, since one event or a dozen mean the same: what
/// was last read is no longer current.
///
/// Lives as long as the program: a device plugged in is news whatever is on the
/// panel. Dropping it ends the thread.
pub struct Watch {
    dirty: Arc<AtomicBool>,
    stopper: Option<Stopper>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Watch {
    pub fn spawn() -> Self {
        let dirty = Arc::new(AtomicBool::new(false));
        let none = |dirty| Self { dirty, stopper: None, thread: None };
        let (waiter, stopper) = match Stop::new() {
            Ok(stop) => stop.split(),
            Err(e) => {
                eprintln!("usb power      no eventfd, not following: {e}");
                return none(dirty);
            }
        };
        let socket = match Socket::subscribe(KOBJECT_UEVENT, UEVENT_KERNEL) {
            Ok(socket) => socket,
            Err(e) => {
                eprintln!("usb power      no uevents, not following: {e}");
                return none(dirty);
            }
        };
        let raised = Arc::clone(&dirty);
        let thread = thread::Builder::new()
            .name("watch-usb-pd".into())
            .spawn(move || {
                let mut buf = vec![0u8; 8192];
                loop {
                    match waiter.wait(&socket) {
                        Ok(Wake::Ready) => {}
                        Ok(Wake::Stopped) | Err(_) => return,
                    }
                    let Ok(got) = socket.recv(&mut buf) else { return };
                    if concerns_port(&buf[..got]) {
                        raised.store(true, Ordering::Relaxed);
                    }
                }
            })
            .ok();
        Self { dirty, stopper: Some(stopper), thread }
    }

    /// True once per burst of events.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(stopper) = self.stopper.as_ref() {
            stopper.stop();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where a test port lives, built fresh for each test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("usb-pd-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("{text}\n")).unwrap();
    }

    fn pdo(dir: &Path, n: u32, mv: u32, ma: u32) {
        let at = dir.join(format!("{n}:fixed_supply"));
        put(&at.join("voltage"), &format!("{mv}mV"));
        put(&at.join("maximum_current"), &format!("{ma}mA"));
    }

    /// The three sets the device tree gives the Flipper One.
    fn flipper(root: &Path, role: &str) -> (PathBuf, PathBuf, PathBuf) {
        let (port, partner, sets) = (root.join("port0"), root.join("partner"), root.join("pd"));
        put(&port.join("select_usb_power_delivery"), "[pd0] pd1 pd2");
        put(&port.join("power_role"), role);
        let caps = |n: &str| sets.join(n).join("source-capabilities");
        pdo(&caps("pd0"), 1, 5000, 3000);
        for (i, (mv, ma)) in
            [(5000, 3000), (9000, 2000), (12000, 1500), (15000, 1100), (20000, 750)]
                .iter()
                .enumerate()
        {
            pdo(&caps("pd1"), i as u32 + 1, *mv, *ma);
        }
        for (i, (mv, ma)) in
            [(5000, 3000), (9000, 3000), (12000, 2250), (15000, 1800), (20000, 1350)]
                .iter()
                .enumerate()
        {
            pdo(&caps("pd2"), i as u32 + 1, *mv, *ma);
        }
        // A variable supply, which must not be mistaken for a fixed one.
        put(&caps("pd2").join("6:variable_supply/maximum_current"), "1350mA");
        (port, partner, sets)
    }

    #[test]
    fn the_bracketed_name_is_the_selected_one() {
        assert_eq!(
            parse_choice("pd0 [pd1] pd2"),
            Some((vec!["pd0".into(), "pd1".into(), "pd2".into()], 1))
        );
        assert_eq!(parse_choice("source [sink]").map(|(_, at)| at), Some(1));
        assert_eq!(parse_choice("pd0 pd1"), None);
    }

    #[test]
    fn every_set_says_its_volts_amps_and_watts() {
        let root = scratch("labels");
        let (port, partner, sets) = flipper(&root, "source [sink]");
        let read = read_at(&port, &partner, &sets).unwrap();
        let labels: Vec<String> = read.sets.iter().map(Set::label).collect();
        assert_eq!(labels, ["5V 3A (15W)", "5-20V (18W)", "5-20V (27W)"]);
        let short: Vec<String> = read.sets.iter().map(Set::short).collect();
        assert_eq!(short, ["5V", "18W", "27W"]);
        let modes: Vec<&str> = read.sets.iter().map(Set::mode).collect();
        assert_eq!(modes, ["Compatibility mode", "Normal mode", "Burst-only high power mode"]);
        let hints: Vec<Vec<&str>> = read.sets.iter().map(Set::hint).collect();
        assert_eq!(hints, [vec![], vec![], vec!["CAUTION: the device will overheat quickly"]]);
        assert_eq!((read.selected, read.role, read.partner), (0, Role::Sink, None));
    }

    /// A phone starts out charging the Flipper, since the port tries to be a
    /// sink, and is still worth asking about. A charger is not, and neither is a
    /// mains-powered monitor that could also take power.
    #[test]
    fn a_phone_is_asked_about_and_a_charger_is_not() {
        let root = scratch("partners");
        let (port, partner, sets) = flipper(&root, "source [sink]");
        put(&partner.join("supports_usb_power_delivery"), "yes");
        let drp =
            partner.join("usb_power_delivery/source-capabilities/1:fixed_supply/dual_role_power");

        put(&drp, "1");
        let phone = read_at(&port, &partner, &sets).unwrap();
        assert!(!phone.charger() && phone.worth_asking());

        let mains = partner
            .join("usb_power_delivery/source-capabilities/1:fixed_supply/unconstrained_power");
        put(&mains, "1");
        let monitor = read_at(&port, &partner, &sets).unwrap();
        assert!(monitor.charger() && !monitor.worth_asking());
        std::fs::remove_file(&mains).unwrap();

        put(&drp, "0");
        let charger = read_at(&port, &partner, &sets).unwrap();
        assert!(charger.charger() && !charger.worth_asking());

        // Something we power, which sends no source capabilities of its own.
        std::fs::remove_file(&drp).unwrap();
        put(&port.join("power_role"), "[source] sink");
        let sink = read_at(&port, &partner, &sets).unwrap();
        assert!(!sink.charger() && sink.worth_asking());

        // And one that never spoke PD, which only ever sees 5V anyway.
        put(&partner.join("supports_usb_power_delivery"), "no");
        assert!(!read_at(&port, &partner, &sets).unwrap().worth_asking());
    }

    #[test]
    fn a_kernel_without_sets_has_no_port() {
        let root = scratch("old");
        put(&root.join("port0/power_role"), "source [sink]");
        assert_eq!(read_at(&root.join("port0"), &root.join("partner"), &root.join("pd")), None);
    }

    #[test]
    fn only_typec_uevents_count() {
        let typec = b"change@/devices/platform/x/typec/port0\0ACTION=change\0SUBSYSTEM=typec\0";
        let usb = b"add@/devices/platform/usb1\0ACTION=add\0SUBSYSTEM=usb\0";
        assert!(concerns_port(typec));
        assert!(!concerns_port(usb));
    }
}
