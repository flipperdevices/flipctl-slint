//! USB Device Bridge: a USB device plugged into the Flipper, passed through to the PC with
//! every transfer written down, used on this Flipper, or served to the network.
//!
//! This is a proxy, not a tap. The Flipper enumerates the device as a host, takes it
//! from its kernel driver, and presents the same descriptors to the PC through
//! raw-gadget. Each transfer the PC makes is made again on the device, and the answer
//! handed back. So the device sees the Flipper's host controller and timing rather
//! than the PC's. The gadget runs at full speed for a low or full speed device and at
//! high speed for the rest; a SuperSpeed device is shown to the PC as a high speed
//! one, its descriptors rewritten to match. Isochronous endpoints are not forwarded.
//!
//! The page is a list. Up and Down move between its rows; on Device, Send to and Log,
//! Left and Right change the choice. Device lists what is plugged into this Flipper,
//! then what other Flippers on the LAN announce, then what the Transmitter at each
//! typed address serves, or the bare address while it serves nothing: the way to one
//! behind a VPN or a router, which a broadcast does not reach. A remote device is shown with its address beside its
//! name. The last choice is Add IP address, where OK opens the keyboard for another
//! address; OK on an address, or on a device served there, changes it, and an empty
//! one forgets it. Send to is the port the PC is on, named as the case
//! names it, this Flipper, or USB IP. Log writes what goes through
//! as a pcap in the usbmon format to ~/Captures, or streams it to Wireshark, which
//! connects to `TCP@<address>:19000`; it starts off, since writing every transfer down
//! costs speed, and may change while running. Run starts and stops.
//!
//! USB IP serves the device by USB/IP on port 3240 and announces it on port 3241. A
//! Linux PC attaches it with the stock client:
//!
//! ```text
//! sudo modprobe vhci-hcd
//! usbip list -r <address>
//! sudo usbip attach -r <address> -b <busid>
//! ```
//!
//! USB/IP and the Wireshark stream have neither authentication nor encryption: keep
//! them on a LAN or a VPN.

mod descriptors;
mod gadget;
mod host;
mod ioctl;
mod pcap;
mod pipe;
mod proxy;
mod remote;
mod sysfs;

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::Ipv4Addr;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use flipctl_app::font::{BitmapFont, ROW, ROW_ACTIVE};
use flipctl_app::{item, App, Item, Key, Page, Typing, Ui};

use crate::pcap::{Mode, WIRESHARK_PORT};
use crate::remote::{own_address, Beacons, Remote, Transmitter};
use crate::sysfs::{describe, device, read, socket_name, sockets};

// A menu row's text runs from past its icon (3 + 14 + 4) to the status inset (5) of
// its 224px line. A value between chevrons gives up room to them.
const MENU_WIDTH: u16 = 224 - 21 - 5;
const GAP: u16 = 8;
const CHEVRONS: u16 = 16;

/// Transfer log lines kept for the view, and how many it shows at once.
const KEPT_LINES: usize = 500;
const LOG_ROWS: usize = 8;

/// Where the typed addresses are kept, in the working directory flipctl gives the app.
const STORE: &str = "usb-device-bridge.conf";

/// The page's rows that take keys, by position.
const ROW_DEVICE: usize = 0;
const ROW_SEND_TO: usize = 1;
const ROW_LOG: usize = 2;

/// Device's last choice, which opens the keyboard for a Transmitter's address.
const ADD_ADDRESS: &str = "Add IP address";

const LOGS: [(Mode, &str); 3] =
    [(Mode::Off, "OFF"), (Mode::File, "File"), (Mode::Wireshark, "Remote Wireshark")];

/// `text`, cut with `..` to fit `room` pixels in `font`. The fonts have no ellipsis.
fn fit(text: &str, room: u16, font: &BitmapFont) -> String {
    let text: String =
        text.chars().map(|c| if (' '..='~').contains(&c) { c } else { '?' }).collect();
    let text = text.trim().to_string();
    if font.text_width(&text) <= room {
        return text;
    }
    let mut cut = text;
    while !cut.is_empty() && font.text_width(&format!("{}..", cut.trim_end())) > room {
        cut.pop();
    }
    format!("{}..", cut.trim_end())
}

/// A row whose label and status share its width without touching. The label is
/// measured in the bold face the selected row draws it in, and keeps at least half
/// the row; the status has what is left.
fn listed(label: &str, status: &str) -> Item {
    let budget = MENU_WIDTH - GAP;
    let room = budget.saturating_sub(ROW.text_width(status)).max(budget / 2);
    let label = fit(label, room, &ROW_ACTIVE);
    let status = fit(status, budget.saturating_sub(ROW_ACTIVE.text_width(&label)), &ROW);
    item(label).status(status)
}

/// A row Left and Right step through, or a plain one while it may not change.
fn chosen(label: &str, value: &str, at: usize, count: usize, locked: bool) -> Item {
    if locked || count < 2 {
        return listed(label, value);
    }
    let room = MENU_WIDTH.saturating_sub(ROW_ACTIVE.text_width(label) + GAP + CHEVRONS);
    item(label).range(fit(value, room, &ROW), at == 0, at + 1 == count)
}

fn size(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "K", "M", "G"] {
        if v < 1024.0 || unit == "G" {
            return if unit == "B" { format!("{n}B") } else { format!("{v:.1}{unit}") };
        }
        v /= 1024.0;
    }
    unreachable!()
}

/// The local time as the capture's name has it.
fn stamp() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

#[derive(Clone, PartialEq)]
enum Entry {
    Local(String),
    Remote(Remote),
    /// A typed address, asked what it serves.
    Address(Ipv4Addr),
    AddAddress,
}

#[derive(Clone, Copy, PartialEq)]
enum Dest {
    Pc,
    Flipper,
    UsbIp,
}

/// What the proxy process, the attach and the reader threads tell the page.
enum Msg {
    Line(String),
    Ended,
    Attached { local: String, held: UnixStream, ip: String, to_pc: bool },
    Error(String),
}

struct Bridge {
    entries: Vec<Entry>,
    choice: usize,
    dest: usize,
    log: usize,
    pc_port: String,
    address: String,
    beacons: Option<Beacons>,
    /// The Transmitters asked directly, the keyboard while one is being typed with the
    /// address it replaces, if any, and the file they are kept in, only where flipctl
    /// runs the app.
    typed: Vec<Ipv4Addr>,
    typing: Option<(Typing, Option<Ipv4Addr>)>,
    store: Option<PathBuf>,
    child: Option<Child>,
    transmitter: Option<Transmitter>,
    /// A remote device attached here: the attach held and whose it is.
    attached: Option<UnixStream>,
    attaching: bool,
    remote_ip: String,
    /// The device something runs for, on this machine's USB tree.
    port: String,
    state: String,
    capture: Option<PathBuf>,
    counts: HashMap<u8, (u64, u64)>,
    viewers: u64,
    lost: u64,
    /// The transfer log the Log key shows, newest last, and whether it is showing
    /// and following the newest line.
    lines: VecDeque<String>,
    viewing: bool,
    follow: bool,
    refreshed: Instant,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Bridge {
    fn new() -> Bridge {
        let (tx, rx) = channel();
        let pc_port = flipctl_app::device_port_name();
        let store = std::env::var_os("FLIPCTL_HOSTED").map(|_| PathBuf::from(STORE));
        let kept = store.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let mut typed: Vec<Ipv4Addr> = Vec::new();
        for ip in kept.lines().filter_map(|l| l.strip_prefix("address=")?.trim().parse().ok()) {
            if !typed.contains(&ip) {
                typed.push(ip);
            }
        }
        let mut s = Bridge {
            entries: vec![],
            choice: 0,
            dest: 0,
            log: 0,
            pc_port: if pc_port.is_empty() { "PC".into() } else { pc_port },
            address: own_address(),
            beacons: Beacons::listen().ok(),
            typed,
            typing: None,
            store,
            child: None,
            transmitter: None,
            attached: None,
            attaching: false,
            remote_ip: String::new(),
            port: String::new(),
            state: "-".into(),
            capture: None,
            counts: HashMap::new(),
            viewers: 0,
            lost: 0,
            lines: VecDeque::new(),
            viewing: false,
            follow: true,
            refreshed: Instant::now(),
            tx,
            rx,
        };
        if let Some(b) = &s.beacons {
            b.ask(&s.typed);
        }
        s.refresh();
        s
    }

    /// What the keyboard came back with, in place of `was` or as a new address: an
    /// address to ask, nothing to forget `was`, or something that is not an address,
    /// which changes nothing. The address typed is the choice afterwards.
    fn typed(&mut self, text: &str, was: Option<Ipv4Addr>) {
        let text = text.trim();
        let typed = match text.parse::<Ipv4Addr>() {
            _ if text.is_empty() => None,
            Ok(ip) => Some(ip),
            Err(_) => {
                self.state = "not an IPv4 address".into();
                return;
            }
        };
        self.typed = edited(&self.typed, was, typed);
        if let Some(b) = &self.beacons {
            b.ask(&self.typed);
        }
        if let Some(path) = &self.store {
            let lines: String = self.typed.iter().map(|ip| format!("address={ip}\n")).collect();
            let _ = if lines.is_empty() {
                std::fs::remove_file(path)
            } else {
                std::fs::write(path, lines)
            };
        }
        self.refresh();
        if let Some(at) = typed.and_then(|ip| self.position(&Entry::Address(ip))) {
            self.choice = at;
        }
    }

    // What there is to choose

    /// Read the devices again, keeping the one chosen.
    fn refresh(&mut self) {
        let keep = self.entries.get(self.choice).cloned();
        let mut entries: Vec<Entry> = sockets().into_iter().map(Entry::Local).collect();
        let remotes: Vec<Remote> = self
            .beacons
            .as_ref()
            .map(|b| b.current().into_iter().filter(|r| r.ip != self.address).collect())
            .unwrap_or_default();
        let typed: Vec<String> = self.typed.iter().map(Ipv4Addr::to_string).collect();
        // The LAN's Transmitters, then each typed address with what it serves
        entries
            .extend(remotes.iter().filter(|r| !typed.contains(&r.ip)).cloned().map(Entry::Remote));
        for (ip, text) in self.typed.iter().zip(&typed) {
            let served: Vec<Entry> =
                remotes.iter().filter(|r| r.ip == *text).cloned().map(Entry::Remote).collect();
            // The bare address only while it has nothing to list
            if served.is_empty() {
                entries.push(Entry::Address(*ip));
            }
            entries.extend(served);
        }
        entries.push(Entry::AddAddress);
        self.entries = entries;
        if let Some(i) = keep.and_then(|k| self.position(&k)) {
            self.choice = i;
        }
        self.choice = self.choice.min(self.entries.len().saturating_sub(1));
        self.dest = self.dest.min(self.destinations().len() - 1);
    }

    /// Where `wanted` is in the list, or failing that whatever else is at its address:
    /// a device gone from a Transmitter falls back to the next thing it serves, or to
    /// the address itself.
    fn position(&self, wanted: &Entry) -> Option<usize> {
        let ip = match wanted {
            Entry::Remote(r) => r.ip.clone(),
            Entry::Address(ip) => ip.to_string(),
            _ => String::new(),
        };
        self.entries.iter().position(|e| e == wanted).or_else(|| {
            self.entries.iter().position(|e| match e {
                Entry::Remote(r) => r.ip == ip,
                Entry::Address(a) => a.to_string() == ip,
                _ => false,
            })
        })
    }

    /// The typed address the choice belongs to: the address itself, or a device
    /// served there.
    fn typed_here(&self) -> Option<Ipv4Addr> {
        let ip = match self.entry()? {
            Entry::Address(ip) => *ip,
            Entry::Remote(r) => r.ip.parse().ok()?,
            _ => return None,
        };
        self.typed.contains(&ip).then_some(ip)
    }

    fn entry(&self) -> Option<&Entry> {
        self.entries.get(self.choice)
    }

    fn destinations(&self) -> Vec<Dest> {
        match self.entry() {
            Some(Entry::Remote(_) | Entry::Address(_) | Entry::AddAddress) => {
                vec![Dest::Pc, Dest::Flipper]
            }
            _ => vec![Dest::Pc, Dest::Flipper, Dest::UsbIp],
        }
    }

    fn destination(&self) -> Dest {
        self.destinations()[self.dest]
    }

    fn dest_name(&self, d: Dest) -> String {
        match d {
            Dest::Pc => self.pc_port.clone(),
            Dest::Flipper => "This FlipperOne".into(),
            Dest::UsbIp => "USB IP".into(),
        }
    }

    /// Product, ID, speed and where it is, for the chosen or running device.
    fn described(&self) -> (String, String, String, String) {
        if self.active()
            && !self.port.is_empty()
            && self.remote_ip.is_empty()
            && device(&self.port).exists()
        {
            let (p, i, s) = describe(&self.port);
            return (p, i, s, place(&self.port));
        }
        match self.entry() {
            None => ("plug one in".into(), "-".into(), "-".into(), "-".into()),
            Some(Entry::Local(port)) => {
                let (p, i, s) = describe(port);
                (p, i, s, place(port))
            }
            Some(Entry::Remote(r)) => {
                let speed = if r.speed.is_empty() { "-".into() } else { format!("{}M", r.speed) };
                (device_at(&r.product, &r.ip), r.ident.clone(), speed, r.ip.clone())
            }
            Some(Entry::Address(ip)) => (ip.to_string(), "-".into(), "-".into(), ip.to_string()),
            Some(Entry::AddAddress) => (ADD_ADDRESS.into(), "-".into(), "-".into(), "-".into()),
        }
    }

    // Running

    fn active(&self) -> bool {
        self.child.is_some()
            || self.attached.is_some()
            || self.transmitter.is_some()
            || self.attaching
    }

    fn startable(&self) -> bool {
        match self.entry() {
            None | Some(Entry::Address(_) | Entry::AddAddress) => false,
            Some(Entry::Local(_)) => self.destination() != Dest::Flipper,
            Some(Entry::Remote(_)) => true,
        }
    }

    fn start(&mut self) {
        if self.active() || !self.startable() {
            return;
        }
        self.counts.clear();
        match self.entry().cloned() {
            Some(Entry::Remote(r)) => self.connect(r, self.destination() == Dest::Pc),
            Some(Entry::Local(port)) if self.destination() == Dest::UsbIp => {
                self.transmitter = Some(Transmitter::start(&port));
                self.port = port;
            }
            Some(Entry::Local(port)) => self.proxy(&port),
            Some(Entry::Address(_) | Entry::AddAddress) | None => {}
        }
    }

    /// Attach what a Transmitter serves, then proxy it to the PC like a local device,
    /// or leave it to this Flipper's own drivers. The attach takes a second or two,
    /// so it runs off the drawing thread.
    fn connect(&mut self, r: Remote, to_pc: bool) {
        self.state = "attaching".into();
        self.attaching = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let held = match flipctl_app::claim(&format!("usbip-attach/{}/{}", r.ip, r.busid)) {
                Ok(h) => h,
                Err(e) => return tx.send(Msg::Error(e)).unwrap_or(()),
            };
            let landed =
                read(&PathBuf::from(format!("/run/flipctl-usbip-attach-{}+{}", r.ip, r.busid)));
            match landed.split_whitespace().nth(1) {
                Some(local) => {
                    let _ = tx.send(Msg::Attached { local: local.into(), held, ip: r.ip, to_pc });
                }
                None => {
                    let _ = tx.send(Msg::Error("attached, but not found".into()));
                }
            }
            flipctl_app::redraw();
        });
    }

    fn proxy(&mut self, port: &str) {
        self.port = port.to_string();
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
        let dir = home.join("Captures");
        let _ = std::fs::create_dir_all(&dir);
        let capture = dir.join(format!("usb-{}.pcap", stamp()));
        self.capture = Some(capture.clone());
        self.state = "starting".into();
        let exe = std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into());
        // The app's working directory is its own, so the trace lands beside it
        let trace =
            std::fs::File::create("proxy.log").map(Stdio::from).unwrap_or_else(|_| Stdio::null());
        let spawned = Command::new(exe)
            .arg("--proxy")
            .arg(port)
            .arg(&capture)
            .args(["--log", LOGS[self.log].0.word()])
            .args(if self.viewing { &["--view"][..] } else { &[] })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(trace)
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                self.state = format!("proxy: {e}");
                return;
            }
        };
        let out = child.stdout.take();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if let Some(out) = out {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let _ = tx.send(Msg::Line(line));
                }
            }
            let _ = tx.send(Msg::Ended);
        });
        self.child = Some(child);
    }

    /// A line on the proxy's stdin, if it runs.
    fn tell(&mut self, what: &str) {
        if let Some(stdin) = self.child.as_mut().and_then(|c| c.stdin.as_mut()) {
            let _ = writeln!(stdin, "{what}");
        }
    }

    fn set_log(&mut self, log: usize) {
        self.log = log;
        self.tell(&format!("log {}", LOGS[log].0.word()));
    }

    fn view(&mut self, on: bool) {
        self.viewing = on;
        self.follow = true;
        self.tell(if on { "view on" } else { "view off" });
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(3);
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
            self.state = "stopped".into();
        } else if self.attached.is_some() {
            self.state = "detached".into();
        } else if self.transmitter.is_some() {
            self.state = "stopped".into();
        }
        if let Some(t) = self.transmitter.take() {
            t.stop();
        }
        self.detach();
    }

    fn detach(&mut self) {
        self.attached = None;
        self.attaching = false;
        self.remote_ip.clear();
    }

    /// Apply whatever has been said since the last look.
    fn take(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Line(line) => self.hear(&line),
                Msg::Ended => {
                    if let Some(mut c) = self.child.take() {
                        let _ = c.wait();
                    }
                    self.detach();
                    if ["starting", "running", "waiting for PC", "enumerating", "unconfigured"]
                        .contains(&self.state.as_str())
                    {
                        self.state = "proxy ended".into();
                    }
                }
                Msg::Attached { local, held, ip, to_pc } => {
                    self.attaching = false;
                    self.attached = Some(held);
                    self.remote_ip = ip;
                    if to_pc {
                        self.proxy(&local);
                    } else {
                        self.port = local;
                        self.state = "on This FlipperOne".into();
                    }
                }
                Msg::Error(e) => {
                    self.attaching = false;
                    self.state = e;
                }
            }
        }
    }

    fn hear(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
        if let Some(e) = v.get("error").and_then(|x| x.as_str()) {
            self.state = e.into();
        }
        if let Some(s) = v.get("state").and_then(|x| x.as_str()) {
            self.state = s.into();
        }
        if let Some(counts) = v.get("counts").and_then(|x| x.as_object()) {
            self.counts = counts
                .iter()
                .filter_map(|(k, v)| {
                    let ep = u8::from_str_radix(k, 16).ok()?;
                    Some((ep, (v.get(0)?.as_u64()?, v.get(1)?.as_u64()?)))
                })
                .collect();
            self.viewers = v.get("viewers").and_then(|x| x.as_u64()).unwrap_or(0);
            self.lost = v.get("lost").and_then(|x| x.as_u64()).unwrap_or(0);
        }
        let said = v.get("lines").and_then(|x| x.as_array()).into_iter().flatten();
        self.lines.extend(said.filter_map(|l| l.as_str()).map(String::from));
        if let Some(n) = v.get("dropped").and_then(|x| x.as_u64()).filter(|n| *n > 0) {
            self.lines.push_back(format!(".. {n} more"));
        }
        while self.lines.len() > KEPT_LINES {
            self.lines.pop_front();
        }
    }

    // Drawing

    fn items(&self) -> Vec<Item> {
        let (product, ident, speed, place) = self.described();
        let locked = self.active() || self.entries.is_empty();
        let dests = self.destinations();
        let mut rows = vec![
            chosen("Device", &product, self.choice, self.entries.len(), locked),
            chosen("Send to", &self.dest_name(self.destination()), self.dest, dests.len(), locked),
            chosen("Log", LOGS[self.log].1, self.log, LOGS.len(), false),
        ];
        match LOGS[self.log].0 {
            // On rows of their own: beside a label they do not fit
            Mode::File => {
                let name = match &self.capture {
                    Some(c) if self.child.is_some() => {
                        c.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    }
                    _ => "~/Captures/".into(),
                };
                rows.push(item(fit(&name, MENU_WIDTH, &ROW_ACTIVE)));
            }
            Mode::Wireshark => {
                rows.push(listed(
                    "Wireshark",
                    &viewers(self.child.is_some(), self.viewers, self.lost),
                ));
                let interface = format!("TCP@{}:{WIRESHARK_PORT}", self.address);
                rows.push(item(fit(&interface, MENU_WIDTH, &ROW_ACTIVE)));
            }
            Mode::Off => {}
        }
        let asking = match self.entry() {
            Some(Entry::Address(ip)) if !self.active() => {
                self.beacons.as_ref().map(|b| match b.answer(*ip) {
                    None => "asking".into(),
                    Some(None) => "no answer".into(),
                    Some(Some(0)) => "nothing served".into(),
                    Some(Some(1)) => "1 device".into(),
                    Some(Some(n)) => format!("{n} devices"),
                })
            }
            _ => None,
        };
        let state = self
            .transmitter
            .as_ref()
            .map(Transmitter::state)
            .or(asking)
            .unwrap_or_else(|| self.state.clone());
        rows.push(listed("State", &state));
        if self.transmitter.is_some() {
            rows.push(listed("Address", &self.address));
        }
        let sent: u64 =
            self.counts.iter().filter(|(ep, _)| **ep & 0x80 != 0).map(|(_, c)| c.1).sum();
        let got: u64 = self
            .counts
            .iter()
            .filter(|(ep, _)| **ep & 0x80 == 0 && **ep != 0)
            .map(|(_, c)| c.1)
            .sum();
        let control = self.counts.get(&0).map(|c| c.0).unwrap_or(0);
        let source = if self.remote_ip.is_empty() { place.clone() } else { self.remote_ip.clone() };
        rows.push(listed(&format!("From {source}"), &size(sent)));
        rows.push(listed(&format!("From {}", self.pc_port), &size(got)));
        rows.push(listed("Control", &format!("{control} requests")));
        rows.push(listed("Port", &place));
        rows.push(listed("ID", &ident));
        rows.push(listed("Speed", &speed));
        rows
    }
}

/// Who is watching, once the proxy runs: "1 viewer", "2 viewers, 30 lost".
fn viewers(running: bool, count: u64, lost: u64) -> String {
    if !running {
        return String::new();
    }
    let plural = if count == 1 { "" } else { "s" };
    let lost = if lost > 0 { format!(", {lost} lost") } else { String::new() };
    format!("{count} viewer{plural}{lost}")
}

/// The addresses once `was` is replaced by `typed`, or `typed` added when `was` is
/// none. Removed when `typed` is none; never listed twice.
fn edited(list: &[Ipv4Addr], was: Option<Ipv4Addr>, typed: Option<Ipv4Addr>) -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for &ip in list {
        let ip = if Some(ip) == was { typed } else { Some(ip) };
        if let Some(ip) = ip.filter(|ip| !out.contains(ip)) {
            out.push(ip);
        }
    }
    if let Some(ip) = typed.filter(|ip| !out.contains(ip)) {
        out.push(ip);
    }
    out
}

/// An IPv4 address without its last number, for the keyboard to start from:
/// "192.168.1." for this Flipper's "192.168.1.125". Anything else starts empty.
fn network_of(address: &str) -> String {
    match address.parse::<Ipv4Addr>() {
        Ok(ip) => {
            let [a, b, c, _] = ip.octets();
            format!("{a}.{b}.{c}.")
        }
        Err(_) => String::new(),
    }
}

/// A remote device's choice: its name, cut to leave the address whole beside it.
fn device_at(product: &str, ip: &str) -> String {
    let room = MENU_WIDTH.saturating_sub(ROW_ACTIVE.text_width("Device") + GAP + CHEVRONS);
    let left = room.saturating_sub(ROW.text_width(&format!(" {ip}")));
    format!("{} {ip}", fit(product, left, &ROW))
}

/// Where a local device is: the socket's name on the case, or its place on the tree.
fn place(port: &str) -> String {
    let name = socket_name(port);
    if name.is_empty() {
        port.to_string()
    } else {
        name
    }
}

impl App for Bridge {
    fn page(&self) -> Page {
        if self.viewing {
            let lines: Vec<String> = self.lines.iter().cloned().collect();
            return Page::log("Transfers", lines);
        }
        Page::menu(self.items())
    }

    fn buttons(&self) -> [&str; 5] {
        let go = if self.active() {
            "Stop"
        } else if self.startable() {
            "Start"
        } else {
            ""
        };
        if self.viewing {
            ["Back", "", "", "", go]
        } else {
            ["Close", "Log", "", "", go]
        }
    }

    fn key(&mut self, key: Key, ui: &mut Ui) -> bool {
        if self.viewing {
            match key {
                Key::Escape | Key::Back => {
                    self.view(false);
                    return true;
                }
                // Scrolling back stops following; scrolling to the end follows again
                Key::Up => self.follow = false,
                Key::Down => self.follow = ui.offset() + LOG_ROWS + 1 >= self.lines.len(),
                _ => {}
            }
        }
        match key {
            Key::View if !self.viewing => {
                self.view(true);
                ui.scroll_to(usize::MAX / 2);
                true
            }
            Key::Ok
                if !self.viewing
                    && ui.selected() == ROW_DEVICE
                    && (self.entry() == Some(&Entry::AddAddress)
                        || self.typed_here().is_some()) =>
            {
                if !self.active() && self.typing.is_none() {
                    let was = self.typed_here();
                    let now = match was {
                        Some(ip) => ip.to_string(),
                        None => network_of(&own_address()),
                    };
                    self.typing = Some((flipctl_app::type_text(ADD_ADDRESS, &now), was));
                }
                true
            }
            Key::Run => {
                if self.active() {
                    self.stop();
                } else {
                    self.start();
                }
                true
            }
            Key::Left | Key::Right => {
                let step = |at: usize, count: usize| -> usize {
                    if key == Key::Right {
                        (at + 1).min(count.saturating_sub(1))
                    } else {
                        at.saturating_sub(1)
                    }
                };
                if self.viewing {
                    return true;
                }
                match ui.selected() {
                    // The log may change while running; what to run may not
                    ROW_LOG => self.set_log(step(self.log, LOGS.len())),
                    ROW_DEVICE if !self.active() && !self.entries.is_empty() => {
                        self.choice = step(self.choice, self.entries.len());
                        self.dest = self.dest.min(self.destinations().len() - 1);
                    }
                    ROW_SEND_TO if !self.active() => {
                        self.dest = step(self.dest, self.destinations().len())
                    }
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }

    fn every(&self) -> Option<Duration> {
        Some(Duration::from_millis(50))
    }

    fn tick(&mut self, ui: &mut Ui) {
        self.take();
        if let Some((answer, was)) =
            self.typing.as_ref().and_then(|(t, was)| Some((t.take()?, *was)))
        {
            self.typing = None;
            match answer {
                Ok(Some(text)) => self.typed(&text, was),
                Ok(None) => {}
                Err(why) => self.state = why,
            }
        }
        // The devices are sysfs reads, so four times a second is enough for them
        if !self.active() && self.refreshed.elapsed() >= Duration::from_millis(250) {
            self.refresh();
            self.refreshed = Instant::now();
        }
        if self.viewing && self.follow {
            ui.scroll_to(usize::MAX / 2);
        }
    }

    fn close(&mut self) {
        self.stop();
        self.beacons = None;
    }
}

/// The app, or with `--proxy` the proxy process the app starts.
fn start() -> Bridge {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--proxy") {
        proxy::main(&args);
    }
    Bridge::new()
}

flipctl_app::main!(start());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_cut_to_their_room() {
        let budget = MENU_WIDTH - GAP;
        let long = "A very long device name that cannot possibly fit on the panel";
        let cut = fit(long, budget / 2, &ROW_ACTIVE);
        assert!(cut.ends_with(".."));
        assert!(ROW_ACTIVE.text_width(&cut) <= budget / 2);
        assert_eq!(fit("Disk", 100, &ROW), "Disk");
        assert_eq!(fit("caf\u{e9}", 100, &ROW), "caf?", "outside the fonts' ASCII");
    }

    #[test]
    fn a_label_and_its_status_never_touch() {
        for (l, s) in [
            ("DataTraveler C10", "192.168.1.115"),
            ("From 192.168.1.115", "13.8M"),
            ("Wireshark", "TCP@192.168.1.125:19000"),
        ] {
            let row = listed(l, s);
            let used = ROW_ACTIVE.text_width(&row.label) + GAP + ROW.text_width(&row.status);
            assert!(used <= MENU_WIDTH, "{l} | {s}: {used}");
        }
    }

    /// The longest interface string an IPv4 address gives fits a row of its own, in
    /// the bold face the selected row draws it in.
    #[test]
    fn the_wireshark_interface_fits_its_row() {
        let longest = format!("TCP@255.255.255.255:{WIRESHARK_PORT}");
        assert!(
            ROW_ACTIVE.text_width(&longest) <= MENU_WIDTH,
            "{}",
            ROW_ACTIVE.text_width(&longest)
        );
        assert_eq!(fit(&longest, MENU_WIDTH, &ROW_ACTIVE), longest);
        assert_eq!(viewers(false, 3, 0), "");
        assert_eq!(viewers(true, 1, 0), "1 viewer");
        assert_eq!(viewers(true, 2, 30), "2 viewers, 30 lost");
    }

    #[test]
    fn addresses_are_added_changed_and_forgotten() {
        let (a, b, c): (Ipv4Addr, Ipv4Addr, Ipv4Addr) =
            ("10.0.0.1".parse().unwrap(), "10.0.0.2".parse().unwrap(), "10.0.0.3".parse().unwrap());
        assert_eq!(edited(&[], None, Some(a)), [a]);
        assert_eq!(edited(&[a], None, Some(b)), [a, b]);
        assert_eq!(edited(&[a, b], None, Some(a)), [a, b], "added twice");
        assert_eq!(edited(&[a, b], Some(a), Some(c)), [c, b], "changed in place");
        assert_eq!(edited(&[a, b], Some(a), Some(b)), [b], "changed to one listed");
        assert_eq!(edited(&[a, b], Some(a), None), [b], "forgotten");
        assert_eq!(edited(&[a, b], None, None), [a, b]);
    }

    #[test]
    fn the_keyboard_starts_on_this_network() {
        assert_eq!(network_of("192.168.1.125"), "192.168.1.");
        assert_eq!(network_of("-"), "");
        assert_eq!(network_of("fe80::1"), "");
    }

    /// The address is never what gets cut.
    #[test]
    fn a_remote_device_keeps_its_address_whole() {
        let shown =
            device_at("A very long device name that cannot possibly fit", "192.168.100.115");
        assert!(shown.ends_with(".. 192.168.100.115"), "{shown}");
        let room = MENU_WIDTH - ROW_ACTIVE.text_width("Device") - GAP - CHEVRONS;
        assert!(ROW.text_width(&shown) <= room, "{shown}");
        assert_eq!(chosen("Device", &shown, 1, 3, false).value, shown);
        assert_eq!(device_at("Disk", "10.0.0.2"), "Disk 10.0.0.2");
    }

    #[test]
    fn sizes_read_short() {
        assert_eq!(size(0), "0B");
        assert_eq!(size(1536), "1.5K");
        assert_eq!(size(14_470_000), "13.8M");
    }
}
