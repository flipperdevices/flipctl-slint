//! The proxy, in a process of its own, so stopping it is a kill: the kernel closes
//! both ends however far it got.
//!
//! It borrows the device port and the device from flipctl, takes the device from its
//! driver, offers the PC the same descriptors through raw-gadget, and makes every
//! request the PC makes again on the device. What it has to say goes to the app as
//! JSON lines on stdout; the app sets the log on its stdin; its trace goes to stderr.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::descriptors::{interfaces, patch_device};
use crate::gadget::Gadget;
use crate::host::Host;
use crate::ioctl::{EV_CONTROL, EV_DISCONNECT, EV_RESET, SPEED_FULL, SPEED_HIGH};
use crate::pcap::{Capture, Mode, KIND_CTRL};
use crate::pipe::{Pipe, Shared};

static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// A line for proxy.log beside the app, where the proxy's stderr goes.
pub fn trace(text: &str) {
    let at = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    let name = std::thread::current().name().unwrap_or("").to_string();
    eprintln!("{at:9.3} {name:10} {text}");
}

/// One line to the app. Lost if it is gone, which ends us anyway.
pub fn say(what: serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{what}");
    let _ = out.flush();
}

/// Lines sent per tick at most; the rest are counted, not shown.
const LINES_PER_TICK: usize = 16;

/// Transfers and bytes per endpoint, and while the app shows the log, a line for
/// each, sent to the app twenty times a second.
#[derive(Default)]
pub struct Counters {
    by_ep: Mutex<HashMap<u8, (u64, u64)>>,
    viewing: AtomicBool,
    lines: Mutex<(Vec<String>, u64)>,
}

impl Counters {
    pub fn count(&self, ep: u8, bytes: usize) {
        let mut map = self.by_ep.lock().unwrap();
        let c = map.entry(ep).or_default();
        c.0 += 1;
        c.1 += bytes as u64;
    }

    /// A line for the log, made only while the app shows it.
    pub fn line(&self, make: impl FnOnce() -> String) {
        if !self.viewing.load(Ordering::Relaxed) {
            return;
        }
        let mut lines = self.lines.lock().unwrap();
        if lines.0.len() < LINES_PER_TICK {
            lines.0.push(make());
        } else {
            lines.1 += 1;
        }
    }

    pub fn view(&self, on: bool) {
        self.viewing.store(on, Ordering::Relaxed);
    }

    fn json(&self) -> serde_json::Value {
        let map = self.by_ep.lock().unwrap();
        let counts: serde_json::Map<String, serde_json::Value> =
            map.iter().map(|(ep, (n, b))| (format!("{ep:02x}"), json!([n, b]))).collect();
        serde_json::Value::Object(counts)
    }

    /// The lines since the last tick and how many did not fit.
    fn take_lines(&self) -> (Vec<String>, u64) {
        std::mem::take(&mut *self.lines.lock().unwrap())
    }
}

/// The first bytes of a transfer, for its log line.
pub fn hexes(data: &[u8]) -> String {
    let shown: Vec<String> = data.iter().take(8).map(|b| format!("{b:02x}")).collect();
    shown.join(" ") + if data.len() > 8 { " .." } else { "" }
}

/// A control request as the log names it.
fn describe(setup: &[u8; 8]) -> String {
    let (rt, req) = (setup[0], setup[1]);
    let value = u16::from_le_bytes([setup[2], setup[3]]);
    let length = u16::from_le_bytes([setup[6], setup[7]]);
    if rt & 0x60 != 0 {
        let kind = if rt & 0x60 == 0x20 { "class" } else { "vendor" };
        return format!("{kind} {rt:02x} {req:02x} {value:04x} {length}");
    }
    let name = match req {
        0 => "GET_STATUS".to_string(),
        1 => "CLEAR_FEATURE".into(),
        3 => "SET_FEATURE".into(),
        6 => {
            let what = match value >> 8 {
                1 => "device".to_string(),
                2 => "config".into(),
                3 => "string".into(),
                6 => "qualifier".into(),
                15 => "BOS".into(),
                0x21 => "HID".into(),
                0x22 => "report".into(),
                other => format!("{other:#x}"),
            };
            format!("GET {what}")
        }
        8 => "GET_CONFIG".into(),
        9 => "SET_CONFIG".into(),
        10 => "GET_INTERFACE".into(),
        11 => "SET_INTERFACE".into(),
        other => format!("req {other}"),
    };
    format!("{name} {length}")
}

struct Proxy {
    shared: Arc<Shared>,
    pipes: HashMap<u8, Pipe>,
    alts: HashMap<u8, u8>,
    config: Option<Vec<u8>>,
}

impl Proxy {
    fn host(&self) -> &Host {
        &self.shared.host
    }

    fn gadget(&self) -> &Gadget {
        &self.shared.gadget
    }

    fn capture_control(&self, setup: &[u8; 8], out: &[u8], back: &[u8], status: i32) {
        let c = &self.shared.capture;
        let ident = c.next_id();
        let ep = if setup[0] & 0x80 != 0 { 0x80 } else { 0 };
        let length = u32::from(u16::from_le_bytes([setup[6], setup[7]]));
        c.write(ident, b'S', KIND_CTRL, ep, -libc::EINPROGRESS, length, out, Some(setup));
        c.write(ident, b'C', KIND_CTRL, ep, status, back.len() as u32, back, None);
    }

    fn open_interface(&mut self, number: u8, alt: u8) {
        let config = self.config.clone().unwrap_or_default();
        for i in interfaces(&config).into_iter().filter(|i| i.number == number && i.alt == alt) {
            for desc in &i.endpoints {
                if desc[3] & 3 == 1 {
                    trace(&format!("ep{:02x}: isochronous, not forwarded", desc[2]));
                    continue;
                }
                match Pipe::start(&self.shared, desc, i.bulk_only()) {
                    Ok(pipe) => {
                        self.pipes.insert(desc[2], pipe);
                    }
                    Err(e) => trace(&format!("ep{:02x}: {e}", desc[2])),
                }
            }
        }
        self.alts.insert(number, alt);
    }

    fn close_interface(&mut self, number: u8) {
        let alt = self.alts.get(&number).copied().unwrap_or(0);
        let config = self.config.clone().unwrap_or_default();
        for i in interfaces(&config).into_iter().filter(|i| i.number == number && i.alt == alt) {
            for desc in &i.endpoints {
                if let Some(pipe) = self.pipes.remove(&desc[2]) {
                    pipe.stop();
                }
            }
        }
    }

    fn close_all(&mut self) {
        for (_, pipe) in self.pipes.drain() {
            pipe.stop();
        }
    }

    fn stall(&self) {
        let _ = self.gadget().ep0_stall();
    }

    fn control(&mut self, setup: [u8; 8]) -> std::io::Result<()> {
        let (rt, req) = (setup[0], setup[1]);
        let value = u16::from_le_bytes([setup[2], setup[3]]);
        let index = u16::from_le_bytes([setup[4], setup[5]]);
        let length = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
        let (standard, recipient) = (rt & 0x60 == 0, rt & 0x1F);
        self.shared.counters.count(0, length);
        self.shared.counters.line(|| describe(&setup));
        if standard && recipient == 0 && req == 9 {
            return self.set_configuration(&setup, value as u8);
        }
        if standard && recipient == 1 && req == 11 {
            return self.set_interface(&setup, index as u8, value as u8);
        }
        if standard && recipient == 2 && req == 1 && value == 0 {
            return self.clear_halt(&setup, index as u8);
        }
        if rt & 0x80 != 0 {
            let data = match self.host().control(&setup, &[]) {
                Ok(d) => d,
                Err(e) => {
                    self.capture_control(&setup, &[], &[], -e.raw_os_error().unwrap_or(libc::EIO));
                    self.stall();
                    return Ok(());
                }
            };
            let data = if standard && req == 6 && value >> 8 == 1 {
                patch_device(&data, self.host().superspeed())
            } else if standard && req == 6 && value >> 8 == 2 && self.host().superspeed() {
                match self.host().config_at(usize::from(value & 0xFF)) {
                    Some(mut c) => {
                        c.truncate(length);
                        c
                    }
                    None => data,
                }
            } else {
                data
            };
            self.capture_control(&setup, &[], &data, 0);
            let data = &data[..data.len().min(length)];
            return self.gadget().ep0_write(data, data.len() < length);
        }
        let data = if length > 0 { self.gadget().ep0_read(length)? } else { vec![] };
        if let Err(e) = self.host().control(&setup, &data) {
            self.capture_control(&setup, &data, &[], -e.raw_os_error().unwrap_or(libc::EIO));
            if length == 0 {
                self.stall();
            }
            return Ok(());
        }
        self.capture_control(&setup, &data, &[], 0);
        if length == 0 {
            self.gadget().ep0_read(0)?;
        }
        Ok(())
    }

    fn set_configuration(&mut self, setup: &[u8; 8], value: u8) -> std::io::Result<()> {
        self.close_all();
        self.host().release_all();
        if let Err(e) = self.host().set_configuration(value) {
            self.capture_control(setup, &[], &[], -e.raw_os_error().unwrap_or(libc::EIO));
            trace(&format!("set config {value}: {e}"));
            self.stall();
            return Ok(());
        }
        self.capture_control(setup, &[], &[], 0);
        self.config = if value != 0 { self.host().config(value) } else { None };
        self.alts.clear();
        if let Some(config) = self.config.clone() {
            let found = interfaces(&config);
            for i in &found {
                if !self.host().claimed(u32::from(i.number)) {
                    self.host().claim(u32::from(i.number))?;
                }
            }
            let mut numbers: Vec<u8> = found.iter().map(|i| i.number).collect();
            numbers.sort_unstable();
            numbers.dedup();
            for n in numbers {
                self.open_interface(n, 0);
            }
            self.gadget().vbus_draw(config.get(8).copied().unwrap_or(50))?;
            self.gadget().configure()?;
        }
        self.gadget().ep0_read(0)?;
        say(json!({"state": if value != 0 { "running" } else { "unconfigured" }}));
        Ok(())
    }

    fn set_interface(&mut self, setup: &[u8; 8], number: u8, alt: u8) -> std::io::Result<()> {
        self.close_interface(number);
        if let Err(e) = self.host().set_interface(number.into(), alt.into()) {
            self.capture_control(setup, &[], &[], -e.raw_os_error().unwrap_or(libc::EIO));
            self.stall();
            return Ok(());
        }
        self.capture_control(setup, &[], &[], 0);
        self.open_interface(number, alt);
        self.gadget().ep0_read(0)?;
        Ok(())
    }

    fn clear_halt(&mut self, setup: &[u8; 8], ep: u8) -> std::io::Result<()> {
        if let Err(e) = self.host().clear_halt(ep) {
            self.capture_control(setup, &[], &[], -e.raw_os_error().unwrap_or(libc::EIO));
            self.stall();
            return Ok(());
        }
        self.capture_control(setup, &[], &[], 0);
        if let Some(pipe) = self.pipes.get(&ep) {
            let _ = self.gadget().clear_halt(pipe.handle);
            pipe.unhalt();
        }
        self.gadget().ep0_read(0)?;
        Ok(())
    }

    fn run(&mut self) -> std::io::Result<()> {
        // CONNECT is the gadget bound to the UDC; a PC shows itself by asking something
        say(json!({"state": "waiting for PC"}));
        let mut asked = false;
        loop {
            let (kind, data) = match self.gadget().event() {
                Ok(e) => e,
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
                Err(e) => return Err(e),
            };
            trace(&format!("event {kind} {}", hex(&data)));
            match kind {
                EV_CONTROL if data.len() >= 8 => {
                    if !asked {
                        asked = true;
                        say(json!({"state": "enumerating"}));
                    }
                    let setup: [u8; 8] = data[..8].try_into().unwrap();
                    if let Err(e) = self.control(setup) {
                        trace(&format!("control: {e}"));
                        self.stall();
                    }
                }
                EV_RESET | EV_DISCONNECT => {
                    let what = if kind == EV_RESET { "PC reset" } else { "PC gone" };
                    trace(what);
                    self.shared.counters.line(|| what.to_string());
                    self.close_all();
                    asked = false;
                    say(json!({"state": "waiting for PC"}));
                }
                _ => {}
            }
        }
    }
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

extern "C" fn on_signal(_: libc::c_int) {}

/// Whose port is the device side: the first UDC that is not USB/IP's virtual one.
fn udc() -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/udc")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.contains("vudc"))
        .collect();
    names.sort();
    names.into_iter().next()
}

/// `--proxy <port> <capture> [--log <mode>] [--view] [--unborrowed]`
pub fn main(args: &[String]) -> ! {
    START.get_or_init(Instant::now);
    // A blocked raw-gadget transfer gives way to this signal; without SA_RESTART the
    // ioctl returns EINTR rather than starting again
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
        libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut());
        // Gone with the app, however it goes
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
    }
    let port = args.get(2).cloned().unwrap_or_default();
    let path = PathBuf::from(args.get(3).cloned().unwrap_or_default());
    let mode = args
        .iter()
        .position(|a| a == "--log")
        .and_then(|i| args.get(i + 1))
        .and_then(|w| Mode::from_word(w))
        .unwrap_or(Mode::Off);
    let borrowed = !args.iter().any(|a| a == "--unborrowed");

    let started = (|| -> Result<(Vec<std::os::unix::net::UnixStream>, Proxy), String> {
        let held = if borrowed {
            vec![flipctl_app::claim("udc")?, flipctl_app::claim(&format!("usb/{port}"))?]
        } else {
            vec![]
        };
        let udc = udc().ok_or("no UDC")?;
        let host = Host::open(&port).map_err(|e| e.to_string())?;
        let speed = if host.speed.parse::<f64>().unwrap_or(480.0) <= 12.0 {
            SPEED_FULL
        } else {
            SPEED_HIGH
        };
        // Taken from its driver before the PC is offered it, so nothing on the
        // Flipper keeps using it meanwhile
        for i in interfaces(&host.config(host.active).unwrap_or_default()) {
            if !host.claimed(u32::from(i.number)) {
                host.claim(u32::from(i.number)).map_err(|e| e.to_string())?;
            }
        }
        let gadget = Gadget::open(&udc, speed).map_err(|e| e.to_string())?;
        let capture = Capture::new(path.clone(), host.bus, host.dev, mode);
        let shared = Arc::new(Shared {
            host,
            gadget: Arc::new(gadget),
            capture: Arc::new(capture),
            counters: Arc::new(Counters::default()),
        });
        Ok((held, Proxy { shared, pipes: HashMap::new(), alts: HashMap::new(), config: None }))
    })();
    let (held, mut proxy) = match started {
        Ok(p) => p,
        Err(e) => {
            say(json!({"error": e}));
            std::process::exit(1);
        }
    };
    say(json!({"held": held.len()}));

    // The app sets the log, and whether it shows the transfers, by a line on our stdin
    proxy.shared.counters.view(args.iter().any(|a| a == "--view"));
    let (capture, counters) = (proxy.shared.capture.clone(), proxy.shared.counters.clone());
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                if let Some(mode) = line.strip_prefix("log ").and_then(Mode::from_word) {
                    capture.set_mode(mode);
                } else if let Some(on) = line.strip_prefix("view ") {
                    counters.view(on == "on");
                }
            }
        })
        .ok();

    // Counters and lines to the app twenty times a second; a gone device ends us
    let shared = proxy.shared.clone();
    std::thread::Builder::new()
        .name("tick".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_millis(50));
            shared.capture.flush();
            let (lines, dropped) = shared.counters.take_lines();
            say(json!({
                "counts": shared.counters.json(),
                "lines": lines,
                "dropped": dropped,
                "viewers": shared.capture.viewers(),
                "lost": shared.capture.lost.load(Ordering::Relaxed),
            }));
            if shared.host.gone.load(Ordering::SeqCst) {
                say(json!({"error": "device unplugged"}));
                std::process::exit(1);
            }
        })
        .ok();

    let result = proxy.run();
    if let Err(e) = result {
        say(json!({"error": e.to_string()}));
    }
    drop(held);
    std::process::exit(1)
}
